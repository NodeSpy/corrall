//! Passthrough paths that must not receive an injected credential: the
//! client's own OAuth token refresh (`/v1/oauth/token`), `/api/oauth/*`, and
//! Remote Control (`/v1/code/*`, including its WebSocket upgrade). Hop-by-hop
//! headers and the proxy key are stripped; the client's `authorization` is
//! kept because it is the client's own session with upstream.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::forward::{error_response, HOP_BY_HOP};
use super::server::BoxBody;
use crate::upstream::{body_idle_timeout, client, headers_timeout};

pub fn is_passthrough_path(path: &str) -> bool {
    path == "/v1/oauth/token" || path.starts_with("/api/oauth/") || path.starts_with("/v1/code/")
}

fn keep_header(name: &str) -> bool {
    !HOP_BY_HOP.contains(&name) && name != "x-api-key" && name != "accept-encoding" && !name.starts_with(':')
}

/// Relay a plain request to `upstream` with the client's own headers.
pub async fn passthrough(upstream: &str, parts: hyper::http::request::Parts, body: Bytes) -> Response<BoxBody> {
    let path = parts.uri.path_and_query().map(|p| p.to_string()).unwrap_or_else(|| "/".into());
    let url = format!("{}{}", upstream.trim_end_matches('/'), path);
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut req = client().request(method.clone(), &url);
    for (k, v) in &parts.headers {
        if keep_header(k.as_str()) {
            if let Ok(s) = v.to_str() {
                req = req.header(k.as_str(), s);
            }
        }
    }
    if method != reqwest::Method::GET && method != reqwest::Method::HEAD {
        req = req.header("content-length", body.len().to_string()).body(body);
    }
    // Headers budget around `send()` only; the body below is bounded by its
    // idle gap, not by a total deadline (see forward.rs).
    let res = match tokio::time::timeout(headers_timeout(), req.send()).await {
        Ok(Ok(r)) => r,
        Err(_) => {
            tracing::warn!("passthrough {path}: no response headers within {:.0}s", headers_timeout().as_secs_f64());
            return error_response(StatusCode::GATEWAY_TIMEOUT, "api_error", "Upstream did not respond in time");
        }
        Ok(Err(e)) => {
            tracing::warn!("passthrough {path}: {e}");
            return error_response(StatusCode::BAD_GATEWAY, "api_error", "Upstream unreachable");
        }
    };
    let mut resp = Response::builder().status(res.status().as_u16());
    for (k, v) in res.headers() {
        if HOP_BY_HOP.contains(&k.as_str()) || k.as_str() == "content-length" {
            continue;
        }
        if let (Ok(n), Ok(val)) = (HeaderName::from_bytes(k.as_str().as_bytes()), HeaderValue::from_bytes(v.as_bytes())) {
            resp = resp.header(n, val);
        }
    }
    let idle = body_idle_timeout();
    let mut s = res.bytes_stream();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(16);
    tokio::spawn(async move {
        loop {
            match tokio::time::timeout(idle, s.next()).await {
                Ok(Some(Ok(c))) => {
                    if tx.send(Ok(Frame::data(c))).await.is_err() {
                        break;
                    }
                }
                Ok(Some(Err(_))) | Err(_) => {
                    let _ = tx.send(Err(std::io::Error::other("upstream stream error"))).await;
                    break;
                }
                Ok(None) => break,
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|i| (i, rx)) });
    resp.body(StreamBody::new(stream).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>).boxed()).unwrap()
}

enum Upstream {
    Plain(tokio::net::TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
}

impl tokio::io::AsyncRead for Upstream {
    fn poll_read(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Upstream::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}
impl tokio::io::AsyncWrite for Upstream {
    fn poll_write(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Upstream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Upstream::Tls(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Upstream::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Upstream::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

async fn connect(upstream: &str) -> anyhow::Result<(Upstream, String)> {
    let u = url::Url::parse(upstream)?;
    let host = u.host_str().ok_or_else(|| anyhow::anyhow!("no host"))?.to_string();
    let tls = u.scheme() == "https";
    let port = u.port().unwrap_or(if tls { 443 } else { 80 });
    let tcp = tokio::time::timeout(Duration::from_secs(20), tokio::net::TcpStream::connect((host.as_str(), port))).await??;
    let _ = tcp.set_nodelay(true);
    if !tls {
        return Ok((Upstream::Plain(tcp), format!("{host}:{port}")));
    }
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.clone())?;
    let s = connector.connect(name, tcp).await?;
    Ok((Upstream::Tls(Box::new(s)), if port == 443 { host } else { format!("{host}:{port}") }))
}

/// Relay an HTTP/1.1 Upgrade (WebSocket) end to end: write the request head
/// to upstream, read its response head, and if it is a 101 splice the two
/// sockets together once hyper hands us the client's upgraded connection.
pub async fn relay_upgrade(upstream: &str, req: Request<Incoming>) -> Response<BoxBody> {
    let (mut up, host_header) = match connect(upstream).await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("upgrade relay connect: {e}");
            return error_response(StatusCode::BAD_GATEWAY, "api_error", "Upstream unreachable");
        }
    };
    let path = req.uri().path_and_query().map(|p| p.to_string()).unwrap_or_else(|| "/".into());
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", req.method(), path, host_header);
    for (k, v) in req.headers() {
        let n = k.as_str();
        let keep = n == "connection" || n == "upgrade" || n.starts_with("sec-websocket-") || keep_header(n);
        if keep && n != "host" {
            if let Ok(s) = v.to_str() {
                if !s.contains(['\r', '\n']) {
                    head.push_str(&format!("{n}: {s}\r\n"));
                }
            }
        }
    }
    head.push_str("\r\n");
    if up.write_all(head.as_bytes()).await.is_err() {
        return error_response(StatusCode::BAD_GATEWAY, "api_error", "Upstream unreachable");
    }
    // Read the upstream response head.
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let head_end = loop {
        let mut chunk = [0u8; 4096];
        let n = match tokio::time::timeout(headers_timeout(), up.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => return error_response(StatusCode::BAD_GATEWAY, "api_error", "Upstream closed during upgrade"),
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return error_response(StatusCode::BAD_GATEWAY, "api_error", "Upstream error during upgrade"),
        };
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 64 * 1024 {
            return error_response(StatusCode::BAD_GATEWAY, "api_error", "Upstream response head too large");
        }
    };
    let head_text = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let leftover = buf[head_end..].to_vec();
    let mut lines = head_text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(502);
    let mut resp = Response::builder().status(status);
    let mut content_length: Option<usize> = None;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        if k == "content-length" {
            content_length = v.parse().ok();
        }
        if k == "transfer-encoding" || k == "content-length" {
            continue;
        }
        if let (Ok(n), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            resp = resp.header(n, val);
        }
    }
    if status == 101 {
        tokio::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    let mut client_io = TokioIo::new(upgraded);
                    if !leftover.is_empty() && client_io.write_all(&leftover).await.is_err() {
                        return;
                    }
                    let _ = tokio::io::copy_bidirectional(&mut client_io, &mut up).await;
                }
                Err(e) => tracing::debug!("client upgrade failed: {e}"),
            }
        });
        return resp.body(Full::new(Bytes::new()).map_err(|e| match e {}).boxed()).unwrap();
    }
    // Not an upgrade: read a bounded body and relay it.
    let mut body = leftover;
    let want = content_length.unwrap_or(0).min(1024 * 1024);
    while body.len() < want {
        let mut chunk = vec![0u8; 8192];
        match tokio::time::timeout(Duration::from_secs(10), up.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
            Ok(Ok(n)) => body.extend_from_slice(&chunk[..n]),
        }
    }
    resp.body(Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passthrough_paths() {
        assert!(is_passthrough_path("/v1/oauth/token"));
        assert!(is_passthrough_path("/v1/code/sessions/x/ws"));
        assert!(is_passthrough_path("/api/oauth/usage"));
        assert!(!is_passthrough_path("/v1/messages"));
    }
}
