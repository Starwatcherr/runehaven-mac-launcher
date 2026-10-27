//! The settings page: display options, the performance overlay and the logs.

use std::path::PathBuf;

use rml_core::config::Config;
use rml_core::doctor;
use rml_core::paths::LauncherPaths;
use rml_core::update::{self, NewVersion};
use serde::Serialize;

/// What the settings page shows.
#[derive(Debug, Serialize)]
pub struct Settings {
    fullscreen: bool,
    retina: bool,
    width: u32,
    height: u32,
    hud: bool,
    #[serde(rename = "update-check")]
    update_check: bool,
}

/// The settings the page may change; paths are set elsewhere.
const PAGE_KEYS: [&str; 5] = ["fullscreen", "retina", "size", "hud", "update-check"];

fn load() -> rml_core::Result<(LauncherPaths, Config)> {
    let launcher = LauncherPaths::detect()?;
    let config = Config::load(&launcher.config_file())?;
    Ok((launcher, config))
}

#[tauri::command]
pub fn get_settings() -> Result<Settings, String> {
    let (_, c) = load().map_err(|e| e.to_string())?;
    Ok(Settings {
        fullscreen: c.display.fullscreen,
        retina: c.display.retina,
        width: c.display.width,
        height: c.display.height,
        hud: c.hud(),
        update_check: !c.skip_update_check,
    })
}

/// A newer launcher on GitHub, if the player has not turned the check off.
/// Network problems count as "nothing new"; the launcher works offline.
#[tauri::command]
pub async fn launcher_update(app: tauri::AppHandle) -> Option<NewVersion> {
    let current = app.package_info().version.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let (_, config) = load().ok()?;
        if config.skip_update_check {
            return None;
        }
        update::check(&current).ok().flatten()
    })
    .await
    .ok()
    .flatten()
}

/// Changes one setting; it applies the next time the game starts.
#[tauri::command]
pub fn set_setting(key: String, value: String) -> Result<(), String> {
    if !PAGE_KEYS.contains(&key.as_str()) {
        return Err(format!("unknown setting `{key}`"));
    }
    let run = || {
        let (launcher, mut config) = load()?;
        config.set(&key, &value)?;
        config.save(&launcher.config_file())
    };
    run().map_err(|e| e.to_string())
}

/// Shows the logs folder in Finder.
#[tauri::command]
pub fn open_logs() -> Result<(), String> {
    let logs = LauncherPaths::detect().map_err(|e| e.to_string())?.logs();
    std::fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    std::process::Command::new("/usr/bin/open")
        .arg(&logs)
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Packs the logs for a bug report onto the desktop and shows the file in
/// Finder. Returns its path.
#[tauri::command]
pub async fn export_logs() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let launcher = LauncherPaths::detect().map_err(|e| e.to_string())?;
        let desktop = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Desktop"))
            .ok_or("HOME is not set")?;
        let zip = doctor::export_logs(&launcher, &desktop).map_err(|e| e.to_string())?;
        let _ = std::process::Command::new("/usr/bin/open").arg("-R").arg(&zip).status();
        Ok(zip.display().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Opens one of the launcher's known web pages in the browser.
#[tauri::command]
pub fn open_link(which: String) -> Result<(), String> {
    let url = match which.as_str() {
        "steam-store" => "https://store.steampowered.com/app/2676030/Runehaven/",
        "releases" => update::RELEASES_PAGE,
        _ => return Err(format!("unknown link `{which}`")),
    };
    std::process::Command::new("/usr/bin/open")
        .arg(url)
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
