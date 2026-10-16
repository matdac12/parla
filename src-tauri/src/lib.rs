use tauri::Manager;

mod bench;
pub mod clip;
mod commands;
pub mod dim;
pub mod failure;
mod flow;
pub mod history;
pub mod hotkey;
pub mod inserter;
pub mod models;
mod overlay;
mod pricing;
pub mod provider;
pub mod recorder;
pub mod segment;
pub mod settings;
pub mod stats;
pub mod streaming;
pub mod timing;
mod tray;

/// Windows 11 runs background processes under EcoQoS, which favours E-cores and
/// low clocks. Parla is always in the background while dictating (the target
/// app has focus), and local inference is latency-critical. Idle cost is nil:
/// this only affects how the process is scheduled while it runs.
#[cfg(windows)]
fn opt_out_of_power_throttling() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    // SAFETY: a valid pseudo-handle and a correctly sized, initialized struct.
    let ok = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const _,
            std::mem::size_of_val(&state) as u32,
        )
    };
    if ok == 0 {
        eprintln!("Could not opt out of power throttling");
    }
}

pub fn run() {
    #[cfg(windows)]
    opt_out_of_power_throttling();
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
            // Undoes a dim left by a crash, off the main thread.
            if let Ok(dir) = app.path().app_config_dir() {
                dim::setup(&dir);
            }
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
            commands::settings::select_model,
            commands::history::get_history,
            commands::history::clear_history,
            commands::failure::retry_last_failed,
            commands::failure::dismiss_failure_pill,
            commands::failure::retry_to_history,
            commands::failure::has_failed_clip,
            commands::models::model_status,
            commands::models::list_models,
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
                dim::shutdown();
                if let Some(stats) = app.try_state::<stats::StatsStore>() {
                    if let Err(error) = stats.flush() {
                        eprintln!("Stats shutdown flush failed: {error}");
                    }
                }
            }
        });
}
