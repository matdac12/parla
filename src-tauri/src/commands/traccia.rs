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
            "The Traccia URL must start with http:// or https:// and have no ? or #."
        );
    }

    // Live checks of the Test connection command's path, minus the IPC:
    // `cargo test --locked --lib commands::traccia -- --ignored --nocapture`.
    // The connection comes from `TRACCIA_URL` and `TRACCIA_TOKEN`, else from
    // `providers.traccia` in Parla's settings.json, read only. Neither the
    // URL nor the token is printed.

    fn live_config() -> Option<TracciaConfig> {
        let saved = std::env::var_os("APPDATA")
            .map(|dir| std::path::Path::new(&dir).join("com.matdac6.parla/settings.json"))
            .and_then(|path| crate::settings::load(&path).ok())
            .unwrap_or_default();
        let config = TracciaConfig::resolved(&saved).ok();
        if config.is_none() {
            println!("Skipping: no TRACCIA_URL/TRACCIA_TOKEN and no providers.traccia");
        }
        config
    }

    #[test]
    #[ignore = "calls the real Traccia over the tailnet"]
    fn live_test_connection_reports_the_actor_token_and_projects() {
        let Some(config) = live_config() else { return };
        let started = std::time::Instant::now();
        let check = test_connection(&config.base_url, &config.token).unwrap();
        let elapsed = started.elapsed().as_millis();
        println!(
            "{} ({elapsed} ms, warning: {:?})",
            check.message, check.warning
        );
        assert!(check.message.starts_with("Connected as you · token "));
        assert_eq!(check.warning, None);
    }

    #[test]
    #[ignore = "calls the real Traccia over the tailnet"]
    fn live_a_wrong_token_is_rejected() {
        let Some(config) = live_config() else { return };
        let error = test_connection(&config.base_url, "test-token").unwrap_err();
        println!("wrong token: {error}");
        assert_eq!(error, traccia::Error::Token.to_string());
    }

    #[test]
    #[ignore = "waits on the network for up to 5 s"]
    fn live_an_unreachable_traccia_fails_within_about_5_s() {
        // A tailnet-style (CGNAT) address no machine uses, and a name that
        // doesn't resolve (as with Tailscale off).
        for base_url in [
            "https://100.64.255.254",
            "https://traccia-missing.example.ts.net",
        ] {
            let started = std::time::Instant::now();
            let error = test_connection(base_url, "test-token").unwrap_err();
            let elapsed = started.elapsed();
            println!("{base_url}: \"{error}\" after {} ms", elapsed.as_millis());
            assert_eq!(error, traccia::Error::Unreachable.to_string());
            assert!(elapsed < std::time::Duration::from_secs(6), "{elapsed:?}");
        }
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
