//! The Wine prefix: Runehaven's private "Windows" with registry, user folder
//! and save games.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, Result};
use crate::util::{free_path, utc_stamp};

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

/// Every Windows user's Runehaven data folder in a prefix that exists, with
/// the user's name. Usually there is one; a prefix used with different Wine
/// builds can have a second.
pub fn saves_sources(prefix: &Path) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = fs::read_dir(prefix.join("drive_c/users"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name() != "Public")
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path().join(SAVE_SUBDIR)))
        .filter(|(_, dir)| dir.is_dir())
        .collect();
    found.sort();
    found
}

/// Packs save game folders into `zip`. A single folder is packed as it is
/// (`Runehaven/…`, the way Unity keeps it); several go into one folder each,
/// named by their label. The ZIP only gets its name once it is complete.
/// Returns `false` when there was nothing to pack.
pub fn pack_saves(sources: &[(String, PathBuf)], zip: &Path) -> Result<bool> {
    match sources {
        [] => Ok(false),
        [(_, only)] => ditto_zip(only, zip).map(|()| true),
        many => {
            let staging = std::env::temp_dir()
                .join(format!("rml-saves-{}-{}", std::process::id(), utc_stamp()))
                .join("Runehaven saves");
            let packed = many
                .iter()
                .try_for_each(|(label, dir)| copy_dir(dir, &staging.join(label).join("Runehaven")))
                .and_then(|()| ditto_zip(&staging, zip));
            if let Some(parent) = staging.parent() {
                let _ = fs::remove_dir_all(parent);
            }
            packed.map(|()| true)
        }
    }
}

/// Zips `src` (the folder itself, not just its contents) into `zip`.
fn ditto_zip(src: &Path, zip: &Path) -> Result<()> {
    let part = zip.with_extension("zip.part");
    let _ = fs::remove_file(&part);
    let status = Command::new("/usr/bin/ditto")
        .args(["-c", "-k", "--norsrc", "--noextattr", "--noqtn", "--keepParent"])
        .arg(src)
        .arg(&part)
        .status()
        .ctx(|| "running ditto".into())?;
    if !status.success() {
        let _ = fs::remove_file(&part);
        return Err(Error::Config("could not pack the save games".into()));
    }
    fs::rename(&part, zip).ctx(|| format!("moving {} into place", zip.display()))
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

/// The newest save game backups that are all kept.
pub const SAVE_BACKUPS: usize = 10;
/// Older backups: the last one of each day is kept for this many days with
/// backups, so a run of bad starts cannot push out every good state.
pub const DAILY_SAVE_BACKUPS: usize = 14;

/// Packs the save games of `prefix` into `backups/saves-<time>-<fingerprint>.zip`
/// if they changed since the last backup, then thins out old backups (see
/// [`SAVE_BACKUPS`]). Returns the new backup; `None` when there are no save
/// games or they are unchanged.
pub fn backup_saves(prefix: &Path, backups: &Path) -> Result<Option<PathBuf>> {
    let sources = saves_sources(prefix);
    if sources.is_empty() {
        return Ok(None);
    }
    fs::create_dir_all(backups).ctx(|| format!("creating {}", backups.display()))?;
    let fingerprint = saves_fingerprint(&sources)?;
    let existing = list_backups(backups);
    let made = if existing.first().is_some_and(|b| b.fingerprint() == Some(fingerprint.as_str())) {
        None
    } else {
        let zip = free_path(backups, &format!("saves-{}-{fingerprint}", utc_stamp()), "zip");
        pack_saves(&sources, &zip)?;
        Some(zip)
    };
    prune_backups(backups, SAVE_BACKUPS, DAILY_SAVE_BACKUPS);
    Ok(made)
}

/// One file in the backup folder.
struct Backup {
    path: PathBuf,
    /// `YYYYMMDDTHHMMSSZ` from the name; empty if the name has none.
    stamp: String,
    modified: SystemTime,
}

impl Backup {
    fn name(&self) -> &str {
        self.path.file_name().and_then(|n| n.to_str()).unwrap_or_default()
    }

    /// The fingerprint in the name, for backups that have one.
    fn fingerprint(&self) -> Option<&str> {
        let rest = self.name().strip_prefix("saves-")?.strip_suffix(".zip")?;
        let (_, fingerprint) = rest.split_once('-')?;
        Some(fingerprint)
    }
}

/// The save game backups in `dir`, newest first. Unfinished ones (`.part`
/// files left by a crash) are deleted.
fn list_backups(dir: &Path) -> Vec<Backup> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("saves-") {
            continue;
        }
        if name.ends_with(".part") {
            let _ = fs::remove_file(&path);
            continue;
        }
        if !name.ends_with(".zip") || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let stamp = name
            .get(6..22)
            .filter(|s| s.len() == 16 && s.as_bytes()[8] == b'T' && s.ends_with('Z'))
            .unwrap_or_default()
            .to_string();
        let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        found.push(Backup { path, stamp, modified });
    }
    found.sort_by(|a, b| (&b.stamp, b.modified).cmp(&(&a.stamp, a.modified)));
    found
}

/// Keeps the newest `recent` backups, and of the older ones the newest of
/// each day for `daily` days; deletes the rest.
fn prune_backups(dir: &Path, recent: usize, daily: usize) {
    let mut days_kept: Vec<String> = Vec::new();
    for backup in list_backups(dir).into_iter().skip(recent) {
        let day = backup.stamp.get(..8).unwrap_or_default().to_string();
        if !day.is_empty() && !days_kept.contains(&day) && days_kept.len() < daily {
            days_kept.push(day);
        } else {
            let _ = fs::remove_file(&backup.path);
        }
    }
}

/// A short fingerprint of the save games' file names and contents.
fn saves_fingerprint(sources: &[(String, PathBuf)]) -> Result<String> {
    let mut hasher = Sha256::new();
    for (label, dir) in sources {
        hasher.update(label.as_bytes());
        hasher.update([0]);
        hash_tree(dir, Path::new(""), &mut hasher)?;
    }
    Ok(hasher.finalize()[..6].iter().map(|b| format!("{b:02x}")).collect())
}

fn hash_tree(dir: &Path, rel: &Path, hasher: &mut Sha256) -> Result<()> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)
        .ctx(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<_>>()
        .ctx(|| format!("reading {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = rel.join(entry.file_name());
        let kind = entry.file_type().ctx(|| "reading file type".into())?;
        hasher.update(if kind.is_dir() { b"d" } else { b"f" });
        hasher.update(path.to_string_lossy().as_bytes());
        hasher.update([0]);
        if kind.is_dir() {
            hash_tree(&entry.path(), &path, hasher)?;
        } else if kind.is_file() {
            let mut file = fs::File::open(entry.path()).ctx(|| format!("reading {}", path.display()))?;
            let mut buf = [0u8; 1 << 16];
            loop {
                let n = file.read(&mut buf).ctx(|| format!("reading {}", path.display()))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
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

    fn zip_listing(zip: &Path) -> String {
        let out = Command::new("/usr/bin/unzip").arg("-l").arg(zip).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn save_backups_only_when_something_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = tmp.path().join("prefix");
        let backups = tmp.path().join("backups");
        assert!(backup_saves(&prefix, &backups).unwrap().is_none(), "no saves yet");
        fs::create_dir_all(prefix.join("drive_c/users/crossover")).unwrap();
        let saves = saves_dir(&prefix).unwrap();
        fs::create_dir_all(&saves).unwrap();
        fs::write(saves.join("saves.dat"), "progress").unwrap();
        let first = backup_saves(&prefix, &backups).unwrap().unwrap();
        assert!(zip_listing(&first).contains("Runehaven/saves.dat"));
        assert!(backup_saves(&prefix, &backups).unwrap().is_none(), "unchanged");
        fs::write(saves.join("saves.dat"), "more progress").unwrap();
        assert!(backup_saves(&prefix, &backups).unwrap().is_some());
        assert_eq!(fs::read_dir(&backups).unwrap().count(), 2);
    }

    #[test]
    fn old_backups_thin_out_to_one_a_day() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Three a day for five days, written in order.
        let mut names = Vec::new();
        for day in 1..=5 {
            for hour in [8, 12, 20] {
                names.push(format!("saves-202610{day:02}T{hour:02}0000Z-abc.zip"));
            }
        }
        // A copy with the Finder-style suffix belongs to its own time stamp.
        names.push("saves-20261005T200000Z-abc (2).zip".into());
        names.push("saves-20261005T200000Z-abc.zip.part".into());
        for n in &names {
            fs::write(dir.join(n), "").unwrap();
        }
        prune_backups(dir, 4, 2);
        let mut left: Vec<String> =
            fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(
            left,
            [
                // the last of days 3 and 4, then the four newest
                "saves-20261003T200000Z-abc.zip",
                "saves-20261004T200000Z-abc.zip",
                "saves-20261005T080000Z-abc.zip",
                "saves-20261005T120000Z-abc.zip",
                "saves-20261005T200000Z-abc (2).zip",
                "saves-20261005T200000Z-abc.zip",
            ]
        );
    }

    #[test]
    fn saves_of_every_windows_user_are_packed() {
        let tmp = tempfile::tempdir().unwrap();
        let prefix = fake_prefix(&tmp.path().join("p"), "crossover", Some("a"));
        let second = prefix.join(format!("drive_c/users/kian/{SAVE_SUBDIR}"));
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("saves.dat"), "b").unwrap();
        let sources = saves_sources(&prefix);
        assert_eq!(sources.len(), 2);
        let zip = tmp.path().join("all.zip");
        assert!(pack_saves(&sources, &zip).unwrap());
        let list = zip_listing(&zip);
        assert!(list.contains("Runehaven saves/crossover/Runehaven/saves.dat"));
        assert!(list.contains("Runehaven saves/kian/Runehaven/saves.dat"));
        assert!(!tmp.path().join("all.zip.part").exists());
    }
}
