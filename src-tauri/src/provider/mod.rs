//! Small synchronous provider interface; callers run it on a blocking worker.
pub mod openai;
pub mod parakeet;
pub mod registry;
pub mod xai;

use crate::{clip::Clip, settings::Settings};
use serde::Serialize;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub trait SttProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn availability(&self, settings: &Settings) -> Availability;
    fn transcribe(&self, clip: &Clip, settings: &Settings) -> Result<String, ProviderError>;
}

/// How soon Esc frees the pill while a blocking request runs.
const CANCEL_POLL: Duration = Duration::from_millis(20);

/// Waits up to `limit` for the answer of work running on its own thread until
/// `cancelled` is set, so Esc returns at once and a late answer lands in a
/// dropped channel. None when cancelled, out of time or the thread died. An
/// answer wakes the wait at once: the poll only bounds how long Esc takes.
pub fn unless_cancelled<T>(
    answer: &mpsc::Receiver<T>,
    cancelled: &AtomicBool,
    limit: Duration,
) -> Option<T> {
    let deadline = Instant::now().checked_add(limit);
    while !cancelled.load(Ordering::Acquire) {
        let left = deadline.map_or(CANCEL_POLL, |at| {
            at.saturating_duration_since(Instant::now())
        });
        if left.is_zero() {
            break;
        }
        match answer.recv_timeout(CANCEL_POLL.min(left)) {
            Ok(value) => return Some(value),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Availability {
    Available,
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    MissingConfig(String),
    Auth(String),
    Network(String),
    Timeout,
    Http { status: u16, body: String },
    Other(String),
}

/// Redact complete or partially masked provider-key tokens from diagnostics.
/// Transcript text itself stays untouched for insertion and the history event.
pub fn redact_secrets(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    let mut index = 0;
    while index < message.len() {
        let rest = &message[index..];
        let prefix = if rest.starts_with("sk-") {
            3
        } else if rest.starts_with("xai-") {
            4
        } else {
            0
        };
        let end = index + prefix;
        if prefix > 0 {
            let token_len = message.as_bytes()[end..]
                .iter()
                .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_' || **byte == b'-')
                .count();
            if token_len > 0 {
                output.push_str("[redacted]");
                index = end + token_len;
                continue;
            }
        }
        let character = rest.chars().next().expect("nonempty string");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::MissingConfig(reason) => format!("Missing configuration: {reason}"),
            Self::Auth(_) => "Authentication failed".into(),
            Self::Network(reason) => format!("Network error: {reason}"),
            Self::Timeout => "Transcription timed out".into(),
            Self::Http { status, .. } => format!("HTTP {status}"),
            Self::Other(reason) => reason.clone(),
        };
        f.write_str(&redact_secrets(&message))
    }
}
impl std::error::Error for ProviderError {}

#[cfg(test)]
mod privacy_tests {
    use super::*;
    #[test]
    fn a_wait_ends_on_the_answer_on_esc_or_at_its_limit() {
        let (sender, answer) = mpsc::channel();
        let idle = AtomicBool::new(false);
        sender.send(7).unwrap();
        assert_eq!(unless_cancelled(&answer, &idle, Duration::MAX), Some(7));
        let started = Instant::now();
        assert_eq!(
            unless_cancelled(&answer, &idle, Duration::from_millis(30)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        // Esc wins over an answer already waiting.
        sender.send(8).unwrap();
        assert_eq!(
            unless_cancelled(&answer, &AtomicBool::new(true), Duration::MAX),
            None
        );
        drop(sender);
        assert_eq!(unless_cancelled(&answer, &idle, Duration::MAX), Some(8));
        assert_eq!(unless_cancelled(&answer, &idle, Duration::MAX), None);
    }
    #[test]
    fn logs_never_include_provider_bodies_or_partial_keys() {
        assert_eq!(
            redact_secrets("✓ sk-abc_DEF-123, xai-secret_42."),
            "✓ [redacted], [redacted]."
        );
        assert_eq!(redact_secrets("sk- xai- fine"), "sk- xai- fine");
        let error = ProviderError::Http {
            status: 429,
            body: "private body sk-masked123***".into(),
        };
        assert_eq!(error.to_string(), "HTTP 429");
        assert_eq!(
            ProviderError::Auth("private body".into()).to_string(),
            "Authentication failed"
        );
    }
}
