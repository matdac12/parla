use tauri::Manager;

mod bench;
pub mod clip;
mod commands;
pub mod failure;
mod flow;
pub mod history;
pub mod hotkey;
pub mod inserter;
pub mod models;
mod overlay;
pub mod provider;
pub mod recorder;
pub mod settings;
pub mod stats;
pub mod streaming;
pub mod timing;
mod tray;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // Webviews can invoke commands as soon as they load, so all state
            // must be managed before anything that creates or shows a window.
            let config = settings::load(&settings::path(app.handle())?).unwrap_or_else(|error| {
                eprintln!("{error}; using defaults until settings are repaired");
                settings::Settings::default()
            });
            models::setup(app.handle(), &config)?;
            app.manage(std::sync::Arc::new(std::sync::RwLock::new(config)));
            app.manage(failure::LastFailedClip::default());
            app.manage(bench::BenchSession::default());
            app.manage(failure::RetryLock::default());
            app.manage(commands::settings::SaveLock::default());
            if let Err(error) = history::setup(app) {
                eprintln!("History disabled: {error}");
            }
            if let Err(error) = stats::setup(app) {
                eprintln!("Stats disabled: {error}");
            }
            tray::setup(app)?;
            overlay::setup(app)?;
            flow::setup(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::providers::list_providers,
            commands::settings::get_settings,
            commands::settings::save_settings,
            commands::settings::select_provider,
            commands::history::get_history,
            commands::history::clear_history,
            commands::failure::retry_last_failed,
            commands::failure::retry_to_history,
            commands::failure::has_failed_clip,
            commands::models::model_status,
            commands::models::download_model,
            commands::bench::bench_start_recording,
            commands::bench::bench_stop_recording,
            commands::bench::bench_clip,
            commands::bench::bench_run,
            commands::bench::bench_runs,
            commands::app::get_stats,
            commands::app::open_window,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Parla")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                if let Some(stats) = app.try_state::<stats::StatsStore>() {
                    if let Err(error) = stats.flush() {
                        eprintln!("Stats shutdown flush failed: {error}");
                    }
                }
            }
        });
}
