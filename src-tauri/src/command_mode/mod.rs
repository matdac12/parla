//! Command mode (PARLA-61): the command run orchestrator and its Responses
//! client. The only code that calls an LLM (ADR-0001).
//!
//! The orchestrator owns a run from the confirmed hold to its outcome: reading
//! the selection (PARLA-64), the empty-instruction rule, the LLM call,
//! cancellation, the paste, giving the user's clipboard back and the history
//! row, or running an allowlisted tool instead of the paste (PARLA-65). It
//! talks to the desktop, the LLM, the browser and history through narrow
//! ports, so it is tested with fakes and flow.rs supplies the real ones.
pub mod responses;
pub mod tools;

pub use crate::inserter::Borrowed;
use crate::{
    provider::ProviderError,
    settings::CommandSettings,
    timing::{self, CommandOutcome, DictationRecord},
};
use responses::{Reply, Request};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// The focused app and the clipboard, through the inserter.
pub trait Desktop {
    /// The clipboard's text; None when it holds no text.
    fn clipboard_text(&self) -> Result<Option<String>, String>;
    /// Moves on whenever any app writes the clipboard.
    fn clipboard_sequence(&self) -> u32;
    /// Ctrl+Insert into the focused app: a copy that, unlike Ctrl+C, never
    /// interrupts a terminal.
    fn send_copy(&self) -> Result<(), String>;
    /// The clipboard holds VS Code's copy of the whole line under an empty
    /// selection.
    fn copied_empty_selection(&self) -> bool;
    /// The focused app's executable name.
    fn foreground_app(&self) -> Option<String>;
    fn wait(&self, delay: Duration);
    /// Pastes `text` once and gives the user's clipboard back once afterwards:
    /// `borrowed`'s saved text while the clipboard still holds the copy.
    /// Returns the instant the paste was injected.
    fn paste(&self, text: &str, borrowed: Option<&Borrowed>) -> Result<Instant, String>;
    /// Writes the saved text back now, unless the clipboard changed since the copy.
    fn give_back(&self, borrowed: &Borrowed);
}

pub trait Llm {
    /// One request, blocking. It may return early once `cancelled` is set.
    fn reply(&self, request: &Request, cancelled: &AtomicBool) -> Result<Reply, ProviderError>;
}

/// The shell's default handler for a web page: the default browser.
pub trait UrlOpener {
    fn open(&self, url: &str) -> Result<(), String>;
}

pub trait HistorySink {
    fn record(&self, record: DictationRecord);
}

pub struct Ports<'a> {
    pub desktop: &'a dyn Desktop,
    pub llm: &'a dyn Llm,
    pub browser: &'a dyn UrlOpener,
    pub history: &'a dyn HistorySink,
}

pub struct Run<'a> {
    pub settings: &'a CommandSettings,
    pub key_up: Instant,
    /// Today, local `YYYY-MM-DD`, for the request.
    pub date: String,
    /// Set by Esc, from key-up until the reply is in.
    pub cancelled: &'a AtomicBool,
    /// The hold hit the cap: the instruction is recorded but not sent.
    pub hold_error: Option<String>,
}

/// How long the focused app gets to answer Ctrl+Insert.
pub const COPY_WAIT: Duration = Duration::from_millis(150);
const COPY_POLL: Duration = Duration::from_millis(5);

/// What was selected when the hold was confirmed, and the user's clipboard
/// while the copy holds it.
#[derive(Debug, Default)]
pub struct Selection {
    /// Sent to the LLM as data; never written to history.
    pub text: Option<String>,
    pub app: Option<String>,
    borrowed: Option<Borrowed>,
    /// No copy within the wait: the saved text and the sequence before the
    /// copy, for a second look once the instruction is in.
    unanswered: Option<(Option<String>, u32)>,
}

impl Selection {
    /// A slow app's copy that landed after the wait, while the user was still
    /// speaking: the selection after all, and the clipboard is borrowed.
    fn late_copy(&mut self, desktop: &dyn Desktop) {
        if let Some((saved, before)) = self.unanswered.take() {
            if desktop.clipboard_sequence() != before {
                self.take_copy(desktop, saved);
            }
        }
    }

    /// A run that pastes nothing gives the user's clipboard back: a cancel,
    /// a failure or a silent instruction.
    pub fn give_back(mut self, desktop: &dyn Desktop) {
        self.late_copy(desktop);
        if let Some(borrowed) = &self.borrowed {
            desktop.give_back(borrowed);
        }
    }

    fn take_copy(&mut self, desktop: &dyn Desktop, saved: Option<String>) {
        let copied = desktop.clipboard_text().unwrap_or_else(|error| {
            eprintln!("Parla selection unreadable: {error}");
            None
        });
        self.text = copied
            .clone()
            .filter(|text| !text.is_empty() && !desktop.copied_empty_selection());
        self.borrowed = Some(Borrowed { saved, copied });
    }
}

/// Reads the selection at the confirmed hold, while the user is still
/// speaking: saves the clipboard, sends Ctrl+Insert and watches the clipboard
/// sequence for up to `COPY_WAIT`. A change with text is the selection.
pub fn read_selection(desktop: &dyn Desktop) -> Selection {
    let mut selection = Selection {
        app: desktop.foreground_app(),
        ..Selection::default()
    };
    // Unsaved, the user's clipboard would be lost to the copy.
    let saved = match desktop.clipboard_text() {
        Ok(saved) => saved,
        Err(error) => {
            eprintln!("Parla selection skipped: clipboard unreadable ({error})");
            return selection;
        }
    };
    let before = desktop.clipboard_sequence();
    // A failed copy leaves the sequence alone: no selection.
    if let Err(error) = desktop.send_copy() {
        eprintln!("Parla selection copy failed: {error}");
    }
    let copied = (0..COPY_WAIT.as_millis() / COPY_POLL.as_millis()).any(|_| {
        desktop.wait(COPY_POLL);
        desktop.clipboard_sequence() != before
    });
    if copied {
        selection.take_copy(desktop, saved);
    } else {
        selection.unanswered = Some((saved, before));
    }
    selection
}

/// Today, local `YYYY-MM-DD`.
#[cfg(windows)]
pub fn today() -> String {
    // SAFETY: SYSTEMTIME is plain data, and GetLocalTime only fills it.
    let mut now = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut now) };
    format!("{:04}-{:02}-{:02}", now.wYear, now.wMonth, now.wDay)
}

/// Off Windows, where Parla does not run, no date.
#[cfg(not(windows))]
pub fn today() -> String {
    String::new()
}

/// What the pill and the notification show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An empty or silent instruction: nothing shown, no row.
    Silent,
    Pasted,
    /// `open_url` opened a page: the pill names its host.
    Opened {
        host: String,
    },
    Cancelled,
    Failed(Failure),
}

impl Outcome {
    /// The pill's check or cross; None drops the pill without one.
    pub fn pill(&self) -> Option<bool> {
        match self {
            Outcome::Silent | Outcome::Cancelled => None,
            Outcome::Pasted | Outcome::Opened { .. } => Some(true),
            Outcome::Failed(_) => Some(false),
        }
    }

    /// What the pill says beside its check.
    pub fn label(&self) -> Option<String> {
        match self {
            Outcome::Opened { host } => Some(format!("Opened {host}")),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The instruction could not be transcribed: the provider's notification.
    Transcription(ProviderError),
    /// The LLM step or the paste: command mode's notification.
    Command(ProviderError),
}

/// One command run after key-up. `transcribe` fills `record` with the
/// instruction and returns None for an empty or silent one; `selection` then
/// yields what the confirmed hold read.
pub fn run(
    ports: &Ports,
    run: Run,
    mut record: DictationRecord,
    selection: impl FnOnce() -> Selection,
    transcribe: impl FnOnce(&mut DictationRecord) -> Option<Result<(), ProviderError>>,
) -> Outcome {
    let cancelled = || run.cancelled.load(Ordering::Acquire);
    let transcribed = transcribe(&mut record);
    let mut selection = selection();
    selection.late_copy(ports.desktop);
    // Whether there was a selection and its length, never its text.
    record.selection_chars = Some(
        selection
            .text
            .as_ref()
            .map_or(0, |text| text.chars().count() as u64),
    );
    record.app = selection.app.clone();
    let outcome = match transcribed {
        None => Outcome::Silent,
        // Esc aborts a live transcription, so its error is the cancel.
        Some(_) if cancelled() => Outcome::Cancelled,
        Some(Err(error)) => {
            record.error = Some(error.to_string());
            Outcome::Failed(Failure::Transcription(error))
        }
        Some(Ok(())) => reply_and_act(ports, &run, &mut record, &selection),
    };
    // A paste gives the clipboard back itself, once.
    if outcome != Outcome::Pasted {
        selection.give_back(ports.desktop);
    }
    if outcome == Outcome::Silent {
        return outcome;
    }
    if let Outcome::Failed(Failure::Command(error)) = &outcome {
        record.error = Some(crate::failure::command_message(error));
    }
    record.outcome = Some(match outcome {
        Outcome::Silent => unreachable!("a silent run returned above"),
        Outcome::Pasted => CommandOutcome::Pasted,
        Outcome::Opened { .. } => CommandOutcome::Opened,
        Outcome::Cancelled => CommandOutcome::Cancelled,
        Outcome::Failed(_) => CommandOutcome::Failed,
    });
    ports.history.record(record);
    outcome
}

/// Asks the LLM, then pastes its text or runs the tool it called.
fn reply_and_act(
    ports: &Ports,
    run: &Run,
    record: &mut DictationRecord,
    selection: &Selection,
) -> Outcome {
    let cancelled = || run.cancelled.load(Ordering::Acquire);
    if let Some(hold) = &run.hold_error {
        return Outcome::Failed(Failure::Command(ProviderError::Other(hold.clone())));
    }
    let request = Request {
        // A blank model field means the default (the card clears to it).
        model: match run.settings.model.trim() {
            "" => CommandSettings::default().model,
            model => model.into(),
        },
        reasoning_effort: run.settings.reasoning_effort.clone(),
        instruction: record.text.clone(),
        selection: selection.text.clone(),
        app: selection.app.clone(),
        date: run.date.clone(),
    };
    record.llm_model = Some(request.model.clone());
    let reply = ports.llm.reply(&request, run.cancelled);
    record.llm_returned = Some(timing::elapsed_ms(run.key_up));
    let reply = match reply {
        // A reply or an error that lands after Esc is discarded.
        _ if cancelled() => return Outcome::Cancelled,
        Ok(reply) => reply,
        Err(error) => return Outcome::Failed(Failure::Command(error)),
    };
    record.tokens_in = reply.tokens_in;
    record.tokens_out = reply.tokens_out;
    if let Some(call) = &reply.call {
        return run_tool(ports, run, record, call);
    }
    if reply.text.trim().is_empty() {
        return Outcome::Failed(Failure::Command(ProviderError::Other(
            "the reply was empty.".into(),
        )));
    }
    // The reply's end comes trimmed; a selection's own trailing line break
    // (a triple-clicked paragraph) stays, or the next line would join it.
    let text = match &selection.text {
        Some(selected) => reply.text + &selected[selected.trim_end().len()..],
        None => reply.text,
    };
    // Over the selection, which is still selected, or at the cursor.
    let pasted = ports.desktop.paste(&text, selection.borrowed.as_ref());
    record.reply = Some(text);
    match pasted {
        Ok(at) => {
            record.pasted = Some(timing::offset_ms(run.key_up, at));
            Outcome::Pasted
        }
        Err(error) => Outcome::Failed(Failure::Command(ProviderError::Other(format!(
            "could not paste the reply ({error})."
        )))),
    }
}

/// A tool call instead of a paste. Nothing goes back to the model.
fn run_tool(
    ports: &Ports,
    run: &Run,
    record: &mut DictationRecord,
    call: &responses::ToolCall,
) -> Outcome {
    let tool = match tools::validate(call) {
        Ok(tool) => tool,
        Err(reason) => return Outcome::Failed(Failure::Command(ProviderError::Other(reason))),
    };
    match tool.run(ports.browser) {
        Ok(()) => {
            record.opened = Some(timing::elapsed_ms(run.key_up));
            record.tool = Some(call.name.clone());
            let tools::Tool::OpenUrl(url) = tool;
            record.url = Some(url.to_string());
            Outcome::Opened {
                host: url.host_str().unwrap_or_default().into(),
            }
        }
        Err(error) => Outcome::Failed(Failure::Command(ProviderError::Other(format!(
            "could not open the page ({error})."
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        time::SystemTime,
    };

    /// A focused app and its clipboard. Ctrl+Insert copies `selected` (None:
    /// the app copies nothing) after `copy_lag` polls.
    struct FakeDesktop {
        clipboard: RefCell<Option<String>>,
        sequence: Cell<u32>,
        selected: Option<String>,
        copy_lag: usize,
        copy_due: Cell<Option<usize>>,
        vscode_line: bool,
        unreadable: bool,
        app: Option<String>,
        copies: Cell<usize>,
        waited: Cell<Duration>,
        /// Every clipboard write by Parla: pastes and restores.
        writes: RefCell<Vec<String>>,
        pasted: RefCell<Vec<String>>,
        fail_paste: bool,
    }
    impl FakeDesktop {
        fn new(clipboard: Option<&str>) -> Self {
            Self {
                clipboard: RefCell::new(clipboard.map(Into::into)),
                sequence: Cell::new(7),
                selected: None,
                copy_lag: 1,
                copy_due: Cell::new(None),
                vscode_line: false,
                unreadable: false,
                app: Some("notepad.exe".into()),
                copies: Cell::new(0),
                waited: Cell::new(Duration::ZERO),
                writes: RefCell::default(),
                pasted: RefCell::default(),
                fail_paste: false,
            }
        }
        fn write(&self, text: &str) {
            *self.clipboard.borrow_mut() = Some(text.into());
            self.writes.borrow_mut().push(text.into());
        }
    }
    impl Desktop for FakeDesktop {
        fn clipboard_text(&self) -> Result<Option<String>, String> {
            if self.unreadable {
                return Err("clipboard occupied".into());
            }
            Ok(self.clipboard.borrow().clone())
        }
        fn clipboard_sequence(&self) -> u32 {
            self.sequence.get()
        }
        fn send_copy(&self) -> Result<(), String> {
            self.copies.set(self.copies.get() + 1);
            if self.selected.is_some() {
                self.copy_due.set(Some(self.copy_lag));
            }
            Ok(())
        }
        fn copied_empty_selection(&self) -> bool {
            self.vscode_line
        }
        fn foreground_app(&self) -> Option<String> {
            self.app.clone()
        }
        fn wait(&self, delay: Duration) {
            self.waited.set(self.waited.get() + delay);
            match self.copy_due.get() {
                Some(1) => {
                    self.copy_due.set(None);
                    *self.clipboard.borrow_mut() = self.selected.clone();
                    self.sequence.set(self.sequence.get() + 1);
                }
                Some(left) => self.copy_due.set(Some(left - 1)),
                None => {}
            }
        }
        /// The inserter's contract: the clipboard is the reply while pasting,
        /// then the user's text once the restore delay passes.
        fn paste(&self, text: &str, borrowed: Option<&Borrowed>) -> Result<Instant, String> {
            if self.fail_paste {
                return Err("clipboard unavailable".into());
            }
            let current = self.clipboard.borrow().clone();
            let owed = match borrowed {
                Some(borrowed) if current == borrowed.copied => borrowed.saved.clone(),
                _ => current,
            };
            self.write(text);
            self.pasted.borrow_mut().push(text.into());
            if let Some(owed) = owed {
                self.write(&owed);
            }
            Ok(Instant::now())
        }
        fn give_back(&self, borrowed: &Borrowed) {
            if *self.clipboard.borrow() == borrowed.copied {
                if let Some(saved) = &borrowed.saved {
                    self.write(saved);
                }
            }
        }
    }

    /// Answers with `reply`; with `late`, Esc is pressed while it waits.
    struct FakeLlm {
        reply: Result<Reply, ProviderError>,
        late: bool,
        requests: RefCell<Vec<Request>>,
    }
    impl FakeLlm {
        fn new(reply: Result<Reply, ProviderError>) -> Self {
            Self {
                reply,
                late: false,
                requests: RefCell::default(),
            }
        }
    }
    impl Llm for FakeLlm {
        fn reply(&self, request: &Request, cancelled: &AtomicBool) -> Result<Reply, ProviderError> {
            self.requests.borrow_mut().push(request.clone());
            if self.late {
                cancelled.store(true, Ordering::Release);
            }
            self.reply.clone()
        }
    }

    /// The default browser: records what it was asked to open.
    #[derive(Default)]
    struct FakeBrowser {
        opened: RefCell<Vec<String>>,
        fail: bool,
    }
    impl UrlOpener for FakeBrowser {
        fn open(&self, url: &str) -> Result<(), String> {
            if self.fail {
                return Err("no default browser".into());
            }
            self.opened.borrow_mut().push(url.into());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeHistory(RefCell<Vec<DictationRecord>>);
    impl HistorySink for FakeHistory {
        fn record(&self, record: DictationRecord) {
            self.0.borrow_mut().push(record);
        }
    }

    fn text(reply: &str) -> Result<Reply, ProviderError> {
        Ok(Reply {
            text: reply.into(),
            call: None,
            tokens_in: Some(92),
            tokens_out: Some(18),
        })
    }

    struct Harness {
        desktop: FakeDesktop,
        llm: FakeLlm,
        browser: FakeBrowser,
        history: FakeHistory,
        cancelled: AtomicBool,
        settings: CommandSettings,
        /// Polls that pass between the selection read and key-up.
        speaking: usize,
    }
    impl Harness {
        fn new(reply: Result<Reply, ProviderError>) -> Self {
            Self {
                desktop: FakeDesktop::new(Some("user copy")),
                llm: FakeLlm::new(reply),
                browser: FakeBrowser::default(),
                history: FakeHistory::default(),
                cancelled: AtomicBool::new(false),
                settings: CommandSettings::default(),
                speaking: 0,
            }
        }
        fn run_with(
            &self,
            hold_error: Option<String>,
            transcript: Option<Result<&str, ProviderError>>,
        ) -> Outcome {
            let ports = Ports {
                desktop: &self.desktop,
                llm: &self.llm,
                browser: &self.browser,
                history: &self.history,
            };
            let input = Run {
                settings: &self.settings,
                key_up: Instant::now(),
                date: "2026-10-17".into(),
                cancelled: &self.cancelled,
                hold_error,
            };
            // Read at the confirmed hold, before the instruction is in.
            let selection = read_selection(&self.desktop);
            for _ in 0..self.speaking {
                self.desktop.wait(COPY_POLL);
            }
            let mut record = DictationRecord::new(SystemTime::now(), "parakeet");
            record.kind = timing::COMMAND_KIND.into();
            run(
                &ports,
                input,
                record,
                || selection,
                |record| {
                    transcript.map(|outcome| {
                        outcome.map(|text| {
                            record.text = text.into();
                            record.provider_returned = Some(timing::elapsed_ms(Instant::now()));
                        })
                    })
                },
            )
        }
        fn run(&self, instruction: &str) -> Outcome {
            self.run_with(None, Some(Ok(instruction)))
        }
        fn rows(&self) -> Vec<DictationRecord> {
            self.history.0.borrow().clone()
        }
        fn pasted(&self) -> Vec<String> {
            self.desktop.pasted.borrow().clone()
        }
    }

    #[test]
    fn the_reply_is_pasted_once_and_the_clipboard_restored() {
        let h = Harness::new(text("Ciao a tutti, buona giornata!"));
        assert_eq!(h.run("Scrivi un saluto breve"), Outcome::Pasted);
        assert_eq!(h.pasted(), ["Ciao a tutti, buona giornata!"]);
        assert_eq!(h.desktop.clipboard.borrow().as_deref(), Some("user copy"));
        assert_eq!(
            *h.llm.requests.borrow(),
            [Request {
                model: "gpt-6-luna".into(),
                reasoning_effort: "low".into(),
                instruction: "Scrivi un saluto breve".into(),
                selection: None,
                app: Some("notepad.exe".into()),
                date: "2026-10-17".into(),
            }]
        );
        let [row] = &h.rows()[..] else {
            panic!("one row")
        };
        assert_eq!(row.kind, "command");
        assert_eq!(row.text, "Scrivi un saluto breve");
        assert_eq!(row.outcome, Some(CommandOutcome::Pasted));
        assert_eq!(row.llm_model.as_deref(), Some("gpt-6-luna"));
        assert_eq!((row.tokens_in, row.tokens_out), (Some(92), Some(18)));
        assert_eq!(row.reply.as_deref(), Some("Ciao a tutti, buona giornata!"));
        assert!(row.error.is_none());
        // Stage offsets from key-up, in order: transcribed, LLM reply, pasted.
        let (transcribed, replied, pasted) = (
            row.provider_returned.unwrap(),
            row.llm_returned.unwrap(),
            row.pasted.unwrap(),
        );
        assert!(transcribed <= replied && replied <= pasted);
    }

    #[test]
    fn a_blank_model_setting_asks_the_default_model() {
        let mut h = Harness::new(text("Ciao"));
        h.settings.model = "  ".into();
        assert_eq!(h.run("Scrivi"), Outcome::Pasted);
        assert_eq!(h.llm.requests.borrow()[0].model, "gpt-6-luna");
        assert_eq!(h.rows()[0].llm_model.as_deref(), Some("gpt-6-luna"));
    }

    #[test]
    fn an_empty_instruction_makes_no_llm_call_and_writes_no_row() {
        let h = Harness::new(text("never"));
        assert_eq!(h.run_with(None, None), Outcome::Silent);
        assert!(h.llm.requests.borrow().is_empty());
        assert!(h.rows().is_empty());
        assert!(h.pasted().is_empty());
        assert_eq!(h.desktop.clipboard.borrow().as_deref(), Some("user copy"));
    }

    #[test]
    fn a_timeout_or_an_api_error_fails_with_nothing_pasted() {
        for error in [
            ProviderError::Timeout,
            ProviderError::Http {
                status: 404,
                body: "model_not_found".into(),
            },
            ProviderError::Auth("OpenAI authentication rejected (HTTP 401)".into()),
            ProviderError::Network("offline".into()),
        ] {
            let h = Harness::new(Err(error.clone()));
            assert_eq!(
                h.run("Scrivi un saluto"),
                Outcome::Failed(Failure::Command(error.clone()))
            );
            assert!(h.pasted().is_empty(), "{error:?}");
            let [row] = &h.rows()[..] else {
                panic!("one row")
            };
            assert_eq!(row.outcome, Some(CommandOutcome::Failed));
            assert!(row.error.is_some());
            assert!(row.llm_returned.is_some());
            assert!(row.pasted.is_none());
        }
    }

    #[test]
    fn an_empty_reply_fails_but_keeps_its_tokens() {
        let h = Harness::new(text(" \n"));
        assert!(matches!(
            h.run("Scrivi un saluto"),
            Outcome::Failed(Failure::Command(_))
        ));
        assert!(h.pasted().is_empty());
        let row = &h.rows()[0];
        assert_eq!(row.outcome, Some(CommandOutcome::Failed));
        assert_eq!(row.tokens_out, Some(18));
    }

    #[test]
    fn esc_during_the_wait_discards_a_late_reply() {
        let mut h = Harness::new(text("too late"));
        h.llm.late = true;
        assert_eq!(h.run("Scrivi un saluto"), Outcome::Cancelled);
        assert!(h.pasted().is_empty());
        let [row] = &h.rows()[..] else {
            panic!("one row")
        };
        assert_eq!(row.outcome, Some(CommandOutcome::Cancelled));
        assert!(row.reply.is_none() && row.pasted.is_none() && row.error.is_none());
    }

    #[test]
    fn esc_before_the_request_makes_no_llm_call() {
        let h = Harness::new(text("never"));
        h.cancelled.store(true, Ordering::Release);
        assert_eq!(h.run("Scrivi un saluto"), Outcome::Cancelled);
        assert!(h.llm.requests.borrow().is_empty());
        assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Cancelled));
        // Esc aborts a live transcription, which then reports an error: still
        // a cancel, with no cross or notification.
        let h = Harness::new(text("never"));
        h.cancelled.store(true, Ordering::Release);
        assert_eq!(
            h.run_with(None, Some(Err(ProviderError::Timeout))),
            Outcome::Cancelled
        );
        assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Cancelled));
        assert!(h.rows()[0].error.is_none());
    }

    #[test]
    fn a_failed_transcription_or_a_capped_hold_makes_no_llm_call() {
        let h = Harness::new(text("never"));
        assert_eq!(
            h.run_with(None, Some(Err(ProviderError::Timeout))),
            Outcome::Failed(Failure::Transcription(ProviderError::Timeout))
        );
        let h2 = Harness::new(text("never"));
        assert!(matches!(
            h2.run_with(Some("hold exceeded 120 s".into()), Some(Ok("Scrivi"))),
            Outcome::Failed(Failure::Command(_))
        ));
        for h in [h, h2] {
            assert!(h.llm.requests.borrow().is_empty());
            assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Failed));
            assert!(h.rows()[0].error.is_some());
        }
    }

    #[test]
    fn a_failed_paste_fails_and_keeps_the_reply() {
        let mut h = Harness::new(text("Ciao"));
        h.desktop.fail_paste = true;
        assert!(matches!(
            h.run("Scrivi"),
            Outcome::Failed(Failure::Command(_))
        ));
        let row = &h.rows()[0];
        assert_eq!(row.reply.as_deref(), Some("Ciao"));
        assert!(row.pasted.is_none());
    }

    impl Harness {
        fn selecting(reply: Result<Reply, ProviderError>, selected: &str) -> Self {
            let mut h = Self::new(reply);
            h.desktop.selected = Some(selected.into());
            h
        }
        fn clipboard(&self) -> Option<String> {
            self.desktop.clipboard.borrow().clone()
        }
        fn writes(&self) -> Vec<String> {
            self.desktop.writes.borrow().clone()
        }
    }

    #[test]
    fn a_selection_is_sent_as_data_and_the_reply_replaces_it() {
        let h = Harness::selecting(text("Buongiorno a tutti."), "ciao a tutti");
        assert_eq!(h.run("Rendilo più formale"), Outcome::Pasted);
        let request = &h.llm.requests.borrow()[0];
        assert_eq!(request.selection.as_deref(), Some("ciao a tutti"));
        assert_eq!(request.app.as_deref(), Some("notepad.exe"));
        assert_eq!(request.date, "2026-10-17");
        // Copied with Ctrl+Insert, then one paste over the still-selected text.
        assert_eq!(h.desktop.copies.get(), 1);
        assert_eq!(h.pasted(), ["Buongiorno a tutti."]);
        // The user's clipboard comes back once, after the paste, not the selection.
        assert_eq!(h.writes(), ["Buongiorno a tutti.", "user copy"]);
        assert_eq!(h.clipboard().as_deref(), Some("user copy"));
        let row = &h.rows()[0];
        assert_eq!(row.selection_chars, Some(12));
        assert_eq!(row.app.as_deref(), Some("notepad.exe"));
        let json = serde_json::to_string(row).unwrap();
        assert!(
            !json.contains("ciao a tutti"),
            "the selection is never kept"
        );
    }

    #[test]
    fn a_selected_paragraph_keeps_its_trailing_line_break() {
        // The parser trims the reply's end; the selection's own end comes back.
        let h = Harness::selecting(text("Buongiorno a tutti."), "ciao a tutti\r\n");
        assert_eq!(h.run("Rendilo più formale"), Outcome::Pasted);
        assert_eq!(h.pasted(), ["Buongiorno a tutti.\r\n"]);
        assert_eq!(
            h.rows()[0].reply.as_deref(),
            Some("Buongiorno a tutti.\r\n")
        );
    }

    #[test]
    fn with_nothing_selected_the_reply_goes_in_at_the_cursor() {
        let h = Harness::new(text("Ciao!"));
        assert_eq!(h.run("Scrivi un saluto"), Outcome::Pasted);
        assert_eq!(h.llm.requests.borrow()[0].selection, None);
        assert_eq!(h.desktop.copies.get(), 1);
        // The copy got the whole wait, and the clipboard was never borrowed.
        assert_eq!(h.desktop.waited.get(), COPY_WAIT);
        assert_eq!(h.pasted(), ["Ciao!"]);
        assert_eq!(h.clipboard().as_deref(), Some("user copy"));
        let row = &h.rows()[0];
        assert_eq!(row.selection_chars, Some(0));
    }

    #[test]
    fn a_copy_that_lands_within_the_wait_counts_and_a_later_one_does_not() {
        let polls = (COPY_WAIT.as_millis() / COPY_POLL.as_millis()) as usize;
        let mut h = Harness::selecting(text("Hi"), "ciao");
        h.desktop.copy_lag = polls;
        h.run("Traduci");
        assert_eq!(
            h.llm.requests.borrow()[0].selection.as_deref(),
            Some("ciao")
        );
    }

    #[test]
    fn a_copy_that_lands_after_the_wait_is_picked_up_when_the_run_joins() {
        let polls = (COPY_WAIT.as_millis() / COPY_POLL.as_millis()) as usize;
        let mut h = Harness::selecting(text("Hello"), "ciao");
        h.desktop.copy_lag = polls + 3;
        // The user keeps speaking past the wait; the slow app's copy lands.
        h.speaking = 3;
        assert_eq!(h.run("Traduci"), Outcome::Pasted);
        assert_eq!(
            h.llm.requests.borrow()[0].selection.as_deref(),
            Some("ciao")
        );
        assert_eq!(h.writes(), ["Hello", "user copy"]);
        // A cancel gives the user's clipboard back over the late copy too.
        let mut h = Harness::selecting(text("Hello"), "ciao");
        h.desktop.copy_lag = polls + 3;
        h.speaking = 3;
        h.llm.late = true;
        assert_eq!(h.run("Traduci"), Outcome::Cancelled);
        assert_eq!(h.writes(), ["user copy"]);
    }

    #[test]
    fn vs_codes_empty_selection_line_copy_counts_as_no_selection() {
        let mut h = Harness::selecting(text("// hello"), "    let x = 1;\r\n");
        h.desktop.vscode_line = true;
        assert_eq!(h.run("Scrivi un commento"), Outcome::Pasted);
        assert_eq!(h.llm.requests.borrow()[0].selection, None);
        assert_eq!(h.rows()[0].selection_chars, Some(0));
        // VS Code still overwrote the clipboard with the line: it is given back.
        assert_eq!(h.writes(), ["// hello", "user copy"]);
        assert_eq!(h.clipboard().as_deref(), Some("user copy"));
    }

    #[test]
    fn an_empty_copy_counts_as_no_selection() {
        let h = Harness::selecting(text("Hi"), "");
        assert_eq!(h.run("Saluta"), Outcome::Pasted);
        assert_eq!(h.llm.requests.borrow()[0].selection, None);
        assert_eq!(h.clipboard().as_deref(), Some("user copy"));
    }

    #[test]
    fn an_unreadable_clipboard_skips_the_copy_rather_than_lose_it() {
        let mut h = Harness::selecting(text("Hi"), "ciao");
        h.desktop.unreadable = true;
        let selection = read_selection(&h.desktop);
        assert_eq!(h.desktop.copies.get(), 0);
        assert_eq!(selection.text, None);
        assert_eq!(selection.app.as_deref(), Some("notepad.exe"));
    }

    #[test]
    fn cancelled_failed_and_silent_runs_give_the_clipboard_back_once() {
        type Setup = fn(&mut Harness) -> Option<Result<&'static str, ProviderError>>;
        let cases: [(&str, Setup); 5] = [
            ("esc while waiting", |h| {
                h.llm.late = true;
                Some(Ok("Traduci"))
            }),
            ("api error", |h| {
                h.llm.reply = Err(ProviderError::Timeout);
                Some(Ok("Traduci"))
            }),
            ("failed paste", |h| {
                h.desktop.fail_paste = true;
                Some(Ok("Traduci"))
            }),
            ("failed transcription", |_| {
                Some(Err(ProviderError::Timeout))
            }),
            ("silent instruction", |_| None),
        ];
        for (case, setup) in cases {
            let mut h = Harness::selecting(text("Hello"), "ciao");
            let transcript = setup(&mut h);
            assert_ne!(h.run_with(None, transcript), Outcome::Pasted, "{case}");
            assert!(h.pasted().is_empty(), "{case}");
            assert_eq!(h.writes(), ["user copy"], "{case}");
        }
        // Esc while still holding: the run never starts, flow gives it back.
        let h = Harness::selecting(text("never"), "ciao");
        read_selection(&h.desktop).give_back(&h.desktop);
        assert_eq!(h.writes(), ["user copy"]);
    }

    #[cfg(windows)]
    #[test]
    fn today_is_a_plain_iso_date() {
        let today = today();
        assert_eq!(today.len(), 10, "{today}");
        assert!(today.starts_with("20") && today.as_bytes()[4] == b'-');
    }

    #[test]
    fn a_copy_the_user_makes_during_the_run_is_kept() {
        let h = Harness::selecting(Err(ProviderError::Timeout), "ciao");
        let selection = read_selection(&h.desktop);
        *h.desktop.clipboard.borrow_mut() = Some("newer copy".into());
        selection.give_back(&h.desktop);
        assert!(h.writes().is_empty());
        assert_eq!(h.clipboard().as_deref(), Some("newer copy"));
    }

    fn tool(name: &str, url: &str) -> Result<Reply, ProviderError> {
        Ok(Reply {
            text: String::new(),
            call: Some(responses::ToolCall {
                name: name.into(),
                arguments: serde_json::json!({ "url": url }).to_string(),
            }),
            tokens_in: Some(295),
            tokens_out: Some(25),
        })
    }

    impl Harness {
        fn opened(&self) -> Vec<String> {
            self.browser.opened.borrow().clone()
        }
    }

    #[test]
    fn open_url_opens_the_page_pastes_nothing_and_gives_the_clipboard_back() {
        let h = Harness::selecting(
            tool("open_url", "https://it.wikipedia.org/wiki/Roma"),
            "ciao",
        );
        let outcome = h.run("Apri la pagina di Wikipedia su Roma");
        assert_eq!(
            outcome,
            Outcome::Opened {
                host: "it.wikipedia.org".into()
            }
        );
        // The pill: the check, with the host beside it.
        assert_eq!(outcome.pill(), Some(true));
        assert_eq!(outcome.label().as_deref(), Some("Opened it.wikipedia.org"));
        assert_eq!(h.opened(), ["https://it.wikipedia.org/wiki/Roma"]);
        assert!(h.pasted().is_empty());
        // The copy borrowed the clipboard; the user's text comes back once.
        assert_eq!(h.writes(), ["user copy"]);
        assert_eq!(h.llm.requests.borrow().len(), 1, "a single round trip");
        let [row] = &h.rows()[..] else {
            panic!("one row")
        };
        assert_eq!(row.outcome, Some(CommandOutcome::Opened));
        assert_eq!(row.tool.as_deref(), Some("open_url"));
        assert_eq!(
            row.url.as_deref(),
            Some("https://it.wikipedia.org/wiki/Roma")
        );
        assert_eq!((row.tokens_in, row.tokens_out), (Some(295), Some(25)));
        assert!(row.error.is_none() && row.reply.is_none() && row.pasted.is_none());
        assert!(row.llm_returned.unwrap() <= row.opened.unwrap());
        let json = serde_json::to_value(row).unwrap();
        assert_eq!(json["outcome"], "opened");
        assert_eq!(json["url"], "https://it.wikipedia.org/wiki/Roma");
        assert_eq!(json["tool"], "open_url");
    }

    #[test]
    fn a_page_that_is_not_http_or_https_is_refused_and_nothing_opens() {
        for url in [
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:privacy",
            "not a url",
        ] {
            let h = Harness::selecting(tool("open_url", url), "ciao");
            assert!(
                matches!(h.run("Apri"), Outcome::Failed(Failure::Command(_))),
                "{url}"
            );
            assert!(h.opened().is_empty() && h.pasted().is_empty(), "{url}");
            assert_eq!(h.writes(), ["user copy"], "{url}");
            let row = &h.rows()[0];
            assert_eq!(row.outcome, Some(CommandOutcome::Failed));
            assert!(row.error.is_some() && row.url.is_none(), "{url}");
            assert_eq!(row.tokens_out, Some(25), "billed anyway");
        }
    }

    #[test]
    fn an_unknown_tool_fails_and_runs_nothing() {
        let h = Harness::new(tool("run_shell", "https://example.com"));
        assert!(matches!(
            h.run("Apri"),
            Outcome::Failed(Failure::Command(_))
        ));
        assert!(h.opened().is_empty() && h.pasted().is_empty());
        let row = &h.rows()[0];
        assert_eq!(row.outcome, Some(CommandOutcome::Failed));
        assert!(row.error.as_deref().unwrap().contains("unknown tool"));
    }

    #[test]
    fn esc_during_the_wait_opens_nothing() {
        let mut h = Harness::new(tool("open_url", "https://example.com"));
        h.llm.late = true;
        assert_eq!(h.run("Apri example.com"), Outcome::Cancelled);
        assert!(h.opened().is_empty());
        assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Cancelled));
    }

    #[test]
    fn a_browser_that_fails_to_open_fails_the_run() {
        let mut h = Harness::new(tool("open_url", "https://example.com"));
        h.browser.fail = true;
        assert!(matches!(
            h.run("Apri example.com"),
            Outcome::Failed(Failure::Command(_))
        ));
        let row = &h.rows()[0];
        assert_eq!(row.outcome, Some(CommandOutcome::Failed));
        assert!(row.error.as_deref().unwrap().contains("no default browser"));
        // `tool` and `url` describe a page that opened.
        assert!(row.tool.is_none() && row.url.is_none());
    }
}
