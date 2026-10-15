//! IPC for the main Parla window.
use crate::stats::{Stats, StatsStore};
use tauri::Manager;

#[tauri::command]
pub fn get_stats(app: tauri::AppHandle) -> Stats {
    app.try_state::<StatsStore>()
        .map(|store| store.get())
        .unwrap_or_default()
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
