use crate::settings::{self, Settings, SharedSettings};
use tauri::Manager;

#[tauri::command]
pub async fn get_settings(app: tauri::AppHandle) -> Result<Settings, String> {
    // A saver can hold the write lock while waiting for main-thread shortcut
    // registration. Never wait on that lock from a synchronous IPC command.
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<SharedSettings>()
            .read()
            .map(|s| s.clone())
            .map_err(|_| "Settings lock poisoned".into())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The async command keeps the main event loop free while set_shortcut dispatches
/// to it. Serialize apply + disk + state to prevent concurrent saves racing.
#[tauri::command]
pub async fn save_settings(app: tauri::AppHandle, settings: Settings) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let shared = app.state::<SharedSettings>();
        let mut current = shared.write().map_err(|_| "Settings lock poisoned")?;
        let path = settings::path(&app)?;
        settings::apply(&app, &settings)?;
        if let Err(error) = settings::save(&path, &settings) {
            if let Err(rollback) = settings::apply(&app, &current) {
                return Err(format!("{error}; hotkey rollback failed: {rollback}"));
            }
            return Err(error);
        }
        *current = settings;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}
