//! Command-line interface.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use crate::config::{AccountConfig, AccountType, Config, RouteConfig, Threshold};
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
    Switch { name: Option<String> },
    /// Remove an account
    Remove { name: String },
    /// Exclude an account from rotation
    Disable { name: String },
    /// Re-enable an account (also clears a stuck error state)
    Enable { name: String },
    /// Set rotation priority (lower = preferred)
    Priority {
        name: String,
        value: Option<i32>,
        #[arg(long)]
        first: bool,
        #[arg(long)]
        last: bool,
    },
    /// Show or set the switch threshold (percent, or bucket=percent)
    Threshold { value: Option<String> },
    /// Spread new sessions across equal-priority accounts (on|off)
    Distribute { value: Option<String> },
    /// Background quota probe interval in seconds (off|N)
    Probe { value: Option<String> },
    /// Keep idle accounts' 5h windows running (off|N seconds, min 60; spends quota)
    Warmup { value: Option<String> },
    /// Expiry-pressure routing (on|off), with --tolerance and --preempt
    Expiry {
        value: Option<String>,
        #[arg(long)]
        tolerance: Option<f64>,
        #[arg(long)]
        preempt: Option<String>,
    },
    /// Session titles in the activity log (on|off)
    Titles { value: Option<String> },
    /// Per-model routing rules
    Route {
        #[command(subcommand)]
        cmd: Option<RouteCmd>,
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
}

#[derive(Args, Debug)]
pub struct EnvArgs {
    /// Base-URL routing only (no forward proxy / CA)
    #[arg(long)]
    pub no_mitm: bool,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(long)]
    pub no_mitm: bool,
    /// Launch claude directly if the proxy is down
    #[arg(long)]
    pub auto_fallback: bool,
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

fn find_account_mut<'a>(cfg: &'a mut Config, name: &str) -> Result<&'a mut AccountConfig> {
    let idx = cfg.find_account_idx(name).ok_or_else(|| anyhow!("no account matches \"{name}\""))?;
    Ok(&mut cfg.accounts[idx])
}

fn upsert_oauth(cfg: &mut Config, name: &str, tokens: &oauth::Tokens, profile: Option<&oauth::Profile>) -> bool {
    let existing = profile
        .and_then(|p| p.account_uuid.as_deref())
        .and_then(|au| {
            let ou = profile.and_then(|p| p.org_uuid.as_deref());
            cfg.accounts.iter().position(|a| a.account_uuid.as_deref() == Some(au) && (ou.is_none() || a.org_uuid.as_deref() == ou))
        })
        .or_else(|| cfg.accounts.iter().position(|a| a.name == name));
    let entry = match existing {
        Some(i) => &mut cfg.accounts[i],
        None => {
            cfg.accounts.push(AccountConfig { name: name.to_string(), kind: AccountType::Oauth, ..Default::default() });
            cfg.accounts.last_mut().unwrap()
        }
    };
    let updated = existing.is_some();
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
        cfg.accounts.iter().any(|a| a.email.as_deref() == Some(email.as_str()) && a.org_uuid.is_some() && a.org_uuid != profile.org_uuid);
    match (&profile.org_name, same_email_other_org) {
        (Some(org), true) => format!("{email} ({org})"),
        _ => email,
    }
}

// ── commands ──────────────────────────────────────────────────

pub async fn login(args: LoginArgs) -> Result<()> {
    let mut cfg = Config::load_or_create()?;
    crate::upstream::init(&cfg)?;
    if args.api {
        eprint!("Anthropic API key: ");
        let key = read_secret()?;
        if !key.starts_with("sk-ant-") {
            bail!("that does not look like an Anthropic API key");
        }
        let name = args.name.unwrap_or_else(|| format!("api-{}", &key[key.len().saturating_sub(6)..]));
        Config::update(|c| {
            if let Some(a) = c.accounts.iter_mut().find(|a| a.name == name) {
                a.api_key = Some(key.clone());
                a.kind = AccountType::Apikey;
            } else {
                c.accounts.push(AccountConfig {
                    name: name.clone(),
                    kind: AccountType::Apikey,
                    api_key: Some(key.clone()),
                    priority: 10,
                    ..Default::default()
                });
            }
            Ok(())
        })?;
        eprintln!("Added API key account \"{name}\"");
        notify_reload(&cfg).await;
        return Ok(());
    }
    if args.codex {
        let c = crate::codex::login_browser(!args.no_browser).await?;
        let access = c.access_token.clone().ok_or_else(|| anyhow!("OpenAI returned no access token"))?;
        let name = args.name.clone().or(c.email.clone()).unwrap_or_else(|| "codex".into());
        let cfg = Config::update(|cfg| {
            let entry = match cfg.accounts.iter().position(|a| a.is_codex() && (a.account_id == c.account_id && c.account_id.is_some() || a.name == name)) {
                Some(i) => &mut cfg.accounts[i],
                None => {
                    cfg.accounts.push(AccountConfig { name: name.clone(), kind: AccountType::Oauth, provider: Some("codex".into()), ..Default::default() });
                    cfg.accounts.last_mut().unwrap()
                }
            };
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
        eprintln!("Added Codex account \"{name}\"{}", c.plan_type.as_ref().map(|p| format!(" (plan {p})")).unwrap_or_default());
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
        upsert_oauth(c, &name, &tokens, profile.as_ref());
        Ok(())
    })
    .map(|c| {
        cfg = c;
        true
    })?;
    let _ = updated;
    if let Some(p) = &profile {
        eprintln!(
            "Logged in as {} ({}){}",
            name,
            p.org_name.clone().unwrap_or_else(|| "personal".into()),
            p.rate_limit_tier.as_ref().map(|t| format!(", tier {t}")).unwrap_or_default()
        );
    } else {
        eprintln!("Added account \"{name}\"");
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
            let entry = match cfg.accounts.iter().position(|a| a.is_codex() && (a.account_id == c.account_id && c.account_id.is_some() || a.name == name)) {
                Some(i) => &mut cfg.accounts[i],
                None => {
                    cfg.accounts.push(AccountConfig { name: name.clone(), kind: AccountType::Oauth, provider: Some("codex".into()), ..Default::default() });
                    cfg.accounts.last_mut().unwrap()
                }
            };
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
        eprintln!("Imported Codex account \"{name}\"{}", if args.link { " (linked)" } else { "" });
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
        upsert_oauth(c, &name, &tokens, profile.as_ref());
        if args.link {
            let a = c.accounts.iter_mut().find(|a| a.name == name).unwrap();
            a.import_from = Some(args.from.clone());
            a.access_token = None;
            a.refresh_token = None;
            a.expires_at = None;
        }
        if let Some(a) = c.accounts.iter_mut().find(|a| a.name == name) {
            if a.subscription_type.is_none() {
                a.subscription_type = creds.subscription_type.clone();
            }
        }
        Ok(())
    })?;
    eprintln!("Imported \"{name}\"{}", if args.link { " (linked to the credential file)" } else { "" });
    notify_reload(&cfg).await;
    Ok(())
}

pub fn accounts(verbose: bool) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet; run `teamclaude login`"))?;
    if cfg.accounts.is_empty() {
        println!("No accounts. Run `teamclaude login` or `teamclaude import`.");
        return Ok(());
    }
    println!("{:<30} {:<7} {:>4} {:<9} {}", "NAME", "TYPE", "PRI", "STATE", if verbose { "DETAILS" } else { "" });
    for a in &cfg.accounts {
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

pub async fn switch(name: Option<String>) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    crate::upstream::init(&cfg)?;
    match name {
        None => {
            let st = control_get(&cfg, "/teamclaude/status").await?;
            for a in st.get("accounts").and_then(Value::as_array).unwrap_or(&vec![]) {
                let cur = a.get("current").and_then(Value::as_bool).unwrap_or(false);
                println!("{} {}", if cur { "►" } else { " " }, a.get("name").and_then(Value::as_str).unwrap_or("?"));
            }
        }
        Some(n) => {
            let r = control_post(&cfg, "/teamclaude/switch", json!({ "account": n })).await?;
            if r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                println!(
                    "Switched to {}{}",
                    r.get("account").and_then(Value::as_str).unwrap_or("?"),
                    r.get("blocked").and_then(Value::as_str).map(|b| format!(" (note: {b})")).unwrap_or_default()
                );
            } else {
                bail!("{}", r.get("error").and_then(Value::as_str).unwrap_or("switch failed"));
            }
        }
    }
    Ok(())
}

pub async fn remove(name: String) -> Result<()> {
    let cfg = Config::update(|c| {
        let idx = c.find_account_idx(&name).ok_or_else(|| anyhow!("no account matches \"{name}\""))?;
        let removed = c.accounts.remove(idx);
        eprintln!("Removed \"{}\"", removed.name);
        Ok(())
    })?;
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn set_disabled(name: String, disabled: bool) -> Result<()> {
    let cfg = Config::update(|c| {
        let a = find_account_mut(c, &name)?;
        a.disabled = disabled;
        eprintln!("{} \"{}\"", if disabled { "Disabled" } else { "Enabled" }, a.name);
        Ok(())
    })?;
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn priority(name: String, value: Option<i32>, first: bool, last: bool) -> Result<()> {
    let cfg = Config::update(|c| {
        let v = if first {
            c.accounts.iter().map(|a| a.priority).min().unwrap_or(0) - 1
        } else if last {
            c.accounts.iter().map(|a| a.priority).max().unwrap_or(0) + 1
        } else {
            value.ok_or_else(|| anyhow!("give a number, --first or --last"))?
        };
        let a = find_account_mut(c, &name)?;
        a.priority = v;
        eprintln!("Priority of \"{}\" is now {v}", a.name);
        Ok(())
    })?;
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn threshold(value: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("{}", serde_json::to_string_pretty(&c.switch_threshold)?);
            return Ok(());
        }
        Some(v) => Config::update(|c| {
            if let Some((bucket, pct)) = v.split_once('=') {
                let mut table = match &c.switch_threshold {
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
                c.switch_threshold = Threshold::Table(table);
            } else {
                let p: f64 = v.parse().context("percent must be a number")?;
                if !(1.0..=100.0).contains(&p) {
                    bail!("percent must be 1-100");
                }
                c.switch_threshold = Threshold::Single(p / 100.0);
            }
            eprintln!("switchThreshold = {}", serde_json::to_string(&c.switch_threshold)?);
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

pub async fn distribute(value: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("distributeSessions: {}", if c.distribute_sessions { "on" } else { "off" });
            return Ok(());
        }
        Some(v) => {
            let on = parse_on_off(&v)?;
            Config::update(|c| {
                c.distribute_sessions = on;
                eprintln!("distributeSessions: {}", if on { "on" } else { "off" });
                Ok(())
            })?
        }
    };
    notify_reload(&cfg).await;
    Ok(())
}

pub async fn probe(value: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("quotaProbeSeconds: {}", c.quota_probe_seconds);
            return Ok(());
        }
        Some(v) => {
            let secs: u64 = if v.eq_ignore_ascii_case("off") { 0 } else { v.parse().context("seconds must be a number or off")? };
            if secs != 0 && secs < 30 {
                bail!("minimum probe interval is 30 seconds");
            }
            Config::update(|c| {
                c.quota_probe_seconds = secs;
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
            let secs: u64 = if v.eq_ignore_ascii_case("off") { 0 } else { v.parse().context("seconds must be a number or off")? };
            if secs != 0 && secs < 60 {
                bail!("minimum keep-warm interval is 60 seconds");
            }
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

pub async fn expiry(value: Option<String>, tolerance: Option<f64>, preempt: Option<String>) -> Result<()> {
    if value.is_none() && tolerance.is_none() && preempt.is_none() {
        let c = Config::load()?.unwrap_or_default();
        println!("{}", serde_json::to_string_pretty(&c.expiry_routing)?);
        return Ok(());
    }
    let cfg = Config::update(|c| {
        if let Some(v) = &value {
            c.expiry_routing.enabled = parse_on_off(v)?;
        }
        if let Some(t) = tolerance {
            if t.is_nan() || t < 1.0 {
                bail!("tolerance must be >= 1.0");
            }
            c.expiry_routing.tolerance = t;
        }
        if let Some(p) = &preempt {
            c.expiry_routing.preempt = parse_on_off(p)?;
        }
        eprintln!("expiryRouting = {}", serde_json::to_string(&c.expiry_routing)?);
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

pub async fn route(cmd: Option<RouteCmd>) -> Result<()> {
    match cmd.unwrap_or(RouteCmd::List) {
        RouteCmd::List => {
            let c = Config::load()?.unwrap_or_default();
            if c.routes.is_empty() {
                println!("no routes");
            }
            for r in &c.routes {
                println!(
                    "{:<12} match={:?} accounts={:?}{}",
                    r.name,
                    r.patterns,
                    r.accounts,
                    r.bucket.as_ref().map(|b| format!(" bucket={b}")).unwrap_or_default()
                );
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
                for a in &accounts {
                    if c.find_account_idx(a).is_none() {
                        bail!("no account matches \"{a}\"");
                    }
                }
                c.routes.retain(|r| r.name != name);
                c.routes.push(RouteConfig {
                    name: name.clone(),
                    patterns: r#match.clone(),
                    accounts: accounts.clone(),
                    bucket: bucket.clone(),
                    color: color.clone(),
                });
                eprintln!("route \"{name}\" saved");
                Ok(())
            })?;
            notify_reload(&cfg).await;
            Ok(())
        }
        RouteCmd::Rm { name } => {
            let cfg = Config::update(|c| {
                let n = c.routes.len();
                c.routes.retain(|r| r.name != name);
                if c.routes.len() == n {
                    bail!("no route named \"{name}\"");
                }
                eprintln!("route \"{name}\" removed");
                Ok(())
            })?;
            notify_reload(&cfg).await;
            Ok(())
        }
    }
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
pub fn env_lines(cfg: &Config, use_mitm: bool, pin: Option<&str>) -> Vec<String> {
    let port = cfg.proxy.port;
    let mut lines = Vec::new();
    let loopback = crate::security::is_loopback_host(&cfg.bind_host());
    let key = if loopback && !cfg.proxy.require_key_on_loopback { "" } else { cfg.proxy.api_key.as_str() };
    if use_mitm {
        let userinfo = match (pin, key.is_empty()) {
            (Some(p), _) => format!("{}:{}@", pin_component(p), pin_component(key)),
            (None, false) => format!(":{}@", pin_component(key)),
            (None, true) => String::new(),
        };
        let url = format!("http://{userinfo}127.0.0.1:{port}");
        for v in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
            lines.push(format!("export {v}={}", shell_quote(&url)));
        }
        lines.push("export NO_PROXY=localhost,127.0.0.1,::1".into());
        lines.push("export no_proxy=localhost,127.0.0.1,::1".into());
        lines.push(format!("export NODE_EXTRA_CA_CERTS={}", shell_quote(&crate::proxy::mitm::ca_cert_path().to_string_lossy())));
        lines.push("unset ANTHROPIC_BASE_URL".into());
    } else {
        let prefix = pin.map(|p| format!("/tc-acct/{}", pin_component(p))).unwrap_or_default();
        lines.push(format!("export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}{prefix}"));
        if !key.is_empty() {
            lines.push("unset ANTHROPIC_AUTH_TOKEN".into());
            lines.push(format!("export ANTHROPIC_API_KEY={}", shell_quote(key)));
        }
    }
    if pin.is_some() {
        lines.push("unset TC_ACCT".into());
    }
    if cfg.hold_seconds > 0 {
        lines.push(format!("export API_TIMEOUT_MS={}", cfg.hold_seconds * 1000 + 60_000));
    }
    lines
}

pub fn env(args: EnvArgs) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    let pin = std::env::var("TC_ACCT").ok().filter(|s| !s.trim().is_empty());
    if let Some(p) = &pin {
        if cfg.find_account_idx(p).is_none() {
            bail!("TC_ACCT={p} matches no configured account");
        }
    }
    if !args.no_mitm {
        crate::proxy::mitm::ensure_certs(&["api.anthropic.com".to_string()])?;
    }
    for l in env_lines(&cfg, !args.no_mitm, pin.as_deref()) {
        println!("{l}");
    }
    Ok(())
}

pub async fn run(args: RunArgs) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet; run `teamclaude login` first"))?;
    crate::upstream::init(&cfg)?;
    let up = control_get(&cfg, "/teamclaude/health").await.is_ok();
    let pin = std::env::var("TC_ACCT").ok().filter(|s| !s.trim().is_empty());
    let mut cmd = std::process::Command::new("claude");
    cmd.args(&args.args);
    cmd.env_remove("TC_ACCT");
    if !up {
        if args.auto_fallback {
            eprintln!("[TeamClaude] proxy is not running; launching claude directly (no rotation)");
        } else {
            bail!("proxy is not running on port {}; start `teamclaude server` or pass --auto-fallback", cfg.proxy.port);
        }
    } else {
        if let Some(p) = &pin {
            if cfg.find_account_idx(p).is_none() {
                bail!("TC_ACCT={p} matches no configured account");
            }
        }
        if !args.no_mitm {
            crate::proxy::mitm::ensure_certs(&["api.anthropic.com".to_string()])?;
        }
        for line in env_lines(&cfg, !args.no_mitm, pin.as_deref()) {
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
            for a in &mut red.accounts {
                a.access_token = a.access_token.as_deref().map(crate::security::redact);
                a.refresh_token = a.refresh_token.as_deref().map(crate::security::redact);
                a.api_key = a.api_key.as_deref().map(crate::security::redact);
            }
            println!("{}", serde_json::to_string_pretty(&red)?);
            eprintln!("config OK: {} account(s), bind {}:{}", cfg.accounts.len(), cfg.bind_host(), cfg.proxy.port);
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

pub async fn api(path: String, account: Option<String>) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    crate::upstream::init(&cfg)?;
    if !path.starts_with('/') {
        bail!("path must start with /");
    }
    let idx = match account {
        Some(n) => cfg.find_account_idx(&n).ok_or_else(|| anyhow!("no account matches \"{n}\""))?,
        None => cfg.accounts.iter().position(|a| !a.disabled).ok_or_else(|| anyhow!("no enabled account"))?,
    };
    let a = &cfg.accounts[idx];
    let m = crate::manager::Manager::new(&cfg);
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
        let lines = env_lines(&cfg, true, None).join("\n");
        assert!(!lines.contains("tc-secret"));
        assert!(lines.contains("HTTPS_PROXY='http://127.0.0.1:3456'"));
        let lines = env_lines(&cfg, true, Some("me@example.com (Acme)")).join("\n");
        assert!(lines.contains("me%40example%2Ecom%20%28Acme%29:@127.0.0.1"));
        cfg.proxy.host = Some("0.0.0.0".into());
        let lines = env_lines(&cfg, false, None).join("\n");
        assert!(lines.contains("ANTHROPIC_API_KEY='tc-secret-0123456789abcdef'"));
    }
}
