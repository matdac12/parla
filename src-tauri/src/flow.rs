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

/// Holds shorter than this (a stray Ctrl+Alt, AltGr typing) are discarded.
pub(crate) const MIN_HOLD: Duration = Duration::from_millis(300);

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Start,
    Stop,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Recording while the Ctrl+Alt chord is held (push to talk).
    Hold(Instant),
    /// Hands-free: started or converted by the main key, ended by the next press.
    Latched,
}

/// Decides what a hotkey event does. Ctrl+Alt held records until released;
/// pressing the main key during the chord (Ctrl+Alt+Space) latches the
/// recording hands-free, and the next press of the chord (or the full
/// shortcut) stops it.
struct Controller {
    phase: Phase,
}
impl Controller {
    fn on_event(&mut self, event: HotkeyEvent, busy: bool, now: Instant) -> Action {
        // A session that ended on its own (timeout, failure) leaves no owner.
        if self.phase != Phase::Idle && !busy {
            self.phase = Phase::Idle;
        }
        match (event, self.phase) {
            (HotkeyEvent::ChordPressed, Phase::Idle) if !busy => {
                self.phase = Phase::Hold(now);
                Action::Start
            }
            (HotkeyEvent::Pressed, Phase::Idle) if !busy => {
                self.phase = Phase::Latched;
                Action::Start
            }
            (HotkeyEvent::Pressed, Phase::Hold(_)) => {
                self.phase = Phase::Latched;
                Action::None
            }
            (HotkeyEvent::Pressed | HotkeyEvent::ChordPressed, Phase::Latched) => {
                self.phase = Phase::Idle;
                Action::Stop
            }
            (HotkeyEvent::ChordReleased, Phase::Hold(since)) => {
                self.phase = Phase::Idle;
                if now.duration_since(since) < MIN_HOLD {
                    Action::Cancel
                } else {
                    Action::Stop
                }
            }
            (HotkeyEvent::ChordCancelled, Phase::Hold(_)) => {
                self.phase = Phase::Idle;
                Action::Cancel
            }
            (HotkeyEvent::Cancel, Phase::Hold(_) | Phase::Latched) => {
                self.phase = Phase::Idle;
                Action::Cancel
            }
            _ => Action::None,
        }
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
            let mut controller = Controller { phase: Phase::Idle };
            let mut session: Option<(mpsc::Sender<Instant>, Arc<AtomicBool>)> = None;
            for event in receiver {
                let before = controller.phase;
                let action = controller.on_event(event, busy.is_busy(), Instant::now());
                if controller.phase != before || action == Action::Start {
                    crate::overlay::set_mode(&app, controller.phase == Phase::Latched);
                }
                match action {
                    Action::None => {}
                    Action::Start => {
                        let Some(guard) = busy.try_begin() else {
                            controller.phase = Phase::Idle;
                            continue;
                        };
                        let started_at = SystemTime::now();
                        let settings = app
                            .state::<SharedSettings>()
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let (sender, released) = mpsc::channel();
                        let cancelled = Arc::new(AtomicBool::new(false));
                        let flag = cancelled.clone();
                        let session_app = app.clone();
                        match std::thread::Builder::new()
                            .name("parla-dictation".into())
                            .spawn(move || {
                                let _guard = guard;
                                run_session(&session_app, settings, started_at, released, flag);
                            }) {
                            Ok(_) => {
                                crate::hotkey::RECORDING.store(true, Ordering::Release);
                                session = Some((sender, cancelled));
                            }
                            Err(error) => {
                                controller.phase = Phase::Idle;
                                eprintln!("Cannot start dictation worker: {error}");
                            }
                        }
                    }
                    action @ (Action::Stop | Action::Cancel) => {
                        if let Some((sender, cancelled)) = session.take() {
                            if action == Action::Cancel {
                                cancelled.store(true, Ordering::Release);
                            }
                            let _ = sender.send(Instant::now());
                        }
                    }
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
    cancelled: Arc<AtomicBool>,
) {
    let recording = crate::recorder::recorder()
        .ok_or_else(|| ProviderError::Other("No microphone available".into()))
        .and_then(|recorder| {
            recorder
                .start()
                .map_err(|e| ProviderError::Other(format!("Recording failed: {e}")))?;
            Ok(recorder)
        });
    let captions = settings.live_captions
        && settings.streaming_enabled
        && matches!(settings.active_provider.as_str(), "openai" | "xai");
    let overlay = recording
        .is_ok()
        .then(|| crate::overlay::begin(app, captions));
    let stream = recording.as_ref().ok().and_then(|recorder| {
        let overlay = overlay.as_ref()?;
        match crate::streaming::Session::start(
            &settings,
            overlay.caption_sink(captions),
            cancelled.clone(),
        ) {
            Ok(Some((session, feed))) => {
                if recorder.subscribe(feed).is_ok() {
                    Some(session)
                } else {
                    None
                }
            }
            Ok(None) => None,
            Err(error) => {
                eprintln!("Live dictation unavailable: {error}");
                None
            }
        }
    });
    let (key_up, hold_error) = wait_for_release(&released, Duration::from_secs(120));
    let stream = if cancelled.load(Ordering::Acquire) {
        drop(stream);
        None
    } else {
        stream
    };
    let audio = recording.map(|recorder| recorder.stop_raw());
    crate::hotkey::RECORDING.store(false, Ordering::Release);
    if cancelled.load(Ordering::Acquire) {
        drop(stream);
        return;
    }
    if let Some(overlay) = &overlay {
        overlay.processing();
    }
    let ok = finish_session_with_error(
        app,
        audio,
        &settings,
        started_at,
        key_up,
        Completion {
            hold_error,
            paste: true,
            stream,
        },
    );
    if let Some(overlay) = overlay {
        overlay.finish(ok);
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
/// A retry uses its own wall-clock start and Instant::now() as key_up. With
/// `paste` false the transcript is only recorded in history.
pub fn finish_session(
    app: &tauri::AppHandle,
    audio: Result<crate::clip::Clip, ProviderError>,
    settings: &Settings,
    started_at: SystemTime,
    key_up: Instant,
    paste: bool,
) {
    finish_session_with_error(
        app,
        audio.map(Into::into),
        settings,
        started_at,
        key_up,
        Completion {
            hold_error: None,
            paste,
            stream: None,
        },
    );
}

struct Completion {
    hold_error: Option<String>,
    paste: bool,
    stream: Option<crate::streaming::Session>,
}

fn finish_session_with_error(
    app: &tauri::AppHandle,
    audio: Result<crate::recorder::RecoveryAudio, ProviderError>,
    settings: &Settings,
    started_at: SystemTime,
    key_up: Instant,
    mut completion: Completion,
) -> bool {
    let mut record = DictationRecord::new(started_at, &settings.active_provider);
    let result = match audio {
        Err(error) => {
            crate::failure::notify_error(app, &settings.active_provider, &error);
            Err(error)
        }
        Ok(clip) => {
            record.audio_ready = Some(timing::elapsed_ms(key_up));
            record.audio_ms = clip.duration().as_millis() as u64;
            let result = if clip.is_empty() {
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
                            completion.stream.take().map(|session| session.finish()),
                            |text, delay| {
                                if completion.paste {
                                    inserter::paste_text_with_options(
                                        text,
                                        delay,
                                        settings.paste_compatibility_mode,
                                    )
                                    .map_err(|e| e.to_string())
                                } else {
                                    // Retried from the app window: the history entry
                                    // is the delivery, there is nothing to paste into.
                                    Ok(Instant::now())
                                }
                            },
                        )
                    })
            };
            if let Err(error) = &result {
                on_failure(
                    app,
                    error.clone(),
                    clip.into_clip(),
                    &settings.active_provider,
                );
            }
            result
        }
    };
    let ok = result.is_ok() && completion.hold_error.is_none();
    if result.is_ok() {
        crate::failure::on_success(app);
    }
    record.error = match (completion.hold_error, result.err()) {
        (Some(hold), Some(error)) => Some(format!("{hold}; {error}")),
        (Some(hold), None) => Some(hold),
        (None, Some(error)) => Some(error.to_string()),
        (None, None) => None,
    };
    // The one completion sink for successful and failed dictations.
    timing::record(app, record);
    ok
}

trait RecoverySource {
    fn batch_clip(&self) -> &crate::clip::Clip;
}
impl RecoverySource for crate::clip::Clip {
    fn batch_clip(&self) -> &crate::clip::Clip {
        self
    }
}
impl RecoverySource for crate::recorder::RecoveryAudio {
    fn batch_clip(&self) -> &crate::clip::Clip {
        self.clip()
    }
}

fn transcribe_and_paste(
    provider: &dyn SttProvider,
    clip: &impl RecoverySource,
    settings: &Settings,
    key_up: Instant,
    record: &mut DictationRecord,
    streamed: Option<Result<crate::streaming::Streamed, ProviderError>>,
    paste: impl FnOnce(&str, Duration) -> Result<Instant, String>,
) -> Result<(), ProviderError> {
    if let crate::provider::Availability::Unavailable(reason) = provider.availability(settings) {
        return Err(ProviderError::MissingConfig(reason));
    }
    let batch_model = match provider.id() {
        "openai" => Some(
            settings
                .provider_config::<crate::provider::openai::OpenAiConfig>("openai")
                .model,
        ),
        "xai" => Some(
            settings
                .provider_config::<crate::provider::xai::XaiConfig>("xai")
                .model,
        ),
        "parakeet" => Some("parakeet-tdt-0.6b-v3".into()),
        _ => None,
    };
    record.model = batch_model;
    let transcript = match streamed {
        Some(Ok(result)) if !result.text.trim().is_empty() => {
            record.transcription_mode = Some("streaming".into());
            record.model = Some(result.model);
            record.stream_setup_ms = result.setup_ms;
            record.first_caption_ms = result.first_caption_ms;
            Ok(result.text)
        }
        attempt => {
            record.transcription_mode = Some(
                if attempt.is_some() {
                    "batch_fallback"
                } else {
                    "batch"
                }
                .into(),
            );
            record.stream_error = attempt.map(|outcome| match outcome {
                Err(error) => error.to_string(),
                Ok(_) => "Live transcription returned an empty final transcript".into(),
            });
            provider.transcribe(clip.batch_clip(), settings)
        }
    };
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

    #[tokio::test]
    async fn rejected_controller_hold_and_escape_close_setup_without_upload() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        for event in [HotkeyEvent::ChordReleased, HotkeyEvent::Cancel] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let t0 = Instant::now();
            let mut controller = Controller { phase: Phase::Idle };
            assert_eq!(
                controller.on_event(HotkeyEvent::ChordPressed, false, t0),
                Action::Start
            );
            let cancelled = Arc::new(AtomicBool::new(false));
            let (session, mut feed) = crate::streaming::Session::test_session(
                format!("ws://{}", listener.local_addr().unwrap()),
                cancelled.clone(),
            );
            feed.push(&vec![0.25; 1600], 16000);
            let (tcp, _) = tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            socket.next().await.unwrap().unwrap(); // setup, never audio
            socket
                .send(Message::Text(r#"{"type":"session.updated"}"#.into()))
                .await
                .unwrap();
            assert_eq!(
                controller.on_event(event, true, t0 + Duration::from_millis(100)),
                Action::Cancel
            );
            cancelled.store(true, Ordering::Release);
            // Simulate a delayed recorder.stop: cancellation alone must terminate setup.
            let closed = tokio::time::timeout(Duration::from_millis(200), socket.next())
                .await
                .unwrap();
            assert!(
                closed.is_none() || closed.unwrap().is_err(),
                "cancelled controller uploaded a frame"
            );
            drop(feed);
            assert!(session.finish().is_err());
        }
    }

    #[test]
    fn streaming_success_never_materializes_recovery_and_fallback_uses_complete_clip() {
        struct RecoveryProbe(std::sync::OnceLock<crate::clip::Clip>);
        impl RecoverySource for RecoveryProbe {
            fn batch_clip(&self) -> &crate::clip::Clip {
                self.0
                    .get_or_init(|| crate::clip::Clip::new(vec![42; 16000]))
            }
        }
        struct FullClipProvider;
        impl SttProvider for FullClipProvider {
            fn id(&self) -> &'static str {
                "fake"
            }
            fn display_name(&self) -> &'static str {
                "Fake"
            }
            fn availability(&self, _: &Settings) -> Availability {
                Availability::Available
            }
            fn transcribe(
                &self,
                clip: &crate::clip::Clip,
                _: &Settings,
            ) -> Result<String, ProviderError> {
                assert_eq!(clip.samples, vec![42; 16000]);
                Ok("Recovered".into())
            }
        }
        let probe = RecoveryProbe(std::sync::OnceLock::new());
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        transcribe_and_paste(
            &FullClipProvider,
            &probe,
            &Settings::default(),
            Instant::now(),
            &mut record,
            Some(Ok(crate::streaming::Streamed {
                text: "Final".into(),
                model: "live".into(),
                setup_ms: Some(20),
                first_caption_ms: Some(400),
            })),
            |_, _| Ok(Instant::now()),
        )
        .unwrap();
        assert!(probe.0.get().is_none());
        assert_eq!(record.first_caption_ms, Some(400));
        transcribe_and_paste(
            &FullClipProvider,
            &probe,
            &Settings::default(),
            Instant::now(),
            &mut record,
            Some(Err(ProviderError::Timeout)),
            |text, _| {
                assert_eq!(text, "Recovered");
                Ok(Instant::now())
            },
        )
        .unwrap();
        assert!(probe.0.get().is_some());
    }

    #[test]
    fn streaming_final_is_pasted_once_without_running_batch() {
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        let mut pastes = Vec::new();
        transcribe_and_paste(
            &FakeProvider(Err(ProviderError::Other("batch must not run".into()))),
            &crate::clip::Clip::new(vec![1]),
            &Settings::default(),
            Instant::now(),
            &mut record,
            Some(Ok(crate::streaming::Streamed {
                text: "Ciao, hello.".into(),
                model: "live-model".into(),
                setup_ms: None,
                first_caption_ms: None,
            })),
            |text, _| {
                pastes.push(text.to_owned());
                Ok(Instant::now())
            },
        )
        .unwrap();
        assert_eq!(pastes, ["Ciao, hello."]);
        assert_eq!(record.transcription_mode.as_deref(), Some("streaming"));
        assert_eq!(record.model.as_deref(), Some("live-model"));
    }

    #[test]
    fn failed_stream_uses_full_clip_batch_and_never_pastes_partial_text() {
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        let mut pastes = Vec::new();
        transcribe_and_paste(
            &FakeProvider(Ok("Complete recovered transcript".into())),
            &crate::clip::Clip::new(vec![1; 16000]),
            &Settings::default(),
            Instant::now(),
            &mut record,
            Some(Err(ProviderError::Timeout)),
            |text, _| {
                pastes.push(text.to_owned());
                Ok(Instant::now())
            },
        )
        .unwrap();
        assert_eq!(pastes, ["Complete recovered transcript"]);
        assert_eq!(record.transcription_mode.as_deref(), Some("batch_fallback"));
        assert!(record.stream_error.is_some());
    }
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
            None,
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
    fn lost_release_times_out() {
        let (_sender, receiver) = mpsc::channel();
        let (_, error) = wait_for_release(&receiver, Duration::from_millis(1));
        assert!(error.unwrap().starts_with("hold exceeded"));
    }
    #[test]
    fn chord_hold_records_until_release_and_short_holds_are_cancelled() {
        let t0 = Instant::now();
        let mut c = Controller { phase: Phase::Idle };
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed, false, t0),
            Action::Start
        );
        let late = t0 + Duration::from_secs(2);
        assert_eq!(
            c.on_event(HotkeyEvent::ChordReleased, true, late),
            Action::Stop
        );
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed, false, late),
            Action::Start
        );
        let quick = late + Duration::from_millis(100);
        assert_eq!(
            c.on_event(HotkeyEvent::ChordReleased, true, quick),
            Action::Cancel
        );
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed, false, quick),
            Action::Start
        );
        assert_eq!(
            c.on_event(HotkeyEvent::ChordCancelled, true, quick),
            Action::Cancel
        );
    }
    #[test]
    fn main_key_latches_hands_free_and_next_press_stops() {
        let t0 = Instant::now();
        let mut c = Controller { phase: Phase::Idle };
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed, false, t0),
            Action::Start
        );
        assert_eq!(c.on_event(HotkeyEvent::Pressed, true, t0), Action::None);
        let later = t0 + Duration::from_secs(5);
        // Releasing the chord does not stop a latched recording; pressing
        // just Ctrl+Alt again does, and the rest of that gesture is ignored.
        assert_eq!(
            c.on_event(HotkeyEvent::ChordReleased, true, later),
            Action::None
        );
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed, true, later),
            Action::Stop
        );
        assert_eq!(c.on_event(HotkeyEvent::Pressed, true, later), Action::None);
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed, true, later),
            Action::None
        );
    }
    #[test]
    fn escape_cancels_a_hold_or_a_locked_recording_but_is_ignored_when_idle() {
        let t0 = Instant::now();
        let mut c = Controller { phase: Phase::Idle };
        assert_eq!(c.on_event(HotkeyEvent::Cancel, false, t0), Action::None);
        c.on_event(HotkeyEvent::ChordPressed, false, t0);
        assert_eq!(c.on_event(HotkeyEvent::Cancel, true, t0), Action::Cancel);
        c.on_event(HotkeyEvent::Pressed, false, t0);
        assert_eq!(c.on_event(HotkeyEvent::Cancel, true, t0), Action::Cancel);
        assert_eq!(c.phase, Phase::Idle);
    }
    #[test]
    fn a_session_that_ended_on_its_own_does_not_wedge_the_controller() {
        let t0 = Instant::now();
        let mut c = Controller {
            phase: Phase::Latched,
        };
        assert_eq!(c.on_event(HotkeyEvent::Pressed, false, t0), Action::Start);
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
                None,
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
            None,
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
            None,
            |_, _| Err("clipboard unavailable".into())
        )
        .is_err());
        assert_eq!(record.text, "Hello");
        assert!(record.pasted.is_none());
    }
}
