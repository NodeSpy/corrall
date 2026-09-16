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

use crate::prober::{ManualProbe, Prober};
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
    pub fn new(ctx: Ctx, prober: Prober, activity_file: Option<std::fs::File>) -> Tui {
        Tui { ctx, prober, log: VecDeque::new(), inflight: Vec::new(), selecting: None, message: None, activity_file }
    }

    /// The `(pool, account)` pairs the table shows, in display order. This is
    /// also what the selection cursor walks, so an index means the same thing
    /// in both places.
    fn flat_accounts(st: &Value) -> Vec<(String, &Value)> {
        let mut out = Vec::new();
        for p in st.get("pools").and_then(Value::as_array).into_iter().flatten() {
            let pool = p.get("pool").and_then(Value::as_str).unwrap_or("").to_string();
            for a in p.get("accounts").and_then(Value::as_array).into_iter().flatten() {
                out.push((pool.clone(), a));
            }
        }
        out
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
                let width = if self.ctx.titles.enabled() { self.ctx.titles.width() } else { 6 };
                let mut sess = self.ctx.titles.label(session.as_deref(), crate::quota::now_ms());
                if sess.chars().count() > width {
                    sess = sess.chars().take(width).collect();
                }
                let sess = format!("{sess:<width$}");
                let who = client.map(|c| format!("[{c}] ")).unwrap_or_default();
                let model = model.map(|m| format!(" ({m})")).unwrap_or_default();
                let line = format!("{who}{sess} {method} {path}{model}");
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
        // Every pool's log lines already arrive here as `Activity::Log`.
        let mut activity = self.ctx.activity.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            let st = self.ctx.pools.status(false);
            terminal.draw(|f| self.draw(f, &st))?;
            tokio::select! {
                _ = tick.tick() => {}
                Ok(a) = activity.recv() => self.on_activity(a),
                Some(Ok(ev)) = events.next() => {
                    if let Event::Key(k) = ev {
                        if k.kind != KeyEventKind::Press { continue; }
                        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                            let _ = shutdown.send(true);
                            return Ok(());
                        }
                        if let Some(sel) = self.selecting {
                            let flat = Self::flat_accounts(&st);
                            match k.code {
                                KeyCode::Esc | KeyCode::Char('q') => self.selecting = None,
                                KeyCode::Up | KeyCode::Char('k') => self.selecting = Some(sel.saturating_sub(1)),
                                KeyCode::Down | KeyCode::Char('j') => self.selecting = Some((sel + 1).min(flat.len().saturating_sub(1))),
                                KeyCode::Enter => {
                                    if let Some((pool, a)) = flat.get(sel) {
                                        let id = a.get("id").and_then(Value::as_str).unwrap_or_default();
                                        if let Some((name, blocked)) = self.ctx.pools.get(pool).and_then(|m| m.switch_to(id)) {
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
                                self.message = Some(match self.prober.request_manual() {
                                    ManualProbe::Started => "probing all OAuth accounts…".into(),
                                    ManualProbe::AlreadyRunning => "probe already running".into(),
                                    ManualProbe::TooSoon { wait_secs } => format!("probed moments ago; try again in {wait_secs}s"),
                                });
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
            " Corrall v{}   current: {}   sessions: {} active / {} known   threshold: {}   probe: {}",
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
        // The release check is daemon-wide and records itself on the default
        // pool, which is what the status document's top level carries.
        let header = match st.get("updateAvailable").and_then(Value::as_str) {
            Some(tag) => format!("{header}   UPDATE {tag} available (corrall update)"),
            None => header,
        };
        f.render_widget(Paragraph::new(header).style(Style::default().bold()), chunks[0]);

        let flat = Self::flat_accounts(st);
        let multi = st.get("pools").and_then(Value::as_array).map(|p| p.len() > 1).unwrap_or(false);
        let mut rows: Vec<Row> = Vec::with_capacity(flat.len() + 2);
        let mut shown_pool: Option<&str> = None;
        for (i, (pool, a)) in flat.iter().enumerate() {
            // With more than one pool, head each block with its name so the
            // rotation each account competes in is obvious.
            if multi && shown_pool != Some(pool.as_str()) {
                shown_pool = Some(pool.as_str());
                let label = format!("pool {}", safe_text(pool, 24));
                rows.push(Row::new(vec![Cell::from(""), Cell::from(label)]).style(Style::default().fg(Color::Yellow).bold()));
            }
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
            rows.push(
                Row::new(vec![
                    Cell::from(if current { "►" } else { " " }),
                    Cell::from(name),
                    Cell::from(a.get("priority").and_then(Value::as_i64).unwrap_or(0).to_string()),
                    Cell::from(q("unified5h")),
                    Cell::from(q("unified7d")),
                    Cell::from(q("unified7dFable")),
                    Cell::from(status),
                ])
                .style(style),
            );
        }
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
