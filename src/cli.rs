//! Command-line interface.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use crate::config::{AccountConfig, AccountType, Config, PoolConfig, RouteConfig, Threshold};
use crate::oauth;

#[derive(Parser, Debug)]
#[command(name = "teamclaude", version, about = "Multi-account Claude proxy with automatic quota-based rotation", long_about = None)]
pub struct Cli {
    /// Log format for the server: text or json
    #[arg(long, global = true, default_value = "text")]
    pub log_format: String,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the proxy server (default). Shows the TUI on a terminal.
    Server(ServerArgs),
    /// OAuth login via browser (or --token for copy/paste, --api for an API key)
    Login(LoginArgs),
    /// Import credentials from Claude Code's credential store
    Import(ImportArgs),
    /// List configured accounts
    Accounts {
        #[arg(short, long)]
        verbose: bool,
        /// Only accounts in this pool
        #[arg(long)]
        pool: Option<String>,
    },
    /// Show live proxy status (needs a running server)
    Status {
        #[arg(long)]
        json: bool,
        /// ANSI colors: auto, always or never
        #[arg(long, default_value = "auto")]
        color: String,
    },
    /// Make the running server prefer one account
    Switch {
        name: Option<String>,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Remove an account
    Remove {
        name: String,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Exclude an account from rotation
    Disable {
        name: String,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Re-enable an account (also clears a stuck error state)
    Enable {
        name: String,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Set rotation priority (lower = preferred)
    Priority {
        name: String,
        value: Option<i32>,
        #[arg(long)]
        first: bool,
        #[arg(long)]
        last: bool,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Show or set the switch threshold (percent, or bucket=percent)
    Threshold {
        value: Option<String>,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Spread new sessions across equal-priority accounts (on|off)
    Distribute {
        value: Option<String>,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Background quota probe interval in seconds (off|N)
    Probe {
        value: Option<String>,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Keep idle accounts' 5h windows running (off|N seconds, min 60; spends quota)
    Warmup { value: Option<String> },
    /// Expiry-pressure routing (on|off), with --tolerance and --preempt
    Expiry {
        value: Option<String>,
        #[arg(long)]
        tolerance: Option<f64>,
        #[arg(long)]
        preempt: Option<String>,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Session titles in the activity log (on|off)
    Titles { value: Option<String> },
    /// Per-model routing rules
    Route {
        #[command(subcommand)]
        cmd: Option<RouteCmd>,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Named account pools, each with its own rotation
    Pool {
        #[command(subcommand)]
        cmd: Option<PoolCmd>,
    },
    /// Print shell export lines that point Claude Code at the proxy
    Env(EnvArgs),
    /// Run Claude Code through the proxy
    Run(RunArgs),
    /// Print the path of the MITM CA certificate
    CaPath,
    /// Validate the config file and print a redacted summary
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
    },
    /// Manage the user service (systemd --user on Linux)
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Replace this binary with the latest release (verified; never runs unattended)
    Update(crate::update::UpdateArgs),
    /// Call an API endpoint with an account's credentials (GET)
    Api {
        path: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        pool: Option<String>,
    },
}

#[derive(Args, Debug, Default, Clone)]
pub struct ServerArgs {
    /// Run without the interactive TUI
    #[arg(long, alias = "no-tui")]
    pub headless: bool,
    /// Log every request/response to DIR (one file per request)
    #[arg(long, value_name = "DIR")]
    pub log_to: Option<String>,
    /// Append activity lines to FILE
    #[arg(long, value_name = "FILE")]
    pub activity_log: Option<String>,
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Copy/paste flow with no local callback listener
    #[arg(long)]
    pub token: bool,
    /// Add an Anthropic API key account instead of OAuth
    #[arg(long)]
    pub api: bool,
    /// Sign in to an OpenAI Codex subscription instead
    #[arg(long)]
    pub codex: bool,
    /// Do not try to open a browser
    #[arg(long)]
    pub no_browser: bool,
    /// Account name (default: email from the profile)
    #[arg(long)]
    pub name: Option<String>,
    /// Pool to put the account in (created if new; moves an existing account)
    #[arg(long)]
    pub pool: Option<String>,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Credentials path (default ~/.claude/.credentials.json)
    #[arg(long, default_value = oauth::DEFAULT_CREDENTIALS_PATH)]
    pub from: String,
    /// Keep reading tokens from the file on every reload instead of copying them
    #[arg(long)]
    pub link: bool,
    /// Import a Codex CLI login (default path ~/.codex/auth.json)
    #[arg(long)]
    pub codex: bool,
    #[arg(long)]
    pub name: Option<String>,
    /// Pool to put the account in (created if new; moves an existing account)
    #[arg(long)]
    pub pool: Option<String>,
}

#[derive(Args, Debug)]
pub struct EnvArgs {
    /// Base-URL routing only (no forward proxy / CA)
    #[arg(long)]
    pub no_mitm: bool,
    /// Route through this pool (default: the configured default pool)
    #[arg(long)]
    pub pool: Option<String>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(long)]
    pub no_mitm: bool,
    /// Launch claude directly if the proxy is down
    #[arg(long)]
    pub auto_fallback: bool,
    /// Route through this pool (default: the configured default pool)
    #[arg(long)]
    pub pool: Option<String>,
    /// Arguments passed to claude
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum RouteCmd {
    List,
    Add {
        name: String,
        #[arg(long, value_delimiter = ',')]
        r#match: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        accounts: Vec<String>,
        #[arg(long)]
        bucket: Option<String>,
        #[arg(long)]
        color: Option<String>,
    },
    Rm {
        name: String,
    },
}

/// Settings shared by `pool add` and `pool set`. Every field is optional: an
/// omitted flag leaves that setting alone, so `set` can change one knob without
/// restating the rest.
#[derive(Args, Debug, Default)]
pub struct PoolSetArgs {
    /// Switch threshold: a percent, or bucket=percent (bucket=default to clear)
    #[arg(long)]
    pub threshold: Option<String>,
    /// Spread new sessions across equal-priority accounts (on|off)
    #[arg(long)]
    pub distribute: Option<String>,
    /// Background quota probe interval in seconds (off|N)
    #[arg(long)]
    pub probe: Option<String>,
    /// Seconds to hold a request while waiting for quota (0 = off)
    #[arg(long)]
    pub hold: Option<u64>,
    /// Move these accounts into the pool
    #[arg(long, value_delimiter = ',')]
    pub account: Vec<String>,
    /// Serve requests that carry no /pool/ prefix from this pool
    #[arg(long)]
    pub make_default: bool,
}

#[derive(Subcommand, Debug)]
pub enum PoolCmd {
    /// List pools, their accounts and their settings
    List,
    /// Create a pool (or change one that exists)
    Add {
        name: String,
        #[command(flatten)]
        set: PoolSetArgs,
    },
    /// Change a pool's settings
    Set {
        name: String,
        #[command(flatten)]
        set: PoolSetArgs,
    },
    /// Delete a pool. It must be empty unless --force.
    Rm {
        name: String,
        /// Delete the pool's accounts along with it
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    Check,
    Path,
}

#[derive(Subcommand, Debug)]
pub enum ServiceCmd {
    Print,
    Install,
    Uninstall,
    Status,
}

// ── helpers ───────────────────────────────────────────────────

pub fn proxy_base(cfg: &Config) -> String {
    format!("http://127.0.0.1:{}", cfg.proxy.port)
}

async fn control_get(cfg: &Config, path: &str) -> Result<Value> {
    let r = crate::upstream::client()
        .get(format!("{}{}", proxy_base(cfg), path))
        .header("x-api-key", &cfg.proxy.api_key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| anyhow!("proxy is not running on port {} (start it with `teamclaude server`)", cfg.proxy.port))?;
    if !r.status().is_success() {
        bail!("proxy answered HTTP {}", r.status());
    }
    Ok(r.json().await?)
}

async fn control_post(cfg: &Config, path: &str, body: Value) -> Result<Value> {
    let r = crate::upstream::client()
        .post(format!("{}{}", proxy_base(cfg), path))
        .header("x-api-key", &cfg.proxy.api_key)
        .timeout(std::time::Duration::from_secs(10))
        .json(&body)
        .send()
        .await?;
    Ok(r.json().await.unwrap_or(json!({})))
}

/// Ask a running server to reload; silently no-op if none.
pub async fn notify_reload(cfg: &Config) {
    let _ = control_post(cfg, "/teamclaude/reload", json!({})).await;
}

/// Resolve a `--pool` flag to a configured pool name, defaulting to the pool
/// that serves unprefixed requests. An unknown name is an error rather than a
/// silent fallback: on the CLI a typo should not quietly retarget the command.
fn pool_name(cfg: &Config, pool: Option<&str>) -> Result<String> {
    match pool {
        None => Ok(cfg.default_pool.clone()),
        Some(p) if cfg.pools.contains_key(p) => Ok(p.to_string()),
        Some(p) => bail!("no pool named \"{p}\"; `teamclaude pool list` shows the configured pools"),
    }
}

/// The pool an account-adding command should write to. Unlike [`pool_name`],
/// `--pool` may name a pool that does not exist yet: `login --pool work` is how
/// you create one.
fn target_pool(cfg: &mut Config, pool: Option<&str>) -> Result<String> {
    let Some(p) = pool else { return Ok(cfg.default_pool.clone()) };
    let fresh = !cfg.pools.contains_key(p);
    cfg.ensure_pool(p)?;
    if fresh {
        eprintln!("Created pool \"{p}\"");
    }
    Ok(p.to_string())
}

/// `" in pool \"x\""`, or nothing when the command did not name a pool.
fn pool_note(pool: Option<&str>) -> String {
    pool.map(|p| format!(" in pool \"{p}\"")).unwrap_or_default()
}

fn pool_of<'a>(cfg: &'a Config, pool: Option<&str>) -> Result<&'a PoolConfig> {
    let name = pool_name(cfg, pool)?;
    Ok(cfg.pool(&name).expect("pool_name only returns configured pools"))
}

fn pool_mut_of<'a>(cfg: &'a mut Config, pool: Option<&str>) -> Result<&'a mut PoolConfig> {
    let name = pool_name(cfg, pool)?;
    Ok(cfg.pool_mut(&name).expect("pool_name only returns configured pools"))
}

/// Locate an account for a command: inside `--pool` when one was given, and
/// anywhere in the file otherwise. Searching every pool by default keeps
/// `teamclaude disable <name>` working exactly as it did before pools existed.
fn locate(cfg: &Config, pool: Option<&str>, needle: &str) -> Result<(String, usize)> {
    let found = match pool {
        Some(p) => {
            let p = pool_name(cfg, Some(p))?;
            cfg.find_account_idx_in(&p, needle).map(|i| (p, i))
        }
        None => cfg.find_account(needle),
    };
    found.ok_or_else(|| match pool {
        Some(p) => anyhow!("no account in pool \"{p}\" matches \"{needle}\""),
        None => anyhow!("no account matches \"{needle}\""),
    })
}

fn find_account_mut<'a>(cfg: &'a mut Config, pool: Option<&str>, name: &str) -> Result<&'a mut AccountConfig> {
    let (p, i) = locate(cfg, pool, name)?;
    Ok(&mut cfg.pool_mut(&p).expect("locate returns a configured pool").accounts[i])
}

/// The first account in the file matching `pred`, with the pool holding it.
/// Pools are searched in display order, so the default pool wins a tie.
fn locate_by(cfg: &Config, mut pred: impl FnMut(&AccountConfig) -> bool) -> Option<(String, usize)> {
    cfg.pool_names().into_iter().find_map(|p| cfg.pool(&p).and_then(|pc| pc.accounts.iter().position(&mut pred)).map(|i| (p, i)))
}

/// The account at `found`, guaranteed to live in `pool` — moved there if it was
/// somewhere else, created from `new` if `found` is `None`. Moving rather than
/// copying matters: two pools rotating the same credential would double-spend
/// one account's quota without either fleet knowing.
///
/// `pool` must already be a valid name — callers go through
/// [`Config::ensure_pool`], which is what checks the charset.
fn entry_at<'a>(cfg: &'a mut Config, pool: &str, found: Option<(String, usize)>, new: impl FnOnce() -> AccountConfig) -> (&'a mut AccountConfig, bool) {
    match found {
        Some((from, i)) if from == pool => (&mut cfg.pool_mut(&from).expect("located pool exists").accounts[i], true),
        Some((from, i)) => {
            let a = cfg.pool_mut(&from).expect("located pool exists").accounts.remove(i);
            let dst = cfg.pools.entry(pool.to_string()).or_default();
            dst.accounts.push(a);
            (dst.accounts.last_mut().unwrap(), true)
        }
        None => {
            let dst = cfg.pools.entry(pool.to_string()).or_default();
            dst.accounts.push(new());
            (dst.accounts.last_mut().unwrap(), false)
        }
    }
}

fn upsert_oauth(cfg: &mut Config, pool: &str, name: &str, tokens: &oauth::Tokens, profile: Option<&oauth::Profile>) -> bool {
    // Identify by uuid when the profile gave us one, and only fall back to the
    // display name — an account first added with `--name` (no profile) has no
    // uuid to match on yet.
    let found = profile
        .and_then(|p| p.account_uuid.as_deref())
        .and_then(|au| {
            let ou = profile.and_then(|p| p.org_uuid.as_deref());
            locate_by(cfg, |a| a.account_uuid.as_deref() == Some(au) && (ou.is_none() || a.org_uuid.as_deref() == ou))
        })
        .or_else(|| locate_by(cfg, |a| a.name == name));
    let (entry, updated) = entry_at(cfg, pool, found, || AccountConfig { name: name.to_string(), kind: AccountType::Oauth, ..Default::default() });
    entry.name = name.to_string();
    entry.kind = AccountType::Oauth;
    entry.import_from = None;
    entry.access_token = Some(tokens.access_token.clone());
    entry.refresh_token = tokens.refresh_token.clone();
    entry.expires_at = Some(tokens.expires_at);
    if let Some(p) = profile {
        entry.account_uuid = p.account_uuid.clone().or(entry.account_uuid.take());
        entry.org_uuid = p.org_uuid.clone().or(entry.org_uuid.take());
        entry.org_name = p.org_name.clone().or(entry.org_name.take());
        entry.email = p.email.clone().or(entry.email.take());
        entry.rate_limit_tier = p.rate_limit_tier.clone().or(entry.rate_limit_tier.take());
        entry.seat_tier = p.seat_tier.clone().or(entry.seat_tier.take());
    }
    updated
}

fn display_name(profile: &oauth::Profile, cfg: &Config) -> String {
    let email = profile.email.clone().or(profile.display_name.clone()).unwrap_or_else(|| "account".into());
    let same_email_other_org =
        cfg.all_accounts().any(|(_, a)| a.email.as_deref() == Some(email.as_str()) && a.org_uuid.is_some() && a.org_uuid != profile.org_uuid);
    match (&profile.org_name, same_email_other_org) {
        (Some(org), true) => format!("{email} ({org})"),
        _ => email,
    }
}

// ── commands ──────────────────────────────────────────────────

pub async fn login(args: LoginArgs) -> Result<()> {
    let mut cfg = Config::load_or_create()?;
    crate::upstream::init(&cfg)?;
    let want_pool = args.pool.clone();
    if args.api {
        eprint!("Anthropic API key: ");
        let key = read_secret()?;
        if !key.starts_with("sk-ant-") {
            bail!("that does not look like an Anthropic API key");
        }
        let name = args.name.unwrap_or_else(|| format!("api-{}", &key[key.len().saturating_sub(6)..]));
        Config::update(|c| {
            let pool = target_pool(c, want_pool.as_deref())?;
            let found = locate_by(c, |a| a.name == name);
            let (entry, _) = entry_at(c, &pool, found, || AccountConfig { name: name.clone(), kind: AccountType::Apikey, priority: 10, ..Default::default() });
            entry.kind = AccountType::Apikey;
            entry.api_key = Some(key.clone());
            Ok(())
        })?;
        eprintln!("Added API key account \"{name}\"{}", pool_note(want_pool.as_deref()));
        notify_reload(&cfg).await;
        return Ok(());
    }
    if args.codex {
        let c = crate::codex::login_browser(!args.no_browser).await?;
        let access = c.access_token.clone().ok_or_else(|| anyhow!("OpenAI returned no access token"))?;
        let name = args.name.clone().or(c.email.clone()).unwrap_or_else(|| "codex".into());
        let cfg = Config::update(|cfg| {
            let pool = target_pool(cfg, want_pool.as_deref())?;
            let found = locate_by(cfg, |a| a.is_codex() && (a.account_id == c.account_id && c.account_id.is_some() || a.name == name));
            let (entry, _) = entry_at(cfg, &pool, found, || AccountConfig {
                name: name.clone(),
                kind: AccountType::Oauth,
                provider: Some("codex".into()),
                ..Default::default()
            });
            entry.name = name.clone();
            entry.import_from = None;
            entry.access_token = Some(access.clone());
            entry.refresh_token = c.refresh_token.clone();
            entry.expires_at = c.expires_at;
            entry.account_id = c.account_id.clone().or(entry.account_id.take());
            entry.email = c.email.clone().or(entry.email.take());
            entry.plan_type = c.plan_type.clone().or(entry.plan_type.take());
            Ok(())
        })?;
        eprintln!(
            "Added Codex account \"{name}\"{}{}",
            c.plan_type.as_ref().map(|p| format!(" (plan {p})")).unwrap_or_default(),
            pool_note(want_pool.as_deref())
        );
        notify_reload(&cfg).await;
        return Ok(());
    }
    let tokens = if args.token { oauth::login_paste().await? } else { oauth::login_browser(!args.no_browser).await? };
    let profile = match oauth::fetch_profile(&tokens.access_token).await {
        Ok(p) => Some(p),
        Err(e) if args.name.is_some() => {
            eprintln!("Profile lookup failed ({e}); continuing with --name");
            None
        }
        Err(e) => bail!("profile lookup failed: {e}. Retry, or pass --name to add the account without profile detection"),
    };
    let name = args.name.clone().unwrap_or_else(|| display_name(profile.as_ref().unwrap(), &cfg));
    let updated = Config::update(|c| {
        let pool = target_pool(c, want_pool.as_deref())?;
        upsert_oauth(c, &pool, &name, &tokens, profile.as_ref());
        Ok(())
    })
    .map(|c| {
        cfg = c;
        true
    })?;
    let _ = updated;
    if let Some(p) = &profile {
        eprintln!(
            "Logged in as {} ({}){}{}",
            name,
            p.org_name.clone().unwrap_or_else(|| "personal".into()),
            p.rate_limit_tier.as_ref().map(|t| format!(", tier {t}")).unwrap_or_default(),
            pool_note(want_pool.as_deref())
        );
    } else {
        eprintln!("Added account \"{name}\"{}", pool_note(want_pool.as_deref()));
    }
    notify_reload(&cfg).await;
    Ok(())
}

fn read_secret() -> Result<String> {
    use std::io::BufRead;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

pub async fn import(args: ImportArgs) -> Result<()> {
    let cfg = Config::load_or_create()?;
    crate::upstream::init(&cfg)?;
    if args.codex {
        let from = if args.from == oauth::DEFAULT_CREDENTIALS_PATH { crate::codex::DEFAULT_CREDENTIALS_PATH.to_string() } else { args.from.clone() };
        let c = crate::codex::import_credentials(&from)?;
        let access = c.access_token.clone().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("{from} carries no access token"))?;
        let name = args.name.clone().or(c.email.clone()).unwrap_or_else(|| "codex".into());
        let cfg = Config::update(|cfg| {
            let pool = target_pool(cfg, args.pool.as_deref())?;
            let found = locate_by(cfg, |a| a.is_codex() && (a.account_id == c.account_id && c.account_id.is_some() || a.name == name));
            let (entry, _) = entry_at(cfg, &pool, found, || AccountConfig {
                name: name.clone(),
                kind: AccountType::Oauth,
                provider: Some("codex".into()),
                ..Default::default()
            });
            entry.name = name.clone();
            if args.link {
                entry.import_from = Some(from.clone());
                entry.access_token = None;
                entry.refresh_token = None;
                entry.expires_at = None;
            } else {
                entry.import_from = None;
                entry.access_token = Some(access.clone());
                entry.refresh_token = c.refresh_token.clone();
                entry.expires_at = c.expires_at;
            }
            entry.account_id = c.account_id.clone().or(entry.account_id.take());
            entry.email = c.email.clone().or(entry.email.take());
            entry.plan_type = c.plan_type.clone().or(entry.plan_type.take());
            Ok(())
        })?;
        eprintln!("Imported Codex account \"{name}\"{}{}", if args.link { " (linked)" } else { "" }, pool_note(args.pool.as_deref()));
        notify_reload(&cfg).await;
        return Ok(());
    }
    let creds = oauth::import_credentials(&args.from)?;
    let access = creds.access_token.clone().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("{} carries no access token", args.from))?;
    let profile = match oauth::fetch_profile(&access).await {
        Ok(p) => Some(p),
        Err(e) if args.name.is_some() => {
            eprintln!("Profile lookup failed ({e}); continuing with --name");
            None
        }
        Err(e) => bail!("profile lookup failed: {e}. Pass --name to import without profile detection"),
    };
    let name = args.name.clone().unwrap_or_else(|| display_name(profile.as_ref().unwrap(), &cfg));
    let tokens = oauth::Tokens { access_token: access, refresh_token: creds.refresh_token.clone(), expires_at: creds.expires_at.unwrap_or(0) };
    let cfg = Config::update(|c| {
        let pool = target_pool(c, args.pool.as_deref())?;
        upsert_oauth(c, &pool, &name, &tokens, profile.as_ref());
        // `upsert_oauth` just put the account in `pool`, so look it up there
        // rather than anywhere in the file.
        let i = c.find_account_idx_in(&pool, &name).expect("the account upsert_oauth just wrote");
        let a = &mut c.pool_mut(&pool).expect("target_pool created it").accounts[i];
        if args.link {
            a.import_from = Some(args.from.clone());
            a.access_token = None;
            a.refresh_token = None;
            a.expires_at = None;
        }
        if a.subscription_type.is_none() {
            a.subscription_type = creds.subscription_type.clone();
        }
        Ok(())
    })?;
    eprintln!("Imported \"{name}\"{}{}", if args.link { " (linked to the credential file)" } else { "" }, pool_note(args.pool.as_deref()));
    notify_reload(&cfg).await;
    Ok(())
}

pub fn accounts(verbose: bool, pool: Option<String>) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet; run `teamclaude login`"))?;
    if let Some(p) = &pool {
        pool_name(&cfg, Some(p))?;
    }
    // The pool column only appears once there is more than one pool, so a
    // single-pool install sees exactly the table it saw before.
    let show_pool = pool.is_none() && cfg.pools.len() > 1;
    let mut listed: Vec<(String, &AccountConfig)> = Vec::new();
    for p in cfg.pool_names() {
        if pool.as_deref().is_some_and(|want| want != p) {
            continue;
        }
        for a in &cfg.pool(&p).expect("pool_names lists configured pools").accounts {
            listed.push((p.clone(), a));
        }
    }
    if listed.is_empty() {
        match &pool {
            Some(p) => println!("No accounts in pool \"{p}\"."),
            None => println!("No accounts. Run `teamclaude login` or `teamclaude import`."),
        }
        return Ok(());
    }
    if show_pool {
        print!("{:<16} ", "POOL");
    }
    println!("{:<30} {:<7} {:>4} {:<9} {}", "NAME", "TYPE", "PRI", "STATE", if verbose { "DETAILS" } else { "" });
    for (pool, a) in &listed {
        let kind = match (a.is_codex(), &a.kind) {
            (true, _) => "codex",
            (false, AccountType::Oauth) => "oauth",
            (false, AccountType::Apikey) => "apikey",
        };
        let state = if a.disabled { "disabled" } else { "enabled" };
        let mut details = String::new();
        if verbose {
            if let Some(e) = a.expires_at {
                let left = (e - crate::quota::now_ms()) / 1000;
                details
                    .push_str(&format!("token {} ", if left > 0 { format!("expires in {}", crate::status::countdown(Some(left))) } else { "expired".into() }));
            }
            if let Some(u) = &a.account_uuid {
                details.push_str(&format!("uuid={u} "));
            }
            if let Some(o) = &a.org_name {
                details.push_str(&format!("org={o} "));
            }
            if let Some(t) = a.rate_limit_tier.as_ref().or(a.seat_tier.as_ref()).or(a.subscription_type.as_ref()) {
                details.push_str(&format!("tier={t} "));
            }
            if let Some(u) = &a.upstream {
                details.push_str(&format!("upstream={u} "));
            }
            if let Some(i) = &a.import_from {
                details.push_str(&format!("from={i} "));
            }
        }
        if show_pool {
            print!("{:<16} ", crate::security::safe_text(pool, 16));
        }
        println!("{:<30} {:<7} {:>4} {:<9} {}", crate::security::safe_text(&a.name, 30), kind, a.priority, state, details);
    }
    Ok(())
}

pub async fn status(json_out: bool, color: &str) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    crate::upstream::init(&cfg)?;
    let st = control_get(&cfg, "/teamclaude/status").await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&st)?);
    } else {
        let use_color = match color {
            "always" => true,
            "never" => false,
            _ => std::io::IsTerminal::is_terminal(&std::io::stdout()) && std::env::var_os("NO_COLOR").is_none(),
        };
        println!("{}", crate::status::render(&st, use_color, crate::quota::now_ms()));
    }
    Ok(())
}

pub async fn switch(name: Option<String>, pool: Option<String>) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    crate::upstream::init(&cfg)?;
    match name {
        None => {
            let st = control_get(&cfg, "/teamclaude/status").await?;
            // Every pool's accounts, so `switch` with no argument still shows
            // the whole set you could switch to.
            let empty = vec![];
            let pools = st.get("pools").and_then(Value::as_array).unwrap_or(&empty);
            let multi = pools.len() > 1;
            for p in pools {
                let this = p.get("pool").and_then(Value::as_str).unwrap_or("?");
                if pool.as_deref().is_some_and(|want| want != this) {
                    continue;
                }
                for a in p.get("accounts").and_then(Value::as_array).unwrap_or(&empty) {
                    let cur = a.get("current").and_then(Value::as_bool).unwrap_or(false);
                    let name = a.get("name").and_then(Value::as_str).unwrap_or("?");
                    let where_ = if multi { format!("{this}/") } else { String::new() };
                    println!("{} {where_}{name}", if cur { "►" } else { " " });
                }
            }
        }
        Some(n) => {
            // The server scopes the search to `pool` when we send one and
            // searches every pool when we do not.
            let mut body = json!({ "account": n });
            if let Some(p) = &pool {
                body["pool"] = json!(p);
            }
            let r = control_post(&cfg, "/teamclaude/switch", body).await?;
            if r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                println!(
                    "Switched to {}{}{}",
                    r.get("account").and_then(Value::as_str).unwrap_or("?"),
                    r.get("pool").and_then(Value::as_str).map(|p| format!(" in pool \"{p}\"")).unwrap_or_default(),
                    r.get("blocked").and_then(Value::as_str).map(|b| format!(" (note: {b})")).unwrap_or_default()
                );
            } else {
                bail!("{}", r.get("error").and_then(Value::as_str).unwrap_or("switch failed"));
            }
        }
    }
    Ok(())
}

pub async fn remove(name: String, pool: Option<String>) -> Result<()> {
    let cfg = Config::update(|c| {
        let (p, i) = locate(c, pool.as_deref(), &name)?;
        let removed = c.pool_mut(&p).expect("locate returns a configured pool").accounts.remove(i);
        eprintln!("Removed \"{}\" from pool \"{p}\"", removed.name);
        Ok(())
    })?;
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn set_disabled(name: String, disabled: bool, pool: Option<String>) -> Result<()> {
    let cfg = Config::update(|c| {
        let a = find_account_mut(c, pool.as_deref(), &name)?;
        a.disabled = disabled;
        eprintln!("{} \"{}\"", if disabled { "Disabled" } else { "Enabled" }, a.name);
        Ok(())
    })?;
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn priority(name: String, value: Option<i32>, first: bool, last: bool, pool: Option<String>) -> Result<()> {
    let cfg = Config::update(|c| {
        // Priority orders one pool's rotation, so --first/--last are relative
        // to the accounts the account in question actually competes with.
        let (owner, _) = locate(c, pool.as_deref(), &name)?;
        let peers = &c.pool(&owner).expect("locate returns a configured pool").accounts;
        let v = if first {
            peers.iter().map(|a| a.priority).min().unwrap_or(0) - 1
        } else if last {
            peers.iter().map(|a| a.priority).max().unwrap_or(0) + 1
        } else {
            value.ok_or_else(|| anyhow!("give a number, --first or --last"))?
        };
        let a = find_account_mut(c, pool.as_deref(), &name)?;
        a.priority = v;
        eprintln!("Priority of \"{}\" is now {v}", a.name);
        Ok(())
    })?;
    notify_reload(&cfg).await;
    Ok(())
}

/// Apply a `PCT` or `bucket=PCT` threshold spec to `t` in place.
fn set_threshold(t: &mut Threshold, v: &str) -> Result<()> {
    if let Some((bucket, pct)) = v.split_once('=') {
        let mut table = match t {
            Threshold::Single(x) => std::collections::BTreeMap::from([("default".to_string(), *x)]),
            Threshold::Table(t) => t.clone(),
        };
        if pct == "default" {
            table.remove(bucket);
        } else {
            let p: f64 = pct.parse().context("percent must be a number")?;
            if !(1.0..=100.0).contains(&p) {
                bail!("percent must be 1-100");
            }
            table.insert(bucket.to_string(), p / 100.0);
        }
        *t = Threshold::Table(table);
    } else {
        let p: f64 = v.parse().context("percent must be a number")?;
        if !(1.0..=100.0).contains(&p) {
            bail!("percent must be 1-100");
        }
        *t = Threshold::Single(p / 100.0);
    }
    Ok(())
}

/// Seconds from an `off|N` argument.
fn parse_secs(v: &str, min: u64, what: &str) -> Result<u64> {
    let secs: u64 = if v.eq_ignore_ascii_case("off") { 0 } else { v.parse().context("seconds must be a number or off")? };
    if secs != 0 && secs < min {
        bail!("minimum {what} is {min} seconds");
    }
    Ok(secs)
}

pub async fn threshold(value: Option<String>, pool: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&pool_of(&c, pool.as_deref())?.switch_threshold)?);
            return Ok(());
        }
        Some(v) => Config::update(|c| {
            let p = pool_mut_of(c, pool.as_deref())?;
            set_threshold(&mut p.switch_threshold, &v)?;
            eprintln!("switchThreshold = {}", serde_json::to_string(&p.switch_threshold)?);
            Ok(())
        })?,
    };
    notify_reload(&cfg).await;
    Ok(())
}

fn parse_on_off(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => bail!("expected on or off"),
    }
}

pub async fn distribute(value: Option<String>, pool: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("distributeSessions: {}", if pool_of(&c, pool.as_deref())?.distribute_sessions { "on" } else { "off" });
            return Ok(());
        }
        Some(v) => {
            let on = parse_on_off(&v)?;
            Config::update(|c| {
                pool_mut_of(c, pool.as_deref())?.distribute_sessions = on;
                eprintln!("distributeSessions: {}", if on { "on" } else { "off" });
                Ok(())
            })?
        }
    };
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn probe(value: Option<String>, pool: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("quotaProbeSeconds: {}", pool_of(&c, pool.as_deref())?.quota_probe_seconds);
            return Ok(());
        }
        Some(v) => {
            let secs = parse_secs(&v, 30, "probe interval")?;
            Config::update(|c| {
                pool_mut_of(c, pool.as_deref())?.quota_probe_seconds = secs;
                eprintln!("quotaProbeSeconds: {secs}");
                Ok(())
            })?
        }
    };
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn warmup(value: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("warmupSeconds: {}", c.warmup_seconds);
            return Ok(());
        }
        Some(v) => {
            let secs = parse_secs(&v, 60, "keep-warm interval")?;
            Config::update(|c| {
                c.warmup_seconds = secs;
                eprintln!("warmupSeconds: {secs}{}", if secs > 0 { " (spends a little quota per idle account per window)" } else { "" });
                Ok(())
            })?
        }
    };
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn expiry(value: Option<String>, tolerance: Option<f64>, preempt: Option<String>, pool: Option<String>) -> Result<()> {
    if value.is_none() && tolerance.is_none() && preempt.is_none() {
        let c = Config::load()?.unwrap_or_default();
        println!("{}", serde_json::to_string_pretty(&pool_of(&c, pool.as_deref())?.expiry_routing)?);
        return Ok(());
    }
    let cfg = Config::update(|c| {
        let p = pool_mut_of(c, pool.as_deref())?;
        if let Some(v) = &value {
            p.expiry_routing.enabled = parse_on_off(v)?;
        }
        if let Some(t) = tolerance {
            if t.is_nan() || t < 1.0 {
                bail!("tolerance must be >= 1.0");
            }
            p.expiry_routing.tolerance = t;
        }
        if let Some(x) = &preempt {
            p.expiry_routing.preempt = parse_on_off(x)?;
        }
        eprintln!("expiryRouting = {}", serde_json::to_string(&p.expiry_routing)?);
        Ok(())
    })?;
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn titles(value: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&c.session_titles)?);
            return Ok(());
        }
        Some(v) => {
            let on = parse_on_off(&v)?;
            Config::update(|c| {
                c.session_titles.enabled = on;
                eprintln!("sessionTitles.enabled: {on}");
                Ok(())
            })?
        }
    };
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn route(cmd: Option<RouteCmd>, pool: Option<String>) -> Result<()> {
    match cmd.unwrap_or(RouteCmd::List) {
        RouteCmd::List => {
            let c = Config::load()?.unwrap_or_default();
            // Routes belong to a pool. Without --pool, list every pool's, since
            // that is the whole routing picture.
            let names = match &pool {
                Some(p) => vec![pool_name(&c, Some(p))?],
                None => c.pool_names(),
            };
            let multi = names.len() > 1;
            let mut any = false;
            for p in &names {
                for r in &c.pool(p).expect("pool_names lists configured pools").routes {
                    any = true;
                    let where_ = if multi { format!("{p}/") } else { String::new() };
                    println!(
                        "{:<12} match={:?} accounts={:?}{}",
                        format!("{where_}{}", r.name),
                        r.patterns,
                        r.accounts,
                        r.bucket.as_ref().map(|b| format!(" bucket={b}")).unwrap_or_default()
                    );
                }
            }
            if !any {
                println!("no routes");
            }
            Ok(())
        }
        RouteCmd::Add { name, r#match, accounts, bucket, color } => {
            if r#match.is_empty() {
                bail!("--match is required");
            }
            if let Some(b) = &bucket {
                if !crate::model::is_weekly_bucket(b) {
                    bail!("bucket must be one of unified7d, unified7dFable, unified7dSonnet");
                }
            }
            let cfg = Config::update(|c| {
                // A route can only steer traffic to accounts in its own pool.
                let owner = pool_name(c, pool.as_deref())?;
                for a in &accounts {
                    if c.find_account_idx_in(&owner, a).is_none() {
                        bail!("no account in pool \"{owner}\" matches \"{a}\"");
                    }
                }
                let p = c.pool_mut(&owner).expect("pool_name only returns configured pools");
                p.routes.retain(|r| r.name != name);
                p.routes.push(RouteConfig {
                    name: name.clone(),
                    patterns: r#match.clone(),
                    accounts: accounts.clone(),
                    bucket: bucket.clone(),
                    color: color.clone(),
                });
                eprintln!("route \"{name}\" saved in pool \"{owner}\"");
                Ok(())
            })?;
            notify_reload(&cfg).await;
            Ok(())
        }
        RouteCmd::Rm { name } => {
            let cfg = Config::update(|c| {
                // Without --pool, remove the route wherever it lives.
                let owner = match &pool {
                    Some(p) => pool_name(c, Some(p))?,
                    None => c
                        .pool_names()
                        .into_iter()
                        .find(|p| c.pool(p).is_some_and(|pc| pc.routes.iter().any(|r| r.name == name)))
                        .ok_or_else(|| anyhow!("no route named \"{name}\""))?,
                };
                let p = c.pool_mut(&owner).expect("pool_name only returns configured pools");
                let n = p.routes.len();
                p.routes.retain(|r| r.name != name);
                if p.routes.len() == n {
                    bail!("no route named \"{name}\" in pool \"{owner}\"");
                }
                eprintln!("route \"{name}\" removed from pool \"{owner}\"");
                Ok(())
            })?;
            notify_reload(&cfg).await;
            Ok(())
        }
    }
}

pub async fn pool(cmd: Option<PoolCmd>) -> Result<()> {
    match cmd.unwrap_or(PoolCmd::List) {
        PoolCmd::List => {
            let c = Config::load()?.unwrap_or_default();
            println!("{:<18} {:>8} {:>10} {:>10} {:>7} {:>5}", "NAME", "ACCOUNTS", "THRESHOLD", "DISTRIBUTE", "PROBE", "HOLD");
            for name in c.pool_names() {
                let p = c.pool(&name).expect("pool_names lists configured pools");
                let star = if name == c.default_pool { "*" } else { "" };
                println!(
                    "{:<18} {:>8} {:>10} {:>10} {:>7} {:>5}",
                    crate::security::safe_text(&format!("{name}{star}"), 18),
                    p.accounts.len(),
                    serde_json::to_string(&p.switch_threshold)?,
                    if p.distribute_sessions { "on" } else { "off" },
                    match p.quota_probe_seconds {
                        0 => "off".to_string(),
                        n => format!("{n}s"),
                    },
                    p.hold_seconds,
                );
            }
            eprintln!("* serves requests with no /pool/<name> prefix");
            Ok(())
        }
        PoolCmd::Add { name, set } => {
            let cfg = Config::update(|c| {
                let fresh = !c.pools.contains_key(&name);
                c.ensure_pool(&name)?;
                apply_pool_set(c, &name, &set)?;
                eprintln!("pool \"{name}\" {}", if fresh { "created" } else { "updated" });
                Ok(())
            })?;
            notify_reload(&cfg).await;
            Ok(())
        }
        PoolCmd::Set { name, set } => {
            let cfg = Config::update(|c| {
                let name = pool_name(c, Some(&name))?;
                apply_pool_set(c, &name, &set)?;
                eprintln!("pool \"{name}\" updated");
                Ok(())
            })?;
            notify_reload(&cfg).await;
            Ok(())
        }
        PoolCmd::Rm { name, force } => {
            let cfg = Config::update(|c| {
                let name = pool_name(c, Some(&name))?;
                if name == c.default_pool {
                    bail!("\"{name}\" is the default pool; point defaultPool elsewhere first (`teamclaude pool set <other> --make-default`)");
                }
                let held = c.pool(&name).map(|p| p.accounts.len()).unwrap_or(0);
                if held > 0 && !force {
                    bail!(
                        "pool \"{name}\" still holds {held} account(s); move them with \
                         `teamclaude pool set <other> --account <name>`, or pass --force to delete them with the pool"
                    );
                }
                c.pools.remove(&name);
                eprintln!("pool \"{name}\" removed{}", if held > 0 { format!(" with {held} account(s)") } else { String::new() });
                Ok(())
            })?;
            notify_reload(&cfg).await;
            Ok(())
        }
    }
}

/// Apply `pool add`/`pool set` flags to an existing pool. Accounts are moved in
/// before the knobs are written so a single command can both populate a pool and
/// configure it.
fn apply_pool_set(c: &mut Config, name: &str, set: &PoolSetArgs) -> Result<()> {
    for want in &set.account {
        let (from, i) = locate(c, None, want)?;
        if from == name {
            continue;
        }
        let a = c.pool_mut(&from).expect("locate returns a configured pool").accounts.remove(i);
        eprintln!("moved \"{}\" from pool \"{from}\" to \"{name}\"", a.name);
        c.pool_mut(name).expect("caller created the pool").accounts.push(a);
    }
    let probe = set.probe.as_deref().map(|v| parse_secs(v, 30, "probe interval")).transpose()?;
    let distribute = set.distribute.as_deref().map(parse_on_off).transpose()?;
    let p = c.pool_mut(name).expect("caller created the pool");
    if let Some(t) = &set.threshold {
        set_threshold(&mut p.switch_threshold, t)?;
    }
    if let Some(on) = distribute {
        p.distribute_sessions = on;
    }
    if let Some(secs) = probe {
        p.quota_probe_seconds = secs;
    }
    if let Some(h) = set.hold {
        p.hold_seconds = h;
    }
    if set.make_default {
        c.default_pool = name.to_string();
        eprintln!("defaultPool = \"{name}\"");
    }
    Ok(())
}

fn shell_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\"'\"'"))
}

fn pin_component(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// Shell lines that point Claude Code at the proxy. The proxy key is only put
/// in the environment when the proxy is bound off-loopback (a remote client
/// needs it); on loopback the exemption applies and no secret leaks into the
/// process tree of every tool claude spawns.
///
/// `pool` selects a fleet. In base-URL mode it becomes a `/pool/<name>` path
/// prefix; in MITM mode there is no local URL to carry it, so it rides in the
/// proxy username next to the optional account pin as `[<pin>]~<pool>`. The
/// default pool is never named in either form: an install with one pool emits
/// byte-for-byte the lines it emitted before pools existed.
pub fn env_lines(cfg: &Config, use_mitm: bool, pin: Option<&str>, pool: Option<&str>) -> Vec<String> {
    let port = cfg.proxy.port;
    let mut lines = Vec::new();
    let loopback = crate::security::is_loopback_host(&cfg.bind_host());
    let key = if loopback && !cfg.proxy.require_key_on_loopback { "" } else { cfg.proxy.api_key.as_str() };
    let named = pool.filter(|p| *p != cfg.default_pool);
    if use_mitm {
        let user = match (pin, named) {
            (Some(p), Some(pool)) => format!("{p}{}{pool}", crate::pools::MITM_POOL_SEP),
            (Some(p), None) => p.to_string(),
            (None, Some(pool)) => format!("{}{pool}", crate::pools::MITM_POOL_SEP),
            (None, None) => String::new(),
        };
        let userinfo = if user.is_empty() && key.is_empty() { String::new() } else { format!("{}:{}@", pin_component(&user), pin_component(key)) };
        let url = format!("http://{userinfo}127.0.0.1:{port}");
        for v in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
            lines.push(format!("export {v}={}", shell_quote(&url)));
        }
        lines.push("export NO_PROXY=localhost,127.0.0.1,::1".into());
        lines.push("export no_proxy=localhost,127.0.0.1,::1".into());
        lines.push(format!("export NODE_EXTRA_CA_CERTS={}", shell_quote(&crate::proxy::mitm::ca_cert_path().to_string_lossy())));
        lines.push("unset ANTHROPIC_BASE_URL".into());
    } else {
        // The server strips `/pool/<name>` before it looks for `/tc-acct/`, so
        // the pool keyword comes first.
        let pool_prefix = named.map(|p| format!("{}{p}", crate::pools::POOL_PREFIX)).unwrap_or_default();
        let pin_prefix = pin.map(|p| format!("/tc-acct/{}", pin_component(p))).unwrap_or_default();
        lines.push(format!("export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}{pool_prefix}{pin_prefix}"));
        if !key.is_empty() {
            lines.push("unset ANTHROPIC_AUTH_TOKEN".into());
            lines.push(format!("export ANTHROPIC_API_KEY={}", shell_quote(key)));
        }
    }
    if pin.is_some() {
        lines.push("unset TC_ACCT".into());
    }
    if named.is_some() {
        lines.push("unset TC_POOL".into());
    }
    let hold = pool_of(cfg, pool).map(|p| p.hold_seconds).unwrap_or(0);
    if hold > 0 {
        lines.push(format!("export API_TIMEOUT_MS={}", hold * 1000 + 60_000));
    }
    lines
}

/// The pin and pool a launch-context command should use: the flag if given,
/// then the environment. `TC_POOL` lets a shell wrapper choose a pool the same
/// way `TC_ACCT` already chooses an account.
fn launch_target(cfg: &Config, pool_flag: Option<&str>) -> Result<(Option<String>, Option<String>)> {
    let pin = std::env::var("TC_ACCT").ok().filter(|s| !s.trim().is_empty());
    let pool = match pool_flag {
        Some(p) => Some(pool_name(cfg, Some(p))?),
        None => match std::env::var("TC_POOL").ok().filter(|s| !s.trim().is_empty()) {
            Some(p) => Some(pool_name(cfg, Some(&p)).context("TC_POOL")?),
            None => None,
        },
    };
    if let Some(p) = &pin {
        // A pin only resolves inside the pool serving the request.
        let scope = pool.clone().unwrap_or_else(|| cfg.default_pool.clone());
        if cfg.find_account_idx_in(&scope, p).is_none() {
            bail!("TC_ACCT={p} matches no account in pool \"{scope}\"");
        }
    }
    Ok((pin, pool))
}

pub fn env(args: EnvArgs) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    let (pin, pool) = launch_target(&cfg, args.pool.as_deref())?;
    if !args.no_mitm {
        crate::proxy::mitm::ensure_certs(&["api.anthropic.com".to_string()])?;
    }
    for l in env_lines(&cfg, !args.no_mitm, pin.as_deref(), pool.as_deref()) {
        println!("{l}");
    }
    Ok(())
}

pub async fn run(args: RunArgs) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet; run `teamclaude login` first"))?;
    crate::upstream::init(&cfg)?;
    let up = control_get(&cfg, "/teamclaude/health").await.is_ok();
    let mut cmd = std::process::Command::new("claude");
    cmd.args(&args.args);
    cmd.env_remove("TC_ACCT");
    cmd.env_remove("TC_POOL");
    if !up {
        if args.auto_fallback {
            eprintln!("[TeamClaude] proxy is not running; launching claude directly (no rotation)");
        } else {
            bail!("proxy is not running on port {}; start `teamclaude server` or pass --auto-fallback", cfg.proxy.port);
        }
    } else {
        let (pin, pool) = launch_target(&cfg, args.pool.as_deref())?;
        if !args.no_mitm {
            crate::proxy::mitm::ensure_certs(&["api.anthropic.com".to_string()])?;
        }
        for line in env_lines(&cfg, !args.no_mitm, pin.as_deref(), pool.as_deref()) {
            if let Some(rest) = line.strip_prefix("export ") {
                if let Some((k, v)) = rest.split_once('=') {
                    let v = v.trim_matches('\'').replace("'\"'\"'", "'");
                    cmd.env(k, v);
                }
            } else if let Some(k) = line.strip_prefix("unset ") {
                cmd.env_remove(k);
            }
        }
    }
    let status = cmd.status().context("could not launch `claude`; is it on PATH?")?;
    std::process::exit(status.code().unwrap_or(1));
}

pub fn ca_path() -> Result<()> {
    crate::proxy::mitm::ensure_certs(&["api.anthropic.com".to_string()])?;
    println!("{}", crate::proxy::mitm::ca_cert_path().display());
    Ok(())
}

pub fn config_cmd(cmd: Option<ConfigCmd>) -> Result<()> {
    match cmd.unwrap_or(ConfigCmd::Check) {
        ConfigCmd::Path => {
            println!("{}", crate::config::config_path().display());
            Ok(())
        }
        ConfigCmd::Check => {
            let cfg = Config::load()?.ok_or_else(|| anyhow!("no config at {}", crate::config::config_path().display()))?;
            let mut red = cfg.clone();
            red.proxy.api_key = crate::security::redact(&red.proxy.api_key);
            for k in &mut red.proxy.client_keys {
                k.key = crate::security::redact(&k.key);
            }
            for p in red.pools.values_mut() {
                for a in &mut p.accounts {
                    a.access_token = a.access_token.as_deref().map(crate::security::redact);
                    a.refresh_token = a.refresh_token.as_deref().map(crate::security::redact);
                    a.api_key = a.api_key.as_deref().map(crate::security::redact);
                }
            }
            println!("{}", serde_json::to_string_pretty(&red)?);
            let pools = match cfg.pools.len() {
                0 | 1 => String::new(),
                n => format!(" in {n} pools"),
            };
            eprintln!("config OK: {} account(s){pools}, bind {}:{}", cfg.all_accounts().count(), cfg.bind_host(), cfg.proxy.port);
            Ok(())
        }
    }
}

pub fn service(cmd: ServiceCmd) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("service management is implemented for systemd --user on Linux only");
    }
    let exe = std::env::current_exe()?;
    let unit = format!(
        "[Unit]\nDescription=TeamClaude multi-account Claude proxy\nAfter=network-online.target\n\n[Service]\nExecStart={} server --headless\nRestart=on-failure\nRestartSec=3\nEnvironment=TEAMCLAUDE_CONFIG={}\nNoNewPrivileges=yes\nPrivateTmp=yes\nProtectSystem=strict\nReadWritePaths={}\n\n[Install]\nWantedBy=default.target\n",
        exe.display(),
        crate::config::config_path().display(),
        crate::config::config_dir().display()
    );
    let dir: PathBuf = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("systemd/user");
    let path = dir.join("teamclaude.service");
    match cmd {
        ServiceCmd::Print => {
            print!("{unit}");
            Ok(())
        }
        ServiceCmd::Install => {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&path, unit)?;
            let _ = std::process::Command::new("systemctl").args(["--user", "daemon-reload"]).status();
            let st = std::process::Command::new("systemctl").args(["--user", "enable", "--now", "teamclaude.service"]).status()?;
            if !st.success() {
                bail!("systemctl enable failed");
            }
            eprintln!("installed {}", path.display());
            Ok(())
        }
        ServiceCmd::Uninstall => {
            let _ = std::process::Command::new("systemctl").args(["--user", "disable", "--now", "teamclaude.service"]).status();
            let _ = std::fs::remove_file(&path);
            let _ = std::process::Command::new("systemctl").args(["--user", "daemon-reload"]).status();
            eprintln!("removed {}", path.display());
            Ok(())
        }
        ServiceCmd::Status => {
            let st = std::process::Command::new("systemctl").args(["--user", "status", "teamclaude.service", "--no-pager"]).status()?;
            std::process::exit(st.code().unwrap_or(1));
        }
    }
}

pub async fn api(path: String, account: Option<String>, pool: Option<String>) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    crate::upstream::init(&cfg)?;
    if !path.starts_with('/') {
        bail!("path must start with /");
    }
    let (owner, idx) = match account {
        Some(n) => locate(&cfg, pool.as_deref(), &n)?,
        None => {
            let owner = pool_name(&cfg, pool.as_deref())?;
            let idx = pool_of(&cfg, pool.as_deref())?
                .accounts
                .iter()
                .position(|a| !a.disabled)
                .ok_or_else(|| anyhow!("no enabled account{}", pool_note(pool.as_deref())))?;
            (owner, idx)
        }
    };
    let a = &cfg.pool(&owner).expect("located pool exists").accounts[idx];
    let m = crate::manager::Manager::new(&cfg, &owner);
    let id = a.id.clone().unwrap_or_default();
    let cred = m.ensure_token_fresh(&id, false).await.or_else(|| a.api_key.clone()).ok_or_else(|| anyhow!("no credential for {}", a.name))?;
    let mut req = crate::upstream::client().get(format!("https://api.anthropic.com{path}"));
    req = match a.kind {
        AccountType::Oauth => req.bearer_auth(cred).header("anthropic-beta", oauth::USAGE_BETA),
        AccountType::Apikey => req.header("x-api-key", cred),
    };
    let r = req.send().await?;
    eprintln!("HTTP {}", r.status());
    let text = r.text().await?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        Err(_) => println!("{}", crate::security::safe_text(&text, 20_000)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_lines_keep_key_out_on_loopback() {
        let mut cfg = Config::default();
        cfg.proxy.api_key = "tc-secret-0123456789abcdef".into();
        let lines = env_lines(&cfg, true, None, None).join("\n");
        assert!(!lines.contains("tc-secret"));
        assert!(lines.contains("HTTPS_PROXY='http://127.0.0.1:3456'"));
        let lines = env_lines(&cfg, true, Some("me@example.com (Acme)"), None).join("\n");
        assert!(lines.contains("me%40example%2Ecom%20%28Acme%29:@127.0.0.1"));
        cfg.proxy.host = Some("0.0.0.0".into());
        let lines = env_lines(&cfg, false, None, None).join("\n");
        assert!(lines.contains("ANTHROPIC_API_KEY='tc-secret-0123456789abcdef'"));
    }

    /// Naming the default pool must not change a single byte: an existing
    /// wrapper doing `eval "$(teamclaude env)"` keeps working untouched.
    #[test]
    fn default_pool_is_never_named() {
        let cfg = Config::default();
        for mitm in [true, false] {
            let plain = env_lines(&cfg, mitm, None, None);
            assert_eq!(plain, env_lines(&cfg, mitm, None, Some(&cfg.default_pool)));
        }
    }

    #[test]
    fn named_pool_rides_the_url_then_the_username() {
        let mut cfg = Config::default();
        cfg.ensure_pool("work").unwrap();

        let lines = env_lines(&cfg, false, None, Some("work")).join("\n");
        assert!(lines.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:3456/pool/work"), "{lines}");
        // Pool keyword first, then the account pin — the order the server strips them in.
        let lines = env_lines(&cfg, false, Some("acct1"), Some("work")).join("\n");
        assert!(lines.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:3456/pool/work/tc-acct/acct1"), "{lines}");

        // MITM has no local URL to hang a path on, so the pool rides the
        // proxy username; `~` percent-encodes but decodes back before Basic auth.
        let lines = env_lines(&cfg, true, None, Some("work")).join("\n");
        assert!(lines.contains("http://%7Ework:@127.0.0.1:3456"), "{lines}");
        let lines = env_lines(&cfg, true, Some("acct1"), Some("work")).join("\n");
        assert!(lines.contains("http://acct1%7Ework:@127.0.0.1:3456"), "{lines}");
    }

    /// A pool's own hold floor is what the client timeout has to clear.
    #[test]
    fn hold_seconds_come_from_the_chosen_pool() {
        let mut cfg = Config::default();
        cfg.ensure_pool("work").unwrap();
        cfg.pool_mut("work").unwrap().hold_seconds = 120;
        assert!(!env_lines(&cfg, false, None, None).iter().any(|l| l.contains("API_TIMEOUT_MS")));
        assert!(env_lines(&cfg, false, None, Some("work")).contains(&"export API_TIMEOUT_MS=180000".to_string()));
    }
}
