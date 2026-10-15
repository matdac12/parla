//! Pre-warmed microphone capture.
//!
//! The default input device is opened once, at startup, and its stream is kept
//! running so pressing the hotkey has no device-startup latency.
//!
//! The realtime cpal callback does two things only: convert the device samples
//! to mono f32 into a reused scratch buffer and push them into a lock-free SPSC
//! ring ([`ringbuf`]). No allocation, no blocking, no shared lock on the audio
//! thread. A worker thread drains the ring and owns the recording buffer; the
//! recording boundary is a generation id carried in the [`Capture`] state, so a
//! clip can never contain audio from before its `start()`. While idle the
//! worker keeps the most recent [`PRE_ROLL_MS`] of audio as a pre-roll, so the
//! first syllable is not clipped.
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
use std::time::Duration;

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
        let shared = Arc::new(Shared {
            control: Mutex::new(Control::default()),
            open: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            rate: AtomicU32::new(SAMPLE_RATE),
            dropped: AtomicU64::new(0),
        });

        let (command_tx, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("parla-audio".into())
            .spawn(move || audio_thread(worker_shared, command_rx, ready_tx))
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
    /// device first if it never opened or was lost.
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

    loop {
        if let Some(session) = session.as_mut() {
            drain(&mut session.consumer, &mut scratch, &mut capture);
        }

        let interval = if capture.active.is_some() {
            POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        match commands.recv_timeout(interval) {
            Ok(Command::Start { generation, ack }) => {
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
            }
            Ok(Command::Subscribe { generation, feed }) => {
                capture.subscribe(generation, feed, shared.rate.load(Ordering::Acquire));
            }
            Ok(Command::Reopen { ack }) => {
                drop(session.take());
                shared.open.store(false, Ordering::SeqCst);
                match open_session(&shared) {
                    Ok(new) => {
                        shared.failed.store(false, Ordering::SeqCst);
                        shared.open.store(true, Ordering::SeqCst);
                        capture.set_preroll_cap(preroll_capacity(new.rate));
                        session = Some(new);
                        let _ = ack.send(Ok(()));
                    }
                    Err(error) => {
                        shared.failed.store(true, Ordering::SeqCst);
                        let _ = ack.send(Err(error));
                    }
                }
            }
            Ok(Command::Shutdown) => {
                shared.open.store(false, Ordering::SeqCst);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// An open microphone owned by the worker thread. `stream` keeps the device
/// running; `consumer` is the read end of its sample ring.
struct OpenSession {
    /// Held only to keep the device running; dropping it closes the stream.
    #[allow(dead_code)]
    stream: cpal::Stream,
    consumer: HeapCons<f32>,
    rate: u32,
}

/// Opens the default input device at its OS default format and starts the
/// stream, returning the session that owns it.
fn open_session(shared: &Arc<Shared>) -> Result<OpenSession, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "no default input device".to_owned())?;
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
        SampleFormat::I8 => build_stream::<i8>(&device, &config, producer, shared, channels),
        SampleFormat::I16 => build_stream::<i16>(&device, &config, producer, shared, channels),
        SampleFormat::I32 => build_stream::<i32>(&device, &config, producer, shared, channels),
        SampleFormat::I64 => build_stream::<i64>(&device, &config, producer, shared, channels),
        SampleFormat::U8 => build_stream::<u8>(&device, &config, producer, shared, channels),
        SampleFormat::U16 => build_stream::<u16>(&device, &config, producer, shared, channels),
        SampleFormat::U32 => build_stream::<u32>(&device, &config, producer, shared, channels),
        SampleFormat::U64 => build_stream::<u64>(&device, &config, producer, shared, channels),
        SampleFormat::F32 => build_stream::<f32>(&device, &config, producer, shared, channels),
        SampleFormat::F64 => build_stream::<f64>(&device, &config, producer, shared, channels),
        other => return Err(format!("unsupported input sample format {other}")),
    }?;

    stream
        .play()
        .map_err(|error| format!("starting the microphone stream failed: {error}"))?;
    Ok(OpenSession {
        stream,
        consumer,
        rate,
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
            move |data: &[T], _: &cpal::InputCallbackInfo| {
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
                error_shared.failed.store(true, Ordering::SeqCst);
                error_shared.open.store(false, Ordering::SeqCst);
            },
            None,
        )
        .map_err(|error| format!("building the input stream failed: {error}"))
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
