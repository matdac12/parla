/// Retry pastes into whichever app is focused when injection runs.
#[tauri::command]
pub async fn retry_last_failed(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || crate::failure::retry_last_failed(&app))
        .await
        .map_err(|_| "Retry worker could not complete".to_string())?
}

/// Give the native menu time to dismiss and return focus before a tray retry.
/// Runs on a blocking worker; the menu callback itself never sleeps.
pub fn retry_from_tray(app: tauri::AppHandle) {
    tauri::async_runtime::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = crate::failure::retry_last_failed(&app);
    });
}
