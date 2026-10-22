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

fn display_name(m: &AppManifest) -> String {
    if m.name.is_empty() {
        m.appid.to_string()
    } else {
        m.name.clone()
    }
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
    client.exec("app_install", &args)?;
    on_event(InstallEvent::Requested);

    let started = Instant::now();
    let mut planned = false;
    let mut last_progress = (0u64, Instant::now());
    loop {
        sleep(opts.poll);
        if opts.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let libs = library::libraries(&client.paths)?;
        let Some((lib, manifest)) = library::find_app(&libs, opts.appid)? else {
            if started.elapsed() > opts.start_timeout {
                return Err(Error::InstallNotStarted(opts.start_timeout.as_secs()));
            }
            continue;
        };
        if !planned && manifest.job_planned() {
            planned = true;
            on_event(InstallEvent::JobPlanned);
            if opts.restore_at == RestoreAt::JobPlanned {
                guard.restore(on_event)?;
            }
        }
        if manifest.is_ready() {
            return Ok(Installed {
                library: lib,
                manifest,
                already_installed: false,
            });
        }
        if manifest.update_result != 0 && !manifest.is_active() {
            return Err(Error::InstallFailed(manifest.update_result as u32));
        }
        if !planned && started.elapsed() > opts.start_timeout {
            return Err(Error::InstallNotStarted(opts.start_timeout.as_secs()));
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

    fn options() -> InstallOptions {
        InstallOptions {
            start_timeout: Duration::from_millis(300),
            stall_timeout: Duration::from_millis(300),
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
}
