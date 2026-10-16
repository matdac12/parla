//! Pastes text into the focused app and restores the previous clipboard.
//!
//! Adapted from Handy's clipboard transaction and Ctrl+V timing patterns.
//! Copyright (c) 2025 CJ Pais. MIT; see THIRD_PARTY_NOTICES.md.
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/clipboard.rs
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/input.rs

use std::time::{Duration, Instant};

pub const DEFAULT_RESTORE_DELAY_MS: u64 = 250;

/// Blocking desktop IO: call from the flow worker, never a shortcut callback.
/// Only nonempty text is saved. Non-text formats are replaced, not restored.
/// Restore is best effort and skipped if somebody copied different content.
pub fn paste_text(text: &str, restore_delay: Duration) -> Result<(), String> {
    paste_text_timed(text, restore_delay).map(|_| ())
}

/// Returns when the V click was injected, excluding the Ctrl key-hold and
/// clipboard restoration.
/// Still waits for restoration before releasing the serialized transaction.
#[cfg(windows)]
pub fn paste_text_timed(text: &str, restore_delay: Duration) -> Result<Instant, String> {
    paste_text_with_options(text, restore_delay, false)
}

/// Compatibility mode retains the original waits for applications that need them.
#[cfg(windows)]
pub fn paste_text_with_options(
    text: &str,
    restore_delay: Duration,
    compatibility_mode: bool,
) -> Result<Instant, String> {
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
        compatibility_mode,
    )
}

/// Windows is the target. Linux deliberately fails before changing clipboard
/// contents; this fallback lets the rest of the app and tests run headlessly.
#[cfg(not(windows))]
pub fn paste_text_timed(_text: &str, _restore_delay: Duration) -> Result<Instant, String> {
    Err("Text insertion is only supported on Windows".into())
}

#[cfg(not(windows))]
pub fn paste_text_with_options(
    text: &str,
    restore_delay: Duration,
    _compatibility_mode: bool,
) -> Result<Instant, String> {
    paste_text_timed(text, restore_delay)
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
    /// Returns the instant the V click was injected, before the key-hold.
    fn send_paste(&mut self, key_hold: Duration) -> Result<Instant, String>;
    fn wait(&mut self, delay: Duration);
}

#[cfg(any(windows, test))]
fn paste_with_backend(
    desktop: &mut impl Desktop,
    text: &str,
    restore_delay: Duration,
    compatibility_mode: bool,
) -> Result<Instant, String> {
    let saved = restorable_text(desktop.read_text()?);
    desktop.write_text(text)?;
    let settle = Duration::from_millis(if compatibility_mode { 100 } else { 25 });
    desktop.wait(settle);
    let result = desktop.send_paste(settle);
    // Always attempt restoration, even if input injection failed.
    desktop.wait(restore_delay);
    if let Some(saved) = saved {
        if let Err(error) = restore_clipboard(desktop, text, &saved) {
            eprintln!("Parla clipboard restore failed after retries: {error}");
        }
    }
    result
}

#[cfg(any(windows, test))]
fn restore_clipboard(desktop: &mut impl Desktop, pasted: &str, saved: &str) -> Result<(), String> {
    for attempt in 0..5 {
        // Re-read on EVERY attempt: a user copy may arrive after a failed write.
        let result = match desktop.read_text() {
            Ok(current) if current.as_deref() != Some(pasted) => return Ok(()),
            Ok(_) => desktop.write_text(saved),
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if attempt == 4 => return Err(error),
            Err(_) => desktop.wait(Duration::from_millis((attempt + 1) * 10)),
        }
    }
    unreachable!("the final restore attempt always returns")
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

    fn send_paste(&mut self, key_hold: Duration) -> Result<Instant, String> {
        let _synthetic_input = crate::hotkey::synthetic_input_guard();
        use enigo::{Direction, Key, Keyboard};
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        // Reuse a Ctrl already held by the user instead of synthetically
        // releasing it. Other modifiers would change Ctrl+V, so clear only
        // those currently down. The hook ignores all injected cleanup events.
        // SAFETY: GetAsyncKeyState has no pointer parameters; the sign bit is
        // the current logical state, sampled outside the keyboard callback.
        let borrow_control = unsafe { GetAsyncKeyState(0xa2) < 0 || GetAsyncKeyState(0xa3) < 0 };
        for (vk, key) in [
            (0xa0, Key::LShift),
            (0xa1, Key::RShift),
            (0xa4, Key::LMenu),
            (0xa5, Key::RMenu),
            (0x5b, Key::LWin),
            (0x5c, Key::RWin),
        ] {
            // SAFETY: vk is a supported Windows virtual-key code.
            if unsafe { GetAsyncKeyState(vk) } < 0 {
                self.input
                    .key(key, Direction::Release)
                    .map_err(|e| e.to_string())?;
            }
        }
        if !borrow_control {
            self.input
                .key(Key::Control, Direction::Press)
                .map_err(|e| e.to_string())?;
        }
        // Virtual VK_V, not a layout-dependent Unicode character.
        let paste = self
            .input
            .key(Key::Other(0x56), Direction::Click)
            .map_err(|e| e.to_string());
        let pasted = Instant::now();
        // Handy holds Ctrl after V for applications polling modifier state.
        std::thread::sleep(key_hold);
        let release_v = self
            .input
            .key(Key::Other(0x56), Direction::Release)
            .map_err(|e| e.to_string());
        let release_ctrl = if borrow_control {
            Ok(())
        } else {
            self.input
                .key(Key::Control, Direction::Release)
                .map_err(|e| e.to_string())
        };
        paste.and(release_v).and(release_ctrl).map(|()| pasted)
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
        restoring: bool,
        read_failures: usize,
        write_failures: usize,
        copy_after_failed_write: Option<String>,
        key_holds: Vec<Duration>,
        paste_stamp: Option<Instant>,
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
                restoring: false,
                read_failures: 0,
                write_failures: 0,
                copy_after_failed_write: None,
                key_holds: Vec::new(),
                paste_stamp: None,
            }
        }
    }

    impl Desktop for MemoryDesktop {
        fn read_text(&mut self) -> Result<Option<String>, String> {
            if self.restoring && self.read_failures > 0 {
                self.read_failures -= 1;
                return Err("clipboard occupied".into());
            }
            Ok(self.text.clone())
        }
        fn write_text(&mut self, text: &str) -> Result<(), String> {
            if self.restoring && self.write_failures > 0 {
                self.write_failures -= 1;
                if let Some(copy) = self.copy_after_failed_write.take() {
                    self.text = Some(copy);
                }
                return Err("clipboard occupied".into());
            }
            self.text = Some(text.into());
            self.writes.push(text.into());
            Ok(())
        }
        fn send_paste(&mut self, key_hold: Duration) -> Result<Instant, String> {
            self.key_holds.push(key_hold);
            self.restoring = true;
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
            let stamp = Instant::now();
            self.paste_stamp = Some(stamp);
            Ok(stamp)
        }
        fn wait(&mut self, delay: Duration) {
            self.waits.push(delay);
        }
    }

    #[test]
    fn retries_restore_reads_after_transient_clipboard_contention() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.read_failures = 2;
        paste_with_backend(&mut desktop, "new", Duration::ZERO, false).unwrap();
        assert_eq!(desktop.text.as_deref(), Some("old"));
        assert_eq!(
            &desktop.waits[2..],
            [Duration::from_millis(10), Duration::from_millis(20)]
        );
    }

    #[test]
    fn retries_restore_write_after_comparing_again() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.write_failures = 1;
        paste_with_backend(&mut desktop, "new", Duration::ZERO, false).unwrap();
        assert_eq!(desktop.text.as_deref(), Some("old"));
        assert_eq!(desktop.write_failures, 0);
    }

    #[test]
    fn retry_does_not_overwrite_a_copy_after_a_failed_write() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.write_failures = 1;
        desktop.copy_after_failed_write = Some("user copy".into());
        paste_with_backend(&mut desktop, "new", Duration::ZERO, false).unwrap();
        assert_eq!(desktop.text.as_deref(), Some("user copy"));
    }

    #[test]
    fn restore_contention_is_bounded() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.read_failures = 99;
        paste_with_backend(&mut desktop, "new", Duration::ZERO, false).unwrap();
        assert_eq!(desktop.read_failures, 94);
        assert_eq!(
            desktop.waits.iter().skip(2).sum::<Duration>(),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn pastes_new_text_before_restoring_saved_text_after_requested_delay() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        paste_with_backend(
            &mut desktop,
            "Parla test ✓",
            Duration::from_millis(275),
            false,
        )
        .unwrap();
        assert_eq!(desktop.pasted, ["Parla test ✓"]);
        assert_eq!(desktop.key_holds, [Duration::from_millis(25)]);
        assert_eq!(desktop.text.as_deref(), Some("old"));
        assert_eq!(
            desktop.waits,
            [Duration::from_millis(25), Duration::from_millis(275)]
        );
    }

    #[test]
    fn compatibility_mode_preserves_original_paste_timing_and_restoration() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        paste_with_backend(&mut desktop, "new", Duration::from_millis(250), true).unwrap();
        assert_eq!(desktop.pasted, ["new"]);
        assert_eq!(desktop.text.as_deref(), Some("old"));
        assert_eq!(
            desktop.waits,
            [Duration::from_millis(100), Duration::from_millis(250)]
        );
        assert_eq!(desktop.key_holds, [Duration::from_millis(100)]);
    }

    #[test]
    fn pasted_instant_is_the_v_injection_stamp_not_the_return_time() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let pasted = paste_with_backend(&mut desktop, "new", Duration::ZERO, false).unwrap();
        assert_eq!(Some(pasted), desktop.paste_stamp);
    }

    #[test]
    fn still_restores_when_input_injection_fails() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.injection_error = true;
        assert!(paste_with_backend(&mut desktop, "new", Duration::ZERO, false).is_err());
        assert_eq!(desktop.text.as_deref(), Some("old"));
    }

    #[test]
    fn does_not_restore_empty_or_non_text_clipboard() {
        for original in [None, Some("")] {
            let mut desktop = MemoryDesktop::new(original);
            paste_with_backend(&mut desktop, "new", Duration::ZERO, false).unwrap();
            assert_eq!(desktop.writes, ["new"]);
        }
    }

    #[test]
    fn does_not_overwrite_a_new_user_copy() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.user_copy = Some("copied during paste".into());
        paste_with_backend(&mut desktop, "new", Duration::ZERO, false).unwrap();
        assert_eq!(desktop.text.as_deref(), Some("copied during paste"));
    }
}
