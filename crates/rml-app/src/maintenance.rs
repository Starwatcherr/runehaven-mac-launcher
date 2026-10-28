//! The maintenance page: what is installed where, and the jobs that repair
//! or remove it. Longer jobs report as `maintenance` events.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rml_core::config::Config;
use rml_core::launch::{self, prefix_path};
use rml_core::lock::JobLock;
use rml_core::maintain::{self, disk_usage, UninstallChoice};
use rml_core::paths::LauncherPaths;
use rml_core::runtime::{Runtime, RuntimeSpec};
use rml_core::state::LauncherState;
use rml_core::steam::{self, InstallEvent, InstallOptions, SteamClient};
use rml_core::Error;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// What the page shows about one part.
#[derive(Debug, Serialize)]
pub struct Part {
    installed: bool,
    detail: String,
    bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Status {
    game: Part,
    update_pending: bool,
    runtime: Part,
    prefix: Part,
    /// The prefix was chosen in the settings; the launcher never deletes it.
    own_prefix: bool,
    /// Runtime, own prefix, downloads and logs together.
    data_bytes: u64,
}

#[tauri::command]
pub async fn maintenance_status() -> Result<Status, String> {
    tauri::async_runtime::spawn_blocking(|| status().map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

fn status() -> rml_core::Result<Status> {
    let launcher = LauncherPaths::detect()?;
    let config = Config::load(&launcher.config_file()).unwrap_or_default();

    let (game, update_pending) = match launch::locate_game() {
        Ok((lib, m, dir)) => (
            Part {
                installed: true,
                detail: if lib.index == 0 {
                    format!("Build {}, in your main Steam library", m.buildid)
                } else {
                    format!("Build {}, in {}", m.buildid, lib.path.display())
                },
                bytes: disk_usage(&dir),
            },
            m.update_pending(),
        ),
        Err(e) => (Part { installed: false, detail: e.to_string(), bytes: 0 }, false),
    };

    let spec = RuntimeSpec::bundled();
    let rt = Runtime::locate(&config, &launcher, &spec);
    let runtime = match &config.runtime.wine {
        Some(wine) => Part { installed: true, detail: format!("Custom: {}", wine.display()), bytes: 0 },
        None if rt.is_installed() => Part {
            installed: true,
            detail: "WineCX 24.0.7 with DXMT 0.80".into(),
            bytes: disk_usage(&rt.root),
        },
        None => Part { installed: false, detail: "Not downloaded".into(), bytes: 0 },
    };

    let prefix_dir = prefix_path(&config, &launcher);
    let prefix = Part {
        installed: prefix_dir.exists(),
        detail: if prefix_dir.exists() {
            "Save games, settings and Windows files".into()
        } else {
            "Not set up yet".into()
        },
        bytes: disk_usage(&prefix_dir),
    };

    let own_prefix = config.prefix.is_none();
    let data_bytes = [launcher.runtime(), launcher.root.join("downloads"), launcher.logs()]
        .iter()
        .map(|d| disk_usage(d))
        .sum::<u64>()
        + if own_prefix { prefix.bytes } else { 0 };
    Ok(Status { game, update_pending, runtime, prefix, own_prefix, data_bytes })
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JobEvent {
    Working { job: &'static str, detail: String },
    Done { job: &'static str, detail: String },
    Failed { job: &'static str, message: String },
}

/// Runs a job off the interface thread and reports its outcome as events.
/// A job that panics still reports, as failed, so the page never waits forever.
pub fn spawn_job(
    app: AppHandle,
    job: &'static str,
    run: impl FnOnce(&dyn Fn(&str)) -> rml_core::Result<String> + Send + 'static,
) {
    std::thread::spawn(move || {
        let emit = |event: JobEvent| {
            let _ = app.emit("maintenance", event);
        };
        let working = |detail: &str| emit(JobEvent::Working { job, detail: detail.into() });
        match catch_unwind(AssertUnwindSafe(|| run(&working))) {
            Ok(Ok(detail)) => emit(JobEvent::Done { job, detail }),
            Ok(Err(e)) => emit(JobEvent::Failed { job, message: e.to_string() }),
            Err(_) => emit(JobEvent::Failed { job, message: "The launcher ran into an internal error. Please export the logs and report it.".into() }),
        }
    });
}

/// Only one restore job at a time.
static RESTORING: AtomicBool = AtomicBool::new(false);

/// After a crash in the middle of a Steam job, Steam may still be set to
/// the Windows platform, so other Mac games would get Windows versions.
/// When that is noted in the state, this switches Steam back in the
/// background (starting Steam if needed) as the job `restore`. It also runs
/// while the game does; a Steam job that is running restores it itself.
pub fn restore_steam_platform(app: AppHandle) {
    let Ok(launcher) = LauncherPaths::detect() else { return };
    let pending = LauncherState::load(&launcher.state_file()).is_ok_and(|s| s.pending_platform_restore.is_some());
    if !pending || RESTORING.swap(true, Ordering::SeqCst) {
        return;
    }
    spawn_job(app, "restore", move |working| {
        let result = (|| {
            working("Steam is still set to the Windows version after an interrupted task. Switching it back…");
            let client = SteamClient::detect()?;
            client.wait_ready_until(&AtomicBool::new(false), &mut |_| {
                working("Waiting for Steam to switch it back to the Mac version. Log in if it asks you to.")
            })?;
            let lock = match JobLock::acquire_beside_game(&launcher) {
                Ok(lock) => lock,
                Err(Error::Busy) => return Ok(String::new()),
                Err(e) => return Err(e),
            };
            Ok(match steam::recover_platform(&client, &launcher, &lock)? {
                Some(_) => "Steam is set back to the Mac version.".into(),
                None => String::new(),
            })
        })();
        RESTORING.store(false, Ordering::SeqCst);
        result
    });
}

/// Has Steam check the game files.
#[tauri::command]
pub fn verify_game(app: AppHandle) {
    spawn_job(app, "verify", |working| {
        working("Starting Steam…");
        let client = SteamClient::detect()?;
        client.wait_ready_until(&AtomicBool::new(false), &mut |_| {
            working("Waiting for Steam. Log in if it asks you to.")
        })?;
        let launcher = LauncherPaths::detect()?;
        let lock = JobLock::acquire(&launcher)?;
        let opts = InstallOptions { cancel: Arc::new(AtomicBool::new(false)), ..Default::default() };
        let seen = steam::verify(&client, &launcher, &lock, &opts, &mut |e| match e {
            InstallEvent::SwitchingPlatform | InstallEvent::Requested => {
                working("Asking Steam to check the files…")
            }
            InstallEvent::JobPlanned | InstallEvent::Progress { .. } => {
                working("Steam is checking the files…")
            }
            _ => {}
        })?;
        Ok(if seen {
            "Steam checked the files.".into()
        } else {
            "Steam got the request; its own window shows the result.".into()
        })
    });
}

/// Removes the game through Steam.
#[tauri::command]
pub fn uninstall_game(app: AppHandle) {
    spawn_job(app, "uninstall", |working| {
        working("Starting Steam…");
        let client = SteamClient::detect()?;
        client.wait_ready_until(&AtomicBool::new(false), &mut |_| {
            working("Waiting for Steam. Log in if it asks you to.")
        })?;
        let launcher = LauncherPaths::detect()?;
        let lock = JobLock::acquire(&launcher)?;
        let opts = InstallOptions { cancel: Arc::new(AtomicBool::new(false)), ..Default::default() };
        working("Uninstalling through Steam…");
        steam::uninstall(&client, &launcher, &lock, &opts, &mut |_| {})?;
        Ok("Runehaven was uninstalled. Your save games are still in the Wine setup.".into())
    });
}

/// Deletes the downloaded runtime; the interface then installs it again.
#[tauri::command]
pub async fn remove_runtime() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let launcher = LauncherPaths::detect().map_err(|e| e.to_string())?;
        let config = Config::load(&launcher.config_file()).map_err(|e| e.to_string())?;
        let lock = JobLock::acquire(&launcher).map_err(|e| e.to_string())?;
        maintain::remove_runtime(&config, &launcher, &lock).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Sets the Wine prefix up again, keeping save games and settings.
#[tauri::command]
pub fn reset_prefix(app: AppHandle) {
    spawn_job(app, "reset", |working| {
        working("Setting up Wine again, about a minute…");
        let launcher = LauncherPaths::detect()?;
        let config = Config::load(&launcher.config_file())?;
        let lock = JobLock::acquire(&launcher)?;
        maintain::reset_prefix(&config, &launcher, &lock, &mut |_| {})?;
        Ok("Wine was set up again; save games and settings were kept.".into())
    });
}

/// Removes the launcher's data after packing the save games onto the desktop.
#[tauri::command]
pub fn remove_data(app: AppHandle) {
    spawn_job(app, "remove", |working| {
        working("Removing…");
        let launcher = LauncherPaths::detect()?;
        let config = Config::load(&launcher.config_file())?;
        let lock = JobLock::acquire(&launcher)?;
        let desktop = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Desktop"))
            .ok_or_else(|| Error::Config("HOME is not set".into()))?;
        Ok(match maintain::remove_data(&config, &launcher, &lock, &desktop)? {
            Some(zip) => format!(
                "Removed. Your save games are on the desktop: {}",
                zip.file_name().unwrap_or_default().to_string_lossy()
            ),
            None => "Removed.".into(),
        })
    });
}

/// Removes the launcher's settings and, as chosen, the runtime, the prefix,
/// the logs and the game itself. The interface then calls `quit_and_trash`.
#[tauri::command]
pub fn uninstall_launcher(app: AppHandle, choice: UninstallChoice, game: bool) {
    spawn_job(app, "uninstall-launcher", move |working| {
        let launcher = LauncherPaths::detect()?;
        // A settings file that cannot be read stops here: it says which
        // Wine setup is the launcher's own and may be deleted.
        let config = Config::load(&launcher.config_file())?;
        let lock = JobLock::acquire(&launcher)?;
        let pending = LauncherState::load(&launcher.state_file())?.pending_platform_restore.is_some();
        if game || pending {
            working("Starting Steam…");
            let client = SteamClient::detect()?;
            client.wait_ready_until(&AtomicBool::new(false), &mut |_| {
                working("Waiting for Steam. Log in if it asks you to.")
            })?;
            if pending {
                steam::recover_platform(&client, &launcher, &lock)?;
            }
            if game {
                working("Uninstalling Runehaven through Steam…");
                let opts = InstallOptions { cancel: Arc::new(AtomicBool::new(false)), ..Default::default() };
                match steam::uninstall(&client, &launcher, &lock, &opts, &mut |_| {}) {
                    Ok(()) | Err(Error::GameNotInstalled) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        working("Removing…");
        let desktop = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Desktop"))
            .ok_or_else(|| Error::Config("HOME is not set".into()))?;
        Ok(match maintain::uninstall_launcher(&config, &launcher, &lock, choice, &desktop)? {
            Some(zip) => format!(
                "Uninstalled. Your save games are on the desktop: {}",
                zip.file_name().unwrap_or_default().to_string_lossy()
            ),
            None => "Uninstalled.".into(),
        })
    });
}

/// Shows the save games (`saves`) or their backups (`backups`) in Finder.
#[tauri::command]
pub fn open_saves(what: String) -> Result<(), String> {
    let run = || -> rml_core::Result<PathBuf> {
        let launcher = LauncherPaths::detect()?;
        let config = Config::load(&launcher.config_file())?;
        let dir = if what == "backups" {
            launcher.root.join("backups/saves")
        } else {
            rml_core::prefix::saves_dir(&prefix_path(&config, &launcher))
                .filter(|d| d.is_dir())
                .ok_or_else(|| Error::Config("There are no save games yet. Start Runehaven once.".into()))?
        };
        if !dir.is_dir() {
            return Err(Error::Config("There are no backups yet. One is made before every start.".into()));
        }
        Ok(dir)
    };
    let dir = run().map_err(|e| e.to_string())?;
    std::process::Command::new("/usr/bin/open").arg(&dir).status().map(|_| ()).map_err(|e| e.to_string())
}

/// Moves the app to the Trash and quits. If macOS does not let it move
/// itself (in a standard account, an app in Applications needs an
/// administrator), Finder shows the app, the error says to drag it to the
/// Trash, and the app stays open. In a development build (not an `.app`) it
/// only quits.
#[tauri::command]
pub fn quit_and_trash(app: AppHandle) -> Result<(), String> {
    let bundle = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(PathBuf::from));
    if let Some(bundle) = bundle {
        if !move_to_trash(&bundle) {
            let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(&bundle).status();
            return Err(
                "The launcher could not move itself to the Trash, sorry. Please quit it and drag the app to the Trash; Finder shows it."
                    .into(),
            );
        }
    }
    app.exit(0);
    Ok(())
}

/// Moves a file to the Trash the way Finder does (NSFileManager), so it can
/// be put back from there. A running app can be moved; it keeps running.
fn move_to_trash(path: &std::path::Path) -> bool {
    const SCRIPT: &str = "function run(argv) { ObjC.import('Foundation'); \
        return $.NSFileManager.defaultManager.trashItemAtURLResultingItemURLError(\
        $.NSURL.fileURLWithPath(argv[0]), null, null); }";
    std::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", SCRIPT])
        .arg(path)
        .output()
        .is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
}
