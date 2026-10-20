use log::log_error;
use tauri::Manager;

mod autostart;
mod bench;
mod card;
pub mod clip;
pub mod command_mode;
mod commands;
pub mod dim;
pub mod failure;
mod flow;
pub mod focus_probe;
pub mod history;
pub mod hotkey;
pub mod inserter;
pub mod log;
pub mod models;
mod overlay;
mod pricing;
pub mod provider;
pub mod recorder;
pub mod segment;
pub mod settings;
pub mod setup;
pub mod single_instance;
pub mod stats;
pub mod streaming;
pub mod timing;
mod tray;
mod updater;

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
    // First of all, before any window, tray icon or keyboard hook (PARLA-102).
    if single_instance::claim() == single_instance::Claim::Running {
        std::process::exit(0);
    }
    #[cfg(windows)]
    opt_out_of_power_throttling();
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            if let Ok(dir) = app.path().app_data_dir() {
                log::init(&dir);
            }
            // Webviews can invoke commands as soon as they load, so all state
            // must be managed before anything that creates or shows a window.
            let settings_path = settings::path(app.handle())?;
            // Decided before anything can write the file (PARLA-100).
            let first_run = setup::is_first_run(
                &settings_path,
                &history::path(app.handle()).unwrap_or_default(),
            );
            let config = settings::load(&settings_path).unwrap_or_else(|error| {
                log_error!("{error}; using defaults until settings are repaired");
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
            app.manage(setup::MicMeter::default());
            if let Err(error) = history::setup(app) {
                log_error!("History disabled: {error}");
            }
            if let Err(error) = stats::setup(app) {
                log_error!("Stats disabled: {error}");
            }
            tray::setup(app)?;
            overlay::setup(app)?;
            flow::setup(app)?;
            updater::setup(app);
            // A second launch asks this copy to show its window.
            let handle = app.handle().clone();
            single_instance::watch(move || tray::open_app(&handle, "home"));
            if first_run {
                tray::open_setup(app.handle(), true);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::providers::list_providers,
            commands::settings::get_settings,
            commands::settings::save_settings,
            commands::settings::select_provider,
            commands::settings::set_cloud_key,
            commands::settings::select_model,
            commands::settings::check_command_hotkey,
            commands::traccia::test_traccia,
            commands::history::get_history,
            commands::history::clear_history,
            commands::card::card_bounds,
            commands::card::card_action,
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
            commands::app::flyout_ready,
            commands::app::quit,
            commands::app::test_mic,
            commands::setup::autostart_status,
            commands::setup::set_autostart,
            commands::setup::open_startup_folder,
            commands::setup::open_sound_settings,
            commands::setup::mic_info,
            commands::setup::mic_meter_start,
            commands::setup::mic_meter_stop,
            commands::setup::finish_setup,
            updater::update_status,
            updater::check_for_update,
            updater::set_auto_update,
            updater::dismiss_update,
            updater::install_update,
            updater::open_release_page,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Parla")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                flush_on_exit(app);
            }
        });
}

/// What Parla saves on the way out. The updater calls it too: the installer
/// ends the process without a `RunEvent::Exit`.
fn flush_on_exit(app: &tauri::AppHandle) {
    dim::shutdown();
    if let Some(history) = app.try_state::<history::History>() {
        if let Err(error) = history.drain(std::time::Duration::from_secs(1)) {
            log_error!("History shutdown drain failed: {error}");
        }
    }
    if let Some(stats) = app.try_state::<stats::StatsStore>() {
        if let Err(error) = stats.flush() {
            log_error!("Stats shutdown flush failed: {error}");
        }
    }
}
