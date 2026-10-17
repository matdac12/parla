//! Hotkey callbacks enqueue only. A responsive coordinator drops overlapping
//! holds while one session worker owns recording, transcription and paste.
pub use crate::failure::on_failure;
use crate::{
    command_mode,
    hotkey::{Chord, Hotkey, HotkeyEvent},
    inserter,
    overlay::{Ending, PillMode},
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
/// A hold that outlasts this ends as if released. The run still pastes; a
/// failed one notes the cap in its error.
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
    /// A released command run, transcribing or waiting on the LLM: Esc still
    /// cancels it (PARLA-63).
    Running,
}

impl Phase {
    /// None when idle: a finished command run stays orange while transcribing.
    fn pill_mode(self) -> Option<PillMode> {
        match self {
            Phase::Idle | Phase::Running => None,
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
                if now.duration_since(since) < MIN_HOLD {
                    self.phase = Phase::Idle;
                    Action::Cancel
                } else {
                    self.phase = match chord {
                        Chord::Command => Phase::Running,
                        Chord::Dictation => Phase::Idle,
                    };
                    Action::Stop
                }
            }
            (HotkeyEvent::ChordCancelled(cancelled), Phase::Hold(chord, _))
                if cancelled == chord =>
            {
                self.phase = Phase::Idle;
                Action::Cancel
            }
            (HotkeyEvent::Cancel, Phase::Hold(..) | Phase::Latched | Phase::Running) => {
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
                let now = Instant::now();
                let action = controller.on_event(event, busy.is_busy(), now);
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
                        // The controller's own key-down time, so the session
                        // confirms a command hold on the same clock.
                        let command = matches!(controller.phase, Phase::Hold(Chord::Command, _))
                            .then_some(now);
                        let settings = app
                            .state::<SharedSettings>()
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        let (sender, released) = mpsc::channel();
                        let cancelled = Arc::new(AtomicBool::new(false));
                        let flag = cancelled.clone();
                        let session_app = app.clone();
                        let name = if command.is_some() {
                            "parla-command"
                        } else {
                            "parla-dictation"
                        };
                        match std::thread::Builder::new()
                            .name(name.into())
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
                                if command.is_none() {
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
                        // A released command run keeps its handle: Esc may
                        // still cancel it until the reply is in.
                        let current = if controller.phase == Phase::Running {
                            session.clone()
                        } else {
                            session.take()
                        };
                        if let Some((sender, cancelled)) = current {
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
    command_pressed_at: Option<Instant>,
) {
    let command = command_pressed_at.is_some();
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
    let early_release = command_pressed_at.and_then(|pressed_at| {
        match released.recv_timeout(MIN_HOLD.saturating_sub(pressed_at.elapsed())) {
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Ok(key_up) => Some((key_up, None)),
            Err(mpsc::RecvTimeoutError::Disconnected) => Some((
                Instant::now(),
                Some("Hotkey release channel disconnected".into()),
            )),
        }
    });
    // The controller decides what a release means: one it kept (just past
    // MIN_HOLD, or this thread started late) ends a confirmed hold.
    let held = recording.is_ok() && (early_release.is_none() || !cancelled.load(Ordering::Acquire));
    if command && held {
        // From here Esc cancels and is swallowed, as in dictation.
        crate::hotkey::RECORDING.store(true, Ordering::Release);
    }
    // While the user is still speaking, never for a dictation or a quick
    // Ctrl+Shift shortcut: read the selection (PARLA-64) and open the
    // connection the LLM request will use.
    let desktop = CommandDesktop::new(&settings);
    let selection = (command && held).then(|| {
        crate::provider::openai::prewarm(&settings);
        SelectionRead::start(desktop)
    });
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
    // A command run keeps Esc armed until it ends (PARLA-63).
    let _disarm_at_end = command.then_some(DisarmEsc);
    if !command {
        crate::hotkey::RECORDING.store(false, Ordering::Release);
    }
    // On stop or cancel, not after the paste.
    drop(dimmed);
    if cancelled.load(Ordering::Acquire) {
        drop(stream);
        if let Some(selection) = selection {
            selection.join().give_back(&desktop);
        }
        return;
    }
    if let Some(overlay) = &overlay {
        overlay.processing();
    }
    if command {
        let run = command_mode::Run {
            settings: &settings.command,
            key_up,
            date: command_mode::today(),
            cancelled: &cancelled,
            hold_error,
        };
        let outcome = finish_command(app, audio, &settings, started_at, run, stream, selection);
        // Dropping the pill unfinished hides it: a silent or cancelled run.
        if let (Some(overlay), Some(ok)) = (overlay, outcome.pill()) {
            match outcome.label() {
                Some(label) => overlay.finish_labelled(&label),
                None if ok => overlay.finish(Ending::Done),
                // A command run keeps no clip: Retry would re-run a dictation's.
                None => overlay.finish(Ending::Failed { retry: false }),
            }
        }
        return;
    }
    let ending = finish_session_with_error(
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
        overlay.finish(ending);
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
) -> Ending {
    let mut record = DictationRecord::new(started_at, &settings.active_provider);
    let mut clip_saved = false;
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
                clip_saved = true;
            }
            result
        }
    };
    if result.is_ok() {
        crate::failure::on_success(app);
    }
    let (ending, error) = dictation_ending(completion.hold_error, result, clip_saved);
    record.error = error;
    // The one completion sink for successful and failed dictations.
    timing::record(app, record);
    ending
}

/// The pill's ending and the history row's error. A paste that landed is a
/// success even when the hold hit the cap; Retry is offered only when this
/// run saved its clip, or it would re-run an older one (PARLA-51).
fn dictation_ending(
    hold_error: Option<String>,
    result: Result<(), ProviderError>,
    clip_saved: bool,
) -> (Ending, Option<String>) {
    match result {
        Ok(()) => {
            if let Some(hold) = hold_error {
                eprintln!("Dictation pasted after the hold ended early: {hold}");
            }
            (Ending::Done, None)
        }
        Err(error) => (
            Ending::Failed { retry: clip_saved },
            Some(match hold_error {
                Some(hold) => format!("{hold}; {error}"),
                None => error.to_string(),
            }),
        ),
    }
}

fn active_provider(settings: &Settings) -> Result<Arc<dyn SttProvider>, ProviderError> {
    registry::get(&settings.active_provider).ok_or_else(|| {
        ProviderError::MissingConfig(format!("Unknown provider: {}", settings.active_provider))
    })
}

/// A command run after key-up (PARLA-63): the orchestrator with the real
/// ports. A failure shows the usual notification but keeps no clip for Retry,
/// which would paste the instruction as a dictation.
fn finish_command(
    app: &tauri::AppHandle,
    audio: Result<crate::recorder::RecoveryAudio, ProviderError>,
    settings: &Settings,
    started_at: SystemTime,
    run: command_mode::Run,
    stream: Option<crate::streaming::Session>,
    selection: Option<SelectionRead>,
) -> command_mode::Outcome {
    let (key_up, cancelled) = (run.key_up, run.cancelled);
    let mut record = DictationRecord::new(started_at, &settings.active_provider);
    record.kind = timing::COMMAND_KIND.into();
    let ports = command_mode::Ports {
        desktop: &CommandDesktop::new(settings),
        llm: &command_mode::responses::OpenAi(settings),
        browser: &command_mode::tools::DefaultBrowser,
        history: &AppHistory(app),
    };
    // Joined once the instruction is in: the read finished long before.
    let selection = || selection.map(SelectionRead::join).unwrap_or_default();
    let outcome = command_mode::run(&ports, run, record, selection, |record| {
        let clip = match audio {
            Err(error) => return Some(Err(error)),
            Ok(clip) if clip.is_empty() => return None,
            Ok(clip) => clip,
        };
        record.audio_ready = Some(timing::elapsed_ms(key_up));
        record.audio_ms = clip.duration().as_millis() as u64;
        let streamed = stream.map(|session| session.finish());
        // Esc aborted the live session: no batch fallback, the run is cancelled.
        if cancelled.load(Ordering::Acquire) {
            return Some(Err(ProviderError::Other("Cancelled".into())));
        }
        match active_provider(settings) {
            Ok(provider) => {
                transcribe_instruction(provider.as_ref(), &clip, settings, key_up, record, streamed)
            }
            Err(error) => Some(Err(error)),
        }
    });
    match &outcome {
        command_mode::Outcome::Pasted | command_mode::Outcome::Opened { .. } => {
            crate::failure::on_success(app)
        }
        command_mode::Outcome::Failed(command_mode::Failure::Transcription(error)) => {
            crate::failure::notify_error(app, &settings.active_provider, error)
        }
        command_mode::Outcome::Failed(command_mode::Failure::Command(error)) => {
            crate::failure::notify_command_error(app, error)
        }
        command_mode::Outcome::Silent | command_mode::Outcome::Cancelled => {}
    }
    outcome
}

/// Disarms Esc when a command run ends, however it ends.
struct DisarmEsc;
impl Drop for DisarmEsc {
    fn drop(&mut self) {
        crate::hotkey::RECORDING.store(false, Ordering::Release);
    }
}

/// The command run's desktop port: the inserter's selection read, paste and
/// clipboard restore.
#[derive(Clone, Copy)]
struct CommandDesktop {
    restore_delay: Duration,
    compatibility_mode: bool,
}
impl CommandDesktop {
    fn new(settings: &Settings) -> Self {
        Self {
            restore_delay: Duration::from_millis(settings.restore_delay_ms),
            compatibility_mode: settings.paste_compatibility_mode,
        }
    }
}
impl command_mode::Desktop for CommandDesktop {
    fn clipboard_text(&self) -> Result<Option<String>, String> {
        inserter::clipboard_text()
    }
    fn clipboard_sequence(&self) -> u32 {
        inserter::clipboard_sequence()
    }
    fn send_copy(&self) -> Result<(), String> {
        inserter::send_copy()
    }
    fn copied_empty_selection(&self) -> bool {
        inserter::copied_empty_selection()
    }
    fn foreground_app(&self) -> Option<String> {
        inserter::foreground_app()
    }
    fn wait(&self, delay: Duration) {
        std::thread::sleep(delay);
    }
    fn paste(
        &self,
        text: &str,
        borrowed: Option<&command_mode::Borrowed>,
    ) -> Result<Instant, String> {
        inserter::paste_over_borrowed(text, borrowed, self.restore_delay, self.compatibility_mode)
    }
    fn give_back(&self, borrowed: &command_mode::Borrowed) {
        inserter::give_back(borrowed);
    }
}

/// The selection read on its own thread, from the confirmed hold until the
/// run joins it (PARLA-64).
struct SelectionRead(Option<std::thread::JoinHandle<command_mode::Selection>>);
impl SelectionRead {
    fn start(desktop: CommandDesktop) -> Self {
        let read = std::thread::Builder::new()
            .name("parla-selection".into())
            .spawn(move || command_mode::read_selection(&desktop));
        Self(
            read.map_err(|error| eprintln!("Cannot read the selection: {error}"))
                .ok(),
        )
    }
    fn join(self) -> command_mode::Selection {
        self.0.and_then(|read| read.join().ok()).unwrap_or_default()
    }
}

struct AppHistory<'a>(&'a tauri::AppHandle);
impl command_mode::HistorySink for AppHistory<'_> {
    fn record(&self, record: DictationRecord) {
        timing::record(self.0, record);
    }
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
    record.pasted = Some(timing::offset_ms(key_up, pasted));
    Ok(())
}

/// A command run's instruction, transcribed by the active provider into its
/// row; it is never pasted. None means an empty or silent instruction: the
/// run ends with no row.
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

    #[test]
    fn a_capped_hold_that_pasted_is_a_success_and_retry_needs_this_runs_clip() {
        use crate::overlay::Ending;
        let cap = || Some("hold exceeded 120 s".to_string());
        // The cap ended the hold but the transcript pasted: a plain success,
        // with no error row the app window would offer to retry (PARLA-51).
        assert_eq!(dictation_ending(cap(), Ok(()), false), (Ending::Done, None));
        assert_eq!(dictation_ending(None, Ok(()), false), (Ending::Done, None));
        // A failed transcription saved this run's clip: Retry re-runs it.
        assert_eq!(
            dictation_ending(cap(), Err(ProviderError::Timeout), true),
            (
                Ending::Failed { retry: true },
                Some(format!("hold exceeded 120 s; {}", ProviderError::Timeout))
            )
        );
        // No clip saved (the microphone failed): Retry would re-run an older one.
        let mic = ProviderError::Other("microphone unavailable".into());
        assert_eq!(
            dictation_ending(None, Err(mic.clone()), false),
            (Ending::Failed { retry: false }, Some(mic.to_string()))
        );
    }

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
    fn esc_after_a_command_release_cancels_the_run_until_it_ends() {
        const PRESSED: HotkeyEvent = HotkeyEvent::ChordPressed(Chord::Command);
        const RELEASED: HotkeyEvent = HotkeyEvent::ChordReleased(Chord::Command);
        let t0 = Instant::now();
        let late = t0 + Duration::from_secs(2);
        let mut c = Controller { phase: Phase::Idle };
        c.on_event(PRESSED, false, t0);
        assert_eq!(c.on_event(RELEASED, true, late), Action::Stop);
        // Transcribing or waiting on the LLM: Esc cancels, once.
        assert_eq!(c.on_event(HotkeyEvent::Cancel, true, late), Action::Cancel);
        assert_eq!(c.on_event(HotkeyEvent::Cancel, true, late), Action::None);
        // A run that already ended has nothing to cancel.
        c.on_event(PRESSED, false, t0);
        c.on_event(RELEASED, true, late);
        assert_eq!(c.on_event(HotkeyEvent::Cancel, false, late), Action::None);
        // A released dictation is never cancelled by Esc.
        c.on_event(HotkeyEvent::ChordPressed(Chord::Dictation), false, t0);
        c.on_event(HotkeyEvent::ChordReleased(Chord::Dictation), true, late);
        assert_eq!(c.on_event(HotkeyEvent::Cancel, true, late), Action::None);
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
