//! Authenticated WebSocket transport. Deadlines bound setup, writes and finalization.
use super::{
    audio::{AudioChunk, PcmEncoder},
    protocol::{Caption, Event, Provider, Transcript},
};
use crate::{
    provider::{openai::OpenAiConfig, xai::XaiConfig, ProviderError},
    settings::Settings,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::mpsc,
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        http::HeaderValue,
        protocol::{Message, WebSocketConfig},
    },
};

#[derive(Default, Clone)]
pub struct Metrics {
    pub setup_ms: Option<u64>,
    pub first_caption_ms: Option<u64>,
}

pub struct Config {
    pub provider: Provider,
    pub endpoint: String,
    pub model: String,
    api_key: String,
    language: Option<String>,
    vocabulary: Vec<String>,
    pub cancelled: Arc<AtomicBool>,
    pub not_before: Instant,
    pub started: Instant,
    pub metrics: Arc<std::sync::Mutex<Metrics>>,
}

impl Config {
    pub fn from_settings(settings: &Settings) -> Result<Option<Self>, ProviderError> {
        if !settings.streaming_enabled {
            return Ok(None);
        }
        crate::settings::validate_vocabulary(&settings.vocabulary)
            .map_err(ProviderError::MissingConfig)?;
        let config = match settings.active_provider.as_str() {
            "openai" => {
                let config = OpenAiConfig::resolved(settings)?;
                if config.streaming_model.trim().is_empty() {
                    return Err(ProviderError::MissingConfig(
                        "OpenAI streaming model".into(),
                    ));
                }
                Self {
                    provider: Provider::OpenAi,
                    endpoint: "wss://api.openai.com/v1/realtime?intent=transcription".into(),
                    model: config.streaming_model,
                    api_key: config.api_key,
                    language: config.language,
                    vocabulary: settings.vocabulary.clone(),
                    cancelled: Arc::default(),
                    not_before: Instant::now(),
                    started: Instant::now(),
                    metrics: Arc::default(),
                }
            }
            "xai" => {
                let config = XaiConfig::resolved(settings)?;
                let mut url = reqwest::Url::parse("wss://api.x.ai/v1/stt").expect("constant URL");
                url.query_pairs_mut()
                    .append_pair("model", &config.model)
                    .append_pair("sample_rate", "16000")
                    .append_pair("encoding", "pcm")
                    .append_pair("interim_results", "true");
                if let Some(language) = config.language.as_ref().filter(|v| !v.trim().is_empty()) {
                    url.query_pairs_mut()
                        .append_pair("language", language)
                        .append_pair("format", "true");
                }
                for term in &settings.vocabulary {
                    url.query_pairs_mut().append_pair("keyterm", term);
                }
                Self {
                    provider: Provider::Xai,
                    endpoint: url.into(),
                    model: config.model,
                    api_key: config.api_key,
                    language: config.language,
                    vocabulary: settings.vocabulary.clone(),
                    cancelled: Arc::default(),
                    not_before: Instant::now(),
                    started: Instant::now(),
                    metrics: Arc::default(),
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(config))
    }

    fn setup(&self) -> Option<Message> {
        if self.provider != Provider::OpenAi {
            return None;
        }
        let mut transcription = serde_json::json!({"model":self.model});
        if matches!(
            self.model.as_str(),
            "gpt-live-transcribe" | "gpt-transcribe"
        ) {
            if self.model == "gpt-live-transcribe" {
                transcription["delay"] = serde_json::json!("low");
            }
            transcription["languages"] = serde_json::json!(["it", "en"]);
            transcription["keywords"] = serde_json::json!(self.vocabulary);
        } else if let Some(language) = self.language.as_ref().filter(|v| !v.trim().is_empty()) {
            transcription["language"] = serde_json::json!(language);
        }
        Some(Message::Text(
            serde_json::json!({"type":"session.update", "session":{
                "type":"transcription", "audio":{"input":{
                    "format":{"type":"audio/pcm", "rate":24000},
                    "transcription":transcription, "turn_detection":null
                }}
            }})
            .to_string()
            .into(),
        ))
    }
}

const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const FINAL_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn transcribe(
    config: Config,
    mut audio: mpsc::Receiver<AudioChunk>,
    failed: Arc<AtomicBool>,
    mut caption: impl FnMut(Caption) + Send,
) -> Result<String, ProviderError> {
    let mut request = config
        .endpoint
        .as_str()
        .into_client_request()
        .map_err(|_| network())?;
    let mut auth = HeaderValue::from_str(&format!("Bearer {}", config.api_key))
        .map_err(|_| ProviderError::MissingConfig("Invalid streaming API key".into()))?;
    auth.set_sensitive(true);
    request.headers_mut().insert("Authorization", auth);
    let limits = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .max_message_size(Some(1024 * 1024))
        .max_frame_size(Some(1024 * 1024));
    let (mut socket, _) = timeout(
        SETUP_TIMEOUT,
        connect_async_with_config(request, Some(limits), true),
    )
    .await
    .map_err(|_| ProviderError::Timeout)?
    .map_err(|_| network())?;
    if let Some(setup) = config.setup() {
        timeout(WRITE_TIMEOUT, socket.send(setup))
            .await
            .map_err(|_| ProviderError::Timeout)?
            .map_err(|_| network())?;
    }
    let mut transcript = Transcript::new(config.provider);
    let ready_deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let incoming = tokio::time::timeout_at(ready_deadline, socket.next())
            .await
            .map_err(|_| ProviderError::Timeout)?
            .ok_or_else(network)?
            .map_err(|_| network())?;
        if let Message::Text(text) = incoming {
            let event = serde_json::from_str(&text).map_err(|_| invalid())?;
            if matches!(transcript.apply(&event)?, Event::Ready) {
                break;
            }
        } else if matches!(incoming, Message::Close(_)) {
            return Err(network());
        }
    }
    config
        .metrics
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .setup_ms = Some(config.started.elapsed().as_millis() as u64);
    // Connection and authenticated setup overlap the hold grace. Audio remains local.
    tokio::time::sleep_until(config.not_before).await;
    if config.cancelled.load(Ordering::Acquire) {
        return Err(invalid());
    }
    let mut encoder = PcmEncoder::new(if config.provider == Provider::OpenAi {
        24000
    } else {
        16000
    });
    let mut final_deadline = None;
    loop {
        if failed.load(Ordering::Acquire) {
            return Err(ProviderError::Other(
                "Live audio queue could not keep up; using saved clip".into(),
            ));
        }
        // The sleep is dormant during recording. Once released, its fixed
        // deadline cannot be extended by pings or other non-final messages.
        let deadline = final_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(150));
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Err(ProviderError::Timeout),
            packet = audio.recv(), if final_deadline.is_none() => {
                if config.cancelled.load(Ordering::Acquire) { return Err(invalid()); }
                let ending = packet.is_none();
                let bytes = match packet { Some(packet) => encoder.push(packet)?, None => encoder.finish()? };
                if !bytes.is_empty() {
                    let message = match config.provider {
                        Provider::OpenAi => Message::Text(serde_json::json!({"type":"input_audio_buffer.append", "audio":STANDARD.encode(bytes)}).to_string().into()),
                        Provider::Xai => Message::Binary(bytes.into()),
                    };
                    timeout(WRITE_TIMEOUT, socket.send(message)).await.map_err(|_| ProviderError::Timeout)?.map_err(|_| network())?;
                }
                if ending {
                    if failed.load(Ordering::Acquire) { return Err(invalid()); }
                    let kind = if config.provider == Provider::OpenAi { "input_audio_buffer.commit" } else { "audio.done" };
                    timeout(WRITE_TIMEOUT, socket.send(Message::Text(serde_json::json!({"type":kind}).to_string().into()))).await.map_err(|_| ProviderError::Timeout)?.map_err(|_| network())?;
                    final_deadline = Some(Instant::now() + FINAL_TIMEOUT);
                }
            }
            incoming = socket.next() => {
                let message = incoming.ok_or_else(network)?.map_err(|_| network())?;
                match message {
                    Message::Text(text) => {
                        let json = serde_json::from_str(&text).map_err(|_| invalid())?;
                        match transcript.apply(&json)? {
                            Event::Caption(update) => {
                                if !update.stable.is_empty() || !update.provisional.is_empty() {
                                    config.metrics.lock().unwrap_or_else(|e| e.into_inner()).first_caption_ms.get_or_insert(config.started.elapsed().as_millis() as u64);
                                }
                                caption(update)
                            },
                            Event::Final(text) if final_deadline.is_some() => return Ok(text),
                            Event::Final(_) => return Err(ProviderError::Other("Live transcription finalized before recording ended".into())),
                            _ => {}
                        }
                    }
                    Message::Close(_) => return Err(network()),
                    _ => {}
                }
            }
        }
    }
}

fn network() -> ProviderError {
    ProviderError::Network("Live transcription connection failed".into())
}
fn invalid() -> ProviderError {
    ProviderError::Other("Invalid live transcription response".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    #[ignore = "Explicit opt-in: uploads only the bundled hello.wav fixture"]
    async fn live_fixture() {
        let path = std::env::var("PARLA_LIVE_STREAM_SETTINGS").expect("settings path required");
        let mut settings = crate::settings::load(std::path::Path::new(&path)).unwrap();
        settings.active_provider = std::env::var("PARLA_LIVE_STREAM_PROVIDER").unwrap();
        settings.streaming_enabled = true;
        settings.vocabulary = vec!["hello".into()];
        let mut config = Config::from_settings(&settings).unwrap().unwrap();
        let started = Instant::now();
        config.started = started;
        config.not_before = started + crate::flow::MIN_HOLD;
        let metrics = config.metrics.clone();
        let reader = hound::WavReader::open("tests/fixtures/hello.wav").unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        let samples: Vec<f32> = reader
            .into_samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        let (mut feed, receiver, failed) = super::super::audio::channel();
        let producer = tokio::spawn(async move {
            for packet in samples.chunks(spec.sample_rate as usize / 10) {
                feed.push(packet, spec.sample_rate);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let mut captions = 0;
        if std::env::var("PARLA_LIVE_DELAY_SETUP").is_ok() {
            tokio::time::sleep(crate::flow::MIN_HOLD).await;
        }
        let text = transcribe(config, receiver, failed, |_| captions += 1)
            .await
            .unwrap();
        let returned_ms = started.elapsed().as_millis();
        producer.await.unwrap();
        let metrics = metrics.lock().unwrap();
        println!("Fixture timings from session start: setup={:?}ms first_caption={:?}ms returned={returned_ms}ms delayed_setup={}", metrics.setup_ms, metrics.first_caption_ms, std::env::var("PARLA_LIVE_DELAY_SETUP").is_ok());
        assert!(!text.trim().is_empty());
        println!(
            "Live fixture passed: {} final characters, {captions} caption updates",
            text.chars().count()
        );
    }

    #[test]
    #[ignore = "Explicit opt-in: uploads only bundled hello.wav through batch recovery"]
    fn live_fixture_batch() {
        let path = std::env::var("PARLA_LIVE_STREAM_SETTINGS").expect("settings path required");
        let mut settings = crate::settings::load(std::path::Path::new(&path)).unwrap();
        settings.active_provider = std::env::var("PARLA_LIVE_STREAM_PROVIDER").unwrap();
        settings.vocabulary = vec!["hello".into()];
        let clip =
            crate::clip::Clip::from_wav_bytes(include_bytes!("../../tests/fixtures/hello.wav"))
                .unwrap();
        let provider = crate::provider::registry::get(&settings.active_provider).unwrap();
        let text = provider.transcribe(&clip, &settings).unwrap();
        assert!(!text.trim().is_empty());
        println!(
            "Batch fixture passed: {} final characters",
            text.chars().count()
        );
    }

    fn config(provider: Provider, endpoint: String) -> Config {
        Config {
            provider,
            endpoint,
            model: "test-model".into(),
            api_key: "not-a-real-key".into(),
            language: None,
            vocabulary: Vec::new(),
            cancelled: Arc::default(),
            not_before: Instant::now(),
            started: Instant::now(),
            metrics: Arc::default(),
        }
    }

    #[tokio::test]
    async fn openai_socket_sends_all_audio_before_commit_and_uses_final_text() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(tcp).await.unwrap();
            let setup = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let setup: serde_json::Value = serde_json::from_str(&setup).unwrap();
            assert_eq!(setup["session"]["type"], "transcription");
            assert!(setup["session"]["audio"]["input"]["turn_detection"].is_null());
            socket
                .send(Message::Text(r#"{"type":"session.updated"}"#.into()))
                .await
                .unwrap();
            let mut received = Vec::new();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let event: serde_json::Value = serde_json::from_str(&text).unwrap();
                if event["type"] == "input_audio_buffer.append" {
                    received.extend(STANDARD.decode(event["audio"].as_str().unwrap()).unwrap());
                    socket.send(Message::Text(r#"{"type":"conversation.item.input_audio_transcription.delta","item_id":"one","delta":"Hello"}"#.into())).await.unwrap();
                } else if event["type"] == "input_audio_buffer.commit" {
                    assert_eq!(received.len(), 6000);
                    socket.send(Message::Text(r#"{"type":"conversation.item.input_audio_transcription.completed","item_id":"one","transcript":"Hello, ciao."}"#.into())).await.unwrap();
                    break;
                }
            }
        });
        let (mut feed, receiver, failed) = super::super::audio::channel();
        feed.push(&vec![0.25; 2000], 16000);
        drop(feed);
        let mut captions = 0;
        let mut config = config(Provider::OpenAi, endpoint);
        config.not_before = Instant::now() + Duration::from_millis(100);
        let text = transcribe(config, receiver, failed, |_| captions += 1)
            .await
            .unwrap();
        assert_eq!(text, "Hello, ciao.");
        assert!(captions > 0);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn xai_socket_ends_only_after_the_last_packet_and_reconciles_finals() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(tcp).await.unwrap();
            socket
                .send(Message::Text(r#"{"type":"transcript.created"}"#.into()))
                .await
                .unwrap();
            let mut bytes = 0;
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Binary(packet) => bytes += packet.len(),
                    Message::Text(text) => {
                        assert_eq!(
                            serde_json::from_str::<serde_json::Value>(&text).unwrap()["type"],
                            "audio.done"
                        );
                        assert_eq!(bytes, 3602);
                        for event in [
                            r#"{"type":"transcript.partial","start":0,"duration":0.1,"text":"Ciao.","is_final":true}"#,
                            r#"{"type":"transcript.done","duration":0.1}"#,
                        ] {
                            socket.send(Message::Text(event.into())).await.unwrap();
                        }
                        break;
                    }
                    _ => {}
                }
            }
        });
        let (mut feed, receiver, failed) = super::super::audio::channel();
        feed.push(&vec![0.5; 1801], 16000);
        drop(feed);
        assert_eq!(
            transcribe(config(Provider::Xai, endpoint), receiver, failed, |_| {})
                .await
                .unwrap(),
            "Ciao."
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_handshake_and_session_readiness_have_setup_deadlines() {
        // Both cases wait out the real SETUP_TIMEOUT; run them side by side.
        futures_util::future::join_all([false, true].map(|handshake_complete| async move {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("ws://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                if handshake_complete {
                    let _socket = accept_async(tcp).await.unwrap();
                    std::future::pending::<()>().await;
                } else {
                    let _tcp = tcp;
                    std::future::pending::<()>().await;
                }
            });
            let (feed, receiver, failed) = super::super::audio::channel();
            let result = timeout(
                SETUP_TIMEOUT + Duration::from_secs(2),
                transcribe(config(Provider::Xai, endpoint), receiver, failed, |_| {
                    panic!("Unready session emitted captions")
                }),
            )
            .await
            .expect("Setup deadline did not fire");
            assert!(matches!(result, Err(ProviderError::Timeout)));
            drop(feed);
            server.abort();
        }))
        .await;
    }

    #[tokio::test]
    async fn missing_final_times_out_even_when_the_connection_stays_open() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(tcp).await.unwrap();
            socket
                .send(Message::Text(r#"{"type":"transcript.created"}"#.into()))
                .await
                .unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if let Message::Text(_) = message {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    break;
                }
            }
        });
        let (mut feed, receiver, failed) = super::super::audio::channel();
        feed.push(&vec![0.25; 1600], 16000);
        drop(feed);
        assert!(matches!(
            transcribe(config(Provider::Xai, endpoint), receiver, failed, |_| {}).await,
            Err(ProviderError::Timeout)
        ));
        server.abort();
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;
    #[test]
    fn committed_turn_model_also_receives_vocabulary_and_bilingual_hints() {
        let mut settings = Settings {
            vocabulary: vec!["Example product".into()],
            ..Default::default()
        };
        settings.providers.insert("openai".into(), serde_json::json!({"api_key":"test", "streaming_model":"gpt-transcribe", "language":"it"}));
        let config = Config::from_settings(&settings).unwrap().unwrap();
        let msg = config.setup().unwrap().into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&msg).unwrap();
        let hints = &json["session"]["audio"]["input"]["transcription"];
        assert_eq!(hints["languages"], serde_json::json!(["it", "en"]));
        assert_eq!(hints["keywords"], serde_json::json!(["Example product"]));
        assert!(hints.get("language").is_none());
        assert!(hints.get("delay").is_none());
    }

    #[test]
    fn native_session_hints_are_validated_encoded_and_model_compatible() {
        let mut settings = Settings {
            vocabulary: vec!["Example & product".into()],
            ..Default::default()
        };
        settings
            .providers
            .insert("openai".into(), serde_json::json!({"api_key":"test"}));
        settings
            .providers
            .insert("xai".into(), serde_json::json!({"api_key":"test"}));
        let config = Config::from_settings(&settings).unwrap().unwrap();
        let msg = config.setup().unwrap().into_text().unwrap();
        let json: serde_json::Value = serde_json::from_str(&msg).unwrap();
        let hints = &json["session"]["audio"]["input"]["transcription"];
        assert_eq!(hints["languages"], serde_json::json!(["it", "en"]));
        assert_eq!(hints["keywords"], serde_json::json!(["Example & product"]));
        assert!(hints.get("language").is_none());
        settings.active_provider = "xai".into();
        let config = Config::from_settings(&settings).unwrap().unwrap();
        let url = reqwest::Url::parse(&config.endpoint).unwrap();
        let pairs: Vec<_> = url.query_pairs().collect();
        assert!(pairs
            .iter()
            .any(|(k, v)| k == "keyterm" && v == "Example & product"));
        assert!(!pairs.iter().any(|(k, _)| k == "language"));
        settings.vocabulary = vec!["<unsafe>".into()];
        assert!(Config::from_settings(&settings).is_err());
        settings.active_provider = "openai".into();
        assert!(Config::from_settings(&settings).is_err());
    }
}
