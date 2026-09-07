//! Terminal dashboard: account table with quota bars, live activity log, and a
//! few keys (q quit, R reload, s switch, p probe). All text that reaches the
//! screen came through `safe_text` first.

use std::collections::VecDeque;
use std::io::Stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use serde_json::Value;

use crate::manager::Manager;
use crate::prober::Prober;
use crate::proxy::server::{Activity, Ctx};
use crate::security::safe_text;
use crate::status::{bar, countdown};

struct InFlight {
    id: String,
    started: std::time::Instant,
    line: String,
    account: String,
}

pub struct Tui {
    ctx: Ctx,
    manager: Manager,
    prober: Prober,
    log: VecDeque<String>,
    inflight: Vec<InFlight>,
    selecting: Option<usize>,
    message: Option<String>,
    activity_file: Option<std::fs::File>,
}

fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

impl Tui {
    pub fn new(ctx: Ctx, manager: Manager, prober: Prober, activity_file: Option<std::fs::File>) -> Tui {
        Tui { ctx, manager, prober, log: VecDeque::new(), inflight: Vec::new(), selecting: None, message: None, activity_file }
    }

    fn push_log(&mut self, line: String) {
        let line = safe_text(&line, 300);
        if let Some(f) = &mut self.activity_file {
            use std::io::Write;
            let _ = writeln!(f, "{line}");
        }
        self.log.push_front(line);
        while self.log.len() > 500 {
            self.log.pop_back();
        }
    }

    fn on_activity(&mut self, a: Activity) {
        match a {
            Activity::Start { id, method, path, model, session, client } => {
                let sess = session.map(|s| s.chars().take(6).collect::<String>()).unwrap_or_default();
                let who = client.map(|c| format!("[{c}] ")).unwrap_or_default();
                let model = model.map(|m| format!(" ({m})")).unwrap_or_default();
                let line = format!("{who}{sess:<6} {method} {path}{model}");
                self.inflight.push(InFlight { id, started: std::time::Instant::now(), line: safe_text(&line, 200), account: String::new() });
            }
            Activity::Account { id, account } => {
                if let Some(f) = self.inflight.iter_mut().find(|f| f.id == id) {
                    f.account = safe_text(&account, 40);
                }
            }
            Activity::End { id, account, status, elapsed_ms, ok } => {
                if let Some(pos) = self.inflight.iter().position(|f| f.id == id) {
                    let f = self.inflight.remove(pos);
                    let mark = if ok { "✓" } else { "✗" };
                    self.push_log(format!("{mark} {}  {} → {} {} ({:.1}s)", ts(), f.line, safe_text(&account, 40), status, elapsed_ms as f64 / 1000.0));
                }
            }
            Activity::Log(s) => self.push_log(format!("  {}  {s}", ts())),
        }
    }

    pub async fn run(mut self, mut shutdown_tx: tokio::sync::watch::Sender<bool>) -> Result<()> {
        let mut terminal = ratatui::init();
        let res = self.event_loop(&mut terminal, &mut shutdown_tx).await;
        ratatui::restore();
        res
    }

    async fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>, shutdown: &mut tokio::sync::watch::Sender<bool>) -> Result<()> {
        let mut events = EventStream::new();
        let mut activity = self.ctx.activity.subscribe();
        let mut mgr_events = self.manager.events.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            let st = self.manager.status(false);
            terminal.draw(|f| self.draw(f, &st))?;
            tokio::select! {
                _ = tick.tick() => {}
                Ok(a) = activity.recv() => self.on_activity(a),
                Ok(m) = mgr_events.recv() => self.push_log(format!("  {}  {m}", ts())),
                Some(Ok(ev)) = events.next() => {
                    if let Event::Key(k) = ev {
                        if k.kind != KeyEventKind::Press { continue; }
                        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                            let _ = shutdown.send(true);
                            return Ok(());
                        }
                        if let Some(sel) = self.selecting {
                            let n = st.get("accounts").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
                            match k.code {
                                KeyCode::Esc | KeyCode::Char('q') => self.selecting = None,
                                KeyCode::Up | KeyCode::Char('k') => self.selecting = Some(sel.saturating_sub(1)),
                                KeyCode::Down | KeyCode::Char('j') => self.selecting = Some((sel + 1).min(n.saturating_sub(1))),
                                KeyCode::Enter => {
                                    if let Some(id) = st.pointer(&format!("/accounts/{sel}/id")).and_then(Value::as_str) {
                                        if let Some((name, blocked)) = self.manager.switch_to(id) {
                                            self.message = Some(match blocked {
                                                None => format!("switched to {name}"),
                                                Some(b) => format!("switched to {name} (currently {b})"),
                                            });
                                        }
                                    }
                                    self.selecting = None;
                                }
                                _ => {}
                            }
                            continue;
                        }
                        match k.code {
                            KeyCode::Char('q') => { let _ = shutdown.send(true); return Ok(()); }
                            KeyCode::Char('R') | KeyCode::Char('r') => {
                                match self.ctx.reload.as_ref().map(|f| f()) {
                                    Some(Ok(n)) => self.message = Some(format!("config reloaded ({n} added)")),
                                    Some(Err(e)) => self.message = Some(format!("reload failed: {}", safe_text(&e.to_string(), 80))),
                                    None => {}
                                }
                            }
                            KeyCode::Char('s') => self.selecting = Some(0),
                            KeyCode::Char('p') => {
                                let p = self.prober.clone();
                                tokio::spawn(async move { p.probe_all().await });
                                self.message = Some("probing all OAuth accounts…".into());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    fn draw(&self, f: &mut Frame, st: &Value) {
        let area = f.area();
        let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(6), Constraint::Percentage(45), Constraint::Length(1)]).split(area);

        let sessions = st.get("sessions");
        let header = format!(
            " TeamClaude v{}   current: {}   sessions: {} active / {} known   threshold: {}   probe: {}",
            st.get("version").and_then(Value::as_str).unwrap_or("?"),
            st.get("current").and_then(Value::as_str).unwrap_or("-"),
            sessions.and_then(|s| s.get("active")).and_then(Value::as_u64).unwrap_or(0),
            sessions.and_then(|s| s.get("known")).and_then(Value::as_u64).unwrap_or(0),
            st.get("switchThreshold").map(|t| t.to_string()).unwrap_or_default(),
            match self.prober.interval() {
                0 => "off".to_string(),
                n => format!("{n}s"),
            },
        );
        f.render_widget(Paragraph::new(header).style(Style::default().bold()), chunks[0]);

        let rows: Vec<Row> = st
            .get("accounts")
            .and_then(Value::as_array)
            .map(|accts| {
                accts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let current = a.get("current").and_then(Value::as_bool).unwrap_or(false);
                        let name = safe_text(a.get("name").and_then(Value::as_str).unwrap_or("?"), 28);
                        let q = |b: &str| {
                            let u = a.pointer(&format!("/quota/{b}/utilization")).and_then(Value::as_f64);
                            let r = a.pointer(&format!("/quota/{b}/resetInSeconds")).and_then(Value::as_i64);
                            format!("{} {} {}", bar(u, 10), u.map(|x| format!("{:>3.0}%", x * 100.0)).unwrap_or("  ?%".into()), countdown(r))
                        };
                        let blocked = a.get("blocked").and_then(Value::as_str);
                        let status = if a.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
                            "disabled".into()
                        } else {
                            blocked.map(|b| safe_text(b, 30)).unwrap_or_else(|| "ready".into())
                        };
                        let style = if self.selecting == Some(i) {
                            Style::default().bg(Color::Blue).fg(Color::White)
                        } else if current {
                            Style::default().fg(Color::Green).bold()
                        } else if blocked.is_some() {
                            Style::default().fg(Color::DarkGray)
                        } else {
                            Style::default()
                        };
                        Row::new(vec![
                            Cell::from(if current { "►" } else { " " }),
                            Cell::from(name),
                            Cell::from(a.get("priority").and_then(Value::as_i64).unwrap_or(0).to_string()),
                            Cell::from(q("unified5h")),
                            Cell::from(q("unified7d")),
                            Cell::from(q("unified7dFable")),
                            Cell::from(status),
                        ])
                        .style(style)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(28),
                Constraint::Length(3),
                Constraint::Length(24),
                Constraint::Length(24),
                Constraint::Length(24),
                Constraint::Min(10),
            ],
        )
        .header(Row::new(vec!["", "account", "pri", "5h", "7d", "7d fable", "state"]).style(Style::default().fg(Color::Yellow)))
        .block(Block::default().borders(Borders::ALL).title(" accounts "));
        f.render_widget(table, chunks[1]);

        let mut lines: Vec<Line> = Vec::new();
        for r in &self.inflight {
            let acct = if r.account.is_empty() { String::new() } else { format!(" → {}", r.account) };
            lines.push(
                Line::from(format!("⠋ {}  {}{} ({:.1}s…)", ts(), r.line, acct, r.started.elapsed().as_secs_f64())).style(Style::default().fg(Color::Cyan)),
            );
        }
        for l in self.log.iter().take(chunks[2].height as usize) {
            lines.push(Line::from(l.clone()));
        }
        f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" activity ")), chunks[2]);

        let footer = match (&self.selecting, &self.message) {
            (Some(_), _) => " ↑/↓ choose account, Enter switch, Esc cancel".to_string(),
            (None, Some(m)) => format!(" {m}   |   q quit  R reload  s switch  p probe"),
            (None, None) => " q quit   R reload config   s switch account   p probe quota   (proxy is running)".to_string(),
        };
        f.render_widget(Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)), chunks[3]);
    }
}
