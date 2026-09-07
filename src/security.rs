//! Small security primitives shared across the crate: constant-time key
//! comparison, loopback detection, private-address checks, secret redaction and
//! private atomic file writes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

use base64::Engine;
use rand::RngCore;
use subtle::ConstantTimeEq;

/// URL-safe random string of `bytes` random bytes.
pub fn random_key(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Constant-time equality that does not leak the length of the secret.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let len_ok = (a.len() as u64).ct_eq(&(b.len() as u64));
    // Compare against a same-length buffer so the loop runs the same either way.
    let mut padded = vec![0u8; a.len()];
    for (i, p) in padded.iter_mut().enumerate() {
        *p = *b.get(i).unwrap_or(&0);
    }
    (a.ct_eq(&padded) & len_ok).into()
}

pub fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().map(|v4| v4.is_loopback()).unwrap_or(false),
    }
}

pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    h.parse::<IpAddr>().map(is_loopback_ip).unwrap_or(false)
}

/// True for addresses that must never be reached through a blind tunnel:
/// loopback, RFC1918, link-local, ULA, multicast, unspecified, cloud metadata.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_v4(v4);
            }
            is_private_v6(v6)
        }
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || v4.is_multicast()
        || v4.octets()[0] == 0
        || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1])) // CGNAT
        || v4 == Ipv4Addr::new(169, 254, 169, 254)
}

fn is_private_v6(v6: Ipv6Addr) -> bool {
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
        || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
}

/// Show only a short prefix of a secret in logs.
pub fn redact(secret: &str) -> String {
    if secret.len() <= 12 {
        return "***".to_string();
    }
    format!("{}…", &secret[..10])
}

/// Strip control characters (and ANSI escapes) from text that came from the
/// network before it reaches a terminal or a log line.
pub fn safe_text(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    for ch in s.chars() {
        if out.chars().count() >= max {
            out.push('…');
            break;
        }
        if ch.is_control() || ch == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&ch) {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Write `data` to `path` with mode 0600 via a temp file + rename, creating the
/// parent directory (0700) if needed. Never leaves a half-written file behind.
pub fn write_private_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
            set_mode(dir, 0o700);
        }
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    set_mode(&tmp, 0o600);
    std::fs::rename(&tmp, path)?;
    set_mode(path, 0o600);
    Ok(())
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_basics() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("", ""));
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::ffff:127.0.0.1"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("10.0.0.1"));
    }

    #[test]
    fn private_ips() {
        assert!(is_private_ip("10.1.2.3".parse().unwrap()));
        assert!(is_private_ip("169.254.169.254".parse().unwrap()));
        assert!(is_private_ip("fd00::1".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn safe_text_strips_controls() {
        assert_eq!(safe_text("a\x1b[31mb\n", 10), "a [31mb ");
    }
}
