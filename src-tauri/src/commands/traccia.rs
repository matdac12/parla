use crate::command_mode::traccia::{self, TracciaConfig};

/// Test connection's result: the summary line, and a warning when the token
/// writes as an agent instead of as Mattia.
#[derive(Debug, serde::Serialize)]
pub struct TracciaCheck {
    pub message: String,
    pub warning: Option<String>,
}

/// Settings → Test connection, with the URL and token as typed (saved or not).
/// Rejects with the failure's message (spec §5). Never logs the token.
#[tauri::command]
pub async fn test_traccia(base_url: String, token: String) -> Result<TracciaCheck, String> {
    tauri::async_runtime::spawn_blocking(move || test_connection(&base_url, &token))
        .await
        .map_err(|e| e.to_string())?
}

fn test_connection(base_url: &str, token: &str) -> Result<TracciaCheck, String> {
    let config = TracciaConfig::new(base_url, token).map_err(|e| e.to_string())?;
    let connection = traccia::check(&config).map_err(|e| e.to_string())?;
    Ok(TracciaCheck {
        message: connection.summary(),
        warning: connection.warning(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_bad_connection_fails_before_any_request() {
        assert_eq!(
            test_connection("", "test-token").unwrap_err(),
            "Add the Traccia URL and token first."
        );
        assert_eq!(
            test_connection("traccia.example.ts.net", "test-token").unwrap_err(),
            "The Traccia URL must start with http:// or https://."
        );
    }

    /// The Test connection command's path, minus the IPC, against the real
    /// Traccia: `cargo test --locked --lib test_traccia_live -- --ignored --nocapture`.
    /// The connection comes as in `tests/traccia_live.rs`; nothing is printed
    /// but the result.
    #[test]
    #[ignore = "calls the real Traccia over the tailnet"]
    fn test_traccia_live() {
        let saved = std::env::var_os("APPDATA")
            .map(|dir| std::path::Path::new(&dir).join("com.matdac6.parla/settings.json"))
            .and_then(|path| crate::settings::load(&path).ok())
            .unwrap_or_default();
        let Ok(config) = TracciaConfig::resolved(&saved) else {
            println!("Skipping: no TRACCIA_URL/TRACCIA_TOKEN and no providers.traccia");
            return;
        };
        let check = test_connection(&config.base_url, &config.token).unwrap();
        println!("{} (warning: {:?})", check.message, check.warning);
        assert!(check.message.starts_with("Connected as you · token "));
        assert_eq!(check.warning, None);
    }

    #[test]
    fn the_result_shape_is_documented_json() {
        assert_eq!(
            serde_json::to_value(TracciaCheck {
                message: "Connected as you · token parla · 3 projects".into(),
                warning: None,
            })
            .unwrap(),
            serde_json::json!({
                "message": "Connected as you · token parla · 3 projects",
                "warning": null,
            })
        );
    }
}
