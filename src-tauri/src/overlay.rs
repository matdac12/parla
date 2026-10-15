//! The small always-on-top pill: live level meter while recording, a spinner
//! while transcribing, and a brief done/error confirmation.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use tauri::{Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "overlay";
const WIDTH: f64 = 144.0;
const HEIGHT: f64 = 40.0;
const SHOW_DELAY: Duration = Duration::from_millis(250);
const DONE_HOLD: Duration = Duration::from_millis(750);
/// A failed dictation keeps its pill (and Retry button) up for a while.
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
    Ok(())
}

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
    let x = origin.x + ((f64::from(size.width) - WIDTH * scale) / 2.0) as i32;
    let y = origin.y + (f64::from(size.height) - (HEIGHT + 72.0) * scale) as i32;
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

/// Bumped by every `begin`, so a delayed hide never hides a newer session.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// One dictation's pill: recording (live meter) -> processing (spinner) -> a
/// brief done/error confirmation. Dropping it without `finish` hides the pill
/// at once (cancelled dictation).
pub struct Overlay {
    app: tauri::AppHandle,
    generation: u64,
    metering: Arc<AtomicBool>,
    settled: bool,
}

/// Starts a session. The pill appears after a short delay (so brief or
/// cancelled holds never flash it) and streams microphone levels.
pub fn begin(app: &tauri::AppHandle) -> Overlay {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let metering = Arc::new(AtomicBool::new(true));
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
    }
}

/// The pill is click-through except while it offers Retry.
fn set_interactive(app: &tauri::AppHandle, interactive: bool) {
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.set_ignore_cursor_events(!interactive);
    }
}

/// Starts a session already in the transcribing phase (used by Retry).
pub fn begin_processing(app: &tauri::AppHandle) -> Overlay {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    set_interactive(app, false);
    let overlay = Overlay {
        app: app.clone(),
        generation,
        metering: Arc::new(AtomicBool::new(false)),
        settled: false,
    };
    overlay.processing();
    overlay
}

fn reveal(app: &tauri::AppHandle) {
    place(app);
    if let Some(window) = app.get_webview_window(LABEL) {
        // `focusable(false)` keeps the paste target focused.
        let _ = window.show();
    }
}

fn hide(app: &tauri::AppHandle) {
    set_interactive(app, false);
    if let Some(window) = app.get_webview_window(LABEL) {
        let _ = window.hide();
    }
}

impl Overlay {
    /// Recording ended; the clip is being transcribed. Shows a spinner.
    pub fn processing(&self) {
        self.metering.store(false, Ordering::Release);
        reveal(&self.app);
        let _ = self.app.emit_to(LABEL, "phase", "processing");
    }

    /// Shows a short confirmation (check on success, cross on failure), then
    /// hides the pill.
    pub fn finish(mut self, ok: bool) {
        self.settled = true;
        self.metering.store(false, Ordering::Release);
        reveal(&self.app);
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
        self.metering.store(false, Ordering::Release);
        if !self.settled && GENERATION.load(Ordering::Acquire) == self.generation {
            hide(&self.app);
        }
    }
}
