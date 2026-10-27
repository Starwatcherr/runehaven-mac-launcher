//! Commands the interface calls, and the `game` event it listens to.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rml_core::config::Config;
use rml_core::launch::{self, LogSignal, PlayerLogWatch, StartEvent};
use rml_core::lock::{self, JobLock};
use rml_core::paths::{LauncherPaths, SteamPaths};
use rml_core::runtime::{self, Runtime, RuntimeSpec};
use rml_core::state::LauncherState;
use rml_core::steam::{MacTransport, Transport};
use rml_core::{doctor, Error};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

/// One line of the requirements list on the home page.
#[derive(Debug, Serialize)]
pub struct Requirement {
    pub id: &'static str,
    name: &'static str,
    /// `ok`, `missing` (the launcher can do something about it) or `failed`.
    pub state: &'static str,
    detail: String,
    /// Missing only because a newer version is waiting.
    update: bool,
}

fn ok(id: &'static str, name: &'static str, detail: impl Into<String>) -> Requirement {
    Requirement { id, name, state: "ok", detail: detail.into(), update: false }
}

fn missing(id: &'static str, name: &'static str, detail: impl Into<String>) -> Requirement {
    Requirement { id, name, state: "missing", detail: detail.into(), update: false }
}

fn update(id: &'static str, name: &'static str, detail: impl Into<String>) -> Requirement {
    Requirement { id, name, state: "missing", detail: detail.into(), update: true }
}

fn failed(id: &'static str, name: &'static str, detail: impl Into<String>) -> Requirement {
    Requirement { id, name, state: "failed", detail: detail.into(), update: false }
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
        Ok((_, m, _)) if m.update_pending() => update("game", NAME, "Update required"),
        Ok(_) => missing("game", NAME, "Steam is working on it"),
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
    if Runtime::locate(&config, launcher, &spec).is_installed() {
        ok("runtime", NAME, "Installed")
    } else if !runtime::older_runtimes(&runtime::runtime_dir(&config, launcher), &spec).is_empty() {
        update(
            "runtime",
            NAME,
            format!("Update required, {} MB", spec.download_size() >> 20),
        )
    } else {
        missing(
            "runtime",
            NAME,
            format!("Not downloaded, {} MB", spec.download_size() >> 20),
        )
    }
}

/// Everything the game needs, in the order the list shows and setup works.
pub fn check_all(launcher: &LauncherPaths) -> Vec<Requirement> {
    vec![rosetta(), steam(), game(), runtime(launcher)]
}

/// Checks everything the game needs. Takes a moment (it starts a few small
/// programs), so it runs off the interface thread.
#[tauri::command]
pub async fn requirements() -> Result<Vec<Requirement>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let launcher = LauncherPaths::detect().map_err(|e| e.to_string())?;
        Ok(check_all(&launcher))
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
    /// `first_start`: the first start with a new Mac helper, which is slow once.
    Starting { first_start: bool },
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
    f: fn(&Config, &LauncherPaths) -> rml_core::Result<()>,
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
    // Steam may still be on Windows after an interrupted task.
    crate::maintenance::restore_steam_platform(app.clone());
    std::thread::spawn(move || {
        let emit = |event: GameEvent| {
            let _ = app.emit("game", event);
        };
        let run = || -> rml_core::Result<Outcome> {
            let launcher = LauncherPaths::detect()?;
            // Held while the start changes things; afterwards `run.pid`
            // keeps other tasks out until the game has exited.
            let job = JobLock::acquire(&launcher)?;
            let (config, plan) = launch::prepare(&launcher, false)?;
            let helper = launch::helper_stamp(&config);
            let state_file = launcher.state_file();
            let first_start = helper.is_some()
                && LauncherState::load(&state_file).map_or(true, |s| s.last_helper != helper);
            emit(GameEvent::Starting { first_start });
            let mut child =
                launch::start(&config, &plan, &launcher, &job, &mut |step| emit(GameEvent::Step(step)))?;
            drop(job);
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
                            // From now on, starts with this helper are quick.
                            if first_start {
                                let _ = LauncherState::update(&state_file, |s| s.last_helper = helper.clone());
                            }
                        }
                        LogSignal::Crashed => crashed = true,
                        LogSignal::Loaded => {}
                    }
                }
                let exited = child.try_wait().map_err(|source| {
                    lock::game_exited(&launcher);
                    Error::Io { context: "waiting for Wine".into(), source }
                })?;
                if let Some(status) = exited {
                    break status;
                }
                std::thread::sleep(Duration::from_millis(500));
            };
            lock::game_exited(&launcher);
            // The crash line can arrive just before the process ends.
            crashed |= log.poll().contains(&LogSignal::Crashed);
            Ok(if crashed { Outcome::Crashed } else { Outcome::Exited(status) })
        };
        match catch_unwind(AssertUnwindSafe(run)) {
            Ok(Ok(Outcome::Exited(status))) => emit(GameEvent::Exited { code: status.code() }),
            Ok(Ok(Outcome::Crashed)) => emit(GameEvent::Crashed),
            Ok(Err(e)) => emit(GameEvent::Failed { message: e.to_string() }),
            Err(_) => emit(GameEvent::Failed {
                message: "The launcher ran into an internal error. Please export the logs and report it.".into(),
            }),
        }
        app.state::<GameState>().running.store(false, Ordering::SeqCst);
    });
    Ok(())
}
