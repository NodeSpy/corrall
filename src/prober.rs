//! Opt-in background quota probe: reads each OAuth account's zero-spend usage
//! endpoint so idle accounts' quota stays fresh. Per-pool: off for a pool
//! unless its `quotaProbeSeconds` > 0.
//!
//! The usage endpoint rate-limits the caller, not the account: when it says
//! 429 it says so for every account of the fleet within the same second. A
//! run that hits one therefore stops instead of asking twice more, and the
//! pool's next run is pushed out. Manual probes (the TUI's `p`) share the
//! same lock as the schedule and are refused while one is running or ran
//! moments ago, so a few key presses cannot turn into a burst.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::manager::{Manager, OnRefreshFail};
use crate::oauth::{fetch_profile, fetch_usage, UsageResult};
use crate::pools::Pools;

/// How often the loop wakes to see which pools are due.
const TICK: Duration = Duration::from_secs(5);
/// Floor on a pool's probe interval, whatever its config says.
const MIN_INTERVAL: u64 = 30;
/// Longest a rate-limited run may push the next one out.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Least time between two operator-requested probes.
const MANUAL_MIN_GAP: Duration = Duration::from_secs(30);

/// Per-account start offset within a probe run, so a fleet does not burst the
/// token/usage endpoints simultaneously.
const PROBE_STAGGER_MS: u64 = 300;

/// What became of an operator's request for an immediate probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualProbe {
    /// A run was started in the background.
    Started,
    /// A scheduled or manual run is in progress; nothing was started.
    AlreadyRunning,
    /// The last manual run was too recent; try again after `wait_secs`.
    TooSoon { wait_secs: u64 },
}

#[derive(Clone)]
pub struct Prober {
    pools: Arc<Pools>,
    /// Per pool: when it was last probed, whether it was on last tick, and any
    /// backoff a rate-limited run imposed.
    seen: Arc<Mutex<BTreeMap<String, Seen>>>,
    /// Held for the whole of a run, scheduled or manual, so two never overlap.
    running: Arc<tokio::sync::Mutex<()>>,
    last_manual: Arc<Mutex<Option<Instant>>>,
}

#[derive(Clone, Copy)]
struct Seen {
    last: Option<Instant>,
    on: bool,
    /// Do not probe before this, whatever the interval says.
    not_before: Option<Instant>,
}

impl Seen {
    const OFF: Seen = Seen { last: None, on: false, not_before: None };
}

/// How a run ended, as far as scheduling cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOutcome {
    Completed,
    /// The usage endpoint rate-limited the caller; the run stopped early.
    RateLimited,
}

impl Prober {
    pub fn new(pools: Arc<Pools>) -> Prober {
        Prober { pools, seen: Arc::new(Mutex::new(BTreeMap::new())), running: Arc::new(tokio::sync::Mutex::new(())), last_manual: Arc::new(Mutex::new(None)) }
    }

    /// The default pool's interval, for the one-line summary in the TUI header.
    pub fn interval(&self) -> u64 {
        self.pools.default().probe_seconds()
    }

    /// Run forever, waking every [`TICK`] and probing each pool whose own
    /// interval has elapsed. Interval changes are picked up from the managers,
    /// which a reload has already re-synced.
    pub async fn run(self) {
        loop {
            for (name, m) in self.pools.each() {
                let secs = m.probe_seconds();
                let Some(newly_on) = self.claim_due(&name, secs, Instant::now()) else { continue };
                if newly_on {
                    m.log(format!("Quota probe enabled for pool \"{name}\" (every {secs}s)"));
                }
                let _g = self.running.lock().await;
                self.probe_pool(&name, &m).await;
            }
            tokio::time::sleep(TICK).await;
        }
    }

    /// Mark `pool` as probed now if its interval has elapsed. `None` means not
    /// due (or switched off, or backing off); `Some(true)` means this is the
    /// pool's first probe since it was turned on, which is worth a log line.
    ///
    /// Sync on purpose: it keeps the lock guard out of `run`'s future, which
    /// otherwise would not be `Send`.
    fn claim_due(&self, pool: &str, secs: u64, now: Instant) -> Option<bool> {
        let mut seen = self.seen.lock();
        let prev = seen.get(pool).copied().unwrap_or(Seen::OFF);
        if secs == 0 {
            seen.insert(pool.to_string(), Seen::OFF);
            return None;
        }
        if prev.not_before.is_some_and(|t| now < t) {
            return None;
        }
        if prev.last.is_some_and(|t| now.duration_since(t) < Duration::from_secs(secs.max(MIN_INTERVAL))) {
            return None;
        }
        seen.insert(pool.to_string(), Seen { last: Some(now), on: true, not_before: None });
        Some(!prev.on)
    }

    /// After a rate-limited run: hold `pool` off for twice its interval. The
    /// returned instant is what the status view shows as the next run.
    fn back_off(&self, pool: &str, secs: u64, now: Instant) -> Instant {
        let wait = Duration::from_secs(secs.max(MIN_INTERVAL) * 2).min(MAX_BACKOFF);
        let until = now + wait;
        let mut seen = self.seen.lock();
        let entry = seen.entry(pool.to_string()).or_insert(Seen::OFF);
        entry.not_before = Some(until);
        until
    }

    /// Ask for an immediate probe of every pool, regardless of interval (the
    /// TUI's `p` key). Refused while a run is in progress or when the last
    /// manual run was under [`MANUAL_MIN_GAP`] ago; otherwise the run happens
    /// in the background and the caller sees its results in status.
    pub fn request_manual(&self) -> ManualProbe {
        let now = Instant::now();
        {
            let mut last = self.last_manual.lock();
            if let Some(t) = *last {
                let since = now.duration_since(t);
                if since < MANUAL_MIN_GAP {
                    return ManualProbe::TooSoon { wait_secs: (MANUAL_MIN_GAP - since).as_secs().max(1) };
                }
            }
            if self.running.try_lock().is_err() {
                return ManualProbe::AlreadyRunning;
            }
            *last = Some(now);
        }
        let p = self.clone();
        tokio::spawn(async move { p.probe_all().await });
        ManualProbe::Started
    }

    /// Probe every pool now, waiting for any run in progress to finish first.
    pub async fn probe_all(&self) {
        let _g = self.running.lock().await;
        for (name, m) in self.pools.each() {
            self.probe_pool(&name, &m).await;
        }
    }

    async fn probe_pool(&self, name: &str, m: &Manager) {
        // Each pool reports its own run, so the status view can say when this
        // pool last probed and when it is next due.
        let started = Instant::now();
        let now = crate::quota::now_ms();
        let secs = m.probe_seconds();
        m.probe_run_started(now, if secs > 0 { Some(now + secs as i64 * 1000) } else { None });
        // Stagger the per-account starts so a multi-account pool does not burst
        // the token/usage endpoints at once (which draws HTTP 429 at boot).
        // `halted` flips on the first 429 and the accounts still waiting on
        // their stagger step aside instead of drawing two more.
        let halted = Arc::new(AtomicBool::new(false));
        let tasks: Vec<_> = m
            .oauth_accounts()
            .into_iter()
            .enumerate()
            .map(|(i, (id, name))| self.probe_one(m, id, name, i as u64 * PROBE_STAGGER_MS, halted.clone()))
            .collect();
        let outcomes = futures_util::future::join_all(tasks).await;
        let finished = crate::quota::now_ms();
        if outcomes.contains(&RunOutcome::RateLimited) && secs > 0 {
            let until = self.back_off(name, secs, Instant::now());
            let wait = until.saturating_duration_since(started).as_secs();
            m.log(format!("Quota probe for pool \"{name}\" was rate-limited; next run in {wait}s"));
            m.probe_run_finished(finished, Some(finished + wait as i64 * 1000));
        } else {
            m.probe_run_finished(finished, None);
        }
    }

    async fn probe_one(&self, m: &Manager, id: String, name: String, delay_ms: u64, halted: Arc<AtomicBool>) -> RunOutcome {
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        let started = crate::quota::now_ms();
        if halted.load(Ordering::Acquire) {
            m.probe_account_status(&id, "skipped", started, Some(started), Some("usage endpoint rate-limited this run".into()));
            return RunOutcome::Completed;
        }
        m.probe_account_status(&id, "running", started, None, None);
        // The probe is diagnostic: it may refresh, but a failure must never
        // disable the account (OnRefreshFail::Keep). Only live request traffic
        // marks a refresh token dead.
        let Some(cred) = m.ensure_token_fresh(&id, false, OnRefreshFail::Keep).await else {
            m.probe_account_status(&id, "error", started, Some(crate::quota::now_ms()), Some("no usable token".into()));
            return RunOutcome::Completed;
        };
        let mut result = tokio::time::timeout(Duration::from_secs(15), fetch_usage(&cred)).await.unwrap_or(UsageResult::Error("probe timed out".into()));
        if matches!(result, UsageResult::Unauthorized) {
            if let Some(c2) = m.ensure_token_fresh(&id, true, OnRefreshFail::Keep).await {
                result = tokio::time::timeout(Duration::from_secs(15), fetch_usage(&c2)).await.unwrap_or(UsageResult::Error("probe timed out".into()));
            }
        }
        let finished = crate::quota::now_ms();
        match result {
            UsageResult::Ok(u) => {
                m.apply_usage(&id, &u);
                if m.needs_profile(&id) {
                    if let Some(c) = m.credential_of(&id) {
                        if let Ok(p) = fetch_profile(&c).await {
                            m.apply_profile(&id, &p);
                        }
                    }
                }
                m.probe_account_status(&id, "ok", started, Some(finished), None);
                RunOutcome::Completed
            }
            UsageResult::Unauthorized => {
                m.log(format!("Quota probe: \"{name}\" token rejected"));
                m.probe_account_status(&id, "error", started, Some(finished), Some("token rejected".into()));
                RunOutcome::Completed
            }
            UsageResult::RateLimited(body) => {
                halted.store(true, Ordering::Release);
                // Through the pool log, not `tracing::warn!`: the TUI installs
                // no tracing subscriber, so only the pool log reaches both it
                // and the headless journal.
                m.log(format!("Quota probe for \"{name}\": usage endpoint said 429 ({})", crate::security::safe_text(&body, 120)));
                m.probe_account_status(&id, "rate-limited", started, Some(finished), Some(crate::security::safe_text(&body, 120)));
                RunOutcome::RateLimited
            }
            UsageResult::Error(e) => {
                m.log(format!("Quota probe for \"{name}\" failed: {}", crate::security::safe_text(&e, 200)));
                let status = if e.contains("timed out") { "timeout" } else { "error" };
                m.probe_account_status(&id, status, started, Some(finished), Some(crate::security::safe_text(&e, 120)));
                RunOutcome::Completed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn prober() -> Prober {
        Prober::new(Pools::new(&Config::default()))
    }

    #[test]
    fn claim_due_respects_interval_floor_and_off() {
        let p = prober();
        let t0 = Instant::now();
        assert_eq!(p.claim_due("default", 300, t0), Some(true), "first claim after enabling logs");
        assert_eq!(p.claim_due("default", 300, t0 + Duration::from_secs(299)), None);
        assert_eq!(p.claim_due("default", 300, t0 + Duration::from_secs(300)), Some(false));
        // Below the floor the floor wins.
        assert_eq!(p.claim_due("default", 5, t0 + Duration::from_secs(310)), None);
        assert_eq!(p.claim_due("default", 5, t0 + Duration::from_secs(330)), Some(false));
        // Switching off forgets the pool; switching back on logs again.
        assert_eq!(p.claim_due("default", 0, t0 + Duration::from_secs(400)), None);
        assert_eq!(p.claim_due("default", 300, t0 + Duration::from_secs(401)), Some(true));
    }

    #[test]
    fn back_off_holds_the_pool_for_twice_its_interval() {
        let p = prober();
        let t0 = Instant::now();
        assert_eq!(p.claim_due("default", 300, t0), Some(true));
        let until = p.back_off("default", 300, t0);
        assert_eq!(until, t0 + Duration::from_secs(600));
        // Due by the plain interval, but still backing off.
        assert_eq!(p.claim_due("default", 300, t0 + Duration::from_secs(301)), None);
        assert_eq!(p.claim_due("default", 300, t0 + Duration::from_secs(599)), None);
        assert_eq!(p.claim_due("default", 300, t0 + Duration::from_secs(600)), Some(false));
        // The backoff is spent once a run happens.
        assert_eq!(p.claim_due("default", 300, t0 + Duration::from_secs(900)), Some(false));
    }

    #[test]
    fn back_off_is_floored_and_capped() {
        let p = prober();
        let t0 = Instant::now();
        assert_eq!(p.back_off("a", 5, t0), t0 + Duration::from_secs(MIN_INTERVAL * 2));
        assert_eq!(p.back_off("b", 86_400, t0), t0 + MAX_BACKOFF);
    }

    #[tokio::test]
    async fn manual_probe_is_gated() {
        let p = prober();
        assert_eq!(p.request_manual(), ManualProbe::Started);
        // The background run on an empty fleet is over in a moment; the gap
        // rule still refuses a second press.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(p.request_manual(), ManualProbe::TooSoon { wait_secs } if (1..=30).contains(&wait_secs)));
    }

    #[tokio::test]
    async fn manual_probe_refused_while_a_run_holds_the_lock() {
        let p = prober();
        let _g = p.running.lock().await;
        assert_eq!(p.request_manual(), ManualProbe::AlreadyRunning);
        // A refusal for running does not consume the manual gap.
        drop(_g);
        assert_eq!(p.request_manual(), ManualProbe::Started);
    }
}
