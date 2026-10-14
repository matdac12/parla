use crate::{
    models::{self, ModelStatus},
    settings::SharedSettings,
};
use tauri::Manager;

#[tauri::command]
pub async fn model_status(app: tauri::AppHandle, id: String) -> Result<ModelStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = app
            .state::<SharedSettings>()
            .read()
            .map_err(|_| "Settings lock poisoned")?
            .clone();
        models::status(&id, &settings)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn download_model(app: tauri::AppHandle, id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = app
            .state::<SharedSettings>()
            .read()
            .map_err(|_| "Settings lock poisoned")?
            .clone();
        models::download(&app, &id, &settings)
    })
    .await
    .map_err(|e| e.to_string())?
}
