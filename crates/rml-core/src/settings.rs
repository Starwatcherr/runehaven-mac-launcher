//! Runehaven's settings and key bindings. Unity keeps them ("PlayerPrefs") in
//! the registry under `HKEY_CURRENT_USER\Software\Cikoria\Runehaven`, not next
//! to the save games, so they need their own import.
//!
//! Sources that can be read:
//! * another Wine prefix (its `user.reg`), e.g. an earlier manual setup,
//! * a `user.reg` file directly,
//! * a `.reg` export made with the Windows registry editor (for players who
//!   bring their settings from a Windows PC).
//!
//! The values are written into the launcher's prefix with Wine's own
//! `reg import`, so the registry stays consistent even if Wine is running.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, IoContext, Result};
use crate::paths::to_wine_path;
use crate::util::utc_stamp;

/// Registry key below HKEY_CURRENT_USER.
pub const KEY: &str = r"Software\Cikoria\Runehaven";

/// Unity's own bookkeeping (session counters, analytics ids). Not settings,
/// so they are not carried over.
const SKIPPED_PREFIXES: [&str; 2] = ["unity.", "unity_connect."];

/// One registry value, kept in registry-file syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    /// Decoded name, e.g. `rebinds_h1376817600`.
    pub name: String,
    /// The value's text as it appears in the file (`"name"=dword:…`),
    /// including continuation lines.
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub values: Vec<Value>,
    /// Values left out because Unity uses them only internally.
    pub skipped: usize,
}

impl Settings {
    /// Reads the settings from a prefix folder, a `user.reg` or a `.reg`
    /// export. Returns `Ok(None)` if the source has no Runehaven settings.
    pub fn read(source: &Path) -> Result<Option<Settings>> {
        let file = if source.is_dir() {
            source.join("user.reg")
        } else {
            source.to_path_buf()
        };
        let bytes = fs::read(&file).ctx(|| format!("reading {}", file.display()))?;
        Ok(Self::parse(&decode(&bytes)))
    }

    /// Finds the Runehaven key in registry-file text (Wine or regedit format).
    pub fn parse(text: &str) -> Option<Settings> {
        let mut lines = text.lines().map(|l| l.trim_end_matches('\r'));
        // Find the section header.
        loop {
            let line = lines.next()?;
            if let Some(key) = section_key(line) {
                if key.eq_ignore_ascii_case(KEY) {
                    break;
                }
            }
        }
        let mut settings = Settings::default();
        let mut current: Option<String> = None;
        let mut continues = false;
        for line in lines {
            if continues {
                if let Some(text) = current.as_mut() {
                    text.push('\n');
                    text.push_str(line);
                }
                continues = line.ends_with('\\');
                continue;
            }
            if let Some(text) = current.take() {
                settings.push(text);
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                break; // next key
            }
            if trimmed.starts_with('"') || trimmed.starts_with('@') {
                current = Some(trimmed.to_string());
                continues = trimmed.ends_with('\\');
            }
            // Blank lines, `#time=` metadata and `;` comments are skipped.
        }
        if let Some(text) = current.take() {
            settings.push(text);
        }
        Some(settings)
    }

    fn push(&mut self, text: String) {
        let name = value_name(&text);
        let data = text[text.find('=').map_or(text.len(), |i| i + 1)..].trim_start();
        // `str(2):` is Wine-only syntax; Unity does not write such values.
        if SKIPPED_PREFIXES.iter().any(|p| name.starts_with(p)) || data.starts_with("str(") {
            self.skipped += 1;
            return;
        }
        self.values.push(Value { name, text });
    }

    /// The settings as a registry-editor file that `reg import` understands.
    pub fn to_reg_file(&self) -> String {
        let mut out = format!("REGEDIT4\n\n[HKEY_CURRENT_USER\\{KEY}]\n");
        for v in &self.values {
            out.push_str(&v.text);
            out.push('\n');
        }
        out.push('\n');
        out
    }

    /// Whether custom key bindings are part of the settings.
    pub fn has_key_bindings(&self) -> bool {
        self.values.iter().any(|v| v.name.starts_with("rebinds_"))
    }
}

/// Registry files from Windows are UTF-16 with a byte order mark; Wine's
/// are plain text.
fn decode(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The key of a `[...]` section line, normalised to `Software\…`.
/// Wine writes `[Software\\Cikoria\\Runehaven] 1792091080`, regedit writes
/// `[HKEY_CURRENT_USER\Software\Cikoria\Runehaven]`.
fn section_key(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix('[')?;
    let raw = &rest[..rest.find(']')?];
    let key = if raw.contains("\\\\") {
        raw.replace("\\\\", "\\")
    } else {
        raw.to_string()
    };
    for root in ["HKEY_CURRENT_USER\\", "HKCU\\"] {
        if key.len() >= root.len() && key[..root.len()].eq_ignore_ascii_case(root) {
            return Some(key[root.len()..].to_string());
        }
    }
    Some(key)
}

/// Decodes the quoted name at the start of a value line (`@` = default value).
fn value_name(text: &str) -> String {
    let mut chars = text.chars();
    if chars.next() != Some('"') {
        return String::new();
    }
    let mut name = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    name.push(next);
                }
            }
            '"' => break,
            c => name.push(c),
        }
    }
    name
}

/// What an import did.
#[derive(Debug)]
pub struct ImportReport {
    pub imported: usize,
    pub skipped: usize,
    pub key_bindings: bool,
    /// The previous settings of the launcher's prefix, if there were any.
    pub backup: Option<PathBuf>,
}

/// Replaces Runehaven's settings in `prefix` with `settings`. The previous
/// settings are exported to `backup_dir` first. The game must not be running,
/// because Unity writes its settings back when it quits.
pub fn import(
    settings: &Settings,
    wine: &Path,
    library_path: &str,
    prefix: &Path,
    backup_dir: &Path,
    log: &Path,
) -> Result<ImportReport> {
    fs::create_dir_all(backup_dir).ctx(|| format!("creating {}", backup_dir.display()))?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).ctx(|| format!("creating {}", dir.display()))?;
    }
    let stamp = utc_stamp();
    let hkcu_key = format!("HKEY_CURRENT_USER\\{KEY}");
    let wine_cmd = WineCmd {
        wine,
        library_path,
        prefix,
        log,
    };

    let backup = backup_dir.join(format!("settings-{stamp}.reg"));
    let backup = if Settings::read(prefix)?.is_some_and(|s| !s.values.is_empty()) {
        wine_cmd.run(&["reg", "export", &hkcu_key, &to_wine_path(&backup), "/y"])?;
        backup.is_file().then_some(backup)
    } else {
        None
    };

    let file = backup_dir.join(format!("import-{stamp}.reg"));
    fs::write(&file, settings.to_reg_file()).ctx(|| format!("writing {}", file.display()))?;
    // Remove the old key so that bindings the source no longer has disappear too.
    wine_cmd.run(&["reg", "delete", &hkcu_key, "/f"])?;
    let ok = wine_cmd.run(&["reg", "import", &to_wine_path(&file)])?;
    wine_cmd.wait_for_server();
    let _ = fs::remove_file(&file);
    if !ok {
        return Err(Error::Config(format!(
            "Wine could not import the settings (log: {})",
            log.display()
        )));
    }

    // Check the result in the registry file Wine has written.
    let written = Settings::read(prefix)?.unwrap_or_default();
    if written.values.len() < settings.values.len() {
        return Err(Error::Config(format!(
            "only {} of {} settings arrived in the prefix (log: {})",
            written.values.len(),
            settings.values.len(),
            log.display()
        )));
    }
    Ok(ImportReport {
        imported: settings.values.len(),
        skipped: settings.skipped,
        key_bindings: settings.has_key_bindings(),
        backup,
    })
}

pub(crate) struct WineCmd<'a> {
    pub wine: &'a Path,
    pub library_path: &'a str,
    pub prefix: &'a Path,
    pub log: &'a Path,
}

impl WineCmd<'_> {
    fn envs(&self) -> [(&'static str, String); 4] {
        [
            ("WINEPREFIX", self.prefix.to_string_lossy().into_owned()),
            ("DYLD_FALLBACK_LIBRARY_PATH", self.library_path.to_string()),
            ("WINEDEBUG", "-all,err+all".to_string()),
            (
                "WINEDLLOVERRIDES",
                "mscoree,mshtml=;winemenubuilder.exe=d".to_string(),
            ),
        ]
    }

    /// Runs `wine <args>`; returns whether it exited successfully.
    pub(crate) fn run(&self, args: &[&str]) -> Result<bool> {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log)
            .ctx(|| format!("opening {}", self.log.display()))?;
        let log_err = log.try_clone().ctx(|| "duplicating log handle".into())?;
        let status = Command::new(self.wine)
            .args(args)
            .envs(self.envs().iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .status()
            .ctx(|| format!("starting {}", self.wine.display()))?;
        Ok(status.success())
    }

    /// Wine writes the registry to disk when its server exits.
    pub(crate) fn wait_for_server(&self) {
        let wineserver = self.wine.with_file_name("wineserver");
        if wineserver.is_file() {
            let _ = Command::new(&wineserver)
                .arg("-w")
                .envs(self.envs().iter().map(|(k, v)| (*k, v.as_str())))
                .status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINE_REG: &str = "WINE REGISTRY Version 2\n\
;; All keys relative to REGISTRY\\\\User\\\\S-1-5-21-0-0-0-1000\n\
\n\
#arch=win64\n\
\n\
[Keyboard Layout\\\\Preload] 1792086834\n\
#time=1dd5cce250465a8\n\
\"1\"=\"00000407\"\n\
\n\
[Software\\\\Cikoria\\\\Runehaven] 1792091080\n\
#time=1dd5cd80816876e\n\
\"anti_aliasing_h2149886334\"=dword:00000000\n\
\"rebinds_h1376817600\"=hex:7b,22,62,69,6e,64,69,6e,67,73,22,3a,5b,7b,22,61,63,\\\n  74,69,6f,6e,22,00\n\
\"Screenmanager Fullscreen mode_h3630240806\"=dword:00000003\n\
\"unity.player_session_count_h922449978\"=hex:35,00\n\
\"unity_connect.session_id_h4145606137\"=hex:39,00\n\
\"weapon_swing_mode_h783000480\"=dword:00000000\n\
\n\
[Software\\\\Microsoft\\\\Internet Explorer\\\\Main] 1792057033\n\
\"Start Page\"=\"https://www.winehq.org\"\n";

    #[test]
    fn reads_wine_registry() {
        let s = Settings::parse(WINE_REG).unwrap();
        let names: Vec<&str> = s.values.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "anti_aliasing_h2149886334",
                "rebinds_h1376817600",
                "Screenmanager Fullscreen mode_h3630240806",
                "weapon_swing_mode_h783000480"
            ]
        );
        assert_eq!(s.skipped, 2);
        assert!(s.has_key_bindings());
        assert!(s.values[1].text.ends_with("\n  74,69,6f,6e,22,00"));
    }

    #[test]
    fn writes_regedit_file() {
        let s = Settings::parse(WINE_REG).unwrap();
        let out = s.to_reg_file();
        assert!(out.starts_with("REGEDIT4\n\n[HKEY_CURRENT_USER\\Software\\Cikoria\\Runehaven]\n"));
        assert!(out.contains("\"rebinds_h1376817600\"=hex:7b,22,"));
        assert!(!out.contains("winehq"));
        // Reading our own output gives the same values back.
        assert_eq!(Settings::parse(&out).unwrap().values, s.values);
    }

    #[test]
    fn reads_windows_export() {
        let text = "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Cikoria\\Runehaven]\r\n\
\"camera_shake_h581529591\"=dword:00000001\r\n\
\"selected_theme_h1228781698\"=hex:44,65,66,61,75,6c,74,00\r\n\r\n\
[HKEY_CURRENT_USER\\Software\\Other]\r\n\"x\"=dword:1\r\n";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let s = Settings::parse(&decode(&bytes)).unwrap();
        assert_eq!(s.values.len(), 2);
        assert_eq!(s.values[1].text, "\"selected_theme_h1228781698\"=hex:44,65,66,61,75,6c,74,00");
    }

    #[test]
    fn missing_key_is_none() {
        assert!(Settings::parse("WINE REGISTRY Version 2\n[Software\\\\Wine] 1\n").is_none());
    }

    #[test]
    fn escaped_names() {
        assert_eq!(value_name(r#""a\"b\\c"=dword:1"#), r#"a"b\c"#);
    }
}
