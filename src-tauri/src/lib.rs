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

/// Tauri's builder with the raw keyboard and mouse input tao asks for by
/// default turned off. Parla reads none of it (Tauri never forwards device
/// events), and while a Parla window is in front that registration stops
/// Windows from handing key-downs to Parla's low-level hook and to the hooks
/// after it, such as another app's shortcuts (PARLA-106).
fn builder() -> tauri::Builder<tauri::Wry> {
    tauri::Builder::default().device_event_filter(tauri::DeviceEventFilter::Always)
}

pub fn run() {
    // First of all, before any window, tray icon or keyboard hook (PARLA-102).
    if single_instance::claim() == single_instance::Claim::Running {
        std::process::exit(0);
    }
    #[cfg(windows)]
    opt_out_of_power_throttling();
    builder()
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
    inserter::flush_pending_restore();
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

#[cfg(all(test, windows, feature = "native-smoke"))]
mod native_tests {
    use crate::overlay::native_tests::wait_until;
    use std::{
        ptr::null_mut,
        sync::atomic::{AtomicU32, Ordering},
        time::Duration,
    };
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    use windows_sys::Win32::{
        Foundation::{LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Input::KeyboardAndMouse::VK_F24,
            WindowsAndMessaging::{
                CallNextHookEx, GetForegroundWindow, GetMessageW, PostThreadMessageW,
                SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, MSG,
                WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT,
            },
        },
    };

    /// F24 key-downs the test hook saw.
    static DOWNS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn count_downs(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 && wparam as u32 == WM_KEYDOWN {
            // SAFETY: HC_ACTION keyboard notifications carry a KBDLLHOOKSTRUCT.
            let input = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            if input.vkCode == u32::from(VK_F24) {
                DOWNS.fetch_add(1, Ordering::SeqCst);
            }
        }
        // SAFETY: forwards every event.
        unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
    }

    /// A hook thread like Parla's (`hotkey::windows_hook::run`). Its hook is
    /// the newest, so first in the chain, as Parla's is after each refresh:
    /// the bug needs that. Returns the thread's id once the hook is in.
    fn hook_thread() -> u32 {
        let (id, installed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // SAFETY: plain user32 calls on this thread, which owns the hook.
            unsafe {
                let hook = SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(count_downs),
                    GetModuleHandleW(null_mut()),
                    0,
                );
                assert!(!hook.is_null(), "SetWindowsHookExW failed");
                id.send(GetCurrentThreadId()).unwrap();
                let mut message = std::mem::zeroed::<MSG>();
                while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {}
                UnhookWindowsHookEx(hook);
            }
        });
        installed.recv().unwrap()
    }

    /// PARLA-106: with one of Parla's windows in front, a low-level hook in
    /// Parla's process saw no key-downs at all (its hotkey did nothing, and
    /// other apps' shortcuts broke) while tao had raw input registered. Takes
    /// the focus and sends F24; run explicitly with nobody typing.
    #[test]
    #[ignore = "takes the focus and sends F24; needs a Windows desktop"]
    fn a_parla_window_in_front_leaves_key_downs_to_keyboard_hooks() {
        use enigo::{Direction, Key, Keyboard};
        let mut context = tauri::generate_context!();
        // The test runner does not embed the app's icon resource.
        context.set_default_window_icon(Some(tauri::image::Image::new_owned(vec![0; 4], 1, 1)));
        let app = super::builder()
            .any_thread()
            .setup(|app| {
                let blank = WebviewUrl::External("about:blank".parse().unwrap());
                WebviewWindowBuilder::new(app, "probe", blank).build()?;
                Ok(())
            })
            .build(context)
            .unwrap();
        let handle = app.handle().clone();
        let (result, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Let WebView2 start and take the focus inside the window.
                std::thread::sleep(Duration::from_secs(3));
                let window = handle.get_webview_window("probe").unwrap();
                window.set_focus().unwrap();
                let hwnd = window.hwnd().unwrap().0;
                // SAFETY: read-only user32 query.
                wait_until("the window never came to the front", || unsafe {
                    GetForegroundWindow() == hwnd
                });
                let hooks = hook_thread();
                enigo::Enigo::new(&enigo::Settings::default())
                    .unwrap()
                    .key(Key::F24, Direction::Click)
                    .unwrap();
                wait_until("the hook never saw the key-down", || {
                    DOWNS.load(Ordering::SeqCst) >= 1
                });
                // SAFETY: ends the hook thread's message loop.
                unsafe { PostThreadMessageW(hooks, WM_QUIT, 0, 0) };
            }));
            result.send(check.is_ok()).unwrap();
            handle.exit(0);
        });
        app.run_return(|_, _| {});
        assert!(outcome.recv().unwrap());
    }
}
