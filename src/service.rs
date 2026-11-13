//! `corrall service`: run the proxy as a per-user service that starts at
//! login and is restarted if it dies: a `systemd --user` unit on Linux, a
//! launchd agent on macOS.
//!
//! Everything that decides what gets written or run is a plain function of
//! its inputs, so the macOS half is exercised by the tests on any platform;
//! only the `launchctl`/`systemctl` calls themselves need the real thing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::cli::ServiceCmd;
use crate::security::safe_text;

/// The systemd unit's name.
pub const UNIT: &str = "corrall.service";
/// The launchd job's label, and its plist's file name.
pub const LABEL: &str = "com.nodespy.corrall";

/// The service manager this platform has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Systemd,
    Launchd,
}

impl Kind {
    pub fn native() -> Option<Kind> {
        if cfg!(target_os = "linux") {
            Some(Kind::Systemd)
        } else if cfg!(target_os = "macos") {
            Some(Kind::Launchd)
        } else {
            None
        }
    }

    /// What the service is called here, for messages.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Systemd => UNIT,
            Kind::Launchd => LABEL,
        }
    }

    /// The command that shows the service's log.
    pub fn logs_hint(self) -> String {
        match self {
            Kind::Systemd => format!("journalctl --user -u {UNIT}"),
            Kind::Launchd => format!("tail -f {}", log_path().display()),
        }
    }

    /// The command that restarts it by hand.
    pub fn restart_hint(self) -> String {
        match self {
            Kind::Systemd => format!("systemctl --user restart {UNIT}"),
            Kind::Launchd => format!("launchctl kickstart -k {}", launchd_target(uid())),
        }
    }
}

/// Where the service stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No service manager this command knows.
    Unsupported,
    /// Never installed.
    Missing,
    /// Its file is there but the manager is not running it.
    Stopped,
    /// The manager has it: running, or being restarted.
    Running,
}

pub fn state() -> State {
    match Kind::native() {
        None => State::Unsupported,
        Some(Kind::Systemd) if quiet("systemctl", &["--user", "is-active", "--quiet", UNIT]) => State::Running,
        Some(Kind::Launchd) if quiet("launchctl", &["print", &launchd_target(uid())]) => State::Running,
        Some(kind) if file_path(kind).is_file() => State::Stopped,
        Some(_) => State::Missing,
    }
}

/// What `corrall setup claude` should do so that the proxy it just pointed
/// Claude Code at is there whenever Claude Code starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ensure {
    /// The service is running and the proxy answers: nothing to do.
    Fine,
    /// The service is running but the proxy is silent: it may still be
    /// starting, or it is failing and being restarted.
    Wait,
    /// No service and no proxy: install the service and start it.
    Install,
    /// A proxy answers, but nothing keeps it running: someone started it by
    /// hand. Installing now would only fight it for the port.
    ByHand,
    /// No service manager here to ask.
    Unsupported,
}

pub fn ensure_plan(state: State, answering: bool) -> Ensure {
    match (state, answering) {
        (State::Unsupported, _) => Ensure::Unsupported,
        (State::Running, true) => Ensure::Fine,
        (State::Running, false) => Ensure::Wait,
        (State::Missing | State::Stopped, true) => Ensure::ByHand,
        (State::Missing | State::Stopped, false) => Ensure::Install,
    }
}

/// What a restart attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// Nothing is running the service, so there was nothing to restart.
    NotRunning,
    Restarted,
    Failed,
}

/// Restart the service if its manager is running it.
pub fn restart() -> Restart {
    let Some(kind) = Kind::native() else { return Restart::NotRunning };
    if state() != State::Running {
        return Restart::NotRunning;
    }
    let ok = match kind {
        Kind::Systemd => quiet("systemctl", &["--user", "restart", UNIT]),
        // `-k` stops the running instance first; launchd then starts the job
        // again from the plist, which names the binary by path.
        Kind::Launchd => quiet("launchctl", &["kickstart", "-k", &launchd_target(uid())]),
    };
    if ok {
        Restart::Restarted
    } else {
        Restart::Failed
    }
}

pub fn run(cmd: ServiceCmd) -> Result<()> {
    let Some(kind) = Kind::native() else {
        bail!("service management covers systemd --user on Linux and launchd on macOS");
    };
    match cmd {
        ServiceCmd::Print => {
            print!("{}", definition(kind)?);
            Ok(())
        }
        ServiceCmd::Install => {
            let path = install()?;
            eprintln!("installed {}", path.display());
            if kind == Kind::Launchd {
                eprintln!("log: {}", log_path().display());
            }
            Ok(())
        }
        ServiceCmd::Uninstall => {
            let path = file_path(kind);
            match kind {
                Kind::Systemd => {
                    quiet("systemctl", &["--user", "disable", "--now", UNIT]);
                    let _ = std::fs::remove_file(&path);
                    quiet("systemctl", &["--user", "daemon-reload"]);
                }
                Kind::Launchd => {
                    quiet("launchctl", &["bootout", &launchd_target(uid())]);
                    let _ = std::fs::remove_file(&path);
                }
            }
            eprintln!("removed {}", path.display());
            Ok(())
        }
        ServiceCmd::Status => {
            let st = match kind {
                Kind::Systemd => Command::new("systemctl").args(["--user", "status", UNIT, "--no-pager"]).status()?,
                Kind::Launchd => {
                    let st = Command::new("launchctl").args(["print", &launchd_target(uid())]).status()?;
                    if !st.success() {
                        let why = if file_path(kind).is_file() { "installed but not loaded" } else { "not installed" };
                        eprintln!("{LABEL} is {why}; `corrall service install` starts it");
                    }
                    eprintln!("log: {}", log_path().display());
                    st
                }
            };
            std::process::exit(st.code().unwrap_or(1));
        }
    }
}

/// Write the service's file for this binary and config, and start it. Safe to
/// repeat: an installed service is rewritten and restarted. Returns the file.
pub fn install() -> Result<PathBuf> {
    let Some(kind) = Kind::native() else {
        bail!("service management covers systemd --user on Linux and launchd on macOS");
    };
    let path = file_path(kind);
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, definition(kind)?).with_context(|| format!("writing {}", path.display()))?;
    match kind {
        Kind::Systemd => {
            quiet("systemctl", &["--user", "daemon-reload"]);
            // Captured, not inherited: the dashboard calls this with the
            // terminal in its hands.
            let out = Command::new("systemctl").args(["--user", "enable", "--now", UNIT]).stdin(Stdio::null()).output().context("running systemctl")?;
            if !out.status.success() {
                bail!("systemctl --user enable --now {UNIT} failed: {}", safe_text(String::from_utf8_lossy(&out.stderr).trim(), 300));
            }
        }
        Kind::Launchd => {
            // The log exists before launchd opens it, so it is private from
            // the first line whatever mode launchd would have created it with.
            let log = log_path();
            if let Some(dir) = log.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if std::fs::OpenOptions::new().create(true).append(true).open(&log).is_ok() {
                crate::security::set_mode(&log, 0o600);
            }
            launchd_load(&path, uid(), &mut launchctl, &std::thread::sleep)?;
        }
    }
    Ok(path)
}

/// The unit or plist text for this binary and the config it is using.
fn definition(kind: Kind) -> Result<String> {
    let exe = std::env::current_exe().context("locating this binary")?;
    let config = crate::config::config_path();
    Ok(match kind {
        Kind::Systemd => systemd_unit(&exe, &config, &crate::config::config_dir()),
        Kind::Launchd => launchd_plist(&exe, &config, &log_path()),
    })
}

fn file_path(kind: Kind) -> PathBuf {
    match kind {
        Kind::Systemd => dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("systemd/user").join(UNIT),
        Kind::Launchd => home().join("Library/LaunchAgents").join(format!("{LABEL}.plist")),
    }
}

/// Where the launchd agent's output goes. There is no journal on macOS, so
/// this file is the service's log.
pub fn log_path() -> PathBuf {
    home().join("Library/Logs/corrall.log")
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn uid() -> u32 {
    #[cfg(unix)]
    // SAFETY: getuid takes no arguments, cannot fail and touches no memory.
    return unsafe { libc::getuid() };
    #[cfg(not(unix))]
    return 0;
}

pub fn systemd_unit(exe: &Path, config: &Path, config_dir: &Path) -> String {
    format!(
        "[Unit]\nDescription=Corrall multi-account Claude proxy\nAfter=network-online.target\n\n[Service]\nExecStart={} server --headless\nRestart=on-failure\nRestartSec=3\nEnvironment=CORRALL_CONFIG={}\nNoNewPrivileges=yes\nPrivateTmp=yes\nProtectSystem=strict\nReadWritePaths={}\n\n[Install]\nWantedBy=default.target\n",
        exe.display(),
        config.display(),
        config_dir.display()
    )
}

/// The user's launchd domain: agents loaded here run while they are logged in.
fn launchd_domain(uid: u32) -> String {
    format!("gui/{uid}")
}

fn launchd_target(uid: u32) -> String {
    format!("{}/{LABEL}", launchd_domain(uid))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The launchd agent: started at login, restarted whenever it exits, its
/// output appended to `log`.
///
/// launchd hands a job a bare `PATH`, so the Homebrew directories are added
/// for the `gh` the release check runs. `ProcessType` is `Interactive` because
/// every Claude Code request waits on this process; the `Background` class is
/// throttled. `Umask` 63 is octal 077: the log names sessions.
pub fn launchd_plist(exe: &Path, config: &Path, log: &Path) -> String {
    let (exe, config, log) = (xml(&exe.display().to_string()), xml(&config.display().to_string()), xml(&log.display().to_string()));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>server</string>
		<string>--headless</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
		<key>CORRALL_CONFIG</key>
		<string>{config}</string>
		<key>PATH</key>
		<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>Umask</key>
	<integer>63</integer>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    )
}

/// One `launchctl` call: arguments in; whether it succeeded, and what it
/// said on stderr, out.
type Launchctl<'a> = &'a mut dyn FnMut(&[&str]) -> (bool, String);

fn launchctl(args: &[&str]) -> (bool, String) {
    match Command::new("launchctl").args(args).stdin(Stdio::null()).output() {
        Ok(o) => (o.status.success(), safe_text(String::from_utf8_lossy(&o.stderr).trim(), 300)),
        Err(e) => (false, format!("could not run launchctl: {e}")),
    }
}

/// (Re)load the agent from `plist` and start it.
fn launchd_load(plist: &Path, uid: u32, launchctl: Launchctl, pause: &dyn Fn(Duration)) -> Result<()> {
    let (domain, target) = (launchd_domain(uid), launchd_target(uid));
    // An agent already loaded keeps the plist it was loaded with: take it out
    // first. `bootout` returns before the job is gone, hence the wait.
    if launchctl(&["print", &target]).0 {
        launchctl(&["bootout", &target]);
        for _ in 0..40 {
            if !launchctl(&["print", &target]).0 {
                break;
            }
            pause(Duration::from_millis(250));
        }
    }
    // A job the user once disabled refuses to bootstrap until re-enabled.
    launchctl(&["enable", &target]);
    let plist = plist.display().to_string();
    let mut last = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            pause(Duration::from_millis(500));
        }
        let (ok, said) = launchctl(&["bootstrap", &domain, &plist]);
        if ok {
            // `RunAtLoad` starts it; this covers a job that was loaded but idle.
            launchctl(&["kickstart", &target]);
            return Ok(());
        }
        last = said;
    }
    Err(anyhow!(
        "launchctl bootstrap {domain} failed: {last}\nA launchd agent needs this user's desktop session: log in at the Mac itself (an SSH-only login has no {domain} domain), then run `corrall service install` again"
    ))
}

/// Run a command for its exit status alone.
fn quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_the_headless_server_with_this_config() {
        let plist =
            launchd_plist(Path::new("/Users/a/.local/bin/corrall"), Path::new("/Users/a/.config/corrall.json"), Path::new("/Users/a/Library/Logs/corrall.log"));
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist "));
        assert!(plist.trim_end().ends_with("</plist>"));
        // Whitespace aside, the program line is exactly the systemd ExecStart.
        let flat: String = plist.split_whitespace().collect();
        assert!(
            flat.contains(
                "<key>ProgramArguments</key><array><string>/Users/a/.local/bin/corrall</string><string>server</string><string>--headless</string></array>"
            ),
            "{plist}"
        );
        assert!(flat.contains("<key>Label</key><string>com.nodespy.corrall</string>"));
        assert!(flat.contains("<key>CORRALL_CONFIG</key><string>/Users/a/.config/corrall.json</string>"));
        assert!(flat.contains("<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>"), "starts at login and is kept alive");
        assert!(flat.contains("<key>StandardErrorPath</key><string>/Users/a/Library/Logs/corrall.log</string>"), "the log has somewhere to go");
        assert!(flat.contains("<key>Umask</key><integer>63</integer>"));
        // Every opened element is closed.
        for tag in ["dict", "array", "string", "key", "plist"] {
            assert_eq!(plist.matches(&format!("<{tag}")).count(), plist.matches(&format!("</{tag}>")).count(), "{tag}");
        }
    }

    #[test]
    fn setup_installs_the_service_only_when_nothing_is_serving() {
        assert_eq!(ensure_plan(State::Running, true), Ensure::Fine);
        assert_eq!(ensure_plan(State::Running, false), Ensure::Wait, "starting, or crash-looping: not a reason to reinstall");
        for absent in [State::Missing, State::Stopped] {
            assert_eq!(ensure_plan(absent, false), Ensure::Install);
            assert_eq!(ensure_plan(absent, true), Ensure::ByHand, "a hand-started server holds the port");
        }
        assert_eq!(ensure_plan(State::Unsupported, false), Ensure::Unsupported);
    }

    /// A stand-in launchd: one job, which takes `slow` more `print`s to leave
    /// after a `bootout` and refuses to bootstrap while it is still there.
    struct FakeLaunchd {
        loaded: bool,
        leaving: u32,
        slow: u32,
        calls: Vec<String>,
    }

    impl FakeLaunchd {
        fn call(&mut self, args: &[&str]) -> (bool, String) {
            self.calls.push(args.join(" "));
            match args[0] {
                "print" => {
                    if self.loaded && self.leaving > 0 {
                        self.leaving -= 1;
                        self.loaded = self.leaving > 0;
                    }
                    (self.loaded, String::new())
                }
                "bootout" => {
                    self.leaving = self.slow;
                    self.loaded = self.slow > 0;
                    (true, String::new())
                }
                "bootstrap" if self.loaded => (false, "Bootstrap failed: 5: Input/output error".into()),
                "bootstrap" => {
                    self.loaded = true;
                    (true, String::new())
                }
                _ => (true, String::new()),
            }
        }
    }

    #[test]
    fn a_first_install_enables_bootstraps_and_starts() {
        let mut fake = FakeLaunchd { loaded: false, leaving: 0, slow: 0, calls: vec![] };
        launchd_load(Path::new("/Users/a/Library/LaunchAgents/com.nodespy.corrall.plist"), 501, &mut |a| fake.call(a), &|_| {}).unwrap();
        assert_eq!(
            fake.calls,
            [
                "print gui/501/com.nodespy.corrall",
                "enable gui/501/com.nodespy.corrall",
                "bootstrap gui/501 /Users/a/Library/LaunchAgents/com.nodespy.corrall.plist",
                "kickstart gui/501/com.nodespy.corrall",
            ]
        );
    }

    #[test]
    fn a_reinstall_waits_for_the_old_job_to_leave() {
        let mut fake = FakeLaunchd { loaded: true, leaving: 0, slow: 3, calls: vec![] };
        let pauses = std::cell::Cell::new(0);
        launchd_load(Path::new("/p.plist"), 501, &mut |a| fake.call(a), &|_| pauses.set(pauses.get() + 1)).unwrap();
        let verbs: Vec<&str> = fake.calls.iter().map(|c| c.split(' ').next().unwrap()).collect();
        assert_eq!(verbs, ["print", "bootout", "print", "print", "print", "enable", "bootstrap", "kickstart"]);
        assert_eq!(pauses.get(), 2, "it waited, rather than bootstrapping into a job still shutting down");
        assert!(fake.loaded);
    }

    #[test]
    fn a_bootstrap_that_keeps_failing_says_what_launchd_said() {
        // Never leaves: every bootstrap is refused.
        let mut fake = FakeLaunchd { loaded: true, leaving: 0, slow: u32::MAX, calls: vec![] };
        let err = launchd_load(Path::new("/p.plist"), 501, &mut |a| fake.call(a), &|_| {}).unwrap_err().to_string();
        assert!(err.contains("launchctl bootstrap gui/501 failed: Bootstrap failed: 5"), "{err}");
        assert!(err.contains("corrall service install"), "{err}");
        assert_eq!(fake.calls.iter().filter(|c| c.starts_with("bootstrap")).count(), 5, "retried, then gave up");
        assert!(!fake.calls.iter().any(|c| c.starts_with("kickstart")));
    }

    #[test]
    fn plist_escapes_paths() {
        let plist = launchd_plist(Path::new("/Users/a & b/<bin>/corrall"), Path::new("/c"), Path::new("/l"));
        assert!(plist.contains("<string>/Users/a &amp; b/&lt;bin&gt;/corrall</string>"), "{plist}");
        assert!(!plist.contains("a & b"));
    }

    #[test]
    fn launchd_names_the_job_in_the_users_gui_domain() {
        assert_eq!(launchd_domain(501), "gui/501");
        assert_eq!(launchd_target(501), "gui/501/com.nodespy.corrall");
        assert!(file_path(Kind::Launchd).ends_with("Library/LaunchAgents/com.nodespy.corrall.plist"));
        assert!(log_path().ends_with("Library/Logs/corrall.log"));
        assert!(Kind::Launchd.restart_hint().starts_with("launchctl kickstart -k gui/"));
        assert!(Kind::Launchd.logs_hint().ends_with("Library/Logs/corrall.log"));
    }

    #[test]
    fn systemd_unit_is_unchanged() {
        let unit = systemd_unit(Path::new("/home/a/.local/bin/corrall"), Path::new("/home/a/.config/corrall.json"), Path::new("/home/a/.config"));
        assert!(unit.contains("ExecStart=/home/a/.local/bin/corrall server --headless\n"));
        assert!(unit.contains("Environment=CORRALL_CONFIG=/home/a/.config/corrall.json\n"));
        assert!(unit.contains("ProtectSystem=strict\nReadWritePaths=/home/a/.config\n"));
        assert!(file_path(Kind::Systemd).ends_with("systemd/user/corrall.service"));
        assert_eq!(Kind::Systemd.logs_hint(), "journalctl --user -u corrall.service");
    }
}
