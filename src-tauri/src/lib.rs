mod bench;
pub mod clip;
mod flow;
mod hotkey;
mod inserter;
mod provider;
pub mod recorder;
mod settings;
mod timing;
mod tray;

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            tray::setup(app)?;
            recorder::warm_up();
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run Parla");
}
