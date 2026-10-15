//! Energy-based phrase splitting for local transcription (16 kHz mono f32).
//!
//! Local inference cost grows faster than clip length, so a recording is cut
//! into phrases at natural pauses and transcribed while the user is still
//! speaking; key-up then only pays for the last phrase. Long silences are
//! shortened on the way, which is free speed for the batch path too.
//!
//! Cuts happen only at clear pauses (about half a second) after enough speech
//! to give the model context, because cutting mid-thought costs accuracy at the
//! joins. Quiet frames are judged against an adaptive noise floor, so a fan or
//! room hum raises the bar instead of reading as speech.
use std::collections::VecDeque;

/// 30 ms at 16 kHz.
pub const FRAME: usize = 480;
/// Quiet frames that end a phrase: 480 ms. Mid-word stops are far shorter.
const PAUSE_FRAMES: usize = 16;
/// Quiet frames kept on each side of a cut (240 ms), so word tails and onsets
/// stay intact and every phrase starts and ends in silence.
const KEEP_FRAMES: usize = 8;
/// Speech a phrase needs before it may be cut (1.5 s): short fragments are
/// carried into the next phrase for context.
const MIN_SPEECH_FRAMES: usize = 50;
/// A phrase without any pause is cut at its quietest frame after 20 s.
const MAX_FRAMES: usize = 666;
/// How far back a forced cut looks for its quietest frame: 2 s.
const SEARCH_FRAMES: usize = 66;
/// Below this RMS (about -50 dBFS) a frame is always quiet.
const ABSOLUTE_FLOOR: f32 = 0.003;
/// A frame is speech when its RMS exceeds the noise floor by this factor.
const SPEECH_RATIO: f32 = 3.0;
/// Caps the noise estimate, so a noisy start cannot hide normal speech.
const NOISE_CEILING: f32 = 0.01;
/// Per-frame rise of the noise estimate (about +7 % per second); it drops
/// immediately to any quieter frame.
const NOISE_RISE: f32 = 1.002;

fn rms(frame: &[f32]) -> f32 {
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt()
}

/// Splits a stream of samples into phrases. Feed it any chunk size.
pub struct PhraseSplitter {
    pending: Vec<f32>,
    phrase: Vec<f32>,
    speech_frames: usize,
    quiet_run: usize,
    /// Silence before the next phrase, kept so its onset is not clipped.
    lead: VecDeque<f32>,
    noise: Option<f32>,
}

impl Default for PhraseSplitter {
    fn default() -> Self {
        Self::new()
    }
}

impl PhraseSplitter {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            phrase: Vec::new(),
            speech_frames: 0,
            quiet_run: 0,
            lead: VecDeque::with_capacity(KEEP_FRAMES * FRAME),
            noise: None,
        }
    }

    /// Returns every phrase completed by `samples`, oldest first.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        let mut done = Vec::new();
        self.pending.extend_from_slice(samples);
        let whole = self.pending.len() / FRAME * FRAME;
        let frames: Vec<f32> = self.pending.drain(..whole).collect();
        for frame in frames.chunks(FRAME) {
            if let Some(phrase) = self.frame(frame) {
                done.push(phrase);
            }
        }
        done
    }

    /// The last phrase, or `None` when nothing but silence remains.
    pub fn finish(mut self) -> Option<Vec<f32>> {
        if self.speech_frames == 0 {
            return None;
        }
        // A stop during a pause leaves trailing silence; keep only the margin.
        // The phrase holds at most PAUSE_FRAMES of it (the rest was dropped).
        let excess = self.quiet_run.min(PAUSE_FRAMES).saturating_sub(KEEP_FRAMES) * FRAME;
        if excess > 0 {
            let end = self.phrase.len() - excess;
            self.phrase.truncate(end);
        } else {
            self.phrase.extend_from_slice(&self.pending);
        }
        Some(self.phrase)
    }

    fn is_speech(&mut self, level: f32) -> bool {
        let noise = match self.noise {
            Some(noise) if level >= noise => (noise * NOISE_RISE).min(NOISE_CEILING),
            _ => level.min(NOISE_CEILING),
        };
        self.noise = Some(noise);
        level > ABSOLUTE_FLOOR.max(noise * SPEECH_RATIO)
    }

    fn frame(&mut self, frame: &[f32]) -> Option<Vec<f32>> {
        let speech = self.is_speech(rms(frame));
        if self.speech_frames == 0 && !speech {
            // Before any speech: hold only the margin that precedes an onset.
            for &sample in frame {
                if self.lead.len() == KEEP_FRAMES * FRAME {
                    self.lead.pop_front();
                }
                self.lead.push_back(sample);
            }
            self.phrase.clear();
            return None;
        }
        if self.speech_frames == 0 {
            self.phrase.clear();
            self.phrase.extend(self.lead.drain(..));
        }
        if speech {
            self.speech_frames += 1;
            self.quiet_run = 0;
        } else {
            self.quiet_run += 1;
            if self.quiet_run > PAUSE_FRAMES {
                // A long pause inside a short phrase: drop the excess silence.
                return None;
            }
        }
        self.phrase.extend_from_slice(frame);
        if self.quiet_run == PAUSE_FRAMES && self.speech_frames >= MIN_SPEECH_FRAMES {
            return Some(self.cut_at_pause());
        }
        if self.phrase.len() >= MAX_FRAMES * FRAME {
            return Some(self.cut_at_quietest());
        }
        None
    }

    /// Ends the phrase inside the pause: it keeps the first KEEP_FRAMES of
    /// silence, the next phrase starts with the last KEEP_FRAMES.
    fn cut_at_pause(&mut self) -> Vec<f32> {
        let end = self.phrase.len() - (PAUSE_FRAMES - KEEP_FRAMES) * FRAME;
        let lead_start = self.phrase.len() - KEEP_FRAMES * FRAME;
        self.lead.clear();
        self.lead.extend(self.phrase[lead_start..].iter().copied());
        self.phrase.truncate(end);
        self.speech_frames = 0;
        self.quiet_run = 0;
        std::mem::take(&mut self.phrase)
    }

    fn cut_at_quietest(&mut self) -> Vec<f32> {
        let frames = self.phrase.len() / FRAME;
        let first = frames.saturating_sub(SEARCH_FRAMES);
        let quietest = (first..frames)
            .min_by(|&a, &b| {
                let level = |i: usize| rms(&self.phrase[i * FRAME..(i + 1) * FRAME]);
                level(a).total_cmp(&level(b))
            })
            .unwrap_or(frames - 1);
        let rest = self.phrase.split_off((quietest + 1) * FRAME);
        let phrase = std::mem::replace(&mut self.phrase, rest);
        // Treat the remainder as speech: it is the middle of a monologue.
        self.speech_frames = self.phrase.len() / FRAME;
        self.quiet_run = 0;
        phrase
    }
}

/// Splits a complete recording into phrases. Empty when no speech is detected.
pub fn phrases(samples: &[f32]) -> Vec<Vec<f32>> {
    let mut splitter = PhraseSplitter::new();
    let mut phrases = splitter.push(samples);
    phrases.extend(splitter.finish());
    phrases
}

/// The recording with leading, trailing and long inner silences shortened, or
/// `None` when no speech is detected (the caller should keep the original).
pub fn compress_silence(samples: &[f32]) -> Option<Vec<f32>> {
    let phrases = phrases(samples);
    (!phrases.is_empty()).then(|| phrases.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: usize = 16_000;

    fn tone(seconds: f32) -> Vec<f32> {
        (0..(seconds * SECOND as f32) as usize)
            .map(|n| (n as f32 * 0.07).sin() * 0.2)
            .collect()
    }
    fn hush(seconds: f32, level: f32) -> Vec<f32> {
        (0..(seconds * SECOND as f32) as usize)
            .map(|n| if n % 2 == 0 { level } else { -level })
            .collect()
    }
    fn seconds(samples: &[f32]) -> f32 {
        samples.len() as f32 / SECOND as f32
    }

    #[test]
    fn splits_at_a_clear_pause_and_keeps_margins() {
        let audio = [
            hush(1.0, 0.0005),
            tone(2.0),
            hush(0.8, 0.0005),
            tone(2.0),
            hush(0.5, 0.0005),
        ]
        .concat();
        let phrases = phrases(&audio);
        assert_eq!(phrases.len(), 2);
        // 240 ms lead + 2 s speech + 240 ms tail, give or take a frame.
        for phrase in &phrases {
            assert!((seconds(phrase) - 2.48).abs() < 0.07, "{}", seconds(phrase));
        }
    }

    #[test]
    fn short_pauses_and_short_phrases_do_not_split() {
        // A 300 ms pause is a breath, not a phrase end.
        let audio = [tone(2.0), hush(0.3, 0.0005), tone(2.0)].concat();
        assert_eq!(phrases(&audio).len(), 1);
        // One second of speech is too little context to stand alone; the long
        // pause after it is shortened instead.
        let audio = [tone(1.0), hush(3.0, 0.0005), tone(2.0)].concat();
        let phrases = phrases(&audio);
        assert_eq!(phrases.len(), 1);
        assert!(seconds(&phrases[0]) < 3.6, "{}", seconds(&phrases[0]));
    }

    #[test]
    fn streaming_in_small_chunks_matches_one_shot() {
        let audio = [
            hush(0.5, 0.001),
            tone(2.0),
            hush(1.0, 0.001),
            tone(1.7),
            hush(0.2, 0.001),
        ]
        .concat();
        let mut splitter = PhraseSplitter::new();
        let mut streamed = Vec::new();
        for chunk in audio.chunks(1_237) {
            streamed.extend(splitter.push(chunk));
        }
        streamed.extend(splitter.finish());
        assert_eq!(streamed, phrases(&audio));
    }

    #[test]
    fn silence_only_yields_nothing_and_compress_keeps_speech() {
        assert!(phrases(&hush(3.0, 0.0005)).is_empty());
        assert!(compress_silence(&hush(3.0, 0.0005)).is_none());
        let audio = [hush(4.0, 0.0005), tone(2.0), hush(4.0, 0.0005)].concat();
        let compressed = compress_silence(&audio).unwrap();
        assert!((seconds(&compressed) - 2.48).abs() < 0.07);
    }

    #[test]
    fn steady_background_noise_raises_the_floor() {
        // Hum at -40 dBFS would read as speech against a fixed threshold.
        let hum = 0.01;
        let speech: Vec<f32> = tone(2.0)
            .iter()
            .zip(hush(2.0, hum))
            .map(|(a, b)| a + b)
            .collect();
        let audio = [hush(1.0, hum), speech.clone(), hush(1.0, hum), speech].concat();
        assert_eq!(phrases(&audio).len(), 2);
    }

    #[test]
    fn monologue_without_pauses_is_cut_at_the_quietest_frame() {
        let mut audio = Vec::new();
        for _ in 0..12 {
            audio.extend(tone(2.0));
            audio.extend(hush(0.09, 0.02)); // a dip, not a pause
        }
        let phrases = phrases(&audio);
        assert!(phrases.len() >= 2);
        assert!(phrases.iter().all(|p| seconds(p) <= 20.01));
        assert_eq!(
            phrases.iter().map(Vec::len).sum::<usize>(),
            audio.len() / FRAME * FRAME
        );
    }
}
