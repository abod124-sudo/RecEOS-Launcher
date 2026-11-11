//! The notification pop-up: a small always-on-top window in the corner of the
//! screen for new Vanilla notifications, the way Steam's appear. It is the
//! only place they pop up; the launcher itself never shows them.
//!
//! The main window decides what to show and hands over ready-made cards
//! (text only, rendered with `textContent` on the other side). This module
//! queues them, makes sure the window exists, and tells it to collect them.
//! The pop-up page reports how tall its stack is, and this module sizes and
//! places the window to match, or hides it when the stack is empty.
//!
//! One window holds the whole stack. A window per card was tried, so that
//! Windows' own blur could be applied behind each one, and it was far too
//! heavy: every card meant creating a webview, and the stack was animated by
//! moving windows. The glass look is done in the page instead.
//!
//! **It never takes focus.** The window is created non-focusable, and it is
//! shown and hidden with `SW_SHOWNOACTIVATE` / `SWP_NOACTIVATE` directly
//! rather than through Tauri's show(), which activates the window on Windows.
//! A card popping up must not pull the player out of the game.

use serde_json::Value;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const POPUP_LABEL: &str = "notif-popup";

/// Width of the pop-up window, in logical pixels: a 320px card plus room for
/// its shadow.
const POPUP_WIDTH: f64 = 356.0;
/// Gap between the window and the edge of the work area (above the taskbar).
const EDGE_GAP: f64 = 8.0;
/// Cards held for the page at most; a flood beyond this is dropped.
const QUEUE_CAP: usize = 10;

static QUEUE: Mutex<Vec<Value>> = Mutex::new(Vec::new());

fn popup_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(win) = app.get_webview_window(POPUP_LABEL) {
        return Ok(win);
    }
    let win = WebviewWindowBuilder::new(app, POPUP_LABEL, WebviewUrl::App("popup.html".into()))
        .title("Radium notifications")
        .inner_size(POPUP_WIDTH, 10.0)
        // Created "visible" (so its webview renders) but far off screen, and
        // hidden straight away below. Visibility from then on is handled with
        // raw calls that never activate it.
        .position(-32000.0, -32000.0)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .focused(false)
        .focusable(false)
        .build()
        .map_err(|e| format!("Couldn't create the notification window: {}", e))?;
    set_shown(&win, None);
    Ok(win)
}

/// Queue cards for the pop-up and wake it.
#[tauri::command]
pub async fn desktop_notify(app: AppHandle, cards: Vec<Value>) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        // notify-send talks to the desktop's notification service over D-Bus;
        // no focus changes or compositor-dependent popup coordinates.
        let cfg = crate::config::current(&app);
        if !cfg.notif_popups || crate::game::game_running(&app) { return Ok(()); }
        for card in cards.into_iter().take(QUEUE_CAP) {
            let title = card["app"].as_str().unwrap_or("Rec EOS Launcher");
            let text: String = card["parts"].as_array().map(|parts| parts.iter()
                .filter_map(|p| p["t"].as_str()).collect::<String>()).unwrap_or_default();
            let text = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            let mut cmd = tokio::process::Command::from(crate::linux::system_command("notify-send"));
            cmd.args(["--app-name=Rec EOS Launcher", "--icon=rec-eos-launcher", "--expire-time=8000"]);
            if !cfg.notif_sound { cmd.arg("--hint=boolean:suppress-sound:true"); }
            cmd.arg("--").arg(title.chars().take(128).collect::<String>()).arg(text.chars().take(2048).collect::<String>());
            cmd.kill_on_drop(true);
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output()).await
                .map_err(|_| "The desktop notification service did not respond.".to_string())?
                .map_err(|e| format!("Cannot send desktop notification. Install libnotify-bin: {e}"))?;
            if !result.status.success() { return Err("The desktop notification service refused the notification.".into()); }
        }
        return Ok(());
    }
    if cards.is_empty() {
        return Ok(());
    }
    {
        let mut q = QUEUE.lock().map_err(|_| "queue poisoned")?;
        q.extend(cards);
        let excess = q.len().saturating_sub(QUEUE_CAP);
        q.drain(..excess);
    }
    let _win = popup_window(&app)?;
    // A page that is still loading collects the queue itself once it's up.
    let _ = app.emit_to(POPUP_LABEL, "desktop-notif-ready", ());
    Ok(())
}

/// Hand the queued cards to the pop-up page.
#[tauri::command]
pub fn desktop_notif_take() -> Vec<Value> {
    QUEUE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

/// Whether the pop-up window is on screen.
static SHOWN: Mutex<bool> = Mutex::new(false);

/// The frost behind the pop-up under Liquid Glass: the blurred screen under
/// the window, captured as it appears (see the `frost` module). Sized in
/// logical pixels, and pinned to the window's bottom like the cards are.
#[derive(serde::Serialize)]
pub struct Backdrop {
    url: String,
    width: f64,
    height: f64,
}

/// Size the window to the page's card stack and pin it to the bottom-right of
/// the screen, above the taskbar; hide it when the stack is empty.
///
/// With `frost` on, a window coming on screen returns its backdrop. One
/// already showing keeps the backdrop it has: capturing now would picture
/// the cards themselves.
#[tauri::command]
pub fn desktop_notif_layout(app: AppHandle, height: f64, frost: bool) -> Result<Option<Backdrop>, String> {
    let Some(win) = app.get_webview_window(POPUP_LABEL) else { return Ok(None) };
    // Written as a positive test of both failure modes rather than as a
    // negated `>`: the page computes this height, so a zero, a negative or a
    // NaN all have to hide the window rather than resize it to nonsense.
    if !height.is_finite() || height <= 0.0 {
        set_shown(&win, None);
        *SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = false;
        return Ok(None);
    }

    // The screen the launcher is on, or the primary one while it's minimised
    // (a minimised window reports no useful monitor).
    let main = app.get_webview_window("main");
    let minimized = main.as_ref().and_then(|m| m.is_minimized().ok()).unwrap_or(false);
    let monitor = if minimized { None } else { main.as_ref().and_then(|m| m.current_monitor().ok().flatten()) }
        .or_else(|| app.primary_monitor().ok().flatten())
        .ok_or("No monitor found")?;

    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let w = (POPUP_WIDTH * scale).round() as i32;
    let h = ((height.min(1200.0) + EDGE_GAP) * scale).round() as i32;
    let gap = (EDGE_GAP * scale).round() as i32;
    let x = area.position.x + area.size.width as i32 - w - gap;
    let y = area.position.y + area.size.height as i32 - h;
    let was_shown = std::mem::replace(&mut *SHOWN.lock().unwrap_or_else(|e| e.into_inner()), true);
    let backdrop = if frost && !was_shown {
        crate::frost::backdrop(x, y, w, h).map(|url| Backdrop {
            url,
            width: w as f64 / scale,
            height: h as f64 / scale,
        })
    } else {
        None
    };
    set_shown(&win, Some((x, y, w, h)));
    Ok(backdrop)
}

/// A card was clicked: bring the launcher forward and let it open the thing.
#[tauri::command]
pub fn desktop_notif_open(app: AppHandle, card: Value) {
    crate::background::show_main(&app);
    let _ = app.emit_to("main", "desktop-notif-open", card);
}

/// Close the pop-up with the launcher, so a hidden pop-up can't keep the
/// process alive after the main window is gone.
pub fn close_popup(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(POPUP_LABEL) {
        let _ = win.destroy();
    }
}

#[cfg(windows)]
fn set_shown(win: &tauri::WebviewWindow, bounds: Option<(i32, i32, i32, i32)>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE,
    };
    let Ok(hwnd) = win.hwnd() else { return };
    let hwnd = hwnd.0 as _;
    // SAFETY: `hwnd` is this live window's handle; both calls only move, size
    // and show or hide it, and neither activates it.
    unsafe {
        match bounds {
            Some((x, y, w, h)) => {
                SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
            None => {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

#[cfg(not(windows))]
fn set_shown(win: &tauri::WebviewWindow, bounds: Option<(i32, i32, i32, i32)>) {
    match bounds {
        Some((x, y, w, h)) => {
            let _ = win.set_size(tauri::PhysicalSize::new(w as u32, h as u32));
            let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
            let _ = win.show();
        }
        None => {
            let _ = win.hide();
        }
    }
}
