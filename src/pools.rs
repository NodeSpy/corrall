//! Named pools: the registry of live fleets, and the URL keyword that routes a
//! request to one of them.
//!
//! Each pool owns a [`Manager`] — its own accounts, rotation strategy and
//! settings — and a request picks its pool from a `/pool/<name>` prefix on the
//! base URL:
//!
//! ```text
//! ANTHROPIC_BASE_URL=http://127.0.0.1:3456/pool/work   → pool "work"
//! ANTHROPIC_BASE_URL=http://127.0.0.1:3456             → the default pool
//! ```
//!
//! Because the prefix sits under the fixed `/pool/` keyword — which no real API
//! path can start with — pool names need no reserved-word list. A pool may be
//! called `v1` or `api` and `/pool/v1/messages` still means "pool v1, path
//! /messages" while `/v1/messages` still means "default pool, path
//! /v1/messages".

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde_json::{json, Value};

use crate::config::{Config, PoolState, State};
use crate::manager::Manager;

/// The URL keyword that introduces a pool name.
pub const POOL_PREFIX: &str = "/pool/";

/// Split a request path into (pool name, remaining path).
///
/// Returns `None` when the path carries no pool prefix, which is every real API
/// path. `/pool/` with nothing after it is not a pool reference either — it
/// stays a literal path so a stray request produces an ordinary upstream 404
/// rather than being silently rerouted.
///
/// ```text
/// /pool/work/v1/messages   → ("work", "/v1/messages")
/// /pool/v1/count_tokens    → ("v1",   "/count_tokens")   // a pool named "v1"
/// /pool/work               → ("work", "/")
/// /pool/work?beta=true     → ("work", "/?beta=true")
/// /v1/messages             → None
/// /pool/                   → None
/// ```
pub fn parse_pool_path(path_and_query: &str) -> Option<(&str, String)> {
    let rest = path_and_query.strip_prefix(POOL_PREFIX)?;
    // The name ends at the next '/', or at the query string, or at the end.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (name, tail) = rest.split_at(end);
    if name.is_empty() {
        return None;
    }
    let tail = if tail.is_empty() {
        "/".to_string()
    } else if tail.starts_with('/') {
        tail.to_string()
    } else {
        // A query or fragment directly after the name: the path is just "/".
        format!("/{tail}")
    };
    Some((name, tail))
}

/// The separator that carries a pool name in CONNECT proxy userinfo.
pub const MITM_POOL_SEP: char = '~';

/// Split a CONNECT userinfo username into (account pin, pool name).
///
/// MITM mode intercepts `api.anthropic.com` directly, so there is no local URL
/// to carry a `/pool/<name>` prefix. The pool rides instead in the proxy
/// userinfo that already carries the account pin:
///
/// ```text
/// HTTPS_PROXY=http://<pin>~<pool>:<key>@127.0.0.1:3456
/// HTTPS_PROXY=http://~<pool>:<key>@127.0.0.1:3456       // pool, no pin
/// HTTPS_PROXY=http://<pin>:<key>@127.0.0.1:3456         // pin, default pool
/// ```
///
/// `~` is unreserved in a URI userinfo and appears in neither account names nor
/// e-mail addresses, so it cannot collide with a pin.
pub fn split_pin_pool(user: &str) -> (Option<String>, Option<String>) {
    let (pin, pool) = match user.split_once(MITM_POOL_SEP) {
        Some((p, pool)) => (p, Some(pool)),
        None => (user, None),
    };
    let clean = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    (clean(pin), pool.and_then(clean))
}

/// The live pools. One [`Manager`] per configured pool, replaced wholesale only
/// when a pool is added or removed; surviving pools keep their manager (and so
/// their live tokens, quota and session affinity) across a reload.
pub struct Pools {
    inner: RwLock<Registry>,
    /// One log bus for every pool: a pool added by a reload publishes to the
    /// same channel the TUI and the headless printer are already reading.
    pub events: tokio::sync::broadcast::Sender<String>,
}

struct Registry {
    map: BTreeMap<String, Manager>,
    default: String,
    /// When each unknown pool name was last warned about.
    unknown: BTreeMap<String, Instant>,
}

/// How often to repeat the warning for one unknown pool name.
const UNKNOWN_WARN_EVERY: Duration = Duration::from_secs(60);
/// Cap on remembered unknown names, so a hostile client cannot grow the map.
const UNKNOWN_WARN_MAX: usize = 64;

impl Pools {
    pub fn new(cfg: &Config) -> Arc<Pools> {
        let reg = Registry { map: BTreeMap::new(), default: cfg.default_pool.clone(), unknown: BTreeMap::new() };
        let (events, _) = tokio::sync::broadcast::channel(256);
        let pools = Arc::new(Pools { inner: RwLock::new(reg), events });
        pools.sync_config(cfg);
        pools
    }

    /// Resolve the pool a request asked for.
    ///
    /// An unknown name degrades to the default pool rather than failing: a
    /// project whose wrapper still points at a renamed pool keeps working. The
    /// warning is rate-limited per name so a stale wrapper cannot flood the log.
    pub fn resolve_request(&self, name: Option<&str>) -> (String, Manager) {
        if let Some(n) = name {
            if let Some(m) = self.get(n) {
                return (n.to_string(), m);
            }
            self.warn_unknown(n);
        }
        self.resolve(None)
    }

    fn warn_unknown(&self, name: &str) {
        let now = Instant::now();
        let mut g = self.inner.write();
        if g.unknown.get(name).is_some_and(|t| now.duration_since(*t) < UNKNOWN_WARN_EVERY) {
            return;
        }
        if g.unknown.len() >= UNKNOWN_WARN_MAX {
            g.unknown.retain(|_, t| now.duration_since(*t) < UNKNOWN_WARN_EVERY);
        }
        g.unknown.insert(name.to_string(), now);
        let default = g.default.clone();
        drop(g);
        tracing::warn!("unknown pool \"{}\" — serving from \"{default}\"", crate::security::safe_text(name, 32));
    }

    /// The name of the pool serving unprefixed requests.
    pub fn default_name(&self) -> String {
        self.inner.read().default.clone()
    }

    /// Pool names, default first then the rest sorted.
    pub fn names(&self) -> Vec<String> {
        let g = self.inner.read();
        let mut names: Vec<String> = g.map.keys().filter(|k| **k != g.default).cloned().collect();
        names.sort();
        if g.map.contains_key(&g.default) {
            names.insert(0, g.default.clone());
        }
        names
    }

    pub fn get(&self, name: &str) -> Option<Manager> {
        self.inner.read().map.get(name).cloned()
    }

    /// The default pool's manager. Present for any validated config; falls back
    /// to any pool at all rather than panicking on a half-built registry.
    pub fn default(&self) -> Manager {
        let g = self.inner.read();
        g.map.get(&g.default).or_else(|| g.map.values().next()).cloned().expect("registry always holds at least one pool")
    }

    /// Resolve a pool name to its manager, falling back to the default pool.
    /// Returns the name actually used, which is what gets logged and reported.
    pub fn resolve(&self, name: Option<&str>) -> (String, Manager) {
        let g = self.inner.read();
        if let Some(n) = name {
            if let Some(m) = g.map.get(n) {
                return (n.to_string(), m.clone());
            }
        }
        let def = g.default.clone();
        match g.map.get(&def) {
            Some(m) => (def, m.clone()),
            None => {
                let (n, m) = g.map.iter().next().expect("registry always holds at least one pool");
                (n.clone(), m.clone())
            }
        }
    }

    /// Every pool with its manager, in display order.
    pub fn each(&self) -> Vec<(String, Manager)> {
        let g = self.inner.read();
        let mut out: Vec<(String, Manager)> = Vec::with_capacity(g.map.len());
        if let Some(m) = g.map.get(&g.default) {
            out.push((g.default.clone(), m.clone()));
        }
        for (k, m) in &g.map {
            if *k != g.default {
                out.push((k.clone(), m.clone()));
            }
        }
        out
    }

    /// Reconcile the registry with a (re)loaded config: create managers for new
    /// pools, drop removed ones, and re-sync the survivors. Returns the total
    /// number of accounts added across all pools.
    pub fn sync_config(&self, cfg: &Config) -> usize {
        let mut added = 0;
        let mut g = self.inner.write();
        g.default = cfg.default_pool.clone();
        g.map.retain(|name, _| {
            let keep = cfg.pools.contains_key(name);
            if !keep {
                tracing::info!("pool \"{name}\" removed");
            }
            keep
        });
        for name in cfg.pools.keys() {
            match g.map.get(name) {
                Some(m) => added += m.sync_config(cfg, name),
                None => {
                    if !g.map.is_empty() {
                        tracing::info!("pool \"{name}\" added");
                    }
                    let m = Manager::with_events(cfg, name, self.events.clone());
                    added += m.account_ids().len();
                    g.map.insert(name.clone(), m);
                }
            }
        }
        added
    }

    /// The merged status document.
    ///
    /// The default pool's fields sit at the top level — the exact shape every
    /// pre-pools client reads — and `pools` carries one entry per pool for
    /// clients that know about them.
    pub fn status(&self, detail: bool) -> Value {
        let default = self.default_name();
        let per_pool: Vec<Value> = self
            .each()
            .into_iter()
            .map(|(name, m)| {
                let mut s = m.status(detail);
                s["default"] = json!(name == default);
                s
            })
            .collect();
        let mut st = per_pool.first().cloned().unwrap_or_else(|| json!({}));
        st["defaultPool"] = json!(default);
        st["pools"] = json!(per_pool);
        st
    }

    /// Snapshot every pool's runtime state into one file-shaped `State`.
    pub fn export_state(&self) -> State {
        let default = self.default_name();
        let mut st = State { version: 2, saved_at: Some(chrono::Utc::now().to_rfc3339()), ..Default::default() };
        for (name, m) in self.each() {
            st.set_pool(&name, &default, m.export_pool_state());
        }
        st
    }

    /// Restore each pool from a state file. A pre-pools file has only the
    /// flattened top-level section, which restores into the default pool.
    pub fn restore_state(&self, st: &State) {
        let default = self.default_name();
        for (name, m) in self.each() {
            if let Some(ps) = st.pool(&name, &default) {
                m.restore_pool_state(ps);
            }
        }
    }

    /// Account ids across every pool, for "is anything configured at all" checks.
    pub fn account_count(&self) -> usize {
        self.each().iter().map(|(_, m)| m.account_ids().len()).sum()
    }

    /// Find the pool holding an account, by any of the names `resolve_pin`
    /// accepts. The default pool is searched first.
    pub fn find_account(&self, needle: &str) -> Option<(String, Manager, String)> {
        self.each().into_iter().find_map(|(name, m)| m.resolve_pin(needle).map(|id| (name, m, id)))
    }
}

impl PoolState {
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.client_usage.is_empty() && self.dimension_usage.is_null()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_paths_split_on_the_keyword() {
        assert_eq!(parse_pool_path("/pool/work/v1/messages"), Some(("work", "/v1/messages".into())));
        assert_eq!(parse_pool_path("/pool/work"), Some(("work", "/".into())));
        assert_eq!(parse_pool_path("/pool/work/"), Some(("work", "/".into())));
        assert_eq!(parse_pool_path("/pool/a/v1/messages?beta=true"), Some(("a", "/v1/messages?beta=true".into())));
        assert_eq!(parse_pool_path("/pool/work?beta=true"), Some(("work", "/?beta=true".into())));
    }

    #[test]
    fn no_pool_name_is_reserved() {
        // The keyword makes these unambiguous: a pool may be named after any
        // API path segment.
        assert_eq!(parse_pool_path("/pool/v1/messages"), Some(("v1", "/messages".into())));
        assert_eq!(parse_pool_path("/pool/api/oauth/token"), Some(("api", "/oauth/token".into())));
        assert_eq!(parse_pool_path("/pool/corrall/status"), Some(("corrall", "/status".into())));
        assert_eq!(parse_pool_path("/pool/pool/x"), Some(("pool", "/x".into())));
    }

    #[test]
    fn real_api_paths_carry_no_pool() {
        assert_eq!(parse_pool_path("/v1/messages"), None);
        assert_eq!(parse_pool_path("/api/oauth/token"), None);
        assert_eq!(parse_pool_path("/corrall/status"), None);
        assert_eq!(parse_pool_path("/"), None);
        assert_eq!(parse_pool_path(""), None);
        // Not the keyword: a path that merely starts with the same letters.
        assert_eq!(parse_pool_path("/pools/work/v1"), None);
        assert_eq!(parse_pool_path("/pool"), None);
    }

    #[test]
    fn bare_keyword_is_a_literal_path() {
        // No name to route on: leave it alone rather than guess.
        assert_eq!(parse_pool_path("/pool/"), None);
        assert_eq!(parse_pool_path("/pool//v1/messages"), None);
        assert_eq!(parse_pool_path("/pool/?x=1"), None);
    }

    /// MITM mode has no local URL to hang a path prefix on, so the pool rides
    /// the CONNECT username beside the optional account pin.
    #[test]
    fn mitm_username_carries_pin_and_pool() {
        let s = |u: &str| {
            let (pin, pool) = split_pin_pool(u);
            (pin.unwrap_or_default(), pool.unwrap_or_default())
        };
        assert_eq!(s("acct1~work"), ("acct1".into(), "work".into()));
        assert_eq!(s("~work"), (String::new(), "work".into()));
        assert_eq!(s("acct1"), ("acct1".into(), String::new()));
        assert_eq!(s(""), (String::new(), String::new()));
        // An e-mail address is a legal pin and contains no `~`.
        assert_eq!(s("me@example.com~work"), ("me@example.com".into(), "work".into()));
        assert_eq!(s("me@example.com"), ("me@example.com".into(), String::new()));
        // A trailing separator names no pool; do not invent one.
        assert_eq!(s("acct1~"), ("acct1".into(), String::new()));
    }
}
