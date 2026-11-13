//! `corrall gnome`: install the GNOME Shell panel indicator that ships in
//! `gnome/` (see `docs/gnome-extension.md`). The extension's files are built
//! into the binary, so a release needs nothing beside it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::cli::GnomeCmd;

pub const UUID: &str = "corrall@nodespy.github.io";

/// The extension, as paths under its directory and their contents.
const FILES: [(&str, &str); 4] = [
    ("extension.js", include_str!("../gnome/corrall@nodespy.github.io/extension.js")),
    ("metadata.json", include_str!("../gnome/corrall@nodespy.github.io/metadata.json")),
    ("stylesheet.css", include_str!("../gnome/corrall@nodespy.github.io/stylesheet.css")),
    ("icons/corrall-symbolic.svg", include_str!("../gnome/corrall@nodespy.github.io/icons/corrall-symbolic.svg")),
];

pub fn run(cmd: GnomeCmd) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("the panel indicator is a GNOME Shell extension; there is no GNOME here");
    }
    let dir = extensions_dir().join(UUID);
    match cmd {
        GnomeCmd::Install => {
            install_into(&dir)?;
            eprintln!("installed {}", dir.display());
            eprintln!("{}", enable());
            Ok(())
        }
        GnomeCmd::Uninstall => {
            let _ = Command::new("gnome-extensions").args(["disable", UUID]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
            if dir.is_symlink() {
                bail!("{} is a symlink (a development checkout); remove it yourself", dir.display());
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => eprintln!("removed {}", dir.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => eprintln!("{} was not installed", dir.display()),
                Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
            }
            Ok(())
        }
    }
}

/// Where GNOME Shell looks for a user's extensions.
fn extensions_dir() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).or_else(|| dirs::home_dir().map(|h| h.join(".local/share")));
    data.unwrap_or_else(|| PathBuf::from(".")).join("gnome-shell/extensions")
}

/// Write the extension into `dir`, replacing what was there in one rename so
/// the shell never loads half of it. A symlink there is a checkout someone is
/// developing in, and is left alone.
fn install_into(dir: &Path) -> Result<()> {
    if dir.is_symlink() {
        bail!("{} is a symlink (a development checkout); not overwriting it", dir.display());
    }
    let parent = dir.parent().context("the extensions directory has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let staging = parent.join(format!(".{UUID}.tmp"));
    let _ = std::fs::remove_dir_all(&staging);
    for (rel, contents) in FILES {
        let path = staging.join(rel);
        std::fs::create_dir_all(path.parent().expect("a file path has a parent")).with_context(|| format!("creating {}", path.display()))?;
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&staging, dir).with_context(|| format!("moving the extension into {}", dir.display()))
}

/// Turn the extension on, and say what that took. A running shell only knows
/// the extensions it found at login (always so on Wayland), so when it has
/// not seen this one yet the setting is written for the next login instead.
fn enable() -> String {
    let tool = Command::new("gnome-extensions").args(["enable", UUID]).stdin(Stdio::null()).output();
    if tool.as_ref().is_ok_and(|o| o.status.success()) {
        return "enabled: the indicator is in the top bar".into();
    }
    let current = match Command::new("gsettings").args(["get", "org.gnome.shell", "enabled-extensions"]).stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => {
            return format!("could not enable it from here (no gnome-extensions or gsettings); turn on \"corrall\" in the Extensions app, or run: gnome-extensions enable {UUID}")
        }
    };
    match enabled_list_with(&current, UUID) {
        None => "already enabled; it appears at your next login".into(),
        Some(list) => match Command::new("gsettings").args(["set", "org.gnome.shell", "enabled-extensions", &list]).stdin(Stdio::null()).status() {
            Ok(s) if s.success() => {
                "enabled for your next login: log out and back in (the running shell only loads extensions it found when it started)".into()
            }
            _ => format!("could not enable it from here; after your next login run: gnome-extensions enable {UUID}"),
        },
    }
}

/// `gsettings get … enabled-extensions` prints a list like `['a@b', 'c@d']`
/// (or `@as []`). The same list with `uuid` added, or `None` when it is
/// already there.
fn enabled_list_with(current: &str, uuid: &str) -> Option<String> {
    let mut items: Vec<&str> = current.split('\'').skip(1).step_by(2).collect();
    if items.contains(&uuid) {
        return None;
    }
    items.push(uuid);
    Some(format!("[{}]", items.iter().map(|i| format!("'{i}'")).collect::<Vec<_>>().join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_is_written_whole_and_replaces_an_older_one() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("gnome-shell/extensions").join(UUID);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stale.js"), "old").unwrap();
        install_into(&dir).unwrap();
        for (rel, contents) in FILES {
            assert_eq!(std::fs::read_to_string(dir.join(rel)).unwrap(), contents, "{rel}");
        }
        assert!(!dir.join("stale.js").exists(), "nothing of the old version is left");
        assert!(!dir.parent().unwrap().join(format!(".{UUID}.tmp")).exists());
        // What the shell reads to recognise it.
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(meta["uuid"], UUID, "the directory name and the uuid must agree or the shell ignores it");
        assert!(meta["shell-version"].as_array().is_some_and(|v| !v.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn a_development_symlink_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let checkout = home.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("extension.js"), "// mine").unwrap();
        let dir = home.path().join(UUID);
        std::os::unix::fs::symlink(&checkout, &dir).unwrap();
        assert!(install_into(&dir).is_err());
        assert_eq!(std::fs::read_to_string(checkout.join("extension.js")).unwrap(), "// mine");
    }

    #[test]
    fn the_enabled_list_gains_the_extension_once() {
        assert_eq!(enabled_list_with("@as []\n", UUID).as_deref(), Some("['corrall@nodespy.github.io']"));
        assert_eq!(enabled_list_with("['a@b', 'c@d']\n", UUID).as_deref(), Some("['a@b', 'c@d', 'corrall@nodespy.github.io']"));
        assert_eq!(enabled_list_with("['a@b', 'corrall@nodespy.github.io']", UUID), None);
    }
}
