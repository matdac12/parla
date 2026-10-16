//! Pre-warmed microphone capture.
//!
//! The default input device is opened once, at startup, and its stream is kept
//! running so pressing the hotkey has no device-startup latency. After
//! [`MIC_IDLE_PAUSE`] without a recording the stream is paused but kept open
//! (`IAudioClient::Stop`), which removes nearly all of the open microphone's
//! idle cost, most of it in the Windows audio engine; the next `start` resumes
//! it in about a millisecond.
//!
//! The realtime cpal callback does two things only: convert the device samples
//! to mono f32 into a reused scratch buffer and push them into a lock-free SPSC
//! ring ([`ringbuf`]). No allocation, no blocking, no shared lock on the audio
//! thread. A worker thread drains the ring and owns the recording buffer; the
//! recording boundary is a generation id carried in the [`Capture`] state, so a
//! clip can never contain audio from before its `start()`. While idle the
//! worker keeps the most recent [`PRE_ROLL_MS`] of audio as a pre-roll, so the
//! first syllable is not clipped. A resume has no pre-roll: the ring and the
//! pre-roll then hold audio from before the pause, which is dropped, as is any
//! packet WASAPI kept across it (see [`StaleFilter`]).
//!
//! Overflow policy: the ring holds [`RING_SECONDS`] of audio. If the consumer
//! stalls and the ring fills, the callback drops the newest samples for the
//! rest of that block and counts them (`Recorder::dropped_samples`).
//!
//! The cpal `Stream` is `!Send` on some platforms, so it lives on the worker
//! thread. The design (dedicated thread, mix-to-mono, pre-warmed stream,
//! rubato resampling, reopen-on-next-start after a device failure) follows
//! Handy's recorder (github.com/cjpais/Handy, MIT); see `THIRD_PARTY_NOTICES.md`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use rubato::audioadapter_buffers::direct::SequentialSlice;
use rubato::{
    Fft, FixedSync, Indexing, ResampleError, Resampler, ResamplerConstructionError, WindowFunction,
};

use crate::clip::{Clip, SAMPLE_RATE};

/// Full original audio; conversion is cached and performed only when batch needs it.
pub struct RecoveryAudio {
    raw: Vec<f32>,
    rate: u32,
    clip: OnceLock<Clip>,
}
impl RecoveryAudio {
    pub fn duration(&self) -> Duration {
        self.clip.get().map_or_else(
            || Duration::from_secs_f64(self.raw.len() as f64 / self.rate as f64),
            Clip::duration,
        )
    }
    pub fn is_empty(&self) -> bool {
        self.clip
            .get()
            .map_or(self.raw.is_empty(), |clip| clip.samples.is_empty())
    }
    pub fn clip(&self) -> &Clip {
        self.clip.get_or_init(|| {
            Clip::new(
                resample_to_16k(&self.raw, self.rate)
                    .into_iter()
                    .map(f32_to_i16)
                    .collect(),
            )
        })
    }
    pub fn into_clip(self) -> Clip {
        let _ = self.clip();
        self.clip.into_inner().expect("clip materialized")
    }
}
impl From<Clip> for RecoveryAudio {
    fn from(clip: Clip) -> Self {
        Self {
            raw: Vec::new(),
            rate: clip.sample_rate,
            clip: OnceLock::from(clip),
        }
    }
}

/// How long a public call waits for the worker before giving up.
const WORKER_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the worker blocks waiting for a command before draining the ring
/// again while recording. Commands wake this wait immediately, so draining less
/// often does not add a polling delay to start/stop. The live meter updates
/// every 40 ms and streaming forwards audio as it is drained.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The same wait while idle, when draining only refreshes the pre-roll. `Start`
/// drains first, so the longer gap never adds stale audio to a clip.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// While idle, how often the worker retries a failed microphone and checks
/// whether a directly opened endpoint is still the system default, so a
/// replugged headset is picked up before the next dictation instead of during it.
/// Paused, the worker does not poll: the same check runs at resume instead.
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// Idle time after the last recording before the stream is paused. Long enough
/// to keep the microphone warm through a burst of dictations (PARLA-28).
const MIC_IDLE_PAUSE: Duration = Duration::from_secs(60);

/// At resume, blocks captured more than this before the resume are stale. Any
/// stale block is at least [`MIC_IDLE_PAUSE`] old; the margin only absorbs
/// clock jitter.
const STALE_MARGIN: Duration = Duration::from_millis(50);

/// Most audio dropped as stale per resume, in ms. WASAPI keeps at most its
/// unread packets across a pause (none seen on the EarPods); the cap stops a
/// driver with bogus capture timestamps from losing more than this.
const STALE_MAX_MS: usize = 100;

/// Ring capacity, in seconds of device audio. Generous: the worker only needs
/// to keep up with a 100 ms drain interval.
const RING_SECONDS: usize = 2;

/// Idle audio kept so `start` does not clip the first syllable.
const PRE_ROLL_MS: usize = 150;

/// RMS of the most recent chunk captured during a recording (f32 bits), read
/// by the recording overlay.
static LEVEL: AtomicU32 = AtomicU32::new(0);

pub fn current_level() -> f32 {
    f32::from_bits(LEVEL.load(Ordering::Relaxed))
}

/// Samples the worker drains from the ring at a time.
const DRAIN_SCRATCH: usize = 4096;

/// Initial scratch capacity for the callback's mono mix, per channel.
const CALLBACK_FRAMES_HINT: usize = 8192;

/// rubato's FFT block size. Larger blocks resample more cleanly at the cost of
/// a little more output latency; 1024 is Handy's proven choice.
const RESAMPLER_CHUNK: usize = 1024;

/// Cap on zero-chunk rounds while draining the resampler's delay line.
const MAX_TAIL_ROUNDS: usize = 8;

/// The one recorder the app uses. Created at startup, retried lazily if the
/// machine had no input device then.
static RECORDER: OnceLock<Arc<Recorder>> = OnceLock::new();
static RECORDER_INIT: Mutex<()> = Mutex::new(());

/// Opens the microphone at startup, logging (not failing) when there is none.
pub fn warm_up() {
    let _ = recorder();
}

/// The shared recorder, or `None` when no input device can be opened.
pub fn recorder() -> Option<Arc<Recorder>> {
    if let Some(existing) = RECORDER.get() {
        return Some(Arc::clone(existing));
    }
    // Startup warm-up and a first hotkey/Test mic can arrive concurrently.
    // Serialize only initialization so they cannot open the device twice.
    let _init = RECORDER_INIT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = RECORDER.get() {
        return Some(Arc::clone(existing));
    }
    match Recorder::new() {
        Ok(new) => {
            let new = Arc::new(new);
            match RECORDER.set(Arc::clone(&new)) {
                Ok(()) => Some(new),
                Err(_) => RECORDER.get().map(Arc::clone),
            }
        }
        Err(error) => {
            eprintln!("microphone unavailable: {error:#}");
            None
        }
    }
}

/// The active recording's generation, if any. Guarded so `start` and `stop`
/// are serialized and a second `start` is rejected.
#[derive(Default)]
struct Control {
    generation: u64,
    active: bool,
}

struct Shared {
    control: Mutex<Control>,
    /// Set by the worker when a stream is running.
    open: AtomicBool,
    /// Set when the stream failed (device unplugged); a later `start` reopens.
    failed: AtomicBool,
    /// The open device's sample rate, so `stop` knows how to resample.
    rate: AtomicU32,
    /// Samples the callback could not push because the ring was full.
    dropped: AtomicU64,
    /// Stream instant (ns, see [`StaleFilter`]) of the last resume while its
    /// stale audio may still arrive; 0 once the first fresh block came.
    resume_at: AtomicU64,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            control: Mutex::new(Control::default()),
            open: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            rate: AtomicU32::new(SAMPLE_RATE),
            dropped: AtomicU64::new(0),
            resume_at: AtomicU64::new(0),
        }
    }
}

/// Commands the worker services while it drains the ring.
enum Command {
    Start {
        generation: u64,
        ack: Sender<()>,
    },
    Stop {
        generation: u64,
        reply: Sender<(u64, Vec<f32>)>,
    },
    Subscribe {
        generation: u64,
        feed: crate::streaming::audio::AudioFeed,
    },
    Reopen {
        ack: Sender<Result<(), String>>,
    },
    Shutdown,
}

/// A microphone that is open and running. Cheap to share via `Arc`.
pub struct Recorder {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Opens the default input device and starts its stream on a worker thread.
    /// Errors when no device can be opened.
    pub fn new() -> Result<Recorder> {
        Self::with_idle_pause(MIC_IDLE_PAUSE)
    }

    /// [`Recorder::new`] pausing the idle stream after `idle_pause`.
    fn with_idle_pause(idle_pause: Duration) -> Result<Recorder> {
        let shared = Arc::new(Shared::default());

        let (command_tx, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("parla-audio".into())
            .spawn(move || audio_thread(worker_shared, command_rx, ready_tx, idle_pause))
            .context("spawning the audio thread")?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Recorder {
                shared,
                commands: command_tx,
                worker: Some(worker),
            }),
            Ok(Err(message)) => {
                let _ = command_tx.send(Command::Shutdown);
                let _ = worker.join();
                Err(anyhow!(message))
            }
            Err(_) => Err(anyhow!("audio thread exited during startup")),
        }
    }

    /// Begins a recording. Errors if one is already in progress, so two callers
    /// (hotkey and the tray test) cannot overwrite each other. Reopens the
    /// device first if it never opened or was lost, and resumes it if it was
    /// paused while idle.
    pub fn start(&self) -> Result<()> {
        let mut control = self.lock_control();
        if control.active {
            bail!("a recording is already in progress");
        }
        if !self.shared.open.load(Ordering::SeqCst) || self.shared.failed.load(Ordering::SeqCst) {
            self.reopen()?;
        }
        control.generation = control.generation.wrapping_add(1);
        let generation = control.generation;

        let (ack_tx, ack_rx) = mpsc::channel();
        self.commands
            .send(Command::Start {
                generation,
                ack: ack_tx,
            })
            .map_err(|_| anyhow!("audio thread is not running"))?;
        ack_rx
            .recv_timeout(WORKER_TIMEOUT)
            .map_err(|_| anyhow!("timed out starting the recording"))?;
        control.active = true;
        Ok(())
    }

    /// Ends the recording and returns what was captured, resampled to 16 kHz
    /// mono i16. Returns an empty clip when nothing is being recorded.
    pub fn stop(&self) -> Clip {
        self.stop_raw().into_clip()
    }

    pub fn stop_raw(&self) -> RecoveryAudio {
        let mut control = self.lock_control();
        if !control.active {
            return Clip::new(Vec::new()).into();
        }
        let generation = control.generation;

        let (reply_tx, reply_rx) = mpsc::channel();
        let raw = if self
            .commands
            .send(Command::Stop {
                generation,
                reply: reply_tx,
            })
            .is_ok()
        {
            match reply_rx.recv_timeout(WORKER_TIMEOUT) {
                Ok((replied, samples)) if replied == generation => samples,
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        control.active = false;

        let dropped = self.shared.dropped.swap(0, Ordering::AcqRel);
        if dropped > 0 {
            eprintln!("recorder: dropped {dropped} samples (ring overflow)");
        }

        let rate = self.shared.rate.load(Ordering::Acquire);
        RecoveryAudio {
            raw,
            rate,
            clip: OnceLock::new(),
        }
    }

    /// Whether audio is currently being buffered.
    pub fn is_recording(&self) -> bool {
        self.lock_control().active
    }

    /// Attaches after start without reopening the microphone or losing its pre-roll.
    pub fn subscribe(&self, feed: crate::streaming::audio::AudioFeed) -> Result<()> {
        let control = self.lock_control();
        if !control.active {
            bail!("no active recording to stream");
        }
        self.commands
            .send(Command::Subscribe {
                generation: control.generation,
                feed,
            })
            .map_err(|_| anyhow!("audio thread is not running"))
    }

    /// Samples dropped since the last `stop` because the ring overflowed.
    pub fn dropped_samples(&self) -> u64 {
        self.shared.dropped.load(Ordering::Acquire)
    }

    fn lock_control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.shared
            .control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reopen(&self) -> Result<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.commands
            .send(Command::Reopen { ack: ack_tx })
            .map_err(|_| anyhow!("audio thread is not running"))?;
        match ack_rx.recv_timeout(WORKER_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(anyhow!(message)),
            Err(_) => Err(anyhow!("timed out reopening the microphone")),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The dedicated WASAPI worker uses MTA, including when a USB device change
/// makes CPAL activate a new default endpoint. Select it before CPAL's default
/// STA initialization. Never change the UI thread's apartment.
#[cfg(windows)]
struct AudioThreadApartment(std::marker::PhantomData<*mut ()>);

#[cfg(windows)]
impl AudioThreadApartment {
    fn initialize() -> Result<Self, String> {
        use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        // SAFETY: called on our dedicated worker before any COM/audio calls.
        let result = unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32) };
        if result < 0 {
            return Err(format!(
                "initializing the audio worker COM apartment failed: 0x{:08X}",
                result as u32
            ));
        }
        Ok(Self(std::marker::PhantomData))
    }
}

#[cfg(windows)]
impl Drop for AudioThreadApartment {
    fn drop(&mut self) {
        // SAFETY: each successful initialization (including S_FALSE) owns one
        // matching uninitialize. The !Send guard stays on its original thread.
        unsafe { windows_sys::Win32::System::Com::CoUninitialize() };
    }
}

/// Owns the cpal stream and the recording buffer.
fn audio_thread(
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    ready: Sender<Result<(), String>>,
    idle_pause: Duration,
) {
    #[cfg(windows)]
    let _apartment = match AudioThreadApartment::initialize() {
        Ok(apartment) => apartment,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut session = match open_session(&shared) {
        Ok(session) => {
            shared.open.store(true, Ordering::SeqCst);
            shared.failed.store(false, Ordering::SeqCst);
            if ready.send(Ok(())).is_err() {
                return;
            }
            Some(session)
        }
        Err(error) => {
            shared.open.store(false, Ordering::SeqCst);
            shared.failed.store(true, Ordering::SeqCst);
            let _ = ready.send(Err(error));
            None
        }
    };

    let mut capture = Capture::new();
    if let Some(session) = session.as_ref() {
        capture.set_preroll_cap(preroll_capacity(session.rate));
    }
    let mut scratch = vec![0.0f32; DRAIN_SCRATCH];
    let mut last_device_check = Instant::now();
    let mut idle = IdleMic::new(idle_pause, Instant::now());

    loop {
        if let Some(session) = session.as_mut() {
            drain(&mut session.consumer, &mut scratch, &mut capture);
        }

        // Paused, nothing arrives to drain: block until the next command.
        let received = match idle.wait(capture.active.is_some()) {
            Some(interval) => commands.recv_timeout(interval),
            None => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Command::Start { generation, ack }) => {
                if idle.paused {
                    idle.paused = false;
                    resume_session(&shared, &mut session, &mut capture, &mut scratch);
                }
                start_capture(
                    session.as_mut().map(|session| &mut session.consumer),
                    &mut scratch,
                    &mut capture,
                    generation,
                );
                let _ = ack.send(());
            }
            Ok(Command::Stop { generation, reply }) => {
                // Drain anything that arrived since the top of the loop, then
                // hand back only this generation's audio.
                if let Some(session) = session.as_mut() {
                    drain(&mut session.consumer, &mut scratch, &mut capture);
                }
                let samples = capture.finish(generation);
                let _ = reply.send((generation, samples));
                idle.active(Instant::now());
            }
            Ok(Command::Subscribe { generation, feed }) => {
                capture.subscribe(generation, feed, shared.rate.load(Ordering::Acquire));
            }
            Ok(Command::Reopen { ack }) => {
                // A reopened stream is running, whether or not the old one was paused.
                idle.paused = false;
                let _ = ack.send(reopen_session(&shared, &mut session, &mut capture));
            }
            Ok(Command::Shutdown) => {
                shared.open.store(false, Ordering::SeqCst);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                if capture.active.is_none() && last_device_check.elapsed() >= DEVICE_CHECK_INTERVAL
                {
                    last_device_check = Instant::now();
                    if device_needs_reopen(&shared, session.as_ref()) {
                        if let Err(error) = reopen_session(&shared, &mut session, &mut capture) {
                            eprintln!("microphone: background reopen failed: {error}");
                        }
                    }
                }
                if idle.pause_due(Instant::now(), capture.active.is_some()) {
                    // A lost microphone keeps the idle poll so it is reopened.
                    if let Some(open) = session.as_ref().filter(|_| !stream_lost(&shared)) {
                        match open.stream.pause() {
                            Ok(()) => {
                                idle.paused = true;
                                eprintln!("microphone: paused after {:?} idle", idle.after);
                            }
                            Err(error) => {
                                // Retry after another idle period, not every poll.
                                idle.active(Instant::now());
                                eprintln!("microphone: pause failed: {error}");
                            }
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// When the idle stream is paused. Idle time counts from the last recording's
/// end (or from startup) and only while nothing is being recorded.
struct IdleMic {
    after: Duration,
    since: Instant,
    paused: bool,
}

impl IdleMic {
    fn new(after: Duration, now: Instant) -> Self {
        Self {
            after,
            since: now,
            paused: false,
        }
    }

    /// A recording just ended: the idle period starts again.
    fn active(&mut self, now: Instant) {
        self.since = now;
    }

    fn pause_due(&self, now: Instant, recording: bool) -> bool {
        !self.paused && !recording && now.saturating_duration_since(self.since) >= self.after
    }

    /// How long the worker waits for a command before draining again; `None`
    /// blocks until one arrives.
    fn wait(&self, recording: bool) -> Option<Duration> {
        if recording {
            Some(POLL_INTERVAL)
        } else if self.paused {
            None
        } else {
            Some(IDLE_POLL_INTERVAL)
        }
    }
}

/// Resumes the paused stream for a recording. Nothing polled the default
/// device while paused, so check it first, as the idle worker would have:
/// reopen when the stream broke or a directly opened endpoint is no longer
/// the default. Audio buffered before the pause never reaches the clip.
fn resume_session(
    shared: &Arc<Shared>,
    session: &mut Option<OpenSession>,
    capture: &mut Capture,
    scratch: &mut [f32],
) {
    let started = Instant::now();
    if device_needs_reopen(shared, session.as_ref()) {
        match reopen_session(shared, session, capture) {
            Ok(()) => eprintln!(
                "microphone: input changed while paused; reopened in {:.1} ms",
                started.elapsed().as_secs_f64() * 1e3
            ),
            Err(error) => eprintln!("microphone: reopen at resume failed: {error}"),
        }
        return;
    }
    let Some(open) = session.as_mut() else {
        return;
    };
    discard_buffered(&mut open.consumer, scratch, capture);
    shared
        .resume_at
        .store(stream_nanos(open.stream.now()), Ordering::Release);
    if let Err(error) = open.stream.play() {
        shared.resume_at.store(0, Ordering::Release);
        eprintln!("microphone: resume failed ({error}); reopening");
        if let Err(error) = reopen_session(shared, session, capture) {
            eprintln!("microphone: reopen at resume failed: {error}");
        }
        return;
    }
    eprintln!(
        "microphone: resumed in {:.2} ms",
        started.elapsed().as_secs_f64() * 1e3
    );
}

/// Drops whatever the ring and the pre-roll hold: after a pause it predates
/// the press by at least the idle period.
fn discard_buffered(consumer: &mut HeapCons<f32>, scratch: &mut [f32], capture: &mut Capture) {
    drain(consumer, scratch, capture);
    capture.preroll.clear();
}

/// A stream instant as nonzero nanoseconds, the [`Shared::resume_at`] encoding.
fn stream_nanos(instant: cpal::StreamInstant) -> u64 {
    u64::try_from(instant.as_nanos()).unwrap_or(u64::MAX).max(1)
}

/// Drops, in the callback, the blocks WASAPI buffered before a pause.
/// `IAudioClient::Stop` keeps packets not yet read and `Start` delivers them
/// first, captured just before the pause. A block is stale while a resume is
/// armed and it was captured before that resume; the first fresh block
/// disarms it. No allocation, no locking.
#[derive(Default)]
struct StaleFilter {
    /// The resume these drops count against.
    resume: u64,
    dropped: usize,
}

impl StaleFilter {
    /// Whether to drop a block of `frames` captured at `captured` (ns).
    /// `max_frames` caps the drops per resume.
    fn drops(
        &mut self,
        resume_at: &AtomicU64,
        captured: u64,
        frames: usize,
        max_frames: usize,
    ) -> bool {
        let resume = resume_at.load(Ordering::Acquire);
        if resume == 0 {
            return false;
        }
        if resume != self.resume {
            self.resume = resume;
            self.dropped = 0;
        }
        let margin = STALE_MARGIN.as_nanos() as u64;
        let stale = captured.saturating_add(margin) < resume && self.dropped + frames <= max_frames;
        if stale {
            self.dropped += frames;
        } else {
            // Fresh audio (or the cap): stop checking until the next resume.
            let _ = resume_at.compare_exchange(resume, 0, Ordering::AcqRel, Ordering::Relaxed);
        }
        stale
    }
}

/// Replaces the session with a freshly opened default microphone.
fn reopen_session(
    shared: &Arc<Shared>,
    session: &mut Option<OpenSession>,
    capture: &mut Capture,
) -> Result<(), String> {
    drop(session.take());
    shared.open.store(false, Ordering::SeqCst);
    match open_session(shared) {
        Ok(new) => {
            shared.failed.store(false, Ordering::SeqCst);
            shared.open.store(true, Ordering::SeqCst);
            capture.set_preroll_cap(preroll_capacity(new.rate));
            *session = Some(new);
            Ok(())
        }
        Err(error) => {
            shared.failed.store(true, Ordering::SeqCst);
            Err(error)
        }
    }
}

/// Whether the idle worker should reopen now: the stream failed, or it was
/// opened directly and Windows has since switched the default input. A stream
/// on CPAL's default device follows the default by itself.
fn device_needs_reopen(shared: &Shared, session: Option<&OpenSession>) -> bool {
    stream_lost(shared)
        || default_moved(session.and_then(|session| session.direct.as_ref()), || {
            cpal::default_host()
                .default_input_device()
                .and_then(|device| device.id().ok())
        })
}

/// The stream failed or never opened.
fn stream_lost(shared: &Shared) -> bool {
    shared.failed.load(Ordering::SeqCst) || !shared.open.load(Ordering::SeqCst)
}

/// Whether Windows switched the default input away from the endpoint opened
/// directly as `direct`. Queries `current_default` only for a direct stream.
fn default_moved<T: PartialEq>(
    direct: Option<&T>,
    current_default: impl FnOnce() -> Option<T>,
) -> bool {
    let Some(direct) = direct else {
        return false;
    };
    current_default().is_some_and(|default| &default != direct)
}

/// An open microphone owned by the worker thread. `stream` keeps the device
/// running; `consumer` is the read end of its sample ring.
struct OpenSession {
    /// Keeps the device running (paused while idle); dropping it closes the stream.
    stream: cpal::Stream,
    consumer: HeapCons<f32>,
    rate: u32,
    /// The endpoint when opened directly rather than through CPAL's default
    /// device, which does not follow the system default.
    direct: Option<cpal::DeviceId>,
}

/// Opens the default input device and starts its stream. CPAL's default device
/// follows the system default but activates through the Windows thread pool,
/// which fails with RPC_E_CHANGED_MODE once a pool thread was left in an STA
/// (CPAL's own device-change callbacks do that). The current default endpoint
/// opened directly activates on this worker instead.
fn open_session(shared: &Arc<Shared>) -> Result<OpenSession, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "no default input device".to_owned())?;
    open_device_session(&device, shared, false).or_else(|error| {
        let endpoint = device
            .id()
            .ok()
            .and_then(|id| host.device_by_id(&id))
            .ok_or_else(|| error.clone())?;
        open_device_session(&endpoint, shared, true)
            .map_err(|fallback| format!("{error}; opening the endpoint directly: {fallback}"))
    })
}

/// Opens `device` at its OS default format and starts the stream. `direct`
/// marks a specific endpoint rather than CPAL's default device.
fn open_device_session(
    device: &cpal::Device,
    shared: &Arc<Shared>,
    direct: bool,
) -> Result<OpenSession, String> {
    let supported = device
        .default_input_config()
        .map_err(|error| format!("fetching the input config failed: {error}"))?;

    let sample_format = supported.sample_format();
    let config = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    shared.rate.store(rate, Ordering::Release);

    let (producer, consumer) = HeapRb::<f32>::new(ring_capacity(rate)).split();

    let stream = match sample_format {
        SampleFormat::I8 => build_stream::<i8>(device, &config, producer, shared, channels),
        SampleFormat::I16 => build_stream::<i16>(device, &config, producer, shared, channels),
        SampleFormat::I32 => build_stream::<i32>(device, &config, producer, shared, channels),
        SampleFormat::I64 => build_stream::<i64>(device, &config, producer, shared, channels),
        SampleFormat::U8 => build_stream::<u8>(device, &config, producer, shared, channels),
        SampleFormat::U16 => build_stream::<u16>(device, &config, producer, shared, channels),
        SampleFormat::U32 => build_stream::<u32>(device, &config, producer, shared, channels),
        SampleFormat::U64 => build_stream::<u64>(device, &config, producer, shared, channels),
        SampleFormat::F32 => build_stream::<f32>(device, &config, producer, shared, channels),
        SampleFormat::F64 => build_stream::<f64>(device, &config, producer, shared, channels),
        other => return Err(format!("unsupported input sample format {other}")),
    }?;

    stream
        .play()
        .map_err(|error| format!("starting the microphone stream failed: {error}"))?;
    let id = device.id().ok();
    eprintln!(
        "microphone: opened {} {} at {rate} Hz, {channels} ch",
        if direct { "direct" } else { "default" },
        id.as_ref().map(ToString::to_string).unwrap_or_default()
    );
    Ok(OpenSession {
        stream,
        consumer,
        rate,
        direct: id.filter(|_| direct),
    })
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut producer: HeapProd<f32>,
    shared: &Arc<Shared>,
    channels: usize,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let max_stale = config.sample_rate as usize * STALE_MAX_MS / 1000;
    let mut stale = StaleFilter::default();
    let frames_hint = match config.buffer_size {
        cpal::BufferSize::Fixed(frames) => frames as usize,
        _ => CALLBACK_FRAMES_HINT,
    };
    // Reused for the life of the stream: no allocation in the callback.
    let mut scratch: Vec<f32> = Vec::with_capacity(frames_hint.max(1024) * channels.max(1));
    let data_shared = Arc::clone(shared);
    let error_shared = Arc::clone(shared);

    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                let captured = stream_nanos(info.timestamp().capture);
                let frames = data.len() / channels.max(1);
                if stale.drops(&data_shared.resume_at, captured, frames, max_stale) {
                    return;
                }
                scratch.clear();
                downmix_into(data, channels, &mut scratch);
                let pushed = producer.push_iter(scratch.iter().copied());
                if pushed < scratch.len() {
                    data_shared
                        .dropped
                        .fetch_add((scratch.len() - pushed) as u64, Ordering::Relaxed);
                }
            },
            move |error: cpal::Error| {
                eprintln!("microphone stream error: {error}");
                if stream_error_is_fatal(error.kind()) {
                    error_shared.failed.store(true, Ordering::SeqCst);
                    error_shared.open.store(false, Ordering::SeqCst);
                }
            },
            None,
        )
        .map_err(|error| format!("building the input stream failed: {error}"))
}

/// Whether a stream error means the microphone must be reopened. A glitch, a
/// reroute to the new default device or a refused real-time priority leave the
/// stream running; reopening for them only delays the next dictation.
fn stream_error_is_fatal(kind: cpal::ErrorKind) -> bool {
    !matches!(
        kind,
        cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied
    )
}

/// Converts a device-format buffer to mono f32, appending to `out` (no
/// allocation when `out` already has capacity, which the callback guarantees).
fn downmix_into<T>(data: &[T], channels: usize, out: &mut Vec<f32>)
where
    T: SizedSample,
    f32: FromSample<T>,
{
    if channels <= 1 {
        out.extend(data.iter().map(|&sample| f32::from_sample(sample)));
    } else {
        out.extend(data.chunks_exact(channels).map(|frame| {
            frame
                .iter()
                .map(|&sample| f32::from_sample(sample))
                .sum::<f32>()
                / channels as f32
        }));
    }
}

/// Reads everything currently in the ring into the capture state.
fn drain(consumer: &mut HeapCons<f32>, scratch: &mut [f32], capture: &mut Capture) {
    loop {
        let count = consumer.pop_slice(scratch);
        if count == 0 {
            break;
        }
        capture.feed(&scratch[..count]);
    }
}

/// Begins session `generation`. Audio still in the ring predates the press, so
/// it goes through the capped pre-roll first rather than into the clip.
fn start_capture(
    consumer: Option<&mut HeapCons<f32>>,
    scratch: &mut [f32],
    capture: &mut Capture,
    generation: u64,
) {
    if let Some(consumer) = consumer {
        drain(consumer, scratch, capture);
    }
    capture.begin(generation);
}

/// The worker's recording state. Keeps the active session's samples and, while
/// idle, a short pre-roll of the most recent audio.
struct Capture {
    active: Option<u64>,
    buffer: Vec<f32>,
    preroll: VecDeque<f32>,
    preroll_cap: usize,
    stream: Option<crate::streaming::audio::AudioFeed>,
}

impl Capture {
    fn new() -> Self {
        Self {
            active: None,
            buffer: Vec::new(),
            preroll: VecDeque::new(),
            preroll_cap: 0,
            stream: None,
        }
    }

    fn set_preroll_cap(&mut self, cap: usize) {
        self.preroll_cap = cap;
        while self.preroll.len() > cap {
            self.preroll.pop_front();
        }
    }

    /// Feeds device audio. Goes to the active session, or to the pre-roll while
    /// idle.
    fn feed(&mut self, chunk: &[f32]) {
        if self.active.is_some() {
            if !chunk.is_empty() {
                let mean_square = chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32;
                LEVEL.store(mean_square.sqrt().to_bits(), Ordering::Relaxed);
            }
            self.buffer.extend_from_slice(chunk);
            if let Some(feed) = &mut self.stream {
                let rate = feed.sample_rate();
                feed.push(chunk, rate);
            }
        } else {
            push_preroll(&mut self.preroll, chunk, self.preroll_cap);
        }
    }

    /// Starts a session: clears the previous buffer and seeds it with the
    /// pre-roll, so the clip begins just before `start`.
    fn begin(&mut self, generation: u64) {
        self.stream = None;
        self.buffer.clear();
        self.buffer.extend(self.preroll.drain(..));
        self.active = Some(generation);
    }

    fn subscribe(
        &mut self,
        generation: u64,
        mut feed: crate::streaming::audio::AudioFeed,
        rate: u32,
    ) {
        if self.active == Some(generation) && self.stream.is_none() {
            feed.push(&self.buffer, rate);
            self.stream = Some(feed);
        }
    }

    /// Ends session `generation`, returning its samples. A mismatched
    /// generation is ignored (nothing is taken).
    fn finish(&mut self, generation: u64) -> Vec<f32> {
        if self.active == Some(generation) {
            self.stream = None;
            self.active = None;
            self.preroll.clear();
            std::mem::take(&mut self.buffer)
        } else {
            Vec::new()
        }
    }
}

/// Keeps the last `cap` samples of `chunk` in `preroll`.
fn push_preroll(preroll: &mut VecDeque<f32>, chunk: &[f32], cap: usize) {
    if cap == 0 {
        return;
    }
    for &sample in chunk {
        if preroll.len() == cap {
            preroll.pop_front();
        }
        preroll.push_back(sample);
    }
}

fn ring_capacity(rate: u32) -> usize {
    (rate as usize * RING_SECONDS).max(8192)
}

fn preroll_capacity(rate: u32) -> usize {
    (rate as usize * PRE_ROLL_MS / 1000).max(1)
}

/// Clamps a normalized float sample into the i16 range.
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// Resamples mono f32 audio from `in_rate` to Parla's 16 kHz using rubato's
/// windowed FFT resampler (proper anti-aliasing, not naive decimation). The
/// output length is `round(input.len() * 16000 / in_rate)`; the resampler's
/// leading delay is trimmed so an onset stays aligned.
fn resample_to_16k(input: &[f32], in_rate: u32) -> Vec<f32> {
    if input.is_empty() || in_rate == 0 || in_rate == SAMPLE_RATE {
        return input.to_vec();
    }
    let mut resampler = MonoResampler::new(in_rate as usize, SAMPLE_RATE as usize)
        .expect("a positive device rate always yields a valid resampler");
    let mut out = Vec::with_capacity(input.len() * SAMPLE_RATE as usize / in_rate as usize + 16);
    if let Err(error) = resampler.push(input, &mut out) {
        eprintln!("resampling failed: {error}");
    }
    if let Err(error) = resampler.finish(&mut out) {
        eprintln!("flushing the resampler failed: {error}");
    }
    out
}

/// Mono-only subset of Handy's `FrameResampler`, plus trimming of the leading
/// delay line so the output starts at input sample zero.
pub(crate) struct MonoResampler {
    resampler: Fft<f32>,
    chunks_in: usize,
    out_buf: Vec<f32>,
    in_buf: Vec<f32>,
    in_count: usize,
    /// Output frames emitted, after the leading delay has been dropped.
    out_count: usize,
    /// Leading output frames still to drop (the resampler's delay).
    skip: usize,
    in_hz: usize,
    out_hz: usize,
}

impl MonoResampler {
    pub(crate) fn new(in_hz: usize, out_hz: usize) -> Result<Self, ResamplerConstructionError> {
        // One FFT block per chunk and the BlackmanHarris2 window, matching
        // Handy's choice (rubato's `Fft::new` would pick smaller blocks and
        // shift the delay and the anti-aliasing cutoff).
        let resampler = Fft::<f32>::new_custom(
            in_hz,
            out_hz,
            RESAMPLER_CHUNK,
            1,
            1,
            WindowFunction::BlackmanHarris2,
            FixedSync::Input,
        )?;
        let out_frames = resampler.output_frames_max();
        let skip = resampler.output_delay();
        Ok(Self {
            resampler,
            chunks_in: RESAMPLER_CHUNK,
            out_buf: vec![0.0; out_frames],
            in_buf: Vec::with_capacity(RESAMPLER_CHUNK),
            in_count: 0,
            out_count: 0,
            skip,
            in_hz,
            out_hz,
        })
    }

    /// Total output frames a finished recording should yield.
    fn expected(&self) -> usize {
        let num = self.in_count as u64 * self.out_hz as u64;
        ((num + self.in_hz as u64 / 2) / self.in_hz as u64) as usize
    }

    /// Emits `frames` frames of `out_buf`, dropping the leading delay and never
    /// exceeding `limit` total frames.
    fn emit(&mut self, frames: usize, limit: usize, out: &mut Vec<f32>) {
        let mut start = 0;
        if self.skip > 0 {
            let drop = self.skip.min(frames);
            self.skip -= drop;
            start = drop;
        }
        let room = limit.saturating_sub(self.out_count);
        let take = (frames - start).min(room);
        if take > 0 {
            out.extend_from_slice(&self.out_buf[start..start + take]);
            self.out_count += take;
        }
    }

    pub(crate) fn push(
        &mut self,
        mut input: &[f32],
        out: &mut Vec<f32>,
    ) -> Result<(), ResampleError> {
        self.in_count += input.len();
        while !input.is_empty() {
            let take = (self.chunks_in - self.in_buf.len()).min(input.len());
            self.in_buf.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.in_buf.len() == self.chunks_in {
                let frames = self.process()?;
                self.emit(frames, usize::MAX, out);
            }
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), ResampleError> {
        if self.in_count == 0 {
            return Ok(());
        }
        let expected = self.expected();
        if !self.in_buf.is_empty() {
            let frames = self.process()?;
            self.emit(frames, expected, out);
        }
        let mut rounds = 0;
        while self.out_count < expected && rounds < MAX_TAIL_ROUNDS {
            rounds += 1;
            let frames = self.process()?;
            self.emit(frames, expected, out);
            if frames == 0 {
                break;
            }
        }
        Ok(())
    }

    /// Resamples and clears `in_buf`, zero-padding it to a whole chunk. Returns
    /// the frames written to `out_buf`.
    fn process(&mut self) -> Result<usize, ResampleError> {
        let in_frames = self.in_buf.len();
        let out_frames = self.out_buf.len();
        let indexing = Indexing::new().partial_len(in_frames);
        let frames = {
            let resampler = &mut self.resampler;
            let input = SequentialSlice::new(&self.in_buf, 1, in_frames).unwrap();
            let mut output = SequentialSlice::new_mut(&mut self.out_buf, 1, out_frames).unwrap();
            resampler
                .process_into_buffer(&input, &mut output, Some(&indexing))?
                .1
        };
        self.in_buf.clear();
        Ok(frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribing_includes_preroll_and_keeps_the_full_recovery_clip() {
        let (feed, mut live, failed) = crate::streaming::audio::channel();
        let mut capture = Capture::new();
        capture.set_preroll_cap(100);
        capture.feed(&[0.1; 100]);
        capture.begin(3);
        capture.feed(&[0.2; 200]);
        capture.subscribe(3, feed, 16000);
        capture.feed(&[0.3; 1701]);
        let full = capture.finish(3);
        let mut streamed = Vec::new();
        while let Ok(packet) = live.try_recv() {
            streamed.extend(packet.samples);
        }
        assert_eq!(full.len(), 2001);
        assert_eq!(streamed, full);
        assert!(!failed.load(Ordering::Acquire));
    }

    #[test]
    fn overloaded_subscription_never_truncates_recovery_or_leaks_into_next_recording() {
        let (feed, _live, failed) = crate::streaming::audio::channel();
        let mut capture = Capture::new();
        capture.begin(1);
        capture.subscribe(1, feed, 16000);
        capture.feed(&vec![0.2; 64000]);
        assert!(failed.load(Ordering::Acquire));
        assert_eq!(capture.finish(1).len(), 64000);
        capture.begin(2);
        capture.feed(&[0.4; 123]);
        assert_eq!(capture.finish(2), vec![0.4; 123]);
    }

    #[test]
    fn high_rate_audio_survives_a_brief_worker_stall_without_losing_samples() {
        // A 250 ms scheduling stall at 192 kHz must fit in the promised
        // two-second ring, even when callbacks continue while the worker waits.
        let (mut producer, mut consumer) = HeapRb::<f32>::new(ring_capacity(192_000)).split();
        let input: Vec<f32> = (0..48_000).map(|n| n as f32).collect();
        assert_eq!(producer.push_slice(&input), input.len());
        let mut capture = Capture::new();
        capture.begin(1);
        let mut scratch = vec![0.0; DRAIN_SCRATCH];
        loop {
            let count = consumer.pop_slice(&mut scratch);
            if count == 0 {
                break;
            }
            capture.feed(&scratch[..count]);
        }
        assert_eq!(capture.finish(1), input);
    }

    #[test]
    fn start_moves_undrained_idle_audio_into_the_capped_preroll() {
        // A full idle interval of audio is still in the ring when Start
        // arrives. The clip must hold exactly the newest pre-roll, then only
        // audio captured after the press.
        let rate = 48_000;
        let cap = preroll_capacity(rate);
        let (mut producer, mut consumer) = HeapRb::<f32>::new(ring_capacity(rate)).split();
        let mut capture = Capture::new();
        capture.set_preroll_cap(cap);
        let mut scratch = vec![0.0; DRAIN_SCRATCH];
        let idle_drain = rate as usize * IDLE_POLL_INTERVAL.as_millis() as usize / 1000;
        assert!(idle_drain < ring_capacity(rate));

        let drained_earlier = vec![1.0; cap];
        assert_eq!(producer.push_slice(&drained_earlier), cap);
        drain(&mut consumer, &mut scratch, &mut capture);
        let undrained: Vec<f32> = (0..idle_drain).map(|n| 2.0 + n as f32).collect();
        assert_eq!(producer.push_slice(&undrained), idle_drain);

        start_capture(Some(&mut consumer), &mut scratch, &mut capture, 1);
        let after = vec![-1.0; 480];
        assert_eq!(producer.push_slice(&after), after.len());
        drain(&mut consumer, &mut scratch, &mut capture);

        let clip = capture.finish(1);
        let before_press: Vec<f32> = drained_earlier.into_iter().chain(undrained).collect();
        assert_eq!(clip.len(), cap + after.len());
        assert_eq!(clip[..cap], before_press[before_press.len() - cap..]);
        assert_eq!(clip[cap..], after[..]);
    }

    #[test]
    fn downmix_averages_stereo_frames() {
        let mut out = Vec::new();
        downmix_into(&[1.0f32, 0.0, 0.0, 1.0], 2, &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
        out.clear();
        downmix_into(&[-1.0f32, 1.0, 1.0, -1.0], 2, &mut out);
        assert_eq!(out, vec![0.0, 0.0]);
    }

    #[test]
    fn downmix_passes_mono_through() {
        let mut out = Vec::new();
        downmix_into(&[0.1f32, 0.2, 0.3], 1, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn f32_to_i16_clamps_and_rounds() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), -i16::MAX);
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX);
        assert_eq!(f32_to_i16(0.5), 16384);
        assert_eq!(f32_to_i16(f32::NAN), 0);
    }

    #[test]
    fn a_new_session_never_contains_previous_audio() {
        let mut capture = Capture::new();
        capture.set_preroll_cap(4);

        // Idle audio fills the (capped) pre-roll.
        capture.feed(&[1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        capture.begin(1);
        capture.feed(&[2.0, 2.0]);
        assert_eq!(capture.finish(1), vec![1.0, 1.0, 1.0, 1.0, 2.0, 2.0]);

        // The next session holds its own pre-roll and audio only.
        capture.feed(&[3.0, 3.0, 3.0, 3.0]);
        capture.begin(2);
        capture.feed(&[4.0]);
        let second = capture.finish(2);
        assert_eq!(second, vec![3.0, 3.0, 3.0, 3.0, 4.0]);
        assert!(!second.contains(&1.0) && !second.contains(&2.0));
    }

    #[test]
    fn preroll_keeps_only_the_most_recent_cap() {
        let mut capture = Capture::new();
        capture.set_preroll_cap(2);
        capture.feed(&[1.0, 2.0, 3.0]);
        capture.begin(1);
        assert_eq!(capture.finish(1), vec![2.0, 3.0]);
    }

    #[test]
    fn finish_with_a_stale_generation_is_a_noop() {
        let mut capture = Capture::new();
        capture.begin(5);
        capture.feed(&[7.0, 7.0]);
        assert!(capture.finish(6).is_empty());
        assert_eq!(capture.finish(5), vec![7.0, 7.0]);
    }

    #[test]
    fn resampling_48k_to_16k_has_exact_length() {
        let in_rate = 48_000usize;
        let frequency = 1_000.0;
        let frames = in_rate / 10; // 100 ms
        let input: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / in_rate as f32;
                (2.0 * std::f32::consts::PI * frequency * t).sin() * 0.5
            })
            .collect();

        let out = resample_to_16k(&input, in_rate as u32);

        assert_eq!(out.len(), frames * SAMPLE_RATE as usize / in_rate);
        let peak = out.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
        assert!(
            (0.35..0.6).contains(&peak),
            "peak amplitude {peak} is not close to 0.5"
        );
    }

    #[test]
    fn resampling_keeps_an_onset_aligned() {
        let in_rate = 48_000u32;
        let frames = in_rate as usize;
        let onset = 12_000usize;
        let input: Vec<f32> = (0..frames)
            .map(|i| if i >= onset { 1.0 } else { 0.0 })
            .collect();

        let out = resample_to_16k(&input, in_rate);
        assert_eq!(out.len(), frames * SAMPLE_RATE as usize / in_rate as usize);

        let crossed = out
            .iter()
            .position(|&sample| sample >= 0.5)
            .expect("the step should cross 0.5");
        let expected = onset * SAMPLE_RATE as usize / in_rate as usize;
        assert!(
            crossed.abs_diff(expected) <= 4,
            "onset crossed at {crossed}, expected ~{expected}"
        );
    }

    #[test]
    fn idle_worker_reopens_a_failed_microphone_only() {
        let shared = Shared::default();
        shared.open.store(true, Ordering::SeqCst);
        assert!(!device_needs_reopen(&shared, None));
        shared.failed.store(true, Ordering::SeqCst);
        assert!(device_needs_reopen(&shared, None));
        shared.failed.store(false, Ordering::SeqCst);
        shared.open.store(false, Ordering::SeqCst);
        assert!(device_needs_reopen(&shared, None));
    }

    #[test]
    fn resume_rechecks_the_default_only_for_a_direct_stream() {
        // The default route is never queried: cpal invalidates it on a switch.
        assert!(!default_moved::<u8>(None, || panic!("queried")));
        assert!(!default_moved(Some(&1), || Some(1)));
        assert!(default_moved(Some(&1), || Some(2)));
        // No default input at all: keep the stream; a failure reopens it.
        assert!(!default_moved(Some(&1), || None));
    }

    #[test]
    fn idle_mic_pauses_only_after_the_idle_period_without_a_recording() {
        let t0 = Instant::now();
        let after = Duration::from_secs(60);
        let mut idle = IdleMic::new(after, t0);
        assert!(!idle.pause_due(t0 + Duration::from_secs(59), false));
        assert!(idle.pause_due(t0 + after, false));
        // Never while recording, however long it lasts.
        assert!(!idle.pause_due(t0 + Duration::from_secs(600), true));

        // A recording ending restarts the count.
        idle.active(t0 + Duration::from_secs(50));
        assert!(!idle.pause_due(t0 + Duration::from_secs(100), false));
        assert!(idle.pause_due(t0 + Duration::from_secs(110), false));

        // Paused once, not again until resumed.
        idle.paused = true;
        assert!(!idle.pause_due(t0 + Duration::from_secs(600), false));
        idle.paused = false;
        idle.active(t0 + Duration::from_secs(600));
        assert!(!idle.pause_due(t0 + Duration::from_secs(601), false));
    }

    #[test]
    fn worker_blocks_while_paused_and_polls_otherwise() {
        let mut idle = IdleMic::new(MIC_IDLE_PAUSE, Instant::now());
        assert_eq!(idle.wait(false), Some(IDLE_POLL_INTERVAL));
        assert_eq!(idle.wait(true), Some(POLL_INTERVAL));
        idle.paused = true;
        assert_eq!(idle.wait(false), None);
    }

    #[test]
    fn resume_drops_the_audio_buffered_before_the_pause() {
        // At the pause the pre-roll is full and the ring holds a last undrained
        // chunk. After the resume the clip must hold only post-resume audio.
        let rate = 48_000;
        let cap = preroll_capacity(rate);
        let (mut producer, mut consumer) = HeapRb::<f32>::new(ring_capacity(rate)).split();
        let mut capture = Capture::new();
        capture.set_preroll_cap(cap);
        let mut scratch = vec![0.0; DRAIN_SCRATCH];
        assert_eq!(producer.push_slice(&vec![1.0; cap]), cap);
        drain(&mut consumer, &mut scratch, &mut capture);
        assert_eq!(producer.push_slice(&[2.0; 480]), 480);

        discard_buffered(&mut consumer, &mut scratch, &mut capture);
        let fresh: Vec<f32> = (0..960).map(|n| -(n as f32)).collect();
        assert_eq!(producer.push_slice(&fresh), fresh.len());
        start_capture(Some(&mut consumer), &mut scratch, &mut capture, 1);
        drain(&mut consumer, &mut scratch, &mut capture);
        assert_eq!(capture.finish(1), fresh);
    }

    #[test]
    fn stale_filter_drops_blocks_captured_before_the_resume() {
        let ms = |n: u64| n * 1_000_000;
        let resume_at = AtomicU64::new(0);
        let mut filter = StaleFilter::default();
        // Nothing armed: every block passes, however old.
        assert!(!filter.drops(&resume_at, ms(1), 480, 4800));

        // Resumed at t = 70 s after a pause at t = 10 s.
        resume_at.store(ms(70_000), Ordering::Release);
        assert!(filter.drops(&resume_at, ms(9_990), 480, 4800));
        assert!(filter.drops(&resume_at, ms(10_000), 480, 4800));
        // The first fresh block passes and disarms the filter.
        assert!(!filter.drops(&resume_at, ms(70_001), 480, 4800));
        assert_eq!(resume_at.load(Ordering::Acquire), 0);
        assert!(!filter.drops(&resume_at, ms(10_000), 480, 4800));
    }

    #[test]
    fn stale_filter_keeps_audio_within_the_margin_and_caps_its_drops() {
        let ms = |n: u64| n * 1_000_000;
        let resume_at = AtomicU64::new(ms(70_000));
        let mut filter = StaleFilter::default();
        // Captured just before the resume instant: clock jitter, not stale.
        assert!(!filter.drops(&resume_at, ms(69_990), 480, 4800));

        // Bogus timestamps (always 0) lose at most the cap, then pass.
        resume_at.store(ms(130_000), Ordering::Release);
        let dropped = (0..20)
            .filter(|_| filter.drops(&resume_at, 0, 480, 960))
            .count();
        assert_eq!(dropped, 2);
        assert_eq!(resume_at.load(Ordering::Acquire), 0);

        // The next resume starts its own count.
        resume_at.store(ms(190_000), Ordering::Release);
        assert!(filter.drops(&resume_at, 0, 960, 960));
    }

    #[test]
    fn only_stream_breaking_errors_force_a_reopen() {
        use cpal::ErrorKind;
        for kind in [
            ErrorKind::Xrun,
            ErrorKind::DeviceChanged,
            ErrorKind::RealtimeDenied,
        ] {
            assert!(!stream_error_is_fatal(kind), "{kind:?}");
        }
        for kind in [
            ErrorKind::StreamInvalidated,
            ErrorKind::DeviceNotAvailable,
            ErrorKind::BackendError,
            ErrorKind::Other,
        ] {
            assert!(stream_error_is_fatal(kind), "{kind:?}");
        }
    }

    #[test]
    fn resampling_at_16k_is_a_passthrough() {
        let input = vec![0.25f32, -0.25, 0.5];
        assert_eq!(resample_to_16k(&input, SAMPLE_RATE), input);
    }

    #[cfg(windows)]
    #[test]
    fn audio_worker_apartment_stays_mta_after_cpal_initialization() {
        thread::spawn(|| {
            use windows_sys::Win32::System::Com::{CoGetApartmentType, APTTYPE_MTA};
            let _apartment = AudioThreadApartment::initialize().unwrap();
            // CPAL normally tries STA initialization. It must retain the MTA
            // already selected for our worker, including subsequent queries.
            let host = cpal::default_host();
            let _ = host.default_input_device();
            let mut apartment = -1;
            let mut qualifier = 0;
            assert!(unsafe { CoGetApartmentType(&mut apartment, &mut qualifier) } >= 0);
            assert_eq!(apartment, APTTYPE_MTA);
        })
        .join()
        .unwrap();
    }

    /// Real hardware check. Run with `cargo test -- --ignored --nocapture`.
    #[cfg(windows)]
    #[test]
    #[ignore = "opens the default microphone without saving or uploading audio"]
    fn default_microphone_opens_and_reopens_on_the_audio_worker() {
        let recorder = Recorder::new().expect("initial microphone open");
        for _ in 0..3 {
            recorder
                .reopen()
                .expect("microphone reopen on the same worker");
            // Device-change callbacks set this flag; the next hotkey start
            // must recover through the same reopen path without a COM clash.
            recorder.shared.failed.store(true, Ordering::SeqCst);
            recorder
                .start()
                .expect("recover at the next recording start");
            let _ = recorder.stop();
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "opens the default microphone without saving or uploading audio"]
    fn microphone_opens_after_thread_pool_threads_turn_sta() {
        use std::ffi::c_void;
        use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        use windows_sys::Win32::System::Threading::{
            TrySubmitThreadpoolCallback, PTP_CALLBACK_INSTANCE,
        };

        // cpal's device notification callbacks call CoInitializeEx(STA) on
        // the Windows thread pool and never undo it. Do the same to every
        // pool thread, then open and reopen as the app does.
        unsafe extern "system" fn taint(_: PTP_CALLBACK_INSTANCE, _: *mut c_void) {
            unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
            thread::sleep(Duration::from_millis(200));
        }
        for _ in 0..64 {
            assert!(
                unsafe {
                    TrySubmitThreadpoolCallback(Some(taint), std::ptr::null_mut(), std::ptr::null())
                } != 0
            );
        }
        thread::sleep(Duration::from_millis(500));

        let recorder = Recorder::new().expect("open with STA pool threads");
        recorder.reopen().expect("reopen with STA pool threads");
    }

    /// Real hardware check. Run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_after_an_idle_pause_without_preroll() {
        let recorder =
            Recorder::with_idle_pause(Duration::from_millis(300)).expect("a default input device");
        for round in 0..3 {
            // Paused after 300 ms (polled every 100 ms).
            thread::sleep(Duration::from_millis(800));
            let started = Instant::now();
            recorder.start().expect("resume and start");
            let start_ms = started.elapsed().as_secs_f64() * 1e3;
            thread::sleep(Duration::from_millis(500));
            let clip = recorder.stop();
            println!(
                "round {round}: start {start_ms:.2} ms, clip {:?}",
                clip.duration()
            );
            // Resumed, so no 150 ms pre-roll: about the hold alone.
            assert!(
                clip.duration() >= Duration::from_millis(400),
                "{:?}",
                clip.duration()
            );
            assert!(
                clip.duration() <= Duration::from_millis(560),
                "{:?}",
                clip.duration()
            );
        }
    }

    /// Real hardware check. Run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_from_the_default_device() {
        let recorder = Recorder::new().expect("a default input device");
        recorder.start().expect("start recording");
        assert!(recorder.is_recording());
        assert!(
            recorder.start().is_err(),
            "a second start must be rejected while recording"
        );

        thread::sleep(Duration::from_secs(1));
        let first = recorder.stop();
        assert!(!recorder.is_recording());
        assert_eq!(first.sample_rate, SAMPLE_RATE);
        println!(
            "first: {} samples ({:?}), dropped {}",
            first.samples.len(),
            first.duration(),
            recorder.dropped_samples()
        );
        assert!(
            first.duration() >= Duration::from_millis(900),
            "only {:?} captured",
            first.duration()
        );

        // Once the idle stream has run a while, a start/stop with no delay
        // should still return the pre-roll, not an empty clip.
        thread::sleep(Duration::from_millis(400));
        recorder.start().expect("pre-roll recording");
        let preroll = recorder.stop();
        println!("pre-roll: {:?}", preroll.duration());
        assert!(
            preroll.duration() >= Duration::from_millis(100),
            "pre-roll missing: only {:?} captured",
            preroll.duration()
        );

        // A later recording still works and is independent.
        recorder.start().expect("second recording");
        thread::sleep(Duration::from_millis(300));
        let second = recorder.stop();
        assert!(second.duration() >= Duration::from_millis(250));
        assert_eq!(second.sample_rate, SAMPLE_RATE);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    #[test]
    fn raw_recovery_is_lazy_complete_and_cached() {
        for rate in [16000, 44100, 48000] {
            let raw: Vec<f32> = (0..rate).map(|i| (i as f32 * 0.02).sin() * 0.25).collect();
            let expected: Vec<i16> = resample_to_16k(&raw, rate)
                .into_iter()
                .map(f32_to_i16)
                .collect();
            let recovery = RecoveryAudio {
                raw,
                rate,
                clip: OnceLock::new(),
            };
            assert_eq!(recovery.duration(), Duration::from_secs(1));
            assert!(!recovery.is_empty());
            assert!(recovery.clip.get().is_none());
            assert_eq!(recovery.clip().samples, expected);
            assert!(std::ptr::eq(recovery.clip(), recovery.clip()));
            assert_eq!(recovery.into_clip().samples, expected);
        }
    }
    #[test]
    fn existing_batch_clip_is_reused_without_conversion() {
        let recovery: RecoveryAudio = Clip::new(vec![-1, 0, 1]).into();
        assert_eq!(recovery.into_clip().samples, [-1, 0, 1]);
    }
    #[test]
    fn multi_second_recovery_converts_to_exact_16k_length() {
        let recovery = RecoveryAudio {
            raw: vec![0.25; 48000 * 5],
            rate: 48000,
            clip: OnceLock::new(),
        };
        assert_eq!(recovery.clip().samples.len(), 16000 * 5);
    }
    /// Timing printout, not a check: 160 s of audio resampled in an unoptimized
    /// build dominates `cargo test` wall time. Run with `-- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement only; slow in debug builds"]
    fn synthetic_conversion_measurements() {
        for seconds in [5, 35, 120] {
            let raw = vec![0.25; 48000 * seconds];
            let started = std::time::Instant::now();
            let recovery = RecoveryAudio {
                raw,
                rate: 48000,
                clip: OnceLock::new(),
            };
            let deferred_us = started.elapsed().as_micros();
            assert!(recovery.clip.get().is_none());
            let conversion = std::time::Instant::now();
            assert_eq!(recovery.clip().samples.len(), 16000 * seconds);
            println!(
                "synthetic {seconds}s: deferred={deferred_us}us conversion={}us",
                conversion.elapsed().as_micros()
            );
        }
    }
}
