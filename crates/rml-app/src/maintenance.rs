//! The maintenance page: what is installed where, and the jobs that repair
//! or remove it. Longer jobs report as `maintenance` events.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use rhl_core::config::Config;
use rhl_core::launch::{self, prefix_path};
use rhl_core::maintain::{self, disk_usage, UninstallChoice};
use rhl_core::paths::LauncherPaths;
use rhl_core::runtime::{Runtime, RuntimeSpec};
use rhl_core::state::LauncherState;
use rhl_core::steam::{self, InstallEvent, InstallOptions, SteamClient};
use rhl_core::Error;
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

fn status() -> rhl_core::Result<Status> {
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
    let rt = Runtime::locate(&launcher, &spec);
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
fn spawn_job(
    app: AppHandle,
    job: &'static str,
    run: impl FnOnce(&dyn Fn(&str)) -> rhl_core::Result<String> + Send + 'static,
) {
    std::thread::spawn(move || {
        let emit = |event: JobEvent| {
            let _ = app.emit("maintenance", event);
        };
        let working = |detail: &str| emit(JobEvent::Working { job, detail: detail.into() });
        match run(&working) {
            Ok(detail) => emit(JobEvent::Done { job, detail }),
            Err(e) => emit(JobEvent::Failed { job, message: e.to_string() }),
        }
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
        let opts = InstallOptions { cancel: Arc::new(AtomicBool::new(false)), ..Default::default() };
        let seen = steam::verify(&client, &launcher, &opts, &mut |e| match e {
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
        let opts = InstallOptions { cancel: Arc::new(AtomicBool::new(false)), ..Default::default() };
        working("Uninstalling through Steam…");
        steam::uninstall(&client, &launcher, &opts, &mut |_| {})?;
        Ok("Runehaven was uninstalled. Your save games are still in the Wine setup.".into())
    });
}

/// Deletes the downloaded runtime; the interface then installs it again.
#[tauri::command]
pub async fn remove_runtime() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let launcher = LauncherPaths::detect().map_err(|e| e.to_string())?;
        maintain::remove_runtime(&launcher).map_err(|e| e.to_string())
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
        maintain::reset_prefix(&config, &launcher, &mut |_| {})?;
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
        let desktop = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Desktop"))
            .ok_or_else(|| Error::Config("HOME is not set".into()))?;
        Ok(match maintain::remove_data(&config, &launcher, &desktop)? {
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
        let config = Config::load(&launcher.config_file()).unwrap_or_default();
        let pending = LauncherState::load(&launcher.state_file())?.pending_platform_restore.is_some();
        if game || pending {
            working("Starting Steam…");
            let client = SteamClient::detect()?;
            client.wait_ready_until(&AtomicBool::new(false), &mut |_| {
                working("Waiting for Steam. Log in if it asks you to.")
            })?;
            if pending {
                steam::recover_platform(&client, &launcher)?;
            }
            if game {
                working("Uninstalling Runehaven through Steam…");
                let opts = InstallOptions { cancel: Arc::new(AtomicBool::new(false)), ..Default::default() };
                match steam::uninstall(&client, &launcher, &opts, &mut |_| {}) {
                    Ok(()) | Err(Error::GameNotInstalled) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        working("Removing…");
        let desktop = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Desktop"))
            .ok_or_else(|| Error::Config("HOME is not set".into()))?;
        Ok(match maintain::uninstall_launcher(&config, &launcher, choice, &desktop)? {
            Some(zip) => format!(
                "Uninstalled. Your save games are on the desktop: {}",
                zip.file_name().unwrap_or_default().to_string_lossy()
            ),
            None => "Uninstalled.".into(),
        })
    });
}

/// Quits the app and moves it to the Trash once it has quit. If moving is
/// not possible, Finder shows the app so it can be dragged there. In a
/// development build (not an `.app`) it only quits.
#[tauri::command]
pub fn quit_and_trash(app: AppHandle) {
    let bundle = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(PathBuf::from));
    if let (Some(bundle), Some(home)) = (bundle, std::env::var_os("HOME")) {
        let name = bundle.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let target = PathBuf::from(home).join(".Trash").join(format!("{name} {stamp}.app"));
        let _ = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                r#"while /bin/kill -0 "$1" 2>/dev/null; do /bin/sleep 0.3; done; /bin/mv "$2" "$3" || /usr/bin/open -R "$2""#,
                "sh",
                &std::process::id().to_string(),
            ])
            .arg(&bundle)
            .arg(&target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    app.exit(0);
}
