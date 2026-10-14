//! Owns the tray menu and settings window lifecycle.

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WindowEvent,
};

pub fn setup(app: &tauri::App) -> tauri::Result<()> {
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&settings, &quit])?;

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
            "settings" => {
                if let Some(window) = app.get_webview_window("main") {
                    if let Err(error) = window
                        .unminimize()
                        .and_then(|()| window.show())
                        .and_then(|()| window.set_focus())
                    {
                        eprintln!("failed to open settings: {error}");
                    }
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    if let Some(window) = app.get_webview_window("main") {
        let settings_window = window.clone();
        window.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Err(error) = settings_window.hide() {
                    eprintln!("failed to hide settings: {error}");
                }
            }
        });
    }
    Ok(())
}
