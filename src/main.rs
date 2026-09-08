//! TeamClaude: multi-account Claude proxy with quota-based rotation.

use teamclaude::*;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use parking_lot::RwLock;

use cli::{Cli, Command, ServerArgs};
use config::{Config, State};
use pools::Pools;
use proxy::server::{Ctx, CtxInner, Metrics};

fn init_logging(json: bool, to_stderr_only: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("TEAMCLAUDE_LOG").unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,rustls=warn,reqwest=warn"));
    let builder = fmt().with_env_filter(filter).with_target(false).with_writer(std::io::stderr);
    let _ = to_stderr_only;
    if json {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.compact().try_init();
    }
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let result = rt.block_on(dispatch(cli));
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    let json_logs = cli.log_format == "json";
    match cli.command.unwrap_or(Command::Server(ServerArgs::default())) {
        Command::Server(args) => {
            let interactive = !args.headless && std::io::IsTerminal::is_terminal(&std::io::stdout());
            if !interactive {
                init_logging(json_logs, true);
            }
            server(args, interactive).await
        }
        Command::Login(a) => {
            init_logging(json_logs, true);
            cli::login(a).await
        }
        Command::Import(a) => {
            init_logging(json_logs, true);
            cli::import(a).await
        }
        Command::Accounts { verbose, pool } => cli::accounts(verbose, pool),
        Command::Status { json } => cli::status(json).await,
        Command::Switch { name, pool } => cli::switch(name, pool).await,
        Command::Remove { name, pool } => cli::remove(name, pool).await,
        Command::Disable { name, pool } => cli::set_disabled(name, true, pool).await,
        Command::Enable { name, pool } => cli::set_disabled(name, false, pool).await,
        Command::Priority { name, value, first, last, pool } => cli::priority(name, value, first, last, pool).await,
        Command::Threshold { value, pool } => cli::threshold(value, pool).await,
        Command::Distribute { value, pool } => cli::distribute(value, pool).await,
        Command::Probe { value, pool } => cli::probe(value, pool).await,
        Command::Warmup { value } => cli::warmup(value).await,
        Command::Expiry { value, tolerance, preempt, pool } => cli::expiry(value, tolerance, preempt, pool).await,
        Command::Titles { value } => cli::titles(value).await,
        Command::Route { cmd, pool } => cli::route(cmd, pool).await,
        Command::Pool { cmd } => cli::pool(cmd).await,
        Command::Env(a) => cli::env(a),
        Command::Run(a) => cli::run(a).await,
        Command::CaPath => cli::ca_path(),
        Command::Config { cmd } => cli::config_cmd(cmd),
        Command::Service { cmd } => cli::service(cmd),
        Command::Api { path, account, pool } => cli::api(path, account, pool).await,
    }
}

async fn server(args: ServerArgs, interactive: bool) -> Result<()> {
    let mut cfg = Config::load_or_create()?;
    if let Some(dir) = &args.log_to {
        cfg.log_dir = Some(dir.clone());
    }
    upstream::init(&cfg)?;
    let bind = proxy::server::parse_bind(&cfg.bind_host(), cfg.proxy.port)?;
    if !security::is_loopback_ip(bind.ip()) {
        tracing::warn!("binding {bind}: remote clients must present proxy.apiKey; put a TLS terminator in front on untrusted networks");
    }

    let pools = Pools::new(&cfg);
    match State::load() {
        Ok(Some(st)) => pools.restore_state(&st),
        Ok(None) => {}
        Err(e) => tracing::warn!("state file ignored: {e}"),
    }
    if pools.account_count() == 0 {
        tracing::warn!("no usable accounts configured; run `teamclaude login` (the server will serve them after a reload)");
    }

    let logger = cfg.log_dir.as_deref().and_then(|d| proxy::log::RequestLogger::new(d, cfg.log_level, cfg.log_max_body_bytes));
    if let Some(l) = &logger {
        l.sweep(cfg.log_retention_hours);
    }
    let (activity_tx, _) = tokio::sync::broadcast::channel(512);
    let prober = prober::Prober::new(pools.clone());
    let warmer = warmer::Warmer::new(pools.clone(), cfg.proxy.port, &cfg.proxy.api_key, cfg.warmup_seconds);
    let titles = titles::Titles::new(&cfg.session_titles);

    let ctx_cell: Arc<parking_lot::Mutex<Option<Ctx>>> = Arc::new(parking_lot::Mutex::new(None));
    let reload: Box<dyn Fn() -> Result<usize> + Send + Sync> = {
        let pools = pools.clone();
        let warmer = warmer.clone();
        let titles = titles.clone();
        let cell = ctx_cell.clone();
        Box::new(move || {
            let cfg = Config::load()?.context("config file disappeared")?;
            // Per-pool probe intervals ride along on each manager, so the
            // prober needs no separate notification.
            let added = pools.sync_config(&cfg);
            warmer.set_interval(cfg.warmup_seconds);
            warmer.set_api_key(&cfg.proxy.api_key);
            titles.configure(&cfg.session_titles);
            if let Some(ctx) = cell.lock().clone() {
                ctx.set_config(cfg);
                *ctx.tls.write() = None;
            }
            Ok(added)
        })
    };
    let ctx = Ctx(Arc::new(CtxInner {
        pools: pools.clone(),
        config: RwLock::new(Arc::new(cfg.clone())),
        logger: logger.clone(),
        activity: activity_tx.clone(),
        reload: Some(reload),
        metrics: Metrics::default(),
        tls: RwLock::new(None),
        titles: titles.clone(),
    }));
    *ctx_cell.lock() = Some(ctx.clone());

    // Pre-mint the MITM chain so the first CONNECT does not pay for it.
    if let Err(e) = ctx.tls_config() {
        tracing::warn!("MITM disabled until certificates can be written: {e}");
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Listener.
    let srv = tokio::spawn(proxy::server::run(ctx.clone(), bind, shutdown_rx.clone()));

    // Background: prober, state saver, log sweeper, signals.
    tokio::spawn(prober.clone().run());
    tokio::spawn(warmer.clone().run());
    {
        let pools = pools.clone();
        let mut rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if let Err(e) = pools.export_state().save() { tracing::warn!("state save failed: {e}"); }
                    }
                    _ = rx.changed() => break,
                }
            }
        });
    }
    if let Some(l) = logger.clone() {
        let hours = cfg.log_retention_hours;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(600));
            loop {
                tick.tick().await;
                l.sweep(hours);
            }
        });
    }
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
                tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = ctrl_c.await;
            }
            let _ = tx.send(true);
        });
    }
    // Forward every pool's log lines into the activity stream.
    {
        let mut ev = pools.events.subscribe();
        let tx = activity_tx.clone();
        tokio::spawn(async move {
            while let Ok(m) = ev.recv().await {
                let _ = tx.send(proxy::server::Activity::Log(m));
            }
        });
    }

    let activity_file = args
        .activity_log
        .as_ref()
        .map(|p| {
            let f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            security::set_mode(std::path::Path::new(p), 0o600);
            Ok::<_, std::io::Error>(f)
        })
        .transpose()
        .context("opening --activity-log")?;

    let pool_note = match pools.names().len() {
        0 | 1 => String::new(),
        n => format!(", {n} pools"),
    };
    tracing::info!(
        "TeamClaude v{} listening on http://{bind} ({} accounts{pool_note}){}",
        env!("CARGO_PKG_VERSION"),
        pools.account_count(),
        if interactive { "" } else { " [headless]" }
    );
    if interactive {
        let t = tui::Tui::new(ctx.clone(), prober.clone(), activity_file);
        let mut tx = shutdown_tx.clone();
        t.run(std::mem::replace(&mut tx, shutdown_tx.clone())).await?;
    } else {
        // Headless: print activity lines to stderr (and the file).
        let mut rx = activity_tx.subscribe();
        let mut file = activity_file;
        let mut sd = shutdown_rx.clone();
        loop {
            tokio::select! {
                Ok(a) = rx.recv() => {
                    let line = match a {
                        proxy::server::Activity::Start { id, method, path, model, session, client } => {
                            let label = ctx.titles.label(session.as_deref(), quota::now_ms());
                            format!("→ {id} {}{label} {method} {path}{}", client.map(|c| format!("[{c}] ")).unwrap_or_default(), model.map(|m| format!(" ({m})")).unwrap_or_default())
                        }
                        proxy::server::Activity::Account { .. } => continue,
                        proxy::server::Activity::End { id, account, status, elapsed_ms, ok } => format!("{} {id} → {account} {status} ({:.1}s)", if ok { "✓" } else { "✗" }, elapsed_ms as f64 / 1000.0),
                        proxy::server::Activity::Log(s) => s,
                    };
                    let line = security::safe_text(&line, 400);
                    tracing::info!("{line}");
                    if let Some(f) = &mut file { use std::io::Write; let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S")); }
                }
                _ = sd.changed() => { if *sd.borrow() { break; } }
            }
        }
    }

    // Shutdown: persist state.
    let _ = shutdown_tx.send(true);
    if let Err(e) = pools.export_state().save() {
        tracing::warn!("final state save failed: {e}");
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), srv).await;
    tracing::info!("stopped");
    Ok(())
}
