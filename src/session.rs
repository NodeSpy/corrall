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
                Session { pins: HashMap::new(), first_seen: now, last_seen: now, count: 0, in_flight: 0, client: None, tokens: HashMap::new() },
            );
        }
        self.sessions.get_mut(id)
    }

    pub fn begin_request(&mut self, id: &str, client: Option<&str>, now: i64) {
        if let Some(s) = self.ensure(id, now) {
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
            s.last_seen = now;
        }
        if now - self.last_sweep > SWEEP_INTERVAL_MS {
            self.sweep(now);
        }
    }

    /// Record that a session was served by `account_id` for `bucket`.
    pub fn touch(&mut self, id: &str, account_id: Option<&str>, bucket: Option<&str>, now: i64) {
        if let Some(s) = self.ensure(id, now) {
            s.last_seen = now;
            s.count += 1;
            if let (Some(a), Some(b)) = (account_id, bucket) {
                s.pins.insert(b.to_string(), a.to_string());
            }
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
            })
            .collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.last_seen));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_and_expiry() {
        let mut t = SessionTracker::new();
        t.touch("s1", Some("acct-a"), Some("unified7d"), 1000);
        assert_eq!(t.pinned_account("s1", "unified7d"), Some("acct-a"));
        assert_eq!(t.pinned_account("s1", "unified7dFable"), None);
        assert_eq!(t.any_pin("s1"), Some("acct-a"));
        t.sweep(1000 + SESSION_KNOWN_TTL_MS + 1);
        assert_eq!(t.pinned_account("s1", "unified7d"), None);
    }

    #[test]
    fn rejects_bad_ids() {
        let mut t = SessionTracker::new();
        t.touch("bad\x1bid", Some("a"), Some("unified7d"), 1);
        assert_eq!(t.stats(1).known, 0);
        let long = "x".repeat(200);
        t.touch(&long, Some("a"), Some("unified7d"), 1);
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
}
