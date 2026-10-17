//! The editable probe (PARLA-79): whether the focused element accepts text.
use std::{
    sync::{mpsc, Arc, LazyLock, OnceLock},
    time::{Duration, Instant},
};

/// What the probe concluded. Unknown counts as editable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Editable,
    NotEditable,
    Unknown,
}

/// Which rule decided, and so the verdict.
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

impl Rule {
    pub fn verdict(self) -> Verdict {
        match self {
            Rule::Edit | Rule::Document | Rule::Value | Rule::Text => Verdict::Editable,
            Rule::NoForeground | Rule::Shell | Rule::NoMatch => Verdict::NotEditable,
            Rule::Error => Verdict::Unknown,
        }
    }
}

/// The foreground window, as far as the verdict cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreground {
    NoWindow,
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

#[derive(Debug, Clone)]
pub struct Facts {
    pub foreground: Foreground,
    /// The UIA error when the focused element could not be read.
    pub focused: Result<Element, String>,
    /// The foreground app's executable, for the log.
    pub app: Option<String>,
}

/// A probe's answer, kept for the log.
#[derive(Debug, Clone)]
pub struct Reading {
    pub rule: Rule,
    pub facts: Facts,
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

pub(crate) struct Slot {
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
}

/// The paste point, for dictations and command replies: reads the verdict
/// with one atomic load (Unknown while the probe runs, or with the Safety Net
/// off), pastes with it, then logs the run, off key-up→pasted.
pub fn around_paste<R>(focus: Option<&Pending>, run: &str, paste: impl FnOnce(Verdict) -> R) -> R {
    let Some(pending) = focus else {
        return paste(Verdict::Unknown);
    };
    let seen = pending.answer();
    let pasted = paste(seen.map_or(Verdict::Unknown, Reading::verdict));
    let line = match (seen, pending.answer()) {
        (Some(reading), _) => reading.describe(),
        (None, Some(late)) => format!(
            "Unknown, no answer at the paste; later: {}",
            late.describe()
        ),
        (None, None) => format!(
            "Unknown, no answer at the paste (still probing {} ms after key-down)",
            pending.0.started.elapsed().as_millis()
        ),
    };
    eprintln!("Parla focus probe ({run}): {line}");
    pasted
}

impl Reading {
    pub fn verdict(&self) -> Verdict {
        self.rule.verdict()
    }

    fn describe(&self) -> String {
        let (control, error) = match &self.facts.focused {
            Ok(element) => (control_name(element.control_type), String::new()),
            Err(error) => ("-".into(), format!(", error {error}")),
        };
        format!(
            "{:?} by {:?}, app {}, control {control}, probe {} ms, answered {} ms after key-down{error}",
            self.verdict(),
            self.rule,
            self.facts.app.as_deref().unwrap_or("-"),
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

/// The probe thread's queue; None if the thread could not start.
type Jobs = Option<mpsc::Sender<Arc<Slot>>>;

/// The probe thread, created by the first `start`, never at startup. One
/// thread serves every session in turn.
pub(crate) struct Worker<F = fn() -> Jobs>(LazyLock<Jobs, F>);

impl<F: FnOnce() -> Jobs> Worker<F> {
    /// Queues a probe and returns at once. Without the thread the answer
    /// never comes: Unknown.
    pub(crate) fn start(&self) -> Pending {
        let pending = Pending::new();
        if let Some(jobs) = &*self.0 {
            let _ = jobs.send(pending.0.clone());
        }
        pending
    }
}

/// A worker whose thread builds its probe with `make`, for tests' fakes.
#[cfg(test)]
pub(crate) fn worker<P: Probe>(
    make: impl FnOnce() -> P + Send + 'static,
) -> Worker<impl FnOnce() -> Jobs> {
    Worker(LazyLock::new(move || spawn(make)))
}

fn spawn<P: Probe>(make: impl FnOnce() -> P + Send + 'static) -> Jobs {
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
                let _ = slot.answer.set(Reading {
                    rule: decide(&facts),
                    facts,
                    probe: began.elapsed(),
                    answered_after: slot.started.elapsed(),
                });
            }
        })
        .map_err(|error| eprintln!("Cannot start the focus probe: {error}"))
        .ok()?;
    Some(jobs)
}

static WORKER: Worker = Worker(LazyLock::new(|| spawn(uia::Uia::new)));

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
            Self(unsafe { client() }.map_err(|error| error.to_string()))
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
                (_, Foreground::NoWindow) => Err("no foreground window".into()),
                (Err(error), _) => Err(error.clone()),
                // SAFETY: the client was made on this thread.
                (Ok((automation, cache)), _) => {
                    unsafe { focused(automation, cache) }.map_err(|error| error.to_string())
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
            // A present one whose flag fails is a UIA error, not a no.
            let value_read_only = element
                .GetCachedPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()
                .map(|value| {
                    value
                        .CachedIsReadOnly()
                        .map(|read_only| read_only.as_bool())
                })
                .transpose()?;
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
            return Foreground::NoWindow;
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
pub fn decide(facts: &Facts) -> Rule {
    if facts.foreground == Foreground::NoWindow {
        return Rule::NoForeground;
    }
    if let Ok(element) = &facts.focused {
        match element.control_type {
            EDIT => return Rule::Edit,
            DOCUMENT => return Rule::Document,
            _ if element.value_read_only == Some(false) => return Rule::Value,
            _ if element.text_pattern && element.keyboard_focusable => return Rule::Text,
            _ => {}
        }
    }
    match (facts.foreground, &facts.focused) {
        (Foreground::Shell, _) => Rule::Shell,
        (_, Err(_)) => Rule::Error,
        (_, Ok(_)) => Rule::NoMatch,
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
    fn judged(facts: &Facts) -> (Verdict, Rule) {
        let rule = decide(facts);
        (rule.verdict(), rule)
    }

    #[test]
    fn edit_and_document_controls_are_editable() {
        assert_eq!(judged(&app(control(EDIT))), (Verdict::Editable, Rule::Edit));
        assert_eq!(
            judged(&app(control(DOCUMENT))),
            (Verdict::Editable, Rule::Document)
        );
    }

    #[test]
    fn a_value_pattern_is_editable_unless_it_is_read_only() {
        let value = |read_only| Element {
            value_read_only: Some(read_only),
            ..control(PANE)
        };
        assert_eq!(judged(&app(value(false))), (Verdict::Editable, Rule::Value));
        assert_eq!(
            judged(&app(value(true))),
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
        assert_eq!(judged(&app(text(true))), (Verdict::Editable, Rule::Text));
        assert_eq!(
            judged(&app(text(false))),
            (Verdict::NotEditable, Rule::NoMatch)
        );
        // Focusable alone is not text: a button or a list item.
        let focusable = Element {
            keyboard_focusable: true,
            ..control(LIST_ITEM)
        };
        assert_eq!(
            judged(&app(focusable)),
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
            judged(&desktop(Ok(control(LIST_ITEM)))),
            (Verdict::NotEditable, Rule::Shell)
        );
        assert_eq!(
            judged(&desktop(Err("UIA timed out".into()))),
            (Verdict::NotEditable, Rule::Shell)
        );
        // Renaming a file on the desktop.
        assert_eq!(
            judged(&desktop(Ok(control(EDIT)))),
            (Verdict::Editable, Rule::Edit)
        );
    }

    #[test]
    fn no_foreground_window_is_not_editable() {
        let facts = Facts {
            foreground: Foreground::NoWindow,
            focused: Err("no element".into()),
            app: None,
        };
        assert_eq!(judged(&facts), (Verdict::NotEditable, Rule::NoForeground));
    }

    #[test]
    fn a_uia_error_in_an_app_is_unknown() {
        let facts = Facts {
            foreground: Foreground::App,
            focused: Err("E_FAIL".into()),
            app: None,
        };
        assert_eq!(judged(&facts), (Verdict::Unknown, Rule::Error));
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
    fn the_paste_point_never_waits_for_a_slow_probe_and_sees_unknown() {
        let (open, probe) = gated();
        let worker = worker(move || probe);
        let pending = worker.start();
        let read_at = Instant::now();
        let mut seen = Vec::new();
        let pasted = around_paste(Some(&pending), "dictation", |verdict| {
            seen.push(verdict);
            "pasted"
        });
        assert!(read_at.elapsed() < Duration::from_millis(50));
        // No answer yet: Unknown, which pastes as today.
        assert_eq!(
            (pasted, seen.as_slice()),
            ("pasted", [Verdict::Unknown].as_slice())
        );
        open.send(()).unwrap();
        let reading = answered(&pending);
        assert_eq!(reading.rule, Rule::Edit);
        assert_eq!(reading.facts.app.as_deref(), Some("notepad.exe"));
        assert!(reading.answered_after >= reading.probe);
        // The next paste sees the answer; with the Safety Net off, Unknown.
        around_paste(Some(&pending), "dictation", |verdict| {
            assert_eq!(verdict, Verdict::Editable)
        });
        around_paste(None, "dictation", |verdict| {
            assert_eq!(verdict, Verdict::Unknown)
        });
    }

    #[test]
    fn the_probe_thread_starts_with_the_first_session() {
        let (made, was_made) = mpsc::channel();
        let worker = worker(move || {
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
        let worker = worker(move || {
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

    #[test]
    fn a_uia_error_is_kept_with_its_reason() {
        let worker = worker(|| {
            || Facts {
                foreground: Foreground::App,
                focused: Err("E_FAIL".into()),
                app: Some("code.exe".into()),
            }
        });
        let reading = answered(&worker.start()).clone();
        assert_eq!(reading.rule, Rule::Error);
        assert_eq!(reading.facts.focused.unwrap_err(), "E_FAIL");
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
        // What around_paste does before the paste, answered and unanswered.
        let (open, gated) = gated();
        let slow = worker(move || gated);
        let unanswered = slow.start();
        for (name, pending) in [("answered", &first), ("unanswered", &unanswered)] {
            let reads = 1_000_000u32;
            let began = Instant::now();
            for _ in 0..reads {
                let seen = std::hint::black_box(pending).answer();
                std::hint::black_box(seen.map_or(Verdict::Unknown, Reading::verdict));
            }
            println!(
                "paste-point read, {name}: {:?} each",
                began.elapsed() / reads
            );
        }
        open.send(()).unwrap();
    }
}
