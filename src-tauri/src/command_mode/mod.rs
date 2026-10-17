//! Command mode (PARLA-61): the command run orchestrator and its Responses
//! client. The only code that calls an LLM (ADR-0001).
//!
//! The orchestrator owns a run from the confirmed hold to its outcome: reading
//! the selection (PARLA-64), the empty-instruction rule, the LLM call,
//! cancellation, the paste, giving the user's clipboard back and the history
//! row. It talks to the desktop, the LLM and history through narrow ports, so
//! it is tested with fakes and flow.rs supplies the real ones.
pub mod responses;

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

pub trait HistorySink {
    fn record(&self, record: DictationRecord);
}

pub struct Ports<'a> {
    pub desktop: &'a dyn Desktop,
    pub llm: &'a dyn Llm,
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
}

impl Selection {
    /// A run that pastes nothing gives the user's clipboard back: a cancel,
    /// a failure or a silent instruction.
    pub fn give_back(self, desktop: &dyn Desktop) {
        if let Some(borrowed) = &self.borrowed {
            desktop.give_back(borrowed);
        }
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
    if !copied {
        return selection;
    }
    let copied = desktop.clipboard_text().unwrap_or_else(|error| {
        eprintln!("Parla selection unreadable: {error}");
        None
    });
    selection.text = copied
        .clone()
        .filter(|text| !text.is_empty() && !desktop.copied_empty_selection());
    selection.borrowed = Some(Borrowed { saved, copied });
    selection
}

/// Today, `YYYY-MM-DD`: local on Windows, UTC elsewhere.
pub fn today() -> String {
    #[cfg(windows)]
    let (year, month, day) = {
        let mut now = unsafe { std::mem::zeroed() };
        // SAFETY: GetLocalTime only fills the struct it is given.
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut now) };
        (
            i64::from(now.wYear),
            u32::from(now.wMonth),
            u32::from(now.wDay),
        )
    };
    #[cfg(not(windows))]
    let (year, month, day) = civil_date(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() / 86_400) as i64,
    );
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
#[cfg(any(not(windows), test))]
fn civil_date(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// What the pill and the notification show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An empty or silent instruction: nothing shown, no row.
    Silent,
    Pasted,
    Cancelled,
    Failed(Failure),
}

impl Outcome {
    /// The pill's check or cross; None drops the pill without one.
    pub fn pill(&self) -> Option<bool> {
        match self {
            Outcome::Silent | Outcome::Cancelled => None,
            Outcome::Pasted => Some(true),
            Outcome::Failed(_) => Some(false),
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
    let selection = selection();
    // Whether there was a selection and its length, never its text.
    record.selection = Some(selection.text.is_some());
    record.selection_chars = selection
        .text
        .as_ref()
        .map(|text| text.chars().count() as u64);
    record.app = selection.app.clone();
    let outcome = match transcribed {
        None => Outcome::Silent,
        // Esc aborts a live transcription, so its error is the cancel.
        Some(_) if cancelled() => Outcome::Cancelled,
        Some(Err(error)) => {
            record.error = Some(error.to_string());
            Outcome::Failed(Failure::Transcription(error))
        }
        Some(Ok(())) => reply_and_paste(ports, &run, &mut record, &selection),
    };
    // A paste gives the clipboard back itself, once.
    if outcome != Outcome::Pasted {
        selection.give_back(ports.desktop);
    }
    if let Outcome::Failed(Failure::Command(error)) = &outcome {
        record.error = Some(crate::failure::command_message(error));
    }
    record.outcome = Some(match outcome {
        Outcome::Silent => return outcome,
        Outcome::Pasted => CommandOutcome::Pasted,
        Outcome::Cancelled => CommandOutcome::Cancelled,
        Outcome::Failed(_) => CommandOutcome::Failed,
    });
    ports.history.record(record);
    outcome
}

fn reply_and_paste(
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
    if reply.text.trim().is_empty() {
        return Outcome::Failed(Failure::Command(ProviderError::Other(
            "the reply was empty.".into(),
        )));
    }
    // Over the selection, which is still selected, or at the cursor.
    let pasted = ports
        .desktop
        .paste(&reply.text, selection.borrowed.as_ref());
    record.reply = Some(reply.text);
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
            tokens_in: Some(92),
            tokens_out: Some(18),
        })
    }

    struct Harness {
        desktop: FakeDesktop,
        llm: FakeLlm,
        history: FakeHistory,
        cancelled: AtomicBool,
        settings: CommandSettings,
    }
    impl Harness {
        fn new(reply: Result<Reply, ProviderError>) -> Self {
            Self {
                desktop: FakeDesktop::new(Some("user copy")),
                llm: FakeLlm::new(reply),
                history: FakeHistory::default(),
                cancelled: AtomicBool::new(false),
                settings: CommandSettings::default(),
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
        assert_eq!(row.selection, Some(true));
        assert_eq!(row.selection_chars, Some(12));
        assert_eq!(row.app.as_deref(), Some("notepad.exe"));
        let json = serde_json::to_string(row).unwrap();
        assert!(
            !json.contains("ciao a tutti"),
            "the selection is never kept"
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
        assert_eq!((row.selection, row.selection_chars), (Some(false), None));
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
        let mut h = Harness::selecting(text("Hi"), "ciao");
        h.desktop.copy_lag = polls + 1;
        h.run("Traduci");
        assert_eq!(h.llm.requests.borrow()[0].selection, None);
    }

    #[test]
    fn vs_codes_empty_selection_line_copy_counts_as_no_selection() {
        let mut h = Harness::selecting(text("// hello"), "    let x = 1;\r\n");
        h.desktop.vscode_line = true;
        assert_eq!(h.run("Scrivi un commento"), Outcome::Pasted);
        assert_eq!(h.llm.requests.borrow()[0].selection, None);
        assert_eq!(h.rows()[0].selection, Some(false));
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

    #[test]
    fn today_is_a_plain_iso_date() {
        assert_eq!(civil_date(0), (1970, 1, 1));
        assert_eq!(civil_date(20_743), (2026, 10, 17));
        assert_eq!(civil_date(11_016), (2000, 2, 29));
        let today = today();
        assert_eq!(today.len(), 10);
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
}
