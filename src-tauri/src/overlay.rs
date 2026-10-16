//! The small always-on-top pill: live level meter while recording, a spinner
//! while transcribing, and a brief done/error confirmation.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use tauri::{Emitter, LogicalSize, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "overlay";
const WIDTH: f64 = 144.0;
const HEIGHT: f64 = 40.0;
const CAPTION_WIDTH: f64 = 366.0;
const CAPTION_HEIGHT: f64 = 116.0;
static CAPTIONS_ENABLED: AtomicBool = AtomicBool::new(false);
const SHOW_DELAY: Duration = Duration::from_millis(250);
const DONE_HOLD: Duration = Duration::from_millis(750);
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

/// Tells the overlay whether the recording is locked hands-free (true) or held
/// push-to-talk (false).
pub fn set_mode(app: &tauri::AppHandle, locked: bool) {
    let _ = app.emit_to(LABEL, "mode", locked);
}

fn place(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window(LABEL) else {
        return;
    };
    let Ok(Some(monitor)) = window.primary_monitor() else {
        return;
    };
    let scale = monitor.scale_factor();
    let size = monitor.size();
    let origin = monitor.position();
    let (width, height) = dimensions(CAPTIONS_ENABLED.load(Ordering::Acquire));
    let x = origin.x + ((f64::from(size.width) - width * scale) / 2.0) as i32;
    let y = origin.y + (f64::from(size.height) - (height + 72.0) * scale) as i32;
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

fn dimensions(captions: bool) -> (f64, f64) {
    if captions {
        (CAPTION_WIDTH, CAPTION_HEIGHT)
    } else {
        (WIDTH, HEIGHT)
    }
}

fn configure(app: &tauri::AppHandle, captions: bool, generation: u64) {
    if CAPTIONS_ENABLED.swap(captions, Ordering::AcqRel) != captions {
        if let Some(window) = app.get_webview_window(LABEL) {
            let (width, height) = dimensions(captions);
            let _ = window.set_size(LogicalSize::new(width, height));
        }
    }
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

/// Starts a session. The pill appears after a short delay (so brief or
/// cancelled holds never flash it) and streams microphone levels.
pub fn begin(app: &tauri::AppHandle, captions: bool) -> Overlay {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
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
            for _ in 0..(SHOW_DELAY.as_millis() / step.as_millis()) {
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
    set_interactive(app, false);
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
    place(app);
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
    pub fn finish(mut self, ok: bool) {
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
        let hold = if ok { DONE_HOLD } else { ERROR_HOLD };
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
    use super::dismissable;

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
                    let overlay = begin(&handle, false);
                    std::thread::sleep(Duration::from_millis(400));
                    overlay.processing();
                    overlay.finish(false);
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
