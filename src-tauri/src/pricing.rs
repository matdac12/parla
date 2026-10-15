//! Built-in list prices for the usage dashboard (PARLA-19). Estimates only:
//! providers bill by audio duration, Parla multiplies the audio it sent.
//! Checked 2026-10-15 against the providers' model and pricing pages.

pub const BATCH: &str = "batch";
pub const STREAMING: &str = "streaming";

/// US dollars per hour of audio, or `None` when the model is not in the table.
pub fn usd_per_hour(provider_id: &str, model: &str, mode: &str) -> Option<f64> {
    let per_minute = |dollars: f64| dollars * 60.0;
    match (provider_id, model, mode) {
        ("parakeet", _, _) => Some(0.0),
        // xAI lists one Speech to Text price for every grok voice model.
        ("xai", model, BATCH) if model.starts_with("grok-") => Some(0.10),
        ("xai", model, STREAMING) if model.starts_with("grok-") => Some(0.20),
        // Billed by audio duration in batch and realtime transcription sessions.
        ("openai", "gpt-transcribe", BATCH | STREAMING) => Some(per_minute(0.0045)),
        ("openai", "gpt-live-transcribe", STREAMING) => Some(per_minute(0.017)),
        ("openai", "gpt-4o-transcribe" | "whisper-1", BATCH) => Some(per_minute(0.006)),
        ("openai", "gpt-4o-mini-transcribe", BATCH) => Some(per_minute(0.003)),
        _ => None,
    }
}

pub fn cost_usd(provider_id: &str, model: &str, mode: &str, audio_ms: u64) -> Option<f64> {
    usd_per_hour(provider_id, model, mode).map(|rate| rate * audio_ms as f64 / 3_600_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_are_priced_per_hour() {
        assert_eq!(
            usd_per_hour("xai", "grok-voice-transcribe-2.0", BATCH),
            Some(0.10)
        );
        assert_eq!(
            usd_per_hour("xai", "grok-voice-transcribe-2.0", STREAMING),
            Some(0.20)
        );
        assert!((usd_per_hour("openai", "gpt-transcribe", BATCH).unwrap() - 0.27).abs() < 1e-9);
        assert!(
            (usd_per_hour("openai", "gpt-live-transcribe", STREAMING).unwrap() - 1.02).abs() < 1e-9
        );
        assert_eq!(
            usd_per_hour("parakeet", "parakeet-tdt-0.6b-v3", BATCH),
            Some(0.0)
        );
    }

    #[test]
    fn unknown_models_are_unpriced_rather_than_guessed() {
        assert_eq!(usd_per_hour("openai", "gpt-live-transcribe", BATCH), None);
        assert_eq!(usd_per_hour("openai", "some-future-model", BATCH), None);
        assert_eq!(usd_per_hour("groq", "whisper-large-v3", BATCH), None);
    }

    #[test]
    fn cost_scales_with_audio_duration() {
        let half_hour = 30 * 60 * 1000;
        let cost = cost_usd("xai", "grok-voice-transcribe-2.0", STREAMING, half_hour).unwrap();
        assert!((cost - 0.10).abs() < 1e-9);
        assert_eq!(cost_usd("openai", "unknown", BATCH, half_hour), None);
    }
}
