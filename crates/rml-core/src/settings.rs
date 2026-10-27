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
//! Each value is read into its type and written out anew, so nothing from
//! the source reaches `reg import` as it was: no other key, no stray line.

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

/// One registry value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    /// Decoded name, e.g. `rebinds_h1376817600`; empty for the default value.
    pub name: String,
    pub data: Data,
}

/// The kinds of data Unity's settings use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Data {
    /// `"text"` (REG_SZ).
    String(String),
    /// `dword:0000002a`.
    Dword(u32),
    /// `hex:…` (REG_BINARY), or `hex(N):…` for another type number `N`.
    Hex { kind: Option<u32>, bytes: Vec<u8> },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub values: Vec<Value>,
    /// Values left out: Unity's own bookkeeping, and anything that is not a
    /// well-formed value of a known kind.
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
        // A value's lines: the first, then continuation lines while the
        // previous one ends with a backslash.
        let mut current: Vec<&str> = Vec::new();
        let mut continues = false;
        for line in lines {
            if continues {
                current.push(line);
                continues = line.ends_with('\\');
                continue;
            }
            if !current.is_empty() {
                settings.push(&std::mem::take(&mut current));
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with('[') {
                break; // next key
            }
            if trimmed.starts_with('"') || trimmed.starts_with('@') {
                current.push(trimmed);
                continues = trimmed.ends_with('\\');
            }
            // Blank lines, `#time=` metadata and `;` comments are skipped.
        }
        if !current.is_empty() {
            settings.push(&current);
        }
        Some(settings)
    }

    fn push(&mut self, lines: &[&str]) {
        match parse_value(lines) {
            Some(v) if !SKIPPED_PREFIXES.iter().any(|p| v.name.starts_with(p)) => self.values.push(v),
            // Unity's bookkeeping, Wine-only `str(N):` values, broken lines.
            _ => self.skipped += 1,
        }
    }

    /// The settings as a registry-editor file that `reg import` understands,
    /// written from the parsed values.
    pub fn to_reg_file(&self) -> String {
        let mut out = format!("REGEDIT4\n\n[HKEY_CURRENT_USER\\{KEY}]\n");
        for v in &self.values {
            out.push_str(&v.to_reg_line());
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
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
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
        if key.get(..root.len()).is_some_and(|start| start.eq_ignore_ascii_case(root)) {
            return Some(key[root.len()..].to_string());
        }
    }
    Some(key)
}

/// Reads one value from its lines (`"name"=data`, hex data may continue on
/// further lines). `None` for anything that is not a well-formed value of a
/// known kind, including control characters anywhere and continuation lines
/// that are not hex data.
fn parse_value(lines: &[&str]) -> Option<Value> {
    let first = lines.first()?;
    if lines.iter().any(|l| l.chars().any(char::is_control)) {
        return None;
    }
    let (name, rest) = if let Some(rest) = first.strip_prefix('@') {
        (String::new(), rest)
    } else {
        quoted(first)?
    };
    let data = rest.trim_start().strip_prefix('=')?.trim_start();
    let data = if data.starts_with('"') {
        let (text, after) = quoted(data)?;
        if !after.trim().is_empty() || lines.len() > 1 {
            return None;
        }
        Data::String(text)
    } else if let Some(hex) = data.strip_prefix("dword:") {
        if lines.len() > 1 || hex.is_empty() || hex.len() > 8 {
            return None;
        }
        Data::Dword(hex_number(hex.trim_end())?)
    } else {
        let rest = data.strip_prefix("hex")?;
        let (kind, rest) = match rest.strip_prefix('(') {
            Some(inner) => {
                let (n, after) = inner.split_once(')')?;
                (Some(hex_number(n)?), after)
            }
            None => (None, rest),
        };
        let mut digits = rest.strip_prefix(':')?.to_string();
        for line in &lines[1..] {
            digits = digits.strip_suffix('\\')?.to_string();
            digits.push_str(line.trim_start());
        }
        let digits = digits.trim_end().trim_end_matches(',');
        let bytes = if digits.is_empty() {
            Vec::new()
        } else {
            digits
                .split(',')
                .map(|b| if b.len() == 2 { hex_number(b).map(|n| n as u8) } else { None })
                .collect::<Option<Vec<u8>>>()?
        };
        Data::Hex { kind, bytes }
    };
    Some(Value { name, data })
}

/// A number written only in hex digits (no sign, no prefix), up to 32 bits.
fn hex_number(digits: &str) -> Option<u32> {
    let plain = !digits.is_empty() && digits.len() <= 8 && digits.bytes().all(|c| c.is_ascii_hexdigit());
    plain.then(|| u32::from_str_radix(digits, 16).ok()).flatten()
}

/// Reads a quoted string with `\\` and `\"` escapes from the start of `text`;
/// returns it and what follows the closing quote.
fn quoted(text: &str) -> Option<(String, &str)> {
    let mut chars = text.strip_prefix('"')?.char_indices();
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                (_, c @ ('\\' | '"')) => out.push(c),
                _ => return None,
            },
            '"' => return Some((out, &text[i + 2..])),
            c => out.push(c),
        }
    }
    None
}

/// `"…"` with the escapes registry files use.
fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

impl Value {
    /// The value in registry-file syntax, long hex data wrapped like regedit
    /// does it.
    fn to_reg_line(&self) -> String {
        let name = if self.name.is_empty() { "@".to_string() } else { quote(&self.name) };
        let data = match &self.data {
            Data::String(text) => quote(text),
            Data::Dword(n) => format!("dword:{n:08x}"),
            Data::Hex { kind, bytes } => {
                let mut out = match kind {
                    Some(k) => format!("hex({k:x}):"),
                    None => "hex:".to_string(),
                };
                let mut width = name.len() + 1 + out.len();
                for (i, b) in bytes.iter().enumerate() {
                    if width > 76 {
                        out.push_str("\\\n  ");
                        width = 2;
                    }
                    out.push_str(&format!("{b:02x}"));
                    width += 2;
                    if i + 1 < bytes.len() {
                        out.push(',');
                        width += 1;
                    }
                }
                out
            }
        };
        format!("{name}={data}")
    }
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

    // Nothing is deleted below unless the current settings are safe first.
    let backup = backup_dir.join(format!("settings-{stamp}.reg"));
    let backup = if Settings::read(prefix)?.is_some_and(|s| !s.values.is_empty()) {
        let exported = wine_cmd.run(&["reg", "export", &hkcu_key, &to_wine_path(&backup), "/y"])?;
        if !exported || !backup.is_file() {
            return Err(Error::Config(format!(
                "Wine could not back up the current settings, so nothing was changed (log: {})",
                log.display()
            )));
        }
        Some(backup)
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
        let Data::Hex { kind: None, bytes } = &s.values[1].data else { panic!() };
        assert_eq!(bytes.last(), Some(&0));
        assert_eq!(bytes.len(), 23);
        assert_eq!(s.values[0].data, Data::Dword(0));
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
        assert_eq!(s.values[1].data, Data::Hex { kind: None, bytes: b"Default\0".to_vec() });
        assert_eq!(s.values[1].to_reg_line(), "\"selected_theme_h1228781698\"=hex:44,65,66,61,75,6c,74,00");
    }

    #[test]
    fn nothing_but_values_of_the_key_reaches_the_import() {
        // A continuation that is not hex data, a header smuggled in as one,
        // a carriage return inside a line and an unknown kind.
        let text = "[Software\\\\Cikoria\\\\Runehaven] 1\n\
\"a\"=dword:00000001\\\n\
[HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run]\n\
\"b\"=hex:01,\\\n\
[HKEY_LOCAL_MACHINE\\Software]\n\
\"c\"=\"x\ry\"\n\
\"d\"=qword:01\n\
\"e\"=\"fine \\\"quoted\\\" \\\\ text\"\n\
\"f\"=hex:02,\\\n\
  03\n";
        let s = Settings::parse(text).unwrap();
        let names: Vec<&str> = s.values.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["e", "f"]);
        assert_eq!(s.skipped, 4);
        assert_eq!(s.values[0].data, Data::String("fine \"quoted\" \\ text".into()));
        let out = s.to_reg_file();
        assert_eq!(out.matches('[').count(), 1, "only our own key: {out}");
        assert_eq!(Settings::parse(&out).unwrap().values, s.values);
    }

    #[test]
    fn long_binary_values_are_wrapped_and_read_back() {
        let v = Value { name: "rebinds_h1".into(), data: Data::Hex { kind: Some(3), bytes: (0..=255).collect() } };
        let s = Settings { values: vec![v], skipped: 0 };
        let out = s.to_reg_file();
        assert!(out.lines().all(|l| l.len() <= 80));
        assert_eq!(Settings::parse(&out).unwrap().values, s.values);
    }

    #[test]
    fn odd_section_names_do_not_crash() {
        // The root's length ends inside the Ü.
        assert_eq!(section_key("[HKEY_CURRENT_USERÜ]").as_deref(), Some("HKEY_CURRENT_USERÜ"));
        assert_eq!(section_key("[HKCUÜ]").as_deref(), Some("HKCUÜ"));
    }

    #[test]
    fn missing_key_is_none() {
        assert!(Settings::parse("WINE REGISTRY Version 2\n[Software\\\\Wine] 1\n").is_none());
    }

    #[test]
    fn escaped_names() {
        let v = parse_value(&[r#""a\"b\\c"=dword:1"#]).unwrap();
        assert_eq!(v.name, r#"a"b\c"#);
        assert_eq!(v.to_reg_line(), r#""a\"b\\c"=dword:00000001"#);
    }
}
