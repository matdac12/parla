use parla_lib::{
    provider::{registry, Availability},
    settings::Settings,
};

#[test]
fn old_settings_load_defaults_and_preserve_provider_sections() {
    let settings: Settings =
        serde_json::from_str(r#"{"providers":{"future":{"model":"custom"}}}"#).unwrap();
    assert_eq!(settings.active_provider, "openai");
    assert_eq!(settings.hotkey, "Ctrl+Space");
    assert_eq!(settings.restore_delay_ms, 250);
    let round_trip = serde_json::to_value(settings).unwrap();
    assert_eq!(round_trip["providers"]["future"]["model"], "custom");
}

#[test]
fn registry_can_resolve_every_advertised_provider() {
    for provider in registry::all() {
        assert!(!provider.display_name().is_empty());
        assert_eq!(registry::get(provider.id()).unwrap().id(), provider.id());
    }
    assert!(registry::get("unknown").is_none());
}

#[test]
fn missing_key_is_unavailable() {
    // This test process never mutates the environment; skip when a real key exists.
    if std::env::var_os("OPENAI_API_KEY").is_none() {
        assert!(matches!(
            registry::get("openai")
                .unwrap()
                .availability(&Settings::default()),
            Availability::Unavailable(_)
        ));
    }
}

#[test]
fn settings_save_replaces_existing_file_and_loads_it() {
    let dir = std::env::temp_dir().join(format!("parla-settings-test-{}", std::process::id()));
    let path = dir.join("settings.json");
    let mut settings = Settings::default();
    parla_lib::settings::save(&path, &settings).unwrap();
    settings.active_provider = "future".into();
    settings
        .providers
        .insert("future".into(), serde_json::json!({"model": "custom"}));
    parla_lib::settings::save(&path, &settings).unwrap();
    assert_eq!(
        parla_lib::settings::load(&path).unwrap().active_provider,
        "future"
    );
    assert!(!path.with_extension("json.tmp").exists());
    std::fs::write(&path, "malformed").unwrap();
    assert!(parla_lib::settings::load(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "malformed");
    std::fs::remove_dir_all(dir).unwrap();
}
