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
