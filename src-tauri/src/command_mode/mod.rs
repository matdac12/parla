//! Command mode (PARLA-61): the command run orchestrator and its Responses
//! client. The only code that calls an LLM (ADR-0001).
//!
//! The orchestrator owns a run from the confirmed hold to its outcome: reading
//! the selection (PARLA-64), the empty-instruction rule, the LLM call,
//! cancellation, giving the user's clipboard back, the Card the reply opens
//! in (PARLA-81) and the history row, or running an allowlisted tool instead
//! (PARLA-65, PARLA-86). It never pastes: the Card's buttons do, on a click
//! (ADR-0003). It talks to the desktop, the LLM, the browser, Traccia, the
//! clock and history through narrow ports, so it is tested with fakes and
//! flow.rs supplies the real ones.
pub mod responses;
pub mod tools;
pub mod traccia;
pub mod when;

pub use crate::inserter::Borrowed;
use crate::{
    card::{Action, Card},
    focus_probe::Verdict,
    provider::ProviderError,
    settings::CommandSettings,
    timing::{self, CommandOutcome, DictationRecord},
};
use responses::{Content, Reply, Request};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tools::Tool;
use when::{LocalClock, When};

/// The focused app and the clipboard, through the inserter.
pub trait Desktop {
    /// The clipboard's text; None when it holds no text.
    fn clipboard_text(&self) -> Result<Option<String>, String>;
    /// Moves on whenever any app writes the clipboard.
    fn clipboard_sequence(&self) -> u32;
    /// A copy shortcut into the focused app.
    fn send_copy(&self, key: CopyKey) -> Result<(), String>;
    /// The clipboard holds an editor's copy of the whole line under an empty
    /// selection (VS Code, Visual Studio, Notepad++).
    fn copied_empty_selection(&self) -> bool;
    /// The focused app's executable name.
    fn foreground_app(&self) -> Option<String>;
    /// The focused top-level window's class, such as `ConsoleWindowClass`.
    fn foreground_class(&self) -> Option<String>;
    fn now(&self) -> Instant;
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

/// Mattia's To-dos in Traccia (ADR-0004).
pub trait Reminders {
    /// One blocking `POST`, never retried.
    fn create(&self, title: &str, when: &When) -> Result<traccia::Created, traccia::Error>;
    /// The Projects a title may reference as `@KEY`: the cached list, which
    /// may be empty.
    fn projects(&self) -> Vec<traccia::Project>;
}

pub struct Ports<'a> {
    pub desktop: &'a dyn Desktop,
    pub llm: &'a dyn Llm,
    pub browser: &'a dyn UrlOpener,
    pub history: &'a dyn HistorySink,
    /// Some only while `command.reminders` is on: it offers `create_reminder`.
    pub reminders: Option<&'a dyn Reminders>,
    /// Turns a reminder's local day and time into UTC.
    pub clock: &'a dyn LocalClock,
}

pub struct Run<'a> {
    pub settings: &'a CommandSettings,
    pub key_up: Instant,
    /// The local date and time at key-up, for the request.
    pub now: when::LocalNow,
    /// Set by Esc, from key-up until the reply is in.
    pub cancelled: &'a AtomicBool,
    /// The hold hit the cap: the instruction is recorded but not sent.
    pub hold_error: Option<String>,
    /// The editable probe from key-down, with the Safety Net on: it picks
    /// the Card's buttons.
    pub focus: Option<&'a crate::focus_probe::Pending>,
}

/// How long the focused app gets to answer each copy key.
pub const COPY_WAIT: Duration = Duration::from_millis(150);
/// A slow app's copy still counts up to this long after the first copy key,
/// while the user is still speaking. A clipboard write after it is somebody
/// else's (a clipboard manager, a sync): never the selection, and never
/// overwritten by the give-back (PARLA-78).
pub const LATE_COPY_WAIT: Duration = Duration::from_secs(1);
/// About: Windows may round a 5 ms wait up to its 15.6 ms timer tick, so the
/// waits above are deadlines, not counts of polls.
const COPY_POLL: Duration = Duration::from_millis(5);

/// A copy shortcut for the selection read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyKey {
    /// Tried first: unlike Ctrl+C it never interrupts a terminal (PARLA-64).
    CtrlInsert,
    /// For the apps that ignore Ctrl+Insert, such as PDFgear (PARLA-84).
    /// Never sent where [`takes_ctrl_c`] says no.
    CtrlC,
}

/// Whether the focused window may get Ctrl+C: never a console, a terminal, an
/// app with a terminal pane (an IDE) or a window onto another machine (remote
/// desktop, a VM), where Ctrl+C with nothing selected interrupts the running
/// program. Nor a Chromium, Firefox or Java window: they all copy on
/// Ctrl+Insert, so a copy that missed there had nothing selected, and they
/// host terminals of their own (VS Code, T3 Code, a web shell in a tab,
/// JetBrains). A window Parla cannot name gets none either.
pub fn takes_ctrl_c(class: Option<&str>, app: Option<&str>) -> bool {
    const CLASSES: &[&str] = &[
        "ConsoleWindowClass",
        "CASCADIA_HOSTING_WINDOW_CLASS",
        "PseudoConsoleWindow",
        "mintty",
        "PuTTY",
        "VirtualConsoleClass",
        "MozillaWindowClass",
        "SunAwtFrame",
        "TscShellContainerClass",
    ];
    // Lowercase, without `.exe`. Any name with `term`, `console` or `conhost`
    // in it is out as well (Windows Terminal, OpenConsole, MobaXterm…).
    const APPS: &[&str] = &[
        // Shells and terminals.
        "cmd",
        "powershell",
        "powershell_ise",
        "pwsh",
        "wsl",
        "wslhost",
        "bash",
        "mintty",
        "alacritty",
        "hyper",
        "tabby",
        "conemu",
        "conemu64",
        "putty",
        "kitty",
        "warp",
        "rio",
        "ghostty",
        "xshell",
        "securecrt",
        "mremoteng",
        // Editors and IDEs with a terminal pane, whatever their window class.
        "code",
        "code - insiders",
        "code - oss",
        "cursor",
        "windsurf",
        "vscodium",
        "positron",
        "zed",
        "devenv",
        "eclipse",
        "fleet",
        // Other machines.
        "mstsc",
        "msrdc",
        "vmconnect",
        "virtualboxvm",
        "vmware",
        "vmplayer",
        "wfica32",
        "cdviewer",
        "anydesk",
        "parsecd",
        "rustdesk",
        "teamviewer",
    ];
    let (Some(class), Some(app)) = (class, app) else {
        return false;
    };
    let app = app.to_lowercase();
    let app = app.strip_suffix(".exe").unwrap_or(&app);
    !CLASSES.contains(&class)
        && !class.starts_with("Chrome_WidgetWin_")
        && !APPS.contains(&app)
        && !["term", "console", "conhost"]
            .iter()
            .any(|name| app.contains(name))
        // T3 Code's channels; JetBrains IDEs end in 64 (`idea64`), and their
        // window is `SunAwtFrame` anyway.
        && !app.starts_with("t3 code")
}

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
    /// Every run gives the user's clipboard back, since none pastes: as soon
    /// as the reply is in, or on a cancel, a failure or a silent instruction.
    pub fn give_back(self, desktop: &dyn Desktop) {
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
/// sequence for `COPY_WAIT`; then Ctrl+C, where the window takes it, for as
/// long again. A change with text is the selection. A slow app's copy counts
/// until `LATE_COPY_WAIT` after the first key, or until `joined`: the run
/// wants the selection. A copy that lands later still overwrites the
/// clipboard, and is not given back: Parla cannot tell it from somebody
/// else's write.
pub fn read_selection(desktop: &dyn Desktop, joined: &AtomicBool) -> Selection {
    // The same window for both, read before any key is sent.
    let (app, class) = (desktop.foreground_app(), desktop.foreground_class());
    let mut selection = Selection {
        app,
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
    let copied = || desktop.clipboard_sequence() != before;
    // Polls until the clipboard moves on, `until` passes or `stop` holds;
    // then one last look.
    let watch = |until: Instant, stop: &dyn Fn() -> bool| {
        while !stop() && desktop.now() < until {
            desktop.wait(COPY_POLL);
            if copied() {
                return true;
            }
        }
        copied()
    };
    // A failed copy leaves the sequence alone: no selection.
    let send = |key| {
        if let Err(error) = desktop.send_copy(key) {
            eprintln!("Parla selection copy ({key:?}) failed: {error}");
        }
    };
    let first = desktop.now();
    let never = || false;
    send(CopyKey::CtrlInsert);
    let mut answered = watch(first + COPY_WAIT, &never);
    if !answered && takes_ctrl_c(class.as_deref(), selection.app.as_deref()) {
        send(CopyKey::CtrlC);
        answered = watch(desktop.now() + COPY_WAIT, &never);
    }
    let joined = || joined.load(Ordering::Acquire);
    if answered || watch(first + LATE_COPY_WAIT, &joined) {
        selection.take_copy(desktop, saved);
    }
    selection
}

/// A text reply's kind (ADR-0003): it picks the Card's main button, never
/// whether Parla pastes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Text meant to go into the document: Replace or Paste.
    Edit,
    /// Text addressed to the user: Copy.
    Answer,
}

/// The Card's buttons for a reply, the main one first. With nothing editable
/// in focus only Copy; otherwise Replace over the selection, or Paste at the
/// cursor. An edit leads with that, an answer with Copy.
fn buttons(kind: Kind, selected: bool, verdict: Verdict) -> Vec<Action> {
    let insert = if selected {
        Action::Replace
    } else {
        Action::Paste
    };
    match (verdict, kind) {
        (Verdict::NotEditable, _) => vec![Action::Copy],
        (_, Kind::Edit) => vec![insert, Action::Copy],
        (_, Kind::Answer) => vec![Action::Copy, insert],
    }
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
    /// `create_reminder` created a To-do: the reminder Card confirms it, with
    /// Open and Undo (PARLA-88).
    Reminded(Card),
    Cancelled,
    Failed(Failure),
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
    let selection = selection();
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
        Outcome::Reminded(_) => CommandOutcome::Created,
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
        now: run.now,
        reminders: ports.reminders.is_some(),
        projects: ports
            .reminders
            .map(|reminders| reminders.projects())
            .unwrap_or_default(),
    };
    record.llm_model = Some(request.model.clone());
    let reply = ports.llm.reply(&request, run.cancelled);
    record.llm_returned = Some(timing::elapsed_ms(run.key_up));
    // Billed whatever comes of it, even when Esc discards it (PARLA-78).
    if let Ok(reply) = &reply {
        record.tokens_in = reply.tokens_in;
        record.tokens_out = reply.tokens_out;
    }
    let reply = match reply {
        // A reply or an error that lands after Esc is discarded.
        _ if cancelled() => return Outcome::Cancelled,
        Ok(reply) => reply,
        Err(error) => return Outcome::Failed(Failure::Command(error)),
    };
    let (text, kind) = match reply.content {
        Content::Call(call) => return run_tool(ports, run, record, &call),
        Content::Unusable(reason) => {
            return Outcome::Failed(Failure::Command(ProviderError::Other(reason)))
        }
        Content::Text { text, kind } => (text, kind),
    };
    // The reply's end comes trimmed; an edit keeps a selection's own trailing
    // line break (a triple-clicked paragraph), or Replace would join the next
    // line to it.
    let text = match (&selection.text, kind) {
        (Some(selected), Kind::Edit) => text + &selected[selected.trim_end().len()..],
        _ => text,
    };
    let actions = crate::focus_probe::around_paste(run.focus, "command", |verdict| {
        buttons(kind, selection.text.is_some(), verdict)
    });
    let card = Card::command(record.text.trim().to_owned(), text.clone(), actions);
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
    let failed = |reason| Outcome::Failed(Failure::Command(ProviderError::Other(reason)));
    let tool = match tools::validate(call, ports.reminders.is_some()) {
        Ok(tool) => tool,
        Err(reason) => return failed(reason),
    };
    match tool {
        Tool::OpenUrl(url) => match ports.browser.open(url.as_str()) {
            Ok(()) => {
                record.opened = Some(timing::elapsed_ms(run.key_up));
                record.tool = Some(call.name.clone());
                record.url = Some(url.to_string());
                Outcome::Opened {
                    host: url.host_str().unwrap_or_default().into(),
                }
            }
            Err(error) => failed(format!("could not open the page ({error}).")),
        },
        Tool::CreateReminder { title, day, time } => {
            let Some(reminders) = ports.reminders else {
                return failed("reminders are off.".into());
            };
            let when = when::resolve(day, time, run.now, ports.clock);
            // Esc counted until the reply was in. From here on it is
            // ignored: a To-do created after Esc would surprise.
            match reminders.create(&title, &when) {
                Ok(created) => {
                    record.opened = Some(timing::elapsed_ms(run.key_up));
                    record.tool = Some(call.name.clone());
                    record.url = Some(created.url.clone());
                    record.reply = Some(created.summary());
                    let when_line = created.when_line(run.now.date);
                    Outcome::Reminded(Card::reminder(
                        record.text.trim().to_owned(),
                        created.title,
                        when_line,
                        created.id,
                        created.url,
                    ))
                }
                Err(error) => failed(traccia::reminder_message(&error, &when, run.now.date)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::Tag;
    use crate::focus_probe::test_support::{answered, probe_on};
    use std::{
        cell::{Cell, RefCell},
        sync::{mpsc, Arc},
        time::SystemTime,
    };
    use when::test_support::{date, saturday_evening, time, RomeInOctober};

    #[test]
    fn the_buttons_follow_the_kind_the_selection_and_the_probe() {
        use Action::{Copy, Paste, Replace};
        use Kind::{Answer, Edit};
        use Verdict::{Editable, NotEditable, Unknown};
        let cases = [
            // An edit's main button puts it in the document.
            (Edit, true, Unknown, vec![Replace, Copy]),
            (Edit, true, Editable, vec![Replace, Copy]),
            (Edit, false, Unknown, vec![Paste, Copy]),
            // An answer's is Copy; Replace or Paste stays on offer.
            (Answer, true, Unknown, vec![Copy, Replace]),
            (Answer, false, Editable, vec![Copy, Paste]),
            // Nothing editable: Copy only, whatever the kind.
            (Edit, true, NotEditable, vec![Copy]),
            (Answer, false, NotEditable, vec![Copy]),
        ];
        for (kind, selected, verdict, want) in cases {
            assert_eq!(
                buttons(kind, selected, verdict),
                want,
                "{kind:?}, selected {selected}, {verdict:?}"
            );
        }
    }

    /// A focused app and its clipboard. A copy key copies `selected` (None:
    /// the app copies nothing) after `copy_lag` polls; an app that
    /// `ignores_insert` answers Ctrl+C only.
    struct FakeDesktop {
        clipboard: RefCell<Option<String>>,
        sequence: Cell<u32>,
        selected: Option<String>,
        ignores_insert: bool,
        copy_lag: usize,
        copy_due: Cell<Option<usize>>,
        vscode_line: bool,
        unreadable: bool,
        app: Option<String>,
        class: Option<String>,
        /// The copy keys sent, in order.
        keys: RefCell<Vec<CopyKey>>,
        /// The fake clock: `start` plus every wait.
        start: Instant,
        waited: Cell<Duration>,
        /// Set once the run joins the read: at once unless `join_after`
        /// polls are given.
        joined: AtomicBool,
        join_after: Cell<Option<usize>>,
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
                ignores_insert: false,
                copy_lag: 1,
                copy_due: Cell::new(None),
                vscode_line: false,
                unreadable: false,
                app: Some("notepad.exe".into()),
                class: Some("Notepad".into()),
                keys: RefCell::default(),
                start: Instant::now(),
                waited: Cell::new(Duration::ZERO),
                joined: AtomicBool::new(true),
                join_after: Cell::new(None),
                writes: RefCell::default(),
            }
        }
        fn write(&self, text: &str) {
            *self.clipboard.borrow_mut() = Some(text.into());
            self.writes.borrow_mut().push(text.into());
        }
        /// Another app writes the clipboard.
        fn written_by_another_app(&self, text: &str) {
            *self.clipboard.borrow_mut() = Some(text.into());
            self.sequence.set(self.sequence.get() + 1);
        }
        /// The run joins after `polls` more polls; None: never.
        fn joins_after(&self, polls: Option<usize>) {
            self.joined.store(false, Ordering::Release);
            self.join_after.set(polls);
        }
        fn keys(&self) -> Vec<CopyKey> {
            self.keys.borrow().clone()
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
        fn send_copy(&self, key: CopyKey) -> Result<(), String> {
            self.keys.borrow_mut().push(key);
            let answered = !(self.ignores_insert && key == CopyKey::CtrlInsert);
            // A copy already on its way is not restarted.
            if answered && self.selected.is_some() && self.copy_due.get().is_none() {
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
        fn foreground_class(&self) -> Option<String> {
            self.class.clone()
        }
        fn now(&self) -> Instant {
            self.start + self.waited.get()
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
            match self.join_after.get() {
                Some(0 | 1) => {
                    self.join_after.set(None);
                    self.joined.store(true, Ordering::Release);
                }
                Some(left) => self.join_after.set(Some(left - 1)),
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
        replied(Content::Text {
            text: reply.into(),
            kind: Kind::Edit,
        })
    }

    fn answer(reply: &str) -> Result<Reply, ProviderError> {
        replied(Content::Text {
            text: reply.into(),
            kind: Kind::Answer,
        })
    }

    fn replied(content: Content) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content,
            tokens_in: Some(92),
            tokens_out: Some(18),
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
        /// Some: reminders are on.
        reminders: Option<FakeReminders>,
        cancelled: Arc<AtomicBool>,
        settings: CommandSettings,
        /// Another app writes the clipboard after the read ended, before
        /// the run joins it.
        stray: Option<&'static str>,
        focus: Option<crate::focus_probe::Pending>,
    }
    impl Harness {
        fn new(reply: Result<Reply, ProviderError>) -> Self {
            Self {
                desktop: FakeDesktop::new(Some("user copy")),
                llm: FakeLlm::new(reply),
                browser: FakeBrowser::default(),
                history: FakeHistory::default(),
                reminders: None,
                cancelled: Arc::default(),
                settings: CommandSettings::default(),
                stray: None,
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
                reminders: self
                    .reminders
                    .as_ref()
                    .map(|reminders| reminders as &dyn Reminders),
                clock: &RomeInOctober,
            };
            let input = Run {
                settings: &self.settings,
                key_up: Instant::now(),
                now: saturday_evening(),
                cancelled: &self.cancelled,
                hold_error,
                focus: self.focus.as_ref(),
            };
            // Read at the confirmed hold, before the instruction is in.
            let selection = read_selection(&self.desktop, &self.desktop.joined);
            if let Some(stray) = self.stray {
                self.desktop.written_by_another_app(stray);
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
        // The desktop, but still probing when the reply is in.
        let (open, gate) = mpsc::channel();
        let worker = probe_on("Progman", gate);
        let mut h = Harness::new(text("Buongiorno"));
        h.focus = Some(worker.start());
        let started = Instant::now();
        let card = card(h.run("Scrivi un saluto"));
        assert!(started.elapsed() < Duration::from_millis(500));
        // Not answered yet: Unknown, which counts as editable.
        assert_eq!(card.actions, [Action::Paste, Action::Copy]);
        open.send(()).unwrap();
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
                now: saturday_evening(),
                reminders: false,
                projects: Vec::new(),
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
        let capital = card(h.run("What's the capital of Australia?"));
        assert_eq!(capital.actions, [Action::Copy, Action::Paste]);
    }

    #[test]
    fn with_nothing_editable_in_focus_the_card_offers_copy_only() {
        for reply in [text("Buongiorno a tutti."), answer("È una clausola.")] {
            let mut h = Harness::selecting(reply, "ciao a tutti");
            let (open, gate) = mpsc::channel();
            let worker = probe_on("Progman", gate);
            let focus = worker.start();
            open.send(()).unwrap();
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
    fn an_unusable_reply_fails_but_keeps_its_tokens() {
        // Cut off, refused, unreadable, empty: billed all the same (PARLA-78).
        let h = Harness::new(replied(Content::Unusable(
            "the reply was cut off (max_output_tokens).".into(),
        )));
        assert_eq!(
            h.run("Scrivi un saluto"),
            Outcome::Failed(Failure::Command(ProviderError::Other(
                "the reply was cut off (max_output_tokens).".into()
            )))
        );
        let row = &h.rows()[0];
        assert_eq!(row.outcome, Some(CommandOutcome::Failed));
        assert_eq!((row.tokens_in, row.tokens_out), (Some(92), Some(18)));
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
        // Discarded, but billed: its tokens count in Total cost (PARLA-78).
        assert_eq!((row.tokens_in, row.tokens_out), (Some(92), Some(18)));
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
        assert_eq!(request.now, saturday_evening());
        // Copied with Ctrl+Insert alone; the user's clipboard comes back once.
        assert_eq!(h.desktop.keys(), [CopyKey::CtrlInsert]);
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
        // Ctrl+Insert and then Ctrl+C each got the whole wait, and the
        // clipboard was never borrowed.
        assert_eq!(h.desktop.keys(), [CopyKey::CtrlInsert, CopyKey::CtrlC]);
        assert_eq!(h.desktop.waited.get(), COPY_WAIT * 2);
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
    fn a_slow_copy_that_lands_while_the_user_speaks_is_the_selection() {
        let polls = (COPY_WAIT.as_millis() / COPY_POLL.as_millis()) as usize;
        for late in [false, true] {
            let mut h = Harness::selecting(text("Hello"), "ciao");
            // Past both copy keys' waits, before the run joins.
            h.desktop.copy_lag = 2 * polls + 3;
            h.desktop.joins_after(Some(2 * polls + 10));
            h.llm.late = late;
            let outcome = h.run("Traduci");
            assert_eq!(
                h.llm.requests.borrow()[0].selection.as_deref(),
                Some("ciao")
            );
            if late {
                assert_eq!(outcome, Outcome::Cancelled);
            } else {
                assert_eq!(card(outcome).primary, Action::Replace);
            }
            // Borrowed, so given back, on a cancel too.
            assert_eq!(h.writes(), ["user copy"]);
            // The read stopped once the run joined.
            assert_eq!(h.desktop.waited.get(), COPY_POLL * (2 * polls as u32 + 3));
        }
    }

    #[test]
    fn a_clipboard_write_after_the_read_is_neither_the_selection_nor_overwritten() {
        // Nothing selected; a clipboard manager or a sync writes the
        // clipboard while the user is still speaking (PARLA-78).
        let mut h = Harness::new(text("Ciao!"));
        h.stray = Some("synced from the phone");
        let card = card(h.run("Scrivi un saluto"));
        assert_eq!(card.actions, [Action::Paste, Action::Copy], "no Replace");
        assert_eq!(h.llm.requests.borrow()[0].selection, None);
        assert_eq!(h.rows()[0].selection_chars, Some(0));
        assert!(h.writes().is_empty());
        assert_eq!(h.clipboard().as_deref(), Some("synced from the phone"));
    }

    #[test]
    fn the_read_waits_for_a_slow_copy_no_longer_than_the_late_wait() {
        let mut desktop = FakeDesktop::new(Some("user copy"));
        desktop.selected = Some("ciao".into());
        desktop.copy_lag = usize::MAX;
        // The user is still speaking: the run has not joined.
        desktop.joins_after(None);
        let selection = read_selection(&desktop, &desktop.joined);
        assert_eq!(desktop.waited.get(), LATE_COPY_WAIT);
        assert_eq!(selection.text, None);
        // Not borrowed: whatever lands later is left alone.
        selection.give_back(&desktop);
        assert!(desktop.writes.borrow().is_empty());
    }

    #[test]
    fn an_app_that_ignores_ctrl_insert_is_read_with_ctrl_c() {
        // PDFgear (PARLA-84).
        let mut h = Harness::selecting(answer("Vuol dire..."), "Art. 13 — Foro competente");
        h.desktop.ignores_insert = true;
        h.desktop.app = Some("pdfeditor.exe".into());
        let card = card(h.run("Mi spieghi cosa significa?"));
        assert_eq!(h.desktop.keys(), [CopyKey::CtrlInsert, CopyKey::CtrlC]);
        assert_eq!(
            h.llm.requests.borrow()[0].selection.as_deref(),
            Some("Art. 13 — Foro competente")
        );
        assert_eq!(card.actions, [Action::Copy, Action::Replace]);
        // Borrowed and given back like Ctrl+Insert's copy.
        assert_eq!(h.writes(), ["user copy"]);
        assert_eq!(h.clipboard().as_deref(), Some("user copy"));
    }

    #[test]
    fn a_terminal_never_gets_ctrl_c() {
        for (class, app) in [
            ("CASCADIA_HOSTING_WINDOW_CLASS", "WindowsTerminal.exe"),
            ("ConsoleWindowClass", "conhost.exe"),
            ("mintty", "mintty.exe"),
            ("Chrome_WidgetWin_1", "Code.exe"),
            ("Chrome_WidgetWin_1", "T3 Code (Nightly).exe"),
        ] {
            let mut h = Harness::selecting(text("Hi"), "ls -la");
            h.desktop.ignores_insert = true;
            h.desktop.class = Some(class.into());
            h.desktop.app = Some(app.into());
            assert_eq!(card(h.run("Spiega")).primary, Action::Paste, "{app}");
            assert_eq!(h.desktop.keys(), [CopyKey::CtrlInsert], "{app}");
            assert!(h.writes().is_empty(), "{app}");
        }
    }

    #[test]
    fn ctrl_c_goes_only_to_a_window_parla_can_name_that_is_no_terminal() {
        for (class, app, takes) in [
            (Some("Notepad"), Some("notepad.exe"), true),
            (
                Some("HwndWrapper[pdfeditor.exe;;1f0e]"),
                Some("pdfeditor.exe"),
                true,
            ),
            (Some("Outlook Host"), Some("olk.exe"), true),
            // These copy on Ctrl+Insert, and may host a terminal.
            (Some("Chrome_WidgetWin_1"), Some("msedge.exe"), false),
            (Some("MozillaWindowClass"), Some("firefox.exe"), false),
            // Windows onto another machine.
            (Some("TscShellContainerClass"), Some("mstsc.exe"), false),
            (Some("VMConnect"), Some("vmconnect.exe"), false),
            (Some("ConsoleWindowClass"), Some("cmd.exe"), false),
            // A console app's own window class gives it away.
            (Some("ConsoleWindowClass"), Some("myapp.exe"), false),
            (
                Some("CASCADIA_HOSTING_WINDOW_CLASS"),
                Some("WindowsTerminal.exe"),
                false,
            ),
            (Some("Window Class"), Some("OpenConsole.exe"), false),
            (Some("Window Class"), Some("Alacritty.exe"), false),
            (Some("VirtualConsoleClass"), Some("ConEmu64.exe"), false),
            (
                Some("Chrome_WidgetWin_1"),
                Some("Code - Insiders.exe"),
                false,
            ),
            (Some("SunAwtFrame"), Some("idea64.exe"), false),
            (Some("Chrome_WidgetWin_1"), Some("T3 Code.exe"), false),
            (Some("Some"), Some("MobaXterm.exe"), false),
            (None, Some("notepad.exe"), false),
            (Some("Notepad"), None, false),
        ] {
            assert_eq!(takes_ctrl_c(class, app), takes, "{class:?} {app:?}");
        }
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
        let selection = read_selection(&h.desktop, &h.desktop.joined);
        assert!(h.desktop.keys().is_empty());
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
        read_selection(&h.desktop, &h.desktop.joined).give_back(&h.desktop);
        assert_eq!(h.writes(), ["user copy"]);
    }

    #[test]
    fn a_copy_the_user_makes_during_the_run_is_kept() {
        let h = Harness::selecting(Err(ProviderError::Timeout), "ciao");
        let selection = read_selection(&h.desktop, &h.desktop.joined);
        *h.desktop.clipboard.borrow_mut() = Some("newer copy".into());
        selection.give_back(&h.desktop);
        assert!(h.writes().is_empty());
        assert_eq!(h.clipboard().as_deref(), Some("newer copy"));
    }

    fn tool(name: &str, url: &str) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: Content::Call(responses::ToolCall {
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

    /// Traccia: answers every POST with `result`; with `esc`, Esc is
    /// pressed while the POST is on its way.
    struct FakeReminders {
        result: Result<traccia::Created, traccia::Error>,
        created: RefCell<Vec<(String, When)>>,
        esc: Option<Arc<AtomicBool>>,
        projects: Vec<traccia::Project>,
    }
    impl FakeReminders {
        fn answering(result: Result<traccia::Created, traccia::Error>) -> Self {
            Self {
                result,
                created: RefCell::default(),
                esc: None,
                projects: Vec::new(),
            }
        }
    }
    impl Reminders for FakeReminders {
        fn create(&self, title: &str, when: &When) -> Result<traccia::Created, traccia::Error> {
            self.created.borrow_mut().push((title.into(), when.clone()));
            if let Some(esc) = &self.esc {
                esc.store(true, Ordering::Release);
            }
            self.result.clone()
        }
        fn projects(&self) -> Vec<traccia::Project> {
            self.projects.clone()
        }
    }

    fn stored() -> traccia::Created {
        traccia::Created {
            id: "01K8TODO0000000000000000AB".into(),
            url: "https://traccia.example.ts.net/personal?todo=01K8TODO0000000000000000AB".into(),
            title: "Chiamare Luca".into(),
            day: Some("2026-10-18".into()),
            time: Some("15:00".into()),
        }
    }

    fn remind(title: &str, day: Option<&str>, time: Option<&str>) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: Content::Call(responses::ToolCall {
                name: tools::CREATE_REMINDER.into(),
                arguments: serde_json::json!({ "title": title, "day": day, "time": time })
                    .to_string(),
            }),
            tokens_in: Some(980),
            tokens_out: Some(38),
        })
    }

    impl Harness {
        /// Reminders on, Traccia answering `result`.
        fn reminding(
            reply: Result<Reply, ProviderError>,
            result: Result<traccia::Created, traccia::Error>,
        ) -> Self {
            let mut h = Self::selecting(reply, "ciao");
            h.reminders = Some(FakeReminders::answering(result));
            h
        }
        fn created(&self) -> Vec<(String, When)> {
            self.reminders.as_ref().unwrap().created.borrow().clone()
        }
    }

    #[test]
    fn a_reminder_call_creates_one_to_do_and_records_it() {
        let h = Harness::reminding(
            remind("Chiamare Luca", Some("2026-10-18"), Some("15:00")),
            Ok(stored()),
        );
        let outcome = h.run("ricordami di chiamare Luca domani alle 15");
        // The Card says what Traccia stored, the day as seen from Saturday.
        assert_eq!(
            outcome,
            Outcome::Reminded(Card::reminder(
                "ricordami di chiamare Luca domani alle 15".into(),
                "Chiamare Luca".into(),
                "Sun 18/10 · 15:00".into(),
                stored().id,
                stored().url,
            ))
        );
        assert_eq!(
            h.created(),
            [(
                "Chiamare Luca".to_string(),
                When::At {
                    day: date("2026-10-18"),
                    time: time("15:00"),
                    utc: Some("2026-10-18T13:00:00Z".into()),
                }
            )]
        );
        // The tool was offered, in a single round trip.
        let requests = h.llm.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].reminders);
        // The clipboard the copy borrowed comes back, and nothing is pasted.
        assert_eq!(h.writes(), ["user copy"]);
        let [row] = &h.rows()[..] else {
            panic!("one row")
        };
        assert_eq!(row.outcome, Some(CommandOutcome::Created));
        assert_eq!(row.tool.as_deref(), Some("create_reminder"));
        assert_eq!(row.url.as_deref(), Some(stored().url.as_str()));
        assert_eq!(
            row.reply.as_deref(),
            Some("Chiamare Luca · 2026-10-18 15:00")
        );
        assert_eq!((row.tokens_in, row.tokens_out), (Some(980), Some(38)));
        assert!(row.error.is_none() && row.reply_kind.is_none() && row.pasted.is_none());
        // Key-up to the To-do created, as for a page opened.
        assert!(row.llm_returned.unwrap() <= row.opened.unwrap());
        let json = serde_json::to_value(row).unwrap();
        assert_eq!(json["outcome"], "created");
        assert_eq!(json["tool"], "create_reminder");
    }

    #[test]
    fn an_undated_reminder_is_an_anytime_to_do() {
        let anytime = traccia::Created {
            title: "Comprare il latte".into(),
            day: None,
            time: None,
            ..stored()
        };
        let h = Harness::reminding(remind("Comprare il latte", None, None), Ok(anytime));
        let Outcome::Reminded(card) = h.run("ricordami di comprare il latte") else {
            panic!("no reminder Card")
        };
        assert_eq!(card.text, "Comprare il latte");
        assert_eq!(card.detail.as_deref(), Some("Anytime · no reminder"));
        assert_eq!(
            h.created(),
            [("Comprare il latte".to_string(), When::Anytime)]
        );
        assert_eq!(h.rows()[0].reply.as_deref(), Some("Comprare il latte"));
    }

    #[test]
    fn esc_before_the_post_creates_nothing() {
        let mut h = Harness::reminding(
            remind("Chiamare Luca", Some("2026-10-18"), Some("15:00")),
            Ok(stored()),
        );
        h.llm.late = true;
        assert_eq!(h.run("ricordami di chiamare Luca"), Outcome::Cancelled);
        assert!(h.created().is_empty());
        assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Cancelled));
        assert_eq!(h.writes(), ["user copy"]);
    }

    #[test]
    fn esc_once_the_post_started_is_ignored() {
        let mut h = Harness::reminding(
            remind("Chiamare Luca", Some("2026-10-18"), Some("15:00")),
            Ok(stored()),
        );
        h.reminders.as_mut().unwrap().esc = Some(h.cancelled.clone());
        assert!(matches!(
            h.run("ricordami di chiamare Luca"),
            Outcome::Reminded(_)
        ));
        assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Created));
    }

    #[test]
    fn every_traccia_failure_fails_loudly_with_the_spec_message() {
        let refused = |field: &str| traccia::Error::Refused {
            code: Some("validation_error".into()),
            fields: vec![field.into()],
        };
        let cases = [
            (
                remind("Chiamare la banca", Some("2026-10-17"), Some("10:00")),
                refused("remindAt"),
                "10:00 today has already passed.",
            ),
            (
                remind("Chiamare Luca", None, None),
                refused("title"),
                "Traccia refused the reminder (title).",
            ),
            (
                remind("Chiamare Luca", None, None),
                traccia::Error::Unreachable,
                "Traccia can't be reached. Is Tailscale on?",
            ),
            (
                remind("Chiamare Luca", None, None),
                traccia::Error::Timeout(traccia::WRITE_TIMEOUT),
                "Traccia didn't answer in 5 s. The reminder may have been created.",
            ),
            (
                remind("Chiamare Luca", None, None),
                traccia::Error::Token,
                "Traccia rejected the token. Check it in Settings → Command mode.",
            ),
            (
                remind("Chiamare Luca", None, None),
                traccia::Error::Scope,
                "This Traccia token can't create To-dos (it needs the todos scope).",
            ),
            (
                remind("Chiamare Luca", None, None),
                traccia::Error::Status(502),
                "Traccia error (HTTP 502).",
            ),
            (
                remind("Chiamare Luca", None, None),
                traccia::Error::Missing,
                "Add the Traccia URL and token first.",
            ),
        ];
        for (reply, error, message) in cases {
            let h = Harness::reminding(reply, Err(error));
            assert_eq!(
                h.run("ricordami di chiamare"),
                Outcome::Failed(Failure::Command(ProviderError::Other(message.into()))),
                "{message}"
            );
            assert_eq!(h.created().len(), 1, "one POST, no retry: {message}");
            assert_eq!(h.writes(), ["user copy"], "{message}");
            let row = &h.rows()[0];
            assert_eq!(row.outcome, Some(CommandOutcome::Failed));
            assert_eq!(row.error.as_deref(), Some(message));
            assert!(row.tool.is_none() && row.url.is_none() && row.reply.is_none());
        }
    }

    #[test]
    fn a_reminder_the_validator_refuses_is_never_sent() {
        for reply in [
            remind("", None, None),
            remind("Pagare", Some("2026-02-30"), None),
            remind("Pagare", None, Some("25:00")),
        ] {
            let h = Harness::reminding(reply, Ok(stored()));
            assert_eq!(
                h.run("ricordami di pagare"),
                Outcome::Failed(Failure::Command(ProviderError::Other(
                    "the reminder could not be read.".into()
                )))
            );
            assert!(h.created().is_empty());
            assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Failed));
        }
    }

    #[test]
    fn with_reminders_off_the_tool_is_not_offered_and_a_stray_call_is_refused() {
        let h = Harness::new(remind("Chiamare Luca", None, None));
        let outcome = h.run("ricordami di chiamare Luca");
        assert_eq!(
            outcome,
            Outcome::Failed(Failure::Command(ProviderError::Other(
                "the reply asked for an unknown tool (create_reminder).".into()
            )))
        );
        assert!(!h.llm.requests.borrow()[0].reminders);
        assert_eq!(h.rows()[0].outcome, Some(CommandOutcome::Failed));
    }

    #[test]
    fn the_request_carries_the_cached_projects_only_while_reminders_are_on() {
        let crm = traccia::Project {
            key: "CRM".into(),
            name: "CRM".into(),
        };
        let mut h = Harness::reminding(
            remind("Riunione @CRM", Some("2026-11-03"), Some("15:00")),
            Ok(stored()),
        );
        h.reminders.as_mut().unwrap().projects = vec![crm.clone()];
        h.run("ricordami il 3 novembre alle 15 della riunione del progetto CRM");
        assert_eq!(h.llm.requests.borrow()[0].projects, [crm]);
        assert_eq!(h.created()[0].0, "Riunione @CRM");
        let off = Harness::new(text("ok"));
        off.run("ciao");
        assert!(off.llm.requests.borrow()[0].projects.is_empty());
    }
}
