//! Opt-in background quota probe: reads each OAuth account's zero-spend usage
//! endpoint so idle accounts' quota stays fresh. Per-pool: off for a pool
//! unless its `quotaProbeSeconds` > 0.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::manager::Manager;
use crate::oauth::{fetch_profile, fetch_usage, UsageResult};
use crate::pools::Pools;

/// How often the loop wakes to see which pools are due.
const TICK: Duration = Duration::from_secs(5);
/// Floor on a pool's probe interval, whatever its config says.
const MIN_INTERVAL: u64 = 30;

#[derive(Clone)]
pub struct Prober {
    pools: Arc<Pools>,
    /// Per pool: when it was last probed, and whether it was on last tick.
    seen: Arc<Mutex<BTreeMap<String, Seen>>>,
    running: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Copy)]
struct Seen {
    last: Option<Instant>,
    on: bool,
}

impl Prober {
    pub fn new(pools: Arc<Pools>) -> Prober {
        Prober { pools, seen: Arc::new(Mutex::new(BTreeMap::new())), running: Arc::new(tokio::sync::Mutex::new(())) }
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
                let Some(newly_on) = self.claim_due(&name, secs) else { continue };
                if newly_on {
                    m.log(format!("Quota probe enabled for pool \"{name}\" (every {secs}s)"));
                }
                self.probe_pool(&m).await;
            }
            tokio::time::sleep(TICK).await;
        }
    }

    /// Mark `pool` as probed now if its interval has elapsed. `None` means not
    /// due (or switched off); `Some(true)` means this is the pool's first probe
    /// since it was turned on, which is worth a log line.
    ///
    /// Sync on purpose: it keeps the lock guard out of `run`'s future, which
    /// otherwise would not be `Send`.
    fn claim_due(&self, pool: &str, secs: u64) -> Option<bool> {
        let mut seen = self.seen.lock();
        let prev = seen.get(pool).copied().unwrap_or(Seen { last: None, on: false });
        if secs == 0 {
            seen.insert(pool.to_string(), Seen { last: None, on: false });
            return None;
        }
        if prev.last.is_some_and(|t| t.elapsed() < Duration::from_secs(secs.max(MIN_INTERVAL))) {
            return None;
        }
        seen.insert(pool.to_string(), Seen { last: Some(Instant::now()), on: true });
        Some(!prev.on)
    }

    /// Probe every pool now, regardless of interval (the TUI's `p` key).
    pub async fn probe_all(&self) {
        let Ok(_g) = self.running.try_lock() else { return };
        for (_, m) in self.pools.each() {
            self.probe_pool(&m).await;
        }
    }

    async fn probe_pool(&self, m: &Manager) {
        let tasks: Vec<_> = m.oauth_accounts().into_iter().map(|(id, name)| self.probe_one(m, id, name)).collect();
        futures_util::future::join_all(tasks).await;
    }

    async fn probe_one(&self, m: &Manager, id: String, name: String) {
        let Some(cred) = m.ensure_token_fresh(&id, false).await else { return };
        let mut result = tokio::time::timeout(Duration::from_secs(15), fetch_usage(&cred)).await.unwrap_or(UsageResult::Error("probe timed out".into()));
        if matches!(result, UsageResult::Unauthorized) {
            if let Some(c2) = m.ensure_token_fresh(&id, true).await {
                result = tokio::time::timeout(Duration::from_secs(15), fetch_usage(&c2)).await.unwrap_or(UsageResult::Error("probe timed out".into()));
            }
        }
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
            }
            UsageResult::Unauthorized => m.log(format!("Quota probe: \"{name}\" token rejected")),
            UsageResult::Error(e) => tracing::warn!("quota probe for \"{name}\": {e}"),
        }
    }
}
