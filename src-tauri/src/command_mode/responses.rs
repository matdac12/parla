//! The OpenAI Responses API client for command mode (never Chat Completions).
//! Pure request building and response parsing, plus one thin blocking call.

use crate::{provider::ProviderError, settings::Settings};
use reqwest::{
    blocking::Client,
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
};
use serde::Deserialize;
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, OnceLock,
    },
    time::Duration,
};

/// About 2,000 tokens, reasoning included: a reply to paste, not an essay.
pub const MAX_OUTPUT_TOKENS: u32 = 2000;

/// The spec's starting point (PARLA-61); PARLA-68 tunes it. The `open_url`
/// line joins with the tool (PARLA-65).
pub const SYSTEM_PROMPT: &str = "You are Parla's command mode. Apply the instruction to the selected text and output only the resulting text: no preamble, quotes or markdown fences. \
Keep the selection's language, formatting and line breaks unless the instruction says otherwise. \
With no selection, answer the instruction briefly as plain text ready to paste.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub model: String,
    pub reasoning_effort: String,
    pub instruction: String,
}

/// One non-streaming request. `store: false` because the Responses API
/// stores requests by default; the effort is explicit because it defaults to
/// `medium`.
pub fn body(request: &Request) -> serde_json::Value {
    json!({
        "model": request.model,
        "instructions": SYSTEM_PROMPT,
        "input": [{"role": "user", "content": request.instruction}],
        "store": false,
        "reasoning": {"effort": request.reasoning_effort},
        "max_output_tokens": MAX_OUTPUT_TOKENS,
    })
}

/// The text to paste and the tokens billed (`usage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
}

#[derive(Deserialize)]
struct Response {
    status: Option<String>,
    incomplete_details: Option<Incomplete>,
    #[serde(default)]
    output: Vec<OutputItem>,
    usage: Option<Usage>,
}
#[derive(Deserialize)]
struct Incomplete {
    reason: Option<String>,
}
#[derive(Deserialize)]
struct OutputItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Vec<Content>,
}
#[derive(Deserialize)]
struct Content {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}
#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}
#[derive(Deserialize)]
struct ApiError {
    error: ApiErrorBody,
}
#[derive(Deserialize)]
struct ApiErrorBody {
    code: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// The reply's `output_text`, joined in order. Reasoning items are skipped;
/// `function_call` items come with the first tool (PARLA-65). An empty text is
/// the orchestrator's to reject. Error bodies are never kept, only their
/// `code`: the message may echo a masked key or the request.
pub fn parse(status: u16, body: &str) -> Result<Reply, ProviderError> {
    match status {
        200..=299 => {
            let response: Response = serde_json::from_str(body)
                .map_err(|_| ProviderError::Other("Invalid OpenAI Responses JSON".into()))?;
            if response.status.as_deref() != Some("completed") {
                let reason = response
                    .incomplete_details
                    .and_then(|details| details.reason)
                    .or(response.status)
                    .unwrap_or_else(|| "no status".into());
                return Err(ProviderError::Other(format!("Reply incomplete ({reason})")));
            }
            let text = response
                .output
                .iter()
                .filter(|item| item.kind == "message")
                .flat_map(|item| &item.content)
                .filter(|content| content.kind == "output_text")
                .map(|content| content.text.as_str())
                .collect();
            let usage = response.usage;
            Ok(Reply {
                text,
                tokens_in: usage.as_ref().and_then(|usage| usage.input_tokens),
                tokens_out: usage.and_then(|usage| usage.output_tokens),
            })
        }
        401 | 403 => Err(ProviderError::Auth(format!(
            "OpenAI authentication rejected (HTTP {status})"
        ))),
        _ => Err(ProviderError::Http {
            status,
            body: serde_json::from_str::<ApiError>(body)
                .ok()
                .and_then(|error| error.error.code.or(error.error.kind))
                .unwrap_or_default(),
        }),
    }
}

const ENDPOINT: &str = "https://api.openai.com/v1/responses";
/// A reply slower than this fails instead of leaving the pill up.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// The one HTTP call, blocking: run it off the flow thread.
pub fn send(api_key: &str, request: &Request) -> Result<Reply, ProviderError> {
    let mut authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .map_err(|_| ProviderError::MissingConfig("Invalid OpenAI API key header".into()))?;
    authorization.set_sensitive(true);
    let response = client()?
        .post(ENDPOINT)
        .header(AUTHORIZATION, authorization)
        .header(CONTENT_TYPE, "application/json")
        .body(body(request).to_string())
        .send()
        .map_err(map_transport)?;
    let status = response.status().as_u16();
    let text = response.text().map_err(map_transport)?;
    parse(status, &text)
}

/// The LLM port: `send` on its own thread, so Esc returns at once and a late
/// reply lands in a dropped channel.
pub struct OpenAi<'a>(pub &'a Settings);
impl super::Llm for OpenAi<'_> {
    fn reply(&self, request: &Request, cancelled: &AtomicBool) -> Result<Reply, ProviderError> {
        let api_key = crate::provider::openai::api_key(self.0)
            .ok_or_else(|| ProviderError::MissingConfig("OpenAI API key".into()))?;
        let request = request.clone();
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("parla-command-llm".into())
            .spawn(move || {
                let _ = sender.send(send(&api_key, &request));
            })
            .map_err(|error| ProviderError::Other(format!("Cannot start the request: {error}")))?;
        loop {
            match receiver.recv_timeout(CANCEL_POLL) {
                Ok(reply) => return reply,
                Err(mpsc::RecvTimeoutError::Timeout) if cancelled.load(Ordering::Acquire) => {
                    return Err(ProviderError::Other("Cancelled".into()))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProviderError::Other("The request thread stopped.".into()))
                }
            }
        }
    }
}
/// How soon Esc frees the pill during the wait.
const CANCEL_POLL: Duration = Duration::from_millis(20);

fn client() -> Result<&'static Client, ProviderError> {
    static CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(TIMEOUT)
                .pool_idle_timeout(Duration::from_secs(90))
                .tcp_keepalive(Duration::from_secs(30))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| ProviderError::Other(e.clone()))
}

fn map_transport(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Network(error.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/responses/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_text_reply_joins_its_output_text_and_reads_usage() {
        assert_eq!(
            parse(200, &fixture("text_reply")),
            Ok(Reply {
                text: "Ciao a tutti, buona giornata!".into(),
                tokens_in: Some(92),
                tokens_out: Some(18),
            })
        );
    }

    #[test]
    fn a_reply_cut_off_by_the_output_cap_is_an_error_not_half_a_paste() {
        assert_eq!(
            parse(200, &fixture("incomplete")),
            Err(ProviderError::Other(
                "Reply incomplete (max_output_tokens)".into()
            ))
        );
    }

    #[test]
    fn api_errors_keep_the_status_and_code_but_never_the_message() {
        assert_eq!(
            parse(404, &fixture("error_model_not_found")),
            Err(ProviderError::Http {
                status: 404,
                body: "model_not_found".into()
            })
        );
        let auth = parse(401, &fixture("error_invalid_key"));
        assert_eq!(
            auth,
            Err(ProviderError::Auth(
                "OpenAI authentication rejected (HTTP 401)".into()
            ))
        );
        // Not JSON at all (a proxy page) still maps to the status.
        assert_eq!(
            parse(502, "<html>Bad gateway</html>"),
            Err(ProviderError::Http {
                status: 502,
                body: String::new()
            })
        );
        assert!(matches!(
            parse(200, "not json"),
            Err(ProviderError::Other(_))
        ));
    }

    fn request() -> Request {
        Request {
            model: "gpt-6-luna".into(),
            reasoning_effort: "low".into(),
            instruction: "Scrivi un saluto breve".into(),
        }
    }

    #[test]
    fn the_request_is_unstored_with_the_system_prompt_effort_and_output_cap() {
        let body = body(&request());
        assert_eq!(body["model"], "gpt-6-luna");
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"], json!({"effort": "low"}));
        assert_eq!(body["max_output_tokens"], 2000);
        let prompt = body["instructions"].as_str().unwrap();
        assert!(prompt.starts_with("You are Parla's command mode."));
        assert!(prompt.contains("no preamble, quotes or markdown fences"));
        assert!(prompt.contains("With no selection, answer the instruction briefly"));
        assert_eq!(
            body["input"],
            json!([{"role": "user", "content": "Scrivi un saluto breve"}])
        );
        // One non-streaming reply, no tools until open_url lands (PARLA-65).
        assert!(body.get("stream").is_none_or(|stream| stream == false));
        assert!(body.get("tools").is_none());
    }
}
