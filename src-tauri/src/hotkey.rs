//! Global hold-to-talk shortcut emitting pressed and released events.
//!
//! Windows uses rdev's low-level key events: global-hotkey 0.8.0 polls release
//! every 50 ms and can merge rapid holds. Tauri reserves the accelerator with
//! RegisterHotKey, but its polled events are ignored on Windows.
//! https://docs.rs/crate/global-hotkey/0.8.0/source/src/platform_impl/windows/mod.rs
//! https://docs.rs/crate/rdev/0.5.3/source/src/windows/listen.rs

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
        hotkey.set_shortcut(DEFAULT_SHORTCUT)?;
        #[cfg(windows)]
        hotkey.start_listener(events)?;
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
                let _ = reply.send(result);
            })
            .map_err(|e| e.to_string())?;
        response.recv().map_err(|e| e.to_string())?
    }

    #[cfg(windows)]
    fn start_listener(&self, events: Sender<HotkeyEvent>) -> Result<(), String> {
        let state = self.state.clone();
        let app = self.app.clone();
        std::thread::Builder::new()
            .name("parla-keys".into())
            .spawn(move || {
                let result = rdev::listen(move |event| {
                    let (key, pressed) = match event.event_type {
                        rdev::EventType::KeyPress(key) => (key, true),
                        rdev::EventType::KeyRelease(key) => (key, false),
                        _ => return,
                    };
                    if let Some(key) = rdev_code(key) {
                        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(event) = state.raw_event(key, pressed) {
                            let _ = events.send(event);
                        }
                    }
                });
                // rdev has no ready/shutdown handshake. It lives until app exit;
                // hook installation failure or unexpected listener exit is fatal.
                eprintln!("Parla keyboard listener stopped: {result:?}");
                app.exit(1);
            })
            .map_err(|e| e.to_string())?;
        Ok(())
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

// Keep validation and rdev conversion in one table; reject accelerators whose
// main key we cannot observe, instead of accepting a hold that never releases.
macro_rules! key_map {
    ($($raw:ident => $code:ident),* $(,)?) => {
        fn supported_code(code: Code) -> bool { matches!(code, $(Code::$code)|*) }
        #[cfg(windows)]
        fn rdev_code(key: rdev::Key) -> Option<Code> {
            match key {
                $(rdev::Key::$raw => Some(Code::$code),)*
                // rdev 0.5.3 omits VK_RWIN from its Windows keycodes table.
                rdev::Key::Unknown(0x5c) => Some(Code::MetaRight),
                _ => None,
            }
        }
    };
}

key_map! {
    Alt => AltLeft, AltGr => AltRight, ControlLeft => ControlLeft, ControlRight => ControlRight,
    ShiftLeft => ShiftLeft, ShiftRight => ShiftRight, MetaLeft => MetaLeft, MetaRight => MetaRight,
    Space => Space, Tab => Tab, Escape => Escape, Backspace => Backspace,
    Delete => Delete, Insert => Insert, Home => Home, End => End,
    PageUp => PageUp, PageDown => PageDown, CapsLock => CapsLock, NumLock => NumLock,
    PrintScreen => PrintScreen, ScrollLock => ScrollLock, Pause => Pause, Return => Enter,
    UpArrow => ArrowUp, DownArrow => ArrowDown, LeftArrow => ArrowLeft, RightArrow => ArrowRight,
    F1 => F1, F2 => F2, F3 => F3, F4 => F4,
    F5 => F5, F6 => F6, F7 => F7, F8 => F8,
    F9 => F9, F10 => F10, F11 => F11, F12 => F12,
    KeyA => KeyA, KeyB => KeyB, KeyC => KeyC, KeyD => KeyD,
    KeyE => KeyE, KeyF => KeyF, KeyG => KeyG, KeyH => KeyH,
    KeyI => KeyI, KeyJ => KeyJ, KeyK => KeyK, KeyL => KeyL,
    KeyM => KeyM, KeyN => KeyN, KeyO => KeyO, KeyP => KeyP,
    KeyQ => KeyQ, KeyR => KeyR, KeyS => KeyS, KeyT => KeyT,
    KeyU => KeyU, KeyV => KeyV, KeyW => KeyW, KeyX => KeyX,
    KeyY => KeyY, KeyZ => KeyZ, Num0 => Digit0, Num1 => Digit1,
    Num2 => Digit2, Num3 => Digit3, Num4 => Digit4, Num5 => Digit5,
    Num6 => Digit6, Num7 => Digit7, Num8 => Digit8, Num9 => Digit9,
    BackQuote => Backquote, Minus => Minus, Equal => Equal, LeftBracket => BracketLeft,
    RightBracket => BracketRight, SemiColon => Semicolon, Quote => Quote, BackSlash => Backslash,
    IntlBackslash => IntlBackslash, Comma => Comma, Dot => Period, Slash => Slash,
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

    #[cfg(windows)]
    #[test]
    fn maps_the_right_windows_key_omitted_by_rdev() {
        assert_eq!(rdev_code(rdev::Key::Unknown(0x5c)), Some(Code::MetaRight));
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
