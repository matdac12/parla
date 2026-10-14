//! Shared persisted settings. Provider sections remain owned by their providers.
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tauri::Manager;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub active_provider: String,
    pub hotkey: String,
    pub restore_delay_ms: u64,
    pub providers: BTreeMap<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            active_provider: "openai".into(),
            hotkey: crate::hotkey::DEFAULT_SHORTCUT.into(),
            restore_delay_ms: crate::inserter::DEFAULT_RESTORE_DELAY_MS,
            providers: BTreeMap::new(),
        }
    }
}

pub type SharedSettings = Arc<RwLock<Settings>>;

impl Settings {
    /// Missing or malformed sections get provider defaults; unknown sections
    /// are preserved when saving. Providers validate required fields themselves.
    pub fn provider_config<T: DeserializeOwned + Default>(&self, id: &str) -> T {
        self.providers
            .get(id)
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }
}

pub fn path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|e| e.to_string())
}

pub fn load(path: &Path) -> Result<Settings, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| "Invalid settings JSON; existing file preserved".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(format!("Reading settings failed: {error}")),
    }
}

/// Write then replace, so an interrupted write does not truncate settings.json.
/// Callers serialize saves using the shared settings write lock.
pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    use std::io::Write;
    let parent = path.parent().ok_or("Settings path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("json.tmp");
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec_pretty(settings).map_err(|_| "Encoding settings failed")?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&temporary, path).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// Must be called on a blocking worker: Hotkey::set_shortcut waits for the main
/// thread. Delay changes take effect through the next flow settings snapshot.
pub fn apply(app: &tauri::AppHandle, settings: &Settings) -> Result<(), String> {
    let hotkey = app
        .try_state::<crate::hotkey::Hotkey>()
        .ok_or("Hotkey listener is unavailable; restart Parla")?;
    hotkey.set_shortcut(&settings.hotkey)
}
