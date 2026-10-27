//! Talking to the running Steam client through its console.
//!
//! Observed behaviour this relies on (macOS Steam client, October 2026):
//! * Running the Steam bootstrapper with a `+command` argument while Steam is
//!   already running hands the command line to the running client, which
//!   logs `ExecCommandLine: "…"` and executes it.
//! * The command must be a single argument and end with ` -`. Without that
//!   terminator Steam appends a stray quote to the last word.
//! * Answers appear in `logs/console_log.txt`.
//!
//! None of this is documented by Valve, so every answer is verified by reading
//! it back instead of being assumed.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, IoContext, Result};

/// Builds the single argument handed to the bootstrapper: `+cmd arg… -`.
pub fn command_argument(command: &str, args: &[&str]) -> Result<String> {
    let tokens = std::iter::once(command).chain(args.iter().copied());
    let mut parts = Vec::new();
    for token in tokens {
        let ok = !token.is_empty()
            && !token.starts_with('+')
            && !token.starts_with('-')
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_@./:".contains(c));
        if !ok {
            return Err(Error::UnsafeToken(token.to_string()));
        }
        parts.push(token);
    }
    let argument = format!("+{} -", parts.join(" "));
    // Steam copies the command into a 512-byte buffer; stay well below it.
    if argument.len() > 400 {
        return Err(Error::UnsafeToken(argument));
    }
    Ok(argument)
}

/// Removes a leading `[YYYY-MM-DD HH:MM:SS] ` timestamp if present.
pub fn strip_timestamp(line: &str) -> &str {
    let bytes = line.as_bytes();
    if bytes.first() == Some(&b'[') {
        if let Some(end) = line.find("] ") {
            if end <= 24 {
                return &line[end + 2..];
            }
        }
    }
    line
}

/// Reads what Steam appended to its console log since a given position.
#[derive(Debug)]
pub struct LogTail {
    path: PathBuf,
    offset: u64,
}

impl LogTail {
    /// Starts at the current end of the log.
    pub fn at_end(path: &Path) -> Result<Self> {
        let len = std::fs::metadata(path)
            .map_err(|_| Error::ConsoleLogMissing(path.to_path_buf()))?
            .len();
        Ok(Self {
            path: path.to_path_buf(),
            offset: len,
        })
    }

    /// Everything written since the start position (not consumed, so callers
    /// can re-read the growing answer).
    pub fn read_since_start(&self) -> Result<String> {
        let mut file = File::open(&self.path).ctx(|| format!("opening {}", self.path.display()))?;
        let len = file
            .metadata()
            .ctx(|| "reading console log size".into())?
            .len();
        if len < self.offset {
            return Err(Error::LogRotated);
        }
        file.seek(SeekFrom::Start(self.offset))
            .ctx(|| "seeking console log".into())?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .ctx(|| "reading console log".into())?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Extracts the answer to `argument` from freshly appended log text: the lines
/// after the matching `ExecCommandLine:` line, up to the next command.
/// Returns `None` while the command has not shown up yet.
pub fn extract_response(new_text: &str, argument: &str) -> Option<Vec<String>> {
    let quoted = format!("'{argument}'");
    let mut lines = new_text.lines().map(strip_timestamp);
    lines
        .by_ref()
        .find(|l| l.contains("ExecCommandLine:") && l.contains(&quoted))?;
    Some(
        lines
            .take_while(|l| !l.contains("ExecCommandLine:"))
            .filter(|l| !l.trim().is_empty() && !l.contains("SlowJSMethodCall"))
            .map(str::to_string)
            .collect(),
    )
}

/// Steam's own words for a refused command.
pub fn rejection(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .find(|l| l.contains("command not found") || l.contains("not the GM"))
        .map(|l| l.trim().to_string())
}

/// Reads `@sSteamCmdForcePlatformType = "…"` from the answer to
/// `find sSteamCmdForcePlatformType` or to a setter.
pub fn parse_platform(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|l| {
        let rest = l.split("@sSteamCmdForcePlatformType").nth(1)?;
        let rest = rest.trim_start_matches('"').trim_start();
        let rest = rest.strip_prefix('=')?.trim_start();
        let rest = rest.strip_prefix('"')?;
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    })
}

/// The useful parts of `app_status <appid>`.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct AppStatus {
    pub name: Option<String>,
    /// Entries inside the parentheses of "release state", e.g.
    /// `Subscribed`, `Invalid Platform`, `Permanent`.
    pub release_flags: Vec<String>,
    pub install_state: Option<String>,
    pub install_dir: Option<String>,
    pub build_id: Option<u64>,
    pub update_state: Option<String>,
}

impl AppStatus {
    pub fn owned(&self) -> bool {
        self.release_flags
            .iter()
            .any(|f| f.eq_ignore_ascii_case("Subscribed"))
    }

    pub fn fully_installed(&self) -> bool {
        self.install_state
            .as_deref()
            .is_some_and(|s| s.contains("Fully Installed"))
    }

    /// Steam's log may split one printed line into several log lines (the
    /// console window shows them joined). Lines that do not start a new field
    /// are therefore glued to the field before them.
    fn logical_lines(lines: &[String]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for line in lines {
            let t = line.trim_start();
            let starts_field = t.starts_with("- ") || t.starts_with("AppID ");
            match out.last_mut() {
                Some(prev) if !starts_field => prev.push_str(line),
                _ => out.push(line.clone()),
            }
        }
        out
    }

    pub fn parse(lines: &[String]) -> Option<Self> {
        let mut status = AppStatus::default();
        let mut seen = false;
        for line in Self::logical_lines(lines) {
            let l = line.trim();
            if let Some(rest) = l.strip_prefix("AppID ") {
                seen = true;
                if let (Some(a), Some(b)) = (rest.find('('), rest.rfind(')')) {
                    status.name = Some(rest[a + 1..b].to_string());
                }
            } else if let Some(rest) = l.strip_prefix("- release state:") {
                if let (Some(a), Some(b)) = (rest.find('('), rest.rfind(')')) {
                    status.release_flags = rest[a + 1..b]
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                }
            } else if let Some(rest) = l.strip_prefix("- install state:") {
                status.install_state = Some(rest.trim().trim_end_matches(',').to_string());
            } else if let Some(rest) = l.strip_prefix("- install dir:") {
                status.install_dir = Some(rest.trim().trim_matches('"').to_string());
            } else if let Some(rest) = l.strip_prefix("- size on disk:") {
                status.build_id = rest
                    .split("BuildID")
                    .nth(1)
                    .and_then(|b| b.trim().parse().ok());
            } else if let Some(rest) = l.strip_prefix("- update state:") {
                status.update_state = Some(rest.trim().to_string());
            }
        }
        seen.then_some(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    #[test]
    fn builds_terminated_arguments() {
        assert_eq!(
            command_argument("@sSteamCmdForcePlatformType", &["windows"]).unwrap(),
            "+@sSteamCmdForcePlatformType windows -"
        );
        assert_eq!(
            command_argument("app_install", &["2676030", "1"]).unwrap(),
            "+app_install 2676030 1 -"
        );
        for bad in ["a b", "x\"", "it's", "-console", "+quit", "", "a;b"] {
            assert!(
                command_argument("find", &[bad]).is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn strips_timestamps() {
        assert_eq!(
            strip_timestamp("[2026-10-16 00:21:03] ExecCommandLine: x"),
            "ExecCommandLine: x"
        );
        assert_eq!(strip_timestamp("ExecCommandLine: x"), "ExecCommandLine: x");
        assert_eq!(
            strip_timestamp("[not a timestamp at all, just text] y"),
            "[not a timestamp at all, just text] y"
        );
    }

    #[test]
    fn extracts_find_answer() {
        let arg = "+find sSteamCmdForcePlatformType -";
        let log = "[2026-10-16 00:21:00] something earlier\n\
[2026-10-16 00:21:03] ExecCommandLine: \"'/Applications/Steam.app/Contents/MacOS/steam_osx' '+find sSteamCmdForcePlatformType -'\"\n\
[2026-10-16 00:21:03] @sSteamCmdForcePlatformType = \"windows\" - Must be one of: [windows | macos | linux | android]\n\
[2026-10-16 00:21:03] STARTUP SlowJSMethodCall UI.GetDesiredSteamUIWindows took 11.0ms\n\
[2026-10-16 00:21:09] ExecCommandLine: \"'/Applications/Steam.app/Contents/MacOS/steam_osx' '+app_status 1 -'\"\n\
[2026-10-16 00:21:09] AppID 1 (x):\n";
        let answer = extract_response(log, arg).unwrap();
        assert_eq!(answer.len(), 1);
        assert_eq!(parse_platform(&answer).as_deref(), Some("windows"));
        assert!(extract_response("ExecCommandLine: \"'x' '+other -'\"\n", arg).is_none());
    }

    #[test]
    fn parses_setter_echo_and_rejections() {
        assert_eq!(
            parse_platform(&lines("\"@sSteamCmdForcePlatformType\" = \"macos\"")).as_deref(),
            Some("macos")
        );
        assert_eq!(parse_platform(&lines("nothing here")), None);
        assert!(rejection(&lines(">>> command not found: foo")).is_some());
        assert!(rejection(&lines(
            "Attempted to change replicated CVar when you are not the GM"
        ))
        .is_some());
        assert!(rejection(&lines("all fine")).is_none());
    }

    #[test]
    fn parses_app_status() {
        let answer = lines(
            "AppID 2676030 (Runehaven): \n\
 - release state: released (Subscribed,Invalid Platform,Permanent,)\n\
 - owner account: 1234\n\
 - install state: Fully Installed,\n\
 - install dir: \"/Users/k/Library/Application Support/Steam/steamapps/common/Runehaven\"\n\
 - mounted depots: \n\
   2676031 : 345.23 MB (manifest 7388440364521527929)\n\
 - size on disk: 345226541 bytes, BuildID 23702508\n\
 - update state:  ( No Error )\n",
        );
        let s = AppStatus::parse(&answer).unwrap();
        assert!(s.owned());
        assert_eq!(s.update_state.as_deref(), Some("( No Error )"));
        assert!(s.fully_installed());
        assert_eq!(s.name.as_deref(), Some("Runehaven"));
        assert_eq!(
            s.release_flags,
            ["Subscribed", "Invalid Platform", "Permanent"]
        );
        assert_eq!(s.build_id, Some(23702508));
        assert!(s.install_dir.unwrap().ends_with("common/Runehaven"));
        assert!(AppStatus::parse(&lines("unrelated")).is_none());
    }

    /// Verbatim from Steam's console_log on macOS (16.10.2026), timestamps
    /// stripped and the owner line removed.
    #[test]
    fn parses_real_app_status_output() {
        let answer = lines(
            "AppID 2676030 (Runehaven): \n - release state: released (\nSubscribed,\nInvalid Platform,\nPermanent,\n)\n - install state: \nFully Installed,\n - install dir: \"/Users/k/games/SteamLibrary/steamapps/common/Runehaven\"\n - mounted depots: \n   2676031 : 345.23 MB (manifest 7388440364521527929)\n - size on disk: 345226541 bytes, BuildID 23702508\n - update started: Thu Jan  1 08:00:00 1970, staged: 0/0 MB 100%, downloaded: 0/0 MB 100% - 0 KB/s\n - update state: \n ( No Error )\n - user config: \n\"UserConfig\"\n{\n}\n",
        );
        let s = AppStatus::parse(&answer).unwrap();
        assert_eq!(
            s.release_flags,
            ["Subscribed", "Invalid Platform", "Permanent"]
        );
        assert_eq!(s.install_state.as_deref(), Some("Fully Installed"));
        assert_eq!(s.update_state.as_deref(), Some("( No Error )"));
        assert_eq!(s.build_id, Some(23702508));
    }

    #[test]
    fn parses_app_status_split_across_log_lines() {
        let answer = lines(
            "AppID 2676030 (Runehaven): \n\
 - release state: \n\
released \n\
(Subscribed,Invalid Platform,Permanent,)\n\
 - install state: \n\
Fully Installed,\n\
 - mounted depots: \n\
   2676031 : 345.23 MB (manifest 7388440364521527929)\n\
 - size on disk: 345226541 bytes, BuildID 23702508\n\
 - update state: \n\
 ( No Error )\n",
        );
        let s = AppStatus::parse(&answer).unwrap();
        assert!(s.owned());
        assert!(s.fully_installed());
        assert_eq!(s.build_id, Some(23702508));
        assert_eq!(s.update_state.as_deref(), Some("( No Error )"));
    }
}
