//! Verified 2026-10-15 against official docs:
//! https://developers.openai.com/api/reference/resources/audio/subresources/transcriptions/methods/create
//! https://developers.openai.com/api/docs/models/gpt-transcribe
//! https://developers.openai.com/api/docs/pricing
//! POST multipart file + model; JSON {text}; gpt-transcribe: estimated $0.0045/min.
//! The spec's model now exists. Keep the model configurable for latency comparisons.
use super::{Availability, ProviderError, SttProvider};
use crate::{
    clip::{Clip, SAMPLE_RATE},
    settings::Settings,
};
use reqwest::blocking::{multipart, Client, Request};
use serde::{Deserialize, Serialize};
use std::{sync::OnceLock, time::Duration};

pub const DEFAULT_MODEL: &str = "gpt-transcribe";
const ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct OpenAiConfig {
    pub api_key: String,
    pub model: String,
    pub language: Option<String>,
    #[serde(skip)]
    pub vocabulary: Vec<String>,
    pub streaming_model: String,
}
impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: DEFAULT_MODEL.into(),
            language: None,
            vocabulary: Vec::new(),
            streaming_model: "gpt-live-transcribe".into(),
        }
    }
}

impl OpenAiConfig {
    pub(crate) fn resolved(settings: &Settings) -> Result<Self, ProviderError> {
        // Resolve at use time; never put environment secrets into persisted/UI settings.
        Self::with_key_override(settings, std::env::var("OPENAI_API_KEY").ok())
    }
    fn with_key(settings: &Settings, key: Option<String>) -> Self {
        let mut config: Self = settings.provider_config("openai");
        if let Some(key) = key {
            config.api_key = key;
        }
        config
    }
    fn with_key_override(settings: &Settings, key: Option<String>) -> Result<Self, ProviderError> {
        let mut config = Self::with_key(settings, key);
        crate::settings::validate_vocabulary(&settings.vocabulary)
            .map_err(ProviderError::MissingConfig)?;
        config.vocabulary = settings.vocabulary.clone();
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> Result<(), ProviderError> {
        if self.api_key.trim().is_empty() {
            return Err(ProviderError::MissingConfig("OpenAI API key".into()));
        }
        if self.model.trim().is_empty() {
            return Err(ProviderError::MissingConfig("OpenAI model".into()));
        }
        Ok(())
    }
}

/// The OpenAI key alone, by the same rule as dictation: `OPENAI_API_KEY`
/// overrides `providers.openai.api_key`. Command mode uses it (PARLA-63).
pub fn api_key(settings: &Settings) -> Option<String> {
    key_with_override(settings, std::env::var("OPENAI_API_KEY").ok())
}
fn key_with_override(settings: &Settings, key: Option<String>) -> Option<String> {
    Some(OpenAiConfig::with_key(settings, key).api_key).filter(|key| !key.trim().is_empty())
}

pub struct OpenAi;
impl SttProvider for OpenAi {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn display_name(&self) -> &'static str {
        "OpenAI"
    }
    fn availability(&self, settings: &Settings) -> Availability {
        match OpenAiConfig::resolved(settings) {
            Ok(_) => Availability::Available,
            Err(e) => Availability::Unavailable(e.to_string()),
        }
    }
    fn transcribe(&self, clip: &Clip, settings: &Settings) -> Result<String, ProviderError> {
        let config = OpenAiConfig::resolved(settings)?;
        let client = client()?;
        let request = request(client, clip, &config)?;
        let response = client.execute(request).map_err(map_transport)?;
        let status = response.status().as_u16();
        let body = response.text().map_err(map_transport)?;
        parse_response(status, &body)
    }
}

/// The shared client's connect timeout; Traccia's probe budget derives from it.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Shared with command mode's Responses call, which sets its own timeout.
pub(crate) fn client() -> Result<&'static Client, ProviderError> {
    static CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(90))
                .tcp_keepalive(Duration::from_secs(30))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| ProviderError::Other(e.clone()))
}

/// Opens TCP and TLS to api.openai.com in the shared client's pool, off the
/// caller's thread, so command mode's request skips the handshake (PARLA-64).
/// Unauthenticated, so OpenAI answers 401 and bills nothing. Skipped without
/// a key: the request will not be sent.
pub fn prewarm(settings: &Settings) {
    if api_key(settings).is_none() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("parla-openai-prewarm".into())
        .spawn(|| {
            if let Ok(response) =
                client().and_then(|client| client.get(PREWARM_URL).send().map_err(map_transport))
            {
                // Read to the end, or the connection is not pooled.
                let _ = response.bytes();
            }
        });
}
const PREWARM_URL: &str = "https://api.openai.com/v1/models";

fn request(client: &Client, clip: &Clip, config: &OpenAiConfig) -> Result<Request, ProviderError> {
    config.validate()?;
    crate::settings::validate_vocabulary(&config.vocabulary)
        .map_err(ProviderError::MissingConfig)?;
    if clip.sample_rate != SAMPLE_RATE || clip.samples.is_empty() {
        return Err(ProviderError::Other(
            "Expected nonempty 16 kHz mono audio".into(),
        ));
    }
    let wav = clip.to_wav_bytes();
    // The API limits the uploaded file to 25 MB.
    if wav.len() > 25_000_000 {
        return Err(ProviderError::Other(
            "Audio exceeds OpenAI's 25 MB upload limit".into(),
        ));
    }
    let file = multipart::Part::bytes(wav)
        .file_name("dictation.wav")
        .mime_str("audio/wav")
        .map_err(|e| ProviderError::Other(e.to_string()))?;
    let mut form = multipart::Form::new()
        .part("file", file)
        .text("model", config.model.clone())
        .text("response_format", "json");
    if config.model == "gpt-transcribe" {
        for language in ["it", "en"] {
            form = form.text("languages[]", language);
        }
        for term in &config.vocabulary {
            form = form.text("keywords[]", term.clone());
        }
    } else if let Some(language) = config.language.as_ref().filter(|v| !v.trim().is_empty()) {
        form = form.text("language", language.clone());
    }
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .map_err(|_| ProviderError::MissingConfig("Invalid OpenAI API key header".into()))?;
    authorization.set_sensitive(true);
    client
        .post(ENDPOINT)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .multipart(form)
        .build()
        .map_err(map_transport)
}

pub(crate) fn map_transport(error: reqwest::Error) -> ProviderError {
    classify_transport(error.is_timeout(), error.without_url().to_string())
}
fn classify_transport(timeout: bool, message: String) -> ProviderError {
    if timeout {
        ProviderError::Timeout
    } else {
        ProviderError::Network(message)
    }
}
fn parse_response(status: u16, body: &str) -> Result<String, ProviderError> {
    // Never retain remote error bodies: they may echo masked keys, transcript
    // content, request details, or other secrets that exact-key masking misses.
    match status {
        200..=299 => {
            #[derive(Deserialize)]
            struct Transcript {
                text: String,
            }
            serde_json::from_str::<Transcript>(body)
                .map(|value| value.text)
                .map_err(|_| ProviderError::Other("Invalid OpenAI transcription JSON".into()))
        }
        401 | 403 => Err(ProviderError::Auth(format!(
            "OpenAI authentication rejected (HTTP {status})"
        ))),
        408 => Err(ProviderError::Timeout),
        _ => Err(ProviderError::Http {
            status,
            body: String::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_defaults_and_environment_override_do_not_mutate_saved_key() {
        let mut settings = Settings::default();
        settings
            .providers
            .insert("openai".into(), serde_json::json!({"api_key": "file-key"}));
        let config = OpenAiConfig::with_key_override(&settings, Some("env-key".into())).unwrap();
        assert_eq!(config.api_key, "env-key");
        assert_eq!(config.model, DEFAULT_MODEL);
        assert!(config.language.is_none());
        assert_eq!(settings.providers["openai"]["api_key"], "file-key");
        assert!(matches!(
            OpenAiConfig::with_key_override(&settings, Some(String::new())),
            Err(ProviderError::MissingConfig(_))
        ));
        assert_eq!(
            OpenAiConfig::with_key_override(&settings, None)
                .unwrap()
                .api_key,
            "file-key"
        );
    }
    #[test]
    fn the_bare_key_follows_the_same_override_rule() {
        let mut settings = Settings::default();
        assert_eq!(key_with_override(&settings, None), None);
        settings
            .providers
            .insert("openai".into(), serde_json::json!({"api_key": "file-key"}));
        assert_eq!(
            key_with_override(&settings, None).as_deref(),
            Some("file-key")
        );
        assert_eq!(
            key_with_override(&settings, Some("env-key".into())).as_deref(),
            Some("env-key")
        );
        assert_eq!(key_with_override(&settings, Some(" ".into())), None);
    }
    #[test]
    fn invalid_audio_and_missing_model_are_rejected_before_network() {
        let client = Client::new();
        let mut config = OpenAiConfig {
            api_key: "fake-key".into(),
            ..Default::default()
        };
        assert!(request(&client, &Clip::new(vec![]), &config).is_err());
        assert!(request(
            &client,
            &Clip {
                samples: vec![1],
                sample_rate: 48000
            },
            &config
        )
        .is_err());
        config.model.clear();
        assert!(matches!(
            request(&client, &Clip::new(vec![1]), &config),
            Err(ProviderError::MissingConfig(_))
        ));
    }
    #[test]
    fn parses_json_and_rejects_missing_text() {
        assert_eq!(
            parse_response(200, r#"{"text":"Hello ✓","usage":{}}"#).unwrap(),
            "Hello ✓"
        );
        assert!(matches!(
            parse_response(200, r#"{"error":"bad"}"#),
            Err(ProviderError::Other(_))
        ));
    }
    #[test]
    fn maps_auth_http_timeout_and_network_without_keys() {
        assert_eq!(
            parse_response(401, "private body sk-masked123*** xai-partial***"),
            Err(ProviderError::Auth(
                "OpenAI authentication rejected (HTTP 401)".into()
            ))
        );
        for status in [429, 500, 503] {
            assert_eq!(
                parse_response(status, "private body sk-masked123*** xai-partial***"),
                Err(ProviderError::Http {
                    status,
                    body: String::new()
                })
            );
        }
        // A 408 is the server timing out the upload, as in xai.rs.
        assert_eq!(
            parse_response(408, "private body"),
            Err(ProviderError::Timeout)
        );
        assert_eq!(
            classify_transport(true, "timed out".into()),
            ProviderError::Timeout
        );
        assert!(matches!(
            classify_transport(false, "offline".into()),
            ProviderError::Network(_)
        ));
    }
    #[test]
    fn multipart_contains_wav_model_json_and_optional_language() {
        let client = Client::new();
        for language in [None, Some("it".into())] {
            let config = OpenAiConfig {
                api_key: "test-key".into(),
                model: "whisper-1".into(),
                language: language.clone(),
                ..Default::default()
            };
            let mut request = request(&client, &Clip::new(vec![42; 160]), &config).unwrap();
            assert_eq!(request.method(), "POST");
            assert_eq!(request.url().as_str(), ENDPOINT);
            assert_eq!(request.headers()["authorization"], "Bearer test-key");
            assert!(request.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("multipart/form-data; boundary="));
            let bytes = request.body_mut().as_mut().unwrap().buffer().unwrap();
            let body = String::from_utf8_lossy(bytes);
            for expected in [
                "name=\"file\"",
                "filename=\"dictation.wav\"",
                "audio/wav",
                "RIFF",
                "name=\"model\"",
                "whisper-1",
                "name=\"response_format\"",
                "json",
            ] {
                assert!(body.contains(expected), "missing {expected}");
            }
            assert_eq!(body.contains("name=\"language\""), language.is_some());
        }
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;
    #[test]
    fn native_vocabulary_request_uses_supported_fields() {
        let config = OpenAiConfig {
            api_key: "test".into(),
            vocabulary: vec!["Example & product".into(), "API".into()],
            ..Default::default()
        };
        let mut req = request(&Client::new(), &Clip::new(vec![1; 160]), &config).unwrap();
        let body = String::from_utf8_lossy(req.body_mut().as_mut().unwrap().buffer().unwrap())
            .into_owned();
        assert!(body.contains("name=\"keywords[]\""));
        assert!(body.contains("Example & product"));
        assert_eq!(body.matches("name=\"languages[]\"").count(), 2);
        assert!(!body.contains("name=\"language\""));
        let old = OpenAiConfig {
            model: "whisper-1".into(),
            ..config
        };
        let mut req = request(&Client::new(), &Clip::new(vec![1]), &old).unwrap();
        let body = String::from_utf8_lossy(req.body_mut().as_mut().unwrap().buffer().unwrap());
        assert!(!body.contains("keywords[]") && !body.contains("languages[]"));
    }
}
