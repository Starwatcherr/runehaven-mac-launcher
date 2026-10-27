//! Looks among the GitHub releases for a newer version of the launcher.
//! Only the list of releases is fetched; nothing is downloaded or installed.

use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const RELEASES_API: &str = "https://api.github.com/repos/Starwatcherr/runehaven-mac-launcher/releases?per_page=20";

/// The page players download new versions from.
pub const RELEASES_PAGE: &str = "https://github.com/Starwatcherr/runehaven-mac-launcher/releases";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NewVersion {
    pub version: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
}

/// Asks GitHub for the releases and returns the newest one above `current`,
/// if there is one. Gives up after a few seconds.
pub fn check(current: &str) -> Result<Option<NewVersion>> {
    let body = crate::runtime::http_agent()
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(Duration::from_secs(8)))
        .build()
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| Error::Config(format!("could not check for a new version: {e}")))?;
    newest(current, &body)
}

/// Betas are only offered to people who already run a beta.
fn newest(current: &str, releases_json: &str) -> Result<Option<NewVersion>> {
    let current = Version::parse(current).map_err(|e| parse_error("the launcher version", e))?;
    let releases: Vec<Release> =
        serde_json::from_str(releases_json).map_err(|e| parse_error("GitHub's release list", e))?;
    Ok(releases
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| Version::parse(r.tag_name.trim_start_matches('v')).ok().map(|v| (v, r.html_url)))
        .filter(|(v, _)| v > &current && (v.pre.is_empty() || !current.pre.is_empty()))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(v, url)| NewVersion { version: v.to_string(), url }))
}

fn parse_error(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Parse { what: what.into(), detail: e.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASES: &str = r#"[
        {"tag_name": "v0.2.0-beta.1", "html_url": "https://example.invalid/b1", "draft": false, "prerelease": true},
        {"tag_name": "v0.1.1", "html_url": "https://example.invalid/011", "draft": false, "prerelease": false},
        {"tag_name": "v0.3.0", "html_url": "https://example.invalid/draft", "draft": true, "prerelease": false},
        {"tag_name": "nightly", "html_url": "https://example.invalid/n", "draft": false, "prerelease": true},
        {"tag_name": "v0.1.0-beta.1", "html_url": "https://example.invalid/old", "draft": false, "prerelease": true}
    ]"#;

    #[test]
    fn beta_users_get_the_newest_release() {
        let found = newest("0.1.0-beta.1", RELEASES).unwrap().unwrap();
        assert_eq!(found.version, "0.2.0-beta.1");
        assert_eq!(found.url, "https://example.invalid/b1");
    }

    #[test]
    fn stable_users_skip_betas_and_drafts() {
        assert_eq!(newest("0.1.0", RELEASES).unwrap().unwrap().version, "0.1.1");
        assert_eq!(newest("0.1.1", RELEASES).unwrap(), None);
    }

    #[test]
    fn the_final_release_beats_its_betas() {
        let json = r#"[{"tag_name": "v0.1.0", "html_url": "u"}]"#;
        assert_eq!(newest("0.1.0-beta.3", json).unwrap().unwrap().version, "0.1.0");
        assert_eq!(newest("0.1.0", json).unwrap(), None);
    }
}
