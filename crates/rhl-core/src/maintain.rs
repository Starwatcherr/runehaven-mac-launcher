//! Maintenance: sizes for the overview, reinstalling the runtime, setting the
//! Wine prefix up again and removing the launcher's data. Nothing here runs
//! while the game is running.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::launch::{self, prefix_path, ResolvedRuntime, RunLock, StartEvent};
use crate::paths::LauncherPaths;
use crate::prefix;
use crate::runtime::{Runtime, RuntimeSpec};
use crate::settings::{self, ImportReport, Settings};
use crate::util::utc_stamp;

/// Bytes a folder takes on disk, 0 if it does not exist.
pub fn disk_usage(path: &Path) -> u64 {
    let Ok(out) = Command::new("/usr/bin/du").arg("-sk").arg(path).output() else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|kb| kb.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// Takes over Runehaven's settings and key bindings from another prefix,
/// its `user.reg` or a `.reg` export. `None` when there were none.
pub fn import_settings(
    from: &Path,
    config: &Config,
    launcher: &LauncherPaths,
    prefix: &Path,
) -> Result<Option<ImportReport>> {
    let Some(found) = Settings::read(from)? else {
        return Ok(None);
    };
    let rt = ResolvedRuntime::resolve(config, launcher)?;
    settings::import(
        &found,
        &rt.wine,
        &rt.library_path(),
        prefix,
        &launcher.root.join("backups"),
        &launcher.logs().join("settings-import.log"),
    )
    .map(Some)
}

/// Deletes the launcher's own Wine runtime so it can be downloaded again.
pub fn remove_runtime(launcher: &LauncherPaths) -> Result<()> {
    let _lock = RunLock::acquire(launcher)?;
    let rt = Runtime::locate(launcher, &RuntimeSpec::bundled());
    if rt.root.exists() {
        fs::remove_dir_all(&rt.root).ctx(|| format!("removing {}", rt.root.display()))?;
    }
    Ok(())
}

/// Sets the Wine prefix up from scratch and keeps save games and settings:
/// the old prefix moves to `backups/`, a fresh one is created, and saves and
/// settings are copied over. Returns where the old prefix went.
pub fn reset_prefix(
    config: &Config,
    launcher: &LauncherPaths,
    on_event: &mut dyn FnMut(StartEvent),
) -> Result<Option<PathBuf>> {
    let _lock = RunLock::acquire(launcher)?;
    let prefix = prefix_path(config, launcher);
    let backup = if prefix.exists() {
        let backups = launcher.root.join("backups");
        fs::create_dir_all(&backups).ctx(|| format!("creating {}", backups.display()))?;
        let to = backups.join(format!("prefix-{}", utc_stamp()));
        // A plain rename; a prefix on another drive fails here and stays untouched.
        fs::rename(&prefix, &to).ctx(|| format!("moving {} aside", prefix.display()))?;
        Some(to)
    } else {
        None
    };
    launch::ensure_prefix(config, launcher, on_event)?;
    if let Some(old) = &backup {
        if prefix::saves_dir(old).is_some_and(|d| d.is_dir()) {
            prefix::import_saves(old, &prefix)?;
        }
        import_settings(old, config, launcher, &prefix)?;
    }
    Ok(backup)
}

/// Deletes the runtime, the launcher's own prefix, downloads and logs. Save
/// games are first packed into a ZIP in `saves_dest`, whose path is
/// returned. A prefix chosen in the settings (for example another app's) is
/// never deleted. The settings file and backups stay.
pub fn remove_data(
    config: &Config,
    launcher: &LauncherPaths,
    saves_dest: &Path,
) -> Result<Option<PathBuf>> {
    let _lock = RunLock::acquire(launcher)?;
    let own_prefix = config.prefix.is_none();
    let saves = prefix::saves_dir(&prefix_path(config, launcher)).filter(|d| d.is_dir());
    let zip = match saves.filter(|_| own_prefix) {
        Some(dir) => {
            fs::create_dir_all(saves_dest).ctx(|| format!("creating {}", saves_dest.display()))?;
            let zip = saves_dest.join(format!("Runehaven saves {}.zip", utc_stamp()));
            let status = Command::new("/usr/bin/ditto")
                .args(["-c", "-k", "--norsrc", "--noextattr", "--noqtn", "--keepParent"])
                .arg(&dir)
                .arg(&zip)
                .status()
                .ctx(|| "running ditto".into())?;
            if !status.success() {
                return Err(Error::Config(
                    "could not pack the save games; nothing was deleted".into(),
                ));
            }
            Some(zip)
        }
        None => None,
    };
    let mut doomed = vec![
        launcher.runtime(),
        launcher.root.join("downloads"),
        launcher.logs(),
    ];
    if own_prefix {
        doomed.push(launcher.prefix());
    }
    for dir in doomed.into_iter().filter(|d| d.exists()) {
        fs::remove_dir_all(&dir).ctx(|| format!("removing {}", dir.display()))?;
    }
    Ok(zip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_data_packs_saves_and_spares_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        fs::create_dir_all(launcher.prefix().join("drive_c/users/crossover")).unwrap();
        let saves = prefix::saves_dir(&launcher.prefix()).unwrap();
        fs::create_dir_all(&saves).unwrap();
        fs::write(saves.join("saves.dat"), "progress").unwrap();
        fs::create_dir_all(launcher.runtime().join("x")).unwrap();
        fs::create_dir_all(launcher.logs()).unwrap();
        fs::write(launcher.config_file(), "{}").unwrap();

        let dest = tmp.path().join("desktop");
        let zip = remove_data(&Config::default(), &launcher, &dest).unwrap().unwrap();
        let list = Command::new("/usr/bin/unzip").arg("-l").arg(&zip).output().unwrap();
        assert!(String::from_utf8_lossy(&list.stdout).contains("saves.dat"));
        assert!(!launcher.prefix().exists() && !launcher.runtime().exists() && !launcher.logs().exists());
        assert!(launcher.config_file().exists(), "settings stay");
    }

    #[test]
    fn a_chosen_prefix_is_never_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let other = tmp.path().join("other-app-bottle");
        fs::create_dir_all(&other).unwrap();
        let config = Config {
            prefix: Some(other.clone()),
            ..Config::default()
        };
        let zip = remove_data(&config, &launcher, &tmp.path().join("desktop")).unwrap();
        assert!(zip.is_none());
        assert!(other.exists());
    }
}
