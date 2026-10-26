//! Where things go: the Steam library for the game, the Wine runtime and the
//! Wine prefix. Each can stay the launcher's default, go to a chosen folder,
//! or use something that already exists.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use rhl_core::config::Config;
use rhl_core::launch::{self, prefix_path};
use rhl_core::paths::{LauncherPaths, SteamPaths};
use rhl_core::prefix;
use rhl_core::runtime::{self, runtime_dir};
use rhl_core::steam::{self, library, InstallOptions, SteamClient};
use rhl_core::Error;
use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

use crate::maintenance::spawn_job;

#[derive(Debug, Serialize)]
pub struct SteamLibrary {
    index: u32,
    path: String,
    drive: String,
    free_bytes: Option<u64>,
    has_game: bool,
}

/// One of the two Wine places: `mode` is `default`, `folder` or `existing`.
#[derive(Debug, Serialize)]
pub struct Place {
    mode: &'static str,
    path: String,
}

#[derive(Debug, Serialize)]
pub struct Locations {
    libraries: Vec<SteamLibrary>,
    /// The library chosen for installing; Steam's default if `None`.
    chosen_library: Option<u32>,
    runtime: Place,
    prefix: Place,
}

fn load() -> rhl_core::Result<(LauncherPaths, Config)> {
    let launcher = LauncherPaths::detect()?;
    let config = Config::load(&launcher.config_file())?;
    Ok((launcher, config))
}

#[tauri::command]
pub async fn install_locations() -> Result<Locations, String> {
    tauri::async_runtime::spawn_blocking(|| locations().map_err(|e| e.to_string()))
        .await
        .map_err(|e| e.to_string())?
}

fn locations() -> rhl_core::Result<Locations> {
    let (launcher, config) = load()?;
    let libraries = match SteamPaths::detect() {
        Ok(steam) => {
            let libs = library::libraries(&steam)?;
            let game = library::find_app(&libs, steam::RUNEHAVEN_APPID)?.map(|(lib, _)| lib.index);
            libs.into_iter()
                .map(|l| SteamLibrary {
                    index: l.index,
                    path: l.path.display().to_string(),
                    drive: library::drive_name(&l.path),
                    free_bytes: library::free_space(&l.path),
                    has_game: game == Some(l.index),
                })
                .collect()
        }
        Err(_) => Vec::new(),
    };
    let runtime = match (&config.runtime.wine, &config.runtime.dir) {
        (Some(wine), _) => Place { mode: "existing", path: wine.display().to_string() },
        (None, Some(dir)) => Place { mode: "folder", path: dir.display().to_string() },
        (None, None) => Place { mode: "default", path: runtime_dir(&config, &launcher).display().to_string() },
    };
    let prefix = Place {
        mode: match &config.prefix {
            None => "default",
            Some(p) if prefix::looks_like_prefix(p) && !p.starts_with(&launcher.root) => "existing",
            Some(_) => "folder",
        },
        path: prefix_path(&config, &launcher).display().to_string(),
    };
    Ok(Locations { libraries, chosen_library: config.steam_library, runtime, prefix })
}

/// Changes one place. `kind` is `library`, `runtime` or `prefix`; `mode` is
/// `default`, `folder` or `existing`; `value` is the library number or the
/// picked path. Takes effect for the next installation or game start.
#[tauri::command]
pub fn set_install_location(kind: String, mode: String, value: Option<String>) -> Result<String, String> {
    let run = || -> rhl_core::Result<String> {
        let (launcher, mut config) = load()?;
        let _lock = launch::RunLock::acquire(&launcher)?;
        let path = value.as_deref().map(PathBuf::from);
        let need_path = || path.clone().ok_or_else(|| Error::Config("no folder was chosen".into()));
        let note = match (kind.as_str(), mode.as_str()) {
            ("library", "default") => {
                config.steam_library = None;
                "Steam's default library is used.".to_string()
            }
            ("library", _) => {
                let index = value
                    .as_deref()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| Error::Config("no library was chosen".into()))?;
                config.steam_library = Some(index);
                "New installations go to this library.".to_string()
            }
            ("runtime", "default") => {
                config.runtime.wine = None;
                config.runtime.library_dirs.clear();
                config.runtime.dir = None;
                "The launcher's own Wine runtime is used.".to_string()
            }
            ("runtime", "folder") => {
                let dir = need_path()?;
                std::fs::create_dir_all(&dir).map_err(|e| Error::Config(e.to_string()))?;
                config.runtime.wine = None;
                config.runtime.library_dirs.clear();
                config.runtime.dir = Some(dir);
                "The Wine runtime is downloaded into this folder.".to_string()
            }
            ("runtime", "existing") => {
                let picked = need_path()?;
                let (wine, libs) = runtime::find_wine(&picked).ok_or_else(|| {
                    Error::Config(format!("no Wine program found in {}", picked.display()))
                })?;
                config.runtime.wine = Some(wine);
                config.runtime.library_dirs = libs;
                "Your own Wine is used. It needs DXMT for Runehaven and is not officially supported.".to_string()
            }
            ("prefix", "default") => {
                config.prefix = None;
                "The launcher's own Wine setup is used.".to_string()
            }
            ("prefix", "folder") => {
                config.prefix = Some(prefix::new_prefix_in(&need_path()?));
                "A new Wine setup is created there on the next start.".to_string()
            }
            ("prefix", "existing") => {
                let p = need_path()?;
                if !prefix::looks_like_prefix(&p) {
                    return Err(Error::Config(format!(
                        "{} is not a Wine setup (it has no drive_c and system.reg).",
                        p.display()
                    )));
                }
                config.prefix = Some(p);
                "This Wine setup is used; its save games stay where they are.".to_string()
            }
            _ => return Err(Error::Config(format!("unknown place {kind}/{mode}"))),
        };
        config.save(&launcher.config_file())?;
        Ok(note)
    };
    run().map_err(|e| e.to_string())
}

/// Shows macOS's own "Open" panel. `what` is `folder` or `wine` (a Wine app
/// or the `wine` program). Returns the picked path, or nothing if cancelled.
#[tauri::command]
pub async fn pick_path(app: AppHandle, what: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dialog = app.dialog().file();
        let picked = if what == "wine" {
            dialog.set_title("Choose a Wine app or the wine program").blocking_pick_file()
        } else {
            dialog.set_title("Choose a folder").blocking_pick_folder()
        };
        picked
            .map(|p| p.into_path().map(|p| p.display().to_string()).map_err(|e| e.to_string()))
            .transpose()
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Opens Steam's settings, where Storage adds a new library.
#[tauri::command]
pub fn open_steam_settings() -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg("steam://open/settings")
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Moves the installed game into another Steam library.
#[tauri::command]
pub fn move_game(app: AppHandle, library: u32) {
    spawn_job(app, "move", move |working| {
        working("Starting Steam…");
        let client = SteamClient::detect()?;
        client.wait_ready_until(&AtomicBool::new(false), &mut |_| {
            working("Waiting for Steam. Log in if it asks you to.")
        })?;
        let launcher = LauncherPaths::detect()?;
        let opts = InstallOptions { cancel: Arc::new(AtomicBool::new(false)), ..Default::default() };
        working("Steam is moving Runehaven…");
        steam::move_game(&client, &launcher, &opts, library, &mut |_| {})?;
        let (_, mut config) = load()?;
        config.steam_library = Some(library);
        config.save(&launcher.config_file())?;
        Ok("Runehaven was moved.".into())
    });
}
