//! Commands the interface calls, and the `game` event it listens to.

use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rhl_core::config::Config;
use rhl_core::launch::{self, LogSignal, PlayerLogWatch, StartEvent};
use rhl_core::paths::{LauncherPaths, SteamPaths};
use rhl_core::runtime::{Runtime, RuntimeSpec};
use rhl_core::steam::{MacTransport, Transport};
use rhl_core::{doctor, Error};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

/// One line of the requirements list on the home page.
#[derive(Debug, Serialize)]
pub struct Requirement {
    id: &'static str,
    name: &'static str,
    /// `ok`, `missing` (the launcher can do something about it) or `failed`.
    state: &'static str,
    detail: String,
}

fn ok(id: &'static str, name: &'static str, detail: impl Into<String>) -> Requirement {
    Requirement { id, name, state: "ok", detail: detail.into() }
}

fn missing(id: &'static str, name: &'static str, detail: impl Into<String>) -> Requirement {
    Requirement { id, name, state: "missing", detail: detail.into() }
}

fn failed(id: &'static str, name: &'static str, detail: impl Into<String>) -> Requirement {
    Requirement { id, name, state: "failed", detail: detail.into() }
}

fn rosetta() -> Requirement {
    const NAME: &str = "Rosetta 2";
    if doctor::rosetta_installed() {
        ok("rosetta", NAME, "Installed")
    } else {
        missing("rosetta", NAME, "Not installed")
    }
}

fn steam() -> Requirement {
    const NAME: &str = "Steam";
    match SteamPaths::detect() {
        Ok(paths) if MacTransport::new(paths.clone()).is_running() => {
            ok("steam", NAME, "Installed, running")
        }
        Ok(_) => ok("steam", NAME, "Installed"),
        Err(_) => missing("steam", NAME, "Not installed"),
    }
}

fn game() -> Requirement {
    const NAME: &str = "Runehaven (Windows version)";
    match launch::locate_game() {
        Ok((_, m, _)) if m.is_ready() => ok("game", NAME, "Installed through Steam"),
        Ok(_) => missing("game", NAME, "Steam is updating it"),
        Err(Error::GameNotInstalled) => missing("game", NAME, "Not installed"),
        Err(Error::SteamNotInstalled(_)) => missing("game", NAME, "Needs Steam"),
        Err(e) => failed("game", NAME, e.to_string()),
    }
}

fn runtime(launcher: &LauncherPaths) -> Requirement {
    const NAME: &str = "Wine runtime";
    let config = Config::load(&launcher.config_file()).unwrap_or_default();
    if let Some(wine) = config.runtime.wine {
        return ok("runtime", NAME, format!("Custom: {}", wine.display()));
    }
    let spec = RuntimeSpec::bundled();
    if Runtime::locate(launcher, &spec).is_installed() {
        ok("runtime", NAME, "Installed")
    } else {
        missing(
            "runtime",
            NAME,
            format!("Not downloaded, {} MB", spec.download_size() >> 20),
        )
    }
}

/// Checks everything the game needs. Takes a moment (it starts a few small
/// programs), so it runs off the interface thread.
#[tauri::command]
pub async fn requirements() -> Result<Vec<Requirement>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let launcher = LauncherPaths::detect().map_err(|e| e.to_string())?;
        Ok(vec![rosetta(), steam(), game(), runtime(&launcher)])
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether this launcher has started the game and it is still running.
#[derive(Default)]
pub struct GameState {
    running: AtomicBool,
}

/// Sent as the `game` event while the game starts, runs and ends.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum GameEvent {
    Starting,
    Step(StartEvent),
    /// Wine is running and the game is loading.
    Launched { pid: u32 },
    /// The main menu is up.
    Running,
    Crashed,
    Exited { code: Option<i32> },
    Failed { message: String },
}

/// How a start ended.
enum Outcome {
    Exited(ExitStatus),
    Crashed,
}

#[tauri::command]
pub fn game_running(state: State<'_, GameState>) -> bool {
    state.running.load(Ordering::SeqCst)
}

fn with_config(
    f: fn(&Config, &LauncherPaths) -> rhl_core::Result<()>,
) -> Result<(), String> {
    let run = || {
        let launcher = LauncherPaths::detect()?;
        let config = Config::load(&launcher.config_file())?;
        f(&config, &launcher)
    };
    run().map_err(|e| e.to_string())
}

/// Asks the game to close its window; whether it does arrives as `game`
/// events.
#[tauri::command]
pub async fn exit_game() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| with_config(launch::request_close))
        .await
        .map_err(|e| e.to_string())?
}

/// Ends the game at once, for when it hangs.
#[tauri::command]
pub async fn force_close_game() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| with_config(launch::force_close))
        .await
        .map_err(|e| e.to_string())?
}

/// Starts the game and returns right away; how it goes arrives as `game`
/// events.
#[tauri::command]
pub fn launch_game(app: AppHandle, state: State<'_, GameState>) -> Result<(), String> {
    if state.running.swap(true, Ordering::SeqCst) {
        return Err(Error::AlreadyRunning.to_string());
    }
    std::thread::spawn(move || {
        let emit = |event: GameEvent| {
            let _ = app.emit("game", event);
        };
        let run = || -> rhl_core::Result<Outcome> {
            let launcher = LauncherPaths::detect()?;
            let (config, plan) = launch::prepare(&launcher, false)?;
            emit(GameEvent::Starting);
            let (mut child, lock) =
                launch::start(&config, &plan, &launcher, &mut |step| emit(GameEvent::Step(step)))?;
            emit(GameEvent::Launched { pid: child.id() });
            // Wine running does not mean the game is up; its own log tells.
            let mut log = PlayerLogWatch::new(&plan.player_log);
            let (mut loaded, mut crashed) = (false, false);
            let status = loop {
                for signal in log.poll() {
                    match signal {
                        LogSignal::Loaded if !loaded => {
                            loaded = true;
                            emit(GameEvent::Running);
                        }
                        LogSignal::Crashed => crashed = true,
                        LogSignal::Loaded => {}
                    }
                }
                let exited = child.try_wait().map_err(|source| Error::Io {
                    context: "waiting for Wine".into(),
                    source,
                })?;
                if let Some(status) = exited {
                    break status;
                }
                std::thread::sleep(Duration::from_millis(500));
            };
            lock.release();
            // The crash line can arrive just before the process ends.
            crashed |= log.poll().contains(&LogSignal::Crashed);
            Ok(if crashed { Outcome::Crashed } else { Outcome::Exited(status) })
        };
        match run() {
            Ok(Outcome::Exited(status)) => emit(GameEvent::Exited { code: status.code() }),
            Ok(Outcome::Crashed) => emit(GameEvent::Crashed),
            Err(e) => emit(GameEvent::Failed { message: e.to_string() }),
        }
        app.state::<GameState>().running.store(false, Ordering::SeqCst);
    });
    Ok(())
}
