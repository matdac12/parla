//! Pre-warmed microphone capture.
//!
//! The default input device is opened once, at startup, and its stream is kept
//! running so pressing the hotkey has no device-startup latency. Audio is
//! dropped while idle; [`Recorder::start`] begins buffering and
//! [`Recorder::stop`] returns the recording as a 16 kHz mono [`Clip`].
//!
//! The cpal `Stream` is `!Send` on some platforms, so it lives on a dedicated
//! thread that talks to callers only through atomics, a mutex and a command
//! channel. The design (dedicated thread, mix-to-mono, rubato resampling,
//! reopen-on-next-start after a device failure) follows Handy's recorder
//! (github.com/cjpais/Handy, MIT); see `THIRD_PARTY_NOTICES.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use rubato::audioadapter_buffers::direct::SequentialSlice;
use rubato::{
    Fft, FixedSync, Indexing, ResampleError, Resampler, ResamplerConstructionError, WindowFunction,
};

use crate::clip::{Clip, SAMPLE_RATE};

/// How long `start` waits for the device after a disconnect before giving up.
const REOPEN_TIMEOUT: Duration = Duration::from_secs(2);

/// rubato's FFT block size. Larger blocks resample more cleanly at the cost of
/// a little more output latency; 1024 is Handy's proven choice.
const RESAMPLER_CHUNK: usize = 1024;

/// Cap on zero-chunk rounds while draining the resampler's delay line.
const MAX_TAIL_ROUNDS: usize = 8;

/// The one recorder the app uses. Created at startup, retried lazily if the
/// machine had no input device then.
static RECORDER: OnceLock<Arc<Recorder>> = OnceLock::new();

/// Opens the microphone at startup, logging (not failing) when there is none.
pub fn warm_up() {
    let _ = recorder();
}

/// The shared recorder, or `None` when no input device can be opened.
pub fn recorder() -> Option<Arc<Recorder>> {
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

/// State shared between the public handle and the audio thread.
struct Shared {
    /// Set while buffering; the callback ignores audio when false.
    recording: AtomicBool,
    /// Mono f32 samples at the device rate, appended by the audio callback.
    buffer: Mutex<Vec<f32>>,
    /// The open device's sample rate, so `stop` knows how to resample.
    rate: Mutex<u32>,
    /// Set by the stream's error callback; cleared by a successful reopen.
    failed: AtomicBool,
    /// Whether a stream is currently running.
    open: AtomicBool,
}

/// Commands the audio thread services while it parks on the open stream.
enum Command {
    Reopen(Sender<Result<(), String>>),
    Shutdown,
}

/// A microphone that is open and running. Cheap to share via `Arc`.
pub struct Recorder {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Opens the default input device and starts its stream inside a dedicated
    /// thread. Errors when no device can be opened.
    pub fn new() -> Result<Recorder> {
        let shared = Arc::new(Shared {
            recording: AtomicBool::new(false),
            buffer: Mutex::new(Vec::new()),
            rate: Mutex::new(SAMPLE_RATE),
            failed: AtomicBool::new(false),
            open: AtomicBool::new(false),
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

    /// Starts buffering. Reopens the device first if it never opened or was
    /// lost (unplugged, changed).
    pub fn start(&self) -> Result<()> {
        if !self.shared.open.load(Ordering::SeqCst) || self.shared.failed.load(Ordering::SeqCst) {
            self.reopen()?;
        }
        self.shared
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        self.shared.recording.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Stops buffering and returns what was captured, resampled to 16 kHz mono
    /// i16. Returns an empty clip when nothing was being recorded.
    pub fn stop(&self) -> Clip {
        self.shared.recording.store(false, Ordering::SeqCst);
        let raw = {
            let mut buffer = self
                .shared
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *buffer)
        };
        let rate = *self
            .shared
            .rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let samples = resample_to_16k(&raw, rate)
            .into_iter()
            .map(f32_to_i16)
            .collect();
        Clip::new(samples)
    }

    /// Whether audio is currently being buffered.
    pub fn is_recording(&self) -> bool {
        self.shared.recording.load(Ordering::SeqCst)
    }

    fn reopen(&self) -> Result<()> {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.commands
            .send(Command::Reopen(ack_tx))
            .map_err(|_| anyhow!("audio thread is not running"))?;
        match ack_rx.recv_timeout(REOPEN_TIMEOUT) {
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

/// Owns the cpal stream. Opened here and never leaves this thread.
fn audio_thread(
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    ready: Sender<Result<(), String>>,
) {
    let (mut stream, startup) = match open_stream(&shared) {
        Ok(stream) => (Some(stream), Ok(())),
        Err(error) => (None, Err(error)),
    };
    shared.open.store(stream.is_some(), Ordering::SeqCst);
    shared.failed.store(stream.is_none(), Ordering::SeqCst);
    if ready.send(startup).is_err() {
        return;
    }

    while let Ok(command) = commands.recv() {
        match command {
            Command::Reopen(ack) => {
                drop(stream.take());
                shared.open.store(false, Ordering::SeqCst);
                match open_stream(&shared) {
                    Ok(new) => {
                        shared.failed.store(false, Ordering::SeqCst);
                        shared.open.store(true, Ordering::SeqCst);
                        stream = Some(new);
                        let _ = ack.send(Ok(()));
                    }
                    Err(error) => {
                        shared.failed.store(true, Ordering::SeqCst);
                        let _ = ack.send(Err(error));
                    }
                }
            }
            Command::Shutdown => {
                drop(stream.take());
                shared.open.store(false, Ordering::SeqCst);
                return;
            }
        }
    }
}

/// Opens the default input device at its OS default format and starts the
/// stream. Records the device format in `shared` for `stop` to read.
fn open_stream(shared: &Arc<Shared>) -> Result<cpal::Stream, String> {
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

    *shared
        .rate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = config.sample_rate;

    let stream = match sample_format {
        SampleFormat::I8 => build_stream::<i8>(&device, &config, shared, channels),
        SampleFormat::I16 => build_stream::<i16>(&device, &config, shared, channels),
        SampleFormat::I32 => build_stream::<i32>(&device, &config, shared, channels),
        SampleFormat::I64 => build_stream::<i64>(&device, &config, shared, channels),
        SampleFormat::U8 => build_stream::<u8>(&device, &config, shared, channels),
        SampleFormat::U16 => build_stream::<u16>(&device, &config, shared, channels),
        SampleFormat::U32 => build_stream::<u32>(&device, &config, shared, channels),
        SampleFormat::U64 => build_stream::<u64>(&device, &config, shared, channels),
        SampleFormat::F32 => build_stream::<f32>(&device, &config, shared, channels),
        SampleFormat::F64 => build_stream::<f64>(&device, &config, shared, channels),
        other => return Err(format!("unsupported input sample format {other}")),
    }?;

    stream
        .play()
        .map_err(|error| format!("starting the microphone stream failed: {error}"))?;
    Ok(stream)
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: &Arc<Shared>,
    channels: usize,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let data_shared = Arc::clone(shared);
    let error_shared = Arc::clone(shared);
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                if !data_shared.recording.load(Ordering::Relaxed) {
                    return;
                }
                let mono = collect_mono(data, channels);
                if let Ok(mut buffer) = data_shared.buffer.lock() {
                    buffer.extend_from_slice(&mono);
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

/// Converts a device-format buffer to mono f32, averaging channels.
fn collect_mono<T>(data: &[T], channels: usize) -> Vec<f32>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let interleaved: Vec<f32> = data
        .iter()
        .map(|&sample| f32::from_sample(sample))
        .collect();
    downmix_to_mono(&interleaved, channels)
}

/// Averages interleaved f32 frames down to one channel.
pub(crate) fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Clamps a normalized float sample into the i16 range.
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// Resamples mono f32 audio from `in_rate` to Parla's 16 kHz using rubato's
/// windowed FFT resampler (proper anti-aliasing, not naive decimation).
fn resample_to_16k(input: &[f32], in_rate: u32) -> Vec<f32> {
    if input.is_empty() || in_rate == 0 || in_rate == SAMPLE_RATE {
        return input.to_vec();
    }
    let mut resampler = MonoResampler::new(in_rate as usize, SAMPLE_RATE as usize)
        .expect("a positive device rate always yields a valid resampler");
    let mut out = Vec::with_capacity(input.len() * SAMPLE_RATE as usize / in_rate as usize + 1024);
    if let Err(error) = resampler.push(input, &mut out) {
        eprintln!("resampling failed: {error}");
    }
    if let Err(error) = resampler.finish(&mut out) {
        eprintln!("flushing the resampler failed: {error}");
    }
    out
}

/// Mono-only subset of Handy's `FrameResampler`: emits whole output samples in
/// order, then flushes the resampler's delay line so the tail is not lost.
struct MonoResampler {
    resampler: Fft<f32>,
    chunks_in: usize,
    out_buf: Vec<f32>,
    in_buf: Vec<f32>,
    in_count: usize,
    out_count: usize,
    in_hz: usize,
    out_hz: usize,
}

impl MonoResampler {
    fn new(in_hz: usize, out_hz: usize) -> Result<Self, ResamplerConstructionError> {
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
        Ok(Self {
            resampler,
            chunks_in: RESAMPLER_CHUNK,
            out_buf: vec![0.0; out_frames],
            in_buf: Vec::with_capacity(RESAMPLER_CHUNK),
            in_count: 0,
            out_count: 0,
            in_hz,
            out_hz,
        })
    }

    fn push(&mut self, mut input: &[f32], out: &mut Vec<f32>) -> Result<(), ResampleError> {
        self.in_count += input.len();
        while !input.is_empty() {
            let take = (self.chunks_in - self.in_buf.len()).min(input.len());
            self.in_buf.extend_from_slice(&input[..take]);
            input = &input[take..];
            if self.in_buf.len() == self.chunks_in {
                let frames = self.process()?;
                self.out_count += frames;
                out.extend_from_slice(&self.out_buf[..frames]);
            }
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), ResampleError> {
        if self.in_count == 0 {
            return Ok(());
        }
        let expected = self.in_count * self.out_hz / self.in_hz + self.resampler.output_delay();
        if !self.in_buf.is_empty() {
            let frames = self.process()?;
            let take = expected.saturating_sub(self.out_count).min(frames);
            self.out_count += take;
            out.extend_from_slice(&self.out_buf[..take]);
        }
        let mut rounds = 0;
        while self.out_count < expected && rounds < MAX_TAIL_ROUNDS {
            rounds += 1;
            let frames = self.process()?;
            let take = (expected - self.out_count).min(frames);
            self.out_count += take;
            out.extend_from_slice(&self.out_buf[..take]);
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
    fn downmix_averages_stereo_frames() {
        assert_eq!(downmix_to_mono(&[1.0, 0.0], 2), vec![0.5]);
        assert_eq!(downmix_to_mono(&[1.0, 0.0, 0.0, 1.0], 2), vec![0.5, 0.5]);
        assert_eq!(downmix_to_mono(&[-1.0, 1.0, 1.0, -1.0], 2), vec![0.0, 0.0]);
    }

    #[test]
    fn downmix_passes_mono_through() {
        assert_eq!(downmix_to_mono(&[0.1, 0.2, 0.3], 1), vec![0.1, 0.2, 0.3]);
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
    fn resampling_48k_to_16k_preserves_length_and_amplitude() {
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

        let ideal = frames * SAMPLE_RATE as usize / in_rate;
        assert!(
            out.len().abs_diff(ideal) < 256,
            "length {} is not close to {}",
            out.len(),
            ideal
        );
        let peak = out.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
        assert!(
            (0.35..0.6).contains(&peak),
            "peak amplitude {peak} is not close to 0.5"
        );
    }

    /// Real hardware check. Run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_from_the_default_device() {
        let recorder = Recorder::new().expect("a default input device");
        recorder.start().expect("start recording");
        thread::sleep(Duration::from_secs(1));
        let clip = recorder.stop();
        assert_eq!(clip.sample_rate, SAMPLE_RATE);
        println!(
            "captured {} samples ({:?})",
            clip.samples.len(),
            clip.duration()
        );
        assert!(
            clip.duration() >= Duration::from_millis(900),
            "only {:?} captured",
            clip.duration()
        );
    }

    #[test]
    fn resampling_at_16k_is_a_passthrough() {
        let input = vec![0.25f32, -0.25, 0.5];
        assert_eq!(resample_to_16k(&input, SAMPLE_RATE), input);
    }
}
