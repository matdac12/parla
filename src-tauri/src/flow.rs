//! PARLA-2 spike wiring. PARLA-4 replaces on_hotkey with the dictation pipeline.

use std::{sync::mpsc, time::Duration};
use tauri::Manager;

use crate::{
    hotkey::{Hotkey, HotkeyEvent},
    inserter,
};

pub fn setup(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let (events, receiver) = mpsc::channel();
    let hotkey = Hotkey::new(app.handle(), events)?;
    // One worker preserves event order and keeps clipboard delays off the
    // shortcut dispatch and Tauri event-loop threads.
    std::thread::Builder::new()
        .name("parla-flow".into())
        .spawn(move || {
            for event in receiver {
                on_hotkey(event);
            }
        })?;
    app.manage(hotkey);
    Ok(())
}

fn on_hotkey(event: HotkeyEvent) {
    if event == HotkeyEvent::Released {
        if let Err(error) = inserter::paste_text(
            "Parla test ✓",
            Duration::from_millis(inserter::DEFAULT_RESTORE_DELAY_MS),
        ) {
            eprintln!("Parla test paste failed: {error}");
        }
    }
}
