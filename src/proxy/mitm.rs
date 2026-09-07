//! MITM forward proxy: a local CA + leaf minted with rcgen so a client launched
//! with `HTTPS_PROXY` pointed at us can be intercepted for the Anthropic host
//! even when it hardcodes `api.anthropic.com`.
//!
//! The CA private key is generated in memory and discarded; only the leaf key
//! is persisted (0600). Blind tunnels to other hosts are OFF by default and,
//! when enabled, refuse private/loopback destinations and non-443 ports.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType};
use tokio_rustls::rustls::ServerConfig;

use crate::config::config_dir;
use crate::security::write_private_atomic;

pub const CA_CERT: &str = "teamclaude-ca.pem";
const LEAF_CERT: &str = "teamclaude-leaf.pem";
const LEAF_KEY: &str = "teamclaude-leaf.key";
pub const TEST_HOST: &str = "www.example.org";

pub fn ca_cert_path() -> PathBuf {
    config_dir().join(CA_CERT)
}

pub struct Certs {
    pub ca_pem: String,
    pub leaf_pem: String,
    pub leaf_key_pem: String,
}

fn leaf_covers(leaf_pem: &str, ca_pem: &str, hosts: &[String]) -> bool {
    use x509_parser::prelude::*;
    let Ok((_, leaf_der)) = x509_parser::pem::parse_x509_pem(leaf_pem.as_bytes()) else { return false };
    let Ok((_, ca_der)) = x509_parser::pem::parse_x509_pem(ca_pem.as_bytes()) else { return false };
    let (Ok((_, leaf)), Ok((_, ca))) = (leaf_der.parse_x509().map(|c| ((), c)), ca_der.parse_x509().map(|c| ((), c))) else { return false };
    if leaf.verify_signature(Some(ca.public_key())).is_err() {
        return false;
    }
    if !leaf.validity().is_valid() {
        return false;
    }
    let sans: Vec<String> = leaf
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|ext| {
            ext.value
                .general_names
                .iter()
                .filter_map(|g| match g {
                    GeneralName::DNSName(d) => Some(d.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    hosts.iter().all(|h| sans.iter().any(|s| s.eq_ignore_ascii_case(h)))
}

fn generate(hosts: &[String]) -> Result<Certs> {
    let ca_key = KeyPair::generate().context("generating CA key")?;
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "TeamClaude Local CA");
    dn.push(DnType::OrganizationName, "TeamClaude (local MITM, not a public CA)");
    ca_params.distinguished_name = dn;
    ca_params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    ca_params.not_after = time_after_days(3650);
    let ca_cert = ca_params.self_signed(&ca_key).context("signing CA")?;

    let leaf_key = KeyPair::generate().context("generating leaf key")?;
    let mut leaf_params = CertificateParams::default();
    leaf_params.is_ca = IsCa::ExplicitNoCa;
    leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, hosts.first().cloned().unwrap_or_else(|| "api.anthropic.com".into()));
    leaf_params.distinguished_name = dn;
    leaf_params.subject_alt_names = hosts.iter().filter_map(|h| h.clone().try_into().ok().map(SanType::DnsName)).collect();
    leaf_params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    leaf_params.not_after = time_after_days(397);
    let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).context("signing leaf")?;

    Ok(Certs { ca_pem: ca_cert.pem(), leaf_pem: leaf_cert.pem(), leaf_key_pem: leaf_key.serialize_pem() })
}

fn time_after_days(days: i64) -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc() + time::Duration::days(days)
}

/// Ensure a CA + leaf covering `hosts` exist on disk, regenerating the chain
/// when missing or stale. The CA key is never persisted.
pub fn ensure_certs(hosts: &[String]) -> Result<Certs> {
    let mut hosts: Vec<String> = hosts.iter().filter(|h| !h.is_empty()).cloned().collect();
    if !hosts.iter().any(|h| h == TEST_HOST) {
        hosts.push(TEST_HOST.to_string());
    }
    hosts.sort();
    hosts.dedup();
    let dir = config_dir();
    let read = |n: &str| std::fs::read_to_string(dir.join(n)).ok();
    if let (Some(ca), Some(leaf), Some(key)) = (read(CA_CERT), read(LEAF_CERT), read(LEAF_KEY)) {
        if leaf_covers(&leaf, &ca, &hosts) {
            return Ok(Certs { ca_pem: ca, leaf_pem: leaf, leaf_key_pem: key });
        }
        tracing::info!("MITM leaf does not cover {:?}; regenerating certificate chain", hosts);
    }
    let c = generate(&hosts)?;
    write_private_atomic(&dir.join(LEAF_KEY), c.leaf_key_pem.as_bytes())?;
    write_private_atomic(&dir.join(LEAF_CERT), c.leaf_pem.as_bytes())?;
    write_private_atomic(&dir.join(CA_CERT), c.ca_pem.as_bytes())?;
    // The CA cert is public by nature; let other users on the box trust it.
    crate::security::set_mode(&dir.join(CA_CERT), 0o644);
    tracing::info!("Generated MITM CA at {}", dir.join(CA_CERT).display());
    Ok(c)
}

pub fn tls_config(certs: &Certs, http1_only: bool) -> Result<Arc<ServerConfig>> {
    let cert_chain: Vec<_> = rustls_pemfile::certs(&mut certs.leaf_pem.as_bytes()).collect::<std::result::Result<_, _>>().context("parsing leaf cert")?;
    let key = rustls_pemfile::private_key(&mut certs.leaf_key_pem.as_bytes()).context("parsing leaf key")?.context("no private key in leaf PEM")?;
    let mut cfg = ServerConfig::builder().with_no_client_auth().with_single_cert(cert_chain, key).context("building TLS config")?;
    cfg.alpn_protocols = if http1_only { vec![b"http/1.1".to_vec()] } else { vec![b"h2".to_vec(), b"http/1.1".to_vec()] };
    Ok(Arc::new(cfg))
}

/// Parse a CONNECT authority into (host, port).
pub fn parse_authority(a: &str) -> Result<(String, u16)> {
    let a = a.trim();
    if let Some(rest) = a.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").context("bad IPv6 authority")?;
        return Ok((host.to_string(), port.parse().context("bad port")?));
    }
    let (host, port) = a.rsplit_once(':').context("CONNECT authority must be host:port")?;
    if host.is_empty() || host.contains('/') {
        bail!("bad host");
    }
    Ok((host.to_ascii_lowercase(), port.parse().context("bad port")?))
}

#[derive(Debug, PartialEq, Eq)]
pub enum HostMode {
    Intercept,
    Test,
    Tunnel,
    Refuse(&'static str),
}

pub fn host_mode(host: &str, port: u16, intercept_hosts: &[String], mitm: &crate::config::MitmConfig) -> HostMode {
    if host.eq_ignore_ascii_case(TEST_HOST) {
        return HostMode::Test;
    }
    if intercept_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) && port == 443 {
        return HostMode::Intercept;
    }
    if !mitm.allow_tunnel {
        return HostMode::Refuse("blind tunnels are disabled (mitm.allowTunnel)");
    }
    if !mitm.tunnel_allow.is_empty() {
        let want = format!("{host}:{port}");
        if !mitm.tunnel_allow.iter().any(|a| a.eq_ignore_ascii_case(&want) || a.eq_ignore_ascii_case(host)) {
            return HostMode::Refuse("host not in mitm.tunnelAllow");
        }
    } else if port != 443 {
        return HostMode::Refuse("blind tunnels are limited to port 443");
    }
    if crate::security::is_loopback_host(host) {
        return HostMode::Refuse("tunnel to loopback refused");
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if crate::security::is_private_ip(ip) {
            return HostMode::Refuse("tunnel to private address refused");
        }
    }
    HostMode::Tunnel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_parsing() {
        assert_eq!(parse_authority("api.anthropic.com:443").unwrap(), ("api.anthropic.com".into(), 443));
        assert_eq!(parse_authority("[::1]:8443").unwrap(), ("::1".into(), 8443));
        assert!(parse_authority("nope").is_err());
    }

    #[test]
    fn modes() {
        let hosts = vec!["api.anthropic.com".to_string()];
        let mut m = crate::config::MitmConfig::default();
        assert_eq!(host_mode("api.anthropic.com", 443, &hosts, &m), HostMode::Intercept);
        assert_eq!(host_mode("www.example.org", 443, &hosts, &m), HostMode::Test);
        assert!(matches!(host_mode("github.com", 443, &hosts, &m), HostMode::Refuse(_)));
        m.allow_tunnel = true;
        assert_eq!(host_mode("github.com", 443, &hosts, &m), HostMode::Tunnel);
        assert!(matches!(host_mode("github.com", 22, &hosts, &m), HostMode::Refuse(_)));
        assert!(matches!(host_mode("10.0.0.1", 443, &hosts, &m), HostMode::Refuse(_)));
        assert!(matches!(host_mode("localhost", 443, &hosts, &m), HostMode::Refuse(_)));
    }

    #[test]
    fn generates_chain_covering_hosts() {
        let c = generate(&["api.anthropic.com".into(), TEST_HOST.into()]).unwrap();
        assert!(leaf_covers(&c.leaf_pem, &c.ca_pem, &["api.anthropic.com".into()]));
        assert!(!leaf_covers(&c.leaf_pem, &c.ca_pem, &["other.example".into()]));
        assert!(tls_config(&c, true).is_ok());
    }
}
