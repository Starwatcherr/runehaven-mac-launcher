//! A handle on the user's running Steam client.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::console::{self, AppStatus, LogTail};
use crate::error::{Error, IoContext, Result};
use crate::paths::SteamPaths;

/// How the launcher reaches Steam. The real implementation runs macOS
/// commands; tests substitute a fake.
pub trait Transport {
    /// Hands one `+command -` argument to the running client.
    fn forward(&self, argument: &str) -> Result<()>;
    /// Whether a Steam client process is running.
    fn is_running(&self) -> bool;
    /// Starts Steam the normal way (as if the user opened it).
    fn start(&self) -> Result<()>;
    /// Whether the running client already listens for forwarded commands.
    fn accepts_commands(&self) -> bool;
}

/// Talks to the installed Steam on macOS.
pub struct MacTransport {
    paths: SteamPaths,
}

impl MacTransport {
    pub fn new(paths: SteamPaths) -> Self {
        Self { paths }
    }
}

const STEAM_PIPE: &str = "/tmp/steam.pipe";

/// Starts Steam's forwarder so that macOS counts it as part of Steam, not of
/// the launcher. The forwarder sometimes updates Steam's app bundle; started
/// as a plain child, macOS would blame the launcher and block it ("prevented
/// from modifying apps"). Terminal apps and browsers use the same call for
/// the programs they start. Output goes nowhere.
fn spawn_disclaimed(program: &Path, argument: &str) -> std::io::Result<libc::pid_t> {
    use std::ffi::CString;
    use std::io::{Error as IoError, ErrorKind};
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        // libSystem, available since macOS 10.14.
        fn responsibility_spawnattrs_setdisclaim(
            attrs: *mut libc::posix_spawnattr_t,
            disclaim: libc::c_int,
        ) -> libc::c_int;
    }
    let check = |rc: libc::c_int| if rc == 0 { Ok(()) } else { Err(IoError::from_raw_os_error(rc)) };
    let invalid = |e| IoError::new(ErrorKind::InvalidInput, e);
    let path = CString::new(program.as_os_str().as_bytes()).map_err(invalid)?;
    let arg = CString::new(argument).map_err(invalid)?;
    let devnull = CString::new("/dev/null").map_err(invalid)?;
    let argv = [path.as_ptr() as *mut libc::c_char, arg.as_ptr() as *mut libc::c_char, std::ptr::null_mut()];

    // SAFETY: the attribute and action objects are initialised before use and
    // destroyed afterwards; all strings outlive the spawn call.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        check(libc::posix_spawnattr_init(&mut attr))?;
        check(libc::posix_spawn_file_actions_init(&mut actions))?;
        let mut result = check(responsibility_spawnattrs_setdisclaim(&mut attr, 1));
        for (fd, flags) in [(0, libc::O_RDONLY), (1, libc::O_WRONLY), (2, libc::O_WRONLY)] {
            if result.is_ok() {
                result = check(libc::posix_spawn_file_actions_addopen(&mut actions, fd, devnull.as_ptr(), flags, 0));
            }
        }
        let mut pid: libc::pid_t = 0;
        if result.is_ok() {
            result = check(libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &actions,
                &attr,
                argv.as_ptr(),
                *libc::_NSGetEnviron(),
            ));
        }
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        result.map(|()| pid)
    }
}

/// The numeric id of the macOS user running the launcher.
fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// What is wrong with Steam's command pipe at `path`, if anything. Steam
/// uses one fixed pipe for all macOS users. If Steam last ran in another
/// account, the pipe still belongs to that account and this user's Steam
/// cannot receive commands through it. Anything but a plain pipe of this
/// user is refused, so commands never end up in someone else's file.
fn pipe_problem(path: &Path) -> Option<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::symlink_metadata(path).ok()?;
    let shown = path.display();
    if meta.uid() != current_uid() {
        return Some(format!(
            "{shown} belongs to another macOS user (uid {}), so Steam in this account cannot \
             receive commands. Quit Steam in all accounts, remove the file from the account \
             that owns it (rm {shown}) and start Steam here again.",
            meta.uid()
        ));
    }
    if !meta.file_type().is_fifo() || meta.nlink() != 1 {
        return Some(format!(
            "{shown} is not Steam's command pipe. Quit Steam, remove the file (rm {shown}) and \
             start Steam again."
        ));
    }
    None
}

impl Transport for MacTransport {
    fn forward(&self, argument: &str) -> Result<()> {
        if let Some(problem) = pipe_problem(Path::new(STEAM_PIPE)) {
            return Err(Error::Config(problem));
        }
        let pid = spawn_disclaimed(&self.paths.bootstrapper, argument)
            .ctx(|| format!("starting {}", self.paths.bootstrapper.display()))?;
        // The forwarder normally exits at once. If it does not, it may have
        // turned into a new Steam instance; leave it alone rather than kill it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let mut status = 0;
            // SAFETY: plain call on our own child's pid.
            let done = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if done == pid || done == -1 {
                return Ok(());
            }
            sleep(Duration::from_millis(100));
        }
        Err(Error::NoResponse(argument.to_string()))
    }

    fn is_running(&self) -> bool {
        // Only this user's Steam counts; other accounts may run their own.
        Command::new("/usr/bin/pgrep")
            .args(["-U", &current_uid().to_string(), "-x", "steam_osx"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn start(&self) -> Result<()> {
        let status = Command::new("/usr/bin/open")
            .arg(&self.paths.app_bundle)
            .status()
            .ctx(|| "opening Steam".into())?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Config(format!(
                "`open {}` failed",
                self.paths.app_bundle.display()
            )))
        }
    }

    fn accepts_commands(&self) -> bool {
        // A pipe left by another account also counts, so that the next
        // command runs and reports that problem instead of waiting forever.
        Path::new(STEAM_PIPE).exists()
    }
}

/// What the client is doing while the launcher waits for Steam.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReadyStatus {
    /// Steam was not running; the launcher started it.
    Starting,
    /// Waiting for the Steam process to appear.
    WaitingForProcess {
        elapsed_secs: u64,
    },
    /// Steam runs but does not answer yet (starting, updating or waiting for login).
    WaitingForAnswer {
        elapsed_secs: u64,
    },
    Ready,
}

#[derive(Debug, Clone)]
pub struct Timing {
    /// Total patience for Steam to become ready. Steam's own update on start
    /// can take several minutes.
    pub ready_timeout: Duration,
    /// Pause after starting Steam before the first command is sent.
    pub start_grace: Duration,
    pub probe_interval: Duration,
    /// While Steam does not answer and its log stays quiet (it updates
    /// itself or waits for a login), it is asked only this often. Every
    /// command briefly shows Steam's helper program in the Dock.
    pub quiet_probe_interval: Duration,
    /// How long to wait for one command's answer.
    pub answer_timeout: Duration,
    /// The answer counts as complete once the log has been quiet this long.
    pub settle: Duration,
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ready_timeout: Duration::from_secs(300),
            start_grace: Duration::from_secs(8),
            probe_interval: Duration::from_secs(4),
            quiet_probe_interval: Duration::from_secs(30),
            answer_timeout: Duration::from_secs(6),
            settle: Duration::from_millis(500),
            poll: Duration::from_millis(100),
        }
    }
}

pub struct SteamClient<T: Transport> {
    pub paths: SteamPaths,
    pub transport: T,
    pub timing: Timing,
}

impl SteamClient<MacTransport> {
    pub fn detect() -> Result<Self> {
        let paths = SteamPaths::detect()?;
        Ok(Self {
            transport: MacTransport::new(paths.clone()),
            paths,
            timing: Timing::default(),
        })
    }
}

impl<T: Transport> SteamClient<T> {
    pub fn new(paths: SteamPaths, transport: T, timing: Timing) -> Self {
        Self {
            paths,
            transport,
            timing,
        }
    }

    pub fn console_log(&self) -> PathBuf {
        self.paths.console_log()
    }

    /// Sends one command and returns Steam's answer lines. `done` may end the
    /// wait early once the expected line has arrived.
    pub fn exec_until(
        &self,
        command: &str,
        args: &[&str],
        done: impl Fn(&[String]) -> bool,
    ) -> Result<Vec<String>> {
        let argument = console::command_argument(command, args)?;
        let tail = LogTail::at_end(&self.console_log())?;
        self.transport.forward(&argument)?;

        let deadline = Instant::now() + self.timing.answer_timeout;
        let mut last: Option<(String, Instant)> = None;
        while Instant::now() < deadline {
            let text = tail.read_since_start()?;
            if let Some(answer) = console::extract_response(&text, &argument) {
                if let Some(reason) = console::rejection(&answer) {
                    return Err(Error::CommandRejected {
                        command: argument,
                        reason,
                    });
                }
                if done(&answer) {
                    return Ok(answer);
                }
                match &last {
                    Some((prev, since)) if *prev == text => {
                        if since.elapsed() >= self.timing.settle {
                            return Ok(answer);
                        }
                    }
                    _ => last = Some((text, Instant::now())),
                }
            }
            sleep(self.timing.poll);
        }
        Err(Error::NoResponse(argument))
    }

    pub fn exec(&self, command: &str, args: &[&str]) -> Result<Vec<String>> {
        self.exec_until(command, args, |_| false)
    }

    /// The current value of `@sSteamCmdForcePlatformType` ("" means Steam's default).
    pub fn platform(&self) -> Result<String> {
        let answer = self.exec_until("find", &["sSteamCmdForcePlatformType"], |l| {
            console::parse_platform(l).is_some()
        })?;
        console::parse_platform(&answer).ok_or(Error::PlatformUnreadable)
    }

    /// Sets the platform and reads it back. Returns the previous value.
    pub fn set_platform(&self, value: &str) -> Result<String> {
        let previous = self.platform()?;
        self.exec_until("@sSteamCmdForcePlatformType", &[value], |l| {
            console::parse_platform(l).is_some()
        })?;
        let actual = self.platform()?;
        if actual != value {
            return Err(Error::PlatformMismatch {
                expected: value.into(),
                actual,
            });
        }
        Ok(previous)
    }

    pub fn app_status(&self, appid: u32) -> Result<AppStatus> {
        let id = appid.to_string();
        // "user config" is printed after "update state"; waiting for it makes
        // sure the update state's value (which may follow on its own log line)
        // has arrived. Otherwise the answer counts as complete once the log is quiet.
        let answer = self.exec_until("app_status", &[&id], |l| {
            l.iter().any(|x| x.contains("- user config:"))
        })?;
        AppStatus::parse(&answer).ok_or_else(|| Error::Parse {
            what: "app_status".into(),
            detail: answer.join(" | "),
        })
    }

    /// Starts Steam if needed and waits until it answers console commands.
    pub fn wait_ready(&self, on_status: &mut dyn FnMut(ReadyStatus)) -> Result<()> {
        self.wait_ready_until(&AtomicBool::new(false), on_status)
    }

    /// Like [`Self::wait_ready`], but gives up as soon as `cancel` is set.
    pub fn wait_ready_until(
        &self,
        cancel: &AtomicBool,
        on_status: &mut dyn FnMut(ReadyStatus),
    ) -> Result<()> {
        let cancelled = || cancel.load(Ordering::Relaxed);
        let started = Instant::now();
        let elapsed = || started.elapsed().as_secs();
        let timeout = self.timing.ready_timeout;
        let mut launched = false;

        if !self.paths.bootstrapper.is_file() {
            return Err(Error::SteamNotInstalled(self.paths.app_bundle.clone()));
        }
        if !self.transport.is_running() {
            on_status(ReadyStatus::Starting);
            self.transport.start()?;
            launched = true;
        }
        while !self.transport.is_running() {
            if cancelled() {
                return Err(Error::Interrupted);
            }
            if started.elapsed() > timeout {
                return Err(Error::SteamNotReady {
                    secs: timeout.as_secs(),
                    hint: "Steam did not start".into(),
                });
            }
            on_status(ReadyStatus::WaitingForProcess {
                elapsed_secs: elapsed(),
            });
            sleep(Duration::from_secs(1));
        }
        if launched {
            sleep(self.timing.start_grace);
        }

        // When the last unanswered question went out, and the size of
        // Steam's log after it.
        let mut last_probe: Option<(Instant, u64)> = None;
        let log_size = || std::fs::metadata(self.console_log()).map_or(0, |m| m.len());
        loop {
            if cancelled() {
                return Err(Error::Interrupted);
            }
            // Ask again soon only when Steam's log shows it doing something.
            let due = last_probe.is_none_or(|(at, size)| {
                at.elapsed() >= self.timing.quiet_probe_interval
                    || (log_size() != size && at.elapsed() >= self.timing.probe_interval)
            });
            if due && self.transport.accepts_commands() && self.console_log().is_file() {
                match self.platform() {
                    Ok(_) => {
                        on_status(ReadyStatus::Ready);
                        return Ok(());
                    }
                    Err(Error::NoResponse(_) | Error::LogRotated | Error::PlatformUnreadable) => {
                        last_probe = Some((Instant::now(), log_size()));
                    }
                    Err(other) => return Err(other),
                }
            }
            if started.elapsed() > timeout {
                let hint = if self.transport.accepts_commands() {
                    "Steam is running but does not answer. Is it updating or waiting for you to log in?"
                } else {
                    // Steam makes its command pipe only when it starts.
                    "Steam is running but does not accept commands. Quit Steam completely and try again"
                };
                return Err(Error::SteamNotReady { secs: timeout.as_secs(), hint: hint.into() });
            }
            on_status(ReadyStatus::WaitingForAnswer {
                elapsed_secs: elapsed(),
            });
            sleep(self.timing.probe_interval.min(Duration::from_secs(1)));
        }
    }
}

#[cfg(test)]
mod spawn_tests {
    use super::*;

    #[test]
    fn disclaimed_child_runs_and_exits() {
        let pid = spawn_disclaimed(Path::new("/bin/echo"), "hello").unwrap();
        let mut status = 0;
        // SAFETY: waiting for our own child.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A pretend Steam that answers commands by appending to a log file.

    use std::cell::RefCell;
    use std::io::Write;

    use super::*;

    pub struct FakeSteam {
        pub log: PathBuf,
        pub running: RefCell<bool>,
        pub platform: RefCell<String>,
        pub sent: RefCell<Vec<String>>,
        /// Called for every forwarded command; may change state (e.g. write a manifest).
        #[allow(clippy::type_complexity)]
        pub on_command: RefCell<Option<Box<dyn FnMut(&str)>>>,
        /// Number of probes to ignore before answering (simulates a slow start).
        pub deaf_for: RefCell<u32>,
    }

    impl FakeSteam {
        pub fn new(log: PathBuf) -> Self {
            std::fs::write(&log, "[2026-10-16 00:00:00] Steam started\n").unwrap();
            Self {
                log,
                running: RefCell::new(true),
                platform: RefCell::new("macos".into()),
                sent: RefCell::new(Vec::new()),
                on_command: RefCell::new(None),
                deaf_for: RefCell::new(0),
            }
        }

        fn append(&self, line: &str) {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.log)
                .unwrap();
            writeln!(f, "[2026-10-16 00:00:01] {line}").unwrap();
        }
    }

    impl Transport for FakeSteam {
        fn forward(&self, argument: &str) -> Result<()> {
            self.sent.borrow_mut().push(argument.to_string());
            if *self.deaf_for.borrow() > 0 {
                *self.deaf_for.borrow_mut() -= 1;
                return Ok(());
            }
            self.append(&format!("ExecCommandLine: \"'/Applications/Steam.app/Contents/MacOS/steam_osx' '{argument}'\""));
            let body = argument.trim_start_matches('+').trim_end_matches(" -");
            let mut words = body.split(' ');
            match (words.next(), words.next()) {
                (Some("find"), Some("sSteamCmdForcePlatformType")) => {
                    let p = self.platform.borrow().clone();
                    self.append(&format!("@sSteamCmdForcePlatformType = \"{p}\" - Must be one of: [windows | macos | linux | android]"));
                }
                (Some("@sSteamCmdForcePlatformType"), Some(v)) => {
                    *self.platform.borrow_mut() = v.to_string();
                    self.append(&format!("\"@sSteamCmdForcePlatformType\" = \"{v}\""));
                }
                (Some("app_status"), Some(id)) => {
                    self.append(&format!("AppID {id} (Runehaven): "));
                    self.append(
                        " - release state: released (Subscribed,Invalid Platform,Permanent,)",
                    );
                    self.append(" - install state: Fully Installed,");
                    self.append(" - update state:  ( No Error )");
                }
                (Some("app_install" | "app_uninstall" | "app_move" | "app_mark_validation" | "app_start_validation"), _) => {}
                _ => self.append(&format!(">>> command not found: {body}")),
            }
            if let Some(cb) = self.on_command.borrow_mut().as_mut() {
                cb(argument);
            }
            Ok(())
        }
        fn is_running(&self) -> bool {
            *self.running.borrow()
        }
        fn start(&self) -> Result<()> {
            *self.running.borrow_mut() = true;
            Ok(())
        }
        fn accepts_commands(&self) -> bool {
            true
        }
    }

    pub fn quick_timing() -> Timing {
        Timing {
            ready_timeout: Duration::from_secs(5),
            start_grace: Duration::from_millis(10),
            probe_interval: Duration::from_millis(10),
            quiet_probe_interval: Duration::from_millis(50),
            answer_timeout: Duration::from_millis(300),
            settle: Duration::from_millis(30),
            poll: Duration::from_millis(5),
        }
    }

    /// A Steam folder layout in a temp dir with a fake bootstrapper file.
    pub fn steam_paths(root: &Path) -> SteamPaths {
        let app = root.join("Steam.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/MacOS/steam_osx"), "").unwrap();
        let data = root.join("data");
        std::fs::create_dir_all(data.join("logs")).unwrap();
        std::fs::create_dir_all(data.join("steamapps")).unwrap();
        SteamPaths::from_parts(app, data)
    }

    pub fn client(root: &Path) -> SteamClient<FakeSteam> {
        let paths = steam_paths(root);
        let fake = FakeSteam::new(paths.console_log());
        SteamClient::new(paths, fake, quick_timing())
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    #[test]
    fn only_a_plain_pipe_of_this_user_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        let pipe = tmp.path().join("steam.pipe");
        assert_eq!(pipe_problem(&pipe), None, "no pipe yet: Steam creates it");
        let c_path = std::ffi::CString::new(pipe.to_str().unwrap()).unwrap();
        // SAFETY: a valid path string for mkfifo.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert_eq!(pipe_problem(&pipe), None);
        let second_name = tmp.path().join("other-name");
        std::fs::hard_link(&pipe, &second_name).unwrap();
        assert!(pipe_problem(&pipe).is_some(), "a pipe with a second name is refused");
        std::fs::remove_file(&second_name).unwrap();
        std::fs::remove_file(&pipe).unwrap();
        std::fs::write(&pipe, "").unwrap();
        assert!(pipe_problem(&pipe).is_some(), "a plain file is refused");
        std::fs::remove_file(&pipe).unwrap();
        std::os::unix::fs::symlink(tmp.path(), &pipe).unwrap();
        assert!(pipe_problem(&pipe).is_some(), "a link is refused");
    }

    #[test]
    fn reads_and_sets_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        assert_eq!(c.platform().unwrap(), "macos");
        assert_eq!(c.set_platform("windows").unwrap(), "macos");
        assert_eq!(c.platform().unwrap(), "windows");
        assert!(c.transport.sent.borrow().iter().all(|a| a.ends_with(" -")));
    }

    #[test]
    fn app_status_and_rejection() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        assert!(c.app_status(2676030).unwrap().owned());
        assert!(matches!(
            c.exec("nonsense", &[]),
            Err(Error::CommandRejected { .. })
        ));
    }

    #[test]
    fn waits_for_a_slow_steam() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        *c.transport.running.borrow_mut() = false;
        *c.transport.deaf_for.borrow_mut() = 3;
        let mut seen = Vec::new();
        c.wait_ready(&mut |s| seen.push(s)).unwrap();
        assert_eq!(seen.first(), Some(&ReadyStatus::Starting));
        assert!(seen
            .iter()
            .any(|s| matches!(s, ReadyStatus::WaitingForAnswer { .. })));
        assert_eq!(seen.last(), Some(&ReadyStatus::Ready));
    }

    #[test]
    fn a_quiet_steam_is_asked_rarely() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = client(tmp.path());
        c.timing.ready_timeout = Duration::from_millis(1500);
        c.timing.answer_timeout = Duration::from_millis(50);
        c.timing.quiet_probe_interval = Duration::from_millis(500);
        *c.transport.deaf_for.borrow_mut() = 1000;
        assert!(c.wait_ready(&mut |_| {}).is_err());
        // Every 10 ms as before would be about 25 questions.
        let asked = c.transport.sent.borrow().len();
        assert!((2..=5).contains(&asked), "asked {asked} times");
    }

    #[test]
    fn gives_up_when_steam_never_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = client(tmp.path());
        c.timing.ready_timeout = Duration::from_millis(400);
        *c.transport.deaf_for.borrow_mut() = 1000;
        assert!(matches!(
            c.wait_ready(&mut |_| {}),
            Err(Error::SteamNotReady { .. })
        ));
    }
}
