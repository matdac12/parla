//! The first-run setup wizard (PARLA-100): when it opens on its own, and the
//! microphone meter its Microphone step shows. The wizard is a normal window
//! (`tray::open_setup`) on the normal settings and dictation code; nothing here
//! sits on the hotkey-to-paste path.
use crate::flow::DictationGuard;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager};

/// The wizard's window label, and the target of its events.
pub const LABEL: &str = "setup";

/// A new install has neither a settings file nor a history yet. Any settings
/// file there, even one that does not parse or cannot be read, means someone
/// already set Parla up; a history file means it was used on defaults.
pub fn is_first_run(settings_path: &Path, history_path: &Path) -> bool {
    let missing = |path: &Path| {
        matches!(
            std::fs::metadata(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    };
    missing(settings_path) && missing(history_path)
}

/// The wizard window is gone, however it closed.
pub fn closed(app: &tauri::AppHandle) {
    app.state::<MicMeter>().stop();
    crate::commands::settings::save_if_absent(app);
}

/// How long the meter listens before it stops itself, so a forgotten wizard
/// cannot hold the microphone and the dictation guard.
const MAX_METERING: Duration = Duration::from_secs(60);
const METER_TICK: Duration = Duration::from_millis(40);

/// The Microphone step's live level. It records into nothing: the clip is
/// dropped unread. While it runs it holds the dictation guard, like the
/// benchmark's recording, so a hotkey press cannot meet it halfway.
#[derive(Default)]
pub struct MicMeter {
    running: Mutex<Option<(u64, DictationGuard)>>,
    last: AtomicU64,
}

impl MicMeter {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(u64, DictationGuard)>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_running(&self, token: u64) -> bool {
        self.lock().as_ref().is_some_and(|(t, _)| *t == token)
    }

    /// Starts the meter. Already running is fine.
    pub fn start(&self, app: &tauri::AppHandle) -> Result<(), String> {
        let mut running = self.lock();
        if running.is_some() {
            return Ok(());
        }
        let guard = app
            .state::<crate::flow::DictationState>()
            .try_begin()
            .ok_or("Dictation in progress")?;
        let recorder = crate::recorder::recorder().ok_or("No microphone available")?;
        recorder
            .start()
            .map_err(|error| format!("Could not open the microphone: {error:#}"))?;
        let token = self.last.fetch_add(1, Ordering::AcqRel) + 1;
        *running = Some((token, guard));
        drop(running);

        let app = app.clone();
        std::thread::Builder::new()
            .name("parla-setup-meter".into())
            .spawn(move || {
                let began = Instant::now();
                while app.state::<MicMeter>().is_running(token) && began.elapsed() < MAX_METERING {
                    let _ = app.emit_to(LABEL, "setup://level", crate::recorder::current_level());
                    std::thread::sleep(METER_TICK);
                }
                // Out of time (not stopped by the page): tell it.
                if app.state::<MicMeter>().stop_token(token) {
                    let _ = app.emit_to(LABEL, "setup://meter-stopped", ());
                }
            })
            .map_err(|error| {
                self.stop();
                error.to_string()
            })?;
        Ok(())
    }

    pub fn stop(&self) {
        let taken = self.lock().take();
        Self::release(taken);
    }

    /// Stops the meter only if it is still the run `token` started.
    fn stop_token(&self, token: u64) -> bool {
        let taken = {
            let mut running = self.lock();
            match running.as_ref() {
                Some((current, _)) if *current == token => running.take(),
                _ => None,
            }
        };
        let stopped = taken.is_some();
        Self::release(taken);
        stopped
    }

    /// Stops the capture and drops the clip, then lets dictation start again.
    fn release(run: Option<(u64, DictationGuard)>) {
        if let Some((_, guard)) = run {
            if let Some(recorder) = crate::recorder::recorder() {
                drop(recorder.stop());
            }
            drop(guard);
        }
    }
}

/// The Windows default microphone's name, which is the one Parla records from.
pub fn default_microphone() -> Option<String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let device = cpal::default_host().default_input_device()?;
    Some(device.description().ok()?.name().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        (
            dir.path().join("settings.json"),
            dir.path().join("history.jsonl"),
        )
    }

    #[test]
    fn nothing_on_disk_is_a_first_run() {
        let dir = tempfile::tempdir().unwrap();
        let (settings, history) = paths(&dir);
        assert!(is_first_run(&settings, &history));
        // Not even the folder exists yet on a fresh install.
        assert!(is_first_run(
            &dir.path().join("new").join("settings.json"),
            &history
        ));
    }

    #[test]
    fn any_settings_file_means_set_up_already() {
        let dir = tempfile::tempdir().unwrap();
        let (path, history) = paths(&dir);
        for contents in ["{}", r#"{"hotkey":"F9"}"#, "not json", ""] {
            std::fs::write(&path, contents).unwrap();
            assert!(!is_first_run(&path, &history), "{contents:?}");
        }
        // A file Parla cannot read is still somebody's settings.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(!is_first_run(&path, &history));
    }

    #[test]
    fn a_history_without_settings_is_a_parla_used_on_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let (settings, history) = paths(&dir);
        std::fs::write(&history, b"{}\n").unwrap();
        assert!(!is_first_run(&settings, &history));
    }

    #[test]
    fn saving_defaults_ends_first_run_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let (path, history) = paths(&dir);
        crate::settings::save(&path, &crate::settings::Settings::default()).unwrap();
        assert!(!is_first_run(&path, &history));
        assert!(crate::settings::load(&path).is_ok());
    }
}
