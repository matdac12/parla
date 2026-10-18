//! Manual checks of Settings → Test connection (PARLA-85) against the real
//! Traccia: `cargo test --locked --test traccia_live -- --ignored --nocapture`.
//! The connection comes from `TRACCIA_URL` and `TRACCIA_TOKEN`, else from
//! `providers.traccia` in Parla's settings.json, read only. Neither the URL
//! nor the token is printed.
use parla_lib::{
    command_mode::traccia::{self, Error, TracciaConfig},
    settings::{self, Settings},
};
use std::time::{Duration, Instant};

fn saved_settings() -> Settings {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Settings::default();
    };
    let path = std::path::Path::new(&appdata).join("com.matdac6.parla/settings.json");
    settings::load(&path).unwrap_or_default()
}

#[test]
#[ignore = "calls the real Traccia over the tailnet"]
fn test_connection_reports_the_actor_token_and_projects() {
    let Ok(config) = TracciaConfig::resolved(&saved_settings()) else {
        println!("Skipping: no TRACCIA_URL/TRACCIA_TOKEN and no providers.traccia");
        return;
    };
    let started = Instant::now();
    let connection = traccia::check(&config).map_err(|e| e.to_string()).unwrap();
    println!(
        "{} ({} ms)",
        connection.summary(),
        started.elapsed().as_millis()
    );
    if let Some(warning) = connection.warning() {
        println!("warning: {warning}");
    }
    assert_eq!(connection.actor, "you");
}

#[test]
#[ignore = "calls the real Traccia over the tailnet"]
fn a_wrong_token_is_rejected_by_the_real_traccia() {
    let Ok(saved) = TracciaConfig::resolved(&saved_settings()) else {
        println!("Skipping: no TRACCIA_URL/TRACCIA_TOKEN and no providers.traccia");
        return;
    };
    let config = TracciaConfig::new(&saved.base_url, "test-token").unwrap();
    let error = traccia::check(&config).unwrap_err();
    println!("wrong token: {error}");
    assert_eq!(error, Error::Token);
}

#[test]
#[ignore = "waits on the network for up to 5 s"]
fn an_unreachable_traccia_fails_within_about_5_s() {
    // A tailnet-style (CGNAT) address no machine uses, and a name that
    // doesn't resolve (as with Tailscale off).
    for base_url in [
        "https://100.64.255.254",
        "https://traccia-missing.example.ts.net",
    ] {
        let config = TracciaConfig::new(base_url, "test-token").unwrap();
        let started = Instant::now();
        let error = traccia::check(&config).unwrap_err();
        let elapsed = started.elapsed();
        println!("{base_url}: \"{error}\" after {} ms", elapsed.as_millis());
        assert_eq!(error, Error::Unreachable);
        assert!(elapsed < Duration::from_secs(6), "{elapsed:?}");
    }
}
