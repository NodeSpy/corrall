//! Per-request logging to `logDir`: one file per request, 0600, bodies capped,
//! credentials redacted, old files swept. Off unless configured.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;

use crate::config::LogLevel;
use crate::security::{redact, set_mode};

#[derive(Clone)]
pub struct RequestLogger {
    dir: PathBuf,
    level: LogLevel,
    max_body: u64,
}

impl RequestLogger {
    pub fn new(dir: &str, level: LogLevel, max_body: u64) -> Option<RequestLogger> {
        if level == LogLevel::Off {
            return None;
        }
        let dir = PathBuf::from(dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!("request logging disabled: cannot create {}: {e}", dir.display());
            return None;
        }
        set_mode(&dir, 0o700);
        Some(RequestLogger { dir, level, max_body })
    }

    fn path(&self, req_id: &str) -> PathBuf {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        self.dir.join(format!("corrall-{ts}-{req_id}.log"))
    }

    fn cap(&self, body: &[u8]) -> Vec<u8> {
        if self.max_body == 0 || (body.len() as u64) <= self.max_body {
            return body.to_vec();
        }
        let keep = self.max_body as usize / 2;
        let mut v = body[..keep].to_vec();
        v.extend_from_slice(format!("\n... truncated {} bytes ...\n", body.len() - 2 * keep).as_bytes());
        v.extend_from_slice(&body[body.len() - keep..]);
        v
    }

    /// Write one request/response pair. Called off the hot path.
    #[allow(clippy::too_many_arguments)]
    pub fn write(
        &self,
        req_id: &str,
        account: &str,
        method: &str,
        url: &str,
        req_headers: &[(String, String)],
        req_body: &Bytes,
        status: u16,
        res_headers: &[(String, String)],
        res_body: Option<&Bytes>,
    ) {
        let mut out = String::new();
        out.push_str(&format!("=== REQUEST (account: {account}) ===\n{method} {url}\n"));
        for (k, v) in req_headers {
            let lk = k.to_ascii_lowercase();
            let shown = if lk == "authorization" || lk == "x-api-key" || lk == "proxy-authorization" || lk == "cookie" {
                redact(v)
            } else {
                crate::security::safe_text(v, 2000)
            };
            out.push_str(&format!("{k}: {shown}\n"));
        }
        let mut bytes = out.into_bytes();
        if self.level == LogLevel::Body && !req_body.is_empty() {
            bytes.extend_from_slice(b"\n--- REQUEST BODY ---\n");
            bytes.extend_from_slice(&self.cap(req_body));
        }
        bytes.extend_from_slice(format!("\n\n=== RESPONSE {status} ===\n").as_bytes());
        for (k, v) in res_headers {
            bytes.extend_from_slice(format!("{k}: {}\n", crate::security::safe_text(v, 2000)).as_bytes());
        }
        if self.level == LogLevel::Body {
            if let Some(b) = res_body {
                bytes.extend_from_slice(b"\n--- RESPONSE BODY ---\n");
                bytes.extend_from_slice(&self.cap(b));
            }
        }
        bytes.push(b'\n');
        let path = self.path(req_id);
        if let Err(e) = crate::security::write_private_atomic(&path, &bytes) {
            tracing::warn!("request log write failed: {e}");
        }
    }

    /// Delete our own log files older than `retention_hours`.
    pub fn sweep(&self, retention_hours: u64) {
        if retention_hours == 0 {
            return;
        }
        sweep_dir(&self.dir, Duration::from_secs(retention_hours * 3600));
    }
}

pub fn sweep_dir(dir: &Path, max_age: Duration) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !(name.starts_with("corrall-") && name.ends_with(".log")) {
            continue;
        }
        if let Ok(md) = e.metadata() {
            if let Ok(m) = md.modified() {
                if now.duration_since(m).unwrap_or_default() > max_age {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}
