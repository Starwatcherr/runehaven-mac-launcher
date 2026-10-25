//! The Runehaven Mac Launcher app. The interface is the HTML page in `ui/`;
//! the work is done by `rhl-core`, reached through the commands in
//! [`commands`].

mod commands;

fn main() {
    tauri::Builder::default()
        .manage(commands::GameState::default())
        .invoke_handler(tauri::generate_handler![
            commands::requirements,
            commands::launch_game,
            commands::game_running,
            commands::exit_game,
            commands::force_close_game,
        ])
        .run(tauri::generate_context!())
        .expect("the launcher window could not be opened");
}
