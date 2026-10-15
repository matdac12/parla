//! Failure notifications and a single persisted clip, shared by tray and IPC retry.
use crate::{
    clip::Clip,
    flow::{self, DictationState},
    provider::{registry, ProviderError},
    settings::SharedSettings,
};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Instant, SystemTime},
};
use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

pub struct FailedDictation {
    pub clip: Clip,
    pub error: ProviderError,
}
pub type LastFailedClip = Mutex<Option<FailedDictation>>;
/// Serializes retry requests, including lazy disk loading, without holding the clip mutex.
#[derive(Default)]
pub struct RetryLock(Mutex<()>);
pub struct RetryMenu(pub tauri::menu::MenuItem<tauri::Wry>);

/// Only trusted summaries are displayed: raw server, transport and config strings
/// can contain credentials of arbitrary shape, so none are copied into the toast.
pub fn user_message(provider_display_name: &str, error: &ProviderError) -> String {
    let reason = match error {
        ProviderError::MissingConfig(reason) => {
            let lower = reason.to_lowercase();
            if lower.contains("model")
                && (lower.contains("not downloaded")
                    || lower.contains("not found")
                    || lower.contains("missing"))
            {
                "model not downloaded — open Settings."
            } else {
                "configuration missing — open Settings."
            }
        }
        ProviderError::Auth(reason) => {
            if reason.contains("403") {
                "API key rejected (403). Check Settings."
            } else if reason.contains("401") {
                "invalid API key (401). Check Settings."
            } else {
                "API key rejected. Check Settings."
            }
        }
        ProviderError::Network(_) => "no network connection. Check your connection.",
        ProviderError::Timeout => "timed out after 30 s. Try again.",
        ProviderError::Http { status: 401, .. } => "invalid API key (401). Check Settings.",
        ProviderError::Http { status: 403, .. } => "API key rejected (403). Check Settings.",
        ProviderError::Http { status, .. } => {
            return format!("{provider_display_name}: server returned HTTP {status}. Try again.")
        }
        ProviderError::Other(reason) => match reason.as_str() {
            "No microphone available" => return "No microphone available.".into(),
            "No audio captured" => {
                return "No audio captured. Hold the hotkey while speaking.".into()
            }
            "Provider returned an empty transcript" => "no speech recognized. Try again.",
            _ if reason.starts_with("Recording failed:") => {
                "microphone recording failed. Check your input device."
            }
            _ if reason.starts_with("Paste failed:") => {
                "could not paste the transcript. Try again."
            }
            _ => "dictation failed. Check Settings and try again.",
        },
    };
    format!("{provider_display_name}: {reason}")
}

pub fn notify_error(app: &tauri::AppHandle, provider_id: &str, error: &ProviderError) {
    let provider = registry::get(provider_id);
    let name = provider.as_ref().map_or("Provider", |p| p.display_name());
    notify_failure(app, &user_message(name, error));
}

fn notify_failure(app: &tauri::AppHandle, message: &str) {
    // Windows support is guaranteed only for installed apps. The plugin uses the
    // configured identifier for installed builds; development can use PowerShell's
    // AUMID. Standalone exe delivery may silently fail, so ALWAYS set the tray fallback.
    // https://v2.tauri.app/plugin/notification/
    // https://github.com/tauri-apps/plugins-workspace/blob/v2/plugins/notification/src/desktop.rs
    if let Some(tray) = app.tray_by_id("parla") {
        if let Err(error) = tray.set_tooltip(Some(format!("Parla — {message}"))) {
            eprintln!("Cannot update error tooltip: {error}");
        }
        // An opaque red square with a white exclamation mark remains visible
        // even if a hotkey rebind subsequently replaces the shared tooltip.
        let mut rgba = vec![0_u8; 16 * 16 * 4];
        for y in 0..16 {
            for x in 0..16 {
                let white = (7..=8).contains(&x) && ((3..=9).contains(&y) || y == 12);
                let offset = (y * 16 + x) * 4;
                rgba[offset..offset + 4].copy_from_slice(if white {
                    &[255, 255, 255, 255]
                } else {
                    &[190, 30, 30, 255]
                });
            }
        }
        if let Err(error) = tray.set_icon(Some(tauri::image::Image::new_owned(rgba, 16, 16))) {
            eprintln!("Cannot update error icon: {error}");
        }
    }
    show_toast(app, "Parla — dictation failed", message);
}

fn notify_info(app: &tauri::AppHandle, message: &str) {
    show_toast(app, "Parla", message);
}

fn show_toast(app: &tauri::AppHandle, title: &str, message: &str) {
    eprintln!("Parla: {message}");
    if let Err(error) = app
        .notification()
        .builder()
        .title(title)
        .body(message)
        .show()
    {
        eprintln!("Notification could not be sent: {error}");
    }
}

/// A successful paste clears error visuals only. Previous recovery audio and
/// retry availability intentionally survive an ordinary successful dictation.
pub fn on_success(app: &tauri::AppHandle) {
    if let Some(tray) = app.tray_by_id("parla") {
        if let Err(error) = tray.set_icon(app.default_window_icon().cloned()) {
            eprintln!("Cannot restore tray icon: {error}");
        }
    }
    crate::hotkey::update_tooltip(app, app.try_state::<crate::hotkey::Hotkey>().is_some());
}

pub fn clip_path(app: &tauri::AppHandle) -> tauri::Result<PathBuf> {
    Ok(app
        .path()
        .app_data_dir()?
        .join("clips")
        .join("last-failed.wav"))
}
fn save_clip(path: &Path, clip: &Clip) -> anyhow::Result<()> {
    // Publish a complete WAV. If replacement fails, remove the previous clip so
    // a restart cannot mistake stale audio for the newest failed dictation.
    let temporary = path.with_extension("wav.tmp");
    let result = clip
        .save_wav(&temporary)
        .and_then(|()| std::fs::rename(&temporary, path).map_err(anyhow::Error::from));
    if result.is_err() {
        for stale in [path, temporary.as_path()] {
            if let Err(error) = std::fs::remove_file(stale) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "Cannot remove stale recovery audio {}: {error}",
                        stale.display()
                    );
                }
            }
        }
    }
    result
}

fn load_clip(path: &Path) -> anyhow::Result<Option<FailedDictation>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some(FailedDictation {
        clip: Clip::from_wav_bytes(&bytes)?,
        error: ProviderError::Other("Recovered failed clip".into()),
    }))
}
fn take_failed(failed: &LastFailedClip) -> Option<FailedDictation> {
    failed.lock().unwrap_or_else(|e| e.into_inner()).take()
}
fn prefer_latest(
    failed: &LastFailedClip,
    candidate: Option<FailedDictation>,
) -> Option<FailedDictation> {
    take_failed(failed).or(candidate)
}
fn restore_failed(failed: &LastFailedClip, clip: FailedDictation) {
    // An active dictation may have failed while retry was checking admission.
    failed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert(clip);
}
fn set_retry_enabled(app: &tauri::AppHandle, enabled: bool) {
    if let Some(menu) = app.try_state::<RetryMenu>() {
        if let Err(error) = menu.0.set_enabled(enabled) {
            eprintln!("Cannot update retry menu: {error}");
        }
    }
}

pub fn on_failure(app: &tauri::AppHandle, error: ProviderError, clip: Clip, provider_id: &str) {
    let persisted = clip_path(app)
        .map_err(anyhow::Error::from)
        .and_then(|path| save_clip(&path, &clip));
    *app.state::<LastFailedClip>()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(FailedDictation {
        clip,
        error: error.clone(),
    });
    set_retry_enabled(app, true);
    notify_error(app, provider_id, &error);
    let _ = app.emit("failure://changed", true);
    if persisted.is_err() {
        notify_info(
            app,
            "Failed clip kept in memory, but could not save it to disk. Retry before quitting.",
        );
    }
}

/// True when a failed dictation can still be retried (in memory or on disk).
pub fn has_failed_clip(app: &tauri::AppHandle) -> bool {
    app.state::<LastFailedClip>()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
        || clip_path(app).is_ok_and(|path| path.exists())
}

/// Retry driven from the pill or the tray: shows the spinner, pastes, then the
/// pill settles on a check or a cross (with another Retry button).
pub fn retry_with_pill(app: &tauri::AppHandle) -> Result<(), String> {
    let overlay = crate::overlay::begin_processing(app);
    let result = retry_last_failed(app, true);
    overlay.finish(result.is_ok());
    let _ = app.emit("failure://changed", has_failed_clip(app));
    result
}

/// Must run on a blocking worker. No clip/settings mutex is held across provider
/// work or paste. The admission guard covers clipboard restoration as well.
pub fn retry_last_failed(app: &tauri::AppHandle, paste: bool) -> Result<(), String> {
    let retry_lock = app.state::<RetryLock>();
    let Ok(_retry) = retry_lock.0.try_lock() else {
        notify_info(app, "Busy, try again");
        return Err("Busy, try again".into());
    };
    let failed = app.state::<LastFailedClip>();
    let saved = match take_failed(&failed) {
        Some(saved) => Some(saved),
        None => match clip_path(app)
            .map_err(anyhow::Error::from)
            .and_then(|path| load_clip(&path))
        {
            Ok(saved) => saved,
            Err(_) => {
                notify_failure(
                    app,
                    "Could not read the saved failed clip. Record a new dictation.",
                );
                return Err("Could not read the saved failed clip".into());
            }
        },
    };
    let state = app.state::<DictationState>();
    let Some(_guard) = state.try_begin() else {
        if let Some(saved) = saved {
            restore_failed(&failed, saved);
            set_retry_enabled(app, true);
        }
        notify_info(app, "Busy, try again");
        return Err("Busy, try again".into());
    };
    // The active dictation may have failed between taking the old clip and
    // acquiring admission. Prefer its newer failure now that we own the gate.
    let Some(saved) = prefer_latest(&failed, saved) else {
        set_retry_enabled(app, false);
        return Err("No failed dictation to retry".into());
    };
    set_retry_enabled(app, false);
    let settings = app
        .state::<SharedSettings>()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    flow::finish_session(
        app,
        Ok(saved.clip),
        &settings,
        SystemTime::now(),
        Instant::now(),
        paste,
    );
    let error = failed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|saved| user_message("Provider", &saved.error));
    if let Some(error) = error {
        return Err(error);
    }
    if let Err(error) = clip_path(app)
        .map_err(anyhow::Error::from)
        .and_then(|path| match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        })
    {
        eprintln!("Cannot remove recovered clip: {error}");
        set_retry_enabled(app, true);
        notify_info(
            app,
            "Dictation pasted, but the old recovery file could not be removed.",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_cover_all_variants_without_remote_details() {
        for (error, expected) in [
            (
                ProviderError::MissingConfig("model not downloaded".into()),
                "Parakeet: model not downloaded — open Settings.",
            ),
            (
                ProviderError::Auth("HTTP 401 sk-secret".into()),
                "Parakeet: invalid API key (401). Check Settings.",
            ),
            (
                ProviderError::Network("private request".into()),
                "Parakeet: no network connection. Check your connection.",
            ),
            (
                ProviderError::Timeout,
                "Parakeet: timed out after 30 s. Try again.",
            ),
            (
                ProviderError::Http {
                    status: 503,
                    body: "private body".into(),
                },
                "Parakeet: server returned HTTP 503. Try again.",
            ),
            (
                ProviderError::Other("No microphone available".into()),
                "No microphone available.",
            ),
            (
                ProviderError::Other("No audio captured".into()),
                "No audio captured. Hold the hotkey while speaking.",
            ),
            (
                ProviderError::Other("private arbitrary-key".into()),
                "Parakeet: dictation failed. Check Settings and try again.",
            ),
        ] {
            assert_eq!(user_message("Parakeet", &error), expected);
        }
    }

    #[test]
    fn configuration_and_auth_details_cannot_leak_secrets() {
        let secret = "arbitrary-provider-secret";
        for error in [
            ProviderError::MissingConfig(secret.into()),
            ProviderError::Auth(secret.into()),
            ProviderError::Http {
                status: 401,
                body: secret.into(),
            },
        ] {
            let message = user_message("OpenAI", &error);
            assert!(!message.contains(secret));
            assert!(message.chars().count() < 160);
        }
        assert!(user_message("OpenAI", &ProviderError::Auth("HTTP 403".into())).contains("403"));
    }

    #[test]
    fn busy_retry_restores_clip_and_releases_mutex_before_admission() {
        let failed = LastFailedClip::new(Some(FailedDictation {
            clip: Clip::new(vec![42]),
            error: ProviderError::Timeout,
        }));
        let state = crate::flow::DictationState::default();
        let guard = state.try_begin().unwrap();
        let clip = take_failed(&failed).unwrap();
        assert!(failed.try_lock().is_ok());
        assert!(state.try_begin().is_none());
        restore_failed(&failed, clip);
        assert_eq!(failed.lock().unwrap().as_ref().unwrap().clip.samples, [42]);
        drop(guard);
        assert!(state.try_begin().is_some());
    }

    #[test]
    fn retry_admitted_after_active_failure_prefers_the_newest_clip() {
        let failed = LastFailedClip::new(Some(FailedDictation {
            clip: Clip::new(vec![1]),
            error: ProviderError::Timeout,
        }));
        let state = DictationState::default();
        let active = state.try_begin().unwrap();
        let old = take_failed(&failed).unwrap();
        restore_failed(
            &failed,
            FailedDictation {
                clip: Clip::new(vec![2]),
                error: ProviderError::Timeout,
            },
        );
        drop(active);
        let _retry = state.try_begin().unwrap();
        let newest = prefer_latest(&failed, Some(old)).unwrap();
        assert_eq!(newest.clip.samples, [2]);
        assert!(take_failed(&failed).is_none());
    }

    #[test]
    fn initially_empty_retry_also_observes_a_new_failure_after_admission() {
        let failed = LastFailedClip::default();
        let old = take_failed(&failed);
        restore_failed(
            &failed,
            FailedDictation {
                clip: Clip::new(vec![3]),
                error: ProviderError::Timeout,
            },
        );
        assert_eq!(prefer_latest(&failed, old).unwrap().clip.samples, [3]);
        assert!(take_failed(&failed).is_none());
    }

    #[test]
    fn busy_retry_does_not_overwrite_newer_failure() {
        let failed = LastFailedClip::new(Some(FailedDictation {
            clip: Clip::new(vec![2]),
            error: ProviderError::Timeout,
        }));
        restore_failed(
            &failed,
            FailedDictation {
                clip: Clip::new(vec![1]),
                error: ProviderError::Timeout,
            },
        );
        assert_eq!(take_failed(&failed).unwrap().clip.samples, [2]);
    }

    #[test]
    fn failed_save_does_not_resurrect_the_previous_clip_after_restart() {
        let dir = std::env::temp_dir().join(format!(
            "parla-retry-save-failure-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("last-failed.wav");
        save_clip(&path, &Clip::new(vec![1])).unwrap();
        // A directory at the temporary path deterministically prevents writing
        // the replacement WAV, without relying on Unix permissions or disk size.
        std::fs::create_dir(path.with_extension("wav.tmp")).unwrap();
        assert!(save_clip(&path, &Clip::new(vec![2])).is_err());
        assert!(load_clip(&path).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn persisted_clip_is_loaded_lazily_and_invalid_wav_is_reported() {
        let dir = std::env::temp_dir().join(format!(
            "parla-retry-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("last-failed.wav");
        assert!(load_clip(&path).unwrap().is_none());
        save_clip(&path, &Clip::new(vec![1, -2, 3])).unwrap();
        let loaded = load_clip(&path).unwrap().unwrap();
        assert_eq!(loaded.clip.samples, [1, -2, 3]);
        assert_eq!(loaded.clip.sample_rate, crate::clip::SAMPLE_RATE);
        save_clip(&path, &Clip::new(vec![4])).unwrap();
        assert_eq!(load_clip(&path).unwrap().unwrap().clip.samples, [4]);
        std::fs::write(&path, b"broken").unwrap();
        assert!(load_clip(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
