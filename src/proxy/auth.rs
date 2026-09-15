//! Client authentication for the local listener and the CONNECT path.
//!
//! Fixes relative to the original: the loopback exemption also requires a
//! loopback `Host` header (DNS-rebinding defence), browser-originated requests
//! are refused on every path, and `x-api-key`/`authorization` from the client
//! never travel upstream.
//!
//! The key may also arrive in [`PROXY_KEY_HEADER`]. Claude Code drops its
//! claude.ai login (and with it the connectors authorised there) as soon as
//! `ANTHROPIC_API_KEY` is set, so `corrall env` hands the key over in a header
//! of its own through `ANTHROPIC_CUSTOM_HEADERS` instead. Stripped upstream
//! like the other credential headers.

use std::net::IpAddr;

use base64::Engine;
use hyper::header::HeaderMap;

use crate::config::ProxyConfig;
use crate::security::{ct_eq, is_loopback_host, is_loopback_ip};

/// Header a client may carry the proxy key in without touching the
/// `x-api-key` / `authorization` slots its own upstream login uses.
pub const PROXY_KEY_HEADER: &str = "x-corrall-key";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// Authenticated with the shared key.
    Shared,
    /// Authenticated with a named client key.
    Client(String),
    /// Loopback exemption.
    Loopback,
    Denied(&'static str),
}

impl Auth {
    #[allow(dead_code)]
    pub fn ok(&self) -> bool {
        !matches!(self, Auth::Denied(_))
    }
    pub fn client_name(&self) -> Option<&str> {
        match self {
            Auth::Client(n) => Some(n),
            _ => None,
        }
    }
}

pub fn check_key(cfg: &ProxyConfig, presented: Option<&str>) -> Option<Auth> {
    let p = presented?.trim();
    if p.is_empty() {
        return None;
    }
    if cfg.api_key.len() >= 16 && ct_eq(p, &cfg.api_key) {
        return Some(Auth::Shared);
    }
    for k in &cfg.client_keys {
        if k.key.len() >= 16 && ct_eq(p, &k.key) {
            return Some(Auth::Client(k.name.clone()));
        }
    }
    None
}

/// A request that a browser made from a web page carries `Origin` or a
/// `Sec-Fetch-Site` other than `none`. curl and the CLIs send neither.
pub fn is_browser_initiated(h: &HeaderMap) -> bool {
    if h.contains_key("origin") {
        return true;
    }
    match h.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        None | Some("none") => false,
        Some(_) => true,
    }
}

/// Authenticate an HTTP request. `host_header` is the request's `Host`.
pub fn authenticate(cfg: &ProxyConfig, peer: IpAddr, headers: &HeaderMap, host_header: Option<&str>) -> Auth {
    let presented = headers
        .get(PROXY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .or_else(|| headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer tc-").map(|_| &v[7..])));
    if let Some(a) = check_key(cfg, presented) {
        return a;
    }
    if is_browser_initiated(headers) {
        return Auth::Denied("browser-originated requests are refused");
    }
    if is_loopback_ip(peer) && !cfg.require_key_on_loopback {
        // DNS rebinding: a page at attacker.example resolved to 127.0.0.1 would
        // otherwise be trusted. Require the client to have addressed us by a
        // loopback name.
        let host_ok = host_header
            .map(|h| {
                let h = h.trim();
                let bare = if let Some(i) = h.rfind(':') {
                    if h.starts_with('[') || !h[..i].contains(':') {
                        &h[..i]
                    } else {
                        h
                    }
                } else {
                    h
                };
                is_loopback_host(bare)
            })
            .unwrap_or(true);
        if host_ok {
            return Auth::Loopback;
        }
        return Auth::Denied("loopback request with a non-loopback Host header");
    }
    Auth::Denied("invalid proxy API key")
}

/// Parse `Proxy-Authorization: Basic base64(user:pass)` into (user, pass).
pub fn parse_proxy_basic(h: &HeaderMap) -> Option<(String, String)> {
    let v = h.get("proxy-authorization")?.to_str().ok()?;
    let b64 = v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic "))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (u, p) = s.split_once(':').unwrap_or((&s, ""));
    let dec = |x: &str| percent_encoding::percent_decode_str(x).decode_utf8_lossy().to_string();
    Some((dec(u), dec(p)))
}

/// CONNECT authentication: the key may ride in the Basic password (pin in the
/// username) or the username itself.
pub fn authenticate_connect(cfg: &ProxyConfig, peer: IpAddr, headers: &HeaderMap) -> (Auth, Option<String>) {
    let basic = parse_proxy_basic(headers);
    let (user, pass) = basic.clone().unwrap_or_default();
    if let Some(a) = check_key(cfg, Some(&pass)) {
        return (a, Some(user).filter(|u| !u.is_empty()));
    }
    if let Some(a) = check_key(cfg, Some(&user)) {
        return (a, None);
    }
    if is_loopback_ip(peer) && !cfg.require_key_on_loopback {
        return (Auth::Loopback, Some(user).filter(|u| !u.is_empty()));
    }
    (Auth::Denied("proxy authentication required"), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::{HeaderName, HeaderValue};

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            api_key: "tc-0123456789abcdef0123456789".into(),
            client_keys: vec![crate::config::ClientKey { name: "alice".into(), key: "tc-alice-0123456789abcdef".into() }],
            ..Default::default()
        }
    }

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn keys() {
        let c = cfg();
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let remote: IpAddr = "10.0.0.5".parse().unwrap();
        assert_eq!(authenticate(&c, remote, &hm(&[("x-api-key", "tc-0123456789abcdef0123456789")]), Some("x")), Auth::Shared);
        assert_eq!(authenticate(&c, remote, &hm(&[("x-api-key", "tc-alice-0123456789abcdef")]), None), Auth::Client("alice".into()));
        assert!(!authenticate(&c, remote, &hm(&[("x-api-key", "wrong")]), None).ok());
        assert!(!authenticate(&c, remote, &hm(&[]), None).ok());
        assert_eq!(authenticate(&c, lo, &hm(&[]), Some("localhost:3456")), Auth::Loopback);
        assert_eq!(authenticate(&c, lo, &hm(&[]), Some("[::1]:3456")), Auth::Loopback);
    }

    #[test]
    fn rebinding_and_browsers_denied() {
        let c = cfg();
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(!authenticate(&c, lo, &hm(&[]), Some("attacker.example:3456")).ok());
        assert!(!authenticate(&c, lo, &hm(&[("origin", "http://localhost:3456")]), Some("localhost:3456")).ok());
        assert!(!authenticate(&c, lo, &hm(&[("sec-fetch-site", "same-origin")]), Some("localhost:3456")).ok());
        assert!(authenticate(&c, lo, &hm(&[("sec-fetch-site", "none")]), Some("localhost:3456")).ok());
        let mut strict = cfg();
        strict.require_key_on_loopback = true;
        assert!(!authenticate(&strict, lo, &hm(&[]), Some("localhost")).ok());
    }

    #[test]
    fn connect_basic() {
        let c = cfg();
        let remote: IpAddr = "10.0.0.5".parse().unwrap();
        let cred = base64::engine::general_purpose::STANDARD.encode("me%40example.com:tc-0123456789abcdef0123456789");
        let (a, pin) = authenticate_connect(&c, remote, &hm(&[("proxy-authorization", &format!("Basic {cred}"))]));
        assert_eq!(a, Auth::Shared);
        assert_eq!(pin.as_deref(), Some("me@example.com"));
        let (a, _) = authenticate_connect(&c, remote, &hm(&[]));
        assert!(!a.ok());
    }
}
