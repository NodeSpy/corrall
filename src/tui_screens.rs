//! The dashboard's screens beyond the main one: an account in detail, the
//! full log, settings, adding an account by login or by import, and the
//! yes/no question asked before anything that removes or installs.
//!
//! This file holds what each screen knows and how it is drawn. The keys, and
//! everything that talks to the server, are in `tui.rs`.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use serde_json::Value;

use crate::config::{Config, DistributeBy, EventLogging, PoolConfig, Threshold};
use crate::security::safe_text;
use crate::session_table::token_count;
use crate::status::{countdown, format_duration, ts as parse_ts};

/// Which screen the dashboard is showing.
pub enum Screen {
    Dashboard,
    /// The account under the cursor.
    Detail,
    Log(LogView),
    Settings(Box<Settings>),
    Login(Login),
    Import(Import),
}

/// A question that must be answered `y` before `then` happens.
pub struct Confirm {
    pub question: String,
    pub detail: String,
    pub then: Action,
}

/// What a confirmed question sets off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    RemoveAccount { pool: String, id: String },
    PointClaude { remove: bool },
    InstallService,
}

/// One line of typed text.
#[derive(Default)]
pub struct Input {
    pub text: String,
}

impl Input {
    pub fn with(text: &str) -> Input {
        Input { text: text.to_string() }
    }

    pub fn push(&mut self, c: char) {
        if !c.is_control() && self.text.chars().count() < 4096 {
            self.text.push(c);
        }
    }

    /// A paste lands as one string; only its first line is the answer.
    pub fn paste(&mut self, s: &str) {
        for c in s.lines().next().unwrap_or_default().chars() {
            self.push(c);
        }
    }

    pub fn backspace(&mut self) {
        self.text.pop();
    }
}

// ── log ───────────────────────────────────────────────────────

#[derive(Default)]
pub struct LogView {
    /// Lines scrolled back from the newest.
    pub offset: usize,
    /// Show only warnings, errors and failed requests.
    pub problems_only: bool,
}

// ── login ─────────────────────────────────────────────────────

pub struct Login {
    /// The account being logged in again, or `None` for a new one.
    pub again: Option<String>,
    /// The page to open, once the listener is up.
    pub url: Option<String>,
    /// Whether a browser was started on it from here.
    pub opened: bool,
    pub input: Input,
    /// What is happening, or what went wrong with the last paste.
    pub note: String,
    pub failed: bool,
    /// Pasted text goes to the task holding the login.
    pub pasted: tokio::sync::mpsc::UnboundedSender<String>,
    pub task: tokio::task::JoinHandle<()>,
}

// ── import ────────────────────────────────────────────────────

pub struct Import {
    /// Credential files found on this machine.
    pub files: Vec<String>,
    pub cursor: usize,
    /// Link the file (Claude Code keeps refreshing it) rather than copy it.
    pub link: bool,
    pub busy: bool,
    /// Why the last attempt did not work.
    pub failed: Option<String>,
}

/// Claude Code logins on this machine: `.credentials.json` in `~/.claude`
/// and in any sibling config directory (`~/.claude-work`, …), `~`-relative.
pub fn credential_files(home: &std::path::Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name.starts_with(".claude") && home.join(name).join(".credentials.json").is_file())
        .map(|name| format!("~/{name}/.credentials.json"))
        .collect();
    // Claude Code's own directory first, the rest by name.
    found.sort_by_key(|p| (p != crate::oauth::DEFAULT_CREDENTIALS_PATH, p.clone()));
    found
}

// ── settings ──────────────────────────────────────────────────

/// A row of the settings screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Pool,
    Threshold,
    Distribute,
    Probe,
    Hold,
    SoonestReset,
    Expiry,
    Names,
    Telemetry,
    Warmup,
    WeeklyFirst,
    PointClaude,
    UnpointClaude,
    Service,
}

/// What Enter does on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Steps to the next value.
    Cycle,
    /// Opens the value for typing.
    Edit,
    /// Asks, then does something.
    Run,
}

impl Setting {
    pub const ALL: [Setting; 14] = {
        use Setting::*;
        [Pool, Threshold, Distribute, Probe, Hold, SoonestReset, Expiry, Names, Telemetry, Warmup, WeeklyFirst, PointClaude, UnpointClaude, Service]
    };

    pub fn label(self) -> &'static str {
        use Setting::*;
        match self {
            Pool => "Pool",
            Threshold => "Switch threshold",
            Distribute => "Spread new sessions across accounts",
            Probe => "Quota probe interval",
            Hold => "Hold requests when every account is spent",
            SoonestReset => "Spend the quota that resets soonest first",
            Expiry => "Expiry-pressure routing",
            Names => "Name sessions from Claude Code's files",
            Telemetry => "Telemetry requests",
            Warmup => "Keep-warm interval",
            WeeklyFirst => "Weekly window shown first",
            PointClaude => "Point Claude Code at the proxy",
            UnpointClaude => "Take the proxy out of Claude Code's settings",
            Service => "Run as a service that starts at login",
        }
    }

    /// What the row under the cursor is for, and what it takes.
    pub fn hint(self) -> &'static str {
        use Setting::*;
        match self {
            Pool => "Which pool the six rows below show and change. Enter steps to the next pool.",
            Threshold => "Leave an account once a quota window is this full. Type a percent, or bucket=percent (unified7d=90) for one window; bucket=default clears it.",
            Distribute => "off: stay on one account until it reaches the threshold. by sessions: a new session goes to the account serving the fewest. by usage: to the account with the most quota left.",
            Probe => "Read each account's quota this often so idle accounts stay current. Costs no quota. Seconds, 30 or more; off for never.",
            Hold => "With every account spent, hold a request this many seconds for one to free up before answering 429. 0 answers at once.",
            SoonestReset => "Among accounts of equal priority, use first the one whose weekly window ends soonest, then the soonest 5h window.",
            Expiry => "Prefer the account whose unused weekly quota is closest to being forfeited.",
            Names => "Read session names, titles and directories from ~/.claude on this machine, for the sessions table and the log.",
            Telemetry => "What happens to Claude Code's /api/event_logging requests: forwarded quietly, forwarded and logged, or answered here without leaving the machine.",
            Warmup => "Send a tiny request through each idle account this often so its 5h window is already running. Seconds, 60 or more; off for never.",
            WeeklyFirst => "Which weekly quota column comes first in the accounts table: the shared 7d window or the Fable one. On a narrow terminal only the first is shown.",
            PointClaude => "Write the proxy into ~/.claude/settings.json so every Claude Code session uses it. What `corrall setup claude` does.",
            UnpointClaude => "Remove what that wrote. What `corrall setup claude --remove` does.",
            Service => "Start the proxy at every login and keep it running: a systemd --user unit on Linux, a launchd agent on macOS.",
        }
    }

    pub fn kind(self) -> Kind {
        use Setting::*;
        match self {
            Pool | Distribute | SoonestReset | Expiry | Names | Telemetry | WeeklyFirst => Kind::Cycle,
            Threshold | Probe | Hold | Warmup => Kind::Edit,
            PointClaude | UnpointClaude | Service => Kind::Run,
        }
    }

    /// The value as the screen shows it. Empty for a row that runs something.
    pub fn value(self, cfg: &Config, pool: &str) -> String {
        use Setting::*;
        let p = cfg.pool_or_default(Some(pool));
        let on_off = |on: bool| if on { "on" } else { "off" }.to_string();
        let secs = |n: u64| if n == 0 { "off".to_string() } else { format!("{n}s") };
        match self {
            Pool if pool == cfg.default_pool => format!("{pool} (the default)"),
            Pool => pool.to_string(),
            Threshold => threshold_text(&p.switch_threshold),
            Distribute => crate::cli::describe_distribute(p),
            Probe => secs(p.quota_probe_seconds),
            Hold => format!("{}s", p.hold_seconds),
            SoonestReset => on_off(p.soonest_reset),
            Expiry => on_off(p.expiry_routing.enabled),
            Names => on_off(cfg.session_titles.enabled),
            Telemetry => match cfg.event_logging {
                EventLogging::Hide => "forwarded, kept out of the log",
                EventLogging::Show => "forwarded, shown in the log",
                EventLogging::Block => "answered here, never forwarded",
            }
            .to_string(),
            Warmup => secs(cfg.warmup_seconds),
            WeeklyFirst => if cfg.fable_column_first { "7d fable" } else { "7d" }.to_string(),
            PointClaude | UnpointClaude | Service => String::new(),
        }
    }

    /// What typing starts from on an [`Kind::Edit`] row.
    pub fn editable(self, cfg: &Config, pool: &str) -> String {
        let p = cfg.pool_or_default(Some(pool));
        match self {
            Setting::Threshold => match &p.switch_threshold {
                Threshold::Single(x) => percent(*x),
                Threshold::Table(_) => String::new(),
            },
            Setting::Probe if p.quota_probe_seconds == 0 => "off".into(),
            Setting::Probe => p.quota_probe_seconds.to_string(),
            Setting::Hold => p.hold_seconds.to_string(),
            Setting::Warmup if cfg.warmup_seconds == 0 => "off".into(),
            Setting::Warmup => cfg.warmup_seconds.to_string(),
            _ => String::new(),
        }
    }

    /// Step a [`Kind::Cycle`] row to its next value. `Pool` is not stored in
    /// the config: the screen steps it itself.
    pub fn step(self, cfg: &mut Config, pool: &str) -> anyhow::Result<()> {
        match self {
            Setting::Distribute => {
                let p = pool_mut(cfg, pool)?;
                (p.distribute_sessions, p.distribute_by) = match (p.distribute_sessions, p.distribute_by) {
                    (false, _) => (true, DistributeBy::Sessions),
                    (true, DistributeBy::Sessions) => (true, DistributeBy::Usage),
                    (true, DistributeBy::Usage) => (false, DistributeBy::Sessions),
                };
            }
            Setting::SoonestReset => {
                let p = pool_mut(cfg, pool)?;
                p.soonest_reset = !p.soonest_reset;
            }
            Setting::Expiry => {
                let p = pool_mut(cfg, pool)?;
                p.expiry_routing.enabled = !p.expiry_routing.enabled;
            }
            Setting::Names => cfg.session_titles.enabled = !cfg.session_titles.enabled,
            Setting::WeeklyFirst => cfg.fable_column_first = !cfg.fable_column_first,
            Setting::Telemetry => {
                cfg.event_logging = match cfg.event_logging {
                    EventLogging::Hide => EventLogging::Show,
                    EventLogging::Show => EventLogging::Block,
                    EventLogging::Block => EventLogging::Hide,
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Store what was typed on a [`Kind::Edit`] row.
    pub fn set(self, cfg: &mut Config, pool: &str, typed: &str) -> anyhow::Result<()> {
        let typed = typed.trim();
        match self {
            Setting::Threshold => crate::cli::set_threshold(&mut pool_mut(cfg, pool)?.switch_threshold, typed.trim_end_matches('%'))?,
            Setting::Probe => pool_mut(cfg, pool)?.quota_probe_seconds = crate::cli::parse_secs(typed, 30, "probe interval")?,
            Setting::Hold => pool_mut(cfg, pool)?.hold_seconds = crate::cli::parse_secs(typed, 0, "hold")?,
            Setting::Warmup => cfg.warmup_seconds = crate::cli::parse_secs(typed, 60, "keep-warm interval")?,
            _ => {}
        }
        Ok(())
    }
}

fn pool_mut<'a>(cfg: &'a mut Config, pool: &str) -> anyhow::Result<&'a mut PoolConfig> {
    cfg.pool_mut(pool).ok_or_else(|| anyhow::anyhow!("no pool named {pool:?}"))
}

fn percent(x: f64) -> String {
    let p = x * 100.0;
    if (p - p.round()).abs() < 1e-9 {
        format!("{p:.0}")
    } else {
        format!("{p:.1}")
    }
}

fn threshold_text(t: &Threshold) -> String {
    match t {
        Threshold::Single(x) => format!("{}%", percent(*x)),
        Threshold::Table(table) => table.iter().map(|(bucket, x)| format!("{bucket}={}%", percent(*x))).collect::<Vec<_>>().join(", "),
    }
}

pub struct Settings {
    pub cursor: usize,
    /// The pool the per-pool rows show and change.
    pub pool: String,
    /// The config as last read from disk; `None` when it would not load.
    pub cfg: Option<Config>,
    /// Open on an [`Kind::Edit`] row.
    pub editing: Option<Input>,
    pub error: Option<String>,
    /// Where the service stands, asked once when the screen opens.
    pub service: crate::service::State,
}

impl Settings {
    pub fn open() -> Settings {
        let cfg = Config::load().ok().flatten();
        let pool = cfg.as_ref().map(|c| c.default_pool.clone()).unwrap_or_default();
        Settings { cursor: 0, pool, cfg, editing: None, error: None, service: crate::service::state() }
    }

    pub fn row(&self) -> Setting {
        Setting::ALL[self.cursor.min(Setting::ALL.len() - 1)]
    }

    /// Move on to the next pool, by name, wrapping.
    pub fn next_pool(&mut self) {
        let Some(cfg) = &self.cfg else { return };
        let names: Vec<&String> = cfg.pools.keys().collect();
        let at = names.iter().position(|n| **n == self.pool).unwrap_or(0);
        if let Some(next) = names.get((at + 1) % names.len().max(1)) {
            self.pool = (*next).clone();
        }
    }

    fn service_text(&self) -> String {
        use crate::service::State;
        let name = crate::service::Kind::native().map(|k| k.name()).unwrap_or("-");
        match self.service {
            State::Unsupported => "not available on this system".into(),
            State::Running => format!("running ({name})"),
            State::Stopped => format!("installed, not running ({name})"),
            State::Missing => "not installed".into(),
        }
    }
}

/// A path with the home directory written `~`, for a title or a message.
pub fn tilde(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(|rest| rest.display().to_string())) {
        Some(rest) if rest.is_empty() => "~".into(),
        Some(rest) => format!("~/{rest}"),
        None => path.display().to_string(),
    }
}

// ── drawing ───────────────────────────────────────────────────

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// A row of a list, with the cursor's mark in its margin when it is the one
/// selected. `text` starts with the space the mark takes.
fn list_row(text: String, selected: bool) -> Line<'static> {
    match selected {
        true => Line::from(format!("›{}", text.chars().skip(1).collect::<String>())).style(Style::default().fg(Color::Cyan).bold()),
        false => Line::from(text),
    }
}

/// A bordered pane titled `title` filling `area`; returns the space inside.
fn pane(f: &mut Frame, area: Rect, title: &str) -> Rect {
    let block = Block::default().borders(Borders::ALL).title(format!(" {title} "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

pub fn draw_settings(f: &mut Frame, area: Rect, s: &Settings) {
    let inner = pane(f, area, &format!("settings: {}", tilde(&crate::config::config_path())));
    let Some(cfg) = &s.cfg else {
        f.render_widget(Paragraph::new("the config file could not be read; `corrall config check` says why").style(Style::default().fg(Color::Red)), inner);
        return;
    };
    let label_w = Setting::ALL.iter().map(|r| r.label().chars().count()).max().unwrap_or(0) + 2;
    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in Setting::ALL.iter().enumerate() {
        if matches!(row, Setting::Names | Setting::PointClaude) {
            lines.push(Line::from("─".repeat(inner.width as usize)).style(dim()));
        }
        let value = match (row, &s.editing) {
            (_, Some(input)) if i == s.cursor => format!("> {}▏", input.text),
            (Setting::Service, _) if s.service == crate::service::State::Running => format!("{}   ↵ reinstall", s.service_text()),
            (Setting::Service, _) => format!("{}   ↵ install", s.service_text()),
            (r, _) if r.kind() == Kind::Run => "↵ run".to_string(),
            (r, _) => r.value(cfg, &s.pool),
        };
        let text = format!(" {:<label_w$}{}", row.label(), safe_text(&value, 120));
        lines.push(list_row(text, i == s.cursor));
    }
    lines.push(Line::default());
    lines.push(match &s.error {
        Some(e) => Line::from(format!(" {}", safe_text(e, 200))).style(Style::default().fg(Color::Red)),
        None => Line::from(format!(" {}", s.row().hint())).style(dim()),
    });
    lines.push(Line::default());
    lines.push(Line::from(" A change is written to the config file and the server reloads it.").style(dim()));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub fn draw_log(f: &mut Frame, area: Rect, lines: &[(String, Style)], view: &LogView, total: usize) {
    let filter = if view.problems_only { "warnings, errors and failed requests" } else { "everything" };
    let at = if view.offset == 0 { "newest".to_string() } else { format!("{} back", view.offset) };
    let inner = pane(f, area, &format!("log: {filter} · {at} of {total}"));
    let text: Vec<Line> = lines.iter().map(|(l, style)| Line::from(l.as_str()).style(*style)).collect();
    f.render_widget(Paragraph::new(text), inner);
}

pub fn draw_login(f: &mut Frame, area: Rect, l: &Login) {
    let title = match &l.again {
        Some(name) => format!("log in again: {}", safe_text(name, 60)),
        None => "add an account".to_string(),
    };
    let inner = pane(f, area, &title);
    let mut lines = vec![Line::default()];
    match &l.url {
        None => lines.push(Line::from(" starting…").style(dim())),
        Some(url) => {
            lines.push(Line::from(if l.opened {
                " A browser has been opened on Claude's sign-in page. If none appeared, open this:"
            } else {
                " Open this in a browser:"
            }));
            lines.push(Line::default());
            lines.push(Line::from(format!(" {url}")).style(Style::default().fg(Color::Cyan)));
            lines.push(Line::default());
            lines.push(Line::from(" Sign in with the account to add. The browser comes back here by itself and this screen closes."));
            lines.push(Line::from(
                " Browser on another machine? It ends on a page that will not load: paste that page's address (or the code) here and press Enter.",
            ));
            lines.push(Line::default());
            lines.push(Line::from(format!(" > {}▏", safe_text(&l.input.text, 400))));
        }
    }
    lines.push(Line::default());
    let style = if l.failed { Style::default().fg(Color::Red) } else { dim() };
    lines.push(Line::from(format!(" {}", safe_text(&l.note, 300))).style(style));
    if l.again.is_none() {
        lines.push(Line::default());
        lines.push(Line::from(" A new account joins the default pool; `corrall pool set <pool> --account <name>` moves it.").style(dim()));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub fn draw_import(f: &mut Frame, area: Rect, i: &Import) {
    let inner = pane(f, area, "import a Claude Code login");
    let mut lines = vec![Line::default()];
    if i.files.is_empty() {
        lines.push(Line::from(" No Claude Code credentials file on this machine (~/.claude*/.credentials.json)."));
        lines.push(
            Line::from(" On macOS Claude Code keeps its login in the Keychain, where there is no file to import: press Esc, then `a` to log in instead.")
                .style(dim()),
        );
    } else {
        lines.push(Line::from(" Which login?").style(dim()));
        for (n, file) in i.files.iter().enumerate() {
            let text = format!("   {}", safe_text(file, 200));
            lines.push(list_row(text, n == i.cursor));
        }
        lines.push(Line::default());
        lines.push(Line::from(if i.link {
            " Tab: LINK the file. Claude Code keeps refreshing the login and corrall reads it. Right while Claude Code still uses it."
        } else {
            " Tab: COPY the tokens. Corrall refreshes the login from now on; signing in with it elsewhere logs one of the two out."
        }));
        lines.push(Line::default());
        match (&i.failed, i.busy) {
            (_, true) => lines.push(Line::from(" importing…").style(dim())),
            (Some(why), false) => lines.push(Line::from(format!(" {}", safe_text(why, 400))).style(Style::default().fg(Color::Red))),
            (None, false) => {}
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The question, centred over whatever screen asked it.
pub fn draw_confirm(f: &mut Frame, area: Rect, c: &Confirm) {
    let width = area.width.saturating_sub(4).min(96);
    let inner_w = width.saturating_sub(4).max(1) as usize;
    let wrapped = |s: &str| s.chars().count().div_ceil(inner_w).max(1) as u16;
    let height = (wrapped(&c.question) + wrapped(&c.detail) + 5).min(area.height);
    let rect = Rect { x: area.x + (area.width.saturating_sub(width)) / 2, y: area.y + (area.height.saturating_sub(height)) / 2, width, height };
    f.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow)).title(" confirm ");
    let lines = vec![
        Line::from(format!(" {}", safe_text(&c.question, 300))).style(Style::default().bold()),
        Line::default(),
        Line::from(format!(" {}", safe_text(&c.detail, 600))),
        Line::default(),
        Line::from(" y yes    n / Esc no").style(dim()),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), rect);
}

/// Everything the status document says about one account, a line per fact.
/// `pool` is the pool it is in.
pub fn account_facts(a: &Value, pool: &str, now: i64) -> Vec<Line<'static>> {
    let text = |k: &str| a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(|s| safe_text(s, 200));
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut fact = |label: &str, value: Line<'static>| {
        let mut spans = vec![Span::styled(format!("   {label:<22}"), dim())];
        spans.extend(value.spans);
        out.push(Line::from(spans));
    };
    let plain = |s: String| Line::from(s);

    let disabled = a.get("disabled").and_then(Value::as_bool).unwrap_or(false);
    let state = match (disabled, text("blocked")) {
        (true, _) => Line::from("disabled").style(Style::default().fg(Color::Yellow)),
        (false, Some(why)) => Line::from(why).style(Style::default().fg(Color::Yellow)),
        (false, None) if a.get("current").and_then(Value::as_bool).unwrap_or(false) => Line::from("ready, current").style(Style::default().fg(Color::Green)),
        (false, None) => Line::from("ready"),
    };
    fact("state", state);
    fact("pool", plain(safe_text(pool, 40)));
    fact("priority", plain(a.get("priority").and_then(Value::as_i64).unwrap_or(0).to_string()));
    let kind = [text("type"), text("provider"), text("tier")].into_iter().flatten().collect::<Vec<_>>().join(", ");
    fact("kind", plain(kind));
    let count = |k: &str| a.get(k).and_then(Value::as_u64).unwrap_or(0);
    let many = |n: u64, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    fact("serving", plain(format!("{}, {} open", many(count("sessions"), "session"), many(count("inFlight"), "request"))));
    for (label, key) in [("e-mail", "email"), ("organisation", "orgName"), ("account uuid", "accountUuid"), ("upstream", "upstream")] {
        if let Some(v) = text(key) {
            fact(label, plain(v));
        }
    }
    for (label, key) in [("access token", "tokenExpiresAt"), ("login", "loginExpiresAt"), ("rate-limited", "rateLimitedUntil")] {
        if let Some(t) = parse_ts(a.get(key)) {
            let left = t - now;
            let when = if left > 0 { format!("for another {}", format_duration(left)) } else { format!("expired {} ago", format_duration(-left)) };
            let line =
                if key == "loginExpiresAt" && left < 3 * 24 * 3600 * 1000 { Line::from(when).style(Style::default().fg(Color::Yellow)) } else { plain(when) };
            fact(label, line);
        }
    }
    for (label, bucket) in [("5h", "unified5h"), ("7d", "unified7d"), ("7d fable", "unified7dFable"), ("7d sonnet", "unified7dSonnet")] {
        let u = a.pointer(&format!("/quota/{bucket}/utilization")).and_then(Value::as_f64);
        let reset = a.pointer(&format!("/quota/{bucket}/resetInSeconds")).and_then(Value::as_i64);
        if u.is_none() && reset.is_none() {
            continue;
        }
        let mut spans = crate::tui::bar(u, 20);
        spans.push(Span::raw(format!(" {}", u.map(|x| format!("{:>3.0}%", x * 100.0)).unwrap_or_else(|| "  ?%".into()))));
        spans.push(Span::styled(format!("   resets in {}", countdown(reset)), dim()));
        fact(label, Line::from(spans));
    }
    if let Some(models) = a.get("models").and_then(Value::as_object) {
        let marks: Vec<String> = models.iter().map(|(m, ok)| format!("{m} {}", if ok.as_bool().unwrap_or(false) { "✓" } else { "✗" })).collect();
        fact("can serve", plain(marks.join("   ")));
    }
    // An API key has no usage endpoint to probe: nothing to report.
    if let Some(p) = a.get("probe").filter(|p| p.get("status").and_then(Value::as_str) != Some("not-applicable")) {
        let status = p.get("status").and_then(Value::as_str).unwrap_or("never");
        let ago = parse_ts(p.get("lastProbedAt")).map(|t| format!(" {} ago", format_duration((now - t).max(0)))).unwrap_or_default();
        let err = p.get("error").and_then(Value::as_str).map(|e| format!(": {}", safe_text(e, 120))).unwrap_or_default();
        fact("quota probe", plain(format!("{}{ago}{err}", safe_text(status, 20))));
    }
    if let Some(u) = a.get("usage") {
        let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
        let last = parse_ts(u.get("lastUsed")).map(|t| format!(", last {} ago", format_duration((now - t).max(0)))).unwrap_or_default();
        fact("requests", plain(format!("{} ({} failed){last}", n("totalRequests"), n("failedRequests"))));
        let (input, read, write, out) = (n("inputTokens"), n("cacheReadTokens"), n("cacheCreationTokens"), n("outputTokens"));
        let prompt = input + read + write;
        let hit = if prompt > 0 { format!("  hit {:.0}%", read as f64 * 100.0 / prompt as f64) } else { String::new() };
        fact(
            "tokens",
            plain(format!("in {}  cached {}  written {}  out {}{hit}", token_count(input), token_count(read), token_count(write), token_count(out))),
        );
    }
    if let Some(e) = text("error") {
        fact("last error", Line::from(e).style(Style::default().fg(Color::Red)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(lines: &[Line]) -> String {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn settings_show_and_step_a_pools_values() {
        let mut cfg = Config::default();
        cfg.pools.insert("work".into(), PoolConfig { quota_probe_seconds: 300, hold_seconds: 120, ..Default::default() });
        let show = |cfg: &Config, row: Setting, pool: &str| row.value(cfg, pool);
        assert_eq!(show(&cfg, Setting::Pool, "default"), "default (the default)");
        assert_eq!(show(&cfg, Setting::Pool, "work"), "work");
        assert_eq!(show(&cfg, Setting::Probe, "work"), "300s");
        assert_eq!(show(&cfg, Setting::Probe, "default"), "off");
        assert_eq!(show(&cfg, Setting::Hold, "work"), "120s");

        // Distribution steps off → by sessions → by usage → off, on that pool only.
        let mut seen = vec![show(&cfg, Setting::Distribute, "work")];
        for _ in 0..3 {
            Setting::Distribute.step(&mut cfg, "work").unwrap();
            seen.push(show(&cfg, Setting::Distribute, "work"));
        }
        assert_eq!(seen, ["off", "on (by sessions)", "on (by usage)", "off"]);
        assert_eq!(show(&cfg, Setting::Distribute, "default"), "off");

        for row in [Setting::SoonestReset, Setting::Expiry, Setting::Names] {
            assert_eq!(show(&cfg, row, "work"), "off");
            row.step(&mut cfg, "work").unwrap();
            assert_eq!(show(&cfg, row, "work"), "on", "{row:?}");
        }
        assert!(cfg.pool("work").unwrap().soonest_reset && !cfg.pool("default").unwrap().soonest_reset);
        assert!(cfg.session_titles.enabled);

        let mut telemetry = vec![cfg.event_logging];
        for _ in 0..3 {
            Setting::Telemetry.step(&mut cfg, "work").unwrap();
            telemetry.push(cfg.event_logging);
        }
        assert_eq!(telemetry, [EventLogging::Hide, EventLogging::Show, EventLogging::Block, EventLogging::Hide]);
        assert!(Setting::Distribute.step(&mut cfg, "ghost").is_err());
    }

    #[test]
    fn settings_take_typed_values_and_refuse_bad_ones() {
        let mut cfg = Config::default();
        let pool = cfg.default_pool.clone();
        assert_eq!(
            Setting::Threshold.editable(&cfg, &pool),
            percent(match cfg.pool(&pool).unwrap().switch_threshold {
                Threshold::Single(x) => x,
                _ => unreachable!(),
            })
        );
        Setting::Threshold.set(&mut cfg, &pool, " 90% ").unwrap();
        assert_eq!(Setting::Threshold.value(&cfg, &pool), "90%");
        Setting::Threshold.set(&mut cfg, &pool, "unified7d=85").unwrap();
        assert_eq!(Setting::Threshold.value(&cfg, &pool), "default=90%, unified7d=85%");
        assert_eq!(Setting::Threshold.editable(&cfg, &pool), "", "a table is typed afresh, one bucket at a time");
        assert!(Setting::Threshold.set(&mut cfg, &pool, "150").is_err());

        Setting::Probe.set(&mut cfg, &pool, "300").unwrap();
        assert_eq!((Setting::Probe.value(&cfg, &pool), Setting::Probe.editable(&cfg, &pool)), ("300s".into(), "300".into()));
        assert!(Setting::Probe.set(&mut cfg, &pool, "5").is_err(), "under the 30s floor");
        Setting::Probe.set(&mut cfg, &pool, "off").unwrap();
        assert_eq!((Setting::Probe.value(&cfg, &pool), Setting::Probe.editable(&cfg, &pool)), ("off".into(), "off".into()));

        Setting::Hold.set(&mut cfg, &pool, "45").unwrap();
        assert_eq!(cfg.pool(&pool).unwrap().hold_seconds, 45);
        assert!(Setting::Hold.set(&mut cfg, &pool, "soon").is_err());
        Setting::Warmup.set(&mut cfg, &pool, "600").unwrap();
        assert_eq!(cfg.warmup_seconds, 600);
        assert!(Setting::Warmup.set(&mut cfg, &pool, "10").is_err());

        // Every row is one of the three kinds, and only edit rows open for typing.
        for row in Setting::ALL {
            assert_eq!(row.kind() == Kind::Run, row.value(&cfg, &pool).is_empty(), "{row:?}");
        }
    }

    #[test]
    fn credential_files_are_found_in_every_claude_directory() {
        let home = tempfile::tempdir().unwrap();
        for dir in [".claude-work", ".claude", ".claudette", ".config"] {
            std::fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        for dir in [".claude-work", ".claude", ".config"] {
            std::fs::write(home.path().join(dir).join(".credentials.json"), "{}").unwrap();
        }
        assert_eq!(credential_files(home.path()), ["~/.claude/.credentials.json", "~/.claude-work/.credentials.json"]);
        assert!(credential_files(&home.path().join("missing")).is_empty());
    }

    #[test]
    fn input_takes_the_first_line_of_a_paste() {
        let mut input = Input::with("ab");
        input.paste("c\u{7}d\nsecond line");
        assert_eq!(input.text, "abcd");
        input.backspace();
        input.push('\t');
        assert_eq!(input.text, "abc");
    }

    #[test]
    fn account_facts_cover_state_quota_and_usage() {
        let now = 1_000_000_000_000_i64;
        let a = json!({
            "name": "a@x.com", "type": "oauth", "tier": "max_20x", "priority": 2, "disabled": false, "current": true, "blocked": null,
            "email": "a@x.com", "orgName": "Acme", "accountUuid": "uuid-1", "sessions": 3, "inFlight": 1,
            "tokenExpiresAt": now + 3 * 3600 * 1000, "loginExpiresAt": now + 2 * 24 * 3600 * 1000,
            "quota": { "unified5h": { "utilization": 0.58, "resetInSeconds": 10_440 }, "unified7d": { "utilization": 0.22, "resetInSeconds": 453_600 }, "unified7dFable": { "utilization": null, "resetInSeconds": null } },
            "models": { "fable": true, "sonnet": true, "opus": false },
            "probe": { "status": "ok", "lastProbedAt": now - 180_000 },
            "usage": { "totalRequests": 1234, "failedRequests": 12, "inputTokens": 1_700, "cacheReadTokens": 48_800_000, "cacheCreationTokens": 418_000, "outputTokens": 150_500 },
        });
        let facts = text(&account_facts(&a, "work", now));
        for want in [
            "state                 ready, current",
            "pool                  work",
            "priority              2",
            "kind                  oauth, max_20x",
            "serving               3 sessions, 1 request open",
            "organisation          Acme",
            "access token          for another 3h",
            "login                 for another 2d",
            " 58%   resets in 2h54m",
            " 22%   resets in 5d6h",
            "fable ✓   sonnet ✓   opus ✗",
            "quota probe           ok 3m ago",
            "requests              1234 (12 failed)",
            "in 1.7k  cached 48.8M  written 418.0k  out 150.5k  hit 99%",
        ] {
            assert!(facts.contains(want), "missing {want:?} in\n{facts}");
        }
        assert!(!facts.contains("7d fable"), "a window with no reading is left out:\n{facts}");

        let blocked = text(&account_facts(&json!({ "disabled": false, "blocked": "5h quota at 100%", "error": "token rejected" }), "default", now));
        assert!(blocked.contains("state                 5h quota at 100%") && blocked.contains("last error            token rejected"), "{blocked}");
        assert!(text(&account_facts(&json!({ "disabled": true, "blocked": "disabled" }), "default", now)).contains("state                 disabled"));
    }
}
