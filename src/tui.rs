//! Terminal dashboard: account table with quota bars and probe state, live
//! activity log with warnings picked out, and a few keys (q quit, R reload,
//! s switch, p probe). All text that reaches the screen came through
//! `safe_text` first.
//!
//! The facts come from a [`Backend`]: inside `corrall server` that is this
//! process's own pools and activity channel; under `corrall attach` it is a
//! running daemon's control API (see `attach.rs`). The drawing code does not
//! know which.

use std::collections::VecDeque;
use std::io::{Stdout, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use serde_json::Value;
use tracing_subscriber::fmt::MakeWriter;

use crate::attach::{Link, Remote};
use crate::prober::{ManualProbe, Prober};
use crate::proxy::server::{Activity, Ctx};
use crate::security::safe_text;
use crate::status::{bar, countdown, format_duration, ts as parse_ts};

/// How loudly a log line is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Info,
    Warn,
    Error,
}

/// Classify a log line. Lines forwarded from `tracing` (see
/// [`install_tracing`]) start with their level; pool-log lines that report a
/// 429, a refusal or a failure count as warnings so they stand out from
/// request traffic.
fn severity_of(line: &str) -> Severity {
    let t = line.trim_start();
    if t.starts_with("ERROR") {
        return Severity::Error;
    }
    if t.starts_with("WARN") {
        return Severity::Warn;
    }
    const WARN_WORDS: [&str; 6] = ["429", "failed", "rejected", "rate-limited", "exhausted", "not persisted"];
    if WARN_WORDS.iter().any(|w| t.contains(w)) {
        return Severity::Warn;
    }
    Severity::Info
}

/// The `probe` column for one account row: what the last probe said and how
/// long ago, in a few characters.
fn probe_cell(account: &Value, now: i64) -> String {
    let Some(p) = account.get("probe") else { return "never".into() };
    let status = p.get("status").and_then(Value::as_str).unwrap_or("never");
    let ago = parse_ts(p.get("lastProbedAt").or_else(|| p.get("startedAt"))).map(|t| format_duration((now - t).max(0))).unwrap_or_default();
    match status {
        "not-applicable" => "-".into(),
        "never" => "never".into(),
        "running" => "running".into(),
        "skipped" => "skipped".into(),
        "rate-limited" => format!("429 {ago}"),
        other => format!("{} {ago}", safe_text(other, 10)),
    }
}

/// Forward `tracing` events at WARN and above into the activity stream as
/// `Activity::Log` lines prefixed with their level, so an interactive run sees
/// what a headless one writes to the journal. `CORRALL_LOG` overrides the
/// level filter as it does for the stderr subscriber.
pub fn install_tracing(tx: tokio::sync::broadcast::Sender<Activity>) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("CORRALL_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let writer = ActivityWriter { tx, backlog: Arc::new(parking_lot::Mutex::new(Vec::new())) };
    let _ = fmt().with_env_filter(filter).with_writer(writer).without_time().with_target(false).with_ansi(false).compact().try_init();
}

/// Lines nobody was listening for yet (the TUI subscribes once it is up) are
/// kept, up to a point, and delivered ahead of the next line.
const BACKLOG_MAX: usize = 100;

#[derive(Clone)]
struct ActivityWriter {
    tx: tokio::sync::broadcast::Sender<Activity>,
    backlog: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl<'a> MakeWriter<'a> for ActivityWriter {
    type Writer = LineSink;
    fn make_writer(&'a self) -> LineSink {
        LineSink { out: self.clone(), buf: Vec::new() }
    }
}

impl ActivityWriter {
    fn emit(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut backlog = self.backlog.lock();
        if self.tx.receiver_count() == 0 {
            if backlog.len() < BACKLOG_MAX {
                backlog.push(line.to_string());
            }
            return;
        }
        for old in backlog.drain(..) {
            let _ = self.tx.send(Activity::Log(old));
        }
        let _ = self.tx.send(Activity::Log(line.to_string()));
    }
}

/// One formatted event; `tracing` writes it and drops the writer.
struct LineSink {
    out: ActivityWriter,
    buf: Vec<u8>,
}

impl LineSink {
    fn drain(&mut self) {
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        for line in text.lines() {
            self.out.emit(line);
        }
    }
}

impl Write for LineSink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.drain();
        Ok(())
    }
}

impl Drop for LineSink {
    fn drop(&mut self) {
        self.drain();
    }
}

struct InFlight {
    id: String,
    started: std::time::Instant,
    line: String,
    account: String,
}

/// A request row still waiting for its `End` after this long is one whose
/// end this process never saw (the attach feed reconnected under it); the
/// proxy's own idle timeouts end a real request well before.
const INFLIGHT_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// Where the dashboard's facts come from.
pub enum Backend {
    /// This process is the server.
    Local { ctx: Ctx, prober: Prober },
    /// A daemon elsewhere, over its control API.
    Remote(Remote),
}

impl Backend {
    /// The status document the table is drawn from.
    fn status(&self) -> Value {
        match self {
            Backend::Local { ctx, .. } => ctx.pools.status(false),
            Backend::Remote(r) => r.status(),
        }
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Activity> {
        match self {
            Backend::Local { ctx, .. } => ctx.activity.subscribe(),
            Backend::Remote(r) => r.subscribe(),
        }
    }

    /// The session column: its label and the width it is padded to. In
    /// process the titles cache answers; attached, the daemon sent the label
    /// with the event and the status document carries the width setting.
    fn session_label(&self, st: &Value, session: Option<&str>, label: Option<String>) -> (String, usize) {
        match self {
            Backend::Local { ctx, .. } => {
                let width = if ctx.titles.enabled() { ctx.titles.width() } else { 6 };
                (label.unwrap_or_else(|| ctx.titles.label(session, crate::quota::now_ms())), width)
            }
            Backend::Remote(_) => {
                let titles = st.get("sessionTitles");
                let enabled = titles.and_then(|t| t.get("enabled")).and_then(Value::as_bool).unwrap_or(false);
                let width = if enabled { titles.and_then(|t| t.get("width")).and_then(Value::as_u64).unwrap_or(18) as usize } else { 6 };
                (label.unwrap_or_else(|| session.map(|s| s.chars().take(6).collect()).unwrap_or_default()), width)
            }
        }
    }

    async fn switch(&self, pool: &str, id: &str) -> Result<Option<(String, Option<String>)>> {
        match self {
            Backend::Local { ctx, .. } => Ok(ctx.pools.get(pool).and_then(|m| m.switch_to(id))),
            Backend::Remote(r) => r.switch(pool, id).await.map(Some),
        }
    }

    /// `None` when there is nothing to reload (no reload hook).
    async fn reload(&self) -> Option<Result<usize>> {
        match self {
            Backend::Local { ctx, .. } => ctx.reload.as_ref().map(|f| f()),
            Backend::Remote(r) => Some(r.reload().await),
        }
    }

    async fn probe(&self) -> String {
        match self {
            Backend::Local { prober, .. } => match prober.request_manual() {
                ManualProbe::Started => "probing all OAuth accounts…".into(),
                ManualProbe::AlreadyRunning => "probe already running".into(),
                ManualProbe::TooSoon { wait_secs } => format!("probed moments ago; try again in {wait_secs}s"),
            },
            Backend::Remote(r) => match r.probe().await {
                Ok(m) => m,
                Err(e) => format!("probe failed: {}", safe_text(&e.to_string(), 80)),
            },
        }
    }

    /// The footer's "where": the address this process bound, or the daemon
    /// this one is attached to. `main` binds before the TUI starts, so the
    /// local answer is what is actually being served, not a hope.
    fn where_line(&self) -> String {
        match self {
            Backend::Local { ctx, .. } => {
                let cfg = ctx.config();
                match crate::proxy::server::parse_bind(&cfg.bind_host(), cfg.proxy.port) {
                    Ok(a) => format!("listening on http://{a}"),
                    Err(_) => format!("listening on http://{}:{}", safe_text(&cfg.bind_host(), 40), cfg.proxy.port),
                }
            }
            Backend::Remote(r) => format!("attached to {}", safe_text(r.base(), 60)),
        }
    }

    /// A warning for the header when the daemon has stopped answering.
    fn link_warning(&self) -> Option<String> {
        match self {
            Backend::Local { .. } => None,
            Backend::Remote(r) => match r.link() {
                Link::Fresh(_) => None,
                Link::Connecting => Some("connecting…".into()),
                Link::Lost { since, why } => {
                    Some(format!("DAEMON UNREACHABLE for {} ({}); showing last known state", format_duration(since.as_millis() as i64), why))
                }
            },
        }
    }
}

pub struct Tui {
    backend: Backend,
    log: VecDeque<(String, Severity)>,
    inflight: Vec<InFlight>,
    selecting: Option<usize>,
    message: Option<String>,
    activity_file: Option<std::fs::File>,
}

fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

impl Tui {
    pub fn new(backend: Backend, activity_file: Option<std::fs::File>) -> Tui {
        Tui { backend, log: VecDeque::new(), inflight: Vec::new(), selecting: None, message: None, activity_file }
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

    fn push_log(&mut self, line: String, severity: Severity) {
        let line = safe_text(&line, 300);
        if let Some(f) = &mut self.activity_file {
            let _ = writeln!(f, "{line}");
        }
        self.log.push_front((line, severity));
        while self.log.len() > 500 {
            self.log.pop_back();
        }
    }

    fn on_activity(&mut self, st: &Value, a: Activity) {
        match a {
            Activity::Start { id, method, path, model, session, client, label } => {
                let (mut sess, width) = self.backend.session_label(st, session.as_deref(), label);
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
                    let severity = if ok { Severity::Info } else { Severity::Warn };
                    self.push_log(
                        format!("{mark} {}  {} → {} {} ({:.1}s)", ts(), f.line, safe_text(&account, 40), status, elapsed_ms as f64 / 1000.0),
                        severity,
                    );
                }
            }
            Activity::Log(s) => {
                let severity = severity_of(&s);
                self.push_log(format!("  {}  {s}", ts()), severity);
            }
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
        let mut activity = self.backend.subscribe();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            let st = self.backend.status();
            self.inflight.retain(|r| r.started.elapsed() < INFLIGHT_MAX_AGE);
            terminal.draw(|f| self.draw(f, &st))?;
            tokio::select! {
                _ = tick.tick() => {}
                Ok(a) = activity.recv() => self.on_activity(&st, a),
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
                                        self.message = match self.backend.switch(pool, id).await {
                                            Ok(Some((name, None))) => Some(format!("switched to {}", safe_text(&name, 40))),
                                            Ok(Some((name, Some(b)))) => Some(format!("switched to {} (currently {})", safe_text(&name, 40), safe_text(&b, 40))),
                                            Ok(None) => None,
                                            Err(e) => Some(format!("switch failed: {}", safe_text(&e.to_string(), 80))),
                                        };
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
                                match self.backend.reload().await {
                                    Some(Ok(n)) => self.message = Some(format!("config reloaded ({n} added)")),
                                    Some(Err(e)) => self.message = Some(format!("reload failed: {}", safe_text(&e.to_string(), 80))),
                                    None => {}
                                }
                            }
                            KeyCode::Char('s') => self.selecting = Some(0),
                            KeyCode::Char('p') => self.message = Some(self.backend.probe().await),
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
            probe_summary(st, crate::quota::now_ms()),
        );
        // The release check is daemon-wide and records itself on the default
        // pool, which is what the status document's top level carries.
        let header = match st.get("updateAvailable").and_then(Value::as_str) {
            Some(tag) => format!("{header}   UPDATE {tag} available (corrall update)"),
            None => header,
        };
        let mut head = vec![Line::from(header).style(Style::default().bold())];
        if let Some(w) = self.backend.link_warning() {
            head.push(Line::from(format!(" {w}")).style(Style::default().fg(Color::Red).bold()));
        }
        f.render_widget(Paragraph::new(head), chunks[0]);

        let flat = Self::flat_accounts(st);
        let now = crate::quota::now_ms();
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
                    Cell::from(probe_cell(a, now)),
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
                Constraint::Length(12),
                Constraint::Min(10),
            ],
        )
        .header(Row::new(vec!["", "account", "pri", "5h", "7d", "7d fable", "probe", "state"]).style(Style::default().fg(Color::Yellow)))
        .block(Block::default().borders(Borders::ALL).title(" accounts "));
        f.render_widget(table, chunks[1]);

        let mut lines: Vec<Line> = Vec::new();
        for r in &self.inflight {
            let acct = if r.account.is_empty() { String::new() } else { format!(" → {}", r.account) };
            lines.push(
                Line::from(format!("⠋ {}  {}{} ({:.1}s…)", ts(), r.line, acct, r.started.elapsed().as_secs_f64())).style(Style::default().fg(Color::Cyan)),
            );
        }
        for (l, severity) in self.log.iter().take(chunks[2].height as usize) {
            let style = match severity {
                Severity::Info => Style::default(),
                Severity::Warn => Style::default().fg(Color::Yellow),
                Severity::Error => Style::default().fg(Color::Red).bold(),
            };
            lines.push(Line::from(l.clone()).style(style));
        }
        f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" activity ")), chunks[2]);

        let footer = match (&self.selecting, &self.message) {
            (Some(_), _) => " ↑/↓ choose account, Enter switch, Esc cancel".to_string(),
            (None, Some(m)) => format!(" {m}   |   q quit  R reload  s switch  p probe"),
            (None, None) => format!(" q quit   R reload config   s switch account   p probe quota   {}", self.backend.where_line()),
        };
        f.render_widget(Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)), chunks[3]);
    }
}

/// The header's probe summary: the default pool's interval (the status
/// document's top level), plus whether a run is on or when the next one is
/// due.
fn probe_summary(st: &Value, now: i64) -> String {
    let probe = st.get("probe");
    let interval = probe.and_then(|p| p.get("intervalSeconds")).and_then(Value::as_u64).unwrap_or(0);
    if interval == 0 {
        return "off".into();
    }
    if probe.and_then(|p| p.get("running")).and_then(Value::as_bool).unwrap_or(false) {
        return format!("{interval}s, running");
    }
    match parse_ts(probe.and_then(|p| p.get("nextRunAt"))) {
        Some(next) if next > now => format!("{interval}s, next {}", format_duration(next - now)),
        _ => format!("{interval}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn severity_from_tracing_prefix_and_pool_log_words() {
        assert_eq!(severity_of("WARN 429 on \"a\" (not a quota rejection)"), Severity::Warn);
        assert_eq!(severity_of("ERROR could not persist refreshed token"), Severity::Error);
        assert_eq!(severity_of("Quota probe for \"a\" failed: probe timed out"), Severity::Warn);
        assert_eq!(severity_of("Quota rejection (429) on \"a\"; throttling 60s"), Severity::Warn);
        assert_eq!(severity_of("Switched to account \"a\""), Severity::Info);
        assert_eq!(severity_of("Quota probe enabled for pool \"default\" (every 300s)"), Severity::Info);
    }

    #[test]
    fn probe_cell_reads_status_and_age() {
        let now = 1_000_000_000_000;
        let ok = json!({ "probe": { "status": "ok", "lastProbedAt": now - 180_000 } });
        assert_eq!(probe_cell(&ok, now), "ok 3m");
        let rl = json!({ "probe": { "status": "rate-limited", "lastProbedAt": now - 5_000 } });
        assert_eq!(probe_cell(&rl, now), "429 5s");
        assert_eq!(probe_cell(&json!({ "probe": { "status": "running", "startedAt": now } }), now), "running");
        assert_eq!(probe_cell(&json!({ "probe": { "status": "skipped" } }), now), "skipped");
        assert_eq!(probe_cell(&json!({ "probe": { "status": "not-applicable" } }), now), "-");
        assert_eq!(probe_cell(&json!({}), now), "never");
    }

    #[test]
    fn header_probe_summary() {
        let now = 1_000_000_000_000;
        assert_eq!(probe_summary(&json!({}), now), "off");
        assert_eq!(probe_summary(&json!({ "probe": { "intervalSeconds": 0, "running": true } }), now), "off");
        assert_eq!(probe_summary(&json!({ "probe": { "intervalSeconds": 300, "running": true } }), now), "300s, running");
        assert_eq!(probe_summary(&json!({ "probe": { "intervalSeconds": 300, "nextRunAt": now + 120_000 } }), now), "300s, next 2m");
        assert_eq!(probe_summary(&json!({ "probe": { "intervalSeconds": 300, "nextRunAt": now - 1 } }), now), "300s");
    }

    #[tokio::test]
    async fn tracing_lines_reach_the_activity_stream_with_their_level() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let w = ActivityWriter { tx, backlog: Arc::new(parking_lot::Mutex::new(Vec::new())) };
        // Nobody listening yet: kept, not dropped.
        drop(rx);
        {
            let mut sink = MakeWriter::make_writer(&w);
            sink.write_all(b"WARN early bird\n").unwrap();
        }
        let mut rx = w.tx.subscribe();
        {
            let mut sink = MakeWriter::make_writer(&w);
            sink.write_all(b"ERROR later\n").unwrap();
        }
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert!(matches!(first, Activity::Log(ref s) if s == "WARN early bird"), "{first:?}");
        assert!(matches!(second, Activity::Log(ref s) if s == "ERROR later"), "{second:?}");
        assert_eq!(severity_of("WARN early bird"), Severity::Warn);
    }
}
