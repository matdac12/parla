//! Single sink for dictation results. PARLA-5 adds history persistence here.
use serde::{Deserialize, Serialize};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager};

/// Wall clock is Unix milliseconds. Stage times are monotonic milliseconds
/// relative to key-up (always zero); absent stages were never completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictationRecord {
    pub id: String,
    /// Row origin in `history.jsonl`; PARLA-10 reuses the file for `benchmark`
    /// rows and command mode for `command` rows. `#[serde(default)]` keeps older rows loadable.
    #[serde(default = "default_kind")]
    pub kind: String,
    pub started_at: u64,
    pub provider_id: String,
    pub key_up: u64,
    pub audio_ready: Option<u64>,
    pub provider_returned: Option<u64>,
    pub pasted: Option<u64>,
    pub audio_ms: u64,
    pub text: String,
    pub error: Option<String>,
    /// Groups the per-provider rows of one benchmark run (PARLA-10). Absent on
    /// dictation rows and on rows written before the benchmark existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_error: Option<String>,
    /// Model of the live session that fell back to batch (billed as well).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_model: Option<String>,
    /// Monotonic offsets from streaming session start (not key-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_setup_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_caption_ms: Option<u64>,
    /// Command runs only (PARLA-63). `text` is the instruction and `model` its
    /// speech model; these describe the LLM step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<CommandOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// Key-up to the LLM reply, like the other stage offsets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_returned: Option<u64>,
    /// The LLM's text reply, kept for Copy (PARLA-66) even if the paste failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    /// The selection's length in characters, 0 for none (PARLA-64); its
    /// text is never kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_chars: Option<u64>,
    /// The focused app's executable name at the confirmed hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// The allowlisted tool an `opened` run called (PARLA-65).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The page an `opened` run opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Key-up to the page handed to the browser, like `pasted` for a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<u64>,
}

/// How a command run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandOutcome {
    Pasted,
    /// A tool opened a page instead of a paste.
    Opened,
    Failed,
    Cancelled,
}
impl DictationRecord {
    pub fn new(started_at: SystemTime, provider_id: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let wall = started_at.duration_since(UNIX_EPOCH).unwrap_or_default();
        Self {
            id: format!(
                "{}-{}",
                wall.as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            kind: default_kind(),
            started_at: wall.as_millis() as u64,
            provider_id: provider_id.into(),
            key_up: 0,
            audio_ready: None,
            provider_returned: None,
            pasted: None,
            audio_ms: 0,
            text: String::new(),
            error: None,
            bench_run_id: None,
            transcription_mode: None,
            model: None,
            stream_error: None,
            stream_model: None,
            stream_setup_ms: None,
            first_caption_ms: None,
            outcome: None,
            llm_model: None,
            tokens_in: None,
            tokens_out: None,
            llm_returned: None,
            reply: None,
            selection_chars: None,
            app: None,
            tool: None,
            url: None,
            opened: None,
        }
    }
}

pub fn elapsed_ms(key_up: Instant) -> u64 {
    key_up.elapsed().as_millis() as u64
}

/// The stage offset of `at`, like `elapsed_ms` for an instant already taken.
pub fn offset_ms(key_up: Instant, at: Instant) -> u64 {
    at.saturating_duration_since(key_up).as_millis() as u64
}

fn default_kind() -> String {
    "dictation".into()
}

/// `kind` of a command run's row (PARLA-61). Never counted as a dictation.
pub const COMMAND_KIND: &str = "command";

/// Per-stage latencies derived from the monotonic offsets (which are relative
/// to key-up). A stage is `None` when either endpoint is missing, so partial
/// or failed dictations do not report a bogus zero. Shared by the history view
/// and available to the benchmark view (PARLA-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StageMs {
    /// Key-up to audio ready.
    pub audio_ready: Option<u64>,
    /// Audio ready to provider returned.
    pub transcribe: Option<u64>,
    /// Provider returned to pasted.
    pub paste: Option<u64>,
    /// Key-up to pasted.
    pub total: Option<u64>,
}

impl DictationRecord {
    pub fn stage_ms(&self) -> StageMs {
        StageMs {
            audio_ready: self.audio_ready,
            transcribe: self
                .provider_returned
                .zip(self.audio_ready)
                .map(|(returned, ready)| returned.saturating_sub(ready)),
            paste: self
                .pasted
                .zip(self.provider_returned)
                .map(|(pasted, returned)| pasted.saturating_sub(returned)),
            total: self.pasted,
        }
    }
}

fn log_summary(record: &DictationRecord) -> String {
    crate::provider::redact_secrets(&format!(
        "dictation id={} provider={} audio_ms={} audio_ready={:?} provider_returned={:?} pasted={:?} text_chars={} failed={}",
        record.id, record.provider_id, record.audio_ms, record.audio_ready,
        record.provider_returned, record.pasted, record.text.chars().count(), record.error.is_some(),
    ))
}

pub fn record(app: &tauri::AppHandle, mut record: DictationRecord) {
    record.error = record
        .error
        .map(|error| crate::provider::redact_secrets(&error));
    eprintln!("{}", log_summary(&record));
    // Persist off the hot path: the history thread owns the disk write.
    if let Some(history) = app.try_state::<crate::history::History>() {
        history.append(record.clone());
    }
    crate::stats::record(app, &record);
    if let Err(error) = app.emit("dictation://recorded", &record) {
        eprintln!("Unable to emit dictation record: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_summary_contains_lengths_instead_of_transcripts_or_errors() {
        let mut record = DictationRecord::new(SystemTime::now(), "sk-providerSecret");
        record.text = "private speech xai-transcriptSecret".into();
        record.error = Some("private body sk-maskedKey***".into());
        let summary = log_summary(&record);
        assert!(summary.contains(&format!("text_chars={}", record.text.chars().count())));
        assert!(summary.contains("failed=true"));
        for private in ["private", "speech", "body", "sk-", "xai-"] {
            assert!(!summary.contains(private));
        }
    }
    #[test]
    fn wall_time_ids_and_relative_times_are_serializable() {
        let start = UNIX_EPOCH + std::time::Duration::from_millis(1234);
        let first = DictationRecord::new(start, "openai");
        let second = DictationRecord::new(start, "openai");
        assert_ne!(first.id, second.id);
        assert_eq!(first.started_at, 1234);
        assert_eq!(first.key_up, 0);
        assert!(first.pasted.is_none());
        assert!(elapsed_ms(Instant::now() - std::time::Duration::from_millis(20)) >= 20);
        let decoded: DictationRecord =
            serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        assert_eq!(decoded.id, first.id);
    }
    #[test]
    fn stage_latencies_handle_missing_endpoints() {
        let mut record = DictationRecord::new(SystemTime::now(), "fake");
        assert_eq!(
            record.stage_ms(),
            StageMs {
                audio_ready: None,
                transcribe: None,
                paste: None,
                total: None,
            }
        );
        record.provider_returned = Some(25);
        assert_eq!(record.stage_ms().transcribe, None);
        record.audio_ready = Some(10);
        assert_eq!(record.stage_ms().transcribe, Some(15));
        assert_eq!(record.stage_ms().paste, None);
        record.pasted = Some(40);
        let stages = record.stage_ms();
        assert_eq!(stages.audio_ready, Some(10));
        assert_eq!(stages.paste, Some(15));
        assert_eq!(stages.total, Some(40));
    }
    #[test]
    fn absent_kind_defaults_to_dictation_for_older_rows() {
        let decoded: DictationRecord = serde_json::from_str(
            r#"{"id":"x","started_at":1,"provider_id":"openai","key_up":0,"audio_ready":null,"provider_returned":null,"pasted":null,"audio_ms":0,"text":"","error":null}"#,
        )
        .unwrap();
        assert_eq!(decoded.kind, "dictation");
    }
}
