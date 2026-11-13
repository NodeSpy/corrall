//! Names for the sessions the proxy serves, read from Claude Code's own files:
//!
//! - `~/.claude/sessions/<pid>.json`, one per running Claude Code process,
//!   holds the name Claude Code itself shows for the session and the
//!   directory it runs in;
//! - `~/.claude/projects/<slug>/<id>/custom-title.json` (a `/rename`), else the
//!   last `custom-title` / `ai-title` record in the transcript tail, plus the
//!   transcript's `cwd` for a session whose process has gone.
//!
//! Only UUID-shaped ids ever touch the filesystem, and in the sessions
//! directory only `.json` files are opened: the key files beside them are
//! never read. Reads are cached and run off the render path.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::security::safe_text;

/// How long what a transcript said about a session is believed.
const TTL_MS: i64 = 30_000;
/// How long the list of running sessions is believed. Shorter: it is one
/// small directory, and a session that just started should get its name soon.
const LIVE_TTL_MS: i64 = 5_000;
const TAIL_BYTES: u64 = 64 * 1024;
/// Where to look for `cwd` when the tail is one long record without it.
const HEAD_BYTES: u64 = 16 * 1024;
/// A session file larger than this is not one of Claude Code's.
const LIVE_FILE_MAX: u64 = 64 * 1024;
/// Entries read from the sessions directory, and names remembered for
/// sessions whose process has since exited.
const LIVE_MAX: usize = 4096;

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36 && b.iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

fn default_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude/projects")
}

/// Claude Code keeps `sessions/` beside `projects/`.
fn sessions_dir(projects: &Path) -> PathBuf {
    projects.parent().unwrap_or(Path::new(".")).join("sessions")
}

/// What Claude Code's files say about one session. All `None` when names are
/// off, the id is not a UUID, or nothing has been read yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    /// A `/rename`d title, else the generated one.
    pub title: Option<String>,
    /// Whether `title` is one the user chose.
    pub custom: bool,
    /// The name Claude Code gives the session while it runs.
    pub name: Option<String>,
    /// The directory the session runs in.
    pub cwd: Option<String>,
}

impl Info {
    /// The session in one short string: a title the user chose, else Claude
    /// Code's name for it, else a generated title, else its directory.
    pub fn display(&self) -> Option<String> {
        let chosen = self.title.as_ref().filter(|_| self.custom);
        chosen.or(self.name.as_ref()).or(self.title.as_ref()).cloned().or_else(|| {
            let dir = self.cwd.as_deref()?.trim_end_matches(['/', '\\']);
            dir.rsplit(['/', '\\']).next().filter(|b| !b.is_empty()).map(str::to_string)
        })
    }

    /// The working directory, with the home directory written `~`.
    pub fn project(&self) -> Option<String> {
        let cwd = self.cwd.as_deref()?;
        let home = dirs::home_dir();
        let rest = home.as_deref().and_then(Path::to_str).filter(|h| h.len() > 1).and_then(|h| cwd.strip_prefix(h));
        Some(match rest {
            Some(r) if r.is_empty() || r.starts_with('/') => format!("~{r}"),
            _ => cwd.to_string(),
        })
    }
}

/// What a session's transcript and title file said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Found {
    title: Option<String>,
    custom: bool,
    cwd: Option<String>,
    /// The project directory holding the transcript, tried first next time.
    project: Option<PathBuf>,
}

/// A running session, from the sessions directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Live {
    name: Option<String>,
    cwd: Option<String>,
}

#[derive(Clone)]
pub struct Titles {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    enabled: bool,
    width: usize,
    dir: PathBuf,
    cache: HashMap<String, (Found, i64)>,
    pending: HashSet<String>,
    live: HashMap<String, Live>,
    live_at: i64,
    live_pending: bool,
}

impl Titles {
    pub fn new(cfg: &crate::config::SessionTitles) -> Titles {
        let dir = cfg.projects_dir.as_deref().map(crate::oauth::expand_home).unwrap_or_else(default_dir);
        Titles {
            inner: Arc::new(Mutex::new(Inner {
                enabled: cfg.enabled,
                width: cfg.width.max(6),
                dir,
                cache: HashMap::new(),
                pending: HashSet::new(),
                live: HashMap::new(),
                live_at: 0,
                live_pending: false,
            })),
        }
    }

    pub fn configure(&self, cfg: &crate::config::SessionTitles) {
        let mut g = self.inner.lock();
        g.enabled = cfg.enabled;
        g.width = cfg.width.max(6);
        let dir = cfg.projects_dir.as_deref().map(crate::oauth::expand_home).unwrap_or_else(default_dir);
        if dir != g.dir {
            g.cache.clear();
            g.live.clear();
            g.live_at = 0;
        }
        g.dir = dir;
    }

    pub fn enabled(&self) -> bool {
        self.inner.lock().enabled
    }

    pub fn width(&self) -> usize {
        self.inner.lock().width
    }

    /// Label for a session: what [`Info::display`] gives when anything is
    /// known, else the first six characters of its id. Never blocks.
    pub fn label(&self, session: Option<&str>, now: i64) -> String {
        let Some(sid) = session else { return String::new() };
        self.info(session, now).display().unwrap_or_else(|| sid.chars().take(6).collect())
    }

    /// What is known about a session right now. Never blocks: a miss or a
    /// stale entry schedules a read and answers with what is cached.
    pub fn info(&self, session: Option<&str>, now: i64) -> Info {
        let Some(sid) = session.filter(|s| is_uuid(s)) else { return Info::default() };
        let (found, live, read_transcript, read_live, dir) = {
            let mut g = self.inner.lock();
            if !g.enabled {
                return Info::default();
            }
            let hit = g.cache.get(sid).cloned();
            let stale = !hit.as_ref().is_some_and(|(_, at)| now - at < TTL_MS);
            let read_transcript = stale && g.pending.insert(sid.to_string());
            let read_live = !g.live_pending && now - g.live_at >= LIVE_TTL_MS;
            g.live_pending |= read_live;
            (hit.map(|(f, _)| f).unwrap_or_default(), g.live.get(sid).cloned(), read_transcript, read_live, g.dir.clone())
        };
        if read_transcript || read_live {
            self.refresh(sid, read_transcript.then(|| found.project.clone()), read_live, dir);
        }
        let cwd = live.as_ref().and_then(|l| l.cwd.clone()).or(found.cwd);
        Info { title: found.title, custom: found.custom, name: live.and_then(|l| l.name), cwd }
    }

    /// Read the files off the calling thread and store what they said.
    /// `transcript` is `Some(hint)` when this session's transcript is due.
    fn refresh(&self, sid: &str, transcript: Option<Option<PathBuf>>, live: bool, dir: PathBuf) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            // No runtime to read on: leave the entries due for the next ask.
            let mut g = self.inner.lock();
            if transcript.is_some() {
                g.pending.remove(sid);
            }
            if live {
                g.live_pending = false;
            }
            return;
        };
        let me = self.clone();
        let sid = sid.to_string();
        rt.spawn_blocking(move || {
            let found = transcript.as_ref().map(|hint| lookup(&dir, &sid, hint.as_deref()));
            let running = live.then(|| read_live(&sessions_dir(&dir)));
            let now = crate::quota::now_ms();
            let mut g = me.inner.lock();
            if let Some(mut found) = found {
                // A session does not change directory; a tail that no longer
                // shows it is not news.
                if found.cwd.is_none() {
                    found.cwd = g.cache.get(&sid).and_then(|(old, _)| old.cwd.clone());
                }
                g.cache.insert(sid.clone(), (found, now));
                g.pending.remove(&sid);
                if g.cache.len() > 5000 {
                    let cutoff = now - crate::session::SESSION_KNOWN_TTL_MS;
                    g.cache.retain(|_, (_, at)| *at > cutoff);
                }
            }
            if let Some(running) = running {
                // Kept after the process exits, so a session that just ended
                // is still named while it is listed.
                if g.live.len() + running.len() > LIVE_MAX {
                    g.live.clear();
                }
                g.live.extend(running);
                g.live_at = now;
                g.live_pending = false;
            }
        });
    }
}

fn clean(v: Option<&Value>, max: usize) -> Option<String> {
    let s = v?.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    Some(safe_text(s, max))
}

/// Whether `project` is the directory this session's files are in.
fn holds(project: &Path, sid: &str) -> bool {
    project.join(format!("{sid}.jsonl")).is_file() || project.join(sid).join("custom-title.json").is_file()
}

fn lookup(dir: &Path, sid: &str, hint: Option<&Path>) -> Found {
    let project =
        hint.filter(|p| holds(p, sid)).map(Path::to_path_buf).or_else(|| std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| holds(p, sid)));
    let Some(project) = project else { return Found::default() };
    let mut found = read_transcript(&project.join(format!("{sid}.jsonl")));
    let renamed = std::fs::read(project.join(sid).join("custom-title.json")).ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
    if let Some(t) = renamed.and_then(|v| clean(v.get("customTitle").or_else(|| v.get("title")), 80)) {
        found.title = Some(t);
        found.custom = true;
    }
    found.project = Some(project);
    found
}

/// The newest title and working directory a transcript records. Reads its
/// tail, and its head too when the tail does not say where the session runs.
fn read_transcript(path: &Path) -> Found {
    use std::io::{Read, Seek, SeekFrom};
    let mut found = Found::default();
    let Ok(mut f) = std::fs::File::open(path) else { return found };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut read = |from: u64, limit: u64| {
        let mut bytes = Vec::new();
        if f.seek(SeekFrom::Start(from)).is_ok() {
            let _ = (&mut f).take(limit).read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let tail = read(len.saturating_sub(TAIL_BYTES), TAIL_BYTES);
    for line in tail.lines().rev() {
        if !line.starts_with('{') {
            continue;
        }
        if line.contains("-title\"") && !found.custom {
            let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
            match rec.get("type").and_then(Value::as_str) {
                Some("custom-title") => {
                    if let Some(t) = clean(rec.get("customTitle"), 80) {
                        found.title = Some(t);
                        found.custom = true;
                    }
                }
                Some("ai-title") if found.title.is_none() => found.title = clean(rec.get("aiTitle"), 80),
                _ => {}
            }
        } else if found.cwd.is_none() {
            found.cwd = cwd_of(line);
        }
        if found.custom && found.cwd.is_some() {
            break;
        }
    }
    if found.cwd.is_none() && len > TAIL_BYTES {
        found.cwd = read(0, HEAD_BYTES).lines().filter(|l| l.starts_with('{')).find_map(cwd_of);
    }
    found
}

/// The top-level `cwd` of one transcript record.
fn cwd_of(line: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Rec {
        cwd: Option<String>,
    }
    if !line.contains("\"cwd\":") {
        return None;
    }
    let cwd = serde_json::from_str::<Rec>(line).ok()?.cwd?;
    clean(Some(&Value::String(cwd)), 400)
}

/// Every running session Claude Code lists in `dir`, by session id.
fn read_live(dir: &Path) -> HashMap<String, Live> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten().take(LIVE_MAX) {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        // The entry's own metadata: a symlink is not followed.
        if !e.metadata().is_ok_and(|m| m.is_file() && m.len() <= LIVE_FILE_MAX) {
            continue;
        }
        let Some(v) = std::fs::read(&path).ok().and_then(|raw| serde_json::from_slice::<Value>(&raw).ok()) else { continue };
        let Some(sid) = v.get("sessionId").and_then(Value::as_str).filter(|s| is_uuid(s)) else { continue };
        out.insert(sid.to_string(), Live { name: clean(v.get("name"), 80), cwd: clean(v.get("cwd"), 400) });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn uuid_check() {
        assert!(is_uuid(SID));
        assert!(!is_uuid("../../etc/passwd"));
        assert!(!is_uuid("11111111-2222-3333-4444-55555555555g"));
    }

    #[test]
    fn reads_sidecar_and_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(proj.join(SID)).unwrap();
        std::fs::write(
            proj.join(format!("{SID}.jsonl")),
            "{\"type\":\"user\",\"cwd\":\"/work/old\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Generated one\"}\n{\"type\":\"assistant\",\"message\":{\"cwd\":\"/not/this\"},\"cwd\":\"/work/app\"}\n{\"type\":\"x\"}\n",
        )
        .unwrap();
        let found = lookup(dir.path(), SID, None);
        assert_eq!(found.title.as_deref(), Some("Generated one"));
        assert!(!found.custom);
        assert_eq!(found.cwd.as_deref(), Some("/work/app"), "the newest record's own cwd, not a nested one");
        assert_eq!(found.project.as_deref(), Some(proj.as_path()));

        std::fs::write(proj.join(SID).join("custom-title.json"), r#"{"customTitle":"my name"}"#).unwrap();
        let found = lookup(dir.path(), SID, Some(&proj));
        assert_eq!((found.title.as_deref(), found.custom), (Some("my name"), true));

        // A hint that no longer holds the session falls back to the scan.
        assert_eq!(lookup(dir.path(), SID, Some(&dir.path().join("gone"))).title.as_deref(), Some("my name"));
        assert_eq!(lookup(dir.path(), "99999999-2222-3333-4444-555555555555", None), Found::default());
    }

    #[test]
    fn a_rename_in_the_transcript_beats_a_generated_title() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"custom-title\",\"customTitle\":\"renamed\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"generated later\"}\n{\"type\":\"user\",\"cwd\":\"/w\"}\n",
        )
        .unwrap();
        let found = read_transcript(&path);
        assert_eq!((found.title.as_deref(), found.custom, found.cwd.as_deref()), (Some("renamed"), true, Some("/w")));
    }

    #[test]
    fn cwd_comes_from_the_head_when_the_tail_is_one_long_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let blob = "x".repeat(TAIL_BYTES as usize + 100);
        std::fs::write(&path, format!("{{\"type\":\"user\",\"cwd\":\"/work/app\"}}\n{{\"type\":\"blob\",\"data\":\"{blob}\"}}\n")).unwrap();
        assert_eq!(read_transcript(&path).cwd.as_deref(), Some("/work/app"));
    }

    #[test]
    fn running_sessions_are_read_from_json_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("100.json"), format!(r#"{{"pid":100,"sessionId":"{SID}","cwd":"/work/app","name":"app-e0"}}"#)).unwrap();
        // Never opened: a key beside the session file, and anything oversized
        // or not shaped like a session.
        std::fs::write(dir.path().join("100.abc.key"), format!(r#"{{"sessionId":"{SID}","name":"from the key file"}}"#)).unwrap();
        std::fs::write(dir.path().join("101.json"), r#"{"sessionId":"../../etc/passwd","name":"x"}"#).unwrap();
        std::fs::write(dir.path().join("102.json"), "not json").unwrap();
        let live = read_live(dir.path());
        assert_eq!(live.len(), 1);
        assert_eq!(live[SID], Live { name: Some("app-e0".into()), cwd: Some("/work/app".into()) });
        assert!(read_live(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn display_prefers_a_chosen_title_then_the_running_name() {
        let info = |title: Option<&str>, custom, name: Option<&str>, cwd: Option<&str>| Info {
            title: title.map(str::to_string),
            custom,
            name: name.map(str::to_string),
            cwd: cwd.map(str::to_string),
        };
        assert_eq!(info(Some("mine"), true, Some("app-e0"), Some("/w/app")).display().as_deref(), Some("mine"));
        assert_eq!(info(Some("generated"), false, Some("app-e0"), Some("/w/app")).display().as_deref(), Some("app-e0"));
        assert_eq!(info(Some("generated"), false, None, Some("/w/app")).display().as_deref(), Some("generated"));
        assert_eq!(info(None, false, None, Some("/w/app/")).display().as_deref(), Some("app"));
        assert_eq!(info(None, false, None, None).display(), None);
        assert_eq!(info(None, false, None, Some("/elsewhere/app")).project().as_deref(), Some("/elsewhere/app"));
        if let Some(home) = dirs::home_dir().as_deref().and_then(Path::to_str).filter(|h| h.len() > 1) {
            assert_eq!(info(None, false, None, Some(&format!("{home}/src/app"))).project().as_deref(), Some("~/src/app"));
            assert_eq!(info(None, false, None, Some(&format!("{home}-other/app"))).project(), Some(format!("{home}-other/app")));
        }
    }

    #[tokio::test]
    async fn info_fills_in_from_both_sources_and_respects_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("projects").join("-work-app");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        std::fs::write(proj.join(format!("{SID}.jsonl")), "{\"type\":\"ai-title\",\"aiTitle\":\"Fix the thing\"}\n{\"type\":\"user\",\"cwd\":\"/work/app\"}\n")
            .unwrap();
        std::fs::write(dir.path().join("sessions/7.json"), format!(r#"{{"sessionId":"{SID}","cwd":"/work/app","name":"app-e0"}}"#)).unwrap();
        let cfg = |enabled| crate::config::SessionTitles { enabled, width: 18, projects_dir: Some(dir.path().join("projects").display().to_string()) };

        let off = Titles::new(&cfg(false));
        assert_eq!(off.info(Some(SID), 1), Info::default());
        assert_eq!(off.label(Some(SID), 1), "111111");

        let on = Titles::new(&cfg(true));
        let want = Info { title: Some("Fix the thing".into()), custom: false, name: Some("app-e0".into()), cwd: Some("/work/app".into()) };
        let mut got = Info::default();
        for _ in 0..200 {
            got = on.info(Some(SID), crate::quota::now_ms());
            if got == want {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(got, want);
        assert_eq!(on.label(Some(SID), crate::quota::now_ms()), "app-e0");
        assert_eq!(on.info(Some("not-a-uuid"), 1), Info::default());
        assert_eq!(on.label(Some("not-a-uuid"), 1), "not-a-");
    }
}
