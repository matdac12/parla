//! Owns the tray icon: its right-click menu, its left-click flyout, and the
//! lifecycle of the windows they open.

use crate::log::log_error;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use tauri::{
    menu::{IsMenuItem, Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, Rect, WebviewUrl,
    WebviewWindowBuilder, WindowEvent,
};

/// The right-click menu, top to bottom, as `(id, label)`. The first item is
/// the default (bold on Windows). History and Timings live in the app window;
/// Benchmark is linked from its sidebar rail.
const MENU: [(&str, &str); 6] = [
    ("open", "Open Parla"),
    ("settings", "Settings…"),
    ("test_mic", "Test mic (3 s)"),
    ("copy_last", "Copy last transcription"),
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
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                on_tray_click(tray.app_handle(), button, rect);
            }
        })
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => open_app(app, "home"),
            "settings" => open_app(app, "settings"),
            "test_mic" => run_test_mic(app),
            "copy_last" => copy_last_transcription(app),
            RETRY => crate::commands::failure::retry_from_tray(app.clone()),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    // Only the recording overlay is kept warm. Secondary windows are created
    // by open_app/open_bench and destroyed normally when the user closes them;
    // the flyout is created per left-click and destroyed when it loses focus.
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

/// Opens the setup wizard: `first_run` on a new install (defaults on, as the
/// page says), otherwise a rerun that starts from the current settings.
pub(crate) fn open_setup(app: &tauri::AppHandle, first_run: bool) {
    queue_window(
        app,
        crate::setup::LABEL,
        if first_run { "first" } else { "again" },
    );
}

fn queue_window(app: &tauri::AppHandle, label: &str, view: &str) {
    let handle = app.clone();
    let label = label.to_owned();
    let view = view.to_owned();
    // Serialize creation on the event loop: simultaneous tray/IPC requests
    // cannot create duplicate labels. Post from a worker: on the main thread
    // (a tray callback, a sync command) `run_on_main_thread` runs the task
    // inline, and a window built inside an IPC handler never shows.
    tauri::async_runtime::spawn(async move {
        let app = handle.clone();
        if let Err(error) =
            app.run_on_main_thread(move || match ensure_window(&handle, &label, &view) {
                Ok(window) => {
                    if let Err(error) = window
                        .unminimize()
                        .and_then(|()| window.show())
                        .and_then(|()| window.set_focus())
                    {
                        log_error!("failed to show {label}: {error}");
                    }
                }
                Err(error) => log_error!("failed to open {label}: {error}"),
            })
        {
            log_error!("failed to schedule window: {error}");
        }
    });
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
        "setup" => ("Parla setup", format!("setup.html#{view}"), 560.0, 660.0),
        _ => return Err(tauri::Error::WindowNotFound),
    };
    let mut builder = WebviewWindowBuilder::new(app, label, WebviewUrl::App(url.into()))
        .title(title)
        .inner_size(width, height)
        .visible(false);
    if label == crate::setup::LABEL {
        builder = builder.center();
    }
    let window = builder.build()?;
    if label == crate::setup::LABEL {
        let handle = app.clone();
        window.on_window_event(move |event| {
            if matches!(event, WindowEvent::Destroyed) {
                // Stopping the meter waits on the audio worker and the settings
                // write takes the save lock: keep both off the event loop.
                let handle = handle.clone();
                std::thread::spawn(move || crate::setup::closed(&handle));
            }
        });
    }
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
pub(crate) const FLYOUT: &str = "flyout";
/// The flyout's logical width; its height comes from the page (`show_flyout`).
const FLYOUT_WIDTH: f64 = 360.0;
/// Logical gap to the work-area edge, as Windows leaves around its own flyouts.
const FLYOUT_MARGIN: f64 = 12.0;

/// How long a hidden flyout may load before another click replaces it, rather
/// than being taken for a double-click's second release.
const LOAD_GRACE: Duration = Duration::from_secs(3);
/// How long after a press on the icon blurred the flyout its release may come.
const CLOSE_CLICK: Duration = Duration::from_secs(1);

/// The icon the flyout opened from, when that click happened (until the
/// flyout shows), and when the flyout last closed with the cursor on the icon.
struct FlyoutState {
    icon: Option<Bounds>,
    clicked_at: Option<Instant>,
    closed_on_icon_at: Option<Instant>,
}

static FLYOUT_STATE: Mutex<FlyoutState> = Mutex::new(FlyoutState {
    icon: None,
    clicked_at: None,
    closed_on_icon_at: None,
});

fn flyout_state() -> std::sync::MutexGuard<'static, FlyoutState> {
    FLYOUT_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A button released on the tray icon. A left-click opens the flyout, or
/// closes it when it is open. Runs on the event loop, like every tray
/// callback, so creation is serialized.
fn on_tray_click(app: &tauri::AppHandle, button: MouseButton, icon: Rect) {
    let now = Instant::now();
    let (spent, loading) = {
        let mut state = flyout_state();
        (
            // Pressing the icon while the flyout is open blurs it, which
            // closes it before this release arrives: that click is spent.
            within(state.closed_on_icon_at.take(), now, CLOSE_CLICK),
            within(state.clicked_at, now, LOAD_GRACE),
        )
    };
    if spent || button != MouseButton::Left {
        return;
    }
    if let Some(window) = app.get_webview_window(FLYOUT) {
        // A visible flyout closes. A hidden one still loading is left alone
        // (a double-click's second release); one that never showed is dropped.
        if window.is_visible().unwrap_or(false) || !loading {
            let _ = window.destroy();
        }
        return;
    }
    // The tray reports the icon in physical pixels already.
    let position = icon.position.to_physical::<i32>(1.0);
    let size = icon.size.to_physical::<i32>(1.0);
    {
        let mut state = flyout_state();
        state.icon = Some(Bounds {
            left: position.x,
            top: position.y,
            right: position.x + size.width,
            bottom: position.y + size.height,
        });
        state.clicked_at = Some(now);
    }
    // Hidden until the page has rendered and reported its height, so it never
    // flashes empty or at the wrong size.
    let built = WebviewWindowBuilder::new(app, FLYOUT, WebviewUrl::App("flyout.html".into()))
        .title("Parla")
        .inner_size(FLYOUT_WIDTH, 400.0)
        .decorations(false)
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .focused(false)
        .visible(false)
        .build();
    let window = match built {
        Ok(window) => window,
        Err(error) => return log_error!("failed to open the flyout: {error}"),
    };
    // A borderless window that keeps its shadow is sized by tao from the
    // frame it measures, which on a window never shown still counts the
    // caption (+43 px at 150 %) until a resize refreshes it. Resize now, while
    // the page loads, so `show_flyout` lands on the right size.
    let _ = window.set_size(LogicalSize::new(FLYOUT_WIDTH, 400.0));
    let target = window.clone();
    window.on_window_event(move |event| {
        // Focus changes before `show_flyout` belong to the hidden window
        // being created, not to the user clicking away.
        if matches!(event, WindowEvent::Focused(false)) && target.is_visible().unwrap_or(false) {
            let cursor = target.app_handle().cursor_position();
            {
                let mut state = flyout_state();
                state.closed_on_icon_at = match (state.icon, cursor) {
                    (Some(icon), Ok(at)) if icon.contains(at.x as i32, at.y as i32) => {
                        Some(Instant::now())
                    }
                    _ => None,
                };
            }
            let _ = target.destroy();
        }
    });
}

/// Called by the page once it has rendered: sizes the flyout to `height`
/// logical px, puts it in the screen corner by the tray and shows it focused,
/// so that a click anywhere else blurs and destroys it.
pub(crate) fn show_flyout(window: &tauri::WebviewWindow, height: f64) -> tauri::Result<()> {
    let (icon, clicked_at) = {
        let mut state = flyout_state();
        (state.icon, state.clicked_at.take())
    };
    let icon = icon.ok_or(tauri::Error::WindowNotFound)?;
    let taskbar = taskbar();
    let anchor = taskbar.map_or(icon, |bar| bar.bounds);
    let monitor = match window.monitor_from_point(
        f64::from(anchor.left + anchor.right) / 2.0,
        f64::from(anchor.top + anchor.bottom) / 2.0,
    )? {
        Some(monitor) => monitor,
        None => window
            .primary_monitor()?
            .ok_or(tauri::Error::WindowNotFound)?,
    };
    let scale = monitor.scale_factor();
    let (position, size) = (monitor.position(), monitor.size());
    let screen = Bounds {
        left: position.x,
        top: position.y,
        right: position.x + size.width as i32,
        bottom: position.y + size.height as i32,
    };
    let work = monitor.work_area();
    let work = Bounds {
        left: work.position.x,
        top: work.position.y,
        right: work.position.x + work.size.width as i32,
        bottom: work.position.y + work.size.height as i32,
    };
    let (area, edge) = tray_area(screen, work, taskbar);
    let height = height.clamp(1.0, f64::from(area.bottom - area.top) / scale);
    let (width, height) = (
        (FLYOUT_WIDTH * scale).round() as i32,
        (height * scale).round() as i32,
    );
    let (x, y) = flyout_origin(
        screen,
        area,
        edge,
        (width, height),
        (FLYOUT_MARGIN * scale).round() as i32,
    );
    // Move first: moving onto a monitor with another scale would rescale a
    // window already sized for it.
    window.set_position(PhysicalPosition::new(x, y))?;
    window.set_size(PhysicalSize::new(width, height))?;
    window.show()?;
    window.set_focus()?;
    // The page calls this again when its content changes; time the first.
    if let Some(at) = clicked_at {
        eprintln!(
            "flyout: shown {} ms after the click",
            at.elapsed().as_millis()
        );
    }
    Ok(())
}

fn within(at: Option<Instant>, now: Instant, limit: Duration) -> bool {
    at.is_some_and(|at| now.saturating_duration_since(at) < limit)
}

/// A screen rectangle in physical pixels; `right` and `bottom` are exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Bounds {
    fn contains(self, x: i32, y: i32) -> bool {
        (self.left..self.right).contains(&x) && (self.top..self.bottom).contains(&y)
    }
}

/// The screen edge the taskbar is docked to.
// Only Windows reports a taskbar; elsewhere the tests build these.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Left,
    Top,
    Right,
    Bottom,
}

/// The taskbar's edge and its rect, reported the same whether it shows or is
/// auto-hidden.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Taskbar {
    edge: Edge,
    bounds: Bounds,
}

#[cfg(windows)]
fn taskbar() -> Option<Taskbar> {
    use windows::Win32::UI::Shell::{
        SHAppBarMessage, ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETTASKBARPOS, APPBARDATA,
    };
    let mut data = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    // SAFETY: `data` is a valid APPBARDATA with `cbSize` set, as required.
    if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut data) } == 0 {
        return None;
    }
    let edge = match data.uEdge {
        ABE_LEFT => Edge::Left,
        ABE_TOP => Edge::Top,
        ABE_RIGHT => Edge::Right,
        ABE_BOTTOM => Edge::Bottom,
        _ => return None,
    };
    let rc = data.rc;
    Some(Taskbar {
        edge,
        bounds: Bounds {
            left: rc.left,
            top: rc.top,
            right: rc.right,
            bottom: rc.bottom,
        },
    })
}

#[cfg(not(windows))]
fn taskbar() -> Option<Taskbar> {
    None
}

/// The area the flyout sits in and the edge it hugs: the `screen` minus the
/// taskbar's thickness on its edge, which is the work area while the taskbar
/// shows, whether it is auto-hidden or not. Without a taskbar, the `work`
/// area and its bottom edge.
fn tray_area(screen: Bounds, work: Bounds, taskbar: Option<Taskbar>) -> (Bounds, Edge) {
    let Some(Taskbar { edge, bounds: bar }) = taskbar else {
        return (work, Edge::Bottom);
    };
    let mut area = screen;
    match edge {
        Edge::Left => area.left += bar.right - bar.left,
        Edge::Top => area.top += bar.bottom - bar.top,
        Edge::Right => area.right -= bar.right - bar.left,
        Edge::Bottom => area.bottom -= bar.bottom - bar.top,
    }
    (area, edge)
}

/// Top-left corner for a `size` flyout `margin` px inside the corner of
/// `area` where the tray is: against the taskbar `edge`, at its right end (its
/// bottom end on a vertical taskbar), like the flyouts Windows opens from the
/// tray. A flyout larger than the `screen` keeps its top-left edge on screen.
fn flyout_origin(
    screen: Bounds,
    area: Bounds,
    edge: Edge,
    size: (i32, i32),
    margin: i32,
) -> (i32, i32) {
    let (width, height) = size;
    let x = if edge == Edge::Left {
        area.left + margin
    } else {
        area.right - margin - width
    };
    let y = if edge == Edge::Top {
        area.top + margin
    } else {
        area.bottom - margin - height
    };
    let fit = |value: i32, low: i32, high: i32| value.min(high).max(low);
    (
        fit(x, screen.left + margin, screen.right - margin - width),
        fit(y, screen.top + margin, screen.bottom - margin - height),
    )
}

/// Puts the newest successful dictation's text on the clipboard. Off the menu
/// callback: the history read waits on its writer thread.
fn copy_last_transcription(app: &tauri::AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let last = app
            .try_state::<crate::history::History>()
            .and_then(|history| {
                history
                    .load_recent_kinds(20, &["dictation"])
                    .into_iter()
                    .find(|row| row.error.is_none() && !row.text.trim().is_empty())
            });
        match last {
            Some(row) => {
                if let Err(error) = crate::inserter::copy_text(&row.text) {
                    log_error!("copy last transcription: {error}");
                }
            }
            None => log_error!("copy last transcription: no transcription in history"),
        }
    });
}

/// Records three seconds from the default microphone, saves it as
/// `clips/test.wav` in the app data dir, and logs the path. Everything runs on
/// a spawned thread: acquiring the recorder can block while the device opens,
/// and the menu callback must not.
pub(crate) fn run_test_mic(app: &tauri::AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let Some(recorder) = crate::recorder::recorder() else {
            log_error!("test mic: no input device available");
            return;
        };
        if let Err(error) = recorder.start() {
            log_error!("test mic: not starting ({error:#})");
            return;
        }
        std::thread::sleep(Duration::from_secs(3));
        let clip = recorder.stop();

        let path = match app.path().app_data_dir() {
            Ok(dir) => dir.join("clips").join("test.wav"),
            Err(error) => {
                log_error!("test mic: resolving the app data dir failed: {error}");
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
            Err(error) => log_error!("test mic: saving failed: {error:#}"),
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
mod flyout_tests {
    use super::*;

    const SCREEN: Bounds = Bounds {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    fn bar(edge: Edge, (left, top, right, bottom): (i32, i32, i32, i32)) -> Option<Taskbar> {
        Some(Taskbar {
            edge,
            bounds: Bounds {
                left,
                top,
                right,
                bottom,
            },
        })
    }

    fn place(work: Bounds, taskbar: Option<Taskbar>) -> (i32, i32) {
        let (area, edge) = tray_area(SCREEN, work, taskbar);
        flyout_origin(SCREEN, area, edge, (360, 500), 12)
    }

    #[test]
    fn bottom_taskbar_puts_the_flyout_in_the_bottom_right_corner_above_it() {
        let work = Bounds {
            bottom: 1032,
            ..SCREEN
        };
        let taskbar = bar(Edge::Bottom, (0, 1032, 1920, 1080));
        assert_eq!(place(work, taskbar), (1548, 520));
        // Auto-hidden: the work area is the whole screen, the corner is the same.
        assert_eq!(place(SCREEN, taskbar), (1548, 520));
        // Reported where a hidden bar slid to: only its thickness counts.
        let slid = bar(Edge::Bottom, (0, 1078, 1920, 1126));
        assert_eq!(place(SCREEN, slid), (1548, 520));
    }

    #[test]
    fn other_taskbar_edges_get_their_own_tray_corner() {
        let top = bar(Edge::Top, (0, 0, 1920, 48));
        assert_eq!(place(SCREEN, top), (1548, 60));
        let left = bar(Edge::Left, (0, 0, 48, 1080));
        assert_eq!(place(SCREEN, left), (60, 568));
        let right = bar(Edge::Right, (1872, 0, 1920, 1080));
        assert_eq!(place(SCREEN, right), (1500, 568));
    }

    #[test]
    fn without_a_taskbar_the_work_area_bottom_right_is_used() {
        let work = Bounds {
            right: 1800,
            bottom: 1000,
            ..SCREEN
        };
        assert_eq!(place(work, None), (1428, 488));
    }

    #[test]
    fn a_flyout_larger_than_the_screen_keeps_its_top_left_on_screen() {
        let small = Bounds {
            left: 0,
            top: 0,
            right: 300,
            bottom: 400,
        };
        let taskbar = bar(Edge::Bottom, (0, 352, 300, 400));
        let (area, edge) = tray_area(small, small, taskbar);
        assert_eq!(flyout_origin(small, area, edge, (360, 500), 12), (12, 12));
    }

    #[test]
    fn recent_events_count_only_inside_their_window() {
        let now = Instant::now();
        assert!(!within(None, now, CLOSE_CLICK));
        assert!(within(
            Some(now),
            now + Duration::from_millis(300),
            CLOSE_CLICK
        ));
        assert!(!within(Some(now), now + CLOSE_CLICK, CLOSE_CLICK));
    }

    #[test]
    fn bounds_contain_their_top_left_edges_but_not_bottom_right() {
        let icon = Bounds {
            left: 10,
            top: 20,
            right: 30,
            bottom: 40,
        };
        assert!(icon.contains(10, 20));
        assert!(icon.contains(29, 39));
        assert!(!icon.contains(30, 25));
        assert!(!icon.contains(15, 40));
        assert!(!icon.contains(9, 25));
    }
}

#[cfg(test)]
mod menu_tests {
    use super::MENU;

    #[test]
    fn right_click_menu_is_the_six_items_with_open_first() {
        let labels: Vec<_> = MENU.iter().map(|&(_, label)| label).collect();
        assert_eq!(
            labels,
            [
                "Open Parla",
                "Settings…",
                "Test mic (3 s)",
                "Copy last transcription",
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
                let plain = crate::overlay::begin(&handle, false, Duration::ZERO);
                std::thread::sleep(Duration::from_millis(400));
                let plain_bottom = overlay.outer_position().unwrap().y
                    + overlay.inner_size().unwrap().height as i32;
                drop(plain);
                let captioned = crate::overlay::begin(&handle, true, Duration::ZERO);
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
