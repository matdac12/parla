//! Pastes text into the focused app and restores the previous clipboard.
//!
//! Adapted from Handy's clipboard transaction and Ctrl+V timing patterns.
//! Copyright (c) 2025 CJ Pais. MIT; see THIRD_PARTY_NOTICES.md.
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/clipboard.rs
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/input.rs

use std::time::Duration;

pub const DEFAULT_RESTORE_DELAY_MS: u64 = 250;

/// Blocking desktop IO: call from the flow worker, never a shortcut callback.
/// Only nonempty text is saved. Non-text formats are replaced, not restored.
/// Restore is best effort and skipped if somebody copied different content.
#[cfg(windows)]
pub fn paste_text(text: &str, restore_delay: Duration) -> Result<(), String> {
    // AudioBud also serializes capture/write/paste/restore as one transaction:
    // https://github.com/jamditis/audiobud/blob/main/src-tauri/src/clipboard.rs
    static TRANSACTION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _transaction = TRANSACTION.lock().map_err(|e| e.to_string())?;
    let clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    // Initialize input before touching the clipboard so setup failure is inert.
    let input = enigo::Enigo::new(&enigo::Settings::default()).map_err(|e| e.to_string())?;
    paste_with_backend(
        &mut WindowsDesktop { clipboard, input },
        text,
        restore_delay,
    )
}

/// Windows is the target. Linux deliberately fails before changing clipboard
/// contents; this fallback lets the rest of the app and tests run headlessly.
#[cfg(not(windows))]
pub fn paste_text(_text: &str, _restore_delay: Duration) -> Result<(), String> {
    Err("Text insertion is only supported on Windows".into())
}

#[cfg(any(windows, test))]
fn restorable_text(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.is_empty())
}

#[cfg(any(windows, test))]
trait Desktop {
    // None means no text format; other clipboard errors abort the transaction.
    fn read_text(&mut self) -> Result<Option<String>, String>;
    fn write_text(&mut self, text: &str) -> Result<(), String>;
    fn send_paste(&mut self) -> Result<(), String>;
    fn wait(&mut self, delay: Duration);
}

#[cfg(any(windows, test))]
fn paste_with_backend(
    desktop: &mut impl Desktop,
    text: &str,
    restore_delay: Duration,
) -> Result<(), String> {
    let saved = restorable_text(desktop.read_text()?);
    desktop.write_text(text)?;
    // Handy's delay before injection allows clipboard consumers to settle.
    desktop.wait(Duration::from_millis(100));
    let result = desktop.send_paste();
    // Always attempt restoration, even if input injection failed.
    desktop.wait(restore_delay);
    if let Some(saved) = saved {
        if desktop.read_text().ok().flatten().as_deref() == Some(text) {
            if let Err(error) = desktop.write_text(&saved) {
                eprintln!("Parla clipboard restore failed: {error}");
            }
        }
    }
    result
}

#[cfg(windows)]
struct WindowsDesktop {
    clipboard: arboard::Clipboard,
    input: enigo::Enigo,
}

#[cfg(windows)]
impl Desktop for WindowsDesktop {
    fn read_text(&mut self) -> Result<Option<String>, String> {
        match self.clipboard.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn write_text(&mut self, text: &str) -> Result<(), String> {
        // Clipboard access can be briefly locked by another process. arboard's
        // Windows implementation retries OpenClipboard internally.
        self.clipboard.set_text(text).map_err(|e| e.to_string())
    }

    fn send_paste(&mut self) -> Result<(), String> {
        use enigo::{Direction, Key, Keyboard};
        // Space can come up while Ctrl is physically held. Clear both sides of
        // all shortcut modifiers first, including AltGr's Ctrl+Alt. Do not
        // restore synthetic key-downs: a physical release racing the paste could
        // otherwise leave modifiers stuck. The user may need to repress Ctrl
        // to use it again if they continue physically holding it after paste.
        for key in [
            Key::LControl,
            Key::RControl,
            Key::LShift,
            Key::RShift,
            Key::LMenu,
            Key::RMenu,
            Key::LWin,
            Key::RWin,
        ] {
            self.input
                .key(key, Direction::Release)
                .map_err(|e| e.to_string())?;
        }
        self.input
            .key(Key::Control, Direction::Press)
            .map_err(|e| e.to_string())?;
        // Virtual VK_V, not a layout-dependent Unicode character.
        let paste = self
            .input
            .key(Key::Other(0x56), Direction::Click)
            .map_err(|e| e.to_string());
        // Handy holds Ctrl after V for applications polling modifier state.
        std::thread::sleep(Duration::from_millis(100));
        let release_v = self
            .input
            .key(Key::Other(0x56), Direction::Release)
            .map_err(|e| e.to_string());
        let release_ctrl = self
            .input
            .key(Key::Control, Direction::Release)
            .map_err(|e| e.to_string());
        paste.and(release_v).and(release_ctrl)
    }

    fn wait(&mut self, delay: Duration) {
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_empty_or_non_text_clipboards() {
        assert_eq!(restorable_text(None), None);
        assert_eq!(restorable_text(Some(String::new())), None);
    }

    #[test]
    fn preserves_nonempty_text_including_whitespace_and_unicode() {
        for text in ["previous ✓", " ", "\n"] {
            assert_eq!(restorable_text(Some(text.into())), Some(text.into()));
        }
    }

    // Desktop IO is the external boundary; the actual transaction runs against
    // an in-memory clipboard so tests need neither a display nor global input.
    struct MemoryDesktop {
        text: Option<String>,
        pasted: Vec<String>,
        waits: Vec<Duration>,
        injection_error: bool,
        user_copy: Option<String>,
        writes: Vec<String>,
    }

    impl MemoryDesktop {
        fn new(text: Option<&str>) -> Self {
            Self {
                text: text.map(str::to_owned),
                pasted: vec![],
                waits: vec![],
                injection_error: false,
                user_copy: None,
                writes: vec![],
            }
        }
    }

    impl Desktop for MemoryDesktop {
        fn read_text(&mut self) -> Result<Option<String>, String> {
            Ok(self.text.clone())
        }
        fn write_text(&mut self, text: &str) -> Result<(), String> {
            self.text = Some(text.into());
            self.writes.push(text.into());
            Ok(())
        }
        fn send_paste(&mut self) -> Result<(), String> {
            if self.injection_error {
                return Err("input failed".into());
            }
            self.pasted.push(
                self.text
                    .clone()
                    .expect("clipboard must be set before paste"),
            );
            if let Some(copy) = self.user_copy.take() {
                self.text = Some(copy);
            }
            Ok(())
        }
        fn wait(&mut self, delay: Duration) {
            self.waits.push(delay);
        }
    }

    #[test]
    fn pastes_new_text_before_restoring_saved_text_after_requested_delay() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        paste_with_backend(&mut desktop, "Parla test ✓", Duration::from_millis(275)).unwrap();
        assert_eq!(desktop.pasted, ["Parla test ✓"]);
        assert_eq!(desktop.text.as_deref(), Some("old"));
        assert_eq!(
            desktop.waits,
            [Duration::from_millis(100), Duration::from_millis(275)]
        );
    }

    #[test]
    fn still_restores_when_input_injection_fails() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.injection_error = true;
        assert!(paste_with_backend(&mut desktop, "new", Duration::ZERO).is_err());
        assert_eq!(desktop.text.as_deref(), Some("old"));
    }

    #[test]
    fn does_not_restore_empty_or_non_text_clipboard() {
        for original in [None, Some("")] {
            let mut desktop = MemoryDesktop::new(original);
            paste_with_backend(&mut desktop, "new", Duration::ZERO).unwrap();
            assert_eq!(desktop.writes, ["new"]);
        }
    }

    #[test]
    fn does_not_overwrite_a_new_user_copy() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.user_copy = Some("copied during paste".into());
        paste_with_backend(&mut desktop, "new", Duration::ZERO).unwrap();
        assert_eq!(desktop.text.as_deref(), Some("copied during paste"));
    }
}
