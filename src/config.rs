//! Configuration and persisted state.
//!
//! The config lives at `~/.config/corrall.json` (or `$XDG_CONFIG_HOME`,
//! or `$CORRALL_CONFIG`). It holds account credentials, so it is always
//! written `0600` and atomically (temp file + rename) so a crash mid-write can
//! never truncate a credential file. Volatile runtime state (observed quota)
//! goes to a sibling `corrall.state.json`.

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
/// The pool that serves requests arriving without a `/pool/<name>` prefix.
pub const DEFAULT_POOL: &str = "default";
pub const MAX_POOL_NAME_LEN: usize = 32;

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("CORRALL_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("corrall.json")
}

/// Where this config lived before the project was renamed from TeamClaude:
/// `teamclaude.json` beside a default-named `corrall.json` that does not exist
/// yet. `None` when there is nothing to migrate or the path was overridden.
pub fn legacy_config_path(path: &Path) -> Option<PathBuf> {
    if path.file_name()? != "corrall.json" || path.exists() {
        return None;
    }
    let legacy = path.with_file_name("teamclaude.json");
    legacy.is_file().then_some(legacy)
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

/// Short-lived cache of the latest release tag, so repeated `status` calls do
/// not each shell out to `gh` while the daemon's own check is still pending.
pub fn update_cache_path() -> PathBuf {
    sibling(&config_path(), ".update-check.json", ".update-check")
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
    /// compatibility with `corrall run`, but recommended on shared hosts.
    pub require_key_on_loopback: bool,
    /// Per-session breakdown in `/corrall/status`. Off: it exposes what each
    /// consumer works on to every other key holder.
    pub session_detail: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub usage_dimensions: Vec<UsageDimension>,
    /// Largest request body accepted from a client, in bytes.
    pub max_body_bytes: u64,
}

/// The shared client key. The `tc-` prefix is what tells an operator what they
/// are looking at in a config file or an environment variable.
pub fn new_api_key() -> String {
    format!("tc-{}", random_key(24))
}

/// Whether a config document on disk still owes itself a shared key. A key the
/// operator set is left alone however short it is — `validate` is what decides
/// whether it is long enough for the address being bound.
fn needs_api_key(doc: &Value) -> bool {
    match doc.get("proxy").and_then(|p| p.get("apiKey")).and_then(Value::as_str) {
        Some(k) => k.trim().is_empty(),
        None => true,
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            host: None,
            api_key: new_api_key(),
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
    /// `codex` for an OpenAI Codex subscription; absent = Anthropic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// ChatGPT account id (Codex).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
}

impl AccountConfig {
    pub fn is_codex(&self) -> bool {
        self.provider.as_deref() == Some("codex")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ExpiryRouting {
    pub enabled: bool,
    pub tolerance: f64,
    pub preempt: bool,
}

impl Default for ExpiryRouting {
    fn default() -> Self {
        Self { enabled: false, tolerance: 1.5, preempt: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionTitles {
    pub enabled: bool,
    pub width: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects_dir: Option<String>,
}

impl Default for SessionTitles {
    fn default() -> Self {
        Self { enabled: false, width: 18, projects_dir: None }
    }
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

/// One pool: its own accounts, its own rotation settings.
///
/// Everything here used to live at the top level of the config, when there was
/// exactly one implicit fleet. A pre-pools file is lifted into `pools.default`
/// on load (see [`migrate_pools`]), so the on-disk shape below is what every
/// install converges to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PoolConfig {
    pub switch_threshold: Threshold,
    pub hold_seconds: u64,
    pub distribute_sessions: bool,
    pub quota_probe_seconds: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_models: Vec<String>,
    pub accounts: Vec<AccountConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteConfig>,
    pub storm_ramp: StormRamp,
    pub expiry_routing: ExpiryRouting,
    /// When set, `corrall env`/`run` can select this pool from the launch
    /// context instead of being told which one to use. The default pool is the
    /// catch-all and needs no rules.
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub match_rules: Option<PoolMatch>,
    /// Unknown keys are preserved so a hand-edited pool is never stripped.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Predicates for auto-selecting a pool at launch. A pool matches when **any**
/// listed path, remote or environment condition matches — the shape a
/// hand-written wrapper usually takes ("this directory, or anything with that
/// git remote"). Empty groups are ignored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PoolMatch {
    /// Directories. Matches when the working directory *is* one of them or is
    /// nested under it. A leading `~` is expanded, and a trailing `/*` or `/**`
    /// is ignored so `~/Projects/foo` and `~/Projects/foo/**` behave alike.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Regular expressions tested against `git remote get-url origin` run in
    /// the working directory, e.g. `(?i)^git@github\.com:acme/`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<String>,
    /// Environment variable name → regular expression its value must match. An
    /// empty pattern means "matches whenever the variable is set".
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl PoolMatch {
    /// True when no rule is present, which can never match anything.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.remotes.is_empty() && self.env.is_empty()
    }

    /// Reject patterns that cannot compile, at config-load time rather than at
    /// the next launch: a bad regex in a file nobody re-reads would otherwise
    /// turn into a pool that silently never matches.
    pub fn validate(&self, pool: &str) -> Result<()> {
        for r in &self.remotes {
            regex::Regex::new(r).with_context(|| format!("pools.{pool}.match.remotes: {r:?} is not a valid regular expression"))?;
        }
        for (k, r) in &self.env {
            if r.is_empty() {
                continue;
            }
            regex::Regex::new(r).with_context(|| format!("pools.{pool}.match.env.{k}: {r:?} is not a valid regular expression"))?;
        }
        Ok(())
    }
}

/// Pool names are routed under the fixed `/pool/` URL keyword, which no real
/// API path can start with, so **no name is reserved** — a pool may be called
/// `v1` or `api`. Only the charset is fixed: 1–32 characters of lowercase
/// ASCII letters, digits and hyphens, not starting or ending with a hyphen.
///
/// Uppercase is rejected rather than folded to lowercase: the router compares
/// names byte-for-byte, and folding would let a case-insensitive filesystem
/// disagree with the router about which pool is which.
pub fn validate_pool_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("pool name is empty");
    }
    if name.len() > MAX_POOL_NAME_LEN {
        bail!("pool name {name:?} is longer than {MAX_POOL_NAME_LEN} characters");
    }
    if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        bail!("pool name {name:?} may only contain lowercase letters, digits and hyphens");
    }
    if name.starts_with('-') || name.ends_with('-') {
        bail!("pool name {name:?} may not start or end with a hyphen");
    }
    Ok(())
}

/// Keys that lived at the top level before pools existed and now belong to a
/// pool.
const LEGACY_POOL_KEYS: &[&str] =
    &["switchThreshold", "holdSeconds", "distributeSessions", "quotaProbeSeconds", "blockedModels", "accounts", "routes", "stormRamp", "expiryRouting"];

/// Upgrade a pre-pools document in place. Returns whether anything moved, which
/// makes [`Config::load`] rewrite the file so the migration happens once.
///
/// The absence of a `pools` key is the signal: a file that already has one is
/// left alone, so a hand-written config that also keeps stale top-level keys
/// keeps them (inert, preserved through `Config::extra`) instead of having them
/// silently merged into a pool.
fn migrate_pools(doc: &mut Value) -> bool {
    let Some(obj) = doc.as_object_mut() else { return false };
    let mut changed = false;
    if !obj.contains_key("pools") {
        let mut pool = serde_json::Map::new();
        for k in LEGACY_POOL_KEYS {
            if let Some(v) = obj.remove(*k) {
                pool.insert((*k).to_string(), v);
            }
        }
        let mut pools = serde_json::Map::new();
        pools.insert(DEFAULT_POOL.to_string(), Value::Object(pool));
        obj.insert("pools".to_string(), Value::Object(pools));
        changed = true;
    }
    if !obj.get("defaultPool").and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty()) {
        obj.insert("defaultPool".to_string(), Value::String(DEFAULT_POOL.to_string()));
        changed = true;
    }
    changed
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub upstream: String,
    /// Pool serving requests with no `/pool/<name>` prefix.
    pub default_pool: String,
    pub pools: BTreeMap<String, PoolConfig>,
    pub event_logging: EventLogging,
    pub session_titles: SessionTitles,
    /// Keep-warm interval in seconds (0 = off). Spends a little quota.
    pub warmup_seconds: u64,
    /// Check GitHub for a newer release and say so in status and the TUI: the
    /// daemon does so once a day, and `status` does one on demand while the
    /// daemon's first check is still pending. Notify-only: nothing is ever
    /// installed automatically. Set to false to disable both.
    pub update_check: bool,
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
            default_pool: DEFAULT_POOL.to_string(),
            pools: BTreeMap::from([(DEFAULT_POOL.to_string(), PoolConfig::default())]),
            event_logging: EventLogging::Hide,
            session_titles: SessionTitles::default(),
            warmup_seconds: 0,
            update_check: true,
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Never start a fresh, empty config next to a pre-rename one:
                // the accounts in it would silently stop being served.
                if let Some(legacy) = legacy_config_path(&path) {
                    bail!(
                        "{} does not exist but the pre-rename {} does; run scripts/install.sh to migrate it, or move it (with its .state.json and the teamclaude-*.pem certificates) to the corrall.* names yourself",
                        path.display(),
                        legacy.display()
                    );
                }
                return Ok(None);
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut doc: Value = serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
        let migrated = migrate_pools(&mut doc);
        // A file with no `proxy.apiKey` picks up a fresh one from
        // `ProxyConfig::default()` on every load, which is worse than none: it
        // is long enough to satisfy an off-loopback bind, yet no client can
        // present a key that changes on every restart. Generate it once and
        // write it back, exactly as the first run does.
        let keyless = needs_api_key(&doc);
        let mut cfg: Config = serde_json::from_value(doc).with_context(|| format!("parsing {}", path.display()))?;
        if keyless {
            cfg.proxy.api_key = new_api_key();
        }
        cfg.ensure_pools();
        cfg.ensure_account_ids();
        cfg.validate()?;
        warn_if_permissive(&path);
        if migrated || keyless {
            // Rewrite once so a pre-pools install lands on the new shape, and a
            // keyless one on a stable key, without any manual migration.
            // Failure is not fatal: we already hold a usable config in memory.
            if let Err(e) = cfg.save() {
                tracing::warn!("could not rewrite {}: {e:#}", path.display());
            } else if keyless {
                tracing::info!("generated proxy.apiKey in {}", path.display());
            }
        }
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
        let _file_lock = crate::security::FileLock::acquire(&config_path()).context("locking config")?;
        let mut cfg = Config::load()?.unwrap_or_default();
        f(&mut cfg)?;
        cfg.save()?;
        Ok(cfg)
    }

    pub fn ensure_account_ids(&mut self) {
        // Ids must be unique across the whole file, not just within a pool:
        // state, session affinity and pins are all keyed by id alone.
        let mut seen = std::collections::HashSet::new();
        for p in self.pools.values_mut() {
            for a in &mut p.accounts {
                let fresh = !matches!(&a.id, Some(id) if !id.is_empty() && !seen.contains(id));
                if fresh {
                    a.id = Some(uuid::Uuid::new_v4().to_string());
                }
                seen.insert(a.id.clone().unwrap());
            }
        }
    }

    /// Guarantee the invariant the router depends on: at least one pool exists
    /// and `default_pool` names one of them.
    pub fn ensure_pools(&mut self) {
        if self.default_pool.trim().is_empty() {
            self.default_pool = DEFAULT_POOL.to_string();
        }
        if self.pools.is_empty() {
            self.pools.insert(self.default_pool.clone(), PoolConfig::default());
        } else if !self.pools.contains_key(&self.default_pool) {
            // A `defaultPool` naming a pool that is not there would leave every
            // unprefixed request unroutable; adopt the first pool instead.
            if let Some(first) = self.pools.keys().next().cloned() {
                tracing::warn!("defaultPool \"{}\" is not configured; using \"{first}\"", self.default_pool);
                self.default_pool = first;
            }
        }
    }

    pub fn pool(&self, name: &str) -> Option<&PoolConfig> {
        self.pools.get(name)
    }

    pub fn pool_mut(&mut self, name: &str) -> Option<&mut PoolConfig> {
        self.pools.get_mut(name)
    }

    /// The pool serving `name`, falling back to the default pool when `name` is
    /// absent or unknown. Never fails: an unroutable request would be worse
    /// than one served by the default fleet.
    pub fn pool_or_default(&self, name: Option<&str>) -> &PoolConfig {
        name.and_then(|n| self.pools.get(n)).or_else(|| self.pools.get(&self.default_pool)).or_else(|| self.pools.values().next()).unwrap_or_else(|| {
            static EMPTY: std::sync::OnceLock<PoolConfig> = std::sync::OnceLock::new();
            EMPTY.get_or_init(PoolConfig::default)
        })
    }

    /// Pool names with the default pool first, the rest sorted. This is the
    /// display and resolution order everywhere: status, TUI, `pool list`.
    pub fn pool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.pools.keys().filter(|k| *k != &self.default_pool).cloned().collect();
        names.sort();
        if self.pools.contains_key(&self.default_pool) {
            names.insert(0, self.default_pool.clone());
        }
        names
    }

    /// Create the pool if it is missing, after checking the name.
    pub fn ensure_pool(&mut self, name: &str) -> Result<&mut PoolConfig> {
        validate_pool_name(name)?;
        Ok(self.pools.entry(name.to_string()).or_default())
    }

    /// Move a pool's configuration under a new name. The `defaultPool` pointer
    /// follows the rename so an unprefixed request still resolves. Fails if the
    /// old name is unknown or the new name already exists; the config is left
    /// untouched on failure. Live runtime state (sessions, quota observations)
    /// is re-keyed separately — see [`crate::pools::Pools::rename_pool`] — so it
    /// survives the reload rather than being dropped and rebuilt.
    pub fn rename_pool(&mut self, old: &str, new: &str) -> Result<()> {
        if old == new {
            return Ok(());
        }
        validate_pool_name(new)?;
        if self.pools.contains_key(new) {
            bail!("a pool named {new:?} already exists");
        }
        let Some(pool) = self.pools.remove(old) else {
            bail!("no pool named {old:?}");
        };
        self.pools.insert(new.to_string(), pool);
        if self.default_pool == old {
            self.default_pool = new.to_string();
        }
        Ok(())
    }

    /// Every account in the file with the pool that owns it.
    pub fn all_accounts(&self) -> impl Iterator<Item = (&str, &AccountConfig)> {
        self.pools.iter().flat_map(|(name, p)| p.accounts.iter().map(move |a| (name.as_str(), a)))
    }

    pub fn bind_host(&self) -> String {
        std::env::var("CORRALL_HOST").ok().filter(|h| !h.is_empty()).or_else(|| self.proxy.host.clone()).unwrap_or_else(|| "127.0.0.1".to_string())
    }

    /// The host a client *on this machine* dials to reach the listener.
    ///
    /// A wildcard bind (`0.0.0.0`, `::`) is an address nothing can connect to,
    /// so a local client reaches the listener over loopback instead, and
    /// `localhost` resolves there anyway — both give the `127.0.0.1` every
    /// release has emitted. Any other host is kept as it stands: when the
    /// listener only answers on `192.168.1.10` (or on `::1`), `127.0.0.1` is
    /// nowhere to dial.
    pub fn dial_host(&self) -> String {
        let host = self.bind_host();
        let h = host.trim().trim_start_matches('[').trim_end_matches(']');
        let wildcard = h.is_empty() || h.parse::<std::net::IpAddr>().map(|ip| ip.is_unspecified()).unwrap_or(false);
        if wildcard || h.eq_ignore_ascii_case("localhost") {
            "127.0.0.1".to_string()
        } else {
            h.to_string()
        }
    }

    /// [`Self::dial_host`] with the port, ready to drop into a URL.
    pub fn dial_authority(&self) -> String {
        let h = self.dial_host();
        // A bare IPv6 literal has to be bracketed before it can carry a port.
        if h.contains(':') {
            format!("[{h}]:{}", self.proxy.port)
        } else {
            format!("{h}:{}", self.proxy.port)
        }
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
        if self.pools.is_empty() {
            bail!("no pools are configured; there must be at least one");
        }
        if !self.pools.contains_key(&self.default_pool) {
            bail!("defaultPool \"{}\" is not one of the configured pools", self.default_pool);
        }
        let mut seen_names: BTreeMap<&str, &str> = BTreeMap::new();
        for (pool, p) in &self.pools {
            validate_pool_name(pool).with_context(|| "pools".to_string())?;
            if let Some(m) = &p.match_rules {
                m.validate(pool)?;
                // The default pool is the fallback every unmatched launch lands
                // on, so rules on it can only ever be dead weight.
                if pool == &self.default_pool && !m.is_empty() {
                    tracing::warn!("pool \"{pool}\" is the default pool; its match rules are never consulted");
                }
            }
            for a in &p.accounts {
                if let Some(u) = &a.upstream {
                    validate_upstream(u, &format!("pools.{pool}.accounts[{}].upstream", a.name))?;
                }
                if a.name.trim().is_empty() {
                    bail!("an account in pool \"{pool}\" has an empty name");
                }
                // Names address accounts in `switch`, `/tc-acct/<pin>` and route
                // config, none of which is pool-qualified. A duplicate is only
                // ambiguous, not unusable, so warn rather than reject a config
                // that was legal before pools existed.
                if let Some(other) = seen_names.insert(a.name.as_str(), pool.as_str()) {
                    tracing::warn!("account name \"{}\" is used in both pool \"{other}\" and pool \"{pool}\"; lookups by name will pick one of them", a.name);
                }
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

    /// Locate an account by name, id, email or uuid within one pool.
    pub fn find_account_idx_in(&self, pool: &str, needle: &str) -> Option<usize> {
        let accounts = &self.pools.get(pool)?.accounts;
        let n = needle.trim();
        if n.is_empty() {
            return None;
        }
        // accountUuid/orgUuid
        if let Some((au, ou)) = n.split_once('/') {
            if let Some(i) = accounts.iter().position(|a| a.account_uuid.as_deref() == Some(au) && a.org_uuid.as_deref() == Some(ou)) {
                return Some(i);
            }
        }
        accounts.iter().position(|a| {
            a.account_uuid.as_deref() == Some(n)
                || a.org_uuid.as_deref() == Some(n)
                || a.id.as_deref() == Some(n)
                || a.name == n
                || a.email.as_deref() == Some(n)
                || a.name.split(" (").next() == Some(n)
        })
    }

    /// Locate an account anywhere in the file. The default pool is searched
    /// first so that a name duplicated across pools resolves the way it did
    /// before pools existed.
    pub fn find_account(&self, needle: &str) -> Option<(String, usize)> {
        self.pool_names().into_iter().find_map(|p| self.find_account_idx_in(&p, needle).map(|i| (p, i)))
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

/// One pool's persisted runtime state: observed quota per account id.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct PoolState {
    pub accounts: BTreeMap<String, Value>,
    pub client_usage: BTreeMap<String, Value>,
    pub dimension_usage: Value,
}

/// Persisted runtime state.
///
/// The default pool's state stays flattened at the top level, byte-for-byte
/// where a pre-pools state file put it, so an existing file restores with no
/// migration and no lost quota. Named pools nest under `pools`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub version: u32,
    pub saved_at: Option<String>,
    #[serde(flatten)]
    pub default: PoolState,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub pools: BTreeMap<String, PoolState>,
}

impl State {
    pub fn pool(&self, name: &str, default_pool: &str) -> Option<&PoolState> {
        if name == default_pool {
            Some(&self.default)
        } else {
            self.pools.get(name)
        }
    }

    pub fn set_pool(&mut self, name: &str, default_pool: &str, st: PoolState) {
        if name == default_pool {
            self.default = st;
        } else {
            self.pools.insert(name.to_string(), st);
        }
    }

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
    use serde_json::json;

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
        let mut c = parse_migrating(raw);
        c.ensure_account_ids();
        assert!(c.pools[DEFAULT_POOL].accounts[0].id.is_some());
        let out = serde_json::to_string(&c).unwrap();
        assert!(out.contains("\"custom\""));
    }

    /// The `load()` path minus the filesystem: migrate the raw document, then
    /// deserialize.
    fn parse_migrating(raw: &str) -> Config {
        let mut doc: Value = serde_json::from_str(raw).unwrap();
        migrate_pools(&mut doc);
        let mut c: Config = serde_json::from_value(doc).unwrap();
        c.ensure_pools();
        c
    }

    #[test]
    fn a_config_without_a_key_asks_for_one_generated() {
        let doc = |raw: &str| serde_json::from_str::<Value>(raw).unwrap();
        assert!(needs_api_key(&doc(r#"{"accounts":[]}"#)), "no proxy block at all");
        assert!(needs_api_key(&doc(r#"{"proxy":{"port":3456}}"#)), "no apiKey");
        assert!(needs_api_key(&doc(r#"{"proxy":{"apiKey":"  "}}"#)), "blank apiKey");
        // A key the operator set is never regenerated, however short: how long
        // it has to be is `validate`'s call, and it depends on the bind address.
        assert!(!needs_api_key(&doc(r#"{"proxy":{"apiKey":"short"}}"#)));
        assert!(!needs_api_key(&doc(r#"{"proxy":{"apiKey":"tc-0123456789abcdef0123"}}"#)));

        let k = new_api_key();
        assert!(k.starts_with("tc-"), "{k}");
        assert!(k.len() >= 16, "{k}");
        assert_ne!(k, new_api_key());
    }

    #[test]
    fn legacy_config_migrates_into_default_pool() {
        let raw = r#"{
            "proxy": {"port": 3456, "apiKey": "tc-0123456789abcdef0123"},
            "upstream": "https://api.anthropic.com",
            "switchThreshold": 0.75,
            "holdSeconds": 30,
            "distributeSessions": true,
            "quotaProbeSeconds": 600,
            "blockedModels": ["*opus*"],
            "accounts": [{"name": "work", "type": "oauth", "priority": 5}],
            "routes": [{"name": "cheap", "match": ["*haiku*"]}],
            "expiryRouting": {"enabled": true},
            "warmupSeconds": 90
        }"#;
        let c = parse_migrating(raw);

        assert_eq!(c.default_pool, DEFAULT_POOL);
        assert_eq!(c.pools.len(), 1);
        let p = &c.pools[DEFAULT_POOL];
        assert_eq!(p.switch_threshold, Threshold::Single(0.75));
        assert_eq!(p.hold_seconds, 30);
        assert!(p.distribute_sessions);
        assert_eq!(p.quota_probe_seconds, 600);
        assert_eq!(p.blocked_models, vec!["*opus*"]);
        assert_eq!(p.accounts.len(), 1);
        assert_eq!(p.accounts[0].name, "work");
        assert_eq!(p.accounts[0].priority, 5);
        assert_eq!(p.routes.len(), 1);
        assert!(p.expiry_routing.enabled);
        // Daemon-global keys stay at the top level.
        assert_eq!(c.warmup_seconds, 90);
        assert_eq!(c.proxy.port, 3456);

        // The migrated keys are gone from the top level, not duplicated into
        // `extra` where they would be written back out.
        let out = serde_json::to_string(&c).unwrap();
        let back: Value = serde_json::from_str(&out).unwrap();
        assert!(back.get("accounts").is_none());
        assert!(back.get("switchThreshold").is_none());
        assert_eq!(back.pointer("/pools/default/holdSeconds").and_then(Value::as_u64), Some(30));
    }

    #[test]
    fn migration_is_idempotent_and_preserves_named_pools() {
        let legacy = r#"{"proxy":{"apiKey":"tc-0123456789abcdef0123"},"accounts":[{"name":"a","type":"oauth"}]}"#;
        let once = parse_migrating(legacy);
        let json = serde_json::to_string(&once).unwrap();

        // Second pass: the file already has `pools`, so nothing moves.
        let mut doc: Value = serde_json::from_str(&json).unwrap();
        assert!(!migrate_pools(&mut doc), "a migrated file must not migrate again");
        let twice: Config = serde_json::from_value(doc).unwrap();
        assert_eq!(once, twice);

        // A file that already declares pools is left alone entirely.
        let modern = r#"{"proxy":{"apiKey":"tc-0123456789abcdef0123"},"defaultPool":"main",
            "pools":{"main":{"accounts":[]},"work":{"switchThreshold":0.5}}}"#;
        let c = parse_migrating(modern);
        assert_eq!(c.default_pool, "main");
        assert_eq!(c.pool_names(), vec!["main", "work"]);
        assert_eq!(c.pools["work"].switch_threshold, Threshold::Single(0.5));
    }

    #[test]
    fn missing_default_pool_backfills() {
        // `pools` present but no `defaultPool`: the name is filled in.
        let c = parse_migrating(r#"{"proxy":{"apiKey":"tc-0123456789abcdef0123"},"pools":{"default":{}}}"#);
        assert_eq!(c.default_pool, DEFAULT_POOL);
        assert!(c.validate().is_ok());

        // `defaultPool` naming a pool that is not there: adopt a real one so no
        // request is left unroutable.
        let c = parse_migrating(r#"{"proxy":{"apiKey":"tc-0123456789abcdef0123"},"defaultPool":"gone","pools":{"work":{}}}"#);
        assert_eq!(c.default_pool, "work");
        assert!(c.validate().is_ok());
    }

    #[test]
    fn a_pre_rename_config_is_detected_only_beside_a_missing_default_one() {
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("corrall.json");
        assert_eq!(legacy_config_path(&new), None, "nothing to migrate");
        std::fs::write(dir.path().join("teamclaude.json"), b"{}").unwrap();
        assert_eq!(legacy_config_path(&new), Some(dir.path().join("teamclaude.json")));
        assert_eq!(legacy_config_path(&dir.path().join("other.json")), None, "overridden paths are never redirected");
        std::fs::write(&new, b"{}").unwrap();
        assert_eq!(legacy_config_path(&new), None, "an existing corrall.json wins");
    }

    #[test]
    fn pool_names_are_charset_checked_but_never_reserved() {
        // No name is reserved: routing lives under the /pool/ keyword.
        for ok in ["default", "work", "v1", "api", "corrall", "a", "a-b-c", "pool", "x9", &"a".repeat(MAX_POOL_NAME_LEN)] {
            assert!(validate_pool_name(ok).is_ok(), "{ok} should be a legal pool name");
        }
        for bad in ["", "Work", "WORK", "-work", "work-", "wo rk", "work/other", "work.io", "work_id", "wörk", &"a".repeat(MAX_POOL_NAME_LEN + 1)] {
            assert!(validate_pool_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn validate_rejects_unknown_default_pool_and_empty_pools() {
        let mut c = Config { default_pool: "nope".into(), ..Default::default() };
        assert!(c.validate().is_err());
        c.pools.clear();
        assert!(c.validate().is_err());
        // An illegally named pool is rejected even if everything else is fine.
        let mut c = Config::default();
        c.pools.insert("Bad Name".into(), PoolConfig::default());
        assert!(c.validate().is_err());
    }

    #[test]
    fn pool_lookup_falls_back_to_default() {
        let mut c = Config::default();
        c.pools.insert("work".into(), PoolConfig { hold_seconds: 7, ..Default::default() });
        assert_eq!(c.pool_or_default(Some("work")).hold_seconds, 7);
        assert_eq!(c.pool_or_default(Some("nonexistent")).hold_seconds, 0);
        assert_eq!(c.pool_or_default(None).hold_seconds, 0);
        // Default first, then sorted.
        c.pools.insert("alpha".into(), PoolConfig::default());
        assert_eq!(c.pool_names(), vec!["default", "alpha", "work"]);
    }

    #[test]
    fn account_ids_are_unique_across_pools() {
        let mut c = Config::default();
        c.pools.get_mut(DEFAULT_POOL).unwrap().accounts.push(AccountConfig { name: "a".into(), id: Some("dup".into()), ..Default::default() });
        c.pools.insert(
            "work".into(),
            PoolConfig { accounts: vec![AccountConfig { name: "b".into(), id: Some("dup".into()), ..Default::default() }], ..Default::default() },
        );
        c.ensure_account_ids();
        let ids: Vec<&str> = c.all_accounts().filter_map(|(_, a)| a.id.as_deref()).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(c.find_account("a"), Some((DEFAULT_POOL.to_string(), 0)));
        assert_eq!(c.find_account("b"), Some(("work".to_string(), 0)));
        assert_eq!(c.find_account("missing"), None);
    }

    #[test]
    fn legacy_state_file_restores_as_default_pool() {
        let raw = r#"{"version":2,"savedAt":"now","accounts":{"id-1":{"name":"work"}},"clientUsage":{"cli":{"totalRequests":3}}}"#;
        let st: State = serde_json::from_str(raw).unwrap();
        assert_eq!(st.version, 2);
        assert!(st.default.accounts.contains_key("id-1"));
        assert!(st.default.client_usage.contains_key("cli"));
        assert!(st.pools.is_empty());
        assert!(st.pool("default", "default").is_some());
        assert!(st.pool("work", "default").is_none());

        // Named pools nest; the default pool keeps writing to the top level.
        let mut st = st;
        st.set_pool("work", "default", PoolState { accounts: BTreeMap::from([("id-2".into(), json!({}))]), ..Default::default() });
        let out = serde_json::to_value(&st).unwrap();
        assert!(out.pointer("/accounts/id-1").is_some(), "default pool stays flattened");
        assert!(out.pointer("/pools/work/accounts/id-2").is_some());
    }

    #[test]
    fn rename_pool_moves_config_and_follows_the_default_pointer() {
        let mut c = Config::default();
        c.pools.insert("work".into(), PoolConfig { hold_seconds: 42, ..Default::default() });

        // Renaming a non-default pool moves its config under the new key.
        c.rename_pool("work", "clients").unwrap();
        assert!(!c.pools.contains_key("work"));
        assert_eq!(c.pool("clients").unwrap().hold_seconds, 42);
        assert_eq!(c.default_pool, DEFAULT_POOL, "an unrelated rename leaves the default alone");

        // Renaming the default pool carries the defaultPool pointer along.
        c.rename_pool(DEFAULT_POOL, "primary").unwrap();
        assert_eq!(c.default_pool, "primary");
        assert!(c.pools.contains_key("primary"));
    }

    #[test]
    fn rename_pool_rejects_collisions_unknowns_and_bad_names() {
        let mut c = Config::default();
        c.pools.insert("work".into(), PoolConfig::default());

        assert!(c.rename_pool("work", DEFAULT_POOL).is_err(), "cannot rename onto an existing pool");
        assert!(c.pools.contains_key("work"), "a rejected rename leaves the config untouched");
        assert!(c.rename_pool("nope", "elsewhere").is_err(), "cannot rename a pool that is not there");
        assert!(c.rename_pool("work", "Work").is_err(), "the new name is charset-checked");
        // A no-op rename to the same name is fine.
        c.rename_pool("work", "work").unwrap();
        assert!(c.pools.contains_key("work"));
    }
}
