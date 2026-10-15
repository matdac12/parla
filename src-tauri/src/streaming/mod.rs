//! Backend-only live dictation. The complete recording remains the recovery source.
pub mod audio;
mod local;
pub mod protocol;
mod transport;

use crate::{provider::ProviderError, settings::Settings};
pub use protocol::Caption;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

pub struct Streamed {
    pub text: String,
    pub model: String,
    pub setup_ms: Option<u64>,
    pub first_caption_ms: Option<u64>,
}

pub struct Session {
    result: mpsc::Receiver<Result<Streamed, ProviderError>>,
    /// The cloud transport task; local sessions run on threads instead.
    task: Option<tauri::async_runtime::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
    finish_timeout: Duration,
}

/// A cloud final arrives within moments of key-up or not at all.
const CLOUD_FINISH: Duration = Duration::from_secs(4);
/// The local tail is still computing; the batch fallback would only redo it.
const LOCAL_FINISH: Duration = Duration::from_secs(60);

impl Session {
    pub fn start(
        settings: &Settings,
        caption: impl FnMut(Caption) + Send + 'static,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Option<(Self, audio::AudioFeed)>, ProviderError> {
        if settings.active_provider == crate::models::ID {
            return Ok(Self::start_local(settings, caption, cancelled));
        }
        let Some(config) = transport::Config::from_settings(settings)? else {
            return Ok(None);
        };
        Ok(Some(Self::spawn_cancellable(config, caption, cancelled)))
    }

    #[cfg(test)]
    pub(crate) fn test_session(
        endpoint: String,
        cancelled: Arc<AtomicBool>,
    ) -> (Self, audio::AudioFeed) {
        let mut settings = Settings::default();
        settings
            .providers
            .insert("openai".into(), serde_json::json!({"api_key":"test"}));
        let mut config = transport::Config::from_settings(&settings)
            .unwrap()
            .unwrap();
        config.endpoint = endpoint;
        Self::spawn_cancellable(
            config,
            |_| panic!("cancelled hold emitted captions"),
            cancelled,
        )
    }

    #[cfg(test)]
    fn spawn(
        config: transport::Config,
        caption: impl FnMut(Caption) + Send + 'static,
    ) -> (Self, audio::AudioFeed) {
        Self::spawn_cancellable(config, caption, Arc::default())
    }

    fn spawn_cancellable(
        mut config: transport::Config,
        mut caption: impl FnMut(Caption) + Send + 'static,
        cancelled: Arc<AtomicBool>,
    ) -> (Self, audio::AudioFeed) {
        config.cancelled = cancelled.clone();
        config.started = tokio::time::Instant::now();
        config.not_before = config.started + crate::flow::MIN_HOLD;
        let metrics = config.metrics.clone();
        let (feed, receiver, failed) = audio::channel();
        let (send, result) = mpsc::channel();
        let task = tauri::async_runtime::spawn(async move {
            let model = config.model.clone();
            let result = tokio::select! {
                biased;
                _ = async {
                    loop {
                        if cancelled.load(Ordering::Acquire) { break; }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                } => return,
                result = transport::transcribe(config, receiver, failed, &mut caption) => result,
            }
            .map(|text| {
                let metrics = metrics.lock().unwrap_or_else(|e| e.into_inner());
                Streamed {
                    text,
                    model,
                    setup_ms: metrics.setup_ms,
                    first_caption_ms: metrics.first_caption_ms,
                }
            });
            if result.is_err() {
                caption(Caption::default());
            }
            let _ = send.send(result);
        });
        let session = Self {
            result,
            task: Some(task),
            stopped: Arc::default(),
            finish_timeout: CLOUD_FINISH,
        };
        (session, feed)
    }

    fn start_local(
        settings: &Settings,
        caption: impl FnMut(Caption) + Send + 'static,
        cancelled: Arc<AtomicBool>,
    ) -> Option<(Self, audio::AudioFeed)> {
        use crate::provider::{parakeet::Parakeet, Availability, SttProvider};
        // Without live dictation or a model, the batch path reports the reason.
        if !settings.streaming_enabled || Parakeet.availability(settings) != Availability::Available
        {
            return None;
        }
        let (feed, receiver, failed) = audio::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let local = local::Local {
            settings: settings.clone(),
            model: crate::flow::batch_model(crate::models::ID, settings).unwrap_or_default(),
            failed,
            stopped: stopped.clone(),
            cancelled,
        };
        let result = local::spawn(local, receiver, caption);
        let session = Self {
            result,
            task: None,
            stopped,
            finish_timeout: LOCAL_FINISH,
        };
        Some((session, feed))
    }

    /// Called after recorder.stop has closed the audio feed. A missing final
    /// never holds delivery indefinitely: the caller falls back to batch.
    pub fn finish(self) -> Result<Streamed, ProviderError> {
        self.result
            .recv_timeout(self.finish_timeout)
            .unwrap_or(Err(ProviderError::Timeout))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a recording through a real local session at microphone pace (100 ms
    /// packets at 48 kHz) and reports what key-up costs. Skips without
    /// PARLA_PARAKEET_MODEL_DIR; PARLA_LIVE_CLIP picks the WAV.
    #[test]
    fn local_session_end_to_end_when_model_is_provided() {
        let Ok(dir) = std::env::var("PARLA_PARAKEET_MODEL_DIR") else {
            return;
        };
        let mut settings = Settings {
            active_provider: crate::models::ID.into(),
            ..Default::default()
        };
        settings
            .providers
            .insert("parakeet".into(), serde_json::json!({"model_dir":dir}));
        let bytes = match std::env::var("PARLA_LIVE_CLIP") {
            Ok(path) => std::fs::read(path).unwrap(),
            Err(_) => include_bytes!("../../tests/fixtures/hello.wav").to_vec(),
        };
        let clip = crate::clip::Clip::from_wav_bytes(&bytes).unwrap();
        // 16 kHz to 48 kHz by repetition is crude but keeps speech intelligible.
        let device: Vec<f32> = clip
            .samples
            .iter()
            .flat_map(|&s| [s as f32 / 32768.0; 3])
            .collect();
        let captions = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = captions.clone();
        let (session, mut feed) = Session::start(
            &settings,
            move |caption: Caption| seen.lock().unwrap().push(caption.stable),
            Arc::default(),
        )
        .unwrap()
        .unwrap();
        for packet in device.chunks(4_800) {
            feed.push(packet, 48_000);
            std::thread::sleep(Duration::from_millis(100));
        }
        let key_up = std::time::Instant::now();
        drop(feed);
        let streamed = session.finish().unwrap();
        println!(
            "key-up to text: {} ms, {} captions\n{}",
            key_up.elapsed().as_millis(),
            captions.lock().unwrap().len(),
            streamed.text
        );
        assert!(!streamed.text.trim().is_empty());
    }

    #[tokio::test]
    async fn cancellation_interrupts_stalled_handshake() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let (session, _feed) = Session::test_session(
            format!("ws://{}", listener.local_addr().unwrap()),
            flag.clone(),
        );
        let (mut tcp, _) = tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = [0; 4096];
        assert!(tcp.read(&mut bytes).await.unwrap() > 0);
        flag.store(true, Ordering::Release);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(200), tcp.read(&mut bytes))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(session.finish().is_err());
    }

    #[tokio::test]
    async fn setup_connects_during_grace_but_sends_no_audio() {
        use futures_util::{SinkExt, StreamExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = Settings::default();
        settings
            .providers
            .insert("openai".into(), serde_json::json!({"api_key":"test"}));
        let mut config = transport::Config::from_settings(&settings)
            .unwrap()
            .unwrap();
        config.endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (session, mut feed) = Session::spawn(config, |_| panic!("no caption expected"));
        feed.push(&vec![0.25; 1600], 16000);
        let (tcp, _) = tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .expect("connection must overlap grace")
            .unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"type":"session.updated"}"#.into(),
            ))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(40), socket.next())
                .await
                .is_err()
        );
        drop(session);
        drop(feed);
        let closed = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .unwrap();
        assert!(closed.is_none() || closed.unwrap().is_err());
    }

    #[tokio::test]
    async fn cancelling_connected_session_closes_socket_without_captions() {
        use futures_util::StreamExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = Settings::default();
        settings
            .providers
            .insert("openai".into(), serde_json::json!({"api_key":"test"}));
        let mut config = transport::Config::from_settings(&settings)
            .unwrap()
            .unwrap();
        config.endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (session, feed) =
            Session::spawn(config, |_| panic!("cancelled session emitted captions"));
        let (tcp, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        assert!(socket.next().await.unwrap().is_ok()); // session setup, awaiting readiness
        drop(session);
        drop(feed);
        let closed = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap();
        assert!(closed.is_none() || closed.unwrap().is_err());
    }
}
