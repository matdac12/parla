//! IPC for the main Parla window.
use crate::stats::{StatsStore, Summary};
use tauri::Manager;

#[tauri::command]
pub fn get_stats(app: tauri::AppHandle) -> Summary {
    app.try_state::<StatsStore>()
        .map(|store| store.get())
        .unwrap_or_default()
        .summary()
}

/// Shows the benchmark window, or the main window on one of its pages: History
/// and Timings are pages there, not windows of their own.
#[tauri::command]
pub fn open_window(app: tauri::AppHandle, name: String) -> Result<(), String> {
    match name.as_str() {
        "bench" => crate::tray::open_bench(&app),
        "home" | "history" | "timings" | "settings" => crate::tray::open_app(&app, &name),
        _ => return Err(format!("Unknown window: {name}")),
    }
    Ok(())
}

/// The tray flyout has rendered: size it to the page and show it.
#[tauri::command]
pub fn flyout_ready(window: tauri::WebviewWindow, height: f64) -> Result<(), String> {
    if window.label() != crate::tray::FLYOUT {
        return Err("Only the flyout can show itself".into());
    }
    crate::tray::show_flyout(&window, height).map_err(|error| {
        eprintln!("failed to show the flyout: {error}");
        error.to_string()
    })
}

/// Quit from the flyout, like the tray menu's Quit Parla.
#[tauri::command]
pub fn quit(app: tauri::AppHandle) {
    app.exit(0);
}

/// Test mic from the app window or the flyout: the tray item's 3 s recording,
/// saved as `clips/test.wav` and revealed. It runs on its own thread and
/// returns at once.
#[tauri::command]
pub fn test_mic(app: tauri::AppHandle) {
    crate::tray::run_test_mic(&app);
}
