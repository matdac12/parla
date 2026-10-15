//! Provider listing for the settings UI.
//!
//! Availability is evaluated against the current settings on every call, so a
//! key typed in the form enables its provider without a restart. The list is
//! built from `provider::registry::all()`, so providers added by other tickets
//! (xAI, Parakeet) appear automatically.
use crate::{
    provider::{redact_secrets, registry, Availability, SttProvider},
    settings::{Settings, SharedSettings},
};
use serde::Serialize;
use tauri::Manager;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub available: bool,
    pub reason: Option<String>,
}

/// Pure mapping so the IPC shape is unit-testable without a Tauri app.
/// Unavailable reasons may embed provider diagnostics, so they are redacted
/// the same way provider errors are before reaching the webview.
fn describe(provider: &dyn SttProvider, settings: &Settings) -> ProviderInfo {
    let (available, reason) = match provider.availability(settings) {
        Availability::Available => (true, None),
        Availability::Unavailable(reason) => (false, Some(redact_secrets(&reason))),
    };
    ProviderInfo {
        id: provider.id().to_string(),
        display_name: provider.display_name().to_string(),
        available,
        reason,
    }
}

#[tauri::command]
pub async fn list_providers(app: tauri::AppHandle) -> Result<Vec<ProviderInfo>, String> {
    // Keep settings access off the main thread, like get_settings.
    tauri::async_runtime::spawn_blocking(move || {
        let settings = app
            .state::<SharedSettings>()
            .read()
            .map_err(|_| "Settings lock poisoned".to_string())?
            .clone();
        Ok(registry::all()
            .iter()
            .map(|provider| describe(provider.as_ref(), &settings))
            .collect())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clip::Clip,
        provider::{Availability, ProviderError, SttProvider},
    };

    struct Fake {
        id: &'static str,
        name: &'static str,
        availability: Availability,
    }
    impl SttProvider for Fake {
        fn id(&self) -> &'static str {
            self.id
        }
        fn display_name(&self) -> &'static str {
            self.name
        }
        fn availability(&self, _: &Settings) -> Availability {
            self.availability.clone()
        }
        fn transcribe(&self, _: &Clip, _: &Settings) -> Result<String, ProviderError> {
            Ok(String::new())
        }
    }

    #[test]
    fn available_provider_has_no_reason() {
        let info = describe(
            &Fake {
                id: "a",
                name: "A",
                availability: Availability::Available,
            },
            &Settings::default(),
        );
        assert!(info.available);
        assert_eq!(info.reason, None);
        assert_eq!(info.id, "a");
        assert_eq!(info.display_name, "A");
    }

    #[test]
    fn unavailable_provider_keeps_its_reason_so_the_ui_can_disable_it() {
        let info = describe(
            &Fake {
                id: "b",
                name: "B",
                availability: Availability::Unavailable("missing API key".into()),
            },
            &Settings::default(),
        );
        assert!(!info.available);
        assert_eq!(info.reason.as_deref(), Some("missing API key"));
    }

    #[test]
    fn reasons_are_redacted_and_the_json_shape_is_stable() {
        let info = describe(
            &Fake {
                id: "c",
                name: "C",
                availability: Availability::Unavailable("key sk-secret123 rejected".into()),
            },
            &Settings::default(),
        );
        assert_eq!(info.reason.as_deref(), Some("key [redacted] rejected"));
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            serde_json::json!({
                "id": "c",
                "display_name": "C",
                "available": false,
                "reason": "key [redacted] rejected",
            })
        );
    }

    #[test]
    fn every_registry_provider_is_described_with_a_unique_id_and_name() {
        let settings = Settings::default();
        let infos: Vec<_> = registry::all()
            .iter()
            .map(|provider| describe(provider.as_ref(), &settings))
            .collect();
        let mut ids: Vec<_> = infos.iter().map(|info| info.id.clone()).collect();
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count, "provider ids must be unique");
        assert!(infos.iter().all(|info| !info.display_name.is_empty()));
    }
}
