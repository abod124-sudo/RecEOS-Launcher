//! Running in the background and starting with Windows.
//!
//! * **Tray icon.** Always there while the launcher runs. Clicking it brings
//!   the window back. Right-clicking opens a themed panel (a small window,
//!   not a native menu): the network with its servers and players, PLAY with
//!   any download or update in progress, Stella friends to join, the
//!   launcher's pages, the network switch and Quit.
//! * **Close to tray.** With `runInBackground` on (the default), closing the
//!   window hides it instead of quitting, so notifications keep arriving. Quit
//!   from the tray menu ends the process. The updater exits the app itself and
//!   is unaffected.
//! * **Start with Windows.** A per-user `Run` registry value — no admin
//!   rights, and the same value name the NSIS uninstaller already removes.
//!   It launches with [`BACKGROUND_ARG`], which keeps the window hidden.

use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

/// Passed by the startup entry: start in the tray, window hidden.
pub const BACKGROUND_ARG: &str = "--background";

/// The registry value name. Must match `${PRODUCTNAME}` in installer.nsi,
/// whose uninstaller deletes it.
#[cfg(windows)]
const RUN_VALUE_NAME: &str = "Radium Launcher";
#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Whether this process was started by the startup entry.
pub fn started_in_background() -> bool {
    std::env::args().any(|a| a == BACKGROUND_ARG)
}

/// Bring the launcher window back from the tray, minimised or behind.
pub fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

// ── The tray menu ────────────────────────────────────────────────────────
//
// Not a native menu: Windows draws those in its own grey, whatever skin the
// launcher wears, and can only list words. It is a small borderless window
// (traymenu.html) painted with the look the launcher page reports for its own
// menus and controls, showing what the page reports about the launcher's
// state, opened at the cursor on a right-click and hidden again when it loses
// focus.

pub const TRAY_MENU_LABEL: &str = "tray-menu";

/// What the menu shows, as last reported by the launcher page.
#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayState {
    network: String,
    game_running: bool,
    /// The launcher's menu look, measured from its own menus (see
    /// `trayMenuStyle` in app.js). Only ever written into CSS variables.
    style: Option<serde_json::Value>,
    /// What the panel shows: server status, players, PLAY's state and
    /// progress, Stella friends (see `trayInfo` in app.js). Only ever written
    /// in as text, and its pictures only through the thumbnail cache.
    info: Option<serde_json::Value>,
}

static TRAY_STATE: std::sync::Mutex<Option<TrayState>> = std::sync::Mutex::new(None);
/// Where the tray was right-clicked, in physical pixels, while the menu is up.
static TRAY_ANCHOR: std::sync::Mutex<Option<(f64, f64)>> = std::sync::Mutex::new(None);

fn tray_state(app: &AppHandle) -> TrayState {
    TRAY_STATE.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| TrayState {
        network: crate::config::current(app).network.clone(),
        ..Default::default()
    })
}

/// Called by the launcher page when anything the panel shows changes: the
/// network, the game's state, a download's progress, the skin, a friend.
///
/// A menu that is open at the time is told to redraw itself. That is what lets
/// the network rows act as a selection: picking one switches the launcher
/// underneath, and the tick moves to the row that won — along with "Play
/// Radium"/"Play Vanilla" and the Feed row, which only Vanilla has.
#[tauri::command]
pub fn set_tray_state(
    app: AppHandle,
    network: String,
    game_running: bool,
    style: Option<serde_json::Value>,
    info: Option<serde_json::Value>,
) {
    let look = panel_look(style.as_ref());
    *TRAY_STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(TrayState { network, game_running, style, info });
    if tray_menu_is_open(&app) {
        let _ = app.emit_to(TRAY_MENU_LABEL, "tray-menu-update", tray_state(&app));
    } else if let Some(look) = look {
        schedule_warm(&app, look);
    }
}

/// Whether the menu window exists and is on screen.
///
/// The anchor is set for as long as a menu is up (`tray_menu_hide` clears it),
/// so it also covers the moment between the right-click and the page calling
/// `tray_menu_show` back.
fn tray_menu_is_open(app: &AppHandle) -> bool {
    TRAY_ANCHOR.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        && app.get_webview_window(TRAY_MENU_LABEL).is_some()
}

/// When the panel was last put away, for [`tray_panel_in_view`].
static TRAY_CLOSED_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// How long after the panel goes Stella is still treated as looked at, so
/// opening it again a moment later finds the friends connection up and the
/// player count already counting, rather than starting both over.
#[cfg_attr(test, allow(dead_code))]
const TRAY_PANEL_LINGER: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether the tray panel is up, or was a moment ago. The panel shows the
/// launcher's live state (friends, the player count), so while it is up the
/// launcher counts as being looked at even with its window hidden in the tray:
/// see `stella_api::in_use`. (Unit tests have no window to ask about, so
/// nothing calls this there.)
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn tray_panel_in_view() -> bool {
    if TRAY_ANCHOR.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return true;
    }
    TRAY_CLOSED_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|at| at.elapsed() < TRAY_PANEL_LINGER)
}

/// Bring the window back from the page's side (see `playFromTray` in app.js).
#[tauri::command]
pub fn show_launcher(app: AppHandle) {
    show_main(&app);
}

fn tray_menu_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(win) = app.get_webview_window(TRAY_MENU_LABEL) {
        return Some(win);
    }
    let win = tauri::WebviewWindowBuilder::new(app, TRAY_MENU_LABEL, tauri::WebviewUrl::App("traymenu.html".into()))
        .title("Rec EOS Launcher")
        .inner_size(320.0, 480.0)
        // Stays hidden until `tray_menu_show` places it. The pop-up next door
        // is built "visible" far off screen so its webview composites; that
        // was tried here and is not safe, because a builder `position` that
        // far out is not honoured — measured 2026-09-20, the window came up at
        // Windows' own cascade position instead, which would flash an empty
        // 320x480 frame on screen at startup.
        .visible(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .build()
        .ok()?;
    let handle = win.clone();
    win.on_window_event(move |event| {
        // Clicking anywhere else closes it, as a menu does. The page does the
        // hiding, so it can blank itself first (see traymenu.js).
        if let tauri::WindowEvent::Focused(false) = event {
            let _ = handle.emit_to(TRAY_MENU_LABEL, "tray-menu-dismiss", ());
        }
    });
    Some(win)
}

/// Right-click on the tray icon: have the page draw the menu. It measures
/// itself and calls [`tray_menu_show`] back with its size.
fn open_tray_menu(app: &AppHandle, x: f64, y: f64) {
    *TRAY_ANCHOR.lock().unwrap_or_else(|e| e.into_inner()) = Some((x, y));
    if let Some(win) = tray_menu_window(app) {
        // A warm-up still showing off screen is put away first, so the open
        // finds the window hidden, as it always does.
        end_warm(&win);
        let _ = app.emit_to(TRAY_MENU_LABEL, "tray-menu-open", tray_state(app));
    }
    // The launcher brings what the panel shows up to date (see
    // `onTrayPanel` in app.js); the panel redraws as the answers come in.
    let _ = app.emit_to("main", "tray-panel", true);
}

// ── Painting the panel once, off screen ──────────────────────────────────
//
// The first panel of a session took 50-90 ms longer to paint than every one
// after it: its fonts rasterised at each size, the icons, the network art
// decoded, the badges' blur set up on the GPU. All of it landed in the middle
// of the first entrance, which stalled and then jumped to its end — measured
// 2026-10-07 as a 79-93 ms gap between frames, with nothing on the page's
// main thread; a window resize alone, the panel painted before, was smooth.
// `warm_tray_menu` gives the webview its first frame at startup, but of an
// empty page, so none of that work was done there.
//
// So once the launcher has reported how the panel looks, the page draws a
// real panel in the window parked off screen, entrance and all, and the first
// right-click finds the work done. Again whenever the look changes (another
// font, or Liquid Glass), since that is new painting too.

/// The look the panel was last painted in off screen.
static WARMED_LOOK: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// Bumped by every warm-up asked for and every one shown, so a timer that a
/// newer one (or a right-click) has overtaken does nothing.
static WARM_REQUEST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WARM_SHOWN_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Whether a warm-up has the window shown off screen right now.
static WARM_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What decides the painting a panel needs: its font, and whether it is glass.
fn panel_look(style: Option<&serde_json::Value>) -> Option<String> {
    let style = style?;
    let font = style.get("font")?.as_str()?;
    let glass = style.get("glass").and_then(|g| g.as_bool()).unwrap_or(false);
    Some(format!("{font}|{glass}"))
}

/// Have the page paint a panel off screen, unless this look has been painted
/// already. Waits a moment first: for the startup warm-up to have hidden the
/// window again, and for a run of changes (skins tried one after another in
/// Settings) to settle on one.
fn schedule_warm(app: &AppHandle, look: String) {
    {
        let mut warmed = WARMED_LOOK.lock().unwrap_or_else(|e| e.into_inner());
        if warmed.as_deref() == Some(look.as_str()) {
            return;
        }
        *warmed = Some(look);
    }
    let request = WARM_REQUEST.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        // A right-click in the meantime paints the panel for real.
        if WARM_REQUEST.load(std::sync::atomic::Ordering::SeqCst) != request || tray_menu_is_open(&app) {
            return;
        }
        let _ = app.emit_to(TRAY_MENU_LABEL, "tray-menu-warm", tray_state(&app));
    });
}

/// The page has drawn its panel at `width` x `height` (logical pixels): show
/// the window off screen at that size long enough for it to be painted, then
/// hide it again and tell the page it can put the panel away.
///
/// Shown without activating it, unlike a real open: nothing is on screen and
/// nothing should lose the focus. Done with Win32 directly, and undone the
/// same way, because tao's own show would activate the window (only its very
/// first show doesn't, which the startup warm-up has used) — and tao, which
/// never saw this show, is left believing the window hidden, which it is
/// again by the time anything asks it.
#[tauri::command]
pub fn tray_menu_warm(app: AppHandle, width: f64, height: f64) {
    let Some(win) = app.get_webview_window(TRAY_MENU_LABEL) else { return };
    if tray_menu_is_open(&app) || win.is_visible().unwrap_or(true) || WARM_SHOWN.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return;
    }
    // Physical pixels at the scale a menu will most likely open at, so the
    // first real one doesn't have to resize the window either.
    let scale = app.primary_monitor().ok().flatten().map(|m| m.scale_factor()).unwrap_or(1.0);
    let (w, h) = ((width.min(600.0) * scale).round() as i32, (height.min(900.0) * scale).round() as i32);
    if !show_off_screen(&win, w, h) {
        return;
    }
    WARM_SHOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    let gen = WARM_SHOWN_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    tauri::async_runtime::spawn(async move {
        // Long enough for the page to paint and run its entrance once.
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
        if WARM_SHOWN_GEN.load(std::sync::atomic::Ordering::SeqCst) == gen {
            end_warm(&win);
        }
    });
}

/// Put a warm-up away, if one is showing.
fn end_warm(win: &tauri::WebviewWindow) {
    WARM_SHOWN_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if WARM_SHOWN.swap(false, std::sync::atomic::Ordering::SeqCst) {
        hide_off_screen(win);
        let _ = win.emit_to(TRAY_MENU_LABEL, "tray-menu-warm-done", ());
    }
}

#[cfg(windows)]
fn show_off_screen(win: &tauri::WebviewWindow, w: i32, h: i32) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW};
    const OFF_SCREEN: i32 = -32000;
    let Ok(hwnd) = win.hwnd() else { return false };
    // SAFETY: `hwnd` is this live window's handle; the call moves, sizes and
    // shows it without activating it or changing its place in the z-order.
    unsafe {
        SetWindowPos(hwnd.0 as _, std::ptr::null_mut(), OFF_SCREEN, OFF_SCREEN, w, h,
                     SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW) != 0
    }
}

#[cfg(windows)]
fn hide_off_screen(win: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    };
    if let Ok(hwnd) = win.hwnd() {
        // SAFETY: as above; this only hides the window again.
        unsafe {
            SetWindowPos(hwnd.0 as _, std::ptr::null_mut(), 0, 0, 0, 0,
                         SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER);
        }
    }
}

#[cfg(not(windows))]
fn show_off_screen(_win: &tauri::WebviewWindow, _w: i32, _h: i32) -> bool {
    false
}

#[cfg(not(windows))]
fn hide_off_screen(_win: &tauri::WebviewWindow) {}

/// What to draw, for a page that finished loading after the right-click
/// that opened it. `None` when no menu is waiting to open.
#[tauri::command]
pub fn tray_menu_state(app: AppHandle) -> Option<TrayState> {
    let pending = TRAY_ANCHOR.lock().unwrap_or_else(|e| e.into_inner()).is_some();
    pending.then(|| tray_state(&app))
}

/// Where [`tray_menu_show`] put the menu.
#[derive(Clone, Default, serde::Serialize)]
pub struct TrayPlacement {
    /// Liquid Glass only: the blurred screen behind the menu, for the page to
    /// paint as its frost (see the `frost` module).
    frost: Option<String>,
    /// The menu opened upwards from the cursor, so its bottom edge is the one
    /// by the cursor. The window has room for every network's menu (see
    /// `roomFor` in traymenu.js), and the page pins the menu to this edge, so
    /// a shorter menu leaves the far side of the window clear.
    above: bool,
}

/// The page has drawn the menu at `width` x `height` (logical pixels): place
/// it by the cursor, where Windows would have put its own menu, and bring it
/// up with focus so a click elsewhere closes it.
#[tauri::command]
pub fn tray_menu_show(app: AppHandle, width: f64, height: f64, frost: bool) -> Result<TrayPlacement, String> {
    let Some((cx, cy)) = *TRAY_ANCHOR.lock().unwrap_or_else(|e| e.into_inner()) else {
        return Ok(TrayPlacement::default());
    };
    let win = app.get_webview_window(TRAY_MENU_LABEL).ok_or("No tray menu window")?;
    if !(width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0) {
        return Err("Bad tray menu size".into());
    }

    let monitor = app
        .monitor_from_point(cx, cy)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())
        .ok_or("No monitor found")?;
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let (left, top) = (area.position.x as f64, area.position.y as f64);
    let (right, bottom) = (left + area.size.width as f64, top + area.size.height as f64);

    let (mw, mh) = ((width.min(600.0) * scale).round(), (height.min(900.0) * scale).round());

    // Above and to the right of the cursor, flipped where that would leave
    // the screen, then kept inside the work area (off the taskbar).
    let above = cy - mh >= top;
    let mut mx = if cx + mw <= right { cx } else { cx - mw };
    let mut my = if above { cy - mh } else { cy };
    mx = mx.clamp(left, (right - mw).max(left));
    my = my.clamp(top, (bottom - mh).max(top));
    let (x, y) = (mx.round() as i32, my.round() as i32);

    // A menu already on screen is calling back to resize itself, which only
    // a skin change while it is open needs: the window already has room for
    // every network's menu, so switching network doesn't resize it. It keeps
    // the frost it was given, since capturing now would photograph the menu
    // itself; the picture is stretched to the window, and it is a heavy blur.
    let already_up = win.is_visible().unwrap_or(false);

    if already_up {
        // One move-and-resize. As two calls, a menu that opens upwards spends
        // a frame grown downwards from its old top edge before the move
        // catches up.
        resize_in_place(&win, x, y, mw as i32, mh as i32)?;
    } else {
        win.set_size(tauri::PhysicalSize::new(mw as u32, mh as u32)).map_err(|e| e.to_string())?;
        win.set_position(tauri::PhysicalPosition::new(x, y)).map_err(|e| e.to_string())?;
    }
    // Captured while the menu is still hidden, so it isn't in the picture.
    let backdrop = if frost && !already_up { crate::frost::backdrop(x, y, mw as i32, mh as i32) } else { None };
    // Only on the way in. Re-showing and re-focusing a menu that is already up
    // makes Windows treat the resize as a fresh activation, which flickers.
    if !already_up {
        win.show().map_err(|e| e.to_string())?;
        let _ = win.set_always_on_top(true);
        let _ = win.set_focus();
    }
    Ok(TrayPlacement { frost: backdrop, above })
}

#[cfg(windows)]
fn resize_in_place(win: &tauri::WebviewWindow, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
    let hwnd = win.hwnd().map_err(|e| e.to_string())?.0 as _;
    // SAFETY: `hwnd` is this live window's handle; the call only moves and
    // sizes it, without activating it or changing its place in the z-order.
    let ok = unsafe { SetWindowPos(hwnd, std::ptr::null_mut(), x, y, w, h, SWP_NOACTIVATE | SWP_NOZORDER) };
    if ok == 0 {
        return Err("Could not resize the tray menu".into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn resize_in_place(win: &tauri::WebviewWindow, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    win.set_size(tauri::PhysicalSize::new(w as u32, h as u32)).map_err(|e| e.to_string())?;
    win.set_position(tauri::PhysicalPosition::new(x, y)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn tray_menu_hide(app: AppHandle) {
    let was_open = TRAY_ANCHOR.lock().unwrap_or_else(|e| e.into_inner()).take().is_some();
    if was_open {
        *TRAY_CLOSED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
        let _ = app.emit_to("main", "tray-panel", false);
    }
    if let Some(win) = app.get_webview_window(TRAY_MENU_LABEL) {
        let _ = win.hide();
    }
}

/// A choice from the tray menu.
///
/// Settings leave the menu up, the way a radio group or a switch in a menu
/// does, so the panel can show the change land: the network, Screen/VR, the
/// pop-ups switch, and a download's Pause/Resume or Stella's UPDATE, whose
/// progress it then shows. Everything else closes the menu, as choosing an
/// item should.
#[tauri::command]
pub fn tray_menu_pick(app: AppHandle, id: String) {
    if !keeps_menu_open(&id) {
        tray_menu_hide(app.clone());
    }
    on_menu(&app, &id);
}

fn keeps_menu_open(id: &str) -> bool {
    ["network:", "mode:", "popups:", "dl:"].iter().any(|prefix| id.starts_with(prefix))
}

/// Close the tray menu with the launcher, so its hidden window can't keep the
/// process alive after the main window is gone.
pub fn close_tray_menu(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(TRAY_MENU_LABEL) {
        let _ = win.destroy();
    }
}

/// A tray menu choice the page carries out, since the page owns what each one
/// means (the pre-launch checks, the network switch's guards, the tabs).
#[derive(Clone, serde::Serialize)]
struct TrayAction {
    action: &'static str,
    value: String,
}

fn on_menu(app: &AppHandle, id: &str) {
    let action = |action: &'static str, value: &str| {
        let _ = app.emit_to("main", "tray-action", TrayAction { action, value: value.to_string() });
    };
    match id {
        "open" => show_main(app),
        "quit" => app.exit(0),
        // Play stays in the tray, like Steam's game entries: the page reveals
        // the window itself if the launch needs the user (a warning, a
        // missing install, the stop confirmation).
        "play" => action("play", ""),
        // Downloading may ask where to put the game.
        "download" => {
            show_main(app);
            action("download", "");
        }
        "friends" => {
            show_main(app);
            action("friends", "");
        }
        _ => {
            if let Some(tab) = id.strip_prefix("tab:") {
                show_main(app);
                action("tab", tab);
            } else if let Some(friend) = id.strip_prefix("friend:") {
                show_main(app);
                action("friend", friend);
            } else if let Some(thread) = id.strip_prefix("chat:") {
                show_main(app);
                action("chat", thread);
            } else if let Some(friend) = id.strip_prefix("join:") {
                // Like Play: the page brings the window up only if the join
                // needs the user.
                action("join", friend);
            } else if let Some(mode) = id.strip_prefix("mode:") {
                action("mode", mode);
            } else if let Some(on) = id.strip_prefix("popups:") {
                action("popups", on);
            } else if let Some(what) = id.strip_prefix("dl:") {
                action("dl", what);
            } else if let Some(name) = id.strip_prefix("network:") {
                // The window stays where it is: switching network from the
                // menu is a setting, not a reason to interrupt whatever is on
                // screen. The page reveals it itself on the one path that
                // needs the user — a switch it has to refuse, mid-download or
                // with the game running, which it explains in a toast nobody
                // would see behind a hidden window.
                action("network", name);
            }
        }
    }
}

pub fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let mut tray = TrayIconBuilder::with_id("main-tray")
        .tooltip("Rec EOS Launcher")
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } => {
                show_main(tray.app_handle());
            }
            TrayIconEvent::Click { button: MouseButton::Right, button_state: MouseButtonState::Up, position, .. } => {
                open_tray_menu(tray.app_handle(), position.x, position.y);
            }
            _ => {}
        });
    match tray_icon() {
        Some(icon) => tray = tray.icon(icon),
        None => {
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
        }
    }
    tray.build(app)?;
    // Made now, hidden, so the first right-click doesn't wait for a webview.
    if let Some(win) = tray_menu_window(app) {
        warm_tray_menu(win);
    }
    Ok(())
}

/// Give the menu's webview one real frame, off screen, before anyone can open
/// it.
///
/// Building the window early is not enough on its own. A window that has been
/// created but never shown has a webview that has never composited, and the
/// first `show()` is where that surface gets made — so the first menu of a
/// session appeared, sat blank for a beat while the renderer caught up, and
/// only then ran its entrance. Every later menu came from a window that had
/// been shown once already and was merely hidden, so it had a surface ready
/// and opened cleanly. That difference is the one that shows.
///
/// So the window is shown once here, far enough off screen that nothing is
/// visible, and hidden again a moment later. From then on it is in exactly the
/// state every later open finds it in — except for the panel itself, which is
/// not drawn yet at this point; `tray_menu_warm` paints that once the launcher
/// has said how it looks.
///
/// The position is set after the build rather than in the builder: a builder
/// `position` this far out is not honoured (measured 2026-09-20 — the window
/// came up at Windows' own cascade position instead), which would have put an
/// empty 320x480 frame on screen at startup. `set_position` goes through
/// `SetWindowPos`, which takes it.
fn warm_tray_menu(win: tauri::WebviewWindow) {
    const OFF_SCREEN: i32 = -32000;
    if win
        .set_position(tauri::PhysicalPosition::new(OFF_SCREEN, OFF_SCREEN))
        .is_err()
    {
        // Without a position we can trust, showing it would flash on screen.
        return;
    }
    // Verified rather than assumed, for the same reason.
    match win.outer_position() {
        Ok(at) if at.x <= OFF_SCREEN / 2 && at.y <= OFF_SCREEN / 2 => {}
        _ => return,
    }
    if win.show().is_err() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        // Long enough for the webview to paint once, short enough that it is
        // done well before anyone reaches the tray icon.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let _ = win.hide();
    });
}

/// The tray icon, drawn at exactly the size the notification area shows.
///
/// Handed a large image, Windows shrinks it for the tray itself, and badly:
/// that is what made the icon look rough. So the 512px artwork is scaled here
/// with a proper filter to the small-icon size for the display's scaling
/// (16px at 100%, 20 at 125%, 24 at 150%, 32 at 200%).
fn tray_icon() -> Option<tauri::image::Image<'static>> {
    const ARTWORK: &[u8] = include_bytes!("../icons/icon.png");
    let size = tray_icon_size();
    let img = image::load_from_memory(ARTWORK).ok()?.to_rgba8();
    let small = image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3);
    Some(tauri::image::Image::new_owned(small.into_raw(), size, size))
}

#[cfg(windows)]
fn tray_icon_size() -> u32 {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON;
    // SAFETY: both are plain queries with no pointers involved.
    let px = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) };
    if (16..=128).contains(&px) { px as u32 } else { 16 }
}

#[cfg(not(windows))]
fn tray_icon_size() -> u32 {
    32
}

/// Give a window the icon sizes Windows actually draws.
///
/// Tauri hands every window one image — the 256px entry of icon.ico — and
/// Windows then shrinks it itself for the title bar, the taskbar and Alt-Tab,
/// which is what made it look blurry: most visibly in the title bar of the
/// Vanilla sign-in window, the one window with a native frame. `icon.ico`
/// carries every size from 16 to 256, and the build embeds it in the exe as
/// resource 32512, so the right entries are loaded at the exact sizes for the
/// window's display scaling and set on the window instead.
///
/// Posted rather than sent, so it is safe from any thread: the sign-in window
/// is created from an async command, not on the thread that owns it.
#[cfg(windows)]
pub fn sharpen_window_icon(win: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        PostMessageW, ICON_BIG, ICON_SMALL, WM_SETICON,
    };

    let Ok(hwnd) = win.hwnd() else { return };
    let hwnd = hwnd.0 as _;
    // SAFETY: plain queries; `hwnd` is this live window's handle.
    let dpi = match unsafe { GetDpiForWindow(hwnd) } {
        0 => unsafe { GetDpiForSystem() },
        dpi => dpi,
    };
    let Some((small, big)) = app_icons_for_dpi(dpi) else { return };
    // SAFETY: the icons are cached for the life of the process (see
    // `app_icons_for_dpi`), so they outlive the window using them.
    unsafe {
        PostMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small);
        PostMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big);
    }
}

#[cfg(not(windows))]
pub fn sharpen_window_icon(_: &tauri::WebviewWindow) {}

/// Round the window's corners the way Windows 11 rounds every other app's.
///
/// Windows 11 only rounds a window that keeps its standard frame, and this
/// one is undecorated, so it is asked to. Windows 10 has no such attribute
/// and refuses the call, which leaves it square — as it has to be there,
/// where the frame stays a sharp rectangle whatever the page draws (see the
/// square `border-radius` in style.css).
#[cfg(windows)]
pub fn round_window_corners(win: &tauri::WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };

    let Ok(hwnd) = win.hwnd() else { return };
    let preference = DWMWCP_ROUND;
    // SAFETY: `hwnd` is this live window's handle, and the value is a
    // DWM_WINDOW_CORNER_PREFERENCE of exactly the size passed. The error on
    // Windows 10 is the expected answer, so it is ignored.
    unsafe {
        DwmSetWindowAttribute(
            hwnd.0 as _,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &preference as *const _ as *const _,
            std::mem::size_of_val(&preference) as u32,
        );
    }
}

#[cfg(not(windows))]
pub fn round_window_corners(_: &tauri::WebviewWindow) {}

/// The app icon at the small (title bar) and large (Alt-Tab) sizes Windows
/// draws at `dpi`, as raw `HICON`s.
///
/// Loaded once per DPI and never freed. Loading afresh for every window leaked
/// two icon handles each time the sign-in window opened, and a window must not
/// be left holding an icon that has been destroyed.
#[cfg(windows)]
fn app_icons_for_dpi(dpi: u32) -> Option<(isize, isize)> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        LoadImageW, IMAGE_ICON, LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON,
    };
    /// The nameID tauri-winres gives the app icon.
    const ICON_RESOURCE_ID: u32 = 32512;
    static CACHE: std::sync::Mutex<Vec<(u32, isize, isize)>> = std::sync::Mutex::new(Vec::new());

    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, small, big)) = cache.iter().find(|(d, _, _)| *d == dpi) {
        return Some((small, big));
    }
    // SAFETY: the module handle is this exe's; LoadImageW returns null when
    // the resource or size is unavailable, and a null icon is not used.
    let load = |metric| unsafe {
        let px = GetSystemMetricsForDpi(metric, dpi);
        let icon = LoadImageW(
            GetModuleHandleW(std::ptr::null()),
            ICON_RESOURCE_ID as *const u16,
            IMAGE_ICON,
            px,
            px,
            LR_DEFAULTCOLOR,
        );
        (!icon.is_null()).then_some(icon as isize)
    };
    let icons = (load(SM_CXSMICON)?, load(SM_CXICON)?);
    cache.push((dpi, icons.0, icons.1));
    Some(icons)
}

/// Called for the main window's close request. Returns true when the close
/// was turned into a hide.
pub fn hide_instead_of_close(app: &AppHandle) -> bool {
    if !crate::config::current(app).run_in_background {
        return false;
    }
    hide_main(app);
    true
}

/// Hide the launcher window to the tray.
pub fn hide_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
    // The launcher's page decides whether to say where it went (once).
    let _ = app.emit_to("main", "launcher-hidden", ());
}

// ── Start with Windows ───────────────────────────────────────────────────

#[tauri::command]
pub fn get_autostart() -> bool {
    autostart::current().is_some()
}

/// Turn the startup entry on or off. Returns the state it ended up in.
#[tauri::command]
pub fn set_autostart(enabled: bool) -> Result<bool, String> {
    if enabled {
        autostart::set(&startup_command()?)?;
    } else {
        autostart::remove()?;
    }
    Ok(get_autostart())
}

fn startup_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't find the launcher's exe: {}", e))?;
    Ok(format!("\"{}\" {}", exe.display(), BACKGROUND_ARG))
}

/// "Start with Windows" is on by default: the first time an installed build
/// runs, it adds the entry once and records that it did, so turning it off
/// later sticks. After that, an existing entry is re-pointed at this exe in
/// case the launcher was reinstalled somewhere else.
///
/// Skipped in debug builds, which would otherwise register the development
/// exe under target/.
pub fn apply_startup_default(app: &AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    let _lock = crate::config::write_lock();
    // Unreadable right now: the default is applied on a later start instead,
    // rather than recorded over settings this can't see.
    let Ok(mut cfg) = crate::config::ensure_config(app) else { return };
    if !cfg.autostart_initialized {
        let _ = set_autostart(true);
        cfg.autostart_initialized = true;
        let _ = crate::config::save_config(app, &cfg);
        return;
    }
    if let (Some(current), Ok(wanted)) = (autostart::current(), startup_command()) {
        if !current.eq_ignore_ascii_case(&wanted) {
            let _ = autostart::set(&wanted);
        }
    }
}

#[cfg(windows)]
mod autostart {
    use super::{RUN_KEY, RUN_VALUE_NAME};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the handle came from RegCreateKeyExW and is closed once.
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open(access: u32) -> Result<Key, String> {
        let path = wide(RUN_KEY);
        let mut hkey: HKEY = std::ptr::null_mut();
        // SAFETY: all pointers are valid for the call; `hkey` is written on success.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER, path.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE,
                access, std::ptr::null(), &mut hkey, std::ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("Couldn't open the startup registry key (error {})", rc));
        }
        Ok(Key(hkey))
    }

    /// The startup command, if the entry exists.
    pub fn current() -> Option<String> {
        let key = open(KEY_READ).ok()?;
        let name = wide(RUN_VALUE_NAME);
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `bytes` bytes; the value is read as REG_SZ only.
        let rc = unsafe {
            RegGetValueW(
                key.0, std::ptr::null(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(),
                buf.as_mut_ptr().cast(), &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    pub fn set(command: &str) -> Result<(), String> {
        let key = open(KEY_WRITE)?;
        let name = wide(RUN_VALUE_NAME);
        let data = wide(command);
        // SAFETY: `data` is a NUL-terminated UTF-16 string of the given byte length.
        let rc = unsafe {
            RegSetValueExW(
                key.0, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("Couldn't add the startup entry (error {})", rc));
        }
        Ok(())
    }

    pub fn remove() -> Result<(), String> {
        let key = open(KEY_WRITE)?;
        let name = wide(RUN_VALUE_NAME);
        // SAFETY: `name` is NUL-terminated.
        let rc = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
        if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
            return Err(format!("Couldn't remove the startup entry (error {})", rc));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod autostart {
    pub fn current() -> Option<String> {
        None
    }
    pub fn set(_: &str) -> Result<(), String> {
        Err("Starting with the system is only supported on Windows".into())
    }
    pub fn remove() -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod icon_tests {
    #[test]
    fn tray_icon_is_drawn_at_the_tray_size() {
        let icon = super::tray_icon().expect("artwork decodes");
        let size = super::tray_icon_size();
        assert_eq!((icon.width(), icon.height()), (size, size));
        assert_eq!(icon.rgba().len(), (size * size * 4) as usize);
        // Not blank: the logo's green survives the scaling.
        assert!(icon.rgba().chunks(4).any(|p| p[1] > 150 && p[3] > 200));
    }
}

#[cfg(test)]
mod menu_tests {
    use super::{keeps_menu_open, panel_look};

    #[test]
    fn the_panel_is_repainted_off_screen_only_for_a_new_font_or_glass() {
        let look = |v: serde_json::Value| panel_look(Some(&v));
        let inter = look(serde_json::json!({ "font": "Inter", "glass": false, "bg": "#111" }));
        assert_eq!(inter, look(serde_json::json!({ "font": "Inter", "glass": false, "bg": "#fff" })));
        assert_ne!(inter, look(serde_json::json!({ "font": "Inter", "glass": true })));
        assert_ne!(inter, look(serde_json::json!({ "font": "Tahoma", "glass": false })));
        // Nothing reported yet: nothing to paint with.
        assert_eq!(panel_look(None), None);
        assert_eq!(look(serde_json::json!({ "glass": true })), None);
    }

    #[test]
    fn settings_keep_the_menu_up_and_commands_close_it() {
        for id in ["network:stella", "mode:vr", "popups:off", "dl:pause", "dl:update"] {
            assert!(keeps_menu_open(id), "{id}");
        }
        for id in ["play", "download", "open", "quit", "tab:rooms", "join:42", "friend:42", "friends"] {
            assert!(!keeps_menu_open(id), "{id}");
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::autostart;

    /// Writes the real per-user Run key, so it only runs when asked for
    /// (`cargo test -- --ignored`), and puts back whatever was there.
    #[test]
    #[ignore]
    fn startup_entry_round_trips_and_is_restored() {
        let before = autostart::current();
        autostart::set(r#""C:\radium-test\radium-launcher.exe" --background"#).unwrap();
        assert_eq!(
            autostart::current().as_deref(),
            Some(r#""C:\radium-test\radium-launcher.exe" --background"#)
        );
        autostart::remove().unwrap();
        assert_eq!(autostart::current(), None);
        // Removing twice is fine.
        autostart::remove().unwrap();
        if let Some(cmd) = before {
            autostart::set(&cmd).unwrap();
        }
    }
}
