//! Owns the tray menu and settings window lifecycle.

use std::time::Duration;

use tauri::{
    menu::{IsMenuItem, Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

/// The right-click menu, top to bottom, as `(id, label)`. The first item is
/// the default (bold on Windows). History and Timings live in the app window;
/// Benchmark is linked from its sidebar rail.
const MENU: [(&str, &str); 5] = [
    ("open", "Open Parla"),
    ("settings", "Settings…"),
    ("test_mic", "Test mic (3 s)"),
    (RETRY, "Retry last failed dictation"),
    ("quit", "Quit Parla"),
];
const RETRY: &str = "retry_failed";

pub fn setup(app: &tauri::App) -> tauri::Result<()> {
    let has_clip = crate::failure::clip_path(app.handle()).is_ok_and(|path| path.is_file());
    let items = MENU
        .iter()
        .map(|&(id, label)| {
            MenuItem::with_id(app, id, label, id != RETRY || has_clip, None::<&str>)
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let retry = items.iter().find(|item| item.id() == RETRY);
    app.manage(crate::failure::RetryMenu(
        retry.expect("MENU lists RETRY").clone(),
    ));
    let refs: Vec<&dyn IsMenuItem<_>> = items.iter().map(|item| item as _).collect();
    let menu = Menu::with_items(app, &refs)?;
    #[cfg(windows)]
    make_first_item_default(&menu);

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
            "open" => open_app(app, "home"),
            "settings" => open_app(app, "settings"),
            "test_mic" => run_test_mic(app),
            RETRY => crate::commands::failure::retry_from_tray(app.clone()),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    // Only the recording overlay is kept warm. Secondary windows are created
    // by open_app/open_bench and destroyed normally when the user closes them.
    Ok(())
}

/// Bolds "Open Parla" like other tray apps' default action. Tauri has no API
/// for it, so this sets the state on the popup's native handle (best effort).
#[cfg(windows)]
fn make_first_item_default<R: tauri::Runtime>(menu: &Menu<R>) {
    use tauri::menu::ContextMenu;
    use windows_sys::Win32::UI::WindowsAndMessaging::SetMenuDefaultItem;
    if let Ok(hmenu) = menu.hpopupmenu() {
        // SAFETY: `hmenu` belongs to `menu`, which the tray keeps alive.
        unsafe { SetMenuDefaultItem(hmenu as _, 0, 1) };
    }
}

/// Opens the main window on `view`: an app page ("home", "history",
/// "timings", …) or "settings" for the first settings page.
pub(crate) fn open_app(app: &tauri::AppHandle, view: &str) {
    queue_window(app, "app", view);
}

pub(crate) fn open_bench(app: &tauri::AppHandle) {
    queue_window(app, "bench", "");
}

fn queue_window(app: &tauri::AppHandle, label: &str, view: &str) {
    let handle = app.clone();
    let label = label.to_owned();
    let view = view.to_owned();
    // Serialize creation on the event loop: simultaneous tray/IPC requests
    // cannot create duplicate labels. Never block a synchronous IPC handler.
    if let Err(error) =
        app.run_on_main_thread(move || match ensure_window(&handle, &label, &view) {
            Ok(window) => {
                if let Err(error) = window
                    .unminimize()
                    .and_then(|()| window.show())
                    .and_then(|()| window.set_focus())
                {
                    eprintln!("failed to show {label}: {error}");
                }
            }
            Err(error) => eprintln!("failed to open {label}: {error}"),
        })
    {
        eprintln!("failed to schedule window: {error}");
    }
}

/// Call on the main thread. The initial route travels in the URL so a newly
/// loaded page cannot miss a navigation event sent before its listeners exist.
fn ensure_window(
    app: &tauri::AppHandle,
    label: &str,
    view: &str,
) -> tauri::Result<tauri::WebviewWindow> {
    if let Some(window) = app.get_webview_window(label) {
        if label == "app" {
            let _ = app.emit_to("app", "navigate", view);
        }
        return Ok(window);
    }
    let (title, url, width, height) = match label {
        // app.ts maps an unknown page to Home.
        "app" => ("Parla", format!("app.html#{view}"), 900.0, 680.0),
        "bench" => ("Parla Benchmark", "bench.html".into(), 960.0, 640.0),
        _ => return Err(tauri::Error::WindowNotFound),
    };
    let window = WebviewWindowBuilder::new(app, label, WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(width, height)
        .visible(false)
        .build()?;
    if label == "bench" {
        if let Some(session) = app.try_state::<crate::bench::BenchSession>() {
            session.open_window();
        }
        let target = window.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let recording = target
                    .app_handle()
                    .try_state::<crate::bench::BenchSession>()
                    .is_some_and(|session| {
                        // Invalidates an in-flight start before it can publish
                        // a capture whose controls have already been destroyed.
                        session.close_window();
                        session.is_recording()
                    });
                if recording {
                    // Finish capture before destroying its controls. Otherwise
                    // reopening resets the UI while the old recording retains
                    // the dictation guard until the 30-second timer expires.
                    api.prevent_close();
                    let target = target.clone();
                    std::thread::spawn(move || {
                        crate::commands::bench::finish_recording(target.app_handle(), None);
                        let _ = target.destroy();
                    });
                }
            }
        });
    }
    Ok(window)
}
/// Records three seconds from the default microphone, saves it as
/// `clips/test.wav` in the app data dir, and logs the path. Everything runs on
/// a spawned thread: acquiring the recorder can block while the device opens,
/// and the menu callback must not.
pub(crate) fn run_test_mic(app: &tauri::AppHandle) {
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

#[cfg(test)]
mod menu_tests {
    use super::MENU;

    #[test]
    fn right_click_menu_is_the_five_items_with_open_first() {
        let labels: Vec<_> = MENU.iter().map(|&(_, label)| label).collect();
        assert_eq!(
            labels,
            [
                "Open Parla",
                "Settings…",
                "Test mic (3 s)",
                "Retry last failed dictation",
                "Quit Parla",
            ]
        );
        // The first item is made the default (bold) on Windows.
        assert_eq!(MENU[0].0, "open");
        assert!(MENU.iter().any(|&(id, _)| id == super::RETRY));
        let mut ids: Vec<_> = MENU.iter().map(|&(id, _)| id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MENU.len(), "menu ids must be unique");
    }
}

#[cfg(all(test, windows, feature = "native-smoke"))]
mod tests {
    use super::*;

    /// Opens real WebView2 windows; run explicitly on a Windows desktop.
    #[test]
    #[ignore = "opens native windows; needs a Windows desktop and built frontend"]
    fn secondary_windows_are_lazy_and_can_be_destroyed_and_reopened() {
        let mut context = tauri::generate_context!();
        // The test runner does not embed the app's icon resource.
        context.set_default_window_icon(Some(tauri::image::Image::new_owned(vec![0; 4], 1, 1)));
        let app = tauri::Builder::default()
            .any_thread()
            .setup(|app| {
                setup(app)?;
                crate::overlay::setup(app)?;
                assert!(app.get_webview_window("overlay").is_some());
                for label in ["app", "bench"] {
                    assert!(
                        app.get_webview_window(label).is_none(),
                        "{label} was eagerly created"
                    );
                }
                Ok(())
            })
            .build(context)
            .unwrap();
        let handle = app.handle().clone();
        let (result, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                let overlay = loop {
                    if let Some(window) = handle.get_webview_window("overlay") {
                        break window;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "Overlay setup never completed"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                };
                let plain = crate::overlay::begin(&handle, false);
                std::thread::sleep(Duration::from_millis(400));
                let plain_bottom = overlay.outer_position().unwrap().y
                    + overlay.inner_size().unwrap().height as i32;
                drop(plain);
                let captioned = crate::overlay::begin(&handle, true);
                std::thread::sleep(Duration::from_millis(400));
                let caption_bottom = overlay.outer_position().unwrap().y
                    + overlay.inner_size().unwrap().height as i32;
                assert!(
                    (plain_bottom - caption_bottom).abs() <= 1,
                    "Captions moved the native pill anchor"
                );
                assert!(
                    (overlay.inner_size().unwrap().width as f64 / overlay.scale_factor().unwrap()
                        - 366.0)
                        .abs()
                        <= 1.0
                );
                drop(captioned);
                for label in ["app", "bench", "app"] {
                    let (done, completed) = std::sync::mpsc::channel();
                    let target = handle.clone();
                    handle
                        .run_on_main_thread(move || {
                            let _ = done.send(ensure_window(&target, label, "settings"));
                        })
                        .unwrap();
                    let window = completed
                        .recv_timeout(Duration::from_secs(15))
                        .unwrap()
                        .unwrap();
                    if label == "app" {
                        let deadline = std::time::Instant::now() + Duration::from_secs(15);
                        // app.ts, once loaded, routes #settings to its first
                        // settings page; without a dev server it never loads.
                        while !matches!(
                            window.url().unwrap().fragment(),
                            Some("settings" | "transcription")
                        ) {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "initial Settings route never loaded"
                            );
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                    window.close().unwrap();
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    while handle.get_webview_window(label).is_some() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "{label} did not close"
                        );
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }));
            result.send(check.is_ok()).unwrap();
            handle.exit(0);
        });
        app.run_return(|_, _| {});
        assert!(outcome.recv().unwrap());
    }
}
