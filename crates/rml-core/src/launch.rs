//! Starting Runehaven from the Steam-managed installation through Wine.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde::Serialize;

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::lock::JobLock;
use crate::paths::{to_wine_path, LauncherPaths, SteamPaths};
use crate::prefix;
use crate::runtime::{Runtime, RuntimeSpec};
use crate::steam::{library, AppManifest, Library, RUNEHAVEN_APPID};
use crate::util::utc_stamp;

pub const GAME_EXE: &str = "Runehaven.exe";
const INPUTFIX_NAME: &str = "version.dll";
/// Text in every build of the launcher's input fix (its log line), which
/// tells it apart from another `version.dll`, a mod for example.
const INPUTFIX_MARK: &[u8] = b"Runehaven Launcher input fix";
const MACFIX_NAME: &str = "rml-macfix.dylib";

/// Wine DLL settings: prefer the input-fix `version.dll` next to the game,
/// fall back to Wine's own; keep Wine from creating menu entries and from
/// offering Mono/Gecko installers.
pub const DLL_OVERRIDES: &str = "version=n,b;winemenubuilder.exe=d;mscoree,mshtml=";

/// Everything needed to start the game once.
#[derive(Debug, Clone)]
pub struct LaunchPlan {
    pub wine: PathBuf,
    pub game_dir: PathBuf,
    pub prefix: PathBuf,
    pub env: Vec<(String, String)>,
    pub args: Vec<String>,
    pub wine_log: PathBuf,
    pub player_log: PathBuf,
}

/// The Wine binary and helper-library folders to use: a manually configured
/// runtime if set, otherwise the launcher's own downloaded one.
#[derive(Debug, Clone)]
pub struct ResolvedRuntime {
    pub wine: PathBuf,
    pub library_dirs: Vec<PathBuf>,
}

impl ResolvedRuntime {
    pub fn resolve(config: &Config, launcher: &LauncherPaths) -> Result<Self> {
        let resolved = match &config.runtime.wine {
            Some(wine) => Self {
                wine: wine.clone(),
                library_dirs: config.runtime.library_dirs.clone(),
            },
            None => {
                let rt = Runtime::locate(config, launcher, &RuntimeSpec::bundled());
                if !rt.is_installed() {
                    return Err(Error::Config(
                        "the Wine runtime is not installed yet (install it from the launcher, or run `rml runtime install`)".into(),
                    ));
                }
                Self {
                    wine: rt.wine(),
                    library_dirs: vec![rt.library_dir()],
                }
            }
        };
        if !resolved.wine.is_file() {
            return Err(Error::Config(format!(
                "Wine not found at {}",
                resolved.wine.display()
            )));
        }
        Ok(resolved)
    }

    /// Value for `DYLD_FALLBACK_LIBRARY_PATH`.
    pub fn library_path(&self) -> String {
        let mut parts: Vec<String> = self
            .library_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        parts.push("/usr/lib".into());
        parts.join(":")
    }
}

/// The prefix to use: configured, or the launcher's own.
pub fn prefix_path(config: &Config, launcher: &LauncherPaths) -> PathBuf {
    config.prefix.clone().unwrap_or_else(|| launcher.prefix())
}

impl LaunchPlan {
    pub fn build(config: &Config, launcher: &LauncherPaths, game_dir: &Path) -> Result<Self> {
        let runtime = ResolvedRuntime::resolve(config, launcher)?;
        let wine = runtime.wine.clone();
        if !game_dir.join(GAME_EXE).is_file() {
            return Err(Error::GameNotInstalled);
        }
        let prefix = prefix_path(config, launcher);
        let stamp = utc_stamp();
        let logs = launcher.logs();
        let wine_log = logs.join(format!("wine-{stamp}.log"));
        let player_log = logs.join(format!("player-{stamp}.log"));

        let mut env = vec![
            (
                "WINEPREFIX".to_string(),
                prefix.to_string_lossy().into_owned(),
            ),
            (
                "DYLD_FALLBACK_LIBRARY_PATH".to_string(),
                runtime.library_path(),
            ),
            ("WINEDEBUG".to_string(), "-all,err+all".to_string()),
            ("WINEDLLOVERRIDES".to_string(), DLL_OVERRIDES.to_string()),
        ];
        env.extend(config.extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));

        let d = &config.display;
        // Full screen means macOS full screen on its own Space, like the green
        // window button: the game starts windowed and the Mac helper switches
        // the window over. Without the helper, Unity's own full screen is used.
        // The helper also keeps the game's cursor after clicks.
        let macfix = config.macfix_dylib.as_ref().filter(|p| p.is_file());
        let unity_fullscreen = d.fullscreen && macfix.is_none();
        if let Some(dylib) = macfix {
            env.push((
                "DYLD_INSERT_LIBRARIES".to_string(),
                dylib.to_string_lossy().into_owned(),
            ));
            env.push(("RML_GAME_PROCESS".to_string(), GAME_EXE.to_string()));
            env.push((
                "RML_FULLSCREEN".to_string(),
                if d.fullscreen { "1" } else { "0" }.to_string(),
            ));
        }
        // Unity takes the size in pixels; in Retina mode a point is 2×2 pixels
        // on Apple's built-in and most external displays.
        let scale = if d.retina { 2 } else { 1 };
        let pixels = |points: u32| {
            points
                .checked_mul(scale)
                .ok_or_else(|| Error::Config(format!("the window size {}x{} is too large", d.width, d.height)))
        };
        let args = vec![
            GAME_EXE.to_string(),
            "-screen-fullscreen".into(),
            if unity_fullscreen { "1" } else { "0" }.into(),
            "-screen-width".into(),
            pixels(d.width)?.to_string(),
            "-screen-height".into(),
            pixels(d.height)?.to_string(),
            "-force-d3d11".into(),
            "-logFile".into(),
            to_wine_path(&player_log),
        ];
        Ok(Self {
            wine,
            game_dir: game_dir.to_path_buf(),
            prefix,
            env,
            args,
            wine_log,
            player_log,
        })
    }

    /// Readable one-line form for logs and diagnostics.
    pub fn describe(&self) -> String {
        let env: Vec<String> = self.env.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
        format!(
            "cd {:?} && {} {:?} {}",
            self.game_dir,
            env.join(" "),
            self.wine,
            self.args.join(" ")
        )
    }
}

/// What putting the input fix in place did.
#[derive(Debug, PartialEq)]
pub struct Placed {
    /// Another `version.dll` was there (a mod, say); its copy went here.
    pub moved_aside: Option<PathBuf>,
}

/// Whether `found` is a build of the launcher's own input fix: the one this
/// launcher ships (`ours`) or an older one.
pub(crate) fn is_own_inputfix(found: &[u8], ours: Option<&[u8]>) -> bool {
    ours == Some(found) || found.windows(INPUTFIX_MARK.len()).any(|w| w == INPUTFIX_MARK)
}

/// Copies the input-fix DLL next to the game if it is missing or differs.
/// Another `version.dll` is copied into `backups` first. The new file is
/// written beside the old one and then renamed over it, so a game that still
/// has the old one open keeps it, and a link in its place is replaced, not
/// followed. Returns `None` when nothing had to change.
pub fn ensure_inputfix(dll: &Path, game_dir: &Path, backups: &Path) -> Result<Option<Placed>> {
    let wanted = fs::read(dll).ctx(|| format!("reading input fix {}", dll.display()))?;
    let target = game_dir.join(INPUTFIX_NAME);
    let found = fs::read(&target).ok();
    if found.as_deref() == Some(wanted.as_slice()) {
        return Ok(None);
    }
    let moved_aside = match found {
        Some(other) if !is_own_inputfix(&other, None) => {
            fs::create_dir_all(backups).ctx(|| format!("creating {}", backups.display()))?;
            let copy = crate::util::free_path(backups, &format!("game version.dll {}", utc_stamp()), "dll");
            fs::write(&copy, other).ctx(|| format!("writing {}", copy.display()))?;
            Some(copy)
        }
        _ => None,
    };
    let tmp = game_dir.join(format!(".{INPUTFIX_NAME}.rml-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&wanted).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&tmp, &target));
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e).ctx(|| format!("writing {}", target.display()));
    }
    Ok(Some(Placed { moved_aside }))
}

/// A helper file shipped with the launcher: next to the program or in
/// `../Resources` (the layout inside a macOS app bundle).
pub fn bundled_file(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join(name), dir.join("../Resources").join(name)]
        .into_iter()
        .find(|p| p.is_file())
}

/// Where Steam keeps the game: its library, manifest and game folder.
pub fn locate_game() -> Result<(Library, AppManifest, PathBuf)> {
    let libs = library::libraries(&SteamPaths::detect()?)?;
    let (lib, manifest) =
        library::find_app(&libs, RUNEHAVEN_APPID)?.ok_or(Error::GameNotInstalled)?;
    let game_dir = manifest.game_dir(&lib)?;
    Ok((lib, manifest, game_dir))
}

/// Loads the configuration, finds the game and builds the plan without
/// changing anything on disk. Without explicit paths in the configuration,
/// the helper files shipped with the launcher are used. Unless `force` is
/// set, this refuses while Steam is updating the game.
pub fn prepare(launcher: &LauncherPaths, force: bool) -> Result<(Config, LaunchPlan)> {
    let mut config = Config::load(&launcher.config_file())?;
    if config.inputfix_dll.is_none() {
        config.inputfix_dll = bundled_file(INPUTFIX_NAME);
    }
    if config.macfix_dylib.is_none() {
        config.macfix_dylib = bundled_file(MACFIX_NAME);
    }
    let (_, manifest, game_dir) = locate_game()?;
    if !manifest.is_ready() && !force {
        return Err(Error::GameNeedsUpdate);
    }
    let plan = LaunchPlan::build(&config, launcher, &game_dir)?;
    Ok((config, plan))
}

/// Identifies the Mac helper a start uses: its path and modification time.
/// The first start with a new one takes longer (on an M5 Pro about 30
/// seconds), because macOS checks it and Rosetta translates it once.
pub fn helper_stamp(config: &Config) -> Option<String> {
    let path = config.macfix_dylib.as_ref()?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}@{secs}", path.display()))
}

/// Steps on the way to a running game, for progress messages.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum StartEvent {
    /// First start only, takes about a minute.
    SettingUpPrefix { prefix: PathBuf },
    PrefixReady,
    RetinaChanged { on: bool },
    InputFixPlaced,
    /// Another `version.dll` was in the game folder; a copy is at `to`.
    ForeignDllMovedAside { to: PathBuf },
    /// The save games could not be backed up; the game starts anyway.
    BackupFailed { message: String },
}

/// Creates the Wine prefix if it does not exist yet.
pub fn ensure_prefix(
    config: &Config,
    launcher: &LauncherPaths,
    on_event: &mut dyn FnMut(StartEvent),
) -> Result<()> {
    let prefix = prefix_path(config, launcher);
    if prefix::is_initialized(&prefix) {
        return Ok(());
    }
    let rt = ResolvedRuntime::resolve(config, launcher)?;
    on_event(StartEvent::SettingUpPrefix {
        prefix: prefix.clone(),
    });
    prefix::init(
        &rt.wine,
        &rt.library_path(),
        &prefix,
        &launcher.logs().join("wineboot.log"),
        Duration::from_secs(300),
    )?;
    on_event(StartEvent::PrefixReady);
    Ok(())
}

/// Puts the prefix, the Retina setting and the input fix in place and starts
/// Wine. The caller holds `lock` from before [`prepare`] until the game
/// exits, then calls [`JobLock::game_exited`].
pub fn start(
    config: &Config,
    plan: &LaunchPlan,
    launcher: &LauncherPaths,
    lock: &JobLock,
    on_event: &mut dyn FnMut(StartEvent),
) -> Result<Child> {
    ensure_prefix(config, launcher, on_event)?;
    let rt = ResolvedRuntime::resolve(config, launcher)?;
    if prefix::ensure_retina(
        &rt.wine,
        &rt.library_path(),
        &plan.prefix,
        config.display.retina,
        &launcher.logs().join("wine-settings.log"),
    )? {
        on_event(StartEvent::RetinaChanged {
            on: config.display.retina,
        });
    }
    if let Some(dll) = &config.inputfix_dll {
        if let Some(placed) = ensure_inputfix(dll, &plan.game_dir, &launcher.root.join("backups"))? {
            on_event(StartEvent::InputFixPlaced);
            if let Some(to) = placed.moved_aside {
                on_event(StartEvent::ForeignDllMovedAside { to });
            }
        }
    }
    // A copy of the save games before a start, in case one gets damaged.
    let backups = launcher.root.join("backups/saves");
    if let Err(e) = prefix::backup_saves(&plan.prefix, &backups) {
        on_event(StartEvent::BackupFailed { message: e.to_string() });
    }
    spawn(plan, launcher, lock)
}

/// Asks the game to close, like clicking the close button of its window.
/// The game decides itself whether and how fast it quits.
pub fn request_close(config: &Config, launcher: &LauncherPaths) -> Result<()> {
    let rt = ResolvedRuntime::resolve(config, launcher)?;
    // Without /f, Wine's taskkill sends the game's windows a close message.
    run_in_prefix(config, launcher, &rt, &rt.wine, &["taskkill", "/im", GAME_EXE])
}

/// Ends every Wine process of the game's prefix at once, for a game that
/// hangs. Unsaved progress is lost.
pub fn force_close(config: &Config, launcher: &LauncherPaths) -> Result<()> {
    let rt = ResolvedRuntime::resolve(config, launcher)?;
    let wineserver = rt.wine.with_file_name("wineserver");
    run_in_prefix(config, launcher, &rt, &wineserver, &["-k"])
}

fn run_in_prefix(
    config: &Config,
    launcher: &LauncherPaths,
    rt: &ResolvedRuntime,
    program: &Path,
    args: &[&str],
) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .env("WINEPREFIX", prefix_path(config, launcher))
        .env("DYLD_FALLBACK_LIBRARY_PATH", rt.library_path())
        .env("WINEDEBUG", "-all")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ctx(|| format!("starting {}", program.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "{} exited with {status}",
            program.display()
        )))
    }
}

/// What the game's own log tells about a running game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSignal {
    /// The first scene is loaded, so the main menu is up.
    Loaded,
    /// Unity's crash handler caught a crash.
    Crashed,
}

/// Follows the Unity log (`Player.log`) of one start while it is written.
pub struct PlayerLogWatch {
    path: PathBuf,
    offset: u64,
    /// The last line, as long as it is not complete yet.
    partial: String,
}

impl PlayerLogWatch {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            offset: 0,
            partial: String::new(),
        }
    }

    /// Reads what was added since the last call and reports what it means.
    /// A log that does not exist yet simply has nothing new.
    pub fn poll(&mut self) -> Vec<LogSignal> {
        use std::io::{Read, Seek, SeekFrom};
        let mut new = Vec::new();
        if let Ok(mut f) = File::open(&self.path) {
            if f.seek(SeekFrom::Start(self.offset)).is_ok() {
                let _ = f.read_to_end(&mut new);
            }
        }
        self.offset += new.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&new));
        let Some(end) = self.partial.rfind('\n') else {
            return Vec::new();
        };
        let complete: String = self.partial.drain(..=end).collect();
        complete
            .lines()
            .filter_map(|line| {
                // Unity writes this after loading a scene, the first time for the menu.
                if line.starts_with("UnloadTime:") {
                    Some(LogSignal::Loaded)
                // Unity's crash handler marks a crash with this line.
                } else if line.trim_end() == "Crash!!!" {
                    Some(LogSignal::Crashed)
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Starts Wine. The caller keeps the returned child and `lock` until the
/// game exits.
pub fn spawn(plan: &LaunchPlan, launcher: &LauncherPaths, lock: &JobLock) -> Result<Child> {
    std::fs::create_dir_all(launcher.logs()).ctx(|| "creating log folder".into())?;
    std::fs::create_dir_all(&plan.prefix)
        .ctx(|| format!("creating prefix {}", plan.prefix.display()))?;
    let log =
        File::create(&plan.wine_log).ctx(|| format!("creating {}", plan.wine_log.display()))?;
    let log_err = log.try_clone().ctx(|| "duplicating log handle".into())?;
    let child = Command::new(&plan.wine)
        .args(&plan.args)
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&plan.game_dir)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .spawn()
        .ctx(|| format!("starting {}", plan.wine.display()))?;
    lock.record_game(child.id())?;
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_contains_the_essentials() {
        let tmp = tempfile::tempdir().unwrap();
        let wine = tmp.path().join("wine");
        std::fs::write(&wine, "").unwrap();
        let game = tmp.path().join("Runehaven");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join(GAME_EXE), "").unwrap();
        let mut config = Config::default();
        config.runtime.wine = Some(wine);
        config.runtime.library_dirs = vec!["/rt/lib".into()];
        let launcher = LauncherPaths::at(tmp.path().join("l"));

        let plan = LaunchPlan::build(&config, &launcher, &game).unwrap();
        let env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        assert_eq!(env["WINEDLLOVERRIDES"], DLL_OVERRIDES);
        assert_eq!(env["DYLD_FALLBACK_LIBRARY_PATH"], "/rt/lib:/usr/lib");
        assert_eq!(plan.args[0], GAME_EXE);
        assert!(plan.args.contains(&"-force-d3d11".to_string()));
        assert!(plan.args.last().unwrap().starts_with("Z:\\"));
    }

    #[test]
    fn fullscreen_uses_the_mac_helper_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let wine = tmp.path().join("wine");
        std::fs::write(&wine, "").unwrap();
        let game = tmp.path().join("Runehaven");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join(GAME_EXE), "").unwrap();
        let mut config = Config::default();
        config.runtime.wine = Some(wine);
        config.display.fullscreen = true;
        let launcher = LauncherPaths::at(tmp.path().join("l"));
        let arg_after = |plan: &LaunchPlan, flag: &str| {
            let i = plan.args.iter().position(|a| a == flag).unwrap();
            plan.args[i + 1].clone()
        };

        // Without the helper: Unity's own full screen; Retina doubles the size.
        let plan = LaunchPlan::build(&config, &launcher, &game).unwrap();
        assert_eq!(arg_after(&plan, "-screen-fullscreen"), "1");
        assert_eq!(arg_after(&plan, "-screen-width"), "2560");
        assert!(!plan.env.iter().any(|(k, _)| k == "DYLD_INSERT_LIBRARIES"));

        // With it: start windowed, the helper switches to macOS full screen.
        let dylib = tmp.path().join("rml-macfix.dylib");
        std::fs::write(&dylib, "").unwrap();
        config.macfix_dylib = Some(dylib.clone());
        let plan = LaunchPlan::build(&config, &launcher, &game).unwrap();
        let env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        assert_eq!(arg_after(&plan, "-screen-fullscreen"), "0");
        assert_eq!(env["DYLD_INSERT_LIBRARIES"], dylib.to_string_lossy());
        assert_eq!(env["RML_FULLSCREEN"], "1");
        assert_eq!(env["RML_GAME_PROCESS"], GAME_EXE);
    }

    #[test]
    fn missing_runtime_or_game_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let launcher = LauncherPaths::at(tmp.path());
        assert!(matches!(
            LaunchPlan::build(&Config::default(), &launcher, tmp.path()),
            Err(Error::Config(_))
        ));
        let mut config = Config::default();
        let wine = tmp.path().join("wine");
        std::fs::write(&wine, "").unwrap();
        config.runtime.wine = Some(wine);
        assert!(matches!(
            LaunchPlan::build(&config, &launcher, tmp.path()),
            Err(Error::GameNotInstalled)
        ));
    }

    #[test]
    fn player_log_watch_reports_menu_and_crash_once() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("player.log");
        let mut watch = PlayerLogWatch::new(&path);
        assert!(watch.poll().is_empty(), "no log yet");

        let mut log = File::create(&path).unwrap();
        write!(log, "Initialize engine version: 6000.2.7f2\r\nUnloadTi").unwrap();
        assert!(watch.poll().is_empty(), "a half-written line is not read yet");
        write!(log, "me: 1.16 ms\r\nUnloadTime: 3.5 ms\r\n").unwrap();
        assert_eq!(watch.poll(), vec![LogSignal::Loaded, LogSignal::Loaded]);
        assert!(watch.poll().is_empty(), "nothing new");
        write!(log, "Could not create SDK Material\nCrash!!!\n").unwrap();
        assert_eq!(watch.poll(), vec![LogSignal::Crashed]);
    }

    #[test]
    fn inputfix_is_copied_only_when_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let (game, backups) = (tmp.path().join("game"), tmp.path().join("backups"));
        std::fs::create_dir_all(&game).unwrap();
        let dll = tmp.path().join("fix.dll");
        std::fs::write(&dll, b"MZ Runehaven Launcher input fix 2").unwrap();
        let placed = |p: Option<PathBuf>| Some(Placed { moved_aside: p });
        assert_eq!(ensure_inputfix(&dll, &game, &backups).unwrap(), placed(None));
        assert_eq!(ensure_inputfix(&dll, &game, &backups).unwrap(), None);

        // An older build of the launcher's own is simply replaced.
        std::fs::write(game.join("version.dll"), b"MZ Runehaven Launcher input fix 1").unwrap();
        assert_eq!(ensure_inputfix(&dll, &game, &backups).unwrap(), placed(None));

        // Someone else's is kept as a copy first.
        std::fs::write(game.join("version.dll"), b"MZ a mod").unwrap();
        let copy = ensure_inputfix(&dll, &game, &backups).unwrap().unwrap().moved_aside.unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"MZ a mod");
        assert_eq!(std::fs::read(game.join("version.dll")).unwrap(), std::fs::read(&dll).unwrap());

        // A link in its place is replaced, its target stays as it was.
        let elsewhere = tmp.path().join("elsewhere.dll");
        std::fs::write(&elsewhere, b"MZ not to be touched").unwrap();
        std::fs::remove_file(game.join("version.dll")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, game.join("version.dll")).unwrap();
        ensure_inputfix(&dll, &game, &backups).unwrap();
        assert_eq!(std::fs::read(&elsewhere).unwrap(), b"MZ not to be touched");
        assert!(!std::fs::symlink_metadata(game.join("version.dll")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_dir(&game).unwrap().count(), 1, "no temporary file is left");
    }

    #[test]
    fn absurd_window_sizes_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let wine = tmp.path().join("wine");
        std::fs::write(&wine, "").unwrap();
        std::fs::write(tmp.path().join(GAME_EXE), "").unwrap();
        let mut config = Config::default();
        config.runtime.wine = Some(wine);
        config.display.width = u32::MAX;
        let launcher = LauncherPaths::at(tmp.path().join("l"));
        assert!(matches!(LaunchPlan::build(&config, &launcher, tmp.path()), Err(Error::Config(_))));
    }
}
