//! Command-line interface.

use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use crate::config::{AccountConfig, AccountType, Config, PoolConfig, RouteConfig, Threshold};
use crate::oauth;

#[derive(Parser, Debug)]
#[command(name = "corrall", version, about = "Multi-account Claude proxy with automatic quota-based rotation", long_about = None)]
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
    /// Import accounts from a claudeacrobat install
    ImportClaudeacrobat(ImportAcrobatArgs),
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
        /// Also list the sessions served in the last hour: which, on what account and model, how busy and how well cached
        #[arg(long)]
        sessions: bool,
    },
    /// Show the TUI for a server that is already running (its status and
    /// activity over the control API)
    Attach(AttachArgs),
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
    /// Spread new sessions across equal-priority accounts (on|off), or name
    /// what to balance on, which also turns it on: `sessions` (fewest sessions,
    /// the default) or `usage` (most quota left)
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
    /// Spend the soonest-ending week first, then the soonest 5h window (on|off)
    SoonestReset {
        value: Option<String>,
        #[arg(long)]
        pool: Option<String>,
    },
    /// Name sessions from Claude Code's own files, in the sessions table and the activity log (on|off)
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
    /// Set a client up to use the proxy by editing its own settings
    Setup {
        #[command(subcommand)]
        cmd: SetupCmd,
    },
    /// Print the path of the MITM CA certificate
    CaPath,
    /// Validate the config file and print a redacted summary
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
    },
    /// Manage the user service (systemd --user on Linux, a launchd agent on macOS)
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// The GNOME Shell panel indicator: quota and sessions in the top bar
    Gnome {
        #[command(subcommand)]
        cmd: GnomeCmd,
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
    /// Bind this address for this run instead of proxy.host/proxy.port
    /// (HOST:PORT, :PORT or PORT)
    #[arg(long, value_name = "ADDR", conflicts_with = "port")]
    pub listen: Option<String>,
    /// Bind this port for this run instead of proxy.port
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,
}

#[derive(Args, Debug, Default)]
pub struct AttachArgs {
    /// Control URL of the server to watch (default: the configured proxy.host/port)
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
    /// Proxy key to present (default: proxy.apiKey from the config)
    #[arg(long, value_name = "KEY")]
    pub key: Option<String>,
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
    /// Pool to put the account in (created if new; moves an existing account).
    /// Without it an existing account stays in its pool and a new one goes to the default pool
    #[arg(long)]
    pub pool: Option<String>,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Credentials path (default ~/.claude/.credentials.json)
    #[arg(long, default_value = oauth::DEFAULT_CREDENTIALS_PATH)]
    pub from: String,
    /// Read tokens from the file instead of copying them; the CLI that owns the
    /// file keeps refreshing them (default for the CLI's own credentials file)
    #[arg(long, conflicts_with = "copy")]
    pub link: bool,
    /// Copy the tokens and let the proxy refresh them (default for any other
    /// path). Stop using that login anywhere else afterwards: a refresh here
    /// rotates its refresh token and logs the other copy out
    #[arg(long)]
    pub copy: bool,
    /// Import a Codex CLI login (default path ~/.codex/auth.json)
    #[arg(long)]
    pub codex: bool,
    #[arg(long)]
    pub name: Option<String>,
    /// Pool to put the account in (created if new; moves an existing account).
    /// Without it an existing account stays in its pool and a new one goes to the default pool
    #[arg(long)]
    pub pool: Option<String>,
}

#[derive(Args, Debug)]
pub struct ImportAcrobatArgs {
    /// claudeacrobat state directory (default: its config's state_dir, else
    /// ~/.local/state/claudeacrobat)
    #[arg(long, value_name = "DIR")]
    pub from: Option<String>,
    /// Import every account into this pool instead of the one it came from
    #[arg(long)]
    pub pool: Option<String>,
    /// Show what would be imported without writing anything
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct EnvArgs {
    /// Base-URL routing only (no forward proxy / CA)
    #[arg(long)]
    pub no_mitm: bool,
    /// Hand Claude Code the proxy key as ANTHROPIC_API_KEY instead of a header of its own. Claude Code then runs on the key rather than its claude.ai login: no login needed, claude.ai connectors off, smaller requests
    #[arg(long)]
    pub api_key: bool,
    /// Route through this pool (default: match the launch context, else the default pool)
    #[arg(long)]
    pub pool: Option<String>,
    /// Match pool rules against this directory instead of the current one
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<String>,
    /// Print fish syntax (`set -gx`), for `corrall env --fish | source`
    #[arg(long)]
    pub fish: bool,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(long)]
    pub no_mitm: bool,
    /// Hand Claude Code the proxy key as ANTHROPIC_API_KEY instead of a header of its own (claude.ai login and connectors off)
    #[arg(long)]
    pub api_key: bool,
    /// Launch claude directly if the proxy is down
    #[arg(long)]
    pub auto_fallback: bool,
    /// Route through this pool (default: match the launch context, else the default pool)
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
    /// Spread new sessions across equal-priority accounts (on|off), or name
    /// what to balance on, which also turns it on: sessions|usage
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
    /// Auto-select this pool when the launch directory is DIR or below it (repeatable)
    #[arg(long, value_name = "DIR")]
    pub match_path: Vec<String>,
    /// Auto-select this pool when the git remote matches REGEX (repeatable)
    #[arg(long, value_name = "REGEX")]
    pub match_remote: Vec<String>,
    /// Auto-select this pool when VAR matches REGEX, or is merely set if REGEX is omitted (repeatable)
    #[arg(long, value_name = "VAR[=REGEX]")]
    pub match_env: Vec<String>,
    /// Drop this pool's auto-selection rules
    #[arg(long, conflicts_with_all = ["match_path", "match_remote", "match_env"])]
    pub no_match: bool,
    /// Serve this pool's requests from POOL while all its accounts are out of quota
    #[arg(long, value_name = "POOL")]
    pub fallback: Option<String>,
    /// Stop falling back to another pool
    #[arg(long, conflicts_with = "fallback")]
    pub no_fallback: bool,
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
pub enum SetupCmd {
    /// Write the proxy into Claude Code's user settings (~/.claude/settings.json), replacing any other gateway, forward proxy, cloud provider or key helper set there
    Claude(SetupClaudeArgs),
}

#[derive(Args, Debug)]
pub struct SetupClaudeArgs {
    /// Take the proxy back out of the settings file
    #[arg(long, conflicts_with_all = ["pool", "api_key"])]
    pub remove: bool,
    /// Route through this pool (default: the default pool)
    #[arg(long)]
    pub pool: Option<String>,
    /// Hand Claude Code the proxy key as ANTHROPIC_API_KEY instead of a header of its own (claude.ai login and connectors off). The key is then stored in the settings file
    #[arg(long)]
    pub api_key: bool,
    /// Settings file to edit (default: $CLAUDE_CONFIG_DIR/settings.json, else ~/.claude/settings.json)
    #[arg(long, value_name = "FILE")]
    pub settings: Option<PathBuf>,
    /// Show what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
    /// macOS: leave the launchd service alone. Without this, setup installs
    /// and starts it when the proxy is not already running
    #[arg(long, conflicts_with = "remove")]
    pub no_service: bool,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    Check,
    Path,
}

#[derive(Subcommand, Debug)]
pub enum GnomeCmd {
    /// Write the extension into ~/.local/share/gnome-shell/extensions and enable it
    Install,
    /// Disable it and remove its directory
    Uninstall,
}

#[derive(Subcommand, Debug)]
pub enum ServiceCmd {
    /// Print the unit or plist that `install` would write
    Print,
    /// Write it for this binary and config, then start the service (and at every login)
    Install,
    /// Stop the service and remove its file
    Uninstall,
    /// Ask the service manager how it is doing
    Status,
}

// ── helpers ───────────────────────────────────────────────────

pub fn proxy_base(cfg: &Config) -> String {
    format!("http://{}", cfg.dial_authority())
}

async fn control_get(cfg: &Config, path: &str) -> Result<Value> {
    let r = crate::upstream::client()
        .get(format!("{}{}", proxy_base(cfg), path))
        .header("x-api-key", &cfg.proxy.api_key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|_| anyhow!("proxy is not running on port {} (start it with `corrall server`)", cfg.proxy.port))?;
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
    let _ = control_post(cfg, "/corrall/reload", json!({})).await;
}

/// Resolve a `--pool` flag to a configured pool name, defaulting to the pool
/// that serves unprefixed requests. An unknown name is an error rather than a
/// silent fallback: on the CLI a typo should not quietly retarget the command.
fn pool_name(cfg: &Config, pool: Option<&str>) -> Result<String> {
    match pool {
        None => Ok(cfg.default_pool.clone()),
        Some(p) if cfg.pools.contains_key(p) => Ok(p.to_string()),
        Some(p) => bail!("no pool named \"{p}\"; `corrall pool list` shows the configured pools"),
    }
}

/// The pool an account-adding command should write to. Unlike [`pool_name`],
/// `--pool` may name a pool that does not exist yet: `login --pool work` is how
/// you create one. Without `--pool`, an account corrall already has (`found`)
/// stays in the pool it is in — a re-login only refreshes the credential — and
/// only a new account lands in the default pool.
pub(crate) fn target_pool(cfg: &mut Config, pool: Option<&str>, found: Option<&(String, usize)>) -> Result<String> {
    let Some(p) = pool else { return Ok(found.map_or_else(|| cfg.default_pool.clone(), |(p, _)| p.clone())) };
    let fresh = !cfg.pools.contains_key(p);
    cfg.ensure_pool(p)?;
    if fresh {
        eprintln!("Created pool \"{p}\"");
    }
    Ok(p.to_string())
}

/// Apply a `--listen`/`--port` override to a freshly loaded config.
///
/// This lives apart from the server startup because the reload path needs it
/// too: a config re-read from disk carries the file's port, and without
/// re-applying the override a reload would move the port the warmer dials and
/// the status page reports away from the one actually bound.
pub fn apply_listen_override(cfg: &mut Config, listen: Option<&str>, port: Option<u16>) -> Result<()> {
    if let Some(addr) = listen {
        let (host, port) = parse_listen(addr)?;
        if let Some(h) = host {
            cfg.proxy.host = Some(h);
        }
        cfg.proxy.port = port;
    }
    if let Some(port) = port {
        cfg.proxy.port = port;
    }
    // An override can open the port to the network as surely as the config can,
    // so it faces the same check.
    cfg.validate()
}

/// Parse `--listen`: `HOST:PORT`, `:PORT` (keep the configured host) or a bare
/// `PORT`. An IPv6 host must be bracketed — `[::1]:3456` — because an
/// unbracketed one cannot be told apart from a host with a port.
pub fn parse_listen(addr: &str) -> Result<(Option<String>, u16)> {
    let s = addr.trim();
    if s.is_empty() {
        bail!("--listen needs an address: HOST:PORT, :PORT or PORT");
    }
    let (host, port) = match s.parse::<u16>() {
        Ok(port) => (None, port),
        Err(_) => {
            let Some((h, p)) = s.rsplit_once(':') else {
                bail!("--listen {addr:?} is not HOST:PORT, :PORT or PORT");
            };
            let h = h.trim();
            if h.contains(':') && !(h.starts_with('[') && h.ends_with(']')) {
                bail!("--listen {addr:?}: bracket an IPv6 host, as in [::1]:{}", p.trim());
            }
            let port: u16 = p.trim().parse().map_err(|_| anyhow!("--listen {addr:?}: {:?} is not a port", p.trim()))?;
            ((!h.is_empty()).then(|| h.to_string()), port)
        }
    };
    if port == 0 {
        bail!("--listen {addr:?}: port 0 would bind an arbitrary port that nothing else could find");
    }
    if let Some(h) = &host {
        if crate::proxy::server::parse_bind(h, port).is_err() {
            bail!("--listen {addr:?}: {h:?} is not an IP address (or localhost)");
        }
    }
    Ok((host, port))
}

/// `" in pool \"x\""`, or nothing when the command did not name a pool.
fn pool_note(pool: Option<&str>) -> String {
    pool.map(|p| format!(" in pool \"{p}\"")).unwrap_or_default()
}

/// `" in pool \"x\""` for where an account ended up, or nothing for the default pool.
fn landed_note(cfg: &Config, pool: &str) -> String {
    if pool == cfg.default_pool {
        String::new()
    } else {
        format!(" in pool \"{pool}\"")
    }
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
/// `corrall disable <name>` working exactly as it did before pools existed.
fn locate(cfg: &Config, pool: Option<&str>, needle: &str) -> Result<(String, usize)> {
    // An ambiguous handle is its own error here (naming the candidates), so
    // the "no account matches" below is only ever the truth.
    let found = match pool {
        Some(p) => {
            let p = pool_name(cfg, Some(p))?;
            cfg.find_account_idx_in(&p, needle)?.map(|i| (p, i))
        }
        None => cfg.find_account(needle)?,
    };
    found.ok_or_else(|| match pool {
        Some(p) => anyhow!("no account in pool \"{p}\" matches \"{needle}\""),
        None => anyhow!("no account matches \"{needle}\""),
    })
}

pub(crate) fn find_account_mut<'a>(cfg: &'a mut Config, pool: Option<&str>, name: &str) -> Result<&'a mut AccountConfig> {
    let (p, i) = locate(cfg, pool, name)?;
    Ok(&mut cfg.pool_mut(&p).expect("locate returns a configured pool").accounts[i])
}

/// The first account in the file matching `pred`, with the pool holding it.
/// Pools are searched in display order, so the default pool wins a tie.
pub(crate) fn locate_by(cfg: &Config, mut pred: impl FnMut(&AccountConfig) -> bool) -> Option<(String, usize)> {
    cfg.pool_names().into_iter().find_map(|p| cfg.pool(&p).and_then(|pc| pc.accounts.iter().position(&mut pred)).map(|i| (p, i)))
}

/// The account at `found`, guaranteed to live in `pool` — moved there if it was
/// somewhere else, created from `new` if `found` is `None`. Moving rather than
/// copying matters: two pools rotating the same credential would double-spend
/// one account's quota without either fleet knowing.
///
/// `pool` must already be a valid name — callers go through
/// [`Config::ensure_pool`], which is what checks the charset.
pub(crate) fn entry_at<'a>(
    cfg: &'a mut Config,
    pool: &str,
    found: Option<(String, usize)>,
    new: impl FnOnce() -> AccountConfig,
) -> (&'a mut AccountConfig, bool) {
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

/// Add or refresh an OAuth account in `pool`; true when it already existed.
pub(crate) fn upsert_oauth(cfg: &mut Config, pool: &str, name: &str, tokens: &oauth::Tokens, profile: Option<&oauth::Profile>) -> bool {
    upsert_oauth_at(cfg, pool, name, tokens, profile).1
}

/// Where an OAuth login already lives, if corrall has it. Identify by uuid
/// when the profile gave us one, and only fall back to the display name — an
/// account first added with `--name` (no profile) has no uuid to match on yet.
pub(crate) fn locate_oauth(cfg: &Config, name: &str, profile: Option<&oauth::Profile>) -> Option<(String, usize)> {
    profile
        .and_then(|p| p.account_uuid.as_deref())
        .and_then(|au| {
            let ou = profile.and_then(|p| p.org_uuid.as_deref());
            locate_by(cfg, |a| a.account_uuid.as_deref() == Some(au) && (ou.is_none() || a.org_uuid.as_deref() == ou))
        })
        .or_else(|| locate_by(cfg, |a| a.name == name))
}

/// [`upsert_oauth`] that also returns the index the account now has in
/// `pool`. Callers that need the entry afterwards must use the index: a fuzzy
/// re-lookup by name can land on a sibling sharing the e-mail (the same user
/// in another org, say) and edit that one instead.
pub(crate) fn upsert_oauth_at(cfg: &mut Config, pool: &str, name: &str, tokens: &oauth::Tokens, profile: Option<&oauth::Profile>) -> (usize, bool) {
    let found = locate_oauth(cfg, name, profile);
    // `entry_at` edits in place when the account is already in `pool` and
    // appends otherwise (moved in from another pool, or brand new).
    let idx = match &found {
        Some((from, i)) if from == pool => *i,
        _ => cfg.pool(pool).map_or(0, |p| p.accounts.len()),
    };
    let (entry, updated) = entry_at(cfg, pool, found, || AccountConfig { name: name.to_string(), kind: AccountType::Oauth, ..Default::default() });
    entry.name = name.to_string();
    entry.kind = AccountType::Oauth;
    entry.import_from = None;
    entry.access_token = Some(tokens.access_token.clone());
    entry.refresh_token = tokens.refresh_token.clone();
    entry.expires_at = Some(tokens.expires_at);
    entry.refresh_token_expires_at = tokens.refresh_expires_at;
    if let Some(p) = profile {
        entry.account_uuid = p.account_uuid.clone().or(entry.account_uuid.take());
        entry.org_uuid = p.org_uuid.clone().or(entry.org_uuid.take());
        entry.org_name = p.org_name.clone().or(entry.org_name.take());
        entry.email = p.email.clone().or(entry.email.take());
        entry.rate_limit_tier = p.rate_limit_tier.clone().or(entry.rate_limit_tier.take());
        entry.seat_tier = p.seat_tier.clone().or(entry.seat_tier.take());
    }
    (idx, updated)
}

/// The write `corrall import` makes: upsert the account, then apply `--link`
/// and the credential file's subscription type to the entry the upsert
/// touched — and only that one. Returns the entry's index in `pool`.
pub(crate) fn apply_import(
    cfg: &mut Config,
    pool: &str,
    name: &str,
    tokens: &oauth::Tokens,
    profile: Option<&oauth::Profile>,
    link_from: Option<&str>,
    subscription_type: Option<&str>,
) -> usize {
    let (i, _) = upsert_oauth_at(cfg, pool, name, tokens, profile);
    let a = &mut cfg.pool_mut(pool).expect("upsert_oauth wrote into this pool").accounts[i];
    if let Some(from) = link_from {
        a.import_from = Some(from.to_string());
        a.access_token = None;
        a.refresh_token = None;
        a.expires_at = None;
        a.refresh_token_expires_at = None;
    }
    if a.subscription_type.is_none() {
        a.subscription_type = subscription_type.map(str::to_string);
    }
    i
}

pub(crate) fn display_name(profile: &oauth::Profile, cfg: &Config) -> String {
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
    let cfg = Config::load_or_create()?;
    crate::upstream::init(&cfg)?;
    let want_pool = args.pool.clone();
    if args.api {
        eprint!("Anthropic API key: ");
        let key = read_secret()?;
        if !key.starts_with("sk-ant-") {
            bail!("that does not look like an Anthropic API key");
        }
        let name = args.name.unwrap_or_else(|| format!("api-{}", &key[key.len().saturating_sub(6)..]));
        let mut landed = String::new();
        let cfg = Config::update(|c| {
            let found = locate_by(c, |a| a.name == name);
            let pool = target_pool(c, want_pool.as_deref(), found.as_ref())?;
            landed = pool.clone();
            let (entry, _) = entry_at(c, &pool, found, || AccountConfig { name: name.clone(), kind: AccountType::Apikey, priority: 10, ..Default::default() });
            entry.kind = AccountType::Apikey;
            entry.api_key = Some(key.clone());
            Ok(())
        })?;
        eprintln!("Added API key account \"{name}\"{}", landed_note(&cfg, &landed));
        notify_reload(&cfg).await;
        return Ok(());
    }
    if args.codex {
        let c = crate::codex::login_browser(!args.no_browser).await?;
        let access = c.access_token.clone().ok_or_else(|| anyhow!("OpenAI returned no access token"))?;
        let name = args.name.clone().or(c.email.clone()).unwrap_or_else(|| "codex".into());
        let mut landed = String::new();
        let cfg = Config::update(|cfg| {
            let found = locate_by(cfg, |a| a.is_codex() && (a.account_id == c.account_id && c.account_id.is_some() || a.name == name));
            let pool = target_pool(cfg, want_pool.as_deref(), found.as_ref())?;
            landed = pool.clone();
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
        eprintln!("Added Codex account \"{name}\"{}{}", c.plan_type.as_ref().map(|p| format!(" (plan {p})")).unwrap_or_default(), landed_note(&cfg, &landed));
        notify_reload(&cfg).await;
        return Ok(());
    }
    let tokens = if args.token { oauth::login_paste().await? } else { oauth::login_browser(!args.no_browser).await? };
    let (cfg, lines) = save_login(&tokens, args.name.as_deref(), want_pool.as_deref()).await?;
    for l in lines {
        eprintln!("{l}");
    }
    notify_reload(&cfg).await;
    Ok(())
}

/// Save a finished OAuth login as an account: a new one, or the account it
/// already is, logged in again. Returns the config as written and what to
/// tell the user; prints nothing, so the dashboard can call it too.
pub async fn save_login(tokens: &oauth::Tokens, name: Option<&str>, pool: Option<&str>) -> Result<(Config, Vec<String>)> {
    let mut lines = Vec::new();
    let profile = match oauth::fetch_profile(&tokens.access_token).await {
        Ok(p) => Some(p),
        Err(e) if name.is_some() => {
            lines.push(format!("Profile lookup failed ({e}); continuing with --name"));
            None
        }
        Err(e) => bail!("profile lookup failed: {e}. Retry, or pass --name to add the account without profile detection"),
    };
    let cfg = Config::load_or_create()?;
    let name = name.map(str::to_string).unwrap_or_else(|| display_name(profile.as_ref().unwrap(), &cfg));
    let mut landed = String::new();
    let cfg = Config::update(|c| {
        let found = locate_oauth(c, &name, profile.as_ref());
        let pool = target_pool(c, pool, found.as_ref())?;
        upsert_oauth(c, &pool, &name, tokens, profile.as_ref());
        landed = pool;
        Ok(())
    })?;
    lines.push(match &profile {
        Some(p) => format!(
            "Logged in as {} ({}){}{}",
            name,
            p.org_name.clone().unwrap_or_else(|| "personal".into()),
            p.rate_limit_tier.as_ref().map(|t| format!(", tier {t}")).unwrap_or_default(),
            landed_note(&cfg, &landed)
        ),
        None => format!("Added account \"{name}\"{}", landed_note(&cfg, &landed)),
    });
    Ok((cfg, lines))
}

fn read_secret() -> Result<String> {
    use std::io::BufRead;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

/// Whether `import` links the file rather than copying it. `--link`/`--copy`
/// decide when given; otherwise the CLI's own credentials file is linked, since
/// that CLI keeps refreshing the login and a copy refreshed here would race it
/// for one refresh token, and any other path is copied.
fn import_links(args: &ImportArgs, from: &str, own_file: &str) -> bool {
    !args.copy && (args.link || oauth::expand_home(from) == oauth::expand_home(own_file))
}

/// What a linked or copied import means for the login it came from.
fn import_note(link: bool, from: &str, owner: &str) -> String {
    if link {
        format!(
            "Linked to {from}: {owner} keeps refreshing this login and Corrall reads the file. \
             It goes stale while {owner} is not used here; `corrall login` adds a login Corrall refreshes itself."
        )
    } else {
        format!(
            "Copied from {from}: Corrall now refreshes this login. Stop using {from} with {owner}, \
             or each refresh logs the other one out; `--link` shares it instead."
        )
    }
}

pub async fn import(args: ImportArgs) -> Result<()> {
    let cfg = Config::load_or_create()?;
    crate::upstream::init(&cfg)?;
    if args.codex {
        let from = if args.from == oauth::DEFAULT_CREDENTIALS_PATH { crate::codex::DEFAULT_CREDENTIALS_PATH.to_string() } else { args.from.clone() };
        let link = import_links(&args, &from, crate::codex::DEFAULT_CREDENTIALS_PATH);
        let c = crate::codex::import_credentials(&from)?;
        let access = c.access_token.clone().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("{from} carries no access token"))?;
        let name = args.name.clone().or(c.email.clone()).unwrap_or_else(|| "codex".into());
        let mut landed = String::new();
        let cfg = Config::update(|cfg| {
            let found = locate_by(cfg, |a| a.is_codex() && (a.account_id == c.account_id && c.account_id.is_some() || a.name == name));
            let pool = target_pool(cfg, args.pool.as_deref(), found.as_ref())?;
            landed = pool.clone();
            let (entry, _) = entry_at(cfg, &pool, found, || AccountConfig {
                name: name.clone(),
                kind: AccountType::Oauth,
                provider: Some("codex".into()),
                ..Default::default()
            });
            entry.name = name.clone();
            if link {
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
        eprintln!("Imported Codex account \"{name}\"{}", landed_note(&cfg, &landed));
        eprintln!("{}", import_note(link, &from, "the Codex CLI"));
        notify_reload(&cfg).await;
        return Ok(());
    }
    let link = import_links(&args, &args.from, oauth::DEFAULT_CREDENTIALS_PATH);
    let (cfg, lines) = import_login(&args.from, link, args.name.as_deref(), args.pool.as_deref()).await?;
    for l in lines {
        eprintln!("{l}");
    }
    notify_reload(&cfg).await;
    Ok(())
}

/// Import the Claude Code login in the credentials file `from`: linked (the
/// file stays the source and Claude Code keeps refreshing it) or copied.
/// Returns the config as written and what to tell the user; prints nothing,
/// so the dashboard can call it too.
pub async fn import_login(from: &str, link: bool, name: Option<&str>, pool: Option<&str>) -> Result<(Config, Vec<String>)> {
    let mut lines = Vec::new();
    let cfg = Config::load_or_create()?;
    let creds = oauth::import_credentials(from)?;
    let access = creds.access_token.clone().filter(|t| !t.is_empty()).ok_or_else(|| anyhow!("{from} carries no access token"))?;
    let profile = match oauth::fetch_profile(&access).await {
        Ok(p) => Some(p),
        Err(e) if name.is_some() => {
            lines.push(format!("Profile lookup failed ({e}); continuing with --name"));
            None
        }
        Err(e) => bail!("profile lookup failed: {e}. Pass --name to import without profile detection"),
    };
    let name = name.map(str::to_string).unwrap_or_else(|| display_name(profile.as_ref().unwrap(), &cfg));
    let tokens = oauth::Tokens {
        access_token: access,
        refresh_token: creds.refresh_token.clone(),
        expires_at: creds.expires_at.unwrap_or(0),
        refresh_expires_at: creds.refresh_token_expires_at,
    };
    let mut landed = String::new();
    let cfg = Config::update(|c| {
        let found = locate_oauth(c, &name, profile.as_ref());
        let pool = target_pool(c, pool, found.as_ref())?;
        landed = pool.clone();
        apply_import(c, &pool, &name, &tokens, profile.as_ref(), link.then_some(from), creds.subscription_type.as_deref());
        Ok(())
    })?;
    lines.push(format!("Imported \"{name}\"{}", landed_note(&cfg, &landed)));
    lines.push(import_note(link, from, "Claude Code"));
    Ok((cfg, lines))
}

/// Take an account out of its pool. `account` is anything that names it.
/// Returns the config as written, the account's name and its pool.
pub fn remove_account(pool: Option<&str>, account: &str) -> Result<(Config, String, String)> {
    let (mut name, mut owner) = (String::new(), String::new());
    let cfg = Config::update(|c| {
        let (p, i) = locate(c, pool, account)?;
        name = c.pool_mut(&p).expect("locate returns a configured pool").accounts.remove(i).name;
        owner = p;
        Ok(())
    })?;
    Ok((cfg, name, owner))
}

/// One account, mapped and with its destination pool resolved.
struct AcrobatRow {
    pool: String,
    kind: &'static str,
    account: AccountConfig,
}

pub async fn import_claudeacrobat(args: ImportAcrobatArgs) -> Result<()> {
    let cfg = Config::load_or_create()?;
    crate::upstream::init(&cfg)?;
    let dir = args.from.as_deref().map(oauth::expand_home).unwrap_or_else(crate::migrate::default_state_dir);
    let plan = crate::migrate::plan(&dir)?;

    // Without --pool, claudeacrobat's pool layout carries over as it stands:
    // two fleets it kept apart stay apart here, because merging them would have
    // both rotations spending one account's quota without either knowing.
    let mut rows: Vec<AcrobatRow> = Vec::new();
    let mut skipped: Vec<(String, String)> = plan.skipped.iter().map(|s| (s.name.clone(), s.reason.clone())).collect();
    for m in &plan.accounts {
        let pool = args.pool.clone().or_else(|| m.pool.clone()).unwrap_or_else(|| cfg.default_pool.clone());
        // An API-key or Codex account of the same name is a different thing
        // wearing the same label; overwriting its credential would be a
        // surprise, so leave it alone and say so.
        if let Some((p, i)) = locate_by(&cfg, |a| a.name == m.account.name) {
            let existing = &cfg.pool(&p).expect("locate_by returns a configured pool").accounts[i];
            if !is_anthropic_oauth(existing) {
                let what = if existing.is_codex() { "a Codex account" } else { "an API-key account" };
                skipped.push((m.account.name.clone(), format!("corrall already has {what} named that in pool \"{p}\"")));
                continue;
            }
        }
        rows.push(AcrobatRow { pool, kind: m.kind(), account: m.account.clone() });
    }

    println!("claudeacrobat state: {}", dir.display());
    println!("{} account(s):", rows.len());
    for r in &rows {
        let disabled = if r.account.disabled { " (disabled)" } else { "" };
        println!("  {:<28} {:<6} priority {} → pool \"{}\"{disabled}", r.account.name, r.kind, r.account.priority, r.pool);
    }
    for (name, reason) in &skipped {
        println!("  skip {name:<28} {reason}");
    }
    if rows.is_empty() {
        println!("\nNothing to import.");
        return Ok(());
    }
    if args.dry_run {
        println!("\n(dry run — nothing written)");
        return Ok(());
    }

    let mut added = 0usize;
    let mut updated = 0usize;
    let cfg = Config::update(|c| {
        for r in &rows {
            let pool = target_pool(c, Some(&r.pool), None)?;
            if upsert_acrobat(c, &pool, &r.account) {
                updated += 1;
            } else {
                added += 1;
            }
        }
        c.ensure_account_ids();
        // The imported accounts have to satisfy the same rules as a hand-written
        // config; a failure here aborts the whole write rather than saving a
        // file corrall would refuse to load.
        c.validate()
    })?;

    let pools: Vec<&str> = {
        let mut v: Vec<&str> = rows.iter().map(|r| r.pool.as_str()).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    println!("\nImported {} account(s) into pool{} {}.", added + updated, if pools.len() == 1 { "" } else { "s" }, quoted_list(&pools));
    if updated > 0 {
        println!("{updated} of them updated an account corrall already had.");
    }
    println!("`corrall accounts` lists them; `eval \"$(corrall env)\"` points Claude Code at this proxy.");
    for p in pools.iter().filter(|p| **p != cfg.default_pool) {
        println!("Pool \"{p}\" serves `/pool/{p}`: `eval \"$(corrall env --pool {p})\"`.");
    }
    println!("claudeacrobat's own files are untouched, so it keeps working on its own port.");
    notify_reload(&cfg).await;
    Ok(())
}

/// Whether an existing entry is the kind of account an import can refresh in
/// place: an Anthropic OAuth account, not an API key and not Codex.
fn is_anthropic_oauth(a: &AccountConfig) -> bool {
    a.kind == AccountType::Oauth && !a.is_codex()
}

/// Write one imported account into `pool`, returning whether it replaced an
/// account corrall already had. Identity comes from the account uuid when
/// claudeacrobat recorded one, and the name otherwise.
///
/// Only the fields the import actually carries are assigned: an account that
/// already had corrall-only settings — a route, a model map, its own upstream
/// — keeps them, and only its credential and profile are refreshed.
fn upsert_acrobat(cfg: &mut Config, pool: &str, src: &AccountConfig) -> bool {
    let found = src
        .account_uuid
        .as_deref()
        .and_then(|au| {
            let ou = src.org_uuid.as_deref();
            locate_by(cfg, |a| is_anthropic_oauth(a) && a.account_uuid.as_deref() == Some(au) && (ou.is_none() || a.org_uuid.as_deref() == ou))
        })
        .or_else(|| locate_by(cfg, |a| is_anthropic_oauth(a) && a.name == src.name));
    let (entry, updated) = entry_at(cfg, pool, found, || src.clone());
    entry.name = src.name.clone();
    entry.kind = AccountType::Oauth;
    entry.priority = src.priority;
    entry.disabled = src.disabled;
    entry.import_from = src.import_from.clone();
    entry.access_token = src.access_token.clone();
    entry.refresh_token = src.refresh_token.clone();
    entry.expires_at = src.expires_at;
    entry.account_uuid = src.account_uuid.clone().or(entry.account_uuid.take());
    entry.org_uuid = src.org_uuid.clone().or(entry.org_uuid.take());
    entry.org_name = src.org_name.clone().or(entry.org_name.take());
    entry.email = src.email.clone().or(entry.email.take());
    entry.subscription_type = src.subscription_type.clone().or(entry.subscription_type.take());
    entry.rate_limit_tier = src.rate_limit_tier.clone().or(entry.rate_limit_tier.take());
    updated
}

/// `"a"`, `"a" and "b"`, `"a", "b" and "c"`.
fn quoted_list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

pub fn accounts(verbose: bool, pool: Option<String>) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet; run `corrall login`"))?;
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
            None => println!("No accounts. Run `corrall login` or `corrall import`."),
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
            if let Some(e) = a.refresh_token_expires_at {
                let left = (e - crate::quota::now_ms()) / 1000;
                details
                    .push_str(&format!("login {} ", if left > 0 { format!("expires in {}", crate::status::countdown(Some(left))) } else { "expired".into() }));
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

pub async fn status(json_out: bool, color: &str, sessions: bool) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    crate::upstream::init(&cfg)?;
    let mut st = control_get(&cfg, "/corrall/status").await?;
    let listed = match sessions {
        true => Some(
            control_get(&cfg, "/corrall/sessions")
                .await
                .map_err(|e| anyhow!("{e}; --sessions needs a server with /corrall/sessions (update and restart it)"))?,
        ),
        false => None,
    };
    // If the daemon has not yet finished its own release check (e.g. it started
    // moments ago), do one on demand so `status` still surfaces an available
    // update. Governed by the same `updateCheck` config flag as the daemon's
    // check, and by CORRALL_DISABLE_UPDATE_CHECK (honoured in latest_tag_cached).
    if cfg.update_check && st.get("updateCheckedAt").and_then(Value::as_i64).is_none() {
        if let Ok(Some(tag)) = tokio::task::spawn_blocking(crate::update::latest_tag_cached).await {
            if matches!(crate::update::compare(crate::update::current_version(), &tag), crate::update::Ordering::Upgrade | crate::update::Ordering::Major) {
                st["updateAvailable"] = Value::String(tag);
            }
        }
    }
    if json_out {
        if let Some(doc) = listed {
            st["sessions"]["list"] = doc.get("sessions").cloned().unwrap_or(Value::Null);
        }
        println!("{}", serde_json::to_string_pretty(&st)?);
    } else {
        let use_color = match color {
            "always" => true,
            "never" => false,
            _ => std::io::IsTerminal::is_terminal(&std::io::stdout()) && std::env::var_os("NO_COLOR").is_none(),
        };
        println!("{}", crate::status::render(&st, use_color, crate::quota::now_ms()));
        if let Some(doc) = listed {
            // As wide as the terminal; wide enough for every column in a pipe.
            let width = match std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                true => crossterm::terminal::size().map(|(w, _)| w).unwrap_or(120),
                false => 200,
            };
            let multi = st.get("pools").and_then(Value::as_array).is_some_and(|p| p.len() > 1);
            println!("\n{}", crate::session_table::render_text(&doc, multi, width));
        }
    }
    Ok(())
}

pub async fn switch(name: Option<String>, pool: Option<String>) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    crate::upstream::init(&cfg)?;
    match name {
        None => {
            let st = control_get(&cfg, "/corrall/status").await?;
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
            let r = control_post(&cfg, "/corrall/switch", body).await?;
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
    let (cfg, name, pool) = remove_account(pool.as_deref(), &name)?;
    eprintln!("Removed \"{name}\" from pool \"{pool}\"");
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
pub(crate) fn set_threshold(t: &mut Threshold, v: &str) -> Result<()> {
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
pub(crate) fn parse_secs(v: &str, min: u64, what: &str) -> Result<u64> {
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

/// A `distribute` value: `on`/`off`, or what to balance new sessions on
/// (`sessions`, `usage`), which also turns distribution on.
pub fn parse_distribute(v: &str) -> Result<(bool, Option<crate::config::DistributeBy>)> {
    use crate::config::DistributeBy;
    match v.to_ascii_lowercase().as_str() {
        "sessions" => Ok((true, Some(DistributeBy::Sessions))),
        "usage" => Ok((true, Some(DistributeBy::Usage))),
        other => parse_on_off(other).map(|on| (on, None)).map_err(|_| anyhow!("expected on, off, sessions or usage")),
    }
}

/// `on (by usage)`, `on (by sessions)` or `off`.
pub fn describe_distribute(p: &crate::config::PoolConfig) -> String {
    match p.distribute_sessions {
        true => format!("on (by {})", p.distribute_by.as_str()),
        false => "off".to_string(),
    }
}

pub async fn distribute(value: Option<String>, pool: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("distributeSessions: {}", describe_distribute(pool_of(&c, pool.as_deref())?));
            return Ok(());
        }
        Some(v) => {
            let (on, by) = parse_distribute(&v)?;
            Config::update(|c| {
                let p = pool_mut_of(c, pool.as_deref())?;
                p.distribute_sessions = on;
                if let Some(by) = by {
                    p.distribute_by = by;
                }
                eprintln!("distributeSessions: {}", describe_distribute(p));
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

pub async fn soonest_reset(value: Option<String>, pool: Option<String>) -> Result<()> {
    let cfg = match value {
        None => {
            let c = Config::load()?.unwrap_or_default();
            println!("soonestReset: {}", if pool_of(&c, pool.as_deref())?.soonest_reset { "on" } else { "off" });
            return Ok(());
        }
        Some(v) => {
            let on = parse_on_off(&v)?;
            Config::update(|c| {
                pool_mut_of(c, pool.as_deref())?.soonest_reset = on;
                eprintln!("soonestReset: {}", if on { "on" } else { "off" });
                Ok(())
            })?
        }
    };
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
                    if c.find_account_idx_in(&owner, a)?.is_none() {
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
                    match (p.distribute_sessions, p.distribute_by) {
                        (false, _) => "off",
                        (true, crate::config::DistributeBy::Sessions) => "on",
                        (true, crate::config::DistributeBy::Usage) => "usage",
                    },
                    match p.quota_probe_seconds {
                        0 => "off".to_string(),
                        n => format!("{n}s"),
                    },
                    p.hold_seconds,
                );
                // Auto-selection rules are what makes a wrapper land here on
                // its own, so they are worth showing next to the pool.
                for rule in match_summary(p) {
                    println!("{:<18} {rule}", "");
                }
                if let Some(f) = &p.fallback_pool {
                    println!("{:<18} fallback: {}", "", crate::security::safe_text(f, 40));
                }
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
                    bail!("\"{name}\" is the default pool; point defaultPool elsewhere first (`corrall pool set <other> --make-default`)");
                }
                let held = c.pool(&name).map(|p| p.accounts.len()).unwrap_or(0);
                if held > 0 && !force {
                    bail!(
                        "pool \"{name}\" still holds {held} account(s); move them with \
                         `corrall pool set <other> --account <name>`, or pass --force to delete them with the pool"
                    );
                }
                c.pools.remove(&name);
                for (other, p) in c.pools.iter_mut() {
                    if p.fallback_pool.as_deref() == Some(name.as_str()) {
                        p.fallback_pool = None;
                        eprintln!("pool \"{other}\" no longer falls back to \"{name}\"");
                    }
                }
                eprintln!("pool \"{name}\" removed{}", if held > 0 { format!(" with {held} account(s)") } else { String::new() });
                Ok(())
            })?;
            notify_reload(&cfg).await;
            Ok(())
        }
    }
}

/// One `match:` line per rule group a pool carries, for `pool list`. Empty when
/// the pool has no auto-selection rules, which is every pool by default.
fn match_summary(p: &crate::config::PoolConfig) -> Vec<String> {
    let Some(m) = &p.match_rules else { return Vec::new() };
    let mut out = Vec::new();
    for path in &m.paths {
        out.push(format!("match: path {}", crate::security::safe_text(path, 60)));
    }
    for r in &m.remotes {
        out.push(format!("match: remote ~ {}", crate::security::safe_text(r, 60)));
    }
    for (k, pat) in &m.env {
        let k = crate::security::safe_text(k, 30);
        out.push(match pat.is_empty() {
            true => format!("match: env {k} set"),
            false => format!("match: env {k} ~ {}", crate::security::safe_text(pat, 40)),
        });
    }
    out
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
    let distribute = set.distribute.as_deref().map(parse_distribute).transpose()?;
    let fallback = set.fallback.as_deref().map(|f| pool_name(c, Some(f))).transpose()?;
    if fallback.as_deref() == Some(name) {
        bail!("pool \"{name}\" cannot fall back to itself");
    }
    let p = c.pool_mut(name).expect("caller created the pool");
    if let Some(t) = &set.threshold {
        set_threshold(&mut p.switch_threshold, t)?;
    }
    if let Some((on, by)) = distribute {
        p.distribute_sessions = on;
        if let Some(by) = by {
            p.distribute_by = by;
        }
    }
    if let Some(secs) = probe {
        p.quota_probe_seconds = secs;
    }
    if let Some(h) = set.hold {
        p.hold_seconds = h;
    }
    // Each --match-* group replaces its own list rather than appending, so
    // rewriting a rule is one command and not a clear-then-add pair.
    if set.no_match {
        p.match_rules = None;
        eprintln!("auto-selection rules cleared");
    } else if !(set.match_path.is_empty() && set.match_remote.is_empty() && set.match_env.is_empty()) {
        let m = p.match_rules.get_or_insert_with(Default::default);
        if !set.match_path.is_empty() {
            m.paths = set.match_path.clone();
        }
        if !set.match_remote.is_empty() {
            m.remotes = set.match_remote.clone();
        }
        if !set.match_env.is_empty() {
            m.env = set
                .match_env
                .iter()
                .map(|v| match v.split_once('=') {
                    Some((k, pat)) => (k.trim().to_string(), pat.to_string()),
                    // Bare VAR means "matches when set".
                    None => (v.trim().to_string(), String::new()),
                })
                .collect();
            if m.env.contains_key("") {
                bail!("--match-env: expected VAR or VAR=REGEX");
            }
        }
        // Rules land in a file the daemon re-reads with no chance to complain,
        // so an uncompilable pattern is refused here, at the source.
        m.validate(name)?;
        if m.is_empty() {
            p.match_rules = None;
        }
    }
    if set.no_fallback {
        p.fallback_pool = None;
        eprintln!("fallback cleared");
    } else if let Some(f) = fallback {
        eprintln!("falls back to pool \"{f}\" when exhausted");
        p.fallback_pool = Some(f);
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
/// How the proxy key reaches Claude Code, which decides what Claude Code does
/// with its own claude.ai login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    /// `ANTHROPIC_CUSTOM_HEADERS` carrying [`PROXY_KEY_HEADER`], only when the
    /// server will ask for a key. Claude Code keeps its login and connectors.
    Header,
    /// `ANTHROPIC_API_KEY`, always. Claude Code runs on the key: no login
    /// needed, connectors off, smaller requests. The form before connectors
    /// were supported.
    ApiKey,
}

impl KeyMode {
    pub fn from_flag(api_key: bool) -> Self {
        if api_key {
            KeyMode::ApiKey
        } else {
            KeyMode::Header
        }
    }
}

/// One line of [`env_lines_with`], taken apart for a caller that sets
/// variables somewhere other than a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvOp {
    Set(String, String),
    Unset(String),
}

pub fn env_ops(lines: &[String]) -> Vec<EnvOp> {
    lines
        .iter()
        .filter_map(|line| {
            if let Some(rest) = line.strip_prefix("export ") {
                let (k, v) = rest.split_once('=')?;
                Some(EnvOp::Set(k.to_string(), v.trim_matches('\'').replace("'\"'\"'", "'")))
            } else {
                line.strip_prefix("unset ").map(|k| EnvOp::Unset(k.to_string()))
            }
        })
        .collect()
}

pub fn env_lines(cfg: &Config, use_mitm: bool, pin: Option<&str>, pool: Option<&str>) -> Vec<String> {
    env_lines_with(cfg, use_mitm, pin, pool, KeyMode::Header)
}

pub fn env_lines_with(cfg: &Config, use_mitm: bool, pin: Option<&str>, pool: Option<&str>, key_mode: KeyMode) -> Vec<String> {
    // What a client on this machine dials, which is not always what the
    // listener binds: a wildcard bind is reached over loopback.
    let authority = cfg.dial_authority();
    let mut lines = Vec::new();
    // The server exempts loopback peers, and a wildcard bind is dialled over
    // loopback, so the key only travels when the client will not be one. In
    // API-key mode ANTHROPIC_API_KEY always carries it: there it selects
    // Claude Code's auth source, not only ours.
    let loopback = crate::security::is_loopback_host(&cfg.dial_host());
    let key = if loopback && !cfg.proxy.require_key_on_loopback { "" } else { cfg.proxy.api_key.as_str() };
    let api_key_env = if key_mode == KeyMode::ApiKey { cfg.proxy.api_key.as_str() } else { "" };
    let named = pool.filter(|p| *p != cfg.default_pool);
    if use_mitm {
        let user = match (pin, named) {
            (Some(p), Some(pool)) => format!("{p}{}{pool}", crate::pools::MITM_POOL_SEP),
            (Some(p), None) => p.to_string(),
            (None, Some(pool)) => format!("{}{pool}", crate::pools::MITM_POOL_SEP),
            (None, None) => String::new(),
        };
        let userinfo = if user.is_empty() && key.is_empty() { String::new() } else { format!("{}:{}@", pin_component(&user), pin_component(key)) };
        let url = format!("http://{userinfo}{authority}");
        for v in ["HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"] {
            lines.push(format!("export {v}={}", shell_quote(&url)));
        }
        lines.push("export NO_PROXY=localhost,127.0.0.1,::1".into());
        lines.push("export no_proxy=localhost,127.0.0.1,::1".into());
        lines.push(format!("export NODE_EXTRA_CA_CERTS={}", shell_quote(&crate::proxy::mitm::ca_cert_path().to_string_lossy())));
        lines.push("unset ANTHROPIC_BASE_URL".into());
        // The tunnel authenticates on CONNECT; this key is for Claude Code's
        // benefit (API-key mode), and the proxy strips it on the way through.
        if !api_key_env.is_empty() {
            lines.push("unset ANTHROPIC_AUTH_TOKEN".into());
            lines.push(format!("export ANTHROPIC_API_KEY={}", shell_quote(api_key_env)));
        }
    } else {
        // The server strips `/pool/<name>` before it looks for `/tc-acct/`, so
        // the pool keyword comes first.
        let pool_prefix = named.map(|p| format!("{}{p}", crate::pools::POOL_PREFIX)).unwrap_or_default();
        let pin_prefix = pin.map(|p| format!("/tc-acct/{}", pin_component(p))).unwrap_or_default();
        lines.push(format!("export ANTHROPIC_BASE_URL=http://{authority}{pool_prefix}{pin_prefix}"));
        // A base URL other than api.anthropic.com puts Claude Code in
        // third-party mode, where deferred tool loading is off and every MCP
        // tool schema rides on every request in full. The proxy forwards
        // requests, tool_reference blocks included, unchanged, so keep Claude
        // Code in first-party mode. Not needed above: MITM leaves the base URL
        // unset.
        lines.push("export ENABLE_TOOL_SEARCH=auto".into());
        lines.push("export _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1".into());
        if !api_key_env.is_empty() {
            lines.push("unset ANTHROPIC_AUTH_TOKEN".into());
            lines.push(format!("export ANTHROPIC_API_KEY={}", shell_quote(api_key_env)));
        } else if !key.is_empty() {
            // Not ANTHROPIC_API_KEY: an API key in the environment makes
            // Claude Code drop its claude.ai login, and with it the connectors
            // authorised there. A header of our own leaves the login in charge
            // and is stripped before anything goes upstream.
            lines.push("unset ANTHROPIC_AUTH_TOKEN".into());
            lines.push(format!("export ANTHROPIC_CUSTOM_HEADERS={}", shell_quote(&format!("{}: {key}", crate::proxy::auth::PROXY_KEY_HEADER))));
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
/// then the environment, then the per-pool `match` rules applied to the launch
/// context. `TC_POOL` lets a shell wrapper choose a pool the same way `TC_ACCT`
/// already chooses an account.
///
/// A returned pool of `None` means "the default pool, unnamed" — the case that
/// has to keep emitting exactly the pre-pools environment, so it is kept
/// distinct from an explicit choice that happens to name the default pool.
fn launch_target(cfg: &Config, pool_flag: Option<&str>, cwd: Option<&str>) -> Result<(Option<String>, Option<String>)> {
    let pin = std::env::var("TC_ACCT").ok().filter(|s| !s.trim().is_empty());
    let pool = match pool_flag {
        Some(p) => Some(pool_name(cfg, Some(p))?),
        None => match std::env::var("TC_POOL").ok().filter(|s| !s.trim().is_empty()) {
            Some(p) => Some(pool_name(cfg, Some(&p)).context("TC_POOL")?),
            // Nothing named a pool, so let the launch context choose one.
            None => {
                let env = match cwd {
                    Some(d) => crate::pool_match::LaunchEnv::at(d),
                    None => crate::pool_match::LaunchEnv::live(),
                };
                let choice = crate::pool_match::resolve(cfg, &env);
                // Silent when no rule fired: an install with no `match` rules —
                // which is every install that has not opted in — must not start
                // writing to a wrapper's stderr on every launch.
                if choice.matched {
                    eprintln!("[Corrall] pool \"{}\" ({})", choice.pool, choice.reason);
                    Some(choice.pool)
                } else {
                    None
                }
            }
        },
    };
    if let Some(p) = &pin {
        // A pin only resolves inside the pool serving the request.
        let scope = pool.clone().unwrap_or_else(|| cfg.default_pool.clone());
        if cfg.find_account_idx_in(&scope, p).map_err(|e| anyhow!("TC_ACCT={p}: {e}"))?.is_none() {
            bail!("TC_ACCT={p} matches no account in pool \"{scope}\"");
        }
    }
    Ok((pin, pool))
}

pub fn env(args: EnvArgs) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet"))?;
    let (pin, pool) = launch_target(&cfg, args.pool.as_deref(), args.cwd.as_deref())?;
    if !args.no_mitm {
        crate::proxy::mitm::ensure_certs(&["api.anthropic.com".to_string()])?;
    }
    let lines = env_lines_with(&cfg, !args.no_mitm, pin.as_deref(), pool.as_deref(), KeyMode::from_flag(args.api_key));
    let lines = if args.fish { fish_lines(&env_ops(&lines)) } else { lines };
    for l in lines {
        println!("{l}");
    }
    Ok(())
}

/// The same variables for fish, which has neither `export K=V` nor `unset`.
/// Inside fish's single quotes only `\` and `'` are special.
pub fn fish_lines(ops: &[EnvOp]) -> Vec<String> {
    ops.iter()
        .map(|op| match op {
            EnvOp::Set(k, v) => format!("set -gx {k} '{}'", v.replace('\\', "\\\\").replace('\'', "\\'")),
            EnvOp::Unset(k) => format!("set -e {k}"),
        })
        .collect()
}

pub async fn run(args: RunArgs) -> Result<()> {
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet; run `corrall login` first"))?;
    crate::upstream::init(&cfg)?;
    let up = control_get(&cfg, "/corrall/health").await.is_ok();
    let mut cmd = std::process::Command::new("claude");
    cmd.args(&args.args);
    cmd.env_remove("TC_ACCT");
    cmd.env_remove("TC_POOL");
    if !up {
        if args.auto_fallback {
            eprintln!("[Corrall] proxy is not running; launching claude directly (no rotation)");
        } else {
            bail!("proxy is not running on port {}; start `corrall server` or pass --auto-fallback", cfg.proxy.port);
        }
    } else {
        let (pin, pool) = launch_target(&cfg, args.pool.as_deref(), None)?;
        if !args.no_mitm {
            crate::proxy::mitm::ensure_certs(&["api.anthropic.com".to_string()])?;
        }
        for op in env_ops(&env_lines_with(&cfg, !args.no_mitm, pin.as_deref(), pool.as_deref(), KeyMode::from_flag(args.api_key))) {
            match op {
                EnvOp::Set(k, v) => cmd.env(k, v),
                EnvOp::Unset(k) => cmd.env_remove(k),
            };
        }
    }
    let status = cmd.status().context("could not launch `claude`; is it on PATH?")?;
    std::process::exit(status.code().unwrap_or(1));
}

pub async fn setup(cmd: SetupCmd) -> Result<()> {
    match cmd {
        SetupCmd::Claude(a) => setup_claude(a).await,
    }
}

async fn setup_claude(args: SetupClaudeArgs) -> Result<()> {
    use crate::claude_setup::{self, SettingsFile};
    let cfg = Config::load()?.ok_or_else(|| anyhow!("no config yet; run `corrall login` first"))?;
    let path = match args.settings {
        Some(p) => p,
        None => claude_setup::default_path()?,
    };
    let mut file = SettingsFile::read(&path)?;
    let shown = file.path.display().to_string();
    let (now, was) = if args.dry_run { ("would be", "would be") } else { ("is now", "was") };
    // Key names only, never values: what is replaced here is often a credential.
    let list = |label: &str, keys: &[String]| {
        if !keys.is_empty() {
            eprintln!("  {label:<9}{}", keys.join(", "));
        }
    };

    if args.remove {
        let report = claude_setup::strip(&mut file.value, &cfg)?;
        if !report.changed() {
            eprintln!("[Corrall] {shown} does not point Claude Code at the proxy; nothing to remove");
            return Ok(());
        }
        let backup = if args.dry_run { None } else { file.write(false)? };
        eprintln!("[Corrall] {shown}: the proxy {was} taken out of Claude Code's settings");
        list("removed:", &report.removed);
        list("backup:", &backup.iter().map(|b| b.display().to_string()).collect::<Vec<_>>());
        return Ok(());
    }

    // No launch-context matching: the file serves every directory, so the pool
    // is the one named here or the default.
    let pool = args.pool.as_deref().map(|p| pool_name(&cfg, Some(p))).transpose()?;
    let ops = env_ops(&env_lines_with(&cfg, false, None, pool.as_deref(), KeyMode::from_flag(args.api_key)));
    let report = claude_setup::apply(&mut file.value, &ops)?;
    if !report.changed() {
        eprintln!("[Corrall] {shown} already points Claude Code at the proxy; nothing to change");
    } else {
        let holds_key = ops.iter().any(|op| matches!(op, EnvOp::Set(k, _) if k == "ANTHROPIC_CUSTOM_HEADERS" || k == "ANTHROPIC_API_KEY"));
        let backup = if args.dry_run { None } else { file.write(holds_key)? };
        let url = ops.iter().find_map(|op| match op {
            EnvOp::Set(k, v) if k == "ANTHROPIC_BASE_URL" => Some(v.as_str()),
            _ => None,
        });
        eprintln!("[Corrall] {shown}: Claude Code {now} pointed at the proxy ({})", url.unwrap_or_default());
        list("set:", &report.set);
        list("removed:", &report.removed);
        list("backup:", &backup.iter().map(|b| b.display().to_string()).collect::<Vec<_>>());
        if !report.models.is_empty() {
            eprintln!("  note: {} still name models for the provider that was removed; check Anthropic serves them", report.models.join(", "));
        }
        if !args.dry_run {
            eprintln!("  Restart running Claude Code sessions so they pick this up.");
        }
    }

    let exported = claude_setup::shell_conflicts(&ops);
    if !exported.is_empty() {
        eprintln!(
            "  warning: {} exported in this shell. Settings cannot unset a shell export, so Claude Code started from it may still go elsewhere; remove it from your shell profile",
            exported.join(", ")
        );
    }
    // On macOS nothing else brings the proxy up at login, and settings that
    // point at a proxy that is not there break every Claude Code session. So
    // there setup also sees to the launchd service.
    let launchd = crate::service::Kind::native() == Some(crate::service::Kind::Launchd);
    if launchd || !args.dry_run {
        crate::upstream::init(&cfg)?;
    }
    if launchd {
        ensure_service(&cfg, args.dry_run, args.no_service).await;
    } else if !args.dry_run && control_get(&cfg, "/corrall/health").await.is_err() {
        eprintln!(
            "  warning: the proxy is not running on port {}; Claude Code cannot connect until it is (`corrall server`, or `corrall service install`)",
            cfg.proxy.port
        );
    }
    Ok(())
}

/// `setup claude` and `setup claude --remove` for the default settings file,
/// from the dashboard: the same edit and the same backup, with what happened
/// returned in one line instead of printed. Key names only, never values.
pub fn point_claude_code(cfg: &Config, remove: bool) -> Result<String> {
    use crate::claude_setup::{self, SettingsFile};
    let mut file = SettingsFile::read(&claude_setup::default_path()?)?;
    // The outcome first: this is read in one line of a terminal.
    let shown = crate::tui_screens::tilde(&file.path);
    if remove {
        let report = claude_setup::strip(&mut file.value, cfg)?;
        if !report.changed() {
            return Ok(format!("nothing to remove: {shown} does not point Claude Code at the proxy"));
        }
        file.write(false)?;
        return Ok(format!("the proxy was taken out of {shown}"));
    }
    let ops = env_ops(&env_lines_with(cfg, false, None, None, KeyMode::Header));
    let report = claude_setup::apply(&mut file.value, &ops)?;
    if !report.changed() {
        return Ok(format!("Claude Code already points at the proxy ({shown})"));
    }
    let holds_key = ops.iter().any(|op| matches!(op, EnvOp::Set(k, _) if k == "ANTHROPIC_CUSTOM_HEADERS" || k == "ANTHROPIC_API_KEY"));
    file.write(holds_key)?;
    let removed = if report.removed.is_empty() { String::new() } else { format!(", replacing {}", report.removed.join(", ")) };
    Ok(format!("Claude Code now points at the proxy{removed}; restart its sessions ({shown})"))
}

/// Ask for `/corrall/health` until it answers or `budget` runs out.
async fn proxy_answers(cfg: &Config, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if control_get(cfg, "/corrall/health").await.is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// The service half of `setup claude`: check that the proxy runs as a service,
/// and install and start that service when nothing is serving at all. Never an
/// error: the settings are already written, so what is wrong is reported.
async fn ensure_service(cfg: &Config, dry_run: bool, no_service: bool) {
    use crate::service::{self, Ensure};
    use std::time::Duration;
    let Some(kind) = service::Kind::native() else { return };
    let (name, port) = (kind.name(), cfg.proxy.port);
    let silent = |after: &str| {
        eprintln!("  warning: the service ({name}) is {after} but the proxy does not answer on port {port}; its log says why: {}", kind.logs_hint())
    };
    match service::ensure_plan(service::state(), proxy_answers(cfg, Duration::ZERO).await) {
        Ensure::Unsupported => {}
        Ensure::Fine => eprintln!("  service: the proxy is running as a service ({name}) and starts at login"),
        // Long enough for a start in progress, short enough not to hang on a
        // service that is failing.
        Ensure::Wait if proxy_answers(cfg, Duration::from_secs(12)).await => {
            eprintln!("  service: the proxy is running as a service ({name}) and starts at login")
        }
        Ensure::Wait => silent("loaded"),
        Ensure::ByHand => eprintln!(
            "  warning: the proxy answers on port {port}, but nothing keeps it running: it was started by hand and stops with its terminal. Stop it, then run `corrall service install` so it starts at login"
        ),
        Ensure::Install if no_service => {
            eprintln!("  warning: the proxy is not running on port {port} and --no-service left the service alone; Claude Code cannot connect until `corrall server` or `corrall service install`")
        }
        Ensure::Install if dry_run => eprintln!("  service: the proxy is not running; the service ({name}) would be installed and started"),
        Ensure::Install => match service::install() {
            Ok(path) if proxy_answers(cfg, Duration::from_secs(15)).await => {
                eprintln!("  service: the proxy was not running, so it is now installed as a service ({name}) that starts at login");
                eprintln!("           {} (`corrall service uninstall` takes it out)", path.display());
            }
            Ok(_) => silent("installed"),
            Err(e) => eprintln!("  warning: the proxy is not running and installing the service failed: {e:#}"),
        },
    }
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

/// The API host `corrall api` may send this account's credential to. A Codex
/// account is `kind: oauth` like an Anthropic one but its bearer token is an
/// OpenAI credential; it must never travel to an Anthropic host.
fn api_base_for(a: &AccountConfig) -> Result<&'static str> {
    if a.is_codex() {
        bail!(
            "\"{}\" is a Codex (OpenAI) account; `corrall api` only calls the Anthropic API and will not send its token there. Pick an Anthropic account with --account",
            a.name
        );
    }
    Ok("https://api.anthropic.com")
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
    // Before any token is fetched or sent: the host is fixed, so the account
    // has to be one whose credential belongs there.
    let base = api_base_for(a)?;
    let m = crate::manager::Manager::new(&cfg, &owner);
    let id = a.id.clone().unwrap_or_default();
    let cred = m
        .ensure_token_fresh(&id, false, crate::manager::OnRefreshFail::MarkDead)
        .await
        .or_else(|| a.api_key.clone())
        .ok_or_else(|| anyhow!("no credential for {}", a.name))?;
    let mut req = crate::upstream::client().get(format!("{base}{path}"));
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
    fn api_key_mode_hands_the_key_over_the_old_way() {
        let mut cfg = Config::default();
        cfg.proxy.api_key = "tc-secret-0123456789abcdef".into();
        // Even on loopback: the point is Claude Code's auth mode, not ours.
        let lines = env_lines_with(&cfg, false, None, None, KeyMode::ApiKey).join("\n");
        assert!(lines.contains("export ANTHROPIC_API_KEY='tc-secret-0123456789abcdef'"));
        assert!(lines.contains("unset ANTHROPIC_AUTH_TOKEN"));
        assert!(!lines.contains("ANTHROPIC_CUSTOM_HEADERS"));
        let lines = env_lines_with(&cfg, true, None, None, KeyMode::ApiKey).join("\n");
        assert!(lines.contains("export ANTHROPIC_API_KEY='tc-secret-0123456789abcdef'"));
        assert!(lines.contains("HTTPS_PROXY='http://127.0.0.1:3456'"), "CONNECT auth is unchanged");
        assert_eq!(env_lines(&cfg, false, None, None), env_lines_with(&cfg, false, None, None, KeyMode::Header));
    }

    #[test]
    fn env_lines_keep_first_party_mode_behind_a_base_url() {
        let cfg = Config::default();
        for key_mode in [KeyMode::Header, KeyMode::ApiKey] {
            let lines = env_lines_with(&cfg, false, None, None, key_mode);
            assert!(lines.contains(&"export ENABLE_TOOL_SEARCH=auto".to_string()), "{lines:?}");
            assert!(lines.contains(&"export _CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1".to_string()), "{lines:?}");
            // MITM mode leaves ANTHROPIC_BASE_URL unset, so Claude Code already
            // sees api.anthropic.com and needs no telling.
            let lines = env_lines_with(&cfg, true, None, None, key_mode).join("\n");
            assert!(!lines.contains("ENABLE_TOOL_SEARCH"), "{lines}");
            assert!(!lines.contains("FIRST_PARTY"), "{lines}");
        }
    }

    #[test]
    fn fish_gets_the_same_variables_in_its_own_syntax() {
        let mut cfg = Config::default();
        cfg.proxy.api_key = "tc-secret-0123456789abcdef".into();
        cfg.proxy.require_key_on_loopback = true;
        let lines = env_lines_with(&cfg, false, None, None, KeyMode::Header);
        let fish = fish_lines(&env_ops(&lines));
        assert_eq!(fish.len(), lines.len(), "one line each");
        assert!(fish.contains(&"set -gx ANTHROPIC_BASE_URL 'http://127.0.0.1:3456'".to_string()), "{fish:?}");
        assert!(fish.contains(&"set -gx ANTHROPIC_CUSTOM_HEADERS 'x-corrall-key: tc-secret-0123456789abcdef'".to_string()), "{fish:?}");
        assert!(fish.iter().all(|l| l.starts_with("set -gx ") || l.starts_with("set -e ")), "{fish:?}");
        for (bash, fish) in lines.iter().zip(&fish) {
            assert_eq!(bash.starts_with("unset "), fish.starts_with("set -e "), "{bash} / {fish}");
        }
        // Only a quote and a backslash need escaping inside fish's quotes.
        let odd = fish_lines(&[EnvOp::Set("K".into(), r"it's a \ path $HOME".into()), EnvOp::Unset("GONE".into())]);
        assert_eq!(odd, [r"set -gx K 'it\'s a \\ path $HOME'", "set -e GONE"]);
    }

    #[test]
    fn distribute_takes_a_switch_or_what_to_balance_on() {
        use crate::config::DistributeBy;
        assert_eq!(parse_distribute("on").unwrap(), (true, None));
        assert_eq!(parse_distribute("OFF").unwrap(), (false, None));
        assert_eq!(parse_distribute("usage").unwrap(), (true, Some(DistributeBy::Usage)));
        assert_eq!(parse_distribute("sessions").unwrap(), (true, Some(DistributeBy::Sessions)));
        assert!(parse_distribute("random").unwrap_err().to_string().contains("on, off, sessions or usage"));
        let mut p = crate::config::PoolConfig::default();
        assert_eq!(describe_distribute(&p), "off");
        (p.distribute_sessions, p.distribute_by) = (true, DistributeBy::Usage);
        assert_eq!(describe_distribute(&p), "on (by usage)");
        // The balance is only written to the file when it is not the default.
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["distributeBy"], "usage");
        assert!(serde_json::to_value(crate::config::PoolConfig::default()).unwrap().get("distributeBy").is_none());
        assert_eq!(
            serde_json::from_value::<crate::config::PoolConfig>(serde_json::json!({ "distributeBy": "usage" })).unwrap().distribute_by,
            DistributeBy::Usage
        );
    }

    #[test]
    fn env_lines_keep_key_out_on_loopback() {
        let mut cfg = Config::default();
        cfg.proxy.api_key = "tc-secret-0123456789abcdef".into();
        let lines = env_lines(&cfg, true, None, None).join("\n");
        assert!(!lines.contains("tc-secret"));
        assert!(lines.contains("HTTPS_PROXY='http://127.0.0.1:3456'"));
        let lines = env_lines(&cfg, true, Some("me@example.com (Acme)"), None).join("\n");
        assert!(lines.contains("me%40example%2Ecom%20%28Acme%29:@127.0.0.1"));
        // A wildcard bind is dialled over loopback, where no key is needed.
        cfg.proxy.host = Some("0.0.0.0".into());
        let lines = env_lines(&cfg, false, None, None).join("\n");
        assert!(!lines.contains("tc-secret"));
        assert!(lines.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:3456"));
        // A specific address is dialled as itself, so the key travels: in its
        // own header, never as ANTHROPIC_API_KEY, which would switch off the
        // claude.ai connectors in Claude Code.
        cfg.proxy.host = Some("192.168.1.10".into());
        let lines = env_lines(&cfg, false, None, None).join("\n");
        assert!(lines.contains("export ANTHROPIC_CUSTOM_HEADERS='x-corrall-key: tc-secret-0123456789abcdef'"));
        assert!(lines.contains("unset ANTHROPIC_AUTH_TOKEN"));
        assert!(!lines.contains("ANTHROPIC_API_KEY"));
        cfg.proxy.host = Some("127.0.0.1".into());
        cfg.proxy.require_key_on_loopback = true;
        let lines = env_lines(&cfg, false, None, None).join("\n");
        assert!(lines.contains("ANTHROPIC_CUSTOM_HEADERS='x-corrall-key: tc-secret"));
    }

    /// The re-login shape that bit claudeacrobat: a profile lookup that comes
    /// back with no `accountUuid` leaves nothing to match on but the name, and
    /// must refresh the account it named — in place, keeping the identity that
    /// account already had — rather than adding a second one beside it.
    #[test]
    fn a_relogin_with_an_empty_profile_refreshes_the_account_it_named() {
        let tokens = |t: &str| oauth::Tokens { access_token: t.into(), refresh_token: Some(format!("r-{t}")), expires_at: 1, refresh_expires_at: None };
        let mut cfg = Config::default();
        let pool = cfg.default_pool.clone();
        let first = oauth::Profile {
            account_uuid: Some("u1".into()),
            email: Some("alice@example.com".into()),
            org_uuid: Some("o1".into()),
            org_name: Some("Acme".into()),
            rate_limit_tier: Some("tier2".into()),
            ..Default::default()
        };
        assert!(!upsert_oauth(&mut cfg, &pool, "alice@example.com", &tokens("old"), Some(&first)), "the first login adds");

        // The lookup answered, with nothing in it.
        assert!(
            upsert_oauth(&mut cfg, &pool, "alice@example.com", &tokens("fresh"), Some(&oauth::Profile::default())),
            "an empty profile must update the named account, not add another"
        );
        // And the same when it failed outright, which is what `--name` is for.
        assert!(upsert_oauth(&mut cfg, &pool, "alice@example.com", &tokens("fresher"), None));

        let accounts = &cfg.pools[&pool].accounts;
        assert_eq!(accounts.len(), 1, "{accounts:#?}");
        let a = &accounts[0];
        assert_eq!(a.access_token.as_deref(), Some("fresher"));
        assert_eq!(a.refresh_token.as_deref(), Some("r-fresher"));
        // Every profile field is merged on its own, so an empty one keeps what
        // was stored instead of wiping the account's identity.
        assert_eq!(a.account_uuid.as_deref(), Some("u1"));
        assert_eq!(a.org_uuid.as_deref(), Some("o1"));
        assert_eq!(a.org_name.as_deref(), Some("Acme"));
        assert_eq!(a.email.as_deref(), Some("alice@example.com"));
        assert_eq!(a.rate_limit_tier.as_deref(), Some("tier2"));

        // A login that is genuinely another account still lands beside it.
        let bob = oauth::Profile { account_uuid: Some("u2".into()), email: Some("bob@example.com".into()), ..Default::default() };
        assert_eq!(upsert_oauth_at(&mut cfg, &pool, "bob@example.com", &tokens("bob"), Some(&bob)), (1, false));
        assert_eq!(cfg.pools[&pool].accounts.len(), 2);
    }

    /// A re-login without `--pool` refreshes the account in the pool it
    /// already lives in instead of dragging it back to the default pool; only
    /// an explicit `--pool` moves it, and a new account still lands in the
    /// default pool.
    #[test]
    fn a_relogin_without_pool_keeps_the_account_where_it_is() {
        let tokens = |t: &str| oauth::Tokens { access_token: t.into(), refresh_token: None, expires_at: 1, refresh_expires_at: None };
        let alice = oauth::Profile { account_uuid: Some("u1".into()), email: Some("alice@example.com".into()), ..Default::default() };
        let mut cfg = Config::default();
        let work = target_pool(&mut cfg, Some("work"), None).unwrap();
        upsert_oauth(&mut cfg, &work, "alice@example.com", &tokens("old"), Some(&alice));

        let found = locate_oauth(&cfg, "alice@example.com", Some(&alice));
        let pool = target_pool(&mut cfg, None, found.as_ref()).unwrap();
        assert_eq!(pool, "work");
        assert!(upsert_oauth(&mut cfg, &pool, "alice@example.com", &tokens("fresh"), Some(&alice)));
        assert_eq!(cfg.pools["work"].accounts[0].access_token.as_deref(), Some("fresh"));
        assert!(cfg.pools[crate::config::DEFAULT_POOL].accounts.is_empty());

        let bob = oauth::Profile { account_uuid: Some("u2".into()), ..Default::default() };
        let found = locate_oauth(&cfg, "bob@example.com", Some(&bob));
        assert_eq!(target_pool(&mut cfg, None, found.as_ref()).unwrap(), crate::config::DEFAULT_POOL, "a new account goes to the default pool");

        let found = locate_oauth(&cfg, "alice@example.com", Some(&alice));
        assert_eq!(
            target_pool(&mut cfg, Some(crate::config::DEFAULT_POOL), found.as_ref()).unwrap(),
            crate::config::DEFAULT_POOL,
            "an explicit --pool still moves it"
        );
    }

    /// The CLI's own credentials file is linked unless `--copy` says otherwise:
    /// that CLI keeps refreshing the login, and a copy refreshed here as well
    /// would rotate the refresh token out from under it.
    #[test]
    fn import_links_the_clis_own_file_and_copies_others() {
        use clap::Parser;
        let parse = |argv: &[&str]| match Cli::parse_from(argv).command {
            Some(Command::Import(a)) => a,
            other => panic!("not an import: {other:?}"),
        };
        let own = oauth::DEFAULT_CREDENTIALS_PATH;
        let a = parse(&["corrall", "import"]);
        assert!(import_links(&a, &a.from, own));
        assert!(!import_links(&parse(&["corrall", "import", "--copy"]), own, own));
        let other = parse(&["corrall", "import", "--from", "/tmp/acct2/.credentials.json"]);
        assert!(!import_links(&other, &other.from, own));
        let other = parse(&["corrall", "import", "--from", "/tmp/acct2/.credentials.json", "--link"]);
        assert!(import_links(&other, &other.from, own));
        assert!(Cli::try_parse_from(["corrall", "import", "--link", "--copy"]).is_err());
        let codex = parse(&["corrall", "import", "--codex", "--from", "~/.codex/auth.json"]);
        assert!(import_links(&codex, &codex.from, crate::codex::DEFAULT_CREDENTIALS_PATH));
    }

    /// `import --link` of the same user's credentials for a second org, where
    /// the profile carries no org name so the display name is the bare e-mail.
    /// The upsert appends a new entry; the link step must land on that entry
    /// and not on the older `me@x.com (Acme)`, which a fuzzy lookup by e-mail
    /// or name prefix finds first.
    #[test]
    fn import_link_touches_the_entry_it_upserted_and_no_sibling() {
        let mut cfg = Config::default();
        let pool = cfg.default_pool.clone();
        let acme = oauth::Profile {
            account_uuid: Some("u1".into()),
            email: Some("me@x.com".into()),
            org_uuid: Some("org-acme".into()),
            org_name: Some("Acme".into()),
            ..Default::default()
        };
        let old = oauth::Tokens { access_token: "acme-access".into(), refresh_token: Some("acme-refresh".into()), expires_at: 1, refresh_expires_at: None };
        upsert_oauth(&mut cfg, &pool, "me@x.com (Acme)", &old, Some(&acme));
        cfg.pools.get_mut(&pool).unwrap().accounts[0].subscription_type = Some("max".into());

        let other =
            oauth::Profile { account_uuid: Some("u1".into()), email: Some("me@x.com".into()), org_uuid: Some("org-other".into()), ..Default::default() };
        let name = display_name(&other, &cfg);
        assert_eq!(name, "me@x.com", "no org name to disambiguate with");
        let fresh = oauth::Tokens { access_token: "other-access".into(), refresh_token: Some("other-refresh".into()), expires_at: 2, refresh_expires_at: None };
        let i = apply_import(&mut cfg, &pool, &name, &fresh, Some(&other), Some("/home/me/.claude/.credentials.json"), Some("pro"));

        let accounts = &cfg.pools[&pool].accounts;
        assert_eq!(accounts.len(), 2, "{accounts:#?}");
        assert_eq!(i, 1);
        // The Acme entry keeps its own tokens and is not linked to anything.
        let a = &accounts[0];
        assert_eq!(a.name, "me@x.com (Acme)");
        assert_eq!(a.access_token.as_deref(), Some("acme-access"));
        assert_eq!(a.refresh_token.as_deref(), Some("acme-refresh"));
        assert_eq!(a.import_from, None);
        assert_eq!(a.subscription_type.as_deref(), Some("max"));
        // The new entry is the linked one: no copied token, reads the file.
        let b = &accounts[1];
        assert_eq!(b.name, "me@x.com");
        assert_eq!(b.org_uuid.as_deref(), Some("org-other"));
        assert_eq!(b.import_from.as_deref(), Some("/home/me/.claude/.credentials.json"));
        assert_eq!(b.access_token, None);
        assert_eq!(b.refresh_token, None);
        assert_eq!(b.expires_at, None);
        assert_eq!(b.subscription_type.as_deref(), Some("pro"));

        // Without --link the same import keeps the tokens on the new entry.
        let mut cfg2 = Config::default();
        upsert_oauth(&mut cfg2, &pool, "me@x.com (Acme)", &old, Some(&acme));
        let i = apply_import(&mut cfg2, &pool, &name, &fresh, Some(&other), None, None);
        assert_eq!(i, 1);
        assert_eq!(cfg2.pools[&pool].accounts[1].access_token.as_deref(), Some("other-access"));
        assert_eq!(cfg2.pools[&pool].accounts[0].access_token.as_deref(), Some("acme-access"));
    }

    /// A Codex account is `kind: oauth` with `provider: codex`; matching on the
    /// kind alone would hand its OpenAI bearer token to api.anthropic.com.
    #[test]
    fn corrall_api_refuses_a_codex_account_before_any_request() {
        let codex = AccountConfig {
            name: "codex".into(),
            kind: AccountType::Oauth,
            provider: Some("codex".into()),
            access_token: Some("sk-oa".into()),
            ..Default::default()
        };
        let err = api_base_for(&codex).unwrap_err().to_string();
        assert!(err.contains("Codex"), "{err}");
        assert!(err.contains("codex"), "names the account: {err}");

        let anthropic = AccountConfig { name: "me".into(), kind: AccountType::Oauth, ..Default::default() };
        assert_eq!(api_base_for(&anthropic).unwrap(), "https://api.anthropic.com");
        let key = AccountConfig { name: "key".into(), kind: AccountType::Apikey, ..Default::default() };
        assert_eq!(api_base_for(&key).unwrap(), "https://api.anthropic.com");
    }

    /// `env` configures a client on this machine, so it emits an address that
    /// client can actually dial: loopback for a bind nothing can connect to,
    /// and the bound host itself when that is the only address answering.
    #[test]
    fn a_local_client_is_pointed_at_an_address_it_can_dial() {
        let mut cfg = Config::default();
        cfg.proxy.api_key = "tc-secret-0123456789abcdef".into();
        for host in ["0.0.0.0", "::", "[::]", "127.0.0.1", "localhost"] {
            cfg.proxy.host = Some(host.into());
            let lines = env_lines(&cfg, false, None, None);
            assert!(lines.contains(&"export ANTHROPIC_BASE_URL=http://127.0.0.1:3456".to_string()), "{host}: {lines:?}");
            assert_eq!(proxy_base(&cfg), "http://127.0.0.1:3456");
        }
        // A specific host is kept: loopback is nowhere to dial when the
        // listener only answers there.
        cfg.proxy.host = Some("192.168.1.10".into());
        assert!(env_lines(&cfg, false, None, None).join("\n").contains("ANTHROPIC_BASE_URL=http://192.168.1.10:3456"));
        assert!(env_lines(&cfg, true, None, None).join("\n").contains("@192.168.1.10:3456'"));
        assert_eq!(proxy_base(&cfg), "http://192.168.1.10:3456");
        // A bare IPv6 literal has to come back bracketed to carry a port, and
        // a specific loopback address is specific too — a listener on `::1`
        // does not answer on `127.0.0.1`.
        cfg.proxy.host = Some("fd00::1".into());
        assert_eq!(proxy_base(&cfg), "http://[fd00::1]:3456");
        cfg.proxy.host = Some("[::1]".into());
        assert_eq!(proxy_base(&cfg), "http://[::1]:3456");
    }

    /// Naming the default pool must not change a single byte: an existing
    /// wrapper doing `eval "$(corrall env)"` keeps working untouched.
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

    /// A matched pool becomes the `/pool/<name>` prefix, and — the part that
    /// matters for every install that never writes a rule — a resolution that
    /// matches nothing produces the same lines as no pools at all.
    #[test]
    fn a_matched_pool_reaches_the_env_lines() {
        use crate::config::PoolMatch;
        use crate::pool_match::{resolve, LaunchEnv};

        let mut cfg = Config::default();
        cfg.ensure_pool("work").unwrap();
        cfg.pool_mut("work").unwrap().match_rules = Some(PoolMatch { paths: vec!["/srv/work".into()], ..Default::default() });
        let at = |cwd: &str| LaunchEnv { cwd: cwd.into(), getenv: Box::new(|_| None), remote: Box::new(|_| None) };

        let hit = resolve(&cfg, &at("/srv/work/api"));
        assert!(hit.matched);
        let lines = env_lines(&cfg, false, None, Some(&hit.pool)).join("\n");
        assert!(lines.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:3456/pool/work"), "{lines}");

        // Nothing matched: `launch_target` keeps the pool unnamed, so the lines
        // are the pre-pools ones byte-for-byte.
        let miss = resolve(&cfg, &at("/elsewhere"));
        assert!(!miss.matched);
        assert_eq!(env_lines(&cfg, false, None, None), env_lines(&Config::default(), false, None, None));
    }

    /// `--match-*` groups replace their own list, `--no-match` drops the block,
    /// and an uncompilable pattern is refused before it is written.
    #[test]
    fn match_flags_write_and_clear_rules() {
        let mut cfg = Config::default();
        cfg.ensure_pool("work").unwrap();
        let set = |path: &[&str], remote: &[&str], env: &[&str], clear: bool| PoolSetArgs {
            threshold: None,
            distribute: None,
            probe: None,
            hold: None,
            account: vec![],
            make_default: false,
            match_path: path.iter().map(|s| s.to_string()).collect(),
            match_remote: remote.iter().map(|s| s.to_string()).collect(),
            match_env: env.iter().map(|s| s.to_string()).collect(),
            no_match: clear,
            ..Default::default()
        };

        apply_pool_set(&mut cfg, "work", &set(&["/srv/a", "/srv/b"], &["acme/"], &["TC_CTX=^a", "TC_FLAG"], false)).unwrap();
        let m = cfg.pool("work").unwrap().match_rules.clone().unwrap();
        assert_eq!(m.paths, ["/srv/a", "/srv/b"]);
        assert_eq!(m.remotes, ["acme/"]);
        // A bare VAR is the "matches when set" form: an empty pattern.
        assert_eq!(m.env.get("TC_CTX").map(String::as_str), Some("^a"));
        assert_eq!(m.env.get("TC_FLAG").map(String::as_str), Some(""));

        // One group is rewritten; the others are left alone.
        apply_pool_set(&mut cfg, "work", &set(&["/srv/c"], &[], &[], false)).unwrap();
        let m = cfg.pool("work").unwrap().match_rules.clone().unwrap();
        assert_eq!(m.paths, ["/srv/c"]);
        assert_eq!(m.remotes, ["acme/"], "an untouched group survives");

        assert!(apply_pool_set(&mut cfg, "work", &set(&[], &["("], &[], false)).is_err(), "a bad regex must be refused");

        apply_pool_set(&mut cfg, "work", &set(&[], &[], &[], true)).unwrap();
        assert_eq!(cfg.pool("work").unwrap().match_rules, None);
    }

    #[test]
    fn fallback_flag_sets_and_clears_the_fallback_pool() {
        let mut cfg = Config::default();
        cfg.ensure_pool("work").unwrap();
        let fb = |f: Option<&str>, clear: bool| PoolSetArgs { fallback: f.map(str::to_string), no_fallback: clear, ..Default::default() };

        apply_pool_set(&mut cfg, "work", &fb(Some("default"), false)).unwrap();
        assert_eq!(cfg.pool("work").unwrap().fallback_pool.as_deref(), Some("default"));
        assert!(apply_pool_set(&mut cfg, "work", &fb(Some("nope"), false)).is_err(), "an unknown pool is refused");
        assert!(apply_pool_set(&mut cfg, "work", &fb(Some("work"), false)).is_err(), "a pool cannot fall back to itself");
        apply_pool_set(&mut cfg, "work", &fb(None, true)).unwrap();
        assert_eq!(cfg.pool("work").unwrap().fallback_pool, None);
    }

    #[test]
    fn listen_accepts_a_port_a_host_and_a_bare_colon() {
        assert_eq!(parse_listen("3456").unwrap(), (None, 3456));
        assert_eq!(parse_listen(":8080").unwrap(), (None, 8080));
        assert_eq!(parse_listen(" 127.0.0.1:8080 ").unwrap(), (Some("127.0.0.1".into()), 8080));
        assert_eq!(parse_listen("0.0.0.0:8080").unwrap(), (Some("0.0.0.0".into()), 8080));
        assert_eq!(parse_listen("[::1]:8080").unwrap(), (Some("[::1]".into()), 8080));
        assert_eq!(parse_listen("localhost:8080").unwrap(), (Some("localhost".into()), 8080));

        for bad in ["", "  ", "nonsense", "example.com:8080", "127.0.0.1:", "127.0.0.1:notaport", "::1:8080", "0", "127.0.0.1:0", "127.0.0.1:99999"] {
            assert!(parse_listen(bad).is_err(), "{bad:?} should not parse");
        }
        // The IPv6 message says what to do about it.
        assert!(parse_listen("::1:8080").unwrap_err().to_string().contains("[::1]:8080"));
    }

    #[test]
    fn a_listen_override_moves_the_bind_and_still_faces_validation() {
        let mut cfg = Config::default();
        apply_listen_override(&mut cfg, None, None).unwrap();
        assert_eq!((cfg.proxy.host.clone(), cfg.proxy.port), (None, 3456), "no flags, no change");

        apply_listen_override(&mut cfg, None, Some(9001)).unwrap();
        assert_eq!((cfg.proxy.host.clone(), cfg.proxy.port), (None, 9001), "--port leaves the host alone");

        apply_listen_override(&mut cfg, Some(":9002"), None).unwrap();
        assert_eq!((cfg.proxy.host.clone(), cfg.proxy.port), (None, 9002));

        apply_listen_override(&mut cfg, Some("127.0.0.2:9003"), None).unwrap();
        assert_eq!((cfg.proxy.host.as_deref(), cfg.proxy.port), (Some("127.0.0.2"), 9003));

        // Opening the port to the network by flag faces the same key check the
        // config file does.
        let mut cfg = Config::default();
        cfg.proxy.api_key = "short".into();
        assert!(apply_listen_override(&mut cfg, Some("0.0.0.0:9004"), None).is_err());
        assert!(apply_listen_override(&mut cfg, Some("127.0.0.1:9004"), None).is_ok());
    }

    /// Re-importing must refresh the account corrall already has rather than
    /// add a second copy of the same credential, and it must leave the
    /// corrall-only settings on that entry in place.
    #[test]
    fn importing_twice_updates_one_account() {
        let mut cfg = Config::default();
        let src = AccountConfig {
            name: "a@example.com".into(),
            kind: AccountType::Oauth,
            account_uuid: Some("au".into()),
            priority: 2,
            access_token: Some("at".into()),
            ..Default::default()
        };
        let pool = cfg.default_pool.clone();
        assert!(!upsert_acrobat(&mut cfg, &pool, &src));
        cfg.pool_mut(&pool).unwrap().accounts[0].upstream = Some("https://alt.example/v1".into());

        // Same uuid, new name and token: one account, refreshed.
        let src = AccountConfig { name: "renamed".into(), access_token: Some("at2".into()), ..src };
        assert!(upsert_acrobat(&mut cfg, &pool, &src));
        let accounts = &cfg.pool(&pool).unwrap().accounts;
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "renamed");
        assert_eq!(accounts[0].access_token.as_deref(), Some("at2"));
        assert_eq!(accounts[0].upstream.as_deref(), Some("https://alt.example/v1"), "corrall-only settings survive");
    }

    /// An API-key account is a different thing wearing the same name; an import
    /// must not adopt it.
    #[test]
    fn an_api_key_account_of_the_same_name_is_left_alone() {
        let mut cfg = Config::default();
        let pool = cfg.default_pool.clone();
        cfg.pool_mut(&pool).unwrap().accounts.push(AccountConfig {
            name: "shared".into(),
            kind: AccountType::Apikey,
            api_key: Some("sk-ant-key".into()),
            ..Default::default()
        });
        assert!(!is_anthropic_oauth(&cfg.pool(&pool).unwrap().accounts[0]));

        let src = AccountConfig { name: "shared".into(), kind: AccountType::Oauth, access_token: Some("at".into()), ..Default::default() };
        assert!(!upsert_acrobat(&mut cfg, &pool, &src), "it must be added, not matched");
        let accounts = &cfg.pool(&pool).unwrap().accounts;
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].api_key.as_deref(), Some("sk-ant-key"));
    }

    #[test]
    fn a_quoted_list_reads_as_a_sentence() {
        assert_eq!(quoted_list(&[]), "");
        assert_eq!(quoted_list(&["a"]), "\"a\"");
        assert_eq!(quoted_list(&["a", "b"]), "\"a\" and \"b\"");
        assert_eq!(quoted_list(&["a", "b", "c"]), "\"a\", \"b\" and \"c\"");
    }
}
