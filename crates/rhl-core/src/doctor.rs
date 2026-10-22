//! System checks shown before setup and copied into bug reports.

use std::process::Command;

use serde::Serialize;

use crate::paths::SteamPaths;
use crate::steam::{library, RUNEHAVEN_APPID};

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

    let rosetta = Command::new("/usr/bin/arch")
        .args(["-x86_64", "/usr/bin/true"])
        .status()
        .is_ok_and(|s| s.success());
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
