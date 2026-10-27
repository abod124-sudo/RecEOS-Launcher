//! Signing in to Vanilla, and the few `/api/website/*` calls that need it.
//!
//! ## How the session is kept away from everything that could leak it
//!
//! * **The password never touches the launcher.** Signing in happens on
//!   Vanilla's own `/login/` page, in a separate window. That window is
//!   private (incognito), so its cookie jar lives in memory and is never shared
//!   with the main window. It loads a remote origin and no capability grants
//!   that window IPC, so the page can't call a single launcher command.
//! * **The cookie never reaches JavaScript.** Rust reads it out of the login
//!   window (HttpOnly cookies included) the moment the site redirects after a
//!   successful sign-in, then closes the window. The main window only ever
//!   receives a whitelisted summary: a name, an avatar, a token count. Nothing
//!   the frontend holds or logs — and so nothing a bug report can carry —
//!   contains the session.
//! * **It is sent to exactly one place.** Every authenticated request goes to
//!   `https://vanillarec.net/api.php` on a dedicated client that is HTTPS-only,
//!   follows no redirects and keeps no cookie jar, with the header marked
//!   sensitive so it is never printed by `Debug`.
//! * **At rest it is encrypted to the Windows user** with DPAPI
//!   (`CryptProtectData`), in the app's local (not roaming) data directory.
//!   Another Windows account on the same machine can't decrypt it, and a copy
//!   of the file taken to another machine is useless.
//! * **It is wiped** from memory on drop ([`Zeroizing`]), and from disk and
//!   Vanilla's server on logout. A 401 from Vanilla clears it too.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::webview::{Cookie, NewWindowResponse};
use tauri::{AppHandle, Emitter, Manager, Url, WebviewUrl, WebviewWindowBuilder};
use zeroize::Zeroizing;

use crate::server::USER_AGENT;
use crate::vanilla::{self, SITE_BASE, SITE_REFERER};

const LOGIN_LABEL: &str = "vanilla-login";
const LOGIN_URL: &str = "https://vanillarec.net/login/";
const SITE_HOST: &str = "vanillarec.net";

/// Emitted to the main window (only) whenever the signed-in state changes.
const AUTH_EVENT: &str = "vanilla-auth-changed";

/// File name of the DPAPI-encrypted session, under the app's local data dir.
const SESSION_FILE: &str = "vanilla-session.bin";

/// Mixed into the DPAPI encryption. Not a secret; it only stops another
/// program running as the same user from decrypting the file with a bare
/// `CryptUnprotectData` call that doesn't know it.
const DPAPI_ENTROPY: &[u8] = b"radium-launcher/vanilla-session/v1";

// ── The secret ───────────────────────────────────────────────────────────

/// The `Cookie` header value for a signed-in Vanilla session.
///
/// Deliberately not `Clone`, `Serialize` or `Display`, and `Debug` is
/// redacted, so it can't end up in a log line, an error string or an IPC
/// payload by accident. Shared through an `Arc` and zeroed when the last
/// reference drops.
struct SessionCookie(Zeroizing<String>);

impl fmt::Debug for SessionCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionCookie(<redacted>)")
    }
}

impl SessionCookie {
    fn header_value(&self) -> Result<reqwest::header::HeaderValue, String> {
        let mut v = reqwest::header::HeaderValue::from_str(&self.0)
            .map_err(|_| "Stored Vanilla session is malformed".to_string())?;
        v.set_sensitive(true);
        Ok(v)
    }
}

#[derive(Default)]
struct State {
    /// `None` until the store has been read once.
    loaded: bool,
    cookie: Option<Arc<SessionCookie>>,
    /// Last known account summary (not secret), so a transient network error
    /// doesn't make the UI look signed out.
    player: Option<Value>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

fn session_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_local_data_dir().ok().map(|d| d.join(SESSION_FILE))
}

/// The current session, reading it from disk the first time.
fn current(app: &AppHandle) -> Option<Arc<SessionCookie>> {
    let mut st = state().lock().ok()?;
    if !st.loaded {
        st.loaded = true;
        st.cookie = session_path(app)
            .and_then(|p| store::load(&p))
            .map(|s| Arc::new(SessionCookie(s)));
    }
    st.cookie.clone()
}

// The file is written and deleted under the state lock, so a sign-in and a
// sign-out landing together can't leave the file saying one thing and memory
// the other.

fn set_session(app: &AppHandle, cookie: Zeroizing<String>, player: Value) {
    let Ok(mut st) = state().lock() else { return };
    if let Some(p) = session_path(app) {
        // Losing persistence only means signing in again next launch, so a
        // failed write is not worth failing the sign-in over.
        let _ = store::save(&p, cookie.as_bytes());
    }
    st.loaded = true;
    st.cookie = Some(Arc::new(SessionCookie(cookie)));
    st.player = Some(player);
}

fn forget(app: &AppHandle, st: &mut State) {
    if let Some(p) = session_path(app) {
        store::delete(&p);
    }
    st.loaded = true;
    st.cookie = None;
    st.player = None;
}

fn clear_session(app: &AppHandle) {
    if let Ok(mut st) = state().lock() {
        forget(app, &mut st);
    }
}

/// Whether `cookie` is still the session in use.
fn is_current(st: &State, cookie: &Arc<SessionCookie>) -> bool {
    st.cookie.as_ref().is_some_and(|c| Arc::ptr_eq(c, cookie))
}

/// Forget `cookie`, but only while it is still the session in use; returns
/// whether it was.
///
/// A request sent with the old session can come back 401 after a new sign-in
/// has replaced it — the 60-second notification poll, say, in flight while the
/// player signs in again. Clearing unconditionally then signed them straight
/// out of the session they had just made.
fn clear_session_if_current(app: &AppHandle, cookie: &Arc<SessionCookie>) -> bool {
    let Ok(mut st) = state().lock() else { return false };
    if !is_current(&st, cookie) {
        return false;
    }
    forget(app, &mut st);
    true
}

fn cached_player() -> Option<Value> {
    state().lock().ok().and_then(|st| st.player.clone())
}

/// The signed-in state as this process knows it right now, for an answer
/// that turned out to be about a session since replaced.
fn current_summary() -> Value {
    let st = state().lock().ok();
    match st.as_ref().and_then(|st| st.cookie.as_ref().and(st.player.clone())) {
        Some(player) => json!({ "authenticated": true, "player": player }),
        None => json!({ "authenticated": false }),
    }
}

fn notify(app: &AppHandle, payload: Value) {
    // `emit_to` rather than `emit`: the login window is remote content and is
    // never an audience for account state.
    let _ = app.emit_to("main", AUTH_EVENT, payload);
}

// ── Requests ─────────────────────────────────────────────────────────────

/// The only client that ever carries the session.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .https_only(true)
            // A redirect could hand the request to another host; refuse it
            // rather than rely on header stripping.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .gzip(true)
            .brotli(true)
            .build()
            .expect("TLS backend unavailable")
    })
}

enum ApiError {
    /// Vanilla no longer accepts the session.
    Unauthorized,
    Other(String),
}

impl From<ApiError> for String {
    fn from(e: ApiError) -> String {
        match e {
            ApiError::Unauthorized => "Not signed in to Vanilla".into(),
            ApiError::Other(s) => s,
        }
    }
}

/// Call a `/api/website/...` path through the site proxy with the session.
///
/// `post` is `None` for a GET, or `Some(body)` for a POST (`Value::Null` sends
/// no body), matching what the website itself sends for each call.
async fn send(cookie: &SessionCookie, post: Option<Value>, path: &str) -> Result<Value, ApiError> {
    let url = format!("{}/api.php?path={}", SITE_BASE, vanilla::urlencoding(path));
    let req = if post.is_some() { client().post(&url) } else { client().get(&url) };
    let mut req = req
        .header("User-Agent", USER_AGENT)
        .header("Referer", SITE_REFERER)
        .header("Accept", "application/json")
        .header(reqwest::header::COOKIE, cookie.header_value().map_err(ApiError::Other)?);
    match post {
        Some(Value::Null) => req = req.header("Origin", SITE_BASE),
        Some(body) => req = req.header("Origin", SITE_BASE).json(&body),
        None => {}
    }

    // Errors are rebuilt from the status alone: reqwest's own messages carry
    // the URL, which is harmless, but nothing here should need to be audited
    // for what it might echo.
    let resp = req
        .send()
        .await
        .map_err(|e| ApiError::Other(if e.is_timeout() { "Vanilla timed out".into() } else { "Could not reach Vanilla".into() }))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ApiError::Unauthorized);
    }
    if status.is_redirection() {
        return Err(ApiError::Other(format!("Vanilla redirected unexpectedly ({})", status.as_u16())));
    }

    let body = crate::server::read_text_capped(resp)
        .await
        .map_err(|_| ApiError::Other("Vanilla sent an unreadable reply".into()))?;
    let value: Option<Value> = serde_json::from_str(&body).ok();
    let api_error = value
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // A 403 is only a dead session when Vanilla says so. The same status comes
    // back for a path its proxy doesn't forward (`{"error":"Forbidden path"}`)
    // and for a Cloudflare challenge page, and reading either as "expired" used
    // to delete the stored session over what was a passing refusal.
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(match api_error.as_deref() {
            Some(err) if is_auth_error(err) => ApiError::Unauthorized,
            _ => ApiError::Other("Vanilla refused the request (HTTP 403)".into()),
        });
    }
    if !status.is_success() {
        return Err(ApiError::Other(format!("Vanilla returned HTTP {}", status.as_u16())));
    }

    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    let value = value.ok_or_else(|| ApiError::Other("Vanilla sent an unexpected reply".into()))?;
    // The proxy reports its own refusals as a 200 carrying an error object.
    if let Some(err) = api_error.as_deref() {
        if is_auth_error(err) {
            return Err(ApiError::Unauthorized);
        }
        return Err(ApiError::Other(format!("Vanilla API: {}", err.chars().take(80).collect::<String>())));
    }
    Ok(value)
}

/// Whether an error message from Vanilla means the session is no longer valid.
fn is_auth_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("unauthor") || lower.contains("not logged") || lower.contains("not authenticated")
}

/// Run an authenticated GET, signing out locally if Vanilla rejects it.
async fn authed_get(app: &AppHandle, path: &str) -> Result<Value, String> {
    authed(app, None, path).await
}

/// Run an authenticated POST. See [`send`] for `body`.
async fn authed_post(app: &AppHandle, path: &str, body: Value) -> Result<Value, String> {
    authed(app, Some(body), path).await
}

async fn authed(app: &AppHandle, post: Option<Value>, path: &str) -> Result<Value, String> {
    let cookie = current(app).ok_or_else(|| "Not signed in to Vanilla".to_string())?;
    match send(&cookie, post, path).await {
        Err(ApiError::Unauthorized) => {
            if clear_session_if_current(app, &cookie) {
                notify(app, json!({ "authenticated": false, "reason": "expired" }));
            }
            Err("Your Vanilla session has expired. Please sign in again.".into())
        }
        other => other.map_err(Into::into),
    }
}

/// `auth/session` → the account summary, or `None` if not signed in.
///
/// Only Vanilla saying so — a 401, or `authenticated: false` — means signed
/// out. Any other reply (an empty body, a shape it has never sent) is an
/// error, as a network failure is: `None` here deletes the stored session, and
/// one odd answer from a proxy is no reason to sign the player out.
async fn check_session(cookie: &SessionCookie) -> Result<Option<Value>, ApiError> {
    match send(cookie, None, "/api/website/auth/session").await {
        Ok(v) => match v.get("authenticated").and_then(Value::as_bool) {
            Some(true) => match v.get("player") {
                Some(player) => Ok(Some(vanilla::account_summary(player))),
                None => Err(ApiError::Other("Vanilla sent an unexpected reply".into())),
            },
            Some(false) => Ok(None),
            None => Err(ApiError::Other("Vanilla sent an unexpected reply".into())),
        },
        Err(ApiError::Unauthorized) => Ok(None),
        Err(e) => Err(e),
    }
}

// ── Reading the cookie out of the login window ───────────────────────────

/// Cloudflare's own cookies are bound to the browser that earned them and
/// are no use to (and not wanted from) a non-browser client.
fn is_cloudflare_cookie(name: &str) -> bool {
    name.starts_with("__cf") || name.starts_with("cf_") || name == "_cfuvid"
}

fn cookie_header(cookies: &[Cookie<'static>]) -> Option<Zeroizing<String>> {
    let mut out = Zeroizing::new(String::new());
    for c in cookies {
        let domain_ok = c
            .domain()
            .map(|d| d.trim_start_matches('.').eq_ignore_ascii_case(SITE_HOST))
            .unwrap_or(true);
        if !domain_ok || is_cloudflare_cookie(c.name()) || c.value().is_empty() {
            continue;
        }
        // Anything that can't go in a header verbatim is dropped rather than
        // escaped: a cookie Vanilla set will never contain these.
        if c.name().chars().chain(c.value().chars()).any(|ch| ch.is_control() || ch == ';') {
            continue;
        }
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(c.name());
        out.push('=');
        out.push_str(c.value());
    }
    (!out.is_empty()).then_some(out)
}

/// Called once the login page tries to leave for the home page, which it only
/// does after `auth/login` answered OK.
async fn capture_session(app: AppHandle) {
    let Some(win) = app.get_webview_window(LOGIN_LABEL) else { return };
    let Ok(site) = Url::parse("https://vanillarec.net/api.php") else { return };

    // Must not run on the main thread: WebView2's cookie call deadlocks there.
    // This is a tokio worker.
    let header = match win.cookies_for_url(site) {
        Ok(cookies) => cookie_header(&cookies),
        Err(_) => None,
    };
    let Some(header) = header else { return };

    let candidate = SessionCookie(header);
    match check_session(&candidate).await {
        Ok(Some(player)) => {
            // Leave nothing behind in the webview before it goes away.
            let _ = win.clear_all_browsing_data();
            let _ = win.close();
            set_session(&app, candidate.0.clone(), player.clone());
            notify(&app, json!({ "authenticated": true, "player": player }));
        }
        Ok(None) => {
            // The redirect wasn't a sign-in (e.g. the logo was clicked). The
            // page stays where it is; nothing to keep.
        }
        Err(e) => {
            let _ = win.close();
            notify(&app, json!({ "authenticated": false, "error": String::from(e) }));
        }
    }
}

fn is_site(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str().is_some_and(|h| h.eq_ignore_ascii_case(SITE_HOST))
}

// ── Commands ─────────────────────────────────────────────────────────────

/// Open (or focus) Vanilla's own sign-in page.
#[tauri::command]
pub async fn vanilla_login(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(LOGIN_LABEL) {
        let _ = win.unminimize();
        let _ = win.set_focus();
        return Ok(());
    }

    let url: Url = LOGIN_URL.parse().map_err(|_| "Bad login URL".to_string())?;
    let nav_app = app.clone();
    let mut builder = WebviewWindowBuilder::new(&app, LOGIN_LABEL, WebviewUrl::External(url))
        .title("Sign in to Vanilla")
        .inner_size(480.0, 680.0)
        .min_inner_size(380.0, 520.0)
        .center()
        .focused(true)
        // Its own in-memory cookie jar: nothing shared with the launcher's
        // window, nothing written to disk by the webview.
        .incognito(true)
        .devtools(false)
        .on_navigation(move |url| {
            if !is_site(url) {
                return false;
            }
            let path = url.path();
            if path.starts_with("/login") || path.starts_with("/cdn-cgi/") {
                return true;
            }
            if path == "/" {
                // login.js sends the page home after a successful sign-in.
                // Read the session instead of following it there.
                tauri::async_runtime::spawn(capture_session(nav_app.clone()));
            }
            false
        })
        // Pop-ups (target=_blank links) never open inside this window.
        .on_new_window(|_, _| NewWindowResponse::Deny);

    if let Some(main) = app.get_webview_window("main") {
        builder = builder.parent(&main).map_err(|e| e.to_string())?;
    }
    let win = builder.build().map_err(|e| format!("Couldn't open the sign-in window: {}", e))?;
    // Its native title bar would otherwise show the 256px app icon shrunk by
    // Windows to 16px, which is what made it look blurry.
    crate::background::sharpen_window_icon(&win);
    Ok(())
}

/// Sign out: end the session on Vanilla's side, then forget it here.
#[tauri::command]
pub async fn vanilla_logout(app: AppHandle) -> Result<(), String> {
    let cookie = current(&app);
    clear_session(&app);
    if let Some(win) = app.get_webview_window(LOGIN_LABEL) {
        let _ = win.close();
    }
    notify(&app, json!({ "authenticated": false }));
    if let Some(cookie) = cookie {
        // Best effort: the local copy is already gone either way.
        let _ = send(&cookie, Some(Value::Null), "/api/website/auth/logout").await;
    }
    Ok(())
}

/// `{ authenticated, player? }` — never the session itself.
#[tauri::command]
pub async fn vanilla_auth_status(app: AppHandle) -> Value {
    let Some(cookie) = current(&app) else {
        return json!({ "authenticated": false });
    };
    match check_session(&cookie).await {
        Ok(Some(player)) => {
            let current = state().lock().ok().map(|mut st| {
                let current = is_current(&st, &cookie);
                if current {
                    st.player = Some(player.clone());
                }
                current
            });
            match current {
                Some(true) => json!({ "authenticated": true, "player": player }),
                // Signed out, or into another account, while this was asked.
                _ => current_summary(),
            }
        }
        Ok(None) => {
            if clear_session_if_current(&app, &cookie) {
                json!({ "authenticated": false, "reason": "expired" })
            } else {
                current_summary()
            }
        }
        Err(e) => match cached_player() {
            Some(player) => json!({ "authenticated": true, "player": player, "offline": true }),
            None => json!({ "authenticated": false, "error": String::from(e), "hasSession": true }),
        },
    }
}

/// The signed-in account's token balance. Only whitelisted fields leave here:
/// `/me` is the caller's own record and may carry things the UI has no use for.
#[tauri::command]
pub async fn vanilla_account(app: AppHandle) -> Result<Value, String> {
    let me = authed_get(&app, "/api/website/me").await?;
    Ok(json!({ "tokens": me.get("tokens").and_then(Value::as_i64).unwrap_or(0) }))
}

/// The signed-in account's notifications, newest first, with sender names
/// resolved. Whitelisted field by field.
#[tauri::command]
pub async fn vanilla_notifications(app: AppHandle) -> Result<Value, String> {
    let raw = authed_get(&app, "/api/website/notifications/all").await?;
    let items = raw.as_array().cloned().unwrap_or_default();

    let sender_ids: BTreeSet<i64> = items
        .iter()
        .filter_map(|n| n.get("FromPlayerId").and_then(Value::as_i64))
        .filter(|id| *id > 0)
        .collect();
    let senders = vanilla::player_names(&sender_ids).await;

    let mut out: Vec<Value> = items
        .iter()
        .map(|n| {
            let kind = match n.get("Type") {
                Some(Value::Number(v)) => v.as_i64().unwrap_or(-1),
                Some(Value::String(s)) => s.parse().unwrap_or(-1),
                _ => -1,
            };
            let from = n.get("FromPlayerId").and_then(Value::as_i64).filter(|id| *id > 0);
            let sender = from.and_then(|id| senders.get(&id));
            // Free text is only meaningful for chat-style notifications, and is
            // rendered as text, never markup.
            let message = if kind == 30 || kind == 100 {
                n.get("Data").and_then(Value::as_str).map(|s| s.chars().take(300).collect::<String>())
            } else {
                None
            };
            json!({
                "id": n.get("Id").cloned().unwrap_or(Value::Null),
                "type": kind,
                "senderId": from,
                "senderName": sender.map(|s| s.0.clone()),
                "senderAvatar": sender.map(|s| s.1.clone()),
                "roomId": n.get("RoomId").and_then(Value::as_i64),
                "message": message,
                "sentTime": n.get("SentTime").and_then(Value::as_str),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b["sentTime"].as_str().unwrap_or("").cmp(a["sentTime"].as_str().unwrap_or(""))
    });
    Ok(Value::Array(out))
}

// ── Cheers, subscriptions and joining a room ───────────────────────────────────
//
// Every id below is checked before it goes into a path. The proxy takes the
// whole API path as one parameter, so an unchecked id like `1/../../auth/logout`
// would otherwise be a way to aim an authenticated request somewhere else.

fn check_id(id: i64) -> Result<i64, String> {
    if id > 0 { Ok(id) } else { Err("Invalid id".into()) }
}

/// Photo ids are numbers in the feed, but the website also falls back to image
/// names, so allow the characters those use and nothing else.
fn check_photo_id(id: &str) -> Result<&str, String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok { Ok(id) } else { Err("Invalid photo id".into()) }
}

/// The yes/no endpoints answer either a bare boolean or an object; the website
/// accepts both, and so does this.
fn flag(v: &Value, keys: &[&str]) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Object(_) => keys
            .iter()
            .chain(["value"].iter())
            .any(|k| v.get(*k).and_then(Value::as_bool).unwrap_or(false)),
        _ => false,
    }
}

/// Whether the signed-in player has cheered a room.
#[tauri::command]
pub async fn vanilla_room_cheered(app: AppHandle, room_id: i64) -> Result<bool, String> {
    let id = check_id(room_id)?;
    let v = authed_get(&app, &format!("/api/website/rooms/{}/cheered", id)).await?;
    Ok(flag(&v, &["cheered"]))
}

/// Cheer or un-cheer a room. Returns the new state.
#[tauri::command]
pub async fn vanilla_set_room_cheer(app: AppHandle, room_id: i64, cheer: bool) -> Result<bool, String> {
    let id = check_id(room_id)?;
    let verb = if cheer { "cheer" } else { "uncheer" };
    authed_post(
        &app,
        &format!("/api/website/me/{}/room/{}", verb, id),
        json!({ "id": id, "roomId": id }),
    )
    .await?;
    Ok(cheer)
}

/// Ids of the photos the signed-in player has cheered, as strings.
#[tauri::command]
pub async fn vanilla_cheered_photos(app: AppHandle) -> Result<Vec<String>, String> {
    let v = authed_get(&app, "/api/website/photos/cheered").await?;
    let list = v.get("results").unwrap_or(&v).as_array().cloned().unwrap_or_default();
    Ok(list
        .iter()
        .filter_map(|p| {
            ["photoId", "imageName", "id"].iter().find_map(|k| match p.get(*k) {
                Some(Value::Number(n)) => Some(n.to_string()),
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                _ => None,
            })
        })
        .collect())
}

/// Toggle a cheer on a photo. Vanilla decides the new state and count.
#[tauri::command]
pub async fn vanilla_toggle_photo_cheer(app: AppHandle, photo_id: String) -> Result<Value, String> {
    let id = check_photo_id(&photo_id)?;
    let v = authed_post(&app, &format!("/api/website/photos/{}/cheers", id), Value::Null).await?;
    Ok(json!({
        "cheered": v.get("cheered").and_then(Value::as_bool).unwrap_or(false),
        "cheerCount": v.get("cheerCount").and_then(Value::as_i64),
    }))
}

/// Whether the signed-in player is subscribed to a player.
#[tauri::command]
pub async fn vanilla_subscribed(app: AppHandle, player_id: i64) -> Result<bool, String> {
    let id = check_id(player_id)?;
    let v = authed_get(&app, &format!("/api/website/players/{}/subscribed", id)).await?;
    Ok(flag(&v, &["subscribed", "isSubscribed"]))
}

/// Subscribe to or unsubscribe from a player. Returns the new state.
#[tauri::command]
pub async fn vanilla_set_subscribed(app: AppHandle, player_id: i64, subscribe: bool) -> Result<bool, String> {
    let id = check_id(player_id)?;
    if cached_player().and_then(|p| p.get("id").and_then(Value::as_i64)) == Some(id) {
        return Err("You can't subscribe to yourself".into());
    }
    let verb = if subscribe { "subscribe" } else { "unsubscribe" };
    authed_post(
        &app,
        &format!("/api/website/me/{}/player/{}", verb, id),
        json!({ "id": id, "playerId": id }),
    )
    .await?;
    Ok(subscribe)
}

/// Ask Vanilla to send the signed-in player's running game into a room.
///
/// `{ success, message }`. The game has to be running and signed in to the
/// same account; otherwise Vanilla answers "Player is not online".
#[tauri::command]
pub async fn vanilla_join_room(app: AppHandle, room_id: i64) -> Result<Value, String> {
    let id = check_id(room_id)?;
    let v = authed_get(&app, &format!("/api/website/rooms/join/{}", id)).await?;
    let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);
    let message = v
        .get("message")
        .and_then(Value::as_str)
        .map(|m| m.chars().take(160).collect::<String>());
    Ok(json!({
        "success": success,
        "notOnline": message.as_deref().is_some_and(|m| m.eq_ignore_ascii_case("Player is not online")),
        "message": message,
    }))
}

/// The room the signed-in player is in right now, if Vanilla says. Only the
/// room's id and name leave here, not the rest of `/me`.
#[tauri::command]
pub async fn vanilla_current_room(app: AppHandle) -> Result<Value, String> {
    let me = authed_get(&app, "/api/website/me").await?;
    let cur = me.get("currentRoom").or_else(|| me.get("room")).unwrap_or(&Value::Null);
    let text = |v: Option<&Value>| match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    Ok(match cur {
        Value::Object(_) => json!({
            "id": text(cur.get("roomId").or_else(|| cur.get("id"))),
            "name": text(cur.get("name").or_else(|| cur.get("roomName"))),
        }),
        Value::String(_) | Value::Number(_) => json!({ "id": text(Some(cur)), "name": text(Some(cur)) }),
        _ => Value::Null,
    })
}

// ── At-rest storage ──────────────────────────────────────────────────────

#[cfg(windows)]
pub(crate) mod store {
    use super::DPAPI_ENTROPY;
    use std::path::Path;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use zeroize::Zeroizing;

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copy a DPAPI output blob and free it (zeroing it first).
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Zeroizing<Vec<u8>> {
        let bytes = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
        let copy = Zeroizing::new(bytes.to_vec());
        bytes.fill(0);
        LocalFree(out.pbData as _);
        copy
    }

    fn protect(plain: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        let input = blob(plain);
        let entropy = blob(DPAPI_ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: all blobs point at live buffers for the call's duration, and
        // `out` is owned by us afterwards and released in `take`.
        unsafe {
            let ok = CryptProtectData(
                &input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN, &mut out,
            );
            (ok != 0 && !out.pbData.is_null()).then(|| take(out))
        }
    }

    fn unprotect(sealed: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        let input = blob(sealed);
        let entropy = blob(DPAPI_ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: as in `protect`.
        unsafe {
            let ok = CryptUnprotectData(
                &input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN, &mut out,
            );
            (ok != 0 && !out.pbData.is_null()).then(|| take(out))
        }
    }

    pub fn save(path: &Path, plain: &[u8]) -> std::io::Result<()> {
        let sealed = protect(plain).ok_or_else(|| std::io::Error::other("DPAPI encryption failed"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &*sealed)?;
        std::fs::rename(&tmp, path)
    }

    pub fn load(path: &Path) -> Option<Zeroizing<String>> {
        let sealed = std::fs::read(path).ok()?;
        let plain = unprotect(&sealed);
        if plain.is_none() {
            // Written by another user or machine, or corrupt: useless, so drop it.
            delete(path);
        }
        let plain = plain?;
        std::str::from_utf8(&plain).ok().map(|s| Zeroizing::new(s.to_string()))
    }

    pub fn delete(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("tmp"));
    }
}

/// No OS-backed encryption is wired up off Windows, so the session is kept in
/// memory only and the user signs in again each launch.
#[cfg(not(windows))]
pub(crate) mod store {
    use std::path::Path;
    use zeroize::Zeroizing;

    pub fn save(_: &Path, _: &[u8]) -> std::io::Result<()> {
        Ok(())
    }
    pub fn load(_: &Path) -> Option<Zeroizing<String>> {
        None
    }
    pub fn delete(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_cookie() {
        let c = SessionCookie(Zeroizing::new("sid=supersecret".into()));
        assert!(!format!("{:?}", c).contains("supersecret"));
    }

    #[test]
    fn only_the_session_in_use_counts_as_current() {
        let old = Arc::new(SessionCookie(Zeroizing::new("sid=old".into())));
        let new = Arc::new(SessionCookie(Zeroizing::new("sid=new".into())));
        let st = State { loaded: true, cookie: Some(new.clone()), player: None };
        assert!(is_current(&st, &new));
        // A 401 for the session a new sign-in replaced must not clear that one.
        assert!(!is_current(&st, &old));
        assert!(!is_current(&State::default(), &old));
    }

    #[test]
    fn header_skips_cloudflare_and_foreign_cookies() {
        let cookies = vec![
            Cookie::build(("sid", "abc")).domain("vanillarec.net").build().into_owned(),
            Cookie::build(("__cf_bm", "x")).domain(".vanillarec.net").build().into_owned(),
            Cookie::build(("cf_clearance", "x")).build().into_owned(),
            Cookie::build(("other", "y")).domain("evil.example").build().into_owned(),
            Cookie::build(("refresh", "def")).domain(".vanillarec.net").build().into_owned(),
        ];
        assert_eq!(cookie_header(&cookies).as_deref().map(String::as_str), Some("sid=abc; refresh=def"));
    }

    #[test]
    fn header_is_none_without_a_usable_cookie() {
        let cookies = vec![Cookie::build(("__cf_bm", "x")).build().into_owned()];
        assert!(cookie_header(&cookies).is_none());
    }

    #[test]
    fn only_the_site_itself_counts() {
        assert!(is_site(&"https://vanillarec.net/login/".parse().unwrap()));
        assert!(!is_site(&"http://vanillarec.net/".parse().unwrap()));
        assert!(!is_site(&"https://vanillarec.net.evil.example/".parse().unwrap()));
        assert!(!is_site(&"https://api.vanillarec.net/".parse().unwrap()));
    }

    #[test]
    fn ids_cannot_steer_the_proxied_path() {
        assert!(check_id(0).is_err());
        assert!(check_id(-5).is_err());
        assert_eq!(check_id(42), Ok(42));
        assert!(check_photo_id("12345").is_ok());
        assert!(check_photo_id("abc_DEF-9").is_ok());
        for bad in ["", "1/../../auth/logout", "1?x=y", "a b", "..", "1%2F2"] {
            assert!(check_photo_id(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn only_an_auth_message_means_the_session_is_gone() {
        assert!(is_auth_error("Unauthorized"));
        assert!(is_auth_error("Not logged in"));
        assert!(is_auth_error("not authenticated"));
        // What the proxy says for a path it doesn't forward, and it says it
        // whether or not the session is fine.
        assert!(!is_auth_error("Forbidden path"));
        assert!(!is_auth_error("query_too_short"));
    }

    #[test]
    fn yes_no_answers_in_either_shape() {
        assert!(flag(&json!(true), &["cheered"]));
        assert!(!flag(&json!(false), &["cheered"]));
        assert!(flag(&json!({ "cheered": true }), &["cheered"]));
        assert!(flag(&json!({ "value": true }), &["cheered"]));
        assert!(flag(&json!({ "isSubscribed": true }), &["subscribed", "isSubscribed"]));
        assert!(!flag(&json!({ "other": true }), &["cheered"]));
        assert!(!flag(&Value::Null, &["cheered"]));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trips_and_is_not_plaintext() {
        let dir = std::env::temp_dir().join(format!("radium-auth-test-{}", std::process::id()));
        let path = dir.join(SESSION_FILE);
        store::save(&path, b"sid=roundtrip").unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(9).any(|w| w == b"roundtrip"));
        assert_eq!(store::load(&path).as_deref().map(String::as_str), Some("sid=roundtrip"));
        store::delete(&path);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
