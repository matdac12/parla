//! The small always-on-top "recording" pill with a live level meter.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use tauri::{Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "overlay";
const WIDTH: f64 = 144.0;
const HEIGHT: f64 = 40.0;
const SHOW_DELAY: Duration = Duration::from_millis(250);

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

/// Shows the overlay after a short delay (so brief or cancelled holds never
/// flash it) and streams microphone levels to it until the returned guard is
/// dropped.
pub fn show(app: &tauri::AppHandle) -> Meter {
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let handle = app.clone();
    let _ = std::thread::Builder::new()
        .name("parla-meter".into())
        .spawn(move || {
            let step = Duration::from_millis(25);
            for _ in 0..(SHOW_DELAY.as_millis() / step.as_millis()) {
                if !flag.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(step);
            }
            if !flag.load(Ordering::Relaxed) {
                return;
            }
            place(&handle);
            if let Some(window) = handle.get_webview_window(LABEL) {
                // `focusable(false)` keeps the paste target focused.
                let _ = window.show();
            }
            while flag.load(Ordering::Relaxed) {
                let _ = handle.emit_to(LABEL, "level", crate::recorder::current_level());
                std::thread::sleep(Duration::from_millis(40));
            }
            // The guard may have hidden the window before it was shown.
            if let Some(window) = handle.get_webview_window(LABEL) {
                let _ = window.hide();
            }
        });
    Meter {
        app: app.clone(),
        running,
    }
}

pub struct Meter {
    app: tauri::AppHandle,
    running: Arc<AtomicBool>,
}

impl Drop for Meter {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(window) = self.app.get_webview_window(LABEL) {
            let _ = window.hide();
        }
    }
}
