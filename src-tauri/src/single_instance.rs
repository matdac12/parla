//! One Parla per Windows user (PARLA-102). Before any window, tray icon or
//! keyboard hook, `claim` creates a named mutex in the session's `Local\`
//! namespace. A second launch finds it, signals the running Parla through a
//! named event ("show the main window") and exits with code 0.
//!
//! The running Parla waits on that event through the system thread pool
//! (`RegisterWaitForSingleObject`): no thread of its own, no polling, and
//! nothing on the hotkey-to-paste path. The event is created before the mutex,
//! so a second launch during the first one's startup still finds it; a signal
//! sent before `watch` registers stays set until it does.

/// What `claim` found.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// No other Parla: carry on starting.
    First,
    /// Another Parla owns the mutex and has been asked to show its window.
    Running,
}

/// Debug builds only: `PARLA_INSTANCE=<name>` makes a separate instance, so a
/// development build can run beside the installed copy.
#[cfg(debug_assertions)]
fn instance_suffix() -> String {
    std::env::var_os("PARLA_INSTANCE")
        .map(|name| {
            let name: String = name
                .to_string_lossy()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format!(".{name}")
        })
        .unwrap_or_default()
}

#[cfg(not(debug_assertions))]
fn instance_suffix() -> String {
    String::new()
}

fn names() -> (String, String) {
    let base = format!("Local\\parla.single-instance{}", instance_suffix());
    let show = format!("{base}.show");
    (base, show)
}

/// Takes the per-user mutex. Fails open: if Windows will not create it for any
/// reason but "someone else owns it", Parla starts as the first copy.
pub fn claim() -> Claim {
    let (mutex, event) = names();
    imp::claim(&mutex, &event)
}

/// Starts calling `on_show` whenever another launch asks for the window. Call
/// it once the app can open windows.
pub fn watch(on_show: impl Fn() + Send + Sync + 'static) {
    imp::watch(Box::new(on_show));
}

#[cfg(windows)]
mod imp {
    use super::Claim;
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, RegisterWaitForSingleObject, SetEvent, INFINITE,
        WT_EXECUTEDEFAULT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The first copy's handles. They live until the process ends (the static
    /// never drops); a handle is kept as an integer so the static is `Sync`.
    pub(super) struct Held {
        mutex: usize,
        pub event: usize,
    }

    impl Drop for Held {
        fn drop(&mut self) {
            close(self.event as HANDLE);
            close(self.mutex as HANDLE);
        }
    }

    static HELD: OnceLock<Held> = OnceLock::new();

    pub(super) fn claim(mutex: &str, event: &str) -> Claim {
        match acquire(mutex, event) {
            Outcome::First(held) => {
                let _ = HELD.set(held);
                Claim::First
            }
            Outcome::Running => Claim::Running,
            Outcome::Unavailable => Claim::First,
        }
    }

    pub(super) enum Outcome {
        First(Held),
        Running,
        Unavailable,
    }

    /// The seam: `claim` with the real names, tests with their own.
    pub(super) fn acquire(mutex: &str, event: &str) -> Outcome {
        let (mutex, event) = (wide(mutex), wide(event));
        // SAFETY: NUL-terminated names that outlive the calls; no attributes.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, event.as_ptr()) };
        // CreateEventW may have left ERROR_ALREADY_EXISTS behind.
        // SAFETY: as above.
        unsafe { SetLastError(0) };
        // SAFETY: as above.
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, mutex.as_ptr()) };
        // Read before any other call can overwrite it.
        let error = unsafe { GetLastError() };
        if mutex.is_null() {
            close(event);
            // Owned by a process we may not open (an elevated Parla): running.
            return if error == ERROR_ACCESS_DENIED {
                Outcome::Running
            } else {
                Outcome::Unavailable
            };
        }
        if error == ERROR_ALREADY_EXISTS {
            if !event.is_null() {
                // SAFETY: a live event handle. The user just launched this
                // process, so it may hand the foreground to the running one.
                unsafe {
                    AllowSetForegroundWindow(ASFW_ANY);
                    SetEvent(event);
                }
            }
            close(event);
            close(mutex);
            return Outcome::Running;
        }
        Outcome::First(Held {
            mutex: mutex as usize,
            event: event as usize,
        })
    }

    fn close(handle: HANDLE) {
        if !handle.is_null() {
            // SAFETY: a handle this module created and no longer uses.
            unsafe { CloseHandle(handle) };
        }
    }

    type Callback = Box<dyn Fn() + Send + Sync>;

    unsafe extern "system" fn on_signal(context: *mut c_void, _timed_out: bool) {
        // SAFETY: `context` is the leaked `Box<Callback>` from `register`.
        let callback = unsafe { &*(context as *const Callback) };
        callback();
    }

    pub(super) fn watch(on_show: Callback) {
        if let Some(held) = HELD.get().filter(|held| held.event != 0) {
            register(held.event as HANDLE, on_show);
        }
    }

    /// Calls `on_show` on a pool thread each time `event` is set. The callback
    /// is leaked on purpose: the wait lasts as long as the process.
    pub(super) fn register(event: HANDLE, on_show: Callback) -> Option<usize> {
        let context = Box::into_raw(Box::new(on_show));
        let mut wait: HANDLE = std::ptr::null_mut();
        // SAFETY: a live auto-reset event, a callback matching the pool's
        // signature, and a context that is never freed.
        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                event,
                Some(on_signal),
                context as *const c_void,
                INFINITE,
                WT_EXECUTEDEFAULT,
            )
        };
        if ok == 0 {
            // SAFETY: registration failed, so nothing else holds the pointer.
            drop(unsafe { Box::from_raw(context) });
            None
        } else {
            Some(wait as usize)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{UnregisterWaitEx, WaitForSingleObject};

        fn names(tag: &str) -> (String, String) {
            let base = format!("Local\\parla.test.{}.{tag}", std::process::id());
            (base.clone(), format!("{base}.show"))
        }

        #[test]
        fn second_claim_finds_the_first_signals_it_and_a_released_name_is_free_again() {
            let (mutex, event) = names("claim");
            let started = Instant::now();
            let Outcome::First(first) = acquire(&mutex, &event) else {
                panic!("nothing owns the name yet");
            };
            println!("first claim took {:?}", started.elapsed());

            let started = Instant::now();
            assert!(matches!(acquire(&mutex, &event), Outcome::Running));
            println!("second claim took {:?}", started.elapsed());
            // SAFETY: the first claim's live event handle.
            let signalled = unsafe { WaitForSingleObject(first.event as HANDLE, 0) };
            assert_eq!(signalled, WAIT_OBJECT_0, "the running copy was not asked");

            drop(first);
            let Outcome::First(again) = acquire(&mutex, &event) else {
                panic!("the name was not freed");
            };
            drop(again);
        }

        #[test]
        fn names_differ_so_instances_do_not_collide() {
            let (a, a_event) = names("a");
            let (b, b_event) = names("b");
            let Outcome::First(first) = acquire(&a, &a_event) else {
                panic!("a is free");
            };
            let Outcome::First(second) = acquire(&b, &b_event) else {
                panic!("b is free");
            };
            drop(first);
            drop(second);
        }

        #[test]
        fn a_signal_reaches_the_watcher_even_when_sent_before_it_registered() {
            let (mutex, event) = names("watch");
            let Outcome::First(first) = acquire(&mutex, &event) else {
                panic!("nothing owns the name yet");
            };
            // The second launch comes first, the watcher after.
            assert!(matches!(acquire(&mutex, &event), Outcome::Running));
            let (sender, receiver) = mpsc::channel();
            let started = Instant::now();
            let wait = register(
                first.event as HANDLE,
                Box::new(move || {
                    let _ = sender.send(());
                }),
            )
            .expect("registered");
            println!("registering the wait took {:?}", started.elapsed());
            receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("the watcher was not called");
            // The event is auto-reset: one signal, one call.
            assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
            // SAFETY: the wait registered above; INVALID_HANDLE_VALUE (-1)
            // waits for a running callback before returning.
            unsafe { UnregisterWaitEx(wait as HANDLE, -1isize as HANDLE) };
            drop(first);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Claim;

    pub(super) fn claim(_mutex: &str, _event: &str) -> Claim {
        Claim::First
    }

    pub(super) fn watch(_on_show: Box<dyn Fn() + Send + Sync>) {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_are_per_user_session_and_share_a_base() {
        let (mutex, event) = super::names();
        assert!(mutex.starts_with("Local\\parla.single-instance"));
        assert_eq!(event, format!("{mutex}.show"));
    }
}
