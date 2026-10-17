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

/// Shows one of Parla's own windows (used by the main window's buttons).
#[tauri::command]
pub fn open_window(app: tauri::AppHandle, name: String) -> Result<(), String> {
    let (label, what) = match name.as_str() {
        "history" => ("history", "history"),
        "bench" => ("bench", "benchmark"),
        _ => return Err(format!("Unknown window: {name}")),
    };
    crate::tray::show_window(&app, label, what);
    Ok(())
}

/// Test mic from the app window: the tray item's 3 s recording, saved as
/// `clips/test.wav` and revealed. It runs on its own thread and returns at once.
#[tauri::command]
pub fn test_mic(app: tauri::AppHandle) {
    crate::tray::run_test_mic(&app);
}
