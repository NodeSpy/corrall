//! Outbound HTTP client shared by the forwarder, the OAuth flows and the
//! quota probe. One pooled client per process; honours `upstreamProxy` /
//! `HTTPS_PROXY`; never disables certificate verification.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::Config;

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub fn headers_timeout() -> Duration {
    env_ms("CORRALL_UPSTREAM_HEADERS_TIMEOUT_MS", 120_000)
}

pub fn body_idle_timeout() -> Duration {
    env_ms("CORRALL_UPSTREAM_BODY_TIMEOUT_MS", 120_000)
}

pub fn refresh_timeout() -> Duration {
    env_ms("CORRALL_REFRESH_TIMEOUT_MS", 30_000)
}

fn env_ms(name: &str, default: u64) -> Duration {
    Duration::from_millis(std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()).filter(|v| *v > 0).unwrap_or(default))
}

/// Install the process-wide client. Must be called once before `client()`.
pub fn init(cfg: &Config) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut b = reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(20))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(std::env::var("CORRALL_UPSTREAM_MAX_SOCKETS").ok().and_then(|v| v.parse().ok()).unwrap_or(256))
        .tcp_keepalive(Duration::from_secs(30))
        .http1_only()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .user_agent(format!("corrall/{}", env!("CARGO_PKG_VERSION")));

    match resolve_proxy(cfg) {
        ProxySetting::Env => {}
        ProxySetting::None => b = b.no_proxy(),
        ProxySetting::Url(u) => {
            let self_port = cfg.proxy.port;
            if let Ok(parsed) = url::Url::parse(&u) {
                let host = parsed.host_str().unwrap_or("");
                if crate::security::is_loopback_host(host) && parsed.port() == Some(self_port) {
                    tracing::warn!("upstreamProxy points at this proxy's own listener; ignoring it");
                    b = b.no_proxy();
                } else {
                    let mut p = reqwest::Proxy::all(&u).context("upstreamProxy is not a valid proxy URL")?;
                    if let Some(np) = cfg.no_proxy.clone().or_else(|| std::env::var("NO_PROXY").ok()) {
                        p = p.no_proxy(reqwest::NoProxy::from_string(&np));
                    }
                    b = b.no_proxy().proxy(p);
                }
            }
        }
    }
    let c = b.build().context("building HTTP client")?;
    let _ = CLIENT.set(c);
    Ok(())
}

pub fn client() -> reqwest::Client {
    CLIENT
        .get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .use_rustls_tls()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(20))
                .build()
                .expect("default client")
        })
        .clone()
}

enum ProxySetting {
    Env,
    None,
    Url(String),
}

fn resolve_proxy(cfg: &Config) -> ProxySetting {
    match cfg.upstream_proxy.as_deref() {
        None => ProxySetting::Env,
        Some("") | Some("false") | Some("off") => ProxySetting::None,
        Some(u) => {
            let u = if u.contains("://") { u.to_string() } else { format!("http://{u}") };
            ProxySetting::Url(u)
        }
    }
}

pub fn describe_proxy(cfg: &Config) -> String {
    match resolve_proxy(cfg) {
        ProxySetting::Env => std::env::var("HTTPS_PROXY")
            .or_else(|_| std::env::var("https_proxy"))
            .or_else(|_| std::env::var("ALL_PROXY"))
            .map(|v| format!("{} (from environment)", redact_url(&v)))
            .unwrap_or_else(|_| "direct".into()),
        ProxySetting::None => "direct (environment ignored)".into(),
        ProxySetting::Url(u) => redact_url(&u),
    }
}

fn redact_url(u: &str) -> String {
    match url::Url::parse(u) {
        Ok(mut p) => {
            if p.password().is_some() {
                let _ = p.set_password(Some("***"));
            }
            p.to_string()
        }
        Err(_) => u.to_string(),
    }
}
