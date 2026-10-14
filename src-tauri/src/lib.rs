use tauri::Manager;

mod bench;
pub mod clip;
mod commands;
pub mod failure;
mod flow;
pub mod hotkey;
pub mod inserter;
pub mod models;
pub mod provider;
pub mod recorder;
pub mod settings;
pub mod timing;
mod tray;

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            tray::setup(app)?;
            let config = settings::load(&settings::path(app.handle())?).unwrap_or_else(|error| {
                eprintln!("{error}; using defaults until settings are repaired");
                settings::Settings::default()
            });
            models::setup(app.handle(), &config)?;
            app.manage(std::sync::Arc::new(std::sync::RwLock::new(config)));
            app.manage(failure::LastFailedClip::default());
            app.manage(commands::settings::SaveLock::default());
            flow::setup(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::settings::get_settings,
            commands::settings::save_settings,
            commands::models::model_status,
            commands::models::download_model,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Parla");
}
