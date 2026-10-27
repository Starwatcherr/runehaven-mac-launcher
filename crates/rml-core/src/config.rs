//! User-adjustable settings, stored as JSON in the launcher folder.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};

/// Turns on Apple's Metal performance HUD.
const HUD_VAR: &str = "MTL_HUD_ENABLED";

/// Window widths and heights the settings accept, in points.
const SIZE_RANGE: std::ops::RangeInclusive<u32> = 320..=8192;

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
    /// Steam library (its number) to install the game into; Steam's default
    /// if `None`.
    pub steam_library: Option<u32>,
    /// Don't ask GitHub for new launcher versions.
    pub skip_update_check: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct RuntimeConfig {
    /// The `wine` (or `wine64`) binary.
    pub wine: Option<PathBuf>,
    /// Folders added to `DYLD_FALLBACK_LIBRARY_PATH` for Wine's helper libraries.
    pub library_dirs: Vec<PathBuf>,
    /// Folder the launcher downloads its runtime into; its own if `None`.
    pub dir: Option<PathBuf>,
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

    /// Whether Apple's Metal performance HUD (FPS, frame time, GPU memory) is on.
    pub fn hud(&self) -> bool {
        self.extra_env.contains_key(HUD_VAR)
    }

    /// Changes one setting by name, as the CLI and the app's settings page do.
    /// For paths, "default" means the launcher's own.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let bad = |what: &str| Error::Config(format!("invalid value for {key}: {what}"));
        let flag = |v: &str| v.parse::<bool>().map_err(|_| bad("true or false"));
        let opt_path = |v: &str| (v != "default").then(|| PathBuf::from(v));
        match key {
            "wine" => self.runtime.wine = opt_path(value),
            "library-dirs" => {
                self.runtime.library_dirs = value
                    .split(',')
                    .filter(|s| !s.is_empty() && *s != "default")
                    .map(PathBuf::from)
                    .collect()
            }
            "prefix" => self.prefix = opt_path(value),
            "runtime-dir" => self.runtime.dir = opt_path(value),
            "steam-library" => {
                self.steam_library = match value {
                    "default" => None,
                    n => Some(n.parse().map_err(|_| bad("a library number or default"))?),
                }
            }
            "inputfix" => self.inputfix_dll = opt_path(value),
            "macfix" => self.macfix_dylib = opt_path(value),
            "hud" => {
                if flag(value)? {
                    self.extra_env.insert(HUD_VAR.into(), "1".into());
                } else {
                    self.extra_env.remove(HUD_VAR);
                }
            }
            "update-check" => self.skip_update_check = !flag(value)?,
            "fullscreen" => self.display.fullscreen = flag(value)?,
            "retina" => self.display.retina = flag(value)?,
            "size" => {
                let size = || -> Option<(u32, u32)> {
                    let (w, h) = value.split_once('x')?;
                    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
                    (SIZE_RANGE.contains(&w) && SIZE_RANGE.contains(&h)).then_some((w, h))
                };
                let (w, h) = size().ok_or_else(|| {
                    bad(&format!("WIDTHxHEIGHT, each from {} to {}", SIZE_RANGE.start(), SIZE_RANGE.end()))
                })?;
                (self.display.width, self.display.height) = (w, h);
            }
            _ => return Err(Error::Config(format!("unknown setting `{key}`"))),
        }
        Ok(())
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

    #[test]
    fn settings_are_changed_by_name() {
        let mut c = Config::default();
        c.set("size", "1600x900").unwrap();
        c.set("hud", "true").unwrap();
        c.set("prefix", "/tmp/p").unwrap();
        assert_eq!((c.display.width, c.display.height), (1600, 900));
        assert!(c.hud());
        assert_eq!(c.prefix.as_deref(), Some(Path::new("/tmp/p")));
        c.set("hud", "false").unwrap();
        c.set("prefix", "default").unwrap();
        assert!(!c.hud() && c.prefix.is_none());
        assert!(c.set("retina", "maybe").is_err());
        assert!(c.set("size", "99999999x720").is_err());
        assert!(c.set("size", "0x0").is_err());
        assert!(c.set("colour", "red").is_err());
    }
}
