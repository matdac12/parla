//! The editable probe (PARLA-79): whether the focused element accepts text.
use std::{
    sync::{mpsc, Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

/// What the probe concluded. Unknown counts as editable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Editable,
    NotEditable,
    Unknown,
}

/// Which rule decided the verdict, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Edit,
    Document,
    Value,
    Text,
    NoForeground,
    Shell,
    NoMatch,
    Error,
}

/// The foreground window, as far as the verdict cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreground {
    None,
    /// The desktop or a taskbar.
    Shell,
    App,
}

/// The UI Automation facts about the focused element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Element {
    /// A UIA control type id, such as 50004 for Edit.
    pub control_type: i32,
    /// None without a ValuePattern.
    pub value_read_only: Option<bool>,
    pub text_pattern: bool,
    pub keyboard_focusable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub foreground: Foreground,
    /// The UIA error when the focused element could not be read.
    pub focused: Result<Element, String>,
    /// The foreground app's executable, for the log.
    pub app: Option<String>,
}

/// A probe's answer, kept for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub verdict: Verdict,
    pub rule: Rule,
    pub app: Option<String>,
    pub control_type: Option<i32>,
    pub error: Option<String>,
    /// The UIA calls alone.
    pub probe: Duration,
    /// From the start of the session to the answer.
    pub answered_after: Duration,
}

/// Reads the focused element's facts, on the probe thread.
pub trait Probe {
    fn facts(&mut self) -> Facts;
}
impl<F: FnMut() -> Facts> Probe for F {
    fn facts(&mut self) -> Facts {
        self()
    }
}

/// One session's probe. Dropping it discards the answer.
pub struct Pending(Arc<Slot>);

struct Slot {
    started: Instant,
    answer: OnceLock<Reading>,
}

impl Pending {
    fn new() -> Self {
        Self(Arc::new(Slot {
            started: Instant::now(),
            answer: OnceLock::new(),
        }))
    }
    /// Never waits: None until the probe has answered.
    pub fn answer(&self) -> Option<&Reading> {
        self.0.answer.get()
    }
    /// The paste point's read: one atomic load, never a wait.
    pub fn read(&self) -> Seen<'_> {
        Seen {
            answer: self.answer(),
            pending: self,
        }
    }
}

/// What the paste point saw. Fixed when read: a later answer is only noted.
#[derive(Clone, Copy)]
pub struct Seen<'a> {
    answer: Option<&'a Reading>,
    pending: &'a Pending,
}

impl Seen<'_> {
    /// Unknown when the probe had not answered.
    pub fn verdict(self) -> Verdict {
        self.answer
            .map_or(Verdict::Unknown, |reading| reading.verdict)
    }

    /// One log line for a `run` ("dictation", "command"). Call it after the
    /// paste: the write is not free.
    pub fn log(self, run: &str) {
        let line = match (self.answer, self.pending.answer()) {
            (Some(reading), _) => reading.describe(),
            (None, Some(late)) => format!(
                "unknown, no answer at the paste; later: {}",
                late.describe()
            ),
            (None, None) => format!(
                "unknown, no answer at the paste (still probing {} ms after key-down)",
                self.pending.0.started.elapsed().as_millis()
            ),
        };
        eprintln!("Parla focus probe ({run}): {line}");
    }
}

impl Reading {
    fn describe(&self) -> String {
        let verdict = match self.verdict {
            Verdict::Editable => "editable",
            Verdict::NotEditable => "not editable",
            Verdict::Unknown => "unknown",
        };
        let rule = match self.rule {
            Rule::Edit => "edit",
            Rule::Document => "document",
            Rule::Value => "value",
            Rule::Text => "text",
            Rule::NoForeground => "no-foreground",
            Rule::Shell => "shell",
            Rule::NoMatch => "no-match",
            Rule::Error => "error",
        };
        let control = self.control_type.map_or("-".into(), control_name);
        let error = self
            .error
            .as_ref()
            .map_or(String::new(), |error| format!(", error {error}"));
        format!(
            "{verdict} by {rule}, app {}, control {control}, probe {} ms, answered {} ms after key-down{error}",
            self.app.as_deref().unwrap_or("-"),
            self.probe.as_millis(),
            self.answered_after.as_millis(),
        )
    }
}

/// UIA's control type names, 50000 (Button) to 50040 (AppBar).
const CONTROL_NAMES: [&str; 41] = [
    "Button",
    "Calendar",
    "CheckBox",
    "ComboBox",
    "Edit",
    "Hyperlink",
    "Image",
    "ListItem",
    "List",
    "Menu",
    "MenuBar",
    "MenuItem",
    "ProgressBar",
    "RadioButton",
    "ScrollBar",
    "Slider",
    "Spinner",
    "StatusBar",
    "Tab",
    "TabItem",
    "Text",
    "ToolBar",
    "ToolTip",
    "Tree",
    "TreeItem",
    "Custom",
    "Group",
    "Thumb",
    "DataGrid",
    "DataItem",
    "Document",
    "SplitButton",
    "Window",
    "Pane",
    "Header",
    "HeaderItem",
    "Table",
    "TitleBar",
    "Separator",
    "SemanticZoom",
    "AppBar",
];

fn control_name(id: i32) -> String {
    usize::try_from(id - 50000)
        .ok()
        .and_then(|index| CONTROL_NAMES.get(index))
        .map_or_else(|| id.to_string(), |name| (*name).to_owned())
}

/// The probe thread, created by the first `start`, never at startup. One
/// thread serves every session in turn.
pub struct Worker<F> {
    make: Mutex<Option<F>>,
    jobs: OnceLock<Option<mpsc::Sender<Arc<Slot>>>>,
}

impl<F, P> Worker<F>
where
    F: FnOnce() -> P + Send + 'static,
    P: Probe,
{
    pub const fn new(make: F) -> Self {
        Self {
            make: Mutex::new(Some(make)),
            jobs: OnceLock::new(),
        }
    }

    /// Queues a probe and returns at once. Without the thread the answer
    /// never comes: Unknown.
    pub fn start(&self) -> Pending {
        let pending = Pending::new();
        let jobs = self.jobs.get_or_init(|| {
            let make = self.make.lock().unwrap_or_else(|e| e.into_inner()).take()?;
            spawn(make)
        });
        if let Some(jobs) = jobs {
            let _ = jobs.send(pending.0.clone());
        }
        pending
    }
}

fn spawn<P: Probe>(make: impl FnOnce() -> P + Send + 'static) -> Option<mpsc::Sender<Arc<Slot>>> {
    let (jobs, queue) = mpsc::channel::<Arc<Slot>>();
    std::thread::Builder::new()
        .name("parla-focus-probe".into())
        .spawn(move || {
            let mut probe = make();
            for slot in queue {
                // Discarded before its turn (a cancelled hold): skip the UIA calls.
                if Arc::strong_count(&slot) == 1 {
                    continue;
                }
                let began = Instant::now();
                let facts = probe.facts();
                let probe = began.elapsed();
                let (verdict, rule) = decide(&facts);
                let _ = slot.answer.set(Reading {
                    verdict,
                    rule,
                    control_type: facts.focused.as_ref().ok().map(|e| e.control_type),
                    error: facts.focused.err(),
                    app: facts.app,
                    probe,
                    answered_after: slot.started.elapsed(),
                });
            }
        })
        .map_err(|error| eprintln!("Cannot start the focus probe: {error}"))
        .ok()?;
    Some(jobs)
}

static WORKER: Worker<fn() -> uia::Uia> = Worker::new(uia::Uia::new);

/// Starts this session's probe on the app's probe thread (created now if
/// this is the first session) and returns at once.
pub fn start() -> Pending {
    WORKER.start()
}

/// UI Automation on the probe thread: COM (MTA) and one cached client. Thin
/// on purpose: `decide` holds the rules.
#[cfg(windows)]
mod uia {
    use super::{Element, Facts, Foreground};
    use windows::{
        core::{Interface, Result},
        Win32::{
            System::Com::{
                CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
            },
            UI::{
                Accessibility::{
                    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest,
                    IUIAutomationTextPattern, IUIAutomationValuePattern, UIA_ControlTypePropertyId,
                    UIA_IsKeyboardFocusablePropertyId, UIA_TextPatternId,
                    UIA_ValueIsReadOnlyPropertyId, UIA_ValuePatternId,
                },
                WindowsAndMessaging::{GetClassNameW, GetForegroundWindow},
            },
        },
    };

    /// A hung app fails its probe (Unknown) instead of holding up the next one.
    const TIMEOUT_MS: u32 = 1000;
    /// The desktop and the taskbars.
    const SHELL_CLASSES: [&str; 4] = [
        "Progman",
        "WorkerW",
        "Shell_TrayWnd",
        "Shell_SecondaryTrayWnd",
    ];

    pub struct Uia(std::result::Result<(IUIAutomation, IUIAutomationCacheRequest), String>);

    impl Uia {
        pub fn new() -> Self {
            // SAFETY: COM is initialised on this thread before any UIA call,
            // and the interfaces never leave it.
            Self(unsafe { client() }.map_err(|error| describe(&error)))
        }
    }

    unsafe fn client() -> Result<(IUIAutomation, IUIAutomationCacheRequest)> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            if let Ok(timeouts) = automation.cast::<IUIAutomation2>() {
                let _ = timeouts.SetConnectionTimeout(TIMEOUT_MS);
                let _ = timeouts.SetTransactionTimeout(TIMEOUT_MS);
            }
            // Everything the verdict reads, in one cross-process call.
            let cache = automation.CreateCacheRequest()?;
            for property in [
                UIA_ControlTypePropertyId,
                UIA_IsKeyboardFocusablePropertyId,
                UIA_ValueIsReadOnlyPropertyId,
            ] {
                cache.AddProperty(property)?;
            }
            cache.AddPattern(UIA_ValuePatternId)?;
            cache.AddPattern(UIA_TextPatternId)?;
            Ok((automation, cache))
        }
    }

    impl super::Probe for Uia {
        fn facts(&mut self) -> Facts {
            let foreground = foreground();
            let focused = match (&self.0, foreground) {
                (_, Foreground::None) => Err("no foreground window".into()),
                (Err(error), _) => Err(error.clone()),
                // SAFETY: the client was made on this thread.
                (Ok((automation, cache)), _) => {
                    unsafe { focused(automation, cache) }.map_err(|error| describe(&error))
                }
            };
            Facts {
                foreground,
                focused,
                app: crate::inserter::foreground_app(),
            }
        }
    }

    unsafe fn focused(
        automation: &IUIAutomation,
        cache: &IUIAutomationCacheRequest,
    ) -> Result<Element> {
        unsafe {
            let element = automation.GetFocusedElementBuildCache(cache)?;
            // An unsupported pattern comes back null, an error here: absent.
            let value_read_only = element
                .GetCachedPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .and_then(|value| value.CachedIsReadOnly())
                .ok()
                .map(|read_only| read_only.as_bool());
            Ok(Element {
                control_type: element.CachedControlType()?.0,
                value_read_only,
                text_pattern: element
                    .GetCachedPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                    .is_ok(),
                keyboard_focusable: element.CachedIsKeyboardFocusable()?.as_bool(),
            })
        }
    }

    fn foreground() -> Foreground {
        // SAFETY: plain Win32 calls; the buffer outlives GetClassNameW.
        let window = unsafe { GetForegroundWindow() };
        if window.is_invalid() {
            return Foreground::None;
        }
        let mut class = [0u16; 64];
        let length = unsafe { GetClassNameW(window, &mut class) };
        let class = String::from_utf16_lossy(&class[..length.max(0) as usize]);
        if SHELL_CLASSES.contains(&class.as_str()) {
            Foreground::Shell
        } else {
            Foreground::App
        }
    }

    fn describe(error: &windows::core::Error) -> String {
        format!("{} {}", error.code(), error.message())
    }
}

/// Off Windows, where Parla does not run, every probe is Unknown: editable.
#[cfg(not(windows))]
mod uia {
    use super::{Facts, Foreground};

    pub struct Uia;

    impl Uia {
        pub fn new() -> Self {
            Self
        }
    }

    impl super::Probe for Uia {
        fn facts(&mut self) -> Facts {
            Facts {
                foreground: Foreground::App,
                focused: Err("no UI Automation on this platform".into()),
                app: None,
            }
        }
    }
}

/// UIA control type ids (UIAutomationClient.h), for every platform.
const EDIT: i32 = 50004;
const DOCUMENT: i32 = 50030;

/// Editable when any rule holds. Not editable only on a confident no; a UIA
/// error is Unknown. An edit box wins over the shell (a desktop rename).
pub fn decide(facts: &Facts) -> (Verdict, Rule) {
    if facts.foreground == Foreground::None {
        return (Verdict::NotEditable, Rule::NoForeground);
    }
    if let Ok(element) = &facts.focused {
        let rule = match element.control_type {
            EDIT => Some(Rule::Edit),
            DOCUMENT => Some(Rule::Document),
            _ if element.value_read_only == Some(false) => Some(Rule::Value),
            _ if element.text_pattern && element.keyboard_focusable => Some(Rule::Text),
            _ => None,
        };
        if let Some(rule) = rule {
            return (Verdict::Editable, rule);
        }
    }
    match (facts.foreground, &facts.focused) {
        (Foreground::Shell, _) => (Verdict::NotEditable, Rule::Shell),
        (_, Err(_)) => (Verdict::Unknown, Rule::Error),
        (_, Ok(_)) => (Verdict::NotEditable, Rule::NoMatch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANE: i32 = 50033;
    const LIST_ITEM: i32 = 50007;

    fn app(element: Element) -> Facts {
        Facts {
            foreground: Foreground::App,
            focused: Ok(element),
            app: None,
        }
    }
    fn control(control_type: i32) -> Element {
        Element {
            control_type,
            ..Element::default()
        }
    }

    #[test]
    fn edit_and_document_controls_are_editable() {
        assert_eq!(decide(&app(control(EDIT))), (Verdict::Editable, Rule::Edit));
        assert_eq!(
            decide(&app(control(DOCUMENT))),
            (Verdict::Editable, Rule::Document)
        );
    }

    #[test]
    fn a_value_pattern_is_editable_unless_it_is_read_only() {
        let value = |read_only| Element {
            value_read_only: Some(read_only),
            ..control(PANE)
        };
        assert_eq!(decide(&app(value(false))), (Verdict::Editable, Rule::Value));
        assert_eq!(
            decide(&app(value(true))),
            (Verdict::NotEditable, Rule::NoMatch)
        );
    }

    #[test]
    fn a_text_pattern_is_editable_only_when_keyboard_focusable() {
        let text = |keyboard_focusable| Element {
            text_pattern: true,
            keyboard_focusable,
            ..control(PANE)
        };
        assert_eq!(decide(&app(text(true))), (Verdict::Editable, Rule::Text));
        assert_eq!(
            decide(&app(text(false))),
            (Verdict::NotEditable, Rule::NoMatch)
        );
        // Focusable alone is not text: a button or a list item.
        let focusable = Element {
            keyboard_focusable: true,
            ..control(LIST_ITEM)
        };
        assert_eq!(
            decide(&app(focusable)),
            (Verdict::NotEditable, Rule::NoMatch)
        );
    }

    #[test]
    fn the_shell_is_not_editable_unless_an_edit_box_has_focus() {
        let desktop = |focused| Facts {
            foreground: Foreground::Shell,
            focused,
            app: Some("explorer.exe".into()),
        };
        assert_eq!(
            decide(&desktop(Ok(control(LIST_ITEM)))),
            (Verdict::NotEditable, Rule::Shell)
        );
        assert_eq!(
            decide(&desktop(Err("UIA timed out".into()))),
            (Verdict::NotEditable, Rule::Shell)
        );
        // Renaming a file on the desktop.
        assert_eq!(
            decide(&desktop(Ok(control(EDIT)))),
            (Verdict::Editable, Rule::Edit)
        );
    }

    #[test]
    fn no_foreground_window_is_not_editable() {
        let facts = Facts {
            foreground: Foreground::None,
            focused: Err("no element".into()),
            app: None,
        };
        assert_eq!(decide(&facts), (Verdict::NotEditable, Rule::NoForeground));
    }

    #[test]
    fn a_uia_error_in_an_app_is_unknown() {
        let facts = Facts {
            foreground: Foreground::App,
            focused: Err("E_FAIL".into()),
            app: None,
        };
        assert_eq!(decide(&facts), (Verdict::Unknown, Rule::Error));
    }

    /// Polls for a test only: production never waits on a probe.
    fn answered(pending: &Pending) -> &Reading {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(reading) = pending.answer() {
                return reading;
            }
            assert!(Instant::now() < deadline, "the probe never answered");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// A probe that answers Edit in Notepad once the test opens its gate.
    fn gated() -> (mpsc::Sender<()>, impl FnMut() -> Facts + Send) {
        let (open, gate) = mpsc::channel();
        let probe = move || {
            let _ = gate.recv();
            Facts {
                app: Some("notepad.exe".into()),
                ..app(control(EDIT))
            }
        };
        (open, probe)
    }

    #[test]
    fn reading_a_probe_never_waits_and_a_late_answer_is_kept() {
        let (open, probe) = gated();
        let worker = Worker::new(move || probe);
        let pending = worker.start();
        let read_at = Instant::now();
        let seen = pending.read();
        assert_eq!(seen.verdict(), Verdict::Unknown);
        assert!(read_at.elapsed() < Duration::from_millis(50));
        open.send(()).unwrap();
        let reading = answered(&pending);
        assert_eq!(
            (reading.verdict, reading.rule),
            (Verdict::Editable, Rule::Edit)
        );
        assert_eq!(reading.app.as_deref(), Some("notepad.exe"));
        assert_eq!(reading.control_type, Some(EDIT));
        assert!(reading.answered_after >= reading.probe);
        // What the paste point saw stays as it was.
        assert_eq!(seen.verdict(), Verdict::Unknown);
        assert_eq!(pending.read().verdict(), Verdict::Editable);
    }

    #[test]
    fn the_probe_thread_starts_with_the_first_session() {
        let (made, was_made) = mpsc::channel();
        let worker = Worker::new(move || {
            made.send(()).unwrap();
            || app(control(EDIT))
        });
        std::thread::sleep(Duration::from_millis(20));
        assert!(was_made.try_recv().is_err(), "built before any session");
        let first = worker.start();
        was_made.recv_timeout(Duration::from_secs(5)).unwrap();
        answered(&first);
        // Later sessions reuse it.
        answered(&worker.start());
        assert!(was_made.try_recv().is_err());
    }

    #[test]
    fn a_probe_discarded_before_its_turn_never_runs() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (open, mut probe) = gated();
        let counted = calls.clone();
        let worker = Worker::new(move || {
            move || {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                probe()
            }
        });
        let first = worker.start();
        // A cancelled hold, queued behind the first probe.
        drop(worker.start());
        let third = worker.start();
        for _ in 0..2 {
            open.send(()).unwrap();
        }
        answered(&first);
        answered(&third);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    /// The timed check of "0 ms on key-up→pasted", against whatever has focus:
    /// `cargo test --release --lib measure_focus_probe -- --ignored --nocapture`.
    #[cfg(windows)]
    #[test]
    #[ignore = "measurement: real UI Automation on the focused app"]
    fn measure_focus_probe() {
        fn summary(mut times: Vec<Duration>) -> String {
            times.sort();
            let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
            format!("p50 {:?}, p90 {:?}, max {:?}", at(0.5), at(0.9), at(1.0))
        }
        let queued_at = Instant::now();
        let first = start();
        let queue_first = queued_at.elapsed();
        let reading = answered(&first).clone();
        println!(
            "first probe: queued in {queue_first:?}; {}",
            reading.describe()
        );
        let (mut queue, mut probe) = (Vec::new(), Vec::new());
        for _ in 0..100 {
            let queued_at = Instant::now();
            let pending = start();
            queue.push(queued_at.elapsed());
            probe.push(answered(&pending).probe);
        }
        println!("{}", answered(&start()).describe());
        println!("key-down queueing (thread up): {}", summary(queue));
        println!("UIA probe alone: {}", summary(probe));
        // The paste point: answered and unanswered probes.
        let (open, gated) = gated();
        let slow = Worker::new(move || gated);
        let unanswered = slow.start();
        for (name, pending) in [("answered", &first), ("unanswered", &unanswered)] {
            let reads = 1_000_000u32;
            let began = Instant::now();
            for _ in 0..reads {
                std::hint::black_box(std::hint::black_box(pending).read().verdict());
            }
            println!(
                "paste-point read, {name}: {:?} each",
                began.elapsed() / reads
            );
        }
        open.send(()).unwrap();
    }

    #[test]
    fn a_uia_error_is_logged_with_its_reason() {
        let worker = Worker::new(|| {
            || Facts {
                foreground: Foreground::App,
                focused: Err("E_FAIL".into()),
                app: Some("code.exe".into()),
            }
        });
        let reading = answered(&worker.start()).clone();
        assert_eq!(
            (reading.verdict, reading.rule),
            (Verdict::Unknown, Rule::Error)
        );
        assert_eq!(reading.error.as_deref(), Some("E_FAIL"));
        assert_eq!(reading.control_type, None);
    }
}
