//! Small synchronous provider interface; callers run it on a blocking worker.
pub mod openai;
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

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingConfig(reason) => write!(f, "Missing configuration: {reason}"),
            Self::Auth(reason) => write!(f, "Authentication failed: {reason}"),
            Self::Network(reason) => write!(f, "Network error: {reason}"),
            Self::Timeout => f.write_str("Transcription timed out"),
            Self::Http { status, body } => write!(f, "HTTP {status}: {body}"),
            Self::Other(reason) => f.write_str(reason),
        }
    }
}
impl std::error::Error for ProviderError {}
