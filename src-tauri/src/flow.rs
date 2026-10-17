//! Hotkey callbacks enqueue only. A responsive coordinator drops overlapping
//! holds while one session worker owns recording, transcription and paste.
pub use crate::failure::on_failure;
use crate::{
    hotkey::{Chord, Hotkey, HotkeyEvent},
    inserter,
    overlay::PillMode,
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
/// A hold that outlasts this ends as if released, with an error.
const HOLD_CAP: Duration = Duration::from_secs(120);

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
    /// Recording while a chord is held: push to talk for dictation, or a
    /// command run's instruction.
    Hold(Chord, Instant),
    /// Hands-free: started or converted by the main key, ended by the next press.
    Latched,
}

impl Phase {
    /// None when idle: a finished command run stays orange while transcribing.
    fn pill_mode(self) -> Option<PillMode> {
        match self {
            Phase::Idle => None,
            Phase::Hold(Chord::Dictation, _) => Some(PillMode::Hold),
            Phase::Hold(Chord::Command, _) => Some(PillMode::Command),
            Phase::Latched => Some(PillMode::Locked),
        }
    }
}

/// Decides what a hotkey event does. Ctrl+Alt held records until released;
/// pressing the main key during the chord (Ctrl+Alt+Space) latches the
/// recording hands-free, and the next press of the chord (or the full
/// shortcut) stops it. The command chord (PARLA-61) is hold only: no latch.
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
            (HotkeyEvent::ChordPressed(chord), Phase::Idle) if !busy => {
                self.phase = Phase::Hold(chord, now);
                Action::Start
            }
            (HotkeyEvent::Pressed, Phase::Idle) if !busy => {
                self.phase = Phase::Latched;
                Action::Start
            }
            (HotkeyEvent::Pressed, Phase::Hold(Chord::Dictation, _)) => {
                self.phase = Phase::Latched;
                Action::None
            }
            (
                HotkeyEvent::Pressed | HotkeyEvent::ChordPressed(Chord::Dictation),
                Phase::Latched,
            ) => {
                self.phase = Phase::Idle;
                Action::Stop
            }
            (HotkeyEvent::ChordReleased(released), Phase::Hold(chord, since))
                if released == chord =>
            {
                self.phase = Phase::Idle;
                if now.duration_since(since) < MIN_HOLD {
                    Action::Cancel
                } else {
                    Action::Stop
                }
            }
            (HotkeyEvent::ChordCancelled(cancelled), Phase::Hold(chord, _))
                if cancelled == chord =>
            {
                self.phase = Phase::Idle;
                Action::Cancel
            }
            (HotkeyEvent::Cancel, Phase::Hold(..) | Phase::Latched) => {
                self.phase = Phase::Idle;
                Action::Cancel
            }
            _ => Action::None,
        }
    }
}
/// Shared admission gate for hotkey sessions and PARLA-9 retries. Keep the
/// returned guard alive through finish_session. The clipboard restore runs
/// after it, off the guard, so the next press is admitted once the text lands.
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
                    if let Some(mode) = controller.phase.pill_mode() {
                        crate::overlay::set_mode(&app, mode);
                    }
                }
                match action {
                    Action::None => {}
                    Action::Start => {
                        let Some(guard) = busy.try_begin() else {
                            controller.phase = Phase::Idle;
                            continue;
                        };
                        let started_at = SystemTime::now();
                        let command = controller.phase.pill_mode() == Some(PillMode::Command);
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
                                run_session(
                                    &session_app,
                                    settings,
                                    started_at,
                                    released,
                                    flag,
                                    command,
                                );
                            }) {
                            Ok(_) => {
                                // A command hold arms Esc once it is confirmed.
                                if !command {
                                    crate::hotkey::RECORDING.store(true, Ordering::Release);
                                }
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

/// One hotkey session: a dictation or, with `command`, a command run's
/// instruction (recorded, never pasted).
fn run_session(
    app: &tauri::AppHandle,
    settings: Settings,
    started_at: SystemTime,
    released: mpsc::Receiver<Instant>,
    cancelled: Arc<AtomicBool>,
    command: bool,
) {
    let recording = crate::recorder::recorder()
        .ok_or_else(|| ProviderError::Other("No microphone available".into()))
        .and_then(|recorder| {
            recorder
                .start()
                .map_err(|e| ProviderError::Other(format!("Recording failed: {e}")))?;
            Ok(recorder)
        });
    // A dictation is confirmed at key-down, a command hold at MIN_HOLD. Until
    // then a quick Ctrl+Shift shortcut costs no pill, dimming, live session or
    // warm-up, and its Esc reaches the app (Ctrl+Shift+Esc). The recorder
    // already runs, and the live feed replays the clip from key-down.
    let confirm_after = if command { MIN_HOLD } else { Duration::ZERO };
    let early_release = match command.then(|| released.recv_timeout(confirm_after)) {
        None | Some(Err(mpsc::RecvTimeoutError::Timeout)) => None,
        Some(Ok(key_up)) => Some((key_up, None)),
        Some(Err(mpsc::RecvTimeoutError::Disconnected)) => Some((
            Instant::now(),
            Some("Hotkey release channel disconnected".into()),
        )),
    };
    let held = recording.is_ok() && early_release.is_none();
    if command && held {
        // From here Esc cancels and is swallowed, as in dictation.
        crate::hotkey::RECORDING.store(true, Ordering::Release);
    }
    // After the mic started, so it never delays the first syllable.
    let dimmed = held.then(|| crate::dim::dim(&settings));
    let captions = settings.live_captions
        && settings.streaming_enabled
        && matches!(
            settings.active_provider.as_str(),
            "openai" | "xai" | crate::models::ID
        );
    let overlay = held.then(|| crate::overlay::begin(app, captions, confirm_after));
    let stream = recording
        .as_ref()
        .ok()
        .filter(|_| held)
        .and_then(|recorder| {
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
    if held && stream.is_none() && settings.active_provider == crate::models::ID {
        // Batch-only local dictation still gets the key-down warm-up.
        crate::provider::parakeet::wake_in_background(&settings);
    }
    let (key_up, hold_error) =
        early_release.unwrap_or_else(|| wait_for_release(&released, HOLD_CAP - confirm_after));
    let stream = if cancelled.load(Ordering::Acquire) {
        drop(stream);
        None
    } else {
        stream
    };
    let audio = recording.map(|recorder| recorder.stop_raw());
    crate::hotkey::RECORDING.store(false, Ordering::Release);
    // On stop or cancel, not after the paste.
    drop(dimmed);
    if cancelled.load(Ordering::Acquire) {
        drop(stream);
        return;
    }
    if let Some(overlay) = &overlay {
        overlay.processing();
    }
    if command {
        // A silent instruction drops the pill without a check or a cross.
        let ok = finish_command(
            app, audio, &settings, started_at, key_up, hold_error, stream,
        );
        if let (Some(overlay), Some(ok)) = (overlay, ok) {
            overlay.finish(ok);
        }
        return;
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
            auto_send: settings.auto_send,
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
            // A retry never sends: focus may have moved since (PARLA-58).
            auto_send: false,
            stream: None,
        },
    );
}

struct Completion {
    hold_error: Option<String>,
    paste: bool,
    auto_send: bool,
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
                active_provider(settings).and_then(|provider| {
                    transcribe_and_paste(
                        provider.as_ref(),
                        &clip,
                        settings,
                        key_up,
                        &mut record,
                        completion.stream.take().map(|session| session.finish()),
                        |text, delay| {
                            if completion.paste {
                                // History keeps the raw text; only the paste drops the trigger.
                                let stripped = completion
                                    .auto_send
                                    .then(|| inserter::strip_send_trigger(text))
                                    .flatten();
                                inserter::paste_text_with_options(
                                    stripped.unwrap_or(text),
                                    delay,
                                    settings.paste_compatibility_mode,
                                    stripped.is_some(),
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
    record.error = combined_error(completion.hold_error, result.err());
    // The one completion sink for successful and failed dictations.
    timing::record(app, record);
    ok
}

fn active_provider(settings: &Settings) -> Result<Arc<dyn SttProvider>, ProviderError> {
    registry::get(&settings.active_provider).ok_or_else(|| {
        ProviderError::MissingConfig(format!("Unknown provider: {}", settings.active_provider))
    })
}

fn combined_error(hold: Option<String>, error: Option<ProviderError>) -> Option<String> {
    match (hold, error) {
        (Some(hold), Some(error)) => Some(format!("{hold}; {error}")),
        (Some(hold), None) => Some(hold),
        (None, Some(error)) => Some(error.to_string()),
        (None, None) => None,
    }
}

/// Command mode 1/7 (PARLA-62): the instruction is transcribed and written to
/// history; the LLM and the paste come with PARLA-63. A failure shows the
/// usual notification but keeps no clip for Retry, which would paste the
/// instruction as a dictation. None for an empty or silent instruction, which
/// ends the run with no row.
fn finish_command(
    app: &tauri::AppHandle,
    audio: Result<crate::recorder::RecoveryAudio, ProviderError>,
    settings: &Settings,
    started_at: SystemTime,
    key_up: Instant,
    hold_error: Option<String>,
    stream: Option<crate::streaming::Session>,
) -> Option<bool> {
    let mut record = DictationRecord::new(started_at, &settings.active_provider);
    record.kind = timing::COMMAND_KIND.into();
    let result = match audio {
        Err(error) => Err(error),
        Ok(clip) if clip.is_empty() => return None,
        Ok(clip) => {
            record.audio_ready = Some(timing::elapsed_ms(key_up));
            record.audio_ms = clip.duration().as_millis() as u64;
            match active_provider(settings) {
                Ok(provider) => transcribe_instruction(
                    provider.as_ref(),
                    &clip,
                    settings,
                    key_up,
                    &mut record,
                    stream.map(|session| session.finish()),
                )?,
                Err(error) => Err(error),
            }
        }
    };
    match &result {
        Ok(()) => crate::failure::on_success(app),
        Err(error) => crate::failure::notify_error(app, &settings.active_provider, error),
    }
    let ok = result.is_ok() && hold_error.is_none();
    record.error = combined_error(hold_error, result.err());
    timing::record(app, record);
    Some(ok)
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

/// The model a batch request to `provider_id` uses with these settings.
pub fn batch_model(provider_id: &str, settings: &Settings) -> Option<String> {
    match provider_id {
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
        "parakeet" => Some(
            crate::models::spec(settings.local_model)
                .history_name
                .into(),
        ),
        _ => None,
    }
}

/// The model a live session with `provider_id` uses (see streaming::transport).
fn streaming_model(provider_id: &str, settings: &Settings) -> Option<String> {
    match provider_id {
        "openai" => Some(
            settings
                .provider_config::<crate::provider::openai::OpenAiConfig>("openai")
                .streaming_model,
        ),
        _ => batch_model(provider_id, settings),
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
    transcribe(provider, clip, settings, key_up, record, streamed)?;
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

/// A command run's instruction, transcribed by the active provider into its
/// row; nothing is pasted (PARLA-62). None means an empty or silent
/// instruction: the run ends with no row.
fn transcribe_instruction(
    provider: &dyn SttProvider,
    clip: &impl RecoverySource,
    settings: &Settings,
    key_up: Instant,
    record: &mut DictationRecord,
    streamed: Option<Result<crate::streaming::Streamed, ProviderError>>,
) -> Option<Result<(), ProviderError>> {
    match transcribe(provider, clip, settings, key_up, record, streamed) {
        Ok(()) if record.text.trim().is_empty() => None,
        outcome => Some(outcome),
    }
}

/// Fills `record` with the transcript and its timings, live final first.
fn transcribe(
    provider: &dyn SttProvider,
    clip: &impl RecoverySource,
    settings: &Settings,
    key_up: Instant,
    record: &mut DictationRecord,
    streamed: Option<Result<crate::streaming::Streamed, ProviderError>>,
) -> Result<(), ProviderError> {
    if let crate::provider::Availability::Unavailable(reason) = provider.availability(settings) {
        return Err(ProviderError::MissingConfig(reason));
    }
    record.model = batch_model(provider.id(), settings);
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
            if attempt.is_some() {
                record.stream_model = streaming_model(provider.id(), settings);
            }
            record.stream_error = attempt.map(|outcome| match outcome {
                Err(error) => error.to_string(),
                Ok(_) => "Live transcription returned an empty final transcript".into(),
            });
            provider.transcribe(clip.batch_clip(), settings)
        }
    };
    record.provider_returned = Some(timing::elapsed_ms(key_up));
    record.text = transcript?;
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
        for event in [
            HotkeyEvent::ChordReleased(Chord::Dictation),
            HotkeyEvent::Cancel,
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let t0 = Instant::now();
            let mut controller = Controller { phase: Phase::Idle };
            assert_eq!(
                controller.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), false, t0),
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
    fn an_instruction_is_transcribed_by_the_active_provider_and_never_pasted() {
        let key_up = Instant::now();
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        let outcome = transcribe_instruction(
            &FakeProvider(Ok("Rendilo più formale".into())),
            &crate::clip::Clip::new(vec![1; 160]),
            &Settings::default(),
            key_up,
            &mut record,
            None,
        );
        assert_eq!(outcome, Some(Ok(())));
        assert_eq!(record.text, "Rendilo più formale");
        assert!(record.provider_returned.is_some());
        assert!(record.pasted.is_none());
        // A live final is the instruction too, without a batch request.
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        let outcome = transcribe_instruction(
            &FakeProvider(Err(ProviderError::Other("batch must not run".into()))),
            &crate::clip::Clip::new(vec![1; 160]),
            &Settings::default(),
            key_up,
            &mut record,
            Some(Ok(crate::streaming::Streamed {
                text: "Translate to English".into(),
                model: "live".into(),
                setup_ms: None,
                first_caption_ms: None,
            })),
        );
        assert_eq!(outcome, Some(Ok(())));
        assert_eq!(record.text, "Translate to English");
    }
    #[test]
    fn an_empty_or_silent_instruction_ends_without_a_row() {
        for transcript in ["", "  \n "] {
            let mut record = DictationRecord::new(SystemTime::now(), "fake");
            let outcome = transcribe_instruction(
                &FakeProvider(Ok(transcript.into())),
                &crate::clip::Clip::new(vec![1; 160]),
                &Settings::default(),
                Instant::now(),
                &mut record,
                None,
            );
            assert_eq!(outcome, None, "{transcript:?}");
        }
    }
    #[test]
    fn a_failed_instruction_is_reported_as_a_failure() {
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        let outcome = transcribe_instruction(
            &FakeProvider(Err(ProviderError::Timeout)),
            &crate::clip::Clip::new(vec![1; 160]),
            &Settings::default(),
            Instant::now(),
            &mut record,
            None,
        );
        assert_eq!(outcome, Some(Err(ProviderError::Timeout)));
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
            c.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), false, t0),
            Action::Start
        );
        let late = t0 + Duration::from_secs(2);
        assert_eq!(
            c.on_event(HotkeyEvent::ChordReleased(Chord::Dictation), true, late),
            Action::Stop
        );
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), false, late),
            Action::Start
        );
        let quick = late + Duration::from_millis(100);
        assert_eq!(
            c.on_event(HotkeyEvent::ChordReleased(Chord::Dictation), true, quick),
            Action::Cancel
        );
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), false, quick),
            Action::Start
        );
        assert_eq!(
            c.on_event(HotkeyEvent::ChordCancelled(Chord::Dictation), true, quick),
            Action::Cancel
        );
    }
    #[test]
    fn command_chord_is_hold_only_and_short_or_interrupted_holds_cancel() {
        const PRESSED: HotkeyEvent = HotkeyEvent::ChordPressed(Chord::Command);
        const RELEASED: HotkeyEvent = HotkeyEvent::ChordReleased(Chord::Command);
        let t0 = Instant::now();
        let late = t0 + Duration::from_secs(2);
        let mut c = Controller { phase: Phase::Idle };
        assert_eq!(c.on_event(PRESSED, false, t0), Action::Start);
        assert_eq!(c.phase, Phase::Hold(Chord::Command, t0));
        assert_eq!(c.on_event(RELEASED, true, late), Action::Stop);
        for interrupt in [
            (RELEASED, t0 + Duration::from_millis(299)),
            (HotkeyEvent::ChordCancelled(Chord::Command), late),
            (HotkeyEvent::Cancel, late),
        ] {
            assert_eq!(c.on_event(PRESSED, false, t0), Action::Start);
            assert_eq!(c.on_event(interrupt.0, true, interrupt.1), Action::Cancel);
            assert_eq!(c.phase, Phase::Idle);
        }
        // No latch: the dictation main key does not convert a command hold.
        assert_eq!(c.on_event(PRESSED, false, t0), Action::Start);
        assert_eq!(c.on_event(HotkeyEvent::Pressed, true, t0), Action::None);
        assert_eq!(c.phase, Phase::Hold(Chord::Command, t0));
        assert_eq!(c.on_event(RELEASED, true, late), Action::Stop);
    }
    #[test]
    fn hotkey_presses_during_a_command_run_or_a_dictation_are_ignored() {
        let t0 = Instant::now();
        let late = t0 + Duration::from_secs(2);
        let dictation = HotkeyEvent::ChordPressed(Chord::Dictation);
        let command = HotkeyEvent::ChordPressed(Chord::Command);
        let mut c = Controller { phase: Phase::Idle };
        c.on_event(command, false, t0);
        for event in [dictation, HotkeyEvent::Pressed, command] {
            assert_eq!(c.on_event(event, true, t0), Action::None);
        }
        assert_eq!(
            c.on_event(HotkeyEvent::ChordReleased(Chord::Dictation), true, late),
            Action::None
        );
        assert_eq!(c.phase, Phase::Hold(Chord::Command, t0));
        // Released and still transcribing: the session guard is busy.
        c.on_event(HotkeyEvent::ChordReleased(Chord::Command), true, late);
        assert_eq!(c.on_event(dictation, true, late), Action::None);
        assert_eq!(c.on_event(command, true, late), Action::None);
        // A dictation hold or lock is neither stopped nor taken over.
        for phase in [Phase::Hold(Chord::Dictation, t0), Phase::Latched] {
            let mut c = Controller { phase };
            assert_eq!(c.on_event(command, true, late), Action::None);
            assert_eq!(
                c.on_event(HotkeyEvent::ChordReleased(Chord::Command), true, late),
                Action::None
            );
            assert_eq!(c.phase, phase);
        }
    }
    #[test]
    fn main_key_latches_hands_free_and_next_press_stops() {
        let t0 = Instant::now();
        let mut c = Controller { phase: Phase::Idle };
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), false, t0),
            Action::Start
        );
        assert_eq!(c.on_event(HotkeyEvent::Pressed, true, t0), Action::None);
        let later = t0 + Duration::from_secs(5);
        // Releasing the chord does not stop a latched recording; pressing
        // just Ctrl+Alt again does, and the rest of that gesture is ignored.
        assert_eq!(
            c.on_event(HotkeyEvent::ChordReleased(Chord::Dictation), true, later),
            Action::None
        );
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), true, later),
            Action::Stop
        );
        assert_eq!(c.on_event(HotkeyEvent::Pressed, true, later), Action::None);
        assert_eq!(
            c.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), true, later),
            Action::None
        );
    }
    #[test]
    fn escape_cancels_a_hold_or_a_locked_recording_but_is_ignored_when_idle() {
        let t0 = Instant::now();
        let mut c = Controller { phase: Phase::Idle };
        assert_eq!(c.on_event(HotkeyEvent::Cancel, false, t0), Action::None);
        c.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), false, t0);
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
        let (observed, error) = wait_for_release(&receiver, HOLD_CAP);
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
