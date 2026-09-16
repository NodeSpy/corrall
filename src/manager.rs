//! The account fleet: eligibility, rotation, quota bookkeeping, storm control,
//! token refresh and session affinity. All state lives behind one mutex; the
//! async parts (token refresh, admission waits) take the lock only briefly.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::{AccountConfig, AccountType, Config, ExpiryRouting, RouteConfig, StormRamp, Threshold};
use crate::model::{any_glob_matches, weekly_bucket_for, BUCKET_5H, BUCKET_7D, BUCKET_7D_FABLE, BUCKET_7D_SONNET};
use crate::oauth;
use crate::quota::{now_ms, Quota, UsagePayload};
use crate::session::SessionTracker;

const TOKEN_REFRESH_AHEAD_MS: i64 = 5 * 60 * 1000;
const FORCED_REFRESH_FLOOR_MS: i64 = 30 * 1000;

/// What a failed refresh should do to the account's health. Live request
/// traffic disables the account and fails over (`MarkDead`); background
/// diagnostics like the quota probe must never disable an account (`Keep`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OnRefreshFail {
    MarkDead,
    Keep,
}

const ENTITLEMENT_COOLDOWN_MS: i64 = 5 * 60 * 1000;
const THROTTLE_PROBE_FLOOR_MS: i64 = 30 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Throttled,
    Error,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub total_requests: u64,
    pub failed_requests: u64,
    pub last_used: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    Codex,
}

impl Provider {
    pub fn for_path(path: &str) -> Provider {
        if crate::codex::is_codex_path(path) {
            Provider::Codex
        } else {
            Provider::Anthropic
        }
    }
    pub fn for_host(host: &str) -> Provider {
        if crate::codex::is_codex_host(host) {
            Provider::Codex
        } else {
            Provider::Anthropic
        }
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub kind: AccountType,
    pub provider: Provider,
    /// ChatGPT account id (Codex).
    pub account_id: Option<String>,
    pub plan_type: Option<String>,
    /// Governing weekly reset observed when this account was last confirmed
    /// as the resting choice, per bucket (expiry-routing preemption).
    pub rollover_baseline: HashMap<String, i64>,
    pub account_uuid: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub email: Option<String>,
    pub priority: i32,
    pub disabled: bool,
    pub credential: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub import_from: Option<String>,
    pub upstream: Option<String>,
    pub model_map: BTreeMap<String, String>,
    pub strip_request_fields: Vec<String>,
    pub max_usage: Option<Threshold>,
    pub rate_limit_tier: Option<String>,
    pub seat_tier: Option<String>,
    pub subscription_type: Option<String>,
    pub quota: Quota,
    pub status: Status,
    pub error_message: Option<String>,
    pub rate_limited_until: Option<i64>,
    pub throttled_at: Option<i64>,
    pub last_probe_at: Option<i64>,
    pub entitlement_denied_until: Option<i64>,
    pub dead_refresh_token: Option<String>,
    pub last_refresh_at: Option<i64>,
    pub ramp_started_at: Option<i64>,
    pub in_flight: u32,
    pub usage: Usage,
    pub probing: bool,
    pub requalify: bool,
}

impl Account {
    fn from_config(c: &AccountConfig) -> Account {
        let mut a = Account {
            id: c.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            name: c.name.clone(),
            kind: c.kind.clone(),
            provider: if c.is_codex() { Provider::Codex } else { Provider::Anthropic },
            account_id: c.account_id.clone(),
            plan_type: c.plan_type.clone(),
            rollover_baseline: HashMap::new(),
            account_uuid: c.account_uuid.clone(),
            org_uuid: c.org_uuid.clone(),
            org_name: c.org_name.clone(),
            email: c.email.clone(),
            priority: c.priority,
            disabled: c.disabled,
            credential: None,
            refresh_token: None,
            expires_at: None,
            import_from: c.import_from.clone(),
            upstream: c.upstream.clone(),
            model_map: c.model_map.clone().unwrap_or_default(),
            strip_request_fields: c.strip_request_fields.clone(),
            max_usage: c.max_usage.clone(),
            rate_limit_tier: c.rate_limit_tier.clone(),
            seat_tier: c.seat_tier.clone(),
            subscription_type: c.subscription_type.clone(),
            quota: Quota::default(),
            status: Status::Active,
            error_message: None,
            rate_limited_until: None,
            throttled_at: None,
            last_probe_at: None,
            entitlement_denied_until: None,
            dead_refresh_token: None,
            last_refresh_at: None,
            ramp_started_at: None,
            in_flight: 0,
            usage: Usage::default(),
            probing: false,
            requalify: false,
        };
        a.apply_credentials(c);
        a
    }

    /// (Re)load credentials from the config entry or its `importFrom` file.
    fn apply_credentials(&mut self, c: &AccountConfig) {
        match self.kind {
            AccountType::Apikey => {
                self.credential = c.api_key.clone().filter(|k| !k.is_empty());
            }
            AccountType::Oauth if self.provider == Provider::Codex => {
                let path = c.import_from.clone();
                let stored = c.access_token.clone().filter(|t| !t.is_empty());
                if stored.is_some() && path.is_none() {
                    self.credential = stored;
                    self.refresh_token = c.refresh_token.clone().filter(|t| !t.is_empty());
                    self.expires_at = c.expires_at;
                } else {
                    let p = path.unwrap_or_else(|| crate::codex::DEFAULT_CREDENTIALS_PATH.to_string());
                    match crate::codex::import_credentials(&p) {
                        Ok(cc) if cc.access_token.as_deref().map(|t| !t.is_empty()).unwrap_or(false) => {
                            self.credential = cc.access_token;
                            self.refresh_token = cc.refresh_token;
                            self.expires_at = cc.expires_at;
                            if self.account_id.is_none() {
                                self.account_id = cc.account_id;
                            }
                            if self.email.is_none() {
                                self.email = cc.email;
                            }
                            if self.plan_type.is_none() {
                                self.plan_type = cc.plan_type;
                            }
                        }
                        Ok(_) => tracing::warn!("account \"{}\": {p} carries no access token; skipping", self.name),
                        Err(e) => tracing::warn!("account \"{}\": cannot read {p}: {e}", self.name),
                    }
                }
            }
            AccountType::Oauth => {
                if let Some(path) = &c.import_from {
                    match oauth::import_credentials(path) {
                        Ok(ic) if ic.access_token.as_deref().map(|t| !t.is_empty()).unwrap_or(false) => {
                            self.credential = ic.access_token;
                            self.refresh_token = ic.refresh_token;
                            self.expires_at = ic.expires_at;
                            if self.subscription_type.is_none() {
                                self.subscription_type = ic.subscription_type;
                            }
                        }
                        Ok(_) => tracing::warn!("account \"{}\": {path} carries no access token; skipping", self.name),
                        Err(e) => tracing::warn!("account \"{}\": cannot read {path}: {e}", self.name),
                    }
                } else {
                    self.credential = c.access_token.clone().filter(|t| !t.is_empty());
                    self.refresh_token = c.refresh_token.clone().filter(|t| !t.is_empty());
                    self.expires_at = c.expires_at;
                }
            }
        }
    }

    pub fn is_subscription(&self) -> bool {
        self.kind == AccountType::Oauth
    }

    /// Upstream base URL. Subscription tokens are only ever sent to their
    /// provider's hosts (Anthropic, or chatgpt.com for Codex).
    pub fn upstream_for(&self, default: &str) -> Result<String> {
        let u = match (&self.upstream, self.provider) {
            (Some(u), _) => u.clone(),
            (None, Provider::Codex) => crate::codex::UPSTREAM.to_string(),
            (None, Provider::Anthropic) => default.to_string(),
        };
        if self.is_subscription() {
            let host = url::Url::parse(&u).ok().and_then(|p| p.host_str().map(str::to_string)).unwrap_or_default();
            let allowed = match self.provider {
                Provider::Anthropic => oauth::is_anthropic_host(&host),
                Provider::Codex => crate::codex::is_codex_host(&host),
            };
            if !allowed && !crate::security::is_loopback_host(&host) {
                anyhow::bail!("account \"{}\" holds a subscription token but points at {u}; refusing to send it there", self.name);
            }
        }
        Ok(u)
    }

    pub fn weekly_reset_for(&self, model: Option<&str>) -> Option<i64> {
        let key = weekly_bucket_for(model);
        self.quota.bucket(key).and_then(|b| b.reset_at).or(self.quota.unified7d.reset_at)
    }
}

/// What the forwarder needs from a selected account. A snapshot so the lock is
/// not held across network I/O.
#[derive(Debug, Clone)]
pub struct Selected {
    pub id: String,
    pub name: String,
    pub kind: AccountType,
    pub provider: Provider,
    pub account_id: Option<String>,
    pub credential: String,
    pub account_uuid: Option<String>,
    pub upstream: String,
    pub model_map: BTreeMap<String, String>,
    pub strip_request_fields: Vec<String>,
}

#[derive(Debug)]
pub enum Selection {
    Account(Selected),
    /// No account can take this request now. `retry_after_secs` is the
    /// soonest known recovery of an account held back by quota or a rate
    /// limit; it is `None` when no account is in that state, which is the case
    /// when every eligible account was tried by the caller and failed.
    Exhausted {
        retry_after_secs: Option<u64>,
        reason: String,
    },
    /// A pinned account exists but cannot serve.
    PinUnavailable {
        name: String,
        reason: String,
    },
    PinUnknown,
}

#[derive(Debug, Default, Clone)]
pub struct SelectRequest<'a> {
    pub model: Option<&'a str>,
    pub advisor_model: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub pin: Option<&'a str>,
    pub exclude: HashSet<String>,
    pub allow_probe: bool,
    pub provider: Option<Provider>,
}

pub struct Fleet {
    /// The pool this fleet serves.
    pub pool: String,
    pub accounts: Vec<Account>,
    pub current: Option<String>,
    pub threshold: Threshold,
    pub routes: Vec<RouteConfig>,
    pub blocked_models: Vec<String>,
    pub storm: StormRamp,
    pub distribute_sessions: bool,
    pub hold_seconds: u64,
    pub default_upstream: String,
    pub route_pins: HashMap<String, String>,
    pub sessions: SessionTracker,
    pub started_at: i64,
    pub client_usage: BTreeMap<String, Usage>,
    /// dimension name -> value -> usage
    pub dimension_usage: BTreeMap<String, BTreeMap<String, Usage>>,
    pub expiry: ExpiryRouting,
    pub probe: ProbeState,
    pub warmup_secs: u64,
    pub update_available: Option<String>,
    /// When the daemon last completed a release check (ms). `None` until the
    /// first check finishes, which lets `status` do one on demand meanwhile.
    pub update_checked_at: Option<i64>,
    refresh_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeAccount {
    pub status: String,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProbeState {
    pub interval_secs: u64,
    pub running: bool,
    pub last_run_started_at: Option<i64>,
    pub last_run_finished_at: Option<i64>,
    pub next_run_at: Option<i64>,
    pub accounts: HashMap<String, ProbeAccount>,
}

pub const DIMENSION_MAX_VALUES: usize = 500;
pub const DIMENSION_OVERFLOW_KEY: &str = "(other)";

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Mutex<Fleet>>,
    pub events: tokio::sync::broadcast::Sender<String>,
}

impl Manager {
    /// Build the fleet for one pool with its own event bus. `pool` names the
    /// entry in `cfg.pools`; an unknown name falls back to the default pool's
    /// settings.
    pub fn new(cfg: &Config, pool: &str) -> Manager {
        let (tx, _) = tokio::sync::broadcast::channel(256);
        Manager::with_events(cfg, pool, tx)
    }

    /// Build the fleet for one pool on a shared event bus, so one subscriber
    /// sees the log lines of every pool.
    pub fn with_events(cfg: &Config, pool: &str, tx: tokio::sync::broadcast::Sender<String>) -> Manager {
        let p = cfg.pool_or_default(Some(pool));
        let fleet = Fleet {
            pool: pool.to_string(),
            accounts: Vec::new(),
            current: None,
            threshold: p.switch_threshold.clone(),
            routes: p.routes.clone(),
            blocked_models: p.blocked_models.clone(),
            storm: p.storm_ramp.clone(),
            distribute_sessions: p.distribute_sessions,
            hold_seconds: p.hold_seconds,
            default_upstream: cfg.upstream.clone(),
            route_pins: HashMap::new(),
            sessions: SessionTracker::new(),
            started_at: now_ms(),
            client_usage: BTreeMap::new(),
            dimension_usage: BTreeMap::new(),
            expiry: p.expiry_routing.clone(),
            // Probing is per pool; keep-warm and the release check are daemon-wide.
            probe: ProbeState { interval_secs: p.quota_probe_seconds, ..Default::default() },
            warmup_secs: cfg.warmup_seconds,
            update_available: None,
            update_checked_at: None,
            refresh_locks: HashMap::new(),
        };
        let m = Manager { inner: Arc::new(Mutex::new(fleet)), events: tx };
        m.sync_config(cfg, pool);
        m
    }

    /// The pool this fleet serves.
    pub fn pool(&self) -> String {
        self.with(|f| f.pool.clone())
    }

    /// How long this pool holds a request when every account is exhausted.
    pub fn hold_seconds(&self) -> u64 {
        self.with(|f| f.hold_seconds)
    }

    /// This pool's background quota-probe interval in seconds (0 = off).
    pub fn probe_seconds(&self) -> u64 {
        self.with(|f| f.probe.interval_secs)
    }

    pub fn log(&self, msg: impl Into<String>) {
        let s: String = msg.into();
        // Subscribers (TUI / headless printer) render this; keep tracing quiet
        // so a line is never printed twice.
        if self.events.receiver_count() == 0 {
            tracing::info!("{s}");
        } else {
            tracing::debug!("{s}");
            let _ = self.events.send(s);
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Fleet) -> R) -> R {
        let mut g = self.inner.lock();
        f(&mut g)
    }

    /// Re-sync accounts and settings from a (re)loaded config for one pool.
    /// Returns the number of accounts added.
    pub fn sync_config(&self, cfg: &Config, pool: &str) -> usize {
        let p = cfg.pool_or_default(Some(pool));
        let mut added = 0;
        self.with(|f| {
            f.pool = pool.to_string();
            f.threshold = p.switch_threshold.clone();
            f.routes = p.routes.clone();
            f.blocked_models = p.blocked_models.clone();
            f.storm = p.storm_ramp.clone();
            f.distribute_sessions = p.distribute_sessions;
            f.hold_seconds = p.hold_seconds;
            f.default_upstream = cfg.upstream.clone();
            if !p.expiry_routing.enabled || !p.expiry_routing.preempt {
                for a in &mut f.accounts {
                    a.rollover_baseline.clear();
                }
            }
            f.expiry = p.expiry_routing.clone();
            f.probe.interval_secs = p.quota_probe_seconds;
            f.warmup_secs = cfg.warmup_seconds;
            let mut next: Vec<Account> = Vec::with_capacity(p.accounts.len());
            for c in &p.accounts {
                let id = c.id.clone().unwrap_or_default();
                if let Some(pos) = f.accounts.iter().position(|a| a.id == id) {
                    let mut a = f.accounts.remove(pos);
                    a.name = c.name.clone();
                    a.priority = c.priority;
                    let re_enabled = a.disabled && !c.disabled;
                    a.disabled = c.disabled;
                    a.upstream = c.upstream.clone();
                    a.model_map = c.model_map.clone().unwrap_or_default();
                    a.strip_request_fields = c.strip_request_fields.clone();
                    a.max_usage = c.max_usage.clone();
                    a.account_uuid = c.account_uuid.clone().or(a.account_uuid);
                    a.org_uuid = c.org_uuid.clone().or(a.org_uuid);
                    a.org_name = c.org_name.clone().or(a.org_name);
                    a.email = c.email.clone().or(a.email);
                    a.account_id = c.account_id.clone().or(a.account_id);
                    a.import_from = c.import_from.clone();
                    // Take the on-disk credential when it differs from what we
                    // hold: a re-login or a rotated API key. An OAuth pair we
                    // refreshed more recently than the file (a later expiry) is
                    // kept. A new credential also lifts an error state, since
                    // the error was about the old one; an unchanged credential
                    // does not, or a 401-dead key would be retried on every
                    // reload and die again.
                    let mut fresh = a.clone();
                    fresh.apply_credentials(c);
                    let changed = fresh.credential.is_some() && (fresh.credential != a.credential || fresh.refresh_token != a.refresh_token);
                    let not_older = fresh.expires_at.unwrap_or(0) >= a.expires_at.unwrap_or(0);
                    if changed && (a.credential.is_none() || not_older) {
                        a.credential = fresh.credential;
                        a.refresh_token = fresh.refresh_token;
                        a.expires_at = fresh.expires_at;
                        if a.status == Status::Error {
                            a.status = Status::Active;
                            a.error_message = None;
                        }
                    }
                    // Re-enabling is the operator's explicit "try it again".
                    if re_enabled && a.status == Status::Error && a.dead_refresh_token.is_none() {
                        a.status = Status::Active;
                        a.error_message = None;
                    }
                    next.push(a);
                } else {
                    let a = Account::from_config(c);
                    if a.credential.is_none() {
                        tracing::warn!("account \"{}\" has no usable credential; skipping", a.name);
                        continue;
                    }
                    added += 1;
                    next.push(a);
                }
            }
            for gone in &f.accounts {
                tracing::info!("account \"{}\" removed", gone.name);
            }
            f.accounts = next;
            let ids: Vec<String> = f.accounts.iter().map(|a| a.id.clone()).collect();
            f.sessions.remap_accounts(&ids);
            f.route_pins.retain(|_, v| ids.contains(v));
            if f.current.as_ref().map(|c| !ids.contains(c)).unwrap_or(true) {
                f.current = None;
            }
        });
        added
    }

    // ── state persistence ────────────────────────────────────

    /// This pool's slice of the state file.
    pub fn export_pool_state(&self) -> crate::config::PoolState {
        self.with(|f| {
            let mut st = crate::config::PoolState::default();
            for a in &f.accounts {
                st.accounts.insert(
                    a.id.clone(),
                    json!({
                        "name": a.name,
                        "quota": a.quota,
                        "usage": a.usage,
                        "rateLimitTier": a.rate_limit_tier,
                        "seatTier": a.seat_tier,
                        "accountUuid": a.account_uuid,
                        "orgUuid": a.org_uuid,
                        "email": a.email,
                    }),
                );
            }
            for (k, v) in &f.client_usage {
                st.client_usage.insert(k.clone(), serde_json::to_value(v).unwrap_or(Value::Null));
            }
            st.dimension_usage = serde_json::to_value(&f.dimension_usage).unwrap_or(Value::Null);
            st
        })
    }

    pub fn restore_pool_state(&self, st: &crate::config::PoolState) {
        let now = now_ms();
        self.with(|f| {
            for a in &mut f.accounts {
                let Some(v) = st.accounts.get(&a.id) else { continue };
                if let Some(q) = v.get("quota").and_then(|q| serde_json::from_value::<Quota>(q.clone()).ok()) {
                    a.quota = q;
                    let th = f.threshold.clone();
                    a.quota.clear_expired(now, |b| th.for_bucket(b));
                }
                if let Some(u) = v.get("usage") {
                    a.usage.total_requests = u.get("totalRequests").and_then(Value::as_u64).unwrap_or(0);
                    a.usage.input_tokens = u.get("inputTokens").and_then(Value::as_i64).unwrap_or(0);
                    a.usage.output_tokens = u.get("outputTokens").and_then(Value::as_i64).unwrap_or(0);
                    a.usage.cache_read_tokens = u.get("cacheReadTokens").and_then(Value::as_i64).unwrap_or(0);
                    a.usage.cache_creation_tokens = u.get("cacheCreationTokens").and_then(Value::as_i64).unwrap_or(0);
                }
                if a.rate_limit_tier.is_none() {
                    a.rate_limit_tier = v.get("rateLimitTier").and_then(Value::as_str).map(str::to_string);
                }
                if a.seat_tier.is_none() {
                    a.seat_tier = v.get("seatTier").and_then(Value::as_str).map(str::to_string);
                }
            }
            for (k, v) in &st.client_usage {
                if let Ok(u) = serde_json::from_value::<UsageDe>(v.clone()) {
                    f.client_usage.insert(k.clone(), u.into());
                }
            }
            if let Ok(d) = serde_json::from_value::<BTreeMap<String, BTreeMap<String, UsageDe>>>(st.dimension_usage.clone()) {
                f.dimension_usage = d.into_iter().map(|(k, m)| (k, m.into_iter().map(|(v, u)| (v, u.into())).collect())).collect();
            }
        });
    }

    // ── eligibility ──────────────────────────────────────────

    #[allow(dead_code)]
    pub fn thresholds(&self) -> Threshold {
        self.with(|f| f.threshold.clone())
    }

    pub fn is_model_blocked(&self, model: &str) -> bool {
        self.with(|f| any_glob_matches(&f.blocked_models, model))
    }

    /// Resolve a pin token (account uuid, org uuid, uuid/org, id, name, email)
    /// to an account id.
    pub fn resolve_pin(&self, token: &str) -> Option<String> {
        let t = token.trim();
        if t.is_empty() {
            return None;
        }
        self.with(|f| {
            if let Some((au, ou)) = t.split_once('/') {
                if let Some(a) = f.accounts.iter().find(|a| a.account_uuid.as_deref() == Some(au) && a.org_uuid.as_deref() == Some(ou)) {
                    return Some(a.id.clone());
                }
            }
            f.accounts
                .iter()
                .find(|a| {
                    a.id == t
                        || a.account_uuid.as_deref() == Some(t)
                        || a.org_uuid.as_deref() == Some(t)
                        || a.name == t
                        || a.email.as_deref() == Some(t)
                        || a.name.split(" (").next() == Some(t)
                })
                .map(|a| a.id.clone())
        })
    }

    pub fn select(&self, req: &SelectRequest) -> Selection {
        let now = now_ms();
        self.with(|f| f.select(req, now, self))
    }

    /// Storm-control admission: wait until the account's concurrency cap
    /// admits this request, or the ramp window ends (fail-open).
    pub async fn admit(&self, id: &str) {
        loop {
            let (ok, wait) = self.with(|f| f.try_admit(id, now_ms()));
            if ok {
                return;
            }
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    }

    pub fn release(&self, id: &str) {
        self.with(|f| {
            if let Some(a) = f.account_mut(id) {
                a.in_flight = a.in_flight.saturating_sub(1);
            }
        });
    }

    // ── token freshness ──────────────────────────────────────

    /// Refresh the OAuth token when it is close to expiry (or `force`d by a
    /// 401). Concurrent callers coalesce on one refresh per account.
    pub async fn ensure_token_fresh(&self, id: &str, force: bool, on_fail: OnRefreshFail) -> Option<String> {
        let (needs, lock, name, refresh_token) = self.with(|f| {
            let lock = f.refresh_locks.entry(id.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone();
            let Some(a) = f.account_mut(id) else { return (false, lock, String::new(), None) };
            if a.kind != AccountType::Oauth {
                return (false, lock, a.name.clone(), None);
            }
            let Some(rt) = a.refresh_token.clone() else { return (false, lock, a.name.clone(), None) };
            if a.dead_refresh_token.as_deref() == Some(rt.as_str()) {
                if on_fail == OnRefreshFail::MarkDead && a.status != Status::Error {
                    a.status = Status::Error;
                    a.error_message = Some("refresh token rejected; run: corrall login".into());
                }
                return (false, lock, a.name.clone(), None);
            }
            if force {
                if let Some(last) = a.last_refresh_at {
                    if now_ms() - last < FORCED_REFRESH_FLOOR_MS {
                        return (false, lock, a.name.clone(), None);
                    }
                }
            } else if !oauth::is_expiring_soon(a.expires_at, TOKEN_REFRESH_AHEAD_MS) {
                return (false, lock, a.name.clone(), None);
            }
            (true, lock, a.name.clone(), Some(rt))
        });
        if !needs {
            return self.with(|f| f.account(id).and_then(|a| a.credential.clone()));
        }
        let _g = lock.lock().await;
        // Re-check after acquiring: a coalesced peer may have refreshed already.
        let (still, rt) = self.with(|f| {
            let Some(a) = f.account(id) else { return (false, None::<String>) };
            let stale = if force {
                a.refresh_token.as_deref() == refresh_token.as_deref() && a.last_refresh_at.map(|l| now_ms() - l >= FORCED_REFRESH_FLOOR_MS).unwrap_or(true)
            } else {
                oauth::is_expiring_soon(a.expires_at, TOKEN_REFRESH_AHEAD_MS)
            };
            (stale, a.refresh_token.clone())
        });
        if !still {
            return self.with(|f| f.account(id).and_then(|a| a.credential.clone()));
        }
        let rt: String = rt?;
        self.log(format!("Refreshing token for account \"{name}\"..."));
        let provider = self.with(|f| f.account(id).map(|a| a.provider).unwrap_or(Provider::Anthropic));
        let refreshed = match provider {
            Provider::Anthropic => oauth::refresh_access_token(&rt).await,
            Provider::Codex => crate::codex::refresh_access_token(&rt).await,
        };
        match refreshed {
            Ok(t) => {
                let (access, refresh, exp) = (t.access_token.clone(), t.refresh_token.clone().unwrap_or(rt), t.expires_at);
                self.with(|f| {
                    if let Some(a) = f.account_mut(id) {
                        a.credential = Some(access.clone());
                        a.refresh_token = Some(refresh.clone());
                        a.expires_at = Some(exp);
                        a.last_refresh_at = Some(now_ms());
                        a.dead_refresh_token = None;
                        if a.status == Status::Error {
                            a.status = Status::Active;
                            a.error_message = None;
                        }
                    }
                });
                self.log(format!("Token refreshed for account \"{name}\""));
                let id_owned = id.to_string();
                // Scope the write to this fleet's own pool: the same account id
                // cannot appear twice, but searching one pool keeps a
                // refresh from ever touching another pool's entry.
                let pool = self.pool();
                let persist = tokio::task::spawn_blocking(move || {
                    Config::update(|c| {
                        let entry = c.pool_mut(&pool).and_then(|p| p.accounts.iter_mut().find(|e| e.id.as_deref() == Some(id_owned.as_str())));
                        if let Some(entry) = entry {
                            if entry.import_from.is_none() {
                                entry.access_token = Some(access);
                                entry.refresh_token = Some(refresh);
                                entry.expires_at = Some(exp);
                            }
                        } else {
                            tracing::warn!("account {id_owned} is not in pool \"{pool}\" on disk; the refreshed token was not persisted");
                        }
                        Ok(())
                    })
                })
                .await;
                if let Err(e) = persist.map_err(anyhow::Error::from).and_then(|r| r.map(|_| ())) {
                    tracing::error!("could not persist refreshed token: {e}");
                }
            }
            Err(e) => {
                self.log(format!("Token refresh failed for \"{name}\": {e}"));
                if e.is_auth_rejection() && on_fail == OnRefreshFail::MarkDead {
                    self.with(|f| {
                        if let Some(a) = f.account_mut(id) {
                            a.status = Status::Error;
                            a.error_message = Some("refresh token rejected; run: corrall login".into());
                            a.dead_refresh_token = Some(rt.clone());
                        }
                    });
                }
            }
        }
        self.with(|f| f.account(id).and_then(|a| a.credential.clone()))
    }

    // ── quota / status updates ───────────────────────────────

    pub fn update_quota(&self, id: &str, headers: &BTreeMap<String, String>) {
        let now = now_ms();
        let mut learned = None;
        self.with(|f| {
            let th = f.threshold.clone();
            if let Some(a) = f.account_mut(id) {
                match a.provider {
                    Provider::Anthropic => a.quota.apply_headers(headers, now),
                    Provider::Codex => {
                        crate::codex::apply_codex_headers(&mut a.quota, headers, now);
                        if let Some(p) = headers.get("x-codex-plan-type") {
                            a.plan_type = Some(p.trim().to_string());
                        }
                    }
                }
                a.usage.total_requests += 1;
                a.usage.last_used = Some(chrono::Utc::now().to_rfc3339());
                if a.probing && a.quota.unified7d.reset_at.is_some() {
                    a.probing = false;
                    a.requalify = true;
                    learned = Some(a.name.clone());
                }
                let key = weekly_bucket_for(None);
                let _ = a.quota.clear_expired(now, |b| th.for_bucket(b));
                let _ = key;
            }
        });
        if let Some(n) = learned {
            self.log(format!("Learned weekly quota for \"{n}\", re-evaluating selection"));
        }
    }

    pub fn apply_usage(&self, id: &str, u: &UsagePayload) {
        let now = now_ms();
        self.with(|f| {
            if let Some(a) = f.account_mut(id) {
                a.quota.apply_usage(u, now);
                a.last_probe_at = Some(now);
            }
        });
    }

    pub fn apply_profile(&self, id: &str, p: &oauth::Profile) {
        self.with(|f| {
            if let Some(a) = f.account_mut(id) {
                if a.account_uuid.is_none() {
                    a.account_uuid = p.account_uuid.clone();
                }
                if a.org_uuid.is_none() {
                    a.org_uuid = p.org_uuid.clone();
                }
                if a.org_name.is_none() {
                    a.org_name = p.org_name.clone();
                }
                if a.email.is_none() {
                    a.email = p.email.clone();
                }
                a.rate_limit_tier = p.rate_limit_tier.clone().or(a.rate_limit_tier.take());
                a.seat_tier = p.seat_tier.clone().or(a.seat_tier.take());
            }
        });
    }

    pub fn mark_rate_limited(&self, id: &str, secs: u64) {
        let now = now_ms();
        let name = self.with(|f| {
            f.account_mut(id).map(|a| {
                a.status = Status::Throttled;
                a.rate_limited_until = Some(now + secs as i64 * 1000);
                a.throttled_at = Some(now);
                a.name.clone()
            })
        });
        if let Some(n) = name {
            self.log(format!("Account \"{n}\" rate limited for {secs}s"));
        }
    }

    pub fn clear_rate_limited(&self, id: &str) {
        self.with(|f| {
            if let Some(a) = f.account_mut(id) {
                if a.status == Status::Throttled {
                    a.status = Status::Active;
                }
                a.rate_limited_until = None;
            }
        });
    }

    pub fn mark_entitlement_denied(&self, id: &str) {
        let now = now_ms();
        self.with(|f| {
            if let Some(a) = f.account_mut(id) {
                a.entitlement_denied_until = Some(now + ENTITLEMENT_COOLDOWN_MS);
            }
        });
    }

    pub fn is_entitlement_denied(&self, id: &str) -> bool {
        let now = now_ms();
        self.with(|f| f.account(id).and_then(|a| a.entitlement_denied_until).map(|u| u > now).unwrap_or(false))
    }

    pub fn mark_error(&self, id: &str, msg: &str) {
        self.with(|f| {
            if let Some(a) = f.account_mut(id) {
                a.status = Status::Error;
                a.error_message = Some(msg.to_string());
                a.usage.failed_requests += 1;
            }
        });
    }

    pub fn record_failure(&self, id: &str) {
        self.with(|f| {
            if let Some(a) = f.account_mut(id) {
                a.usage.failed_requests += 1;
            }
        });
    }

    #[allow(dead_code)]
    pub fn clear_error(&self, id: &str) {
        self.with(|f| {
            if let Some(a) = f.account_mut(id) {
                a.status = Status::Active;
                a.error_message = None;
                a.dead_refresh_token = None;
                a.rate_limited_until = None;
            }
        });
    }

    pub fn record_token_usage(&self, id: &str, session: Option<&str>, client: Option<&str>, model: Option<&str>, usage: &Value) {
        self.record_token_usage_dims(id, session, client, &[], model, usage)
    }

    pub fn record_token_usage_dims(
        &self,
        id: &str,
        session: Option<&str>,
        client: Option<&str>,
        dims: &[(String, String)],
        model: Option<&str>,
        usage: &Value,
    ) {
        let now = now_ms();
        let g = |k: &str| usage.get(k).and_then(Value::as_i64).unwrap_or(0);
        self.with(|f| {
            for (dim, value) in dims {
                let table = f.dimension_usage.entry(dim.clone()).or_default();
                let key = if table.contains_key(value) || table.len() < DIMENSION_MAX_VALUES { value.clone() } else { DIMENSION_OVERFLOW_KEY.to_string() };
                let u = table.entry(key).or_default();
                u.total_requests += 1;
                u.input_tokens += g("input_tokens");
                u.output_tokens += g("output_tokens");
                u.cache_read_tokens += g("cache_read_input_tokens");
                u.cache_creation_tokens += g("cache_creation_input_tokens");
                u.last_used = Some(chrono::Utc::now().to_rfc3339());
            }
            if let Some(a) = f.account_mut(id) {
                a.usage.input_tokens += g("input_tokens");
                a.usage.output_tokens += g("output_tokens");
                a.usage.cache_read_tokens += g("cache_read_input_tokens");
                a.usage.cache_creation_tokens += g("cache_creation_input_tokens");
            }
            if let Some(c) = client {
                let u = f.client_usage.entry(c.to_string()).or_default();
                u.total_requests += 1;
                u.input_tokens += g("input_tokens");
                u.output_tokens += g("output_tokens");
                u.cache_read_tokens += g("cache_read_input_tokens");
                u.cache_creation_tokens += g("cache_creation_input_tokens");
                u.last_used = Some(chrono::Utc::now().to_rfc3339());
            }
            if let Some(s) = session {
                f.sessions.record_tokens(s, weekly_bucket_for(model), usage, now);
            }
        });
    }

    pub fn record_session(&self, session: Option<&str>, id: &str, model: Option<&str>) {
        let Some(s) = session else { return };
        let now = now_ms();
        self.with(|f| f.sessions.touch(s, Some(id), Some(weekly_bucket_for(model)), now));
    }

    pub fn begin_session_request(&self, session: Option<&str>, client: Option<&str>) {
        if let Some(s) = session {
            let now = now_ms();
            self.with(|f| f.sessions.begin_request(s, client, now));
        }
    }

    pub fn end_session_request(&self, session: Option<&str>) {
        if let Some(s) = session {
            let now = now_ms();
            self.with(|f| f.sessions.end_request(s, now));
        }
    }

    /// Make one account the preferred one. Returns whether it is eligible now.
    pub fn switch_to(&self, id: &str) -> Option<(String, Option<String>)> {
        let now = now_ms();
        self.with(|f| {
            let a = f.account(id)?.clone();
            let reason = f.unavailable_reason(&a, None, None, now);
            f.current = Some(id.to_string());
            f.begin_ramp(id, now);
            Some((a.name, reason))
        })
    }

    pub fn set_route_pin(&self, route: &str, id: Option<&str>) {
        self.with(|f| match id {
            Some(i) => {
                f.route_pins.insert(route.to_string(), i.to_string());
            }
            None => {
                f.route_pins.remove(route);
            }
        });
    }

    pub fn account_ids(&self) -> Vec<(String, String)> {
        self.with(|f| f.accounts.iter().map(|a| (a.id.clone(), a.name.clone())).collect())
    }

    pub fn oauth_accounts(&self) -> Vec<(String, String)> {
        self.with(|f| {
            f.accounts
                .iter()
                .filter(|a| a.kind == AccountType::Oauth && a.provider == Provider::Anthropic && a.credential.is_some() && a.upstream.is_none())
                .map(|a| (a.id.clone(), a.name.clone()))
                .collect()
        })
    }

    pub fn set_update_available(&self, tag: Option<String>) {
        let announce = self.with(|f| {
            let changed = f.update_available != tag;
            f.update_available = tag.clone();
            f.update_checked_at = Some(now_ms());
            changed && tag.is_some()
        });
        if announce {
            self.log(format!("Update available: {} → {} (run: corrall update)", env!("CARGO_PKG_VERSION"), tag.unwrap_or_default()));
        }
    }

    pub fn update_available(&self) -> Option<String> {
        self.with(|f| f.update_available.clone())
    }

    pub fn probe_run_started(&self, now: i64, next: Option<i64>) {
        self.with(|f| {
            f.probe.running = true;
            f.probe.last_run_started_at = Some(now);
            f.probe.next_run_at = next;
        });
    }

    /// `next` overrides the due time recorded at start, for a run that ended
    /// in a backoff.
    pub fn probe_run_finished(&self, now: i64, next: Option<i64>) {
        self.with(|f| {
            f.probe.running = false;
            f.probe.last_run_finished_at = Some(now);
            if next.is_some() {
                f.probe.next_run_at = next;
            }
        });
    }

    pub fn probe_account_status(&self, id: &str, status: &str, started_at: i64, finished_at: Option<i64>, error: Option<String>) {
        self.with(|f| {
            f.probe.accounts.insert(
                id.to_string(),
                ProbeAccount { status: status.to_string(), started_at: Some(started_at), finished_at, duration_ms: finished_at.map(|t| t - started_at), error },
            );
        });
    }

    /// Accounts keep-warm may touch: idle Anthropic subscription accounts
    /// whose 5h window is not running.
    pub fn warm_candidates(&self) -> Vec<(String, String)> {
        let now = now_ms();
        self.with(|f| {
            f.accounts
                .iter()
                .filter(|a| a.kind == AccountType::Oauth && a.provider == Provider::Anthropic && a.upstream.is_none() && a.credential.is_some())
                .filter(|a| f.unavailable_reason(a, None, None, now).is_none())
                .filter(|a| a.quota.unified5h.reset_at.map(|r| r <= now).unwrap_or(true))
                .map(|a| (a.id.clone(), a.name.clone()))
                .collect()
        })
    }

    pub fn credential_of(&self, id: &str) -> Option<String> {
        self.with(|f| f.account(id).and_then(|a| a.credential.clone()))
    }

    pub fn needs_profile(&self, id: &str) -> bool {
        self.with(|f| f.account(id).map(|a| a.rate_limit_tier.is_none() && a.seat_tier.is_none()).unwrap_or(false))
    }

    // ── status ───────────────────────────────────────────────

    pub fn status(&self, session_detail: bool) -> Value {
        let now = now_ms();
        self.with(|f| f.status_json(now, session_detail))
    }

    pub fn quota_summary(&self) -> Value {
        let now = now_ms();
        self.with(|f| f.quota_summary(now))
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct UsageDe {
    total_requests: u64,
    failed_requests: u64,
    last_used: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
}
impl From<UsageDe> for Usage {
    fn from(u: UsageDe) -> Usage {
        Usage {
            total_requests: u.total_requests,
            failed_requests: u.failed_requests,
            last_used: u.last_used,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_read_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
        }
    }
}

impl Fleet {
    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }
    pub fn account_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }
    pub fn account_by_name(&self, name: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.name == name || a.id == name)
    }

    fn threshold_for(&self, bucket: &str) -> f64 {
        self.threshold.for_bucket(bucket)
    }

    /// The first route whose globs match `model`.
    pub fn route_for(&self, model: &str) -> Option<&RouteConfig> {
        self.routes.iter().find(|r| any_glob_matches(&r.patterns, model))
    }

    fn route_allows(&self, a: &Account, model: Option<&str>) -> bool {
        let Some(m) = model else { return true };
        let Some(r) = self.route_for(m) else { return true };
        if r.accounts.is_empty() {
            return true;
        }
        r.accounts
            .iter()
            .any(|n| n == &a.name || n == &a.id || n.parse::<usize>().ok().and_then(|i| self.accounts.get(i)).map(|x| x.id == a.id).unwrap_or(false))
    }

    /// Bucket key that governs eligibility for a model, honouring a route's
    /// explicit `bucket` override.
    fn weekly_key_for(&self, model: Option<&str>) -> &'static str {
        if let Some(m) = model {
            if let Some(b) = self.route_for(m).and_then(|r| r.bucket.as_deref()) {
                return match b {
                    BUCKET_7D_FABLE => BUCKET_7D_FABLE,
                    BUCKET_7D_SONNET => BUCKET_7D_SONNET,
                    _ => BUCKET_7D,
                };
            }
        }
        weekly_bucket_for(model)
    }

    /// The bucket at which `a` has reached its own hard cap, if any.
    fn capped_bucket(&self, a: &Account, model: Option<&str>) -> Option<&'static str> {
        let caps = a.max_usage.as_ref()?;
        let q = &a.quota;
        if let (Some(c), Some(u)) = (caps.cap_for(BUCKET_5H), q.unified5h.utilization) {
            if u >= c {
                return Some(BUCKET_5H);
            }
        }
        if let (Some(c), Some(u)) = (caps.cap_for(BUCKET_7D), q.unified7d.utilization) {
            if u >= c {
                return Some(BUCKET_7D);
            }
        }
        let key = self.weekly_key_for(model);
        if key != BUCKET_7D {
            if let (Some(c), Some(u)) = (caps.cap_for(key), q.bucket(key).and_then(|b| b.utilization)) {
                if u >= c {
                    return Some(key);
                }
            }
        }
        if let (Some(c), Some(u)) = (caps.cap_for("tokens"), q.tokens_used()) {
            if u >= c {
                return Some("tokens");
            }
        }
        None
    }

    fn near_quota(&self, a: &Account, model: Option<&str>) -> Option<String> {
        let q = &a.quota;
        if let Some(u) = q.unified5h.utilization {
            if u >= self.threshold_for(BUCKET_5H) {
                return Some(format!("5h at {:.0}%", u * 100.0));
            }
        }
        let key = self.weekly_key_for(model);
        if let Some(u) = q.gating_weekly(key) {
            if u >= self.threshold_for(key) {
                return Some(format!("{key} at {:.0}%", u * 100.0));
            }
        }
        if let Some(u) = q.tokens_used() {
            if u >= self.threshold_for("tokens") {
                return Some(format!("tokens at {:.0}%", u * 100.0));
            }
        }
        if let Some(u) = q.requests_used() {
            if u >= self.threshold_for("requests") {
                return Some(format!("requests at {:.0}%", u * 100.0));
            }
        }
        None
    }

    /// Why `a` cannot serve `model` right now, or None if it can. "Soft"
    /// reasons (near threshold) are prefixed `quota:` so the probe path can
    /// tell them from hard exclusions.
    pub fn unavailable_reason(&self, a: &Account, model: Option<&str>, advisor: Option<&str>, now: i64) -> Option<String> {
        self.unavailable_reason_for(a, model, advisor, None, now)
    }

    pub fn unavailable_reason_for(&self, a: &Account, model: Option<&str>, advisor: Option<&str>, provider: Option<Provider>, now: i64) -> Option<String> {
        if let Some(p) = provider {
            if a.provider != p {
                return Some("other provider".into());
            }
        }
        if a.disabled {
            return Some("disabled".into());
        }
        if a.credential.is_none() {
            return Some("no credential".into());
        }
        if a.upstream_for(&self.default_upstream).is_err() {
            return Some("upstream refused: subscription token must stay with its provider".into());
        }
        if a.status == Status::Error {
            return Some(format!("error: {}", a.error_message.clone().unwrap_or_else(|| "needs re-login".into())));
        }
        if let Some(until) = a.rate_limited_until {
            if until > now {
                return Some(format!("rate-limited for {}s", (until - now) / 1000));
            }
        }
        if let Some(until) = a.entitlement_denied_until {
            if until > now {
                return Some("oauth not allowed for organization (cooldown)".into());
            }
        }
        if !self.route_allows(a, model) {
            return Some("route excludes it".into());
        }
        if advisor.is_some() && !self.route_allows(a, advisor) {
            return Some("route excludes the advisor model".into());
        }
        if let Some(b) = self.capped_bucket(a, model) {
            return Some(format!("capped: {b}"));
        }
        if advisor.is_some() {
            if let Some(b) = self.capped_bucket(a, advisor) {
                return Some(format!("advisor-capped: {b}"));
            }
        }
        if let Some(r) = self.near_quota(a, model) {
            return Some(format!("quota: {r}"));
        }
        if advisor.is_some() {
            if let Some(r) = self.near_quota(a, advisor) {
                return Some(format!("quota (advisor): {r}"));
            }
        }
        None
    }

    fn is_available_for(&self, a: &Account, req: &SelectRequest, now: i64) -> bool {
        self.unavailable_reason_for(a, req.model, req.advisor_model, req.provider, now).is_none()
    }

    /// Expiry pressure: spendable headroom in the governing weekly bucket per
    /// second until it resets, and whether the reset was actually known. A
    /// known utilization with no reset is ranked by a lower bound (a full
    /// week out) so it never beats a measured account.
    fn pressure(&self, a: &Account, model: Option<&str>, now: i64) -> (Option<f64>, bool) {
        let key = self.weekly_key_for(model);
        let b = a.quota.bucket(key).copied().unwrap_or_default();
        let Some(u) = b.utilization else { return (None, false) };
        let spendable = 1.0 - u.clamp(0.0, 1.0);
        match b.reset_at {
            Some(r) if r > now => (Some(spendable / ((r - now) as f64 / 1000.0)), true),
            Some(_) => (Some(0.0), true),
            None => (Some(spendable / (7.0 * 24.0 * 3600.0)), false),
        }
    }

    /// Restrict the top priority tier to the accounts within `tolerance` of
    /// the highest known pressure, highest pressure first. Accounts with no
    /// utilization reading stay in (being used is how they become known).
    fn band<'a>(&self, mut v: Vec<&'a Account>, model: Option<&str>, now: i64) -> Vec<&'a Account> {
        if !self.expiry.enabled || v.len() <= 1 {
            return v;
        }
        let top = v.iter().map(|a| a.priority).min().unwrap();
        let known: Vec<f64> = v
            .iter()
            .filter(|a| a.priority == top)
            .filter_map(|a| {
                let (p, measured) = self.pressure(a, model, now);
                p.filter(|_| measured)
            })
            .collect();
        let Some(max) = known.iter().cloned().reduce(f64::max) else { return v };
        let ratio = if self.expiry.tolerance.is_finite() && self.expiry.tolerance > 0.0 { self.expiry.tolerance } else { 1.0 };
        let floor = (max / ratio).min(max);
        v.retain(|a| a.priority != top || self.pressure(a, model, now).0.map(|p| p >= floor).unwrap_or(true));
        v.sort_by(|a, b| {
            a.priority.cmp(&b.priority).then_with(|| {
                let pa = self.pressure(a, model, now).0.unwrap_or(f64::INFINITY);
                let pb = self.pressure(b, model, now).0.unwrap_or(f64::INFINITY);
                pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        v
    }

    /// With preemption on: has the governing weekly window of `a` rolled over
    /// since we last confirmed it as the resting choice?
    fn rolled_over(&self, a: &Account, model: Option<&str>) -> bool {
        if !(self.expiry.enabled && self.expiry.preempt) {
            return false;
        }
        let key = self.weekly_key_for(model);
        let Some(reset) = a.quota.bucket(key).and_then(|b| b.reset_at) else { return false };
        a.rollover_baseline.get(key).map(|base| reset > *base).unwrap_or(false)
    }

    fn note_baseline(&mut self, id: &str, model: Option<&str>) {
        if !(self.expiry.enabled && self.expiry.preempt) {
            return;
        }
        let key = self.weekly_key_for(model).to_string();
        if let Some(a) = self.account_mut(id) {
            if let Some(r) = a.quota.bucket(&key).and_then(|b| b.reset_at) {
                a.rollover_baseline.insert(key, r);
            }
        }
    }

    fn clear_expired_all(&mut self, now: i64) {
        let th = self.threshold.clone();
        for a in &mut self.accounts {
            let _ = a.quota.clear_expired(now, |b| th.for_bucket(b));
            if let Some(until) = a.rate_limited_until {
                if until <= now {
                    a.rate_limited_until = None;
                    if a.status == Status::Throttled {
                        a.status = Status::Active;
                    }
                }
            }
        }
    }

    fn snapshot(&self, a: &Account) -> Option<Selected> {
        let upstream = match a.upstream_for(&self.default_upstream) {
            Ok(u) => u,
            Err(e) => {
                tracing::error!("{e}");
                return None;
            }
        };
        Some(Selected {
            id: a.id.clone(),
            name: a.name.clone(),
            kind: a.kind.clone(),
            provider: a.provider,
            account_id: a.account_id.clone(),
            credential: a.credential.clone()?,
            account_uuid: a.account_uuid.clone(),
            upstream,
            model_map: a.model_map.clone(),
            strip_request_fields: a.strip_request_fields.clone(),
        })
    }

    /// Ranking: lowest priority number, then soonest governing weekly reset
    /// (unknown last), then fewest active sessions when distributing.
    fn ranked_available(&self, req: &SelectRequest, now: i64) -> Vec<&Account> {
        let stats = if self.distribute_sessions { Some(self.sessions.stats(now)) } else { None };
        let v: Vec<&Account> = self.accounts.iter().filter(|a| !req.exclude.contains(&a.id) && self.is_available_for(a, req, now)).collect();
        if self.expiry.enabled {
            return self.band(v, req.model, now);
        }
        let mut v = v;
        v.sort_by(|a, b| {
            a.priority.cmp(&b.priority).then_with(|| {
                if let Some(st) = &stats {
                    let la = st.per_account_active.get(&a.id).copied().unwrap_or(0);
                    let lb = st.per_account_active.get(&b.id).copied().unwrap_or(0);
                    if la != lb {
                        return la.cmp(&lb);
                    }
                    let ia = st.per_account_in_flight.get(&a.id).copied().unwrap_or(0) + a.in_flight;
                    let ib = st.per_account_in_flight.get(&b.id).copied().unwrap_or(0) + b.in_flight;
                    if ia != ib {
                        return ia.cmp(&ib);
                    }
                }
                let ra = a.weekly_reset_for(req.model).unwrap_or(i64::MAX);
                let rb = b.weekly_reset_for(req.model).unwrap_or(i64::MAX);
                ra.cmp(&rb)
            })
        });
        v
    }

    fn begin_ramp(&mut self, id: &str, now: i64) {
        if let Some(a) = self.account_mut(id) {
            a.ramp_started_at = Some(now);
        }
    }

    /// The storm ramp guards a cold account against a burst. In distribute
    /// mode a new session lands on the least-loaded account, which is usually
    /// one already serving other sessions; restarting its ramp on every new
    /// session throttled a warm account back to `startConc` each time. Ramp
    /// only an account that is idle and has no ramp running.
    fn ramp_if_cold(&mut self, id: &str, now: i64) {
        let active_sessions = self.sessions.stats(now).per_account_active.get(id).copied().unwrap_or(0);
        if let Some(a) = self.account_mut(id) {
            if a.ramp_started_at.is_none() && a.in_flight == 0 && active_sessions == 0 {
                a.ramp_started_at = Some(now);
            }
        }
    }

    fn set_current(&mut self, id: &str, now: i64, mgr: &Manager, scoped: bool) {
        let switched = self.current.as_deref() != Some(id);
        if !scoped {
            self.current = Some(id.to_string());
        }
        if switched {
            self.begin_ramp(id, now);
            if let Some(a) = self.account_mut(id) {
                a.probing = a.quota.unified7d.reset_at.is_none();
                let name = a.name.clone();
                mgr.log(if scoped { format!("Diverting request to \"{name}\"") } else { format!("Switched to account \"{name}\"") });
            }
        }
    }

    fn select(&mut self, req: &SelectRequest, now: i64, mgr: &Manager) -> Selection {
        self.clear_expired_all(now);

        // 1. Explicit pin (TC_ACCT / Proxy-Authorization / /tc-acct/): never fails over.
        if let Some(pin) = req.pin {
            let Some(id) = mgr_resolve(self, pin) else { return Selection::PinUnknown };
            let a = self.account(&id).unwrap().clone();
            // A pin bypasses the switch threshold but never a hard cap or a dead token.
            match self.unavailable_reason_for(&a, req.model, req.advisor_model, req.provider, now) {
                None => {}
                Some(r) if r.starts_with("quota:") => {}
                Some(r) => return Selection::PinUnavailable { name: a.name.clone(), reason: r },
            }
            return match self.snapshot(&a) {
                Some(s) => Selection::Account(s),
                None => Selection::PinUnavailable { name: a.name.clone(), reason: "no credential".into() },
            };
        }

        // 2. Manual route pin.
        if let Some(m) = req.model {
            if let Some(route) = self.route_for(m) {
                if let Some(id) = self.route_pins.get(&route.name).cloned() {
                    if let Some(a) = self.account(&id) {
                        if !req.exclude.contains(&id) && self.is_available_for(a, req, now) {
                            if let Some(s) = self.snapshot(a) {
                                return Selection::Account(s);
                            }
                        }
                    }
                }
            }
        }

        // 3. Session affinity.
        if let Some(sid) = req.session_id {
            let bucket = self.weekly_key_for(req.model);
            let pinned = self.sessions.pinned_account(sid, bucket).map(str::to_string).or_else(|| {
                if self.distribute_sessions {
                    self.sessions.any_pin(sid).map(str::to_string)
                } else {
                    None
                }
            });
            if let Some(id) = pinned {
                if let Some(a) = self.account(&id).cloned() {
                    if !req.exclude.contains(&id) && self.is_available_for(&a, req, now) {
                        // Priority still wins: a strictly better-priority account preempts the pin,
                        // and so does a rollover of the governing weekly window (expiry routing).
                        let better = self.ranked_available(req, now).first().map(|b| b.priority < a.priority).unwrap_or(false);
                        let rolled = self.rolled_over(&a, req.model);
                        if !better && !rolled {
                            self.note_baseline(&id, req.model);
                            if let Some(s) = self.snapshot(&a) {
                                return Selection::Account(s);
                            }
                        }
                        if rolled {
                            mgr.log(format!("Weekly window of \"{}\" rolled over; re-ranking the pinned session", a.name));
                        }
                    }
                }
            }
            if self.distribute_sessions {
                // New session: least loaded eligible account (ranked_available already sorts by load).
                if let Some(best) = self.ranked_available(req, now).first().cloned() {
                    let id = best.id.clone();
                    if self.current.is_none() {
                        self.set_current(&id, now, mgr, false);
                    } else {
                        self.ramp_if_cold(&id, now);
                    }
                    if let Some(s) = self.snapshot(self.account(&id).unwrap()) {
                        return Selection::Account(s);
                    }
                }
            }
        }

        // 4. Sticky current account.
        if let Some(cur) = self.current.clone() {
            if let Some(a) = self.account(&cur).cloned() {
                if a.requalify {
                    if let Some(a) = self.account_mut(&cur) {
                        a.requalify = false;
                    }
                    if let Some(best) = self.ranked_available(req, now).first().cloned() {
                        let id = best.id.clone();
                        self.set_current(&id, now, mgr, false);
                        self.note_baseline(&id, req.model);
                        if let Some(s) = self.snapshot(self.account(&id).unwrap()) {
                            return Selection::Account(s);
                        }
                    }
                }
                if !req.exclude.contains(&cur) && self.is_available_for(&a, req, now) {
                    // A strictly lower priority number elsewhere preempts, and so
                    // does a rollover of the governing weekly window (expiry routing).
                    let better = self.ranked_available(req, now).first().map(|b| b.priority < a.priority).unwrap_or(false);
                    let rolled = self.rolled_over(&a, req.model);
                    if !better && !rolled {
                        self.note_baseline(&cur, req.model);
                        if let Some(s) = self.snapshot(&a) {
                            return Selection::Account(s);
                        }
                    }
                    if rolled {
                        mgr.log(format!("Weekly window of \"{}\" rolled over; re-ranking", a.name));
                    }
                    if let Some(best) = self.ranked_available(req, now).first().cloned() {
                        let id = best.id.clone();
                        if id != cur {
                            self.set_current(&id, now, mgr, false);
                        }
                        self.note_baseline(&id, req.model);
                        if let Some(s) = self.snapshot(self.account(&id).unwrap()) {
                            return Selection::Account(s);
                        }
                    }
                } else {
                    // Barred only for this model (family bucket / route / cap)? Divert without moving the cursor.
                    let scoped = self.unavailable_reason_for(&a, None, None, req.provider, now).is_none() && !req.exclude.contains(&cur);
                    if let Some(best) = self.ranked_available(req, now).first().cloned() {
                        let id = best.id.clone();
                        self.set_current(&id, now, mgr, scoped);
                        if let Some(s) = self.snapshot(self.account(&id).unwrap()) {
                            return Selection::Account(s);
                        }
                    }
                }
            }
        }

        // 5. Best available.
        if let Some(best) = self.ranked_available(req, now).first().cloned() {
            let id = best.id.clone();
            self.set_current(&id, now, mgr, false);
            self.note_baseline(&id, req.model);
            if let Some(s) = self.snapshot(self.account(&id).unwrap()) {
                return Selection::Account(s);
            }
        }

        // 6. Nothing under threshold. Revalidate one soft-excluded account so a
        //    stale reading cannot pin the fleet in "all exhausted" forever.
        if req.allow_probe {
            let mut probe: Option<(i64, String)> = None;
            for a in &self.accounts {
                if req.exclude.contains(&a.id) {
                    continue;
                }
                match self.unavailable_reason_for(a, req.model, req.advisor_model, req.provider, now) {
                    Some(r) if r.starts_with("quota:") => {}
                    _ => continue,
                }
                if a.last_probe_at.map(|t| now - t < THROTTLE_PROBE_FLOOR_MS).unwrap_or(false) {
                    continue;
                }
                let reset = a.weekly_reset_for(req.model).or(a.quota.unified5h.reset_at).unwrap_or(i64::MAX);
                if probe.as_ref().map(|(r, _)| reset < *r).unwrap_or(true) {
                    probe = Some((reset, a.id.clone()));
                }
            }
            if let Some((_, id)) = probe {
                if let Some(a) = self.account_mut(&id) {
                    a.last_probe_at = Some(now);
                }
                let a = self.account(&id).unwrap().clone();
                mgr.log(format!("All accounts at threshold; revalidating \"{}\" with a live request", a.name));
                if let Some(s) = self.snapshot(&a) {
                    return Selection::Account(s);
                }
            }
        }

        let (retry, reason) = self.exhausted_info(req, now);
        Selection::Exhausted { retry_after_secs: retry, reason }
    }

    /// Why nothing can serve, and when to retry. Only accounts held back by
    /// quota or a rate limit contribute a reset: an account the caller merely
    /// tried (and got a 5xx or a timeout from) is healthy, and its own 5h/7d
    /// reset says nothing about when the upstream will answer again. Using it
    /// turned two overloaded siblings into a 429 with `retry-after: 3600`.
    fn exhausted_info(&self, req: &SelectRequest, now: i64) -> (Option<u64>, String) {
        let mut soonest: Option<i64> = None;
        let mut quota_bound = false;
        let mut parts = Vec::new();
        for a in &self.accounts {
            if let Some(p) = req.provider {
                if a.provider != p {
                    continue;
                }
            }
            let reason = self.unavailable_reason(a, req.model, req.advisor_model, now);
            let held_back =
                reason.as_deref().map(|r| matches!(unavailable_key(r), "throttled" | "capped" | "advisor-capped" | "quota" | "advisor-quota")).unwrap_or(false);
            parts.push(format!("{}: {}", a.name, reason.unwrap_or_else(|| "tried".into())));
            if !held_back {
                continue;
            }
            quota_bound = true;
            let r = a.rate_limited_until.or_else(|| a.quota.soonest_reset());
            if let Some(r) = r {
                if r > now && soonest.map(|s| r < s).unwrap_or(true) {
                    soonest = Some(r);
                }
            }
        }
        let secs = if quota_bound { Some(soonest.map(|s| ((s - now) / 1000).clamp(5, 3600) as u64).unwrap_or(60)) } else { None };
        (secs, parts.join("; "))
    }

    fn try_admit(&mut self, id: &str, now: i64) -> (bool, u64) {
        let storm = self.storm.clone();
        let Some(a) = self.account_mut(id) else { return (true, 0) };
        if !storm.enabled {
            a.in_flight += 1;
            return (true, 0);
        }
        let Some(started) = a.ramp_started_at else {
            a.in_flight += 1;
            return (true, 0);
        };
        let elapsed = (now - started).max(0) as u64;
        if elapsed >= storm.window_ms {
            a.ramp_started_at = None;
            a.in_flight += 1;
            return (true, 0);
        }
        let cap = storm.start_conc.max(1) as u64 + (elapsed / storm.step_ms.max(1)) * storm.step_conc as u64;
        if (a.in_flight as u64) < cap {
            a.in_flight += 1;
            (true, 0)
        } else {
            (false, 50)
        }
    }

    fn account_json(&self, a: &Account, now: i64) -> Value {
        let reason = self.unavailable_reason(a, None, None, now);
        let b = |x: &crate::quota::Bucket| {
            json!({
                "utilization": x.utilization,
                "resetAt": x.reset_at.map(iso),
                "resetInSeconds": x.reset_at.map(|r| ((r - now) / 1000).max(0)),
            })
        };
        json!({
            "id": a.id,
            "name": a.name,
            "type": match a.kind { AccountType::Oauth => "oauth", AccountType::Apikey => "apikey" },
            "provider": a.provider,
            "planType": a.plan_type,
            "pressure": self.pressure(a, None, now).0,
            "email": a.email,
            "accountUuid": a.account_uuid,
            "orgUuid": a.org_uuid,
            "orgName": a.org_name,
            "priority": a.priority,
            "disabled": a.disabled,
            "status": a.status,
            "error": a.error_message,
            "blocked": reason,
            "current": self.current.as_deref() == Some(a.id.as_str()),
            "upstream": a.upstream,
            "tier": a.rate_limit_tier.clone().or(a.seat_tier.clone()).or(a.subscription_type.clone()),
            "tokenExpiresAt": a.expires_at.map(iso),
            "rateLimitedUntil": a.rate_limited_until.map(iso),
            "entitlementDeniedUntil": a.entitlement_denied_until.filter(|u| *u > now).map(iso),
            "unavailable": reason.as_deref().map(unavailable_key),
            "maxUsage": a.max_usage,
            "sessions": self.sessions.stats(now).per_account_active.get(&a.id).copied().unwrap_or(0),
            "probe": match a.kind {
                AccountType::Apikey => json!({ "status": "not-applicable" }),
                _ => self.probe.accounts.get(&a.id).map(|p| json!({
                    "status": p.status,
                    "startedAt": p.started_at.map(iso),
                    "lastProbedAt": p.finished_at.map(iso),
                    "durationMs": p.duration_ms,
                    "error": p.error,
                })).unwrap_or(json!({ "status": "never" })),
            },
            "inFlight": a.in_flight,
            "quota": {
                "unified5h": b(&a.quota.unified5h),
                "unified7d": b(&a.quota.unified7d),
                "unified7dFable": b(&a.quota.unified7d_fable),
                "unified7dSonnet": b(&a.quota.unified7d_sonnet),
                "scopedWeekly": a.quota.scoped_weekly,
                "unifiedStatus": a.quota.unified_status,
                "tokensUsed": a.quota.tokens_used(),
                "requestsUsed": a.quota.requests_used(),
                "resetsAt": a.quota.resets_at,
                "spend": a.quota.spend,
            },
            "usage": a.usage,
            "models": {
                "fable": self.unavailable_reason(a, Some("claude-fable"), None, now).is_none(),
                "sonnet": self.unavailable_reason(a, Some("claude-sonnet"), None, now).is_none(),
                "opus": self.unavailable_reason(a, Some("claude-opus"), None, now).is_none(),
            },
        })
    }

    pub fn status_json(&self, now: i64, session_detail: bool) -> Value {
        let stats = self.sessions.stats(now);
        let current_name = self.current.as_ref().and_then(|c| self.account(c)).map(|a| a.name.clone());
        let sample_model = |patterns: &[String]| -> String {
            patterns.first().map(|g| g.replace('*', "")).filter(|s| !s.is_empty()).map(|s| format!("claude-{s}")).unwrap_or_else(|| "claude-opus".into())
        };
        let eligible = |a: &Account, model: &str| self.unavailable_reason(a, Some(model), None, now).is_none();
        let mut routes: Vec<Value> = self
            .routes
            .iter()
            .map(|r| {
                let model = sample_model(&r.patterns);
                let accounts: Vec<Value> = if r.accounts.is_empty() {
                    self.accounts.iter().map(|a| json!({ "name": a.name, "eligible": eligible(a, &model) })).collect()
                } else {
                    r.accounts
                        .iter()
                        .filter_map(|n| self.account_by_name(n).or_else(|| n.parse::<usize>().ok().and_then(|i| self.accounts.get(i))))
                        .map(|a| json!({ "name": a.name, "eligible": eligible(a, &model) }))
                        .collect()
                };
                json!({
                    "name": r.name,
                    "match": r.patterns,
                    "accounts": accounts,
                    "bucket": r.bucket,
                    "color": r.color,
                    "autocreated": false,
                    "pinned": self.route_pins.get(&r.name).and_then(|id| self.account(id)).map(|a| a.name.clone()),
                })
            })
            .collect();
        // Families metered separately with no configured route surface as auto routes.
        for (family, key) in [("fable", BUCKET_7D_FABLE), ("sonnet", BUCKET_7D_SONNET)] {
            let metered = self.accounts.iter().any(|a| a.quota.bucket(key).and_then(|b| b.utilization).is_some());
            let configured = self.routes.iter().any(|r| r.patterns.iter().any(|g| g.to_ascii_lowercase().contains(family)));
            if metered && !configured {
                let model = format!("claude-{family}");
                routes.push(json!({
                    "name": family,
                    "match": [format!("*{family}*")],
                    "accounts": self.accounts.iter().map(|a| json!({ "name": a.name, "eligible": eligible(a, &model) })).collect::<Vec<_>>(),
                    "bucket": Value::Null,
                    "color": Value::Null,
                    "autocreated": true,
                    "pinned": self.route_pins.get(family).and_then(|id| self.account(id)).map(|a| a.name.clone()),
                }));
            }
        }
        let mut v = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pool": self.pool,
            "uptimeSeconds": (now - self.started_at) / 1000,
            "server": { "uptimeSeconds": (now - self.started_at) / 1000, "startedAt": iso(self.started_at) },
            "current": current_name,
            "currentAccount": current_name,
            "probe": {
                "enabled": self.probe.interval_secs > 0,
                "intervalSeconds": self.probe.interval_secs,
                "running": self.probe.running,
                "lastRunStartedAt": self.probe.last_run_started_at.map(iso),
                "lastRunFinishedAt": self.probe.last_run_finished_at.map(iso),
                "nextRunAt": self.probe.next_run_at.map(iso),
            },
            "warm": { "enabled": self.warmup_secs > 0, "intervalSeconds": self.warmup_secs },
            "updateAvailable": self.update_available,
            "updateCheckedAt": self.update_checked_at,
            "switchThreshold": self.threshold,
            "distributeSessions": self.distribute_sessions,
            "holdSeconds": self.hold_seconds,
            "quotaProbeSeconds": self.probe.interval_secs,
            "stormRamp": self.storm,
            "blockedModels": self.blocked_models,
            "expiryRouting": self.expiry,
            "usageDimensions": self.dimension_usage,
            "routes": routes,
            "accounts": self.accounts.iter().map(|a| self.account_json(a, now)).collect::<Vec<_>>(),
            "sessions": {
                "active": stats.active,
                "known": stats.known,
                "inFlight": stats.in_flight,
                "distribute": self.distribute_sessions,
                "perAccount": self.accounts.iter().map(|a| json!({
                    "name": a.name,
                    "active": stats.per_account_active.get(&a.id).copied().unwrap_or(0),
                })).collect::<Vec<_>>(),
            },
            "clients": self.client_usage,
        });
        if session_detail {
            v["sessions"]["items"] = serde_json::to_value(self.sessions.items(now)).unwrap_or(Value::Null);
        }
        v
    }

    pub fn quota_summary(&self, now: i64) -> Value {
        let weight = |a: &Account| -> Option<f64> {
            let t = a.rate_limit_tier.clone().or(a.seat_tier.clone()).or(a.subscription_type.clone())?.to_ascii_lowercase();
            if t.contains("20x") || t.contains("tier_2") || t.contains("max_20") {
                Some(20.0)
            } else if t.contains("5x") || t.contains("tier_1") || t.contains("max_5") || t == "max" {
                Some(5.0)
            } else if t.contains("pro") || t.contains("standard") || t.contains("team") {
                Some(1.0)
            } else {
                None
            }
        };
        let agg = |key: &str| -> Value {
            let (mut used, mut total) = (0.0, 0.0);
            let mut soonest: Option<i64> = None;
            for a in &self.accounts {
                if a.disabled || a.kind != AccountType::Oauth {
                    continue;
                }
                let Some(w) = weight(a) else { continue };
                let b = a.quota.bucket(key).copied().unwrap_or_default();
                let u = b.utilization.or(if key != BUCKET_7D && key != BUCKET_5H { a.quota.unified7d.utilization } else { None });
                if let Some(u) = u {
                    used += u.min(1.0) * w;
                    total += w;
                }
                if let Some(r) = b.reset_at.or(a.quota.unified7d.reset_at) {
                    if r > now && soonest.map(|s| r < s).unwrap_or(true) {
                        soonest = Some(r);
                    }
                }
            }
            json!({
                "utilization": if total > 0.0 { Some(used / total) } else { None },
                "weight": total,
                "soonestResetAt": soonest.map(iso),
            })
        };
        json!({
            "fleet": {
                "unified5h": agg(BUCKET_5H),
                "unified7d": agg(BUCKET_7D),
                "unified7dFable": agg(BUCKET_7D_FABLE),
                "unified7dSonnet": agg(BUCKET_7D_SONNET),
            },
            "unknownTiers": self.accounts.iter().filter(|a| a.kind == AccountType::Oauth && weight(a).is_none()).map(|a| a.name.clone()).collect::<Vec<_>>(),
            "accounts": self.accounts.iter().map(|a| json!({
                "name": a.name,
                "tier": a.rate_limit_tier.clone().or(a.seat_tier.clone()).or(a.subscription_type.clone()),
                "unified5h": a.quota.unified5h.utilization,
                "unified7d": a.quota.unified7d.utilization,
                "unified7dFable": a.quota.unified7d_fable.utilization,
                "unified7dSonnet": a.quota.unified7d_sonnet.utilization,
                "resetAt5h": a.quota.unified5h.reset_at.map(iso),
                "resetAt7d": a.quota.unified7d.reset_at.map(iso),
            })).collect::<Vec<_>>(),
        })
    }
}

/// Map an internal unavailability reason to the original's stable keys so the
/// status renderer can phrase it in the operator's terms.
pub fn unavailable_key(reason: &str) -> &'static str {
    if reason.starts_with("disabled") {
        "disabled"
    } else if reason.starts_with("rate-limited") {
        "throttled"
    } else if reason.starts_with("error") {
        "error"
    } else if reason.starts_with("oauth not allowed") {
        "entitlement"
    } else if reason.starts_with("advisor-capped") {
        "advisor-capped"
    } else if reason.starts_with("capped") {
        "capped"
    } else if reason.starts_with("quota (advisor)") {
        "advisor-quota"
    } else if reason.starts_with("quota") {
        "quota"
    } else if reason.starts_with("route excludes the advisor") {
        "advisor-route"
    } else if reason.starts_with("route") {
        "route"
    } else if reason.starts_with("upstream refused") {
        "upstream-refused"
    } else if reason.starts_with("other provider") {
        "other-provider"
    } else {
        "unknown"
    }
}

fn mgr_resolve(f: &Fleet, token: &str) -> Option<String> {
    let t = token.trim();
    if let Some((au, ou)) = t.split_once('/') {
        if let Some(a) = f.accounts.iter().find(|a| a.account_uuid.as_deref() == Some(au) && a.org_uuid.as_deref() == Some(ou)) {
            return Some(a.id.clone());
        }
    }
    f.accounts
        .iter()
        .find(|a| a.id == t || a.account_uuid.as_deref() == Some(t) || a.org_uuid.as_deref() == Some(t) || a.name == t || a.email.as_deref() == Some(t))
        .map(|a| a.id.clone())
}

pub fn iso(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|d| d.to_rfc3339()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-pool config: everything the tests exercise lives in the default
    /// pool, which is where a pre-pools config lands after migration.
    fn cfg_with(accounts: Vec<AccountConfig>) -> Config {
        let mut c = Config::default();
        pool_of(&mut c).accounts = accounts;
        c.ensure_account_ids();
        c
    }

    fn pool_of(c: &mut Config) -> &mut crate::config::PoolConfig {
        c.pool_mut(crate::config::DEFAULT_POOL).expect("default pool always exists")
    }

    fn mgr(cfg: &Config) -> Manager {
        Manager::new(cfg, crate::config::DEFAULT_POOL)
    }

    fn acct(name: &str, prio: i32) -> AccountConfig {
        AccountConfig {
            name: name.into(),
            kind: AccountType::Oauth,
            access_token: Some("tok".into()),
            refresh_token: Some("rt".into()),
            expires_at: Some(now_ms() + 3_600_000),
            priority: prio,
            ..Default::default()
        }
    }

    fn select_name(m: &Manager, model: Option<&str>) -> Option<String> {
        match m.select(&SelectRequest { model, allow_probe: true, ..Default::default() }) {
            Selection::Account(s) => Some(s.name),
            _ => None,
        }
    }

    #[test]
    fn priority_then_soonest_reset() {
        let cfg = cfg_with(vec![acct("a", 1), acct("b", 0), acct("c", 0)]);
        let m = mgr(&cfg);
        let ids = m.account_ids();
        // c resets sooner than b → c preferred among priority 0
        m.with(|f| {
            f.account_mut(&ids[1].0).unwrap().quota.unified7d.reset_at = Some(now_ms() + 500_000);
            f.account_mut(&ids[2].0).unwrap().quota.unified7d.reset_at = Some(now_ms() + 100_000);
        });
        assert_eq!(select_name(&m, None).as_deref(), Some("c"));
        // sticky: stays on c
        assert_eq!(select_name(&m, None).as_deref(), Some("c"));
    }

    /// With `distributeSessions` every new session re-ran the account's storm
    /// ramp, so a warm account serving many sessions was throttled back to
    /// `startConc` each time one began. A running or finished ramp is left
    /// alone; only an idle account with no ramp gets one.
    #[test]
    fn new_session_does_not_restart_the_ramp_on_a_warm_account() {
        let mut cfg = cfg_with(vec![acct("a", 0)]);
        pool_of(&mut cfg).distribute_sessions = true;
        let m = mgr(&cfg);
        let id = m.account_ids()[0].0.clone();
        let sel = |sid: &str| m.select(&SelectRequest { session_id: Some(sid), allow_probe: true, ..Default::default() });
        assert!(matches!(sel("11111111-1111-1111-1111-111111111111"), Selection::Account(_)));
        m.record_session(Some("11111111-1111-1111-1111-111111111111"), &id, None);
        let an_hour_ago = now_ms() - 3_600_000;
        m.with(|f| f.account_mut(&id).unwrap().ramp_started_at = Some(an_hour_ago));
        assert!(matches!(sel("22222222-2222-2222-2222-222222222222"), Selection::Account(_)));
        m.with(|f| assert_eq!(f.account(&id).unwrap().ramp_started_at, Some(an_hour_ago), "ramp not restarted for the same account"));
        // A finished ramp on an account still carrying an active session is
        // not restarted either; it is warm.
        m.with(|f| f.account_mut(&id).unwrap().ramp_started_at = None);
        assert!(matches!(sel("33333333-3333-3333-3333-333333333333"), Selection::Account(_)));
        m.with(|f| assert_eq!(f.account(&id).unwrap().ramp_started_at, None, "warm account: no new ramp"));

        // A second, idle account does get a ramp when its first session lands.
        let mut cfg = cfg_with(vec![acct("a", 0), acct("b", 0)]);
        pool_of(&mut cfg).distribute_sessions = true;
        let m = mgr(&cfg);
        let ids = m.account_ids();
        let sel = |sid: &str| match m.select(&SelectRequest { session_id: Some(sid), allow_probe: true, ..Default::default() }) {
            Selection::Account(s) => s.id,
            other => panic!("{other:?}"),
        };
        let first = sel("11111111-1111-1111-1111-111111111111");
        m.record_session(Some("11111111-1111-1111-1111-111111111111"), &first, None);
        let other = ids.iter().map(|(i, _)| i.clone()).find(|i| *i != first).unwrap();
        m.with(|f| assert_eq!(f.account(&other).unwrap().ramp_started_at, None));
        let second = sel("22222222-2222-2222-2222-222222222222");
        assert_eq!(second, other, "new session spreads to the idle account");
        m.with(|f| assert!(f.account(&other).unwrap().ramp_started_at.is_some(), "an idle account ramps up"));
    }

    #[tokio::test]
    async fn probe_refresh_never_disables_account() {
        // A refresh token already known dead. The fast path in ensure_token_fresh
        // returns without any network call, so this exercises the health-mutation
        // gating hermetically.
        let cfg = cfg_with(vec![acct("a", 0)]);
        let m = mgr(&cfg);
        let id = m.account_ids()[0].0.clone();
        m.with(|f| f.account_mut(&id).unwrap().dead_refresh_token = Some("rt".into()));

        // Keep: still no usable credential is minted, but the account stays healthy.
        let cred = m.ensure_token_fresh(&id, false, OnRefreshFail::Keep).await;
        assert_eq!(cred.as_deref(), Some("tok"));
        m.with(|f| {
            let a = f.account(&id).unwrap();
            assert_eq!(a.status, Status::Active);
            assert!(a.error_message.is_none());
            assert_eq!(a.dead_refresh_token.as_deref(), Some("rt"));
        });

        // MarkDead: live traffic disables the account and asks for re-login.
        let _ = m.ensure_token_fresh(&id, false, OnRefreshFail::MarkDead).await;
        m.with(|f| {
            let a = f.account(&id).unwrap();
            assert_eq!(a.status, Status::Error);
            assert!(a.error_message.as_deref().unwrap().contains("corrall login"));
        });
    }

    #[test]
    fn rotates_at_threshold_and_respects_family_bucket() {
        let cfg = cfg_with(vec![acct("a", 0), acct("b", 0)]);
        let m = mgr(&cfg);
        let ids = m.account_ids();
        assert_eq!(select_name(&m, Some("claude-opus-5")).as_deref(), Some("a"));
        // a's fable bucket spent: fable diverts to b, opus stays on a
        m.with(|f| {
            let a = f.account_mut(&ids[0].0).unwrap();
            a.quota.unified7d_fable.utilization = Some(1.0);
            a.quota.unified7d_fable.seen_at = Some(now_ms());
        });
        assert_eq!(select_name(&m, Some("claude-fable-5-1")).as_deref(), Some("b"));
        assert_eq!(select_name(&m, Some("claude-opus-5")).as_deref(), Some("a"));
        // a's 5h spent → everything moves to b
        m.with(|f| f.account_mut(&ids[0].0).unwrap().quota.unified5h.utilization = Some(0.99));
        assert_eq!(select_name(&m, Some("claude-opus-5")).as_deref(), Some("b"));
    }

    #[test]
    fn exhausted_then_probe() {
        let cfg = cfg_with(vec![acct("a", 0)]);
        let m = mgr(&cfg);
        let ids = m.account_ids();
        m.with(|f| {
            let a = f.account_mut(&ids[0].0).unwrap();
            a.quota.unified5h.utilization = Some(1.0);
            a.quota.unified5h.reset_at = Some(now_ms() + 600_000);
        });
        // probe allowed once
        assert!(select_name(&m, None).is_some());
        match m.select(&SelectRequest { allow_probe: true, ..Default::default() }) {
            Selection::Exhausted { retry_after_secs, .. } => assert!(retry_after_secs.unwrap() >= 5),
            other => panic!("expected exhausted, got {other:?}"),
        }
    }

    /// Two healthy accounts the caller already tried (upstream 5xx or a
    /// timeout) are not a quota exhaustion: no reset-derived retry-after.
    #[test]
    fn exhaustion_after_transient_failures_carries_no_reset() {
        let cfg = cfg_with(vec![acct("a", 0), acct("b", 0)]);
        let m = mgr(&cfg);
        let ids = m.account_ids();
        m.with(|f| {
            for (id, _) in &ids {
                let a = f.account_mut(id).unwrap();
                a.quota.unified5h.utilization = Some(0.3);
                a.quota.unified5h.reset_at = Some(now_ms() + 3_600_000 * 4);
                a.quota.unified7d.utilization = Some(0.3);
                a.quota.unified7d.reset_at = Some(now_ms() + 86_400_000 * 6);
            }
        });
        let exclude: HashSet<String> = ids.iter().map(|(id, _)| id.clone()).collect();
        match m.select(&SelectRequest { exclude: exclude.clone(), allow_probe: true, ..Default::default() }) {
            Selection::Exhausted { retry_after_secs, reason } => {
                assert_eq!(retry_after_secs, None, "{reason}");
                assert!(reason.contains("a: tried") && reason.contains("b: tried"), "{reason}");
            }
            other => panic!("expected exhausted, got {other:?}"),
        }
        // Once one of them is genuinely rate-limited its recovery is the answer.
        m.with(|f| f.account_mut(&ids[1].0).unwrap().rate_limited_until = Some(now_ms() + 120_000));
        match m.select(&SelectRequest { exclude, allow_probe: true, ..Default::default() }) {
            Selection::Exhausted { retry_after_secs, .. } => assert!((100..=130).contains(&retry_after_secs.unwrap())),
            other => panic!("expected exhausted, got {other:?}"),
        }
    }

    #[test]
    fn pin_never_fails_over_but_respects_caps() {
        let mut a = acct("a", 0);
        a.max_usage = Some(Threshold::Single(0.5));
        let cfg = cfg_with(vec![a, acct("b", 0)]);
        let m = mgr(&cfg);
        let ids = m.account_ids();
        m.with(|f| f.account_mut(&ids[0].0).unwrap().quota.unified7d.utilization = Some(0.6));
        match m.select(&SelectRequest { pin: Some("a"), ..Default::default() }) {
            Selection::PinUnavailable { reason, .. } => assert!(reason.contains("capped")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(m.select(&SelectRequest { pin: Some("nope"), ..Default::default() }), Selection::PinUnknown));
    }

    #[test]
    fn routes_restrict_accounts() {
        let mut cfg = cfg_with(vec![acct("a", 0), acct("b", 5)]);
        pool_of(&mut cfg).routes.push(RouteConfig { name: "fable".into(), patterns: vec!["*fable*".into()], accounts: vec!["b".into()], ..Default::default() });
        let m = mgr(&cfg);
        assert_eq!(select_name(&m, Some("claude-fable-5-1")).as_deref(), Some("b"));
        assert_eq!(select_name(&m, Some("claude-opus-5")).as_deref(), Some("a"));
    }

    #[test]
    fn subscription_token_never_leaves_anthropic() {
        let mut a = acct("a", 0);
        a.upstream = Some("https://api.deepseek.com/anthropic".into());
        let cfg = cfg_with(vec![a]);
        let m = mgr(&cfg);
        assert!(select_name(&m, None).is_none());
    }

    fn apikey_cfg(key: &str) -> Config {
        let mut c = cfg_with(vec![AccountConfig { name: "k".into(), kind: AccountType::Apikey, api_key: Some(key.into()), ..Default::default() }]);
        // Same id across reloads, as `ensure_account_ids` on a saved file gives.
        pool_of(&mut c).accounts[0].id = Some("acct-k".into());
        c
    }

    /// A rotated API key (no refresh token to compare) must be picked up by a
    /// reload, and only a changed credential clears a 401 error: an unchanged
    /// dead key retried on every reload just dies again.
    #[test]
    fn reload_applies_a_rotated_api_key_and_clears_the_error_only_then() {
        let m = mgr(&apikey_cfg("sk-old"));
        let id = m.account_ids()[0].0.clone();
        assert_eq!(m.credential_of(&id).as_deref(), Some("sk-old"));

        m.mark_error(&id, "upstream rejected the API key (401)");
        m.sync_config(&apikey_cfg("sk-old"), crate::config::DEFAULT_POOL);
        m.with(|f| {
            let a = f.account(&id).unwrap();
            assert_eq!(a.status, Status::Error, "same key: still dead");
            assert!(a.error_message.is_some());
        });

        // Disabling and re-enabling is the operator's explicit retry.
        let mut off = apikey_cfg("sk-old");
        pool_of(&mut off).accounts[0].disabled = true;
        m.sync_config(&off, crate::config::DEFAULT_POOL);
        m.with(|f| assert_eq!(f.account(&id).unwrap().status, Status::Error));
        m.sync_config(&apikey_cfg("sk-old"), crate::config::DEFAULT_POOL);
        m.with(|f| assert_eq!(f.account(&id).unwrap().status, Status::Active, "re-enabled: retried"));

        m.mark_error(&id, "upstream rejected the API key (401)");
        m.sync_config(&apikey_cfg("sk-new"), crate::config::DEFAULT_POOL);
        assert_eq!(m.credential_of(&id).as_deref(), Some("sk-new"));
        m.with(|f| {
            let a = f.account(&id).unwrap();
            assert_eq!(a.status, Status::Active, "a new key lifts the error");
            assert!(a.error_message.is_none());
        });
    }

    /// An OAuth access token rotated on disk with the same refresh token is
    /// taken too, while a pair we refreshed more recently than the file is kept.
    #[test]
    fn reload_takes_a_newer_access_token_and_keeps_our_fresher_one() {
        let mut base = acct("a", 0);
        base.id = Some("acct-a".into());
        let m = mgr(&cfg_with(vec![base.clone()]));
        let id = m.account_ids()[0].0.clone();

        let mut rotated = base.clone();
        rotated.access_token = Some("tok2".into());
        rotated.expires_at = Some(base.expires_at.unwrap() + 60_000);
        m.sync_config(&cfg_with(vec![rotated]), crate::config::DEFAULT_POOL);
        assert_eq!(m.credential_of(&id).as_deref(), Some("tok2"));

        // We refreshed in memory after the file was written: keep ours.
        m.with(|f| {
            let a = f.account_mut(&id).unwrap();
            a.credential = Some("tok3".into());
            a.expires_at = Some(now_ms() + 7_200_000);
        });
        m.sync_config(&cfg_with(vec![base]), crate::config::DEFAULT_POOL);
        assert_eq!(m.credential_of(&id).as_deref(), Some("tok3"));
    }
}
