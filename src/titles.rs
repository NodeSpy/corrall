//! Session titles for the activity log, read from Claude Code's own files
//! under `~/.claude/projects/<slug>/`: `<id>/custom-title.json` (a `/rename`),
//! else the last `custom-title` / `ai-title` record in the transcript tail.
//! Only UUID-shaped ids ever touch the filesystem. Reads are cached and run
//! off the render path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::security::safe_text;

const TTL_MS: i64 = 30_000;
const TAIL_BYTES: u64 = 64 * 1024;

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36 && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

fn default_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude/projects")
}

#[derive(Clone)]
pub struct Titles {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    enabled: bool,
    width: usize,
    dir: PathBuf,
    cache: HashMap<String, (Option<String>, i64)>,
    pending: std::collections::HashSet<String>,
}

impl Titles {
    pub fn new(cfg: &crate::config::SessionTitles) -> Titles {
        let dir = cfg.projects_dir.as_deref().map(crate::oauth::expand_home).unwrap_or_else(default_dir);
        Titles { inner: Arc::new(Mutex::new(Inner { enabled: cfg.enabled, width: cfg.width.max(6), dir, cache: HashMap::new(), pending: Default::default() })) }
    }

    pub fn configure(&self, cfg: &crate::config::SessionTitles) {
        let mut g = self.inner.lock();
        g.enabled = cfg.enabled;
        g.width = cfg.width.max(6);
        let dir = cfg.projects_dir.as_deref().map(crate::oauth::expand_home).unwrap_or_else(default_dir);
        if dir != g.dir {
            g.cache.clear();
        }
        g.dir = dir;
    }

    pub fn enabled(&self) -> bool {
        self.inner.lock().enabled
    }

    pub fn width(&self) -> usize {
        self.inner.lock().width
    }

    /// Label for a session: its title when known, else the first six hex
    /// characters. Never blocks; schedules a read on a miss.
    pub fn label(&self, session: Option<&str>, now: i64) -> String {
        let Some(sid) = session else { return String::new() };
        let short: String = sid.chars().take(6).collect();
        let (enabled, hit, dir) = {
            let g = self.inner.lock();
            (g.enabled, g.cache.get(sid).cloned(), g.dir.clone())
        };
        if !enabled || !is_uuid(sid) {
            return short;
        }
        let stale = hit.as_ref().map(|(_, at)| now - at >= TTL_MS).unwrap_or(true);
        if stale {
            let schedule = {
                let mut g = self.inner.lock();
                g.pending.insert(sid.to_string())
            };
            if schedule {
                let me = self.clone();
                let sid = sid.to_string();
                tokio::task::spawn_blocking(move || {
                    let title = lookup(&dir, &sid);
                    let mut g = me.inner.lock();
                    g.cache.insert(sid.clone(), (title, crate::quota::now_ms()));
                    g.pending.remove(&sid);
                    if g.cache.len() > 5000 {
                        let cutoff = crate::quota::now_ms() - crate::session::SESSION_KNOWN_TTL_MS;
                        g.cache.retain(|_, (_, at)| *at > cutoff);
                    }
                });
            }
        }
        match hit {
            Some((Some(t), _)) => t,
            _ => short,
        }
    }
}

fn clean(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(safe_text(s, 80))
}

fn lookup(dir: &std::path::Path, sid: &str) -> Option<String> {
    let projects = std::fs::read_dir(dir).ok()?;
    let mut generated = None;
    for p in projects.flatten() {
        let project = p.path();
        if !project.is_dir() {
            continue;
        }
        let sidecar = project.join(sid).join("custom-title.json");
        if let Ok(raw) = std::fs::read(&sidecar) {
            if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                if let Some(t) = clean(v.get("customTitle").or_else(|| v.get("title"))) {
                    return Some(t);
                }
            }
        }
        let transcript = project.join(format!("{sid}.jsonl"));
        let Ok(md) = std::fs::metadata(&transcript) else { continue };
        let Ok(mut f) = std::fs::File::open(&transcript) else { continue };
        use std::io::{Read, Seek, SeekFrom};
        let start = md.len().saturating_sub(TAIL_BYTES);
        let _ = f.seek(SeekFrom::Start(start));
        let mut bytes = Vec::new();
        let _ = f.read_to_end(&mut bytes);
        let buf = String::from_utf8_lossy(&bytes);
        for line in buf.lines().rev() {
            if !line.starts_with('{') || !line.contains("-title\"") {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
            match rec.get("type").and_then(Value::as_str) {
                Some("custom-title") => {
                    if let Some(t) = clean(rec.get("customTitle")) {
                        return Some(t);
                    }
                }
                Some("ai-title") if generated.is_none() => generated = clean(rec.get("aiTitle")),
                _ => {}
            }
        }
        if generated.is_some() {
            return generated;
        }
    }
    generated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_check() {
        assert!(is_uuid("11111111-2222-3333-4444-555555555555"));
        assert!(!is_uuid("../../etc/passwd"));
        assert!(!is_uuid("11111111-2222-3333-4444-55555555555g"));
    }

    #[test]
    fn reads_sidecar_and_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let sid = "11111111-2222-3333-4444-555555555555";
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join(sid)).unwrap();
        std::fs::write(proj.join(format!("{sid}.jsonl")), "{\"type\":\"ai-title\",\"aiTitle\":\"Generated one\"}\n{\"type\":\"x\"}\n").unwrap();
        assert_eq!(lookup(dir.path(), sid).as_deref(), Some("Generated one"));
        std::fs::write(proj.join(sid).join("custom-title.json"), r#"{"customTitle":"my name"}"#).unwrap();
        assert_eq!(lookup(dir.path(), sid).as_deref(), Some("my name"));
    }
}
