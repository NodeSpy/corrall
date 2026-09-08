//! Forward one client request upstream with account selection, credential
//! injection, body rewrites, 429 classification, failover and streaming.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Response, StatusCode};
use serde_json::{json, Value};

use super::server::{BoxBody, Ctx, ReqInfo};
use crate::config::AccountType;
use crate::manager::{Manager, Provider, SelectRequest, Selected, Selection};
use crate::upstream::{body_idle_timeout, client, headers_timeout};

const MAX_ATTEMPTS: usize = 6;
const INLINE_RETRY_AFTER_MAX_SECONDS: u64 = 15;
const ERROR_BODY_INSPECTION_LIMIT: usize = 64 * 1024;

pub const HOP_BY_HOP: &[&str] =
    &["host", "connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade", "proxy-authorization", "proxy-authenticate", "proxy-connection"];
/// Client credentials never travel upstream; the proxy is the credential authority.
pub const CLIENT_CREDENTIAL_HEADERS: &[&str] = &["x-api-key", "authorization", "chatgpt-account-id", "cookie"];

fn rate_limit_absorb_max() -> u64 {
    std::env::var("TEAMCLAUDE_RATE_LIMIT_ABSORB_MAX_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60)
}

pub fn json_response(status: StatusCode, body: Value) -> Response<BoxBody> {
    let mut r = Response::new(Full::new(Bytes::from(body.to_string())).map_err(|e| match e {}).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert("content-type", HeaderValue::from_static("application/json"));
    r
}

pub fn error_response(status: StatusCode, kind: &str, message: &str) -> Response<BoxBody> {
    json_response(status, json!({ "type": "error", "error": { "type": kind, "message": message } }))
}

fn with_retry_after(mut r: Response<BoxBody>, secs: u64) -> Response<BoxBody> {
    if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
        r.headers_mut().insert("retry-after", v);
    }
    r
}

/// Outcome of one upstream attempt.
enum Attempt {
    Done(Response<BoxBody>),
    /// Try another account (this one is added to `tried`).
    Failover {
        reason: String,
        transient: bool,
    },
    /// Retry the same account after a wait (rate limit) or a token refresh.
    RetrySame {
        wait: Duration,
    },
    /// Client is gone or the request is unrecoverable.
    Abort(Response<BoxBody>),
}

/// `mgr` is the manager of `info.pool` — resolved once by the caller so every
/// attempt in this request stays inside the pool the client selected.
pub async fn forward(ctx: &Ctx, mgr: &Manager, info: &ReqInfo, mut headers: HeaderMap, body: Bytes) -> Response<BoxBody> {
    // Strip hop-by-hop, client credentials, dimension headers, and encodings
    // we cannot faithfully relay.
    let strip: HashSet<String> = ctx.dimension_headers();
    headers = headers
        .into_iter()
        .filter_map(|(k, v)| k.map(|k| (k, v)))
        .filter(|(k, _)| {
            let n = k.as_str();
            !HOP_BY_HOP.contains(&n) && !CLIENT_CREDENTIAL_HEADERS.contains(&n) && n != "accept-encoding" && !n.starts_with(':') && !strip.contains(n)
        })
        .collect();

    let session = info.session_id.as_deref();
    let (model, advisor) = (info.model.as_deref(), info.advisor_model.as_deref());
    let mut tried: HashSet<String> = HashSet::new();
    let hold_deadline = if info.hold_ms > 0 { Some(tokio::time::Instant::now() + Duration::from_millis(info.hold_ms)) } else { None };
    let mut attempts = 0usize;
    let mut same_account_retries = 0usize;
    let mut last_exhausted: Option<(u64, String)> = None;

    loop {
        attempts += 1;
        if attempts > MAX_ATTEMPTS * 3 {
            return error_response(StatusCode::BAD_GATEWAY, "api_error", "No account could serve the request after repeated attempts");
        }
        let sel = mgr.select(&SelectRequest {
            model,
            advisor_model: advisor,
            session_id: session,
            pin: info.pin.as_deref(),
            exclude: tried.clone(),
            allow_probe: true,
            provider: Some(info.provider),
        });
        let account = match sel {
            Selection::Account(a) => a,
            Selection::PinUnknown => {
                return error_response(StatusCode::NOT_FOUND, "not_found_error", "Pinned account is not configured on this proxy");
            }
            Selection::PinUnavailable { name, reason } => {
                let msg = format!(
                    "Pinned account \"{}\" cannot serve this request: {}",
                    crate::security::safe_text(&name, 80),
                    crate::security::safe_text(&reason, 120)
                );
                return with_retry_after(error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &msg), 30);
            }
            Selection::Exhausted { retry_after_secs, reason } => {
                last_exhausted = Some((retry_after_secs, reason.clone()));
                if let Some(deadline) = hold_deadline {
                    if tokio::time::Instant::now() < deadline {
                        let wait = Duration::from_secs(retry_after_secs.clamp(2, 30));
                        mgr.log(format!("All accounts exhausted; holding request {} for {}s", info.id, wait.as_secs()));
                        tokio::time::sleep(wait).await;
                        tried.clear();
                        continue;
                    }
                }
                tracing::warn!("exhausted: {reason}");
                let msg = if tried.is_empty() {
                    "All accounts have reached their quota. Retry after the soonest reset.".to_string()
                } else {
                    format!("No account could serve the request ({} tried).", tried.len())
                };
                return with_retry_after(error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &msg), retry_after_secs);
            }
        };

        ctx.notify_account(info, &account.name);
        match attempt(ctx, mgr, info, &account, &headers, &body).await {
            Attempt::Done(r) => {
                mgr.record_session(session, &account.id, model);
                return r;
            }
            Attempt::Abort(r) => return r,
            Attempt::Failover { reason, transient } => {
                tried.insert(account.id.clone());
                if transient {
                    mgr.record_failure(&account.id);
                }
                tracing::info!("failover off \"{}\": {reason}", account.name);
                if info.pin.is_some() {
                    // A pin never fails over.
                    return error_response(StatusCode::BAD_GATEWAY, "api_error", "Pinned account failed to serve the request");
                }
                if tried.len() >= MAX_ATTEMPTS {
                    let (ra, _) = last_exhausted.clone().unwrap_or((30, String::new()));
                    return with_retry_after(error_response(StatusCode::BAD_GATEWAY, "api_error", "Every eligible account failed to serve the request"), ra);
                }
            }
            Attempt::RetrySame { wait } => {
                same_account_retries += 1;
                if same_account_retries > 4 {
                    return with_retry_after(
                        error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", "Upstream rate limit persisted across retries"),
                        wait.as_secs().max(1),
                    );
                }
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                // Re-select with a pin so we land on the same account.
                tried.remove(&account.id);
            }
        }
    }
}

fn is_entitlement_denied(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/details/error_code").and_then(Value::as_str).map(str::to_string))
        .map(|c| c == "oauth_not_allowed_for_organization")
        .unwrap_or(false)
}

fn rl_headers(h: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    h.iter()
        .filter(|(k, _)| k.as_str().starts_with("anthropic-ratelimit-") || k.as_str().starts_with("x-codex-"))
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
        .collect()
}

async fn attempt(ctx: &Ctx, mgr: &Manager, info: &ReqInfo, account: &Selected, headers: &HeaderMap, body: &Bytes) -> Attempt {
    // Freshen the OAuth token first (coalesced across callers).
    let credential = match account.kind {
        AccountType::Oauth => match mgr.ensure_token_fresh(&account.id, false, crate::manager::OnRefreshFail::MarkDead).await {
            Some(c) => c,
            None => return Attempt::Failover { reason: "no usable token".into(), transient: false },
        },
        AccountType::Apikey => account.credential.clone(),
    };

    // Body rewrites, one pass over the body parsed once in `handle`. A Codex
    // (Responses API) body has no Anthropic shape to repair and is sent as is.
    let send = match account.provider {
        Provider::Codex => body.clone(),
        Provider::Anthropic => {
            super::body::apply_all(body, info.parsed.as_deref(), account.account_uuid.as_deref(), true, &account.model_map, &account.strip_request_fields)
        }
    };

    let url = format!("{}{}", account.upstream.trim_end_matches('/'), info.path_and_query);
    let method = reqwest::Method::from_bytes(info.method.as_bytes()).unwrap_or(reqwest::Method::POST);
    let mut req = client().request(method.clone(), &url).timeout(headers_timeout());
    let mut out_headers: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        if let Ok(s) = v.to_str() {
            req = req.header(k.as_str(), s);
            out_headers.push((k.as_str().to_string(), s.to_string()));
        }
    }
    match (account.provider, &account.kind) {
        (Provider::Codex, _) => {
            req = req.header("authorization", format!("Bearer {credential}"));
            out_headers.push(("authorization".into(), format!("Bearer {credential}")));
            if let Some(aid) = &account.account_id {
                req = req.header("chatgpt-account-id", aid);
                out_headers.push(("chatgpt-account-id".into(), aid.clone()));
            }
        }
        (Provider::Anthropic, AccountType::Oauth) => {
            req = req.header("authorization", format!("Bearer {credential}"));
            out_headers.push(("authorization".into(), format!("Bearer {credential}")));
        }
        (Provider::Anthropic, AccountType::Apikey) => {
            req = req.header("x-api-key", &credential);
            out_headers.push(("x-api-key".into(), credential.clone()));
        }
    }
    if method != reqwest::Method::GET && method != reqwest::Method::HEAD {
        req = req.header("content-length", send.len().to_string()).body(send.clone());
    }

    mgr.admit(&account.id).await;
    if info.pin.is_none() && mgr.is_entitlement_denied(&account.id) {
        mgr.release(&account.id);
        return Attempt::Failover { reason: "oauth not allowed for organization (cooldown)".into(), transient: false };
    }
    let started = std::time::Instant::now();
    let res = req.send().await;
    mgr.release(&account.id);

    let res = match res {
        Ok(r) => r,
        Err(e) => {
            let reason = describe_reqwest(&e);
            tracing::warn!("upstream error on \"{}\": {reason}", account.name);
            if e.is_timeout() || e.is_connect() || e.is_request() {
                return Attempt::Failover { reason, transient: true };
            }
            return Attempt::Failover { reason, transient: true };
        }
    };

    let status = res.status();
    let rl = rl_headers(res.headers());
    mgr.update_quota(&account.id, &rl);
    if status.as_u16() != 429 {
        mgr.clear_rate_limited(&account.id);
    }

    match status.as_u16() {
        429 => {
            let retry_after = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(60);
            let _ = res.bytes().await;
            let general_rejected = rl.get("anthropic-ratelimit-unified-5h-status").map(|s| s == "rejected").unwrap_or(false)
                || rl.get("anthropic-ratelimit-unified-7d-status").map(|s| s == "rejected").unwrap_or(false);
            let family_rejected = !general_rejected && rl.get("anthropic-ratelimit-unified-7d_oi-status").map(|s| s == "rejected").unwrap_or(false);
            if general_rejected || family_rejected {
                if family_rejected {
                    mgr.log(format!("Family weekly quota exhausted on \"{}\"; switching account for this request", account.name));
                } else {
                    let hold = retry_after.clamp(1, 3600) as u64;
                    mgr.log(format!("Quota rejection (429) on \"{}\"; throttling {hold}s and switching account", account.name));
                    mgr.mark_rate_limited(&account.id, hold);
                }
                return Attempt::Failover { reason: "quota rejected".into(), transient: false };
            }
            // Per-minute rate limit: pause the account, retry the same one.
            let ra = retry_after.clamp(1, 300) as u64;
            mgr.mark_rate_limited(&account.id, ra);
            if ra <= INLINE_RETRY_AFTER_MAX_SECONDS || (ra <= rate_limit_absorb_max() && info.hold_ms > 0) {
                mgr.log(format!("Rate-limit 429 on \"{}\"; waiting {ra}s and retrying the same account", account.name));
                return Attempt::RetrySame { wait: Duration::from_secs(ra) };
            }
            if info.pin.is_none() {
                // One failover hop onto an idle sibling; a second throttle is IP-scoped.
                mgr.log(format!("Rate-limit 429 on \"{}\" (retry-after {ra}s); one failover hop", account.name));
                return Attempt::Failover { reason: format!("rate limited {ra}s"), transient: false };
            }
            return Attempt::Abort(with_retry_after(error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", "Upstream rate limit"), ra));
        }
        401 => {
            let bytes = res.bytes().await.unwrap_or_default();
            if account.kind == AccountType::Oauth {
                mgr.log(format!("401 from upstream on \"{}\"; refreshing token", account.name));
                let before = account.credential.clone();
                let after = mgr.ensure_token_fresh(&account.id, true, crate::manager::OnRefreshFail::MarkDead).await;
                if after.is_some() && after.as_deref() != Some(before.as_str()) {
                    return Attempt::RetrySame { wait: Duration::ZERO };
                }
                mgr.mark_error(&account.id, "upstream rejected the token (401)");
                return Attempt::Failover { reason: "401 and refresh did not help".into(), transient: false };
            }
            mgr.mark_error(&account.id, "upstream rejected the API key (401)");
            let _ = bytes;
            return Attempt::Failover { reason: "401".into(), transient: false };
        }
        403 => {
            let bytes = read_limited(res).await;
            if is_entitlement_denied(&bytes) {
                mgr.mark_entitlement_denied(&account.id);
                mgr.log(format!("Organization of \"{}\" does not allow OAuth; cooling it down 5 minutes", account.name));
                return Attempt::Failover { reason: "oauth_not_allowed_for_organization".into(), transient: false };
            }
            if info.pin.is_none() {
                return Attempt::Failover { reason: "403".into(), transient: false };
            }
            return Attempt::Done(json_response(
                StatusCode::FORBIDDEN,
                serde_json::from_slice(&bytes).unwrap_or(json!({"type":"error","error":{"type":"permission_error","message":"forbidden"}})),
            ));
        }
        500..=599 => {
            let bytes = read_limited(res).await;
            if info.pin.is_none() {
                return Attempt::Failover { reason: format!("upstream {status}"), transient: true };
            }
            let mut r = Response::new(Full::new(bytes).map_err(|e| match e {}).boxed());
            *r.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            r.headers_mut().insert("content-type", HeaderValue::from_static("application/json"));
            return Attempt::Done(r);
        }
        _ => {}
    }

    // Success or a client-side 4xx: relay headers + stream the body.
    let mut resp = Response::builder().status(status.as_u16());
    let mut res_headers: Vec<(String, String)> = Vec::new();
    for (k, v) in res.headers() {
        let n = k.as_str();
        if HOP_BY_HOP.contains(&n) || n == "content-length" {
            continue;
        }
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(n.as_bytes()), HeaderValue::from_bytes(v.as_bytes())) {
            resp = resp.header(name, val);
            res_headers.push((n.to_string(), v.to_str().unwrap_or("").to_string()));
        }
    }
    let is_sse = res.headers().get("content-type").and_then(|v| v.to_str().ok()).map(|c| c.contains("text/event-stream")).unwrap_or(false);
    let manager = mgr.clone();
    let account_id = account.id.clone();
    let session = info.session_id.clone();
    let client_name = info.client.clone();
    let model_owned = info.model.clone();
    let dims = info.dimensions.clone();
    let logger = ctx.logger.clone();
    let log_req = (info.id.clone(), account.name.clone(), info.method.clone(), url.clone(), out_headers, send.clone(), status.as_u16(), res_headers);
    let elapsed_notify = ctx.clone();
    let info_c = info.clone();
    let acct_name = account.name.clone();

    if is_sse {
        let idle = body_idle_timeout();
        let mut usage_acc = UsageAccumulator::default();
        let mut upstream_stream = res.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(32);
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut logged_body: Vec<u8> = Vec::new();
            let mut ok = true;
            loop {
                match tokio::time::timeout(idle, upstream_stream.next()).await {
                    Ok(Some(Ok(chunk))) => {
                        if logger.is_some() && logged_body.len() < 4 * 1024 * 1024 {
                            logged_body.extend_from_slice(&chunk);
                        }
                        usage_acc.feed(&mut buf, &chunk);
                        if tx.send(Ok(Frame::data(chunk))).await.is_err() {
                            break; // client gone
                        }
                    }
                    Ok(Some(Err(e))) => {
                        tracing::warn!("upstream stream error: {e}");
                        let _ = tx.send(Err(std::io::Error::other("upstream stream error"))).await;
                        ok = false;
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        tracing::warn!("upstream stream idle timeout");
                        let _ = tx.send(Err(std::io::Error::other("upstream idle timeout"))).await;
                        ok = false;
                        break;
                    }
                }
            }
            usage_acc.flush(&buf);
            if let Some(u) = usage_acc.merged() {
                manager.record_token_usage_dims(&account_id, session.as_deref(), client_name.as_deref(), &dims, model_owned.as_deref(), &u);
            }
            elapsed_notify.notify_end(&info_c, &acct_name, status.as_u16(), started.elapsed(), ok);
            if let Some(l) = logger {
                let (id, acct, method, url, oh, sb, st, rh) = log_req;
                let body = Bytes::from(logged_body);
                tokio::task::spawn_blocking(move || l.write(&id, &acct, &method, &url, &oh, &sb, st, &rh, Some(&body)));
            }
        });
        let stream = tokio_stream_from(rx);
        let body = StreamBody::new(stream).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>).boxed();
        return Attempt::Done(resp.body(body).unwrap());
    }

    // Non-streaming: buffer (bounded by upstream), record usage, relay.
    let bytes = match tokio::time::timeout(body_idle_timeout() * 4, res.bytes()).await {
        Ok(Ok(b)) => b,
        _ => {
            ctx.notify_end(info, &account.name, 502, started.elapsed(), false);
            return Attempt::Failover { reason: "upstream body read failed".into(), transient: true };
        }
    };
    if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
        if let Some(u) = v.get("usage") {
            mgr.record_token_usage_dims(&account.id, info.session_id.as_deref(), info.client.as_deref(), &info.dimensions, info.model.as_deref(), u);
        }
    }
    ctx.notify_end(info, &account.name, status.as_u16(), started.elapsed(), status.is_success());
    if let Some(l) = ctx.logger.clone() {
        let (id, acct, method, url, oh, sb, st, rh) = log_req;
        let b = bytes.clone();
        tokio::task::spawn_blocking(move || l.write(&id, &acct, &method, &url, &oh, &sb, st, &rh, Some(&b)));
    }
    Attempt::Done(resp.body(Full::new(bytes).map_err(|e| match e {}).boxed()).unwrap())
}

fn tokio_stream_from(
    rx: tokio::sync::mpsc::Receiver<Result<Frame<Bytes>, std::io::Error>>,
) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> {
    futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) })
}

async fn read_limited(res: reqwest::Response) -> Bytes {
    let mut out = Vec::new();
    let mut s = res.bytes_stream();
    while let Some(Ok(c)) = s.next().await {
        out.extend_from_slice(&c);
        if out.len() > ERROR_BODY_INSPECTION_LIMIT {
            break;
        }
    }
    Bytes::from(out)
}

fn describe_reqwest(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else if e.is_connect() {
        "connect failed".into()
    } else if e.is_body() || e.is_decode() {
        "body error".into()
    } else {
        "request failed".into()
    }
}

/// Merges the usage a streamed message reports across `message_start`
/// (input side) and `message_delta` (cumulative output side).
#[derive(Default)]
struct UsageAccumulator {
    merged: Option<Value>,
}

impl UsageAccumulator {
    fn feed(&mut self, buf: &mut Vec<u8>, chunk: &[u8]) {
        buf.extend_from_slice(chunk);
        while let Some(pos) = find_double_newline(buf) {
            let event: Vec<u8> = buf.drain(..pos + 2).collect();
            self.event(&event);
        }
        if buf.len() > 1024 * 1024 {
            buf.clear();
        }
    }

    fn flush(&mut self, buf: &[u8]) {
        if !buf.is_empty() {
            self.event(buf);
        }
    }

    fn event(&mut self, raw: &[u8]) {
        let text = String::from_utf8_lossy(raw);
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data:") else { continue };
            let Ok(v) = serde_json::from_str::<Value>(data.trim()) else { continue };
            let usage = match v.get("type").and_then(Value::as_str) {
                Some("message_start") => v.pointer("/message/usage").cloned(),
                Some("message_delta") => v.get("usage").cloned(),
                _ => None,
            };
            if let Some(Value::Object(u)) = usage {
                let m = self.merged.get_or_insert_with(|| json!({}));
                for (k, val) in u {
                    if val.is_number() {
                        m[k] = val;
                    }
                }
            }
        }
    }

    fn merged(&self) -> Option<Value> {
        self.merged.clone()
    }
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_merge() {
        let mut acc = UsageAccumulator::default();
        let mut buf = Vec::new();
        acc.feed(&mut buf, b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":5,\"output_tokens\":1}}}\n\n");
        acc.feed(&mut buf, b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\n");
        let m = acc.merged().unwrap();
        assert_eq!(m["input_tokens"], 10);
        assert_eq!(m["output_tokens"], 42);
        assert_eq!(m["cache_read_input_tokens"], 5);
    }

    #[test]
    fn entitlement_denial() {
        assert!(is_entitlement_denied(br#"{"error":{"details":{"error_code":"oauth_not_allowed_for_organization"}}}"#));
        assert!(!is_entitlement_denied(br#"{"error":{"message":"nope"}}"#));
    }
}
