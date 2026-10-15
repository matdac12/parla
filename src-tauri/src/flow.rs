//! Hotkey callbacks enqueue only. A responsive coordinator drops overlapping
//! holds while one session worker owns recording, transcription and paste.
pub use crate::failure::on_failure;
use crate::{
    hotkey::{Hotkey, HotkeyEvent},
    inserter,
    provider::{registry, ProviderError, SttProvider},
    settings::{Settings, SharedSettings},
    timing::{self, DictationRecord},
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant, SystemTime},
};
use tauri::Manager;

/// Remember ignored holds until their release, even when the previous session
/// finishes in the middle of that hold. Repeats cannot start a partial clip.
#[derive(Default)]
struct HoldGate {
    holding: bool,
    accepted: bool,
}
impl HoldGate {
    fn press(&mut self, busy: bool) -> bool {
        // A session may have timed out without receiving its release. Once
        // its worker exits, the next genuine press can recover that stale hold.
        if self.holding && self.accepted && !busy {
            self.release();
        }
        if self.holding {
            return false;
        }
        self.holding = true;
        self.accepted = !busy;
        self.accepted
    }
    fn release(&mut self) -> bool {
        self.holding = false;
        std::mem::take(&mut self.accepted)
    }
}
/// Shared admission gate for hotkey sessions and PARLA-9 retries. Keep the
/// returned guard alive through finish_session (including clipboard restore).
#[derive(Default)]
pub struct DictationState(Arc<AtomicBool>);
impl DictationState {
    pub fn try_begin(&self) -> Option<DictationGuard> {
        self.0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| DictationGuard(self.0.clone()))
    }
    fn is_busy(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
pub struct DictationGuard(Arc<AtomicBool>);
impl Drop for DictationGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub fn setup(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    app.manage(DictationState::default());
    // Warm even when hotkey registration is unavailable (Test mic still works).
    std::thread::Builder::new()
        .name("parla-warmup".into())
        .spawn(crate::recorder::warm_up)?;
    let (events, receiver) = mpsc::channel();
    let hotkey = match Hotkey::new(app.handle(), events) {
        Ok(hotkey) => hotkey,
        Err(error) => {
            eprintln!("Parla hotkey disabled: {error}");
            crate::hotkey::update_tooltip(app.handle(), false);
            return Ok(());
        }
    };
    app.manage(hotkey);
    let app = app.handle().clone();
    std::thread::Builder::new()
        .name("parla-flow".into())
        .spawn(move || {
            let initial = app
                .state::<SharedSettings>()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Err(error) = crate::settings::apply(&app, &initial) {
                eprintln!("Saved hotkey could not be applied: {error}");
            }
            let busy = app.state::<DictationState>();
            let mut gate = HoldGate::default();
            let mut release: Option<mpsc::Sender<Instant>> = None;
            for event in receiver {
                match event {
                    HotkeyEvent::Pressed if gate.press(busy.is_busy()) => {
                        let Some(guard) = busy.try_begin() else {
                            gate.accepted = false;
                            continue;
                        };
                        let started_at = SystemTime::now();
                        let settings = app
                            .state::<SharedSettings>()
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let (sender, released) = mpsc::channel();
                        let session_app = app.clone();
                        match std::thread::Builder::new()
                            .name("parla-dictation".into())
                            .spawn(move || {
                                let _guard = guard;
                                run_session(&session_app, settings, started_at, released);
                            }) {
                            Ok(_) => release = Some(sender),
                            Err(error) => eprintln!("Cannot start dictation worker: {error}"),
                        }
                    }
                    HotkeyEvent::Released if gate.release() => {
                        if let Some(sender) = release.take() {
                            let _ = sender.send(Instant::now());
                        }
                    }
                    _ => {}
                }
            }
        })?;
    Ok(())
}

fn run_session(
    app: &tauri::AppHandle,
    settings: Settings,
    started_at: SystemTime,
    released: mpsc::Receiver<Instant>,
) {
    let recording = crate::recorder::recorder()
        .ok_or_else(|| ProviderError::Other("No microphone available".into()))
        .and_then(|recorder| {
            recorder
                .start()
                .map_err(|e| ProviderError::Other(format!("Recording failed: {e}")))?;
            Ok(recorder)
        });
    let (key_up, hold_error) = wait_for_release(&released, Duration::from_secs(120));
    let audio = recording.map(|recorder| recorder.stop());
    if hold_error.is_some() {
        finish_session_with_error(app, audio, &settings, started_at, key_up, hold_error);
    } else {
        finish_session(app, audio, &settings, started_at, key_up);
    }
}

fn wait_for_release(
    released: &mpsc::Receiver<Instant>,
    limit: Duration,
) -> (Instant, Option<String>) {
    match released.recv_timeout(limit) {
        Ok(key_up) => (key_up, None),
        Err(mpsc::RecvTimeoutError::Timeout) => (
            Instant::now(),
            Some(format!("hold exceeded {} s", limit.as_secs())),
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => (
            Instant::now(),
            Some("Hotkey release channel disconnected".into()),
        ),
    }
}

/// Reusable completion path for PARLA-9: acquire DictationState::try_begin on a
/// blocking worker, snapshot SharedSettings, then pass Ok(saved_clip) here.
/// A retry uses its own wall-clock start and Instant::now() as key_up.
pub fn finish_session(
    app: &tauri::AppHandle,
    audio: Result<crate::clip::Clip, ProviderError>,
    settings: &Settings,
    started_at: SystemTime,
    key_up: Instant,
) {
    finish_session_with_error(app, audio, settings, started_at, key_up, None);
}

fn finish_session_with_error(
    app: &tauri::AppHandle,
    audio: Result<crate::clip::Clip, ProviderError>,
    settings: &Settings,
    started_at: SystemTime,
    key_up: Instant,
    hold_error: Option<String>,
) {
    let mut record = DictationRecord::new(started_at, &settings.active_provider);
    let result = match audio {
        Err(error) => {
            crate::failure::notify_error(app, &settings.active_provider, &error);
            Err(error)
        }
        Ok(clip) => {
            record.audio_ready = Some(timing::elapsed_ms(key_up));
            record.audio_ms = clip.duration().as_millis() as u64;
            let result = if clip.samples.is_empty() {
                Err(ProviderError::Other("No audio captured".into()))
            } else {
                registry::get(&settings.active_provider)
                    .ok_or_else(|| {
                        ProviderError::MissingConfig(format!(
                            "Unknown provider: {}",
                            settings.active_provider
                        ))
                    })
                    .and_then(|provider| {
                        transcribe_and_paste(
                            provider.as_ref(),
                            &clip,
                            settings,
                            key_up,
                            &mut record,
                            |text, delay| {
                                inserter::paste_text_timed(text, delay).map_err(|e| e.to_string())
                            },
                        )
                    })
            };
            if let Err(error) = &result {
                on_failure(app, error.clone(), clip, &settings.active_provider);
            }
            result
        }
    };
    if result.is_ok() {
        crate::failure::on_success(app);
    }
    record.error = match (hold_error, result.err()) {
        (Some(hold), Some(error)) => Some(format!("{hold}; {error}")),
        (Some(hold), None) => Some(hold),
        (None, Some(error)) => Some(error.to_string()),
        (None, None) => None,
    };
    // The one completion sink for successful and failed dictations.
    timing::record(app, record);
}

fn transcribe_and_paste(
    provider: &dyn SttProvider,
    clip: &crate::clip::Clip,
    settings: &Settings,
    key_up: Instant,
    record: &mut DictationRecord,
    paste: impl FnOnce(&str, Duration) -> Result<Instant, String>,
) -> Result<(), ProviderError> {
    if let crate::provider::Availability::Unavailable(reason) = provider.availability(settings) {
        return Err(ProviderError::MissingConfig(reason));
    }
    let transcript = provider.transcribe(clip, settings);
    record.provider_returned = Some(timing::elapsed_ms(key_up));
    let text = transcript?;
    record.text = text;
    if record.text.trim().is_empty() {
        return Err(ProviderError::Other(
            "Provider returned an empty transcript".into(),
        ));
    }
    let pasted = paste(
        &record.text,
        Duration::from_millis(settings.restore_delay_ms),
    )
    .map_err(|error| ProviderError::Other(format!("Paste failed: {error}")))?;
    record.pasted = Some(pasted.saturating_duration_since(key_up).as_millis() as u64);
    Ok(())
}
#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::provider::{Availability, SttProvider};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct FakeProvider(Result<String, ProviderError>);
    impl SttProvider for FakeProvider {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn display_name(&self) -> &'static str {
            "Fake"
        }
        fn availability(&self, _: &Settings) -> Availability {
            Availability::Available
        }
        fn transcribe(&self, _: &crate::clip::Clip, _: &Settings) -> Result<String, ProviderError> {
            self.0.clone()
        }
    }
    #[test]
    fn unavailable_provider_never_transcribes_or_pastes() {
        struct Unavailable;
        impl SttProvider for Unavailable {
            fn id(&self) -> &'static str {
                "unavailable"
            }
            fn display_name(&self) -> &'static str {
                "Unavailable"
            }
            fn availability(&self, _: &Settings) -> Availability {
                Availability::Unavailable("model not downloaded".into())
            }
            fn transcribe(
                &self,
                _: &crate::clip::Clip,
                _: &Settings,
            ) -> Result<String, ProviderError> {
                panic!("unavailable provider must not transcribe")
            }
        }
        let mut record = DictationRecord::new(SystemTime::now(), "unavailable");
        let result = transcribe_and_paste(
            &Unavailable,
            &crate::clip::Clip::new(vec![1]),
            &Settings::default(),
            Instant::now(),
            &mut record,
            |_, _| panic!("must not paste"),
        );
        assert_eq!(
            result,
            Err(ProviderError::MissingConfig("model not downloaded".into()))
        );
        assert!(record.provider_returned.is_none());
        assert!(record.pasted.is_none());
    }
    #[test]
    fn lost_release_times_out_and_a_later_press_can_start_again() {
        let (_sender, receiver) = mpsc::channel();
        let state = DictationState::default();
        let mut gate = HoldGate::default();
        assert!(gate.press(state.is_busy()));
        let guard = state.try_begin().unwrap();
        let (_, error) = wait_for_release(&receiver, Duration::from_millis(1));
        assert!(error.unwrap().starts_with("hold exceeded"));
        drop(guard);
        assert!(gate.press(state.is_busy()));
    }
    #[test]
    fn real_release_keeps_its_timestamp_without_timeout_error() {
        let (sender, receiver) = mpsc::channel();
        let key_up = Instant::now();
        sender.send(key_up).unwrap();
        let (observed, error) = wait_for_release(&receiver, Duration::from_secs(120));
        assert_eq!(observed, key_up);
        assert!(error.is_none());
    }
    #[test]
    fn shared_gate_prevents_retry_overlap_and_recovers_after_worker_exit() {
        let state = DictationState::default();
        let guard = state.try_begin().unwrap();
        assert!(state.is_busy());
        assert!(state.try_begin().is_none());
        drop(guard);
        assert!(!state.is_busy());
        assert!(state.try_begin().is_some());
    }
    #[test]
    fn busy_hold_is_dropped_even_if_it_finishes_before_release() {
        let mut gate = HoldGate::default();
        assert!(!gate.press(true));
        assert!(!gate.press(false)); // still same ignored hold
        assert!(!gate.release());
        assert!(gate.press(false));
        assert!(gate.release());
        assert!(!gate.release());
    }
    #[test]
    fn pipeline_pastes_only_successful_nonempty_transcripts() {
        for output in [
            Ok("Hello".into()),
            Err(ProviderError::Timeout),
            Ok("  ".into()),
        ] {
            let pasted = AtomicUsize::new(0);
            let mut record = DictationRecord::new(SystemTime::now(), "fake");
            let key_up = Instant::now();
            let result = transcribe_and_paste(
                &FakeProvider(output.clone()),
                &crate::clip::Clip::new(vec![1; 160]),
                &Settings::default(),
                key_up,
                &mut record,
                |text, delay| {
                    assert_eq!(text, "Hello");
                    assert_eq!(delay, Duration::from_millis(250));
                    pasted.fetch_add(1, Ordering::Relaxed);
                    Ok(Instant::now())
                },
            );
            let success = output == Ok("Hello".into());
            assert_eq!(result.is_ok(), success);
            assert_eq!(pasted.load(Ordering::Relaxed), usize::from(success));
            assert!(record.provider_returned.is_some());
            assert_eq!(record.pasted.is_some(), success);
            if success {
                assert!(record.pasted >= record.provider_returned);
            }
        }
    }
    #[test]
    fn timing_uses_injection_time_instead_of_restoration_completion() {
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        let key_up = Instant::now() - Duration::from_millis(20);
        let injected = key_up + Duration::from_millis(5);
        transcribe_and_paste(
            &FakeProvider(Ok("Hello".into())),
            &crate::clip::Clip::new(vec![1]),
            &Settings::default(),
            key_up,
            &mut record,
            |_, _| Ok(injected),
        )
        .unwrap();
        assert_eq!(record.pasted, Some(5));
    }

    #[test]
    fn paste_failure_does_not_mark_pasted() {
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        assert!(transcribe_and_paste(
            &FakeProvider(Ok("Hello".into())),
            &crate::clip::Clip::new(vec![1]),
            &Settings::default(),
            Instant::now(),
            &mut record,
            |_, _| Err("clipboard unavailable".into())
        )
        .is_err());
        assert_eq!(record.text, "Hello");
        assert!(record.pasted.is_none());
    }
}
