//! Owns the tray menu and settings window lifecycle.

use std::time::Duration;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

pub fn setup(app: &tauri::App) -> tauri::Result<()> {
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let history = MenuItem::with_id(app, "history", "History…", true, None::<&str>)?;
    let bench = MenuItem::with_id(app, "bench", "Benchmark…", true, None::<&str>)?;
    let test_mic = MenuItem::with_id(app, "test_mic", "Test mic (3 s)", true, None::<&str>)?;
    let retry = MenuItem::with_id(
        app,
        "retry_failed",
        "Retry last failed dictation",
        crate::failure::clip_path(app.handle()).is_ok_and(|path| path.is_file()),
        None::<&str>,
    )?;
    app.manage(crate::failure::RetryMenu(retry.clone()));
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&settings, &history, &bench, &test_mic, &retry, &quit],
    )?;

    // Tauri's native tray API: https://v2.tauri.app/learn/system-tray/
    TrayIconBuilder::with_id("parla")
        .icon(
            app.default_window_icon()
                .expect("app icon is configured")
                .clone(),
        )
        .tooltip("Parla")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "settings" => show_window(app, "main", "settings"),
            "history" => show_window(app, "history", "history"),
            "bench" => show_window(app, "bench", "benchmark"),
            "test_mic" => run_test_mic(app),
            "retry_failed" => {
                crate::commands::failure::retry_from_tray(app.clone());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    // Closing either window hides it instead of destroying it, so the tray
    // item can show it again.
    // Created here, not in tauri.conf.json: config windows load their pages
    // before `setup`, and their first IPC calls would race the managed state.
    for (label, what, title, url, width, height) in [
        (
            "main",
            "settings",
            "Parla Settings",
            "settings.html",
            640.0,
            480.0,
        ),
        (
            "history",
            "history",
            "Parla History",
            "history.html",
            960.0,
            600.0,
        ),
        (
            "bench",
            "benchmark",
            "Parla Benchmark",
            "bench.html",
            960.0,
            640.0,
        ),
    ] {
        let window = WebviewWindowBuilder::new(app, label, WebviewUrl::App(url.into()))
            .title(title)
            .inner_size(width, height)
            .visible(false)
            .build()?;
        {
            let hide_target = window.clone();
            window.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    if let Err(error) = hide_target.hide() {
                        eprintln!("failed to hide the {what} window: {error}");
                    }
                }
            });
        }
    }
    Ok(())
}

/// Shows, unminimizes and focuses a hidden window, or logs why it could not.
fn show_window(app: &tauri::AppHandle, label: &str, what: &str) {
    let Some(window) = app.get_webview_window(label) else {
        eprintln!("failed to open {what}: window is unavailable");
        return;
    };
    if let Err(error) = window
        .unminimize()
        .and_then(|()| window.show())
        .and_then(|()| window.set_focus())
    {
        eprintln!("failed to open {what}: {error}");
    }
}

/// Records three seconds from the default microphone, saves it as
/// `clips/test.wav` in the app data dir, and logs the path. Everything runs on
/// a spawned thread: acquiring the recorder can block while the device opens,
/// and the menu callback must not.
fn run_test_mic(app: &tauri::AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(recorder) = crate::recorder::recorder() else {
            eprintln!("test mic: no input device available");
            return;
        };
        if let Err(error) = recorder.start() {
            eprintln!("test mic: not starting ({error:#})");
            return;
        }
        std::thread::sleep(Duration::from_secs(3));
        let clip = recorder.stop();

        let path = match app.path().app_data_dir() {
            Ok(dir) => dir.join("clips").join("test.wav"),
            Err(error) => {
                eprintln!("test mic: resolving the app data dir failed: {error}");
                return;
            }
        };
        match clip.save_wav(&path) {
            Ok(()) => {
                println!(
                    "test mic: saved {} ({:.1}s, {} samples)",
                    path.display(),
                    clip.duration().as_secs_f32(),
                    clip.samples.len()
                );
                reveal(&path);
            }
            Err(error) => eprintln!("test mic: saving failed: {error:#}"),
        }
    });
}

/// Reveals `path` in the OS file manager (best effort). A release Windows build
/// has no console, so this is how the test clip's location reaches the user.
fn reveal(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .status();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .status();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(dir) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(dir).status();
        }
    }
}
