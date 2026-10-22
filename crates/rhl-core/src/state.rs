//! Small persistent state, mainly so that a crash in the middle of an
//! installation never leaves Steam on the Windows platform.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LauncherState {
    /// Set before Steam's platform is switched, cleared after it is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_platform_restore: Option<PendingRestore>,
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
        };
        state.save(&path).unwrap();
        let loaded = LauncherState::load(&path).unwrap();
        assert_eq!(loaded, state);
        assert_eq!(
            loaded.pending_platform_restore.unwrap().restore_value(),
            "macos"
        );
    }
}
