//! System checks shown before setup and copied into bug reports.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::launch::prefix_path;
use crate::paths::{LauncherPaths, SteamPaths};
use crate::steam::{library, RUNEHAVEN_APPID};
use crate::util::{free_path, utc_stamp};

/// How many of the latest game starts go into a log export.
const EXPORTED_STARTS: usize = 10;
/// How many of Unity's latest crash folders go into a log export.
const EXPORTED_CRASHES: usize = 3;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    /// `None` when the check could not be decided.
    pub ok: Option<bool>,
    pub detail: String,
}

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether Intel programs run, i.e. Rosetta 2 is installed.
pub fn rosetta_installed() -> bool {
    Command::new("/usr/bin/arch")
        .args(["-x86_64", "/usr/bin/true"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Installs Rosetta 2 through Apple's software update. macOS asks for an
/// administrator password in its own window; the launcher never sees it.
pub fn install_rosetta() -> Result<()> {
    let script = "do shell script \"/usr/sbin/softwareupdate --install-rosetta --agree-to-license\" \
                  with prompt \"Runehaven Mac Launcher wants to install Rosetta 2 from Apple. \
                  This accepts Apple's Rosetta license.\" with administrator privileges";
    let out = Command::new("/usr/bin/osascript")
        .args(["-e", script])
        .output()
        .map_err(|e| Error::Config(format!("could not ask macOS to install Rosetta 2: {e}")))?;
    if out.status.success() && rosetta_installed() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    // -128: the password window was closed with Cancel.
    if err.contains("(-128)") {
        return Err(Error::Interrupted);
    }
    Err(Error::Config(format!(
        "Rosetta 2 could not be installed: {}",
        err.trim()
    )))
}

/// Checks that need no running Steam.
pub fn run() -> Vec<Check> {
    let mut checks = Vec::new();

    let arch = output("/usr/bin/uname", &["-m"]).unwrap_or_default();
    checks.push(Check {
        name: "Apple Silicon",
        ok: Some(arch == "arm64"),
        detail: arch,
    });

    let version = output("/usr/bin/sw_vers", &["-productVersion"]);
    let major = version
        .as_deref()
        .and_then(|v| v.split('.').next()?.parse::<u32>().ok());
    checks.push(Check {
        name: "macOS version",
        ok: major.map(|m| m >= 14),
        detail: version.unwrap_or_else(|| "unknown".into()),
    });

    let rosetta = rosetta_installed();
    checks.push(Check {
        name: "Rosetta 2",
        ok: Some(rosetta),
        detail: if rosetta {
            "installed".into()
        } else {
            "missing: softwareupdate --install-rosetta".into()
        },
    });

    match SteamPaths::detect() {
        Err(e) => checks.push(Check {
            name: "Steam app",
            ok: Some(false),
            detail: e.to_string(),
        }),
        Ok(steam) => {
            checks.push(Check {
                name: "Steam app",
                ok: Some(true),
                detail: steam.app_bundle.display().to_string(),
            });
            let log = steam.console_log();
            checks.push(Check {
                name: "Steam console log",
                ok: Some(log.is_file()),
                detail: log.display().to_string(),
            });
            match library::libraries(&steam) {
                Err(e) => checks.push(Check {
                    name: "Steam libraries",
                    ok: Some(false),
                    detail: e.to_string(),
                }),
                Ok(libs) => {
                    let list: Vec<String> = libs
                        .iter()
                        .map(|l| format!("{}: {}", l.index, l.path.display()))
                        .collect();
                    checks.push(Check {
                        name: "Steam libraries",
                        ok: Some(true),
                        detail: list.join("; "),
                    });
                    let game = match library::find_app(&libs, RUNEHAVEN_APPID) {
                        Ok(Some((lib, m))) => Check {
                            name: "Runehaven",
                            ok: Some(m.is_ready()),
                            detail: format!(
                                "library {}, build {}, flags {}{}",
                                lib.index,
                                m.buildid,
                                m.state_flags,
                                if m.update_pending() {
                                    ", update pending"
                                } else {
                                    ""
                                }
                            ),
                        },
                        Ok(None) => Check {
                            name: "Runehaven",
                            ok: Some(false),
                            detail: "not installed".into(),
                        },
                        Err(e) => Check {
                            name: "Runehaven",
                            ok: None,
                            detail: e.to_string(),
                        },
                    };
                    checks.push(game);
                }
            }
        }
    }
    checks
}

/// The checks as plain text, one per line, for bug reports.
pub fn report() -> String {
    run()
        .iter()
        .map(|c| {
            let mark = match c.ok {
                Some(true) => "ok  ",
                Some(false) => "FAIL",
                None => "??  ",
            };
            format!("[{mark}] {:<18} {}\n", c.name, c.detail)
        })
        .collect()
}

/// The newest entries of `dir` whose names pass `keep`, newest first.
fn newest(dir: &Path, keep: impl Fn(&str) -> bool, count: usize) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(&keep))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().take(count).map(|(_, p)| p).collect()
}

/// Packs what a bug report needs into one ZIP file in `dest_dir` and returns
/// its path: the logs of the latest game starts, Unity's latest crash
/// reports (their memory dumps only with `crash_dumps`), the system checks
/// and the launcher configuration, plus a README saying what is inside. The
/// user's home folder in paths is shown as `~`, and the values of the user's
/// own environment variables are hidden.
pub fn export_logs(launcher: &LauncherPaths, dest_dir: &Path, crash_dumps: bool) -> Result<PathBuf> {
    let name = format!("Runehaven Mac Launcher logs {}", utc_stamp());
    let staging = std::env::temp_dir().join(format!("rml-export-{}", std::process::id())).join(&name);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(staging.join("logs")).ctx(|| format!("creating {}", staging.display()))?;
    let packed = stage_report(launcher, &staging, crash_dumps).and_then(|()| {
        fs::create_dir_all(dest_dir).ctx(|| format!("creating {}", dest_dir.display()))?;
        let zip = free_path(dest_dir, &name, "zip");
        let status = Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--norsrc", "--noextattr", "--noqtn", "--keepParent"])
            .arg(&staging)
            .arg(&zip)
            .status()
            .ctx(|| "running ditto".into())?;
        if !status.success() {
            let _ = fs::remove_file(&zip);
            return Err(Error::Config(format!("packing {} failed", zip.display())));
        }
        Ok(zip)
    });
    if let Some(parent) = staging.parent() {
        let _ = fs::remove_dir_all(parent);
    }
    packed
}

fn stage_report(launcher: &LauncherPaths, staging: &Path, crash_dumps: bool) -> Result<()> {
    let redact = Redactor::for_this_user();
    let logs = launcher.logs();
    let starts = |prefix: &'static str| move |n: &str| n.starts_with(prefix) && n.ends_with(".log");
    for f in newest(&logs, starts("player-"), EXPORTED_STARTS)
        .into_iter()
        .chain(newest(&logs, starts("wine-"), EXPORTED_STARTS))
        .chain(newest(&logs, |n| n == "wineboot.log" || n == "wine-settings.log", 2))
    {
        redact.copy_tree(&f, &staging.join("logs").join(f.file_name().unwrap_or_default()), true)?;
    }

    let config = Config::load(&launcher.config_file()).unwrap_or_default();
    let users = prefix_path(&config, launcher).join("drive_c/users");
    let mut crashes = 0;
    for user in fs::read_dir(&users).into_iter().flatten().flatten() {
        let dir = user.path().join("Temp/Cikoria/Runehaven/Crashes");
        for crash in newest(&dir, |n| n.starts_with("Crash_"), EXPORTED_CRASHES) {
            let to = staging.join("crashes").join(crash.file_name().unwrap_or_default());
            redact.copy_tree(&crash, &to, crash_dumps)?;
            crashes += 1;
        }
    }

    fs::write(staging.join("system.txt"), redact.text(&report())).ctx(|| "writing system.txt".into())?;
    let config_text = serde_json::to_string_pretty(&shareable(config)).expect("config serializes");
    fs::write(staging.join("config.json"), redact.text(&config_text)).ctx(|| "writing config.json".into())?;
    let dumps = match (crashes, crash_dumps) {
        (0, _) => "There were no crash reports.",
        (_, true) => "Unity's crash reports, with their memory dumps (crash.dmp).",
        (_, false) => "Unity's crash reports, without their memory dumps.",
    };
    let readme = format!(
        "Runehaven Mac Launcher: logs for a bug report\n\n\
         logs/         the game's log (player-...) and Wine's log (wine-...) of the latest starts\n\
         crashes/      {dumps}\n\
         system.txt    macOS, Rosetta 2, Steam and where the game is installed\n\
         config.json   the launcher's settings; values of your own environment variables are hidden\n\n\
         Your home folder in paths is shown as ~. Look through the files before you share them.\n"
    );
    fs::write(staging.join("README.txt"), readme).ctx(|| "writing README.txt".into())
}

/// The settings as they go into a bug report: environment variables the
/// launcher itself knows keep their values, others only their names.
fn shareable(mut config: Config) -> Config {
    const KNOWN: [&str; 5] = ["MTL_", "DXMT_", "WINE", "RHL_", "RML_"];
    for (key, value) in config.extra_env.iter_mut() {
        if !KNOWN.iter().any(|k| key.starts_with(k)) {
            *value = "(hidden)".into();
        }
    }
    config
}

/// Replaces the user's home folder in text with `~`, in Mac paths
/// (`/Users/name`) and Wine's Windows paths (`\Users\name`).
struct Redactor {
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Redactor {
    fn for_this_user() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self::for_home(home.trim_end_matches('/'))
    }

    fn for_home(home: &str) -> Self {
        let mut pairs = Vec::new();
        if home.len() > 1 {
            pairs.push((home.as_bytes().to_vec(), b"~".to_vec()));
            let windows = home.replace('/', "\\");
            pairs.push((windows.as_bytes().to_vec(), b"\\~".to_vec()));
            pairs.push((windows.replace('\\', "\\\\").into_bytes(), b"\\\\~".to_vec()));
        }
        Self { pairs }
    }

    fn bytes(&self, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        for (from, to) in &self.pairs {
            let mut result = Vec::with_capacity(out.len());
            let mut rest = out.as_slice();
            while let Some(at) = rest.windows(from.len()).position(|w| w == from.as_slice()) {
                result.extend_from_slice(&rest[..at]);
                result.extend_from_slice(to);
                rest = &rest[at + from.len()..];
            }
            result.extend_from_slice(rest);
            out = result;
        }
        out
    }

    fn text(&self, text: &str) -> Vec<u8> {
        self.bytes(text.as_bytes())
    }

    /// Copies a file or folder with the home folder replaced in every file.
    /// Memory dumps (`.dmp`) are copied unchanged with `dumps`, else left out.
    fn copy_tree(&self, from: &Path, to: &Path, dumps: bool) -> Result<()> {
        let meta = fs::symlink_metadata(from).ctx(|| format!("reading {}", from.display()))?;
        if meta.is_dir() {
            fs::create_dir_all(to).ctx(|| format!("creating {}", to.display()))?;
            for entry in fs::read_dir(from).ctx(|| format!("reading {}", from.display()))?.flatten() {
                self.copy_tree(&entry.path(), &to.join(entry.file_name()), dumps)?;
            }
        } else if meta.is_file() {
            let is_dump = from.extension().is_some_and(|e| e.eq_ignore_ascii_case("dmp"));
            if is_dump && !dumps {
                return Ok(());
            }
            if let Some(dir) = to.parent() {
                fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
            }
            let data = fs::read(from).ctx(|| format!("reading {}", from.display()))?;
            let data = if is_dump { data } else { self.bytes(&data) };
            fs::write(to, data).ctx(|| format!("writing {}", to.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_export_takes_the_latest_starts_and_crashes() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let logs = launcher.logs();
        fs::create_dir_all(&logs).unwrap();
        for i in 0..12 {
            fs::write(logs.join(format!("player-2026102{i:02}.log")), "x").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let crash = launcher
            .prefix()
            .join("drive_c/users/crossover/Temp/Cikoria/Runehaven/Crashes/Crash_1");
        fs::create_dir_all(&crash).unwrap();
        fs::write(crash.join("crash.dmp"), "dump").unwrap();

        fs::write(crash.join("error.log"), "error").unwrap();

        let zip = export_logs(&launcher, tmp.path(), false).unwrap();
        let list = Command::new("/usr/bin/unzip").arg("-l").arg(&zip).output().unwrap();
        let list = String::from_utf8_lossy(&list.stdout);
        assert_eq!(list.matches("player-").count(), EXPORTED_STARTS);
        assert!(list.contains("player-202610211.log"), "the newest start is in");
        assert!(!list.contains("player-202610200.log"), "the oldest is left out");
        assert!(list.contains("crashes/Crash_1/error.log"));
        assert!(!list.contains("crash.dmp"), "no memory dump unless asked for");
        assert!(list.contains("system.txt") && list.contains("config.json") && list.contains("README.txt"));

        let zip = export_logs(&launcher, tmp.path(), true).unwrap();
        let list = Command::new("/usr/bin/unzip").arg("-l").arg(&zip).output().unwrap();
        assert!(String::from_utf8_lossy(&list.stdout).contains("crashes/Crash_1/crash.dmp"));
    }

    #[test]
    fn home_folder_and_own_values_are_hidden() {
        let r = Redactor::for_home("/Users/kian");
        let log = "open /Users/kian/Library/x and Z:\\Users\\kian\\Library and \"Z:\\\\Users\\\\kian\"";
        assert_eq!(
            String::from_utf8(r.text(log)).unwrap(),
            "open ~/Library/x and Z:\\~\\Library and \"Z:\\\\~\""
        );
        let mut config = Config::default();
        config.extra_env.insert("MTL_HUD_ENABLED".into(), "1".into());
        config.extra_env.insert("MY_TOKEN".into(), "secret".into());
        let shared = shareable(config);
        assert_eq!(shared.extra_env["MTL_HUD_ENABLED"], "1");
        assert_eq!(shared.extra_env["MY_TOKEN"], "(hidden)");
    }
}
