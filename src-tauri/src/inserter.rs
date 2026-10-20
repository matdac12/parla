//! Pastes text into the focused app and restores the previous clipboard.
//!
//! Adapted from Handy's clipboard transaction and Ctrl+V timing patterns.
//! Copyright (c) 2025 CJ Pais. MIT; see THIRD_PARTY_NOTICES.md.
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/clipboard.rs
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/input.rs

use crate::log::log_error;
use std::time::{Duration, Instant};

pub const DEFAULT_RESTORE_DELAY_MS: u64 = 250;

/// The user's clipboard while command mode's copy holds it (PARLA-64):
/// `saved` is owed back as long as the clipboard still holds `copied`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Borrowed {
    pub saved: Option<String>,
    pub copied: Option<String>,
}

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
    paste_transaction(text, restore_delay, compatibility_mode, send)
}

#[cfg(windows)]
fn paste_transaction(
    text: &str,
    restore_delay: Duration,
    compatibility_mode: bool,
    send: bool,
) -> Result<Instant, String> {
    // AudioBud also serializes capture/write/paste/restore as one transaction:
    // https://github.com/jamditis/audiobud/blob/main/src-tauri/src/clipboard.rs
    let mut pending = pending_restore();
    let result = paste_over(
        &mut WindowsDesktop::new()?,
        &mut pending,
        text,
        restore_delay,
        compatibility_mode,
        send,
    );
    if let Some(restore) = pending.as_ref() {
        schedule_restore(restore.due);
    }
    result
}

/// The clipboard's text for command mode's selection read. A dictation's
/// restore still pending runs first, so the user's own text is what is read.
#[cfg(windows)]
pub fn clipboard_text() -> Result<Option<String>, String> {
    read_settled(&mut WindowsDesktop::new()?, &mut pending_restore())
}

/// Writes the user's text back now, unless the clipboard moved on from the
/// copy: a command run that pastes nothing.
#[cfg(windows)]
pub fn give_back(borrowed: &Borrowed) {
    // Serialized with the restore timer, like a paste.
    let _pending = pending_restore();
    match WindowsDesktop::new() {
        Ok(mut desktop) => give_back_to(&mut desktop, borrowed),
        Err(error) => log_error!("Parla clipboard give-back failed: {error}"),
    }
}

/// The Card's Copy (PARLA-80): the clipboard keeps `text`, with no restore.
#[cfg(windows)]
pub fn copy_text(text: &str) -> Result<(), String> {
    copy_to(&mut WindowsDesktop::new()?, &mut pending_restore(), text)
}

/// Ctrl+Insert, or Ctrl+C where command mode found the window takes it
/// (PARLA-84), into the focused app, for command mode's selection read. It
/// runs while the user still holds the command hotkey, so the modifiers it
/// released go back down before the synthetic-input guard drops: the hook's
/// watchdog samples the logical state and would read a missing Shift as the
/// hotkey let go. No waits in between, so a real release can hardly slip in.
#[cfg(windows)]
pub fn send_copy(key: crate::command_mode::CopyKey) -> Result<(), String> {
    use enigo::{Direction, Key, Keyboard};
    let _synthetic_input = crate::hotkey::synthetic_input_guard();
    // Enigo releases the keys it pressed when dropped; these stay held.
    let settings = enigo::Settings {
        release_keys_when_dropped: false,
        ..enigo::Settings::default()
    };
    let mut input = enigo::Enigo::new(&settings).map_err(|e| e.to_string())?;
    let (borrow_control, released) = clear_for_control(&mut input)?;
    let press = if borrow_control {
        Ok(())
    } else {
        input.key(Key::Control, Direction::Press)
    };
    let key = match key {
        crate::command_mode::CopyKey::CtrlInsert => Key::Insert,
        // Virtual VK_C, not a layout-dependent Unicode character.
        crate::command_mode::CopyKey::CtrlC => Key::Other(0x43),
    };
    let copy = press.and_then(|()| input.key(key, Direction::Click));
    let release_control = if borrow_control {
        Ok(())
    } else {
        input.key(Key::Control, Direction::Release)
    };
    let held_again = released
        .into_iter()
        .try_for_each(|key| input.key(key, Direction::Press));
    copy.and(release_control)
        .and(held_again)
        .map_err(|e| e.to_string())
}

/// Moves on whenever any app writes the clipboard.
#[cfg(windows)]
pub fn clipboard_sequence() -> u32 {
    // SAFETY: no parameters; it only reads the window station's counter.
    unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

/// Editors copy the whole line when nothing is selected. Visual Studio and
/// Scintilla's (Notepad++) add an empty `MSDEVLineSelect` format (PARLA-78).
/// VS Code marks it in its `vscode-editor-data`
/// (`"isFromEmptySelection":true`), which Chromium keeps in its web custom
/// format, a pickle of UTF-16 strings.
#[cfg(windows)]
pub fn copied_empty_selection() -> bool {
    use windows_sys::Win32::{
        Foundation::HGLOBAL,
        System::{DataExchange::*, Memory::*},
    };
    let available = |name: &str| {
        let name: Vec<u16> = name.encode_utf16().chain([0]).collect();
        // SAFETY: `name` is NUL-terminated UTF-16; plain value parameters.
        let format = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
        (format != 0 && unsafe { IsClipboardFormatAvailable(format) } != 0).then_some(format)
    };
    if available("MSDEVLineSelect").is_some() {
        return true;
    }
    let Some(format) = available("Chromium Web Custom MIME Data Format") else {
        return false;
    };
    // Listeners such as clipboard managers open it right after every copy.
    let opened = (0..5).any(|attempt| {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // SAFETY: no owner window; closed below.
        unsafe { OpenClipboard(std::ptr::null_mut()) != 0 }
    });
    if !opened {
        return false;
    }
    // SAFETY: the clipboard is open; the handle is only read while locked,
    // within its GlobalSize, and the clipboard is closed before returning.
    let units = unsafe {
        let handle = GetClipboardData(format) as HGLOBAL;
        let mut units = Vec::new();
        if !handle.is_null() {
            let pointer = GlobalLock(handle) as *const u8;
            if !pointer.is_null() {
                units = std::slice::from_raw_parts(pointer, GlobalSize(handle))
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&pair| u16::from_le_bytes(pair))
                    .collect();
                GlobalUnlock(handle);
            }
        }
        CloseClipboard();
        units
    };
    String::from_utf16_lossy(&units).contains("\"isFromEmptySelection\":true")
}

/// The focused window's executable name, such as `notepad.exe`.
#[cfg(windows)]
pub fn foreground_app() -> Option<String> {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    };
    let mut path = [0u16; 1024];
    let mut length = path.len() as u32;
    // SAFETY: the out pointers are valid for the given sizes, and the process
    // handle is closed once its name is read.
    let named = unsafe {
        let mut process_id = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), &mut process_id);
        if process_id == 0 {
            return None;
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return None;
        }
        let named =
            QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut length);
        CloseHandle(process);
        named != 0
    };
    let path = String::from_utf16_lossy(&path[..length as usize]);
    named.then(|| path.rsplit('\\').next().unwrap_or(&path).to_owned())
}

/// The focused top-level window's class, such as `ConsoleWindowClass`.
#[cfg(windows)]
pub fn foreground_class() -> Option<String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetForegroundWindow};
    let mut class = [0u16; 256];
    // SAFETY: the buffer outlives the call and its length is passed.
    let length = unsafe { GetClassNameW(GetForegroundWindow(), class.as_mut_ptr(), 256) };
    (length > 0).then(|| String::from_utf16_lossy(&class[..length as usize]))
}

/// A mouse button is down: a Ctrl+Shift+drag, not a command (PARLA-78).
#[cfg(windows)]
pub fn mouse_button_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
    };
    // SAFETY: no pointer parameters.
    [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2]
        .into_iter()
        .any(|vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
}

/// The paste transaction: at most one restore is pending at a time.
#[cfg(windows)]
fn pending_restore() -> std::sync::MutexGuard<'static, Option<PendingRestore>> {
    static PENDING: std::sync::Mutex<Option<PendingRestore>> = std::sync::Mutex::new(None);
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Puts the user's clipboard back now if a paste still owes it. For the exit
/// path: the timer thread would not get to it.
#[cfg(windows)]
pub fn flush_pending_restore() {
    let mut pending = pending_restore();
    if pending.is_some() {
        if let Ok(mut desktop) = WindowsDesktop::new() {
            restore_pending(&mut desktop, &mut pending);
        }
    }
}

#[cfg(not(windows))]
pub fn flush_pending_restore() {}

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
            .map_err(|error| log_error!("Parla clipboard restore thread failed: {error}"))
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
pub fn clipboard_text() -> Result<Option<String>, String> {
    Err("The clipboard is only supported on Windows".into())
}

#[cfg(not(windows))]
pub fn give_back(_borrowed: &Borrowed) {}

#[cfg(not(windows))]
pub fn copy_text(_text: &str) -> Result<(), String> {
    Err("The clipboard is only supported on Windows".into())
}

#[cfg(not(windows))]
pub fn send_copy(_key: crate::command_mode::CopyKey) -> Result<(), String> {
    Err("Input is only supported on Windows".into())
}

#[cfg(not(windows))]
pub fn foreground_class() -> Option<String> {
    None
}

#[cfg(not(windows))]
pub fn mouse_button_down() -> bool {
    false
}

#[cfg(not(windows))]
pub fn clipboard_sequence() -> u32 {
    0
}

#[cfg(not(windows))]
pub fn copied_empty_selection() -> bool {
    false
}

#[cfg(not(windows))]
pub fn foreground_app() -> Option<String> {
    None
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
fn paste_over(
    desktop: &mut impl Desktop,
    pending: &mut Option<PendingRestore>,
    text: &str,
    restore_delay: Duration,
    compatibility_mode: bool,
    send: bool,
) -> Result<Instant, String> {
    let current = restorable_text(desktop.read_text()?);
    let saved = match pending.as_ref() {
        // Still our previous transcript: the user's text is the one owed back.
        Some(restore) if current.as_deref() == Some(restore.pasted.as_str()) => {
            Some(restore.saved.clone())
        }
        _ => current,
    };
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
            log_error!("Parla auto-send Enter failed: {error}");
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
    if let Err(error) = restore_clipboard(desktop, Some(&restore.pasted), &restore.saved) {
        log_error!("Parla clipboard restore failed after retries: {error}");
    }
}

/// Command mode's read: the pending restore first, so a dictation's
/// transcript is never taken for the user's clipboard.
#[cfg(any(windows, test))]
fn read_settled(
    desktop: &mut impl Desktop,
    pending: &mut Option<PendingRestore>,
) -> Result<Option<String>, String> {
    restore_pending(desktop, pending);
    Ok(restorable_text(desktop.read_text()?))
}

/// Sets the clipboard for good: a restore still owed would bring the user's
/// old text back over it, so it is dropped.
#[cfg(any(windows, test))]
fn copy_to(
    desktop: &mut impl Desktop,
    pending: &mut Option<PendingRestore>,
    text: &str,
) -> Result<(), String> {
    *pending = None;
    desktop.write_text(text)
}

#[cfg(any(windows, test))]
fn give_back_to(desktop: &mut impl Desktop, borrowed: &Borrowed) {
    let Some(saved) = &borrowed.saved else {
        return;
    };
    if let Err(error) = restore_clipboard(desktop, borrowed.copied.as_deref(), saved) {
        log_error!("Parla clipboard give-back failed after retries: {error}");
    }
}

/// Writes `saved` while the clipboard still holds `ours`, Parla's paste or
/// command mode's copy (None: no text).
#[cfg(any(windows, test))]
fn restore_clipboard(
    desktop: &mut impl Desktop,
    ours: Option<&str>,
    saved: &str,
) -> Result<(), String> {
    for attempt in 0..5 {
        // Re-read on EVERY attempt: a user copy may arrive after a failed write.
        let result = match desktop.read_text() {
            Ok(current) if current.as_deref().filter(|text| !text.is_empty()) != ours => {
                return Ok(())
            }
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

/// Readies Ctrl+<key>: reuse a Ctrl already held by the user instead of
/// synthetically releasing it. Other modifiers would change the shortcut, so
/// clear only those currently down. The hook ignores all injected cleanup
/// events. Returns whether Ctrl is borrowed and the keys released.
#[cfg(windows)]
fn clear_for_control(input: &mut enigo::Enigo) -> Result<(bool, Vec<enigo::Key>), String> {
    use enigo::{Direction, Key, Keyboard};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // SAFETY: GetAsyncKeyState has no pointer parameters; the sign bit is
    // the current logical state, sampled outside the keyboard callback.
    let borrow_control = unsafe { GetAsyncKeyState(0xa2) < 0 || GetAsyncKeyState(0xa3) < 0 };
    let mut released = Vec::new();
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
            input
                .key(key, Direction::Release)
                .map_err(|e| e.to_string())?;
            released.push(key);
        }
    }
    Ok((borrow_control, released))
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
        let (borrow_control, _) = clear_for_control(&mut self.input)?;
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
        let result = paste_over(
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
        paste_over(&mut desktop, &mut pending, "new", delay, false, false).unwrap();
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
        paste_over(&mut desktop, &mut pending, "first", delay, false, false).unwrap();
        paste_over(&mut desktop, &mut pending, "second", delay, false, false).unwrap();
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
        paste_over(
            &mut desktop,
            &mut pending,
            "first",
            Duration::ZERO,
            false,
            false,
        )
        .unwrap();
        desktop.text = Some("user copy".into());
        paste_over(
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
    fn the_cards_copy_keeps_the_text_on_the_clipboard_for_good() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let mut pending = None;
        let delay = Duration::from_millis(250);
        // A restore still owed for an earlier paste of the same text.
        paste_over(&mut desktop, &mut pending, "Ciao", delay, false, false).unwrap();
        copy_to(&mut desktop, &mut pending, "Ciao").unwrap();
        restore_pending(&mut desktop, &mut pending);
        assert_eq!(desktop.text.as_deref(), Some("Ciao"));
        assert_eq!(desktop.pasted, ["Ciao"], "Copy never pastes");
    }

    #[test]
    fn the_timer_restore_runs_exactly_once() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        let mut pending = None;
        paste_over(
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
        paste_over(
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
        paste_over(
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
        let result = paste_over(
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

    fn borrowed(saved: Option<&str>, copied: Option<&str>) -> Borrowed {
        Borrowed {
            saved: saved.map(Into::into),
            copied: copied.map(Into::into),
        }
    }

    #[test]
    fn giving_back_writes_the_users_text_unless_the_clipboard_moved_on() {
        for (clipboard, lent, after) in [
            (
                Some("line"),
                borrowed(Some("user"), Some("line")),
                Some("user"),
            ),
            (
                Some("newer"),
                borrowed(Some("user"), Some("line")),
                Some("newer"),
            ),
            (Some("line"), borrowed(None, Some("line")), Some("line")),
            // A copy with no text (an image): the user's text still comes back.
            (None, borrowed(Some("user"), None), Some("user")),
        ] {
            let mut desktop = MemoryDesktop::new(clipboard);
            give_back_to(&mut desktop, &lent);
            assert_eq!(desktop.text.as_deref(), after, "{lent:?}");
            assert!(desktop.writes.len() <= 1);
        }
    }

    #[test]
    fn reading_for_command_mode_runs_a_pending_dictation_restore_first() {
        let mut desktop = MemoryDesktop::new(Some("user copy"));
        let mut pending = None;
        let delay = Duration::from_secs(1);
        paste_over(&mut desktop, &mut pending, "dictated", delay, false, false).unwrap();
        assert_eq!(
            read_settled(&mut desktop, &mut pending),
            Ok(Some("user copy".into()))
        );
        assert!(pending.is_none());
    }

    #[test]
    fn does_not_overwrite_a_new_user_copy() {
        let mut desktop = MemoryDesktop::new(Some("old"));
        desktop.user_copy = Some("copied during paste".into());
        paste_with_backend(&mut desktop, "new", Duration::ZERO, false).unwrap();
        assert_eq!(desktop.text.as_deref(), Some("copied during paste"));
    }
}
