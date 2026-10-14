/// Retry pastes into whichever app is focused when injection runs.
#[tauri::command]
pub async fn retry_last_failed(app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || crate::failure::retry_last_failed(&app))
        .await
        .map_err(|_| "Retry worker could not complete".to_string())?
}
