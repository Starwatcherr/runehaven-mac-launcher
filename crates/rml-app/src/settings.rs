//! The settings page: display options, the performance overlay and the logs.

use std::path::PathBuf;

use rhl_core::config::Config;
use rhl_core::doctor;
use rhl_core::paths::LauncherPaths;
use serde::Serialize;

/// What the settings page shows.
#[derive(Debug, Serialize)]
pub struct Settings {
    fullscreen: bool,
    retina: bool,
    width: u32,
    height: u32,
    hud: bool,
}

/// The settings the page may change; paths are set elsewhere.
const PAGE_KEYS: [&str; 4] = ["fullscreen", "retina", "size", "hud"];

fn load() -> rhl_core::Result<(LauncherPaths, Config)> {
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
    })
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
        _ => return Err(format!("unknown link `{which}`")),
    };
    std::process::Command::new("/usr/bin/open")
        .arg(url)
        .status()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
