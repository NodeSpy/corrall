//! Anthropic OAuth: PKCE browser login, paste login, token refresh, profile and
//! usage lookups, and import of Claude Code's own credential store.
//!
//! Hardening relative to the original: the callback listener binds loopback
//! only, `state` is verified before the `error` parameter is trusted, refresh
//! tokens only ever go to the fixed token endpoint, and subscription tokens
//! are only presented to Anthropic hosts (see `is_anthropic_host`).

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::quota::{normalize_usage, now_ms, UsagePayload};
use crate::security::random_key;
use crate::upstream::{client, refresh_timeout};

pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const MANUAL_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
pub const USAGE_BETA: &str = "oauth-2025-04-20";
pub const DEFAULT_CREDENTIALS_PATH: &str = "~/.claude/.credentials.json";

/// Hosts a subscription (OAuth) bearer token may be sent to.
pub fn is_anthropic_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "api.anthropic.com" || h.ends_with(".anthropic.com") || h == "platform.claude.com" || h.ends_with(".claude.com")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// ms since epoch
    pub expires_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("refresh token rejected (HTTP {status}): {detail}")]
    Rejected { status: u16, detail: String },
    #[error("token refresh failed: {0}")]
    Transient(String),
}

impl RefreshError {
    pub fn is_auth_rejection(&self) -> bool {
        matches!(self, RefreshError::Rejected { .. })
    }
}

/// Normalise an `expires_at` that may be seconds or milliseconds.
pub fn normalize_expires_at(v: Option<&Value>) -> Option<i64> {
    let n = v?.as_f64()?;
    if n <= 0.0 {
        return None;
    }
    Some(if n < 1e12 { (n * 1000.0) as i64 } else { n as i64 })
}

fn tokens_from_response(data: &Value, fallback_refresh: Option<&str>) -> Result<Tokens> {
    let access = data.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow!("token response has no access_token"))?.to_string();
    let refresh = data.get("refresh_token").and_then(Value::as_str).map(str::to_string).or_else(|| fallback_refresh.map(str::to_string));
    let expires_at = normalize_expires_at(data.get("expires_at")).unwrap_or_else(|| {
        let secs = data.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
        now_ms() + secs * 1000
    });
    Ok(Tokens { access_token: access, refresh_token: refresh, expires_at })
}

pub fn is_expiring_soon(expires_at: Option<i64>, threshold_ms: i64) -> bool {
    match expires_at {
        None => true,
        Some(e) => e - now_ms() < threshold_ms,
    }
}

/// Refresh with retry on 5xx / network errors. The endpoint is fixed.
pub async fn refresh_access_token(refresh_token: &str) -> std::result::Result<Tokens, RefreshError> {
    let mut last = String::new();
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt - 1))).await;
        }
        let res = client()
            .post(TOKEN_URL)
            .timeout(refresh_timeout())
            .header("accept", "application/json")
            .json(&json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": CLIENT_ID,
            }))
            .send()
            .await;
        match res {
            Ok(r) => {
                let status = r.status();
                if status.is_success() {
                    let data: Value = r.json().await.map_err(|e| RefreshError::Transient(e.to_string()))?;
                    return tokens_from_response(&data, Some(refresh_token)).map_err(|e| RefreshError::Transient(e.to_string()));
                }
                let text = r.text().await.unwrap_or_default();
                if status.is_server_error() {
                    last = format!("HTTP {status}");
                    continue;
                }
                let detail = crate::security::safe_text(&text, 200);
                return Err(RefreshError::Rejected { status: status.as_u16(), detail });
            }
            Err(e) => {
                last = e.to_string();
                continue;
            }
        }
    }
    Err(RefreshError::Transient(last))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub account_uuid: Option<String>,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub organization_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub seat_tier: Option<String>,
    pub has_claude_max: Option<bool>,
    pub has_claude_pro: Option<bool>,
}

pub fn normalize_profile(d: &Value) -> Profile {
    let s = |p: &str| d.pointer(p).and_then(Value::as_str).map(str::to_string);
    Profile {
        account_uuid: s("/account/uuid"),
        email: s("/account/email"),
        display_name: s("/account/display_name"),
        org_uuid: s("/organization/uuid"),
        org_name: s("/organization/name"),
        organization_type: s("/organization/organization_type"),
        rate_limit_tier: s("/organization/rate_limit_tier"),
        seat_tier: s("/organization/seat_tier"),
        has_claude_max: d.pointer("/account/has_claude_max").and_then(Value::as_bool),
        has_claude_pro: d.pointer("/account/has_claude_pro").and_then(Value::as_bool),
    }
}

pub async fn fetch_profile(access_token: &str) -> Result<Profile> {
    let r = client().get(PROFILE_URL).timeout(Duration::from_secs(20)).bearer_auth(access_token).send().await.context("profile request failed")?;
    if !r.status().is_success() {
        let st = r.status();
        let body = r.text().await.unwrap_or_default();
        bail!("profile lookup failed: HTTP {st}: {}", crate::security::safe_text(&body, 200));
    }
    Ok(normalize_profile(&r.json().await?))
}

#[derive(Debug)]
pub enum UsageResult {
    Ok(Box<UsagePayload>),
    Unauthorized,
    /// HTTP 429 from the usage endpoint. Seen for every account of a fleet at
    /// once, so it is a limit on the caller, not on the account's quota.
    RateLimited(String),
    Error(String),
}

pub async fn fetch_usage(access_token: &str) -> UsageResult {
    let r = client()
        .get(USAGE_URL)
        .timeout(Duration::from_secs(20))
        .bearer_auth(access_token)
        .header("anthropic-beta", USAGE_BETA)
        .header("accept", "application/json")
        .send()
        .await;
    match r {
        Err(e) => UsageResult::Error(e.to_string()),
        Ok(r) if r.status().as_u16() == 401 => UsageResult::Unauthorized,
        Ok(r) if r.status().as_u16() == 429 => {
            let body = r.text().await.unwrap_or_default();
            UsageResult::RateLimited(crate::security::safe_text(&body, 160))
        }
        Ok(r) if !r.status().is_success() => {
            let st = r.status();
            let body = r.text().await.unwrap_or_default();
            UsageResult::Error(format!("HTTP {st}: {}", crate::security::safe_text(&body, 160)))
        }
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => UsageResult::Ok(Box::new(normalize_usage(&v))),
            Err(e) => UsageResult::Error(e.to_string()),
        },
    }
}

// ── login flows ───────────────────────────────────────────────

struct Pkce {
    verifier: String,
    challenge: String,
    state: String,
}

fn pkce() -> Pkce {
    let verifier = random_key(32);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    Pkce { verifier, challenge, state: random_key(32) }
}

fn authorize_url(p: &Pkce, redirect_uri: &str) -> String {
    let mut u = url::Url::parse(AUTHORIZE_URL).unwrap();
    u.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &p.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &p.state);
    u.to_string()
}

async fn exchange_code(code: &str, state: &str, verifier: &str, redirect_uri: &str) -> Result<Tokens> {
    let r = client()
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(30))
        .json(&json!({
            "code": code,
            "state": state,
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        }))
        .send()
        .await
        .context("token exchange request failed")?;
    if !r.status().is_success() {
        let st = r.status();
        let body = r.text().await.unwrap_or_default();
        bail!("token exchange failed: HTTP {st}: {}", crate::security::safe_text(&body, 300));
    }
    tokens_from_response(&r.json().await?, None)
}

/// Parse pasted input: a full callback URL, `code#state`, or a bare code.
pub fn parse_auth_code(input: &str, expected_state: &str) -> Result<(String, String)> {
    let t = input.trim();
    if t.is_empty() {
        bail!("empty input");
    }
    if let Ok(u) = url::Url::parse(t) {
        let mut code = None;
        let mut state = None;
        for (k, v) in u.query_pairs() {
            match &*k {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                _ => {}
            }
        }
        if let Some(c) = code {
            if let Some(s) = &state {
                if s != expected_state {
                    bail!("OAuth state mismatch");
                }
            }
            return Ok((c, expected_state.to_string()));
        }
    }
    if let Some((c, s)) = t.split_once('#') {
        let (c, s) = (c.trim(), s.trim());
        if !c.is_empty() {
            if !s.is_empty() && s != expected_state {
                bail!("OAuth state mismatch");
            }
            return Ok((c.to_string(), expected_state.to_string()));
        }
    }
    if !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') {
        bail!("that does not look like an authorization code");
    }
    Ok((t.to_string(), expected_state.to_string()))
}

/// Browser login with a loopback callback server. Falls back to a pasted
/// code/URL typed on stdin.
pub async fn login_browser(open_browser: bool) -> Result<Tokens> {
    let p = pkce();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("binding callback listener")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/callback");
    let url = authorize_url(&p, &redirect_uri);

    eprintln!("Opening browser for authentication...");
    eprintln!("If it doesn't open, visit:\n  {url}\n");
    eprintln!("Or paste the redirect URL / authorization code here and press Enter.");
    if open_browser {
        let _ = webbrowser::open(&url);
    }

    let state = p.state.clone();
    let callback = tokio::spawn(callback_server(listener, state));
    let stdin_state = p.state.clone();
    let stdin_task = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(l)) => {
                    if l.trim().is_empty() {
                        continue;
                    }
                    return parse_auth_code(&l, &stdin_state).map(|(c, _)| c);
                }
                Ok(None) => return Err(anyhow!("stdin closed")),
                Err(e) => return Err(e.into()),
            }
        }
    });

    let code = tokio::select! {
        r = callback => r.context("callback task")??,
        r = stdin_task => r.context("stdin task")??,
        _ = tokio::time::sleep(Duration::from_secs(180)) => bail!("login timed out after 3 minutes"),
    };
    eprintln!("Exchanging authorization code for tokens...");
    exchange_code(&code, &p.state, &p.verifier, &redirect_uri).await
}

/// PKCE material for a manual (copy/paste) login that is carried across two
/// separate calls — the `authorize_url` is handed out first, then the pasted
/// code is exchanged later. Used by both `login_paste` (stdin) and the control
/// API's two-step `/corrall/login/{start,submit}` flow, where the two halves
/// happen on different HTTP connections.
pub struct ManualLogin {
    pub verifier: String,
    pub state: String,
}

/// Begin a manual login: returns the URL the user opens in a browser plus the
/// PKCE material needed to later exchange the code it hands back. No network I/O.
pub fn start_manual_login() -> (String, ManualLogin) {
    let p = pkce();
    let url = authorize_url(&p, MANUAL_REDIRECT_URI);
    (url, ManualLogin { verifier: p.verifier, state: p.state })
}

/// Finish a manual login: parse the pasted `code#state` / URL / bare code and
/// exchange it for tokens against the fixed manual redirect URI.
pub async fn finish_manual_login(pending: &ManualLogin, pasted: &str) -> Result<Tokens> {
    let (code, state) = parse_auth_code(pasted, &pending.state)?;
    exchange_code(&code, &state, &pending.verifier, MANUAL_REDIRECT_URI).await
}

/// Copy/paste login for headless machines: no local listener at all.
pub async fn login_paste() -> Result<Tokens> {
    let (url, pending) = start_manual_login();
    eprintln!("Open this URL in any browser, log in, then paste the code shown:\n  {url}\n");
    eprint!("Code: ");
    let mut line = String::new();
    tokio::io::BufReader::new(tokio::io::stdin()).read_line(&mut line).await?;
    finish_manual_login(&pending, &line).await
}

async fn callback_server(listener: TcpListener, expected_state: String) -> Result<String> {
    loop {
        let (mut sock, peer) = listener.accept().await?;
        if !crate::security::is_loopback_ip(peer.ip()) {
            continue;
        }
        let mut buf = vec![0u8; 8192];
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await.unwrap_or(Ok(0))?;
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        let Some(line) = head.lines().next() else { continue };
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else { continue };
        if method != "GET" {
            let _ = sock.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").await;
            continue;
        }
        let Ok(u) = url::Url::parse(&format!("http://localhost{target}")) else { continue };
        if u.path() != "/callback" {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").await;
            continue;
        }
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for (k, v) in u.query_pairs() {
            match &*k {
                "code" => code = Some(v.to_string()),
                "state" => state = Some(v.to_string()),
                "error" => error = Some(v.to_string()),
                _ => {}
            }
        }
        // State first: an unauthenticated peer must not be able to abort the
        // login by hitting /callback?error=... without the state.
        if state.as_deref() != Some(expected_state.as_str()) {
            let _ = respond(&mut sock, "HTTP/1.1 400 Bad Request", "<h2>Authentication failed</h2><p>State mismatch. You can close this tab.</p>").await;
            continue;
        }
        if let Some(e) = error {
            let _ = respond(&mut sock, "HTTP/1.1 200 OK", "<h2>Authentication failed</h2><p>You can close this tab.</p>").await;
            bail!("OAuth error: {}", crate::security::safe_text(&e, 100));
        }
        if let Some(c) = code {
            let _ = sock
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: https://platform.claude.com/oauth/code/success?app=claude-code\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                .await;
            return Ok(c);
        }
        let _ = respond(&mut sock, "HTTP/1.1 400 Bad Request", "<h2>Missing code</h2>").await;
    }
}

async fn respond(sock: &mut tokio::net::TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let html = format!("<html><body>{body}</body></html>");
    let msg = format!("{status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len());
    sock.write_all(msg.as_bytes()).await
}

// ── import from Claude Code ───────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ImportedCredentials {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

pub fn expand_home(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    std::path::PathBuf::from(p)
}

/// Read Claude Code's credential file (`{"claudeAiOauth": {...}}` or flat).
pub fn import_credentials(path: &str) -> Result<ImportedCredentials> {
    let p = expand_home(path);
    let raw = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
    parse_credentials(&raw)
}

pub fn parse_credentials(raw: &[u8]) -> Result<ImportedCredentials> {
    let v: Value = serde_json::from_slice(raw).context("credentials file is not JSON")?;
    let inner = v.get("claudeAiOauth").cloned().unwrap_or(v);
    let mut c: ImportedCredentials = serde_json::from_value(inner).context("unexpected credentials shape")?;
    if let Some(e) = c.expires_at {
        if e > 0 && e < 1_000_000_000_000 {
            c.expires_at = Some(e * 1000);
        }
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_codes() {
        let (c, s) = parse_auth_code("abc#st", "st").unwrap();
        assert_eq!((c.as_str(), s.as_str()), ("abc", "st"));
        assert!(parse_auth_code("abc#other", "st").is_err());
        let (c, _) = parse_auth_code("http://localhost:1/callback?code=xyz&state=st", "st").unwrap();
        assert_eq!(c, "xyz");
        assert!(parse_auth_code("http://localhost:1/callback?code=xyz&state=bad", "st").is_err());
        assert!(parse_auth_code("has space", "st").is_err());
    }

    #[test]
    fn credentials_shapes() {
        let c = parse_credentials(br#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1700000000}}"#).unwrap();
        assert_eq!(c.access_token.as_deref(), Some("a"));
        assert_eq!(c.expires_at, Some(1_700_000_000_000));
        let c = parse_credentials(br#"{"accessToken":"a","expiresAt":1700000000000}"#).unwrap();
        assert_eq!(c.expires_at, Some(1_700_000_000_000));
    }

    #[test]
    fn anthropic_hosts() {
        assert!(is_anthropic_host("api.anthropic.com"));
        assert!(!is_anthropic_host("api.deepseek.com"));
        assert!(!is_anthropic_host("anthropic.com.evil.example"));
    }

    #[test]
    fn manual_login_url_carries_pkce_and_round_trips_state() {
        let (url, pending) = start_manual_login();
        let u = url::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        // The authorize URL targets the manual redirect and carries the PKCE
        // challenge + the very state the pending flow will verify against.
        assert_eq!(q.get("redirect_uri").map(String::as_str), Some(MANUAL_REDIRECT_URI));
        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
        assert_eq!(q.get("state"), Some(&pending.state));
        assert!(q.contains_key("code_challenge"));
        // The state the redirect page hands back is accepted; a forged one is not.
        let (code, state) = parse_auth_code(&format!("thecode#{}", pending.state), &pending.state).unwrap();
        assert_eq!((code.as_str(), state.as_str()), ("thecode", pending.state.as_str()));
        assert!(parse_auth_code("thecode#forged", &pending.state).is_err());
    }
}
