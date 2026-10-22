//! Starting Runehaven from the Steam-managed installation through Wine.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::config::Config;
use crate::error::{Error, IoContext, Result};
use crate::paths::{to_wine_path, LauncherPaths};
use crate::runtime::{Runtime, RuntimeSpec};
use crate::util::utc_stamp;

pub const GAME_EXE: &str = "Runehaven.exe";
const INPUTFIX_NAME: &str = "version.dll";

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
                let rt = Runtime::locate(launcher, &RuntimeSpec::bundled());
                if !rt.is_installed() {
                    return Err(Error::Config(
                        "the Wine runtime is not installed yet (run `rhl runtime install`)".into(),
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
        let args = vec![
            GAME_EXE.to_string(),
            "-screen-fullscreen".into(),
            if unity_fullscreen { "1" } else { "0" }.into(),
            "-screen-width".into(),
            (d.width * scale).to_string(),
            "-screen-height".into(),
            (d.height * scale).to_string(),
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

/// Copies the input-fix DLL next to the game if it is missing or differs.
/// Returns `true` when a copy was made.
pub fn ensure_inputfix(dll: &Path, game_dir: &Path) -> Result<bool> {
    let wanted = std::fs::read(dll).ctx(|| format!("reading input fix {}", dll.display()))?;
    let target = game_dir.join(INPUTFIX_NAME);
    if std::fs::read(&target).ok().as_deref() == Some(wanted.as_slice()) {
        return Ok(false);
    }
    std::fs::write(&target, &wanted).ctx(|| format!("writing {}", target.display()))?;
    Ok(true)
}

/// Guards against starting the game twice.
pub struct RunLock {
    path: PathBuf,
}

impl RunLock {
    pub fn acquire(launcher: &LauncherPaths) -> Result<Self> {
        let path = launcher.root.join("run.pid");
        if let Ok(pid) = std::fs::read_to_string(&path) {
            if pid_alive(pid.trim()) {
                return Err(Error::AlreadyRunning);
            }
        }
        Ok(Self { path })
    }

    pub fn record(&self, pid: u32) -> Result<()> {
        std::fs::write(&self.path, pid.to_string())
            .ctx(|| format!("writing {}", self.path.display()))
    }

    pub fn release(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn pid_alive(pid: &str) -> bool {
    !pid.is_empty()
        && pid.chars().all(|c| c.is_ascii_digit())
        && Command::new("/bin/kill")
            .args(["-0", pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// Starts Wine. The caller keeps the returned child and the lock until the
/// game exits.
pub fn spawn(plan: &LaunchPlan, launcher: &LauncherPaths) -> Result<(Child, RunLock)> {
    let lock = RunLock::acquire(launcher)?;
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
    lock.record(child.id())?;
    Ok((child, lock))
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
    fn inputfix_is_copied_only_when_needed() {
        let tmp = tempfile::tempdir().unwrap();
        let dll = tmp.path().join("fix.dll");
        std::fs::write(&dll, b"MZ fix").unwrap();
        assert!(ensure_inputfix(&dll, tmp.path()).unwrap());
        assert!(!ensure_inputfix(&dll, tmp.path()).unwrap());
        std::fs::write(tmp.path().join("version.dll"), b"old").unwrap();
        assert!(ensure_inputfix(&dll, tmp.path()).unwrap());
    }
}
