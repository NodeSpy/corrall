//! `corrall setup claude`: write the proxy into Claude Code's user settings.
//!
//! Claude Code applies the `env` block of `~/.claude/settings.json` to every
//! session, so the variables `corrall env` prints can live there instead of in
//! a shell wrapper. The file is static, so this is base-URL mode with one pool
//! chosen at setup time; the per-launch pool matching and the MITM mode stay
//! with `corrall env` and `corrall run`.
//!
//! Setting up also takes out whatever else in the file would send Claude Code
//! somewhere other than the proxy, or sign it in with something other than its
//! own login: another gateway's URL and token, a forward proxy, a cloud
//! provider switch, an `apiKeyHelper`. Nothing here prints a value from the
//! file, only key names: the values it replaces are often credentials.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};

use crate::cli::EnvOp;
use crate::config::Config;

/// `env` entries that route Claude Code, or authenticate it, somewhere else.
/// Any of these that setup does not itself write is removed.
const COMPETING_ENV: &[&str] = &[
    // Another gateway, and the credential or headers that went with it.
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_CUSTOM_HEADERS",
    // A forward proxy: base-URL mode dials the proxy directly.
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "https_proxy",
    "http_proxy",
    // A cloud provider: with one selected, ANTHROPIC_BASE_URL is not read.
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_AWS_BASE_URL",
];

/// The provider switches among [`COMPETING_ENV`]. Model ids written for one of
/// these are not ids Anthropic serves, hence [`Report::models`].
const PROVIDER_SWITCHES: &[&str] =
    &["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY", "CLAUDE_CODE_USE_MANTLE", "CLAUDE_CODE_USE_ANTHROPIC_AWS"];

/// Top-level settings that replace Claude Code's own login as the credential.
const COMPETING_TOP_LEVEL: &[&str] = &["apiKeyHelper"];

/// What an edit did to the settings, by key name only.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Keys written: new, or holding a different value before.
    pub set: Vec<String>,
    /// Keys taken out.
    pub removed: Vec<String>,
    /// Model-selection keys left in place after a provider switch was removed.
    pub models: Vec<String>,
}

impl Report {
    pub fn changed(&self) -> bool {
        !self.set.is_empty() || !self.removed.is_empty()
    }
}

fn root_of(settings: &mut Value) -> Result<&mut Map<String, Value>> {
    settings.as_object_mut().ok_or_else(|| anyhow!("the settings file does not hold a JSON object"))
}

fn is_model_key(k: &str) -> bool {
    k == "ANTHROPIC_MODEL" || k == "ANTHROPIC_SMALL_FAST_MODEL" || (k.starts_with("ANTHROPIC_DEFAULT_") && k.ends_with("_MODEL"))
}

/// Point `settings` at the proxy: apply `ops` (the lines of `corrall env`,
/// taken apart) to its `env` block and remove everything that competes.
pub fn apply(settings: &mut Value, ops: &[EnvOp]) -> Result<Report> {
    let root = root_of(settings)?;
    let mut report = Report::default();
    if !root.contains_key("env") {
        root.insert("env".into(), Value::Object(Map::new()));
    }
    let env = root.get_mut("env").and_then(Value::as_object_mut).ok_or_else(|| anyhow!("\"env\" in the settings file is not an object"))?;

    let written: Vec<&str> = ops.iter().filter_map(|op| if let EnvOp::Set(k, _) = op { Some(k.as_str()) } else { None }).collect();
    let unset = ops.iter().filter_map(|op| if let EnvOp::Unset(k) = op { Some(k.as_str()) } else { None });
    let mut switched_provider = false;
    for k in COMPETING_ENV.iter().copied().chain(unset) {
        if !written.contains(&k) && env.shift_remove(k).is_some() {
            switched_provider |= PROVIDER_SWITCHES.contains(&k);
            report.removed.push(k.to_string());
        }
    }
    for op in ops {
        if let EnvOp::Set(k, v) = op {
            if env.get(k).and_then(Value::as_str) != Some(v) {
                // An existing key keeps its place in the file.
                env.insert(k.clone(), Value::String(v.clone()));
                report.set.push(k.clone());
            }
        }
    }
    if switched_provider {
        report.models = env.keys().filter(|k| is_model_key(k)).cloned().collect();
    }
    for k in COMPETING_TOP_LEVEL {
        if root.shift_remove(*k).is_some() {
            report.removed.push(k.to_string());
        }
    }
    if switched_provider && root.contains_key("model") {
        report.models.push("model".into());
    }
    Ok(report)
}

/// Whether `value` under `key` is something [`apply`] wrote for this config,
/// as opposed to a setting of the user's own that shares the name.
fn is_ours(cfg: &Config, key: &str, value: &str) -> bool {
    match key {
        "ANTHROPIC_BASE_URL" => points_at_proxy(cfg, value),
        "ENABLE_TOOL_SEARCH" => value == "auto",
        "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL" => value == "1",
        // Matched on the header name, not the key: the key may have been
        // rotated since setup.
        "ANTHROPIC_CUSTOM_HEADERS" => {
            !value.contains('\n') && value.split_once(':').is_some_and(|(n, _)| n.trim().eq_ignore_ascii_case(crate::proxy::auth::PROXY_KEY_HEADER))
        }
        "ANTHROPIC_API_KEY" => value == cfg.proxy.api_key,
        "API_TIMEOUT_MS" => cfg.pools.values().any(|p| p.hold_seconds > 0 && value == (p.hold_seconds * 1000 + 60_000).to_string()),
        _ => false,
    }
}

fn points_at_proxy(cfg: &Config, url: &str) -> bool {
    let base = crate::cli::proxy_base(cfg);
    url.strip_prefix(base.as_str()).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Undo [`apply`]: take the proxy's own entries back out. Does nothing when
/// the settings do not point at the proxy, and leaves alone any entry whose
/// value is not one setup writes. What setup removed is not put back; that is
/// what the backup file is for.
pub fn strip(settings: &mut Value, cfg: &Config) -> Result<Report> {
    let root = root_of(settings)?;
    let mut report = Report::default();
    let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) else { return Ok(report) };
    if !env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str).is_some_and(|u| points_at_proxy(cfg, u)) {
        return Ok(report);
    }
    let ours: Vec<String> = env.iter().filter(|(k, v)| v.as_str().is_some_and(|v| is_ours(cfg, k, v))).map(|(k, _)| k.clone()).collect();
    for k in ours {
        env.shift_remove(&k);
        report.removed.push(k);
    }
    if env.is_empty() {
        root.shift_remove("env");
    }
    Ok(report)
}

/// Competing variables exported in the environment this command runs in. A
/// settings `env` value overrides a shell export of the same name, but a
/// variable the settings do not set still reaches Claude Code from the shell.
pub fn shell_conflicts(ops: &[EnvOp]) -> Vec<&'static str> {
    COMPETING_ENV
        .iter()
        .copied()
        .filter(|k| !ops.iter().any(|op| matches!(op, EnvOp::Set(w, _) if w.as_str() == *k)))
        .filter(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
        .collect()
}

/// Claude Code's user settings file: under `CLAUDE_CONFIG_DIR` when that is
/// set, `~/.claude` otherwise.
pub fn default_path() -> Result<PathBuf> {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => dirs::home_dir().ok_or_else(|| anyhow!("cannot find the home directory; pass --settings FILE"))?.join(".claude"),
    };
    Ok(dir.join("settings.json"))
}

/// A settings file read for editing.
pub struct SettingsFile {
    /// Where the file really is: a symlink (a dotfiles checkout, say) is
    /// followed, so the write replaces its target and not the link.
    pub path: PathBuf,
    pub value: Value,
    original: Option<Vec<u8>>,
}

impl SettingsFile {
    /// A missing or empty file reads as `{}`. One that is not valid JSON is an
    /// error: Claude Code would reject it as well, and guessing at a repair
    /// could drop settings.
    pub fn read(path: &Path) -> Result<SettingsFile> {
        let path = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
            Err(e) => return Err(e).with_context(|| format!("resolving {}", path.display())),
        };
        let original = match std::fs::read(&path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let value = match &original {
            Some(b) if !b.iter().all(u8::is_ascii_whitespace) => {
                serde_json::from_slice(b).with_context(|| format!("{} is not valid JSON; fix it before running setup", path.display()))?
            }
            _ => Value::Object(Map::new()),
        };
        if !value.is_object() {
            bail!("{} does not hold a JSON object", path.display());
        }
        Ok(SettingsFile { path, value, original })
    }

    /// Write the edited settings back, atomically, after copying what was
    /// there to a timestamped backup beside it. Returns the backup's path.
    ///
    /// `private` is for a file that now holds the proxy key: it is written at
    /// mode 0600. Otherwise the file keeps the mode it had. The backup is
    /// always 0600, since what setup removed may have been a credential.
    pub fn write(&self, private: bool) -> Result<Option<PathBuf>> {
        use std::io::Write;
        let dir = self.path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

        let backup = match &self.original {
            Some(bytes) => {
                let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
                let mut f = tempfile::NamedTempFile::new_in(dir)?;
                f.write_all(bytes)?;
                f.as_file().sync_all()?;
                // Never over an earlier backup: the first one taken is the
                // file as it was before setup touched it.
                let mut n = 0;
                loop {
                    let mut name = self.path.file_name().unwrap_or_default().to_os_string();
                    name.push(format!(".corrall-backup-{stamp}"));
                    if n > 0 {
                        name.push(format!("-{n}"));
                    }
                    let backup = self.path.with_file_name(name);
                    match f.persist_noclobber(&backup) {
                        Ok(_) => break Some(backup),
                        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => {
                            f = e.file;
                            n += 1;
                        }
                        Err(e) => return Err(e.error).with_context(|| format!("writing {}", backup.display())),
                    }
                }
            }
            None => None,
        };

        let mut json = serde_json::to_vec_pretty(&self.value)?;
        json.push(b'\n');
        // Created 0600, which is where a file holding the key should stay.
        let mut f = tempfile::NamedTempFile::new_in(dir)?;
        f.write_all(&json)?;
        f.as_file().sync_all()?;
        if !private {
            if let Ok(md) = std::fs::metadata(&self.path) {
                let _ = f.as_file().set_permissions(md.permissions());
            }
        }
        f.persist(&self.path).with_context(|| format!("writing {}", self.path.display()))?;
        Ok(backup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ops(cfg: &Config, pool: Option<&str>, mode: crate::cli::KeyMode) -> Vec<EnvOp> {
        crate::cli::env_ops(&crate::cli::env_lines_with(cfg, false, None, pool, mode))
    }

    fn header_ops() -> Vec<EnvOp> {
        ops(&Config::default(), None, crate::cli::KeyMode::Header)
    }

    #[test]
    fn apply_writes_the_proxy_into_empty_settings() {
        let mut s = json!({});
        let r = apply(&mut s, &header_ops()).unwrap();
        assert_eq!(
            s,
            json!({"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:3456", "ENABLE_TOOL_SEARCH": "auto", "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL": "1"}})
        );
        assert_eq!(r.set, ["ANTHROPIC_BASE_URL", "ENABLE_TOOL_SEARCH", "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL"]);
        assert!(r.removed.is_empty());
        // A second run finds nothing to do.
        assert!(!apply(&mut s, &header_ops()).unwrap().changed());
    }

    #[test]
    fn apply_replaces_another_gateway() {
        let mut s = json!({
            "model": "opus",
            "apiKeyHelper": "/bin/get-gateway-key",
            "env": {
                "DISABLE_AUTO_COMPACT": "1",
                "ANTHROPIC_BASE_URL": "https://gateway.example.com",
                "ANTHROPIC_AUTH_TOKEN": "gw-token",
                "ANTHROPIC_API_KEY": "gw-key",
                "ANTHROPIC_CUSTOM_HEADERS": "x-gateway-team: blue",
                "HTTPS_PROXY": "http://user:pw@proxy.example.com:8080",
                "NODE_EXTRA_CA_CERTS": "/etc/corp-ca.pem"
            },
            "permissions": {"allow": ["Bash(ls)"]}
        });
        let r = apply(&mut s, &header_ops()).unwrap();
        assert_eq!(
            s,
            json!({
                "model": "opus",
                "env": {
                    "DISABLE_AUTO_COMPACT": "1",
                    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3456",
                    "NODE_EXTRA_CA_CERTS": "/etc/corp-ca.pem",
                    "ENABLE_TOOL_SEARCH": "auto",
                    "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL": "1"
                },
                "permissions": {"allow": ["Bash(ls)"]}
            })
        );
        // Key order survives, with the replaced URL where the old one was.
        let keys: Vec<&String> = s["env"].as_object().unwrap().keys().collect();
        assert_eq!(keys[..3], ["DISABLE_AUTO_COMPACT", "ANTHROPIC_BASE_URL", "NODE_EXTRA_CA_CERTS"]);
        assert_eq!(r.removed, ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY", "ANTHROPIC_CUSTOM_HEADERS", "HTTPS_PROXY", "apiKeyHelper"]);
        assert!(r.set.contains(&"ANTHROPIC_BASE_URL".to_string()));
        assert!(r.models.is_empty(), "no provider was switched off");
    }

    #[test]
    fn apply_switches_a_cloud_provider_off_and_flags_its_model_ids() {
        let mut s = json!({
            "model": "us.anthropic.claude-opus",
            "env": {"CLAUDE_CODE_USE_BEDROCK": "1", "ANTHROPIC_BEDROCK_BASE_URL": "https://bedrock-gw.example.com", "AWS_REGION": "us-east-1", "ANTHROPIC_DEFAULT_HAIKU_MODEL": "us.anthropic.claude-haiku"}
        });
        let r = apply(&mut s, &header_ops()).unwrap();
        let env = s["env"].as_object().unwrap();
        assert!(!env.contains_key("CLAUDE_CODE_USE_BEDROCK") && !env.contains_key("ANTHROPIC_BEDROCK_BASE_URL"));
        assert_eq!(env["AWS_REGION"], "us-east-1", "inert without the switch, so left alone");
        assert_eq!(r.models, ["ANTHROPIC_DEFAULT_HAIKU_MODEL", "model"]);
    }

    #[test]
    fn apply_carries_the_key_only_when_the_server_asks_for_one() {
        let mut cfg = Config::default();
        cfg.proxy.api_key = "tc-secret-0123456789abcdef".into();
        let mut s = json!({"env": {"ANTHROPIC_CUSTOM_HEADERS": "x-gateway-team: blue"}});
        apply(&mut s, &ops(&cfg, None, crate::cli::KeyMode::Header)).unwrap();
        assert!(!s.to_string().contains("tc-secret"), "loopback needs no key");
        assert!(s["env"].get("ANTHROPIC_CUSTOM_HEADERS").is_none());

        cfg.proxy.require_key_on_loopback = true;
        let mut s = json!({"env": {"ANTHROPIC_API_KEY": "sk-other"}});
        apply(&mut s, &ops(&cfg, None, crate::cli::KeyMode::Header)).unwrap();
        assert_eq!(s["env"]["ANTHROPIC_CUSTOM_HEADERS"], "x-corrall-key: tc-secret-0123456789abcdef");
        assert!(s["env"].get("ANTHROPIC_API_KEY").is_none(), "an API key would switch Claude Code off its login");

        let mut s = json!({"env": {"ANTHROPIC_API_KEY": "sk-other", "ANTHROPIC_CUSTOM_HEADERS": "x-corrall-key: stale"}});
        apply(&mut s, &ops(&cfg, None, crate::cli::KeyMode::ApiKey)).unwrap();
        assert_eq!(s["env"]["ANTHROPIC_API_KEY"], "tc-secret-0123456789abcdef");
        assert!(s["env"].get("ANTHROPIC_CUSTOM_HEADERS").is_none());
    }

    #[test]
    fn apply_names_a_pool_in_the_base_url() {
        let mut cfg = Config::default();
        cfg.pools.insert("work".into(), crate::config::PoolConfig { hold_seconds: 30, ..Default::default() });
        let mut s = json!({});
        apply(&mut s, &ops(&cfg, Some("work"), crate::cli::KeyMode::Header)).unwrap();
        assert_eq!(s["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:3456/pool/work");
        assert_eq!(s["env"]["API_TIMEOUT_MS"], "90000");
        // Whichever pool it was set up with, removal finds it.
        let r = strip(&mut s, &cfg).unwrap();
        assert_eq!(s, json!({}));
        assert_eq!(r.removed.len(), 4);
    }

    #[test]
    fn apply_rejects_settings_it_cannot_edit() {
        assert!(apply(&mut json!([]), &header_ops()).is_err());
        assert!(apply(&mut json!({"env": "nope"}), &header_ops()).is_err());
    }

    #[test]
    fn strip_takes_out_only_what_setup_wrote() {
        let cfg = Config::default();
        let mut s = json!({"env": {"DISABLE_AUTO_COMPACT": "1", "API_TIMEOUT_MS": "600000"}, "model": "opus"});
        apply(&mut s, &header_ops()).unwrap();
        let r = strip(&mut s, &cfg).unwrap();
        assert_eq!(s, json!({"env": {"DISABLE_AUTO_COMPACT": "1", "API_TIMEOUT_MS": "600000"}, "model": "opus"}));
        assert_eq!(r.removed, ["ANTHROPIC_BASE_URL", "ENABLE_TOOL_SEARCH", "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL"]);
        assert!(!strip(&mut s, &cfg).unwrap().changed());
    }

    #[test]
    fn strip_leaves_settings_that_point_elsewhere() {
        let cfg = Config::default();
        for url in ["https://gateway.example.com", "http://127.0.0.1:34567", "http://127.0.0.1:3456.example.com"] {
            let mut s = json!({"env": {"ANTHROPIC_BASE_URL": url, "ENABLE_TOOL_SEARCH": "auto"}});
            let before = s.clone();
            assert!(!strip(&mut s, &cfg).unwrap().changed(), "{url}");
            assert_eq!(s, before);
        }
        assert!(!strip(&mut json!({}), &cfg).unwrap().changed());
    }

    #[test]
    fn file_round_trip_backs_up_and_follows_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-settings.json");
        std::fs::write(&real, "{\n  \"model\": \"opus\",\n  \"env\": {\"ANTHROPIC_AUTH_TOKEN\": \"gw-token\"}\n}\n").unwrap();
        let link = dir.path().join("settings.json");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        let link = real.clone();

        let mut f = SettingsFile::read(&link).unwrap();
        assert!(apply(&mut f.value, &header_ops()).unwrap().changed());
        let backup = f.write(false).unwrap().expect("an existing file is backed up");

        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link itself is not replaced");
        let written: Value = serde_json::from_slice(&std::fs::read(&link).unwrap()).unwrap();
        assert_eq!(written["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:3456");
        assert_eq!(written["model"], "opus");
        assert!(std::fs::read_to_string(&backup).unwrap().contains("gw-token"), "the backup is the file as it was");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn file_modes_follow_what_the_file_holds() {
        let dir = tempfile::tempdir().unwrap();
        // A file that does not exist yet reads as empty and has no backup.
        let path = dir.path().join("claude").join("settings.json");
        let mut f = SettingsFile::read(&path).unwrap();
        apply(&mut f.value, &header_ops()).unwrap();
        assert!(f.write(false).unwrap().is_none());
        assert!(path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let f = SettingsFile::read(&path).unwrap();
            let first = f.write(false).unwrap().unwrap();
            assert_eq!(mode(&path), 0o644, "no secret: the file keeps its mode");
            let second = f.write(true).unwrap().unwrap();
            assert_eq!(mode(&path), 0o600, "holding the key: private");
            assert_ne!(first, second, "a backup taken in the same second does not replace the earlier one");
            assert!(first.exists() && second.exists());
        }
    }

    #[test]
    fn read_refuses_a_file_it_cannot_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ \"env\": { // comment\n } }").unwrap();
        assert!(SettingsFile::read(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ \"env\": { // comment\n } }", "left untouched");
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(SettingsFile::read(&path).unwrap().value, json!({}));
    }
}
