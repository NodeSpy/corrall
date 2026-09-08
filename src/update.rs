//! Updating an installed release in place.
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

const UNIT: &str = "teamclaude.service";
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
}

/// The release this binary was built from.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `owner/repo` to update from. Overridable so a fork — or a test — can point
/// the updater somewhere else, the same knob `scripts/install.sh` offers.
pub fn repo() -> String {
    std::env::var("TEAMCLAUDE_REPO").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "NodeSpy/teamclaude".to_string())
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
            println!("Update available: {current} → {latest}\nRun `teamclaude update` to install it.");
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

    println!("Updating {current} → {latest} …");
    apply(&repo, &latest, &target)?;
    println!("Updated to {latest} at {}", target.display());
    restart_service();
    Ok(())
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

fn apply(repo: &str, tag: &str, target: &Path) -> Result<()> {
    let triple = target_triple(std::env::consts::OS, std::env::consts::ARCH)?;
    let asset = asset_name(tag, triple);
    let work = tempfile::Builder::new().prefix("teamclaude-update-").tempdir().context("creating a temporary directory")?;
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
    let new_bin = dir.join(asset.trim_end_matches(".tar.gz")).join("teamclaude");
    if !new_bin.is_file() {
        bail!("{asset} does not contain a teamclaude binary");
    }
    // A binary that will not run here is not worth swapping in.
    let v = Command::new(&new_bin).arg("--version").stdin(Stdio::null()).output().context("running the downloaded binary")?;
    if !v.status.success() {
        bail!("the downloaded binary does not run on this machine");
    }
    println!("New binary: {}", safe_text(String::from_utf8_lossy(&v.stdout).trim(), 80));

    replace(&new_bin, target)
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
    let tmp = dir.join(format!(".teamclaude-update-{}", crate::security::random_key(6)));
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

fn restart_service() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let active = Command::new("systemctl").args(["--user", "is-active", "--quiet", UNIT]).status().map(|s| s.success()).unwrap_or(false);
    if !active {
        return;
    }
    match Command::new("systemctl").args(["--user", "restart", UNIT]).status() {
        Ok(s) if s.success() => println!("Restarted {UNIT}."),
        _ => eprintln!("note: could not restart {UNIT} automatically; run: systemctl --user restart {UNIT}"),
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
    format!("teamclaude-{tag}-{triple}.tar.gz")
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
        assert_eq!(asset_name("v2.1.0", "x86_64-unknown-linux-musl"), "teamclaude-v2.1.0-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(target_triple("linux", "x86_64").unwrap(), "x86_64-unknown-linux-musl");
        assert_eq!(target_triple("macos", "aarch64").unwrap(), "aarch64-apple-darwin");
        assert!(target_triple("windows", "x86_64").is_err());
        assert!(target_triple("linux", "riscv64").is_err());
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
            "{h}  teamclaude-v2.1.0-x86_64-unknown-linux-musl.tar.gz\n\
             {other} *teamclaude-v2.1.0-aarch64-apple-darwin.tar.gz\n"
        );
        assert_eq!(expected_sha256(&sums, "teamclaude-v2.1.0-x86_64-unknown-linux-musl.tar.gz"), Some(h.as_str()));
        assert_eq!(expected_sha256(&sums, "teamclaude-v2.1.0-aarch64-apple-darwin.tar.gz"), Some(other.as_str()));
        // A suffix must not satisfy a different asset.
        assert_eq!(expected_sha256(&sums, "unknown-linux-musl.tar.gz"), None);
        assert_eq!(expected_sha256(&sums, "teamclaude-v2.2.0-x86_64-unknown-linux-musl.tar.gz"), None);
        // A truncated hash is not a hash.
        assert_eq!(expected_sha256("abc  teamclaude-v1.tar.gz", "teamclaude-v1.tar.gz"), None);
    }

    #[test]
    fn a_file_hashes_to_its_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, b"teamclaude").unwrap();
        // printf teamclaude | sha256sum
        assert_eq!(sha256_file(&p).unwrap(), "2539f24a38fcd8c6ed1d89e908d35b16411772fc36e4cab62b02b555d92b1a95");
        // Larger than one read buffer, to exercise the loop.
        let big = dir.path().join("b.bin");
        std::fs::write(&big, vec![7u8; 200 * 1024]).unwrap();
        assert_eq!(sha256_file(&big).unwrap().len(), 64);
    }

    #[test]
    fn a_cargo_built_binary_is_recognised() {
        assert!(in_cargo_target(Path::new("/home/u/src/teamclaude/target/release/teamclaude")));
        assert!(in_cargo_target(Path::new("/home/u/src/teamclaude/target/debug/teamclaude")));
        assert!(!in_cargo_target(Path::new("/home/u/.local/bin/teamclaude")));
        assert!(!in_cargo_target(Path::new("/usr/local/bin/teamclaude")));
        assert!(!in_cargo_target(Path::new("teamclaude")));
    }

    #[test]
    fn the_binary_is_swapped_by_rename() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("teamclaude"), dir.path().join("new"));
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
