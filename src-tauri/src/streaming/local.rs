//! Live dictation with the local model. Phrases are cut at natural pauses and
//! transcribed while the user is still speaking, so key-up pays only for the
//! last one (measured: a 2-3 s tail costs 120-260 ms on an i7-1370P, while a
//! whole 20 s clip costs 2.5 s). Any failure leaves the saved recording as the
//! batch fallback, like the cloud sessions.
use super::{audio::AudioChunk, Caption, Streamed};
use crate::{
    clip::SAMPLE_RATE,
    provider::{parakeet, ProviderError},
    recorder::MonoResampler,
    segment::PhraseSplitter,
    settings::Settings,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Instant,
};

/// Everything the two worker threads share.
pub struct Local {
    pub settings: Settings,
    pub model: String,
    /// Set by the audio feed on overflow or a rate change, or by the splitter.
    pub failed: Arc<AtomicBool>,
    /// Esc, or the session being dropped.
    pub stopped: Arc<AtomicBool>,
    pub cancelled: Arc<AtomicBool>,
}

impl Local {
    fn halted(&self) -> bool {
        self.stopped.load(Ordering::Acquire) || self.cancelled.load(Ordering::Acquire)
    }
}

pub fn spawn(
    local: Local,
    audio: tokio::sync::mpsc::Receiver<AudioChunk>,
    mut caption: impl FnMut(Caption) + Send + 'static,
) -> mpsc::Receiver<Result<Streamed, ProviderError>> {
    let local = Arc::new(local);
    let (phrases, queued) = mpsc::channel();
    let (send, result) = mpsc::channel();
    let splitter = local.clone();
    let spawned = std::thread::Builder::new()
        .name("parla-live-split".into())
        .spawn(move || split(&splitter, audio, phrases));
    if spawned.is_err() {
        let _ = send.send(Err(ProviderError::Other(
            "Cannot start local live dictation".into(),
        )));
        return result;
    }
    let _ = std::thread::Builder::new()
        .name("parla-live-transcribe".into())
        .spawn(move || {
            let outcome = transcribe(&local, queued, &mut caption);
            if outcome.is_err() {
                caption(Caption::default());
            }
            let _ = send.send(outcome);
        });
    result
}

/// Resamples the device audio to 16 kHz and queues each completed phrase.
/// Returns when the recorder closes the feed (key-up) or the session halts.
fn split(
    local: &Local,
    mut audio: tokio::sync::mpsc::Receiver<AudioChunk>,
    phrases: mpsc::Sender<Vec<f32>>,
) {
    let mut splitter = PhraseSplitter::new();
    let mut resampler: Option<MonoResampler> = None;
    let mut rate = 0;
    let mut samples = Vec::new();
    let fail = || local.failed.store(true, Ordering::Release);
    while let Some(chunk) = audio.blocking_recv() {
        if local.halted() {
            return;
        }
        if rate == 0 {
            rate = chunk.sample_rate;
            if rate != SAMPLE_RATE {
                match MonoResampler::new(rate as usize, SAMPLE_RATE as usize) {
                    Ok(converter) => resampler = Some(converter),
                    Err(_) => return fail(),
                }
            }
        }
        if chunk.sample_rate != rate {
            return fail();
        }
        samples.clear();
        match &mut resampler {
            Some(converter) => {
                if converter.push(&chunk.samples, &mut samples).is_err() {
                    return fail();
                }
            }
            None => samples.extend_from_slice(&chunk.samples),
        }
        for phrase in splitter.push(&samples) {
            if phrases.send(phrase).is_err() {
                return;
            }
        }
    }
    samples.clear();
    if let Some(converter) = &mut resampler {
        if converter.finish(&mut samples).is_err() {
            return fail();
        }
    }
    for phrase in splitter.push(&samples).into_iter().chain(splitter.finish()) {
        if phrases.send(phrase).is_err() {
            return;
        }
    }
}

/// Wakes the model (the key-down warm-up), then transcribes phrases in order
/// until the splitter has queued the last one.
fn transcribe(
    local: &Local,
    phrases: mpsc::Receiver<Vec<f32>>,
    caption: &mut impl FnMut(Caption),
) -> Result<Streamed, ProviderError> {
    let started = Instant::now();
    parakeet::wake(&local.settings)?;
    let mut transcript = Transcript::default();
    let mut first_caption_ms = None;
    for phrase in phrases {
        if local.halted() {
            return Err(ProviderError::Other("Live dictation cancelled".into()));
        }
        if transcript.add(&local.settings, &phrase)? {
            first_caption_ms.get_or_insert(started.elapsed().as_millis() as u64);
            caption(Caption::new(transcript.text.clone(), String::new()));
        }
    }
    if local.failed.load(Ordering::Acquire) {
        return Err(ProviderError::Other("Live audio feed failed".into()));
    }
    Ok(Streamed {
        text: transcript.text,
        model: local.model.clone(),
        setup_ms: None,
        first_caption_ms,
    })
}

/// Audio of earlier phrases handed to the model as left context: 2 s.
const CONTEXT_SAMPLES: usize = 2 * SAMPLE_RATE as usize;

/// The text so far, plus the recent audio that gives the next phrase context.
#[derive(Default)]
struct Transcript {
    text: String,
    recent: Vec<f32>,
}

impl Transcript {
    /// Transcribes the next phrase. False when it held no words.
    fn add(&mut self, settings: &Settings, phrase: &[f32]) -> Result<bool, ProviderError> {
        let piece = parakeet::transcribe_after(settings, &self.recent, phrase)?;
        self.recent.extend_from_slice(phrase);
        let excess = self.recent.len().saturating_sub(CONTEXT_SAMPLES);
        self.recent.drain(..excess);
        Ok(join(&mut self.text, &piece))
    }
}

/// Appends a phrase's text with a single space. False when it was empty.
/// A phrase the model starts in lowercase continues the sentence across the
/// cut, so the full stop it guessed at the end of the previous phrase goes.
fn join(text: &mut String, piece: &str) -> bool {
    let piece = piece.trim();
    if piece.is_empty() {
        return false;
    }
    if !text.is_empty() {
        if piece.chars().next().is_some_and(char::is_lowercase) && text.ends_with('.') {
            text.pop();
        }
        text.push(' ');
    }
    text.push_str(piece);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_join_with_single_spaces_and_skip_empty_ones() {
        let mut text = String::new();
        assert!(join(&mut text, " Ciao, sono Mattia. "));
        assert!(!join(&mut text, "  "));
        assert!(join(&mut text, "Sono uno sviluppatore."));
        assert_eq!(text, "Ciao, sono Mattia. Sono uno sviluppatore.");
    }

    #[test]
    fn a_lowercase_continuation_drops_the_guessed_full_stop() {
        let mut text = "Questa è una prova per vedere i nostri providers.".to_string();
        join(&mut text, "e quanto accurati sono.");
        assert_eq!(
            text,
            "Questa è una prova per vedere i nostri providers e quanto accurati sono."
        );
        let mut text = "Is it fast?".to_string();
        join(&mut text, "yes.");
        assert_eq!(text, "Is it fast? yes.");
    }

    #[test]
    fn splitter_queues_phrases_and_the_tail_after_the_feed_closes() {
        let local = Local {
            settings: Settings::default(),
            model: "test".into(),
            failed: Arc::default(),
            stopped: Arc::default(),
            cancelled: Arc::default(),
        };
        let tone = |seconds: f32| -> Vec<f32> {
            (0..(seconds * 48_000.0) as usize)
                .map(|n| (n as f32 * 0.03).sin() * 0.2)
                .collect()
        };
        let audio = [tone(2.0), vec![0.0; 48_000], tone(2.0)].concat();
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        for piece in audio.chunks(4_800) {
            sender
                .try_send(AudioChunk {
                    samples: piece.to_vec(),
                    sample_rate: 48_000,
                })
                .unwrap();
        }
        drop(sender);
        let (phrases, queued) = mpsc::channel();
        split(&local, receiver, phrases);
        let lengths: Vec<f32> = queued.iter().map(|p| p.len() as f32 / 16_000.0).collect();
        assert_eq!(lengths.len(), 2, "{lengths:?}");
        assert!(
            lengths.iter().all(|&s| (2.0..2.6).contains(&s)),
            "{lengths:?}"
        );
        assert!(!local.failed.load(Ordering::Acquire));
    }

    /// Compares whole-clip and phrase-by-phrase transcripts of real recordings
    /// and prints what key-up would cost. Run in release for honest timings:
    /// PARLA_PARAKEET_MODEL_DIR=<model> PARLA_LIVE_CLIPS=<dir of 16 kHz wavs>
    /// cargo test --release --lib live_clips -- --nocapture
    #[test]
    fn live_clips_when_model_and_clips_are_provided() {
        let (Ok(model_dir), Ok(clips)) = (
            std::env::var("PARLA_PARAKEET_MODEL_DIR"),
            std::env::var("PARLA_LIVE_CLIPS"),
        ) else {
            return;
        };
        let mut settings = Settings::default();
        settings.providers.insert(
            "parakeet".into(),
            serde_json::json!({"model_dir":model_dir}),
        );
        let mut paths: Vec<_> = std::fs::read_dir(clips)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "wav"))
            .collect();
        paths.sort();
        for path in paths {
            let clip = crate::clip::Clip::from_wav_bytes(&std::fs::read(&path).unwrap()).unwrap();
            let audio: Vec<f32> = clip.samples.iter().map(|&s| s as f32 / 32768.0).collect();
            let timed = |samples: &[f32]| {
                std::thread::sleep(std::time::Duration::from_secs(2));
                parakeet::wake(&settings).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(500));
                let started = Instant::now();
                let text = parakeet::transcribe_samples(&settings, samples).unwrap();
                (started.elapsed().as_millis(), text.trim().to_string())
            };
            let (whole_ms, whole) = timed(&audio);
            let compressed = crate::segment::compress_silence(&audio).unwrap();
            let (compressed_ms, _) = timed(&compressed);
            let phrases = crate::segment::phrases(&audio);
            let mut transcript = Transcript::default();
            let mut tail_ms = 0;
            for phrase in &phrases {
                std::thread::sleep(std::time::Duration::from_secs(2));
                parakeet::wake(&settings).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(500));
                let started = Instant::now();
                transcript.add(&settings, phrase).unwrap();
                tail_ms = started.elapsed().as_millis();
            }
            let text = transcript.text;
            println!(
                "{}: {:.1} s audio\n  whole      {whole_ms} ms: {whole}\n  trimmed    {compressed_ms} ms ({:.1} s)\n  live       tail {tail_ms} ms over {} phrases {:?}: {text}",
                path.file_name().unwrap().to_string_lossy(),
                audio.len() as f32 / 16_000.0,
                compressed.len() as f32 / 16_000.0,
                phrases.len(),
                phrases.iter().map(|p| format!("{:.1}", p.len() as f32 / 16_000.0)).collect::<Vec<_>>(),
            );
        }
    }

    #[test]
    fn a_sample_rate_change_fails_the_session() {
        let local = Local {
            settings: Settings::default(),
            model: "test".into(),
            failed: Arc::default(),
            stopped: Arc::default(),
            cancelled: Arc::default(),
        };
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        for rate in [48_000, 44_100] {
            sender
                .try_send(AudioChunk {
                    samples: vec![0.0; 480],
                    sample_rate: rate,
                })
                .unwrap();
        }
        drop(sender);
        let (phrases, _queued) = mpsc::channel();
        split(&local, receiver, phrases);
        assert!(local.failed.load(Ordering::Acquire));
    }
}
