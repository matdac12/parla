//! Cached CPU inference, following Handy (MIT, Copyright (c) 2025 CJ Pais):
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/managers/transcription.rs
use super::{Availability, ProviderError, SttProvider};
use crate::{
    clip::{Clip, SAMPLE_RATE},
    models,
    settings::{LocalModel, Settings},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::Duration,
};
use transcribe_rs::onnx::{
    parakeet::{ParakeetModel, ParakeetParams, TimestampGranularity},
    Quantization,
};

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ParakeetConfig {
    /// Overrides the Parakeet v3 folder.
    pub model_dir: Option<PathBuf>,
    /// Overrides the Parakeet Ultra folder (tests and the comparison example).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ultra_model_dir: Option<PathBuf>,
}

struct CachedModel {
    /// The model and directory as configured, so a phrase with unchanged
    /// settings skips the file checks and `canonicalize` below.
    requested: (LocalModel, PathBuf),
    dir: PathBuf,
    model: ParakeetModel,
}
fn engine() -> &'static Mutex<Option<CachedModel>> {
    static ENGINE: OnceLock<Mutex<Option<CachedModel>>> = OnceLock::new();
    ENGINE.get_or_init(|| Mutex::new(None))
}
fn with_model<T>(
    settings: &Settings,
    use_model: impl FnOnce(&mut ParakeetModel) -> Result<T, ProviderError>,
) -> Result<T, ProviderError> {
    let requested = (
        settings.local_model,
        models::model_dir(settings).map_err(ProviderError::MissingConfig)?,
    );
    let mut cached = engine()
        .lock()
        .map_err(|_| ProviderError::Other("Parakeet model lock poisoned".into()))?;
    // Registry lookups create new provider objects. The engine therefore lives
    // at process scope, and settings changes reload only when the directory
    // changes, which includes switching between v3 and Ultra.
    if cached
        .as_ref()
        .is_none_or(|entry| entry.requested != requested)
    {
        let (model_kind, dir) = &requested;
        if !models::available(*model_kind, dir) {
            return Err(ProviderError::MissingConfig("model not downloaded".into()));
        }
        let dir = dir
            .canonicalize()
            .map_err(|e| ProviderError::Other(e.to_string()))?;
        if cached.as_ref().is_none_or(|entry| entry.dir != dir) {
            let model = load(&dir)?;
            *cached = Some(CachedModel {
                requested,
                dir,
                model,
            });
        } else if let Some(entry) = cached.as_mut() {
            entry.requested = requested;
        }
    }
    let result = use_model(&mut cached.as_mut().expect("loaded model").model);
    drop(cached);
    let use_id = USES.fetch_add(1, Ordering::AcqRel) + 1;
    if settings.active_provider != models::ID {
        release_when_idle(use_id);
    }
    result
}

/// Bumped by every model use, so a delayed release never drops a model that
/// was used again in the meantime.
static USES: AtomicU64 = AtomicU64::new(0);
/// How long a non-active model (e.g. loaded by the bench) stays resident.
const IDLE_RELEASE: Duration = Duration::from_secs(60);

fn release_when_idle(use_id: u64) {
    let _ = std::thread::Builder::new()
        .name("parla-parakeet-release".into())
        .spawn(move || {
            std::thread::sleep(IDLE_RELEASE);
            if USES.load(Ordering::Acquire) == use_id {
                release();
            }
        });
}

/// Switches models: frees the old one first, so v3 and Ultra (~700 MB each)
/// are never resident together, then warms the selected one if Parakeet is active.
pub fn reload(settings: &Settings) {
    release();
    sync_residency(settings);
}

/// Drops the cached model (~700 MB resident) until it is next needed.
pub fn release() {
    if let Ok(mut cached) = engine().lock() {
        *cached = None;
    }
}

/// Keeps the model resident only while Parakeet is the active provider: warms
/// it in the background when selected, frees it when another provider is.
pub fn sync_residency(settings: &Settings) {
    if settings.active_provider == models::ID {
        let settings = settings.clone();
        let _ = std::thread::Builder::new()
            .name("parla-parakeet-warm".into())
            .spawn(move || {
                if let Err(error) = warm(&settings) {
                    eprintln!("Parakeet warm-up: {error}");
                }
            });
    } else {
        release();
    }
}
fn load(dir: &Path) -> Result<ParakeetModel, ProviderError> {
    ParakeetModel::load(dir, &Quantization::Int8)
        .map_err(|e| ProviderError::Other(format!("Loading Parakeet failed: {e}")))
}
pub fn warm(settings: &Settings) -> Result<(), ProviderError> {
    with_model(settings, |_| Ok(()))
}

/// Half a second of silence: enough to run every session, ~70 ms of work.
const WAKE_SAMPLES: usize = SAMPLE_RATE as usize / 2;

/// Runs a tiny inference so the first real one finds the CPU clocked up and the
/// model's pages resident. Measured on an i7-1370P after 60 s idle, a 12.7 s
/// clip took 1.7-8.3 s cold and a steady ~0.67 s after this.
pub fn wake(settings: &Settings) -> Result<(), ProviderError> {
    transcribe_samples(settings, &[0.0; WAKE_SAMPLES]).map(drop)
}

/// Wakes the model on a background thread when a recording starts; the user is
/// still speaking for seconds, which hides the cost.
pub fn wake_in_background(settings: &Settings) {
    let settings = settings.clone();
    let _ = std::thread::Builder::new()
        .name("parla-parakeet-wake".into())
        .spawn(move || {
            if let Err(error) = wake(&settings) {
                eprintln!("Parakeet wake-up: {error}");
            }
        });
}

/// Transcribes 16 kHz mono samples in [-1, 1].
pub fn transcribe_samples(settings: &Settings, audio: &[f32]) -> Result<String, ProviderError> {
    with_model(settings, |model| {
        model
            .transcribe_with(audio, &ParakeetParams::default())
            .map(|r| r.text)
            .map_err(|e| ProviderError::Other(format!("Parakeet transcription failed: {e}")))
    })
}

/// Transcribes `audio` heard right after `context` (audio already transcribed)
/// and returns only the words spoken in `audio`. The context lets the model
/// see a sentence continue across a cut, so it neither capitalizes the next
/// word nor guesses the language from a fragment.
pub fn transcribe_after(
    settings: &Settings,
    context: &[f32],
    audio: &[f32],
) -> Result<String, ProviderError> {
    if context.is_empty() {
        return transcribe_samples(settings, audio);
    }
    let joined = [context, audio].concat();
    // Cuts sit inside pauses with 240 ms of silence on each side, so context
    // words end well before the boundary. Onset tokens can be stamped a frame
    // or two early (80 ms steps), hence the cutoff sits two frames before it.
    let boundary = context.len() as f32 / SAMPLE_RATE as f32 - 0.16;
    let params = ParakeetParams {
        timestamp_granularity: Some(TimestampGranularity::Token),
        ..Default::default()
    };
    with_model(settings, |model| {
        let result = model
            .transcribe_with(&joined, &params)
            .map_err(|e| ProviderError::Other(format!("Parakeet transcription failed: {e}")))?;
        let text: String = result
            .segments
            .unwrap_or_default()
            .into_iter()
            .filter(|token| token.start >= boundary)
            .map(|token| token.text)
            .collect();
        Ok(text.split_whitespace().collect::<Vec<_>>().join(" "))
    })
}

pub struct Parakeet;
impl SttProvider for Parakeet {
    fn id(&self) -> &'static str {
        "parakeet"
    }
    fn display_name(&self) -> &'static str {
        "Parakeet (local)"
    }
    fn availability(&self, settings: &Settings) -> Availability {
        if models::model_dir(settings)
            .is_ok_and(|dir| models::available(settings.local_model, &dir))
        {
            Availability::Available
        } else {
            Availability::Unavailable("model not downloaded".into())
        }
    }
    fn transcribe(&self, clip: &Clip, settings: &Settings) -> Result<String, ProviderError> {
        if clip.sample_rate != SAMPLE_RATE || clip.samples.is_empty() {
            return Err(ProviderError::Other(
                "Expected nonempty 16 kHz mono audio".into(),
            ));
        }
        let audio: Vec<f32> = clip.samples.iter().map(|&s| s as f32 / 32768.0).collect();
        // Inference cost grows with length, so silence is trimmed first. When no
        // speech is detected (a very quiet mic), the whole clip is used.
        let speech = crate::segment::compress_silence(&audio);
        transcribe_samples(settings, speech.as_deref().unwrap_or(&audio))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_defaults_to_app_data_model() {
        let config: ParakeetConfig = Settings::default().provider_config("parakeet");
        assert!(config.model_dir.is_none());
        assert_eq!(
            serde_json::to_value(config).unwrap(),
            serde_json::json!({"model_dir":null})
        );
    }
    #[test]
    fn availability_requires_every_complete_model_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.providers.insert(
            "parakeet".into(),
            serde_json::json!({"model_dir":dir.path()}),
        );
        assert_eq!(
            Parakeet.availability(&settings),
            Availability::Unavailable("model not downloaded".into())
        );
        for file in crate::models::V3.files {
            std::fs::write(dir.path().join(file.name), b"partial").unwrap();
        }
        assert!(matches!(
            Parakeet.availability(&settings),
            Availability::Unavailable(_)
        ));
        for file in crate::models::V3.files {
            // Sparse files test the size check without allocating a model on disk.
            std::fs::File::create(dir.path().join(file.name))
                .unwrap()
                .set_len(file.size)
                .unwrap();
        }
        assert_eq!(Parakeet.availability(&settings), Availability::Available);
        // Ultra has its own folder and file sizes, so the v3 files do not count.
        let ultra = Settings {
            local_model: crate::settings::LocalModel::ParakeetUltra,
            ..settings.clone()
        };
        assert!(matches!(
            Parakeet.availability(&ultra),
            Availability::Unavailable(_)
        ));
        std::fs::write(dir.path().join(crate::models::V3.files[0].name), b"").unwrap();
        assert!(matches!(
            Parakeet.availability(&settings),
            Availability::Unavailable(_)
        ));
    }
    #[test]
    fn missing_model_is_reported_before_any_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.providers.insert(
            "parakeet".into(),
            serde_json::json!({"model_dir":dir.path()}),
        );
        assert!(matches!(
            warm(&settings),
            Err(ProviderError::MissingConfig(_))
        ));
    }
    #[test]
    fn live_fixture_when_model_dir_is_provided() {
        use crate::settings::LocalModel;
        for (variable, model, key) in [
            (
                "PARLA_PARAKEET_MODEL_DIR",
                LocalModel::ParakeetV3,
                "model_dir",
            ),
            (
                "PARLA_PARAKEET_ULTRA_MODEL_DIR",
                LocalModel::ParakeetUltra,
                "ultra_model_dir",
            ),
        ] {
            let Ok(dir) = std::env::var(variable) else {
                continue;
            };
            let mut settings = Settings {
                local_model: model,
                ..Settings::default()
            };
            settings
                .providers
                .insert("parakeet".into(), serde_json::json!({ key: dir }));
            let bytes = include_bytes!("../../tests/fixtures/hello.wav");
            let clip = Clip::from_wav_bytes(bytes).unwrap();
            assert!(!Parakeet
                .transcribe(&clip, &settings)
                .unwrap()
                .trim()
                .is_empty());
        }
    }
}
