//! History IPC (PARLA-5). Reads and clears run on blocking workers so the
//! webview thread never waits on disk.
use crate::history::{History, DEFAULT_LIMIT};
use tauri::Manager;

/// Dictations, and with `commands` command runs too (the main window's list;
/// the tray flyout shows dictations only). Benchmark rows belong to the
/// benchmark window (`bench_runs`).
const DICTATIONS: &[&str] = &["dictation"];
const DICTATIONS_AND_COMMANDS: &[&str] = &["dictation", crate::timing::COMMAND_KIND];

/// Newest-first history rows. `limit` defaults to `DEFAULT_LIMIT` and is
/// capped by `History::load_recent`, so the file never has to fit in memory.
#[tauri::command]
pub async fn get_history(
    app: tauri::AppHandle,
    limit: Option<usize>,
    commands: Option<bool>,
) -> Result<Vec<crate::timing::DictationRecord>, String> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let kinds = if commands.unwrap_or(false) {
        DICTATIONS_AND_COMMANDS
    } else {
        DICTATIONS
    };
    tauri::async_runtime::spawn_blocking(move || {
        Ok(app
            .try_state::<History>()
            .map(|history| history.load_recent_kinds(limit, kinds))
            .unwrap_or_default())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Truncates `history.jsonl`, propagating a write failure to the UI.
#[tauri::command]
pub async fn clear_history(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || match app.try_state::<History>() {
        Some(history) => history.clear(),
        None => Err("History is disabled".into()),
    })
    .await
    .map_err(|error| error.to_string())?
}
