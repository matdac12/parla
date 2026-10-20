//! IPC for the setup wizard (PARLA-100) and the Start-with-Windows setting
//! (PARLA-52). The wizard's engine, key and model steps use the Settings
//! commands as they are.
use crate::{autostart, setup};
use tauri::Manager;

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn autostart_status() -> Result<autostart::Status, String> {
    blocking(autostart::current_status).await
}

/// Turns Start with Windows on or off and returns the state afterwards. On is
/// refused (with the reason) while a Parla shortcut is in the Startup folder.
#[tauri::command]
pub async fn set_autostart(enabled: bool) -> Result<autostart::Status, String> {
    blocking(move || autostart::set_current(enabled)).await
}

/// Hands `target` (a folder, or a settings page such as `ms-settings:sound`)
/// to Explorer.
fn explorer(target: impl AsRef<std::ffi::OsStr>) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(target)
        .spawn()
        .map(drop)
        .map_err(|error| error.to_string())
}

/// Opens the Startup folder, so the user can delete a shortcut.
#[tauri::command]
pub fn open_startup_folder() -> Result<(), String> {
    explorer(autostart::startup_dir().ok_or("The Startup folder is unknown")?)
}

/// Windows' Sound settings, where the default microphone is chosen.
#[tauri::command]
pub fn open_sound_settings() -> Result<(), String> {
    explorer("ms-settings:sound")
}

#[tauri::command]
pub async fn mic_info() -> Result<Option<String>, String> {
    blocking(|| Ok(setup::default_microphone())).await
}

/// The meter holds the microphone and the dictation guard: only the wizard
/// may run it.
fn only_setup(window: &tauri::WebviewWindow) -> Result<(), String> {
    if window.label() == setup::LABEL {
        Ok(())
    } else {
        Err("Only the setup window can use the microphone meter".into())
    }
}

#[tauri::command]
pub async fn mic_meter_start(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<(), String> {
    only_setup(&window)?;
    blocking(move || app.state::<setup::MicMeter>().start(&app)).await
}

#[tauri::command]
pub async fn mic_meter_stop(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<(), String> {
    only_setup(&window)?;
    blocking(move || {
        app.state::<setup::MicMeter>().stop();
        Ok(())
    })
    .await
}

/// Closes the wizard (only it may), and opens the main window on Home when
/// asked.
#[tauri::command]
pub fn finish_setup(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    open_app: bool,
) -> Result<(), String> {
    if window.label() != setup::LABEL {
        return Err("Only the setup window can finish setup".into());
    }
    if open_app {
        crate::tray::open_app(&app, "home");
    }
    // `closed` runs from the window's destroy event.
    window.destroy().map_err(|error| error.to_string())
}
