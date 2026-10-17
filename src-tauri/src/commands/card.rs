/// The Card's box once the overlay page has laid it out: the window takes
/// clicks only over it. Async, so it never runs on the main thread while the
/// cursor thread holds the Card and switches the window.
#[tauri::command]
pub async fn card_bounds(generation: u64, bounds: crate::overlay::Bounds) {
    crate::overlay::set_card_bounds(generation, bounds);
}

/// A Card button. Off the main thread: Copy and Paste wait on the clipboard.
/// The outcome goes to the log.
#[tauri::command]
pub async fn card_action(app: tauri::AppHandle, generation: u64, action: crate::card::Action) {
    let _ =
        tauri::async_runtime::spawn_blocking(move || crate::card::act(&app, generation, action))
            .await;
}
