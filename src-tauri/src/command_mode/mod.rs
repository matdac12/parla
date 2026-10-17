//! Command mode (PARLA-61): the command run orchestrator and its Responses
//! client. The only code that calls an LLM (ADR-0001).
//!
//! The orchestrator owns a run from the confirmed hold to its outcome: reading
//! the selection (PARLA-64), the empty-instruction rule, the LLM call,
//! cancellation, giving the user's clipboard back, the Card the reply opens
//! in (PARLA-81) and the history row, or running an allowlisted tool instead
//! (PARLA-65). It never pastes: the Card's buttons do, on a click (ADR-0003).
//! It talks to the desktop, the LLM, the browser and history through narrow
//! ports, so it is tested with fakes and flow.rs supplies the real ones.
pub mod responses;
pub mod tools;

pub use crate::inserter::Borrowed;
use crate::{
    card::Card,
    provider::ProviderError,
    settings::CommandSettings,
    timing::{self, CommandOutcome, DictationRecord},
};
use responses::{Kind, Reply, Request};
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
    /// The editable probe from key-down, with the Safety Net on: it picks
    /// the Card's buttons.
    pub focus: Option<&'a crate::focus_probe::Pending>,
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

    /// Every run gives the user's clipboard back, since none pastes: as soon
    /// as the reply is in, or on a cancel, a failure or a silent instruction.
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
    /// The text reply, in the Card above the pill's check.
    Shown(Card),
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
            Outcome::Shown(_) | Outcome::Opened { .. } => Some(true),
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
    /// The LLM step or the tool: command mode's notification.
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
    // Before the Card opens: its Replace or Paste saves and restores the
    // user's clipboard like any paste.
    selection.give_back(ports.desktop);
    if outcome == Outcome::Silent {
        return outcome;
    }
    if let Outcome::Failed(Failure::Command(error)) = &outcome {
        record.error = Some(crate::failure::command_message(error));
    }
    record.outcome = Some(match outcome {
        Outcome::Silent => unreachable!("a silent run returned above"),
        Outcome::Shown(_) => CommandOutcome::Shown,
        Outcome::Opened { .. } => CommandOutcome::Opened,
        Outcome::Cancelled => CommandOutcome::Cancelled,
        Outcome::Failed(_) => CommandOutcome::Failed,
    });
    ports.history.record(record);
    outcome
}

/// Asks the LLM, then opens its text in the Card or runs the tool it called.
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
    let kind = match reply.kind {
        Some(kind) if !reply.text.trim().is_empty() => kind,
        _ => {
            return Outcome::Failed(Failure::Command(ProviderError::Other(
                "the reply was empty.".into(),
            )))
        }
    };
    // The reply's end comes trimmed; an edit keeps a selection's own trailing
    // line break (a triple-clicked paragraph), or Replace would join the next
    // line to it.
    let text = match (&selection.text, kind) {
        (Some(selected), Kind::Edit) => reply.text + &selected[selected.trim_end().len()..],
        _ => reply.text,
    };
    let card = crate::focus_probe::around_paste(run.focus, "command", |verdict| {
        Card::command_reply(
            record.text.trim().to_owned(),
            text.clone(),
            kind,
            selection.text.is_some(),
            verdict,
        )
    });
    record.reply = Some(text);
    record.reply_kind = Some(kind);
    Outcome::Shown(card)
}

/// A tool call instead of a text reply. Nothing goes back to the model.
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
    use crate::card::{Action, Tag};
    use responses::Kind;
    use std::{
        cell::{Cell, RefCell},
        time::SystemTime,
    };

    /// A probe that answers at once that `foreground` has the focus.
    fn probe_on(
        foreground: &'static str,
    ) -> crate::focus_probe::Worker<
        impl FnOnce() -> Option<std::sync::mpsc::Sender<std::sync::Arc<crate::focus_probe::Slot>>>,
    > {
        crate::focus_probe::worker(move || {
            move || crate::focus_probe::Facts {
                foreground: Some(foreground.into()),
                app: None,
                focus: crate::focus_probe::Focus::Nothing,
            }
        })
    }
    fn answered(pending: &crate::focus_probe::Pending) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while pending.answer().is_none() {
            assert!(Instant::now() < deadline, "the probe never answered");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

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
        /// Every clipboard write by Parla: only the give-back, since a run
        /// never pastes.
        writes: RefCell<Vec<String>>,
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
            kind: Some(Kind::Edit),
            call: None,
            tokens_in: Some(92),
            tokens_out: Some(18),
        })
    }

    fn answer(reply: &str) -> Result<Reply, ProviderError> {
        text(reply).map(|reply| Reply {
            kind: Some(Kind::Answer),
            ..reply
        })
    }

    /// The Card a run opened; panics on any other outcome.
    fn card(outcome: Outcome) -> Card {
        match outcome {
            Outcome::Shown(card) => card,
            other => panic!("no Card: {other:?}"),
        }
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
        focus: Option<crate::focus_probe::Pending>,
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
                focus: None,
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
                focus: self.focus.as_ref(),
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
    }

    #[test]
    fn a_reply_opens_the_card_without_waiting_for_a_slow_probe() {
        use crate::focus_probe::{Facts, Focus};
        let (answer, gate) = std::sync::mpsc::channel::<()>();
        let worker = crate::focus_probe::worker(move || {
            move || {
                // Still running when the reply is pasted.
                let _ = gate.recv_timeout(Duration::from_secs(5));
                Facts {
                    foreground: Some("Progman".into()),
                    app: None,
                    focus: Focus::Nothing,
                }
            }
        });
        let mut h = Harness::new(text("Buongiorno"));
        h.focus = Some(worker.start());
        let started = Instant::now();
        let card = card(h.run("Scrivi un saluto"));
        assert!(started.elapsed() < Duration::from_millis(500));
        // Not answered yet: Unknown, which counts as editable.
        assert_eq!(card.actions, [Action::Paste, Action::Copy]);
        answer.send(()).unwrap();
    }

    #[test]
    fn a_reply_opens_in_the_card_and_its_row_is_shown() {
        let h = Harness::new(text("Ciao a tutti, buona giornata!"));
        let card = card(h.run("Scrivi un saluto breve"));
        assert_eq!(card.tag, Tag::Command);
        assert_eq!(card.header.as_deref(), Some("Scrivi un saluto breve"));
        assert_eq!(card.text, "Ciao a tutti, buona giornata!");
        // An edit with nothing selected: Paste at the cursor leads.
        assert_eq!(card.actions, [Action::Paste, Action::Copy]);
        assert_eq!(card.primary, Action::Paste);
        // Never pasted, and the clipboard was never touched.
        assert!(h.writes().is_empty());
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
        assert_eq!(row.outcome, Some(CommandOutcome::Shown));
        assert_eq!(row.reply_kind, Some(Kind::Edit));
        assert_eq!(row.llm_model.as_deref(), Some("gpt-6-luna"));
        assert_eq!((row.tokens_in, row.tokens_out), (Some(92), Some(18)));
        assert_eq!(row.reply.as_deref(), Some("Ciao a tutti, buona giornata!"));
        assert!(row.error.is_none() && row.pasted.is_none());
        // Stage offsets from key-up, in order: transcribed, LLM reply.
        assert!(row.provider_returned.unwrap() <= row.llm_returned.unwrap());
        let json = serde_json::to_value(row).unwrap();
        assert_eq!(json["outcome"], "shown");
        assert_eq!(json["reply_kind"], "edit");
        assert_eq!(json["kind"], "command");
    }

    #[test]
    fn an_edit_over_a_selection_offers_replace_first_and_gives_the_clipboard_back() {
        let h = Harness::selecting(
            text("Ciao Giulia, grazie di cuore!"),
            "Gentile Giulia, grazie.",
        );
        let card = card(h.run("Rendilo più personale"));
        assert_eq!(card.actions, [Action::Replace, Action::Copy]);
        assert_eq!(card.primary, Action::Replace);
        assert_eq!(card.text, "Ciao Giulia, grazie di cuore!");
        // The copy borrowed the clipboard: the user's text is back before the
        // Card opens, so Replace saves and restores it like any paste.
        assert_eq!(h.writes(), ["user copy"]);
        assert_eq!(h.clipboard().as_deref(), Some("user copy"));
    }

    #[test]
    fn an_answer_leads_with_copy_and_keeps_replace_or_paste_on_offer() {
        let h = Harness::selecting(
            answer("Vuol dire che il foro competente è quello di Milano."),
            "Art. 13 — Foro competente: Milano.",
        );
        let explained = card(h.run("Cosa significa questa clausola?"));
        assert_eq!(explained.primary, Action::Copy);
        assert_eq!(explained.actions, [Action::Copy, Action::Replace]);
        assert_eq!(h.rows()[0].reply_kind, Some(Kind::Answer));
        let h = Harness::new(answer("Canberra."));
        let answered = card(h.run("What's the capital of Australia?"));
        assert_eq!(answered.actions, [Action::Copy, Action::Paste]);
    }

    #[test]
    fn with_nothing_editable_in_focus_the_card_offers_copy_only() {
        for reply in [text("Buongiorno a tutti."), answer("È una clausola.")] {
            let mut h = Harness::selecting(reply, "ciao a tutti");
            let worker = probe_on("Progman");
            let focus = worker.start();
            answered(&focus);
            h.focus = Some(focus);
            let card = card(h.run("Rendilo più formale"));
            assert_eq!(card.actions, [Action::Copy]);
            assert_eq!(card.primary, Action::Copy);
            assert_eq!(h.writes(), ["user copy"]);
        }
    }

    #[test]
    fn no_reply_is_ever_pasted() {
        // Every kind, with and without a selection: the only clipboard write
        // is the give-back of a borrowed clipboard, never the reply.
        for reply in [text("Hello"), answer("It means hello.")] {
            for selected in [None, Some("ciao")] {
                let mut h = Harness::new(reply.clone());
                h.desktop.selected = selected.map(Into::into);
                assert!(matches!(h.run("Traduci"), Outcome::Shown(_)));
                let owed: &[&str] = if selected.is_some() {
                    &["user copy"]
                } else {
                    &[]
                };
                assert_eq!(h.writes(), owed, "{selected:?}");
            }
        }
    }

    #[test]
    fn a_blank_model_setting_asks_the_default_model() {
        let mut h = Harness::new(text("Ciao"));
        h.settings.model = "  ".into();
        assert!(matches!(h.run("Scrivi"), Outcome::Shown(_)));
        assert_eq!(h.llm.requests.borrow()[0].model, "gpt-6-luna");
        assert_eq!(h.rows()[0].llm_model.as_deref(), Some("gpt-6-luna"));
    }

    #[test]
    fn an_empty_instruction_makes_no_llm_call_and_writes_no_row() {
        let h = Harness::new(text("never"));
        assert_eq!(h.run_with(None, None), Outcome::Silent);
        assert!(h.llm.requests.borrow().is_empty());
        assert!(h.rows().is_empty());
        assert_eq!(h.desktop.clipboard.borrow().as_deref(), Some("user copy"));
    }

    #[test]
    fn a_timeout_or_an_api_error_fails_with_no_card() {
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
        let row = &h.rows()[0];
        assert_eq!(row.outcome, Some(CommandOutcome::Failed));
        assert_eq!(row.tokens_out, Some(18));
        assert!(row.reply.is_none() && row.reply_kind.is_none());
    }

    #[test]
    fn esc_during_the_wait_discards_a_late_reply() {
        let mut h = Harness::new(text("too late"));
        h.llm.late = true;
        assert_eq!(h.run("Scrivi un saluto"), Outcome::Cancelled);
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
    fn a_selection_is_sent_as_data_and_never_kept() {
        let h = Harness::selecting(text("Buongiorno a tutti."), "ciao a tutti");
        assert!(matches!(h.run("Rendilo più formale"), Outcome::Shown(_)));
        let request = &h.llm.requests.borrow()[0];
        assert_eq!(request.selection.as_deref(), Some("ciao a tutti"));
        assert_eq!(request.app.as_deref(), Some("notepad.exe"));
        assert_eq!(request.date, "2026-10-17");
        // Copied with Ctrl+Insert; the user's clipboard comes back once.
        assert_eq!(h.desktop.copies.get(), 1);
        assert_eq!(h.writes(), ["user copy"]);
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
    fn an_edit_of_a_selected_paragraph_keeps_its_trailing_line_break() {
        // The parser trims the reply's end; the selection's own end comes
        // back, so Replace does not join the next line.
        let h = Harness::selecting(text("Buongiorno a tutti."), "ciao a tutti\r\n");
        assert_eq!(
            card(h.run("Rendilo più formale")).text,
            "Buongiorno a tutti.\r\n"
        );
        assert_eq!(
            h.rows()[0].reply.as_deref(),
            Some("Buongiorno a tutti.\r\n")
        );
        // An answer is about the selection, not in its place.
        let h = Harness::selecting(answer("Un saluto."), "ciao a tutti\r\n");
        assert_eq!(card(h.run("Cos'è?")).text, "Un saluto.");
    }

    #[test]
    fn with_nothing_selected_the_card_offers_paste_at_the_cursor() {
        let h = Harness::new(text("Ciao!"));
        assert_eq!(card(h.run("Scrivi un saluto")).primary, Action::Paste);
        assert_eq!(h.llm.requests.borrow()[0].selection, None);
        assert_eq!(h.desktop.copies.get(), 1);
        // The copy got the whole wait, and the clipboard was never borrowed.
        assert_eq!(h.desktop.waited.get(), COPY_WAIT);
        assert!(h.writes().is_empty());
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
        assert_eq!(card(h.run("Traduci")).primary, Action::Replace);
        assert_eq!(
            h.llm.requests.borrow()[0].selection.as_deref(),
            Some("ciao")
        );
        assert_eq!(h.writes(), ["user copy"]);
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
        assert_eq!(card(h.run("Scrivi un commento")).primary, Action::Paste);
        assert_eq!(h.llm.requests.borrow()[0].selection, None);
        assert_eq!(h.rows()[0].selection_chars, Some(0));
        // VS Code still overwrote the clipboard with the line: it is given back.
        assert_eq!(h.writes(), ["user copy"]);
        assert_eq!(h.clipboard().as_deref(), Some("user copy"));
    }

    #[test]
    fn an_empty_copy_counts_as_no_selection() {
        let h = Harness::selecting(text("Hi"), "");
        assert_eq!(card(h.run("Saluta")).primary, Action::Paste);
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
        let cases: [(&str, Setup); 4] = [
            ("esc while waiting", |h| {
                h.llm.late = true;
                Some(Ok("Traduci"))
            }),
            ("api error", |h| {
                h.llm.reply = Err(ProviderError::Timeout);
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
            let outcome = h.run_with(None, transcript);
            assert!(!matches!(outcome, Outcome::Shown(_)), "{case}");
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
            kind: None,
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
    fn open_url_opens_the_page_with_no_card_and_gives_the_clipboard_back() {
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
        assert!(row.reply_kind.is_none());
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
            assert!(h.opened().is_empty(), "{url}");
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
        assert!(h.opened().is_empty());
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
