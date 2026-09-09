//! Reading accounts out of a claudeacrobat install.
//!
//! claudeacrobat is the sibling Go proxy. It keeps one JSON file per account
//! under its state directory: `accounts/` for the pool that serves unprefixed
//! requests, and `pools/<name>/accounts/` for a named one. This module turns
//! those files into corrall [`AccountConfig`] values; `corrall
//! import-claudeacrobat` is what writes them into the config.
//!
//! The mapping is the inverse of claudeacrobat's own `import-teamclaude`. An
//! `owned` account — claudeacrobat holds the tokens and refreshes them — becomes
//! an OAuth account with the tokens inline. A `linked` one — tokens are read
//! live out of a Claude Code credentials file — becomes an OAuth account whose
//! `importFrom` points at that same file, which is exactly how corrall
//! models the arrangement. Nothing else in claudeacrobat's model has a
//! corrall equivalent, so anything that cannot map is reported rather than
//! guessed at.
//!
//! Nothing here touches claudeacrobat's files: the import is a read, and both
//! proxies can keep the same account afterwards (on different ports).

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::config::{validate_pool_name, AccountConfig, AccountType};

/// claudeacrobat's own default for `state_dir`.
pub const DEFAULT_STATE_DIR: &str = "~/.local/state/claudeacrobat";

/// The state directory to import from when `--from` is not given: whatever
/// claudeacrobat's config says, so a relocated install is found without the
/// operator having to repeat the path, and the documented default otherwise.
pub fn default_state_dir() -> PathBuf {
    state_dir_of(&config_path()).unwrap_or_else(|| crate::oauth::expand_home(DEFAULT_STATE_DIR))
}

/// claudeacrobat's config location: `$XDG_CONFIG_HOME/claudeacrobat/config.json`.
fn config_path() -> PathBuf {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    dir.join("claudeacrobat").join("config.json")
}

/// `state_dir` out of a claudeacrobat config, if the file is there and says.
fn state_dir_of(path: &Path) -> Option<PathBuf> {
    let raw = std::fs::read(path).ok()?;
    let doc: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let dir = doc.get("state_dir")?.as_str()?.trim();
    (!dir.is_empty()).then(|| crate::oauth::expand_home(dir))
}

/// One claudeacrobat account, mapped but not yet written.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapped {
    /// The pool it came from: `None` for claudeacrobat's default pool, which
    /// belongs in whichever pool corrall serves unprefixed requests from.
    pub pool: Option<String>,
    pub account: AccountConfig,
}

impl Mapped {
    /// `"linked"` or `"owned"`, in claudeacrobat's vocabulary, for display.
    pub fn kind(&self) -> &'static str {
        if self.account.import_from.is_some() {
            "linked"
        } else {
            "owned"
        }
    }
}

/// A claudeacrobat account that has no corrall equivalent.
#[derive(Debug, Clone, PartialEq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// Everything one state directory offers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    pub accounts: Vec<Mapped>,
    pub skipped: Vec<Skipped>,
}

/// Read every account file under `dir`, in a stable order: the default pool
/// first, then named pools by name, and files within a pool by file name.
pub fn plan(dir: &Path) -> Result<Plan> {
    if !dir.is_dir() {
        bail!("no claudeacrobat state directory at {}; pass --from DIR", dir.display());
    }
    let mut plan = Plan::default();
    collect(&dir.join("accounts"), None, &mut plan);
    for name in pool_names(&dir.join("pools")) {
        let accounts = dir.join("pools").join(&name).join("accounts");
        // corrall's pool charset is the stricter of the two, and the name
        // becomes a URL segment here. Reject it loudly rather than mangling it
        // into something the router would not recognise.
        if let Err(e) = validate_pool_name(&name) {
            for f in account_files(&accounts) {
                plan.skipped.push(Skipped { name: file_label(&f), reason: format!("pool \"{name}\" cannot be named that in corrall: {e}") });
            }
            continue;
        }
        collect(&accounts, Some(name), &mut plan);
    }
    Ok(plan)
}

/// Named pool directories, sorted, so two runs agree on the order.
fn pool_names(pools: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(pools) else { return Vec::new() };
    let mut names: Vec<String> =
        rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).filter_map(|e| e.file_name().to_str().map(str::to_string)).collect();
    names.sort();
    names
}

/// `*.json` files in one `accounts/` directory, sorted by name.
fn account_files(accounts: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(accounts) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json") && p.is_file()).collect();
    files.sort();
    files
}

fn collect(accounts: &Path, pool: Option<String>, plan: &mut Plan) {
    for path in account_files(accounts) {
        match read_account(&path) {
            Ok(account) => plan.accounts.push(Mapped { pool: pool.clone(), account }),
            Err(reason) => plan.skipped.push(Skipped { name: file_label(&path), reason }),
        }
    }
}

/// What to call an account whose file would not parse: the file name is all we
/// can be sure of.
fn file_label(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

fn read_account(path: &Path) -> Result<AccountConfig, String> {
    let raw = std::fs::read(path).map_err(|e| format!("unreadable: {e}"))?;
    let file: AccountFile = serde_json::from_slice(&raw).map_err(|e| format!("not a claudeacrobat account file: {e}"))?;
    map(file)
}

// ── claudeacrobat's on-disk shape ─────────────────────────────
//
// Only the fields that carry over are named. `source` and `addedAt` are
// claudeacrobat bookkeeping, and its per-account state (usage windows, cooldown)
// lives in a separate state.json that corrall rebuilds by probing.

#[derive(Debug, Default, Deserialize)]
struct AccountFile {
    #[serde(default)]
    name: String,
    /// `owned` or `linked`.
    #[serde(default)]
    kind: String,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    disabled: bool,
    /// Claude Code credentials file a linked account reads. Note the snake_case
    /// key: claudeacrobat spells this one field that way on disk.
    #[serde(default)]
    credentials_file: Option<String>,
    #[serde(default)]
    profile: Profile,
    #[serde(default)]
    oauth: Option<OAuthTokens>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    #[serde(default)]
    account_uuid: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    org_uuid: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
    #[serde(default)]
    subscription_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuthTokens {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    /// ms since epoch, same unit as corrall's `expiresAt`.
    #[serde(default)]
    expires_at: i64,
    #[serde(default)]
    subscription_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
}

/// Map one account file, or say why it cannot be mapped.
fn map(file: AccountFile) -> Result<AccountConfig, String> {
    let name = file.name.trim();
    if name.is_empty() {
        return Err("the file carries no account name".into());
    }
    let mut a = AccountConfig { name: name.to_string(), kind: AccountType::Oauth, priority: file.priority, disabled: file.disabled, ..Default::default() };
    match file.kind.as_str() {
        "owned" => {
            let o = file.oauth.filter(|o| !o.access_token.trim().is_empty()).ok_or("an owned account with no access token")?;
            a.access_token = Some(o.access_token);
            a.refresh_token = some_text(o.refresh_token);
            // 0 means "unknown" in claudeacrobat; corrall reads a missing
            // expiry as "refresh before the next request", which is what we want.
            a.expires_at = (o.expires_at > 0).then_some(o.expires_at);
            a.subscription_type = some_text(o.subscription_type);
            a.rate_limit_tier = some_text(o.rate_limit_tier);
        }
        "linked" => {
            let f = some_text(file.credentials_file).ok_or("a linked account with no credentials_file")?;
            a.import_from = Some(f);
        }
        other => return Err(format!("unknown account kind {other:?}")),
    }
    a.account_uuid = some_text(file.profile.account_uuid);
    a.org_uuid = some_text(file.profile.org_uuid);
    a.org_name = some_text(file.profile.org_name);
    a.email = some_text(file.profile.email).or_else(|| some_text(file.profile.display_name));
    // The tokens are the fresher source for these two, so only fall back to the
    // profile copy.
    a.subscription_type = a.subscription_type.take().or_else(|| some_text(file.profile.subscription_type));
    a.rate_limit_tier = a.rate_limit_tier.take().or_else(|| some_text(file.profile.rate_limit_tier));
    Ok(a)
}

/// `Some` only for a string with something in it: claudeacrobat omits empty
/// fields, but a hand-edited file may carry `""`, and an empty email or uuid in
/// the config reads as data when it is really absence.
fn some_text(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn an_owned_account_carries_its_tokens_over() {
        let f: AccountFile = serde_json::from_str(
            r#"{"name":"a@example.com","kind":"owned","priority":3,"disabled":true,
                "profile":{"accountUuid":"au","email":"a@example.com","orgUuid":"ou","orgName":"Acme","subscriptionType":"max"},
                "oauth":{"accessToken":"at","refreshToken":"rt","expiresAt":1700000000000,"rateLimitTier":"tier-5"}}"#,
        )
        .unwrap();
        let a = map(f).unwrap();
        assert_eq!(a.name, "a@example.com");
        assert_eq!(a.kind, AccountType::Oauth);
        assert_eq!(a.priority, 3);
        assert!(a.disabled);
        assert_eq!(a.access_token.as_deref(), Some("at"));
        assert_eq!(a.refresh_token.as_deref(), Some("rt"));
        assert_eq!(a.expires_at, Some(1_700_000_000_000));
        assert_eq!(a.import_from, None);
        assert_eq!(a.account_uuid.as_deref(), Some("au"));
        assert_eq!(a.org_uuid.as_deref(), Some("ou"));
        assert_eq!(a.org_name.as_deref(), Some("Acme"));
        assert_eq!(a.email.as_deref(), Some("a@example.com"));
        assert_eq!(a.subscription_type.as_deref(), Some("max"));
        assert_eq!(a.rate_limit_tier.as_deref(), Some("tier-5"));
    }

    #[test]
    fn a_linked_account_becomes_an_import_from() {
        let f: AccountFile =
            serde_json::from_str(r#"{"name":"work","kind":"linked","credentials_file":"~/.claude/.credentials.json","profile":{"displayName":"Work"}}"#)
                .unwrap();
        let a = map(f).unwrap();
        assert_eq!(a.import_from.as_deref(), Some("~/.claude/.credentials.json"));
        assert_eq!(a.access_token, None);
        assert_eq!(a.refresh_token, None);
        // No email in the profile, so the display name stands in for one.
        assert_eq!(a.email.as_deref(), Some("Work"));
    }

    #[test]
    fn an_unusable_account_says_why() {
        let cases = [
            (r#"{"name":"","kind":"owned"}"#, "the file carries no account name"),
            (r#"{"name":"x","kind":"owned"}"#, "an owned account with no access token"),
            (r#"{"name":"x","kind":"owned","oauth":{"accessToken":"  "}}"#, "an owned account with no access token"),
            (r#"{"name":"x","kind":"linked"}"#, "a linked account with no credentials_file"),
            (r#"{"name":"x","kind":"linked","credentials_file":""}"#, "a linked account with no credentials_file"),
            (r#"{"name":"x","kind":"apikey"}"#, "unknown account kind \"apikey\""),
            (r#"{"name":"x"}"#, "unknown account kind \"\""),
        ];
        for (json, reason) in cases {
            let f: AccountFile = serde_json::from_str(json).unwrap();
            assert_eq!(map(f).unwrap_err(), reason, "{json}");
        }
    }

    #[test]
    fn a_state_directory_is_read_pool_by_pool() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(&d.join("accounts/b.json"), r#"{"name":"b","kind":"owned","oauth":{"accessToken":"t"}}"#);
        write(&d.join("accounts/a.json"), r#"{"name":"a","kind":"linked","credentials_file":"/creds.json"}"#);
        write(&d.join("accounts/notes.txt"), "not an account");
        write(&d.join("pools/work/accounts/c.json"), r#"{"name":"c","kind":"owned","oauth":{"accessToken":"t"}}"#);
        write(&d.join("pools/alt/accounts/d.json"), r#"{"name":"d","kind":"owned"}"#);
        write(&d.join("pools/alt/accounts/broken.json"), "{");

        let p = plan(d).unwrap();
        // Default pool first, then named pools by name, files by name.
        let got: Vec<(Option<&str>, &str)> = p.accounts.iter().map(|m| (m.pool.as_deref(), m.account.name.as_str())).collect();
        assert_eq!(got, vec![(None, "a"), (None, "b"), (Some("work"), "c")]);
        assert_eq!(p.accounts[0].kind(), "linked");
        assert_eq!(p.accounts[1].kind(), "owned");
        let skipped: Vec<(&str, &str)> = p.skipped.iter().map(|s| (s.name.as_str(), s.reason.as_str())).collect();
        assert_eq!(skipped[0].0, "broken.json");
        assert!(skipped[0].1.starts_with("not a claudeacrobat account file"), "{:?}", skipped[0].1);
        assert_eq!(skipped[1], ("d.json", "an owned account with no access token"));
    }

    #[test]
    fn a_pool_corrall_cannot_name_is_skipped_not_mangled() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(&d.join("pools/Work/accounts/a.json"), r#"{"name":"a","kind":"owned","oauth":{"accessToken":"t"}}"#);
        let p = plan(d).unwrap();
        assert!(p.accounts.is_empty());
        assert_eq!(p.skipped.len(), 1);
        assert!(p.skipped[0].reason.contains("cannot be named that in corrall"), "{:?}", p.skipped[0].reason);
    }

    #[test]
    fn a_missing_directory_is_an_error_with_the_flag_to_fix_it() {
        let dir = tempfile::tempdir().unwrap();
        let e = plan(&dir.path().join("nope")).unwrap_err().to_string();
        assert!(e.contains("--from DIR"), "{e}");
    }

    #[test]
    fn a_relocated_state_dir_is_read_from_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        std::fs::write(&cfg, r#"{"listen":"127.0.0.1:8080","state_dir":"/srv/acrobat"}"#).unwrap();
        assert_eq!(state_dir_of(&cfg), Some(PathBuf::from("/srv/acrobat")));
        std::fs::write(&cfg, r#"{"state_dir":"  "}"#).unwrap();
        assert_eq!(state_dir_of(&cfg), None);
        std::fs::write(&cfg, "{}").unwrap();
        assert_eq!(state_dir_of(&cfg), None);
        assert_eq!(state_dir_of(&dir.path().join("absent.json")), None);
    }
}
