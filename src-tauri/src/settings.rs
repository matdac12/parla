//! Shared persisted settings. Provider sections remain owned by their providers.
use crate::hotkey::CommandChord;
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
    pub paste_compatibility_mode: bool,
    /// Press Enter after pasting a dictation that ends in "send"/"invia" (PARLA-58).
    pub auto_send: bool,
    pub streaming_enabled: bool,
    pub live_captions: bool,
    pub vocabulary: Vec<String>,
    /// Which Parakeet export the local provider runs. Missing means v3.
    pub local_model: LocalModel,
    /// Lower other apps' audio (music) while dictating (PARLA-42).
    pub dim_other_audio: bool,
    /// Percent of each app's volume kept while dimmed.
    pub dim_audio_level: u8,
    /// Command mode (PARLA-61), off by default and the only LLM path (ADR-0001).
    pub command: CommandSettings,
    pub providers: BTreeMap<String, serde_json::Value>,
}

/// The `command` section, edited in the Settings "Command mode" card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandSettings {
    pub enabled: bool,
    /// The command hotkey: a hold of 2+ modifiers (`hotkey::parse_command_hotkey`).
    /// Kept as typed, so the default stays byte-identical in settings.json.
    pub hotkey: String,
    pub model: String,
    /// Provisional until the PARLA-68 eval picks `none` or `low`.
    pub reasoning_effort: String,
}

pub const DEFAULT_COMMAND_HOTKEY: &str = "LCtrl+Shift";

impl Default for CommandSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            hotkey: DEFAULT_COMMAND_HOTKEY.into(),
            model: "gpt-6-luna".into(),
            reasoning_effort: "low".into(),
        }
    }
}

/// The local Parakeet exports Parla can run on the same engine (see models.rs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalModel {
    #[default]
    ParakeetV3,
    ParakeetUltra,
}

/// An unknown value (e.g. written by a newer Parla) means v3, so the rest of
/// settings.json still loads.
impl<'de> Deserialize<'de> for LocalModel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(value.as_str().and_then(Self::from_id).unwrap_or_default())
    }
}

impl LocalModel {
    pub const ALL: [Self; 2] = [Self::ParakeetV3, Self::ParakeetUltra];
    pub fn id(self) -> &'static str {
        match self {
            Self::ParakeetV3 => "parakeet-v3",
            Self::ParakeetUltra => "parakeet-ultra",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|model| model.id() == id)
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            active_provider: "openai".into(),
            hotkey: crate::hotkey::DEFAULT_SHORTCUT.into(),
            restore_delay_ms: crate::inserter::DEFAULT_RESTORE_DELAY_MS,
            paste_compatibility_mode: false,
            auto_send: true,
            streaming_enabled: true,
            live_captions: true,
            vocabulary: Vec::new(),
            local_model: LocalModel::default(),
            dim_other_audio: true,
            dim_audio_level: crate::dim::DEFAULT_LEVEL_PERCENT,
            command: CommandSettings::default(),
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
/// Callers serialize saves using commands::settings::SaveLock.
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

/// The chord to arm: `None` while command mode is off, so a command hotkey
/// that is off can never block saving the rest of the settings.
pub fn command_chord(settings: &Settings) -> Result<Option<CommandChord>, String> {
    if !settings.command.enabled {
        return Ok(None);
    }
    crate::hotkey::parse_command_hotkey(&settings.command.hotkey, &settings.hotkey).map(Some)
}

/// Must be called on a blocking worker: Hotkey::set_shortcut waits for the main
/// thread. Delay changes take effect through the next flow settings snapshot.
/// The command chord was checked against `settings.hotkey`, so it changes only
/// once that dictation hotkey is bound: a failed rebind keeps both old ones.
/// An invalid command hotkey leaves command mode disarmed, never dictation.
pub fn apply(app: &tauri::AppHandle, settings: &Settings) -> Result<(), String> {
    let hotkey = app
        .try_state::<crate::hotkey::Hotkey>()
        .ok_or("Hotkey listener is unavailable; restart Parla")?;
    hotkey.set_shortcut(&settings.hotkey)?;
    match command_chord(settings) {
        Ok(chord) => {
            hotkey.set_command_chord(chord);
            Ok(())
        }
        Err(error) => {
            hotkey.set_command_chord(None);
            Err(error)
        }
    }
}

#[cfg(test)]
mod vocabulary_tests {
    use super::*;
    #[test]
    fn old_settings_get_empty_user_vocabulary_and_preserve_provider() {
        let s: Settings =
            serde_json::from_str(r#"{"providers":{"future":{"api_key":"test"}}}"#).unwrap();
        assert_eq!(
            serde_json::to_value(&s).unwrap()["vocabulary"],
            serde_json::json!([])
        );
        assert_eq!(s.providers["future"]["api_key"], "test");
        assert!(s.dim_other_audio, "dimming is on for existing users");
        assert!(s.auto_send, "auto-send is on for existing users");
        assert_eq!(s.dim_audio_level, crate::dim::DEFAULT_LEVEL_PERCENT);
    }
    #[test]
    fn old_settings_load_command_mode_off_with_its_defaults() {
        let old: Settings = serde_json::from_str(r#"{"hotkey":"Ctrl+Alt+Space"}"#).unwrap();
        let json = serde_json::to_value(&old).unwrap();
        assert_eq!(
            json["command"],
            serde_json::json!({
                "enabled": false,
                "hotkey": "LCtrl+Shift",
                "model": "gpt-6-luna",
                "reasoning_effort": "low",
            })
        );
        let partial: Settings = serde_json::from_str(r#"{"command":{"enabled":true}}"#).unwrap();
        assert!(partial.command.enabled);
        assert_eq!(partial.command.model, "gpt-6-luna");
    }
    #[test]
    fn local_model_defaults_to_v3_and_round_trips() {
        let old: Settings = serde_json::from_str(r#"{"active_provider":"parakeet"}"#).unwrap();
        assert_eq!(old.local_model, LocalModel::ParakeetV3);
        for unknown in [r#""parakeet-next""#, "null", "3"] {
            let future: Settings =
                serde_json::from_str(&format!(r#"{{"local_model":{unknown},"hotkey":"F9"}}"#))
                    .unwrap();
            assert_eq!(future.local_model, LocalModel::ParakeetV3);
            assert_eq!(future.hotkey, "F9");
        }
        for model in LocalModel::ALL {
            let json = serde_json::to_value(Settings {
                local_model: model,
                ..Settings::default()
            })
            .unwrap();
            assert_eq!(json["local_model"], model.id());
            let back: Settings = serde_json::from_value(json).unwrap();
            assert_eq!(back.local_model, model);
            assert_eq!(LocalModel::from_id(model.id()), Some(model));
        }
        assert_eq!(LocalModel::from_id("parakeet"), None);
    }
}

/// Shared conservative native-provider limit. Never include user terms in errors.
pub fn validate_vocabulary(terms: &[String]) -> Result<(), String> {
    if terms.len() > 100 {
        return Err("Vocabulary supports at most 100 entries".into());
    }
    if terms.iter().any(|term| {
        term.trim().is_empty()
            || term.chars().count() > 50
            || term.chars().any(|c| c.is_control() || c == '<' || c == '>')
    }) {
        return Err("Each vocabulary entry must contain 1-50 characters, without control characters or angle brackets".into());
    }
    Ok(())
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    #[test]
    fn vocabulary_rejects_limits_and_unsafe_characters_without_echoing_terms() {
        for term in ["", " ", "<unsafe>", "a\rb", "a\nb", "a\0b", "a\tb"] {
            assert!(validate_vocabulary(&[term.into()]).is_err());
        }
        assert!(validate_vocabulary(&vec!["term".into(); 101]).is_err());
        assert!(validate_vocabulary(&["x".repeat(51)]).is_err());
        assert!(validate_vocabulary(&["è".repeat(50), "API & tools".into()]).is_ok());
    }
}
