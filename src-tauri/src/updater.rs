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

use crate::autostart::{Registry, RunKey};
use crate::flow::{DictationGuard, DictationState};
use crate::log::log_error;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt, VersionComparator};

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
/// Sent when the download is in and Install waits for a dictation to end.
const WAITING: &str = "update://waiting";
/// How long Install waits for a dictation or command run to end, and how often
/// it looks. A latched dictation can run for minutes; after that Install says
/// so instead of hanging.
const IDLE_LIMIT: Duration = Duration::from_secs(120);
const IDLE_POLL: Duration = Duration::from_millis(100);
const STILL_DICTATING: &str =
    "Parla is still dictating. Finish the dictation, then press Install again.";

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
    /// Edits made so far; orders the writes of `updater.json`.
    edits: u64,
}

pub struct UpdaterState {
    /// A public key is configured; everything else is off without one.
    enabled: bool,
    /// Not installed by the installer, so it cannot replace itself.
    portable: bool,
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
    /// The last edit written to `updater.json` (and the lock for writing it);
    /// see `edit_with`.
    file: Mutex<u64>,
    /// One check or install at a time.
    busy: tokio::sync::Mutex<()>,
}

impl UpdaterState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Changes the state and saves the preferences.
    fn edit<R>(&self, change: impl FnOnce(&mut Inner) -> R) -> R {
        self.edit_with(change, |prefs| {
            if let Some(path) = &self.path {
                if let Err(error) = save(path, prefs) {
                    log_error!("Could not save {}: {error}", path.display());
                }
            }
        })
    }

    /// The disk write runs after the state lock is released, so a slow disk
    /// never stalls `status` or the background tick, whoever else is writing.
    /// Each edit is numbered under the state lock; a write that finds a newer
    /// edit already on disk is skipped, so the file never goes back in time.
    fn edit_with<R>(&self, change: impl FnOnce(&mut Inner) -> R, write: impl FnOnce(&Prefs)) -> R {
        let mut inner = self.lock();
        let result = change(&mut inner);
        inner.edits += 1;
        let (edit, prefs) = (inner.edits, inner.prefs.clone());
        drop(inner);
        let mut written = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if edit > *written {
            write(&prefs);
            *written = edit;
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

/// Where the NSIS installer records the install (its `UNINSTKEY`: the product
/// name, `Parla`, not the identifier), with the folder as `InstallLocation`.
/// The release installs per user, so only HKCU is read.
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Parla";

/// `InstallLocation` as the installer wrote it: quoted, maybe with a trailing
/// backslash. Compared the way Windows compares paths.
fn same_folder(recorded: &str, dir: &Path) -> bool {
    fn normal(path: &str) -> String {
        path.trim()
            .trim_matches('"')
            .trim_start_matches(r"\\?\")
            .replace('/', r"\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
    normal(recorded) == normal(&dir.display().to_string())
}

/// Installed when the installer's record names this folder. With no record,
/// the old test: `uninstall.exe` beside the exe. A zip run from another
/// folder, or a build run from `target/`, has neither.
fn portable_given(
    dir: &Path,
    recorded: Option<&str>,
    uninstaller_beside: impl FnOnce() -> bool,
) -> bool {
    match recorded {
        Some(location) => !same_folder(location, dir),
        None => !uninstaller_beside(),
    }
}

fn is_portable() -> bool {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return true;
    };
    // Resolved on both sides, so a junction, a subst drive or an 8.3 name for
    // the same folder is not taken for another folder.
    let resolved = |path: &str| {
        std::fs::canonicalize(path.trim().trim_matches('"'))
            .map_or_else(|_| path.to_string(), |path| path.display().to_string())
    };
    let recorded = Registry::new(UNINSTALL_KEY)
        .get("InstallLocation")
        .ok()
        .flatten()
        .filter(|location| !location.trim().is_empty())
        .map(|location| resolved(&location));
    let dir = PathBuf::from(resolved(&dir.display().to_string()));
    portable_given(&dir, recorded.as_deref(), || {
        dir.join("uninstall.exe").is_file()
    })
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
            edits: 0,
        }),
        file: Mutex::new(0),
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

/// Which announced releases count as an update: strictly newer than the
/// running version, by semver precedence with build metadata ignored, and never
/// a pre-release for a stable install. The plugin's own default is
/// `remote > current`, which treats `0.2.0+1` as newer than `0.2.0` and moves a
/// stable install to `0.3.0-rc.1`. This only judges the number in `latest.json`;
/// that the installer really is that version is `requireSignedVersion`'s job
/// (see `docs/development.md`, Releasing > Updates).
fn newer_stable_only() -> VersionComparator {
    Arc::new(|current, release| {
        let (mut current, mut remote) = (current, release.version);
        if current.pre.is_empty() && !remote.pre.is_empty() {
            return false;
        }
        current.build = Default::default();
        remote.build = Default::default();
        remote > current
    })
}

/// An updater whose installer step first saves what Parla saves on exit: on
/// Windows the plugin ends the process itself, so `RunEvent::Exit` never runs.
fn updater(app: &AppHandle) -> tauri_plugin_updater::Result<tauri_plugin_updater::Updater> {
    let exiting = app.clone();
    let newer = newer_stable_only();
    app.updater_builder()
        .version_comparator(move |current, release| newer(current, release))
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

/// Waits until `try_begin` hands out the dictation gate, i.e. no dictation or
/// command run is in flight, and returns the guard. `on_wait` runs once, only
/// if the first try fails. Gives up after `limit`.
async fn wait_for_idle(
    try_begin: impl Fn() -> Option<DictationGuard>,
    on_wait: impl FnOnce(),
    poll: Duration,
    limit: Duration,
) -> Result<DictationGuard, String> {
    if let Some(guard) = try_begin() {
        return Ok(guard);
    }
    on_wait();
    let started = Instant::now();
    loop {
        tokio::time::sleep(poll).await;
        if let Some(guard) = try_begin() {
            return Ok(guard);
        }
        if started.elapsed() >= limit {
            return Err(STILL_DICTATING.into());
        }
    }
}

/// Downloads the update and checks its signature. Then, once no dictation or
/// command run is in flight, hands over to the installer, which closes Parla
/// and starts the new version. Returns only when something failed.
///
/// The download may overlap a dictation; the install may not, since the
/// installer ends the process. The install takes the gate a dictation takes
/// and keeps it, so a hotkey press during the hand-over finds Parla busy. The
/// dictation path gains nothing: it already does this one atomic swap.
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
    let fail = |error: tauri_plugin_updater::Error| {
        let message = format!("Update failed: {error}");
        crate::log::error(&message);
        message
    };
    let bytes = update
        .download(
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
        .map_err(fail)?;
    let dictation = app.state::<DictationState>();
    let _gate = wait_for_idle(
        || dictation.try_begin(),
        || {
            let _ = app.emit_to(WINDOW, WAITING, ());
        },
        IDLE_POLL,
        IDLE_LIMIT,
    )
    .await?;
    update.install(bytes).map_err(fail)
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

    fn state(path: Option<PathBuf>) -> UpdaterState {
        UpdaterState {
            enabled: true,
            portable: false,
            path,
            inner: Mutex::new(Inner {
                prefs: Prefs::default(),
                available: None,
                error: None,
                edits: 0,
            }),
            file: Mutex::new(0),
            busy: tokio::sync::Mutex::new(()),
        }
    }

    #[test]
    fn the_prefs_write_runs_without_the_state_lock() {
        let updater = state(None);
        let mut written = None;
        updater.edit_with(
            |inner| inner.prefs.auto = false,
            |prefs| {
                // `status` and the background tick lock this while a write runs.
                assert!(updater.inner.try_lock().is_ok(), "state locked during I/O");
                written = Some(prefs.clone());
            },
        );
        assert_eq!(written.map(|prefs| prefs.auto), Some(false));
        assert!(!updater.lock().prefs.auto);
    }

    #[test]
    fn edits_reach_the_file_in_order_and_leave_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updater.json");
        let updater = std::sync::Arc::new(state(Some(path.clone())));
        updater.edit(|inner| inner.prefs.last_check = 1);
        updater.edit(|inner| inner.prefs.dismissed = "0.2.0".into());
        assert_eq!(
            load(&path),
            Prefs {
                last_check: 1,
                dismissed: "0.2.0".into(),
                ..Prefs::default()
            }
        );
        // Many edits at once: the file ends up holding the final state.
        let threads: Vec<_> = (1..=16)
            .map(|n| {
                let updater = updater.clone();
                std::thread::spawn(move || updater.edit(|inner| inner.prefs.last_check += n))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let expected = updater.lock().prefs.clone();
        assert_eq!(expected.last_check, 1 + (1..=16).sum::<u64>());
        assert_eq!(load(&path), expected);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["updater.json"]);
    }

    #[test]
    fn a_missing_or_broken_prefs_file_loads_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("updater.json");
        assert_eq!(load(&path), Prefs::default());
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load(&path), Prefs::default());
    }

    #[test]
    fn the_install_location_matches_however_the_installer_wrote_it() {
        let dir = Path::new(r"C:\Users\Zoë\AppData\Local\Parla");
        for recorded in [
            r"C:\Users\Zoë\AppData\Local\Parla",
            r#""C:\Users\Zoë\AppData\Local\Parla""#,
            r"C:\Users\Zoë\AppData\Local\Parla\",
            r"c:\users\zoë\appdata\local\PARLA",
            r"C:/Users/Zoë/AppData/Local/Parla",
            r"\\?\C:\Users\Zoë\AppData\Local\Parla",
            r#"  "C:\Users\Zoë\AppData\Local\Parla\"  "#,
        ] {
            assert!(same_folder(recorded, dir), "{recorded}");
        }
        for recorded in [
            r"C:\Users\Zoë\AppData\Local\Parla2",
            r"C:\Users\Zoë\AppData\Local",
            r"D:\Parla",
            "",
        ] {
            assert!(!same_folder(recorded, dir), "{recorded}");
        }
    }

    #[test]
    fn the_installers_record_decides_and_uninstall_exe_is_the_fallback() {
        let dir = Path::new(r"C:\Users\me\AppData\Local\Parla");
        let here = Some(r#""C:\Users\me\AppData\Local\Parla""#);
        let elsewhere = Some(r"D:\Apps\Parla");
        // The record names this folder: installed, with or without the uninstaller.
        assert!(!portable_given(dir, here, || true));
        assert!(!portable_given(dir, here, || false));
        // The record names another folder: a zip beside an installed copy, even
        // when a stale uninstall.exe sits next to it.
        assert!(portable_given(dir, elsewhere, || false));
        assert!(portable_given(dir, elsewhere, || true));
        // No record: the old test.
        assert!(!portable_given(dir, None, || true));
        assert!(portable_given(dir, None, || false));
    }

    /// The installer writes `Uninstall\<productName>`; read from the config so
    /// a rename cannot silently leave the lookup pointing at nothing.
    #[test]
    fn the_uninstall_key_is_the_one_the_installer_names_after_the_product() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let product = config["productName"].as_str().unwrap();
        assert!(UNINSTALL_KEY.ends_with(&format!(r"\Uninstall\{product}")));
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tauri::async_runtime::block_on(future)
    }

    #[test]
    fn install_goes_ahead_at_once_when_nothing_is_in_flight() {
        let dictation = DictationState::default();
        let mut waited = false;
        let gate = block_on(wait_for_idle(
            || dictation.try_begin(),
            || waited = true,
            Duration::from_millis(5),
            Duration::from_millis(200),
        ));
        assert!(gate.is_ok() && !waited);
        // It holds the gate, so a dictation started now is turned away.
        assert!(dictation.try_begin().is_none());
        drop(gate);
        assert!(dictation.try_begin().is_some());
    }

    #[test]
    fn install_waits_for_a_dictation_to_finish() {
        let dictation = DictationState::default();
        let running = dictation.try_begin().unwrap();
        let finish = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(running);
        });
        let mut waited = 0;
        let gate = block_on(wait_for_idle(
            || dictation.try_begin(),
            || waited += 1,
            Duration::from_millis(5),
            Duration::from_secs(5),
        ));
        finish.join().unwrap();
        assert!(gate.is_ok());
        assert_eq!(waited, 1, "the window is told once");
    }

    #[test]
    fn install_gives_up_while_a_dictation_runs_and_leaves_it_alone() {
        let dictation = DictationState::default();
        let running = dictation.try_begin().unwrap();
        let gate = block_on(wait_for_idle(
            || dictation.try_begin(),
            || {},
            Duration::from_millis(5),
            Duration::from_millis(40),
        ));
        assert_eq!(gate.err().as_deref(), Some(STILL_DICTATING));
        // The dictation still owns the gate; once it ends, a retry works.
        assert!(dictation.try_begin().is_none());
        drop(running);
        assert!(dictation.try_begin().is_some());
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

    fn release(version: &str) -> tauri_plugin_updater::RemoteRelease {
        serde_json::from_value(serde_json::json!({
            "version": version,
            "platforms": {
                "windows-x86_64": { "signature": "c2ln", "url": "https://example.invalid/setup.exe" }
            }
        }))
        .unwrap()
    }

    /// Whether `from` (the running version) is offered `to`.
    fn offered(from: &str, to: &str) -> bool {
        newer_stable_only()(release(from).version, release(to))
    }

    #[test]
    fn only_a_strictly_newer_version_is_an_update() {
        assert!(offered("0.1.3", "0.1.4"));
        assert!(offered("0.1.9", "0.2.0"));
        assert!(offered("0.9.0", "1.0.0"));
        assert!(offered("0.1.10", "0.1.11"), "numeric, not textual");
        assert!(offered("0.1.3", "v0.1.4"), "the tag's v prefix is fine");
        // The same version, or an older one: nothing to install.
        assert!(!offered("0.1.3", "0.1.3"));
        assert!(!offered("0.1.4", "0.1.3"));
        assert!(!offered("0.2.0", "0.1.99"));
        assert!(!offered("1.0.0", "0.9.9"));
    }

    #[test]
    fn build_metadata_never_makes_an_update() {
        // semver ordering alone would rank `0.1.3+1` above `0.1.3`.
        assert!(release("0.1.3+1").version > release("0.1.3").version);
        assert!(!offered("0.1.3", "0.1.3+1"));
        assert!(!offered("0.1.3+2", "0.1.3+1"));
        assert!(!offered("0.1.3+1", "0.1.3"));
        // A real step still counts with metadata on either side.
        assert!(offered("0.1.3+1", "0.1.4"));
        assert!(offered("0.1.3", "0.1.4+7"));
    }

    #[test]
    fn a_stable_install_is_never_moved_to_a_pre_release() {
        // semver ordering alone would rank `0.2.0-rc.1` above `0.1.3`.
        assert!(release("0.2.0-rc.1").version > release("0.1.3").version);
        assert!(!offered("0.1.3", "0.2.0-rc.1"));
        assert!(!offered("0.1.3", "0.1.4-beta.1"));
        assert!(!offered("0.1.3", "0.1.4-0"));
        // A pre-release does not stand in for its own release either way.
        assert!(!offered("0.1.3", "0.1.3-rc.1"));
    }

    #[test]
    fn a_pre_release_install_follows_pre_releases_and_the_stable_release() {
        assert!(offered("0.2.0-rc.1", "0.2.0-rc.2"));
        assert!(offered("0.2.0-rc.2", "0.2.0-rc.10"), "numeric identifiers");
        assert!(offered("0.2.0-beta.1", "0.2.0-rc.1"));
        assert!(offered("0.2.0-rc.1", "0.2.0"));
        assert!(offered("0.2.0-rc.1", "0.3.0-beta.1"));
        assert!(!offered("0.2.0-rc.1", "0.2.0-rc.1"));
        assert!(!offered("0.2.0-rc.2", "0.2.0-rc.1"));
        assert!(!offered("0.2.0-rc.1", "0.2.0-beta.9"));
        assert!(!offered("0.2.0", "0.2.0-rc.1"));
    }

    /// Not a protection by itself: this is the key `tauri.conf.json` would carry,
    /// checked against the plugin so a typo cannot silently turn it off.
    #[test]
    fn the_plugin_reads_require_signed_version_from_its_config() {
        let config = |extra: &str| -> tauri_plugin_updater::Config {
            serde_json::from_str(&format!(
                r#"{{"pubkey":"k","endpoints":["https://example.invalid/latest.json"]{extra}}}"#
            ))
            .unwrap()
        };
        assert!(!config("").require_signed_version);
        assert!(config(r#","requireSignedVersion":true"#).require_signed_version);
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
        assert!(updater.require_signed_version);
    }
}
