//! OpenAI Codex subscription accounts: credential import from the Codex CLI's
//! `~/.codex/auth.json`, browser login, token refresh at `auth.openai.com`,
//! and normalisation of the `x-codex-*` quota headers into the same buckets
//! the Anthropic path fills.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::oauth::{RefreshError, Tokens};
use crate::quota::{now_ms, Bucket, Quota};
use crate::security::random_key;
use crate::upstream::{client, refresh_timeout};

pub const DEFAULT_CREDENTIALS_PATH: &str = "~/.codex/auth.json";
pub const UPSTREAM: &str = "https://chatgpt.com";
pub const HOST: &str = "chatgpt.com";
pub const NEVER_INTERCEPT: &str = "ab.chatgpt.com";
pub const PATH_PREFIX: &str = "/backend-api/codex";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const SCOPES: &str = "openid profile email offline_access";
const CALLBACK_PORT: u16 = 1455;
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub fn is_codex_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == HOST || h == "auth.openai.com"
}

pub fn is_codex_path(path: &str) -> bool {
    path == PATH_PREFIX || path.starts_with(&format!("{PATH_PREFIX}/"))
}

#[derive(Debug, Clone, Default)]
pub struct CodexCredentials {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub expires_at: Option<i64>,
}

fn jwt_claims(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let (_h, payload) = (parts.next()?, parts.next()?);
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn creds_from(tokens: &Value) -> CodexCredentials {
    let claims = tokens.get("id_token").and_then(Value::as_str).and_then(jwt_claims).unwrap_or(Value::Null);
    let auth = claims.get("https://api.openai.com/auth").cloned().unwrap_or(Value::Null);
    let access = tokens.get("access_token").and_then(Value::as_str).map(str::to_string);
    let expires_at = access
        .as_deref()
        .and_then(jwt_claims)
        .and_then(|c| c.get("exp").and_then(Value::as_i64))
        .map(|e| e * 1000)
        .or_else(|| tokens.get("expires_in").and_then(Value::as_i64).map(|s| now_ms() + s * 1000));
    CodexCredentials {
        access_token: access,
        refresh_token: tokens.get("refresh_token").and_then(Value::as_str).map(str::to_string),
        account_id: auth.get("chatgpt_account_id").and_then(Value::as_str).or_else(|| tokens.get("account_id").and_then(Value::as_str)).map(str::to_string),
        email: claims.get("email").and_then(Value::as_str).map(str::to_string),
        plan_type: auth.get("chatgpt_plan_type").and_then(Value::as_str).map(str::to_string),
        expires_at,
    }
}

pub fn import_credentials(path: &str) -> Result<CodexCredentials> {
    let p = crate::oauth::expand_home(path);
    let raw = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
    parse_credentials(&raw)
}

pub fn parse_credentials(raw: &[u8]) -> Result<CodexCredentials> {
    let v: Value = serde_json::from_slice(raw).context("Codex auth file is not JSON")?;
    let tokens = v.get("tokens").cloned().unwrap_or(v);
    Ok(creds_from(&tokens))
}

pub async fn refresh_access_token(refresh_token: &str) -> std::result::Result<Tokens, RefreshError> {
    let r = client()
        .post(TOKEN_URL)
        .timeout(refresh_timeout())
        .header("accept", "application/json")
        .json(&json!({ "grant_type": "refresh_token", "refresh_token": refresh_token, "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;
    let status = r.status();
    if !status.is_success() {
        let text = r.text().await.unwrap_or_default();
        if status.is_server_error() {
            return Err(RefreshError::Transient(format!("HTTP {status}")));
        }
        return Err(RefreshError::Rejected { status: status.as_u16(), detail: crate::security::safe_text(&text, 200) });
    }
    let data: Value = r.json().await.map_err(|e| RefreshError::Transient(e.to_string()))?;
    let c = creds_from(&data);
    Ok(Tokens {
        access_token: c.access_token.ok_or_else(|| RefreshError::Transient("no access_token".into()))?,
        refresh_token: c.refresh_token.or_else(|| Some(refresh_token.to_string())),
        expires_at: c.expires_at.unwrap_or_else(|| now_ms() + 3600 * 1000),
    })
}

/// Browser login. The redirect URI is fixed by the OpenAI client registration
/// (`http://localhost:1455/auth/callback`), so the listener must take that port.
pub async fn login_browser(open_browser: bool) -> Result<CodexCredentials> {
    let verifier = random_key(32);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    let state = random_key(32);
    let redirect = format!("http://localhost:{CALLBACK_PORT}/auth/callback");
    let mut u = url::Url::parse(AUTHORIZE_URL).unwrap();
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "codex_cli_rs");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .await
        .with_context(|| format!("port {CALLBACK_PORT} is busy (is the Codex CLI logging in?)"))?;
    eprintln!("Opening browser for OpenAI authentication...\nIf it doesn't open, visit:\n  {u}\n");
    if open_browser {
        let _ = webbrowser::open(u.as_str());
    }
    let code = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let (mut sock, peer) = listener.accept().await?;
            if !crate::security::is_loopback_ip(peer.ip()) {
                continue;
            }
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await?;
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_string();
            let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else { continue };
            if url.path() != "/auth/callback" {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                continue;
            }
            let q: BTreeMap<String, String> = url.query_pairs().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            if q.get("state") != Some(&state) {
                let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                continue;
            }
            if let Some(e) = q.get("error") {
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                bail!("OAuth error: {}", crate::security::safe_text(e, 100));
            }
            if let Some(c) = q.get("code") {
                let body = "<html><body><h2>Signed in. You can close this tab.</h2></body></html>";
                let _ = sock
                    .write_all(
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes(),
                    )
                    .await;
                return Ok::<String, anyhow::Error>(c.clone());
            }
        }
    })
    .await
    .map_err(|_| anyhow!("login timed out"))??;
    let r = client()
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(30))
        .header("accept", "application/json")
        .json(&json!({ "grant_type": "authorization_code", "code": code, "client_id": CLIENT_ID, "redirect_uri": redirect, "code_verifier": verifier }))
        .send()
        .await?;
    if !r.status().is_success() {
        let st = r.status();
        bail!("Codex token exchange failed: HTTP {st}: {}", crate::security::safe_text(&r.text().await.unwrap_or_default(), 300));
    }
    Ok(creds_from(&r.json().await?))
}

// ── quota headers ─────────────────────────────────────────────

const FIVE_HOUR_MINUTES: f64 = 300.0;
const SEVEN_DAY_MINUTES: f64 = 10080.0;

fn near(v: f64, target: f64) -> bool {
    (v - target).abs() <= target * 0.1
}

#[derive(Default)]
struct Window {
    used_percent: Option<f64>,
    window_minutes: Option<f64>,
    reset_at: Option<i64>,
}

#[derive(Default)]
struct FamilyWindows {
    name: Option<String>,
    windows: BTreeMap<String, Window>,
}

/// Apply `x-codex-*` headers (lowercased keys) to a quota. Limits arrive in
/// families: the unnamed family is account-wide, a family with `-limit-name`
/// is model-scoped. Windows are classified by `window-minutes`, never by
/// their `primary`/`secondary` position.
pub fn apply_codex_headers(q: &mut Quota, h: &BTreeMap<String, String>, now: i64) {
    let mut fams: BTreeMap<String, FamilyWindows> = BTreeMap::new();
    for (k, v) in h {
        let Some(rest) = k.strip_prefix("x-codex-") else { continue };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        if let Some(slug) = rest.strip_suffix("-limit-name") {
            fams.entry(slug.to_string()).or_default().name = Some(v.to_string());
            continue;
        }
        for field in ["used-percent", "window-minutes", "reset-at"] {
            let Some(head) = rest.strip_suffix(&format!("-{field}")) else { continue };
            let (slug, pos) = match head.rsplit_once('-') {
                Some((s, p)) if p == "primary" || p == "secondary" => (s.to_string(), p.to_string()),
                _ if head == "primary" || head == "secondary" => (String::new(), head.to_string()),
                _ => continue,
            };
            let w = fams.entry(slug).or_default().windows.entry(pos).or_default();
            match field {
                "used-percent" => w.used_percent = v.parse().ok(),
                "window-minutes" => w.window_minutes = v.parse().ok(),
                _ => w.reset_at = v.parse::<i64>().ok().filter(|r| *r > 0).map(|r| r * 1000),
            }
        }
    }
    let classify = |f: &FamilyWindows| -> (Option<Bucket>, Option<Bucket>) {
        let (mut five, mut weekly) = (None, None);
        for w in f.windows.values() {
            let (Some(m), Some(p)) = (w.window_minutes, w.used_percent) else { continue };
            let b = Bucket { utilization: Some(p / 100.0), reset_at: w.reset_at, seen_at: Some(now) };
            if near(m, FIVE_HOUR_MINUTES) {
                five = Some(b);
            } else if near(m, SEVEN_DAY_MINUTES) {
                weekly = Some(b);
            }
        }
        (five, weekly)
    };
    if let Some(acct) = fams.get("") {
        let (five, weekly) = classify(acct);
        if let Some(b) = five {
            q.unified5h = b;
        }
        if let Some(b) = weekly {
            q.unified7d = b;
        }
    }
    for (slug, f) in &fams {
        if slug.is_empty() {
            continue;
        }
        let (_, weekly) = classify(f);
        if let Some(b) = weekly {
            q.scoped_weekly.insert(f.name.clone().unwrap_or_else(|| slug.clone()).to_ascii_lowercase(), b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_by_minutes_not_position() {
        let mut h = BTreeMap::new();
        h.insert("x-codex-primary-used-percent".to_string(), "40".to_string());
        h.insert("x-codex-primary-window-minutes".to_string(), "10080".to_string());
        h.insert("x-codex-primary-reset-at".to_string(), "1800000000".to_string());
        h.insert("x-codex-secondary-used-percent".to_string(), "10".to_string());
        h.insert("x-codex-secondary-window-minutes".to_string(), "300".to_string());
        h.insert("x-codex-gpt5-limit-name".to_string(), "GPT-5".to_string());
        h.insert("x-codex-gpt5-primary-used-percent".to_string(), "90".to_string());
        h.insert("x-codex-gpt5-primary-window-minutes".to_string(), "10080".to_string());
        let mut q = Quota::default();
        apply_codex_headers(&mut q, &h, 1);
        assert_eq!(q.unified7d.utilization, Some(0.4));
        assert_eq!(q.unified7d.reset_at, Some(1_800_000_000_000));
        assert_eq!(q.unified5h.utilization, Some(0.1));
        assert_eq!(q.scoped_weekly.get("gpt-5").unwrap().utilization, Some(0.9));
    }

    #[test]
    fn credentials_from_auth_json() {
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"email":"me@x.com","https://api.openai.com/auth":{"chatgpt_account_id":"acct_1","chatgpt_plan_type":"pro"}}"#);
        let id_token = format!("h.{claims}.s");
        let raw = format!(r#"{{"tokens":{{"access_token":"a","refresh_token":"r","id_token":"{id_token}"}}}}"#);
        let c = parse_credentials(raw.as_bytes()).unwrap();
        assert_eq!(c.account_id.as_deref(), Some("acct_1"));
        assert_eq!(c.email.as_deref(), Some("me@x.com"));
        assert_eq!(c.plan_type.as_deref(), Some("pro"));
    }

    #[test]
    fn paths_and_hosts() {
        assert!(is_codex_path("/backend-api/codex/responses"));
        assert!(!is_codex_path("/v1/messages"));
        assert!(is_codex_host("chatgpt.com"));
    }
}
