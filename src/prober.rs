//! Opt-in background quota probe: reads each OAuth account's zero-spend usage
//! endpoint so idle accounts' quota stays fresh. Off unless
//! `quotaProbeSeconds` > 0.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::manager::Manager;
use crate::oauth::{fetch_profile, fetch_usage, UsageResult};

#[derive(Clone)]
pub struct Prober {
    manager: Manager,
    interval: Arc<Mutex<u64>>,
    running: Arc<tokio::sync::Mutex<()>>,
}

impl Prober {
    pub fn new(manager: Manager, interval_secs: u64) -> Prober {
        Prober { manager, interval: Arc::new(Mutex::new(interval_secs)), running: Arc::new(tokio::sync::Mutex::new(())) }
    }

    pub fn set_interval(&self, secs: u64) {
        *self.interval.lock() = secs;
    }

    pub fn interval(&self) -> u64 {
        *self.interval.lock()
    }

    /// Run forever; picks up interval changes each tick.
    pub async fn run(self) {
        let mut was_on = false;
        loop {
            let secs = self.interval();
            if secs == 0 {
                was_on = false;
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            if !was_on {
                self.manager.log(format!("Quota probe enabled (every {secs}s)"));
                was_on = true;
                self.probe_all().await;
            }
            tokio::time::sleep(Duration::from_secs(secs.max(30))).await;
            if self.interval() > 0 {
                self.probe_all().await;
            }
        }
    }

    pub async fn probe_all(&self) {
        let Ok(_g) = self.running.try_lock() else { return };
        let now = crate::quota::now_ms();
        let secs = self.interval();
        self.manager.probe_run_started(now, if secs > 0 { Some(now + secs as i64 * 1000) } else { None });
        let accounts = self.manager.oauth_accounts();
        let tasks: Vec<_> = accounts.into_iter().map(|(id, name)| self.probe_one(id, name)).collect();
        futures_util::future::join_all(tasks).await;
        self.manager.probe_run_finished(crate::quota::now_ms());
    }

    async fn probe_one(&self, id: String, name: String) {
        let started = crate::quota::now_ms();
        self.manager.probe_account_status(&id, "running", started, None, None);
        let Some(cred) = self.manager.ensure_token_fresh(&id, false).await else {
            self.manager.probe_account_status(&id, "error", started, Some(crate::quota::now_ms()), Some("no usable token".into()));
            return;
        };
        let mut result = tokio::time::timeout(Duration::from_secs(15), fetch_usage(&cred)).await.unwrap_or(UsageResult::Error("probe timed out".into()));
        if matches!(result, UsageResult::Unauthorized) {
            if let Some(c2) = self.manager.ensure_token_fresh(&id, true).await {
                result = tokio::time::timeout(Duration::from_secs(15), fetch_usage(&c2)).await.unwrap_or(UsageResult::Error("probe timed out".into()));
            }
        }
        let finished = crate::quota::now_ms();
        match result {
            UsageResult::Ok(u) => {
                self.manager.apply_usage(&id, &u);
                if self.manager.needs_profile(&id) {
                    if let Some(c) = self.manager.credential_of(&id) {
                        if let Ok(p) = fetch_profile(&c).await {
                            self.manager.apply_profile(&id, &p);
                        }
                    }
                }
                self.manager.probe_account_status(&id, "ok", started, Some(finished), None);
            }
            UsageResult::Unauthorized => {
                self.manager.log(format!("Quota probe: \"{name}\" token rejected"));
                self.manager.probe_account_status(&id, "error", started, Some(finished), Some("token rejected".into()));
            }
            UsageResult::Error(e) => {
                tracing::warn!("quota probe for \"{name}\": {e}");
                let status = if e.contains("timed out") { "timeout" } else { "error" };
                self.manager.probe_account_status(&id, status, started, Some(finished), Some(crate::security::safe_text(&e, 120)));
            }
        }
    }
}
