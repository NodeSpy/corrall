//! Updating an installed release in place, and the notify-only check that
//! tells the operator a release exists. Nothing here ever runs unattended:
//! `corrall update` installs only when invoked, and the background check
//! only records the latest tag for `status` and the TUI to show.
//!
//! Adapted from draft PR #3 by @danielcbaldwin, with a version fence, a
//! backup + health-checked restart, and the passive check added.
//!
//! The repository is private, so every network hop goes through the GitHub CLI
//! instead of a bare HTTPS fetch: `gh` already holds a token with access to it,
//! and this way the updater carries no credential handling of its own. The
//! steps mirror `scripts/install.sh` — resolve the tag, download the archive
//! plus `SHA256SUMS`, verify the checksum (and the Sigstore signature when
//! cosign is installed), then swap the binary over with a rename inside its own
//! directory, which is atomic and safe to do while a server is running.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use sha2::{Digest, Sha256};

use crate::security::safe_text;

const UNIT: &str = "corrall.service";
const GH_HINT: &str = "the GitHub CLI (gh) is required: the repository is private, so its releases cannot be downloaded anonymously. Install gh and run `gh auth login`, or set GH_TOKEN";

#[derive(Args, Debug, Default)]
pub struct UpdateArgs {
    /// Report the latest release without installing it
    #[arg(long)]
    pub check: bool,
    /// Install this tag instead of the latest release (e.g. v2.1.0)
    #[arg(long, value_name = "TAG")]
    pub version: Option<String>,
    /// Replace this file instead of the running binary
    #[arg(long, value_name = "PATH")]
    pub binary: Option<PathBuf>,
    /// Allow a jump to a new major version (which may change behaviour)
    #[arg(long)]
    pub allow_major: bool,
    /// Do not restart the systemd --user unit after swapping the binary
    #[arg(long)]
    pub no_restart: bool,
    /// Skip the post-restart health check (and its automatic rollback)
    #[arg(long)]
    pub no_health_check: bool,
}

/// The release this binary was built from.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `owner/repo` to update from. Overridable so a fork — or a test — can point
/// the updater somewhere else, the same knob `scripts/install.sh` offers.
pub fn repo() -> String {
    std::env::var("CORRALL_REPO").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "NodeSpy/corrall".to_string())
}

pub fn run(args: &UpdateArgs) -> Result<()> {
    let repo = repo();
    let current = current_version();
    let target = match &args.binary {
        Some(p) => p.clone(),
        None => std::env::current_exe().context("locating the running binary")?,
    };

    let latest = match &args.version {
        Some(v) => v.trim().to_string(),
        None => latest_tag(&repo)?,
    };

    // `--check` answers a question rather than touching the filesystem, so it
    // stays useful even for a build this updater would refuse to replace.
    if args.check {
        if same_version(current, &latest) {
            println!("Up to date ({current}).");
        } else {
            println!("Update available: {current} → {latest}\nRun `corrall update` to install it.");
        }
        return Ok(());
    }
    // An explicit --version is a deliberate re-install or downgrade; only the
    // implicit "latest" path short-circuits.
    if args.version.is_none() && same_version(current, &latest) {
        println!("Already up to date ({current}).");
        return Ok(());
    }
    if args.binary.is_none() && is_source_build(&target) {
        bail!(
            "{} came out of a source build, not a release archive; update it with `git pull && cargo build --release` (or pass --binary to replace an installed copy)",
            target.display()
        );
    }
    // Version fence: the implicit "latest" path never downgrades and never
    // crosses a major version silently. An explicit --version is deliberate.
    if args.version.is_none() {
        match compare(current, &latest) {
            Ordering::Downgrade => {
                bail!("the latest release ({latest}) is older than this binary ({current}); pass --version {latest} to downgrade on purpose")
            }
            Ordering::Major if !args.allow_major => bail!("{latest} is a new major version; read the release notes, then re-run with --allow-major"),
            Ordering::Unknown => bail!("cannot compare {current} with tag {latest}; pass --version to install it explicitly"),
            _ => {}
        }
    }

    println!("Updating {current} → {latest} …");
    let backup = apply(&repo, &latest, &target)?;
    println!("Updated to {latest} at {}", target.display());
    if args.no_restart {
        println!("Not restarting {UNIT} (--no-restart); the running server keeps the old binary until restarted.");
        return Ok(());
    }
    if !restart_service() {
        return Ok(());
    }
    if args.no_health_check {
        return Ok(());
    }
    match wait_healthy() {
        Ok(v) => println!("Health check passed (server reports {v})."),
        Err(e) => {
            eprintln!("warning: the new binary did not come up healthy: {e}");
            if let Some(b) = backup.filter(|b| b.is_file()) {
                eprintln!("rolling back to the previous binary");
                std::fs::rename(&b, &target).context("restoring the previous binary")?;
                restart_service();
                bail!("update rolled back; {UNIT} is running the previous binary again");
            }
            bail!("no backup to roll back to; inspect: journalctl --user -u {UNIT}");
        }
    }
    Ok(())
}

/// Relationship between the running version and a candidate tag.
#[derive(Debug, PartialEq, Eq)]
pub enum Ordering {
    Same,
    Upgrade,
    Major,
    Downgrade,
    Unknown,
}

pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

pub fn compare(current: &str, candidate: &str) -> Ordering {
    let (Some(c), Some(n)) = (parse_version(current), parse_version(candidate)) else { return Ordering::Unknown };
    if n == c {
        Ordering::Same
    } else if n < c {
        Ordering::Downgrade
    } else if n.0 > c.0 {
        Ordering::Major
    } else {
        Ordering::Upgrade
    }
}

/// Poll the local proxy's health endpoint until it answers with a version.
fn wait_healthy() -> Result<String> {
    let port = crate::config::Config::load().ok().flatten().map(|c| c.proxy.port).unwrap_or(crate::config::DEFAULT_PORT);
    let url = format!("http://127.0.0.1:{port}/corrall/health");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        match std::process::Command::new("curl").args(["-fsS", "--max-time", "2", &url]).stdin(Stdio::null()).output() {
            Ok(o) if o.status.success() => {
                let body = String::from_utf8_lossy(&o.stdout);
                let v = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|j| j.get("version").and_then(|v| v.as_str()).map(str::to_string))
                    .unwrap_or_else(|| "ok".into());
                return Ok(v);
            }
            Ok(o) => last = safe_text(String::from_utf8_lossy(&o.stderr).trim(), 120),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    bail!("no healthy answer from {url} within 20s ({last})")
}

/// The latest release tag, or None when it cannot be determined (no gh, no
/// network, no access). Used by the passive check; never fails loudly.
pub fn latest_tag_quiet() -> Option<String> {
    if std::env::var_os("CORRALL_DISABLE_UPDATE_CHECK").is_some() {
        return None;
    }
    latest_tag(&repo()).ok()
}

/// How long a `status` on-demand check reuses its last on-disk result before
/// shelling out to `gh` again.
const CACHE_TTL_MS: i64 = 60 * 60 * 1000;

/// `latest_tag_quiet` behind a short-lived on-disk cache, for the `status`
/// on-demand check. A fresh cache entry is trusted as-is — including a cached
/// "could not determine" (`None`), so a machine without `gh`/network is not
/// re-probed on every `status`. Stale or missing entries trigger one real
/// check, whose result (tag or not) is written back with the current time.
pub fn latest_tag_cached() -> Option<String> {
    if std::env::var_os("CORRALL_DISABLE_UPDATE_CHECK").is_some() {
        return None;
    }
    let now = crate::quota::now_ms();
    let path = crate::config::update_cache_path();
    if let Some((checked_at, tag)) = read_tag_cache(&path) {
        if now - checked_at < CACHE_TTL_MS {
            return tag;
        }
    }
    let tag = latest_tag_quiet();
    write_tag_cache(&path, now, tag.as_deref());
    tag
}

fn read_tag_cache(path: &Path) -> Option<(i64, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let checked_at = v.get("checkedAt")?.as_i64()?;
    let tag = v.get("tag").and_then(serde_json::Value::as_str).map(str::to_string);
    Some((checked_at, tag))
}

fn write_tag_cache(path: &Path, now: i64, tag: Option<&str>) {
    let body = serde_json::json!({ "checkedAt": now, "tag": tag }).to_string();
    let _ = std::fs::write(path, body);
}

// ── steps ─────────────────────────────────────────────────────

fn latest_tag(repo: &str) -> Result<String> {
    let out = gh(&["release", "view", "-R", repo, "--json", "tagName", "--jq", ".tagName"])?;
    let tag = out.trim().to_string();
    if tag.is_empty() {
        bail!("could not determine the latest release of {repo}");
    }
    Ok(tag)
}

/// Download, verify and swap. Returns the path of the previous binary's
/// backup (`<target>.prev`) so a failed restart can be rolled back.
fn apply(repo: &str, tag: &str, target: &Path) -> Result<Option<PathBuf>> {
    let triple = target_triple(std::env::consts::OS, std::env::consts::ARCH)?;
    let asset = asset_name(tag, triple);
    let work = tempfile::Builder::new().prefix("corrall-update-").tempdir().context("creating a temporary directory")?;
    let dir = work.path();

    println!("Downloading {asset} from {repo} {tag}");
    let dir_arg = dir.to_string_lossy().into_owned();
    gh(&["release", "download", tag, "-R", repo, "-D", &dir_arg, "-p", &asset, "-p", "SHA256SUMS", "-p", "SHA256SUMS.sigstore.json"])?;

    let archive = dir.join(&asset);
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).context("reading SHA256SUMS from the release")?;
    let want = expected_sha256(&sums, &asset).ok_or_else(|| anyhow!("{asset} is not listed in SHA256SUMS"))?;
    let got = sha256_file(&archive)?;
    if !got.eq_ignore_ascii_case(want) {
        bail!("checksum mismatch for {asset}: SHA256SUMS says {want}, the download hashes to {got}");
    }
    verify_signature(repo, dir)?;

    // `tar` is already a prerequisite of the installer, and shelling out to it
    // keeps a decompressor out of the daemon's dependency tree.
    let st = Command::new("tar").arg("-C").arg(dir).arg("-xzf").arg(&archive).stdin(Stdio::null()).status().context("running tar")?;
    if !st.success() {
        bail!("tar could not extract {asset}");
    }
    let new_bin = dir.join(asset.trim_end_matches(".tar.gz")).join("corrall");
    if !new_bin.is_file() {
        bail!("{asset} does not contain a corrall binary");
    }
    // A binary that will not run here is not worth swapping in.
    let v = Command::new(&new_bin).arg("--version").stdin(Stdio::null()).output().context("running the downloaded binary")?;
    if !v.status.success() {
        bail!("the downloaded binary does not run on this machine");
    }
    println!("New binary: {}", safe_text(String::from_utf8_lossy(&v.stdout).trim(), 80));

    let backup = target.with_extension("prev");
    let kept = if target.is_file() { std::fs::copy(target, &backup).map(|_| backup).ok() } else { None };
    replace(&new_bin, target)?;
    Ok(kept)
}

/// A present-but-invalid signature is a failure; an absent verifier is not.
/// Provenance attestations are only offered to private repositories on some
/// plans, so the Sigstore bundle is the guarantee this checks.
fn verify_signature(repo: &str, dir: &Path) -> Result<()> {
    let bundle = dir.join("SHA256SUMS.sigstore.json");
    if !bundle.is_file() {
        eprintln!("warning: this release has no Sigstore bundle; relying on the verified checksum");
        return Ok(());
    }
    let out = Command::new("cosign")
        .arg("verify-blob")
        .arg("--bundle")
        .arg(&bundle)
        .arg("--certificate-identity-regexp")
        .arg(format!("github.com/{repo}/"))
        .arg("--certificate-oidc-issuer")
        .arg("https://token.actions.githubusercontent.com")
        .arg(dir.join("SHA256SUMS"))
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            println!("Sigstore signature verified");
            Ok(())
        }
        Ok(o) => bail!("Sigstore verification failed: {}", safe_text(String::from_utf8_lossy(&o.stderr).trim(), 300)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("warning: cosign is not installed; skipping signature verification (checksum verified)");
            Ok(())
        }
        Err(e) => Err(e).context("running cosign"),
    }
}

fn replace(new_bin: &Path, target: &Path) -> Result<()> {
    let dir = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".corrall-update-{}", crate::security::random_key(6)));
    std::fs::copy(new_bin, &tmp).map_err(|e| write_err(dir, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| write_err(dir, e))?;
    }
    // A rename within the destination directory is atomic, and it only unlinks
    // the old inode: a server still running the previous binary is unaffected
    // and keeps serving until it is restarted.
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(write_err(dir, e));
    }
    Ok(())
}

fn write_err(dir: &Path, e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow!("cannot write to {}: {e}; re-run with sudo, or point --binary at a copy you own", dir.display())
    } else {
        anyhow!("writing to {}: {e}", dir.display())
    }
}

/// Restart the unit if it is running. Returns whether a restart happened.
fn restart_service() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let active = Command::new("systemctl").args(["--user", "is-active", "--quiet", UNIT]).status().map(|s| s.success()).unwrap_or(false);
    if !active {
        println!("{UNIT} is not running; nothing to restart.");
        return false;
    }
    match Command::new("systemctl").args(["--user", "restart", UNIT]).status() {
        Ok(s) if s.success() => {
            println!("Restarted {UNIT}.");
            true
        }
        _ => {
            eprintln!("note: could not restart {UNIT} automatically; run: systemctl --user restart {UNIT}");
            false
        }
    }
}

fn gh(args: &[&str]) -> Result<String> {
    let out = Command::new("gh").args(args).stdin(Stdio::null()).output().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow!(GH_HINT),
        _ => anyhow!("running gh: {e}"),
    })?;
    if !out.status.success() {
        let err = safe_text(String::from_utf8_lossy(&out.stderr).trim(), 400);
        bail!("gh {} failed: {}", args.join(" "), if err.is_empty() { "no output".to_string() } else { err });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── pure helpers ──────────────────────────────────────────────

/// Releases ship musl on Linux, so a glibc-built local binary still updates to
/// the portable build. Anything else has no published asset to fetch.
pub fn target_triple(os: &str, arch: &str) -> Result<&'static str> {
    Ok(match (os, arch) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        _ => bail!("no release is published for {os}/{arch}; build from source instead"),
    })
}

/// Must match the `Package` step of `.github/workflows/release.yml`.
pub fn asset_name(tag: &str, triple: &str) -> String {
    format!("corrall-{tag}-{triple}.tar.gz")
}

/// Tags carry a leading `v`, `CARGO_PKG_VERSION` does not.
pub fn same_version(a: &str, b: &str) -> bool {
    let strip = |s: &str| s.trim().trim_start_matches('v').to_string();
    !a.trim().is_empty() && strip(a) == strip(b)
}

/// `SHA256SUMS` is `shasum -a 256` output: `<hex>  <name>`, with a `*` marking
/// binary mode on some platforms.
pub fn expected_sha256<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == asset && hash.len() == 64).then_some(hash)
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A binary sitting in `target/debug` or `target/release` came from `cargo
/// build` in a checkout. Dropping a release tarball on top of it would throw
/// away the working tree's build and confuse the next `cargo run`.
fn in_cargo_target(exe: &Path) -> bool {
    let Some(profile) = exe.parent() else { return false };
    let named = |p: &Path, want: &str| p.file_name().is_some_and(|n| n == want);
    (named(profile, "debug") || named(profile, "release")) && profile.parent().is_some_and(|p| named(p, "target"))
}

fn is_source_build(exe: &Path) -> bool {
    cfg!(debug_assertions) || in_cargo_target(exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_match_the_release_workflow() {
        assert_eq!(asset_name("v2.1.0", "x86_64-unknown-linux-musl"), "corrall-v2.1.0-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(target_triple("linux", "x86_64").unwrap(), "x86_64-unknown-linux-musl");
        assert_eq!(target_triple("macos", "aarch64").unwrap(), "aarch64-apple-darwin");
        assert!(target_triple("windows", "x86_64").is_err());
        assert!(target_triple("linux", "riscv64").is_err());
    }

    #[test]
    fn tag_cache_round_trips_and_expires() {
        let path = std::env::temp_dir().join(format!("tc-update-cache-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // Missing file → nothing to read.
        assert_eq!(read_tag_cache(&path), None);
        // A recorded tag comes back with its timestamp.
        write_tag_cache(&path, 1_000, Some("v9.9.9"));
        assert_eq!(read_tag_cache(&path), Some((1_000, Some("v9.9.9".to_string()))));
        // A recorded "no tag" is a real cache entry, distinct from a miss.
        write_tag_cache(&path, 2_000, None);
        assert_eq!(read_tag_cache(&path), Some((2_000, None)));
        // Freshness is the caller's TTL window against checkedAt.
        let now = 2_000 + CACHE_TTL_MS;
        assert!(now - 2_000 >= CACHE_TTL_MS, "entry at the TTL edge counts as stale");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn version_fence() {
        assert_eq!(compare("2.0.1", "v2.0.2"), Ordering::Upgrade);
        assert_eq!(compare("2.0.1", "v2.1.0"), Ordering::Upgrade);
        assert_eq!(compare("2.0.1", "v2.0.1"), Ordering::Same);
        assert_eq!(compare("2.0.1", "v2.0.0"), Ordering::Downgrade);
        assert_eq!(compare("2.0.1", "v3.0.0"), Ordering::Major);
        assert_eq!(compare("2.0.1", "nightly"), Ordering::Unknown);
        assert_eq!(parse_version("v2.1.0-rc.1"), Some((2, 1, 0)));
    }

    #[test]
    fn a_tag_and_a_crate_version_compare_equal() {
        assert!(same_version("2.0.0", "v2.0.0"));
        assert!(same_version("v2.0.0", "2.0.0 "));
        assert!(!same_version("2.0.0", "v2.1.0"));
        assert!(!same_version("", "v2.0.0"));
    }

    #[test]
    fn the_checksum_line_is_found_by_exact_asset_name() {
        let h = "a".repeat(64);
        let other = "b".repeat(64);
        let sums = format!(
            "{h}  corrall-v2.1.0-x86_64-unknown-linux-musl.tar.gz\n\
             {other} *corrall-v2.1.0-aarch64-apple-darwin.tar.gz\n"
        );
        assert_eq!(expected_sha256(&sums, "corrall-v2.1.0-x86_64-unknown-linux-musl.tar.gz"), Some(h.as_str()));
        assert_eq!(expected_sha256(&sums, "corrall-v2.1.0-aarch64-apple-darwin.tar.gz"), Some(other.as_str()));
        // A suffix must not satisfy a different asset.
        assert_eq!(expected_sha256(&sums, "unknown-linux-musl.tar.gz"), None);
        assert_eq!(expected_sha256(&sums, "corrall-v2.2.0-x86_64-unknown-linux-musl.tar.gz"), None);
        // A truncated hash is not a hash.
        assert_eq!(expected_sha256("abc  corrall-v1.tar.gz", "corrall-v1.tar.gz"), None);
    }

    #[test]
    fn a_file_hashes_to_its_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, b"corrall").unwrap();
        // printf corrall | sha256sum
        assert_eq!(sha256_file(&p).unwrap(), "9332178735a64a58e3b50e997960586b5b9fecf121685cd3f2304a2df31fabcf");
        // Larger than one read buffer, to exercise the loop.
        let big = dir.path().join("b.bin");
        std::fs::write(&big, vec![7u8; 200 * 1024]).unwrap();
        assert_eq!(sha256_file(&big).unwrap().len(), 64);
    }

    #[test]
    fn a_cargo_built_binary_is_recognised() {
        assert!(in_cargo_target(Path::new("/home/u/src/corrall/target/release/corrall")));
        assert!(in_cargo_target(Path::new("/home/u/src/corrall/target/debug/corrall")));
        assert!(!in_cargo_target(Path::new("/home/u/.local/bin/corrall")));
        assert!(!in_cargo_target(Path::new("/usr/local/bin/corrall")));
        assert!(!in_cargo_target(Path::new("corrall")));
    }

    #[test]
    fn the_binary_is_swapped_by_rename() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("corrall"), dir.path().join("new"));
        std::fs::write(&old, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        replace(&new, &old).unwrap();
        assert_eq!(std::fs::read(&old).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&old).unwrap().permissions().mode() & 0o777, 0o755);
        }
        // No leftovers beside the target.
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 2, "unexpected leftovers: {left:?}");
    }
}
