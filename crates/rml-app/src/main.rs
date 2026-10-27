//! The Runehaven Mac Launcher app. The interface is the HTML page in `ui/`;
//! the work is done by `rml-core`, reached through the commands in
//! [`commands`].

mod commands;
mod locations;
mod maintenance;
mod settings;
mod setup;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::GameState::default())
        .manage(setup::SetupState::default())
        .setup(|app| {
            maintenance::restore_steam_platform(app.handle().clone());
            Ok(())
        })
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
            settings::launcher_update,
            maintenance::maintenance_status,
            maintenance::verify_game,
            maintenance::uninstall_game,
            maintenance::remove_runtime,
            maintenance::reset_prefix,
            maintenance::remove_data,
            maintenance::uninstall_launcher,
            maintenance::quit_and_trash,
            maintenance::open_saves,
            locations::install_locations,
            locations::set_install_location,
            locations::pick_path,
            locations::open_steam_settings,
            locations::move_game,
        ])
        .run(tauri::generate_context!())
        .expect("the launcher window could not be opened");
}
