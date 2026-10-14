//! PARLA-9 owns this seam: add notifications and retry without changing flow.
use crate::{clip::Clip, provider::ProviderError};
use std::sync::Mutex;
use tauri::Manager;

pub struct FailedDictation {
    pub clip: Clip,
    pub error: ProviderError,
}
pub type LastFailedClip = Mutex<Option<FailedDictation>>;

pub fn on_failure(app: &tauri::AppHandle, error: ProviderError, clip: Clip) {
    eprintln!("Dictation failed: {error}");
    *app.state::<LastFailedClip>()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(FailedDictation { clip, error });
}
