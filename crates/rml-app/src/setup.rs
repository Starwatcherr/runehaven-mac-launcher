//! "Install requirements": does what the requirements list is missing, one
//! after the other, and reports each step as a `setup` event.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rml_core::config::Config;
use rml_core::paths::LauncherPaths;
use rml_core::runtime::{self, RuntimeEvent, RuntimeSpec};
use rml_core::steam::{self, InstallEvent, InstallOptions, ReadyStatus, SteamClient};
use rml_core::{doctor, launch, Error};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::commands::{check_all, Requirement};

const STEAM_DOWNLOAD_PAGE: &str = "https://store.steampowered.com/about/";

/// The running installation, if any, and the switch that stops it.
#[derive(Default)]
pub struct SetupState {
    cancel: Mutex<Option<Arc<AtomicBool>>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SetupEvent {
    /// Working on one requirement; `progress` from 0 to 1 when known.
    Working {
        id: &'static str,
        detail: String,
        progress: Option<f64>,
    },
    Done { id: &'static str },
    Failed { id: &'static str, message: String },
    /// The run is over; the interface checks the requirements again.
    Finished { cancelled: bool },
}

/// What stopped the run.
struct Stop {
    id: &'static str,
    error: Error,
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
}

/// Starts installing what is missing and returns right away.
#[tauri::command]
pub fn install_requirements(app: AppHandle, state: State<'_, SetupState>) -> Result<(), String> {
    let cancel = {
        let mut slot = state.cancel.lock().unwrap();
        if slot.is_some() {
            return Err("The installation is already running.".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *slot = Some(cancel.clone());
        cancel
    };
    std::thread::spawn(move || {
        let emit = |event: SetupEvent| {
            let _ = app.emit("setup", event);
        };
        let outcome = run(&cancel, &emit);
        let cancelled = match outcome {
            Ok(()) => false,
            Err(Stop { error: Error::Interrupted | Error::Cancelled, .. }) => true,
            Err(Stop { id, error }) => {
                emit(SetupEvent::Failed { id, message: error.to_string() });
                false
            }
        };
        emit(SetupEvent::Finished { cancelled });
        *app.state::<SetupState>().cancel.lock().unwrap() = None;
    });
    Ok(())
}

/// Stops the running installation as soon as the current step allows.
#[tauri::command]
pub fn cancel_install(state: State<'_, SetupState>) {
    if let Some(cancel) = state.cancel.lock().unwrap().as_ref() {
        cancel.store(true, Ordering::Relaxed);
    }
}

/// Opens Steam's download page in the browser.
#[tauri::command]
pub fn open_steam_download() -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg(STEAM_DOWNLOAD_PAGE)
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn run(cancel: &Arc<AtomicBool>, emit: &dyn Fn(SetupEvent)) -> Result<(), Stop> {
    let at = |id: &'static str| move |error: Error| Stop { id, error };
    let launcher = LauncherPaths::detect().map_err(at("rosetta"))?;
    let missing = |reqs: &[Requirement], id: &str| reqs.iter().any(|r| r.id == id && r.state != "ok");
    let reqs = check_all(&launcher);
    let working = |id: &'static str, detail: &str, progress: Option<f64>| {
        emit(SetupEvent::Working { id, detail: detail.into(), progress })
    };

    if missing(&reqs, "rosetta") {
        working("rosetta", "Waiting for your password…", None);
        doctor::install_rosetta().map_err(at("rosetta"))?;
        emit(SetupEvent::Done { id: "rosetta" });
    }

    if missing(&reqs, "steam") {
        return Err(Stop {
            id: "steam",
            error: Error::Config("Install Steam first, then try again.".into()),
        });
    }

    if missing(&reqs, "game") {
        let id = "game";
        working(id, "Starting Steam…", None);
        let client = SteamClient::detect().map_err(at(id))?;
        client
            .wait_ready_until(cancel, &mut |status| match status {
                ReadyStatus::WaitingForAnswer { .. } => {
                    working(id, "Waiting for Steam. Log in if it asks you to.", None)
                }
                ReadyStatus::Starting | ReadyStatus::WaitingForProcess { .. } => {
                    working(id, "Starting Steam…", None)
                }
                ReadyStatus::Ready => {}
            })
            .map_err(at(id))?;
        let opts = InstallOptions {
            cancel: cancel.clone(),
            library_index: Config::load(&launcher.config_file()).map_err(at(id))?.steam_library,
            ..Default::default()
        };
        steam::install(&client, &launcher, &opts, &mut |event| match event {
            InstallEvent::CheckingOwnership => working(id, "Checking your Steam library…", None),
            InstallEvent::SwitchingPlatform => {
                working(id, "Switching Steam to the Windows version…", None)
            }
            InstallEvent::Requested | InstallEvent::JobPlanned => {
                working(id, "Downloading through Steam…", None)
            }
            InstallEvent::Progress { downloaded, total } if total > 0 => working(
                id,
                &format!("Downloading through Steam, {} of {}", gb(downloaded), gb(total)),
                Some(downloaded as f64 / total as f64),
            ),
            InstallEvent::Progress { .. } | InstallEvent::RestoringPlatform { .. } | InstallEvent::Done => {}
        })
        .map_err(at(id))?;
        emit(SetupEvent::Done { id });
    }

    if missing(&reqs, "runtime") {
        let id = "runtime";
        let spec = RuntimeSpec::bundled();
        let total = spec.download_size().max(1);
        // Each component reports its own bytes; the bar shows all of them together.
        let sizes: Vec<(String, u64)> = spec.components.iter().map(|c| (c.name.clone(), c.size)).collect();
        let before = |name: &str| -> u64 {
            sizes.iter().take_while(|(n, _)| n != name).map(|(_, s)| s).sum()
        };
        let config = Config::load(&launcher.config_file()).map_err(at(id))?;
        runtime::install(&config, &launcher, &spec, false, cancel, &mut |event| match event {
            RuntimeEvent::Downloading { name, done, .. } => {
                let all = before(&name) + done;
                working(
                    id,
                    &format!("Downloading, {} of {} MB", all >> 20, total >> 20),
                    Some(all as f64 / total as f64),
                )
            }
            RuntimeEvent::AlreadyDownloaded { .. } => {}
            RuntimeEvent::Extracting { .. } => working(id, "Unpacking…", None),
            RuntimeEvent::Done => {}
        })
        .map_err(at(id))?;
        // The first start would do this too; doing it now keeps that start quick.
        launch::ensure_prefix(&config, &launcher, &mut |_| {
            working(id, "Setting up Wine, about a minute…", None)
        })
        .map_err(at(id))?;
        emit(SetupEvent::Done { id });
    }
    Ok(())
}
