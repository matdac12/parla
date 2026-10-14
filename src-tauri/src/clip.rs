//! A recorded utterance: 16 kHz mono 16-bit PCM, plus WAV input/output.
//!
//! Speech-to-text providers ([`crate::provider`]) consume [`Clip`], so the type
//! is deliberately small and stable: owned samples plus their rate.

use std::io::{Cursor, Read, Seek};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};

use crate::recorder::f32_to_i16;

/// Parla's canonical audio format, and the one every provider in the spec
/// expects: mono, signed 16-bit PCM, 16 kHz.
pub const SAMPLE_RATE: u32 = 16_000;

/// A captured clip. `sample_rate` is 16 000 for anything the recorder
/// produces; [`Clip::from_wav_bytes`] preserves whatever the file declares.
pub struct Clip {
    pub samples: Vec<i16>,
    pub sample_rate: u32,
}

impl Clip {
    /// A clip at the canonical 16 kHz rate.
    pub fn new(samples: Vec<i16>) -> Self {
        Self {
            samples,
            sample_rate: SAMPLE_RATE,
        }
    }

    /// Playback length, derived from the sample count and rate.
    pub fn duration(&self) -> Duration {
        if self.sample_rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(self.samples.len() as f64 / self.sample_rate as f64)
    }

    /// The clip as a complete RIFF/WAVE byte string (16-bit mono PCM).
    pub fn to_wav_bytes(&self) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            // Writing to an in-memory cursor cannot fail; only the header and
            // samples are, at most, a few hundred kilobytes.
            let mut writer =
                WavWriter::new(&mut cursor, self.spec()).expect("in-memory WAV writer");
            for &sample in &self.samples {
                writer.write_sample(sample).expect("in-memory WAV write");
            }
            writer.finalize().expect("in-memory WAV finalize");
        }
        cursor.into_inner()
    }

    /// Writes [`Clip::to_wav_bytes`] to `path`, creating parent directories.
    pub fn save_wav(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        std::fs::write(path, self.to_wav_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Reads a WAV from memory, downmixing multi-channel audio to mono. The
    /// supported encodings are the ones a mic or an espeak-ng fixture produce.
    pub fn from_wav_bytes(bytes: &[u8]) -> Result<Clip> {
        let mut reader = WavReader::new(Cursor::new(bytes)).context("reading WAV header")?;
        let spec = reader.spec();
        let interleaved = read_samples(&mut reader, &spec)?;
        Ok(Clip {
            samples: downmix_i16(&interleaved, spec.channels as usize),
            sample_rate: spec.sample_rate,
        })
    }

    fn spec(&self) -> WavSpec {
        WavSpec {
            channels: 1,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        }
    }
}

fn read_samples<R: Read + Seek>(reader: &mut WavReader<R>, spec: &WavSpec) -> Result<Vec<i16>> {
    let samples = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Int, 8) => reader
            .samples::<i8>()
            .map(|s| s.map(|v| (v as i16) << 8))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        (SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .collect::<std::result::Result<Vec<_>, _>>()?,
        (SampleFormat::Int, 24) => reader
            .samples::<i32>()
            .map(|s| s.map(|v| (v >> 8) as i16))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        (SampleFormat::Int, 32) => reader
            .samples::<i32>()
            .map(|s| s.map(|v| (v >> 16) as i16))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        (SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .map(|s| s.map(f32_to_i16))
            .collect::<std::result::Result<Vec<_>, _>>()?,
        _ => bail!(
            "unsupported WAV format: {:?} {} bit",
            spec.sample_format,
            spec.bits_per_sample
        ),
    };
    Ok(samples)
}

fn downmix_i16(interleaved: &[i16], channels: usize) -> Vec<i16> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&sample| sample as i32).sum();
            (sum / channels as i32) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_is_samples_over_rate() {
        let clip = Clip::new(vec![0; SAMPLE_RATE as usize]);
        assert_eq!(clip.duration(), Duration::from_secs(1));

        let half = Clip::new(vec![0; SAMPLE_RATE as usize / 2]);
        assert_eq!(half.duration(), Duration::from_millis(500));
    }

    #[test]
    fn wav_round_trip_preserves_samples() {
        let samples: Vec<i16> = (-1000..1000).map(|v| (v * 7) as i16).collect();
        let clip = Clip::new(samples.clone());

        let bytes = clip.to_wav_bytes();
        let restored = Clip::from_wav_bytes(&bytes).expect("round trip");

        assert_eq!(restored.samples, samples);
        assert_eq!(restored.sample_rate, SAMPLE_RATE);
    }

    #[test]
    fn fixture_is_16k_mono_speech() {
        let bytes = include_bytes!("../tests/fixtures/hello.wav");
        let clip = Clip::from_wav_bytes(bytes).expect("the committed fixture decodes");
        assert_eq!(clip.sample_rate, SAMPLE_RATE);
        assert!(
            clip.duration() >= Duration::from_secs(3),
            "fixture is only {:?} long",
            clip.duration()
        );
        let energy: f32 = clip
            .samples
            .iter()
            .map(|&s| (s as f32 / i16::MAX as f32).powi(2))
            .sum();
        assert!(energy > 1.0, "fixture is silent");
    }

    #[test]
    fn from_wav_bytes_downmixes_stereo_to_mono() {
        let spec = WavSpec {
            channels: 2,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = WavWriter::new(&mut cursor, spec).unwrap();
            for &sample in &[1000i16, 0, -1000, 0, 3000, 1000] {
                writer.write_sample(sample).unwrap();
            }
            writer.finalize().unwrap();
        }

        let clip = Clip::from_wav_bytes(&cursor.into_inner()).expect("stereo decode");
        assert_eq!(clip.samples, vec![500, -500, 2000]);
    }
}
