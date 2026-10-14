//! Single sink for dictation results. PARLA-5 adds history persistence here.
use serde::{Deserialize, Serialize};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tauri::Emitter;

/// Wall clock is Unix milliseconds. Stage times are monotonic milliseconds
/// relative to key-up (always zero); absent stages were never completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictationRecord {
    pub id: String,
    pub started_at: u64,
    pub provider_id: String,
    pub key_up: u64,
    pub audio_ready: Option<u64>,
    pub provider_returned: Option<u64>,
    pub pasted: Option<u64>,
    pub audio_ms: u64,
    pub text: String,
    pub error: Option<String>,
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
            started_at: wall.as_millis() as u64,
            provider_id: provider_id.into(),
            key_up: 0,
            audio_ready: None,
            provider_returned: None,
            pasted: None,
            audio_ms: 0,
            text: String::new(),
            error: None,
        }
    }
}

pub fn elapsed_ms(key_up: Instant) -> u64 {
    key_up.elapsed().as_millis() as u64
}

pub fn record(app: &tauri::AppHandle, record: DictationRecord) {
    if let Ok(json) = serde_json::to_string(&record) {
        eprintln!("dictation: {json}");
    }
    if let Err(error) = app.emit("dictation://recorded", &record) {
        eprintln!("Unable to emit dictation record: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
