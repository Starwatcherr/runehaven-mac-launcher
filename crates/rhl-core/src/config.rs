//! User-adjustable settings, stored as JSON in the launcher folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    pub runtime: RuntimeConfig,
    pub display: DisplayConfig,
    /// The input-fix DLL copied next to `Runehaven.exe` before each start.
    pub inputfix_dll: Option<PathBuf>,
    /// The Mac helper library that switches the game into macOS full screen
    /// (used when `display.fullscreen` is on).
    pub macfix_dylib: Option<PathBuf>,
    /// Wine prefix; the launcher's own folder if `None`.
    pub prefix: Option<PathBuf>,
    /// Additional environment variables for Wine.
    pub extra_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct RuntimeConfig {
    /// The `wine` (or `wine64`) binary.
    pub wine: Option<PathBuf>,
    /// Folders added to `DYLD_FALLBACK_LIBRARY_PATH` for Wine's helper libraries.
    pub library_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DisplayConfig {
    pub fullscreen: bool,
    /// Window size in macOS points (what you see on screen); with `retina`
    /// the game renders twice as many pixels in each direction.
    pub width: u32,
    pub height: u32,
    /// Wine's Retina mode: the game renders in the display's full pixel
    /// resolution instead of being scaled up.
    pub retina: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            fullscreen: false,
            width: 1280,
            height: 720,
            retina: true,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).ctx(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self).expect("config serializes");
        std::fs::write(path, text + "\n").ctx(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_uses_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{ "display": { "fullscreen": true } }"#).unwrap();
        let c = Config::load(&path).unwrap();
        assert!(c.display.fullscreen);
        assert_eq!(c.display.width, 1280);
        assert!(c.display.retina);
        assert!(c.runtime.wine.is_none());
    }
}
