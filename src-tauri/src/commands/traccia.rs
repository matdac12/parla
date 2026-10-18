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

    fn live_settings() -> crate::settings::Settings {
        std::env::var_os("APPDATA")
            .map(|dir| std::path::Path::new(&dir).join("com.matdac6.parla/settings.json"))
            .and_then(|path| crate::settings::load(&path).ok())
            .unwrap_or_default()
    }

    fn live_config() -> Option<TracciaConfig> {
        let config = TracciaConfig::resolved(&live_settings()).ok();
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
    fn live_the_projects_cache_fetches_the_real_list() {
        use std::time::{Duration, Instant};
        let Some(config) = live_config() else { return };
        let cache: &'static traccia::ProjectsCache =
            Box::leak(Box::new(traccia::ProjectsCache::new()));
        let started = Instant::now();
        cache.refresh_if_stale(started, move || traccia::Client::new(&config).projects());
        // The run's wait is 500 ms; a cold TLS handshake may take longer here.
        let projects = cache.snapshot(Duration::from_secs(3));
        println!(
            "{} projects in {} ms:",
            projects.len(),
            started.elapsed().as_millis()
        );
        for project in &projects {
            println!("  {}: {}", project.key, project.name);
        }
        assert!(!projects.is_empty());
    }

    #[test]
    #[ignore = "calls the real Traccia over the tailnet"]
    fn live_a_wrong_token_is_rejected() {
        let Some(config) = live_config() else { return };
        let error = test_connection(&config.base_url, "test-token").unwrap_err();
        println!("wrong token: {error}");
        assert_eq!(error, traccia::Error::Token.to_string());
    }

    // PARLA-86: `[parla-test]` To-dos with no Reminder, or one in 2027, so
    // nothing pings; each is deleted before anything is checked. Only ids,
    // titles, days and times are printed, never the URL.

    #[test]
    #[ignore = "creates and deletes [parla-test] To-dos in the real Traccia"]
    fn live_create_returns_the_to_do_and_it_is_deleted() {
        use crate::command_mode::when::{Date, LocalClock, SystemClock, Time, When};
        let Some(config) = live_config() else { return };
        let client = traccia::Client::for_writes(&config);
        let (day, time) = (
            Date::parse("2027-01-15").unwrap(),
            Time::parse("10:00").unwrap(),
        );
        let utc = SystemClock.to_utc(day, time);
        println!("2027-01-15 10:00 local is remindAt {utc:?}");
        let cases = [
            ("[parla-test] PARLA-86 anytime", When::Anytime, None, None),
            (
                "[parla-test] PARLA-86 day only",
                When::Day(day),
                Some("2027-01-15"),
                None,
            ),
            (
                "[parla-test] PARLA-86 remindAt 2027",
                When::At { day, time, utc },
                Some("2027-01-15"),
                Some("10:00"),
            ),
        ];
        for (title, when, want_day, want_time) in &cases {
            let started = std::time::Instant::now();
            let created = client.create(title, when).unwrap();
            let elapsed = started.elapsed().as_millis();
            let deleted = client.undo(&created.id);
            println!(
                "created {} {:?} (day {:?}, time {:?}) in {elapsed} ms; deleted: {deleted:?}",
                created.id, created.title, created.day, created.time
            );
            assert_eq!(deleted, Ok(()));
            assert_eq!(created.title, *title);
            assert_eq!(created.day.as_deref(), *want_day);
            assert_eq!(created.time.as_deref(), *want_time);
            assert!(created.url.starts_with(&config.base_url));
            assert!(created.url.ends_with(&created.id));
        }
        let left: Vec<String> = client
            .open_titles()
            .unwrap()
            .into_iter()
            .filter(|title| cases.iter().any(|(created, ..)| title == created))
            .collect();
        assert!(left.is_empty(), "left open: {left:?}");
    }

    #[test]
    #[ignore = "creates a [parla-test] To-do in the real Traccia and undoes it"]
    fn live_the_cards_undo_removes_the_to_do() {
        use crate::command_mode::when::When;
        let Some(config) = live_config() else { return };
        let client = traccia::Client::for_writes(&config);
        let title = "[parla-test] PARLA-88 undo";
        // Anytime, no Reminder: nothing can ping.
        let created = client.create(title, &When::Anytime).unwrap();
        // The Card's own path: the saved connection, resolved at the click.
        let settings = live_settings();
        let started = std::time::Instant::now();
        let undone = traccia::Remote(&settings).undo(&created.id);
        let elapsed = started.elapsed().as_millis();
        println!(
            "created {} {:?}; undo in {elapsed} ms: {undone:?}",
            created.id, created.title
        );
        if undone.is_err() {
            println!("fallback delete: {:?}", client.undo(&created.id));
        }
        assert_eq!(undone, Ok(()));
        let open = client.open_titles().unwrap();
        assert!(!open.iter().any(|open| open == title), "left open");
        // Undone already: Traccia's 404 counts as done.
        assert_eq!(client.undo(&created.id), Ok(()));
    }

    #[test]
    #[ignore = "asks the real Traccia for a past reminder, which it refuses"]
    fn live_a_past_moment_is_refused_and_creates_nothing() {
        use crate::command_mode::when::{Date, LocalClock, SystemClock, Time, When};
        let Some(config) = live_config() else { return };
        let client = traccia::Client::for_writes(&config);
        let title = "[parla-test] PARLA-86 past";
        let (day, time) = (
            Date::parse("2026-10-17").unwrap(),
            Time::parse("10:00").unwrap(),
        );
        let when = When::At {
            day,
            time,
            utc: SystemClock.to_utc(day, time),
        };
        let error = match client.create(title, &when) {
            Err(error) => error,
            Ok(created) => {
                let deleted = client.undo(&created.id);
                panic!("created {} (deleted: {deleted:?})", created.id);
            }
        };
        let message = traccia::reminder_message(&error, &when, SystemClock.now().date);
        println!("{error:?}: {message}");
        assert_eq!(message, "10:00 on Sat 17/10 has already passed.");
        let titles = client.open_titles().unwrap();
        assert!(!titles.iter().any(|open| open == title));
    }

    #[test]
    #[ignore = "calls the real Traccia over the tailnet"]
    fn live_the_wrong_scheme_says_to_check_the_url_not_the_token() {
        let Some(config) = live_config() else { return };
        let swapped = match config.base_url.split_once("://") {
            Some(("https", rest)) => format!("http://{rest}"),
            Some((_, rest)) => format!("https://{rest}"),
            None => return,
        };
        let error = test_connection(&swapped, &config.token).unwrap_err();
        println!("the other scheme: {error}");
        assert!(
            [
                traccia::Error::Redirect,
                traccia::Error::Protocol,
                traccia::Error::Dropped,
                traccia::Error::Unreachable
            ]
            .iter()
            .any(|expected| error == expected.to_string()),
            "{error}"
        );
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
