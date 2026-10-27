//! `rml`: command-line front end for testing the launcher core before the
//! app's interface exists.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;

use clap::{Parser, Subcommand};
use rml_core::config::Config;
use rml_core::lock::JobLock;
use rml_core::paths::LauncherPaths;
use rml_core::steam::{
    self, library, InstallEvent, InstallOptions, ReadyStatus, SteamClient, RUNEHAVEN_APPID,
};
use rml_core::{doctor, launch, maintain, prefix, runtime, Error, Result};

#[derive(Parser)]
#[command(
    name = "rml",
    version,
    about = "Runehaven Mac Launcher (unofficial) – command-line tool. Project by Starwatcherr."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check the system, Steam and the game installation.
    Doctor,
    /// Talk to the running Steam client.
    Steam {
        #[command(subcommand)]
        command: SteamCmd,
    },
    /// Find, install or start the game.
    Game {
        #[command(subcommand)]
        command: GameCmd,
    },
    /// Show or change the launcher configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCmd,
    },
    /// Download and assemble the Wine runtime.
    Runtime {
        #[command(subcommand)]
        command: RuntimeCmd,
    },
    /// Set up the Wine prefix and manage save games.
    Prefix {
        #[command(subcommand)]
        command: PrefixCmd,
    },
}

#[derive(Subcommand)]
enum RuntimeCmd {
    /// Download (about 376 MB) and assemble the runtime.
    Install {
        /// Keep the downloaded archives afterwards.
        #[arg(long)]
        keep_downloads: bool,
    },
    /// Show whether the runtime is installed and where.
    Status,
}

#[derive(Subcommand)]
enum PrefixCmd {
    /// Create the Wine prefix if it does not exist yet.
    Init,
    /// Show where save games live.
    Saves,
    /// Copy save games, settings and key bindings from another Wine prefix.
    Import { from_prefix: PathBuf },
    /// Copy save games from another Wine prefix into the launcher's prefix.
    ImportSaves { from_prefix: PathBuf },
    /// Take over settings and key bindings from another Wine prefix, its
    /// user.reg or a .reg export from the Windows registry editor.
    ImportSettings { from: PathBuf },
    /// Run a Wine program inside the launcher's prefix (for troubleshooting),
    /// e.g. `prefix wine reg query "HKCU\Software\Wine\Mac Driver"`.
    Wine {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SteamCmd {
    /// Start Steam if needed and wait until it answers commands.
    Wait {
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Show Steam's platform setting.
    Platform,
    /// Set Steam's platform setting (windows or macos) and read it back.
    SetPlatform { value: String },
    /// Show Steam's own status of Runehaven.
    Status,
    /// Send any simple console command and print the answer (for testing).
    Exec { command: String, args: Vec<String> },
    /// Restore Steam's platform if an earlier run was interrupted.
    Recover,
    /// List other games Steam is downloading or has updates waiting for.
    Downloads,
}

#[derive(Subcommand)]
enum GameCmd {
    /// Show where Steam keeps Runehaven and its state.
    Locate,
    /// Install the Windows build through Steam.
    Install {
        /// Steam library number (see `rml doctor`); Steam's default otherwise.
        #[arg(long)]
        library: Option<u32>,
        /// Install even while other Steam downloads are running.
        #[arg(long)]
        ignore_other_downloads: bool,
        /// Keep Steam on the Windows platform until the download has finished
        /// (default: switch back as soon as Steam has planned the download).
        #[arg(long)]
        hold_platform: bool,
    },
    /// Have Steam check the game files and fetch broken or missing ones.
    Verify,
    /// Remove the game through Steam (save games stay in the prefix).
    Uninstall,
    /// Move the game into another Steam library (number from `rml doctor`).
    Move {
        #[arg(long)]
        library: u32,
    },
    /// Start the game through Wine.
    Launch {
        /// Only print what would be started.
        #[arg(long)]
        dry_run: bool,
        /// Start even if Steam reports a pending update.
        #[arg(long)]
        force: bool,
        /// Extra arguments for the game, after `--` (e.g. `-- -force-gfx-direct`).
        #[arg(last = true)]
        game_args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the configuration and where it is stored.
    Show,
    /// Change one setting: wine, library-dirs (comma-separated), prefix,
    /// inputfix, macfix (paths, or `default` for the launcher's own), fullscreen, retina
    /// (true/false), size (WIDTHxHEIGHT), hud (true/false: FPS overlay),
    /// update-check (true/false: look for new launcher versions on GitHub).
    Set { key: String, value: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let launcher = LauncherPaths::detect()?;
    match cli.command {
        Cmd::Doctor => {
            print!("{}", doctor::report());
            Ok(())
        }
        Cmd::Steam { command } => steam_cmd(command, &launcher),
        Cmd::Game { command } => game_cmd(command, &launcher),
        Cmd::Config { command } => config_cmd(command, &launcher),
        Cmd::Runtime { command } => runtime_cmd(command, &launcher),
        Cmd::Prefix { command } => prefix_cmd(command, &launcher),
    }
}

fn ready_client(timeout: u64) -> Result<SteamClient<steam::MacTransport>> {
    let mut client = SteamClient::detect()?;
    client.timing.ready_timeout = std::time::Duration::from_secs(timeout);
    let mut last = None;
    client.wait_ready(&mut |status| {
        let text = match &status {
            ReadyStatus::Starting => "Starting Steam …".to_string(),
            ReadyStatus::WaitingForProcess { elapsed_secs } => format!("Waiting for Steam to start … {elapsed_secs}s"),
            ReadyStatus::WaitingForAnswer { elapsed_secs } => {
                format!("Steam is running, waiting for it to answer (updating or login?) … {elapsed_secs}s")
            }
            ReadyStatus::Ready => "Steam is ready.".to_string(),
        };
        if last.as_ref() != Some(&text) {
            eprintln!("{text}");
            last = Some(text);
        }
    })?;
    Ok(client)
}

fn steam_cmd(command: SteamCmd, launcher: &LauncherPaths) -> Result<()> {
    match command {
        SteamCmd::Wait { timeout } => ready_client(timeout).map(|_| ()),
        SteamCmd::Platform => {
            let client = ready_client(300)?;
            println!("{:?}", client.platform()?);
            Ok(())
        }
        SteamCmd::SetPlatform { value } => {
            if !matches!(value.as_str(), "windows" | "macos") {
                return Err(Error::Config(
                    "platform must be `windows` or `macos`".into(),
                ));
            }
            let client = ready_client(300)?;
            let previous = client.set_platform(&value)?;
            println!("platform: {previous:?} -> {value:?} (verified)");
            Ok(())
        }
        SteamCmd::Status => {
            let client = ready_client(300)?;
            let status = client.app_status(RUNEHAVEN_APPID)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&status).expect("serializes")
            );
            Ok(())
        }
        SteamCmd::Exec { command, args } => {
            let client = ready_client(300)?;
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            for line in client.exec(&command, &args)? {
                if !line.contains("owner account") {
                    println!("{line}");
                }
            }
            Ok(())
        }
        SteamCmd::Recover => {
            let lock = JobLock::acquire(launcher)?;
            let client = ready_client(300)?;
            match steam::recover_platform(&client, launcher, &lock)? {
                Some(v) => println!("Restored Steam's platform to {v:?}."),
                None => println!("Nothing to restore."),
            }
            Ok(())
        }
        SteamCmd::Downloads => {
            let paths = rml_core::paths::SteamPaths::detect()?;
            let libs = library::libraries(&paths)?;
            let active = library::active_apps(&libs, RUNEHAVEN_APPID);
            let pending = library::pending_apps(&libs, RUNEHAVEN_APPID);
            println!("Active right now ({}):", active.len());
            for m in &active {
                println!("  {} ({}), flags {}", m.name, m.appid, m.state_flags);
            }
            println!("Updates waiting ({}):", pending.len());
            for m in &pending {
                println!("  {} ({}), flags {}", m.name, m.appid, m.state_flags);
            }
            Ok(())
        }
    }
}

fn game_cmd(command: GameCmd, launcher: &LauncherPaths) -> Result<()> {
    match command {
        GameCmd::Locate => {
            let paths = rml_core::paths::SteamPaths::detect()?;
            let libs = library::libraries(&paths)?;
            match library::find_app(&libs, RUNEHAVEN_APPID)? {
                None => println!("Runehaven is not installed in any Steam library."),
                Some((lib, m)) => {
                    println!("library {}: {}", lib.index, lib.path.display());
                    println!(
                        "game folder: {}",
                        lib.steamapps().join("common").join(&m.installdir).display()
                    );
                    println!(
                        "build {} (target {}), flags {}, ready: {}, update pending: {}",
                        m.buildid,
                        m.target_buildid,
                        m.state_flags,
                        m.is_ready(),
                        m.update_pending()
                    );
                }
            }
            Ok(())
        }
        GameCmd::Install {
            library,
            ignore_other_downloads,
            hold_platform,
        } => {
            let lock = JobLock::acquire(launcher)?;
            let client = ready_client(300)?;
            let opts = InstallOptions {
                library_index: library,
                ignore_other_downloads,
                restore_at: if hold_platform {
                    steam::RestoreAt::Completion
                } else {
                    steam::RestoreAt::JobPlanned
                },
                ..Default::default()
            };
            let cancel = opts.cancel.clone();
            let _ = ctrlc::set_handler(move || {
                eprintln!("\nCancelling, restoring Steam's platform …");
                cancel.store(true, Ordering::Relaxed);
            });
            // Steam only writes progress into the manifest now and then, so the
            // numbers are unreliable; point to Steam's own download view instead.
            let mut announced = false;
            let installed = steam::install(&client, launcher, &lock, &opts, &mut |event| match event {
                InstallEvent::Progress { total, .. } => {
                    if !announced {
                        if total > 0 {
                            eprintln!(
                                "Downloading ({} MB) … check Steam's downloads for progress.",
                                total >> 20
                            );
                        } else {
                            eprintln!("Downloading … check Steam's downloads for progress.");
                        }
                        announced = true;
                    }
                }
                other => eprintln!("{other:?}"),
            })?;
            if installed.already_installed {
                println!(
                    "Runehaven is already installed (build {}).",
                    installed.manifest.buildid
                );
            } else {
                println!(
                    "Installed build {} in library {}.",
                    installed.manifest.buildid, installed.library.index
                );
            }
            Ok(())
        }
        GameCmd::Verify => {
            let lock = JobLock::acquire(launcher)?;
            let client = ready_client(300)?;
            let seen = steam::verify(&client, launcher, &lock, &InstallOptions::default(), &mut |e| eprintln!("{e:?}"))?;
            println!("{}", if seen { "Steam checked the files." } else { "Steam showed no sign of checking." });
            Ok(())
        }
        GameCmd::Move { library } => {
            let lock = JobLock::acquire(launcher)?;
            let client = ready_client(300)?;
            steam::move_game(&client, launcher, &lock, &InstallOptions::default(), library, &mut |e| eprintln!("{e:?}"))?;
            println!("Runehaven is now in library {library}.");
            Ok(())
        }
        GameCmd::Uninstall => {
            let lock = JobLock::acquire(launcher)?;
            let client = ready_client(300)?;
            steam::uninstall(&client, launcher, &lock, &InstallOptions::default(), &mut |e| eprintln!("{e:?}"))?;
            println!("Runehaven was uninstalled.");
            Ok(())
        }
        GameCmd::Launch { dry_run, force, game_args } => {
            let lock = if dry_run { None } else { Some(JobLock::acquire(launcher)?) };
            let (config, mut plan) = launch::prepare(launcher, force).map_err(|e| match e {
                Error::GameNeedsUpdate => Error::Config(format!("{e} Or pass --force.")),
                e => e,
            })?;
            plan.args.extend(game_args);
            let Some(lock) = lock else {
                println!("{}", plan.describe());
                return Ok(());
            };
            let mut child = launch::start(&config, &plan, launcher, &lock, &mut print_start_event)?;
            eprintln!(
                "Runehaven started (Wine pid {}). Logs:\n  {}\n  {}",
                child.id(),
                plan.wine_log.display(),
                plan.player_log.display()
            );
            let status = child.wait().map_err(|e| Error::Io {
                context: "waiting for Wine".into(),
                source: e,
            })?;
            rml_core::lock::game_exited(launcher);
            eprintln!("Runehaven exited ({status}).");
            Ok(())
        }
    }
}

fn print_start_event(event: launch::StartEvent) {
    match event {
        launch::StartEvent::SettingUpPrefix { prefix } => eprintln!(
            "Setting up the Wine prefix at {} (first start only, about a minute) …",
            prefix.display()
        ),
        launch::StartEvent::PrefixReady => eprintln!("Wine prefix ready."),
        launch::StartEvent::RetinaChanged { on } => {
            eprintln!("Retina mode {}.", if on { "on" } else { "off" })
        }
        launch::StartEvent::InputFixPlaced => eprintln!("Placed the input fix next to the game."),
        launch::StartEvent::ForeignDllMovedAside { to } => eprintln!(
            "Another version.dll was in the game folder; it was copied to {}.",
            to.display()
        ),
        launch::StartEvent::BackupFailed { message } => {
            eprintln!("Could not back up the save games: {message}")
        }
    }
}

fn ensure_prefix(config: &Config, launcher: &LauncherPaths) -> Result<()> {
    launch::ensure_prefix(config, launcher, &mut print_start_event)
}

fn runtime_cmd(command: RuntimeCmd, launcher: &LauncherPaths) -> Result<()> {
    let spec = runtime::RuntimeSpec::bundled();
    let config = Config::load(&launcher.config_file())?;
    match command {
        RuntimeCmd::Status => {
            let rt = runtime::Runtime::locate(&config, launcher, &spec);
            println!(
                "runtime {}: {}",
                spec.id,
                if rt.is_installed() {
                    "installed"
                } else {
                    "not installed"
                }
            );
            println!("folder: {}", rt.root.display());
            for c in &spec.components {
                println!("  - {} ({} MB)", c.name, c.size >> 20);
            }
            Ok(())
        }
        RuntimeCmd::Install { keep_downloads } => {
            eprintln!(
                "Runtime {} – download size {} MB",
                spec.id,
                spec.download_size() >> 20
            );
            let _lock = JobLock::acquire(launcher)?;
            let mut last_pct = u64::MAX;
            let go = std::sync::atomic::AtomicBool::new(false);
            let rt = runtime::install(&config, launcher, &spec, keep_downloads, &go, &mut |e| match e {
                runtime::RuntimeEvent::Downloading { name, done, total } => {
                    let pct = done * 100 / total.max(1);
                    if pct / 10 != last_pct / 10 || last_pct == u64::MAX {
                        eprintln!("  {name}: {pct}% ({} of {} MB)", done >> 20, total >> 20);
                        last_pct = pct;
                    }
                    if done == total {
                        last_pct = u64::MAX;
                    }
                }
                runtime::RuntimeEvent::AlreadyDownloaded { name } => {
                    eprintln!("  {name}: already downloaded")
                }
                runtime::RuntimeEvent::Extracting { name } => eprintln!("  unpacking {name} …"),
                runtime::RuntimeEvent::Done => {}
            })?;
            println!("Runtime ready: {}", rt.root.display());
            Ok(())
        }
    }
}

fn prefix_cmd(command: PrefixCmd, launcher: &LauncherPaths) -> Result<()> {
    let config = Config::load(&launcher.config_file())?;
    let prefix = launch::prefix_path(&config, launcher);
    match command {
        PrefixCmd::Init => {
            let _lock = JobLock::acquire(launcher)?;
            ensure_prefix(&config, launcher)?;
            println!("Prefix: {}", prefix.display());
            Ok(())
        }
        PrefixCmd::Saves => {
            match prefix::saves_dir(&prefix) {
                Some(dir) if dir.is_dir() => println!("{}", dir.display()),
                Some(dir) => println!("{} (no saves yet)", dir.display()),
                None => println!("The prefix at {} is not set up yet.", prefix.display()),
            }
            Ok(())
        }
        PrefixCmd::Import { from_prefix } => {
            let _lock = JobLock::acquire(launcher)?;
            ensure_prefix(&config, launcher)?;
            let to = prefix::import_saves(&from_prefix, &prefix)?;
            println!("Copied save games to {}", to.display());
            import_settings(&from_prefix, &config, launcher, &prefix)
        }
        PrefixCmd::ImportSaves { from_prefix } => {
            let _lock = JobLock::acquire(launcher)?;
            ensure_prefix(&config, launcher)?;
            let to = prefix::import_saves(&from_prefix, &prefix)?;
            println!("Copied save games to {}", to.display());
            Ok(())
        }
        PrefixCmd::Wine { args } => {
            ensure_prefix(&config, launcher)?;
            let rt = launch::ResolvedRuntime::resolve(&config, launcher)?;
            let status = std::process::Command::new(&rt.wine)
                .args(&args)
                .env("WINEPREFIX", &prefix)
                .env("DYLD_FALLBACK_LIBRARY_PATH", rt.library_path())
                .env("WINEDEBUG", "-all")
                .env("WINEDLLOVERRIDES", "mscoree,mshtml=;winemenubuilder.exe=d")
                .status()
                .map_err(|e| Error::Config(format!("starting {}: {e}", rt.wine.display())))?;
            if status.success() {
                Ok(())
            } else {
                Err(Error::Config(format!("wine exited with {status}")))
            }
        }
        PrefixCmd::ImportSettings { from } => {
            let _lock = JobLock::acquire(launcher)?;
            ensure_prefix(&config, launcher)?;
            import_settings(&from, &config, launcher, &prefix)
        }
    }
}


fn import_settings(
    from: &std::path::Path,
    config: &Config,
    launcher: &LauncherPaths,
    prefix: &std::path::Path,
) -> Result<()> {
    eprintln!("Taking over settings …");
    let Some(report) = maintain::import_settings(from, config, launcher, prefix)? else {
        println!("No Runehaven settings found in {} – nothing to take over.", from.display());
        return Ok(());
    };
    println!(
        "Took over {} settings{} ({} internal Unity values left out).",
        report.imported,
        if report.key_bindings { " including key bindings" } else { "" },
        report.skipped
    );
    if let Some(backup) = report.backup {
        println!("Previous settings saved to {}", backup.display());
    }
    Ok(())
}

fn config_cmd(command: ConfigCmd, launcher: &LauncherPaths) -> Result<()> {
    let path = launcher.config_file();
    let mut config = Config::load(&path)?;
    match command {
        ConfigCmd::Show => {
            println!("{}", path.display());
            println!(
                "{}",
                serde_json::to_string_pretty(&config).expect("serializes")
            );
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            config.set(&key, &value)?;
            config.save(&path)?;
            println!("Saved {}", path.display());
            Ok(())
        }
    }
}
