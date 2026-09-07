//! Configuration and persisted state.
//!
//! The config lives at `~/.config/teamclaude.json` (or `$XDG_CONFIG_HOME`,
//! or `$TEAMCLAUDE_CONFIG`). It holds account credentials, so it is always
//! written `0600` and atomically (temp file + rename) so a crash mid-write can
//! never truncate a credential file. Volatile runtime state (observed quota)
//! goes to a sibling `teamclaude.state.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::security::{random_key, write_private_atomic};

pub const DEFAULT_PORT: u16 = 3456;
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";
pub const DEFAULT_SWITCH_THRESHOLD: f64 = 0.98;
pub const DEFAULT_MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("TEAMCLAUDE_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("teamclaude.json")
}

pub fn config_dir() -> PathBuf {
    config_path().parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

pub fn state_path() -> PathBuf {
    sibling(&config_path(), ".state.json", ".state")
}

#[allow(dead_code)]
pub fn crash_log_path() -> PathBuf {
    sibling(&config_path(), "-crash.log", "-crash.log")
}

fn sibling(cfg: &Path, json_suffix: &str, other_suffix: &str) -> PathBuf {
    let s = cfg.to_string_lossy();
    if let Some(stem) = s.strip_suffix(".json") {
        PathBuf::from(format!("{stem}{json_suffix}"))
    } else {
        PathBuf::from(format!("{s}{other_suffix}"))
    }
}

/// A per-bucket threshold table or a single number.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Threshold {
    Single(f64),
    Table(BTreeMap<String, f64>),
}

impl Default for Threshold {
    fn default() -> Self {
        Threshold::Single(DEFAULT_SWITCH_THRESHOLD)
    }
}

impl Threshold {
    /// Threshold for one bucket key (`unified5h`, `unified7d`, `unified7dFable`,
    /// `unified7dSonnet`, `tokens`, `requests`). Unlisted keys take `default`.
    pub fn for_bucket(&self, bucket: &str) -> f64 {
        match self {
            Threshold::Single(v) => clamp01(*v),
            Threshold::Table(t) => t.get(bucket).or_else(|| t.get("default")).map(|v| clamp01(*v)).unwrap_or(DEFAULT_SWITCH_THRESHOLD),
        }
    }

    /// A cap table differs from a threshold: an unlisted bucket is *uncapped*.
    pub fn cap_for(&self, bucket: &str) -> Option<f64> {
        match self {
            Threshold::Single(v) => Some(clamp01(*v)),
            Threshold::Table(t) => t.get(bucket).or_else(|| t.get("default")).map(|v| clamp01(*v)),
        }
    }
}

fn clamp01(v: f64) -> f64 {
    if !v.is_finite() {
        DEFAULT_SWITCH_THRESHOLD
    } else {
        v.clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClientKey {
    pub name: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageDimension {
    pub name: String,
    pub header: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProxyConfig {
    pub port: u16,
    /// Interface to bind. Defaults to loopback. Binding anything else requires
    /// `apiKey` to be set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Shared key clients present via `x-api-key` (or `Proxy-Authorization` on
    /// CONNECT). Generated on first run.
    pub api_key: String,
    /// Per-client keys; usage is attributed to `name`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub client_keys: Vec<ClientKey>,
    /// Require the proxy key even from loopback clients. Off by default for
    /// compatibility with `teamclaude run`, but recommended on shared hosts.
    pub require_key_on_loopback: bool,
    /// Per-session breakdown in `/teamclaude/status`. Off: it exposes what each
    /// consumer works on to every other key holder.
    pub session_detail: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub usage_dimensions: Vec<UsageDimension>,
    /// Largest request body accepted from a client, in bytes.
    pub max_body_bytes: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            host: None,
            api_key: format!("tc-{}", random_key(24)),
            client_keys: Vec::new(),
            require_key_on_loopback: false,
            session_detail: false,
            usage_dimensions: Vec::new(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AccountType {
    #[default]
    Oauth,
    Apikey,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountConfig {
    /// Stable id tying a config entry to a running account; issued on first read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: AccountType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub priority: i32,
    pub disabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// ms since epoch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Read tokens from this file (Claude Code credential store) instead of
    /// storing them here. Re-read on every reload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_from: Option<String>,
    /// Alternative upstream base URL (third-party Anthropic-compatible API).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_map: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub strip_request_fields: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_usage: Option<Threshold>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RouteConfig {
    pub name: String,
    #[serde(rename = "match")]
    pub patterns: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct StormRamp {
    pub enabled: bool,
    pub start_conc: u32,
    pub step_conc: u32,
    pub step_ms: u64,
    pub window_ms: u64,
}

impl Default for StormRamp {
    fn default() -> Self {
        Self { enabled: true, start_conc: 1, step_conc: 1, step_ms: 250, window_ms: 30_000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct MitmConfig {
    /// Offer only HTTP/1.1 inside the intercepted tunnel. Needed for WebSocket
    /// (Remote Control). Default true: the loopback hop gains nothing from h2.
    pub http1_only: bool,
    /// Allow blind CONNECT tunnels to hosts other than the intercepted ones.
    /// Off by default so the proxy cannot be used as an open relay.
    pub allow_tunnel: bool,
    /// Explicit allow-list of `host:port` for blind tunnels when `allowTunnel`
    /// is on. Empty means any public host on port 443.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tunnel_allow: Vec<String>,
}

impl Default for MitmConfig {
    fn default() -> Self {
        Self { http1_only: true, allow_tunnel: false, tunnel_allow: Vec::new() }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum EventLogging {
    #[default]
    Hide,
    Block,
    Show,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    #[default]
    Body,
    Headers,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub upstream: String,
    pub switch_threshold: Threshold,
    pub hold_seconds: u64,
    pub distribute_sessions: bool,
    pub quota_probe_seconds: u64,
    pub event_logging: EventLogging,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_models: Vec<String>,
    pub accounts: Vec<AccountConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteConfig>,
    pub storm_ramp: StormRamp,
    pub mitm: MitmConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<String>,
    pub log_level: LogLevel,
    pub log_max_body_bytes: u64,
    pub log_retention_hours: u64,
    /// Outbound HTTP(S) proxy for everything sent upstream. `None` = honour
    /// `HTTPS_PROXY`/`ALL_PROXY`; `Some("")` = ignore environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_proxy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
    /// Unknown keys are preserved so a hand-edited file is never stripped.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy: ProxyConfig::default(),
            upstream: DEFAULT_UPSTREAM.to_string(),
            switch_threshold: Threshold::default(),
            hold_seconds: 0,
            distribute_sessions: false,
            quota_probe_seconds: 0,
            event_logging: EventLogging::Hide,
            blocked_models: Vec::new(),
            accounts: Vec::new(),
            routes: Vec::new(),
            storm_ramp: StormRamp::default(),
            mitm: MitmConfig::default(),
            log_dir: None,
            log_level: LogLevel::Body,
            log_max_body_bytes: 262_144,
            log_retention_hours: 72,
            upstream_proxy: None,
            no_proxy: None,
            extra: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Option<Config>> {
        let path = config_path();
        let raw = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut cfg: Config = serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
        cfg.ensure_account_ids();
        cfg.validate()?;
        warn_if_permissive(&path);
        Ok(Some(cfg))
    }

    pub fn load_or_create() -> Result<Config> {
        if let Some(c) = Config::load()? {
            return Ok(c);
        }
        let cfg = Config::default();
        cfg.save()?;
        eprintln!("Created config at {}", config_path().display());
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        let mut json = serde_json::to_vec_pretty(self)?;
        json.push(b'\n');
        write_private_atomic(&path, &json).with_context(|| format!("writing {}", path.display()))
    }

    /// Re-read from disk, apply `f`, and save. Serialised process-wide so two
    /// concurrent token refreshes cannot clobber each other's write.
    pub fn update<F: FnOnce(&mut Config) -> Result<()>>(f: F) -> Result<Config> {
        static LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        let _g = LOCK.lock();
        let mut cfg = Config::load()?.unwrap_or_default();
        f(&mut cfg)?;
        cfg.save()?;
        Ok(cfg)
    }

    pub fn ensure_account_ids(&mut self) {
        let mut seen = std::collections::HashSet::new();
        for a in &mut self.accounts {
            let fresh = !matches!(&a.id, Some(id) if !id.is_empty() && !seen.contains(id));
            if fresh {
                a.id = Some(uuid::Uuid::new_v4().to_string());
            }
            seen.insert(a.id.clone().unwrap());
        }
    }

    pub fn bind_host(&self) -> String {
        std::env::var("TEAMCLAUDE_HOST").ok().filter(|h| !h.is_empty()).or_else(|| self.proxy.host.clone()).unwrap_or_else(|| "127.0.0.1".to_string())
    }

    /// Reject configurations that would leak credentials or misroute traffic.
    pub fn validate(&self) -> Result<()> {
        let host = self.bind_host();
        let loopback = crate::security::is_loopback_host(&host);
        if !loopback && self.proxy.api_key.trim().len() < 16 {
            bail!(
                "proxy.host is {host} (non-loopback) but proxy.apiKey is missing or too short; \
                 an open port would hand out account tokens. Set a key of at least 16 characters."
            );
        }
        validate_upstream(&self.upstream, "upstream")?;
        for a in &self.accounts {
            if let Some(u) = &a.upstream {
                validate_upstream(u, &format!("accounts[{}].upstream", a.name))?;
            }
            if a.name.trim().is_empty() {
                bail!("an account has an empty name");
            }
        }
        for k in &self.proxy.client_keys {
            if k.key.len() < 16 {
                bail!("proxy.clientKeys[{}].key is shorter than 16 characters", k.name);
            }
        }
        for d in &self.proxy.usage_dimensions {
            let h = d.header.to_ascii_lowercase();
            if RESERVED_DIMENSION_HEADERS.contains(&h.as_str()) {
                bail!("proxy.usageDimensions: header {h} is reserved and cannot be a dimension");
            }
        }
        Ok(())
    }

    pub fn find_account_idx(&self, needle: &str) -> Option<usize> {
        let n = needle.trim();
        if n.is_empty() {
            return None;
        }
        // accountUuid/orgUuid
        if let Some((au, ou)) = n.split_once('/') {
            if let Some(i) = self.accounts.iter().position(|a| a.account_uuid.as_deref() == Some(au) && a.org_uuid.as_deref() == Some(ou)) {
                return Some(i);
            }
        }
        self.accounts.iter().position(|a| {
            a.account_uuid.as_deref() == Some(n)
                || a.org_uuid.as_deref() == Some(n)
                || a.id.as_deref() == Some(n)
                || a.name == n
                || a.email.as_deref() == Some(n)
                || a.name.split(" (").next() == Some(n)
        })
    }
}

pub const RESERVED_DIMENSION_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-api-key",
    "x-app",
    "x-claude-code-session-id",
    "x-claude-code-agent-id",
    "x-claude-code-parent-agent-id",
    "x-anthropic-additional-protection",
    "host",
    "content-length",
];

fn validate_upstream(u: &str, what: &str) -> Result<()> {
    let url = url::Url::parse(u).with_context(|| format!("{what}: not a valid URL"))?;
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let h = url.host_str().unwrap_or("");
            if crate::security::is_loopback_host(h) {
                Ok(())
            } else {
                bail!("{what}: plaintext http upstream is only allowed for loopback hosts (got {u})")
            }
        }
        s => bail!("{what}: unsupported scheme {s}"),
    }
}

#[cfg(unix)]
fn warn_if_permissive(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(path) {
        let mode = md.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!("{} was mode {:o}; it holds credentials, tightening to 0600", path.display(), mode);
            crate::security::set_mode(path, 0o600);
        }
    }
}

#[cfg(not(unix))]
fn warn_if_permissive(_path: &Path) {}

/// Persisted runtime state: observed quota per account id.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub version: u32,
    pub saved_at: Option<String>,
    pub accounts: BTreeMap<String, Value>,
    pub client_usage: BTreeMap<String, Value>,
}

impl State {
    pub fn load() -> Result<Option<State>> {
        let path = state_path();
        match std::fs::read(&path) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).unwrap_or_default())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = state_path();
        let mut json = serde_json::to_vec_pretty(self)?;
        json.push(b'\n');
        write_private_atomic(&path, &json).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_table_falls_back_to_default() {
        let mut t = BTreeMap::new();
        t.insert("default".into(), 0.9);
        t.insert("unified7d".into(), 0.8);
        let th = Threshold::Table(t);
        assert_eq!(th.for_bucket("unified7d"), 0.8);
        assert_eq!(th.for_bucket("unified5h"), 0.9);
        assert_eq!(th.cap_for("unified5h"), Some(0.9));
        let th2 = Threshold::Table(BTreeMap::from([("unified7dFable".to_string(), 0.5)]));
        assert_eq!(th2.cap_for("unified5h"), None);
        assert_eq!(th2.for_bucket("unified5h"), DEFAULT_SWITCH_THRESHOLD);
    }

    #[test]
    fn non_loopback_bind_requires_key() {
        let mut c = Config::default();
        c.proxy.host = Some("0.0.0.0".into());
        c.proxy.api_key = "short".into();
        assert!(c.validate().is_err());
        c.proxy.api_key = "tc-a-much-longer-key-value".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn plaintext_upstream_only_for_loopback() {
        let mut c = Config { upstream: "http://example.com".into(), ..Default::default() };
        assert!(c.validate().is_err());
        c.upstream = "http://127.0.0.1:8080".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn unknown_keys_survive_roundtrip() {
        let raw = r#"{"proxy":{"port":1,"apiKey":"tc-0123456789abcdef0123"},"custom":{"a":1},"accounts":[{"name":"x","type":"oauth"}]}"#;
        let mut c: Config = serde_json::from_str(raw).unwrap();
        c.ensure_account_ids();
        assert!(c.accounts[0].id.is_some());
        let out = serde_json::to_string(&c).unwrap();
        assert!(out.contains("\"custom\""));
    }
}
