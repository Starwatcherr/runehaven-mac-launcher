//! Where things live on disk.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const APP_DIR_NAME: &str = "Runehaven Launcher";

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Config("HOME is not set".into()))
}

/// Locations belonging to the user's Steam installation.
#[derive(Debug, Clone)]
pub struct SteamPaths {
    /// The bootstrapper binary that forwards `+commands` to a running client.
    pub bootstrapper: PathBuf,
    /// The `.app` bundle, used to start Steam the normal way.
    pub app_bundle: PathBuf,
    /// `~/Library/Application Support/Steam`
    pub data_dir: PathBuf,
}

impl SteamPaths {
    /// Finds Steam in `/Applications` or `~/Applications`. The data directory is
    /// fixed on macOS.
    pub fn detect() -> Result<Self> {
        let home = home()?;
        let candidates = [
            PathBuf::from("/Applications/Steam.app"),
            home.join("Applications/Steam.app"),
        ];
        let app_bundle = candidates
            .iter()
            .find(|p| p.join("Contents/MacOS/steam_osx").is_file())
            .cloned()
            .ok_or_else(|| Error::SteamNotInstalled(candidates[0].clone()))?;
        Ok(Self::from_parts(
            app_bundle,
            home.join("Library/Application Support/Steam"),
        ))
    }

    pub fn from_parts(app_bundle: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            bootstrapper: app_bundle.join("Contents/MacOS/steam_osx"),
            app_bundle,
            data_dir,
        }
    }

    pub fn console_log(&self) -> PathBuf {
        self.data_dir.join("logs/console_log.txt")
    }

    /// Steam has kept `libraryfolders.vdf` in two places over the years.
    pub fn library_folders_files(&self) -> [PathBuf; 2] {
        [
            self.data_dir.join("steamapps/libraryfolders.vdf"),
            self.data_dir.join("config/libraryfolders.vdf"),
        ]
    }
}

/// The launcher's own folder: `~/Library/Application Support/Runehaven Launcher`.
#[derive(Debug, Clone)]
pub struct LauncherPaths {
    pub root: PathBuf,
}

impl LauncherPaths {
    pub fn detect() -> Result<Self> {
        Ok(Self {
            root: home()?
                .join("Library/Application Support")
                .join(APP_DIR_NAME),
        })
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn runtime(&self) -> PathBuf {
        self.root.join("runtime")
    }
    pub fn prefix(&self) -> PathBuf {
        self.root.join("prefix")
    }
}

/// Converts a macOS path into the `Z:\…` form Wine programs understand.
pub fn to_wine_path(path: &Path) -> String {
    format!("Z:{}", path.to_string_lossy().replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wine_path() {
        assert_eq!(
            to_wine_path(Path::new("/Users/k/a b/log.txt")),
            "Z:\\Users\\k\\a b\\log.txt"
        );
    }
}
