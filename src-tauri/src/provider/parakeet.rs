//! Cached CPU inference, following Handy (MIT, Copyright (c) 2025 CJ Pais):
//! https://github.com/cjpais/Handy/blob/main/src-tauri/src/managers/transcription.rs
use super::{Availability, ProviderError, SttProvider};
use crate::{
    clip::{Clip, SAMPLE_RATE},
    models,
    settings::Settings,
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
    parakeet::{ParakeetModel, ParakeetParams},
    Quantization,
};

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ParakeetConfig {
    pub model_dir: Option<PathBuf>,
}

struct CachedModel {
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
    let dir = models::model_dir(settings).map_err(ProviderError::MissingConfig)?;
    if !models::available(&dir) {
        return Err(ProviderError::MissingConfig("model not downloaded".into()));
    }
    let dir = dir
        .canonicalize()
        .map_err(|e| ProviderError::Other(e.to_string()))?;
    let mut cached = engine()
        .lock()
        .map_err(|_| ProviderError::Other("Parakeet model lock poisoned".into()))?;
    // Registry lookups create new provider objects. The engine therefore lives
    // at process scope, and settings changes reload only when the directory changes.
    if cached.as_ref().is_none_or(|entry| entry.dir != dir) {
        let model = load(&dir)?;
        *cached = Some(CachedModel { dir, model });
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

pub struct Parakeet;
impl SttProvider for Parakeet {
    fn id(&self) -> &'static str {
        "parakeet"
    }
    fn display_name(&self) -> &'static str {
        "Parakeet V3 (local)"
    }
    fn availability(&self, settings: &Settings) -> Availability {
        if models::model_dir(settings).is_ok_and(|dir| models::available(&dir)) {
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
        with_model(settings, |model| {
            model
                .transcribe_with(&audio, &ParakeetParams::default())
                .map(|r| r.text)
                .map_err(|e| ProviderError::Other(format!("Parakeet transcription failed: {e}")))
        })
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
        for file in crate::models::FILES {
            std::fs::write(dir.path().join(file.name), b"partial").unwrap();
        }
        assert!(matches!(
            Parakeet.availability(&settings),
            Availability::Unavailable(_)
        ));
        for file in crate::models::FILES {
            // Sparse files test the size check without allocating a model on disk.
            std::fs::File::create(dir.path().join(file.name))
                .unwrap()
                .set_len(file.size)
                .unwrap();
        }
        assert_eq!(Parakeet.availability(&settings), Availability::Available);
        std::fs::write(dir.path().join(crate::models::FILES[0].name), b"").unwrap();
        assert!(matches!(
            Parakeet.availability(&settings),
            Availability::Unavailable(_)
        ));
    }
    #[test]
    fn live_fixture_when_model_dir_is_provided() {
        let Ok(dir) = std::env::var("PARLA_PARAKEET_MODEL_DIR") else {
            return;
        };
        let mut settings = Settings::default();
        settings
            .providers
            .insert("parakeet".into(), serde_json::json!({"model_dir":dir}));
        let bytes = include_bytes!("../../tests/fixtures/hello.wav");
        let clip = Clip::from_wav_bytes(bytes).unwrap();
        assert!(!Parakeet
            .transcribe(&clip, &settings)
            .unwrap()
            .trim()
            .is_empty());
    }
}
