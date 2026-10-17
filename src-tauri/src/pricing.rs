//! Built-in list prices for the usage dashboard (PARLA-19). Estimates only:
//! speech providers bill by audio duration, Parla multiplies the audio it sent;
//! command mode's LLM bills by token (PARLA-66). Speech prices checked
//! 2026-10-15 against the providers' model and pricing pages.

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

/// Command mode's LLM: US dollars per million tokens (in, out), or `None` when
/// the model is not in the table. Checked 2026-10-17 on OpenAI's pricing page.
fn usd_per_million_tokens(model: &str) -> Option<(f64, f64)> {
    match model {
        "gpt-6-luna" => Some((0.10, 0.50)),
        _ => None,
    }
}

/// A command run's LLM cost, or `None` when its model has no known price.
pub fn llm_cost_usd(model: &str, tokens_in: u64, tokens_out: u64) -> Option<f64> {
    usd_per_million_tokens(model).map(|(input, output)| {
        (input * tokens_in as f64 + output * tokens_out as f64) / 1_000_000.0
    })
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

    #[test]
    fn command_tokens_are_priced_per_million_in_and_out() {
        // gpt-6-luna list price, 2026-10-17: $0.10 in, $0.50 out per million.
        let cost = llm_cost_usd("gpt-6-luna", 2_000_000, 1_000_000).unwrap();
        assert!((cost - 0.70).abs() < 1e-9);
        let run = llm_cost_usd("gpt-6-luna", 92, 18).unwrap();
        assert!((run - 0.0000182).abs() < 1e-12);
        assert_eq!(llm_cost_usd("gpt-7-future", 1_000, 1_000), None);
    }
}
