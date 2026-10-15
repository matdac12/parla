//! Verified 2026-10-15 against official xAI docs:
//! https://docs.x.ai/developers/model-capabilities/audio/speech-to-text
//! https://docs.x.ai/developers/rest-api-reference/inference/speech-to-text
//! https://docs.x.ai/developers/model-capabilities/audio/voice
//! POST https://api.x.ai/v1/stt multipart/form-data; `file` last, JSON {text}.
//! Batch model `grok-voice-transcribe-2.0` is the documented default; batch
//! pricing is $0.10 per audio hour ($0.0017/min).
use super::{Availability, ProviderError, SttProvider};
use crate::{
    clip::{Clip, SAMPLE_RATE},
    settings::Settings,
};
use reqwest::blocking::{multipart, Client, Request};
use serde::{Deserialize, Serialize};
use std::{sync::OnceLock, time::Duration};

pub const DEFAULT_MODEL: &str = "grok-voice-transcribe-2.0";
const ENDPOINT: &str = "https://api.x.ai/v1/stt";
/// xAI's batch endpoint accepts up to 500 MB; guard well below it.
const MAX_UPLOAD_BYTES: usize = 500 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct XaiConfig {
    pub api_key: String,
    pub model: String,
    pub language: Option<String>,
    #[serde(skip)]
    pub vocabulary: Vec<String>,
}
impl Default for XaiConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: DEFAULT_MODEL.into(),
            language: None,
            vocabulary: Vec::new(),
        }
    }
}

impl XaiConfig {
    pub(crate) fn resolved(settings: &Settings) -> Result<Self, ProviderError> {
        // Resolve at use time; never put environment secrets into persisted/UI settings.
        Self::with_key_override(settings, std::env::var("XAI_API_KEY").ok())
    }
    fn with_key_override(settings: &Settings, key: Option<String>) -> Result<Self, ProviderError> {
        let mut config: Self = settings.provider_config("xai");
        if let Some(key) = key {
            config.api_key = key;
        }
        crate::settings::validate_vocabulary(&settings.vocabulary)
            .map_err(ProviderError::MissingConfig)?;
        config.vocabulary = settings.vocabulary.clone();
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> Result<(), ProviderError> {
        if self.api_key.trim().is_empty() {
            return Err(ProviderError::MissingConfig("xAI API key".into()));
        }
        if self.model.trim().is_empty() {
            return Err(ProviderError::MissingConfig("xAI model".into()));
        }
        Ok(())
    }
}

pub struct Xai;
impl SttProvider for Xai {
    fn id(&self) -> &'static str {
        "xai"
    }
    fn display_name(&self) -> &'static str {
        "xAI Grok"
    }
    fn availability(&self, settings: &Settings) -> Availability {
        match XaiConfig::resolved(settings) {
            Ok(_) => Availability::Available,
            Err(e) => Availability::Unavailable(e.to_string()),
        }
    }
    fn transcribe(&self, clip: &Clip, settings: &Settings) -> Result<String, ProviderError> {
        let config = XaiConfig::resolved(settings)?;
        let client = client()?;
        let request = request(client, clip, &config)?;
        let response = client.execute(request).map_err(map_transport)?;
        let status = response.status().as_u16();
        let body = response.text().map_err(map_transport)?;
        parse_response(status, &body)
    }
}

fn client() -> Result<&'static Client, ProviderError> {
    static CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .pool_idle_timeout(Duration::from_secs(90))
                .tcp_keepalive(Duration::from_secs(30))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| ProviderError::Other(e.clone()))
}

fn request(client: &Client, clip: &Clip, config: &XaiConfig) -> Result<Request, ProviderError> {
    config.validate()?;
    crate::settings::validate_vocabulary(&config.vocabulary)
        .map_err(ProviderError::MissingConfig)?;
    if clip.sample_rate != SAMPLE_RATE || clip.samples.is_empty() {
        return Err(ProviderError::Other(
            "Expected nonempty 16 kHz mono audio".into(),
        ));
    }
    let wav = clip.to_wav_bytes();
    if wav.len() > MAX_UPLOAD_BYTES {
        return Err(ProviderError::Other(
            "Audio exceeds xAI's 500 MB upload limit".into(),
        ));
    }
    // xAI requires `file` to be the last field, with option fields before it.
    let mut form = multipart::Form::new().text("model", config.model.clone());
    if let Some(language) = config.language.as_ref().filter(|v| !v.trim().is_empty()) {
        // `format` (inverse text normalization) requires a language code.
        form = form
            .text("language", language.clone())
            .text("format", "true");
    }
    for term in &config.vocabulary {
        form = form.text("keyterm", term.clone());
    }
    let file = multipart::Part::bytes(wav)
        .file_name("dictation.wav")
        .mime_str("audio/wav")
        .map_err(|e| ProviderError::Other(e.to_string()))?;
    let form = form.part("file", file);
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.api_key))
            .map_err(|_| ProviderError::MissingConfig("Invalid xAI API key header".into()))?;
    authorization.set_sensitive(true);
    client
        .post(ENDPOINT)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .multipart(form)
        .build()
        .map_err(map_transport)
}

fn map_transport(error: reqwest::Error) -> ProviderError {
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
                .map_err(|_| ProviderError::Other("Invalid xAI transcription JSON".into()))
        }
        401 | 403 => Err(ProviderError::Auth(format!(
            "xAI authentication rejected (HTTP {status})"
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
            .insert("xai".into(), serde_json::json!({"api_key": "file-key"}));
        let config = XaiConfig::with_key_override(&settings, Some("env-key".into())).unwrap();
        assert_eq!(config.api_key, "env-key");
        assert_eq!(config.model, DEFAULT_MODEL);
        assert!(config.language.is_none());
        assert_eq!(settings.providers["xai"]["api_key"], "file-key");
        assert!(matches!(
            XaiConfig::with_key_override(&settings, Some(String::new())),
            Err(ProviderError::MissingConfig(_))
        ));
        assert_eq!(
            XaiConfig::with_key_override(&settings, None)
                .unwrap()
                .api_key,
            "file-key"
        );
    }
    #[test]
    fn availability_reports_missing_key() {
        let settings = Settings::default();
        assert_eq!(
            Xai.availability(&settings),
            Availability::Unavailable("Missing configuration: xAI API key".into())
        );
    }
    #[test]
    fn invalid_audio_and_missing_model_are_rejected_before_network() {
        let client = Client::new();
        let mut config = XaiConfig {
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
            parse_response(
                200,
                r#"{"text":"Hello ✓","language":"en","duration":1.2,"words":[]}"#
            )
            .unwrap(),
            "Hello ✓"
        );
        assert!(matches!(
            parse_response(200, r#"{"error":"bad"}"#),
            Err(ProviderError::Other(_))
        ));
    }
    #[test]
    fn maps_auth_timeout_http_and_network_without_keys() {
        assert_eq!(
            parse_response(401, "private body sk-masked123*** xai-partial***"),
            Err(ProviderError::Auth(
                "xAI authentication rejected (HTTP 401)".into()
            ))
        );
        assert_eq!(
            parse_response(403, "private body"),
            Err(ProviderError::Auth(
                "xAI authentication rejected (HTTP 403)".into()
            ))
        );
        assert_eq!(
            parse_response(408, "private body"),
            Err(ProviderError::Timeout)
        );
        for status in [400, 413, 429, 500, 503] {
            assert_eq!(
                parse_response(status, "private body sk-masked123*** xai-partial***"),
                Err(ProviderError::Http {
                    status,
                    body: String::new()
                })
            );
        }
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
    fn multipart_puts_model_and_options_before_file() {
        let client = Client::new();
        for language in [None, Some("it".into())] {
            let config = XaiConfig {
                api_key: "test-key".into(),
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
                "name=\"model\"",
                DEFAULT_MODEL,
                "name=\"file\"",
                "filename=\"dictation.wav\"",
                "audio/wav",
                "RIFF",
            ] {
                assert!(body.contains(expected), "missing {expected}");
            }
            // `file` must be the last part; options precede it.
            assert!(body.find("name=\"model\"").unwrap() < body.find("name=\"file\"").unwrap());
            assert_eq!(body.contains("name=\"language\""), language.is_some());
            assert_eq!(body.contains("name=\"format\""), language.is_some());
            if language.is_some() {
                assert!(
                    body.find("name=\"language\"").unwrap() < body.find("name=\"file\"").unwrap()
                );
            }
        }
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;
    #[test]
    fn native_vocabulary_request_uses_supported_fields() {
        let config = XaiConfig {
            api_key: "test".into(),
            vocabulary: vec!["Example & product".into(), "API".into()],
            ..Default::default()
        };
        let mut req = request(&Client::new(), &Clip::new(vec![1; 160]), &config).unwrap();
        let body = String::from_utf8_lossy(req.body_mut().as_mut().unwrap().buffer().unwrap())
            .into_owned();
        assert!(body.contains("name=\"keyterm\""));
        assert!(body.contains("Example & product"));
        assert_eq!(body.matches("name=\"keyterm\"").count(), 2);
        assert!(body.rfind("name=\"keyterm\"").unwrap() < body.find("name=\"file\"").unwrap());
        assert!(!body.contains("name=\"language\""));
    }
}
