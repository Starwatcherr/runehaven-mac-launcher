//! Steam libraries and app manifests (`appmanifest_<appid>.acf`).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, IoContext, Result};
use crate::paths::SteamPaths;
use crate::vdf;

/// Bits of Steam's `StateFlags` field that matter here.
pub mod flags {
    pub const UPDATE_REQUIRED: u64 = 2;
    pub const FULLY_INSTALLED: u64 = 4;
    pub const FILES_MISSING: u64 = 32;
    pub const APP_RUNNING: u64 = 64;
    pub const FILES_CORRUPT: u64 = 128;
    pub const UPDATE_RUNNING: u64 = 256;
    pub const UPDATE_PAUSED: u64 = 512;
    pub const UPDATE_STARTED: u64 = 1024;
    pub const UNINSTALLING: u64 = 2048;
    pub const RECONFIGURING: u64 = 65_536;
    pub const VALIDATING: u64 = 131_072;
    pub const ADDING_FILES: u64 = 262_144;
    pub const PREALLOCATING: u64 = 524_288;
    pub const DOWNLOADING: u64 = 1_048_576;
    pub const STAGING: u64 = 2_097_152;
    pub const COMMITTING: u64 = 4_194_304;

    /// Steam is working on the app right now.
    pub const ACTIVE: u64 = UPDATE_RUNNING
        | UNINSTALLING
        | RECONFIGURING
        | VALIDATING
        | ADDING_FILES
        | PREALLOCATING
        | DOWNLOADING
        | STAGING
        | COMMITTING;

    /// The app is not in a finished state: active, or an update was started
    /// and is paused or waiting outside the queue.
    pub const BUSY: u64 = ACTIVE | UPDATE_STARTED | UPDATE_PAUSED;

    /// An update is waiting (not necessarily queued).
    pub const PENDING: u64 = UPDATE_REQUIRED | UPDATE_STARTED | UPDATE_PAUSED;
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Library {
    /// The key in `libraryfolders.vdf`; this is what `app_install` calls the
    /// volume index.
    pub index: u32,
    pub path: PathBuf,
}

impl Library {
    pub fn steamapps(&self) -> PathBuf {
        self.path.join("steamapps")
    }
    pub fn manifest_path(&self, appid: u32) -> PathBuf {
        self.steamapps().join(format!("appmanifest_{appid}.acf"))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InstalledDepot {
    pub depot: u32,
    pub manifest: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AppManifest {
    pub appid: u32,
    pub name: String,
    pub state_flags: u64,
    pub installdir: String,
    pub buildid: u64,
    pub target_buildid: u64,
    pub update_result: u64,
    pub bytes_to_download: u64,
    pub bytes_downloaded: u64,
    pub bytes_to_stage: u64,
    pub bytes_staged: u64,
    pub depots: Vec<InstalledDepot>,
}

impl AppManifest {
    pub fn parse(text: &str) -> Result<Self> {
        let doc = vdf::parse(text)?;
        let app = doc.get("AppState").ok_or_else(|| Error::Parse {
            what: "app manifest".into(),
            detail: "no AppState block".into(),
        })?;
        let num = |k: &str| app.u64(k).unwrap_or(0);
        let appid = app.u64("appid").ok_or_else(|| Error::Parse {
            what: "app manifest".into(),
            detail: "no appid".into(),
        })? as u32;
        let depots = app
            .get("InstalledDepots")
            .map(|d| {
                d.entries()
                    .iter()
                    .filter_map(|(id, v)| {
                        Some(InstalledDepot {
                            depot: id.parse().ok()?,
                            manifest: v.str("manifest")?.to_string(),
                            size: v.u64("size").unwrap_or(0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            appid,
            name: app.str("name").unwrap_or_default().to_string(),
            state_flags: num("StateFlags"),
            installdir: app.str("installdir").unwrap_or_default().to_string(),
            buildid: num("buildid"),
            target_buildid: num("TargetBuildID"),
            update_result: num("UpdateResult"),
            bytes_to_download: num("BytesToDownload"),
            bytes_downloaded: num("BytesDownloaded"),
            bytes_to_stage: num("BytesToStage"),
            bytes_staged: num("BytesStaged"),
            depots,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).ctx(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// The game's folder in `lib`. Steam names it with a single folder name;
    /// anything else (empty, a path, `..`) is refused rather than followed.
    pub fn game_dir(&self, lib: &Library) -> Result<PathBuf> {
        let name = Path::new(&self.installdir);
        let mut parts = name.components();
        match (parts.next(), parts.next()) {
            (Some(std::path::Component::Normal(_)), None) if !self.installdir.contains('/') => {
                Ok(lib.steamapps().join("common").join(name))
            }
            _ => Err(Error::Parse {
                what: format!("Steam's manifest for app {}", self.appid),
                detail: format!("unusable game folder name {:?}", self.installdir),
            }),
        }
    }

    fn has(&self, flag: u64) -> bool {
        self.state_flags & flag != 0
    }

    pub fn is_busy(&self) -> bool {
        self.has(flags::BUSY)
    }

    /// Steam is downloading, validating or otherwise working on it right now.
    pub fn is_active(&self) -> bool {
        self.has(flags::ACTIVE)
    }

    /// Steam has planned the job: sizes are known or work has begun.
    pub fn job_planned(&self) -> bool {
        self.bytes_to_download > 0 || self.bytes_to_stage > 0 || self.is_active() || self.is_ready()
    }

    /// Installed, up to date and nothing running on it.
    pub fn is_ready(&self) -> bool {
        self.has(flags::FULLY_INSTALLED)
            && !self.has(flags::UPDATE_REQUIRED)
            && !self.is_busy()
            && (self.target_buildid == 0 || self.buildid == self.target_buildid)
    }

    pub fn update_pending(&self) -> bool {
        self.has(flags::UPDATE_REQUIRED)
            || (self.target_buildid != 0 && self.buildid != self.target_buildid)
    }

    /// Download progress between 0 and 1, if Steam reports sizes.
    pub fn progress(&self) -> Option<f64> {
        let total = self.bytes_to_download + self.bytes_to_stage;
        (total > 0).then(|| (self.bytes_downloaded + self.bytes_staged) as f64 / total as f64)
    }
}

/// Free space on the drive holding `path`, in bytes.
pub fn free_space(path: &Path) -> Option<u64> {
    let out = std::process::Command::new("/bin/df").arg("-k").arg(path).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let kb: u64 = text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb * 1024)
}

/// A short name for the drive holding `path`: the volume name for an
/// external drive, "This Mac" for the internal one.
pub fn drive_name(path: &Path) -> String {
    let mut parts = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned());
    match (parts.nth(1).as_deref(), parts.next()) {
        (Some("Volumes"), Some(name)) => name,
        _ => "This Mac".to_string(),
    }
}

/// Reads all Steam libraries. The main Steam folder is always included as
/// library 0, even if `libraryfolders.vdf` cannot be read.
pub fn libraries(steam: &SteamPaths) -> Result<Vec<Library>> {
    let mut libs = Vec::new();
    for file in steam.library_folders_files() {
        if let Ok(text) = std::fs::read_to_string(&file) {
            libs = parse_library_folders(&text)?;
            if !libs.is_empty() {
                break;
            }
        }
    }
    if !libs.iter().any(|l| l.path == steam.data_dir) {
        libs.insert(
            0,
            Library {
                index: 0,
                path: steam.data_dir.clone(),
            },
        );
    }
    Ok(libs)
}

pub fn parse_library_folders(text: &str) -> Result<Vec<Library>> {
    let doc = vdf::parse(text)?;
    let root = doc
        .get("libraryfolders")
        .or_else(|| doc.get("LibraryFolders"))
        .ok_or_else(|| Error::Parse {
            what: "libraryfolders.vdf".into(),
            detail: "no libraryfolders block".into(),
        })?;
    let mut libs: Vec<Library> = root
        .entries()
        .iter()
        .filter_map(|(key, value)| {
            let index = key.parse().ok()?;
            let path = match value {
                vdf::Value::Obj(_) => value.str("path")?.to_string(),
                vdf::Value::Str(s) => s.clone(), // very old format: "1" "/path"
            };
            Some(Library {
                index,
                path: PathBuf::from(path),
            })
        })
        .collect();
    libs.sort_by_key(|l| l.index);
    Ok(libs)
}

/// Finds the manifest of `appid` in any library.
pub fn find_app(libs: &[Library], appid: u32) -> Result<Option<(Library, AppManifest)>> {
    for lib in libs {
        let path = lib.manifest_path(appid);
        if path.is_file() {
            return Ok(Some((lib.clone(), AppManifest::load(&path)?)));
        }
    }
    Ok(None)
}

/// Other apps Steam is downloading, updating or validating right now.
pub fn active_apps(libs: &[Library], except: u32) -> Vec<AppManifest> {
    other_apps(libs, except, AppManifest::is_active)
}

/// Other apps with an update waiting (queued or not).
pub fn pending_apps(libs: &[Library], except: u32) -> Vec<AppManifest> {
    other_apps(libs, except, |m| m.state_flags & flags::PENDING != 0)
}

fn other_apps(
    libs: &[Library],
    except: u32,
    keep: impl Fn(&AppManifest) -> bool,
) -> Vec<AppManifest> {
    let mut busy = Vec::new();
    for lib in libs {
        let Ok(dir) = std::fs::read_dir(lib.steamapps()) else {
            continue;
        };
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("appmanifest_") && name.ends_with(".acf")) {
                continue;
            }
            if let Ok(m) = AppManifest::load(&entry.path()) {
                if m.appid != except && keep(&m) {
                    busy.push(m);
                }
            }
        }
    }
    busy
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const RUNEHAVEN_MANIFEST: &str = r#""AppState"
{
	"appid"		"2676030"
	"Universe"		"1"
	"name"		"Runehaven"
	"StateFlags"		"4"
	"installdir"		"Runehaven"
	"lastupdated"		"1792073782"
	"SizeOnDisk"		"345226541"
	"buildid"		"23702508"
	"UpdateResult"		"0"
	"BytesToDownload"		"0"
	"BytesDownloaded"		"0"
	"BytesToStage"		"0"
	"BytesStaged"		"0"
	"TargetBuildID"		"23702508"
	"InstalledDepots"
	{
		"2676031"
		{
			"manifest"		"7388440364521527929"
			"size"		"345226541"
		}
	}
	"UserConfig"
	{
	}
	"MountedConfig"
	{
	}
}
"#;

    #[test]
    fn manifest_fields() {
        let m = AppManifest::parse(RUNEHAVEN_MANIFEST).unwrap();
        assert_eq!(m.appid, 2676030);
        assert_eq!(m.buildid, 23702508);
        assert!(m.is_ready());
        assert!(!m.update_pending());
        assert_eq!(m.depots[0].manifest, "7388440364521527929");
    }

    #[test]
    fn game_folder_must_be_one_plain_name() {
        let lib = Library { index: 0, path: "/steam".into() };
        let mut m = AppManifest::parse(RUNEHAVEN_MANIFEST).unwrap();
        assert_eq!(m.game_dir(&lib).unwrap(), Path::new("/steam/steamapps/common/Runehaven"));
        for bad in ["", ".", "..", "../Other", "/Users/x", "a/b", "Runehaven/"] {
            m.installdir = bad.into();
            assert!(m.game_dir(&lib).is_err(), "{bad:?} is refused");
        }
    }

    #[test]
    fn manifest_update_states() {
        let pending =
            RUNEHAVEN_MANIFEST.replace("\"StateFlags\"\t\t\"4\"", "\"StateFlags\"\t\t\"6\"");
        let m = AppManifest::parse(&pending).unwrap();
        assert!(m.update_pending());
        assert!(!m.is_ready());

        let downloading = RUNEHAVEN_MANIFEST
            .replace("\"StateFlags\"\t\t\"4\"", "\"StateFlags\"\t\t\"1026\"")
            .replace(
                "\"BytesToDownload\"\t\t\"0\"",
                "\"BytesToDownload\"\t\t\"200\"",
            )
            .replace(
                "\"BytesDownloaded\"\t\t\"0\"",
                "\"BytesDownloaded\"\t\t\"50\"",
            );
        let m = AppManifest::parse(&downloading).unwrap();
        assert!(m.is_busy());
        assert_eq!(m.progress(), Some(0.25));
    }

    #[test]
    fn library_folders() {
        let text = r#""libraryfolders"
{
	"0"
	{
		"path"		"/Users/k/Library/Application Support/Steam"
		"apps" { "620" "1" }
	}
	"1"
	{
		"path"		"/Users/k/games/SteamLibrary"
		"label"		""
		"contentid"		"3035990792676168867"
	}
}"#;
        let libs = parse_library_folders(text).unwrap();
        assert_eq!(libs.len(), 2);
        assert_eq!(
            libs[1],
            Library {
                index: 1,
                path: "/Users/k/games/SteamLibrary".into()
            }
        );
    }

    #[test]
    fn finds_app_and_other_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let lib0 = Library {
            index: 0,
            path: tmp.path().join("main"),
        };
        let lib1 = Library {
            index: 1,
            path: tmp.path().join("extra"),
        };
        std::fs::create_dir_all(lib0.steamapps()).unwrap();
        std::fs::create_dir_all(lib1.steamapps()).unwrap();
        std::fs::write(lib1.manifest_path(2676030), RUNEHAVEN_MANIFEST).unwrap();
        let other = RUNEHAVEN_MANIFEST
            .replace("2676030", "620")
            .replace("\"StateFlags\"\t\t\"4\"", "\"StateFlags\"\t\t\"1026\"");
        std::fs::write(lib0.manifest_path(620), other).unwrap();

        let libs = vec![lib0.clone(), lib1.clone()];
        let (lib, m) = find_app(&libs, 2676030).unwrap().unwrap();
        assert_eq!(lib, lib1);
        assert_eq!(m.name, "Runehaven");

        // 1026 = update required + started, but not running: waiting outside
        // the queue (as Stellaris was on Kian's Mac). Not active.
        assert!(active_apps(&libs, 2676030).is_empty());
        assert_eq!(pending_apps(&libs, 2676030)[0].appid, 620);

        let downloading = RUNEHAVEN_MANIFEST
            .replace("2676030", "620")
            .replace("\"StateFlags\"\t\t\"4\"", "\"StateFlags\"\t\t\"1049862\"");
        std::fs::write(lib0.manifest_path(620), downloading).unwrap();
        assert_eq!(active_apps(&libs, 2676030)[0].appid, 620);
    }
}
