//! The local listener: one port serving plain HTTP requests (base-URL mode),
//! the control plane under `/teamclaude/*`, and `CONNECT` tunnels for the MITM
//! mode. Every accepted connection is dispatched by `serve_connection`.

use std::collections::HashSet;
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
use parking_lot::RwLock;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use super::auth::{authenticate, authenticate_connect, Auth};
use super::forward::{error_response, json_response};
use super::log::RequestLogger;
use super::mitm::{self, HostMode};
use crate::config::{Config, EventLogging};
use crate::manager::Manager;

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
    #[allow(dead_code)]
    pub started: std::time::Instant,
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
    pub manager: Manager,
    pub config: RwLock<Arc<Config>>,
    pub logger: Option<RequestLogger>,
    pub hold_ms: u64,
    pub activity: tokio::sync::broadcast::Sender<Activity>,
    pub reload: Option<Box<dyn Fn() -> Result<usize> + Send + Sync>>,
    pub metrics: Metrics,
    pub tls: RwLock<Option<Arc<tokio_rustls::rustls::ServerConfig>>>,
}

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
    if path.starts_with("/teamclaude/") && tunnel.is_none() {
        return control(&ctx, req, &auth).await;
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

    let (model, advisor) = if body.is_empty() { (None, None) } else { crate::model::models_in_body(&body) };
    if let Some(m) = &model {
        if ctx.manager.is_model_blocked(m) {
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
        started: std::time::Instant::now(),
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
    ctx.manager.begin_session_request(info.session_id.as_deref(), info.client.as_deref());
    let resp = super::forward::forward(&ctx, &info, parts.headers, body).await;
    ctx.manager.end_session_request(info.session_id.as_deref());
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

async fn control(ctx: &Ctx, req: Request<Incoming>, auth: &Auth) -> Response<BoxBody> {
    let cfg = ctx.config();
    let path = req.uri().path().to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::GET, "/teamclaude/health") => json_response(StatusCode::OK, json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })),
        (Method::GET, "/teamclaude/status") => {
            let detail = cfg.proxy.session_detail;
            let mut st = ctx.manager.status(detail);
            st["upstream"] = json!(cfg.upstream);
            st["upstreamProxy"] = json!(crate::upstream::describe_proxy(&cfg));
            st["holdSeconds"] = json!(cfg.hold_seconds);
            st["quotaProbeSeconds"] = json!(cfg.quota_probe_seconds);
            st["mitm"] = json!({ "caPath": mitm::ca_cert_path(), "http1Only": cfg.mitm.http1_only, "allowTunnel": cfg.mitm.allow_tunnel });
            st["client"] = json!(auth.client_name());
            json_response(StatusCode::OK, st)
        }
        (Method::GET, "/teamclaude/quota") => json_response(StatusCode::OK, ctx.manager.quota_summary()),
        (Method::GET, "/teamclaude/metrics") => {
            let mut r = Response::new(Full::new(Bytes::from(render_metrics(ctx))).map_err(|e| match e {}).boxed());
            r.headers_mut().insert("content-type", HeaderValue::from_static("text/plain; version=0.0.4"));
            r
        }
        (Method::POST, "/teamclaude/reload") => match &ctx.reload {
            None => json_response(StatusCode::NOT_IMPLEMENTED, json!({ "ok": false, "error": "reload not supported" })),
            Some(f) => match f() {
                Ok(added) => json_response(StatusCode::OK, json!({ "ok": true, "added": added })),
                Err(e) => {
                    tracing::error!("reload failed: {e}");
                    json_response(StatusCode::INTERNAL_SERVER_ERROR, json!({ "ok": false, "error": "reload failed; see server log" }))
                }
            },
        },
        (Method::POST, "/teamclaude/switch") => {
            let body = match read_body(req.into_body(), CONTROL_BODY_LIMIT).await {
                Ok(b) => b,
                Err(true) => return json_response(StatusCode::PAYLOAD_TOO_LARGE, json!({ "ok": false, "error": "request body too large" })),
                Err(false) => return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "invalid request body" })),
            };
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let names: Vec<String> = ctx.manager.account_ids().into_iter().map(|(_, n)| n).collect();
            let Some(target) = v.get("account").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
                return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "missing \"account\"", "accounts": names }));
            };
            let Some(id) = ctx.manager.resolve_pin(target) else {
                return json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "unknown account", "accounts": names }));
            };
            match ctx.manager.switch_to(&id) {
                Some((name, reason)) => {
                    ctx.manager.log(format!("Switched to account \"{name}\" by request"));
                    json_response(StatusCode::OK, json!({ "ok": true, "account": name, "effective": reason.is_none(), "blocked": reason }))
                }
                None => json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "unknown account" })),
            }
        }
        (Method::POST, "/teamclaude/route-pin") => {
            let body = match read_body(req.into_body(), CONTROL_BODY_LIMIT).await {
                Ok(b) => b,
                Err(_) => return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "invalid request body" })),
            };
            let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let Some(route) = v.get("route").and_then(Value::as_str) else {
                return json_response(StatusCode::BAD_REQUEST, json!({ "ok": false, "error": "missing \"route\"" }));
            };
            let id = match v.get("account").and_then(Value::as_str) {
                Some(a) => match ctx.manager.resolve_pin(a) {
                    Some(id) => Some(id),
                    None => return json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "unknown account" })),
                },
                None => None,
            };
            ctx.manager.set_route_pin(route, id.as_deref());
            json_response(StatusCode::OK, json!({ "ok": true }))
        }
        _ => json_response(StatusCode::NOT_FOUND, json!({ "ok": false, "error": "not found" })),
    }
}

fn render_metrics(ctx: &Ctx) -> String {
    use std::sync::atomic::Ordering;
    let st = ctx.manager.status(false);
    let mut out = String::new();
    out.push_str("# TYPE teamclaude_requests_total counter\n");
    out.push_str(&format!("teamclaude_requests_total {}\n", ctx.metrics.requests_total.load(Ordering::Relaxed)));
    out.push_str("# TYPE teamclaude_requests_failed_total counter\n");
    out.push_str(&format!("teamclaude_requests_failed_total {}\n", ctx.metrics.requests_failed.load(Ordering::Relaxed)));
    out.push_str("# TYPE teamclaude_auth_failures_total counter\n");
    out.push_str(&format!("teamclaude_auth_failures_total {}\n", ctx.metrics.auth_failures.load(Ordering::Relaxed)));
    out.push_str("# TYPE teamclaude_connects_total counter\n");
    out.push_str(&format!("teamclaude_connects_total {}\n", ctx.metrics.connects_total.load(Ordering::Relaxed)));
    out.push_str("# TYPE teamclaude_account_quota_utilization gauge\n");
    out.push_str("# TYPE teamclaude_account_available gauge\n");
    out.push_str("# TYPE teamclaude_account_requests_total counter\n");
    if let Some(accts) = st.get("accounts").and_then(Value::as_array) {
        for a in accts {
            let name = a.get("name").and_then(Value::as_str).unwrap_or("").replace(['"', '\\', '\n'], "_");
            for b in ["unified5h", "unified7d", "unified7dFable", "unified7dSonnet"] {
                if let Some(u) = a.pointer(&format!("/quota/{b}/utilization")).and_then(Value::as_f64) {
                    out.push_str(&format!("teamclaude_account_quota_utilization{{account=\"{name}\",bucket=\"{b}\"}} {u}\n"));
                }
            }
            let avail = if a.get("blocked").map(Value::is_null).unwrap_or(false) { 1 } else { 0 };
            out.push_str(&format!("teamclaude_account_available{{account=\"{name}\"}} {avail}\n"));
            let n = a.pointer("/usage/totalRequests").and_then(Value::as_u64).unwrap_or(0);
            out.push_str(&format!("teamclaude_account_requests_total{{account=\"{name}\"}} {n}\n"));
        }
    }
    out.push_str("# TYPE teamclaude_sessions_active gauge\n");
    out.push_str(&format!("teamclaude_sessions_active {}\n", st.pointer("/sessions/active").and_then(Value::as_u64).unwrap_or(0)));
    out
}

// ── CONNECT / MITM ────────────────────────────────────────────

async fn handle_connect(ctx: Ctx, req: Request<Incoming>, peer: IpAddr) -> Response<BoxBody> {
    ctx.metrics.connects_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let cfg = ctx.config();
    let (auth, pin) = authenticate_connect(&cfg.proxy, peer, req.headers());
    if let Auth::Denied(why) = &auth {
        ctx.metrics.auth_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!("CONNECT denied from {peer}: {why}");
        let mut r = error_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED, "authentication_error", "proxy authentication required");
        r.headers_mut().insert("proxy-authenticate", HeaderValue::from_static("Basic realm=\"teamclaude\""));
        return r;
    }
    let authority = req.uri().authority().map(|a| a.to_string()).unwrap_or_default();
    let (host, port) = match mitm::parse_authority(&authority) {
        Ok(x) => x,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid_request_error", "bad CONNECT authority"),
    };
    let mode = mitm::host_mode(&host, port, &ctx.intercept_hosts(), &cfg.mitm);

    // Pins only matter for intercepted hosts; validate them there so a typo
    // meant for Anthropic cannot take down unrelated tunnels.
    let pin = match (&mode, pin) {
        (HostMode::Intercept, Some(p)) => {
            if ctx.manager.resolve_pin(&p).is_none() {
                tracing::warn!("CONNECT {host}: unknown account pin");
                let mut r = error_response(StatusCode::PROXY_AUTHENTICATION_REQUIRED, "authentication_error", "unknown account pin");
                r.headers_mut().insert("proxy-authenticate", HeaderValue::from_static("Basic realm=\"teamclaude\""));
                return r;
            }
            Some(p)
        }
        _ => None,
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
            let tunnel = TunnelCtx { auth: auth.clone(), pin, host: host.clone() };
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
        let body = format!("TeamClaude MITM proxy is working (v{}).\n", env!("CARGO_PKG_VERSION"));
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
