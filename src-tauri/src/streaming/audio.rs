//! A bounded, nonblocking audio tee and stateful PCM conversion.

use crate::{
    provider::ProviderError,
    recorder::{f32_to_i16, MonoResampler},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::mpsc;

pub struct AudioChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

/// Only the recorder worker owns this writer. No network backpressure can block it.
pub struct AudioFeed {
    sender: mpsc::Sender<AudioChunk>,
    failed: Arc<AtomicBool>,
    pending: Vec<f32>,
    rate: u32,
}

pub fn channel() -> (AudioFeed, mpsc::Receiver<AudioChunk>, Arc<AtomicBool>) {
    // 3.2 seconds of 100 ms packets, sufficient for ordinary connection setup.
    // A slower connection falls back to the complete saved clip.
    let (sender, receiver) = mpsc::channel(32);
    let failed = Arc::new(AtomicBool::new(false));
    (
        AudioFeed {
            sender,
            failed: failed.clone(),
            pending: Vec::new(),
            rate: 0,
        },
        receiver,
        failed,
    )
}

impl AudioFeed {
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    pub fn push(&mut self, mut input: &[f32], rate: u32) {
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        if rate == 0 || (self.rate != 0 && self.rate != rate) {
            self.failed.store(true, Ordering::Release);
            self.pending.clear();
            return;
        }
        self.rate = rate;
        let size = (rate as usize / 10).max(1);
        while !input.is_empty() {
            let count = (size - self.pending.len()).min(input.len());
            self.pending.extend_from_slice(&input[..count]);
            input = &input[count..];
            if self.pending.len() == size && !self.flush() {
                return;
            }
        }
    }

    fn flush(&mut self) -> bool {
        if self.pending.is_empty() {
            return true;
        }
        let chunk = AudioChunk {
            samples: std::mem::take(&mut self.pending),
            sample_rate: self.rate,
        };
        if self.sender.try_send(chunk).is_err() {
            self.failed.store(true, Ordering::Release);
            return false;
        }
        true
    }
}

impl Drop for AudioFeed {
    fn drop(&mut self) {
        if !self.failed.load(Ordering::Acquire) {
            self.flush();
        }
    }
}

pub struct PcmEncoder {
    target_rate: u32,
    input_rate: Option<u32>,
    resampler: Option<MonoResampler>,
}

impl PcmEncoder {
    pub fn new(target_rate: u32) -> Self {
        Self {
            target_rate,
            input_rate: None,
            resampler: None,
        }
    }

    pub fn push(&mut self, chunk: AudioChunk) -> Result<Vec<u8>, ProviderError> {
        if chunk.sample_rate == 0
            || self
                .input_rate
                .is_some_and(|rate| rate != chunk.sample_rate)
        {
            return Err(ProviderError::Other("Streaming audio rate changed".into()));
        }
        if self.input_rate.is_none() {
            self.input_rate = Some(chunk.sample_rate);
            if chunk.sample_rate != self.target_rate {
                self.resampler = Some(
                    MonoResampler::new(chunk.sample_rate as usize, self.target_rate as usize)
                        .map_err(|_| {
                            ProviderError::Other("Streaming resampler could not start".into())
                        })?,
                );
            }
        }
        let samples = if let Some(resampler) = &mut self.resampler {
            let mut out = Vec::new();
            resampler
                .push(&chunk.samples, &mut out)
                .map_err(|_| ProviderError::Other("Streaming resampling failed".into()))?;
            out
        } else {
            chunk.samples
        };
        Ok(pcm(&samples))
    }

    pub fn finish(&mut self) -> Result<Vec<u8>, ProviderError> {
        let mut out = Vec::new();
        if let Some(resampler) = &mut self.resampler {
            resampler
                .finish(&mut out)
                .map_err(|_| ProviderError::Other("Streaming audio flush failed".into()))?;
        }
        Ok(pcm(&out))
    }
}

fn pcm(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&sample| f32_to_i16(sample).to_le_bytes())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tee_preserves_order_and_flushes_the_last_partial_packet() {
        let (mut feed, mut receiver, failed) = channel();
        feed.push(&vec![0.25; 1600], 16000);
        feed.push(&vec![-0.5; 201], 16000);
        drop(feed);
        assert_eq!(receiver.try_recv().unwrap().samples, vec![0.25; 1600]);
        assert_eq!(receiver.try_recv().unwrap().samples, vec![-0.5; 201]);
        assert!(receiver.try_recv().is_err());
        assert!(!failed.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn overflow_marks_the_stream_unusable_without_blocking_capture() {
        let (mut feed, _receiver, failed) = channel();
        feed.push(&vec![0.5; 1600 * 40], 16000);
        assert!(failed.load(std::sync::atomic::Ordering::Acquire));
        assert!(feed.pending.is_empty());
    }

    #[test]
    fn incremental_pcm_matches_full_resampling_including_the_tail() {
        let samples: Vec<f32> = (0..48123).map(|n| (n as f32 / 17.0).sin() * 0.5).collect();
        let mut encoder = PcmEncoder::new(24000);
        let mut actual = Vec::new();
        for piece in samples.chunks(481) {
            actual.extend(
                encoder
                    .push(AudioChunk {
                        samples: piece.to_vec(),
                        sample_rate: 48000,
                    })
                    .unwrap(),
            );
        }
        actual.extend(encoder.finish().unwrap());
        let mut full = PcmEncoder::new(24000);
        let mut expected = full
            .push(AudioChunk {
                samples,
                sample_rate: 48000,
            })
            .unwrap();
        expected.extend(full.finish().unwrap());
        assert_eq!(actual.len(), 48124);
        assert_eq!(actual, expected);
    }
}
