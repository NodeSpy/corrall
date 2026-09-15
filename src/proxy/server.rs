//! The local listener: one port serving plain HTTP requests (base-URL mode),
//! the control plane under `/corrall/*`, and `CONNECT` tunnels for the MITM
//! mode. Every accepted connection is dispatched by `serve_connection`.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use super::auth::{authenticate, authenticate_connect, Auth};
use super::forward::{error_response, json_response};
use super::log::RequestLogger;
use super::mitm::{self, HostMode};
use crate::config::{Config, EventLogging};
use crate::manager::{Manager, Provider};
use crate::pools::Pools;

pub type BoxBody = http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

const PIN_PREFIX: &str = "/tc-acct/";
const CONTROL_BODY_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct ReqInfo {
    pub id: String,
    pub method: String,
    pub path_and_query: String,
    pub model: Option<String>,
    pub advisor_model: Option<String>,
    pub session_id: Option<String>,
    pub client: Option<String>,
    pub pin: Option<String>,
    /// The pool serving this request — always a live pool name, never the
    /// unknown name a client may have asked for.
    pub pool: String,
    /// The serving pool's `holdSeconds`, in milliseconds.
    pub hold_ms: u64,
    #[allow(dead_code)]
    pub started: std::time::Instant,
    pub provider: Provider,
    /// The request body parsed once; rewrites work on this.
    pub parsed: Option<Arc<super::body::Obj>>,
    /// (dimension name, sanitized value) pairs from configured usage headers.
    pub dimensions: Vec<(String, String)>,
}

/// Activity events for the TUI / activity log.
#[derive(Debug, Clone)]
pub enum Activity {
    Start { id: String, method: String, path: String, model: Option<String>, session: Option<String>, client: Option<String> },
    Account { id: String, account: String },
    End { id: String, account: String, status: u16, elapsed_ms: u128, ok: bool },
    Log(String),
}

pub struct CtxInner {
    pub pools: Arc<Pools>,
    pub config: RwLock<Arc<Config>>,
    pub logger: Option<RequestLogger>,
    pub activity: tokio::sync::broadcast::Sender<Activity>,
    pub reload: Option<Box<dyn Fn() -> Result<usize> + Send + Sync>>,
    pub metrics: Metrics,
    pub tls: RwLock<Option<Arc<tokio_rustls::rustls::ServerConfig>>>,
    pub titles: crate::titles::Titles,
    /// In-flight manual OAuth logins (`/corrall/login/{start,submit}`), keyed by
    /// the flow id handed to the caller. Entries are one-shot and swept on TTL.
    pub oauth_flows: Mutex<HashMap<String, PendingFlow>>,
}

/// A manual OAuth login awaiting its pasted code. The PKCE material lives here
/// between the `start` call that built the authorize URL and the `submit` call
/// that exchanges the code, since the two arrive on different connections.
pub struct PendingFlow {
    pub login: crate::oauth::ManualLogin,
    pub pool: Option<String>,
    pub name: Option<String>,
    /// ms since epoch, for TTL eviction.
    pub created_at: i64,
}

/// How long a started login may sit before `submit` must arrive.
const OAUTH_FLOW_TTL_MS: i64 = 15 * 60 * 1000;

#[derive(Default)]
pub struct Metrics {
    pub requests_total: std::sync::atomic::AtomicU64,
    pub requests_failed: std::sync::atomic::AtomicU64,
    pub auth_failures: std::sync::atomic::AtomicU64,
    pub connects_total: std::sync::atomic::AtomicU64,
}

#[derive(Clone)]
pub struct Ctx(pub Arc<CtxInner>);

impl std::ops::Deref for Ctx {
    type Target = CtxInner;
    fn deref(&self) -> &CtxInner {
        &self.0
    }
}

impl Ctx {
    pub fn config(&self) -> Arc<Config> {
        self.config.read().clone()
    }

    pub fn set_config(&self, c: Config) {
        *self.config.write() = Arc::new(c);
    }

    /// The pool serving requests that name no pool. Most callers that used to
    /// reach for "the" manager want this one.
    pub fn manager(&self) -> Manager {
        self.pools.default()
    }

    /// The manager for a request's pool. `info.pool` is always a live name, so
    /// this is a lookup rather than a fallback.
    pub fn manager_for(&self, pool: &str) -> Manager {
        self.pools.resolve(Some(pool)).1
    }

    pub fn dimension_headers(&self) -> HashSet<String> {
        self.config().proxy.usage_dimensions.iter().map(|d| d.header.to_ascii_lowercase()).collect()
    }

    pub fn notify_account(&self, info: &ReqInfo, account: &str) {
        let _ = self.activity.send(Activity::Account { id: info.id.clone(), account: account.to_string() });
    }

    pub fn notify_end(&self, info: &ReqInfo, account: &str, status: u16, elapsed: Duration, ok: bool) {
        use std::sync::atomic::Ordering;
        self.metrics.requests_total.fetch_add(1, Ordering::Relaxed);
        if !ok {
            self.metrics.requests_failed.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.activity.send(Activity::End { id: info.id.clone(), account: account.to_string(), status, elapsed_ms: elapsed.as_millis(), ok });
    }

    pub fn intercept_hosts(&self) -> Vec<String> {
        let cfg = self.config();
        let mut hosts = vec![];
        if let Ok(u) = url::Url::parse(&cfg.upstream) {
            if let Some(h) = u.host_str() {
                hosts.push(h.to_string());
            }
        }
        if !hosts.iter().any(|h| h == "api.anthropic.com") {
            hosts.push("api.anthropic.com".to_string());
        }
        if cfg.all_accounts().any(|(_, a)| a.is_codex()) {
            hosts.push(crate::codex::HOST.to_string());
        }
        hosts
    }

    /// Lazily build (and cache) the MITM TLS config.
    pub fn tls_config(&self) -> Result<Arc<tokio_rustls::rustls::ServerConfig>> {
        if let Some(t) = self.tls.read().clone() {
            return Ok(t);
        }
        let certs = mitm::ensure_certs(&self.intercept_hosts())?;
        let cfg = mitm::tls_config(&certs, self.config().mitm.http1_only)?;
        *self.tls.write() = Some(cfg.clone());
        Ok(cfg)
    }
}

pub async fn run(ctx: Ctx, bind: SocketAddr, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
    let listener = TcpListener::bind(bind).await.with_context(|| format!("binding {bind}"))?;
    tracing::info!("listening on http://{bind}");
    loop {
        tokio::select! {
            r = listener.accept() => {
                let (stream, peer) = match r {
                    Ok(x) => x,
                    Err(e) => { tracing::warn!("accept: {e}"); tokio::time::sleep(Duration::from_millis(50)).await; continue; }
                };
                let _ = stream.set_nodelay(true);
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    serve_connection(ctx, stream, peer.ip(), None).await;
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
        }
    }
    Ok(())
}

/// Serve HTTP/1.1 on any stream (the raw TCP socket, or a terminated TLS
/// tunnel). `forced_pin` carries a CONNECT-level account pin into the tunnel.
pub fn serve_connection<S>(ctx: Ctx, stream: S, peer: IpAddr, tunnel: Option<TunnelCtx>) -> futures_util::future::BoxFuture<'static, ()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Box::pin(async move {
        let io = TokioIo::new(stream);
        let tunnel = Arc::new(tunnel);
        let svc = service_fn(move |req: Request<Incoming>| {
            let ctx = ctx.clone();
            let tunnel = tunnel.clone();
            async move { Ok::<_, hyper::Error>(handle(ctx, req, peer, tunnel.as_ref().as_ref()).await) }
        });
        let conn = http1::Builder::new().keep_alive(true).preserve_header_case(true).max_buf_size(1024 * 1024).serve_connection(io, svc).with_upgrades();
        if let Err(e) = conn.await {
            let s = e.to_string();
            if !s.contains("connection closed") && !s.contains("reset") && !s.contains("broken pipe") {
                tracing::debug!("connection ended: {s}");
            }
        }
    })
}

#[derive(Debug, Clone)]
pub struct TunnelCtx {
    pub auth: Auth,
    pub pin: Option<String>,
    /// Pool chosen at CONNECT time, from the proxy username's `~<pool>` suffix.
    pub pool: Option<String>,
    #[allow(dead_code)]
    pub host: String,
}

fn host_header(req: &Request<Incoming>) -> Option<String> {
    req.headers().get("host").and_then(|v| v.to_str().ok()).map(str::to_string).or_else(|| req.uri().authority().map(|a| a.to_string()))
}

async fn handle(ctx: Ctx, req: Request<Incoming>, peer: IpAddr, tunnel: Option<&TunnelCtx>) -> Response<BoxBody> {
    if req.method() == Method::CONNECT {
        return handle_connect(ctx, req, peer).await;
    }

    // Absolute-form URL = a plain-HTTP forward-proxy request. We never relay
    // those: the only hosts we manage are HTTPS.
    if req.uri().scheme().is_some() && tunnel.is_none() {
        return error_response(StatusCode::FORBIDDEN, "permission_error", "plain-HTTP forward proxying is not supported");
    }

    let cfg = ctx.config();
    let path = req.uri().path().to_string();
    let path_and_query = req.uri().path_and_query().map(|p| p.to_string()).unwrap_or_else(|| "/".into());

    // Pool keyword. `/pool/<name>` is stripped here, before anything else looks
    // at the path, so it composes with the control plane, the passthrough list
    // and the deprecated `/tc-acct/` pin alike.
    let (asked_pool, path, path_and_query) = match crate::pools::parse_pool_path(&path_and_query) {
        Some((name, rest)) => {
            let path = rest.split(['?', '#']).next().unwrap_or("/").to_string();
            (Some(name.to_string()), path, rest)
        }
        None => (None, path, path_and_query),
    };
    // In MITM mode there is no local URL to carry the keyword, so the pool
    // comes from the CONNECT username instead. An explicit keyword still wins.
    let asked_pool = asked_pool.or_else(|| tunnel.and_then(|t| t.pool.clone()));

    // The dashboard page is a static asset with no data in it; everything it
    // shows is fetched with the key. Serving it unauthenticated lets a browser
    // load it, which an address bar cannot do with a header.
    if req.method() == Method::GET && path == "/corrall/dashboard" && tunnel.is_none() {
        let mut r = Response::new(Full::new(Bytes::from_static(super::dashboard::HTML.as_bytes())).map_err(|e| match e {}).boxed());
        r.headers_mut().insert("content-type", HeaderValue::from_static("text/html; charset=utf-8"));
        r.headers_mut().insert("cache-control", HeaderValue::from_static("no-store"));
        r.headers_mut().insert(
            "content-security-policy",
            HeaderValue::from_static("default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'"),
        );
        return r;
    }

    // Auth. Inside a tunnel the CONNECT already authenticated.
    let auth = match tunnel {
        Some(t) => t.auth.clone(),
        None => authenticate(&cfg.proxy, peer, req.headers(), host_header(&req).as_deref()),
    };
    if let Auth::Denied(why) = &auth {
        ctx.metrics.auth_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!("denied {} {} from {peer}: {why}", req.method(), path);
        // Slow down brute force without blocking the runtime.
        tokio::time::sleep(Duration::from_millis(250)).await;
        return error_response(StatusCode::UNAUTHORIZED, "authentication_error", "Invalid proxy API key");
    }

    // Control plane.
    if path.starts_with("/corrall/") && tunnel.is_none() {
        return control(&ctx, req, &auth, asked_pool.as_deref()).await;
    }

    let (pool, manager) = ctx.pools.resolve_request(asked_pool.as_deref());

    // Passthrough paths carry the client's own upstream session (its token
    // refresh, Remote Control): no account is selected and no credential
    // injected. Anthropic only; Codex has no such paths.
    let provider = match tunnel {
        Some(t) => Provider::for_host(&t.host),
        None => Provider::for_path(&path),
    };
    if provider == Provider::Anthropic && super::relay::is_passthrough_path(&path) {
        let upstream = cfg.upstream.clone();
        let is_upgrade = req.headers().get("upgrade").and_then(|v| v.to_str().ok()).map(|u| u.eq_ignore_ascii_case("websocket")).unwrap_or(false);
        let _ = ctx.activity.send(Activity::Log(format!("passthrough {} {}", req.method(), crate::security::safe_text(&path, 100))));
        if is_upgrade {
            return super::relay::relay_upgrade(&upstream, req).await;
        }
        let (parts, body) = req.into_parts();
        let body = match read_body(body, CONTROL_BODY_LIMIT * 16).await {
            Ok(b) => b,
            Err(_) => return error_response(StatusCode::PAYLOAD_TOO_LARGE, "invalid_request_error", "request body too large"),
        };
        return super::relay::passthrough(&upstream, parts, body).await;
    }

    // Deprecated path pin.
    let (pin, path_and_query) = if let Some(rest) = path_and_query.strip_prefix(PIN_PREFIX) {
        match rest.split_once('/') {
            Some((token, tail)) => {
                let tok = percent_encoding::percent_decode_str(token).decode_utf8_lossy().to_string();
                (Some(tok), format!("/{tail}"))
            }
            None => (None, path_and_query),
        }
    } else {
        (tunnel.and_then(|t| t.pin.clone()), path_and_query)
    };

    // Telemetry noise.
    if path.starts_with("/api/event_logging") && cfg.event_logging == EventLogging::Block {
        return json_response(StatusCode::OK, json!({}));
    }

    // Read the body with a hard cap.
    let (parts, body) = req.into_parts();
    let limit = cfg.proxy.max_body_bytes as usize;
    let body = match read_body(body, limit).await {
        Ok(b) => b,
        Err(too_large) => {
            return if too_large {
                error_response(StatusCode::PAYLOAD_TOO_LARGE, "invalid_request_error", "request body too large")
            } else {
                error_response(StatusCode::BAD_REQUEST, "invalid_request_error", "could not read request body")
            }
        }
    };

    let parsed = if body.is_empty() || provider == Provider::Codex { None } else { super::body::parse(&body).map(Arc::new) };
    let (model, advisor) = match &parsed {
        Some(m) => {
            let main = m.get("model").and_then(serde_json::Value::as_str).map(str::to_string);
            let adv = m.get("tools").and_then(serde_json::Value::as_array).and_then(|tools| {
                tools.iter().find_map(|t| {
                    let ty = t.get("type")?.as_str()?;
                    if ty.to_ascii_lowercase().starts_with("advisor") {
                        t.get("model")?.as_str().map(str::to_string)
                    } else {
                        None
                    }
                })
            });
            (main, adv)
        }
        None if provider == Provider::Codex => (crate::model::request_model(&body), None),
        None => (None, None),
    };
    let dimensions: Vec<(String, String)> = cfg
        .proxy
        .usage_dimensions
        .iter()
        .filter_map(|d| {
            let v = parts.headers.get(d.header.to_ascii_lowercase().as_str())?.to_str().ok()?;
            let v = crate::security::safe_text(v.trim(), 200);
            if v.is_empty() {
                None
            } else {
                Some((d.name.clone(), v))
            }
        })
        .collect();
    if let Some(m) = &model {
        if manager.is_model_blocked(m) {
            return error_response(StatusCode::BAD_REQUEST, "invalid_request_error", "This model is blocked by the proxy configuration (blockedModels)");
        }
    }
    let session_id =
        parts.headers.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).filter(|s| crate::session::valid_session_id(s)).map(str::to_string);
    let client = auth.client_name().map(str::to_string);
    let info = ReqInfo {
        id: short_id(),
        method: parts.method.to_string(),
        path_and_query,
        model: model.map(|m| crate::security::safe_text(&m, 120)),
        advisor_model: advisor,
        session_id,
        client,
        pin,
        hold_ms: manager.hold_seconds().saturating_mul(1000),
        pool,
        started: std::time::Instant::now(),
        provider,
        parsed,
        dimensions,
    };

    let show = !(path.starts_with("/api/event_logging") && cfg.event_logging == EventLogging::Hide);
    if show {
        let _ = ctx.activity.send(Activity::Start {
            id: info.id.clone(),
            method: info.method.clone(),
            path: crate::security::safe_text(&path, 100),
            model: info.model.clone(),
            session: info.session_id.clone(),
            client: info.client.clone(),
        });
    }
    manager.begin_session_request(info.session_id.as_deref(), info.client.as_deref());
    let resp = super::forward::forward(&ctx, &manager, &info, parts.headers, body).await;
    manager.end_session_request(info.session_id.as_deref());
    resp
}

async fn read_body(body: Incoming, limit: usize) -> std::result::Result<Bytes, bool> {
    let mut out: Vec<u8> = Vec::new();
    let mut b = body;
    while let Some(frame) = b.frame().await {
        let f = frame.map_err(|_| false)?;
        if let Ok(d) = f.into_data() {
            if out.len() + d.len() > limit {
                return Err(true);
            }
            out.extend_from_slice(&d);
        }
    }
    Ok(Bytes::from(out))
}

fn short_id() -> String {
    crate::security::random_key(6).to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect()
}

// ── control plane ─────────────────────────────────────────────

async fn control(ctx: &Ctx, req: Request<Incoming>, auth: &Auth, asked_pool: Option<&str>) -> Response<BoxBody> {
    let cfg = ctx.config();
    let path = req.uri().path().to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::GET, "/corrall/health") => json_response(StatusCode::OK, json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })),
        (Method::GET, "/corrall/status") => {
            let mut st = ctx.pools.status(cfg.proxy.session_detail);
            st["upstream"] = json!(cfg.upstream);
            st["upstreamProxy"] = json!(crate::upstream::describe_proxy(&cfg));
            st["mitm"] = json!({ "caPath": mitm::ca_cert_path(), "http1Only": cfg.mitm.http1_only, "allowTunnel": cfg.mitm.allow_tunnel });
            st["client"] = json!(auth.client_name());
            st["sessionTitles"] = json!(cfg.session_titles);
            st["warmupSeconds"] = json!(cfg.warmup_seconds);
            json_response(StatusCode::OK, st)
        }
        (Method::GET, "/corrall/quota") => {
            let (name, m) = ctx.pools.resolve_request(asked_pool);
            let mut q = m.quota_summary();
            if let Some(o) = q.as_object_mut() {
                o.insert("pool".to_string(), json!(name));
            }
            json_response(StatusCode::OK, q)
        }
        (Method::GET, "/corrall/metrics") => {
            let mut r = Response::new(Full::new(Bytes::from(render_metrics(ctx))).map_err(|e| match e {}).boxed());
            r.headers_mut().insert("content-type", HeaderValue::from_static("text/plain; version=0.0.4"));
            r
        }
        (Method::POST, "/corrall/reload") => match &ctx.reload {
            None => json_response(StatusCode::NOT_IMPLEMENTED, json!({ "ok": false, "error": "reload not supported" })),
            Some(f) => match f() {
                Ok(added) => json_response(StatusCode::OK, json!({ "ok": true, "added": added })),
                Err(e) => {
                    tracing::error!("reload failed: {e}");
                    json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "ok": false, "error": "reload failed; see server log" }))
                }
            },
        },
        (Method::POST, "/corrall/switch") => {
            let body = match read_body(req.into_body(), CONTROL_BODY_LIMIT).await {
                Ok(b) => b,
                Err(true) => return json_response(StatusCode::PAYLOAD_TOO_LARGE, json!({ "ok": false, "error": "request body too large" })),
                Err(false) => return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "invalid request body" })),
            };
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            // A pool from the body, the URL keyword, or — with neither — search
            // every pool so `corrall switch <name>` keeps working unqualified.
            let asked = v.get("pool").and_then(Value::as_str).or(asked_pool);
            let scoped = asked.map(|p| ctx.pools.resolve_request(Some(p)));
            let names: Vec<String> = match &scoped {
                Some((_, m)) => m.account_ids().into_iter().map(|(_, n)| n).collect(),
                None => ctx.pools.each().into_iter().flat_map(|(_, m)| m.account_ids().into_iter().map(|(_, n)| n)).collect(),
            };
            let Some(target) = v.get("account").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
                return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "missing \"account\"", "accounts": names }));
            };
            let found = match &scoped {
                Some((pool, m)) => m.resolve_pin(target).map(|id| (pool.clone(), m.clone(), id)),
                None => ctx.pools.find_account(target),
            };
            let Some((pool, mgr, id)) = found else {
                return json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "unknown account", "accounts": names }));
            };
            match mgr.switch_to(&id) {
                Some((name, reason)) => {
                    mgr.log(format!("Switched to account \"{name}\" by request"));
                    json_response(StatusCode::OK, json!({ "ok": true, "pool": pool, "account": name, "effective": reason.is_none(), "blocked": reason }))
                }
                None => json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "unknown account" })),
            }
        }
        (Method::POST, "/corrall/route-pin") => {
            let body = match read_body(req.into_body(), CONTROL_BODY_LIMIT).await {
                Ok(b) => b,
                Err(_) => return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "invalid request body" })),
            };
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let Some(route) = v.get("route").and_then(Value::as_str) else {
                return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "missing \"route\"" }));
            };
            // Routes are per-pool, so an account name resolves within the
            // pool that owns the route.
            let asked = v.get("pool").and_then(Value::as_str).or(asked_pool);
            let (pool, mgr) = ctx.pools.resolve_request(asked);
            let id = match v.get("account").and_then(Value::as_str) {
                Some(a) => match mgr.resolve_pin(a) {
                    Some(id) => Some(id),
                    None => return json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "unknown account" })),
                },
                None => None,
            };
            mgr.set_route_pin(route, id.as_deref());
            json_response(StatusCode::OK, json!({ "ok": true, "pool": pool }))
        }
        (Method::POST, "/corrall/pools") => match control_body(req).await {
            Ok(v) => pool_create(ctx, &v),
            Err(r) => *r,
        },
        (Method::POST, "/corrall/login/start") => match control_body(req).await {
            Ok(v) => login_start(ctx, &v),
            Err(r) => *r,
        },
        (Method::POST, "/corrall/login/submit") => match control_body(req).await {
            Ok(v) => login_submit(ctx, &v).await,
            Err(r) => *r,
        },
        (Method::POST, "/corrall/login/cancel") => match control_body(req).await {
            Ok(v) => login_cancel(ctx, &v),
            Err(r) => *r,
        },
        // Parameterised account/pool routes: the arms above match literal paths,
        // so these are dispatched by prefix here.
        (Method::POST, _) if path.starts_with("/corrall/pools/") => {
            let sub = path.trim_start_matches("/corrall/pools/").to_string();
            match control_body(req).await {
                Ok(v) => pools_param(ctx, &sub, &v),
                Err(r) => *r,
            }
        }
        _ => json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "not found" })),
    }
}

/// Read a control-route request body and parse it as JSON, defaulting to `{}`
/// on an empty/absent body. Returns a ready error response on an oversized or
/// unreadable body, matching the other control routes' conventions. The error
/// is boxed so the `Ok` path does not carry a full `Response` in its size.
async fn control_body(req: Request<Incoming>) -> std::result::Result<Value, Box<Response<BoxBody>>> {
    match read_body(req.into_body(), CONTROL_BODY_LIMIT).await {
        Ok(b) => Ok(serde_json::from_slice(&b).unwrap_or(json!({}))),
        Err(true) => Err(Box::new(json_response(StatusCode::PAYLOAD_TOO_LARGE, json!({ "ok": false, "error": "request body too large" })))),
        Err(false) => Err(Box::new(json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "invalid request body" })))),
    }
}

/// Re-apply a just-persisted config change to the running fleet. In the daemon
/// this runs the same reload the `/corrall/reload` route uses (re-read the file,
/// re-sync every pool, swap the live config). With no reload wired — e.g. in
/// tests — it syncs the already-loaded config in place.
fn reconcile(ctx: &Ctx, cfg: Config) {
    match &ctx.reload {
        Some(reload) => {
            if let Err(e) = reload() {
                tracing::error!("post-mutation reload failed: {e}");
            }
        }
        None => {
            ctx.pools.sync_config(&cfg);
            ctx.set_config(cfg);
        }
    }
}

/// Apply the pool "balancing" knobs the management UI exposes — switch threshold
/// and session distribution — leaving anything not supplied untouched.
fn apply_pool_knobs(c: &mut Config, name: &str, threshold: Option<f64>, distribute: Option<bool>) {
    if let Some(p) = c.pool_mut(name) {
        if let Some(t) = threshold {
            p.switch_threshold = crate::config::Threshold::Single(t);
        }
        if let Some(on) = distribute {
            p.distribute_sessions = on;
        }
    }
}

/// `POST /corrall/pools` — create a pool with optional threshold / distribute.
fn pool_create(ctx: &Ctx, v: &Value) -> Response<BoxBody> {
    let Some(name) = v.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string) else {
        return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "missing \"name\"" }));
    };
    // Refuse to silently edit an existing pool through the create route.
    if ctx.config().pools.contains_key(&name) {
        return json_response(StatusCode::CONFLICT, json!({ "ok": false, "error": format!("a pool named {name:?} already exists") }));
    }
    let threshold = v.get("switchThreshold").and_then(Value::as_f64);
    let distribute = v.get("distributeSessions").and_then(Value::as_bool);
    match Config::update(|c| {
        c.ensure_pool(&name)?;
        apply_pool_knobs(c, &name, threshold, distribute);
        Ok(())
    }) {
        Ok(cfg) => {
            reconcile(ctx, cfg);
            json_response(StatusCode::OK, json!({ "ok": true, "pool": name }))
        }
        Err(e) => json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Dispatch the parameterised `/corrall/pools/...` POST routes: an account
/// action (`{pool}/accounts/{id}/{action}`) or a pool edit (`{name}`).
fn pools_param(ctx: &Ctx, sub: &str, v: &Value) -> Response<BoxBody> {
    let dec = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().to_string();
    let raw: Vec<&str> = sub.split('/').collect();
    match raw.as_slice() {
        [pool, "accounts", id, action] => account_action(ctx, &dec(pool), &dec(id), action, v),
        [name] if !name.is_empty() => pool_edit(ctx, &dec(name), v),
        _ => json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "not found" })),
    }
}

/// `POST /corrall/pools/{pool}/accounts/{id}/{enable|disable|priority}` — flip a
/// per-account flag and persist it, mirroring `corrall enable/disable/priority`.
fn account_action(ctx: &Ctx, pool: &str, id: &str, action: &str, v: &Value) -> Response<BoxBody> {
    let priority = match action {
        "enable" | "disable" => None,
        "priority" => match v.get("priority").and_then(Value::as_i64) {
            Some(p) => Some(p as i32),
            None => return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "missing or invalid \"priority\"" })),
        },
        _ => return json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "unknown account action" })),
    };
    if !ctx.config().pools.contains_key(pool) {
        return json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": format!("no pool named {pool:?}") }));
    }
    let mut account_name = String::new();
    match Config::update(|c| {
        let a = crate::cli::find_account_mut(c, Some(pool), id)?;
        match action {
            "enable" => a.disabled = false,
            "disable" => a.disabled = true,
            "priority" => a.priority = priority.expect("priority parsed above"),
            _ => unreachable!(),
        }
        account_name = a.name.clone();
        Ok(())
    }) {
        Ok(cfg) => {
            reconcile(ctx, cfg);
            json_response(StatusCode::OK, json!({ "ok": true, "pool": pool, "account": account_name }))
        }
        Err(e) => json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": e.to_string() })),
    }
}

/// `POST /corrall/pools/{name}` — change a pool's threshold / distribute and/or
/// rename it. The live manager is re-keyed before the reconcile so a rename
/// keeps the pool's sessions and quota rather than resetting them.
fn pool_edit(ctx: &Ctx, name: &str, v: &Value) -> Response<BoxBody> {
    if !ctx.config().pools.contains_key(name) {
        return json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": format!("no pool named {name:?}") }));
    }
    let threshold = v.get("switchThreshold").and_then(Value::as_f64);
    let distribute = v.get("distributeSessions").and_then(Value::as_bool);
    let new_name = v.get("newName").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let name = name.to_string();
    match Config::update(|c| {
        apply_pool_knobs(c, &name, threshold, distribute);
        if let Some(nn) = &new_name {
            c.rename_pool(&name, nn)?;
        }
        Ok(())
    }) {
        Ok(cfg) => {
            if let Some(nn) = &new_name {
                ctx.pools.rename_pool(&name, nn);
            }
            reconcile(ctx, cfg);
            json_response(StatusCode::OK, json!({ "ok": true, "pool": new_name.unwrap_or(name) }))
        }
        Err(e) => json_response(StatusCode::CONFLICT, json!({ "ok": false, "error": e.to_string() })),
    }
}

/// `POST /corrall/login/start` — begin a manual OAuth login and stash the PKCE
/// material under a returned flow id.
fn login_start(ctx: &Ctx, v: &Value) -> Response<BoxBody> {
    let pool = v.get("pool").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let name = v.get("name").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let (url, login) = crate::oauth::start_manual_login();
    let flow_id = crate::security::random_key(16);
    let now = crate::quota::now_ms();
    {
        let mut flows = ctx.oauth_flows.lock();
        flows.retain(|_, f| now - f.created_at < OAUTH_FLOW_TTL_MS);
        flows.insert(flow_id.clone(), PendingFlow { login, pool, name, created_at: now });
    }
    json_response(StatusCode::OK, json!({ "ok": true, "flow_id": flow_id, "authorize_url": url, "redirect_uri": crate::oauth::MANUAL_REDIRECT_URI }))
}

/// `POST /corrall/login/submit` — exchange the pasted code for tokens and upsert
/// the account into the target pool. Tokens are exchanged here, never in the app.
async fn login_submit(ctx: &Ctx, v: &Value) -> Response<BoxBody> {
    let Some(flow_id) = v.get("flow_id").and_then(Value::as_str) else {
        return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "missing \"flow_id\"" }));
    };
    let Some(code) = v.get("code").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string) else {
        return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "missing \"code\"" }));
    };
    let now = crate::quota::now_ms();
    // Take the flow out (one-shot). Never hold the lock across the awaits below.
    let pending = {
        let mut flows = ctx.oauth_flows.lock();
        flows.retain(|_, f| now - f.created_at < OAUTH_FLOW_TTL_MS);
        flows.remove(flow_id)
    };
    let Some(pending) = pending else {
        return json_response(StatusCode::GONE, json!({ "ok": false, "error": "login flow not found or expired; start again" }));
    };
    let tokens = match crate::oauth::finish_manual_login(&pending.login, &code).await {
        Ok(t) => t,
        Err(e) => return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": e.to_string() })),
    };
    // Best-effort: an explicit --name may stand in when the profile call fails.
    let profile = crate::oauth::fetch_profile(&tokens.access_token).await.ok();
    let mut created = false;
    let mut account_name = String::new();
    match Config::update(|c| {
        let pool = crate::cli::target_pool(c, pending.pool.as_deref())?;
        let name = match &pending.name {
            Some(n) => n.clone(),
            None => match &profile {
                Some(p) => crate::cli::display_name(p, c),
                None => "account".to_string(),
            },
        };
        let updated = crate::cli::upsert_oauth(c, &pool, &name, &tokens, profile.as_ref());
        created = !updated;
        account_name = name;
        Ok(())
    }) {
        Ok(cfg) => {
            reconcile(ctx, cfg);
            json_response(StatusCode::OK, json!({ "ok": true, "name": account_name, "created": created }))
        }
        Err(e) => json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "ok": false, "error": e.to_string() })),
    }
}

/// `POST /corrall/login/cancel` — drop a pending login. Idempotent.
fn login_cancel(ctx: &Ctx, v: &Value) -> Response<BoxBody> {
    if let Some(flow_id) = v.get("flow_id").and_then(Value::as_str) {
        ctx.oauth_flows.lock().remove(flow_id);
    }
    json_response(StatusCode::OK, json!({ "ok": true }))
}

fn render_metrics(ctx: &Ctx) -> String {
    use std::sync::atomic::Ordering;
    let mut out = String::new();
    out.push_str("# TYPE corrall_requests_total counter\n");
    out.push_str(&format!("corrall_requests_total {}\n", ctx.metrics.requests_total.load(Ordering::Relaxed)));
    out.push_str("# TYPE corrall_requests_failed_total counter\n");
    out.push_str(&format!("corrall_requests_failed_total {}\n", ctx.metrics.requests_failed.load(Ordering::Relaxed)));
    out.push_str("# TYPE corrall_auth_failures_total counter\n");
    out.push_str(&format!("corrall_auth_failures_total {}\n", ctx.metrics.auth_failures.load(Ordering::Relaxed)));
    out.push_str("# TYPE corrall_connects_total counter\n");
    out.push_str(&format!("corrall_connects_total {}\n", ctx.metrics.connects_total.load(Ordering::Relaxed)));
    out.push_str("# TYPE corrall_account_quota_utilization gauge\n");
    out.push_str("# TYPE corrall_account_available gauge\n");
    out.push_str("# TYPE corrall_account_requests_total counter\n");
    let esc = |s: &str| s.replace(['"', '\\', '\n'], "_");
    let mut sessions = String::new();
    for (pool, m) in ctx.pools.each() {
        let st = m.status(false);
        let pool = esc(&pool);
        if let Some(accts) = st.get("accounts").and_then(Value::as_array) {
            for a in accts {
                let name = esc(a.get("name").and_then(Value::as_str).unwrap_or(""));
                for b in ["unified5h", "unified7d", "unified7dFable", "unified7dSonnet"] {
                    if let Some(u) = a.pointer(&format!("/quota/{b}/utilization")).and_then(Value::as_f64) {
                        out.push_str(&format!("corrall_account_quota_utilization{{pool=\"{pool}\",account=\"{name}\",bucket=\"{b}\"}} {u}\n"));
                    }
                }
                let avail = if a.get("blocked").map(Value::is_null).unwrap_or(false) { 1 } else { 0 };
                out.push_str(&format!("corrall_account_available{{pool=\"{pool}\",account=\"{name}\"}} {avail}\n"));
                let n = a.pointer("/usage/totalRequests").and_then(Value::as_u64).unwrap_or(0);
                out.push_str(&format!("corrall_account_requests_total{{pool=\"{pool}\",account=\"{name}\"}} {n}\n"));
            }
        }
        let active = st.pointer("/sessions/active").and_then(Value::as_u64).unwrap_or(0);
        sessions.push_str(&format!("corrall_sessions_active{{pool=\"{pool}\"}} {active}\n"));
    }
    out.push_str("# TYPE corrall_sessions_active gauge\n");
    out.push_str(&sessions);
    out
}

// ── CONNECT / MITM ────────────────────────────────────────────

async fn handle_connect(ctx: Ctx, req: Request<Incoming>, peer: IpAddr) -> Response<BoxBody> {
    ctx.metrics.connects_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cfg = ctx.config();
    let (auth, user) = authenticate_connect(&cfg.proxy, peer, req.headers());
    // The proxy username carries `[<pin>][~<pool>]`; MITM mode has no URL for
    // the `/pool/` keyword to ride on.
    let (pin, asked_pool) = match &user {
        Some(u) => crate::pools::split_pin_pool(u),
        None => (None, None),
    };
    if let Auth::Denied(why) = &auth {
        ctx.metrics.auth_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!("CONNECT denied from {peer}: {why}");
        let mut r = error_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED, "authentication_error", "proxy authentication required");
        r.headers_mut().insert("proxy-authenticate", HeaderValue::from_static("Basic realm=\"corrall\""));
        return r;
    }
    let authority = req.uri().authority().map(|a| a.to_string()).unwrap_or_default();
    let (host, port) = match mitm::parse_authority(&authority) {
        Ok(x) => x,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request_error", "bad CONNECT authority"),
    };
    let mode = if host.eq_ignore_ascii_case(crate::codex::NEVER_INTERCEPT) {
        if cfg.mitm.allow_tunnel {
            mitm::HostMode::Tunnel
        } else {
            mitm::HostMode::Refuse("telemetry host is never intercepted; enable mitm.allowTunnel to pass it through")
        }
    } else {
        mitm::host_mode(&host, port, &ctx.intercept_hosts(), &cfg.mitm)
    };

    // Pins and pools only matter for intercepted hosts; validate them there so
    // a typo meant for Anthropic cannot take down unrelated tunnels.
    let (pin, tunnel_pool) = match (&mode, pin) {
        (HostMode::Intercept, p) => {
            let (name, mgr) = ctx.pools.resolve_request(asked_pool.as_deref());
            if let Some(p) = &p {
                if mgr.resolve_pin(p).is_none() {
                    tracing::warn!("CONNECT {host}: unknown account pin");
                    let mut r = error_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED, "authentication_error", "unknown account pin");
                    r.headers_mut().insert("proxy-authenticate", HeaderValue::from_static("Basic realm=\"corrall\""));
                    return r;
                }
            }
            (p, Some(name))
        }
        _ => (None, None),
    };

    match mode {
        HostMode::Refuse(why) => {
            tracing::warn!("CONNECT {host}:{port} refused: {why}");
            return error_response(StatusCode::FORBIDDEN, "permission_error", why);
        }
        HostMode::Tunnel => {
            tokio::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        let mut client = TokioIo::new(upgraded);
                        match tokio::time::timeout(Duration::from_secs(30), tokio::net::TcpStream::connect((host.as_str(), port))).await {
                            Ok(Ok(mut up)) => {
                                let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
                            }
                            _ => {
                                let _ = client.shutdown().await;
                            }
                        }
                    }
                    Err(e) => tracing::debug!("upgrade failed: {e}"),
                }
            });
        }
        HostMode::Intercept | HostMode::Test => {
            let tls = match ctx.tls_config() {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("MITM certificate error: {e}");
                    return error_response(StatusCode::BAD_GATEWAY, "api_error", "MITM certificate unavailable");
                }
            };
            let is_test = mode == HostMode::Test;
            let tunnel = TunnelCtx { auth: auth.clone(), pin, pool: tunnel_pool, host: host.clone() };
            tokio::spawn(async move {
                let upgraded = match hyper::upgrade::on(req).await {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::debug!("upgrade failed: {e}");
                        return;
                    }
                };
                let acceptor = tokio_rustls::TlsAcceptor::from(tls);
                let tls_stream = match acceptor.accept(TokioIo::new(upgraded)).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!("TLS handshake inside tunnel failed: {e}");
                        return;
                    }
                };
                if is_test {
                    serve_test(tls_stream).await;
                } else {
                    serve_connection(ctx, tls_stream, peer, Some(tunnel)).await;
                }
            });
        }
    }
    // 200 with an empty body signals the tunnel is established.
    Response::new(Full::new(Bytes::new()).map_err(|e| match e {}).boxed())
}

/// Answer the built-in test host locally so the CA + proxy can be verified
/// end-to-end with no credentials.
async fn serve_test<S>(stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = service_fn(|_req: Request<Incoming>| async move {
        let body = format!("Corrall MITM proxy is working (v{}).\n", env!("CARGO_PKG_VERSION"));
        let mut r = Response::new(Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed());
        r.headers_mut().insert("content-type", HeaderValue::from_static("text/plain"));
        Ok::<Response<BoxBody>, hyper::Error>(r)
    });
    let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
}

pub fn parse_bind(host: &str, port: u16) -> Result<SocketAddr> {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    let ip: IpAddr = if h.eq_ignore_ascii_case("localhost") {
        "127.0.0.1".parse().unwrap()
    } else {
        h.parse().with_context(|| format!("proxy.host {host} is not an IP address"))?
    };
    Ok(SocketAddr::new(ip, port))
}

#[allow(dead_code)]
pub fn header_str<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).and_then(|v| v.to_str().ok())
}
