//! `corrall attach`: the dashboard for a server that is already running.
//!
//! The TUI in `tui.rs` draws whatever its [`Backend`](crate::tui::Backend)
//! hands it. Inside the server that is the process's own pools and activity
//! channel; here it is a daemon reached over its control API: `/corrall/status`
//! polled every couple of seconds, `/corrall/activity` tailed as server-sent
//! events, and `switch`/`reload`/`probe` posted on request. The daemon keeps
//! doing everything; this process holds no accounts and probes nothing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::cli::AttachArgs;
use crate::config::Config;
use crate::proxy::server::Activity;
use crate::security::safe_text;

/// How often the status document is re-fetched.
const STATUS_EVERY: Duration = Duration::from_secs(2);
/// A control call that takes longer than this on loopback is not coming back.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Reconnect delay for the activity feed: doubles from the first to the
/// second value.
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(10);
/// An SSE line longer than this is not one of ours; drop the buffer.
const LINE_MAX: usize = 1 << 20;

/// Entry point for the subcommand.
pub async fn run(args: AttachArgs) -> Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        bail!("attach draws a terminal dashboard; use `corrall status` (or `--json`) from a script");
    }
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet; run `corrall login` first"))?;
    let base = args.url.clone().unwrap_or_else(|| crate::cli::proxy_base(&cfg));
    let key = args.key.clone().unwrap_or_else(|| cfg.proxy.api_key.clone());
    let remote = Remote::new(&base, &key)?;
    // Fail here, plainly, rather than draw an empty dashboard around a
    // daemon that is not there.
    remote
        .fetch_status()
        .await
        .map_err(|e| anyhow!("no corrall answering at {base}: {e}\nStart one with `corrall server`, or point --url at the right address"))?;
    remote.start();
    let activity_file = args
        .activity_log
        .as_ref()
        .map(|p| {
            let f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            crate::security::set_mode(std::path::Path::new(p), 0o600);
            Ok::<_, std::io::Error>(f)
        })
        .transpose()
        .context("opening --activity-log")?;
    let tui = crate::tui::Tui::new(crate::tui::Backend::Remote(remote), activity_file);
    // Nothing else in this process listens for shutdown; `q` just returns.
    let (tx, _rx) = tokio::sync::watch::channel(false);
    tui.run(tx).await
}

/// What the poller last learned about the daemon.
struct Snapshot {
    status: Value,
    fetched: Option<Instant>,
    error: Option<String>,
}

/// How the link to the daemon looks right now, for the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// The last status fetch succeeded this long ago.
    Fresh(Duration),
    /// Fetches are failing; the table shows the last good document.
    Lost { since: Duration, why: String },
    /// Nothing fetched yet.
    Connecting,
}

/// A running daemon seen through its control API.
#[derive(Clone)]
pub struct Remote {
    base: String,
    key: String,
    client: reqwest::Client,
    snap: Arc<Mutex<Snapshot>>,
    activity: broadcast::Sender<Activity>,
}

impl Remote {
    /// `base` is the daemon's origin, `http://host:port`. Does no I/O.
    pub fn new(base: &str, key: &str) -> Result<Remote> {
        let base = base.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            bail!("--url must start with http:// or https:// (got {})", safe_text(&base, 60));
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        // No upstream proxy and no overall timeout: the feed stays open for
        // as long as the dashboard is.
        let client =
            reqwest::Client::builder().use_rustls_tls().no_proxy().redirect(reqwest::redirect::Policy::none()).connect_timeout(CALL_TIMEOUT).build()?;
        let (activity, _) = broadcast::channel(512);
        Ok(Remote { base, key: key.to_string(), client, snap: Arc::new(Mutex::new(Snapshot { status: Value::Null, fetched: None, error: None })), activity })
    }

    /// Where this is attached, for the footer.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Start the status poller and the activity tail. Both run until the
    /// process exits.
    pub fn start(&self) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(STATUS_EVERY);
            loop {
                tick.tick().await;
                let _ = me.fetch_status().await;
            }
        });
        let me = self.clone();
        tokio::spawn(async move { me.tail_activity().await });
    }

    /// The last status document (or `Null` before the first fetch).
    pub fn status(&self) -> Value {
        self.snap.lock().status.clone()
    }

    pub fn link(&self) -> Link {
        let s = self.snap.lock();
        match (&s.error, s.fetched) {
            (None, Some(t)) => Link::Fresh(t.elapsed()),
            (Some(why), Some(t)) => Link::Lost { since: t.elapsed(), why: why.clone() },
            (Some(why), None) => Link::Lost { since: Duration::ZERO, why: why.clone() },
            (None, None) => Link::Connecting,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Activity> {
        self.activity.subscribe()
    }

    /// Fetch `/corrall/status` once and record the outcome.
    pub async fn fetch_status(&self) -> Result<Value> {
        match self.get("/corrall/status").await {
            Ok(v) => {
                let mut s = self.snap.lock();
                s.status = v.clone();
                s.fetched = Some(Instant::now());
                s.error = None;
                Ok(v)
            }
            Err(e) => {
                let msg = safe_text(&e.to_string(), 120);
                self.snap.lock().error = Some(msg.clone());
                Err(anyhow!("{msg}"))
            }
        }
    }

    /// `POST /corrall/switch`. Returns the account's name and, if the
    /// switch could not take effect yet, why.
    pub async fn switch(&self, pool: &str, id: &str) -> Result<(String, Option<String>)> {
        let r = self.post("/corrall/switch", json!({ "account": id, "pool": pool })).await?;
        if !r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            bail!("{}", r.get("error").and_then(Value::as_str).unwrap_or("switch failed"));
        }
        let name = r.get("account").and_then(Value::as_str).unwrap_or(id).to_string();
        let blocked = r.get("blocked").and_then(Value::as_str).map(str::to_string);
        Ok((name, blocked))
    }

    /// `POST /corrall/reload`; the number of accounts the reload added.
    pub async fn reload(&self) -> Result<usize> {
        let r = self.post("/corrall/reload", json!({})).await?;
        if !r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            bail!("{}", r.get("error").and_then(Value::as_str).unwrap_or("reload failed"));
        }
        Ok(r.get("added").and_then(Value::as_u64).unwrap_or(0) as usize)
    }

    /// `POST /corrall/probe`. The daemon's one-line answer either way: a
    /// refused probe (already running, too soon) is an outcome, not an error.
    pub async fn probe(&self) -> Result<String> {
        let r = self.post("/corrall/probe", json!({})).await?;
        if r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return Ok("probing all OAuth accounts…".into());
        }
        Ok(r.get("error").and_then(Value::as_str).unwrap_or("probe refused").to_string())
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let r = self.client.get(format!("{}{path}", self.base)).header("x-api-key", &self.key).timeout(CALL_TIMEOUT).send().await?;
        Self::json_of(r).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let r = self.client.post(format!("{}{path}", self.base)).header("x-api-key", &self.key).timeout(CALL_TIMEOUT).json(&body).send().await?;
        Self::json_of(r).await
    }

    async fn json_of(r: reqwest::Response) -> Result<Value> {
        let status = r.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("HTTP 401: the proxy key was refused (pass --key)");
        }
        let v: Value = r.json().await.unwrap_or(Value::Null);
        // Control routes answer 200 with `ok: false` for a refused action and
        // 4xx/5xx for a bad request; both carry `error`.
        if !status.is_success() && v.get("error").is_none() {
            bail!("HTTP {status}");
        }
        Ok(v)
    }

    /// Follow `/corrall/activity` forever, reconnecting when it drops.
    async fn tail_activity(&self) {
        let mut delay = RECONNECT_MIN;
        let mut was_up = false;
        loop {
            match self.client.get(format!("{}/corrall/activity", self.base)).header("x-api-key", &self.key).send().await {
                Ok(r) if r.status().is_success() => {
                    if was_up {
                        self.say("attach: activity feed reconnected");
                    }
                    was_up = true;
                    delay = RECONNECT_MIN;
                    let why = self.pump(r).await;
                    self.say(&format!("WARN attach: activity feed dropped ({why}); reconnecting"));
                }
                Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                    // A daemon from before the feed existed. Status still
                    // polls, so the table works; say why the pane is quiet.
                    self.say(&format!(
                        "WARN attach: this server predates /corrall/activity; update and restart it for a live pane; retrying in {}s",
                        delay.as_secs()
                    ));
                }
                Ok(r) => {
                    self.say(&format!("WARN attach: activity feed refused (HTTP {}); retrying in {}s", r.status(), delay.as_secs()));
                }
                Err(e) => {
                    if was_up {
                        self.say(&format!("WARN attach: activity feed unreachable ({}); retrying in {}s", safe_text(&e.to_string(), 80), delay.as_secs()));
                    }
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(RECONNECT_MAX);
        }
    }

    /// Read one feed connection until it ends; returns why.
    async fn pump(&self, r: reqwest::Response) -> String {
        let mut stream = r.bytes_stream();
        let mut parser = SseParser::default();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => return safe_text(&e.to_string(), 80),
            };
            for ev in parser.feed(&chunk) {
                if let Some(a) = Activity::from_json(&ev) {
                    let _ = self.activity.send(a);
                }
            }
        }
        "closed by server".into()
    }

    /// A line about the link itself, into the same pane as the daemon's.
    fn say(&self, line: &str) {
        let _ = self.activity.send(Activity::Log(line.to_string()));
    }
}

/// Server-sent events, the little we use: `data:` lines accumulate, a blank
/// line ends an event, `:` comments are skipped. Everything else is ignored.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    /// Feed bytes; get back every complete event they finished, as JSON.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Value> {
        let mut out = Vec::new();
        self.buf.extend_from_slice(chunk);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line[..nl]).trim_end_matches('\r').to_string();
            if line.is_empty() {
                if !self.data.is_empty() {
                    let joined = self.data.join("\n");
                    self.data.clear();
                    if let Ok(v) = serde_json::from_str::<Value>(&joined) {
                        out.push(v);
                    }
                }
            } else if let Some(d) = line.strip_prefix("data:") {
                self.data.push(d.strip_prefix(' ').unwrap_or(d).to_string());
            }
        }
        if self.buf.len() > LINE_MAX {
            self.buf.clear();
            self.data.clear();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parser_reassembles_events_across_chunks_and_skips_comments() {
        let mut p = SseParser::default();
        assert!(p.feed(b": corrall activity\n\n").is_empty());
        let mut got = p.feed(b"data: {\"type\":\"log\",\"li");
        assert!(got.is_empty());
        got.extend(p.feed(b"ne\":\"hello\"}\r\n\r\n: ping\n\ndata: {\"type\":\"end\",\"id\":\"a\",\"ok\":true}\n"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["line"], "hello");
        let rest = p.feed(b"\n");
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0]["type"], "end");
    }

    #[test]
    fn sse_parser_drops_garbage_that_is_not_json() {
        let mut p = SseParser::default();
        assert!(p.feed(b"data: not json\n\n").is_empty());
        assert!(p.feed(b"event: hello\nid: 1\n\n").is_empty());
    }

    #[test]
    fn remote_requires_a_scheme() {
        assert!(Remote::new("127.0.0.1:3456", "k").is_err());
        assert!(Remote::new("http://127.0.0.1:3456/", "k").is_ok());
        assert_eq!(Remote::new("http://127.0.0.1:3456/", "k").unwrap().base(), "http://127.0.0.1:3456");
    }
}
