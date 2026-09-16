//! Integration tests: the proxy runs in-process against a mock upstream that
//! speaks the Anthropic wire shape (rate-limit headers, SSE, 429 flavours).
//! Each test mirrors a behaviour the original project learned from a real
//! incident: quota-vs-rate-limit 429s, family buckets, storm control, pins,
//! session affinity, credential stripping, passthrough and MITM.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use corrall::config::{AccountConfig, AccountType, Config, PoolConfig, ProxyConfig, DEFAULT_POOL};
use corrall::manager::Manager;
use corrall::pools::Pools;
use corrall::proxy::server::{bind as bind_listener, serve, Ctx, CtxInner, Metrics};

const KEY: &str = "tc-test-key-0123456789abcdef";

#[derive(Debug, Clone)]
struct Seen {
    path: String,
    /// Last value of each header; see `header_values` for repeated lines.
    headers: HashMap<String, String>,
    /// Every value of every header, in wire order.
    header_values: HashMap<String, Vec<String>>,
    body: Value,
}

#[derive(Default)]
struct MockState {
    seen: Vec<Seen>,
    /// token -> behaviour for the next matching request
    behaviours: HashMap<String, Vec<Behaviour>>,
}

#[derive(Debug, Clone)]
enum Behaviour {
    Ok,
    QuotaRejected { retry_after: u64 },
    FamilyRejected,
    RateLimited { retry_after: u64 },
    RateLimitedNoHeader,
    Unauthorized,
    ServerError,
    EntitlementDenied,
    SlowStream { chunks: u32, gap_ms: u64 },
    Hang { ms: u64 },
}

type MockBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

#[derive(Clone)]
struct Mock {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    in_flight: Arc<AtomicU32>,
    max_in_flight: Arc<AtomicU32>,
    delay_ms: Arc<AtomicU32>,
}

impl Mock {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }
    fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen.clone()
    }
    fn queue(&self, token: &str, b: Behaviour) {
        self.state.lock().unwrap().behaviours.entry(token.to_string()).or_default().push(b);
    }
}

fn token_of(h: &HashMap<String, String>) -> String {
    h.get("authorization").map(|a| a.trim_start_matches("Bearer ").to_string()).or_else(|| h.get("x-api-key").cloned()).unwrap_or_default()
}

async fn mock_handler(mock: Mock, req: Request<Incoming>) -> Result<Response<MockBody>, hyper::Error> {
    let path = req.uri().path_and_query().map(|p| p.to_string()).unwrap_or_default();
    let is_upgrade = req.headers().get("upgrade").is_some();
    let headers: HashMap<String, String> = req.headers().iter().map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())).collect();
    let mut header_values: HashMap<String, Vec<String>> = HashMap::new();
    for (k, v) in req.headers().iter() {
        header_values.entry(k.as_str().to_string()).or_default().push(v.to_str().unwrap_or("").to_string());
    }
    // Cloudflare fronts the real upstream and rejects a request that carries
    // `content-length` twice with a bare HTML 400, even when the values agree.
    // hyper would quietly accept that here, so refuse it the same way.
    if req.headers().get_all("content-length").iter().count() > 1 {
        let r = Response::builder()
            .status(400)
            .header("content-type", "text/html")
            .body(Full::new(Bytes::from_static(b"<html><head><title>400 Bad Request</title></head><body><center><h1>400 Bad Request</h1></center><hr><center>cloudflare</center></body></html>")).boxed())
            .unwrap();
        return Ok(r);
    }
    if is_upgrade {
        // Answer a WebSocket-style 101; the test then talks raw bytes.
        tokio::spawn(async move {
            if let Ok(up) = hyper::upgrade::on(req).await {
                let mut io = TokioIo::new(up);
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 64];
                if let Ok(n) = io.read(&mut buf).await {
                    let _ = io.write_all(&buf[..n]).await;
                    let _ = io.write_all(b" echoed").await;
                }
            }
        });
        let r = Response::builder()
            .status(101)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-accept", "x")
            .body(Full::new(Bytes::new()).boxed())
            .unwrap();
        return Ok(r);
    }
    let body = req.into_body().collect().await?.to_bytes();
    let body_json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let tok = token_of(&headers);
    let behaviour = {
        let mut st = mock.state.lock().unwrap();
        st.seen.push(Seen { path: path.clone(), headers: headers.clone(), header_values, body: body_json.clone() });
        st.behaviours.get_mut(&tok).and_then(|v| if v.is_empty() { None } else { Some(v.remove(0)) }).unwrap_or(Behaviour::Ok)
    };
    let cur = mock.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    mock.max_in_flight.fetch_max(cur, Ordering::SeqCst);
    let delay = mock.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay as u64)).await;
    }
    mock.in_flight.fetch_sub(1, Ordering::SeqCst);
    let now = chrono::Utc::now().timestamp();
    let base = |status: u16| {
        Response::builder()
            .status(status)
            .header("anthropic-ratelimit-unified-5h-utilization", "0.40")
            .header("anthropic-ratelimit-unified-7d-utilization", "0.20")
            .header("anthropic-ratelimit-unified-7d-reset", (now + 86_400).to_string())
            .header("anthropic-ratelimit-unified-status", "allowed")
    };
    let behaviour = match behaviour {
        Behaviour::SlowStream { chunks, gap_ms } => {
            let stream = futures_util::stream::unfold(0u32, move |i| async move {
                if i >= chunks {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(gap_ms)).await;
                let ev = format!("event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"chunk{i}\"}}}}\n\n");
                Some((Ok::<_, std::convert::Infallible>(Frame::data(Bytes::from(ev))), i + 1))
            });
            return Ok(base(200).header("content-type", "text/event-stream").body(StreamBody::new(stream).boxed()).unwrap());
        }
        Behaviour::Hang { ms } => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Behaviour::Ok
        }
        other => other,
    };
    let resp = match behaviour {
        Behaviour::Ok => {
            let stream = body_json.get("stream").and_then(Value::as_bool).unwrap_or(false);
            if stream {
                let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n";
                base(200).header("content-type", "text/event-stream").body(Full::new(Bytes::from(sse))).unwrap()
            } else {
                let out = json!({ "id": "msg", "type": "message", "served_by": tok, "model": body_json.get("model"), "usage": { "input_tokens": 11, "output_tokens": 3 } });
                base(200).header("content-type", "application/json").body(Full::new(Bytes::from(out.to_string()))).unwrap()
            }
        }
        Behaviour::QuotaRejected { retry_after } => base(429)
            .header("anthropic-ratelimit-unified-5h-status", "rejected")
            .header("anthropic-ratelimit-unified-5h-utilization", "1.0")
            .header("anthropic-ratelimit-unified-5h-reset", (now + 600).to_string())
            .header("retry-after", retry_after.to_string())
            .body(Full::new(Bytes::from(r#"{"type":"error","error":{"type":"rate_limit_error","message":"spent"}}"#)))
            .unwrap(),
        Behaviour::FamilyRejected => base(429)
            .header("anthropic-ratelimit-unified-7d_oi-status", "rejected")
            .header("anthropic-ratelimit-unified-7d_oi-utilization", "1.0")
            .header("anthropic-ratelimit-unified-7d_oi-reset", (now + 86_400).to_string())
            .header("retry-after", "30")
            .body(Full::new(Bytes::from(r#"{"type":"error","error":{"type":"rate_limit_error","message":"fable spent"}}"#)))
            .unwrap(),
        Behaviour::RateLimited { retry_after } => base(429)
            .header("retry-after", retry_after.to_string())
            .body(Full::new(Bytes::from(r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#)))
            .unwrap(),
        Behaviour::RateLimitedNoHeader => base(429)
            .header("content-type", "text/html")
            .body(Full::new(Bytes::from("<html><head><title>429 Too Many Requests</title></head><body>cloudflare</body></html>")))
            .unwrap(),
        Behaviour::Unauthorized => Response::builder()
            .status(401)
            .body(Full::new(Bytes::from(r#"{"type":"error","error":{"type":"authentication_error","message":"bad token"}}"#)))
            .unwrap(),
        Behaviour::ServerError => Response::builder()
            .status(529)
            .body(Full::new(Bytes::from(r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#)))
            .unwrap(),
        Behaviour::EntitlementDenied => Response::builder()
            .status(403)
            .body(Full::new(Bytes::from(
                r#"{"type":"error","error":{"type":"permission_error","message":"no","details":{"error_code":"oauth_not_allowed_for_organization"}}}"#,
            )))
            .unwrap(),
        Behaviour::SlowStream { .. } | Behaviour::Hang { .. } => unreachable!("handled above"),
    };
    Ok(resp.map(BodyExt::boxed))
}

async fn spawn_mock() -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = Mock {
        addr: listener.local_addr().unwrap(),
        state: Default::default(),
        in_flight: Default::default(),
        max_in_flight: Default::default(),
        delay_ms: Default::default(),
    };
    let m2 = mock.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let m = m2.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| mock_handler(m.clone(), req));
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).with_upgrades().await;
            });
        }
    });
    mock
}

fn test_env() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("corrall-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("CORRALL_CONFIG", dir.join("corrall.json"));
        // Short time-to-headers budget so the timeout tests run in seconds.
        // Every mock reply that is not deliberately hung answers well inside it.
        std::env::set_var("CORRALL_UPSTREAM_HEADERS_TIMEOUT_MS", "1500");
        let cfg = Config { proxy: ProxyConfig { api_key: KEY.into(), ..Default::default() }, ..Default::default() };
        corrall::upstream::init(&cfg).unwrap();
    });
}

fn account(name: &str, token: &str, prio: i32, upstream: &str) -> AccountConfig {
    AccountConfig {
        name: name.into(),
        kind: AccountType::Oauth,
        access_token: Some(token.into()),
        // no refresh token: a 401 must not try the network
        refresh_token: None,
        expires_at: Some(chrono::Utc::now().timestamp_millis() + 3_600_000),
        priority: prio,
        upstream: Some(upstream.into()),
        account_uuid: Some(format!("{:0>8}-0000-0000-0000-000000000000", name.len())),
        ..Default::default()
    }
}

struct Proxy {
    port: u16,
    ctx: Ctx,
    manager: Manager,
}

impl Proxy {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
}

async fn spawn_proxy(mut cfg: Config) -> Proxy {
    test_env();
    cfg.proxy.api_key = KEY.into();
    cfg.ensure_account_ids();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    cfg.proxy.port = port;
    // These tests drive one pool; `Pools` hands back the default fleet for any
    // request that arrives without a /pool/<name> prefix.
    let pools = Pools::new(&cfg);
    let manager = pools.default();
    let (tx, _) = tokio::sync::broadcast::channel(64);
    let ctx = Ctx(Arc::new(CtxInner {
        pools: pools.clone(),
        config: parking_lot::RwLock::new(Arc::new(cfg.clone())),
        logger: None,
        activity: tx,
        reload: None,
        metrics: Metrics::default(),
        tls: parking_lot::RwLock::new(None),
        titles: corrall::titles::Titles::new(&cfg.session_titles),
        oauth_flows: Default::default(),
    }));
    let (_stx, srx) = tokio::sync::watch::channel(false);
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let c2 = ctx.clone();
    let listener = bind_listener(bind).await.expect("bind test listener");
    tokio::spawn(async move {
        let _ = serve(c2, listener, srx).await;
    });
    std::mem::forget(_stx);
    Proxy { port, ctx, manager }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn msg(model: &str) -> Value {
    json!({ "model": model, "max_tokens": 5, "messages": [{ "role": "user", "content": "hi" }] })
}

async fn post(p: &Proxy, path: &str, body: Value) -> (StatusCode, Value, reqwest::header::HeaderMap) {
    let r = http().post(p.url(path)).header("host", format!("127.0.0.1:{}", p.port)).json(&body).send().await.unwrap();
    let st = r.status();
    let h = r.headers().clone();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    (StatusCode::from_u16(st.as_u16()).unwrap(), v, h)
}

fn cfg_with(accounts: Vec<AccountConfig>) -> Config {
    let mut c = Config::default();
    pool_of(&mut c).accounts = accounts;
    c
}

/// The default pool, which is where every knob these tests set now lives.
fn pool_of(c: &mut Config) -> &mut PoolConfig {
    c.pool_mut(DEFAULT_POOL).expect("default pool always exists")
}

// ── tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn auth_gate_loopback_rebinding_browser_and_keys() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    let c = http();
    let ok = c.get(p.url("/corrall/health")).send().await.unwrap();
    assert_eq!(ok.status(), 200, "loopback with loopback Host is exempt");
    let rebind = c.get(p.url("/corrall/status")).header("host", "attacker.example:3456").send().await.unwrap();
    assert_eq!(rebind.status(), 401, "DNS rebinding: non-loopback Host is refused");
    let origin = c.get(p.url("/corrall/status")).header("origin", "http://evil").send().await.unwrap();
    assert_eq!(origin.status(), 401, "browser-originated request is refused");
    let sfs = c.post(p.url("/v1/messages")).header("sec-fetch-site", "cross-site").json(&msg("claude-opus-5")).send().await.unwrap();
    assert_eq!(sfs.status(), 401, "no-cors POST from a page is refused on data paths too");
    let keyed = c.get(p.url("/corrall/status")).header("host", "attacker.example").header("x-api-key", KEY).send().await.unwrap();
    assert_eq!(keyed.status(), 200, "a valid key works regardless of Host");
    let wrong = c.get(p.url("/corrall/status")).header("host", "attacker.example").header("x-api-key", "nope").send().await.unwrap();
    assert_eq!(wrong.status(), 401);
    assert!(mock.seen().is_empty(), "nothing reached upstream");
}

#[tokio::test]
async fn client_credentials_are_stripped_and_account_token_injected() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    let r = http()
        .post(p.url("/v1/messages"))
        .header("authorization", "Bearer CLIENT-SECRET")
        .header("x-api-key", "CLIENT-KEY")
        .header("cookie", "session=1")
        .json(&json!({ "model": "claude-sonnet-4-6", "messages": [], "metadata": { "user_id": "{\"device_id\":\"d\",\"account_uuid\":\"00000000-0000-0000-0000-000000000000\"}" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let h = &seen[0].headers;
    assert_eq!(h.get("authorization").unwrap(), "Bearer tok-a");
    assert!(!h.contains_key("x-api-key"));
    assert!(!h.contains_key("cookie"));
    let user_id = seen[0].body["metadata"]["user_id"].as_str().unwrap();
    assert!(user_id.contains("00000001-0000-0000-0000-000000000000"), "account_uuid rewritten: {user_id}");
}

/// Claude Code sends `anthropic-beta` as several header lines. The forward
/// path used to rebuild the header map from `into_iter()`, whose second and
/// later values of a repeated name come with no name, and dropped them.
#[tokio::test]
async fn repeated_request_headers_all_reach_upstream() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    let r = http()
        .post(p.url("/v1/messages"))
        .header("anthropic-beta", "interleaved-thinking-2025-05-14")
        .header("anthropic-beta", "context-management-2025-06-27")
        .json(&msg("claude-opus-5"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    let betas = seen[0].header_values.get("anthropic-beta").cloned().unwrap_or_default();
    assert_eq!(betas, vec!["interleaved-thinking-2025-05-14".to_string(), "context-management-2025-06-27".to_string()], "both lines forwarded, in order");
}

#[tokio::test]
async fn quota_rejection_rotates_but_rate_limit_retries_same_account() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 1, &mock.url())])).await;
    // quota 429 on a → served by b, a throttled
    mock.queue("tok-a", Behaviour::QuotaRejected { retry_after: 30 });
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b");
    let status = p.manager.status(false);
    let a = &status["accounts"][0];
    assert_eq!(a["status"], "throttled");
    assert!(a["blocked"].as_str().unwrap().contains("rate-limited"));
    // rate-limit 429 on b with a short retry-after → same account retried, no rotation
    mock.queue("tok-b", Behaviour::RateLimited { retry_after: 1 });
    let n = mock.seen().len();
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b");
    let after = mock.seen();
    assert_eq!(after.len(), n + 2, "one 429 then one retry");
    assert!(after[n..].iter().all(|s| token_of(&s.headers) == "tok-b"));
}

#[tokio::test]
async fn headerless_429_pauses_briefly_and_retries_the_same_account() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 0, &mock.url())])).await;
    // No retry-after: a short pause on the same account, not a 60s mark and a
    // hop that would have marked b too.
    mock.queue("tok-a", Behaviour::RateLimitedNoHeader);
    let started = std::time::Instant::now();
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-a", "no failover onto b");
    let took = started.elapsed();
    assert!(took >= Duration::from_secs(4) && took < Duration::from_secs(20), "a short pause: {took:?}");
    let seen = mock.seen();
    assert_eq!(seen.len(), 2, "one 429 then one retry");
    assert!(seen.iter().all(|s| token_of(&s.headers) == "tok-a"));
    let status = p.manager.status(false);
    assert_eq!(status["accounts"][1]["status"], "active", "b was never touched");
}

#[tokio::test]
async fn two_rate_limited_accounts_in_a_row_stop_the_cascade() {
    let mock = spawn_mock().await;
    let p =
        spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 0, &mock.url()), account("c", "tok-c", 0, &mock.url())])).await;
    // A long retry-after hops once; a second rate-limited account means the
    // limit follows the request, so c is left alone and the client gets the
    // upstream retry-after.
    mock.queue("tok-a", Behaviour::RateLimited { retry_after: 60 });
    mock.queue("tok-b", Behaviour::RateLimited { retry_after: 60 });
    let (st, _, h) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 429);
    assert_eq!(h.get("retry-after").unwrap(), "60");
    let seen = mock.seen();
    assert_eq!(seen.len(), 2, "a and b were tried, c was not");
    assert!(seen.iter().all(|s| token_of(&s.headers) != "tok-c"));
    let status = p.manager.status(false);
    assert_eq!(status["accounts"][2]["status"], "active", "c stays available to everyone else");
    // Anyone else is served by c meanwhile.
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-c");
}

#[tokio::test]
async fn family_rejection_diverts_only_that_family() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 0, &mock.url())])).await;
    let (_, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(v["served_by"], "tok-a");
    mock.queue("tok-a", Behaviour::FamilyRejected);
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-fable-5-1")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b", "fable diverted");
    let (_, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(v["served_by"], "tok-a", "opus stays on a");
    let (_, v, _) = post(&p, "/v1/messages", msg("claude-fable-5-1")).await;
    assert_eq!(v["served_by"], "tok-b", "fable keeps going to b while the reading is spent");
    let st = p.manager.status(false);
    assert_eq!(st["accounts"][0]["models"]["fable"], false);
    assert_eq!(st["accounts"][0]["models"]["opus"], true);
}

#[tokio::test]
async fn streaming_passes_through_and_usage_is_recorded() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    let mut body = msg("claude-sonnet-4-6");
    body["stream"] = json!(true);
    let r = http().post(p.url("/v1/messages")).header("x-claude-code-session-id", "11111111-2222-3333-4444-555555555555").json(&body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("content-type").unwrap().to_str().unwrap().contains("text/event-stream"));
    let text = r.text().await.unwrap();
    assert!(text.contains("message_start") && text.contains("message_delta"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let st = p.manager.status(true);
    assert_eq!(st["accounts"][0]["usage"]["inputTokens"], 11);
    assert_eq!(st["accounts"][0]["usage"]["outputTokens"], 9, "message_delta supersedes the placeholder");
    assert_eq!(st["sessions"]["known"], 1);
    let items = st["sessions"]["items"].as_array().unwrap();
    assert_eq!(items[0]["tokens"]["unified7dSonnet"]["output"], 9);
}

/// `/pool/<name>` picks a fleet, is stripped before forwarding, composes with
/// an account pin, and — because it sits under a fixed keyword real API paths
/// never use — needs no reserved-name list.
#[tokio::test]
async fn pool_prefix_routes_to_its_own_fleet() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    // "v1" would be a reserved word under a bare-prefix scheme. It is not one here.
    for pool in ["work", "v1"] {
        cfg.pools.insert(pool.into(), PoolConfig { accounts: vec![account(pool, &format!("tok-{pool}"), 0, &mock.url())], ..Default::default() });
    }
    let p = spawn_proxy(cfg).await;

    // No prefix: the default pool, exactly as before pools existed.
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-a");
    assert_eq!(mock.seen().last().unwrap().path, "/v1/messages");

    for pool in ["work", "v1"] {
        let (st, v, _) = post(&p, &format!("/pool/{pool}/v1/messages"), msg("claude-opus-5")).await;
        assert_eq!(st, 200);
        assert_eq!(v["served_by"], format!("tok-{pool}"), "pool {pool} served the wrong account");
        assert_eq!(mock.seen().last().unwrap().path, "/v1/messages", "pool prefix stripped");
    }

    // A pin resolves inside the addressed pool, and both prefixes come off.
    let (st, v, _) = post(&p, "/pool/work/tc-acct/work/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-work");
    assert_eq!(mock.seen().last().unwrap().path, "/v1/messages");

    // An unknown pool degrades to the default fleet rather than failing the
    // request: a stale ANTHROPIC_BASE_URL should not take a client down.
    let (st, v, _) = post(&p, "/pool/gone/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-a");

    // Each fleet accounts for its own traffic.
    let st = p.ctx.pools.status(false);
    let pools = st["pools"].as_array().unwrap();
    let served =
        |name: &str| pools.iter().find(|p| p["pool"] == name).and_then(|p| p.pointer("/accounts/0/usage/totalRequests").and_then(Value::as_u64)).unwrap_or(0);
    assert_eq!(served("default"), 2, "no-prefix plus the unknown-pool fallback");
    assert_eq!(served("work"), 2, "plain plus pinned");
    assert_eq!(served("v1"), 1);
}

#[tokio::test]
async fn pins_never_fail_over_and_unknown_pin_is_404() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 1, &mock.url())])).await;
    let (st, v, _) = post(&p, "/tc-acct/b/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b");
    assert_eq!(mock.seen().last().unwrap().path, "/v1/messages", "pin prefix stripped");
    let (st, _, _) = post(&p, "/tc-acct/nobody/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    mock.queue("tok-b", Behaviour::ServerError);
    let (st, _, _) = post(&p, "/tc-acct/b/v1/messages", msg("claude-opus-5")).await;
    assert!(st.is_server_error(), "a pinned account's failure is not hidden by failover: {st}");
    assert!(mock.seen().iter().all(|s| token_of(&s.headers) != "tok-a"));
}

#[tokio::test]
async fn oauth_token_refresh_passthrough_keeps_client_credentials() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    cfg.upstream = mock.url();
    let p = spawn_proxy(cfg).await;
    // The client's own refresh must not get an account token injected.
    let r = http()
        .post(p.url("/v1/oauth/token"))
        .header("authorization", "Bearer CLIENT-OWN")
        .header("x-api-key", KEY)
        .json(&json!({ "grant_type": "refresh_token" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let seen = mock.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/oauth/token");
    assert_eq!(seen[0].headers.get("authorization").unwrap(), "Bearer CLIENT-OWN");
    assert!(!seen[0].headers.contains_key("x-api-key"), "the proxy key never leaves");
}

#[tokio::test]
async fn connector_list_is_passthrough_with_the_clients_own_login() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    cfg.upstream = mock.url();
    let p = spawn_proxy(cfg).await;
    // Claude Code lists the connectors authorised on claude.ai with its own
    // login token. Rotating that onto an account would answer with someone
    // else's connectors, so the request goes through untouched.
    for path in ["/v1/mcp_servers?limit=1000", "/api/organizations/org_1/mcp/start-auth/srv_1"] {
        let r = http()
            .get(p.url(path))
            .header("authorization", "Bearer CLIENT-OWN")
            .header("x-corrall-key", KEY)
            .header("anthropic-beta", "mcp-client-2025-04-04")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
    }
    let seen = mock.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, "/v1/mcp_servers?limit=1000", "the query survives");
    for s in &seen {
        assert_eq!(s.headers.get("authorization").unwrap(), "Bearer CLIENT-OWN", "{}", s.path);
        assert_eq!(s.headers.get("anthropic-beta").unwrap(), "mcp-client-2025-04-04");
        assert!(!s.headers.contains_key("x-corrall-key"), "the proxy key never leaves");
    }
}

#[tokio::test]
async fn proxy_key_header_authenticates_and_is_stripped() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    let c = http();
    // The header form of the key: what `corrall env` hands Claude Code through
    // ANTHROPIC_CUSTOM_HEADERS so ANTHROPIC_API_KEY can stay unset.
    let keyed = c.get(p.url("/corrall/status")).header("host", "attacker.example").header("x-corrall-key", KEY).send().await.unwrap();
    assert_eq!(keyed.status(), 200, "the header key works regardless of Host");
    let wrong = c.get(p.url("/corrall/status")).header("host", "attacker.example").header("x-corrall-key", "nope").send().await.unwrap();
    assert_eq!(wrong.status(), 401);
    mock.queue("tok-a", Behaviour::Ok);
    let r = c
        .post(p.url("/v1/messages"))
        .header("host", "attacker.example")
        .header("x-corrall-key", KEY)
        .header("authorization", "Bearer sk-ant-oat01-CLIENT-LOGIN")
        .json(&msg("claude-opus-5"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let seen = mock.seen();
    let last = seen.last().unwrap();
    assert_eq!(last.path, "/v1/messages");
    assert_eq!(token_of(&last.headers), "tok-a", "the account token is injected");
    assert!(!last.headers.contains_key("x-corrall-key"), "the proxy key never leaves");
}

#[tokio::test]
async fn websocket_upgrade_is_relayed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    cfg.upstream = mock.url();
    let p = spawn_proxy(cfg).await;
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
    s.write_all(format!("GET /v1/code/sessions/abc/ws HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n", p.port).as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let mut got = Vec::new();
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.unwrap().unwrap();
        got.extend_from_slice(&buf[..n]);
        if got.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&got);
    assert!(head.starts_with("HTTP/1.1 101"), "got: {head}");
    s.write_all(b"ping").await.unwrap();
    let mut echo = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while echo.len() < "ping echoed".len() && tokio::time::Instant::now() < deadline {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.unwrap().unwrap();
        if n == 0 {
            break;
        }
        echo.extend_from_slice(&buf[..n]);
    }
    assert_eq!(String::from_utf8_lossy(&echo), "ping echoed");
}

#[tokio::test]
async fn blocked_models_and_body_limits() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    pool_of(&mut cfg).blocked_models = vec!["*fable*".into()];
    cfg.proxy.max_body_bytes = 2048;
    let p = spawn_proxy(cfg).await;
    let (st, _, _) = post(&p, "/v1/messages", msg("claude-fable-5-1")).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let big = json!({ "model": "claude-opus-5", "messages": [{ "role": "user", "content": "x".repeat(5000) }] });
    let (st, _, _) = post(&p, "/v1/messages", big).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    let r = http().post(p.url("/corrall/switch")).body("a".repeat(70_000)).send().await.unwrap();
    assert_eq!(r.status(), 413);
    assert!(mock.seen().is_empty());
}

#[tokio::test]
async fn unauthorized_upstream_marks_account_and_fails_over() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 1, &mock.url())])).await;
    mock.queue("tok-a", Behaviour::Unauthorized);
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b");
    let status = p.manager.status(false);
    assert_eq!(status["accounts"][0]["status"], "error");
}

#[tokio::test]
async fn entitlement_denial_cools_the_account_down() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 1, &mock.url())])).await;
    mock.queue("tok-a", Behaviour::EntitlementDenied);
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b");
    let (_, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(v["served_by"], "tok-b", "a stays out during the cooldown");
    assert!(p.manager.status(false)["accounts"][0]["blocked"].as_str().unwrap().contains("oauth not allowed"));
}

#[tokio::test]
async fn overloaded_upstream_takes_one_failover_hop() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 1, &mock.url())])).await;
    mock.queue("tok-a", Behaviour::ServerError);
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b");
}

/// Every account answering 5xx is an upstream outage, not a quota exhaustion.
/// With fewer accounts than the attempt cap the loop used to re-select, find
/// nothing, and answer 429 `rate_limit_error` with a retry-after taken from
/// the healthy accounts' 5h/7d reset (up to an hour), so clients backed off
/// from a blip as if the fleet were spent.
#[tokio::test]
async fn every_account_overloaded_is_a_502_not_a_429() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 1, &mock.url())])).await;
    // Both accounts have healthy, far-off resets on record.
    let _ = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    mock.queue("tok-a", Behaviour::ServerError);
    mock.queue("tok-b", Behaviour::ServerError);
    let (st, v, h) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY, "{v}");
    assert_eq!(v["error"]["type"], "api_error");
    assert!(h.get("retry-after").is_none(), "no reset-derived retry-after: {:?}", h.get("retry-after"));
    let status = p.manager.status(false);
    assert_eq!(status["accounts"][0]["status"], "active", "a 5xx does not throttle the account");
    assert_eq!(status["accounts"][1]["status"], "active");
    // A quota-held sibling still yields the 429 with its recovery time.
    mock.queue("tok-a", Behaviour::QuotaRejected { retry_after: 300 });
    mock.queue("tok-b", Behaviour::ServerError);
    let (st, _, h) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    let ra: u64 = h.get("retry-after").unwrap().to_str().unwrap().parse().unwrap();
    assert!((250..=300).contains(&ra), "retry-after {ra} comes from a's throttle");
}

#[tokio::test]
async fn all_exhausted_returns_429_with_retry_after_then_hold_waits() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    mock.queue("tok-a", Behaviour::QuotaRejected { retry_after: 600 });
    let (st, _, h) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    assert!(h.get("retry-after").is_some());
    // With holdSeconds the request waits for the throttle to lift instead.
    let mock2 = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock2.url())]);
    pool_of(&mut cfg).hold_seconds = 20;
    let p2 = spawn_proxy(cfg).await;
    mock2.queue("tok-a", Behaviour::QuotaRejected { retry_after: 2 });
    let t0 = std::time::Instant::now();
    let (st, v, _) = post(&p2, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200, "held until the account recovered");
    assert_eq!(v["served_by"], "tok-a");
    assert!(t0.elapsed() >= Duration::from_secs(2));
}

#[tokio::test]
async fn storm_control_paces_a_fresh_account() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    pool_of(&mut cfg).storm_ramp.start_conc = 1;
    pool_of(&mut cfg).storm_ramp.step_conc = 1;
    pool_of(&mut cfg).storm_ramp.step_ms = 150;
    pool_of(&mut cfg).storm_ramp.window_ms = 10_000;
    let p = spawn_proxy(cfg).await;
    mock.delay_ms.store(120, Ordering::SeqCst);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let url = p.url("/v1/messages");
        tasks.push(tokio::spawn(async move { http().post(url).json(&msg("claude-opus-5")).send().await.unwrap().status().as_u16() }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), 200);
    }
    let max = mock.max_in_flight.load(Ordering::SeqCst);
    assert!(max <= 4, "burst was paced onto the fresh account (max in flight {max})");
}

#[tokio::test]
async fn distribute_sessions_pins_and_spreads() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 0, &mock.url())]);
    pool_of(&mut cfg).distribute_sessions = true;
    let p = spawn_proxy(cfg).await;
    let s1 = "11111111-1111-1111-1111-111111111111";
    let s2 = "22222222-2222-2222-2222-222222222222";
    let send = |sid: &'static str| {
        let url = p.url("/v1/messages");
        async move {
            let r = http().post(url).header("x-claude-code-session-id", sid).json(&msg("claude-opus-5")).send().await.unwrap();
            r.json::<Value>().await.unwrap()["served_by"].as_str().unwrap().to_string()
        }
    };
    let first = send(s1).await;
    let second = send(s2).await;
    assert_ne!(first, second, "two new sessions spread across equal-priority accounts");
    for _ in 0..3 {
        assert_eq!(send(s1).await, first, "session keeps its account for cache reuse");
        assert_eq!(send(s2).await, second);
    }
}

#[tokio::test]
async fn routes_restrict_and_route_pin_endpoint_works() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 5, &mock.url())]);
    pool_of(&mut cfg).routes.push(corrall::config::RouteConfig {
        name: "fable".into(),
        patterns: vec!["*fable*".into()],
        accounts: vec!["b".into()],
        ..Default::default()
    });
    let p = spawn_proxy(cfg).await;
    let (_, v, _) = post(&p, "/v1/messages", msg("claude-fable-5-1")).await;
    assert_eq!(v["served_by"], "tok-b");
    let (_, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(v["served_by"], "tok-a");
    let r = http().post(p.url("/corrall/switch")).json(&json!({ "account": "b" })).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let (_, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(v["served_by"], "tok-a", "a has strictly better priority, so the switch is a weak preference");
}

#[tokio::test]
async fn tool_pairs_are_repaired_and_model_map_applied() {
    let mock = spawn_mock().await;
    let mut third = account("deepseek", "sk-third", 0, &mock.url());
    third.kind = AccountType::Apikey;
    third.api_key = Some("sk-third".into());
    third.access_token = None;
    third.model_map = Some([("claude-sonnet-4-6".to_string(), "deepseek-v4".to_string())].into_iter().collect());
    third.strip_request_fields = vec!["context_management".into()];
    let p = spawn_proxy(cfg_with(vec![third])).await;
    let body = json!({
        "model": "claude-sonnet-4-6", "context_management": {},
        "messages": [
            { "role": "assistant", "content": [ { "type": "text", "text": "t" }, { "type": "tool_use", "id": "t1", "name": "x", "input": {} } ] },
            { "role": "user", "content": [ { "type": "text", "text": "no result for t1" } ] }
        ]
    });
    let (st, _, _) = post(&p, "/v1/messages", body).await;
    assert_eq!(st, 200);
    let seen = mock.seen();
    let sent = &seen[0].body;
    assert_eq!(sent["model"], "deepseek-v4");
    assert!(sent.get("context_management").is_none());
    assert_eq!(sent["messages"][0]["content"].as_array().unwrap().len(), 1, "orphaned tool_use dropped");
    assert_eq!(seen[0].headers.get("x-api-key").unwrap(), "sk-third");
    assert!(!seen[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn subscription_token_is_never_sent_to_a_third_party_host() {
    let mock = spawn_mock().await;
    // An OAuth account pointing at a non-Anthropic, non-loopback host: refused.
    let mut a = account("a", "tok-a", 0, "https://api.deepseek.com/anthropic");
    a.refresh_token = Some("rt".into());
    let p = spawn_proxy(cfg_with(vec![a, account("b", "tok-b", 1, &mock.url())])).await;
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b");
}

#[tokio::test]
async fn codex_requests_use_codex_pool_only() {
    let mock = spawn_mock().await;
    let mut codex = account("cx", "tok-cx", 0, &mock.url());
    codex.provider = Some("codex".into());
    codex.account_id = Some("acct_9".into());
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), codex])).await;
    let (st, _, _) = post(&p, "/backend-api/codex/responses", json!({ "model": "gpt-5", "input": "hi" })).await;
    assert_eq!(st, 200);
    let seen = mock.seen();
    assert_eq!(seen.last().unwrap().headers.get("authorization").unwrap(), "Bearer tok-cx");
    assert_eq!(seen.last().unwrap().headers.get("chatgpt-account-id").unwrap(), "acct_9");
    let (_, v, _) = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    assert_eq!(v["served_by"], "tok-a", "Anthropic requests never land on the Codex account");
}

/// The `/tc-acct/<pin>/` prefix was stripped from the query string the proxy
/// forwards but not from the path it routes on, so a pinned Codex request
/// looked like an Anthropic one and the Codex account was refused as "other
/// provider".
#[tokio::test]
async fn path_pin_is_stripped_before_provider_routing() {
    let mock = spawn_mock().await;
    let mut codex = account("cx", "tok-cx", 0, &mock.url());
    codex.provider = Some("codex".into());
    codex.account_id = Some("acct_9".into());
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), codex])).await;
    let (st, v, _) = post(&p, "/tc-acct/cx/backend-api/codex/responses", json!({ "model": "gpt-5", "input": "hi" })).await;
    assert_eq!(st, 200, "{v}");
    let seen = mock.seen();
    let last = seen.last().unwrap();
    assert_eq!(last.headers.get("authorization").unwrap(), "Bearer tok-cx", "the pinned Codex account served it");
    assert_eq!(last.path, "/backend-api/codex/responses", "upstream never sees the pin");
    // The Anthropic pin still works and keeps its query string.
    let (st, v, _) = post(&p, "/tc-acct/a/v1/messages?beta=true", msg("claude-opus-5")).await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["served_by"], "tok-a");
    assert_eq!(mock.seen().last().unwrap().path, "/v1/messages?beta=true");
}

#[tokio::test]
async fn usage_dimensions_are_consumed_and_attributed() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    cfg.proxy.usage_dimensions = vec![corrall::config::UsageDimension { name: "project".into(), header: "x-corrall-project".into() }];
    let p = spawn_proxy(cfg).await;
    let r = http().post(p.url("/v1/messages")).header("x-corrall-project", "NodeSpy/corrall").json(&msg("claude-opus-5")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(!mock.seen()[0].headers.contains_key("x-corrall-project"), "dimension header stays on this side");
    let st = p.manager.status(false);
    assert_eq!(st["usageDimensions"]["project"]["NodeSpy/corrall"]["totalRequests"], 1);
}

#[tokio::test]
async fn control_plane_endpoints() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    let _ = post(&p, "/v1/messages", msg("claude-opus-5")).await;
    let c = http();
    let metrics = c.get(p.url("/corrall/metrics")).send().await.unwrap().text().await.unwrap();
    assert!(metrics.contains("corrall_requests_total 1"));
    // Per-account series carry the pool they rotate in, so two pools holding
    // same-named accounts stay distinguishable.
    assert!(metrics.contains("corrall_account_quota_utilization{pool=\"default\",account=\"a\",bucket=\"unified5h\"} 0.4"), "{metrics}");
    let quota: Value = c.get(p.url("/corrall/quota")).send().await.unwrap().json().await.unwrap();
    assert_eq!(quota["pool"], "default");
    assert_eq!(quota["accounts"][0]["unified5h"], 0.4);
    let dash = c.get(p.url("/corrall/dashboard")).send().await.unwrap();
    assert_eq!(dash.status(), 200);
    assert!(dash.headers().get("content-security-policy").is_some());
    let reload = c.post(p.url("/corrall/reload")).send().await.unwrap();
    assert_eq!(reload.status(), 501, "no reload hook in tests");
    let _ = p.ctx.config();
}

#[tokio::test]
async fn mitm_connect_intercepts_with_local_ca_and_refuses_blind_tunnels() {
    let mock = spawn_mock().await;
    let mut cfg = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    cfg.upstream = mock.url();
    let p = spawn_proxy(cfg).await;
    let ca = std::fs::read(corrall::proxy::mitm::ca_cert_path())
        .or_else(|_| {
            p.ctx.tls_config().unwrap();
            std::fs::read(corrall::proxy::mitm::ca_cert_path())
        })
        .unwrap();
    let cert = reqwest::Certificate::from_pem(&ca).unwrap();
    let client = reqwest::Client::builder().proxy(reqwest::Proxy::all(p.url("")).unwrap()).add_root_certificate(cert).use_rustls_tls().build().unwrap();
    let r = client.get("https://www.example.org/").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("MITM proxy is working"));
    let r = client.post("https://api.anthropic.com/v1/messages").json(&msg("claude-opus-5")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["served_by"], "tok-a", "intercepted CONNECT went through account selection");
    let blind = client.get("https://example.com/").send().await;
    assert!(blind.is_err(), "blind tunnels are off by default");

    // With `mitm.http1Only` off the tunnel offers `h2` in ALPN; an h2-capable
    // client takes it, and the proxy has to actually serve HTTP/2 on that
    // stream rather than answer the first frame with an HTTP/1.1 parse error.
    let mut cfg2 = cfg_with(vec![account("a", "tok-a", 0, &mock.url())]);
    cfg2.upstream = mock.url();
    cfg2.mitm.http1_only = false;
    let p2 = spawn_proxy(cfg2).await;
    let cert = reqwest::Certificate::from_pem(&ca).unwrap();
    let h2 = reqwest::Client::builder().proxy(reqwest::Proxy::all(p2.url("")).unwrap()).add_root_certificate(cert).use_rustls_tls().build().unwrap();
    let r = h2.post("https://api.anthropic.com/v1/messages").json(&msg("claude-opus-5")).send().await.unwrap();
    assert_eq!(r.version(), reqwest::Version::HTTP_2, "the client negotiated h2 inside the tunnel");
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["served_by"], "tok-a", "an h2 request inside the tunnel is served");
}

// The management control routes: per-account enable/disable/priority, pool
// create/edit(+rename), and the manual OAuth login flow registry. These mutate
// the on-disk config the way the CLI does, so the test seeds that file first
// (spawn_proxy uses reload: None, so `reconcile` applies each change to the live
// ctx in place).
#[tokio::test]
async fn control_routes_manage_accounts_and_pools() {
    test_env();
    let mut seed = cfg_with(vec![account("a", "tok-a", 0, "http://127.0.0.1:1"), account("b", "tok-b", 1, "http://127.0.0.1:1")]);
    seed.proxy.api_key = KEY.into();
    seed.ensure_account_ids();
    // Seed the file the mutating routes read/write.
    Config::update(|c| {
        *c = seed.clone();
        Ok(())
    })
    .unwrap();
    let p = spawn_proxy(seed).await;

    let acct = |c: &Config, name: &str| c.pool(DEFAULT_POOL).unwrap().accounts.iter().find(|a| a.name == name).cloned().unwrap();

    // Disable, then re-enable — the account is addressed by name (find_account_mut
    // accepts it) and the change persists onto the on-disk AccountConfig.
    let (st, v, _) = post(&p, "/corrall/pools/default/accounts/a/disable", json!({})).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["ok"].as_bool().unwrap());
    assert!(acct(&p.ctx.config(), "a").disabled, "disable persisted");

    let (st, _, _) = post(&p, "/corrall/pools/default/accounts/a/enable", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!acct(&p.ctx.config(), "a").disabled, "enable persisted");

    // Priority.
    let (st, _, _) = post(&p, "/corrall/pools/default/accounts/a/priority", json!({ "priority": 7 })).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(acct(&p.ctx.config(), "a").priority, 7, "priority persisted");
    let (st, _, _) = post(&p, "/corrall/pools/default/accounts/a/priority", json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "priority needs a value");

    // Unknown account / pool 404s rather than mutating.
    let (st, _, _) = post(&p, "/corrall/pools/default/accounts/nope/disable", json!({})).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, _) = post(&p, "/corrall/pools/ghost/accounts/a/disable", json!({})).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Create a pool with the corrall knobs (threshold + distribute).
    let (st, v, _) = post(&p, "/corrall/pools", json!({ "name": "clients", "switchThreshold": 0.8, "distributeSessions": true })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    {
        let c = p.ctx.config();
        let pc = c.pool("clients").expect("pool created");
        assert_eq!(pc.switch_threshold, corrall::config::Threshold::Single(0.8));
        assert!(pc.distribute_sessions);
    }
    assert!(p.ctx.pools.get("clients").is_some(), "the live registry gained the pool");

    // Creating over an existing name is a conflict, not a silent edit.
    let (st, _, _) = post(&p, "/corrall/pools", json!({ "name": "clients" })).await;
    assert_eq!(st, StatusCode::CONFLICT);

    // Rename a pool: config moves and the live manager is re-keyed (not rebuilt).
    let (st, v, _) = post(&p, "/corrall/pools/clients", json!({ "newName": "vip" })).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["pool"], "vip");
    assert!(p.ctx.config().pool("clients").is_none());
    assert!(p.ctx.config().pool("vip").is_some());
    assert!(p.ctx.pools.get("vip").is_some());

    // Manual login registry: start hands back a flow + URL; cancel and a bogus
    // submit exercise the one-shot lifecycle without any network exchange.
    let (st, v, _) = post(&p, "/corrall/login/start", json!({ "pool": "vip" })).await;
    assert_eq!(st, StatusCode::OK);
    let flow = v["flow_id"].as_str().unwrap().to_string();
    assert!(v["authorize_url"].as_str().unwrap().contains("code_challenge"));
    let (st, _, _) = post(&p, "/corrall/login/cancel", json!({ "flow_id": flow })).await;
    assert_eq!(st, StatusCode::OK);
    // A submit against an unknown/cancelled flow is gone, not a 500.
    let (st, _, _) = post(&p, "/corrall/login/submit", json!({ "flow_id": flow, "code": "x#y" })).await;
    assert_eq!(st, StatusCode::GONE);
    let (st, _, _) = post(&p, "/corrall/login/submit", json!({ "code": "x" })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "submit needs a flow_id");
}

/// The headers budget is time-to-headers, not a total deadline. A stream that
/// keeps producing chunks past that budget must still arrive whole; before
/// this was fixed every response longer than the budget was cut mid-stream.
#[tokio::test]
async fn long_stream_is_not_cut_by_the_headers_timeout() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    // Six chunks 500ms apart: three seconds of streaming against a 1.5s budget.
    mock.queue("tok-a", Behaviour::SlowStream { chunks: 6, gap_ms: 500 });
    let mut body = msg("claude-sonnet-4-6");
    body["stream"] = json!(true);
    let started = std::time::Instant::now();
    let r = http().post(p.url("/v1/messages")).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.expect("stream must run to completion");
    assert!(started.elapsed() >= Duration::from_millis(2_500), "stream ended early after {:?}", started.elapsed());
    for i in 0..6 {
        assert!(text.contains(&format!("chunk{i}")), "missing chunk{i} in {text:?}");
    }
}

/// No response headers within the budget is a transient failure on that
/// account: the request moves to a sibling instead of waiting on it.
#[tokio::test]
async fn first_byte_timeout_fails_over_to_a_sibling() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url()), account("b", "tok-b", 1, &mock.url())])).await;
    mock.queue("tok-a", Behaviour::Hang { ms: 6_000 });
    let started = std::time::Instant::now();
    let (st, v, _) = post(&p, "/v1/messages", msg("claude-sonnet-4-6")).await;
    let took = started.elapsed();
    assert_eq!(st, 200);
    assert_eq!(v["served_by"], "tok-b");
    assert!(took >= Duration::from_millis(1_400) && took < Duration::from_millis(5_000), "took {took:?}");
    let status = p.manager.status(true);
    assert_eq!(status["accounts"][0]["usage"]["failedRequests"], 1);
}

/// A request the proxy gives up on still reports a completion, so it leaves
/// the in-flight list and shows up as ✗ in the log rather than vanishing.
#[tokio::test]
async fn abandoned_requests_report_completion() {
    let mock = spawn_mock().await;
    let p = spawn_proxy(cfg_with(vec![account("a", "tok-a", 0, &mock.url())])).await;
    mock.queue("tok-a", Behaviour::Hang { ms: 6_000 });
    let mut rx = p.ctx.activity.subscribe();
    let (st, _, h) = post(&p, "/v1/messages", msg("claude-sonnet-4-6")).await;
    assert_eq!(st, StatusCode::GATEWAY_TIMEOUT, "the only account timed out and nothing else could serve it");
    assert!(h.get("retry-after").is_none(), "a timeout is not a quota exhaustion");
    let mut ended = None;
    while let Ok(a) = rx.try_recv() {
        if let corrall::proxy::server::Activity::End { account, status, ok, .. } = a {
            ended = Some((account, status, ok));
        }
    }
    assert_eq!(ended, Some(("a".to_string(), 504, false)));
}
