//! Global hold-to-talk shortcut emitting pressed and released events.
//!
//! Windows uses a keyboard-only low-level hook with injected-event filtering.
//! Tauri reserves the accelerator; its polled release events are ignored.
//! https://docs.rs/crate/global-hotkey/0.8.0/source/src/platform_impl/windows/mod.rs
//! https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc

use std::sync::{
    mpsc::{self, Sender},
    Arc, Mutex,
};
use tauri::AppHandle;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};

pub const DEFAULT_SHORTCUT: &str = "Ctrl+Space";

/// Process-lifetime registration, kept in Tauri managed state. The receiver is
/// the core flow's seam; callbacks only enqueue events and never do desktop IO.
pub struct Hotkey {
    app: AppHandle,
    state: Arc<Mutex<Registration>>,
}

// Snapshotting must never observe our temporary synthetic Ctrl-down. The hook
// uses try_lock (never waits behind SendInput, which can wait for the hook).
#[cfg(any(windows, test))]
static SYNTHETIC_INPUT: Mutex<()> = Mutex::new(());

#[cfg(any(windows, test))]
pub(crate) fn synthetic_input_guard() -> std::sync::MutexGuard<'static, ()> {
    SYNTHETIC_INPUT.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(any(windows, test))]
fn physical_snapshot<T>(sample: impl FnOnce() -> T) -> Option<T> {
    let _guard = match SYNTHETIC_INPUT.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::WouldBlock) => return None,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
    };
    Some(sample())
}

#[cfg(any(windows, test))]
const MODIFIER_KEYS: [Code; 8] = [
    Code::ControlLeft,
    Code::ControlRight,
    Code::ShiftLeft,
    Code::ShiftRight,
    Code::AltLeft,
    Code::AltRight,
    Code::MetaLeft,
    Code::MetaRight,
];

#[derive(Default)]
struct Registration {
    #[cfg(any(windows, test))]
    keys: std::collections::HashSet<Code>,
    shortcut: Option<Shortcut>,
    hold: HoldState,
}

#[cfg(any(windows, test))]
impl Registration {
    fn raw_event(&mut self, key: Code, pressed: bool) -> Option<HotkeyEvent> {
        let first_press = if pressed {
            self.keys.insert(key)
        } else {
            self.keys.remove(&key);
            false
        };
        let shortcut = self.shortcut?;
        if key != shortcut.key {
            return None;
        }
        // Release depends only on the main key, in either Ctrl/Space release
        // order. Synthetic modifier cleanup cannot end or start a hold.
        if !pressed {
            return self.hold.update(false);
        }
        if !first_press {
            return None;
        }
        let mut modifiers = Modifiers::empty();
        for (left, right, modifier) in [
            (Code::ControlLeft, Code::ControlRight, Modifiers::CONTROL),
            (Code::ShiftLeft, Code::ShiftRight, Modifiers::SHIFT),
            (Code::AltLeft, Code::AltRight, Modifiers::ALT),
            (Code::MetaLeft, Code::MetaRight, Modifiers::SUPER),
        ] {
            if self.keys.contains(&left) || self.keys.contains(&right) {
                modifiers |= modifier;
            }
        }
        if modifiers == shortcut.mods {
            self.hold.update(true)
        } else {
            None
        }
    }
}

#[cfg(any(windows, test))]
impl Registration {
    fn sync_modifiers(&mut self, down: &[Code]) {
        self.keys.retain(|key| !MODIFIER_KEYS.contains(key));
        self.keys.extend(
            down.iter()
                .copied()
                .filter(|key| MODIFIER_KEYS.contains(key)),
        );
    }

    fn key_event(
        &mut self,
        key: Code,
        pressed: bool,
        injected: bool,
        was_down: bool,
        modifiers: Option<&[Code]>,
    ) -> [Option<HotkeyEvent>; 2] {
        // SendInput changes logical OS state, not the physically held keys.
        if injected {
            return [None, None];
        }
        let mut recovered = None;
        if pressed && self.shortcut.is_some_and(|shortcut| shortcut.key == key) {
            if was_down {
                self.keys.insert(key);
                return [None, None];
            }
            // The hook runs BEFORE the async state changes for this press.
            // A previous 'up' sample proves this is fresh, even if we missed
            // the prior key-up and still have a cached Pressed.
            self.keys.remove(&key);
            if self.hold.pressed {
                let Some(modifiers) = modifiers else {
                    // Pair the stale hold, but do not guess a new chord while
                    // synthetic input prevents a reliable recovery snapshot.
                    return [self.hold.update(false), None];
                };
                self.sync_modifiers(modifiers);
            }
            recovered = self.hold.update(false);
        }
        [recovered, self.raw_event(key, pressed)]
    }

    fn reconcile_main_key(
        &mut self,
        down: bool,
        modifiers: Option<&[Code]>,
    ) -> Option<HotkeyEvent> {
        if down {
            return None;
        }
        if let Some(shortcut) = self.shortcut {
            self.keys.remove(&shortcut.key);
        }
        if self.hold.pressed {
            if let Some(modifiers) = modifiers {
                self.sync_modifiers(modifiers);
            }
        }
        self.hold.update(false)
    }
}

impl Hotkey {
    pub fn new(app: &AppHandle, events: Sender<HotkeyEvent>) -> Result<Self, String> {
        let state = Arc::new(Mutex::new(Registration::default()));
        let plugin = tauri_plugin_global_shortcut::Builder::new();
        #[cfg(not(windows))]
        let plugin = {
            let handler_state = state.clone();
            plugin.with_handler(move |_, shortcut, event| {
                let mut state = handler_state.lock().unwrap_or_else(|e| e.into_inner());
                if state.shortcut.as_ref() == Some(shortcut) {
                    if let Some(event) = state.hold.update(
                        event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed,
                    ) {
                        let _ = events.send(event);
                    }
                }
            })
        };
        app.plugin(plugin.build()).map_err(|e| e.to_string())?;
        let hotkey = Self {
            app: app.clone(),
            state,
        };
        #[cfg(windows)]
        hotkey.start_listener(events)?;
        if let Err(error) = hotkey.set_shortcut(DEFAULT_SHORTCUT) {
            eprintln!("Parla default hotkey disabled: {error}");
        }
        Ok(hotkey)
    }

    /// Accepts supported Tauri accelerators (e.g. Ctrl+Space or Ctrl+Shift+F9).
    /// A failed registration preserves the current binding. Rebinding during a
    /// hold is rejected so it cannot manufacture a release (and a test paste).
    pub fn set_shortcut(&self, accelerator: &str) -> Result<(), String> {
        let shortcut = parse_shortcut(accelerator)?;
        let app = self.app.clone();
        let state = self.state.clone();
        let (reply, response) = mpsc::channel();
        // Serialize whole transactions on the main thread. Never hold callback
        // state while waiting for main-thread dispatch: a queued hotkey press
        // could otherwise deadlock registration against its own callback.
        self.app
            .run_on_main_thread(move || {
                let result = replace_shortcut(&app, &state, shortcut);
                let enabled = state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .shortcut
                    .is_some();
                update_tooltip(&app, enabled);
                let _ = reply.send(result);
            })
            .map_err(|e| e.to_string())?;
        response.recv().map_err(|e| e.to_string())?
    }

    #[cfg(windows)]
    fn start_listener(&self, events: Sender<HotkeyEvent>) -> Result<(), String> {
        let state = self.state.clone();
        let (ready, response) = mpsc::channel();
        std::thread::Builder::new()
            .name("parla-keys".into())
            .spawn(move || {
                if let Err(error) = windows_hook::run(state, events, ready) {
                    eprintln!("Parla keyboard hook stopped: {error}");
                }
            })
            .map_err(|e| e.to_string())?;
        response.recv().map_err(|e| e.to_string())?
    }
}

pub(crate) fn update_tooltip(app: &AppHandle, enabled: bool) {
    if let Some(tray) = app.tray_by_id("parla") {
        let tooltip = if enabled {
            "Parla"
        } else {
            "Parla — hotkey disabled (rebind in Settings)"
        };
        if let Err(error) = tray.set_tooltip(Some(tooltip)) {
            eprintln!("Parla tray tooltip failed: {error}");
        }
    }
}

fn replace_shortcut(
    app: &AppHandle,
    state: &Mutex<Registration>,
    shortcut: Shortcut,
) -> Result<(), String> {
    let previous = {
        let mut state = state.lock().map_err(|e| e.to_string())?;
        if state.shortcut == Some(shortcut) {
            return Ok(());
        }
        if state.hold.pressed {
            return Err("Release the current shortcut before rebinding".into());
        }
        // Pause dispatch during registration; raw physical-key state keeps
        // updating so held keys cannot become repeat-generated new presses.
        state.shortcut.take()
    };
    let backend = app.global_shortcut();
    let result = (|| {
        backend.register(shortcut).map_err(|e| e.to_string())?;
        if let Some(previous) = previous {
            if let Err(error) = backend.unregister(previous) {
                if let Err(rollback) = backend.unregister(shortcut) {
                    eprintln!("Parla shortcut rollback failed: {rollback}");
                }
                return Err(error.to_string());
            }
        }
        Ok(())
    })();
    state.lock().map_err(|e| e.to_string())?.shortcut = if result.is_ok() {
        Some(shortcut)
    } else {
        previous
    };
    result
}

fn parse_shortcut(accelerator: &str) -> Result<Shortcut, String> {
    let shortcut: Shortcut = accelerator
        .parse()
        .map_err(|e| format!("Invalid shortcut: {e}"))?;
    if !supported_code(shortcut.key) {
        return Err("Key is not supported by the Windows listener".into());
    }
    // RegisterHotKey would consume our synthetic paste and retrigger the flow.
    if shortcut == Shortcut::new(Some(Modifiers::CONTROL), Code::KeyV) {
        return Err("Ctrl+V is reserved for pasting".into());
    }
    Ok(shortcut)
}

// Virtual-key table adapted from rdev 0.5.3's Windows keycodes.rs (MIT).
// Copyright (c) 2020 Nicolas Patry. See THIRD_PARTY_NOTICES.md.
// Keep validation, decoding, and watchdog sampling in one table.
macro_rules! key_map {
    ($($vk:literal => $code:ident),* $(,)?) => {
        fn supported_code(code: Code) -> bool { matches!(code, $(Code::$code)|*) }
        #[cfg(any(windows, test))]
        fn virtual_key(code: Code) -> Option<i32> {
            match code { $(Code::$code => Some($vk),)* _ => None }
        }
        #[cfg(any(windows, test))]
        fn key_code(vk: u32) -> Option<Code> {
            match vk { $($vk => Some(Code::$code),)* _ => None }
        }
    };
}

key_map! {
    164 => AltLeft, 165 => AltRight, 162 => ControlLeft, 163 => ControlRight,
    160 => ShiftLeft, 161 => ShiftRight, 91 => MetaLeft, 0x5c => MetaRight,
    32 => Space, 0x09 => Tab, 27 => Escape, 0x08 => Backspace,
    46 => Delete, 45 => Insert, 36 => Home, 35 => End,
    33 => PageUp, 34 => PageDown, 20 => CapsLock, 144 => NumLock,
    44 => PrintScreen, 145 => ScrollLock, 19 => Pause, 0x0D => Enter,
    38 => ArrowUp, 40 => ArrowDown, 37 => ArrowLeft, 39 => ArrowRight,
    112 => F1, 113 => F2, 114 => F3, 115 => F4,
    116 => F5, 117 => F6, 118 => F7, 119 => F8,
    120 => F9, 121 => F10, 122 => F11, 123 => F12,
    65 => KeyA, 66 => KeyB, 67 => KeyC, 68 => KeyD,
    69 => KeyE, 70 => KeyF, 71 => KeyG, 72 => KeyH,
    73 => KeyI, 74 => KeyJ, 75 => KeyK, 76 => KeyL,
    77 => KeyM, 78 => KeyN, 79 => KeyO, 80 => KeyP,
    81 => KeyQ, 82 => KeyR, 83 => KeyS, 84 => KeyT,
    85 => KeyU, 86 => KeyV, 87 => KeyW, 88 => KeyX,
    89 => KeyY, 90 => KeyZ, 48 => Digit0, 49 => Digit1,
    50 => Digit2, 51 => Digit3, 52 => Digit4, 53 => Digit5,
    54 => Digit6, 55 => Digit7, 56 => Digit8, 57 => Digit9,
    192 => Backquote, 189 => Minus, 187 => Equal, 219 => BracketLeft,
    221 => BracketRight, 186 => Semicolon, 222 => Quote, 220 => Backslash,
    226 => IntlBackslash, 188 => Comma, 190 => Period, 191 => Slash,
}

/// One logical transition per hold, regardless of OS key repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
}

#[derive(Default)]
struct HoldState {
    pressed: bool,
}

impl HoldState {
    fn update(&mut self, pressed: bool) -> Option<HotkeyEvent> {
        if self.pressed == pressed {
            return None;
        }
        self.pressed = pressed;
        Some(if pressed {
            HotkeyEvent::Pressed
        } else {
            HotkeyEvent::Released
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_snapshot_cannot_cache_a_synthetic_ctrl_hold() {
        let input = synthetic_input_guard();
        assert_eq!(physical_snapshot(|| vec![Code::ControlLeft]), None);
        drop(input);
        assert_eq!(physical_snapshot(Vec::<Code>::new), Some(vec![]));
    }

    #[test]
    fn watchdog_recovery_removes_modifiers_released_while_hook_was_absent() {
        let mut state = registration();
        state.key_event(Code::ControlLeft, true, false, false, Some(&[]));
        state.key_event(Code::Space, true, false, false, Some(&[Code::ControlLeft]));
        assert_eq!(
            state.reconcile_main_key(false, Some(&[])),
            Some(HotkeyEvent::Released)
        );
        assert_eq!(
            state.key_event(Code::Space, true, false, false, Some(&[])),
            [None, None]
        );
        assert_eq!(
            state.key_event(Code::Space, false, false, true, Some(&[])),
            [None, None]
        );
    }

    #[test]
    fn fresh_press_recovery_does_not_reuse_a_stale_ctrl() {
        let mut state = registration();
        state.key_event(Code::ControlLeft, true, false, false, Some(&[]));
        state.key_event(Code::Space, true, false, false, Some(&[Code::ControlLeft]));
        assert_eq!(
            state.key_event(Code::Space, true, false, false, Some(&[])),
            [Some(HotkeyEvent::Released), None]
        );
    }

    #[test]
    fn virtual_key_mapping_covers_right_win_and_default_main_key() {
        assert_eq!(key_code(0x5c), Some(Code::MetaRight));
        assert_eq!(virtual_key(Code::Space), Some(32));
    }

    fn registration() -> Registration {
        Registration {
            shortcut: Some(parse_shortcut("Ctrl+Space").unwrap()),
            ..Default::default()
        }
    }

    #[test]
    fn injected_modifier_release_does_not_break_next_hold_with_ctrl_still_held() {
        let mut state = registration();
        state.key_event(
            Code::ControlLeft,
            true,
            false,
            false,
            Some(&[Code::ControlLeft]),
        );
        assert_eq!(
            state.key_event(Code::Space, true, false, false, Some(&[Code::ControlLeft])),
            [None, Some(HotkeyEvent::Pressed)]
        );
        state.key_event(Code::Space, false, false, true, Some(&[Code::ControlLeft]));
        state.key_event(
            Code::ControlLeft,
            false,
            true,
            true,
            Some(&[Code::ControlLeft]),
        );
        state.key_event(
            Code::ControlLeft,
            true,
            true,
            false,
            Some(&[Code::ControlLeft]),
        );
        state.key_event(
            Code::ControlLeft,
            false,
            true,
            true,
            Some(&[Code::ControlLeft]),
        );
        assert_eq!(
            state.key_event(Code::Space, true, false, false, Some(&[Code::ControlLeft])),
            [None, Some(HotkeyEvent::Pressed)]
        );
    }

    #[test]
    fn watchdog_closes_a_missed_release_once_and_allows_next_hold() {
        let mut state = registration();
        state.key_event(
            Code::ControlLeft,
            true,
            false,
            false,
            Some(&[Code::ControlLeft]),
        );
        state.key_event(Code::Space, true, false, false, Some(&[Code::ControlLeft]));
        assert_eq!(
            state.reconcile_main_key(true, Some(&[Code::ControlLeft])),
            None
        );
        assert_eq!(
            state.reconcile_main_key(false, Some(&[Code::ControlLeft])),
            Some(HotkeyEvent::Released)
        );
        assert_eq!(
            state.reconcile_main_key(false, Some(&[Code::ControlLeft])),
            None
        );
        assert_eq!(
            state.key_event(Code::Space, true, false, false, Some(&[Code::ControlLeft])),
            [None, Some(HotkeyEvent::Pressed)]
        );
        assert_eq!(
            state.key_event(Code::Space, false, false, true, Some(&[Code::ControlLeft])),
            [None, Some(HotkeyEvent::Released)]
        );
    }

    #[test]
    fn fresh_press_reconciles_a_missed_release_before_next_press() {
        let mut state = registration();
        state.key_event(
            Code::ControlLeft,
            true,
            false,
            false,
            Some(&[Code::ControlLeft]),
        );
        state.key_event(Code::Space, true, false, false, Some(&[Code::ControlLeft]));
        // Physical up was missed, but the pre-event async state is now up.
        assert_eq!(
            state.key_event(Code::Space, true, false, false, Some(&[Code::ControlLeft])),
            [Some(HotkeyEvent::Released), Some(HotkeyEvent::Pressed)]
        );
        assert_eq!(
            state.key_event(Code::Space, true, false, true, Some(&[Code::ControlLeft])),
            [None, None]
        );
    }

    #[test]
    fn extra_modifiers_do_not_activate_a_different_accelerator() {
        let mut state = Registration {
            shortcut: Some(parse_shortcut("Ctrl+Space").unwrap()),
            ..Default::default()
        };
        state.raw_event(Code::ControlLeft, true);
        state.raw_event(Code::MetaRight, true);
        assert_eq!(state.raw_event(Code::Space, true), None);
        assert_eq!(state.raw_event(Code::Space, false), None);
    }

    #[test]
    fn raw_events_preserve_rapid_holds_without_polling() {
        let mut state = Registration {
            shortcut: Some(parse_shortcut("Ctrl+Space").unwrap()),
            ..Default::default()
        };
        assert_eq!(state.raw_event(Code::ControlLeft, true), None);
        let events: Vec<_> = [true, true, false, true, false]
            .into_iter()
            .filter_map(|down| state.raw_event(Code::Space, down))
            .collect();
        assert_eq!(
            events,
            [
                HotkeyEvent::Pressed,
                HotkeyEvent::Released,
                HotkeyEvent::Pressed,
                HotkeyEvent::Released
            ]
        );
    }

    #[test]
    fn raw_release_works_for_both_modifier_release_orders() {
        for control in [Code::ControlLeft, Code::ControlRight] {
            for ctrl_first in [true, false] {
                let mut state = Registration {
                    shortcut: Some(parse_shortcut("Ctrl+Space").unwrap()),
                    ..Default::default()
                };
                state.raw_event(control, true);
                assert_eq!(
                    state.raw_event(Code::Space, true),
                    Some(HotkeyEvent::Pressed)
                );
                if ctrl_first {
                    assert_eq!(state.raw_event(control, false), None);
                }
                assert_eq!(
                    state.raw_event(Code::Space, false),
                    Some(HotkeyEvent::Released)
                );
                if !ctrl_first {
                    assert_eq!(state.raw_event(control, false), None);
                }
            }
        }
    }

    #[test]
    fn main_key_repeat_cannot_start_a_hold_after_modifiers_are_added() {
        let mut state = Registration {
            shortcut: Some(parse_shortcut("Ctrl+Space").unwrap()),
            ..Default::default()
        };
        assert_eq!(state.raw_event(Code::Space, true), None);
        state.raw_event(Code::ControlLeft, true);
        assert_eq!(state.raw_event(Code::Space, true), None);
        assert_eq!(state.raw_event(Code::Space, false), None);
        assert_eq!(
            state.raw_event(Code::Space, true),
            Some(HotkeyEvent::Pressed)
        );
    }

    #[test]
    fn rejects_invalid_accelerators_and_the_synthetic_paste_chord() {
        for accelerator in ["", "Ctrl+NotAKey", "Ctrl+V", "Control+KeyV"] {
            assert!(parse_shortcut(accelerator).is_err(), "{accelerator}");
        }
        assert!(parse_shortcut("Ctrl+Space").is_ok());
        assert!(parse_shortcut("Ctrl+Shift+F9").is_ok());
    }

    #[test]
    fn emits_one_pair_per_hold_despite_repeat_and_duplicate_release() {
        let mut state = HoldState::default();
        let events: Vec<_> = [false, true, true, true, false, false, true, false]
            .into_iter()
            .filter_map(|pressed| state.update(pressed))
            .collect();
        assert_eq!(
            events,
            [
                HotkeyEvent::Pressed,
                HotkeyEvent::Released,
                HotkeyEvent::Pressed,
                HotkeyEvent::Released
            ]
        );
    }

    #[test]
    fn release_does_not_require_modifiers_to_still_be_down() {
        let mut state = HoldState::default();
        assert_eq!(state.update(true), Some(HotkeyEvent::Pressed));
        // The Windows backend reports main-key release independently of Ctrl.
        assert_eq!(state.update(false), Some(HotkeyEvent::Released));
    }
}

#[cfg(windows)]
mod windows_hook {
    use super::*;
    use std::{cell::RefCell, ptr::null_mut};
    use windows_sys::Win32::{
        Foundation::{LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Input::KeyboardAndMouse::GetAsyncKeyState, WindowsAndMessaging::*},
    };

    struct Context {
        state: Arc<Mutex<Registration>>,
        events: Sender<HotkeyEvent>,
    }

    thread_local! {
        // Windows calls a low-level hook on its installing message-loop thread.
        static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
    }

    fn modifier_snapshot() -> Option<Vec<Code>> {
        physical_snapshot(|| {
            MODIFIER_KEYS
                .into_iter()
                .filter(|key| {
                    // SAFETY: all entries have a valid virtual-key mapping. Snapshot
                    // only at startup/recovery; normal events retain physical state so
                    // injected cleanup cannot make a held modifier appear released.
                    (unsafe { GetAsyncKeyState(virtual_key(*key).expect("modifier mapping")) }) < 0
                })
                .collect()
        })
    }

    struct Hook(HHOOK);

    impl Hook {
        fn install() -> Result<Self, String> {
            // SAFETY: callback has the required ABI, and the module handle is
            // valid for this process. The hook is owned/unhooked on this thread.
            let hook = unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(callback),
                    GetModuleHandleW(null_mut()),
                    0,
                )
            };
            if hook.is_null() {
                Err(std::io::Error::last_os_error().to_string())
            } else {
                Ok(Self(hook))
            }
        }
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            // SAFETY: self owns this hook. A timeout may already have removed it.
            unsafe {
                UnhookWindowsHookEx(self.0);
            }
        }
    }

    unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: HC_ACTION keyboard notifications provide KBDLLHOOKSTRUCT
            // at lparam, valid for the duration of this callback.
            let input = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            if input.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) == 0 {
                let pressed = match wparam as u32 {
                    WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
                    WM_KEYUP | WM_SYSKEYUP => Some(false),
                    _ => None,
                };
                if let (Some(key), Some(pressed)) = (key_code(input.vkCode), pressed) {
                    // This is the PRE-event logical state, not the new press.
                    // https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc
                    let was_down = unsafe { GetAsyncKeyState(input.vkCode as i32) } < 0;
                    CONTEXT.with(|context| {
                        if let Some(context) = context.borrow().as_ref() {
                            let mut state = context.state.lock().unwrap_or_else(|e| e.into_inner());
                            for event in state
                                .key_event(
                                    key,
                                    pressed,
                                    false,
                                    was_down,
                                    modifier_snapshot().as_deref(),
                                )
                                .into_iter()
                                .flatten()
                            {
                                let _ = context.events.send(event);
                            }
                        }
                    });
                }
            }
        }
        // SAFETY: every event is forwarded; Tauri's RegisterHotKey reserves the
        // accelerator. No mouse hook or text translation is installed here.
        unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
    }

    pub(super) fn run(
        state: Arc<Mutex<Registration>>,
        events: Sender<HotkeyEvent>,
        ready: Sender<Result<(), String>>,
    ) -> Result<(), String> {
        CONTEXT.with(|context| {
            *context.borrow_mut() = Some(Context {
                state: state.clone(),
                events: events.clone(),
            })
        });
        let mut hook = match Hook::install() {
            Ok(hook) => hook,
            Err(error) => {
                let _ = ready.send(Err(error.clone()));
                return Err(error);
            }
        };
        // SAFETY: a thread-owned timer posts WM_TIMER to this message queue.
        let timer = unsafe { SetTimer(null_mut(), 0, 100, None) };
        if timer == 0 {
            let error = std::io::Error::last_os_error().to_string();
            let _ = ready.send(Err(error.clone()));
            return Err(error);
        }
        if let Some(snapshot) = modifier_snapshot() {
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sync_modifiers(&snapshot);
        }
        let _ = ready.send(Ok(()));
        let mut ticks = 0_u32;
        loop {
            // SAFETY: initialized output storage; no HWND filter means all
            // messages for this dedicated thread, including keyboard hooks.
            let mut message = unsafe { std::mem::zeroed::<MSG>() };
            let status = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
            if status <= 0 {
                break;
            }
            if message.message == WM_TIMER && message.wParam == timer {
                ticks = ticks.wrapping_add(1);
                let Some(snapshot) = modifier_snapshot() else {
                    continue;
                };
                let (recovered, active) = {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    let down = state
                        .shortcut
                        .and_then(|shortcut| virtual_key(shortcut.key))
                        .is_some_and(|vk| unsafe { GetAsyncKeyState(vk) } < 0);
                    let recovered = state.reconcile_main_key(down, Some(&snapshot));
                    if let Some(event) = recovered {
                        let _ = events.send(event);
                    }
                    (recovered.is_some(), state.hold.pressed)
                };
                // Reinstall after a lost release (possibly timeout removal),
                // and periodically while idle to recover silent hook removal.
                // Install first so failure leaves the existing hook intact.
                if recovered || (!active && ticks.is_multiple_of(50)) {
                    match Hook::install() {
                        Ok(replacement) => {
                            hook = replacement;
                            state
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .sync_modifiers(&snapshot);
                        }
                        Err(error) => eprintln!("Parla keyboard hook refresh failed: {error}"),
                    }
                }
            } else {
                // SAFETY: message was obtained from this thread's message queue.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
        // Pair a final release before stopping if the message loop terminates.
        if let Some(event) = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reconcile_main_key(false, Some(&[]))
        {
            let _ = events.send(event);
        }
        // SAFETY: timer belongs to this thread and is no longer needed.
        unsafe {
            KillTimer(null_mut(), timer);
        }
        drop(hook);
        Err("keyboard message loop ended".into())
    }
}
