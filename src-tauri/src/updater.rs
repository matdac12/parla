//! Auto-update from GitHub Releases (PARLA-101).
//!
//! `tauri-plugin-updater` reads `latest.json` from the newest release and
//! verifies the installer's signature against the public key in
//! `tauri.conf.json` (`plugins.updater.pubkey`). Without that key the whole
//! module stays off: no check, no notice, nothing to fail.
//!
//! Nothing here touches the dictation path. One sleeping task wakes up an
//! hour at a time; it checks only when the last check is a day old, and only
//! after Parla has been up for a couple of minutes. The webviews reach it
//! through the commands below; they have no access to the plugin's own.

use crate::log::log_error;
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::command_mode::UrlOpener;

/// Parla runs for hours, so wait out the busy minutes after start.
const START_DELAY: Duration = Duration::from_secs(120);
/// How often the background task looks at the clock (not at the network).
const TICK: Duration = Duration::from_secs(3600);
const DAY: u64 = 86_400;
const RELEASES: &str = "https://github.com/matdac12/parla/releases";
/// The Updates UI lives in the app window only; the always-warm overlay and
/// the flyout get none of these events.
const WINDOW: &str = "app";
/// Sent when the status changes behind the window's back.
const CHANGED: &str = "update://changed";
const PROGRESS: &str = "update://progress";

/// What Parla remembers between runs, in `updater.json` next to
/// `settings.json`. Not part of `Settings`: the toggle applies at once and
/// does not go through the Save bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    /// "Update automatically": check once a day in the background.
    auto: bool,
    /// Unix seconds of the last check that reached the update feed.
    last_check: u64,
    /// The version already announced with a notification.
    notified: String,
    /// The version whose banner was dismissed with "Later".
    dismissed: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            auto: true,
            last_check: 0,
            notified: String::new(),
            dismissed: String::new(),
        }
    }
}

impl Prefs {
    fn due(&self, now: u64) -> bool {
        // A last check in the future means the clock moved back: check again.
        self.auto && (self.last_check > now || now - self.last_check >= DAY)
    }
}

struct Inner {
    prefs: Prefs,
    available: Option<Update>,
    error: Option<String>,
}

pub struct UpdaterState {
    /// A public key is configured; everything else is off without one.
    enabled: bool,
    /// Not installed by the installer, so it cannot replace itself.
    portable: bool,
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
    /// One check or install at a time.
    busy: tokio::sync::Mutex<()>,
}

impl UpdaterState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Changes the state and saves the preferences. The save happens under the
    /// lock, so two edits cannot write the file out of order.
    fn edit<R>(&self, change: impl FnOnce(&mut Inner) -> R) -> R {
        let mut inner = self.lock();
        let result = change(&mut inner);
        if let Some(path) = &self.path {
            if let Err(error) = save(path, &inner.prefs) {
                log_error!("Could not save {}: {error}", path.display());
            }
        }
        result
    }
}

fn save(path: &Path, prefs: &Prefs) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(prefs).map_err(|error| error.to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    // Written beside the file and renamed, so a crash leaves the old one.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|error| error.to_string())?;
    std::fs::rename(&tmp, path).map_err(|error| error.to_string())
}

fn load(path: &Path) -> Prefs {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            log_error!("Ignoring {}: {error}", path.display());
            Prefs::default()
        }),
        Err(_) => Prefs::default(),
    }
}

/// Everything the Updates page and the banner show.
#[derive(Serialize, Clone)]
pub struct Status {
    enabled: bool,
    current: String,
    portable: bool,
    auto: bool,
    last_check: Option<u64>,
    /// The newest version, when it is newer than this one.
    available: Option<String>,
    dismissed: bool,
    error: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The updater is on only when `plugins.updater.pubkey` holds a key.
fn key_configured(app: &AppHandle) -> bool {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|config| config.get("pubkey"))
        .and_then(|key| key.as_str())
        .is_some_and(|key| !key.trim().is_empty())
}

/// The installer leaves `uninstall.exe` beside `parla.exe`; the zip, and a
/// build run from `target/`, do not.
fn is_portable() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| !dir.join("uninstall.exe").is_file()))
        .unwrap_or(true)
}

fn release_page(version: &str) -> String {
    format!("{RELEASES}/tag/v{version}")
}

fn status(app: &AppHandle) -> Status {
    let state = app.state::<UpdaterState>();
    let inner = state.lock();
    let available = inner
        .available
        .as_ref()
        .map(|update| update.version.clone());
    Status {
        enabled: state.enabled,
        current: app.package_info().version.to_string(),
        portable: state.portable,
        auto: inner.prefs.auto,
        last_check: Some(inner.prefs.last_check).filter(|&at| at > 0),
        dismissed: available.as_deref() == Some(inner.prefs.dismissed.as_str()),
        available,
        error: inner.error.clone(),
    }
}

pub fn setup(app: &tauri::App) {
    let path = app
        .path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join("updater.json"));
    let prefs = path.as_deref().map(load).unwrap_or_default();
    let enabled = key_configured(app.handle());
    app.manage(UpdaterState {
        enabled,
        portable: is_portable(),
        path,
        inner: Mutex::new(Inner {
            prefs,
            available: None,
            error: None,
        }),
        busy: tokio::sync::Mutex::new(()),
    });
    if enabled {
        tauri::async_runtime::spawn(background(app.handle().clone()));
    }
}

async fn background(app: AppHandle) {
    tokio::time::sleep(START_DELAY).await;
    loop {
        let due = app.state::<UpdaterState>().lock().prefs.due(now());
        // A failed check is not recorded, so it is tried again at the next
        // tick; a successful one waits a day.
        if due {
            check(&app).await;
            announce(&app);
        }
        tokio::time::sleep(TICK).await;
    }
}

/// An updater whose installer step first saves what Parla saves on exit: on
/// Windows the plugin ends the process itself, so `RunEvent::Exit` never runs.
fn updater(app: &AppHandle) -> tauri_plugin_updater::Result<tauri_plugin_updater::Updater> {
    let exiting = app.clone();
    app.updater_builder()
        .on_before_exit(move || {
            exiting.cleanup_before_exit();
            crate::flush_on_exit(&exiting);
        })
        .build()
}

/// Asks the feed and keeps the answer, or the error for the Updates page.
/// Used by the daily check and the button. True when the feed answered.
async fn check(app: &AppHandle) -> bool {
    let state = app.state::<UpdaterState>();
    let _busy = state.busy.lock().await;
    let found = async { updater(app)?.check().await }.await;
    match found {
        Ok(update) => {
            state.edit(|inner| {
                inner.available = update;
                inner.error = None;
                inner.prefs.last_check = now();
            });
            true
        }
        Err(error) => {
            log_error!("Update check failed: {error}");
            state.lock().error = Some("Could not reach the update feed. Try again later.".into());
            false
        }
    }
}

/// After a background check: tell the open window, and show one notification
/// per new version. The banner in the app is where Install and Later live.
fn announce(app: &AppHandle) {
    let state = app.state::<UpdaterState>();
    let new_version = {
        let inner = state.lock();
        inner
            .available
            .as_ref()
            .map(|update| update.version.clone())
            .filter(|version| *version != inner.prefs.notified)
    };
    if let Some(version) = new_version {
        let how = if state.portable {
            "Open Parla for the download link."
        } else {
            "Open Parla to install it."
        };
        let message = format!("Parla {version} is available. {how}");
        // Only a shown notification counts, or a blocked one is never retried.
        if crate::failure::show_toast(app, "Parla update", &message) {
            state.edit(|inner| inner.prefs.notified = version);
        }
    }
    if let Err(error) = app.emit_to(WINDOW, CHANGED, status(app)) {
        log_error!("Could not tell the window about the update: {error}");
    }
}

#[tauri::command]
pub fn update_status(app: AppHandle) -> Status {
    status(&app)
}

/// The "Check for updates now" button. A failed check is part of the status
/// (`error`), not an error of the command.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<Status, String> {
    if !app.state::<UpdaterState>().enabled {
        return Err("Updates are not set up in this build.".into());
    }
    check(&app).await;
    Ok(status(&app))
}

#[tauri::command]
pub fn set_auto_update(app: AppHandle, state: State<'_, UpdaterState>, auto: bool) -> Status {
    state.edit(|inner| inner.prefs.auto = auto);
    status(&app)
}

/// "Later": hides the banner for this version. The Updates page still offers
/// the install, and a newer version brings the banner back.
#[tauri::command]
pub fn dismiss_update(app: AppHandle, state: State<'_, UpdaterState>) -> Status {
    state.edit(|inner| {
        if let Some(update) = &inner.available {
            inner.prefs.dismissed.clone_from(&update.version);
        }
    });
    status(&app)
}

/// Downloads the update, checks its signature, and hands over to the
/// installer, which closes Parla and starts the new version. Returns only
/// when something failed.
#[tauri::command]
pub async fn install_update(app: AppHandle, state: State<'_, UpdaterState>) -> Result<(), String> {
    if state.portable {
        return Err("A portable copy cannot update itself. Download the new zip.".into());
    }
    let _busy = state.busy.lock().await;
    let update = state
        .lock()
        .available
        .clone()
        .ok_or("There is no update to install.")?;
    let mut received = 0u64;
    let mut shown = 0u8;
    update
        .download_and_install(
            |chunk, total| {
                received += chunk as u64;
                let percent = total
                    .filter(|&total| total > 0)
                    .map_or(0, |total| (received * 100 / total).min(100) as u8);
                if percent != shown {
                    shown = percent;
                    let _ = app.emit_to(WINDOW, PROGRESS, percent);
                }
            },
            || {},
        )
        .await
        .map_err(|error| {
            let message = format!("Update failed: {error}");
            crate::log::error(&message);
            message
        })
}

/// The portable build's link to the release.
#[tauri::command]
pub fn open_release_page(state: State<'_, UpdaterState>) -> Result<(), String> {
    let url = state.lock().available.as_ref().map_or_else(
        || format!("{RELEASES}/latest"),
        |update| release_page(&update.version),
    );
    crate::command_mode::tools::DefaultBrowser.open(&url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_default_to_checking_daily() {
        let prefs: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(prefs, Prefs::default());
        assert!(prefs.auto);
    }

    #[test]
    fn prefs_keep_their_values_and_ignore_unknown_keys() {
        let prefs: Prefs =
            serde_json::from_str(r#"{"auto":false,"last_check":7,"other":1}"#).unwrap();
        assert!(!prefs.auto);
        assert_eq!(prefs.last_check, 7);
    }

    #[test]
    fn a_check_is_due_once_a_day() {
        let at = |last_check| Prefs {
            last_check,
            ..Prefs::default()
        };
        let now = 10 * DAY;
        assert!(at(0).due(now));
        assert!(!at(now - DAY + 1).due(now));
        assert!(at(now - DAY).due(now));
        // The clock moved back since the last check.
        assert!(at(now + 5).due(now));
    }

    #[test]
    fn nothing_is_due_with_auto_update_off() {
        let prefs = Prefs {
            auto: false,
            ..Prefs::default()
        };
        assert!(!prefs.due(10 * DAY));
    }

    #[test]
    fn the_release_page_names_the_tag() {
        assert_eq!(
            release_page("0.2.0"),
            "https://github.com/matdac12/parla/releases/tag/v0.2.0"
        );
    }

    /// The shape `release.yml` writes into `latest.json`.
    #[test]
    fn the_plugin_reads_the_latest_json_the_release_workflow_writes() {
        let release: tauri_plugin_updater::RemoteRelease = serde_json::from_str(
            r#"{
              "version": "0.1.4",
              "notes": "Parla 0.1.4",
              "pub_date": "2026-10-20T13:41:08Z",
              "platforms": {
                "windows-x86_64": {
                  "signature": "c2ln",
                  "url": "https://github.com/matdac12/parla/releases/download/v0.1.4/Parla_0.1.4_x64-setup.exe"
                }
              }
            }"#,
        )
        .unwrap();
        assert_eq!(release.version.to_string(), "0.1.4");
        assert_eq!(release.signature("windows-x86_64").unwrap(), "c2ln");
        assert!(release
            .download_url("windows-x86_64")
            .unwrap()
            .as_str()
            .ends_with("/v0.1.4/Parla_0.1.4_x64-setup.exe"));
    }

    /// The plugin refuses to start, and so would Parla, if this does not parse.
    #[test]
    fn the_checked_in_config_loads_into_the_plugin_with_the_feed_url() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let updater: tauri_plugin_updater::Config =
            serde_json::from_value(config["plugins"]["updater"].clone()).unwrap();
        assert_eq!(
            updater.endpoints[0].as_str(),
            "https://github.com/matdac12/parla/releases/latest/download/latest.json"
        );
    }
}
