//! The small always-on-top pill: live level meter while recording, a spinner
//! while transcribing, and a brief done/error confirmation, with a label
//! beside the check when there is something to say ("Opened <host>"). The
//! Card (PARLA-80) opens above the check, in the same window.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use tauri::{Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "overlay";
const WIDTH: f64 = 144.0;
const HEIGHT: f64 = 40.0;
const CAPTION_WIDTH: f64 = 366.0;
const CAPTION_HEIGHT: f64 = 116.0;
/// Room for a label such as "Opened <host>" beside the check (PARLA-65).
const LABELLED_WIDTH: f64 = 248.0;
/// The Card at its tallest (12 lines of text) above the pill, with room for
/// its shadow. A shorter Card leaves the top empty, and click-through.
const CARD_WIDTH: f64 = 456.0;
const CARD_HEIGHT: f64 = 380.0;
const SHOW_DELAY: Duration = Duration::from_millis(250);
const DONE_HOLD: Duration = Duration::from_millis(750);
/// Long enough to read the label.
const LABELLED_HOLD: Duration = Duration::from_millis(1500);
/// A failed dictation keeps its pill (Dismiss and Retry buttons) up for a while.
const ERROR_HOLD: Duration = Duration::from_secs(10);

/// Creates the hidden, non-focusable, click-through overlay window.
pub fn setup(app: &tauri::App) -> tauri::Result<()> {
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("overlay.html".into()))
        .title("Parla Recording")
        .inner_size(WIDTH, HEIGHT)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .focusable(false)
        .visible(false)
        .build()?;
    window.set_ignore_cursor_events(true)?;
    // See `hide`: an invisible window alone does not stop WebView2 rendering.
    let _ = window.as_ref().hide();
    set_memory_target(&window, true);
    Ok(())
}

/// While the pill is hidden, WebView2 may trim the overlay's caches and heap
/// (it keeps running, so events still arrive). Restored before every reveal.
#[cfg(windows)]
fn set_memory_target(window: &tauri::WebviewWindow, low: bool) {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
        COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
    };
    use windows_core::Interface;
    let _ = window.with_webview(move |webview| {
        let level = if low {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
        } else {
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
        };
        // SAFETY: runs on the UI thread that owns this live controller. Older
        // runtimes without ICoreWebView2_19 are left at their default.
        unsafe {
            if let Ok(core) = webview.controller().CoreWebView2() {
                if let Ok(core) = core.cast::<ICoreWebView2_19>() {
                    let _ = core.SetMemoryUsageTargetLevel(level);
                }
            }
        }
    });
}

#[cfg(not(windows))]
fn set_memory_target(_window: &tauri::WebviewWindow, _low: bool) {}

/// What the pill is recording: a held or locked dictation, or a command run
/// (orange, so it never looks like dictation). Kept until the next session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PillMode {
    Hold,
    Locked,
    Command,
}

pub fn set_mode(app: &tauri::AppHandle, mode: PillMode) {
    let _ = app.emit_to(LABEL, "mode", mode);
}

/// Puts the window above the taskbar on the primary monitor. Returns where
/// (physical pixels) and the monitor's scale.
fn place(app: &tauri::AppHandle) -> Option<(PhysicalPosition<i32>, f64)> {
    let window = app.get_webview_window(LABEL)?;
    let monitor = window.primary_monitor().ok()??;
    let scale = monitor.scale_factor();
    let size = monitor.size();
    let origin = monitor.position();
    let (width, height) = Size::current().dimensions();
    let x = origin.x + ((f64::from(size.width) - width * scale) / 2.0) as i32;
    let y = origin.y + (f64::from(size.height) - (height + 72.0) * scale) as i32;
    let at = PhysicalPosition::new(x, y);
    let _ = window.set_position(at);
    Some((at, scale))
}

/// The overlay window's size: the pill alone, under live captions, with a
/// result label, or under the Card.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Size {
    Pill,
    Captions,
    Labelled,
    Card,
}
/// The window's size now.
static SIZE: AtomicU8 = AtomicU8::new(Size::Pill as u8);
/// The session's size: a label widens the window only until the pill hides,
/// so the next key-down finds it at the right size.
static SESSION_SIZE: AtomicU8 = AtomicU8::new(Size::Pill as u8);

impl Size {
    fn load(cell: &AtomicU8) -> Self {
        match cell.load(Ordering::Acquire) {
            size if size == Size::Captions as u8 => Size::Captions,
            size if size == Size::Labelled as u8 => Size::Labelled,
            size if size == Size::Card as u8 => Size::Card,
            _ => Size::Pill,
        }
    }

    fn current() -> Self {
        Self::load(&SIZE)
    }

    fn dimensions(self) -> (f64, f64) {
        match self {
            Size::Pill => (WIDTH, HEIGHT),
            Size::Captions => (CAPTION_WIDTH, CAPTION_HEIGHT),
            Size::Labelled => (LABELLED_WIDTH, HEIGHT),
            Size::Card => (CARD_WIDTH, CARD_HEIGHT),
        }
    }

    /// Resizes the window only when the size changes.
    fn apply(self, app: &tauri::AppHandle) {
        if SIZE.swap(self as u8, Ordering::AcqRel) != self as u8 {
            if let Some(window) = app.get_webview_window(LABEL) {
                let (width, height) = self.dimensions();
                let _ = window.set_size(LogicalSize::new(width, height));
            }
        }
    }
}

fn configure(app: &tauri::AppHandle, captions: bool, generation: u64) {
    let size = if captions { Size::Captions } else { Size::Pill };
    SESSION_SIZE.store(size as u8, Ordering::Release);
    size.apply(app);
    let _ = app.emit_to(
        LABEL,
        "caption-session",
        serde_json::json!({"generation":generation,"enabled":captions}),
    );
}

/// Bumped by every `begin`, so a delayed hide never hides a newer session.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// The generation whose failed pill (cross and Retry) is on screen, 0 if none.
static FAILED: AtomicU64 = AtomicU64::new(0);

/// A dismissal only applies to the failed pill it was clicked on.
fn dismissable(current: u64, failed: u64) -> bool {
    failed != 0 && current == failed
}

/// Dismiss (the cross on a failed pill): hides it before `ERROR_HOLD` runs out.
/// A click that lands after a new dictation began is ignored.
pub fn dismiss(app: &tauri::AppHandle) {
    let current = GENERATION.load(Ordering::Acquire);
    if dismissable(current, FAILED.load(Ordering::Acquire))
        && FAILED
            .compare_exchange(current, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        hide(app);
    }
}

/// How a session's pill settles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    /// The check.
    Done,
    /// The cross and Dismiss; the Retry button only when `retry` (PARLA-51).
    Failed { retry: bool },
    /// The check, under the Card (PARLA-80).
    Card(crate::card::Card),
}

/// One dictation's pill: recording (live meter) -> processing (spinner) -> a
/// brief done/error confirmation. Dropping it without `finish` hides the pill
/// at once (cancelled dictation).
pub struct Overlay {
    app: tauri::AppHandle,
    generation: u64,
    metering: Arc<AtomicBool>,
    settled: bool,
    captions_live: Arc<AtomicBool>,
}

/// Starts a session. The pill appears once the keys have been held for a
/// short delay (so brief or cancelled holds never flash it) and streams
/// microphone levels. `held` is how long they were already down.
pub fn begin(app: &tauri::AppHandle, captions: bool, held: Duration) -> Overlay {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    close_card(app, "closed by the next session");
    let metering = Arc::new(AtomicBool::new(true));
    // Back to normal well before the delayed reveal.
    if let Some(window) = app.get_webview_window(LABEL) {
        set_memory_target(&window, false);
    }
    configure(app, captions, generation);
    set_interactive(app, false);
    let _ = app.emit_to(LABEL, "phase", "recording");
    let flag = metering.clone();
    let handle = app.clone();
    let _ = std::thread::Builder::new()
        .name("parla-meter".into())
        .spawn(move || {
            let live =
                || flag.load(Ordering::Acquire) && GENERATION.load(Ordering::Acquire) == generation;
            let step = Duration::from_millis(25);
            for _ in 0..(SHOW_DELAY.saturating_sub(held).as_millis() / step.as_millis()) {
                if !live() {
                    return;
                }
                std::thread::sleep(step);
            }
            if !live() {
                return;
            }
            reveal(&handle);
            while live() {
                let _ = handle.emit_to(LABEL, "level", crate::recorder::current_level());
                std::thread::sleep(Duration::from_millis(40));
            }
        });
    Overlay {
        app: app.clone(),
        generation,
        metering,
        settled: false,
        captions_live: Arc::new(AtomicBool::new(captions)),
    }
}

/// The pill is click-through except while it offers Dismiss and Retry.
fn set_interactive(app: &tauri::AppHandle, interactive: bool) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.set_ignore_cursor_events(!interactive);
    }
}

/// Starts a session already in the transcribing phase (used by Retry).
pub fn begin_processing(app: &tauri::AppHandle) -> Overlay {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    close_card(app, "closed by a retry");
    set_interactive(app, false);
    // A retry is a dictation, even right after an orange command pill.
    set_mode(app, PillMode::Hold);
    configure(app, false, generation);
    let overlay = Overlay {
        app: app.clone(),
        generation,
        metering: Arc::new(AtomicBool::new(false)),
        settled: false,
        captions_live: Arc::new(AtomicBool::new(false)),
    };
    overlay.processing();
    overlay
}

fn reveal(app: &tauri::AppHandle) {
    let _ = place(app);
    if let Some(window) = app.get_webview_window(LABEL) {
        // Usually already normal since `begin`; Retry and a racing delayed
        // `hide` reach here without it.
        set_memory_target(&window, false);
        // `focusable(false)` keeps the paste target focused.
        let _ = window.show();
        // Resume rendering; see `hide`.
        let _ = window.as_ref().show();
    }
}

fn hide(app: &tauri::AppHandle) {
    set_interactive(app, false);
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.hide();
        // Hiding the window leaves WebView2 "visible": it keeps compositing at
        // 60 fps (~15% of a core) while idle in the tray. Hide the webview too.
        let _ = window.as_ref().hide();
        set_memory_target(&window, true);
    }
    // Off the key-down path: undoes a label's widening while hidden.
    Size::load(&SESSION_SIZE).apply(app);
}

/// The Card on screen, and the session it belongs to.
struct ShownCard {
    generation: u64,
    card: crate::card::Card,
    /// The window's position and scale, when it could be placed. Read by the
    /// Windows cursor thread only, like `bounds`.
    #[cfg_attr(not(windows), allow(dead_code))]
    placed: Option<(PhysicalPosition<i32>, f64)>,
    /// The Card's box in the window, once the page has laid it out.
    #[cfg_attr(not(windows), allow(dead_code))]
    bounds: Option<Bounds>,
}

/// A box in the overlay page, in CSS pixels from the window's top left.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
pub struct Bounds {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Bounds {
    /// Whether the physical point `cursor` is inside, for a window at `origin`
    /// with this `scale`.
    fn contains(&self, cursor: (i32, i32), origin: PhysicalPosition<i32>, scale: f64) -> bool {
        let x = f64::from(cursor.0 - origin.x) / scale;
        let y = f64::from(cursor.1 - origin.y) / scale;
        x >= self.left && x < self.left + self.width && y >= self.top && y < self.top + self.height
    }
}

fn shown_card() -> MutexGuard<'static, Option<ShownCard>> {
    static SHOWN: Mutex<Option<ShownCard>> = Mutex::new(None);
    SHOWN.lock().unwrap_or_else(|e| e.into_inner())
}

/// The Card's own box, from the page once it has laid it out.
pub fn set_card_bounds(generation: u64, bounds: Bounds) {
    if let Some(shown) = shown_card().as_mut().filter(|s| s.generation == generation) {
        shown.bounds = Some(bounds);
    }
}

/// A button's click: the Card of `generation`, taken once and closed with the
/// window. None if it is gone: clicked twice, or a newer session closed it.
pub fn take_card(app: &tauri::AppHandle, generation: u64) -> Option<crate::card::Card> {
    let card = {
        let mut shown = shown_card();
        shown.as_ref().filter(|s| s.generation == generation)?;
        shown.take()?.card
    };
    let _ = app.emit_to(LABEL, "card", serde_json::Value::Null);
    if GENERATION.load(Ordering::Acquire) == generation {
        hide(app);
    }
    Some(card)
}

/// A new session closes the Card, and hides the window until its own pill
/// is placed.
fn close_card(app: &tauri::AppHandle, why: &str) {
    if shown_card().take().is_some() {
        crate::card::log(why);
        let _ = app.emit_to(LABEL, "card", serde_json::Value::Null);
        hide(app);
    }
}

/// The window takes clicks only while the cursor is over the Card, so the
/// empty space around it never blocks the app underneath. Polls on its own
/// thread while the Card is open.
#[cfg(windows)]
fn follow_cursor(app: &tauri::AppHandle, generation: u64) {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
    let app = app.clone();
    let _ = std::thread::Builder::new()
        .name("parla-card-cursor".into())
        .spawn(move || {
            let mut over = None;
            loop {
                std::thread::sleep(Duration::from_millis(30));
                let mut cursor = POINT { x: 0, y: 0 };
                // SAFETY: a valid out pointer.
                let read = unsafe { GetCursorPos(&mut cursor) } != 0;
                // Held while switching, so a closed Card is never made
                // clickable. The main thread never takes it (`card_bounds`
                // is async), so a setter that waits on it cannot deadlock.
                let shown = shown_card();
                let Some(shown) = shown.as_ref().filter(|s| s.generation == generation) else {
                    return;
                };
                let inside = read
                    && match (shown.placed, shown.bounds) {
                        (Some((origin, scale)), Some(bounds)) => {
                            bounds.contains((cursor.x, cursor.y), origin, scale)
                        }
                        _ => false,
                    };
                if over != Some(inside) {
                    over = Some(inside);
                    set_interactive(&app, inside);
                }
            }
        });
}

/// Off Windows, where Parla does not run, the whole window takes clicks.
#[cfg(not(windows))]
fn follow_cursor(app: &tauri::AppHandle, _generation: u64) {
    set_interactive(app, true);
}

impl Overlay {
    pub fn caption_sink(
        &self,
        enabled: bool,
    ) -> impl FnMut(crate::streaming::Caption) + Send + 'static {
        let app = self.app.clone();
        let generation = self.generation;
        let live = self.captions_live.clone();
        move |caption| {
            if enabled
                && live.load(Ordering::Acquire)
                && GENERATION.load(Ordering::Acquire) == generation
            {
                let _ = app.emit_to(LABEL, "caption", serde_json::json!({
                    "generation":generation, "stable":caption.stable, "provisional":caption.provisional
                }));
            }
        }
    }

    fn clear_captions(&self) {
        self.captions_live.store(false, Ordering::Release);
        let _ = self.app.emit_to(
            LABEL,
            "caption",
            serde_json::json!({"generation":self.generation,"stable":"","provisional":""}),
        );
    }
    /// Recording ended; the clip is being transcribed. Shows a spinner.
    pub fn processing(&self) {
        self.metering.store(false, Ordering::Release);
        reveal(&self.app);
        let _ = self.app.emit_to(LABEL, "phase", "processing");
    }

    /// Shows a short confirmation (check on success, cross on failure), then
    /// hides the pill.
    pub fn finish(self, ending: Ending) {
        let hold = match ending {
            Ending::Done => DONE_HOLD,
            Ending::Failed { .. } => ERROR_HOLD,
            Ending::Card(card) => return self.show_card(card),
        };
        self.settle(ending, hold);
    }

    /// The check, with the Card above it until a button closes it or the
    /// next session starts: no timer.
    fn show_card(mut self, card: crate::card::Card) {
        self.clear_captions();
        self.settled = true;
        self.metering.store(false, Ordering::Release);
        let generation = self.generation;
        Size::Card.apply(&self.app);
        let placed = place(&self.app);
        let view = serde_json::json!({"generation": generation, "card": &card});
        // Before the page lays it out, so its box is never dropped.
        *shown_card() = Some(ShownCard {
            generation,
            card,
            placed,
            bounds: None,
        });
        let _ = self.app.emit_to(LABEL, "card", view);
        reveal(&self.app);
        let _ = self.app.emit_to(LABEL, "phase", "done");
        follow_cursor(&self.app, generation);
    }

    /// The check with `label` beside it, held a little longer to be read.
    pub fn finish_labelled(self, label: &str) {
        Size::Labelled.apply(&self.app);
        let _ = self.app.emit_to(LABEL, "label", label);
        self.settle(Ending::Done, LABELLED_HOLD);
    }

    fn settle(mut self, ending: Ending, hold: Duration) {
        let ok = ending == Ending::Done;
        if let Ending::Failed { retry } = ending {
            // Before the phase, like the label: the cross renders with or
            // without its Retry button.
            let _ = self.app.emit_to(LABEL, "retry", retry);
        }
        self.clear_captions();
        self.settled = true;
        self.metering.store(false, Ordering::Release);
        reveal(&self.app);
        if !ok {
            FAILED.store(self.generation, Ordering::Release);
        }
        set_interactive(&self.app, !ok);
        let _ = self
            .app
            .emit_to(LABEL, "phase", if ok { "done" } else { "error" });
        let app = self.app.clone();
        let generation = self.generation;
        let _ = std::thread::Builder::new()
            .name("parla-overlay-hide".into())
            .spawn(move || {
                std::thread::sleep(hold);
                if GENERATION.load(Ordering::Acquire) == generation {
                    hide(&app);
                }
            });
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.clear_captions();
        self.metering.store(false, Ordering::Release);
        if !self.settled && GENERATION.load(Ordering::Acquire) == self.generation {
            hide(&self.app);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{dismissable, Bounds};
    use tauri::PhysicalPosition;

    #[test]
    fn only_the_cards_own_box_takes_clicks() {
        // At 150 %, a 420 × 100 Card 18 px in and 250 px down the window.
        let card = Bounds {
            left: 18.0,
            top: 250.0,
            width: 420.0,
            height: 100.0,
        };
        let window = PhysicalPosition::new(1000, 2000);
        let at = |x: f64, y: f64| card.contains((1000 + x as i32, 2000 + y as i32), window, 1.5);
        // Its corners, in physical pixels.
        assert!(at(27.0, 375.0));
        assert!(at(656.0, 524.0));
        // Beside it, above it (empty window) and below it (the pill).
        assert!(!at(26.0, 400.0));
        assert!(!at(658.0, 400.0));
        assert!(!at(300.0, 373.0));
        assert!(!at(300.0, 526.0));
    }

    #[test]
    fn dismiss_only_hides_the_failed_pill_it_was_clicked_on() {
        assert!(dismissable(4, 4));
        // No failed pill, or a newer dictation already took the overlay.
        assert!(!dismissable(4, 0));
        assert!(!dismissable(5, 4));
    }
}

#[cfg(all(test, windows, feature = "native-smoke"))]
mod native_tests {
    use super::*;
    use std::sync::Mutex;

    static RETRIES: Mutex<u32> = Mutex::new(0);

    /// Stands in for the real command, which would transcribe and paste.
    #[tauri::command]
    fn retry_last_failed() {
        *RETRIES.lock().unwrap() += 1;
    }
    #[tauri::command]
    fn dismiss_failure_pill(app: tauri::AppHandle) {
        dismiss(&app);
    }
    #[tauri::command]
    fn report_point(x: f64, y: f64) {
        *POINT.lock().unwrap() = Some((x, y));
    }
    static POINT: Mutex<Option<(f64, f64)>> = Mutex::new(None);

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> isize;
        fn IsWindowVisible(hwnd: isize) -> i32;
        fn SetCursorPos(x: i32, y: i32) -> i32;
        fn GetCursorPos(point: *mut Point) -> i32;
        fn mouse_event(flags: u32, dx: i32, dy: i32, data: i32, extra: usize);
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Real left click at the centre of `selector` in the overlay.
    fn click(window: &tauri::WebviewWindow, selector: &str) {
        *POINT.lock().unwrap() = None;
        window
            .eval(format!(
                "{{const r=document.querySelector('{selector}').getBoundingClientRect();\
                 window.__TAURI_INTERNALS__.invoke('report_point',{{x:r.x+r.width/2,y:r.y+r.height/2}});}}"
            ))
            .unwrap();
        wait_until("button position never reported", || {
            POINT.lock().unwrap().is_some()
        });
        let (x, y) = POINT.lock().unwrap().unwrap();
        let scale = window.scale_factor().unwrap();
        let origin = window.inner_position().unwrap();
        let mut saved = Point::default();
        // SAFETY: plain user32 calls on this desktop; the cursor is put back.
        unsafe {
            GetCursorPos(&mut saved);
            SetCursorPos(origin.x + (x * scale) as i32, origin.y + (y * scale) as i32);
            std::thread::sleep(Duration::from_millis(200));
            mouse_event(0x0002, 0, 0, 0, 0); // MOUSEEVENTF_LEFTDOWN
            std::thread::sleep(Duration::from_millis(50));
            mouse_event(0x0004, 0, 0, 0, 0); // MOUSEEVENTF_LEFTUP
            std::thread::sleep(Duration::from_millis(200));
            SetCursorPos(saved.x, saved.y);
        }
    }

    /// PARLA-50: both buttons on a failed pill take clicks without moving focus.
    /// Moves the mouse and clicks; run explicitly with the dev server up.
    #[test]
    #[ignore = "moves the mouse and clicks on a real overlay; needs a Windows desktop"]
    fn failed_pill_buttons_take_clicks_without_stealing_focus() {
        let mut context = tauri::generate_context!();
        context.set_default_window_icon(Some(tauri::image::Image::new_owned(vec![0; 4], 1, 1)));
        let app = tauri::Builder::default()
            .any_thread()
            .invoke_handler(tauri::generate_handler![
                retry_last_failed,
                dismiss_failure_pill,
                report_point
            ])
            .setup(|app| {
                setup(app)?;
                Ok(())
            })
            .build(context)
            .unwrap();
        let handle = app.handle().clone();
        let (result, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Let the overlay page load.
                std::thread::sleep(Duration::from_secs(3));
                let window = handle.get_webview_window(LABEL).unwrap();
                let hwnd = window.hwnd().unwrap().0 as isize;
                // SAFETY: read-only user32 queries.
                let focused = unsafe { GetForegroundWindow() };
                let failed_pill = || {
                    let overlay = begin(&handle, false, Duration::ZERO);
                    std::thread::sleep(Duration::from_millis(400));
                    overlay.processing();
                    overlay.finish(Ending::Failed { retry: true });
                    std::thread::sleep(Duration::from_millis(500));
                };

                failed_pill();
                click(&window, ".retry");
                wait_until("Retry click never reached the command", || {
                    *RETRIES.lock().unwrap() == 1
                });

                failed_pill();
                click(&window, ".result");
                wait_until("Dismiss did not hide the pill", || unsafe {
                    IsWindowVisible(hwnd) == 0
                });
                assert_eq!(unsafe { GetForegroundWindow() }, focused);
            }));
            result.send(check.is_ok()).unwrap();
            handle.exit(0);
        });
        app.run_return(|_, _| {});
        assert!(outcome.recv().unwrap());
    }
}
