//! Dims other apps' audio (music) while dictating, never pausing it (PARLA-42).
//!
//! The dictation thread only sends a request on a channel. A worker owns the
//! COM objects and the fade, so dimming adds nothing to the hotkey-to-paste
//! path and never delays the mic. Each session of the default output device
//! (except Parla's own and system sounds) is faded to a share of its volume,
//! then back to exactly what it had, unless the user moved it meanwhile.
//!
//! While dimmed, the original volumes are saved to [`FILE_NAME`] in Parla's
//! config folder, so a crash or kill is undone at the next start.
// Only the Windows worker drives the fade; elsewhere `dim` is a no-op.
#![cfg_attr(not(windows), allow(dead_code))]
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{mpsc, OnceLock},
    time::{Duration, Instant},
};

/// Each session plays at this percent of its volume while dictating: 50 %
/// amplitude is about -6 dB, clearly lower yet still present.
pub const DEFAULT_LEVEL_PERCENT: u8 = 50;
/// Fade length each way: short enough to finish before the first word, long
/// enough not to sound like a cut.
const FADE: Duration = Duration::from_millis(120);
/// Fade step. Progress follows real elapsed time, so coarse timers only mean
/// fewer, larger steps.
const TICK: Duration = Duration::from_millis(10);
/// A volume read back within this of what Parla set is still Parla's; the
/// Volume Mixer moves in 1 % steps.
const TOLERANCE: f32 = 0.005;
pub const FILE_NAME: &str = "dimmed-audio.json";

/// The dimmed share of a session's volume for a Settings percent. Never mute.
pub fn level(percent: u8) -> f32 {
    f32::from(percent.clamp(10, 90)) / 100.0
}

/// One audio session's volume (ISimpleAudioVolume on Windows).
trait Volume {
    fn get(&self) -> Option<f32>;
    fn set(&self, value: f32) -> bool;
}

/// A session on the default output device.
struct Found<V> {
    pid: u32,
    system_sounds: bool,
    /// Unique to this session instance (includes the process id).
    instance: String,
    /// Stable for the app across restarts; Windows remembers volume by it.
    app: String,
    volume: V,
}

/// What [`FILE_NAME`] holds per dimmed session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Saved {
    instance: String,
    app: String,
    original: f32,
    dimmed: f32,
}

struct Tracked<V> {
    volume: V,
    original: f32,
    /// What Parla last set; anything else means the user moved it.
    last: f32,
}

impl<V: Volume> Tracked<V> {
    /// Sets `value`, or returns false (stop touching this session) when the
    /// session is gone or the user changed its volume since Parla last set it.
    fn apply(&mut self, value: f32) -> bool {
        match self.volume.get() {
            Some(now) if (now - self.last).abs() <= TOLERANCE => {}
            _ => return false,
        }
        if (value - self.last).abs() > f32::EPSILON && !self.volume.set(value) {
            return false;
        }
        self.last = value;
        true
    }
}

/// The fade state. `depth` 0 is full volume, 1 fully dimmed; the gain is
/// `level ^ depth`, so the fade is even in dB.
struct Dimmer<V> {
    tracked: Vec<Tracked<V>>,
    level: f32,
    depth: f32,
    dimming: bool,
    file: PathBuf,
}

impl<V: Volume> Dimmer<V> {
    fn new(file: PathBuf) -> Self {
        Self {
            tracked: Vec::new(),
            level: 1.0,
            depth: 0.0,
            dimming: false,
            file,
        }
    }

    fn is_idle(&self) -> bool {
        self.tracked.is_empty()
    }

    fn fading(&self) -> bool {
        !self.tracked.is_empty()
            && if self.dimming {
                self.depth < 1.0
            } else {
                self.depth > 0.0
            }
    }

    /// Starts dimming. `found` is only needed when idle; a dictation that
    /// starts during the previous fade-up just turns the fade around.
    fn dim(&mut self, found: Vec<Found<V>>, own_pid: u32, level: f32) {
        if self.is_idle() {
            self.level = level;
            self.depth = 0.0;
            let mut saved = Vec::new();
            for session in found {
                if session.pid == own_pid || session.system_sounds {
                    continue;
                }
                let Some(original) = session.volume.get() else {
                    continue;
                };
                saved.push(Saved {
                    instance: session.instance,
                    app: session.app,
                    original,
                    dimmed: original * level,
                });
                self.tracked.push(Tracked {
                    volume: session.volume,
                    original,
                    last: original,
                });
            }
            if !saved.is_empty() {
                if let Err(error) = write_saved(&self.file, &saved) {
                    eprintln!("dim: saving original volumes failed: {error}");
                }
            }
        }
        self.dimming = !self.tracked.is_empty();
    }

    fn restore(&mut self) {
        self.dimming = false;
    }

    /// Advances the fade by `elapsed`.
    fn step(&mut self, elapsed: Duration) {
        if self.tracked.is_empty() {
            return;
        }
        let delta = elapsed.as_secs_f32() / FADE.as_secs_f32();
        self.depth = if self.dimming {
            (self.depth + delta).min(1.0)
        } else {
            (self.depth - delta).max(0.0)
        };
        let gain = self.level.powf(self.depth);
        self.tracked
            .retain_mut(|session| session.apply(session.original * gain));
        if self.tracked.is_empty() || (!self.dimming && self.depth == 0.0) {
            self.release();
        }
    }

    /// Restores at once (Parla is quitting).
    fn restore_now(&mut self) {
        self.dimming = false;
        self.depth = 0.0;
        for session in &mut self.tracked {
            session.apply(session.original);
        }
        self.release();
    }

    fn release(&mut self) {
        self.tracked.clear();
        self.depth = 0.0;
        self.dimming = false;
        remove_saved(&self.file);
    }
}

fn write_saved(file: &Path, saved: &[Saved]) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = file.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(saved)?)?;
    std::fs::rename(&temporary, file)
}

fn remove_saved(file: &Path) {
    match std::fs::remove_file(file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("dim: removing {FILE_NAME} failed: {error}"),
    }
}

/// Undoes a dim left by a crash or kill: each saved session still between its
/// dimmed and original volume goes back to the original; one the user moved
/// elsewhere is left alone. Lists sessions only when the file exists.
fn restore_crashed<V: Volume>(file: &Path, sessions: impl FnOnce() -> Vec<Found<V>>) {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!("dim: reading {FILE_NAME} failed: {error}");
            return;
        }
    };
    let saved: Vec<Saved> = serde_json::from_slice(&bytes).unwrap_or_default();
    let mut live: Vec<Option<Found<V>>> = sessions().into_iter().map(Some).collect();
    // The same instance if it still runs, else the app's new session (Windows
    // kept the dimmed volume for it). All instances first, so a dead entry's
    // app fallback cannot take a live entry's own session.
    let mut matched: Vec<Option<Found<V>>> = saved.iter().map(|_| None).collect();
    for by_instance in [true, false] {
        for (entry, slot) in saved.iter().zip(&mut matched) {
            if slot.is_some() {
                continue;
            }
            let key = |session: &Found<V>| {
                if by_instance {
                    !entry.instance.is_empty() && session.instance == entry.instance
                } else {
                    !entry.app.is_empty() && session.app == entry.app
                }
            };
            if let Some(index) = live.iter().position(|s| s.as_ref().is_some_and(key)) {
                *slot = live[index].take();
            }
        }
    }
    let mut restored = 0;
    for (entry, session) in saved.iter().zip(matched) {
        let Some(session) = session else {
            continue;
        };
        let ours = session.volume.get().is_some_and(|now| {
            now >= entry.dimmed - TOLERANCE && now <= entry.original + TOLERANCE
        });
        if ours && session.volume.set(entry.original) {
            restored += 1;
        }
    }
    eprintln!(
        "dim: restored {restored} of {} sessions left dimmed",
        saved.len()
    );
    remove_saved(file);
}

enum Request {
    Dim(f32),
    Restore,
    Exit(mpsc::Sender<()>),
}

/// Serves requests until Parla quits. `sessions(active_only)` lists the
/// default output device's sessions.
fn serve<V: Volume>(
    file: PathBuf,
    requests: mpsc::Receiver<Request>,
    own_pid: u32,
    sessions: impl Fn(bool) -> Vec<Found<V>>,
) {
    restore_crashed(&file, || sessions(false));
    let mut dimmer = Dimmer::new(file);
    let mut last = Instant::now();
    loop {
        let fading = dimmer.fading();
        let request = if fading {
            requests.recv_timeout(TICK)
        } else {
            requests
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        if !fading {
            // A fade starts now, not when the last one ended.
            last = Instant::now();
        }
        match request {
            Ok(Request::Dim(level)) => {
                // Paused players keep their session but are not dimmed.
                let found = if dimmer.is_idle() {
                    sessions(true)
                } else {
                    Vec::new()
                };
                dimmer.dim(found, own_pid, level);
                last = Instant::now();
            }
            Ok(Request::Restore) => dimmer.restore(),
            Ok(Request::Exit(done)) => {
                dimmer.restore_now();
                let _ = done.send(());
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                dimmer.restore_now();
                return;
            }
        }
        let now = Instant::now();
        dimmer.step(now.saturating_duration_since(last));
        last = now;
    }
}

static REQUESTS: OnceLock<mpsc::Sender<Request>> = OnceLock::new();

fn send(request: Request) -> bool {
    REQUESTS
        .get()
        .is_some_and(|requests| requests.send(request).is_ok())
}

/// Starts the worker, which first undoes a dim left by a crash. `config_dir`
/// is Parla's config folder. A no-op off Windows.
pub fn setup(config_dir: &Path) {
    #[cfg(windows)]
    {
        let file = config_dir.join(FILE_NAME);
        let (sender, requests) = mpsc::channel();
        match std::thread::Builder::new()
            .name("parla-dim".into())
            .spawn(move || wasapi::run(file, requests))
        {
            Ok(_) => {
                let _ = REQUESTS.set(sender);
            }
            Err(error) => eprintln!("dim: cannot start worker: {error}"),
        }
    }
    #[cfg(not(windows))]
    let _ = config_dir;
}

/// Restores other apps' volume when dropped.
pub struct Dimmed(bool);

impl Drop for Dimmed {
    fn drop(&mut self) {
        if self.0 {
            send(Request::Restore);
        }
    }
}

/// Fades other apps' audio down until the returned guard drops, when the
/// setting is on. Only a channel send: safe on the dictation path.
pub fn dim(settings: &crate::settings::Settings) -> Dimmed {
    Dimmed(settings.dim_other_audio && send(Request::Dim(level(settings.dim_audio_level))))
}

/// Restores at once on quit, waiting briefly for the worker.
pub fn shutdown() {
    let (done, finished) = mpsc::channel();
    if send(Request::Exit(done)) {
        let _ = finished.recv_timeout(Duration::from_millis(500));
    }
}

#[cfg(windows)]
mod wasapi {
    use super::{Found, Request, Volume};
    use std::{path::PathBuf, sync::mpsc};
    use windows::{
        core::{Interface, GUID, PWSTR},
        Win32::{
            Foundation::S_OK,
            Media::Audio::{
                eConsole, eRender, AudioSessionStateActive, AudioSessionStateExpired,
                IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
                ISimpleAudioVolume, MMDeviceEnumerator,
            },
            System::Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED,
            },
        },
    };

    /// Tags Parla's changes in other apps' volume notifications.
    const CONTEXT: GUID = GUID::from_u128(0x5041524c_412d_3432_8000_64696d617564);

    pub(super) struct Session(ISimpleAudioVolume);

    impl Volume for Session {
        fn get(&self) -> Option<f32> {
            // SAFETY: a live COM interface, used on the worker that created it.
            unsafe { self.0.GetMasterVolume() }.ok()
        }
        fn set(&self, value: f32) -> bool {
            // SAFETY: as above; CONTEXT outlives the call.
            unsafe { self.0.SetMasterVolume(value.clamp(0.0, 1.0), &CONTEXT) }.is_ok()
        }
    }

    pub(super) fn run(file: PathBuf, requests: mpsc::Receiver<Request>) {
        // SAFETY: the first COM call on this dedicated thread, which lives as
        // long as Parla, so it never uninitializes.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_err() {
            eprintln!("dim: COM init failed: {result}");
            return;
        }
        // SAFETY: COM is initialized on this thread.
        let devices: IMMDeviceEnumerator =
            match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
                Ok(devices) => devices,
                Err(error) => {
                    eprintln!("dim: audio devices unavailable: {error}");
                    return;
                }
            };
        super::serve(file, requests, std::process::id(), |active_only| {
            list(&devices, active_only).unwrap_or_else(|error| {
                eprintln!("dim: listing audio sessions failed: {error}");
                Vec::new()
            })
        });
    }

    fn list(
        devices: &IMMDeviceEnumerator,
        active_only: bool,
    ) -> windows::core::Result<Vec<Found<Session>>> {
        // SAFETY: plain COM calls on interfaces owned by this thread.
        unsafe {
            let device = devices.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            let mut found = Vec::new();
            for index in 0..sessions.GetCount()? {
                let Ok(control) = sessions.GetSession(index) else {
                    continue;
                };
                let Ok(state) = control.GetState() else {
                    continue;
                };
                if state == AudioSessionStateExpired
                    || (active_only && state != AudioSessionStateActive)
                {
                    continue;
                }
                let (Ok(control), Ok(volume)) = (
                    control.cast::<IAudioSessionControl2>(),
                    control.cast::<ISimpleAudioVolume>(),
                ) else {
                    continue;
                };
                found.push(Found {
                    pid: control.GetProcessId().unwrap_or(0),
                    system_sounds: control.IsSystemSoundsSession() == S_OK,
                    instance: take(control.GetSessionInstanceIdentifier()),
                    app: take(control.GetSessionIdentifier()),
                    volume: Session(volume),
                });
            }
            Ok(found)
        }
    }

    /// Lists the real sessions without changing any volume:
    /// `cargo test --lib dim::wasapi -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the machine's live audio sessions"]
    fn live_lists_output_sessions() {
        // SAFETY: COM setup on this test thread, as in `run`.
        let devices: IMMDeviceEnumerator = unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).unwrap();
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap()
        };
        let started = std::time::Instant::now();
        let found = list(&devices, false).unwrap();
        let elapsed = started.elapsed();
        for session in &found {
            println!(
                "pid {} system {} volume {:?} app {}",
                session.pid,
                session.system_sounds,
                session.volume.get(),
                session.app
            );
        }
        println!("{} sessions listed in {elapsed:?}", found.len());
    }

    /// Copies and frees a COM-allocated string.
    unsafe fn take(text: windows::core::Result<PWSTR>) -> String {
        match text {
            Ok(text) if !text.is_null() => {
                // SAFETY: a valid NUL-terminated string from COM, freed once.
                let copy = unsafe { text.to_string() }.unwrap_or_default();
                unsafe { CoTaskMemFree(Some(text.0 as *const _)) };
                copy
            }
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        rc::Rc,
        sync::{Arc, Mutex},
    };

    /// A fake session volume shared with the test, which plays the user.
    #[derive(Clone, Default)]
    struct Fake(Rc<Cell<f32>>, Rc<Cell<usize>>);
    impl Fake {
        fn at(value: f32) -> Self {
            let fake = Self::default();
            fake.0.set(value);
            fake
        }
        fn now(&self) -> f32 {
            self.0.get()
        }
    }
    impl Volume for Fake {
        fn get(&self) -> Option<f32> {
            Some(self.0.get())
        }
        fn set(&self, value: f32) -> bool {
            self.0.set(value);
            self.1.set(self.1.get() + 1);
            true
        }
    }

    fn found(pid: u32, name: &str, volume: &Fake) -> Found<Fake> {
        Found {
            pid,
            system_sounds: false,
            instance: format!("{name}|{pid}"),
            app: name.into(),
            volume: volume.clone(),
        }
    }

    const OWN: u32 = 7;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn dims_other_sessions_and_restores_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        let (music, browser) = (Fake::at(0.8), Fake::at(0.3));
        let mut dimmer = Dimmer::new(file.clone());
        dimmer.dim(
            vec![found(1, "spotify", &music), found(2, "chrome", &browser)],
            OWN,
            0.5,
        );
        assert!(file.exists(), "originals are saved before any change");
        dimmer.step(FADE / 2);
        assert!(music.now() < 0.8 && music.now() > 0.4, "mid-fade");
        dimmer.step(FADE);
        assert!(close(music.now(), 0.4) && close(browser.now(), 0.15));
        assert!(!dimmer.fading(), "no polling while dimmed");
        dimmer.restore();
        dimmer.step(FADE / 3);
        assert!(music.now() > 0.4 && music.now() < 0.8);
        dimmer.step(FADE);
        assert_eq!(music.now(), 0.8, "bit-exact original");
        assert_eq!(browser.now(), 0.3);
        assert!(dimmer.is_idle() && !file.exists());
    }

    #[test]
    fn never_touches_parla_or_system_sounds() {
        let dir = tempfile::tempdir().unwrap();
        let (own, system, music) = (Fake::at(1.0), Fake::at(1.0), Fake::at(1.0));
        let mut sounds = found(3, "system", &system);
        sounds.system_sounds = true;
        let mut dimmer = Dimmer::new(dir.path().join(FILE_NAME));
        dimmer.dim(
            vec![
                found(OWN, "parla", &own),
                sounds,
                found(1, "spotify", &music),
            ],
            OWN,
            0.5,
        );
        dimmer.step(FADE);
        dimmer.restore();
        dimmer.step(FADE);
        assert_eq!((own.1.get(), system.1.get()), (0, 0));
        assert!(music.1.get() > 0);
    }

    #[test]
    fn keeps_a_volume_the_user_changed_while_dimmed() {
        let dir = tempfile::tempdir().unwrap();
        let (music, browser) = (Fake::at(0.8), Fake::at(0.6));
        let mut dimmer = Dimmer::new(dir.path().join(FILE_NAME));
        dimmer.dim(
            vec![found(1, "spotify", &music), found(2, "chrome", &browser)],
            OWN,
            0.5,
        );
        dimmer.step(FADE);
        music.0.set(0.2); // the user drags Spotify's slider in the mixer
        let touched = music.1.get();
        dimmer.restore();
        dimmer.step(FADE);
        assert_eq!(music.now(), 0.2, "user's value kept");
        assert_eq!(music.1.get(), touched);
        assert_eq!(browser.now(), 0.6, "others still restored");
    }

    #[test]
    fn a_new_dictation_during_the_fade_up_turns_it_around() {
        let dir = tempfile::tempdir().unwrap();
        let music = Fake::at(1.0);
        let mut dimmer = Dimmer::new(dir.path().join(FILE_NAME));
        dimmer.dim(vec![found(1, "spotify", &music)], OWN, 0.5);
        dimmer.step(FADE);
        dimmer.restore();
        dimmer.step(FADE / 2);
        // Not idle: no new listing, the originals stay the first ones.
        dimmer.dim(Vec::new(), OWN, 0.5);
        dimmer.step(FADE);
        assert!(close(music.now(), 0.5));
        dimmer.restore_now();
        assert_eq!(music.now(), 1.0);
        assert!(dimmer.is_idle());
    }

    #[test]
    fn nothing_to_dim_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        let own = Fake::at(1.0);
        let mut dimmer = Dimmer::new(file.clone());
        dimmer.dim(vec![found(OWN, "parla", &own)], OWN, 0.5);
        assert!(dimmer.is_idle() && !dimmer.fading() && !file.exists());
    }

    #[test]
    fn crash_restore_puts_back_dimmed_sessions_only_and_deletes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        let (music, browser, moved, gone) =
            (Fake::at(0.8), Fake::at(0.3), Fake::at(0.5), Fake::at(0.9));
        {
            // Parla dies mid-fade, without restoring.
            let mut dimmer = Dimmer::new(file.clone());
            dimmer.dim(
                vec![
                    found(1, "spotify", &music),
                    found(2, "chrome", &browser),
                    found(3, "teams", &moved),
                    found(4, "vlc", &gone),
                ],
                OWN,
                0.5,
            );
            dimmer.step(FADE / 2);
        }
        assert!(file.exists());
        moved.0.set(0.05); // the user turned Teams down after the crash
                           // Spotify restarted: a new instance of the same app, at the volume
                           // Windows remembered. VLC is gone.
        let mut restarted = found(11, "spotify", &music);
        restarted.instance = "spotify|11".into();
        restore_crashed(&file, || {
            vec![
                restarted,
                found(2, "chrome", &browser),
                found(3, "teams", &moved),
            ]
        });
        assert_eq!(music.now(), 0.8);
        assert_eq!(browser.now(), 0.3);
        assert_eq!(moved.now(), 0.05);
        assert!(!file.exists());
        // Nothing saved: sessions are not even listed.
        restore_crashed::<Fake>(&file, || panic!("listed sessions without a file"));
    }

    #[test]
    fn crash_restore_prefers_each_entrys_own_instance() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        // Two Chrome sessions were dimmed; the first one's process died.
        let saved = [("chrome|1", 1.0), ("chrome|2", 0.6)].map(|(instance, original)| Saved {
            instance: instance.into(),
            app: "chrome".into(),
            original,
            dimmed: original * 0.5,
        });
        write_saved(&file, &saved).unwrap();
        let (alive, fresh) = (Fake::at(0.3), Fake::at(0.5));
        restore_crashed(&file, || {
            vec![found(2, "chrome", &alive), found(9, "chrome", &fresh)]
        });
        assert_eq!(alive.now(), 0.6, "its own original, not the dead one's");
        assert_eq!(fresh.now(), 1.0, "the dead one's app fallback");
    }

    #[test]
    fn a_corrupt_file_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        std::fs::write(&file, b"{not json").unwrap();
        restore_crashed::<Fake>(&file, Vec::new);
        assert!(!file.exists());
    }

    #[test]
    fn worker_fades_both_ways_over_the_channel() {
        let dir = tempfile::tempdir().unwrap();
        // Every value the worker set, current last.
        let history = Arc::new(Mutex::new(vec![1.0f32]));
        let (sender, requests) = mpsc::channel();
        let worker_history = history.clone();
        let worker = std::thread::spawn(move || {
            serve(dir.path().join(FILE_NAME), requests, OWN, |_| {
                vec![Found {
                    pid: 1,
                    system_sounds: false,
                    instance: "a".into(),
                    app: "a".into(),
                    volume: Locked(worker_history.clone()),
                }]
            })
        });
        // Waits for `target`, then returns whether the way there was a fade.
        let fades_to = |target: f32| {
            let from = history.lock().unwrap().len();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !close(*history.lock().unwrap().last().unwrap(), target) {
                assert!(Instant::now() < deadline, "never reached {target}");
                std::thread::sleep(TICK);
            }
            history.lock().unwrap()[from..]
                .iter()
                .any(|&value| value > 0.51 && value < 0.99)
        };
        sender.send(Request::Dim(0.5)).unwrap();
        assert!(fades_to(0.5), "fades down");
        // Dimmed for a while: the fade up still takes its own time.
        std::thread::sleep(FADE * 2);
        sender.send(Request::Restore).unwrap();
        assert!(fades_to(1.0), "fades up");
        assert_eq!(*history.lock().unwrap().last().unwrap(), 1.0);
        sender.send(Request::Dim(0.5)).unwrap();
        let (done, finished) = mpsc::channel();
        sender.send(Request::Exit(done)).unwrap();
        finished.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            *history.lock().unwrap().last().unwrap(),
            1.0,
            "quit restores at once"
        );
        worker.join().unwrap();
    }

    struct Locked(Arc<Mutex<Vec<f32>>>);
    impl Volume for Locked {
        fn get(&self) -> Option<f32> {
            self.0.lock().unwrap().last().copied()
        }
        fn set(&self, value: f32) -> bool {
            self.0.lock().unwrap().push(value);
            true
        }
    }

    #[test]
    fn level_is_clamped_and_never_mutes() {
        assert_eq!(level(DEFAULT_LEVEL_PERCENT), 0.5);
        assert_eq!(level(0), 0.1);
        assert_eq!(level(100), 0.9);
    }
}
