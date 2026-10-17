//! Pastes text into the focused app and restores the previous clipboard.
//!
//! Adapted from Handy's clipboard transaction and Ctrl+V timing patterns.
//! Copyright (c) 2025 CJ Pais. MIT; see THIRD_PARTY_NOTICES.md.
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/clipboard.rs
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/input.rs

use std::time::{Duration, Instant};

pub const DEFAULT_RESTORE_DELAY_MS: u64 = 250;

/// Lets clipboard listeners (Win+V, clipboard managers) release the clipboard
/// before Ctrl+V. 15 ms measured no more contention than 25 ms (PARLA-45).
#[cfg(any(windows, test))]
const CLIPBOARD_SETTLE: Duration = Duration::from_millis(15);
/// Ctrl stays down after V; it runs after the paste, off the visible path.
#[cfg(any(windows, test))]
const PASTE_KEY_HOLD: Duration = Duration::from_millis(25);
/// Compatibility mode uses this for both the settle and the key-hold.
#[cfg(any(windows, test))]
const COMPATIBILITY_WAIT: Duration = Duration::from_millis(100);

/// Blocking desktop IO: call from the flow worker, never a shortcut callback.
/// Only nonempty text is saved. Non-text formats are replaced, not restored.
/// Returns right after the paste; `parla-clipboard-restore` restores the
/// clipboard `restore_delay` later. Restore is best effort and skipped if
/// somebody copied different content.
pub fn paste_text(text: &str, restore_delay: Duration) -> Result<(), String> {
    paste_text_timed(text, restore_delay).map(|_| ())
}

/// Returns when the V click was injected, excluding the Ctrl key-hold and
/// clipboard restoration.
#[cfg(windows)]
pub fn paste_text_timed(text: &str, restore_delay: Duration) -> Result<Instant, String> {
    paste_text_with_options(text, restore_delay, false, false)
}

/// Compatibility mode retains the original waits for applications that need them.
/// `send` presses Enter right after a successful paste (PARLA-58).
#[cfg(windows)]
pub fn paste_text_with_options(
    text: &str,
    restore_delay: Duration,
    compatibility_mode: bool,
    send: bool,
) -> Result<Instant, String> {
    // AudioBud also serializes capture/write/paste/restore as one transaction:
    // https://github.com/jamditis/audiobud/blob/main/src-tauri/src/clipboard.rs
    in_transaction(|desktop, pending| {
        paste_deferring_restore(
            desktop,
            pending,
            text,
            restore_delay,
            compatibility_mode,
            send,
        )
    })
}

/// The user's clipboard text, for a later `paste_text_restoring` (command
/// mode saves it before anything changes the clipboard).
#[cfg(windows)]
pub fn user_clipboard_text() -> Result<Option<String>, String> {
    let pending = pending_restore();
    user_text(&mut WindowsDesktop::new()?, &pending)
}

/// Pastes `text` and restores `saved` afterwards, on the same timer as a
/// dictation's restore.
#[cfg(windows)]
pub fn paste_text_restoring(
    text: &str,
    saved: Option<String>,
    restore_delay: Duration,
    compatibility_mode: bool,
) -> Result<Instant, String> {
    in_transaction(|desktop, pending| {
        paste_then_restore(
            desktop,
            pending,
            text,
            saved,
            restore_delay,
            compatibility_mode,
            false,
        )
    })
}

#[cfg(windows)]
fn in_transaction(
    paste: impl FnOnce(&mut WindowsDesktop, &mut Option<PendingRestore>) -> Result<Instant, String>,
) -> Result<Instant, String> {
    let mut pending = pending_restore();
    let result = paste(&mut WindowsDesktop::new()?, &mut pending);
    if let Some(restore) = pending.as_ref() {
        schedule_restore(restore.due);
    }
    result
}

/// The paste transaction: at most one restore is pending at a time.
#[cfg(windows)]
fn pending_restore() -> std::sync::MutexGuard<'static, Option<PendingRestore>> {
    static PENDING: std::sync::Mutex<Option<PendingRestore>> = std::sync::Mutex::new(None);
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Wakes the long-lived restore thread at `due`. A restore it cannot run stays
/// pending, and the next paste carries the user's text forward.
#[cfg(windows)]
fn schedule_restore(due: Instant) {
    use std::sync::{mpsc, OnceLock};
    static TIMER: OnceLock<Option<mpsc::Sender<Instant>>> = OnceLock::new();
    let timer = TIMER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Instant>();
        std::thread::Builder::new()
            .name("parla-clipboard-restore".into())
            .spawn(move || {
                for due in receiver {
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                    let mut pending = pending_restore();
                    // Not yet due: a newer paste replaced it and sent its own wake-up.
                    if pending.as_ref().is_none_or(|r| r.due > Instant::now()) {
                        continue;
                    }
                    match WindowsDesktop::new() {
                        Ok(mut desktop) => restore_pending(&mut desktop, &mut pending),
                        Err(error) => eprintln!("Parla clipboard restore deferred: {error}"),
                    }
                }
            })
            .map_err(|error| eprintln!("Parla clipboard restore thread failed: {error}"))
            .ok()
            .map(|_| sender)
    });
    if let Some(sender) = timer {
        let _ = sender.send(due);
    }
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
    _send: bool,
) -> Result<Instant, String> {
    paste_text_timed(text, restore_delay)
}

#[cfg(not(windows))]
pub fn user_clipboard_text() -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(not(windows))]
pub fn paste_text_restoring(
    text: &str,
    _saved: Option<String>,
    restore_delay: Duration,
    _compatibility_mode: bool,
) -> Result<Instant, String> {
    paste_text_timed(text, restore_delay)
}

/// Auto-send (PARLA-58): when the last word is "send" or "invia", returns the
/// text to paste without it, and the caller presses Enter after the paste.
/// Case and trailing punctuation are ignored; only a whole word counts
/// ("resend" does not). A dangling comma or dash before it is dropped, a full
/// stop is kept. A dictation that is only the trigger stays plain text.
/// A suffix scan of the final transcript, so it costs nothing before the paste.
pub fn strip_send_trigger(text: &str) -> Option<&str> {
    let body = text.trim_end_matches(|c: char| !c.is_alphanumeric());
    let start = body
        .char_indices()
        .rev()
        .find(|&(_, c)| !is_word_char(c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    let word = &body[start..];
    if !["send", "invia"]
        .iter()
        .any(|t| word.eq_ignore_ascii_case(t))
    {
        return None;
    }
    let kept = body[..start].trim_end_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ',' | ';' | ':' | '-' | '–' | '—' | '(' | '[' | '«' | '“' | '‘'
            )
    });
    kept.contains(char::is_alphanumeric).then_some(kept)
}

/// Apostrophes and hyphens join a word, so "re-send" is not "send".
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '\'' | '’' | '-')
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
    /// A plain Enter, after the paste's Ctrl is released.
    fn send_enter(&mut self) -> Result<(), String>;
    fn wait(&mut self, delay: Duration);
}

/// The user's clipboard text, owed back once `due` passes.
#[cfg(any(windows, test))]
struct PendingRestore {
    saved: String,
    pasted: String,
    due: Instant,
}

/// Pastes and returns without waiting for the restore, which is left in
/// `pending`. Keeping the dictation busy through the restore delay dropped a
/// dictation started right after a paste (PARLA-48).
#[cfg(any(windows, test))]
fn paste_deferring_restore(
    desktop: &mut impl Desktop,
    pending: &mut Option<PendingRestore>,
    text: &str,
    restore_delay: Duration,
    compatibility_mode: bool,
    send: bool,
) -> Result<Instant, String> {
    let saved = user_text(desktop, pending)?;
    paste_then_restore(
        desktop,
        pending,
        text,
        saved,
        restore_delay,
        compatibility_mode,
        send,
    )
}

/// The clipboard text owed back to the user: the clipboard itself, unless it
/// still holds our previous paste, whose restore is pending.
#[cfg(any(windows, test))]
fn user_text(
    desktop: &mut impl Desktop,
    pending: &Option<PendingRestore>,
) -> Result<Option<String>, String> {
    let current = restorable_text(desktop.read_text()?);
    Ok(match pending.as_ref() {
        Some(restore) if current.as_deref() == Some(restore.pasted.as_str()) => {
            Some(restore.saved.clone())
        }
        _ => current,
    })
}

/// Pastes `text` and leaves the restore of `saved` pending.
#[cfg(any(windows, test))]
fn paste_then_restore(
    desktop: &mut impl Desktop,
    pending: &mut Option<PendingRestore>,
    text: &str,
    saved: Option<String>,
    restore_delay: Duration,
    compatibility_mode: bool,
    send: bool,
) -> Result<Instant, String> {
    desktop.write_text(text)?;
    let (settle, key_hold) = if compatibility_mode {
        (COMPATIBILITY_WAIT, COMPATIBILITY_WAIT)
    } else {
        (CLIPBOARD_SETTLE, PASTE_KEY_HOLD)
    };
    desktop.wait(settle);
    let result = desktop.send_paste(key_hold);
    // No extra delay: injected keys reach the app in order (Mattia, PARLA-58).
    // The text is already pasted, so a failed Enter is not a failed dictation.
    if send && result.is_ok() {
        if let Err(error) = desktop.send_enter() {
            eprintln!("Parla auto-send Enter failed: {error}");
        }
    }
    // Always attempt restoration, even if input injection failed.
    *pending = saved.map(|saved| PendingRestore {
        saved,
        pasted: text.into(),
        due: Instant::now() + restore_delay,
    });
    result
}

/// Runs the pending restore, at most once: it is taken.
#[cfg(any(windows, test))]
fn restore_pending(desktop: &mut impl Desktop, pending: &mut Option<PendingRestore>) {
    let Some(restore) = pending.take() else {
        return;
    };
    if let Err(error) = restore_clipboard(desktop, &restore.pasted, &restore.saved) {
        eprintln!("Parla clipboard restore failed after retries: {error}");
    }
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
impl WindowsDesktop {
    fn new() -> Result<Self, String> {
        let clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        // Initialize input before touching the clipboard so setup failure is inert.
        let input = enigo::Enigo::new(&enigo::Settings::default()).map_err(|e| e.to_string())?;
        Ok(Self { clipboard, input })
    }
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

    fn send_enter(&mut self) -> Result<(), String> {
        let _synthetic_input = crate::hotkey::synthetic_input_guard();
        use enigo::{Direction, Key, Keyboard};
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
        // Any held modifier, including a Ctrl the paste borrowed, would turn
        // this into Ctrl+Enter or similar. The hook ignores injected releases.
        for (vk, key) in [
            (0xa0, Key::LShift),
            (0xa1, Key::RShift),
            (0xa2, Key::LControl),
            (0xa3, Key::RControl),
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
        self.input
            .key(Key::Return, Direction::Click)
            .map_err(|e| e.to_string())
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
        enters: usize,
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
                enters: 0,
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
        fn send_enter(&mut self) -> Result<(), String> {
            assert!(!self.pasted.is_empty(), "Enter must follow the paste");
            self.enters += 1;
            Ok(())
        }
        fn wait(&mut self, delay: Duration) {
            self.waits.push(delay);
        }
    }

    /// The blocking form: paste, wait out the delay, then restore.
    fn paste_with_backend(
        desktop: &mut MemoryDesktop,
        text: &str,
        restore_delay: Duration,
        compatibility_mode: bool,
    ) -> Result<Instant, String> {
        let mut pending = None;
        let result = paste_deferring_restore(
            desktop,
            &mut pending,
            text,
            restore_delay,
            compatibility_mode,
            false,
        );
        desktop.wait(restore_delay);
        restore_pending(desktop, &mut pending);
        result
    }

    #[test]
    fn paste_returns_before_the_restore_delay_and_leaves_it_pending() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let mut pending = None;
        let delay = Duration::from_millis(250);
        let before = Instant::now();
        paste_deferring_restore(&mut desktop, &mut pending, "new", delay, false, false).unwrap();
        assert_eq!(desktop.waits, [Duration::from_millis(15)]);
        assert_eq!(desktop.text.as_deref(), Some("new"));
        let restore = pending.as_ref().unwrap();
        assert_eq!(restore.saved, "old");
        assert!(restore.due >= before + delay);
    }

    #[test]
    fn a_paste_with_a_pending_restore_keeps_the_users_text_as_the_original() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let mut pending = None;
        let delay = Duration::from_millis(250);
        paste_deferring_restore(&mut desktop, &mut pending, "first", delay, false, false).unwrap();
        paste_deferring_restore(&mut desktop, &mut pending, "second", delay, false, false).unwrap();
        assert_eq!(desktop.pasted, ["first", "second"]);
        // No early restore, no wait: the pending restore is carried forward.
        assert_eq!(desktop.writes, ["first", "second"]);
        assert_eq!(desktop.waits.len(), 2);
        // Our first transcript was never saved as the user's clipboard.
        assert_eq!(pending.as_ref().unwrap().saved, "old");
        restore_pending(&mut desktop, &mut pending);
        assert_eq!(desktop.text.as_deref(), Some("old"));
    }

    #[test]
    fn a_copy_between_pastes_replaces_the_pending_restore() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let mut pending = None;
        paste_deferring_restore(
            &mut desktop,
            &mut pending,
            "first",
            Duration::ZERO,
            false,
            false,
        )
        .unwrap();
        desktop.text = Some("user copy".into());
        paste_deferring_restore(
            &mut desktop,
            &mut pending,
            "second",
            Duration::ZERO,
            false,
            false,
        )
        .unwrap();
        assert_eq!(pending.as_ref().unwrap().saved, "user copy");
    }

    #[test]
    fn a_paste_handed_the_users_text_restores_that_text() {
        // A command run saved the user's copy before the clipboard changed.
        let mut desktop = MemoryDesktop::new(Some("something else"));
        let mut pending = None;
        paste_then_restore(
            &mut desktop,
            &mut pending,
            "reply",
            Some("user copy".into()),
            Duration::ZERO,
            false,
            false,
        )
        .unwrap();
        assert_eq!(desktop.pasted, ["reply"]);
        restore_pending(&mut desktop, &mut pending);
        assert_eq!(desktop.text.as_deref(), Some("user copy"));
    }

    #[test]
    fn the_users_text_is_the_pending_one_while_our_paste_is_still_there() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let mut pending = None;
        let delay = Duration::from_millis(250);
        assert_eq!(user_text(&mut desktop, &pending), Ok(Some("old".into())));
        paste_deferring_restore(&mut desktop, &mut pending, "first", delay, false, false).unwrap();
        assert_eq!(user_text(&mut desktop, &pending), Ok(Some("old".into())));
        desktop.text = Some("user copy".into());
        assert_eq!(
            user_text(&mut desktop, &pending),
            Ok(Some("user copy".into()))
        );
    }

    #[test]
    fn the_timer_restore_runs_exactly_once() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let mut pending = None;
        paste_deferring_restore(
            &mut desktop,
            &mut pending,
            "new",
            Duration::ZERO,
            false,
            false,
        )
        .unwrap();
        restore_pending(&mut desktop, &mut pending);
        assert_eq!(desktop.text.as_deref(), Some("old"));
        assert!(pending.is_none());
        restore_pending(&mut desktop, &mut pending);
        assert_eq!(desktop.writes, ["new", "old"]);
    }

    #[test]
    fn nothing_is_pending_for_an_empty_or_non_text_clipboard() {
        let mut desktop = MemoryDesktop::new(None);
        let mut pending = None;
        paste_deferring_restore(
            &mut desktop,
            &mut pending,
            "new",
            Duration::ZERO,
            false,
            false,
        )
        .unwrap();
        assert!(pending.is_none());
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
            [Duration::from_millis(15), Duration::from_millis(275)]
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
    fn send_presses_enter_once_right_after_the_paste_and_still_restores() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let mut pending = None;
        paste_deferring_restore(
            &mut desktop,
            &mut pending,
            "hi",
            Duration::ZERO,
            false,
            true,
        )
        .unwrap();
        assert_eq!((desktop.pasted.len(), desktop.enters), (1, 1));
        // No wait is added for the Enter: only the settle.
        assert_eq!(desktop.waits, [CLIPBOARD_SETTLE]);
        restore_pending(&mut desktop, &mut pending);
        assert_eq!(desktop.text.as_deref(), Some("old"));
    }

    #[test]
    fn no_enter_without_send_or_when_the_paste_failed() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        paste_with_backend(&mut desktop, "hi", Duration::ZERO, false).unwrap();
        assert_eq!(desktop.enters, 0);
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.injection_error = true;
        let mut pending = None;
        let result = paste_deferring_restore(
            &mut desktop,
            &mut pending,
            "hi",
            Duration::ZERO,
            false,
            true,
        );
        assert!(result.is_err());
        assert_eq!(desktop.enters, 0);
    }

    #[test]
    fn send_trigger_is_stripped_from_the_last_word() {
        for (said, pasted) in [
            ("See you tomorrow, send.", "See you tomorrow"),
            ("Ci vediamo domani, invia.", "Ci vediamo domani"),
            ("Done. Invia.", "Done."),
            ("Are you coming? Send!", "Are you coming?"),
            ("ok send", "ok"),
            ("Ok SEND", "Ok"),
            ("Va bene invia…", "Va bene"),
            ("Call me — send.", "Call me"),
            ("Ciao «invia»", "Ciao"),
            ("Perfetto, grazie; Invia ", "Perfetto, grazie"),
            ("È già pronto, invia.", "È già pronto"),
        ] {
            assert_eq!(strip_send_trigger(said), Some(pasted), "{said:?}");
        }
    }

    #[test]
    fn other_words_and_a_lone_trigger_are_not_sent() {
        for said in [
            "Please resend.",
            "re-send",
            "L'ho inviato.",
            "Who is the sender?",
            "Send it to Marco.",
            "Invia il file a Marco.",
            "inviare",
            "Sent.",
            "Send.",
            "invia",
            " , invia!",
            "",
            "...",
        ] {
            assert_eq!(strip_send_trigger(said), None, "{said:?}");
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
