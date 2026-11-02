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

/// Sends `request`, and once more when no connection could be made: nothing
/// reached the server, so the retry cannot double a transcription (PARLA-108).
/// The body must be in memory (`Body::buffer`) for the request to be resent.
pub(crate) fn execute(
    client: &reqwest::blocking::Client,
    request: reqwest::blocking::Request,
) -> Result<reqwest::blocking::Response, ProviderError> {
    let again = request.try_clone();
    match (client.execute(request), again) {
        (Err(error), Some(again)) if error.is_connect() => {
            // The cached address may have gone stale: look it up again for
            // the next request, without waiting for the answer here.
            if let Some(host) = again.url().host_str() {
                crate::dns::refresh(host);
            }
            crate::log::error(&format!(
                "Connection failed, retrying once: {}",
                transport_error(error)
            ));
            client.execute(again).map_err(transport_error)
        }
        (result, _) => result.map_err(transport_error),
    }
}

/// Reads a multipart body into memory. Streamed, a failed connection surfaces
/// as reqwest's "request or response body error" with its cause dropped, and
/// the request cannot be sent twice. The clip is in memory already: one copy.
pub(crate) fn buffered(
    mut request: reqwest::blocking::Request,
) -> Result<reqwest::blocking::Request, ProviderError> {
    if let Some(body) = request.body_mut() {
        body.buffer().map_err(transport_error)?;
    }
    Ok(request)
}

/// A connect timeout is a failed connection, not the 30 s request timeout.
/// The detail is reqwest's error chain, never a header or the URL.
pub(crate) fn transport_error(error: reqwest::Error) -> ProviderError {
    classify_transport(
        error.is_connect(),
        error.is_timeout(),
        error_chain(&error.without_url()),
    )
}
fn classify_transport(connect: bool, timeout: bool, detail: String) -> ProviderError {
    if timeout && !connect {
        ProviderError::Timeout
    } else {
        ProviderError::Network(detail)
    }
}
/// "error sending request: client error (Connect): tcp connect error: ..."
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut chain = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        chain.push_str(": ");
        chain.push_str(&cause.to_string());
        source = cause.source();
    }
    chain
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

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::{clip::Clip, settings::Settings};
    use reqwest::dns::{Addrs, Name, Resolve, Resolving};
    use std::{
        net::{SocketAddr, TcpListener},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    #[test]
    fn a_connect_timeout_is_a_failed_connection_not_the_request_timeout() {
        assert_eq!(
            classify_transport(false, true, "timed out".into()),
            ProviderError::Timeout
        );
        for connect in [true, false] {
            assert_eq!(
                classify_transport(connect, connect, "tcp connect error".into()),
                ProviderError::Network("tcp connect error".into())
            );
        }
    }

    /// Sends every host to a port nothing listens on, counting the lookups:
    /// one per connection attempt.
    struct Refused(SocketAddr, Arc<AtomicUsize>);
    impl Resolve for Refused {
        fn resolve(&self, _: Name) -> Resolving {
            self.1.fetch_add(1, Ordering::SeqCst);
            let addr = self.0;
            Box::pin(async move {
                let addrs: Addrs = Box::new(std::iter::once(addr));
                Ok(addrs)
            })
        }
    }

    #[test]
    fn a_failed_connection_is_retried_once_and_named_as_such() {
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let lookups = Arc::new(AtomicUsize::new(0));
        let client = reqwest::blocking::Client::builder()
            .dns_resolver(Arc::new(Refused(addr, lookups.clone())))
            .build()
            .unwrap();
        let mut settings = Settings::default();
        settings
            .providers
            .insert("openai".into(), serde_json::json!({"api_key": "test-key"}));
        // 22 s of audio, ~700 KB: streamed, this upload hid the failed
        // connection behind "request or response body error".
        let clip = Clip::new(vec![0; 16_000 * 22]);
        let config = openai::OpenAiConfig::resolved(&settings).unwrap();
        let request = openai::request(&client, &clip, &config).unwrap();
        let Err(ProviderError::Network(detail)) = execute(&client, request) else {
            panic!("expected a network error");
        };
        assert!(detail.contains("Connect"), "{detail}");
        assert!(!detail.contains("body error"), "{detail}");
        assert_eq!(lookups.load(Ordering::SeqCst), 2);
    }
}
