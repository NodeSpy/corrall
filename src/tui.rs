//! Terminal dashboard: account table with quota bars and probe state, a
//! table of the sessions being served (which one, on which account, how busy
//! and how well cached), the activity log with warnings picked out, and a few
//! keys (q quit, R reload, p probe, s to pick an account and switch to it,
//! disable it or move its priority). All text that reaches the screen came
//! through `safe_text` first.
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
use serde_json::{json, Value};
use tracing_subscriber::fmt::MakeWriter;

use crate::attach::{Link, Remote};
use crate::prober::{ManualProbe, Prober};
use crate::proxy::server::{AccountChange, Activity, Ctx};
use crate::security::safe_text;
use crate::session_table::SessionCol;
use crate::status::{countdown, format_duration, gradient, ts as parse_ts};
use crate::tui_screens::{self as screens, Action, Confirm, Import, Input, Kind, LogView, Login, Screen, Setting, Settings};

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
    const WARN_WORDS: [&str; 7] = ["429", "failed", "rejected", "rate-limited", "exhausted", "not persisted", "login expires"];
    if WARN_WORDS.iter().any(|w| t.contains(w)) {
        return Severity::Warn;
    }
    Severity::Info
}

/// The `probe` column for one account row: what the last probe said and how
/// long ago, in a few characters.
/// Compact quota bar: filled cells run green → yellow → red across the width,
/// so a full bar ends in red.
pub(crate) fn bar(v: Option<f64>, width: usize) -> Vec<Span<'static>> {
    let Some(u) = v.filter(|u| u.is_finite()) else { return vec![Span::raw("·".repeat(width))] };
    let filled = (u.clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut spans: Vec<Span> = (0..filled)
        .map(|i| {
            let (r, g, b) = gradient(i, width);
            Span::styled("█", Style::default().fg(Color::Rgb(r, g, b)))
        })
        .collect();
    spans.push(Span::raw("░".repeat(width - filled)));
    spans
}

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
    /// Whether the request named a session, and so has a row in the table.
    in_table: bool,
}

/// A request row still waiting for its `End` after this long is one whose
/// end this process never saw (the attach feed reconnected under it); the
/// proxy's own idle timeouts end a real request well before.
const INFLIGHT_MAX_AGE: Duration = Duration::from_secs(30 * 60);

/// Where the dashboard's facts come from.
#[derive(Clone)]
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

    /// The sessions document the sessions table is drawn from. `Null` when
    /// the daemon attached to has no such route.
    fn sessions(&self) -> Value {
        match self {
            Backend::Local { ctx, .. } => ctx.pools.sessions(&ctx.titles, crate::proxy::server::SESSIONS_LIMIT),
            Backend::Remote(r) => r.sessions(),
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

    /// Enable or disable an account, or move its priority. Returns its name.
    async fn edit_account(&self, pool: &str, id: &str, change: AccountChange) -> Result<String> {
        match self {
            Backend::Local { ctx, .. } => crate::proxy::server::edit_account(ctx, pool, id, change),
            Backend::Remote(r) => r.edit_account(pool, id, change).await,
        }
    }

    /// Whether the config file on this machine is the server's own, so that
    /// editing it and asking for a reload is how to change the server. Not so
    /// for a dashboard attached to a server elsewhere (`attach --url`).
    fn owns_config(&self) -> bool {
        match self {
            Backend::Local { .. } => true,
            Backend::Remote(r) => r.owns_config(),
        }
    }

    /// Probe one account's quota now. The line to show either way.
    async fn probe_account(&self, pool: &str, id: &str) -> String {
        match self {
            Backend::Local { prober, .. } => match prober.request_account(pool, id) {
                ManualProbe::Started => "probing…".into(),
                ManualProbe::AlreadyRunning => "probe already running".into(),
                ManualProbe::TooSoon { wait_secs } => format!("probed moments ago; try again in {wait_secs}s"),
                ManualProbe::NotProbeable => "only an OAuth account has a quota to probe".into(),
            },
            Backend::Remote(r) => match r.probe_account(pool, id).await {
                Ok(m) => m,
                Err(e) => format!("probe failed: {}", safe_text(&e.to_string(), 80)),
            },
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
                ManualProbe::NotProbeable => "nothing to probe".into(),
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

/// What a job running off the event loop has to say.
enum Done {
    /// A line for the footer.
    Say(String),
    /// What the login or import screen was waiting for has ended.
    Finished { ok: bool, message: String },
    /// The login's sign-in page is ready to be shown; `opened` is whether a
    /// browser was started on it.
    LoginUrl { url: String, opened: bool },
    /// Where the login stands, or what was wrong with what was pasted.
    LoginNote { text: String, failed: bool },
}

/// Lines the log keeps, newest first.
const LOG_MAX: usize = 2000;

pub struct Tui {
    backend: Backend,
    log: VecDeque<(String, Severity)>,
    inflight: Vec<InFlight>,
    /// The account row the cursor is on.
    cursor: usize,
    screen: Screen,
    /// A question waiting for y or n, over whatever screen asked it.
    confirm: Option<Confirm>,
    message: Option<String>,
    activity_file: Option<std::fs::File>,
    done_tx: tokio::sync::mpsc::UnboundedSender<Done>,
    done_rx: tokio::sync::mpsc::UnboundedReceiver<Done>,
}

fn ts() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn severity_style(severity: Severity) -> Style {
    match severity {
        Severity::Info => Style::default(),
        Severity::Warn => Style::default().fg(Color::Yellow),
        Severity::Error => Style::default().fg(Color::Red).bold(),
    }
}

impl Tui {
    pub fn new(backend: Backend, activity_file: Option<std::fs::File>) -> Tui {
        let (done_tx, done_rx) = tokio::sync::mpsc::unbounded_channel();
        Tui {
            backend,
            log: VecDeque::new(),
            inflight: Vec::new(),
            cursor: 0,
            screen: Screen::Dashboard,
            confirm: None,
            message: None,
            activity_file,
            done_tx,
            done_rx,
        }
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
        while self.log.len() > LOG_MAX {
            self.log.pop_back();
        }
        // A reader scrolled back keeps looking at the same lines.
        if let Screen::Log(view) = &mut self.screen {
            if view.offset > 0 && (!view.problems_only || severity != Severity::Info) {
                view.offset = (view.offset + 1).min(LOG_MAX);
            }
        }
    }

    fn on_activity(&mut self, st: &Value, a: Activity) {
        match a {
            Activity::Start { id, method, path, model, session, client, label, bytes, .. } => {
                let (mut sess, width) = self.backend.session_label(st, session.as_deref(), label);
                if sess.chars().count() > width {
                    sess = sess.chars().take(width).collect();
                }
                let sess = format!("{sess:<width$}");
                let who = client.map(|c| format!("[{c}] ")).unwrap_or_default();
                // The size beside the model: megabytes a turn is the sign of
                // a session started without deferred tool loading.
                let model = match model {
                    Some(m) => format!(" ({m}, {})", crate::proxy::server::body_size(bytes)),
                    None => String::new(),
                };
                let line = format!("{who}{sess} {method} {path}{model}");
                self.inflight.push(InFlight {
                    id,
                    started: std::time::Instant::now(),
                    line: safe_text(&line, 200),
                    account: String::new(),
                    in_table: session.is_some(),
                });
            }
            Activity::Account { id, account } => {
                if let Some(f) = self.inflight.iter_mut().find(|f| f.id == id) {
                    f.account = safe_text(&account, 40);
                }
            }
            Activity::End { id, account, status, elapsed_ms, ok, tokens } => {
                if let Some(pos) = self.inflight.iter().position(|f| f.id == id) {
                    let f = self.inflight.remove(pos);
                    let mark = if ok { "✓" } else { "✗" };
                    let severity = if ok { Severity::Info } else { Severity::Warn };
                    let used = tokens.map(|t| format!("  {}", t.note())).unwrap_or_default();
                    self.push_log(
                        format!("{mark} {}  {} → {} {} ({:.1}s){used}", ts(), f.line, safe_text(&account, 40), status, elapsed_ms as f64 / 1000.0),
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
        // A paste then arrives whole, instead of as keys with an Enter in it.
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
        let res = self.event_loop(&mut terminal, &mut shutdown_tx).await;
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
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
            let sessions = self.backend.sessions();
            self.inflight.retain(|r| r.started.elapsed() < INFLIGHT_MAX_AGE);
            self.cursor = self.cursor.min(Self::flat_accounts(&st).len().saturating_sub(1));
            terminal.draw(|f| self.draw(f, &st, &sessions))?;
            tokio::select! {
                _ = tick.tick() => {}
                Ok(a) = activity.recv() => self.on_activity(&st, a),
                Some(done) = self.done_rx.recv() => self.on_done(done),
                Some(Ok(ev)) = events.next() => match ev {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        if self.on_key(k.code, k.modifiers, &st).await {
                            let _ = shutdown.send(true);
                            return Ok(());
                        }
                    }
                    Event::Paste(text) => self.on_paste(&text),
                    _ => {}
                },
            }
        }
    }

    /// Feed one key press, as the event loop would. Returns whether it asked
    /// to quit. With [`Self::screen_text`], this is how tests drive the
    /// dashboard without a terminal.
    #[doc(hidden)]
    pub async fn press(&mut self, code: KeyCode) -> bool {
        let st = self.backend.status();
        self.cursor = self.cursor.min(Self::flat_accounts(&st).len().saturating_sub(1));
        while let Ok(done) = self.done_rx.try_recv() {
            self.on_done(done);
        }
        self.on_key(code, KeyModifiers::NONE, &st).await
    }

    /// The screen as it would be drawn on a `width` by `height` terminal.
    #[doc(hidden)]
    pub fn screen_text(&self, width: u16, height: u16) -> String {
        let (st, sessions) = (self.backend.status(), self.backend.sessions());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).expect("an in-memory terminal");
        terminal.draw(|f| self.draw(f, &st, &sessions)).expect("drawing to memory");
        let buf = terminal.backend().buffer().clone();
        (0..height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    /// Text pasted into whichever line is open for typing.
    fn on_paste(&mut self, text: &str) {
        match &mut self.screen {
            Screen::Login(l) => l.input.paste(text),
            Screen::Settings(s) => {
                if let Some(input) = &mut s.editing {
                    input.paste(text);
                }
            }
            _ => {}
        }
    }

    fn on_done(&mut self, done: Done) {
        match done {
            Done::Say(m) => {
                self.message = Some(m);
                // The settings screen shows what a job may just have changed.
                if let Screen::Settings(s) = &mut self.screen {
                    s.cfg = crate::config::Config::load().ok().flatten();
                    s.service = crate::service::state();
                }
            }
            Done::LoginUrl { url, opened } => {
                if let Screen::Login(l) = &mut self.screen {
                    (l.url, l.opened) = (Some(url), opened);
                    l.note = "waiting for the browser to come back…".into();
                }
            }
            Done::LoginNote { text, failed } => {
                if let Screen::Login(l) = &mut self.screen {
                    (l.note, l.failed) = (text, failed);
                }
            }
            Done::Finished { ok: true, message } => {
                self.message = Some(message);
                if matches!(self.screen, Screen::Login(_) | Screen::Import(_)) {
                    self.screen = Screen::Dashboard;
                }
            }
            Done::Finished { ok: false, message } => match &mut self.screen {
                Screen::Login(l) => (l.note, l.failed) = (message, true),
                Screen::Import(i) => (i.busy, i.failed) = (false, Some(message)),
                _ => self.message = Some(message),
            },
        }
    }

    /// One key press. Returns whether to quit.
    async fn on_key(&mut self, code: KeyCode, modifiers: KeyModifiers, st: &Value) -> bool {
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return true;
        }
        // What was said stays until the next key, which may say something new.
        self.message = None;
        if let Some(confirm) = self.confirm.take() {
            if matches!(code, KeyCode::Char('y' | 'Y')) {
                self.act(confirm.then).await;
            }
            return false;
        }
        let flat = Self::flat_accounts(st);
        let picked = flat.get(self.cursor).map(|(pool, a)| (pool.as_str(), *a));
        // Each screen hands back the screen to show next, or `None` to quit.
        let next = match std::mem::replace(&mut self.screen, Screen::Dashboard) {
            Screen::Dashboard => self.key_dashboard(code, picked, flat.len()).await,
            Screen::Detail => Some(self.key_detail(code, picked, flat.len()).await),
            Screen::Log(view) => Some(self.key_log(code, view)),
            Screen::Settings(s) => Some(self.key_settings(code, s).await),
            Screen::Login(l) => Some(Self::key_login(code, l)),
            Screen::Import(i) => Some(self.key_import(code, i)),
        };
        match next {
            Some(screen) => {
                self.screen = screen;
                false
            }
            None => true,
        }
    }

    fn move_cursor(&mut self, code: KeyCode, rows: usize) -> bool {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(rows.saturating_sub(1)),
            _ => return false,
        }
        true
    }

    /// The keys that act on the account under the cursor, on the dashboard
    /// and in its detail view alike. Returns whether the key was one of them.
    async fn account_key(&mut self, code: KeyCode, picked: Option<(&str, &Value)>) -> bool {
        let KeyCode::Char(c @ ('s' | 'd' | '+' | '=' | '-' | '_')) = code else { return false };
        let Some((pool, a)) = picked else { return true };
        let id = a.get("id").and_then(Value::as_str).unwrap_or_default();
        self.message = Some(if c == 's' {
            match self.backend.switch(pool, id).await {
                Ok(Some((name, None))) => format!("switched to {}", safe_text(&name, 40)),
                Ok(Some((name, Some(why)))) => format!("switched to {} (currently {})", safe_text(&name, 40), safe_text(&why, 40)),
                Ok(None) => return true,
                Err(e) => format!("switch failed: {}", safe_text(&e.to_string(), 80)),
            }
        } else {
            let change = account_change(a, c);
            match self.backend.edit_account(pool, id, change).await {
                Ok(name) => match change {
                    AccountChange::Enable => format!("enabled {}", safe_text(&name, 40)),
                    AccountChange::Disable => format!("disabled {}", safe_text(&name, 40)),
                    AccountChange::Priority(p) => format!("{} now has priority {p}", safe_text(&name, 40)),
                },
                Err(e) => format!("change failed: {}", safe_text(&e.to_string(), 80)),
            }
        });
        true
    }

    /// Whether this dashboard may change the server's config; says why not.
    fn may_configure(&mut self) -> bool {
        let owns = self.backend.owns_config();
        if !owns {
            self.message = Some("this dashboard is attached to a server on another machine; accounts and settings are changed there".into());
        }
        owns
    }

    async fn key_dashboard(&mut self, code: KeyCode, picked: Option<(&str, &Value)>, rows: usize) -> Option<Screen> {
        if self.move_cursor(code, rows) || self.account_key(code, picked).await {
            return Some(Screen::Dashboard);
        }
        Some(match code {
            KeyCode::Char('q') => return None,
            KeyCode::Enter if picked.is_some() => Screen::Detail,
            KeyCode::Char('R' | 'r') => {
                match self.backend.reload().await {
                    Some(Ok(n)) => self.message = Some(format!("config reloaded ({n} added)")),
                    Some(Err(e)) => self.message = Some(format!("reload failed: {}", safe_text(&e.to_string(), 80))),
                    None => {}
                }
                Screen::Dashboard
            }
            KeyCode::Char('p') => {
                self.message = Some(self.backend.probe().await);
                Screen::Dashboard
            }
            KeyCode::Char('l') => Screen::Log(LogView::default()),
            KeyCode::Char('g') if self.may_configure() => Screen::Settings(Box::new(Settings::open())),
            KeyCode::Char('a') if self.may_configure() => self.start_login(None),
            KeyCode::Char('i') if self.may_configure() => {
                let files = dirs::home_dir().map(|h| screens::credential_files(&h)).unwrap_or_default();
                // Every file listed is one a Claude Code keeps refreshing, so
                // linking is where the choice starts.
                Screen::Import(Import { files, cursor: 0, link: true, busy: false, failed: None })
            }
            _ => Screen::Dashboard,
        })
    }

    async fn key_detail(&mut self, code: KeyCode, picked: Option<(&str, &Value)>, rows: usize) -> Screen {
        if self.move_cursor(code, rows) || self.account_key(code, picked).await {
            return Screen::Detail;
        }
        let Some((pool, a)) = picked else { return Screen::Dashboard };
        let text = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Enter => return Screen::Dashboard,
            KeyCode::Char('p') => self.message = Some(self.backend.probe_account(pool, &text("id")).await),
            KeyCode::Char('x') if self.may_configure() => {
                self.confirm = Some(Confirm {
                    question: format!("Remove {} from pool {pool}?", text("name")),
                    detail:
                        "Its entry and its tokens are taken out of the config file. A login it was linked to (a Claude Code credentials file) is left as it is."
                            .into(),
                    then: Action::RemoveAccount { pool: pool.to_string(), id: text("id") },
                });
            }
            KeyCode::Char('L') if a.get("type").and_then(Value::as_str) != Some("oauth") => {
                self.message = Some("an API-key account has no login; `corrall login --api` replaces its key".into());
            }
            KeyCode::Char('L') if self.may_configure() => return self.start_login(Some(text("name"))),
            _ => {}
        }
        Screen::Detail
    }

    fn key_log(&mut self, code: KeyCode, mut view: LogView) -> Screen {
        let shown = self.log.iter().filter(|(_, s)| !view.problems_only || *s != Severity::Info).count();
        match code {
            KeyCode::Esc | KeyCode::Char('q' | 'l') => return Screen::Dashboard,
            // Newest is at the top, so down is back in time.
            KeyCode::Down | KeyCode::Char('j') => view.offset += 1,
            KeyCode::Up | KeyCode::Char('k') => view.offset = view.offset.saturating_sub(1),
            KeyCode::PageDown | KeyCode::Char(' ') => view.offset += 20,
            KeyCode::PageUp => view.offset = view.offset.saturating_sub(20),
            KeyCode::Home => view.offset = 0,
            KeyCode::End => view.offset = shown,
            KeyCode::Char('w') => {
                view.problems_only = !view.problems_only;
                view.offset = 0;
            }
            _ => {}
        }
        view.offset = view.offset.min(shown.saturating_sub(1));
        Screen::Log(view)
    }

    async fn key_settings(&mut self, code: KeyCode, mut s: Box<Settings>) -> Screen {
        let row = s.row();
        if let Some(mut input) = s.editing.take() {
            match code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let pool = s.pool.clone();
                    self.save_setting(&mut s, |c| row.set(c, &pool, &input.text)).await;
                    // What was typed stays open when it was refused.
                    if s.error.is_some() {
                        s.editing = Some(input);
                    }
                }
                KeyCode::Backspace => {
                    input.backspace();
                    s.editing = Some(input);
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    s.editing = Some(input);
                }
                _ => s.editing = Some(input),
            }
            return Screen::Settings(s);
        }
        match code {
            KeyCode::Esc | KeyCode::Char('q' | 'g') => return Screen::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => s.cursor = s.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => s.cursor = (s.cursor + 1).min(Setting::ALL.len() - 1),
            KeyCode::Enter | KeyCode::Char(' ') => {
                s.error = None;
                let Some(cfg) = &s.cfg else { return Screen::Settings(s) };
                match (row, row.kind()) {
                    (Setting::Pool, _) => s.next_pool(),
                    (_, Kind::Cycle) => {
                        let pool = s.pool.clone();
                        self.save_setting(&mut s, |c| row.step(c, &pool)).await;
                    }
                    (_, Kind::Edit) => s.editing = Some(Input::with(&row.editable(cfg, &s.pool))),
                    (_, Kind::Run) => self.confirm = Some(Self::question(row, cfg, &self.backend, s.service)),
                }
            }
            _ => {}
        }
        Screen::Settings(s)
    }

    /// Write one change to the config file and have the server take it up.
    /// A refused change lands in `s.error` and nothing is written.
    async fn save_setting(&mut self, s: &mut Settings, change: impl FnOnce(&mut crate::config::Config) -> Result<()>) {
        match crate::config::Config::update(change) {
            Ok(cfg) => {
                s.cfg = Some(cfg);
                s.error = None;
                self.message = Some(match self.backend.reload().await {
                    Some(Err(e)) => format!("saved, but the server did not reload: {}", safe_text(&e.to_string(), 80)),
                    _ => "saved".into(),
                });
            }
            Err(e) => s.error = Some(e.to_string()),
        }
    }

    /// What to ask before a settings row that runs something.
    fn question(row: Setting, cfg: &crate::config::Config, backend: &Backend, service: crate::service::State) -> Confirm {
        use crate::service::{Kind as Manager, State};
        let base = crate::cli::proxy_base(cfg);
        let (question, detail, then) = match row {
            Setting::UnpointClaude => (
                "Take the proxy out of Claude Code's settings?".to_string(),
                "Removes what setup wrote from ~/.claude/settings.json (a backup is kept). Claude Code then talks to Anthropic directly again.".to_string(),
                Action::PointClaude { remove: true },
            ),
            Setting::Service => {
                let manager = match Manager::native() {
                    Some(Manager::Systemd) => "a systemd --user unit",
                    Some(Manager::Launchd) => "a launchd agent",
                    None => "a service",
                };
                // A server already holds the port: this one, or the one this
                // dashboard is attached to.
                let detail = match (service, backend) {
                    (State::Running, _) => format!("It is already running as a service. Installing again rewrites {manager} for this binary and config and restarts it."),
                    (_, Backend::Local { .. }) => format!("Writes {manager} that starts the proxy at every login. This terminal's server holds the port, so the service takes over once you quit here."),
                    (_, Backend::Remote(_)) => format!("Writes {manager} that starts the proxy at every login. The server you are attached to was started by hand and holds the port: the service takes over once that one is stopped."),
                };
                ("Install the proxy as a service?".to_string(), detail, Action::InstallService)
            }
            _ => (
                format!("Point Claude Code at {base}?"),
                "Writes the proxy into ~/.claude/settings.json and removes any other gateway, forward proxy, cloud provider or key helper set there (a backup is kept). Restart running Claude Code sessions afterwards.".to_string(),
                Action::PointClaude { remove: false },
            ),
        };
        Confirm { question, detail, then }
    }

    fn key_login(code: KeyCode, mut l: Login) -> Screen {
        match code {
            KeyCode::Esc => {
                // Ends the listener and forgets the half-made login.
                l.task.abort();
                return Screen::Dashboard;
            }
            KeyCode::Enter if !l.input.text.trim().is_empty() => {
                let _ = l.pasted.send(std::mem::take(&mut l.input.text));
                (l.note, l.failed) = ("exchanging the code…".into(), false);
            }
            KeyCode::Backspace => l.input.backspace(),
            KeyCode::Char(c) => l.input.push(c),
            _ => {}
        }
        Screen::Login(l)
    }

    fn key_import(&mut self, code: KeyCode, mut i: Import) -> Screen {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => return Screen::Dashboard,
            _ if i.busy || i.files.is_empty() => {}
            KeyCode::Up | KeyCode::Char('k') => i.cursor = i.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => i.cursor = (i.cursor + 1).min(i.files.len().saturating_sub(1)),
            KeyCode::Tab => i.link = !i.link,
            KeyCode::Enter => {
                if let Some(file) = i.files.get(i.cursor).cloned() {
                    (i.busy, i.failed) = (true, None);
                    let (backend, tx, link) = (self.backend.clone(), self.done_tx.clone(), i.link);
                    tokio::spawn(async move {
                        let done = match crate::cli::import_login(&file, link, None, None).await {
                            Ok((_, lines)) => {
                                let _ = backend.reload().await;
                                Done::Finished { ok: true, message: lines.first().cloned().unwrap_or_default() }
                            }
                            Err(e) => Done::Finished { ok: false, message: format!("import failed: {}", safe_text(&format!("{e:#}"), 200)) },
                        };
                        let _ = tx.send(done);
                    });
                }
            }
            _ => {}
        }
        Screen::Import(i)
    }

    /// Open the login screen and start a browser login behind it. `again` is
    /// the account being logged in again, for the screen's title: whichever
    /// account the browser signs in as is the one that is saved.
    fn start_login(&mut self, again: Option<String>) -> Screen {
        let (pasted, mut pastes) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (backend, tx) = (self.backend.clone(), self.done_tx.clone());
        let task = tokio::spawn(async move {
            let say = |text: String, failed: bool| {
                let _ = tx.send(Done::LoginNote { text, failed });
            };
            let mut login = match crate::oauth::start_browser_login().await {
                Ok(l) => l,
                Err(e) => return say(format!("could not start a login: {e:#}"), true),
            };
            // Only where there is a desktop to open it on: over SSH the same
            // call can start a text browser on the terminal this dashboard
            // is drawn in.
            let url = login.url.clone();
            let opened = desktop_session() && tokio::task::spawn_blocking(move || webbrowser::open(&url).is_ok()).await.unwrap_or(false);
            let _ = tx.send(Done::LoginUrl { url: login.url.clone(), opened });
            // Whichever comes first: the browser's redirect, or a paste.
            let mut redirect = std::pin::pin!(login.redirect());
            let mut listening = true;
            let tokens = loop {
                let came_back = tokio::select! {
                    code = &mut redirect, if listening => {
                        listening = false;
                        code
                    }
                    Some(text) = pastes.recv() => Ok(text),
                    else => return,
                };
                let exchanged = match came_back {
                    Ok(code) => login.finish(&code).await,
                    Err(e) => Err(e),
                };
                match exchanged {
                    Ok(tokens) => break tokens,
                    Err(e) => say(format!("{} (paste the address again, or Esc to give up)", safe_text(&format!("{e:#}"), 200)), true),
                }
            };
            say("signed in; saving the account…".into(), false);
            let done = match crate::cli::save_login(&tokens, None, None).await {
                Ok((_, lines)) => {
                    let _ = backend.reload().await;
                    Done::Finished { ok: true, message: lines.last().cloned().unwrap_or_default() }
                }
                Err(e) => Done::Finished { ok: false, message: format!("could not save the login: {}", safe_text(&format!("{e:#}"), 200)) },
            };
            let _ = tx.send(done);
        });
        Screen::Login(Login { again, url: None, opened: false, input: Input::default(), note: "starting…".into(), failed: false, pasted, task })
    }

    /// Do what a question was answered `y` to.
    async fn act(&mut self, action: Action) {
        match action {
            Action::RemoveAccount { pool, id } => {
                self.message = Some(match crate::cli::remove_account(Some(&pool), &id) {
                    Ok((_, name, pool)) => {
                        let _ = self.backend.reload().await;
                        self.screen = Screen::Dashboard;
                        format!("removed {} from pool {}", safe_text(&name, 40), safe_text(&pool, 24))
                    }
                    Err(e) => format!("remove failed: {}", safe_text(&e.to_string(), 120)),
                });
            }
            Action::PointClaude { remove } => {
                let said = crate::config::Config::load()
                    .and_then(|c| c.ok_or_else(|| anyhow::anyhow!("no config yet")))
                    .and_then(|c| crate::cli::point_claude_code(&c, remove));
                self.message = Some(match said {
                    Ok(m) => m,
                    Err(e) => format!("settings.json was not changed: {}", safe_text(&format!("{e:#}"), 160)),
                });
            }
            Action::InstallService => {
                self.message = Some("installing the service…".into());
                let tx = self.done_tx.clone();
                // Off the event loop: installing waits on the service manager.
                tokio::task::spawn_blocking(move || {
                    let _ = tx.send(Done::Say(match crate::service::install() {
                        Ok(path) => format!("service installed: {}", path.display()),
                        Err(e) => format!("service install failed: {}", safe_text(&format!("{e:#}"), 200)),
                    }));
                });
            }
        }
    }

    fn draw(&self, f: &mut Frame, st: &Value, sessions: &Value) {
        let area = f.area();
        // Header; the screen; then a line for what was last said and a line of keys.
        let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(2)]).split(area);
        self.draw_header(f, chunks[0], st);
        match &self.screen {
            Screen::Dashboard => self.draw_dashboard(f, chunks[1], st, sessions),
            Screen::Detail => self.draw_detail(f, chunks[1], st, sessions),
            Screen::Log(view) => {
                let rows = chunks[1].height.saturating_sub(2) as usize;
                let wanted = |severity: &Severity| !view.problems_only || *severity != Severity::Info;
                let total = self.log.iter().filter(|(_, s)| wanted(s)).count();
                let lines: Vec<(String, Style)> =
                    self.log.iter().filter(|(_, s)| wanted(s)).skip(view.offset).take(rows).map(|(l, s)| (l.clone(), severity_style(*s))).collect();
                screens::draw_log(f, chunks[1], &lines, view, total);
            }
            Screen::Settings(s) => screens::draw_settings(f, chunks[1], s),
            Screen::Login(l) => screens::draw_login(f, chunks[1], l),
            Screen::Import(i) => screens::draw_import(f, chunks[1], i),
        }
        // The most-wanted keys first: a narrow terminal cuts the line short.
        let keys = match &self.screen {
            Screen::Dashboard => "↑↓ account  Enter detail  a add  i import  l log  g settings  q quit  s switch  d disable  +/- priority  p probe  R reload",
            Screen::Detail => "Esc back  ↑↓ other accounts  s switch  d disable/enable  +/- priority  p probe  L log in again  x remove",
            Screen::Log(_) => "Esc back  ↓↑ older/newer  PgDn/PgUp a page  Home newest  End oldest  w warnings only",
            Screen::Settings(s) if s.editing.is_some() => "Enter save  Esc cancel",
            Screen::Settings(_) => "Esc back  ↑↓ row  Enter change, edit or run",
            Screen::Login(_) => "Esc cancel  Enter submit what you pasted",
            Screen::Import(_) => "Esc back  ↑↓ choose  Tab link or copy  Enter import",
        };
        let keys = match &self.screen {
            Screen::Dashboard => format!(" {keys}   {}", self.backend.where_line()),
            _ => format!(" {keys}"),
        };
        let said = Line::from(format!(" {}", self.message.as_deref().unwrap_or_default())).style(Style::default().fg(Color::White));
        f.render_widget(Paragraph::new(vec![said, Line::from(keys).style(Style::default().fg(Color::DarkGray))]), chunks[2]);
        if let Some(confirm) = &self.confirm {
            screens::draw_confirm(f, chunks[1], confirm);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect, st: &Value) {
        let counts = st.get("sessions");
        let header = format!(
            " Corrall v{}   current: {}   routing: {}   sessions: {} active / {} known   threshold: {}   probe: {}",
            st.get("version").and_then(Value::as_str).unwrap_or("?"),
            st.get("current").and_then(Value::as_str).unwrap_or("-"),
            routing_summary(st),
            counts.and_then(|s| s.get("active")).and_then(Value::as_u64).unwrap_or(0),
            counts.and_then(|s| s.get("known")).and_then(Value::as_u64).unwrap_or(0),
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
        f.render_widget(Paragraph::new(head), area);
    }

    /// The account under the cursor: every fact the status document has on
    /// it, the sessions it is serving, and the log lines that name it.
    fn draw_detail(&self, f: &mut Frame, area: Rect, st: &Value, sessions: &Value) {
        let flat = Self::flat_accounts(st);
        let Some((pool, a)) = flat.get(self.cursor) else {
            f.render_widget(Paragraph::new(" no account selected").block(Block::default().borders(Borders::ALL)), area);
            return;
        };
        let now = crate::quota::now_ms();
        let name = a.get("name").and_then(Value::as_str).unwrap_or("?");
        let multi = st.get("pools").and_then(Value::as_array).map(|p| p.len() > 1).unwrap_or(false);
        let facts = screens::account_facts(a, pool, now);
        let mine: Vec<Value> = sessions
            .get("sessions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|s| s.get("account").and_then(Value::as_str) == Some(name) && s.get("pool").and_then(Value::as_str) == Some(pool.as_str()))
            .cloned()
            .collect();
        let active = mine.iter().filter(|s| s.get("active").and_then(Value::as_bool).unwrap_or(false)).count();
        let doc = json!({ "active": active, "known": mine.len(), "names": sessions.get("names"), "sessions": [] });
        let sessions_h = (mine.len().max(1) as u16 + 3).min(area.height / 3);
        let chunks = Layout::vertical([Constraint::Length(facts.len() as u16 + 2), Constraint::Length(sessions_h), Constraint::Min(0)]).split(area);
        f.render_widget(Paragraph::new(facts).block(Block::default().borders(Borders::ALL).title(format!(" {} ", safe_text(name, 60)))), chunks[0]);
        f.render_widget(sessions_table(&doc, &mine, chunks[1], multi, now), chunks[1]);
        let lines: Vec<Line> = self
            .log
            .iter()
            .filter(|(l, _)| l.contains(name))
            .take(chunks[2].height.saturating_sub(2) as usize)
            .map(|(l, severity)| Line::from(l.clone()).style(severity_style(*severity)))
            .collect();
        f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" log lines naming this account ")), chunks[2]);
    }

    fn draw_dashboard(&self, f: &mut Frame, area: Rect, st: &Value, sessions: &Value) {
        let flat = Self::flat_accounts(st);
        let multi = st.get("pools").and_then(Value::as_array).map(|p| p.len() > 1).unwrap_or(false);
        let pools = if multi { st.get("pools").and_then(Value::as_array).map(Vec::len).unwrap_or(0) } else { 0 };
        let session_rows = sessions.get("sessions").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let (accounts_h, sessions_h) = pane_heights(area.height, flat.len() + pools, session_rows.len());
        let chunks = Layout::vertical([Constraint::Length(accounts_h), Constraint::Length(sessions_h), Constraint::Min(0)]).split(area);

        let now = crate::quota::now_ms();
        let shape = AccountsShape::fitting(area.width);
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
                let mut spans = bar(u, shape.bar as usize);
                spans.push(Span::raw(format!(" {} {}", u.map(|x| format!("{:>3.0}%", x * 100.0)).unwrap_or("  ?%".into()), countdown(r))));
                Line::from(spans)
            };
            let blocked = a.get("blocked").and_then(Value::as_str);
            let login = crate::status::login_expiry(a, now);
            let status = if a.get("disabled").and_then(Value::as_bool).unwrap_or(false) {
                "disabled".into()
            } else if let Some(b) = blocked {
                safe_text(b, 30)
            } else if let Some(left) = login.filter(|l| *l > 0) {
                format!("ready, login expires {}", format_duration(left))
            } else {
                "ready".into()
            };
            let style = if i == self.cursor {
                Style::default().bg(Color::Blue).fg(Color::White)
            } else if current {
                Style::default().fg(Color::Green).bold()
            } else if blocked.is_some() {
                Style::default().fg(Color::DarkGray)
            } else if login.is_some() {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            let mut cells = vec![
                Cell::from(if current { "►" } else { " " }),
                Cell::from(name),
                Cell::from(a.get("priority").and_then(Value::as_i64).unwrap_or(0).to_string()),
                Cell::from(q("unified5h")),
                Cell::from(q("unified7d")),
            ];
            if shape.fable {
                cells.push(Cell::from(q("unified7dFable")));
            }
            if shape.probe {
                cells.push(Cell::from(probe_cell(a, now)));
            }
            cells.push(Cell::from(status));
            rows.push(Row::new(cells).style(style));
        }
        let (mut head, mut widths) = (
            vec!["", "account", "pri", "5h", "7d"],
            vec![
                Constraint::Length(1),
                Constraint::Length(shape.name),
                Constraint::Length(3),
                Constraint::Length(shape.quota),
                Constraint::Length(shape.quota),
            ],
        );
        if shape.fable {
            head.push("7d fable");
            widths.push(Constraint::Length(shape.quota));
        }
        if shape.probe {
            head.push("probe");
            widths.push(Constraint::Length(12));
        }
        head.push("state");
        widths.push(Constraint::Min(8));
        let table = Table::new(rows, widths)
            .header(Row::new(head).style(Style::default().fg(Color::Yellow)))
            .block(Block::default().borders(Borders::ALL).title(" accounts "));
        f.render_widget(table, chunks[0]);

        f.render_widget(sessions_table(sessions, session_rows, chunks[1], multi, now), chunks[1]);

        // A request that named a session shows as its row above; only the
        // rest (other clients, a daemon with no sessions route) are listed.
        let listed = sessions.get("sessions").is_some();
        let mut lines: Vec<Line> = Vec::new();
        for r in self.inflight.iter().filter(|r| !(listed && r.in_table)) {
            let acct = if r.account.is_empty() { String::new() } else { format!(" → {}", r.account) };
            lines.push(
                Line::from(format!("⠋ {}  {}{} ({:.1}s…)", ts(), r.line, acct, r.started.elapsed().as_secs_f64())).style(Style::default().fg(Color::Cyan)),
            );
        }
        for (l, severity) in self.log.iter().take(chunks[2].height as usize) {
            lines.push(Line::from(l.clone()).style(severity_style(*severity)));
        }
        f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" activity ")), chunks[2]);
    }
}

/// How the accounts table is laid out in the width it has. At full width
/// every column shows; as it narrows the Fable window goes, then the probe
/// column, then the quota bars shorten and the name gives up what is left,
/// so that an account can still be told from its neighbour at 80 columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AccountsShape {
    name: u16,
    /// Cells in a quota bar, and the width of the column it sits in.
    bar: u16,
    quota: u16,
    fable: bool,
    probe: bool,
}

impl AccountsShape {
    fn fitting(width: u16) -> AccountsShape {
        // Borders, mark, priority, state and the gaps between columns.
        let fixed = |columns: u16| 2 + 1 + 3 + 10 + (columns - 1);
        let full = AccountsShape { name: 28, bar: 10, quota: 24, fable: true, probe: true };
        if width >= fixed(8) + 28 + 24 * 3 + 12 {
            full
        } else if width >= fixed(7) + 28 + 24 * 2 + 12 {
            AccountsShape { fable: false, ..full }
        } else if width >= fixed(6) + 28 + 24 * 2 {
            AccountsShape { fable: false, probe: false, ..full }
        } else {
            let quota = 19;
            let name = width.saturating_sub(fixed(6) + quota * 2).clamp(12, 28);
            AccountsShape { name, bar: 5, quota, fable: false, probe: false }
        }
    }
}

/// Whether there is a graphical session to open a browser in.
fn desktop_session() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows")) || ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|d| !d.is_empty()))
}

/// What a key pressed on an account row asks for: `d` flips whether it is
/// disabled, `+` and `-` move the number in its `pri` column by one.
fn account_change(account: &Value, key: char) -> AccountChange {
    let priority = account.get("priority").and_then(Value::as_i64).unwrap_or(0) as i32;
    match key {
        'd' if account.get("disabled").and_then(Value::as_bool).unwrap_or(false) => AccountChange::Enable,
        'd' => AccountChange::Disable,
        '-' => AccountChange::Priority(priority.saturating_sub(1)),
        _ => AccountChange::Priority(priority.saturating_add(1)),
    }
}

/// Heights of the accounts and sessions panes in `room` rows. Each takes what
/// its rows need (plus borders and a header line); when that is more than
/// there is, accounts keep up to two fifths of the room and sessions up to
/// half of what is left, so the activity log never vanishes.
fn pane_heights(room: u16, account_rows: usize, session_rows: usize) -> (u16, u16) {
    let want = |rows: usize| u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(3);
    let accounts = want(account_rows).min((room / 5 * 2).max(4)).min(room);
    // An empty table still says why it is empty.
    let sessions = want(session_rows.max(1)).min(((room - accounts) / 2).max(4)).min(room - accounts);
    (accounts, sessions)
}

/// The sessions pane: one row per session heard from in the last hour, those
/// with a request open first. `doc` is the sessions document, `rows` its
/// `sessions` array.
fn sessions_table<'a>(doc: &Value, rows: &'a [Value], area: Rect, multi: bool, now: i64) -> Table<'a> {
    let count = |k: &str| doc.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mut title = format!(" sessions: {} active, {} in the last hour ", count("active"), count("known"));
    if doc.get("names").and_then(Value::as_bool) == Some(false) {
        title.push_str("· names are off (corrall titles on) ");
    }
    let block = Block::default().borders(Borders::ALL).title(title);
    let cols = SessionCol::fitting(area.width);
    let align = |c: SessionCol, text: String| if c.numeric() { Cell::from(Line::from(text).right_aligned()) } else { Cell::from(text) };

    let note = if doc.get("sessions").is_none() {
        Some("this server has no /corrall/sessions; update and restart it to list sessions here")
    } else if rows.is_empty() {
        Some("no sessions in the last hour; a request carrying a Claude Code session id starts one")
    } else {
        None
    };
    if let Some(note) = note {
        return Table::new([Row::new([Cell::from(note)]).style(Style::default().fg(Color::DarkGray))], [Constraint::Min(0)]).block(block);
    }

    let body = rows.iter().map(|s| {
        let busy = s.get("inFlight").and_then(Value::as_u64).unwrap_or(0) > 0;
        let style = if busy {
            Style::default().fg(Color::Cyan)
        } else if s.get("active").and_then(Value::as_bool).unwrap_or(false) {
            Style::default()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Row::new(cols.iter().map(|c| align(*c, c.cell(s, multi, now)))).style(style)
    });
    let widths = cols.iter().map(|c| if *c == SessionCol::Title { Constraint::Min(c.width()) } else { Constraint::Length(c.width()) });
    Table::new(body, widths).header(Row::new(cols.iter().map(|c| align(*c, c.head().to_string()))).style(Style::default().fg(Color::Yellow))).block(block)
}

/// The header's routing summary for the default pool (the status document's
/// top level): every ranking strategy that is on, or `sticky` when none is.
fn routing_summary(st: &Value) -> String {
    let on = |v: Option<&Value>| v.and_then(Value::as_bool).unwrap_or(false);
    let strategies = [
        ("soonest-reset", on(st.get("soonestReset"))),
        ("expiry", on(st.get("expiryRouting").and_then(|e| e.get("enabled")))),
        (if st.get("distributeBy").and_then(Value::as_str) == Some("usage") { "distribute by usage" } else { "distribute" }, on(st.get("distributeSessions"))),
    ];
    let names: Vec<&str> = strategies.iter().filter(|(_, enabled)| *enabled).map(|(name, _)| *name).collect();
    if names.is_empty() {
        "sticky".into()
    } else {
        names.join(" + ")
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

    use crate::session_table::SPINNER;

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
    fn bar_runs_green_to_red() {
        let text = |spans: &[Span]| spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        assert_eq!(text(&bar(None, 10)), "·".repeat(10));
        assert_eq!(text(&bar(Some(0.5), 10)), format!("{}{}", "█".repeat(5), "░".repeat(5)));
        let full = bar(Some(1.0), 10);
        assert_eq!(full[0].style.fg, Some(Color::Rgb(35, 209, 96)));
        assert_eq!(full[9].style.fg, Some(Color::Rgb(239, 68, 68)));
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
    fn header_routing_summary() {
        assert_eq!(routing_summary(&json!({})), "sticky");
        assert_eq!(routing_summary(&json!({ "soonestReset": false, "expiryRouting": { "enabled": false }, "distributeSessions": false })), "sticky");
        assert_eq!(routing_summary(&json!({ "soonestReset": true })), "soonest-reset");
        assert_eq!(
            routing_summary(&json!({ "soonestReset": true, "expiryRouting": { "enabled": true }, "distributeSessions": true })),
            "soonest-reset + expiry + distribute"
        );
        assert_eq!(routing_summary(&json!({ "distributeSessions": true })), "distribute");
        assert_eq!(routing_summary(&json!({ "distributeSessions": true, "distributeBy": "usage" })), "distribute by usage");
        assert_eq!(routing_summary(&json!({ "distributeSessions": false, "distributeBy": "usage" })), "sticky");
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

    /// Draw the dashboard into an off-screen terminal and hand back its rows.
    fn render(width: u16, height: u16, st: &Value, sessions: &Value) -> Vec<String> {
        let tui = Tui::new(Backend::Remote(Remote::new("http://127.0.0.1:3456", "k").unwrap()), None);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| tui.draw(f, st, sessions)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect()
    }

    fn sample_sessions() -> Value {
        json!({ "active": 2, "known": 3, "names": true, "shown": 3, "sessions": [
            { "id": "2174c65f-1111-4222-8333-444455556666", "pool": "default", "name": "rosterstream-e0", "title": "Fix the roster sync retry", "account": "erik.claude.3@ednition.com",
              "model": "claude-fable-5-2", "active": true, "inFlight": 1, "busyMs": 12_000, "idleMs": 12_000, "ageMs": 7_260_000, "requests": 271,
              "tokens": { "input": 536, "output": 292_000, "cacheRead": 89_000_000, "cacheCreation": 939_500 }, "cacheHit": 0.99 },
            { "id": "b3acd49f-1111-4222-8333-444455556666", "pool": "default", "name": "rosterstream-8a", "title": null, "account": "erik.claude.3@ednition.com",
              "model": "claude-opus-5-5-20260101", "active": true, "inFlight": 0, "busyMs": null, "idleMs": 45_000, "ageMs": 5_820_000, "requests": 174,
              "tokens": { "input": 1_700, "output": 150_500, "cacheRead": 48_800_000, "cacheCreation": 418_000 }, "cacheHit": 0.98 },
            { "id": "c14711e0-1111-4222-8333-444455556666", "pool": "default", "name": null, "client": "ci", "account": null,
              "model": null, "active": false, "inFlight": 0, "busyMs": null, "idleMs": 960_000, "ageMs": 4_080_000, "requests": 0,
              "tokens": { "input": 0, "output": 0, "cacheRead": 0, "cacheCreation": 0 }, "cacheHit": null },
        ]})
    }

    fn sample_status() -> Value {
        json!({ "version": "9.9.9", "current": "a", "sessions": { "active": 2, "known": 3 },
            "pools": [{ "pool": "default", "accounts": [{ "id": "1", "name": "erik.claude.3@ednition.com", "current": true, "priority": 0 }] }] })
    }

    #[test]
    fn sessions_table_shows_who_is_running_on_what() {
        let rows = render(170, 24, &sample_status(), &sample_sessions());
        let screen = rows.join("\n");
        let line = |needle: &str| rows.iter().find(|r| r.contains(needle)).unwrap_or_else(|| panic!("no row with {needle:?} in\n{screen}")).clone();
        assert!(screen.contains("sessions: 2 active, 3 in the last hour"), "{screen}");
        let head = line("cache r");
        for h in ["session", "name", "account", "model", "last", "age", "req", "in", "cache w", "out", "hit", "title"] {
            assert!(head.contains(h), "header lacks {h}: {head}");
        }
        let busy = line("rosterstream-e0");
        for cell in [
            "2174c65f",
            "erik.claude.3@ednition.com",
            "fable-5-2",
            "12s…",
            "2h1m",
            "271",
            "536",
            "89.0M",
            "939.5k",
            "292.0k",
            "99%",
            "Fix the roster sync retry",
        ] {
            assert!(busy.contains(cell), "busy row lacks {cell}: {busy}");
        }
        assert!(SPINNER.iter().any(|f| busy.contains(f)), "a session with a request open is marked: {busy}");
        let idle = line("rosterstream-8a");
        assert!(idle.contains("45s ago") && idle.contains("opus-5-5 "), "{idle}");
        assert!(!idle.contains("20260101"), "the model's date suffix is dropped: {idle}");
        let unnamed = line("c14711e0");
        assert!(unnamed.contains("[ci] c14711e0…") && unnamed.contains("16m ago"), "{unnamed}");
        assert!(screen.contains(" activity "), "the log keeps its pane:\n{screen}");
    }

    #[test]
    fn sessions_table_gives_up_columns_on_a_narrow_terminal() {
        let rows = render(100, 20, &sample_status(), &sample_sessions());
        let busy = rows.iter().find(|r| r.contains("rosterstream-e0")).expect("the session is still listed");
        assert!(busy.contains("erik.claude.3@ednition.com") && busy.contains("99%") && !busy.contains("Fix the roster"), "{busy}");
    }

    #[test]
    fn sessions_pane_explains_itself_when_it_has_nothing_to_list() {
        let none = json!({ "active": 0, "known": 0, "names": false, "shown": 0, "sessions": [] });
        let screen = render(140, 20, &sample_status(), &none).join("\n");
        assert!(screen.contains("no sessions in the last hour"), "{screen}");
        assert!(screen.contains("names are off (corrall titles on)"), "{screen}");
        // A daemon from before the route existed.
        let screen = render(140, 20, &sample_status(), &Value::Null).join("\n");
        assert!(screen.contains("this server has no /corrall/sessions"), "{screen}");
    }

    #[test]
    fn accounts_table_keeps_names_readable_as_it_narrows() {
        let wide = AccountsShape::fitting(170);
        assert_eq!(wide, AccountsShape { name: 28, bar: 10, quota: 24, fable: true, probe: true });
        assert_eq!(AccountsShape::fitting(135), wide);
        assert_eq!(AccountsShape::fitting(134), AccountsShape { fable: false, ..wide }, "the Fable window goes first");
        assert_eq!(AccountsShape::fitting(109), AccountsShape { fable: false, probe: false, ..wide });
        assert_eq!(AccountsShape::fitting(80), AccountsShape { name: 21, bar: 5, quota: 19, fable: false, probe: false });
        assert_eq!(AccountsShape::fitting(40).name, 12, "never less than this");

        let rows = render(80, 20, &sample_status(), &sample_sessions());
        let account = rows.iter().find(|r| r.contains("►")).expect("the current account's row");
        assert!(account.contains("erik.claude.3@ednitio "), "an 80-column terminal still names the account: {account}");
        assert!(account.contains("ready"), "{account}");
    }

    #[test]
    fn panes_share_a_short_screen() {
        assert_eq!(pane_heights(47, 3, 13), (6, 16), "everything fits");
        assert_eq!(pane_heights(21, 3, 30), (6, 7), "sessions take half of what accounts leave");
        assert_eq!(pane_heights(21, 30, 0), (8, 4), "accounts are held to two fifths");
        let (a, s) = pane_heights(5, 5, 5);
        assert!(a + s <= 5, "never more than the screen: {a} + {s}");
        assert_eq!(pane_heights(0, 5, 5), (0, 0));
        let (a, s) = pane_heights(u16::MAX, usize::MAX, usize::MAX);
        assert!(a > 0 && s > 0, "absurd sizes do not overflow");
    }

    fn tui_on(screen: Screen) -> Tui {
        let mut tui = Tui::new(Backend::Remote(Remote::new("http://127.0.0.1:3456", "k").unwrap()), None);
        tui.screen = screen;
        tui
    }

    fn drawn(tui: &Tui, width: u16, height: u16, st: &Value, sessions: &Value) -> String {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| tui.draw(f, st, sessions)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height).map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn detail_screen_shows_the_account_its_sessions_and_its_log_lines() {
        let mut tui = tui_on(Screen::Detail);
        tui.push_log("✓ 12:00:00  rosterstream-e0 POST /v1/messages → erik.claude.3@ednition.com 200 (1.0s)".into(), Severity::Info);
        tui.push_log("  12:00:01  Switched to account \"someone-else\"".into(), Severity::Info);
        let screen = drawn(&tui, 170, 40, &sample_status(), &sample_sessions());
        assert!(screen.contains("┌ erik.claude.3@ednition.com "), "{screen}");
        assert!(screen.contains("state                 ready, current") && screen.contains("pool                  default"), "{screen}");
        assert!(screen.contains("sessions: 2 active, 2 in the last hour"), "only this account's sessions:\n{screen}");
        assert!(screen.contains("rosterstream-e0") && screen.contains("rosterstream-8a") && !screen.contains("c14711e0"), "{screen}");
        assert!(screen.contains("log lines naming this account") && screen.contains("POST /v1/messages") && !screen.contains("someone-else"), "{screen}");
        assert!(screen.contains("x remove") && screen.contains("L log in again"), "the footer lists this screen's keys:\n{screen}");
    }

    #[test]
    fn log_screen_scrolls_and_filters() {
        let mut tui = tui_on(Screen::Log(LogView::default()));
        for i in 0..50 {
            tui.push_log(format!("line {i}"), if i % 10 == 0 { Severity::Warn } else { Severity::Info });
        }
        let screen = drawn(&tui, 100, 16, &sample_status(), &Value::Null);
        assert!(screen.contains("log: everything · newest of 50"), "{screen}");
        assert!(screen.contains("line 49") && !screen.contains("line 30"), "newest first, a screenful:\n{screen}");

        // Down is back in time; the view cannot run off the end.
        tui.screen = tui.key_log(KeyCode::PageDown, LogView::default());
        let screen = drawn(&tui, 100, 16, &sample_status(), &Value::Null);
        assert!(screen.contains("20 back of 50") && screen.contains("line 29") && !screen.contains("line 49"), "{screen}");
        let Screen::Log(view) = tui.key_log(KeyCode::End, LogView::default()) else { panic!() };
        assert_eq!(view.offset, 49);
        // A line arriving while scrolled back does not move what is on screen.
        tui.push_log("line 50".into(), Severity::Info);
        let Screen::Log(view) = &tui.screen else { panic!() };
        assert_eq!(view.offset, 21);

        tui.screen = tui.key_log(KeyCode::Char('w'), LogView::default());
        let screen = drawn(&tui, 100, 16, &sample_status(), &Value::Null);
        assert!(screen.contains("warnings, errors and failed requests · newest of 5"), "{screen}");
        assert!(screen.contains("line 40") && screen.contains("line 0 ") && !screen.contains("line 41"), "{screen}");
        assert!(matches!(tui.key_log(KeyCode::Esc, LogView::default()), Screen::Dashboard));
    }

    #[tokio::test]
    async fn settings_login_import_and_confirm_screens_draw() {
        let settings = Settings {
            cursor: 2,
            pool: "default".into(),
            cfg: Some(crate::config::Config::default()),
            editing: None,
            error: None,
            service: crate::service::State::Missing,
        };
        let screen = drawn(&tui_on(Screen::Settings(Box::new(settings))), 130, 30, &sample_status(), &Value::Null);
        for want in [
            "settings: ",
            "off: stay on one account until it reaches the threshold",
            "default (the default)",
            "Spread new sessions across accounts",
            "Quota probe interval",
            "↵ run",
            "not installed   ↵ install",
        ] {
            assert!(screen.contains(want), "missing {want:?} in\n{screen}");
        }

        let (pasted, _rx) = tokio::sync::mpsc::unbounded_channel();
        let login = Login {
            again: None,
            url: Some("https://claude.example/oauth/authorize?x=1".into()),
            opened: true,
            input: Input::with("http://localhost:1/callback?code=abc"),
            note: "waiting for the browser…".into(),
            failed: false,
            pasted,
            task: tokio::spawn(async {}),
        };
        let screen = drawn(&tui_on(Screen::Login(login)), 130, 30, &sample_status(), &Value::Null);
        for want in [
            "add an account",
            "https://claude.example/oauth/authorize?x=1",
            "> http://localhost:1/callback?code=abc",
            "A browser has been opened",
            "waiting for the browser…",
            "joins the default pool",
        ] {
            assert!(screen.contains(want), "missing {want:?} in\n{screen}");
        }

        let import = Import {
            files: vec!["~/.claude/.credentials.json".into(), "~/.claude-work/.credentials.json".into()],
            cursor: 1,
            link: true,
            busy: false,
            failed: Some("import failed: profile lookup failed".into()),
        };
        let screen = drawn(&tui_on(Screen::Import(import)), 130, 30, &sample_status(), &Value::Null);
        assert!(
            screen.contains("import a Claude Code login") && screen.contains("~/.claude-work/.credentials.json") && screen.contains("Tab: LINK the file"),
            "{screen}"
        );
        assert!(screen.contains("import failed: profile lookup failed"), "what went wrong is on the screen it went wrong on:\n{screen}");
        let none = Import { files: vec![], cursor: 0, link: true, busy: false, failed: None };
        assert!(drawn(&tui_on(Screen::Import(none)), 130, 30, &sample_status(), &Value::Null).contains("keeps its login in the Keychain"));

        // A question sits over the screen that asked it, and `n` drops it.
        let mut tui = tui_on(Screen::Detail);
        tui.confirm = Some(Confirm {
            question: "Remove a@x.com from pool default?".into(),
            detail: "Its entry is taken out.".into(),
            then: Action::PointClaude { remove: true },
        });
        let screen = drawn(&tui, 130, 30, &sample_status(), &sample_sessions());
        assert!(screen.contains("┌ confirm ") && screen.contains("Remove a@x.com from pool default?") && screen.contains("y yes    n / Esc no"), "{screen}");
        assert!(!tui.on_key(KeyCode::Char('n'), KeyModifiers::NONE, &sample_status()).await);
        assert!(tui.confirm.is_none() && matches!(tui.screen, Screen::Detail));
    }

    #[tokio::test]
    async fn dashboard_keys_move_the_cursor_and_open_screens() {
        let st = json!({ "version": "9.9.9", "pools": [{ "pool": "default", "accounts": [
            { "id": "1", "name": "a@x.com", "type": "oauth", "priority": 0 }, { "id": "2", "name": "b@x.com", "type": "apikey", "priority": 1 }] }] });
        let mut tui = tui_on(Screen::Dashboard);
        let key = |c| (c, KeyModifiers::NONE);
        for (code, want) in [(KeyCode::Down, 1), (KeyCode::Down, 1), (KeyCode::Char('k'), 0), (KeyCode::Up, 0)] {
            assert!(!tui.on_key(code, KeyModifiers::NONE, &st).await);
            assert_eq!(tui.cursor, want, "{code:?}");
        }
        let (code, mods) = key(KeyCode::Enter);
        tui.on_key(code, mods, &st).await;
        assert!(matches!(tui.screen, Screen::Detail));
        // The cursor still moves, now between accounts in detail.
        tui.on_key(KeyCode::Down, KeyModifiers::NONE, &st).await;
        assert!(tui.cursor == 1 && matches!(tui.screen, Screen::Detail));
        // An API-key account has no login to renew.
        tui.on_key(KeyCode::Char('L'), KeyModifiers::NONE, &st).await;
        assert!(matches!(tui.screen, Screen::Detail) && tui.message.as_deref().is_some_and(|m| m.contains("no login")), "{:?}", tui.message);
        tui.on_key(KeyCode::Esc, KeyModifiers::NONE, &st).await;
        assert!(matches!(tui.screen, Screen::Dashboard));
        tui.on_key(KeyCode::Char('l'), KeyModifiers::NONE, &st).await;
        assert!(matches!(tui.screen, Screen::Log(_)));
        tui.on_key(KeyCode::Char('q'), KeyModifiers::NONE, &st).await;
        assert!(matches!(tui.screen, Screen::Dashboard), "q leaves a screen; it quits only from the dashboard");

        // Attached to a server elsewhere, this machine's config is not that
        // server's: the screens that edit it stay shut and say why.
        for c in ['g', 'a', 'i'] {
            tui.message = None;
            assert!(!tui.on_key(KeyCode::Char(c), KeyModifiers::NONE, &st).await);
            assert!(matches!(tui.screen, Screen::Dashboard), "{c}");
            assert!(tui.message.as_deref().is_some_and(|m| m.contains("another machine")), "{c}: {:?}", tui.message);
        }
        assert!(tui.on_key(KeyCode::Char('q'), KeyModifiers::NONE, &st).await, "q on the dashboard quits");
        assert!(tui.on_key(KeyCode::Char('c'), KeyModifiers::CONTROL, &st).await);
    }

    #[test]
    fn account_keys_toggle_and_step() {
        let enabled = json!({ "priority": 2, "disabled": false });
        assert_eq!(account_change(&enabled, 'd'), AccountChange::Disable);
        assert_eq!(account_change(&json!({ "disabled": true }), 'd'), AccountChange::Enable);
        assert_eq!(account_change(&enabled, '+'), AccountChange::Priority(3));
        assert_eq!(account_change(&enabled, '='), AccountChange::Priority(3), "the unshifted + key");
        assert_eq!(account_change(&enabled, '-'), AccountChange::Priority(1));
        assert_eq!(account_change(&json!({}), '-'), AccountChange::Priority(-1));
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
