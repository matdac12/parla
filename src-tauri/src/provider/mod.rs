//! Small synchronous provider interface; callers run it on a blocking worker.
pub mod openai;
pub mod parakeet;
pub mod registry;

use crate::{clip::Clip, settings::Settings};
use serde::Serialize;

pub trait SttProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn availability(&self, settings: &Settings) -> Availability;
    fn transcribe(&self, clip: &Clip, settings: &Settings) -> Result<String, ProviderError>;
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
