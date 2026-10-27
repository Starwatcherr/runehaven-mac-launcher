//! The Wine runtime the game runs on, assembled from official upstream
//! releases: WineCX (Sikarugir), DXMT (Direct3D 11 → Metal) and a handful of
//! support libraries from Gcenx's Wine Stable build.
//!
//! Nothing is redistributed by this project. The launcher downloads each
//! archive from its original release page, checks it against the SHA-256 in
//! `runtime.json` and assembles the runtime locally.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::paths::LauncherPaths;

/// The folder holding the downloaded runtime: chosen in the settings, or the
/// launcher's own.
pub fn runtime_dir(config: &Config, launcher: &LauncherPaths) -> PathBuf {
    config.runtime.dir.clone().unwrap_or_else(|| launcher.runtime())
}

const SPEC_JSON: &str = include_str!("../runtime.json");
const MARKER: &str = ".runehaven-runtime";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RuntimeSpec {
    pub id: String,
    /// Path of the wine binary inside the runtime.
    pub wine: String,
    /// Folder (inside the runtime) for `DYLD_FALLBACK_LIBRARY_PATH`.
    pub library_dir: String,
    pub components: Vec<Component>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Component {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub extract: Extract,
    /// Only these archive members are unpacked (speeds up big archives).
    #[serde(default)]
    pub members: Vec<String>,
    /// Single files to copy (for `extract: map`).
    #[serde(default)]
    pub files: Vec<FileMap>,
    /// Shorthand for many files from one folder: `from_dir/name` → `to_dir/name`.
    #[serde(default)]
    pub from_dir: Option<String>,
    #[serde(default)]
    pub to_dir: Option<String>,
    #[serde(default)]
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Extract {
    /// Unpack the whole archive into the runtime folder.
    All,
    /// Unpack into a temporary folder and copy selected files.
    Map,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct FileMap {
    pub from: String,
    pub to: String,
}

impl Component {
    /// All (from, to) pairs for `extract: map`.
    pub fn mappings(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .files
            .iter()
            .map(|f| (f.from.clone(), f.to.clone()))
            .collect();
        if let (Some(from), Some(to)) = (&self.from_dir, &self.to_dir) {
            out.extend(
                self.names
                    .iter()
                    .map(|n| (format!("{from}/{n}"), format!("{to}/{n}"))),
            );
        }
        out
    }

    fn archive_name(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or("download")
    }
}

impl RuntimeSpec {
    /// The runtime this launcher version is built and tested for.
    pub fn bundled() -> Self {
        serde_json::from_str(SPEC_JSON).expect("runtime.json is valid")
    }

    pub fn download_size(&self) -> u64 {
        self.components.iter().map(|c| c.size).sum()
    }
}

/// An assembled runtime on disk.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub root: PathBuf,
    pub spec: RuntimeSpec,
}

impl Runtime {
    /// The runtime in the folder the configuration names, or the launcher's own.
    pub fn locate(config: &Config, launcher: &LauncherPaths, spec: &RuntimeSpec) -> Self {
        Self::locate_in(&runtime_dir(config, launcher), spec)
    }

    pub fn locate_in(dir: &Path, spec: &RuntimeSpec) -> Self {
        Self {
            root: dir.join(&spec.id),
            spec: spec.clone(),
        }
    }

    pub fn wine(&self) -> PathBuf {
        self.root.join(&self.spec.wine)
    }

    pub fn wineserver(&self) -> PathBuf {
        self.wine().with_file_name("wineserver")
    }

    pub fn library_dir(&self) -> PathBuf {
        self.root.join(&self.spec.library_dir)
    }

    pub fn is_installed(&self) -> bool {
        fs::read_to_string(self.root.join(MARKER)).is_ok_and(|id| id.trim() == self.spec.id)
            && self.wine().is_file()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Downloading { name: String, done: u64, total: u64 },
    AlreadyDownloaded { name: String },
    Extracting { name: String },
    Done,
}

/// Runtimes of other versions the launcher downloaded earlier into `dir`.
/// One being there while the current version is not means an update.
pub fn older_runtimes(dir: &Path, spec: &RuntimeSpec) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            fs::read_to_string(p.join(MARKER)).is_ok_and(|id| id.trim() != spec.id)
        })
        .collect()
}

/// Downloads and assembles the runtime unless it is already there. Setting
/// `cancel` stops the download; what was finished stays for the next try.
pub fn install(
    config: &Config,
    launcher: &LauncherPaths,
    spec: &RuntimeSpec,
    keep_downloads: bool,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(RuntimeEvent),
) -> Result<Runtime> {
    let runtime = Runtime::locate(config, launcher, spec);
    if runtime.is_installed() {
        on_event(RuntimeEvent::Done);
        return Ok(runtime);
    }
    let downloads = launcher.root.join("downloads");
    fs::create_dir_all(&downloads).ctx(|| format!("creating {}", downloads.display()))?;
    // Unfinished downloads of an earlier run that crashed.
    for entry in fs::read_dir(&downloads).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().ends_with(".part") {
            let _ = fs::remove_file(entry.path());
        }
    }

    let mut archives = Vec::new();
    for c in &spec.components {
        let dest = downloads.join(c.archive_name());
        if file_matches(&dest, &c.sha256)? {
            on_event(RuntimeEvent::AlreadyDownloaded {
                name: c.name.clone(),
            });
        } else {
            download_verified(&c.url, &c.sha256, c.size, &dest, cancel, &mut |done, total| {
                on_event(RuntimeEvent::Downloading {
                    name: c.name.clone(),
                    done,
                    total,
                })
            })?;
        }
        archives.push(dest);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Interrupted);
    }

    let staging = runtime_dir(config, launcher).join(format!("{}.staging", spec.id));
    if staging.exists() {
        fs::remove_dir_all(&staging).ctx(|| format!("removing {}", staging.display()))?;
    }
    fs::create_dir_all(&staging).ctx(|| format!("creating {}", staging.display()))?;
    for (c, archive) in spec.components.iter().zip(&archives) {
        on_event(RuntimeEvent::Extracting {
            name: c.name.clone(),
        });
        assemble(c, archive, &staging)?;
    }

    let staged = Runtime {
        root: staging.clone(),
        spec: spec.clone(),
    };
    if !staged.wine().is_file() {
        return Err(Error::Config(format!(
            "runtime assembly incomplete: {} missing",
            staged.wine().display()
        )));
    }
    fs::write(staging.join(MARKER), &spec.id).ctx(|| "writing runtime marker".into())?;
    if runtime.root.exists() {
        fs::remove_dir_all(&runtime.root)
            .ctx(|| format!("removing old {}", runtime.root.display()))?;
    }
    fs::rename(&staging, &runtime.root)
        .ctx(|| format!("moving runtime into {}", runtime.root.display()))?;
    // An update leaves the previous version behind; it is no longer used.
    for old in older_runtimes(&runtime_dir(config, launcher), spec) {
        let _ = fs::remove_dir_all(old);
    }
    if !keep_downloads {
        for a in &archives {
            let _ = fs::remove_file(a);
        }
    }
    on_event(RuntimeEvent::Done);
    Ok(runtime)
}

fn assemble(c: &Component, archive: &Path, staging: &Path) -> Result<()> {
    match c.extract {
        Extract::All => untar(archive, &c.sha256, staging, &c.members),
        Extract::Map => {
            let tmp = staging.join(format!(".tmp-{}", c.archive_name()));
            fs::create_dir_all(&tmp).ctx(|| format!("creating {}", tmp.display()))?;
            untar(archive, &c.sha256, &tmp, &c.members)?;
            for (from, to) in c.mappings() {
                let src = tmp.join(&from);
                let dst = staging.join(&to);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent).ctx(|| format!("creating {}", parent.display()))?;
                }
                // fs::copy follows symlinks, so versioned library links become real files.
                fs::copy(&src, &dst).ctx(|| format!("copying {from} from {}", c.name))?;
            }
            fs::remove_dir_all(&tmp).ctx(|| format!("removing {}", tmp.display()))
        }
    }
}

/// Unpacks with the system `tar`, which handles .tar.xz and .tar.gz on macOS.
/// The archive is opened once, checked against `sha256`, and that same open
/// file is what `tar` reads, so a file swapped in between is never unpacked.
fn untar(archive: &Path, sha256: &str, into: &Path, members: &[String]) -> Result<()> {
    let mut file = File::open(archive).ctx(|| format!("opening {}", archive.display()))?;
    if sha256_reader(&mut file).ctx(|| format!("reading {}", archive.display()))? != sha256 {
        return Err(Error::Config(format!(
            "{} changed after it was downloaded; delete it and try again",
            archive.display()
        )));
    }
    file.seek(SeekFrom::Start(0)).ctx(|| format!("reading {}", archive.display()))?;
    let out = Command::new("/usr/bin/tar")
        .args(["-x", "-f", "-", "-C"])
        .arg(into)
        .args(members)
        .stdin(file)
        .output()
        .ctx(|| "running tar".into())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err
            .lines()
            .filter(|l| !l.contains("Ignoring unknown extended header"))
            .rev()
            .take(3)
            .collect();
        return Err(Error::Config(format!(
            "unpacking {} failed: {}",
            archive.display(),
            tail.join(" | ")
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).ctx(|| format!("opening {}", path.display()))?;
    sha256_reader(&mut file).ctx(|| format!("reading {}", path.display()))
}

fn sha256_reader(reader: &mut impl Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn file_matches(path: &Path, sha256: &str) -> Result<bool> {
    Ok(path.is_file() && sha256_file(path)? == sha256)
}

/// HTTP client that trusts the operating system's certificate store, so
/// downloads also work behind company proxies with their own certificates.
pub(crate) fn http_agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .user_agent(concat!("runehaven-mac-launcher/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// How long a download may go without receiving anything before it counts
/// as stalled.
const DOWNLOAD_STALL: Duration = Duration::from_secs(60);

/// Downloads `url` to `dest` while hashing; the file only appears under its
/// final name if size and checksum match. It stops as soon as more than
/// `size` bytes arrive, nothing arrives for a minute, or `cancel` is set.
pub fn download_verified(
    url: &str,
    sha256: &str,
    size: u64,
    dest: &Path,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let file_name = dest.file_name().unwrap_or_default().to_string_lossy();
    let part = dest.with_file_name(format!("{file_name}.{}.part", std::process::id()));
    let response = http_agent()
        .get(url)
        .config()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        // Only so that a reader left behind below ends some time.
        .timeout_recv_body(Some(Duration::from_secs(4 * 3600)))
        .build()
        .call()
        .map_err(|e| Error::Config(format!("download of {url} failed: {e}")))?;
    let mut reader = response.into_body().into_reader();
    // Reading happens on its own thread, so that cancelling or a stalled
    // connection ends the wait at once instead of when a read returns.
    let (tx, rx) = mpsc::sync_channel::<std::io::Result<Vec<u8>>>(16);
    std::thread::spawn(move || loop {
        let mut buf = vec![0u8; 1 << 16];
        let read = reader.read(&mut buf).map(|n| {
            buf.truncate(n);
            buf
        });
        let more = matches!(&read, Ok(chunk) if !chunk.is_empty());
        if tx.send(read).is_err() || !more {
            break;
        }
    });

    let _ = fs::remove_file(&part);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .ctx(|| format!("creating {}", part.display()))?;
    let mut receive = || -> Result<()> {
        let mut hasher = Sha256::new();
        let (mut done, mut last_report, mut last_data) = (0u64, 0u64, Instant::now());
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Interrupted);
            }
            let chunk = match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(read) => read.ctx(|| format!("downloading {url}"))?,
                Err(RecvTimeoutError::Timeout) if last_data.elapsed() < DOWNLOAD_STALL => continue,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::Config(format!(
                        "the download of {url} stalled (nothing arrived for {} seconds)",
                        DOWNLOAD_STALL.as_secs()
                    )))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::Config(format!("the download of {url} broke off")))
                }
            };
            if chunk.is_empty() {
                break;
            }
            last_data = Instant::now();
            done += chunk.len() as u64;
            if done > size {
                return Err(Error::Config(format!(
                    "download of {url} is larger than the expected {size} bytes"
                )));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).ctx(|| format!("writing {}", part.display()))?;
            if done - last_report >= 1 << 20 || done == size {
                on_progress(done, size);
                last_report = done;
            }
        }
        file.flush().ctx(|| format!("writing {}", part.display()))?;
        let actual = hex(&hasher.finalize());
        if done != size || actual != sha256 {
            return Err(Error::Config(format!(
                "download of {url} is not the expected file (got {done} bytes, SHA-256 {actual})"
            )));
        }
        Ok(())
    };
    let received = receive();
    drop(file);
    match received {
        Ok(()) => fs::rename(&part, dest).ctx(|| format!("moving {}", dest.display())),
        Err(e) => {
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

/// Finds the `wine` program in what the user picked: the program itself, a
/// Wine app (Wine Stable, CrossOver, Whisky's Wine) or a folder with
/// `bin/wine`. Returns it with its helper library folder, if there is one.
pub fn find_wine(choice: &Path) -> Option<(PathBuf, Vec<PathBuf>)> {
    let is_wine = |p: &Path| {
        p.is_file() && p.file_name().is_some_and(|n| n == "wine" || n == "wine64")
    };
    let wine = if is_wine(choice) {
        choice.to_path_buf()
    } else {
        [
            "bin/wine64",
            "bin/wine",
            "wswine.bundle/bin/wine",
            "Contents/Resources/wine/bin/wine64",
            "Contents/Resources/wine/bin/wine",
            "Contents/SharedSupport/CrossOver/bin/wine",
            "Contents/SharedSupport/wine/bin/wine64",
            "Contents/SharedSupport/wine/bin/wine",
        ]
        .iter()
        .map(|rel| choice.join(rel))
        .find(|p| is_wine(p))?
    };
    // Builds keep their libraries in ../lib next to bin/.
    let libs = wine
        .parent()
        .and_then(Path::parent)
        .map(|base| base.join("lib"))
        .filter(|l| l.is_dir())
        .into_iter()
        .collect();
    Some((wine, libs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_spec_is_consistent() {
        let spec = RuntimeSpec::bundled();
        assert_eq!(spec.components.len(), 3);
        assert!(spec
            .components
            .iter()
            .all(|c| c.sha256.len() == 64 && c.url.starts_with("https://")));
        let libs = &spec.components[2];
        assert_eq!(libs.mappings().len(), 23);
        assert!(libs
            .mappings()
            .iter()
            .any(|(_, to)| to == "lib/libinotify.0.dylib"));
        assert_eq!(spec.components[1].mappings().len(), 9);
    }

    /// Builds tiny archives with the system tar and runs the whole assembly.
    #[test]
    fn assembles_runtime_from_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(src.join("wine/wswine.bundle/bin")).unwrap();
        fs::write(src.join("wine/wswine.bundle/bin/wine"), "wine").unwrap();
        fs::create_dir_all(src.join("dx/v1/x64")).unwrap();
        fs::write(src.join("dx/v1/x64/d3d11.dll"), "dxmt").unwrap();
        fs::write(src.join("dx/v1/x64/unused.dll"), "no").unwrap();
        fs::create_dir_all(src.join("libs/App/lib")).unwrap();
        fs::write(src.join("libs/App/lib/libz.1.2.dylib"), "zlib").unwrap();
        std::os::unix::fs::symlink("libz.1.2.dylib", src.join("libs/App/lib/libz.1.dylib"))
            .unwrap();

        let tar = |dir: &str, name: &str| {
            let out = tmp.path().join(name);
            let ok = Command::new("tar")
                .arg("-czf")
                .arg(&out)
                .arg("-C")
                .arg(src.join(dir))
                .arg(".")
                .status()
                .unwrap();
            assert!(ok.success());
            out
        };
        let a_wine = tar("wine", "wine.tar.gz");
        let a_dx = tar("dx", "dx.tar.gz");
        let a_libs = tar("libs", "libs.tar.gz");
        let comp = |name: &str, path: &Path, extract| Component {
            name: name.into(),
            url: format!("file://{}", path.display()),
            sha256: sha256_file(path).unwrap(),
            size: fs::metadata(path).unwrap().len(),
            extract,
            members: vec![],
            files: vec![],
            from_dir: None,
            to_dir: None,
            names: vec![],
        };
        let mut dx = comp("dx", &a_dx, Extract::Map);
        dx.files = vec![FileMap {
            from: "v1/x64/d3d11.dll".into(),
            to: "wswine.bundle/lib/d3d11.dll".into(),
        }];
        let mut libs = comp("libs", &a_libs, Extract::Map);
        libs.from_dir = Some("App/lib".into());
        libs.to_dir = Some("lib".into());
        libs.names = vec!["libz.1.dylib".into()];
        let spec = RuntimeSpec {
            id: "test-1".into(),
            wine: "wswine.bundle/bin/wine".into(),
            library_dir: "lib".into(),
            components: vec![comp("wine", &a_wine, Extract::All), dx, libs],
        };

        // Pretend the archives were downloaded already (no network in tests).
        let launcher = LauncherPaths::at(tmp.path().join("launcher"));
        let downloads = launcher.root.join("downloads");
        fs::create_dir_all(&downloads).unwrap();
        for c in &spec.components {
            fs::copy(
                c.url.trim_start_matches("file://"),
                downloads.join(c.archive_name()),
            )
            .unwrap();
        }

        let mut events = Vec::new();
        let rt = install(&Config::default(), &launcher, &spec, true, &AtomicBool::new(false), &mut |e| events.push(e)).unwrap();
        assert!(rt.is_installed());
        assert_eq!(
            fs::read_to_string(rt.root.join("wswine.bundle/lib/d3d11.dll")).unwrap(),
            "dxmt"
        );
        assert!(!rt.root.join("wswine.bundle/lib/unused.dll").exists());
        let lib = rt.library_dir().join("libz.1.dylib");
        assert!(!fs::symlink_metadata(&lib).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(lib).unwrap(), "zlib");
        assert!(
            events
                .iter()
                .filter(|e| matches!(e, RuntimeEvent::AlreadyDownloaded { .. }))
                .count()
                == 3
        );
        assert!(!launcher.runtime().join("test-1.staging").exists());

        // Second call is a no-op.
        assert!(install(&Config::default(), &launcher, &spec, true, &AtomicBool::new(false), &mut |_| {})
            .unwrap()
            .is_installed());
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    /// Real download of the smallest component. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn downloads_and_verifies_dxmt() {
        let spec = RuntimeSpec::bundled();
        let c = &spec.components[1];
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join(c.archive_name());
        let mut calls = 0;
        let go = AtomicBool::new(false);
        download_verified(&c.url, &c.sha256, c.size, &dest, &go, &mut |_, _| calls += 1).unwrap();
        assert!(calls > 10);
        assert_eq!(sha256_file(&dest).unwrap(), c.sha256);
        // A wrong checksum must not leave a file behind.
        let bad = tmp.path().join("bad.tar.gz");
        assert!(download_verified(&c.url, &"0".repeat(64), c.size, &bad, &go, &mut |_, _| {}).is_err());
        // Cancelling leaves nothing behind either.
        let stopped = tmp.path().join("stopped.tar.gz");
        let stop = AtomicBool::new(true);
        assert!(matches!(
            download_verified(&c.url, &c.sha256, c.size, &stopped, &stop, &mut |_, _| {}),
            Err(Error::Interrupted)
        ));
        assert!(!stopped.exists() && !stopped.with_extension("part").exists());
        assert!(!bad.exists() && !bad.with_extension("part").exists());
    }

    #[test]
    fn finds_wine_in_what_the_user_picked() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("Wine Stable.app");
        let bin = app.join("Contents/Resources/wine/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(app.join("Contents/Resources/wine/lib")).unwrap();
        fs::write(bin.join("wine"), "").unwrap();
        let (wine, libs) = find_wine(&app).unwrap();
        assert_eq!(wine, bin.join("wine"));
        assert_eq!(libs, vec![app.join("Contents/Resources/wine/lib")]);
        assert_eq!(find_wine(&bin.join("wine")).unwrap().0, bin.join("wine"));
        assert!(find_wine(tmp.path()).is_none());
    }

    #[test]
    fn finds_older_runtimes_to_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = RuntimeSpec::bundled();
        let old = tmp.path().join("winecx-old");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join(MARKER), "winecx-old").unwrap();
        let current = tmp.path().join(&spec.id);
        fs::create_dir_all(&current).unwrap();
        fs::write(current.join(MARKER), &spec.id).unwrap();
        fs::create_dir_all(tmp.path().join("unrelated")).unwrap();
        assert_eq!(older_runtimes(tmp.path(), &spec), vec![old]);
    }
}
