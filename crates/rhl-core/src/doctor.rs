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
use crate::util::utc_stamp;

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
/// reports, the system checks and the launcher configuration.
pub fn export_logs(launcher: &LauncherPaths, dest_dir: &Path) -> Result<PathBuf> {
    let name = format!("Runehaven Launcher logs {}", utc_stamp());
    let staging = std::env::temp_dir().join(&name);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(staging.join("logs")).ctx(|| format!("creating {}", staging.display()))?;
    let copy = |from: &Path, to: &Path| -> Result<()> {
        let status = Command::new("/bin/cp")
            .arg("-R")
            .arg(from)
            .arg(to)
            .status()
            .ctx(|| format!("copying {}", from.display()))?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::Config(format!("copying {} failed", from.display())))
        }
    };

    let logs = launcher.logs();
    let starts = |prefix: &'static str| move |n: &str| n.starts_with(prefix) && n.ends_with(".log");
    for f in newest(&logs, starts("player-"), EXPORTED_STARTS)
        .into_iter()
        .chain(newest(&logs, starts("wine-"), EXPORTED_STARTS))
        .chain(newest(&logs, |n| n == "wineboot.log" || n == "wine-settings.log", 2))
    {
        copy(&f, &staging.join("logs"))?;
    }

    let config = Config::load(&launcher.config_file()).unwrap_or_default();
    let users = prefix_path(&config, launcher).join("drive_c/users");
    for user in fs::read_dir(&users).into_iter().flatten().flatten() {
        let crashes = user.path().join("Temp/Cikoria/Runehaven/Crashes");
        let latest = newest(&crashes, |n| n.starts_with("Crash_"), EXPORTED_CRASHES);
        if !latest.is_empty() {
            fs::create_dir_all(staging.join("crashes")).ctx(|| "creating crashes folder".into())?;
        }
        for dir in latest {
            copy(&dir, &staging.join("crashes"))?;
        }
    }

    fs::write(staging.join("system.txt"), report()).ctx(|| "writing system.txt".into())?;
    let config_text = serde_json::to_string_pretty(&config).expect("config serializes");
    fs::write(staging.join("config.json"), config_text).ctx(|| "writing config.json".into())?;

    fs::create_dir_all(dest_dir).ctx(|| format!("creating {}", dest_dir.display()))?;
    let zip = dest_dir.join(format!("{name}.zip"));
    let status = Command::new("/usr/bin/ditto")
        .args(["-c", "-k", "--norsrc", "--noextattr", "--noqtn", "--keepParent"])
        .arg(&staging)
        .arg(&zip)
        .status()
        .ctx(|| "running ditto".into())?;
    let _ = fs::remove_dir_all(&staging);
    if !status.success() {
        return Err(Error::Config(format!("packing {} failed", zip.display())));
    }
    Ok(zip)
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

        let zip = export_logs(&launcher, tmp.path()).unwrap();
        let list = Command::new("/usr/bin/unzip").arg("-l").arg(&zip).output().unwrap();
        let list = String::from_utf8_lossy(&list.stdout);
        assert_eq!(list.matches("player-").count(), EXPORTED_STARTS);
        assert!(list.contains("player-202610211.log"), "the newest start is in");
        assert!(!list.contains("player-202610200.log"), "the oldest is left out");
        assert!(list.contains("crashes/Crash_1/crash.dmp"));
        assert!(list.contains("system.txt") && list.contains("config.json"));
    }
}
