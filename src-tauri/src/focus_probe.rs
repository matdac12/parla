//! The editable probe (PARLA-79): whether a paste has somewhere to land.
//! UI Automation is asked only when the shell has the foreground, or nothing
//! does: inside an app every answer would paste anyway (Mattia, 17/10).
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
    /// An app has the focus: never queried, it pastes as before.
    App,
    /// The desktop or a taskbar, with nothing editable focused.
    Shell,
    /// No foreground window, and the focus is on the shell or nowhere.
    NoForeground,
    Error,
}

impl Rule {
    /// Never stop a paste that would have worked: only the shell, or no
    /// window with the focus on the shell, is a confident no.
    pub fn verdict(self) -> Verdict {
        match self {
            Rule::Edit | Rule::Document | Rule::Value | Rule::Text => Verdict::Editable,
            Rule::Shell | Rule::NoForeground => Verdict::NotEditable,
            Rule::App | Rule::Error => Verdict::Unknown,
        }
    }
}

/// What the probe saw. `focus` is only read on the shell or with no
/// foreground window.
#[derive(Debug, Clone)]
pub struct Facts {
    /// The foreground window's class; None with no foreground window.
    pub foreground: Option<String>,
    /// The foreground app's executable, for the log.
    pub app: Option<String>,
    pub focus: Focus,
}

#[derive(Debug, Clone, Default)]
pub enum Focus {
    /// An app has the foreground: UI Automation is not asked.
    #[default]
    NotAsked,
    /// UI Automation found no focused element.
    Nothing,
    Element(Element),
    Error(String),
}

/// The UI Automation facts about the focused element.
#[derive(Debug, Clone, Default)]
pub struct Element {
    /// A UIA control type id, such as 50004 for Edit.
    pub control_type: i32,
    /// None without a ValuePattern.
    pub value_read_only: Option<bool>,
    pub text_pattern: bool,
    pub keyboard_focusable: bool,
    /// The class of the top-level window that holds it, when UIA has one.
    pub window_class: Option<String>,
    /// Its process's executable.
    pub process: Option<String>,
}

/// The desktop (Progman, WorkerW) and the taskbars.
const SHELL_CLASSES: [&str; 4] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
];
/// The desktop window, UIA's root: focus there is focus on nothing.
const DESKTOP_ROOT: &str = "#32769";

fn is_shell_class(class: &str) -> bool {
    SHELL_CLASSES.contains(&class)
}

impl Element {
    /// The desktop, a taskbar or the root. Without a window, explorer.exe.
    fn in_shell(&self) -> bool {
        match &self.window_class {
            Some(class) => is_shell_class(class) || class == DESKTOP_ROOT,
            None => self
                .process
                .as_deref()
                .is_some_and(|process| process.eq_ignore_ascii_case("explorer.exe")),
        }
    }

    fn editable_rule(&self) -> Option<Rule> {
        match self.control_type {
            EDIT => Some(Rule::Edit),
            DOCUMENT => Some(Rule::Document),
            _ if self.value_read_only == Some(false) => Some(Rule::Value),
            _ if self.text_pattern && self.keyboard_focusable => Some(Rule::Text),
            _ => None,
        }
    }
}

/// A probe's answer, kept for the log.
#[derive(Debug, Clone)]
pub struct Reading {
    pub rule: Rule,
    pub facts: Facts,
    /// The probe's own calls.
    pub probe: Duration,
    /// From the start of the session to the answer.
    pub answered_after: Duration,
}

/// Reads the facts, on the probe thread.
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
        let focus = match &self.facts.focus {
            Focus::NotAsked => String::new(),
            Focus::Nothing => ", focus nothing".into(),
            Focus::Element(element) => format!(
                ", focus {} in {} / {}",
                control_name(element.control_type),
                element.window_class.as_deref().unwrap_or("-"),
                element.process.as_deref().unwrap_or("-"),
            ),
            Focus::Error(error) => format!(", error {error}"),
        };
        format!(
            "{:?} by {:?}, app {}, window {}{focus}, probe {} µs, answered {} ms after key-down",
            self.verdict(),
            self.rule,
            self.facts.app.as_deref().unwrap_or("-"),
            self.facts.foreground.as_deref().unwrap_or("-"),
            self.probe.as_micros(),
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
                // Discarded before its turn (a cancelled hold): skip the probe.
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

/// The foreground window and, on the shell or with no window, UI Automation
/// (COM MTA and one cached client, both made on the first such probe). Thin on
/// purpose: `decide` holds the rules.
#[cfg(windows)]
mod uia {
    use super::{Element, Facts, Focus};
    use windows::{
        core::{Interface, Result},
        Win32::{
            Foundation::HWND,
            System::Com::{
                CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
            },
            UI::{
                Accessibility::{
                    CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationCacheRequest,
                    IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationTreeWalker,
                    IUIAutomationValuePattern, UIA_ControlTypePropertyId,
                    UIA_IsKeyboardFocusablePropertyId, UIA_NativeWindowHandlePropertyId,
                    UIA_ProcessIdPropertyId, UIA_TextPatternId, UIA_ValueIsReadOnlyPropertyId,
                    UIA_ValuePatternId,
                },
                WindowsAndMessaging::{GetAncestor, GetClassNameW, GetForegroundWindow, GA_ROOT},
            },
        },
    };

    /// A hung app fails its probe (Unknown) instead of holding up the next one.
    const TIMEOUT_MS: u32 = 1000;
    /// Ancestors climbed to find the window that holds the focused element.
    const MAX_CLIMB: usize = 16;

    /// Made on the first shell or no-window probe, never otherwise.
    pub struct Uia(Option<std::result::Result<Client, String>>);

    struct Client {
        automation: IUIAutomation,
        cache: IUIAutomationCacheRequest,
        walker: IUIAutomationTreeWalker,
    }

    impl Uia {
        pub fn new() -> Self {
            Self(None)
        }
    }

    impl super::Probe for Uia {
        fn facts(&mut self) -> Facts {
            // SAFETY: plain Win32 calls.
            let window = unsafe { GetForegroundWindow() };
            let foreground = (!window.is_invalid()).then(|| class_name(window));
            let focus = match &foreground {
                Some(class) if !super::is_shell_class(class) => Focus::NotAsked,
                // SAFETY: COM is initialised on this thread before any UIA
                // call, and the interfaces never leave it.
                _ => match self
                    .0
                    .get_or_insert_with(|| unsafe { Client::new() }.map_err(|e| e.to_string()))
                {
                    Ok(client) => unsafe { client.focus() },
                    Err(error) => Focus::Error(error.clone()),
                },
            };
            Facts {
                foreground,
                app: crate::inserter::foreground_app(),
                focus,
            }
        }
    }

    impl Client {
        unsafe fn new() -> Result<Self> {
            unsafe {
                CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
                let automation: IUIAutomation =
                    CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
                if let Ok(timeouts) = automation.cast::<IUIAutomation2>() {
                    let _ = timeouts.SetConnectionTimeout(TIMEOUT_MS);
                    let _ = timeouts.SetTransactionTimeout(TIMEOUT_MS);
                }
                // Everything the rules read, in one cross-process call.
                let cache = automation.CreateCacheRequest()?;
                for property in [
                    UIA_ControlTypePropertyId,
                    UIA_IsKeyboardFocusablePropertyId,
                    UIA_ValueIsReadOnlyPropertyId,
                    UIA_ProcessIdPropertyId,
                    UIA_NativeWindowHandlePropertyId,
                ] {
                    cache.AddProperty(property)?;
                }
                cache.AddPattern(UIA_ValuePatternId)?;
                cache.AddPattern(UIA_TextPatternId)?;
                let walker = automation.ControlViewWalker()?;
                Ok(Self {
                    automation,
                    cache,
                    walker,
                })
            }
        }

        unsafe fn focus(&self) -> Focus {
            unsafe {
                match self.automation.GetFocusedElementBuildCache(&self.cache) {
                    // A null element, not a failed call.
                    Err(error) if error.code().is_ok() => Focus::Nothing,
                    Err(error) => Focus::Error(error.to_string()),
                    Ok(element) => self
                        .element(&element)
                        .map_or_else(|error| Focus::Error(error.to_string()), Focus::Element),
                }
            }
        }

        unsafe fn element(&self, element: &IUIAutomationElement) -> Result<Element> {
            unsafe {
                // An unsupported pattern comes back null, an error here: absent.
                // A present one whose flag fails is a UIA error.
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
                    window_class: self.window_class(element),
                    process: element
                        .CachedProcessId()
                        .ok()
                        .and_then(|id| process_name(id as u32)),
                })
            }
        }

        /// The class of the top-level window holding `element`: its own
        /// window, or its nearest ancestor's.
        unsafe fn window_class(&self, element: &IUIAutomationElement) -> Option<String> {
            unsafe {
                let mut current = element.clone();
                for _ in 0..MAX_CLIMB {
                    let window = current.CachedNativeWindowHandle().ok()?;
                    if !window.is_invalid() {
                        let top = GetAncestor(window, GA_ROOT);
                        return Some(class_name(if top.is_invalid() { window } else { top }));
                    }
                    current = self
                        .walker
                        .GetParentElementBuildCache(&current, &self.cache)
                        .ok()?;
                }
                None
            }
        }
    }

    fn class_name(window: HWND) -> String {
        let mut class = [0u16; 64];
        // SAFETY: the buffer outlives the call.
        let length = unsafe { GetClassNameW(window, &mut class) };
        String::from_utf16_lossy(&class[..length.max(0) as usize])
    }

    /// The executable of process `id`, such as `explorer.exe`.
    fn process_name(id: u32) -> Option<String> {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        // SAFETY: the out pointers are valid for the given sizes, and the
        // handle is closed once the name is read.
        let named = unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, id);
            if process.is_null() {
                return None;
            }
            let named = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                path.as_mut_ptr(),
                &mut length,
            );
            CloseHandle(process);
            named != 0
        };
        let path = String::from_utf16_lossy(&path[..length as usize]);
        named.then(|| path.rsplit('\\').next().unwrap_or(&path).to_owned())
    }
}

/// Off Windows, where Parla does not run, every probe is Unknown: editable.
#[cfg(not(windows))]
mod uia {
    use super::{Facts, Focus};

    pub struct Uia;

    impl Uia {
        pub fn new() -> Self {
            Self
        }
    }

    impl super::Probe for Uia {
        fn facts(&mut self) -> Facts {
            Facts {
                foreground: None,
                app: None,
                focus: Focus::Error("no UI Automation on this platform".into()),
            }
        }
    }
}

/// UIA control type ids (UIAutomationClient.h), for every platform.
const EDIT: i32 = 50004;
const DOCUMENT: i32 = 50030;

/// An app in the foreground pastes as before, unqueried. On the shell or with
/// no foreground window, an editable element still wins (a desktop rename);
/// otherwise the shell is a confident no, and so is no window with the focus
/// on the shell or nowhere.
pub fn decide(facts: &Facts) -> Rule {
    let on_shell = match facts.foreground.as_deref() {
        Some(class) if !is_shell_class(class) => return Rule::App,
        Some(_) => true,
        None => false,
    };
    if let Focus::Element(element) = &facts.focus {
        if let Some(rule) = element.editable_rule() {
            return rule;
        }
    }
    match &facts.focus {
        _ if on_shell => Rule::Shell,
        Focus::Element(element) if !element.in_shell() => Rule::App,
        Focus::Error(_) => Rule::Error,
        Focus::NotAsked | Focus::Nothing | Focus::Element(_) => Rule::NoForeground,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANE: i32 = 50033;
    const LIST_ITEM: i32 = 50007;

    fn control(control_type: i32) -> Element {
        Element {
            control_type,
            ..Element::default()
        }
    }
    fn facts(foreground: Option<&str>, focus: Focus) -> Facts {
        Facts {
            foreground: foreground.map(Into::into),
            app: None,
            focus,
        }
    }
    fn judged(facts: &Facts) -> (Verdict, Rule) {
        let rule = decide(facts);
        (rule.verdict(), rule)
    }
    /// The editable rules, each on its own.
    fn editable_elements() -> [(Element, Rule); 4] {
        [
            (control(EDIT), Rule::Edit),
            (control(DOCUMENT), Rule::Document),
            (
                Element {
                    value_read_only: Some(false),
                    ..control(PANE)
                },
                Rule::Value,
            ),
            (
                Element {
                    text_pattern: true,
                    keyboard_focusable: true,
                    ..control(PANE)
                },
                Rule::Text,
            ),
        ]
    }

    #[test]
    fn an_app_in_the_foreground_is_unknown_and_never_queried() {
        // WhatsApp's message box read as a bare Pane (hand check, 17/10).
        for app in [
            "Notepad",
            "Chrome_WidgetWin_1",
            "WinUIDesktopWin32WindowClass",
        ] {
            assert_eq!(
                judged(&facts(Some(app), Focus::NotAsked)),
                (Verdict::Unknown, Rule::App)
            );
        }
    }

    #[test]
    fn on_the_shell_an_editable_element_wins_and_anything_else_is_not_editable() {
        for shell in SHELL_CLASSES {
            for (element, rule) in editable_elements() {
                // Renaming a file on the desktop, the taskbar's search box.
                assert_eq!(
                    judged(&facts(Some(shell), Focus::Element(element))),
                    (Verdict::Editable, rule)
                );
            }
            for focus in [
                Focus::Element(control(LIST_ITEM)),
                Focus::Element(Element {
                    value_read_only: Some(true),
                    ..control(PANE)
                }),
                Focus::Element(Element {
                    text_pattern: true,
                    ..control(PANE)
                }),
                Focus::Nothing,
                // The window class is the confident signal.
                Focus::Error("UIA timed out".into()),
            ] {
                assert_eq!(
                    judged(&facts(Some(shell), focus)),
                    (Verdict::NotEditable, Rule::Shell)
                );
            }
        }
    }

    #[test]
    fn with_no_foreground_window_focus_on_the_shell_or_nowhere_is_not_editable() {
        let on = |window_class: Option<&str>, process: Option<&str>| {
            Focus::Element(Element {
                window_class: window_class.map(Into::into),
                process: process.map(Into::into),
                ..control(LIST_ITEM)
            })
        };
        for focus in [
            on(Some("Progman"), Some("explorer.exe")),
            on(Some("Shell_TrayWnd"), Some("explorer.exe")),
            on(Some(DESKTOP_ROOT), None),
            on(None, Some("Explorer.EXE")),
            Focus::Nothing,
        ] {
            assert_eq!(
                judged(&facts(None, focus)),
                (Verdict::NotEditable, Rule::NoForeground)
            );
        }
    }

    #[test]
    fn with_no_foreground_window_focus_in_an_app_is_unknown() {
        // Mid Alt+Tab, or just after a window closed.
        for (window_class, process) in [
            (Some("Chrome_WidgetWin_1"), Some("WhatsApp.Root.exe")),
            // File Explorer is explorer.exe, but its window is no shell.
            (Some("CabinetWClass"), Some("explorer.exe")),
            (None, Some("Notepad.exe")),
            (None, None),
        ] {
            let focus = Focus::Element(Element {
                window_class: window_class.map(Into::into),
                process: process.map(Into::into),
                ..control(PANE)
            });
            assert_eq!(judged(&facts(None, focus)), (Verdict::Unknown, Rule::App));
        }
        // An editable element anywhere wins.
        for (element, rule) in editable_elements() {
            assert_eq!(
                judged(&facts(None, Focus::Element(element))),
                (Verdict::Editable, rule)
            );
        }
    }

    #[test]
    fn with_no_foreground_window_a_uia_error_is_unknown() {
        assert_eq!(
            judged(&facts(None, Focus::Error("E_FAIL".into()))),
            (Verdict::Unknown, Rule::Error)
        );
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

    /// A probe that answers the desktop once the test opens its gate.
    fn gated() -> (mpsc::Sender<()>, impl FnMut() -> Facts + Send) {
        let (open, gate) = mpsc::channel();
        let probe = move || {
            let _ = gate.recv();
            Facts {
                app: Some("explorer.exe".into()),
                ..facts(Some("Progman"), Focus::Element(control(LIST_ITEM)))
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
        assert_eq!(reading.rule, Rule::Shell);
        assert_eq!(reading.facts.app.as_deref(), Some("explorer.exe"));
        assert!(reading.answered_after >= reading.probe);
        // The next paste sees the answer; with the Safety Net off, Unknown.
        around_paste(Some(&pending), "dictation", |verdict| {
            assert_eq!(verdict, Verdict::NotEditable)
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
            || facts(Some("Notepad"), Focus::NotAsked)
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

    /// The timed check, against whatever has the foreground (an app: no UIA):
    /// `cargo test --release --lib measure_focus_probe -- --ignored --nocapture`.
    #[cfg(windows)]
    #[test]
    #[ignore = "measurement: the real probe on the foreground app"]
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
        println!("probe alone: {}", summary(probe));
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
