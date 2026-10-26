//! The Runehaven Mac Launcher app. The interface is the HTML page in `ui/`;
//! the work is done by `rhl-core`, reached through the commands in
//! [`commands`].

mod commands;
mod maintenance;
mod settings;
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
            settings::get_settings,
            settings::set_setting,
            settings::open_logs,
            settings::export_logs,
            settings::open_link,
            maintenance::maintenance_status,
            maintenance::verify_game,
            maintenance::uninstall_game,
            maintenance::remove_runtime,
            maintenance::reset_prefix,
            maintenance::remove_data,
        ])
        .run(tauri::generate_context!())
        .expect("the launcher window could not be opened");
}
