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

/// Test mic from the app window: the tray item's 3 s recording, saved as
/// `clips/test.wav` and revealed. It runs on its own thread and returns at once.
#[tauri::command]
pub fn test_mic(app: tauri::AppHandle) {
    crate::tray::run_test_mic(&app);
}
