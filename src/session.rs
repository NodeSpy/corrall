//! Tracks Claude Code sessions by their `x-claude-code-session-id` header so
//! the proxy can report load and, with `distributeSessions`, keep each session
//! pinned to one account per weekly quota bucket for prompt-cache reuse.
//!
//! Hardening: session ids are validated (UUID-shaped or short token) and the
//! map is capped so a client cannot grow it without bound.

use std::collections::HashMap;

use serde::Serialize;

pub const SESSION_KNOWN_TTL_MS: i64 = 60 * 60 * 1000;
pub const SESSION_ACTIVE_TTL_MS: i64 = 2 * 60 * 1000;
pub const MAX_SESSIONS: usize = 10_000;
const SWEEP_INTERVAL_MS: i64 = 60 * 1000;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
    pub cache_read: i64,
    pub cache_creation: i64,
    pub input: i64,
    pub output: i64,
    pub context: i64,
    pub reports: i64,
}

#[derive(Debug, Clone)]
pub struct Session {
    /// account id per weekly bucket
    pub pins: HashMap<String, String>,
    pub first_seen: i64,
    pub last_seen: i64,
    pub count: u64,
    pub in_flight: u32,
    pub client: Option<String>,
    pub tokens: HashMap<String, TokenTotals>,
    /// The account that served the most recent request.
    pub account: Option<String>,
    /// Requests per model, to name the one the session mostly runs on: the
    /// last model seen would flip to the small one on every side request.
    pub models: HashMap<String, u64>,
    /// When the oldest request still open arrived; `None` while idle.
    pub busy_since: Option<i64>,
}

/// Distinct models counted per session. Past this a new name is not counted.
const MAX_MODELS_PER_SESSION: usize = 8;

impl Session {
    /// The model most of this session's requests named.
    pub fn main_model(&self) -> Option<&str> {
        self.models.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))).map(|(m, _)| m.as_str())
    }

    /// Token counts summed over every weekly bucket the session touched.
    pub fn token_totals(&self) -> TokenTotals {
        let mut sum = TokenTotals::default();
        for t in self.tokens.values() {
            sum.cache_read += t.cache_read;
            sum.cache_creation += t.cache_creation;
            sum.input += t.input;
            sum.output += t.output;
            sum.reports += t.reports;
        }
        sum
    }
}

impl TokenTotals {
    /// Share of the prompt tokens that came out of the cache, `None` before
    /// any were counted.
    pub fn cache_hit(&self) -> Option<f64> {
        let prompt = self.cache_read + self.cache_creation + self.input;
        (prompt > 0).then(|| self.cache_read as f64 / prompt as f64)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    pub active: usize,
    pub known: usize,
    pub in_flight: u32,
    /// active sessions per account id
    pub per_account_active: HashMap<String, usize>,
    pub per_account_in_flight: HashMap<String, u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionItem {
    pub id: String,
    pub active: bool,
    pub client: Option<String>,
    pub pins: HashMap<String, String>,
    pub requests: u64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub tokens: HashMap<String, TokenTotals>,
    pub in_flight: u32,
    /// Id of the account that served the most recent request.
    pub account: Option<String>,
    pub model: Option<String>,
    pub busy_since: Option<i64>,
    pub totals: TokenTotals,
}

#[derive(Debug)]
pub struct SessionTracker {
    sessions: HashMap<String, Session>,
    last_sweep: i64,
}

/// Accept UUID-like ids and short opaque tokens; reject anything else so the
/// header cannot carry control characters into logs or grow keys unbounded.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ':')
}

impl Default for SessionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionTracker {
    pub fn new() -> Self {
        Self { sessions: HashMap::new(), last_sweep: 0 }
    }

    fn ensure(&mut self, id: &str, now: i64) -> Option<&mut Session> {
        if !valid_session_id(id) {
            return None;
        }
        if !self.sessions.contains_key(id) {
            if self.sessions.len() >= MAX_SESSIONS {
                self.sweep(now);
                if self.sessions.len() >= MAX_SESSIONS {
                    // Evict the longest-idle session rather than refuse.
                    let k = self.sessions.iter().filter(|(_, s)| s.in_flight == 0).min_by_key(|(_, s)| s.last_seen).map(|(k, _)| k.clone())?;
                    self.sessions.remove(&k);
                }
            }
            self.sessions.insert(
                id.to_string(),
                Session {
                    pins: HashMap::new(),
                    first_seen: now,
                    last_seen: now,
                    count: 0,
                    in_flight: 0,
                    client: None,
                    tokens: HashMap::new(),
                    account: None,
                    models: HashMap::new(),
                    busy_since: None,
                },
            );
        }
        self.sessions.get_mut(id)
    }

    pub fn begin_request(&mut self, id: &str, client: Option<&str>, now: i64) {
        if let Some(s) = self.ensure(id, now) {
            if s.in_flight == 0 {
                s.busy_since = Some(now);
            }
            s.in_flight += 1;
            s.last_seen = now;
            if let Some(c) = client {
                s.client = Some(c.to_string());
            }
        }
    }

    pub fn end_request(&mut self, id: &str, now: i64) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.in_flight = s.in_flight.saturating_sub(1);
            if s.in_flight == 0 {
                s.busy_since = None;
            }
            s.last_seen = now;
        }
        if now - self.last_sweep > SWEEP_INTERVAL_MS {
            self.sweep(now);
        }
    }

    /// Record that a session's request for `model` was served by
    /// `account_id`, which becomes its pin for `bucket`.
    pub fn touch(&mut self, id: &str, account_id: Option<&str>, bucket: Option<&str>, model: Option<&str>, now: i64) {
        if let Some(s) = self.ensure(id, now) {
            s.last_seen = now;
            s.count += 1;
            if let Some(a) = account_id {
                s.account = Some(a.to_string());
            }
            if let Some(m) = model {
                if s.models.contains_key(m) || s.models.len() < MAX_MODELS_PER_SESSION {
                    *s.models.entry(m.to_string()).or_default() += 1;
                }
            }
            if let (Some(a), Some(b)) = (account_id, bucket) {
                s.pins.insert(b.to_string(), a.to_string());
            }
        }
    }

    /// Note which account a session's open request went to. `touch` records
    /// the same once the response starts; until then this is what the
    /// sessions table has to show for a request still waiting.
    pub fn serving(&mut self, id: &str, account_id: &str) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.account = Some(account_id.to_string());
        }
    }

    pub fn pinned_account(&self, id: &str, bucket: &str) -> Option<&str> {
        self.sessions.get(id)?.pins.get(bucket).map(String::as_str)
    }

    /// Any account this session already sits on (for cache affinity when the
    /// bucket-specific pin is absent).
    pub fn any_pin(&self, id: &str) -> Option<&str> {
        self.sessions.get(id)?.pins.values().next().map(String::as_str)
    }

    pub fn record_tokens(&mut self, id: &str, bucket: &str, usage: &serde_json::Value, now: i64) {
        let Some(s) = self.sessions.get_mut(id) else { return };
        let t = s.tokens.entry(bucket.to_string()).or_default();
        let g = |k: &str| usage.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
        t.cache_read += g("cache_read_input_tokens");
        t.cache_creation += g("cache_creation_input_tokens");
        t.input += g("input_tokens");
        t.output += g("output_tokens");
        t.context = g("cache_read_input_tokens") + g("cache_creation_input_tokens") + g("input_tokens");
        t.reports += 1;
        s.last_seen = now;
    }

    /// Drop pins that name an account that no longer exists.
    pub fn remap_accounts(&mut self, live_ids: &[String]) {
        for s in self.sessions.values_mut() {
            s.pins.retain(|_, a| live_ids.contains(a));
            if s.account.as_ref().is_some_and(|a| !live_ids.contains(a)) {
                s.account = None;
            }
        }
    }

    pub fn sweep(&mut self, now: i64) {
        self.last_sweep = now;
        self.sessions.retain(|_, s| s.in_flight > 0 || now - s.last_seen < SESSION_KNOWN_TTL_MS);
    }

    pub fn is_active(s: &Session, now: i64) -> bool {
        s.in_flight > 0 || now - s.last_seen < SESSION_ACTIVE_TTL_MS
    }

    pub fn stats(&self, now: i64) -> SessionStats {
        let mut st = SessionStats { active: 0, known: 0, in_flight: 0, per_account_active: HashMap::new(), per_account_in_flight: HashMap::new() };
        for s in self.sessions.values() {
            if now - s.last_seen >= SESSION_KNOWN_TTL_MS && s.in_flight == 0 {
                continue;
            }
            st.known += 1;
            st.in_flight += s.in_flight;
            let active = Self::is_active(s, now);
            if active {
                st.active += 1;
            }
            let mut seen = std::collections::HashSet::new();
            for a in s.pins.values() {
                if seen.insert(a.clone()) {
                    if active {
                        *st.per_account_active.entry(a.clone()).or_default() += 1;
                    }
                    *st.per_account_in_flight.entry(a.clone()).or_default() += s.in_flight;
                }
            }
        }
        st
    }

    pub fn items(&self, now: i64) -> Vec<SessionItem> {
        let mut v: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.in_flight > 0 || now - s.last_seen < SESSION_KNOWN_TTL_MS)
            .map(|(id, s)| SessionItem {
                id: id.clone(),
                active: Self::is_active(s, now),
                client: s.client.clone(),
                pins: s.pins.clone(),
                requests: s.count,
                first_seen: s.first_seen,
                last_seen: s.last_seen,
                tokens: s.tokens.clone(),
                in_flight: s.in_flight,
                account: s.account.clone(),
                model: s.main_model().map(str::to_string),
                busy_since: s.busy_since,
                totals: s.token_totals(),
            })
            .collect();
        // Sessions with a request open first, then the most recently heard from.
        v.sort_by_key(|i| (i.in_flight == 0, std::cmp::Reverse(i.last_seen)));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_and_expiry() {
        let mut t = SessionTracker::new();
        t.touch("s1", Some("acct-a"), Some("unified7d"), None, 1000);
        assert_eq!(t.pinned_account("s1", "unified7d"), Some("acct-a"));
        assert_eq!(t.pinned_account("s1", "unified7dFable"), None);
        assert_eq!(t.any_pin("s1"), Some("acct-a"));
        t.sweep(1000 + SESSION_KNOWN_TTL_MS + 1);
        assert_eq!(t.pinned_account("s1", "unified7d"), None);
    }

    #[test]
    fn rejects_bad_ids() {
        let mut t = SessionTracker::new();
        t.touch("bad\x1bid", Some("a"), Some("unified7d"), None, 1);
        assert_eq!(t.stats(1).known, 0);
        let long = "x".repeat(200);
        t.touch(&long, Some("a"), Some("unified7d"), None, 1);
        assert_eq!(t.stats(1).known, 0);
    }

    #[test]
    fn in_flight_keeps_active() {
        let mut t = SessionTracker::new();
        t.begin_request("s", Some("alice"), 0);
        let st = t.stats(SESSION_ACTIVE_TTL_MS * 10);
        assert_eq!(st.active, 1);
        t.end_request("s", SESSION_ACTIVE_TTL_MS * 10);
        let st = t.stats(SESSION_ACTIVE_TTL_MS * 20);
        assert_eq!(st.active, 0);
        assert_eq!(st.known, 1);
    }

    #[test]
    fn items_carry_what_the_sessions_table_shows() {
        let mut t = SessionTracker::new();
        // An idle session heard from more recently than the busy one.
        t.touch("idle", Some("acct-b"), Some("unified7d"), Some("claude-opus-5-5"), 5_000);
        t.begin_request("busy", None, 1_000);
        t.begin_request("busy", None, 2_000);
        for _ in 0..3 {
            t.touch("busy", Some("acct-a"), Some("unified7dFable"), Some("claude-fable-5-2"), 2_000);
        }
        // One side request on the small model does not rename the session.
        t.touch("busy", Some("acct-a"), Some("unified7d"), Some("claude-haiku-5-5"), 2_500);
        let usage = serde_json::json!({ "input_tokens": 10, "cache_read_input_tokens": 80, "cache_creation_input_tokens": 10, "output_tokens": 7 });
        t.record_tokens("busy", "unified7dFable", &usage, 2_600);
        t.record_tokens("busy", "unified7d", &usage, 2_700);

        // Before anything has answered, the account the request went to.
        t.begin_request("waiting", None, 5_500);
        t.serving("waiting", "acct-b");
        t.serving("unknown", "acct-b");
        let waiting = t.items(6_000).into_iter().find(|i| i.id == "waiting").unwrap();
        assert_eq!((waiting.account.as_deref(), waiting.requests), (Some("acct-b"), 0));
        assert_eq!(t.items(6_000).len(), 3, "noting an account does not create a session");
        t.end_request("waiting", 5_600);
        t.sessions.remove("waiting");

        let items = t.items(6_000);
        assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["busy", "idle"], "a session with a request open sorts first");
        let busy = &items[0];
        assert_eq!((busy.in_flight, busy.busy_since), (2, Some(1_000)), "busy since the oldest open request");
        assert_eq!(busy.account.as_deref(), Some("acct-a"));
        assert_eq!(busy.model.as_deref(), Some("claude-fable-5-2"));
        assert_eq!((busy.totals.input, busy.totals.cache_read, busy.totals.cache_creation, busy.totals.output), (20, 160, 20, 14));
        assert_eq!(busy.totals.cache_hit(), Some(0.8));
        assert_eq!(items[1].totals.cache_hit(), None, "no tokens counted yet");

        t.end_request("busy", 3_000);
        assert_eq!(t.items(6_000)[0].busy_since, Some(1_000), "one request is still open");
        t.end_request("busy", 4_000);
        let items = t.items(6_000);
        assert_eq!(items[0].id, "idle", "idle sessions sort by recency");
        assert_eq!(items[1].busy_since, None);

        t.remap_accounts(&["acct-b".to_string()]);
        assert_eq!(t.items(6_000)[1].account, None, "a removed account is not named");
    }
}
