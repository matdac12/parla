use crate::settings::{self, Settings, SharedSettings};
use tauri::Manager;

#[tauri::command]
pub async fn get_settings(app: tauri::AppHandle) -> Result<Settings, String> {
    // Keep settings access off the main thread, including future disk work.
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<SharedSettings>()
            .read()
            .map(|s| s.clone())
            .map_err(|_| "Settings lock poisoned".into())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Serializes saves without holding SharedSettings across disk or main-thread IO.
#[derive(Default)]
pub struct SaveLock(std::sync::Mutex<()>);

/// PARLA-8 IPC result: { "warning": null | string }. Persistence failure rejects
/// the invoke. A hotkey failure resolves with a warning: requested settings are
/// saved, while the previous runtime binding remains until a successful rebind.
#[derive(serde::Serialize)]
pub struct SaveResult {
    pub warning: Option<String>,
}

#[tauri::command]
pub async fn save_settings(
    app: tauri::AppHandle,
    settings: Settings,
) -> Result<SaveResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let serial = app.state::<SaveLock>();
        let _save = serial.0.lock().map_err(|_| "Settings save lock poisoned")?;
        let shared = app.state::<SharedSettings>();
        let path = settings::path(&app)?;
        let previous = active_provider(&shared)?;
        let saved = save_with(
            &shared,
            settings,
            |settings| settings::save(&path, settings),
            |settings| settings::apply(&app, settings),
        )?;
        sync_model_residency(&shared, &previous);
        Ok(saved)
    })
    .await
    .map_err(|e| e.to_string())?
}

// Caller holds SaveLock; the settings RwLock is held only to snapshot/commit.
#[tauri::command]
pub async fn select_provider(app: tauri::AppHandle, id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let serial = app.state::<SaveLock>();
        let _save = serial.0.lock().map_err(|_| "Settings save lock poisoned")?;
        let shared = app.state::<SharedSettings>();
        let path = settings::path(&app)?;
        let previous = active_provider(&shared)?;
        select_provider_with(&shared, &id, |value| settings::save(&path, value))?;
        sync_model_residency(&shared, &previous);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn active_provider(shared: &SharedSettings) -> Result<String, String> {
    Ok(shared
        .read()
        .map_err(|_| "Settings lock poisoned")?
        .active_provider
        .clone())
}

/// Loads or frees the local model when the active provider changes.
fn sync_model_residency(shared: &SharedSettings, previous: &str) {
    let Ok(current) = shared.read().map(|s| s.clone()) else {
        return;
    };
    if current.active_provider != previous {
        crate::provider::parakeet::sync_residency(&current);
    }
}

// Caller holds SaveLock. Update only the persisted provider choice; unfinished
// settings-form edits never cross this boundary.
fn select_provider_with(
    shared: &std::sync::RwLock<Settings>,
    id: &str,
    persist: impl FnOnce(&Settings) -> Result<(), String>,
) -> Result<(), String> {
    if crate::provider::registry::get(id).is_none() {
        return Err("Unknown transcription provider".into());
    }
    let mut requested = shared.read().map_err(|_| "Settings lock poisoned")?.clone();
    requested.active_provider = id.into();
    save_with(shared, requested, persist, |_| Ok(())).map(|_| ())
}

// Caller holds SaveLock; the settings RwLock is held only to snapshot/commit.
fn save_with(
    shared: &std::sync::RwLock<Settings>,
    requested: Settings,
    persist: impl FnOnce(&Settings) -> Result<(), String>,
    apply: impl FnOnce(&Settings) -> Result<(), String>,
) -> Result<SaveResult, String> {
    settings::validate_vocabulary(&requested.vocabulary)?;
    let previous = shared.read().map_err(|_| "Settings lock poisoned")?.clone();
    persist(&requested)?;
    let warning = if previous.hotkey != requested.hotkey {
        apply(&requested).err().map(|error| {
            crate::provider::redact_secrets(&format!(
                "Settings saved, but hotkey was not applied: {error}"
            ))
        })
    } else {
        None
    };
    *shared.write().map_err(|_| "Settings lock poisoned")? = requested;
    Ok(SaveResult { warning })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        RwLock,
    };

    #[test]
    fn invalid_vocabulary_does_not_persist_or_replace_settings() {
        let shared = RwLock::new(Settings::default());
        let requested = Settings {
            vocabulary: vec!["<unsafe>".into()],
            ..Default::default()
        };
        assert!(save_with(
            &shared,
            requested,
            |_| panic!("invalid hints persisted"),
            |_| Ok(())
        )
        .is_err());
        assert!(shared.read().unwrap().vocabulary.is_empty());
    }

    #[test]
    fn provider_selection_survives_reload_without_changing_other_preferences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut initial = Settings {
            restore_delay_ms: 777,
            live_captions: false,
            ..Settings::default()
        };
        initial
            .providers
            .insert("future".into(), serde_json::json!({"model":"keep"}));
        let shared = RwLock::new(initial.clone());
        select_provider_with(&shared, "xai", |value| settings::save(&path, value)).unwrap();
        let reloaded = settings::load(&path).unwrap();
        assert_eq!(reloaded.active_provider, "xai");
        assert_eq!(shared.read().unwrap().active_provider, "xai");
        initial.active_provider = "xai".into();
        assert_eq!(
            serde_json::to_value(reloaded).unwrap(),
            serde_json::to_value(initial).unwrap()
        );
        assert!(select_provider_with(&shared, "openai", |_| Err("disk full".into())).is_err());
        assert_eq!(shared.read().unwrap().active_provider, "xai");
        assert!(select_provider_with(&shared, "unknown", |_| panic!(
            "must reject unknown providers"
        ))
        .is_err());
    }

    #[test]
    fn unchanged_hotkey_saves_without_needing_listener() {
        let shared = RwLock::new(Settings::default());
        let requested = Settings {
            active_provider: "xai".into(),
            ..Settings::default()
        };
        let result = save_with(
            &shared,
            requested,
            |_| Ok(()),
            |_| panic!("unchanged hotkey must not be applied"),
        )
        .unwrap();
        assert!(result.warning.is_none());
        assert_eq!(shared.read().unwrap().active_provider, "xai");
    }
    #[test]
    fn apply_failure_is_warning_and_holds_no_settings_lock() {
        let shared = RwLock::new(Settings::default());
        let persisted = AtomicBool::new(false);
        let requested = Settings {
            hotkey: "Ctrl+F9".into(),
            active_provider: "xai".into(),
            ..Settings::default()
        };
        let result = save_with(
            &shared,
            requested,
            |_| {
                assert!(shared.try_write().is_ok());
                persisted.store(true, Ordering::Relaxed);
                Ok(())
            },
            |_| {
                assert!(persisted.load(Ordering::Relaxed));
                assert!(shared.try_write().is_ok(), "apply must not hold the RwLock");
                Err("Shortcut taken".into())
            },
        )
        .unwrap();
        assert!(result.warning.unwrap().contains("Shortcut taken"));
        assert_eq!(shared.read().unwrap().active_provider, "xai");
        assert_eq!(shared.read().unwrap().hotkey, "Ctrl+F9");
    }
    #[test]
    fn persist_failure_does_not_apply_or_commit() {
        let shared = RwLock::new(Settings::default());
        let requested = Settings {
            hotkey: "Ctrl+F9".into(),
            ..Settings::default()
        };
        assert!(save_with(
            &shared,
            requested,
            |_| Err("disk full".into()),
            |_| panic!("must not apply after disk failure")
        )
        .is_err());
        assert_eq!(
            shared.read().unwrap().hotkey,
            crate::hotkey::DEFAULT_SHORTCUT
        );
    }
    #[test]
    fn warning_shape_is_documented_json() {
        assert_eq!(
            serde_json::to_value(SaveResult { warning: None }).unwrap(),
            serde_json::json!({"warning": null})
        );
    }
}
