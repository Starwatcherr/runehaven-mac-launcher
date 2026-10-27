//! Installing the Windows build of a game through the user's Mac Steam client.
//!
//! Steam on macOS only installs Windows depots while its platform setting is
//! `windows`. The setting is global, so it is switched for as short a time as
//! possible, recorded on disk before switching and always restored, also
//! after a crash (see [`recover_platform`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::client::{SteamClient, Transport};
use super::console::LogTail;
use super::library::{self, AppManifest, Library};
use crate::error::{Error, Result};
use crate::paths::LauncherPaths;
use crate::state::{LauncherState, PendingRestore};

pub const RUNEHAVEN_APPID: u32 = 2_676_030;

/// When Steam's platform goes back to its previous value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAt {
    /// As soon as Steam has planned the download (sizes known or work begun).
    /// Keeps the global switch to a few seconds, so other games' updates are
    /// not affected. Steam keeps serving an installed Windows depot in macOS
    /// mode (tested 16.10.2026).
    JobPlanned,
    /// Only after the whole installation finished.
    Completion,
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub appid: u32,
    /// Steam library ("volume index") to install into; Steam's default if `None`.
    pub library_index: Option<u32>,
    /// Install even if other downloads are running right now.
    pub ignore_other_downloads: bool,
    pub restore_at: RestoreAt,
    /// How long Steam may take to start the job.
    pub start_timeout: Duration,
    /// Abort if the download makes no progress for this long.
    pub stall_timeout: Duration,
    pub poll: Duration,
    /// Set from outside (UI button, Ctrl-C) to stop waiting. The platform is
    /// still restored.
    pub cancel: Arc<AtomicBool>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            appid: RUNEHAVEN_APPID,
            library_index: None,
            ignore_other_downloads: false,
            restore_at: RestoreAt::JobPlanned,
            start_timeout: Duration::from_secs(90),
            stall_timeout: Duration::from_secs(600),
            poll: Duration::from_secs(2),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum InstallEvent {
    CheckingOwnership,
    SwitchingPlatform,
    Requested,
    JobPlanned,
    Progress { downloaded: u64, total: u64 },
    RestoringPlatform { value: String },
    Done,
}

#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub library: Library,
    pub manifest: AppManifest,
    pub already_installed: bool,
}

/// Puts Steam's platform back if an earlier run did not get to it.
/// Returns the value that was restored, if any.
pub fn recover_platform<T: Transport>(
    client: &SteamClient<T>,
    launcher: &LauncherPaths,
) -> Result<Option<String>> {
    let mut state = LauncherState::load(&launcher.state_file())?;
    let Some(pending) = state.pending_platform_restore.clone() else {
        return Ok(None);
    };
    let value = pending.restore_value().to_string();
    if client.platform()? != value {
        client.set_platform(&value)?;
    }
    state.pending_platform_restore = None;
    state.save(&launcher.state_file())?;
    Ok(Some(value))
}

/// Restores the platform exactly once and clears the crash marker.
struct PlatformGuard<'a, T: Transport> {
    client: &'a SteamClient<T>,
    launcher: &'a LauncherPaths,
    value: String,
    restored: bool,
}

impl<T: Transport> PlatformGuard<'_, T> {
    fn restore(&mut self, on_event: &mut dyn FnMut(InstallEvent)) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        on_event(InstallEvent::RestoringPlatform {
            value: self.value.clone(),
        });
        self.client.set_platform(&self.value)?;
        self.restored = true;
        let mut state = LauncherState::load(&self.launcher.state_file())?;
        state.pending_platform_restore = None;
        state.save(&self.launcher.state_file())
    }
}

/// Installs the game. Steam must already be ready (see `wait_ready`).
pub fn install<T: Transport>(
    client: &SteamClient<T>,
    launcher: &LauncherPaths,
    opts: &InstallOptions,
    on_event: &mut dyn FnMut(InstallEvent),
) -> Result<Installed> {
    recover_platform(client, launcher)?;
    let libs = library::libraries(&client.paths)?;
    if let Some((lib, manifest)) = library::find_app(&libs, opts.appid)? {
        if manifest.is_ready() {
            return Ok(Installed {
                library: lib,
                manifest,
                already_installed: true,
            });
        }
    }

    on_event(InstallEvent::CheckingOwnership);
    if !client.app_status(opts.appid)?.owned() {
        return Err(Error::NotOwned);
    }
    if !opts.ignore_other_downloads {
        let active = library::active_apps(&libs, opts.appid);
        if !active.is_empty() {
            let names: Vec<_> = active.iter().map(display_name).collect();
            return Err(Error::OtherDownloadsActive(names.join(", ")));
        }
    }

    // Record first, then switch: a crash in between still leads to a restore.
    let pending = PendingRestore::new(client.platform()?, "installing Runehaven");
    let mut guard = PlatformGuard {
        client,
        launcher,
        value: pending.restore_value().to_string(),
        restored: false,
    };
    let mut state = LauncherState::load(&launcher.state_file())?;
    state.pending_platform_restore = Some(pending);
    state.save(&launcher.state_file())?;

    on_event(InstallEvent::SwitchingPlatform);
    let result = client
        .set_platform("windows")
        .and_then(|_| run_install(client, opts, &mut guard, on_event));

    // Always restore, whatever happened above (no-op if already done).
    let restored = guard.restore(on_event);
    let installed = result?;
    restored?;
    on_event(InstallEvent::Done);
    Ok(installed)
}

/// Has Steam check the game's files and fetch what is broken or missing.
/// Steam must be ready. Its platform stays set to Windows until the check is
/// over: switched back earlier, Steam aborts the check. Returns `false`
/// when Steam showed no sign of checking within `start_timeout` (a quick
/// check may already be over by then; Steam shows its result itself).
pub fn verify<T: Transport>(
    client: &SteamClient<T>,
    launcher: &LauncherPaths,
    opts: &InstallOptions,
    on_event: &mut dyn FnMut(InstallEvent),
) -> Result<bool> {
    recover_platform(client, launcher)?;
    let libs = library::libraries(&client.paths)?;
    if library::find_app(&libs, opts.appid)?.is_none() {
        return Err(Error::GameNotInstalled);
    }
    let pending = PendingRestore::new(client.platform()?, "checking Runehaven's files");
    let mut guard = PlatformGuard {
        client,
        launcher,
        value: pending.restore_value().to_string(),
        restored: false,
    };
    let mut state = LauncherState::load(&launcher.state_file())?;
    state.pending_platform_restore = Some(pending);
    state.save(&launcher.state_file())?;

    on_event(InstallEvent::SwitchingPlatform);
    let result = client
        .set_platform("windows")
        .and_then(|_| run_verify(client, opts, on_event));
    let restored = guard.restore(on_event);
    let seen = result?;
    restored?;
    on_event(InstallEvent::Done);
    Ok(seen)
}

fn run_verify<T: Transport>(
    client: &SteamClient<T>,
    opts: &InstallOptions,
    on_event: &mut dyn FnMut(InstallEvent),
) -> Result<bool> {
    // Steam's content log tells when the check starts and how it ended,
    // whatever started it.
    let tail = LogTail::at_end(&client.paths.content_log())?;
    let id = opts.appid.to_string();
    // Without the mark, Steam only looks at file sizes; with it, it checks
    // every file's contents like its own "Verify integrity" button.
    client.exec("app_mark_validation", &[&id, "1"])?;
    client.exec("app_start_validation", &[&id])?;
    on_event(InstallEvent::Requested);
    let start_line = format!("Start validating appID {id}");
    let end_line = format!("AppID {id} scheduler finished : removed from schedule (result ");
    let started = Instant::now();
    let mut seen = false;
    loop {
        sleep(opts.poll);
        if opts.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let log = tail.read_since_start()?;
        if !seen && log.contains(&start_line) {
            seen = true;
            on_event(InstallEvent::JobPlanned);
        }
        if seen {
            if let Some(rest) = log.lines().find_map(|l| l.split_once(&end_line).map(|(_, r)| r)) {
                let result = rest.split(',').next().unwrap_or(rest).trim();
                return if result == "No Error" {
                    Ok(true)
                } else {
                    Err(Error::VerifyFailed(result.to_string()))
                };
            }
            if started.elapsed() > opts.stall_timeout {
                return Err(Error::InstallStalled(opts.stall_timeout.as_secs()));
            }
        } else if started.elapsed() > opts.start_timeout {
            return Ok(false);
        }
    }
}

/// Removes the game through Steam; the Steam library on a Mac does not offer
/// it for a Windows game. Steam must be ready. Steam is set to Windows for it,
/// where it treats the game as a normal installed one, and set back after.
pub fn uninstall<T: Transport>(
    client: &SteamClient<T>,
    launcher: &LauncherPaths,
    opts: &InstallOptions,
    on_event: &mut dyn FnMut(InstallEvent),
) -> Result<()> {
    recover_platform(client, launcher)?;
    let (lib, manifest) = library::find_app(&library::libraries(&client.paths)?, opts.appid)?
        .ok_or(Error::GameNotInstalled)?;
    let game_dir = lib.steamapps().join("common").join(&manifest.installdir);
    let pending = PendingRestore::new(client.platform()?, "uninstalling Runehaven");
    let mut guard = PlatformGuard {
        client,
        launcher,
        value: pending.restore_value().to_string(),
        restored: false,
    };
    let mut state = LauncherState::load(&launcher.state_file())?;
    state.pending_platform_restore = Some(pending);
    state.save(&launcher.state_file())?;

    on_event(InstallEvent::SwitchingPlatform);
    let result = client
        .set_platform("windows")
        .and_then(|_| run_uninstall(client, opts, on_event));
    let restored = guard.restore(on_event);
    result?;
    restored?;
    remove_leftovers(&game_dir);
    on_event(InstallEvent::Done);
    Ok(())
}

/// Moves the installed game into another Steam library. Steam must be ready.
/// Steam checks the moved files afterwards; Windows stays set until that is
/// over, like for an installation.
pub fn move_game<T: Transport>(
    client: &SteamClient<T>,
    launcher: &LauncherPaths,
    opts: &InstallOptions,
    to_library: u32,
    on_event: &mut dyn FnMut(InstallEvent),
) -> Result<()> {
    recover_platform(client, launcher)?;
    let libs = library::libraries(&client.paths)?;
    let (lib, manifest) = library::find_app(&libs, opts.appid)?.ok_or(Error::GameNotInstalled)?;
    if lib.index == to_library {
        return Ok(());
    }
    let target_lib = libs
        .iter()
        .find(|l| l.index == to_library)
        .ok_or_else(|| Error::Config(format!("Steam has no library {to_library}.")))?;
    let source = lib.steamapps().join("common").join(&manifest.installdir);
    let target = target_lib.steamapps().join("common").join(&manifest.installdir);
    // Steam refuses to move into an existing folder. One holding only the
    // launcher's own files is a leftover of an earlier move and can go.
    remove_leftovers(&target);
    if target.exists() {
        return Err(Error::Config(format!(
            "There is already a folder {} in that library. Move or delete it, then try again.",
            target.display()
        )));
    }
    let pending = PendingRestore::new(client.platform()?, "moving Runehaven");
    let mut guard = PlatformGuard {
        client,
        launcher,
        value: pending.restore_value().to_string(),
        restored: false,
    };
    let mut state = LauncherState::load(&launcher.state_file())?;
    state.pending_platform_restore = Some(pending);
    state.save(&launcher.state_file())?;

    on_event(InstallEvent::SwitchingPlatform);
    let result = client
        .set_platform("windows")
        .and_then(|_| run_move(client, opts, to_library, on_event));
    let restored = guard.restore(on_event);
    result?;
    restored?;
    // Steam leaves the launcher's input fix behind in the old folder.
    remove_leftovers(&source);
    on_event(InstallEvent::Done);
    Ok(())
}

fn run_move<T: Transport>(
    client: &SteamClient<T>,
    opts: &InstallOptions,
    to_library: u32,
    on_event: &mut dyn FnMut(InstallEvent),
) -> Result<()> {
    let content = LogTail::at_end(&client.paths.content_log())?;
    client.exec("app_move", &[&opts.appid.to_string(), &to_library.to_string()])?;
    on_event(InstallEvent::Requested);
    let started = Instant::now();
    let mut moved_at: Option<Instant> = None;
    loop {
        sleep(opts.poll);
        if opts.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let log = content.read_since_start()?;
        let canceled = format!("AppID {} canceled moving : ", opts.appid);
        if let Some(reason) = log.lines().find_map(|l| l.split_once(&canceled).map(|(_, r)| r.trim())) {
            return Err(Error::MoveFailed(reason.to_string()));
        }
        let Some((_, after)) = log.split_once("Finished moving install folder") else {
            // A big game takes a while; Steam has said it started moving.
            let moving = log.contains(&format!("AppID {} moving from", opts.appid));
            let limit = if moving { opts.stall_timeout } else { opts.start_timeout };
            if started.elapsed() > limit {
                return Err(Error::Config("Steam did not move the game.".into()));
            }
            continue;
        };
        let at = *moved_at.get_or_insert_with(|| {
            on_event(InstallEvent::JobPlanned);
            Instant::now()
        });
        // Afterwards Steam usually checks the moved files once.
        let check = content_signals(after, opts.appid);
        match check.finished.as_deref() {
            Some("No Error") => return Ok(()),
            Some(other) => return Err(Error::InstallFailedBecause(other.to_string())),
            None if !check.activity && at.elapsed() > MOVE_SETTLE => return Ok(()),
            None if at.elapsed() > opts.stall_timeout => {
                return Err(Error::InstallStalled(opts.stall_timeout.as_secs()))
            }
            None => {}
        }
    }
}

/// How long to wait for Steam's check after a move before calling it done.
const MOVE_SETTLE: Duration = Duration::from_secs(20);

/// Files the launcher itself puts into the game folder, which Steam leaves
/// behind on uninstall.
const OWN_FILES: [&str; 2] = ["version.dll", ".DS_Store"];

/// Deletes the game folder if nothing but the launcher's own files is left.
fn remove_leftovers(game_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(game_dir) else {
        return;
    };
    let names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
    if names.iter().all(|n| OWN_FILES.iter().any(|own| n == *own)) {
        let _ = std::fs::remove_dir_all(game_dir);
    }
}

fn run_uninstall<T: Transport>(
    client: &SteamClient<T>,
    opts: &InstallOptions,
    on_event: &mut dyn FnMut(InstallEvent),
) -> Result<()> {
    client.exec("app_uninstall", &[&opts.appid.to_string()])?;
    on_event(InstallEvent::Requested);
    let started = Instant::now();
    loop {
        sleep(opts.poll);
        // Steam deletes the manifest once the files are gone.
        if library::find_app(&library::libraries(&client.paths)?, opts.appid)?.is_none() {
            return Ok(());
        }
        if started.elapsed() > opts.start_timeout {
            return Err(Error::Config(format!(
                "Steam did not uninstall the game within {} seconds.",
                opts.start_timeout.as_secs()
            )));
        }
    }
}

fn display_name(m: &AppManifest) -> String {
    if m.name.is_empty() {
        m.appid.to_string()
    } else {
        m.name.clone()
    }
}

/// What Steam's content log says about one app since a given moment.
#[derive(Debug, Default, PartialEq)]
struct ContentSignals {
    /// Steam works on the app at all (also while it only prepares).
    activity: bool,
    /// The download itself has begun.
    download_started: bool,
    /// How Steam's last attempt ended, e.g. "No Error" or "No connection".
    finished: Option<String>,
}

fn content_signals(log: &str, appid: u32) -> ContentSignals {
    let id = format!("AppID {appid} ");
    let mut s = ContentSignals::default();
    for line in log.lines().filter(|l| l.contains(&id)) {
        s.activity = true;
        if line.contains(" scheduler update :") || line.contains(" update started :") {
            s.finished = None;
        }
        if line.contains(" update started :") {
            s.download_started = true;
        }
        if let Some((_, rest)) = line.split_once("scheduler finished : removed from schedule (result ") {
            s.finished = Some(rest.split(',').next().unwrap_or(rest).trim().to_string());
        }
    }
    s
}

fn run_install<T: Transport>(
    client: &SteamClient<T>,
    opts: &InstallOptions,
    guard: &mut PlatformGuard<T>,
    on_event: &mut dyn FnMut(InstallEvent),
) -> Result<Installed> {
    let id = opts.appid.to_string();
    let lib_index = opts.library_index.map(|i| i.to_string());
    let mut args = vec![id.as_str()];
    if let Some(i) = &lib_index {
        args.push(i);
    }
    // Steam's content log is the reliable source for what happens; the
    // manifest is only written now and then. Without the log (tests, or a
    // Steam that never wrote one) the manifest alone has to do.
    let content = LogTail::at_end(&client.paths.content_log()).ok();
    // A result left over from Steam's own failed attempts (in macOS mode it
    // cannot fetch updates for the Windows build) must not count as ours.
    let earlier_result = library::find_app(&library::libraries(&client.paths)?, opts.appid)?
        .map_or(0, |(_, m)| m.update_result);
    client.exec("app_install", &args)?;
    on_event(InstallEvent::Requested);

    let started = Instant::now();
    let mut planned = false;
    let mut failure: Option<(String, Instant)> = None;
    let mut last_progress = (0u64, Instant::now());
    loop {
        sleep(opts.poll);
        if opts.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let signals = content
            .as_ref()
            .and_then(|t| t.read_since_start().ok())
            .map(|log| content_signals(&log, opts.appid))
            .unwrap_or_default();
        let libs = library::libraries(&client.paths)?;
        let found = library::find_app(&libs, opts.appid)?;

        // Only once the download runs is the platform no longer needed:
        // switched back while Steam still prepares, Steam suspends the job.
        let downloading = signals.download_started
            || found.as_ref().is_some_and(|(_, m)| {
                m.bytes_to_download > 0 || m.bytes_to_stage > 0 || m.bytes_downloaded > 0
            });
        if !planned && downloading {
            planned = true;
            on_event(InstallEvent::JobPlanned);
            if opts.restore_at == RestoreAt::JobPlanned {
                guard.restore(on_event)?;
            }
        }

        match signals.finished.as_deref() {
            Some(result) if result != "No Error" => {
                let since = failure.get_or_insert_with(|| (result.to_string(), Instant::now())).1;
                // Steam retries a lost connection by itself after 30 seconds.
                if !result.starts_with("No connection") || since.elapsed() > opts.start_timeout {
                    return Err(Error::InstallFailedBecause(result.to_string()));
                }
            }
            _ => failure = None,
        }

        let Some((lib, manifest)) = found else {
            if started.elapsed() > opts.start_timeout {
                return Err(Error::InstallNotStarted(opts.start_timeout.as_secs()));
            }
            continue;
        };
        if manifest.is_ready() {
            return Ok(Installed {
                library: lib,
                manifest,
                already_installed: false,
            });
        }
        if content.is_none()
            && manifest.update_result != 0
            && manifest.update_result != earlier_result
            && !manifest.is_active()
        {
            return Err(Error::InstallFailed(manifest.update_result as u32));
        }
        if !planned {
            let busy = signals.activity || manifest.is_active();
            if !busy && started.elapsed() > opts.start_timeout {
                return Err(Error::InstallNotStarted(opts.start_timeout.as_secs()));
            }
            if started.elapsed() > opts.stall_timeout {
                return Err(Error::InstallStalled(opts.stall_timeout.as_secs()));
            }
            continue;
        }
        let done = manifest.bytes_downloaded + manifest.bytes_staged;
        let total = manifest.bytes_to_download + manifest.bytes_to_stage;
        on_event(InstallEvent::Progress {
            downloaded: done,
            total,
        });
        if done != last_progress.0 {
            last_progress = (done, Instant::now());
        } else if last_progress.1.elapsed() > opts.stall_timeout {
            return Err(Error::InstallStalled(opts.stall_timeout.as_secs()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steam::client::fake::client;
    use crate::steam::library::Library;

    const READY: &str = "\"AppState\" { \"appid\" \"2676030\" \"name\" \"Runehaven\" \"StateFlags\" \"4\" \"installdir\" \"Runehaven\" \"buildid\" \"5\" \"TargetBuildID\" \"5\" }";
    const DOWNLOADING: &str = "\"AppState\" { \"appid\" \"2676030\" \"name\" \"Runehaven\" \"StateFlags\" \"1049862\" \"BytesToDownload\" \"100\" \"BytesDownloaded\" \"40\" }";

    // Generous limits: the fake Steam answers within milliseconds, but tests
    // must also pass on a machine that is busy compiling.
    fn options() -> InstallOptions {
        InstallOptions {
            start_timeout: Duration::from_millis(1500),
            stall_timeout: Duration::from_millis(1500),
            poll: Duration::from_millis(10),
            ..Default::default()
        }
    }

    #[test]
    fn installs_and_restores_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let manifest = Library {
            index: 0,
            path: c.paths.data_dir.clone(),
        }
        .manifest_path(RUNEHAVEN_APPID);

        // Steam "downloads" right after app_install, then finishes.
        let m = manifest.clone();
        *c.transport.on_command.borrow_mut() = Some(Box::new(move |arg: &str| {
            if arg.starts_with("+app_install") {
                std::fs::write(&m, DOWNLOADING).unwrap();
                let m2 = m.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(60));
                    std::fs::write(&m2, READY).unwrap();
                });
            }
        }));

        let mut events = Vec::new();
        let installed = install(&c, &launcher, &options(), &mut |e| events.push(e)).unwrap();
        assert!(!installed.already_installed);
        // Restored as soon as the job was planned, before the download finished.
        let pos = |want: fn(&InstallEvent) -> bool| events.iter().position(want).unwrap();
        assert!(
            pos(|e| *e == InstallEvent::JobPlanned)
                < pos(|e| matches!(e, InstallEvent::RestoringPlatform { .. }))
        );
        assert!(
            pos(|e| matches!(e, InstallEvent::RestoringPlatform { .. }))
                < pos(|e| matches!(e, InstallEvent::Progress { .. }))
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, InstallEvent::RestoringPlatform { .. }))
                .count(),
            1
        );
        assert_eq!(*c.transport.platform.borrow(), "macos");
        assert!(events.contains(&InstallEvent::Progress {
            downloaded: 40,
            total: 100
        }));
        assert_eq!(events.last(), Some(&InstallEvent::Done));
        assert!(LauncherState::load(&launcher.state_file())
            .unwrap()
            .pending_platform_restore
            .is_none());
        let sent = c.transport.sent.borrow();
        let switch = sent
            .iter()
            .position(|a| a == "+@sSteamCmdForcePlatformType windows -")
            .unwrap();
        let request = sent
            .iter()
            .position(|a| a == "+app_install 2676030 -")
            .unwrap();
        assert!(switch < request);
    }

    #[test]
    fn restores_platform_when_steam_never_starts_the_job() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let err = install(&c, &launcher, &options(), &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::InstallNotStarted(_)));
        assert_eq!(*c.transport.platform.borrow(), "macos");
    }

    #[test]
    fn skips_when_already_installed_and_refuses_with_other_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let main = Library {
            index: 0,
            path: c.paths.data_dir.clone(),
        };

        std::fs::write(
            main.manifest_path(620),
            DOWNLOADING
                .replace("2676030", "620")
                .replace("Runehaven", "Portal 2"),
        )
        .unwrap();
        let err = install(&c, &launcher, &options(), &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::OtherDownloadsActive(ref n) if n == "Portal 2"));
        assert!(c
            .transport
            .sent
            .borrow()
            .iter()
            .all(|a| !a.contains("windows")));

        // An update that merely waits outside the queue does not block.
        std::fs::write(
            main.manifest_path(620),
            READY
                .replace("2676030", "620")
                .replace("\"StateFlags\" \"4\"", "\"StateFlags\" \"1026\""),
        )
        .unwrap();
        let err = install(&c, &launcher, &options(), &mut |_| {}).unwrap_err();
        assert!(matches!(err, Error::InstallNotStarted(_)), "{err}");

        std::fs::write(main.manifest_path(RUNEHAVEN_APPID), READY).unwrap();
        assert!(
            install(&c, &launcher, &options(), &mut |_| {})
                .unwrap()
                .already_installed
        );
    }

    #[test]
    fn can_hold_platform_until_completion() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let manifest = Library {
            index: 0,
            path: c.paths.data_dir.clone(),
        }
        .manifest_path(RUNEHAVEN_APPID);
        let m = manifest.clone();
        *c.transport.on_command.borrow_mut() = Some(Box::new(move |arg: &str| {
            if arg.starts_with("+app_install") {
                std::fs::write(&m, DOWNLOADING).unwrap();
                let m2 = m.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(60));
                    std::fs::write(&m2, READY).unwrap();
                });
            }
        }));
        let opts = InstallOptions {
            restore_at: RestoreAt::Completion,
            ..options()
        };
        let mut events = Vec::new();
        install(&c, &launcher, &opts, &mut |e| events.push(e)).unwrap();
        let restore = events
            .iter()
            .position(|e| matches!(e, InstallEvent::RestoringPlatform { .. }))
            .unwrap();
        let last_progress = events
            .iter()
            .rposition(|e| matches!(e, InstallEvent::Progress { .. }))
            .unwrap();
        assert!(last_progress < restore);
        assert_eq!(*c.transport.platform.borrow(), "macos");
    }

    #[test]
    fn cancel_still_restores_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let opts = options();
        opts.cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            install(&c, &launcher, &opts, &mut |_| {}),
            Err(Error::Cancelled)
        ));
        assert_eq!(*c.transport.platform.borrow(), "macos");
        assert!(LauncherState::load(&launcher.state_file())
            .unwrap()
            .pending_platform_restore
            .is_none());
    }

    #[test]
    fn recovers_after_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        *c.transport.platform.borrow_mut() = "windows".into();
        LauncherState {
            pending_platform_restore: Some(PendingRestore::new("macos".into(), "test")),
        }
        .save(&launcher.state_file())
        .unwrap();
        assert_eq!(
            recover_platform(&c, &launcher).unwrap().as_deref(),
            Some("macos")
        );
        assert_eq!(*c.transport.platform.borrow(), "macos");
        assert_eq!(recover_platform(&c, &launcher).unwrap(), None);
    }

    // Lines as Steam writes them to content_log.txt (from a real install on 16.10.2026).
    const PREPARING: &str = "[2026-10-16 02:25:20] AppID 2676030 App update changed : Running Update,Reconfiguring,\n";
    const DOWNLOAD_STARTED: &str = "[2026-10-16 02:25:21] AppID 2676030 update started : download 0/172029408, store 0/0, reuse 0/0, delta 0/0, stage 0/345226541 \n";
    const FINISHED: &str = "[2026-10-16 02:26:37] AppID 2676030 scheduler finished : removed from schedule (result No Error, state 0xc) \n";
    const UPDATING: &str = "\"AppState\" { \"appid\" \"2676030\" \"name\" \"Runehaven\" \"StateFlags\" \"1030\" \"installdir\" \"Runehaven\" \"buildid\" \"4\" \"TargetBuildID\" \"5\" \"UpdateResult\" \"3\" }";

    fn append(path: &std::path::Path, text: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).create(true).open(path).unwrap().write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn reads_steams_content_log() {
        let retry = "[x] AppID 2676030 scheduler finished : removed from schedule (result No connection, state 0xe) \n\
                     [x] AppID 2676030 scheduler update : Priority First\n";
        assert_eq!(content_signals(PREPARING, RUNEHAVEN_APPID), ContentSignals { activity: true, ..Default::default() });
        let all = format!("{PREPARING}{DOWNLOAD_STARTED}{FINISHED}");
        let s = content_signals(&all, RUNEHAVEN_APPID);
        assert!(s.download_started);
        assert_eq!(s.finished.as_deref(), Some("No Error"));
        assert_eq!(content_signals(retry, RUNEHAVEN_APPID).finished, None, "a retry clears the failure");
        assert!(!content_signals(PREPARING, 26760300).activity, "other apps do not count");
    }

    #[test]
    fn update_keeps_windows_until_the_download_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let manifest = Library { index: 0, path: c.paths.data_dir.clone() }.manifest_path(RUNEHAVEN_APPID);
        // Installed, an update waits, and Steam's own failed try left a result code behind.
        std::fs::write(&manifest, UPDATING).unwrap();
        let content = c.paths.content_log();
        std::fs::create_dir_all(content.parent().unwrap()).unwrap();
        std::fs::write(&content, "").unwrap();

        let download_started = Arc::new(AtomicBool::new(false));
        let restored_early = Arc::new(AtomicBool::new(false));
        let (m, log, started, early) = (manifest.clone(), content.clone(), download_started.clone(), restored_early.clone());
        *c.transport.on_command.borrow_mut() = Some(Box::new(move |arg: &str| {
            if arg == "+@sSteamCmdForcePlatformType macos -" && !started.load(Ordering::SeqCst) {
                early.store(true, Ordering::SeqCst);
            }
            if arg.starts_with("+app_install") {
                append(&log, PREPARING);
                let (m, log, started) = (m.clone(), log.clone(), started.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(80));
                    started.store(true, Ordering::SeqCst);
                    append(&log, DOWNLOAD_STARTED);
                    std::thread::sleep(Duration::from_millis(80));
                    std::fs::write(&m, READY).unwrap();
                    append(&log, FINISHED);
                });
            }
        }));

        let mut events = Vec::new();
        let installed = install(&c, &launcher, &options(), &mut |e| events.push(e)).unwrap();
        assert!(!installed.already_installed);
        assert!(!restored_early.load(Ordering::SeqCst), "switched back while Steam was still preparing");
        assert_eq!(*c.transport.platform.borrow(), "macos");
    }

    #[test]
    fn a_failed_attempt_in_the_content_log_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let content = c.paths.content_log();
        std::fs::create_dir_all(content.parent().unwrap()).unwrap();
        std::fs::write(&content, "").unwrap();
        let log = content.clone();
        *c.transport.on_command.borrow_mut() = Some(Box::new(move |arg: &str| {
            if arg.starts_with("+app_install") {
                append(&log, PREPARING);
                append(&log, "[x] AppID 2676030 scheduler finished : removed from schedule (result Disk write failure, state 0x6) \n");
            }
        }));
        let err = install(&c, &launcher, &options(), &mut |_| {}).unwrap_err();
        assert!(matches!(&err, Error::InstallFailedBecause(r) if r == "Disk write failure"), "{err}");
        assert_eq!(*c.transport.platform.borrow(), "macos", "restored after a failure too");
    }

    #[test]
    fn uninstall_removes_the_launchers_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let lib = Library { index: 0, path: c.paths.data_dir.clone() };
        let manifest = lib.manifest_path(RUNEHAVEN_APPID);
        std::fs::write(&manifest, READY).unwrap();
        let game = lib.steamapps().join("common/Runehaven");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("Runehaven.exe"), "MZ").unwrap();
        std::fs::write(game.join("version.dll"), "fix").unwrap();
        // Steam removes its own files and the manifest, nothing else.
        let (m, g) = (manifest.clone(), game.clone());
        *c.transport.on_command.borrow_mut() = Some(Box::new(move |arg: &str| {
            if arg.starts_with("+app_uninstall") {
                std::fs::remove_file(g.join("Runehaven.exe")).unwrap();
                std::fs::remove_file(&m).unwrap();
            }
        }));
        uninstall(&c, &launcher, &options(), &mut |_| {}).unwrap();
        assert!(!game.exists());
        assert_eq!(*c.transport.platform.borrow(), "macos");
    }

    #[test]
    fn leftovers_are_kept_when_other_files_remain() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("version.dll"), "fix").unwrap();
        std::fs::write(tmp.path().join("my-mod.txt"), "keep me").unwrap();
        remove_leftovers(tmp.path());
        assert!(tmp.path().join("my-mod.txt").exists());
    }

    #[test]
    fn moves_the_game_and_waits_for_steams_check() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let main = Library { index: 0, path: c.paths.data_dir.clone() };
        let second = Library { index: 1, path: tmp.path().join("SteamLibrary") };
        std::fs::create_dir_all(second.steamapps()).unwrap();
        std::fs::write(
            c.paths.data_dir.join("steamapps/libraryfolders.vdf"),
            format!("\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} }}",
                main.path.display(), second.path.display()),
        ).unwrap();
        std::fs::write(main.manifest_path(RUNEHAVEN_APPID), READY).unwrap();
        let content = c.paths.content_log();
        std::fs::create_dir_all(content.parent().unwrap()).unwrap();
        std::fs::write(&content, "").unwrap();

        let (from, to, log) = (main.manifest_path(RUNEHAVEN_APPID), second.manifest_path(RUNEHAVEN_APPID), content.clone());
        *c.transport.on_command.borrow_mut() = Some(Box::new(move |arg: &str| {
            if arg.starts_with("+app_move") {
                std::fs::rename(&from, &to).unwrap();
                append(&log, "[x] AppID 2676030 moving from \"a\" to \"b\"\n[x] Finished moving install folder (345226541 bytes, 615 files)\n");
                let log = log.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(60));
                    append(&log, "[x] AppID 2676030 state changed : Update Required,Fully Installed,Files Missing,\n");
                    append(&log, "[x] AppID 2676030 scheduler finished : removed from schedule (result No Error, state 0xc) \n");
                });
            }
        }));
        let mut events = Vec::new();
        move_game(&c, &launcher, &options(), 1, &mut |e| events.push(e)).unwrap();
        assert!(second.manifest_path(RUNEHAVEN_APPID).exists());
        assert_eq!(*c.transport.platform.borrow(), "macos");
        assert_eq!(c.transport.sent.borrow().iter().filter(|a| a.starts_with("+app_move 2676030 1")).count(), 1);
    }

    #[test]
    fn a_canceled_move_is_reported_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let c = client(tmp.path());
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let main = Library { index: 0, path: c.paths.data_dir.clone() };
        let second = Library { index: 1, path: tmp.path().join("SteamLibrary") };
        std::fs::create_dir_all(second.steamapps()).unwrap();
        std::fs::write(
            c.paths.data_dir.join("steamapps/libraryfolders.vdf"),
            format!("\"libraryfolders\" {{ \"0\" {{ \"path\" \"{}\" }} \"1\" {{ \"path\" \"{}\" }} }}",
                main.path.display(), second.path.display()),
        ).unwrap();
        std::fs::write(main.manifest_path(RUNEHAVEN_APPID), READY).unwrap();
        // A leftover from an earlier move: only the launcher's input fix.
        let leftover = second.steamapps().join("common/Runehaven");
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::write(leftover.join("version.dll"), "old fix").unwrap();
        let content = c.paths.content_log();
        std::fs::create_dir_all(content.parent().unwrap()).unwrap();
        std::fs::write(&content, "").unwrap();
        let log = content.clone();
        *c.transport.on_command.borrow_mut() = Some(Box::new(move |arg: &str| {
            if arg.starts_with("+app_move") {
                append(&log, "[x] AppID 2676030 canceled moving : Not enough disk space\n");
            }
        }));
        let started = Instant::now();
        let err = move_game(&c, &launcher, &options(), 1, &mut |_| {}).unwrap_err();
        assert!(matches!(&err, Error::MoveFailed(r) if r == "Not enough disk space"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(1), "reported without waiting");
        assert!(!leftover.exists(), "the old leftover was cleared before the move");
        assert_eq!(*c.transport.platform.borrow(), "macos");
    }
}
