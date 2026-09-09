//! Choosing a pool from the launch context.
//!
//! A pool can be named explicitly (`--pool`, `TC_POOL`), but the point of
//! per-pool `match` rules is that a single shell wrapper —
//!
//! ```sh
//! eval "$(corrall env)"; exec claude "$@"
//! ```
//!
//! — lands in the right fleet on its own, based on where it was started.
//!
//! Selection happens here, at launch, and not in the daemon: the working
//! directory, the git remote and the environment are all present in the client
//! process and none of them survives into an HTTP request. What the daemon sees
//! is the `/pool/<name>` prefix this resolution produces.

use crate::config::{Config, PoolMatch};
use std::path::{Path, PathBuf};

/// An environment lookup: variable name → value, `None` when unset.
pub type Getenv = Box<dyn Fn(&str) -> Option<String>>;

/// A git-remote lookup: directory → `origin` URL, `None` when there is none.
pub type GitRemote = Box<dyn Fn(&Path) -> Option<String>>;

/// The launch context a resolution runs against.
///
/// The two lookups are boxed closures rather than direct calls so a test can
/// describe a context — a cwd, an environment, a git remote — without a
/// temporary directory or a real repository. [`LaunchEnv::live`] is the one
/// production caller.
pub struct LaunchEnv {
    pub cwd: PathBuf,
    pub getenv: Getenv,
    pub remote: GitRemote,
}

impl LaunchEnv {
    /// The real process: its working directory, its environment, and `git`.
    pub fn live() -> Self {
        Self::at(std::env::current_dir().unwrap_or_default())
    }

    /// Like [`LaunchEnv::live`], but matching against a given directory. Used
    /// by `corrall env --cwd DIR`, which lets a wrapper resolve the pool for
    /// a project it is about to `cd` into.
    pub fn at(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into(), getenv: Box::new(|k| std::env::var(k).ok()), remote: Box::new(git_remote) }
    }
}

/// A resolved pool and the rule that chose it.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub pool: String,
    /// False when nothing matched and the default pool was used.
    pub matched: bool,
    /// Short human-readable justification, for the line `env` writes to stderr.
    pub reason: String,
}

/// Pick the pool for a launch context.
///
/// Every non-default pool that carries `match` rules is tried in sorted name
/// order and the first match wins; with nothing matching the answer is the
/// default pool. Sorted order — rather than config order, which a `BTreeMap`
/// does not preserve anyway — makes the outcome reproducible when two pools
/// could both claim a directory.
///
/// The default pool is skipped even if it has rules: it is the fallback, so a
/// rule on it could only ever restate the default.
pub fn resolve(cfg: &Config, env: &LaunchEnv) -> Choice {
    // BTreeMap iteration is already sorted by name.
    for (name, pool) in &cfg.pools {
        if name == &cfg.default_pool {
            continue;
        }
        let Some(m) = &pool.match_rules else { continue };
        if let Some(reason) = match_reason(m, env) {
            return Choice { pool: name.clone(), matched: true, reason };
        }
    }
    Choice { pool: cfg.default_pool.clone(), matched: false, reason: "no rule matched".into() }
}

/// Why `m` matches this context, or `None` if it does not. Any one satisfied
/// path, remote or environment condition is a match.
///
/// Paths come first and the environment last because the middle test shells out
/// to `git`: a rule that can be settled from the cwd alone never pays for it.
fn match_reason(m: &PoolMatch, env: &LaunchEnv) -> Option<String> {
    if let Some(cwd) = clean_dir(&env.cwd.to_string_lossy()) {
        for p in &m.paths {
            let Some(dir) = clean_dir(p.trim_end_matches("/**").trim_end_matches("/*")) else { continue };
            if cwd == dir || cwd.starts_with(&dir) {
                return Some(format!("path {p}"));
            }
        }
    }
    if !m.remotes.is_empty() {
        // No remote (not a repo, or no `origin`) simply fails every remote
        // rule; it is not an error worth reporting to a launching wrapper.
        if let Some(url) = (env.remote)(&env.cwd).filter(|u| !u.is_empty()) {
            for r in &m.remotes {
                if regex::Regex::new(r).is_ok_and(|re| re.is_match(&url)) {
                    return Some(format!("remote ~ {r}"));
                }
            }
        }
    }
    for (k, pat) in &m.env {
        // An unset variable matches nothing, including the empty pattern:
        // "matches when set" has to mean set.
        let Some(v) = (env.getenv)(k).filter(|v| !v.is_empty()) else { continue };
        if pat.is_empty() {
            return Some(format!("env {k} set"));
        }
        if regex::Regex::new(pat).is_ok_and(|re| re.is_match(&v)) {
            return Some(format!("env {k} ~ {pat}"));
        }
    }
    None
}

/// `git remote get-url origin` in `dir`, or `None` when `dir` is not a
/// repository, has no `origin`, or `git` is not installed.
fn git_remote(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "remote", "get-url", "origin"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(url).filter(|u| !u.is_empty())
}

/// Normalize a configured directory or a cwd: expand a leading `~`, make it
/// absolute, and drop `.`/`..` components so the prefix test below compares
/// like with like. `None` for an empty string.
///
/// This is a lexical clean, deliberately: it does not resolve symlinks, so a
/// rule written for `~/Projects/foo` still fires in a directory the user
/// reached by that name.
fn clean_dir(p: &str) -> Option<PathBuf> {
    let p = p.trim();
    if p.is_empty() {
        return None;
    }
    let p = crate::oauth::expand_home(p);
    let p = if p.is_absolute() { p } else { std::env::current_dir().unwrap_or_default().join(p) };
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PoolConfig, DEFAULT_POOL};

    /// A config with the named pools, each carrying the given rules.
    fn cfg(pools: &[(&str, PoolMatch)]) -> Config {
        let mut c = Config::default();
        for (name, m) in pools {
            c.ensure_pool(name).unwrap();
            c.pool_mut(name).unwrap().match_rules = Some(m.clone());
        }
        c
    }

    fn env(cwd: &str) -> LaunchEnv {
        LaunchEnv { cwd: cwd.into(), getenv: Box::new(|_| None), remote: Box::new(|_| None) }
    }

    fn paths(p: &[&str]) -> PoolMatch {
        PoolMatch { paths: p.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    fn remotes(r: &[&str]) -> PoolMatch {
        PoolMatch { remotes: r.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    fn vars(v: &[(&str, &str)]) -> PoolMatch {
        PoolMatch { env: v.iter().map(|(k, p)| (k.to_string(), p.to_string())).collect(), ..Default::default() }
    }

    #[test]
    fn a_path_rule_claims_the_directory_and_everything_under_it() {
        let c = cfg(&[("work", paths(&["/srv/work"]))]);
        for cwd in ["/srv/work", "/srv/work/", "/srv/work/api", "/srv/work/api/deep", "/srv/work/./api", "/srv/work/api/../other"] {
            let got = resolve(&c, &env(cwd));
            assert_eq!(got.pool, "work", "{cwd} should be work: {got:?}");
        }
        // A sibling that merely shares a name prefix is a different directory.
        for cwd in ["/srv", "/srv/workshop", "/srv/work-other", "/elsewhere"] {
            assert_eq!(resolve(&c, &env(cwd)).pool, DEFAULT_POOL, "{cwd} should fall through");
        }
    }

    /// `foo`, `foo/*` and `foo/**` are the same rule: the glob suffixes are
    /// what a user writes out of habit, and a path rule always covers the tree.
    #[test]
    fn trailing_globs_and_tildes_are_accepted() {
        for rule in ["/srv/work", "/srv/work/", "/srv/work/*", "/srv/work/**"] {
            let c = cfg(&[("work", paths(&[rule]))]);
            assert_eq!(resolve(&c, &env("/srv/work/api")).pool, "work", "rule {rule}");
        }
        let home = dirs::home_dir().unwrap();
        let c = cfg(&[("work", paths(&["~/proj"]))]);
        assert_eq!(resolve(&c, &env(&home.join("proj/x").to_string_lossy())).pool, "work");
        assert_eq!(resolve(&c, &env("/proj/x")).pool, DEFAULT_POOL, "~ must expand, not match literally");
    }

    #[test]
    fn a_remote_rule_matches_the_git_origin_url() {
        let c = cfg(&[("work", remotes(&[r"(?i)^git@github\.com:acme/"]))]);
        let with = |url: &'static str| LaunchEnv { cwd: "/tmp/x".into(), getenv: Box::new(|_| None), remote: Box::new(move |_| Some(url.into())) };
        assert_eq!(resolve(&c, &with("git@github.com:acme/thing.git")).pool, "work");
        assert_eq!(resolve(&c, &with("git@GitHub.com:ACME/thing.git")).pool, "work", "the pattern's own (?i) should apply");
        assert_eq!(resolve(&c, &with("git@github.com:other/thing.git")).pool, DEFAULT_POOL);
        // Not a repository, no origin, or no git at all: the rule just fails.
        assert_eq!(resolve(&c, &env("/tmp/x")).pool, DEFAULT_POOL);
        assert_eq!(resolve(&c, &with("")).pool, DEFAULT_POOL);
    }

    #[test]
    fn an_env_rule_matches_a_value_or_mere_presence() {
        let c = cfg(&[("work", vars(&[("TC_CTX", "^work-")]))]);
        let with = |val: &'static str| LaunchEnv {
            cwd: "/tmp/x".into(),
            getenv: Box::new(move |k| (k == "TC_CTX").then(|| val.to_string())),
            remote: Box::new(|_| None),
        };
        assert_eq!(resolve(&c, &with("work-eu")).pool, "work");
        assert_eq!(resolve(&c, &with("personal")).pool, DEFAULT_POOL);
        assert_eq!(resolve(&c, &env("/tmp/x")).pool, DEFAULT_POOL, "unset must not match");

        // An empty pattern means "set to anything"; unset still does not match.
        let c = cfg(&[("work", vars(&[("TC_CTX", "")]))]);
        assert_eq!(resolve(&c, &with("anything")).pool, "work");
        assert_eq!(resolve(&c, &with("")).pool, DEFAULT_POOL, "empty is not set");
        assert_eq!(resolve(&c, &env("/tmp/x")).pool, DEFAULT_POOL);
    }

    /// Rules within one pool are OR'd: any single satisfied condition matches,
    /// which is how a "this directory, or anything with that remote" wrapper
    /// reads.
    #[test]
    fn rules_within_a_pool_are_alternatives() {
        let m = PoolMatch { paths: vec!["/srv/work".into()], remotes: vec!["acme/".into()], env: [("TC_CTX".to_string(), String::new())].into() };
        let c = cfg(&[("work", m)]);
        // Path alone.
        assert_eq!(resolve(&c, &env("/srv/work/api")).reason, "path /srv/work");
        // Remote alone, from an unrelated directory.
        let by_remote = LaunchEnv { cwd: "/tmp/x".into(), getenv: Box::new(|_| None), remote: Box::new(|_| Some("git@gh:acme/x".into())) };
        assert_eq!(resolve(&c, &by_remote).reason, "remote ~ acme/");
        // Environment alone.
        let by_env = LaunchEnv { cwd: "/tmp/x".into(), getenv: Box::new(|k| (k == "TC_CTX").then(|| "1".to_string())), remote: Box::new(|_| None) };
        assert_eq!(resolve(&c, &by_env).reason, "env TC_CTX set");
    }

    /// Two pools claiming the same directory resolve by sorted name, so the
    /// answer never depends on config order or map iteration.
    #[test]
    fn ties_break_on_sorted_name() {
        let c = cfg(&[("zeta", paths(&["/srv/work"])), ("alpha", paths(&["/srv/work"]))]);
        assert_eq!(resolve(&c, &env("/srv/work")).pool, "alpha");
    }

    #[test]
    fn nothing_matching_is_the_default_pool() {
        let c = cfg(&[("work", paths(&["/srv/work"]))]);
        let got = resolve(&c, &env("/elsewhere"));
        assert_eq!(got, Choice { pool: DEFAULT_POOL.into(), matched: false, reason: "no rule matched".into() });

        // So is a pool with no rules at all, however many of them there are.
        let mut c = Config::default();
        for n in ["work", "spare"] {
            c.ensure_pool(n).unwrap();
        }
        assert_eq!(resolve(&c, &env("/srv/work")).pool, DEFAULT_POOL);
    }

    /// The default pool is the fallback, so rules on it are never consulted —
    /// otherwise a stray `paths` entry there would shadow every other pool.
    #[test]
    fn the_default_pool_never_matches_on_rules() {
        let mut c = cfg(&[("work", paths(&["/srv/work"]))]);
        c.pool_mut(DEFAULT_POOL).unwrap().match_rules = Some(paths(&["/srv"]));
        let got = resolve(&c, &env("/srv/work/api"));
        assert_eq!(got.pool, "work");
        assert!(got.matched);
        // And it is still the answer when nothing else matches, by fallback and
        // not by its own rule.
        assert!(!resolve(&c, &env("/srv/other")).matched);
    }

    /// A pattern that does not compile can never match. Config validation
    /// rejects it up front; resolution must not panic if one reaches it anyway
    /// (a hand-edited file the daemon has not re-read, say).
    #[test]
    fn an_uncompilable_pattern_is_inert() {
        let c = cfg(&[("work", remotes(&["("]))]);
        let with = LaunchEnv { cwd: "/tmp/x".into(), getenv: Box::new(|_| None), remote: Box::new(|_| Some("(".into())) };
        assert_eq!(resolve(&c, &with).pool, DEFAULT_POOL);

        let mut c = Config::default();
        c.ensure_pool("work").unwrap();
        c.pool_mut("work").unwrap().match_rules = Some(remotes(&["("]));
        assert!(c.validate().is_err(), "config validation should reject it");
    }

    /// Rules are per-pool state like any other, so they survive a save/load
    /// round trip under the `match` key.
    #[test]
    fn rules_round_trip_through_json() {
        let m = PoolMatch { paths: vec!["~/proj".into()], remotes: vec!["acme/".into()], env: [("TC_CTX".into(), "^a".into())].into() };
        let pool = PoolConfig { match_rules: Some(m.clone()), ..Default::default() };
        let json = serde_json::to_string(&pool).unwrap();
        assert!(json.contains(r#""match":{"paths":["~/proj"],"remotes":["acme/"],"env":{"TC_CTX":"^a"}}"#), "{json}");
        assert_eq!(serde_json::from_str::<PoolConfig>(&json).unwrap().match_rules, Some(m));
        // Absent rather than null when there are no rules, so a config written
        // before this feature is unchanged by a re-save.
        let plain = serde_json::to_string(&PoolConfig::default()).unwrap();
        assert!(!plain.contains("match"), "{plain}");
    }
}
