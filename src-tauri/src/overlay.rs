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
