mod bench;
mod flow;
pub mod hotkey;
pub mod inserter;
mod provider;
mod recorder;
mod settings;
mod timing;
mod tray;

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            tray::setup(app)?;
            flow::setup(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run Parla");
}
