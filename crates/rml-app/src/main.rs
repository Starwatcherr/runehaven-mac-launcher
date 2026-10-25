//! The Runehaven Mac Launcher app. The interface is the HTML page in `ui/`;
//! the work is done by `rhl-core`, reached through the commands in
//! [`commands`].

mod commands;
mod setup;

fn main() {
    tauri::Builder::default()
        .manage(commands::GameState::default())
        .manage(setup::SetupState::default())
        .invoke_handler(tauri::generate_handler![
            commands::requirements,
            commands::launch_game,
            commands::game_running,
            commands::exit_game,
            commands::force_close_game,
            setup::install_requirements,
            setup::cancel_install,
            setup::open_steam_download,
        ])
        .run(tauri::generate_context!())
        .expect("the launcher window could not be opened");
}
