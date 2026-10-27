//! Small persistent state, mainly so that a crash in the middle of an
//! installation never leaves Steam on the Windows platform.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LauncherState {
    /// Set before Steam's platform is switched, cleared after it is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_platform_restore: Option<PendingRestore>,
    /// The Mac helper the game last started with (see `launch::helper_stamp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_helper: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingRestore {
    /// The value to put back. Empty means Steam's default, which the launcher
    /// restores as `macos`.
    pub previous: String,
    pub since_unix: u64,
    pub reason: String,
}

impl PendingRestore {
    pub fn new(previous: String, reason: &str) -> Self {
        let since_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            previous,
            since_unix,
            reason: reason.into(),
        }
    }

    /// The value to restore; Steam's default on a Mac behaves like `macos`.
    pub fn restore_value(&self) -> &str {
        match self.previous.as_str() {
            "" => "macos",
            other => other,
        }
    }
}

impl LauncherState {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Parse {
                what: path.display().to_string(),
                detail: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).ctx(|| format!("reading {}", path.display())),
        }
    }

    /// Reads, changes and writes the state in one step. Others changing it at
    /// the same time, in this process or another, wait until it is done, so
    /// no change gets lost.
    pub fn update<R>(path: &Path, change: impl FnOnce(&mut Self) -> R) -> Result<R> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
        }
        let lock_path = Self::lock_file(path);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .ctx(|| format!("opening {}", lock_path.display()))?;
        // SAFETY: flock on a descriptor this function owns; it is released
        // when `lock` is closed at the end.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).ctx(|| format!("locking {}", lock_path.display()));
        }
        let mut state = Self::load(path)?;
        let result = change(&mut state);
        state.save(path)?;
        Ok(result)
    }

    /// The file [`Self::update`] locks.
    pub fn lock_file(path: &Path) -> PathBuf {
        path.with_extension("lock")
    }

    /// Writes atomically (temp file + rename) so a crash cannot leave half a file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).expect("state serializes");
        std::fs::write(&tmp, text).ctx(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).ctx(|| format!("replacing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub/state.json");
        assert_eq!(
            LauncherState::load(&path).unwrap(),
            LauncherState::default()
        );
        let state = LauncherState {
            pending_platform_restore: Some(PendingRestore::new(String::new(), "test")),
            ..Default::default()
        };
        state.save(&path).unwrap();
        let loaded = LauncherState::load(&path).unwrap();
        assert_eq!(loaded, state);
        assert_eq!(
            loaded.pending_platform_restore.unwrap().restore_value(),
            "macos"
        );
    }

    #[test]
    fn simultaneous_updates_all_arrive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    LauncherState::update(&path, |s| {
                        let mut seen = s.last_helper.clone().unwrap_or_default();
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        seen.push_str(&i.to_string());
                        s.last_helper = Some(seen);
                    })
                    .unwrap()
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(LauncherState::load(&path).unwrap().last_helper.unwrap().len(), 8);
    }
}
