//! Maintenance: sizes for the overview, reinstalling the runtime, setting the
//! Wine prefix up again and removing the launcher's data. Each job needs the
//! [`JobLock`], so nothing here runs while the game or another job is running.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::launch::{self, prefix_path, ResolvedRuntime, StartEvent};
use crate::lock::{JobLock, LOCK_FILE, PID_FILE};
use crate::state::LauncherState;
use crate::paths::LauncherPaths;
use crate::prefix;
use crate::runtime::{Runtime, RuntimeSpec};
use crate::settings::{self, ImportReport, Settings};
use crate::util::{free_path, utc_stamp};

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
pub fn remove_runtime(config: &Config, launcher: &LauncherPaths, _lock: &JobLock) -> Result<()> {
    let rt = Runtime::locate(config, launcher, &RuntimeSpec::bundled());
    if rt.root.exists() {
        fs::remove_dir_all(&rt.root).ctx(|| format!("removing {}", rt.root.display()))?;
    }
    Ok(())
}

/// Sets the launcher's own Wine prefix up from scratch and keeps save games
/// and settings: the old prefix moves to `backups/`, a fresh one is created,
/// and saves and settings are copied over. If that fails, the old prefix is
/// put back. A prefix chosen in the settings (another app's, say) is never
/// touched. Returns where the old prefix went.
pub fn reset_prefix(
    config: &Config,
    launcher: &LauncherPaths,
    _lock: &JobLock,
    on_event: &mut dyn FnMut(StartEvent),
) -> Result<Option<PathBuf>> {
    if config.prefix.is_some() {
        return Err(Error::Config(
            "Only the launcher's own Wine setup can be reset. The one chosen in the settings is left as it is.".into(),
        ));
    }
    let prefix = launcher.prefix();
    if !prefix.exists() {
        launch::ensure_prefix(config, launcher, on_event)?;
        return Ok(None);
    }
    let backups = launcher.root.join("backups");
    fs::create_dir_all(&backups).ctx(|| format!("creating {}", backups.display()))?;
    let old = backups.join(format!("prefix-{}", utc_stamp()));
    fs::rename(&prefix, &old).ctx(|| format!("moving {} aside", prefix.display()))?;
    let rebuilt = (|| -> Result<()> {
        launch::ensure_prefix(config, launcher, on_event)?;
        if prefix::saves_dir(&old).is_some_and(|d| d.is_dir()) {
            prefix::import_saves(&old, &prefix)?;
        }
        if old.join("user.reg").is_file() {
            import_settings(&old, config, launcher, &prefix)?;
        }
        Ok(())
    })();
    if let Err(e) = rebuilt {
        let put_back = (!prefix.exists() || fs::remove_dir_all(&prefix).is_ok())
            && fs::rename(&old, &prefix).is_ok();
        return Err(if put_back {
            e
        } else {
            Error::Config(format!("{e} The previous Wine setup is in {}.", old.display()))
        });
    }
    Ok(Some(old))
}

/// Deletes the runtime, the launcher's own prefix, downloads and logs. Save
/// games are first packed into a ZIP in `saves_dest`, whose path is
/// returned. A prefix chosen in the settings (for example another app's) is
/// never deleted. The settings file and backups stay.
pub fn remove_data(
    config: &Config,
    launcher: &LauncherPaths,
    _lock: &JobLock,
    saves_dest: &Path,
) -> Result<Option<PathBuf>> {
    let own_prefix = config.prefix.is_none();
    let zip = if own_prefix { pack_saves(config, launcher, saves_dest, false)? } else { None };
    let mut doomed = vec![
        Runtime::locate(config, launcher, &RuntimeSpec::bundled()).root,
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

/// Packs the save games of the prefix into a ZIP in `dest`, if there are
/// any. With `older`, also those of the prefixes earlier resets kept in
/// `backups/`.
fn pack_saves(
    config: &Config,
    launcher: &LauncherPaths,
    dest: &Path,
    older: bool,
) -> Result<Option<PathBuf>> {
    let current = prefix::saves_sources(&prefix_path(config, launcher));
    let mut earlier = Vec::new();
    if older {
        for (stamp, dir) in old_prefixes(launcher) {
            earlier.extend(
                prefix::saves_sources(&dir)
                    .into_iter()
                    .map(|(user, saves)| (format!("Wine setup before reset {stamp}/{user}"), saves)),
            );
        }
    }
    let sources: Vec<(String, PathBuf)> = if earlier.is_empty() {
        current
    } else {
        current
            .into_iter()
            .map(|(user, saves)| (format!("Current Wine setup/{user}"), saves))
            .chain(earlier)
            .collect()
    };
    if sources.is_empty() {
        return Ok(None);
    }
    fs::create_dir_all(dest).ctx(|| format!("creating {}", dest.display()))?;
    let zip = free_path(dest, &format!("Runehaven saves {}", utc_stamp()), "zip");
    match prefix::pack_saves(&sources, &zip) {
        Ok(_) => Ok(Some(zip)),
        Err(_) => Err(Error::Config("could not pack the save games; nothing was deleted".into())),
    }
}

/// The prefixes earlier resets moved into `backups/`, with their time stamps.
fn old_prefixes(launcher: &LauncherPaths) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = fs::read_dir(launcher.root.join("backups"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let stamp = e.file_name().to_str()?.strip_prefix("prefix-")?.to_string();
            Some((stamp, e.path()))
        })
        .collect();
    found.sort();
    found
}

/// What goes away together with the launcher. Its settings always do.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct UninstallChoice {
    /// The downloaded Wine runtime (WineCX, DXMT, libraries) and downloads.
    pub runtime: bool,
    /// The launcher's own Wine prefix and its backups; save games are packed
    /// first. A prefix chosen in the settings is never touched.
    pub prefix: bool,
    pub logs: bool,
}

/// Removes what the launcher put onto the Mac, as chosen, plus its settings
/// and its input fix in the game folder. Returns the ZIP with the save games
/// (also those of earlier setups in `backups/`) when the prefix was removed.
/// `config` must be the real settings, not a default for a file that could
/// not be read: it decides which prefix is the launcher's own. The app
/// itself is not touched here.
pub fn uninstall_launcher(
    config: &Config,
    launcher: &LauncherPaths,
    _lock: &JobLock,
    choice: UninstallChoice,
    saves_dest: &Path,
) -> Result<Option<PathBuf>> {
    if LauncherState::load(&launcher.state_file())?.pending_platform_restore.is_some() {
        return Err(Error::Config(
            "Steam's platform setting still has to be switched back first. Start Steam and try again.".into(),
        ));
    }
    remove_own_inputfix(config);
    let mut zip = None;
    let mut doomed = Vec::new();
    if choice.prefix && config.prefix.is_none() {
        zip = pack_saves(config, launcher, saves_dest, true)?;
        doomed.extend([launcher.prefix(), launcher.root.join("backups")]);
    }
    if choice.runtime {
        // In a folder chosen in the settings only the runtime itself goes.
        doomed.extend([
            Runtime::locate(config, launcher, &RuntimeSpec::bundled()).root,
            launcher.runtime(),
            launcher.root.join("downloads"),
        ]);
    }
    if choice.logs {
        doomed.push(launcher.logs());
    }
    for dir in doomed.into_iter().filter(|d| d.exists()) {
        fs::remove_dir_all(&dir).ctx(|| format!("removing {}", dir.display()))?;
    }
    for file in [
        launcher.config_file(),
        launcher.state_file(),
        LauncherState::lock_file(&launcher.state_file()),
        launcher.root.join(PID_FILE),
        launcher.root.join(LOCK_FILE),
    ] {
        let _ = fs::remove_file(file);
    }
    // Gone completely if nothing was kept.
    let _ = fs::remove_file(launcher.root.join(".DS_Store"));
    let _ = fs::remove_dir(&launcher.root);
    Ok(zip)
}

/// Deletes the input fix from the game folder, but only if it is the
/// launcher's (this one or an older build): a mod with the same file name
/// stays.
fn remove_own_inputfix(config: &Config) {
    let ours = config
        .inputfix_dll
        .clone()
        .or_else(|| launch::bundled_file("version.dll"))
        .and_then(|p| fs::read(p).ok());
    let Ok((_, _, game_dir)) = launch::locate_game() else {
        return;
    };
    let target = game_dir.join("version.dll");
    let is_file = fs::symlink_metadata(&target).is_ok_and(|m| m.is_file());
    if is_file && fs::read(&target).is_ok_and(|found| launch::is_own_inputfix(&found, ours.as_deref())) {
        let _ = fs::remove_file(target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(launcher: &LauncherPaths) -> JobLock {
        JobLock::acquire(launcher).unwrap()
    }

    fn save_in(prefix: &Path, user: &str, content: &str) {
        fs::create_dir_all(prefix.join(format!("drive_c/users/{user}"))).unwrap();
        let saves = prefix::saves_dir(prefix).unwrap();
        fs::create_dir_all(&saves).unwrap();
        fs::write(saves.join("saves.dat"), content).unwrap();
    }

    #[test]
    fn a_chosen_prefix_is_never_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let other = tmp.path().join("other-app-bottle");
        save_in(&other, "crossover", "theirs");
        let config = Config { prefix: Some(other.clone()), ..Config::default() };
        assert!(reset_prefix(&config, &launcher, &lock(&launcher), &mut |_| {}).is_err());
        assert!(prefix::saves_dir(&other).unwrap().join("saves.dat").exists());
        assert!(!launcher.root.join("backups").exists());
    }

    #[test]
    fn a_failed_reset_puts_the_old_prefix_back() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        save_in(&launcher.prefix(), "crossover", "progress");
        fs::write(launcher.prefix().join("system.reg"), "").unwrap();
        // No Wine runtime: setting the prefix up again fails.
        assert!(reset_prefix(&Config::default(), &launcher, &lock(&launcher), &mut |_| {}).is_err());
        let saves = prefix::saves_dir(&launcher.prefix()).unwrap();
        assert_eq!(fs::read_to_string(saves.join("saves.dat")).unwrap(), "progress");
        let left = fs::read_dir(launcher.root.join("backups")).unwrap().count();
        assert_eq!(left, 0, "nothing stays behind in backups");
    }

    #[test]
    fn uninstalling_packs_the_saves_of_earlier_setups_too() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        save_in(&launcher.prefix(), "crossover", "now");
        save_in(&launcher.root.join("backups/prefix-20261001T100000Z"), "crossover", "before");
        let choice = UninstallChoice { runtime: false, prefix: true, logs: false };
        let desktop = tmp.path().join("desktop");
        let zip = uninstall_launcher(&Config::default(), &launcher, &lock(&launcher), choice, &desktop)
            .unwrap()
            .unwrap();
        let list = Command::new("/usr/bin/unzip").arg("-l").arg(&zip).output().unwrap();
        let list = String::from_utf8_lossy(&list.stdout);
        assert!(list.contains("Current Wine setup/crossover/Runehaven/saves.dat"));
        assert!(list.contains("Wine setup before reset 20261001T100000Z/crossover/Runehaven/saves.dat"));
        assert!(!launcher.root.join("backups").exists());
    }

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
        let zip = remove_data(&Config::default(), &launcher, &lock(&launcher), &dest).unwrap().unwrap();
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
        let zip = remove_data(&config, &launcher, &lock(&launcher), &tmp.path().join("desktop")).unwrap();
        assert!(zip.is_none());
        assert!(other.exists());
    }

    #[test]
    fn uninstalling_removes_what_was_chosen() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        fs::create_dir_all(launcher.prefix().join("drive_c/users/crossover")).unwrap();
        let saves = prefix::saves_dir(&launcher.prefix()).unwrap();
        fs::create_dir_all(&saves).unwrap();
        fs::write(saves.join("saves.dat"), "progress").unwrap();
        fs::create_dir_all(launcher.runtime().join("x")).unwrap();
        fs::create_dir_all(launcher.logs()).unwrap();
        fs::write(launcher.config_file(), "{}").unwrap();
        let desktop = tmp.path().join("desktop");

        // Keep the runtime: settings go, the runtime stays.
        let keep_runtime = UninstallChoice { runtime: false, prefix: true, logs: true };
        let zip = uninstall_launcher(&Config::default(), &launcher, &lock(&launcher), keep_runtime, &desktop).unwrap();
        assert!(zip.is_some_and(|z| z.exists()));
        assert!(!launcher.prefix().exists() && !launcher.logs().exists() && !launcher.config_file().exists());
        assert!(launcher.runtime().exists());

        // Everything: the folder is gone.
        let all = UninstallChoice { runtime: true, prefix: true, logs: true };
        uninstall_launcher(&Config::default(), &launcher, &lock(&launcher), all, &desktop).unwrap();
        assert!(!launcher.root.exists());
    }

    #[test]
    fn uninstalling_waits_for_a_pending_steam_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let state = LauncherState {
            pending_platform_restore: Some(crate::state::PendingRestore::new(String::new(), "test")),
            ..Default::default()
        };
        state.save(&launcher.state_file()).unwrap();
        let all = UninstallChoice { runtime: true, prefix: true, logs: true };
        assert!(uninstall_launcher(&Config::default(), &launcher, &lock(&launcher), all, tmp.path()).is_err());
        assert!(launcher.state_file().exists());
    }
}
