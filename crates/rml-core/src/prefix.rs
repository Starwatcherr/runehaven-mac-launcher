//! The Wine prefix: Runehaven's private "Windows" with registry, user folder
//! and save games.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::error::{Error, IoContext, Result};
use crate::util::utc_stamp;

/// Where Unity keeps Runehaven's data inside the user's AppData.
const SAVE_SUBDIR: &str = "AppData/LocalLow/Cikoria/Runehaven";

pub fn is_initialized(prefix: &Path) -> bool {
    prefix.join("system.reg").is_file() && prefix.join("drive_c").is_dir()
}

/// Creates the prefix with `wineboot --init` and waits until Wine is done.
/// Mono and Gecko installers are suppressed; the game needs neither.
pub fn init(
    wine: &Path,
    library_path: &str,
    prefix: &Path,
    log: &Path,
    timeout: Duration,
) -> Result<()> {
    if is_initialized(prefix) {
        return Ok(());
    }
    fs::create_dir_all(prefix).ctx(|| format!("creating {}", prefix.display()))?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
    }
    let log_file = fs::File::create(log).ctx(|| format!("creating {}", log.display()))?;
    let log_err = log_file
        .try_clone()
        .ctx(|| "duplicating log handle".into())?;
    let envs = [
        ("WINEPREFIX", prefix.to_string_lossy().into_owned()),
        ("DYLD_FALLBACK_LIBRARY_PATH", library_path.to_string()),
        ("WINEDEBUG", "-all,err+all".to_string()),
        (
            "WINEDLLOVERRIDES",
            "mscoree,mshtml=;winemenubuilder.exe=d".to_string(),
        ),
    ];
    let mut child = Command::new(wine)
        .args(["wineboot", "--init"])
        .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        .spawn()
        .ctx(|| format!("starting {}", wine.display()))?;

    let started = Instant::now();
    loop {
        if child
            .try_wait()
            .ctx(|| "waiting for wineboot".into())?
            .is_some()
        {
            break;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            return Err(Error::Config(format!(
                "setting up the Wine prefix took longer than {}s (log: {})",
                timeout.as_secs(),
                log.display()
            )));
        }
        sleep(Duration::from_millis(250));
    }
    // wineboot returns before the wineserver has written everything; wait for it.
    let wineserver = wine.with_file_name("wineserver");
    if wineserver.is_file() {
        let _ = Command::new(&wineserver)
            .arg("-w")
            .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
            .status();
    }
    if is_initialized(prefix) {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "Wine did not create the prefix (log: {})",
            log.display()
        )))
    }
}

/// Runehaven's data folder in a prefix (save games and settings). WineCX
/// builds name the Windows user "crossover"; other builds use the Mac user
/// name, so every user folder is checked.
pub fn saves_dir(prefix: &Path) -> Option<PathBuf> {
    let users = prefix.join("drive_c/users");
    let mut candidates: Vec<PathBuf> = fs::read_dir(&users)
        .ok()?
        .flatten()
        .filter(|e| e.file_name() != "Public")
        .map(|e| e.path().join(SAVE_SUBDIR))
        .collect();
    candidates.sort();
    candidates
        .iter()
        .find(|p| p.is_dir())
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

/// Copies save games from another prefix (e.g. an earlier manual setup).
/// An existing folder in the target is kept as a dated backup.
pub fn import_saves(from_prefix: &Path, to_prefix: &Path) -> Result<PathBuf> {
    let from = saves_dir(from_prefix)
        .filter(|p| p.is_dir())
        .ok_or_else(|| {
            Error::Config(format!(
                "no Runehaven saves found in {}",
                from_prefix.display()
            ))
        })?;
    let to = saves_dir(to_prefix).ok_or_else(|| {
        Error::Config(format!(
            "{} is not an initialized Wine prefix",
            to_prefix.display()
        ))
    })?;
    if to.exists() {
        let backup = to.with_file_name(format!("Runehaven.backup-{}", utc_stamp()));
        fs::rename(&to, &backup).ctx(|| format!("backing up {}", to.display()))?;
    }
    copy_dir(&from, &to)?;
    Ok(to)
}

/// Key of Wine's macOS driver settings below HKEY_CURRENT_USER.
const MAC_DRIVER_KEY: &str = r"Software\Wine\Mac Driver";

/// Reads a string value from a prefix's `user.reg` (Wine registry format).
fn user_reg_string(prefix: &Path, key: &str, name: &str) -> Option<String> {
    let text = fs::read_to_string(prefix.join("user.reg")).ok()?;
    let header = format!("[{}]", key.replace('\\', "\\\\"));
    let wanted = format!("\"{name}\"=\"");
    let mut in_key = false;
    for line in text.lines() {
        if line.starts_with('[') {
            in_key = line.starts_with(&header);
        } else if in_key {
            if let Some(rest) = line.strip_prefix(&wanted) {
                return rest.strip_suffix('"').map(str::to_string);
            }
        }
    }
    None
}

/// Switches Wine's Retina mode on or off. Only starts Wine when the setting
/// actually changes; returns whether it did.
pub fn ensure_retina(
    wine: &Path,
    library_path: &str,
    prefix: &Path,
    on: bool,
    log: &Path,
) -> Result<bool> {
    let wanted = if on { "y" } else { "n" };
    let current = user_reg_string(prefix, MAC_DRIVER_KEY, "RetinaMode");
    if current.as_deref() == Some(wanted) || (!on && current.is_none()) {
        return Ok(false);
    }
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
    }
    let cmd = crate::settings::WineCmd {
        wine,
        library_path,
        prefix,
        log,
    };
    let key = format!("HKEY_CURRENT_USER\\{MAC_DRIVER_KEY}");
    let ok = cmd.run(&["reg", "add", &key, "/v", "RetinaMode", "/t", "REG_SZ", "/d", wanted, "/f"])?;
    cmd.wait_for_server();
    if !ok {
        return Err(Error::Config(format!(
            "Wine could not change the Retina setting (log: {})",
            log.display()
        )));
    }
    Ok(true)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).ctx(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from).ctx(|| format!("reading {}", from.display()))? {
        let entry = entry.ctx(|| format!("reading {}", from.display()))?;
        let target = to.join(entry.file_name());
        if entry
            .file_type()
            .ctx(|| "reading file type".into())?
            .is_dir()
        {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)
                .ctx(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Whether `path` is a Wine prefix (for example a Whisky or CrossOver bottle).
pub fn looks_like_prefix(path: &Path) -> bool {
    path.join("drive_c").is_dir() && path.join("system.reg").is_file()
}

/// Where a new prefix goes when the user picks `folder`: the folder itself
/// if it is empty, otherwise a new folder inside it.
pub fn new_prefix_in(folder: &Path) -> PathBuf {
    let empty = fs::read_dir(folder)
        .map(|mut e| e.all(|f| f.is_ok_and(|f| f.file_name() == ".DS_Store")))
        .unwrap_or(true);
    if empty {
        folder.to_path_buf()
    } else {
        folder.join("Runehaven Wine")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_prefix(root: &Path, user: &str, save: Option<&str>) -> PathBuf {
        let p = root.to_path_buf();
        fs::create_dir_all(p.join("drive_c/users/Public")).unwrap();
        fs::create_dir_all(p.join(format!("drive_c/users/{user}"))).unwrap();
        fs::write(p.join("system.reg"), "").unwrap();
        if let Some(content) = save {
            let dir = p.join(format!("drive_c/users/{user}/{SAVE_SUBDIR}"));
            fs::create_dir_all(dir.join("sub")).unwrap();
            fs::write(dir.join("saves.dat"), content).unwrap();
            fs::write(dir.join("sub/x"), "x").unwrap();
        }
        p
    }

    #[test]
    fn finds_and_imports_saves() {
        let tmp = tempfile::tempdir().unwrap();
        let old = fake_prefix(&tmp.path().join("old"), "crossover", Some("mine"));
        let new = fake_prefix(&tmp.path().join("new"), "crossover", None);
        assert!(is_initialized(&new));
        assert!(saves_dir(&old)
            .unwrap()
            .ends_with("crossover/AppData/LocalLow/Cikoria/Runehaven"));

        let to = import_saves(&old, &new).unwrap();
        assert_eq!(fs::read_to_string(to.join("saves.dat")).unwrap(), "mine");
        assert!(to.join("sub/x").is_file());

        // Importing again keeps the previous copy as a backup.
        import_saves(&old, &new).unwrap();
        let backups = fs::read_dir(to.parent().unwrap())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("Runehaven.backup-")
            })
            .count();
        assert_eq!(backups, 1);
    }

    #[test]
    fn reads_mac_driver_setting() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("user.reg"),
            "WINE REGISTRY Version 2\n\n[Software\\\\Wine\\\\Mac Driver] 1792140200\n#time=1\n\"RetinaMode\"=\"y\"\n\n[Software\\\\Wine\\\\X] 1\n\"RetinaMode\"=\"n\"\n",
        )
        .unwrap();
        assert_eq!(
            user_reg_string(tmp.path(), MAC_DRIVER_KEY, "RetinaMode").as_deref(),
            Some("y")
        );
        assert_eq!(user_reg_string(tmp.path(), MAC_DRIVER_KEY, "Other"), None);
    }

    #[test]
    fn missing_saves_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let old = fake_prefix(&tmp.path().join("old"), "kian", None);
        let new = fake_prefix(&tmp.path().join("new"), "crossover", None);
        assert!(import_saves(&old, &new).is_err());
    }

    #[test]
    fn recognises_prefixes_and_picks_a_place_for_new_ones() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!looks_like_prefix(tmp.path()));
        assert_eq!(new_prefix_in(tmp.path()), tmp.path());
        fs::create_dir_all(tmp.path().join("drive_c")).unwrap();
        fs::write(tmp.path().join("system.reg"), "WINE REGISTRY").unwrap();
        assert!(looks_like_prefix(tmp.path()));
        assert_eq!(new_prefix_in(tmp.path()), tmp.path().join("Runehaven Wine"));
    }
}
