//! Opt-in keep-warm: start idle accounts' 5-hour windows ahead of time by
//! sending one minimal `claude -p` through this proxy, pinned to the account.
//! Spends a little quota; off unless `warmupSeconds` > 0 (minimum 60).

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use crate::manager::Manager;

#[derive(Clone)]
pub struct Warmer {
    manager: Manager,
    port: u16,
    api_key: Arc<Mutex<String>>,
    interval: Arc<Mutex<u64>>,
    running: Arc<tokio::sync::Mutex<()>>,
    model: String,
}

impl Warmer {
    pub fn new(manager: Manager, port: u16, api_key: &str, interval_secs: u64) -> Warmer {
        Warmer {
            manager,
            port,
            api_key: Arc::new(Mutex::new(api_key.to_string())),
            interval: Arc::new(Mutex::new(interval_secs)),
            running: Arc::new(tokio::sync::Mutex::new(())),
            model: "haiku".into(),
        }
    }

    pub fn set_interval(&self, secs: u64) {
        *self.interval.lock() = secs;
    }
    pub fn set_api_key(&self, key: &str) {
        *self.api_key.lock() = key.to_string();
    }
    pub fn interval(&self) -> u64 {
        *self.interval.lock()
    }

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
                self.manager.log(format!("Keep-warm enabled (every {secs}s); this spends a little quota"));
                was_on = true;
                self.warm_all().await;
            }
            tokio::time::sleep(Duration::from_secs(secs.max(60))).await;
            if self.interval() > 0 {
                self.warm_all().await;
            }
        }
    }

    pub async fn warm_all(&self) {
        let Ok(_g) = self.running.try_lock() else { return };
        for (id, name) in self.manager.warm_candidates() {
            self.warm_one(&id, &name).await;
        }
    }

    async fn warm_one(&self, id: &str, name: &str) {
        let key = self.api_key.lock().clone();
        let base = format!("http://127.0.0.1:{}/tc-acct/{}", self.port, id);
        let mut cmd = tokio::process::Command::new("claude");
        cmd.args(["-p", "--bare", "--model", &self.model, "--output-format", "text", "hi"])
            .env_remove("TC_ACCT")
            .env_remove("HTTPS_PROXY")
            .env_remove("HTTP_PROXY")
            .env_remove("https_proxy")
            .env_remove("http_proxy")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env("ANTHROPIC_BASE_URL", base)
            .env("ANTHROPIC_API_KEY", key)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(120), cmd.status()).await {
            Ok(Ok(st)) if st.success() => self.manager.log(format!("Keep-warm: warmed \"{name}\"")),
            Ok(Ok(st)) => self.manager.log(format!("Keep-warm: claude exited {} for \"{name}\"", st.code().unwrap_or(-1))),
            Ok(Err(e)) => self.manager.log(format!("Keep-warm: cannot run claude: {e}")),
            Err(_) => self.manager.log(format!("Keep-warm: timed out warming \"{name}\"")),
        }
    }
}
