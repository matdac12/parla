/// Retry from the recording pill: transcribes the saved clip and pastes it into
/// whichever app is focused (the pill never takes focus).
#[tauri::command]
pub async fn retry_last_failed(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || crate::failure::retry_with_pill(&app))
        .await
        .map_err(|_| "Retry worker could not complete".to_string())?
}

/// Dismiss from the failed pill: hides it at once instead of after 10 s.
#[tauri::command]
pub fn dismiss_failure_pill(app: tauri::AppHandle) {
    crate::overlay::dismiss(&app);
}

/// Retry from the app window: the transcript lands in history (with a Copy
/// button) instead of being pasted into Parla itself.
#[tauri::command]
pub async fn retry_to_history(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let result = crate::failure::retry_last_failed(&app, false);
        let _ = tauri::Emitter::emit(
            &app,
            "failure://changed",
            crate::failure::has_failed_clip(&app),
        );
        result
    })
    .await
    .map_err(|_| "Retry worker could not complete".to_string())?
}

#[tauri::command]
pub fn has_failed_clip(app: tauri::AppHandle) -> bool {
    crate::failure::has_failed_clip(&app)
}

/// Give the native menu time to dismiss and return focus before a tray retry.
/// Runs on a blocking worker; the menu callback itself never sleeps.
pub fn retry_from_tray(app: tauri::AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = crate::failure::retry_with_pill(&app);
    });
}
