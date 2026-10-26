use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `dir/name.ext`, or like the Finder `dir/name (2).ext`, `(3)` … when that
/// already exists, so nothing gets overwritten.
pub fn free_path(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{name}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{name} ({n}).{ext}")))
        .find(|p| !p.exists())
        .expect("some number is free")
}

/// `YYYYMMDDTHHMMSSZ` in UTC, for log file names.
pub fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    stamp_from_unix(secs)
}

pub fn stamp_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(super::stamp_from_unix(0), "19700101T000000Z");
        // 2026-10-15 16:13:02 UTC, the moment Steam rescheduled the test update.
        assert_eq!(super::stamp_from_unix(1_792_080_782), "20261015T161302Z");
    }

    #[test]
    fn free_path_never_overwrites() {
        let tmp = tempfile::tempdir().unwrap();
        let first = free_path(tmp.path(), "Runehaven saves", "zip");
        assert!(first.ends_with("Runehaven saves.zip"));
        std::fs::write(&first, "").unwrap();
        let second = free_path(tmp.path(), "Runehaven saves", "zip");
        assert!(second.ends_with("Runehaven saves (2).zip"));
        std::fs::write(&second, "").unwrap();
        assert!(free_path(tmp.path(), "Runehaven saves", "zip").ends_with("Runehaven saves (3).zip"));
    }
}
