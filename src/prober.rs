//! Opt-in background quota probe: reads each OAuth account's zero-spend usage
//! endpoint so idle accounts' quota stays fresh. Per-pool: off for a pool
//! unless its `quotaProbeSeconds` > 0.
//!
//! Each account keeps its own clock. When a pool comes on, every account is
//! probed at once (a few hundred ms apart) and then spread evenly across the
//! interval: with six accounts on a 300s interval one is probed every 50s, so
//! the fleet's quota is never all the same age. An account that arrives later
//! (a `corrall login` while the server runs) is probed on the next tick, which
//! a reload wakes up early, and then joins the rotation from that moment.
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
    /// Per pool: whether it was on last tick, each account's next due time,
    /// and any backoff a rate-limited run imposed.
    seen: Arc<Mutex<BTreeMap<String, Seen>>>,
    /// Held for the whole of a run, scheduled or manual, so two never overlap.
    running: Arc<tokio::sync::Mutex<()>>,
    last_manual: Arc<Mutex<Option<Instant>>>,
    /// Wakes the loop before its next tick; a reload rings it so an account
    /// just added by a login is probed right away.
    wake: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Default)]
struct Seen {
    on: bool,
    /// Do not probe before this, whatever the schedule says.
    not_before: Option<Instant>,
    /// Account id → when it is next due. Accounts no longer in the pool are
    /// dropped on the next tick; unknown ones are due at once.
    due: BTreeMap<String, Instant>,
}

/// What a tick found due for one pool.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    /// This is the pool's first tick since it was turned on: worth a log line.
    newly_on: bool,
    /// `(id, name)` of the accounts to probe now.
    accounts: Vec<(String, String)>,
    /// The earliest remaining due time, for the status view.
    next: Option<Instant>,
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
        Prober {
            pools,
            seen: Arc::new(Mutex::new(BTreeMap::new())),
            running: Arc::new(tokio::sync::Mutex::new(())),
            last_manual: Arc::new(Mutex::new(None)),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Cut the current tick short. A reload calls this after re-syncing the
    /// pools so a newly added account does not wait out the rest of [`TICK`].
    pub fn kick(&self) {
        self.wake.notify_one();
    }

    /// The default pool's interval, for the one-line summary in the TUI header.
    pub fn interval(&self) -> u64 {
        self.pools.default().probe_seconds()
    }

    /// Run forever, waking every [`TICK`] (or sooner when kicked) and probing
    /// each account whose own due time has passed. Interval changes are picked
    /// up from the managers, which a reload has already re-synced.
    pub async fn run(self) {
        loop {
            for (name, m) in self.pools.each() {
                let secs = m.probe_seconds();
                let Some(claim) = self.claim_due(&name, secs, &m.oauth_accounts(), Instant::now()) else { continue };
                if claim.newly_on {
                    m.log(format!("Quota probe enabled for pool \"{name}\" (every {secs}s, accounts staggered)"));
                }
                if claim.accounts.is_empty() {
                    continue;
                }
                let _g = self.running.lock().await;
                self.probe_pool(&name, &m, claim.accounts, claim.next).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(TICK) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    /// Work out which of `accounts` are due in `pool` and book their next
    /// turn. `None` means nothing to do (off, backing off, or nobody due yet).
    ///
    /// On the pool's first tick every account is due, and the following turns
    /// are spread across the interval: account `i` of `n` is next due at
    /// `now + interval + i * interval / n`, and every `interval` after that.
    /// An account without a booking (added since the last tick) is due now and
    /// then keeps the phase of its arrival. A booking that has slipped more
    /// than a whole interval (a long backoff) restarts from `now`.
    ///
    /// Sync on purpose: it keeps the lock guard out of `run`'s future, which
    /// otherwise would not be `Send`.
    fn claim_due(&self, pool: &str, secs: u64, accounts: &[(String, String)], now: Instant) -> Option<Claim> {
        let mut seen = self.seen.lock();
        if secs == 0 {
            seen.remove(pool);
            return None;
        }
        let entry = seen.entry(pool.to_string()).or_default();
        if entry.not_before.is_some_and(|t| now < t) {
            return None;
        }
        let interval = Duration::from_secs(secs.max(MIN_INTERVAL));
        let newly_on = !entry.on;
        entry.on = true;
        entry.due.retain(|id, _| accounts.iter().any(|(a, _)| a == id));
        let n = accounts.len().max(1) as u32;
        let mut picked = Vec::new();
        for (i, (id, name)) in accounts.iter().enumerate() {
            let next = match entry.due.get(id) {
                Some(&t) if t > now => continue,
                // Already booked: keep the phase unless it has slipped a whole
                // interval, which would only queue up a burst of catch-up runs.
                Some(&t) if t + interval > now => t + interval,
                Some(_) => now + interval,
                // First tick of the pool: spread the second round out. Later
                // arrivals simply keep the phase of the tick they arrived on.
                None if newly_on => now + interval + interval * i as u32 / n,
                None => now + interval,
            };
            entry.due.insert(id.clone(), next);
            picked.push((id.clone(), name.clone()));
        }
        if picked.is_empty() && !newly_on {
            return None;
        }
        Some(Claim { newly_on, accounts: picked, next: entry.due.values().min().copied() })
    }

    /// The earliest booked due time in `pool`, if any.
    fn next_due(&self, pool: &str) -> Option<Instant> {
        self.seen.lock().get(pool).and_then(|s| s.due.values().min().copied())
    }

    /// After a rate-limited run: hold `pool` off for twice its interval. The
    /// returned instant is what the status view shows as the next run.
    fn back_off(&self, pool: &str, secs: u64, now: Instant) -> Instant {
        let wait = Duration::from_secs(secs.max(MIN_INTERVAL) * 2).min(MAX_BACKOFF);
        let until = now + wait;
        let mut seen = self.seen.lock();
        seen.entry(pool.to_string()).or_default().not_before = Some(until);
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
            // The schedule is left alone: a manual run is extra, not a reset.
            let next = self.next_due(&name);
            self.probe_pool(&name, &m, m.oauth_accounts(), next).await;
        }
    }

    /// Probe `accounts` of pool `name`. `next` is the schedule's earliest
    /// remaining due time, shown by status as the next run.
    async fn probe_pool(&self, name: &str, m: &Manager, accounts: Vec<(String, String)>, next: Option<Instant>) {
        // Each pool reports its own run, so the status view can say when this
        // pool last probed and when it is next due.
        let started = Instant::now();
        let now = crate::quota::now_ms();
        let secs = m.probe_seconds();
        m.probe_run_started(now, next.map(|t| now + t.saturating_duration_since(started).as_millis() as i64));
        // Stagger the per-account starts so several accounts due on the same
        // tick do not burst the token/usage endpoints at once (which draws
        // HTTP 429 at boot). `halted` flips on the first 429 and the accounts
        // still waiting on their stagger step aside instead of drawing two more.
        let halted = Arc::new(AtomicBool::new(false));
        let tasks: Vec<_> =
            accounts.into_iter().enumerate().map(|(i, (id, name))| self.probe_one(m, id, name, i as u64 * PROBE_STAGGER_MS, halted.clone())).collect();
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

    fn accts(ids: &[&str]) -> Vec<(String, String)> {
        ids.iter().map(|i| (i.to_string(), format!("name-{i}"))).collect()
    }

    fn ids(c: &Claim) -> Vec<&str> {
        c.accounts.iter().map(|(id, _)| id.as_str()).collect()
    }

    #[test]
    fn first_tick_probes_everyone_then_spreads_them_across_the_interval() {
        let p = prober();
        let t0 = Instant::now();
        let a = accts(&["a", "b", "c"]);
        let c = p.claim_due("default", 300, &a, t0).expect("first tick is due");
        assert!(c.newly_on, "first claim after enabling logs");
        assert_eq!(ids(&c), ["a", "b", "c"]);
        assert_eq!(c.next, Some(t0 + Duration::from_secs(300)), "the first account is next, a whole interval on");
        // Nobody is due again before the interval.
        assert_eq!(p.claim_due("default", 300, &a, t0 + Duration::from_secs(299)), None);
        // Then one at a time, 100s apart: a at 300, b at 400, c at 500.
        let c = p.claim_due("default", 300, &a, t0 + Duration::from_secs(300)).unwrap();
        assert!(!c.newly_on);
        assert_eq!(ids(&c), ["a"]);
        assert_eq!(c.next, Some(t0 + Duration::from_secs(400)));
        assert_eq!(p.claim_due("default", 300, &a, t0 + Duration::from_secs(350)), None);
        assert_eq!(ids(&p.claim_due("default", 300, &a, t0 + Duration::from_secs(400)).unwrap()), ["b"]);
        assert_eq!(ids(&p.claim_due("default", 300, &a, t0 + Duration::from_secs(500)).unwrap()), ["c"]);
        // Each keeps its phase: a is back at 600, not at 300 after its late tick.
        let c = p.claim_due("default", 300, &a, t0 + Duration::from_secs(603)).unwrap();
        assert_eq!(ids(&c), ["a"]);
        assert_eq!(c.next, Some(t0 + Duration::from_secs(700)));
    }

    #[test]
    fn a_new_account_is_due_at_once_and_a_removed_one_is_forgotten() {
        let p = prober();
        let t0 = Instant::now();
        p.claim_due("default", 300, &accts(&["a", "b"]), t0).unwrap();
        // A login lands "c" between ticks: it is probed now, nobody else is.
        let c = p.claim_due("default", 300, &accts(&["a", "b", "c"]), t0 + Duration::from_secs(20)).unwrap();
        assert!(!c.newly_on);
        assert_eq!(ids(&c), ["c"]);
        // and it then keeps the phase of its arrival, behind a's turn at 300.
        assert_eq!(p.claim_due("default", 300, &accts(&["a", "b", "c"]), t0 + Duration::from_secs(299)), None);
        assert_eq!(ids(&p.claim_due("default", 300, &accts(&["a", "b", "c"]), t0 + Duration::from_secs(300)).unwrap()), ["a"]);
        assert_eq!(ids(&p.claim_due("default", 300, &accts(&["a", "b", "c"]), t0 + Duration::from_secs(320)).unwrap()), ["c"]);
        // Dropping "b" forgets its booking; adding it back treats it as new.
        assert_eq!(p.claim_due("default", 300, &accts(&["a", "c"]), t0 + Duration::from_secs(330)), None);
        assert_eq!(ids(&p.claim_due("default", 300, &accts(&["a", "b", "c"]), t0 + Duration::from_secs(340)).unwrap()), ["b"]);
    }

    #[test]
    fn claim_due_respects_interval_floor_and_off() {
        let p = prober();
        let t0 = Instant::now();
        let a = accts(&["a"]);
        assert!(p.claim_due("default", 5, &a, t0).unwrap().newly_on);
        // Below the floor the floor wins.
        assert_eq!(p.claim_due("default", 5, &a, t0 + Duration::from_secs(10)), None);
        assert_eq!(ids(&p.claim_due("default", 5, &a, t0 + Duration::from_secs(30)).unwrap()), ["a"]);
        // Switching off forgets the pool; switching back on logs again and
        // probes everyone.
        assert_eq!(p.claim_due("default", 0, &a, t0 + Duration::from_secs(40)), None);
        let c = p.claim_due("default", 300, &a, t0 + Duration::from_secs(41)).unwrap();
        assert!(c.newly_on);
        assert_eq!(ids(&c), ["a"]);
        // A pool with no OAuth accounts still logs that it is on, once.
        assert!(p.claim_due("empty", 300, &[], t0).unwrap().newly_on);
        assert_eq!(p.claim_due("empty", 300, &[], t0 + Duration::from_secs(1)), None);
    }

    #[test]
    fn back_off_holds_the_pool_for_twice_its_interval() {
        let p = prober();
        let t0 = Instant::now();
        let a = accts(&["a", "b"]);
        assert!(p.claim_due("default", 300, &a, t0).unwrap().newly_on);
        let until = p.back_off("default", 300, t0);
        assert_eq!(until, t0 + Duration::from_secs(600));
        // Due by the plain schedule, but still backing off.
        assert_eq!(p.claim_due("default", 300, &a, t0 + Duration::from_secs(301)), None);
        assert_eq!(p.claim_due("default", 300, &a, t0 + Duration::from_secs(599)), None);
        // When it lifts, both overdue accounts run in one (staggered) run: a's
        // booking of 300 has slipped a whole interval and restarts from now;
        // b's of 450 keeps its phase.
        let c = p.claim_due("default", 300, &a, t0 + Duration::from_secs(600)).unwrap();
        assert_eq!(ids(&c), ["a", "b"]);
        assert_eq!(c.next, Some(t0 + Duration::from_secs(750)));
        // The backoff is spent once a run happens: b at 750, a at 900.
        assert_eq!(ids(&p.claim_due("default", 300, &a, t0 + Duration::from_secs(800)).unwrap()), ["b"]);
        assert_eq!(ids(&p.claim_due("default", 300, &a, t0 + Duration::from_secs(900)).unwrap()), ["a"]);
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
