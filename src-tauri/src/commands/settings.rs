use crate::settings::{self, LocalModel, Settings, SharedSettings};
use tauri::{Emitter, Manager};

/// Tells every window (the app's settings pages, the tray flyout) that the
/// saved settings changed, so none of them saves a stale copy over them.
fn announce(app: &tauri::AppHandle) {
    let _ = app.emit("settings://changed", ());
}

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

/// The Settings card's inline check of a command hotkey against the dictation
/// hotkey, as typed or recorded and before saving: the reason it would be
/// rejected, or null. Pure parsing, so it runs inline.
#[tauri::command]
pub fn check_command_hotkey(command: String, dictation: String) -> Option<String> {
    crate::hotkey::validate_command_hotkey(&command, &dictation).err()
}

/// Serializes saves without holding SharedSettings across disk or main-thread IO.
#[derive(Default)]
pub struct SaveLock(std::sync::Mutex<()>);

/// PARLA-8 IPC result: { "warning": null | string }. Persistence failure rejects
/// the invoke. A hotkey failure resolves with a warning: requested settings are
/// saved, while the previous runtime binding remains until a successful rebind.
#[derive(Debug, serde::Serialize)]
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
        let previous = snapshot(&shared)?;
        let saved = save_with(
            &shared,
            settings,
            |settings| settings::save(&path, settings),
            |settings| settings::apply(&app, settings),
        )?;
        sync_model_residency(&shared, &previous);
        announce(&app);
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
        let previous = snapshot(&shared)?;
        select_provider_with(&shared, &id, |value| settings::save(&path, value))?;
        sync_model_residency(&shared, &previous);
        announce(&app);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Picks the local model. A model that is not on disk is downloaded first
/// (with the usual progress events), so the provider never points at missing
/// files. Resolves with the model selected afterwards: a later selection made
/// during the download wins over this one.
#[tauri::command]
pub async fn select_model(app: tauri::AppHandle, id: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let model = LocalModel::from_id(&id).ok_or("Unknown local model")?;
        let selection = crate::models::begin_selection();
        let shared = app.state::<SharedSettings>();
        let current = snapshot(&shared)?;
        if !crate::models::status(model.id(), &current)?.available {
            crate::models::download(&app, model.id(), &current, false)?;
        }
        // The download ran without SaveLock, so other saves were not blocked.
        let serial = app.state::<SaveLock>();
        let _save = serial.0.lock().map_err(|_| "Settings save lock poisoned")?;
        let previous = snapshot(&shared)?;
        if crate::models::selection_current(selection) {
            let path = settings::path(&app)?;
            select_model_with(&shared, model, |value| settings::save(&path, value))?;
            sync_model_residency(&shared, &previous);
            announce(&app);
        }
        Ok(snapshot(&shared)?.local_model.id().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn snapshot(shared: &SharedSettings) -> Result<Settings, String> {
    Ok(shared.read().map_err(|_| "Settings lock poisoned")?.clone())
}

/// Loads or frees the local model when the active provider changes, and
/// swaps it when the local model changes.
fn sync_model_residency(shared: &SharedSettings, previous: &Settings) {
    let Ok(current) = shared.read().map(|s| s.clone()) else {
        return;
    };
    if current.local_model != previous.local_model {
        crate::provider::parakeet::reload(&current);
    } else if current.active_provider != previous.active_provider {
        crate::provider::parakeet::sync_residency(&current);
    }
}

// Caller holds SaveLock. Like select_provider_with, only the model choice
// changes; unfinished settings-form edits never cross this boundary.
fn select_model_with(
    shared: &std::sync::RwLock<Settings>,
    model: LocalModel,
    persist: impl FnOnce(&Settings) -> Result<(), String>,
) -> Result<(), String> {
    let mut requested = shared.read().map_err(|_| "Settings lock poisoned")?.clone();
    requested.local_model = model;
    save_with(shared, requested, persist, |_| Ok(())).map(|_| ())
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
    settings::command_chord(&requested)?;
    let previous = shared.read().map_err(|_| "Settings lock poisoned")?.clone();
    persist(&requested)?;
    // Model and effort are read per command run; only the chord needs applying.
    let rebind = previous.hotkey != requested.hotkey
        || previous.command.enabled != requested.command.enabled
        || previous.command.hotkey != requested.command.hotkey;
    let warning = if rebind {
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
    fn model_selection_persists_only_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let initial = Settings {
            active_provider: "parakeet".into(),
            vocabulary: vec!["Traccia".into()],
            ..Settings::default()
        };
        let shared = RwLock::new(initial.clone());
        select_model_with(&shared, LocalModel::ParakeetUltra, |value| {
            settings::save(&path, value)
        })
        .unwrap();
        let reloaded = settings::load(&path).unwrap();
        assert_eq!(reloaded.local_model, LocalModel::ParakeetUltra);
        assert_eq!(
            shared.read().unwrap().local_model,
            LocalModel::ParakeetUltra
        );
        let expected = Settings {
            local_model: LocalModel::ParakeetUltra,
            ..initial
        };
        assert_eq!(
            serde_json::to_value(reloaded).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert!(
            select_model_with(&shared, LocalModel::ParakeetV3, |_| Err("disk full".into()))
                .is_err()
        );
        assert_eq!(
            shared.read().unwrap().local_model,
            LocalModel::ParakeetUltra
        );
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
    fn command_mode(enabled: bool, hotkey: &str) -> Settings {
        Settings {
            command: settings::CommandSettings {
                enabled,
                hotkey: hotkey.into(),
                model: "gpt-6-luna-test".into(),
                reasoning_effort: "none".into(),
            },
            ..Settings::default()
        }
    }

    #[test]
    fn command_mode_saves_reloads_and_applies_when_only_command_mode_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let shared = RwLock::new(Settings::default());
        for requested in [
            command_mode(true, "LCtrl+Shift"),
            command_mode(true, "Ctrl+Win"),
            command_mode(false, "Ctrl+Win"),
        ] {
            let applied = AtomicBool::new(false);
            save_with(
                &shared,
                requested.clone(),
                |value| settings::save(&path, value),
                |value| {
                    assert_eq!(value.command, requested.command);
                    applied.store(true, Ordering::Relaxed);
                    Ok(())
                },
            )
            .unwrap();
            assert!(applied.load(Ordering::Relaxed), "{:?}", requested.command);
            assert_eq!(settings::load(&path).unwrap().command, requested.command);
            assert_eq!(shared.read().unwrap().command, requested.command);
        }
    }

    #[test]
    fn the_default_command_hotkey_is_written_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let shared = RwLock::new(Settings::default());
        save_with(
            &shared,
            command_mode(true, "LCtrl+Shift"),
            |value| settings::save(&path, value),
            |_| Ok(()),
        )
        .unwrap();
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(file.contains(r#""hotkey": "LCtrl+Shift""#), "{file}");
    }

    #[test]
    fn an_enabled_command_hotkey_that_collides_is_rejected_and_not_saved() {
        let shared = RwLock::new(Settings::default());
        for hotkey in ["Ctrl+Alt", "Ctrl+V", "LCtrl"] {
            let error = save_with(
                &shared,
                command_mode(true, hotkey),
                |_| panic!("a colliding command hotkey persisted"),
                |_| panic!("a colliding command hotkey applied"),
            )
            .unwrap_err();
            assert!(
                error.contains("command hotkey") || error.contains("Ctrl+V"),
                "{error}"
            );
        }
        assert_eq!(shared.read().unwrap().command, Default::default());
        // Off, it cannot collide with anything, so it never blocks a save.
        save_with(
            &shared,
            command_mode(false, "Ctrl+Alt"),
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(shared.read().unwrap().command.hotkey, "Ctrl+Alt");
    }

    #[test]
    fn warning_shape_is_documented_json() {
        assert_eq!(
            serde_json::to_value(SaveResult { warning: None }).unwrap(),
            serde_json::json!({"warning": null})
        );
    }
}
