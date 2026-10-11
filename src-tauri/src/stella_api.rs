//! Stella's live game API — rooms, people and profiles — reached the way the
//! game client reaches it.
//!
//! Stella runs the stock Rec Room backend behind Cloudflare, its services
//! (auth, accounts, rooms, matchmaking, the rest) on hosts named by the name
//! server at `api.stellaonline.org/` — see [`url`]. Three things, and only these three, get a request
//! through and answered (measured against the real client, 2026-10-01):
//!
//! 1. **`User-Agent: BestHTTP`.** Cloudflare's rule is purely on this string —
//!    the game's HTTP library. Anything else is a 403 block page before the
//!    origin is even reached (`/download/*` is the one open exception, which is
//!    why the installer in `stella.rs` uses a different User-Agent). The whole
//!    "we can't read Stella" story up to now was this one header.
//! 2. **A bearer token**, from [`login`]: the game signs in with a Steam
//!    auth-session ticket, and so does this. No ticket, 401.
//! 3. For a handful of write paths, an `X-RNSIG` request signature — which none
//!    of the read endpoints here need, so it is not implemented.
//!
//! The Stella developers run this same access for their own Discord bot and
//! asked us to use it; it is a community revival of a shut-down game, read with
//! the player's own account. Everything below is read-only: room and people
//! listings and public profiles, reshaped into the exact envelope the frontend
//! already consumes for Radium and Vanilla (`{ Results, TotalResults }`, with
//! absolute `ThumbUrl` / `AvatarUrl`), so `loadRooms()` / `loadPeople()` do not
//! care which network answered.
//!
//! ## Auth, and why there is no refresh
//!
//! `POST /auth/connect/token` with `grant_type=cached_login` takes the Steam
//! ticket and returns an access token (a JWT) and a refresh token. The refresh
//! grant answers 500 on this server, so this does not use it: when the access
//! token nears its `exp`, [`ensure_session`] simply signs in again with a fresh
//! ticket. A sign-in needs Steam running and the account owning Rec Room
//! (AppId 471710); when it isn't, the tabs show that rather than an error.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::applog;
use crate::server::{http_client_besthttp, read_capped, MAX_API_BYTES};

/// Stella's name server: the API root answers with each service's base URL, as
/// it does for the game (`{"Auth":"https://auth.stellaonline.org/auth", …}`).
const NAME_SERVER: &str = "https://api.stellaonline.org/";

/// The name-server key for each first path segment used here, and where it
/// pointed when last measured (2026-10-05) — used until the name server
/// answers. Stella moved auth, accounts, rooms and matchmaking off
/// `api.stellaonline.org` that day, and the notification hub later the same
/// day; the old paths there are 404s. Images moved to `img.stellaonline.org`
/// by 2026-10-11, and `api.stellaonline.org/img` is a 404 too.
const SERVICES: &[(&str, &str, &str)] = &[
    ("auth", "Auth", "https://auth.stellaonline.org/auth"),
    ("notify", "Notifications", "https://notify.stellaonline.org/notify"),
    ("account", "Accounts", "https://accounts.stellaonline.org/"),
    ("roomserver", "Rooms", "https://rooms.stellaonline.org/roomserver"),
    ("match", "Matchmaking", "https://match.stellaonline.org/match"),
    ("img", "Images", "https://img.stellaonline.org/img"),
    ("api", "API", "https://api.stellaonline.org/"),
];

/// How long a name-server answer is trusted, and how long to wait before asking
/// again after it failed (the built-in map is used meanwhile).
const NAME_SERVER_TTL: Duration = Duration::from_secs(60 * 60);
const NAME_SERVER_RETRY: Duration = Duration::from_secs(60);

/// A name-server answer: service key → base URL.
type ServiceMap = Arc<std::collections::HashMap<String, String>>;

/// The last name-server answer, and when it lapses.
static SERVICE_URLS: Mutex<Option<(ServiceMap, std::time::Instant)>> = Mutex::new(None);

/// The name server's map, fetched at most once per [`NAME_SERVER_TTL`]. Only
/// https URLs on Stella's own domain are taken from it, since the session token
/// goes wherever it points; anything else falls back to [`SERVICES`].
async fn service_urls() -> ServiceMap {
    if let Some((map, until)) = SERVICE_URLS.lock().ok().and_then(|g| g.clone()) {
        if std::time::Instant::now() < until {
            return map;
        }
    }
    let fetched = async {
        let resp = http_client_besthttp()
            .get(NAME_SERVER)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| unreachable_cause(&e))?;
        let status = resp.status();
        let body = read_capped(resp, MAX_API_BYTES).await?;
        if !status.is_success() {
            return Err(format!("HTTP {}{}", status_text(status), quoted_reply(&body)));
        }
        let v: Value = serde_json::from_slice(&body)
            .map_err(|_| format!("not JSON: {}", applog::snippet(&body, 300)))?;
        let mut map = std::collections::HashMap::new();
        for (k, url) in v.as_object().ok_or("not a JSON object")? {
            let Some(url) = url.as_str() else { continue };
            if on_stella(url) {
                map.insert(k.clone(), url.to_string());
            } else if SERVICES.iter().any(|(_, key, _)| key == k) {
                applog::backend_once(
                    &format!("ns-off-stella-{k}-{url}"),
                    Duration::from_secs(24 * 60 * 60),
                    "warn",
                    "server",
                    format!("Stella's name server sends {k} to {url}, which isn't on stellaonline.org, so the built-in address is used instead."),
                );
            }
        }
        if map.is_empty() {
            return Err("it listed no services".into());
        }
        Ok(map)
    }
    .await;
    let (map, ttl) = match fetched {
        Ok(map) => {
            log_moved_services(&map);
            (map, NAME_SERVER_TTL)
        }
        Err(reason) => {
            applog::backend_once(
                "ns-failed",
                Duration::from_secs(10 * 60),
                "warn",
                "server",
                format!("Couldn't read Stella's name server (GET {NAME_SERVER}): {reason}. Using the built-in addresses for now."),
            );
            (Default::default(), NAME_SERVER_RETRY)
        }
    };
    let map = Arc::new(map);
    if let Ok(mut g) = SERVICE_URLS.lock() {
        *g = Some((map.clone(), std::time::Instant::now() + ttl));
    }
    map
}

/// Note in the log each service the name server puts somewhere other than its
/// built-in address, or no longer lists: when Stella moves a service, the log
/// says where to.
fn log_moved_services(map: &std::collections::HashMap<String, String>) {
    for (_, key, built_in) in SERVICES {
        let msg = match map.get(*key) {
            Some(now) if now.trim_end_matches('/') == built_in.trim_end_matches('/') => continue,
            Some(now) => format!("Stella's name server puts {key} at {now} (built in: {built_in})."),
            None => format!("Stella's name server no longer lists {key}; using the built-in {built_in}."),
        };
        let at = map.get(*key).map(String::as_str).unwrap_or("");
        applog::backend_once(&format!("ns-moved-{key}-{at}"), Duration::from_secs(24 * 60 * 60), "info", "server", msg);
    }
}

/// "404 Not Found": the status code with its name.
fn status_text(status: reqwest::StatusCode) -> String {
    match status.canonical_reason() {
        Some(reason) => format!("{} {reason}", status.as_u16()),
        None => status.as_u16().to_string(),
    }
}

/// Log a request Stella answered with an error status: the address, the
/// status and the start of the reply, which is what tells a moved service
/// (404) from an outage (5xx) or a block page. Repeats of the same failure
/// within a minute are left out.
fn log_http_failure(source: &'static str, method: &str, url: &str, status: reqwest::StatusCode, body: &[u8]) {
    let url = &for_log(url);
    let without_query = url.split('?').next().unwrap_or(url);
    applog::backend_once(
        &format!("http-{method}-{without_query}-{}", status.as_u16()),
        Duration::from_secs(60),
        if status.is_server_error() { "error" } else { "warn" },
        source,
        format!("Stella answered {method} {url} with HTTP {}{}", status_text(status), quoted_reply(body)),
    );
}

/// `url` as written to the log: the sign-in lookup's Steam id left out, since
/// logs go into bug reports.
fn for_log(url: &str) -> String {
    const LOOKUP: &str = "/forplatformid/0/";
    match url.find(LOOKUP) {
        Some(at) => format!("{}{LOOKUP}<steam id>", &url[..at]),
        None => url.to_string(),
    }
}

/// ": <the start of the reply>", or nothing for an empty one. A web page (a
/// Cloudflare error or block page) is given by its title, which says what
/// happened ("stellaonline.org | 520: Web server is returning an unknown
/// error"), where its first 300 characters were only markup.
pub(crate) fn quoted_reply(body: &[u8]) -> String {
    if let Some(title) = html_title(body) {
        return format!(": a web page, \"{title}\"");
    }
    match applog::snippet(body, 300) {
        s if s.is_empty() => String::new(),
        s => format!(": {s}"),
    }
}

/// The `<title>` of an HTML reply, if it is one and has one.
fn html_title(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let start = text.trim_start().get(..15)?.to_ascii_lowercase();
    if !start.starts_with("<!doctype html") && !start.starts_with("<html") {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let from = open + lower[open..].find('>')? + 1;
    let to = from + lower[from..].find("</title>")?;
    let title = applog::snippet(text[from..to].as_bytes(), 200);
    (!title.is_empty()).then_some(title)
}

/// Whether `url` is https on `stellaonline.org` or one of its subdomains.
///
/// Judged on the parsed URL, which is what the request is then sent to. Cut
/// out of the string by hand, the host was everything up to the first `/`,
/// so `https://evil.example\.stellaonline.org/` passed — a URL parser reads
/// that `\` as a `/`, and the session token went to `evil.example`.
fn on_stella(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && (host == "stellaonline.org" || host.ends_with(".stellaonline.org"))
}

/// Absolute URL for an API `path` such as `/account/bulk?id=1`: its first
/// segment picks the service, whose base URL (from the name server) replaces
/// that segment when the base already ends in it (`…/auth` + `/auth/x` →
/// `…/auth/x`), and is prefixed to the whole path otherwise.
async fn url(path: &str) -> String {
    let seg = path.trim_start_matches('/').split(['/', '?']).next().unwrap_or("");
    let (key, fallback) = SERVICES
        .iter()
        .find(|(s, _, _)| *s == seg)
        .map(|(_, k, f)| (*k, *f))
        .unwrap_or(("API", "https://api.stellaonline.org/"));
    let map = service_urls().await;
    join_service(map.get(key).map(String::as_str).unwrap_or(fallback), seg, path)
}

/// The notification hub's websocket URL (see `stella_hub`).
pub(crate) async fn hub_url() -> String {
    let url = url("/notify/hub/v1").await;
    match url.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => url,
    }
}

fn join_service(base: &str, seg: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = format!("/{}", path.trim_start_matches('/'));
    match path.strip_prefix(&format!("/{seg}")) {
        Some(rest) if !seg.is_empty() && base.ends_with(&format!("/{seg}")) => format!("{base}{rest}"),
        _ => format!("{base}{path}"),
    }
}

/// Stella's Cloudflare answers the API only for this User-Agent.
pub const USER_AGENT: &str = "BestHTTP";

/// The game's OAuth client. `client_id` and `client_secret` are a single shared
/// credential baked into every copy of the Rec Room client (and every revival
/// that reuses it) — not a per-user secret, and extractable from any install —
/// so it lives here as a constant rather than being asked for.
const CLIENT_ID: &str = "recroom";
const CLIENT_SECRET: &str = "VxZ53kgbbEaRoZAeMe00MagtgD12GLL2";

/// Rec Room's Steam App ID; the auth ticket is minted for it.
const STEAM_APP_ID: u32 = 471710;

/// Re-sign-in this many seconds before the token's own expiry, so a request is
/// never sent with a token about to lapse.
const EXPIRY_SKEW_SECS: u64 = 120;

/// A signed-in session, cached until the token nears expiry.
struct Session {
    access_token: String,
    /// Unix seconds; 0 if the token carried no readable `exp` (then a short
    /// fixed lifetime is assumed by [`ensure_session`]).
    expires_at: u64,
    account_id: i64,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Clear the cached session, so the next call signs in again. Called when the
/// user signs in or out.
pub fn forget_session() {
    if let Ok(mut s) = SESSION.lock() {
        *s = None;
    }
}

/// Clear the cached session if it is still the one holding `token`: what a
/// 401 does. A page of room counts sent on an expired token gets a 401 for
/// each, some of them after the first has already signed in again, and
/// clearing whatever was cached threw that fresh session away and signed in
/// with Steam once more.
pub(crate) fn forget_session_if(token: &str) {
    if let Ok(mut s) = SESSION.lock() {
        if s.as_ref().is_some_and(|s| s.access_token == token) {
            *s = None;
        }
    }
}

// ─── Account (signed in / signed out) ───────────────────────────────────────

/// Set by LOG OUT and kept across restarts (as a marker file), so a signed-out
/// launcher stays signed out: with it set, nothing signs in on its own — every
/// API call answers [`SIGNED_OUT_ERROR`] until LOG IN clears it. Without this a
/// log-out would last only until the next Rooms page silently signed back in.
static SIGNED_OUT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SIGNED_OUT_MARKER: OnceLock<std::path::PathBuf> = OnceLock::new();
const SIGNED_OUT_FILE: &str = "stella-signed-out";

/// What a sign-in answers when Steam isn't running. The frontend matches on it
/// to offer "start Steam" and to retry when the window is next focused.
pub const STEAM_NOT_RUNNING: &str = "Steam isn't running.";

/// What an API call answers while signed out; the frontend shows a sign-in
/// prompt for it rather than an error.
pub const SIGNED_OUT_ERROR: &str = "Signed out of Stella.";

/// What a sign-in answers when the Steam account has no Stella account. The
/// frontend matches on it to say how to make one (play Stella once).
pub const NO_ACCOUNT: &str = "This Steam account has no Stella account yet.";

/// Whether the page has Stella picked ([`stella_set_in_use`]). Half of
/// [`in_use`]; the window being on screen is the other.
///
/// Signing in starts Rec Room's Steam API for a moment, which Steam shows to
/// the player's friends as playing Rec Room, so it happens only while Stella
/// is in use. Hidden in the tray, minimized or on another network, a sign-in
/// that has expired is left until Stella is opened again, rather than renewed
/// in the background, and the live hub (`stella_hub`) is closed. A sign-in
/// that is still valid is kept, so coming back needs no Steam at all. Opening
/// the tray panel counts as coming back (see [`window_on_screen`]).
static IN_USE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The app, for [`window_on_screen`].
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

/// What a sign-in answers while Stella isn't in use.
pub const NOT_IN_USE: &str = "Stella signs in when you open it.";

/// Whether Stella is in use: picked, in a launcher window that is on screen.
pub(crate) fn in_use() -> bool {
    IN_USE.load(std::sync::atomic::Ordering::SeqCst) && window_on_screen()
}

/// Whether the main window is showing: not hidden in the tray, not minimized.
/// Asked of the window itself: hidden in the tray, the page inside it still
/// reports itself visible.
///
/// The tray panel counts too, while it is up and for a little after: it shows
/// Stella's friends and player count, so opening it is looking at Stella, and
/// it has to be able to bring those up to date (see `tray_panel_in_view`).
#[cfg(not(test))]
fn window_on_screen() -> bool {
    use tauri::Manager;
    let main = APP
        .get()
        .and_then(|app| app.get_webview_window("main"))
        .is_some_and(|w| w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(true));
    main || crate::background::tray_panel_in_view()
}

/// Unit tests have no window. Linking the window calls into the test binary
/// also stops it starting on Windows: they bring in a common-controls import
/// that only the app's manifest provides.
#[cfg(test)]
fn window_on_screen() -> bool {
    false
}

/// The page saying whether it has Stella picked (see [`IN_USE`]).
#[tauri::command]
pub fn stella_set_in_use(in_use: bool) {
    IN_USE.store(in_use, std::sync::atomic::Ordering::SeqCst);
    if !in_use {
        crate::stella_hub::suspend();
    }
}

/// Called once at startup with the app and its local data dir.
pub fn init(app: tauri::AppHandle, local_data_dir: std::path::PathBuf) {
    let _ = APP.set(app);
    let marker = local_data_dir.join(SIGNED_OUT_FILE);
    SIGNED_OUT.store(marker.exists(), std::sync::atomic::Ordering::SeqCst);
    let _ = SIGNED_OUT_MARKER.set(marker);
    let _ = DEVICE_ID_FILE.set(local_data_dir.join(DEVICE_ID_FILE_NAME));
    let _ = ACCOUNT_FILE.set(local_data_dir.join(ACCOUNT_FILE_NAME));
}

pub(crate) fn signed_out() -> bool {
    SIGNED_OUT.load(std::sync::atomic::Ordering::SeqCst)
}

fn set_signed_out(out: bool) {
    SIGNED_OUT.store(out, std::sync::atomic::Ordering::SeqCst);
    if let Some(marker) = SIGNED_OUT_MARKER.get() {
        if out {
            if let Some(dir) = marker.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(marker, b"");
        } else {
            let _ = std::fs::remove_file(marker);
        }
    }
}

/// The signed-in account as the frontend's person row, or an error.
async fn me() -> Result<Value, String> {
    let (_, account_id) = ensure_session().await?;
    let account = api_get(&format!("/account/bulk?id={account_id}"))
        .await?
        .as_array()
        .and_then(|a| a.first().cloned())
        .ok_or("Stella returned no account.")?;
    Ok(person_row(&account))
}

/// `{ authenticated, player?, signedOut?, error? }`. Signs in (with Steam) if
/// not already, unless the user logged out — then it only reports that.
#[tauri::command]
pub async fn stella_auth_status() -> Value {
    if signed_out() {
        return json!({ "authenticated": false, "signedOut": true });
    }
    match me().await {
        Ok(player) => {
            prefetch_rooms();
            json!({ "authenticated": true, "player": player })
        }
        Err(e) => json!({ "authenticated": false, "error": e }),
    }
}

/// LOG IN: clear the signed-out state and sign in with Steam now. Refused,
/// with the current sign-in left as it was, while Stella isn't in use. Also
/// how the page asks again after "no Stella account" (the player may have
/// made one in the game since).
#[tauri::command]
pub async fn stella_login() -> Result<Value, String> {
    if !in_use() {
        return Err(NOT_IN_USE.into());
    }
    set_signed_out(false);
    forget_no_account();
    forget_session();
    let player = me().await?;
    prefetch_rooms();
    Ok(player)
}

/// The Stella accounts on the Steam account last signed in with, for the
/// page's account chooser: `{ current, chosen, accounts: [person row +
/// lastLogin, requirePassword, isJunior] }`, most recently played first.
/// `chosen` is whether one of them was picked in the launcher: with several
/// and none picked, the page asks which to use. Asked of
/// Stella again each time (the lookup needs only the Steam id, no sign-in), so
/// an account made in the game since shows up; the last answer stands in if
/// Stella can't be reached.
#[tauri::command]
pub async fn stella_accounts() -> Value {
    let steam_id = LINKED.lock().ok().and_then(|l| l.as_ref().map(|l| l.steam_id.clone()));
    if let Some(steam_id) = steam_id {
        if let Ok(accounts) = lookup_accounts(&steam_id).await {
            remember_linked(&steam_id, accounts);
        }
    }
    let mut accounts = LINKED
        .lock()
        .ok()
        .and_then(|l| l.as_ref().map(|l| l.accounts.clone()))
        .unwrap_or_default();
    accounts.sort_by_key(|a| std::cmp::Reverse(login_time_key(&a.last_login)));
    let current = cached_valid().map(|(_, id)| id).filter(|_| !signed_out());
    let (_, chosen) = linked_summary();
    json!({
        "current": current,
        "chosen": chosen,
        "accounts": accounts.iter().map(|a| {
            let mut row = person_row(&a.account);
            row["id"] = json!(a.account_id);
            row["lastLogin"] = json!(a.last_login);
            row["requirePassword"] = json!(a.require_password);
            row["isJunior"] = json!(a.account.get("isJunior").and_then(Value::as_bool).unwrap_or(false));
            row
        }).collect::<Vec<_>>(),
    })
}

/// Use `account_id`, one of the Stella accounts on this Steam account, from
/// now on: sign in to it and keep it as the one picked. The current sign-in
/// stays until the new one has worked, so a refusal (an account with a
/// password, say) changes nothing. Picking the account already signed in only
/// keeps the choice.
#[tauri::command]
pub async fn stella_use_account(account_id: i64) -> Result<Value, String> {
    if !in_use() {
        return Err(NOT_IN_USE.into());
    }
    let signed_in_as = cached_valid().map(|(_, id)| id).filter(|_| !signed_out());
    if signed_in_as == Some(account_id) {
        save_chosen_account(account_id);
        return me().await;
    }
    {
        let _guard = login_lock().lock().await;
        forget_no_account();
        let session = login(Some(account_id)).await?;
        save_chosen_account(account_id);
        set_signed_out(false);
        if let Ok(mut s) = SESSION.lock() {
            *s = Some(session);
        }
        // The friends connection belongs to the account it signed in as; the
        // page's next friends refresh starts it again for this one.
        crate::stella_hub::stop();
    }
    applog::backend("info", "account", format!("Switched to Stella account {account_id}; it is the one used from now on."));
    let player = me().await?;
    prefetch_rooms();
    Ok(player)
}

/// LOG OUT: drop the session and stay signed out until LOG IN.
#[tauri::command]
pub fn stella_logout() {
    set_signed_out(true);
    forget_session();
    crate::stella_hub::stop();
    if let Ok(mut c) = HOT_CACHE.lock() {
        *c = None;
    }
}

// ─── Accounts on one Steam account ──────────────────────────────────────────
//
// A Steam account can hold more than one Stella account (the game asks which
// one is playing), or none at all yet (the game makes one on its first run).
// The sign-in lookup, `/auth/cachedlogin/forplatformid/0/{steam id}`, answers
// the list without a token: `[{ accountId, lastLoginTime, requirePassword,
// account: { username, displayName, profileImage, … } }]`, and `[]` (HTTP
// 200) for none. The launcher signs in to the account picked in its chooser
// (kept in [`ACCOUNT_FILE_NAME`]), else to the one played last.

/// One Stella account on the signed-in Steam account.
#[derive(Debug, Clone, PartialEq)]
struct LinkedAccount {
    account_id: i64,
    /// When it last signed in, as Stella writes it (ISO 8601); may be empty.
    last_login: String,
    /// It has a password set in the game.
    require_password: bool,
    /// The account as `/account/bulk` has it (names, picture), which the
    /// lookup carries along; `{}` if it didn't.
    account: Value,
}

/// The last lookup: whose it was, and what it found.
struct Linked {
    steam_id: String,
    accounts: Vec<LinkedAccount>,
}
static LINKED: Mutex<Option<Linked>> = Mutex::new(None);

/// Where the account picked in the launcher is kept (its id, as text).
static ACCOUNT_FILE: OnceLock<std::path::PathBuf> = OnceLock::new();
const ACCOUNT_FILE_NAME: &str = "stella-account";

/// When a sign-in last found no Stella account. Until [`NO_ACCOUNT_RECHECK`]
/// has passed, a sign-in answers [`NO_ACCOUNT`] without asking: each would
/// start Steam's API again, showing the player as playing Rec Room for a
/// moment, and every list on screen asks for one. LOG IN asks at once, and the
/// page presses it for the player after the game has run (when an account may
/// have been made).
static NO_ACCOUNT_AT: Mutex<Option<std::time::Instant>> = Mutex::new(None);
const NO_ACCOUNT_RECHECK: Duration = Duration::from_secs(10 * 60);

fn no_account_recently() -> bool {
    NO_ACCOUNT_AT
        .lock()
        .ok()
        .and_then(|at| *at)
        .is_some_and(|at| at.elapsed() < NO_ACCOUNT_RECHECK)
}

fn note_no_account() {
    if let Ok(mut at) = NO_ACCOUNT_AT.lock() {
        *at = Some(std::time::Instant::now());
    }
}

fn forget_no_account() {
    if let Ok(mut at) = NO_ACCOUNT_AT.lock() {
        *at = None;
    }
}

/// The accounts in a lookup's answer, each once. Rows without an account id
/// are left out.
fn parse_linked_accounts(v: &Value) -> Vec<LinkedAccount> {
    let mut out: Vec<LinkedAccount> = Vec::new();
    for row in v.as_array().map(Vec::as_slice).unwrap_or_default() {
        let account_id = i64_at(row, "accountId");
        if account_id <= 0 || out.iter().any(|a| a.account_id == account_id) {
            continue;
        }
        out.push(LinkedAccount {
            account_id,
            last_login: str_at(row, "lastLoginTime").to_string(),
            require_password: row.get("requirePassword").and_then(Value::as_bool).unwrap_or(false),
            account: row.get("account").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({})),
        });
    }
    out
}

/// A sign-in time as something that sorts in time order: the date and time to
/// the second, then the fraction as microseconds. Compared as plain text,
/// ".2Z" would come after ".209Z".
fn login_time_key(s: &str) -> (String, u32) {
    let (whole, rest) = s.split_once('.').unwrap_or((s.trim_end_matches('Z'), ""));
    let digits: String = rest.chars().take_while(char::is_ascii_digit).take(6).collect();
    let micros = format!("{digits:0<6}").parse().unwrap_or(0);
    (whole.to_string(), micros)
}

/// The account to use when none was picked: the one played last, as the
/// game's own chooser has it. One with a password set is passed over while
/// another has none.
fn default_account(accounts: &[LinkedAccount]) -> Option<&LinkedAccount> {
    let key = |a: &&LinkedAccount| login_time_key(&a.last_login);
    accounts
        .iter()
        .filter(|a| !a.require_password)
        .max_by_key(key)
        .or_else(|| accounts.iter().max_by_key(key))
}

/// The account to sign in to: the one picked in the launcher while it is
/// still on this Steam account, else [`default_account`].
fn pick_account(accounts: &[LinkedAccount], chosen: Option<i64>) -> Option<&LinkedAccount> {
    chosen
        .and_then(|id| accounts.iter().find(|a| a.account_id == id))
        .or_else(|| default_account(accounts))
}

/// Ask Stella which accounts `steam_id` holds. Needs no sign-in.
async fn lookup_accounts(steam_id: &str) -> Result<Vec<LinkedAccount>, String> {
    let lookup_url = url(&format!("/auth/cachedlogin/forplatformid/0/{steam_id}")).await;
    let lookup = send_with_retry(http_client_besthttp().get(&lookup_url).timeout(Duration::from_secs(15))).await?;
    // Checked before the body is read as an account list, or a Cloudflare
    // block page or an outage would read as "no Stella account".
    if !lookup.status().is_success() {
        let status = lookup.status();
        let body = read_capped(lookup, 64 * 1024).await.unwrap_or_default();
        log_http_failure("account", "GET", &lookup_url, status, &body);
        return Err(format!("Stella sign-in failed (HTTP {}). Try again later.", status.as_u16()));
    }
    let body = read_capped(lookup, MAX_API_BYTES).await?;
    match serde_json::from_slice::<Value>(&body) {
        Ok(v) if v.is_array() => Ok(parse_linked_accounts(&v)),
        _ => {
            applog::backend(
                "error",
                "account",
                format!("Stella's account lookup (GET {}) wasn't a list: {}", for_log(&lookup_url), applog::snippet(&body, 300)),
            );
            Err("Stella sign-in failed: its account lookup wasn't readable. Try again later.".into())
        }
    }
}

fn remember_linked(steam_id: &str, accounts: Vec<LinkedAccount>) {
    if let Ok(mut l) = LINKED.lock() {
        *l = Some(Linked { steam_id: steam_id.to_string(), accounts });
    }
}

/// How many Stella accounts the Steam account has, and whether the one picked
/// in the launcher is among them.
fn linked_summary() -> (usize, bool) {
    let chosen = chosen_account();
    LINKED
        .lock()
        .ok()
        .and_then(|l| {
            l.as_ref()
                .map(|l| (l.accounts.len(), chosen.is_some_and(|id| l.accounts.iter().any(|a| a.account_id == id))))
        })
        .unwrap_or((0, false))
}

/// The account picked in the launcher's chooser, if one was.
fn chosen_account() -> Option<i64> {
    let text = std::fs::read_to_string(ACCOUNT_FILE.get()?).ok()?;
    text.trim().parse().ok().filter(|id: &i64| *id > 0)
}

fn save_chosen_account(account_id: i64) {
    let Some(path) = ACCOUNT_FILE.get() else { return };
    if chosen_account() == Some(account_id) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, account_id.to_string()) {
        applog::backend("warn", "account", format!("Couldn't keep the picked Stella account: {e}"));
    }
}

/// A refused sign-in's message, with a note when the account has a password
/// set in the game: the launcher signs in the way the game does for an
/// account without one.
fn password_note(message: String, require_password: bool) -> String {
    if require_password {
        format!("{message} That account has a password set in the game, which may be why.")
    } else {
        message
    }
}

// ─── Steam ticket ─────────────────────────────────────────────────────────
//
// Steam counts a process that has started the Steam API as Rec Room as
// playing Rec Room until that process exits: shutting the API down again
// doesn't end it. Measured 2026-10-02: after one sign-in, Steam's
// RunningAppID stayed 471710 for as long as the launcher ran, hidden in the
// tray or not, and went back to 0 two seconds after it quit. So the launcher
// never starts the Steam API itself. A sign-in runs a second copy of the
// launcher ([`TICKET_HELPER_ARG`], see [`steam_ticket_helper`]) that gets the
// ticket, holds it until Stella has checked it, and exits; Steam shows Rec
// Room for those few seconds only. That copy's environment is its own, too,
// so the `SteamAppId` the Steam API sets never reaches the games the launcher
// starts.

/// The argument that makes the launcher's exe the Steam ticket helper.
pub const TICKET_HELPER_ARG: &str = "--stella-steam-ticket";
/// How the helper's answer starts on its stdout, which the Steam API prints
/// its own lines to as well.
const TICKET_LINE: &str = "RADIUM-STEAM-TICKET ";
/// The longest the helper stays, should the launcher never let it go.
const HELPER_MAX_LIFE: Duration = Duration::from_secs(90);

/// A ticket for Rec Room, as uppercase hex, with the signed-in Steam id. The
/// helper that got it, and with it the ticket, stays until this is dropped.
struct SteamTicket {
    steam_id: String,
    ticket_hex: String,
    _helper: TicketHelper,
}

/// The ticket helper's process. Dropped, it is told to go (its stdin closes),
/// and stopped if it hasn't within a few seconds.
struct TicketHelper(Option<std::process::Child>);

impl Drop for TicketHelper {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else { return };
        drop(child.stdin.take());
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_secs(3) {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

/// Get a Steam auth-session ticket for Rec Room from the ticket helper.
/// Blocking (it waits for the helper's answer), so callers run it on a
/// blocking thread.
///
/// Fails cleanly when Steam isn't running or the account doesn't own the app —
/// the caller turns that into a message the tab can show.
#[cfg(target_os = "windows")]
fn steam_ticket() -> Result<SteamTicket, String> {
    use std::io::BufRead;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    // Told apart from a failed init so the UI can say "start Steam" and retry
    // when it does, rather than a vaguer either-or.
    if !crate::game::check_steam() {
        return Err(STEAM_NOT_RUNNING.into());
    }
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't start Stella's Steam sign-in: {e}"))?;
    let mut child = std::process::Command::new(exe)
        .arg(TICKET_HELPER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn()
        .map_err(|e| format!("Couldn't start Stella's Steam sign-in: {e}"))?;
    let stdout = child.stdout.take();
    let helper = TicketHelper(Some(child));
    let stdout = stdout.ok_or("Couldn't start Stella's Steam sign-in.")?;

    // Read on a thread, so a helper that never answers can be given up on.
    // It goes on reading to the end, so the helper never writes into a
    // closed pipe.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(answer) = line.strip_prefix(TICKET_LINE) {
                let _ = tx.send(answer.to_string());
            }
        }
    });
    let answer = rx
        .recv_timeout(Duration::from_secs(20))
        .map_err(|_| "Steam didn't answer Stella's sign-in. Try again.".to_string())?;
    let answer: Value = serde_json::from_str(&answer).map_err(|e| e.to_string())?;
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    let (steam_id, ticket_hex) = (str_at(&answer, "steamId").to_string(), str_at(&answer, "ticket").to_string());
    if steam_id.is_empty() || ticket_hex.is_empty() {
        return Err("Steam returned an empty auth ticket.".into());
    }
    Ok(SteamTicket { steam_id, ticket_hex, _helper: helper })
}

#[cfg(not(target_os = "windows"))]
fn steam_ticket() -> Result<SteamTicket, String> {
    Err("Stella sign-in is only supported on Windows.".into())
}

/// The ticket helper: what the launcher's exe does when started with
/// [`TICKET_HELPER_ARG`] (see `main.rs`). Starts the Steam API as Rec Room,
/// prints the ticket on one [`TICKET_LINE`], keeps it (and Steam's callbacks)
/// alive until the launcher closes this process's stdin, and exits. Returns
/// the exit code.
#[cfg(target_os = "windows")]
pub fn steam_ticket_helper() -> i32 {
    use std::io::{Read, Write};
    let answer = |v: Value| {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{TICKET_LINE}{v}");
        let _ = out.flush();
    };

    let dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(std::path::Path::to_path_buf));
    if !dir.is_some_and(|dir| load_steam_api(&dir)) {
        answer(json!({ "error": "Stella sign-in needs steam_api64.dll, which is missing from the launcher's folder. \
                                 Reinstall the launcher to put it back." }));
        return 1;
    }
    let Ok(client) = steamworks::Client::init_app(STEAM_APP_ID) else {
        answer(json!({ "error": "Steam couldn't sign you in to Stella. Make sure you're logged in to Steam with an account that owns Rec Room." }));
        return 1;
    };
    let steam_id = client.user().steam_id().raw().to_string();
    let identity = steamworks::networking_types::NetworkingIdentity::new();
    let (_handle, ticket) = client.user().authentication_session_ticket(identity);

    // GetAuthSessionTicket fills the bytes synchronously but Steam validates it
    // with its backend a moment later; pump callbacks briefly so the ticket is
    // live by the time the server checks it.
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(1300) {
        client.run_callbacks();
        std::thread::sleep(Duration::from_millis(50));
    }
    if ticket.is_empty() {
        answer(json!({ "error": "Steam returned an empty auth ticket." }));
        return 1;
    }
    answer(json!({ "steamId": steam_id, "ticket": hex_upper(&ticket) }));

    // Held until the launcher is done with it: its end of stdin closes.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        while matches!(std::io::stdin().read(&mut buf), Ok(n) if n > 0) {}
        let _ = tx.send(());
    });
    let start = std::time::Instant::now();
    while start.elapsed() < HELPER_MAX_LIFE {
        client.run_callbacks();
        if !matches!(rx.recv_timeout(Duration::from_millis(100)), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
            break;
        }
    }
    0
}

#[cfg(not(target_os = "windows"))]
pub fn steam_ticket_helper() -> i32 {
    1
}

/// Load `steam_api64.dll` from `dir`, the launcher's own folder.
///
/// The launcher links the Steam API delay-loaded (see build.rs), so it starts
/// without the DLL and every network but Stella works whatever happens to it;
/// only the ticket helper ever loads it. The price is that a delay-loaded DLL
/// found missing at its first call doesn't fail the call, it crashes the
/// process, so it is loaded here first. Loading it by full path also means the
/// one beside the launcher is the one used: the delay-load helper finds it
/// already loaded by name.
#[cfg(target_os = "windows")]
fn load_steam_api(dir: &std::path::Path) -> bool {
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
    let path: Vec<u16> = dir
        .join("steam_api64.dll")
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `path` is NUL-terminated. The module is left loaded on
    // purpose: the Steam API is called through it until the helper exits.
    !unsafe { LoadLibraryW(path.as_ptr()) }.is_null()
}

fn hex_upper(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

// ─── Device id ───────────────────────────────────────────────────────────

/// Rec Room's `device_id` for `cached_login` is Unity's
/// `deviceUniqueIdentifier` — a per-machine value the game registered to this
/// account on its first sign-in. The server binds it: `cached_login` with any
/// other device id is rejected ("platform verification failed"), because an
/// unregistered device would need the full `dinfo` registration blob the game
/// computes. So the launcher must present the *same* id the game did.
///
/// Unity doesn't store it as a plain field, but it stamps it on every analytics
/// event as `"deviceid":"<40 hex>"`, and that is where it is read from first,
/// which is why signing in needs the game to have run once (it always has: the
/// launcher installed it).
///
/// Those files are the game's to tidy away, though. So once an id has signed
/// in, it is kept in the launcher's own local data ([`DEVICE_ID_FILE_NAME`])
/// and used from there, and a sign-in no longer depends on the files still
/// being there. A kept id Stella stops accepting is looked up in the game's
/// files again (see [`login`]).
struct DeviceId {
    id: String,
    /// Read from the launcher's own copy rather than the game's files.
    kept: bool,
}

/// Where the device id that last signed in is kept.
static DEVICE_ID_FILE: OnceLock<std::path::PathBuf> = OnceLock::new();
const DEVICE_ID_FILE_NAME: &str = "stella-device-id";

const NO_DEVICE_ID: &str =
    "Couldn't read your Rec Room device id — launch Rec Room once so Stella registers this device, then try again.";

/// What a sign-in answers when Stella turns down the device id it was sent.
const DEVICE_REJECTED: &str =
    "Stella couldn't verify this PC for your account. Play Stella once from the launcher, then log in again.";

/// The id to sign in with: the one that last signed in, else the game's own.
fn device_id() -> Result<DeviceId, String> {
    if let Some(id) = DEVICE_ID_FILE.get().and_then(|path| read_device_id_file(path)) {
        return Ok(DeviceId { id, kept: true });
    }
    scan_device_id()
        .map(|id| DeviceId { id, kept: false })
        .ok_or_else(|| NO_DEVICE_ID.to_string())
}

/// Keep `id`, which Stella has just accepted, for the sign-ins after this one.
fn keep_device_id(id: &str) {
    if let Some(path) = DEVICE_ID_FILE.get() {
        let _ = write_device_id_file(path, id);
    }
}

fn read_device_id_file(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let id = text.trim();
    is_device_id(id).then(|| id.to_string())
}

fn write_device_id_file(path: &std::path::Path, id: &str) -> std::io::Result<()> {
    if read_device_id_file(path).as_deref() == Some(id) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, id)
}

/// Whether `s` has the shape of a Unity device id: 40 hex digits.
fn is_device_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Walk Rec Room's Unity analytics events for the first `"deviceid":"<40 hex>"`.
fn scan_device_id() -> Option<String> {
    let base = std::env::var("USERPROFILE").ok()?;
    let analytics = std::path::Path::new(&base)
        .join("AppData")
        .join("LocalLow")
        .join("Against Gravity")
        .join("Rec Room")
        .join("Unity");
    let mut found = None;
    // Bounded walk: Unity/<appid>/Analytics/ArchivedEvents/<batch>/{e,s}
    visit_files(&analytics, 6, &mut |path| {
        if found.is_some() {
            return;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name != "e" && name != "s" {
            return;
        }
        if let Ok(bytes) = std::fs::read(path) {
            if let Some(id) = extract_deviceid(&bytes) {
                found = Some(id);
            }
        }
    });
    found
}

/// First `"deviceid":"<40 lowercase hex>"` value in `bytes`.
fn extract_deviceid(bytes: &[u8]) -> Option<String> {
    let hay = String::from_utf8_lossy(bytes);
    let key = "\"deviceid\":\"";
    let start = hay.find(key)? + key.len();
    let rest = &hay[start..];
    let end = rest.find('"')?;
    let val = &rest[..end];
    is_device_id(val).then(|| val.to_string())
}

/// Depth-bounded file walk, calling `f` on every file found.
fn visit_files(dir: &std::path::Path, depth: usize, f: &mut impl FnMut(&std::path::Path)) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_files(&path, depth - 1, f);
        } else {
            f(&path);
        }
    }
}

// ─── Sign-in ─────────────────────────────────────────────────────────────

/// The `exp` claim of a JWT, as unix seconds, or 0 if it can't be read.
fn jwt_expiry(token: &str) -> u64 {
    let Some(payload) = token.split('.').nth(1) else { return 0 };
    let Some(bytes) = base64url_decode(payload) else { return 0 };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("exp").and_then(|e| e.as_u64()))
        .unwrap_or(0)
}

/// Minimal base64url (no padding) decode, enough for a JWT payload.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let v = val(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// Sign in with a fresh Steam ticket and return a live [`Session`].
///
/// The device id must be the one the game registered for this account (see
/// [`DeviceId`]). It is read first: without it there is no point starting
/// the Steam API. One read from the game's files is kept once it works. A kept
/// one Stella turns down may simply be out of date (the game registers a new
/// id when this PC's hardware changes), so the game's files are asked again,
/// and a different id there gets one more try.
///
/// `want` is an account to sign in to (the account chooser); `None` signs in
/// to the one picked before, or the one played last.
async fn login(want: Option<i64>) -> Result<Session, String> {
    let device = tokio::task::spawn_blocking(device_id).await.map_err(|e| e.to_string())??;
    match login_as(&device.id, want).await {
        Ok(session) => {
            if !device.kept {
                keep_device_id(&device.id);
            }
            Ok(session)
        }
        Err(e) if e == DEVICE_REJECTED && device.kept => {
            let kept = device.id;
            let fresh = tokio::task::spawn_blocking(scan_device_id)
                .await
                .ok()
                .flatten()
                .filter(|id| *id != kept)
                .ok_or(e)?;
            let session = login_as(&fresh, want).await?;
            keep_device_id(&fresh);
            Ok(session)
        }
        Err(e) => Err(e),
    }
}

/// Whether a refused sign-in's answer is Stella turning down the device id.
fn is_device_rejection(body: &[u8]) -> bool {
    String::from_utf8_lossy(body).to_ascii_lowercase().contains("platform verification failed")
}

/// One sign-in attempt with `device` as the device id, to `want` if given,
/// else to the account picked in the launcher (see [`pick_account`]).
async fn login_as(device: &str, want: Option<i64>) -> Result<Session, String> {
    // Kept until this sign-in is over: the helper, and Steam's "playing Rec
    // Room" with it, goes when this is dropped.
    let ticket = tokio::task::spawn_blocking(steam_ticket).await.map_err(|e| e.to_string())??;
    let (steam_id, ticket_hex) = (ticket.steam_id.clone(), ticket.ticket_hex.clone());

    let client = http_client_besthttp();

    // The Stella accounts on this Steam account (none: it has no account yet).
    let accounts = lookup_accounts(&steam_id).await?;
    remember_linked(&steam_id, accounts.clone());
    if accounts.is_empty() {
        applog::backend("info", "account", "This Steam account has no Stella account yet; playing Stella once makes one.");
        return Err(NO_ACCOUNT.into());
    }
    let account = match want {
        Some(id) => accounts
            .iter()
            .find(|a| a.account_id == id)
            .ok_or("That Stella account isn't on this Steam account any more.")?,
        None => pick_account(&accounts, chosen_account()).ok_or(NO_ACCOUNT)?,
    };
    let account_id = account.account_id;
    if accounts.len() > 1 {
        applog::backend(
            "info",
            "account",
            format!(
                "This Steam account has {} Stella accounts; signing in to @{} ({}).",
                accounts.len(),
                str_at(&account.account, "username"),
                if want.is_some() { "picked just now" } else if chosen_account() == Some(account_id) { "picked in the launcher" } else { "played last" },
            ),
        );
    }
    let require_password = account.require_password;

    // eac challenge (the server wants the value echoed back; it isn't verified)
    let eac_url = url("/auth/eac/challenge").await;
    let eac = client.get(&eac_url).timeout(Duration::from_secs(15)).send().await;
    let eac = match eac {
        Ok(r) if r.status().is_success() => {
            let b = read_capped(r, MAX_API_BYTES).await.unwrap_or_default();
            String::from_utf8_lossy(&b).trim().trim_matches('"').to_string()
        }
        // Signed in without one before; noted in case the sign-in fails.
        Ok(r) => {
            let status = r.status();
            let b = read_capped(r, 64 * 1024).await.unwrap_or_default();
            log_http_failure("account", "GET", &eac_url, status, &b);
            String::new()
        }
        Err(e) => {
            unreachable_message(&e);
            String::new()
        }
    };

    let asid = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis().to_string();
    let platform_auth = json!({ "Ticket": ticket_hex, "AppId": "471710", "Error": "-1" }).to_string();

    let form = [
        ("grant_type", "cached_login"),
        ("account_id", &account_id.to_string()),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("platform", "0"),
        ("platform_id", &steam_id),
        ("device_id", device),
        ("device_class", "2"),
        ("ver", "20240418"),
        ("cid", "13735"),
        ("asid", &asid),
        ("locale", "en"),
        ("isInitialLogin", "true"),
        ("eac_challenge", &eac),
        ("eac_response", "test"),
        ("platform_auth", &platform_auth),
    ];

    let token_url = url("/auth/connect/token").await;
    let resp = client
        .post(&token_url)
        .form(&form)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| {
            unreachable_message(&e);
            format!("Stella sign-in failed: {e}")
        })?;
    let status = resp.status();
    let body = read_capped(resp, MAX_API_BYTES).await?;
    // Looked at whatever the status: Stella turns a device id down with a
    // 200 that carries no token, not with an error status.
    if is_device_rejection(&body) {
        applog::backend("warn", "account", format!("Stella turned down this PC's device id (POST {token_url}, HTTP {})", status_text(status)));
        return Err(DEVICE_REJECTED.into());
    }
    // The body is quoted only for a failure: a successful one holds the token.
    if !status.is_success() {
        log_http_failure("account", "POST", &token_url, status, &body);
        return Err(password_note(format!("Stella sign-in was refused (HTTP {}).", status.as_u16()), require_password));
    }
    let token: Value = serde_json::from_slice(&body).map_err(|e| {
        applog::backend(
            "error",
            "account",
            format!("Stella's sign-in reply (POST {token_url}) wasn't JSON ({e}): {}", applog::snippet(&body, 300)),
        );
        e.to_string()
    })?;
    let access_token = token
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            // Only the reply's field names: whatever else it holds may be a
            // credential.
            let fields = token.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default();
            // Stella's own reason, when it gives one (OAuth's error fields).
            let reason = ["error_description", "error"].iter().find_map(|k| token.get(*k).and_then(Value::as_str));
            applog::backend(
                "error",
                "account",
                format!(
                    "Stella's sign-in (POST {token_url}) answered HTTP {} without a token; reason: {}; fields: {fields}",
                    status_text(status),
                    reason.unwrap_or("none given"),
                ),
            );
            let message = match reason {
                Some(reason) => format!("Stella sign-in was refused ({}).", reason.chars().take(200).collect::<String>()),
                None => "Stella sign-in returned no token.".to_string(),
            };
            password_note(message, require_password)
        })?
        .to_string();
    let expires_at = jwt_expiry(&access_token);

    Ok(Session { access_token, expires_at, account_id })
}

/// The cached session, if present and not within [`EXPIRY_SKEW_SECS`] of its
/// `exp`. A token with no readable `exp` is treated as already stale, so a fresh
/// sign-in replaces it (in practice Stella's tokens always carry `exp`).
fn cached_valid() -> Option<(String, i64)> {
    let guard = SESSION.lock().ok()?;
    let s = guard.as_ref()?;
    if s.expires_at > 0 && now_secs() < s.expires_at.saturating_sub(EXPIRY_SKEW_SECS) {
        Some((s.access_token.clone(), s.account_id))
    } else {
        None
    }
}

/// Serializes sign-in. Steamworks allows only one `Client` in a process at a
/// time, so two concurrent `login()` calls would call `SteamAPI_Init` twice and
/// abort the whole launcher. Opening the Rooms tab fires several API calls at
/// once (rooms + filters), so this is the normal case, not an edge one.
fn login_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Return a valid `(access_token, account_id)`, signing in if the cache is empty
/// or the token is near expiry. Concurrent callers sign in at most once: the
/// first takes [`login_lock`] and signs in, the rest wait and then find the
/// session it cached.
pub(crate) async fn ensure_session() -> Result<(String, i64), String> {
    if signed_out() {
        return Err(SIGNED_OUT_ERROR.into());
    }
    if let Some(hit) = cached_valid() {
        return Ok(hit);
    }
    let _guard = login_lock().lock().await;
    // Another task may have signed in while we waited for the lock, or the
    // user logged out.
    if signed_out() {
        return Err(SIGNED_OUT_ERROR.into());
    }
    if let Some(hit) = cached_valid() {
        return Ok(hit);
    }
    // A sign-in uses Steam: only while Stella is in use (see IN_USE).
    if !in_use() {
        return Err(NOT_IN_USE.into());
    }
    // Found no account a moment ago: not asked again yet (see NO_ACCOUNT_AT).
    if no_account_recently() {
        return Err(NO_ACCOUNT.into());
    }

    let session = match login(None).await {
        Ok(session) => session,
        Err(e) => {
            if e == NO_ACCOUNT {
                note_no_account();
            }
            return Err(e);
        }
    };
    // Logged out while that sign-in was under way: don't keep it.
    if signed_out() {
        return Err(SIGNED_OUT_ERROR.into());
    }
    let out = (session.access_token.clone(), session.account_id);
    if let Ok(mut guard) = SESSION.lock() {
        *guard = Some(session);
    }
    Ok(out)
}

// ─── Authenticated GET ─────────────────────────────────────────────────────

/// GET an API path as JSON, signing in as needed and retrying once on a 401.
pub(crate) async fn api_get(path: &str) -> Result<Value, String> {
    api_request(reqwest::Method::GET, path).await
}

/// [`api_get`] with any method. The writes used here (PUT/DELETE on a room's
/// cheer and favorite) take no body and, unlike the game's matchmaking calls,
/// need no request signature.
async fn api_request(method: reqwest::Method, path: &str) -> Result<Value, String> {
    api_request_with(method, path, Body::None).await
}

/// What a request carries.
#[derive(Clone, Copy)]
enum Body<'a> {
    None,
    /// JSON (a photo's cheer).
    Json(&'a Value),
    /// A form, as the game sends its messages.
    Form(&'a [(&'a str, String)]),
}

/// How many more times a GET is sent after Stella's edge (Cloudflare) said
/// its server failed (see [`edge_failure`]), and how long before each.
const EDGE_RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(1500), Duration::from_millis(4000)];

/// Whether `status` is Cloudflare reporting that Stella's own server failed or
/// couldn't be reached — usually a moment's trouble behind it, gone on the
/// next try (a 520 on the rooms list at sign-in, 2026-10-05, while the same
/// server was also dropping the friends connection).
fn edge_failure(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 502 | 503 | 504 | 520..=527)
}

/// [`api_request`] with a body.
async fn api_request_with(method: reqwest::Method, path: &str, body: Body<'_>) -> Result<Value, String> {
    // A GET is asked again after an edge failure: every one used here only
    // reads, or sets a state rather than toggling one (a relationship call).
    let mut edge_retries = if method == reqwest::Method::GET { EDGE_RETRY_DELAYS.len() } else { 0 };
    let mut edge_failed: Option<reqwest::StatusCode> = None;
    let mut attempt = 0;
    while attempt < 2 {
        let (token, _account) = ensure_session().await?;
        let full_url = url(path).await;
        let mut req = http_client_besthttp()
            .request(method.clone(), &full_url)
            .bearer_auth(&token);
        match body {
            Body::Json(v) => req = req.json(v),
            Body::Form(fields) => req = req.form(fields),
            // An explicit empty body, so a PUT goes out with Content-Length: 0
            // (what was tested) rather than none.
            Body::None if method != reqwest::Method::GET => req = req.body(""),
            Body::None => {}
        }
        let req = req.timeout(Duration::from_secs(20));
        let resp = send_with_retry(req).await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
            // Token rejected: drop it and sign in fresh once.
            forget_session_if(&token);
            attempt += 1;
            continue;
        }
        if edge_failure(status) && edge_retries > 0 {
            let wait = EDGE_RETRY_DELAYS[EDGE_RETRY_DELAYS.len() - edge_retries];
            edge_retries -= 1;
            edge_failed.get_or_insert(status);
            tokio::time::sleep(wait).await;
            continue;
        }
        if !status.is_success() {
            let body = read_capped(resp, 64 * 1024).await.unwrap_or_default();
            log_http_failure("server", method.as_str(), &full_url, status, &body);
            return Err(format!("Stella API error: HTTP {}", status.as_u16()));
        }
        if let Some(first) = edge_failed {
            applog::backend_once(
                &format!("edge-recovered-{}", full_url.split('?').next().unwrap_or("")),
                Duration::from_secs(60),
                "info",
                "server",
                format!(
                    "Stella answered {method} {} with HTTP {}, then worked when asked again.",
                    for_log(&full_url),
                    status_text(first)
                ),
            );
        }
        let body = read_capped(resp, MAX_API_BYTES).await?;
        return serde_json::from_slice(&body).map_err(|e| {
            applog::backend_once(
                &format!("not-json-{}", full_url.split('?').next().unwrap_or("")),
                Duration::from_secs(60),
                "error",
                "server",
                format!("Stella's reply to {method} {full_url} wasn't readable ({e}): {}", applog::snippet(&body, 300)),
            );
            e.to_string()
        });
    }
    applog::backend(
        "error",
        "account",
        format!("Stella turned the session down twice (HTTP 401 for {method} {path}), even after a fresh sign-in."),
    );
    Err("Stella rejected the session twice.".into())
}

/// Send a request, trying again (twice, briefly spaced) when it fails before
/// any answer arrives: a pooled connection Stella's edge had already closed,
/// or a blip in the network. Every request through here is safe to repeat
/// (reads, PUT/DELETE of a room's cheer or favorite, and a photo's cheer, which
/// sets a state rather than toggling one, as starring a friend does; a join
/// request sent twice only asks twice, and a friend request accepted twice is
/// still one friend). A timeout isn't retried —
/// that has already waited its full 20 seconds.
async fn send_with_retry(req: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
    let mut last = None;
    for attempt in 0..3u64 {
        let Some(this) = req.try_clone() else {
            return req.send().await.map_err(|e| unreachable_message(&e));
        };
        match this.send().await {
            Ok(resp) => return Ok(resp),
            Err(e) if !e.is_timeout() && attempt < 2 => {
                last = Some(e);
                tokio::time::sleep(Duration::from_millis(400 * (attempt + 1))).await;
            }
            Err(e) => return Err(unreachable_message(&e)),
        }
    }
    Err(last.map(|e| unreachable_message(&e)).unwrap_or_else(|| "Couldn't reach Stella.".into()))
}

/// "Couldn't reach Stella (<cause>)." reqwest's own text is only "error sending
/// request for url (…)"; the reason — refused, reset, closed early, DNS — is
/// at the bottom of its source chain.
pub(crate) fn unreachable_message(e: &reqwest::Error) -> String {
    let reason = unreachable_cause(e);
    if let Some(url) = e.url() {
        let host = url.host_str().unwrap_or("");
        applog::backend_once(
            &format!("unreachable-{host}-{reason}"),
            Duration::from_secs(60),
            "error",
            "server",
            format!("Couldn't reach Stella at {}: {reason}", for_log(url.as_str())),
        );
    }
    format!("Couldn't reach Stella ({reason}). Check your internet connection and try again.")
}

/// The root cause of a failed request: "timed out", "connection refused",
/// "dns error: …".
fn unreachable_cause(e: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = e;
    while let Some(next) = cause.source() {
        cause = next;
    }
    if e.is_timeout() { "timed out".to_string() } else { cause.to_string() }
}

/// `account_id` of the signed-in user (used for "my rooms" etc.).
pub async fn my_account_id() -> Result<i64, String> {
    ensure_session().await.map(|(_, id)| id)
}

// ─── Reshaping ──────────────────────────────────────────────────────────────

/// Absolute image URL for a Rec Room `ImageName`, at `width` px. Stella serves
/// these openly (no User-Agent or token needed). The base is the name server's
/// `Images`, as last fetched — rows are reshaped after an API call, which has
/// fetched it by then — or the built-in one before that.
fn img_url(name: &str, width: u32) -> String {
    let name = name.trim();
    if name.is_empty() || name == "DefaultProfileImage" {
        return String::new();
    }
    let cached = SERVICE_URLS
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|(map, _)| map.get("Images").cloned()));
    let base = cached.unwrap_or_else(|| {
        SERVICES.iter().find(|(_, k, _)| *k == "Images").map(|(_, _, f)| f.to_string()).unwrap_or_default()
    });
    format!("{}/{name}?width={width}", base.trim_end_matches('/'))
}

fn i64_at(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

/// Map of accountId → (username, displayName, avatarUrl) for creator attribution.
pub(crate) type People = std::collections::HashMap<i64, Value>;

/// Look up a set of accounts in one `/account/bulk` call.
pub(crate) async fn resolve_accounts(ids: &[i64]) -> People {
    let mut map = People::new();
    if ids.is_empty() {
        return map;
    }
    let query = ids.iter().map(|id| format!("id={id}")).collect::<Vec<_>>().join("&");
    let Ok(arr) = api_get(&format!("/account/bulk?{query}")).await else {
        return map;
    };
    if let Some(rows) = arr.as_array() {
        for p in rows {
            let id = i64_at(p, "accountId");
            if id != 0 {
                map.insert(id, p.clone());
            }
        }
    }
    map
}

/// Players asked about in one `/api/players/v2/progression/bulk` call.
const LEVEL_BATCH: usize = 25;

/// Each of `ids`' level, from `/api/players/v2/progression/bulk`, which
/// answers `[{ PlayerId, Level, XP }]`. Batched, side by side. A player it
/// leaves out, or a level of 0, is simply absent.
async fn player_levels(ids: &[i64]) -> std::collections::HashMap<i64, i64> {
    let batches = futures_util::future::join_all(ids.chunks(LEVEL_BATCH).map(|chunk| async move {
        let query = chunk.iter().map(|id| format!("id={id}")).collect::<Vec<_>>().join("&");
        api_get(&format!("/api/players/v2/progression/bulk?{query}")).await.ok()
    }))
    .await;
    batches
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_array().cloned())
        .flatten()
        .map(|p| (i64_at(&p, "PlayerId"), i64_at(&p, "Level")))
        .filter(|&(id, level)| id != 0 && level > 0)
        .collect()
}

/// Set `level` on each people row (null where unknown, which hides it).
async fn attach_levels(rows: &mut [Value]) {
    let ids: Vec<i64> = rows.iter().map(|r| i64_at(r, "id")).filter(|&id| id > 0).collect();
    if ids.is_empty() {
        return;
    }
    let levels = player_levels(&ids).await;
    for row in rows {
        row["level"] = levels.get(&i64_at(row, "id")).map_or(Value::Null, |level| json!(level));
    }
}

/// Reshape one Stella room into the frontend's room row, flattening `Stats` and
/// attaching an absolute thumbnail and (when resolved) its creator.
fn room_row(r: &Value, creators: &People) -> Value {
    let stats = r.get("Stats").cloned().unwrap_or(Value::Null);
    let stat = |k: &str| stats.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let creator_id = i64_at(r, "CreatorAccountId");
    let creator = creators.get(&creator_id);
    let image_name = str_at(r, "ImageName");

    json!({
        "RoomId": i64_at(r, "RoomId"),
        "Name": str_at(r, "Name"),
        "Description": str_at(r, "Description"),
        "ImageName": image_name,
        "ThumbUrl": img_url(image_name, 512),
        "CreatorPlayerId": creator_id,
        "CreatorUsername": creator.map(|c| str_at(c, "username")).filter(|u| !u.is_empty()).unwrap_or("Unknown"),
        "CreatorAvatarUrl": creator.map(|c| img_url(str_at(c, "profileImage"), 128)).unwrap_or_default(),
        "CheerCount": stat("CheerCount"),
        "FavoriteCount": stat("FavoriteCount"),
        "VisitCount": stat("VisitCount"),
        "ActivePlayerCount": stat("VisitorCount"),
        "CreatedAt": str_at(r, "CreatedAt"),
        "Accessibility": r.get("Accessibility").cloned().unwrap_or(Value::Null),
    })
}

/// Reshape one Stella account into the frontend's people row.
pub(crate) fn person_row(p: &Value) -> Value {
    let image = str_at(p, "profileImage");
    json!({
        "id": i64_at(p, "accountId"),
        "userName": str_at(p, "username"),
        "displayName": str_at(p, "displayName"),
        "bio": "",
        "profileImage": image,
        "AvatarUrl": img_url(image, 256),
        // Stella has no public presence for arbitrary players (only friends,
        // via a separate call), so leave it unknown rather than assert offline.
        "isOnline": Value::Null,
    })
}

/// Pull the room array out of whichever envelope an endpoint used: `hot` returns
/// `{ Results: [...] }`, `search` returns `{ TotalResults, Results }`, and the
/// `ownedby`/`createdby` lists return a bare array. Takes the answer, so the
/// rooms are moved out of it rather than copied.
fn rooms_of(v: Value) -> (Vec<Value>, Option<i64>) {
    match v {
        Value::Array(rooms) => (rooms, None),
        Value::Object(mut map) => {
            let total = map.get("TotalResults").and_then(Value::as_i64);
            match map.remove("Results") {
                Some(Value::Array(rooms)) => (rooms, total),
                _ => (Vec::new(), total),
            }
        }
        _ => (Vec::new(), None),
    }
}

// ─── Public API (mirrors the server.rs command shapes) ───────────────────────

/// The full `hot` list: every public room, official and community, in
/// Stella's Hot order. Stella serves it in two parts now ([`fetch_hot_rooms`]),
/// so it is put back together once and everything else is done here: paging,
/// sorting, and the tag filters (see [`filter_tag`]), which matched Stella's
/// own `?tag=` lists room for room and in the same order.
///
/// Built once per [`HOT_FRESH`]: the list is fetched in the background as
/// soon as the player signs in ([`prefetch_rooms`]); once it is stale it is
/// still served at once while a fresh copy is fetched behind it; and only one
/// fetch is ever in flight.
///
/// Shared rather than copied: the list is tens of megabytes as parsed JSON,
/// and every page, sort and tag works from references into it, so only the
/// rows a page shows are ever copied.
struct HotCache {
    rooms: Arc<Vec<Value>>,
    at: std::time::Instant,
}
static HOT_CACHE: Mutex<Option<HotCache>> = Mutex::new(None);
const HOT_FRESH: Duration = Duration::from_secs(180);
static HOT_REFRESHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn hot_fetch_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The cached list and whether it is still fresh.
fn hot_cached() -> Option<(Arc<Vec<Value>>, bool)> {
    let guard = HOT_CACHE.lock().ok()?;
    let c = guard.as_ref()?;
    Some((Arc::clone(&c.rooms), c.at.elapsed() < HOT_FRESH))
}

/// Download the list and keep it. One at a time: a caller that waited for
/// another's download uses that instead of starting its own.
async fn fetch_hot(force: bool) -> Result<Arc<Vec<Value>>, String> {
    let _guard = hot_fetch_lock().lock().await;
    if !force {
        if let Some((rooms, true)) = hot_cached() {
            return Ok(rooms);
        }
    }
    let rows = Arc::new(fetch_hot_rooms().await?);
    // Logged out meanwhile: don't keep it.
    if !signed_out() {
        if let Ok(mut guard) = HOT_CACHE.lock() {
            *guard = Some(HotCache { rooms: Arc::clone(&rows), at: std::time::Instant::now() });
        }
    }
    Ok(rows)
}

/// Rows per `hot` page: Stella answers at most this many, whatever `take` asks.
const HOT_PAGE: i64 = 100;
/// Pages asked for at once.
const HOT_PARALLEL: usize = 10;

/// Every page of one `hot` list (`tag` empty for the plain one).
async fn hot_pages(tag: &str) -> Result<Vec<Value>, String> {
    let tag = if tag.is_empty() { String::new() } else { format!("&tag={}", urlenc(tag)) };
    let page = |skip: i64| format!("/roomserver/rooms/hot?skip={skip}&take={HOT_PAGE}{tag}");
    let (mut rows, total) = rooms_of(api_get(&page(0)).await?);
    let total = total.unwrap_or(0);
    let skips: Vec<i64> = (1..).map(|n| n * HOT_PAGE).take_while(|&s| s < total).collect();
    for chunk in skips.chunks(HOT_PARALLEL) {
        let pages = futures_util::future::join_all(chunk.iter().map(|&s| api_get_owned(page(s)))).await;
        for p in pages {
            rows.extend(rooms_of(p?).0);
        }
    }
    Ok(rows)
}

async fn api_get_owned(path: String) -> Result<Value, String> {
    api_get(&path).await
}

/// The whole room list. Until October 2026 one `hot` answer held every
/// public room, RecCenter first. Now (checked 2026-10-06) the plain list is
/// the community rooms only, 100 a page, and the 25 official ones (RecCenter,
/// Paintball…) come only from `?tag=rro`. Both are ordered by visits, most
/// first, with no exceptions across all of them, so merging the two by
/// visits rebuilds Stella's own Hot. Its paging also repeats rooms (3,963 rows
/// for 3,783 rooms, the same every time), so repeats are dropped.
async fn fetch_hot_rooms() -> Result<Vec<Value>, String> {
    let (official, community) = futures_util::future::join(hot_pages("rro"), hot_pages("")).await;
    Ok(merge_hot(official?, community?))
}

/// Official and community rooms as one list in Hot's order (visits, most
/// first; ties keep the order they came in), each room once.
fn merge_hot(official: Vec<Value>, community: Vec<Value>) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut rooms: Vec<Value> = official
        .into_iter()
        .chain(community)
        .filter(|r| seen.insert(i64_at(r, "RoomId")))
        .collect();
    let visits = |r: &Value| r.get("Stats").and_then(|s| s.get("VisitCount")).and_then(Value::as_i64).unwrap_or(0);
    rooms.sort_by_key(|r| std::cmp::Reverse(visits(r)));
    rooms
}

/// Fetch a fresh copy in the background, unless one is already coming.
fn refresh_hot_in_background() {
    use std::sync::atomic::Ordering;
    if HOT_REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }
    tauri::async_runtime::spawn(async {
        let _ = fetch_hot(true).await;
        HOT_REFRESHING.store(false, Ordering::SeqCst);
    });
}

/// Start downloading the room list now, so Rooms opens without the wait.
pub fn prefetch_rooms() {
    if signed_out() || hot_cached().is_some_and(|(_, fresh)| fresh) {
        return;
    }
    refresh_hot_in_background();
}

/// The whole list: from the cache (refreshed behind it once stale), or
/// downloaded now if there is none yet.
async fn hot_all() -> Result<Arc<Vec<Value>>, String> {
    match hot_cached() {
        Some((rooms, fresh)) => {
            if !fresh {
                refresh_hot_in_background();
            }
            Ok(rooms)
        }
        None => fetch_hot(false).await,
    }
}

/// Stella's room tags, applied here as its `?tag=` does: "rro" is the official
/// rooms (`IsRRO`), "community" every other room, and any other tag a room
/// whose `Tags` list holds it. Checked against Stella's own lists for "pvp",
/// "rro" and "community": the same rooms, in the same order.
fn filter_tag<'a>(rooms: &'a [Value], tag: &str) -> Vec<&'a Value> {
    let tag = tag.trim();
    let rro = |r: &Value| r.get("IsRRO").and_then(Value::as_bool) == Some(true);
    let kind = tag.to_ascii_lowercase();
    rooms
        .iter()
        .filter(|r| match kind.as_str() {
            "" => true,
            "rro" => rro(r),
            "community" => !rro(r),
            _ => r
                .get("Tags")
                .and_then(Value::as_array)
                .is_some_and(|tags| tags.iter().any(|t| str_at(t, "Tag").eq_ignore_ascii_case(tag))),
        })
        .collect()
}

/// Rooms list: `query` → search (server-paged), else `tag`/none → hot (paged
/// locally from the cached full list, sorted by `sort_by`; see [`sort_rooms`]).
pub async fn fetch_rooms(app: &tauri::AppHandle, skip: i64, take: i64, query: &str, tag: &str, sort_by: i64) -> Value {
    let query = query.trim();
    let tag = tag.trim();
    let skip = skip.max(0);

    // Search is the one endpoint that pages server-side and returns a real
    // total, so it is used as-is.
    if !query.is_empty() {
        let path = format!("/roomserver/rooms/search?query={}&skip={}&take={}", urlenc(query), skip, take);
        let data = match api_get(&path).await {
            Ok(v) => v,
            Err(e) => return json!({ "success": false, "error": e }),
        };
        let (rows, total) = rooms_of(data);
        return finish_room_page(rows, skip, take, total).await;
    }

    // Hot (optionally tag-filtered): whole list cached, sorted and sliced
    // locally. Only the page's rows are copied out of the cache.
    let hot = match hot_all().await {
        Ok(v) => v,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    let mut all = filter_tag(&hot, tag);
    sort_rooms(app, &mut all, sort_by).await;
    let total = all.len() as i64;
    let page: Vec<Value> = all.into_iter().skip(skip as usize).take(take.max(1) as usize).cloned().collect();
    finish_room_page(page, skip, take, Some(total)).await
}

/// The Rooms tab's sort list, applied to the whole hot list (Stella's API takes
/// no sort): 0 Hot (Stella's own order), 1 Newest, 2 Most Visited, 3 Most
/// Cheered, 4 Most Favorited, 5 Most Players — which also drops every room
/// with nobody in it, so the list is only where people are. Ties keep Hot's
/// order.
async fn sort_rooms(app: &tauri::AppHandle, rooms: &mut Vec<&Value>, sort_by: i64) {
    let stat = |r: &Value, k: &str| r.get("Stats").and_then(|s| s.get(k)).and_then(Value::as_i64).unwrap_or(0);
    match sort_by {
        // ISO-8601 timestamps sort as text.
        1 => rooms.sort_by(|a, b| str_at(b, "CreatedAt").cmp(str_at(a, "CreatedAt"))),
        2 => rooms.sort_by_key(|r| std::cmp::Reverse(stat(r, "VisitCount"))),
        3 => rooms.sort_by_key(|r| std::cmp::Reverse(stat(r, "CheerCount"))),
        4 => rooms.sort_by_key(|r| std::cmp::Reverse(stat(r, "FavoriteCount"))),
        5 => {
            let live = live_by_room(app, rooms).await;
            rooms.retain(|r| live.contains_key(&i64_at(r, "RoomId")));
            rooms.sort_by_key(|r| std::cmp::Reverse(live.get(&i64_at(r, "RoomId")).copied().unwrap_or(0)));
        }
        _ => {}
    }
}

/// Rooms from the top of Hot always checked for Most Players, besides those
/// the hub has seen players in; and the most that are ever checked at once.
const LIVE_TOP_ROOMS: usize = 12;
const LIVE_MAX_CHECKED: usize = 80;
/// Without the hub, how far down Hot is checked instead.
const LIVE_FALLBACK_ROOMS: usize = 120;

/// Players in each room now, for Most Players: the exact per-room counts of
/// public copies (they include hidden players, which the hub can't place),
/// plus the private copies the hub sees — what the cards' badges add up. Asking every room would be thousands of requests, so the hub picks
/// which to ask: every room it has seen anyone in — it sees all rooms at once,
/// for free — plus the top of Hot. While the hub can't help (just started, or
/// paused because the game is running), the top of Hot is asked instead, where
/// nearly everyone is. All through the same one-minute cache as the badges.
async fn live_by_room(app: &tauri::AppHandle, rooms: &[&Value]) -> std::collections::HashMap<i64, usize> {
    let listed: std::collections::HashSet<i64> = rooms.iter().map(|r| i64_at(r, "RoomId")).collect();
    let mut ids: Vec<i64> = Vec::new();
    match crate::stella_hub::room_counts(app.clone()).filter(|c| !c.is_empty()) {
        Some(seen) => {
            // Busiest first, so the cap drops the quietest; only rooms in this
            // list (not dorms or unlisted rooms).
            let mut busy: Vec<(i64, usize)> = seen.into_iter().filter(|(id, _)| listed.contains(id)).collect();
            busy.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
            ids.extend(busy.into_iter().map(|(id, _)| id));
            for r in rooms.iter().take(LIVE_TOP_ROOMS) {
                let id = i64_at(r, "RoomId");
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            ids.truncate(LIVE_MAX_CHECKED);
        }
        None => ids.extend(rooms.iter().take(LIVE_FALLBACK_ROOMS).map(|r| i64_at(r, "RoomId"))),
    }
    ids.retain(|&id| id != 0);

    let mut counts = std::collections::HashMap::new();
    for chunk in ids.chunks(20) {
        let got = futures_util::future::join_all(chunk.iter().map(|&id| async move {
            let n = match cached_room_players(id) {
                Some(n) => Some(n),
                None => fetch_room_players(id).await,
            };
            (id, n.unwrap_or(0))
        }))
        .await;
        for (id, n) in got {
            if n > 0 {
                counts.insert(id, n as usize);
            }
        }
    }
    // Plus the private copies, which the per-room list leaves out.
    if let Some(private) = crate::stella_hub::private_counts() {
        for (id, n) in private {
            if listed.contains(&id) {
                *counts.entry(id).or_insert(0) += n;
            }
        }
    }
    counts
}

/// Resolve the page's creators and shape the rows. `total` is `Some` when known
/// exactly (hot's full length, or search's reported total).
async fn finish_room_page(rows: Vec<Value>, skip: i64, take: i64, total: Option<i64>) -> Value {
    let ids: Vec<i64> = {
        let mut set = std::collections::BTreeSet::new();
        for r in &rows {
            let id = i64_at(r, "CreatorAccountId");
            if id != 0 {
                set.insert(id);
            }
        }
        set.into_iter().collect()
    };
    let creators = resolve_accounts(&ids).await;
    let mut shaped: Vec<Value> = rows.iter().map(|r| room_row(r, &creators)).collect();
    attach_live_players(&mut shaped).await;

    let (total_results, total_known) = match total {
        Some(t) => (t, true),
        None => {
            let shown = skip + shaped.len() as i64;
            if (shaped.len() as i64) < take { (shown, true) } else { (shown + 1, false) }
        }
    };
    json!({
        "success": true,
        "data": { "Results": shaped, "TotalResults": total_results, "TotalKnown": total_known }
    })
}

// ─── Live player counts ─────────────────────────────────────────────────────
//
// Stella publishes no players-online total, and every room's `VisitorCount`
// stat is 0. What it does answer is `/match/room/{id}/instances`: each running
// copy of a room with the ids of the players in it. Summed, that is the room's
// live count — one request per room, so it is fetched for the rooms on screen
// only, all at once, and kept briefly so paging back and forth doesn't ask again.

const LIVE_TTL: Duration = Duration::from_secs(60);
/// More rooms than this in one list (a creator's whole catalogue) aren't
/// counted: that would be a request per room for rooms nobody is looking at.
const LIVE_MAX_ROOMS: usize = 24;

/// Each room's listed player ids, briefly, so the ghost filter in
/// [`count_real`] is applied to whatever the hub knows at the time of asking.
type RoomPlayers = std::collections::HashMap<i64, (Vec<i64>, std::time::Instant)>;
static LIVE_CACHE: Mutex<Option<RoomPlayers>> = Mutex::new(None);

/// How many of a room's listed players are really there.
///
/// Stella's instance lists keep "ghosts": players who left without signing
/// off (measured 2026-10-02: a Paintball instance created the day before still
/// listed three players the hub never heard from, which is why Paintball kept
/// showing 3 at any hour). Every player in the game, hidden ones included, is
/// re-sent on the hub every 20–45 s, so once the hub has listened long enough
/// a listed player it hasn't heard from is a ghost. Until then — or while it is
/// paused for the game — the list is taken as it is.
fn count_real(ids: &[i64]) -> i64 {
    crate::stella_hub::heard_among(ids).unwrap_or(ids.len()) as i64
}

/// Players in `room_id` right now, straight from Stella (and remembered).
async fn fetch_room_players(room_id: i64) -> Option<i64> {
    let data = api_get(&format!("/match/room/{room_id}/instances")).await.ok()?;
    let ids: Vec<i64> = data
        .as_array()?
        .iter()
        .flat_map(|i| i.get("playerIds").and_then(|p| p.as_array()).cloned().unwrap_or_default())
        .filter_map(|p| p.as_i64())
        .collect();
    let players = count_real(&ids);
    if let Ok(mut guard) = LIVE_CACHE.lock() {
        let map = guard.get_or_insert_with(Default::default);
        // Forget lapsed entries so the map holds only what was recently shown.
        map.retain(|_, (_, at)| at.elapsed() < LIVE_TTL);
        map.insert(room_id, (ids, std::time::Instant::now()));
    }
    Some(players)
}

fn cached_room_players(room_id: i64) -> Option<i64> {
    let guard = LIVE_CACHE.lock().ok()?;
    let (ids, at) = guard.as_ref()?.get(&room_id)?;
    (at.elapsed() < LIVE_TTL).then(|| count_real(ids))
}

/// Set `LivePlayers` on each shaped room row: a number, or null if it couldn't
/// be read (the UI then shows nothing rather than a misleading 0).
async fn attach_live_players(rows: &mut [Value]) {
    if rows.is_empty() || rows.len() > LIVE_MAX_ROOMS {
        return;
    }
    let counts = futures_util::future::join_all(rows.iter().map(|r| {
        let id = i64_at(r, "RoomId");
        async move {
            match cached_room_players(id) {
                Some(n) => Some(n),
                None if id != 0 => fetch_room_players(id).await,
                None => None,
            }
        }
    }))
    .await;
    // Private copies, which that list leaves out, from the live hub.
    let private = crate::stella_hub::private_counts();
    for (row, count) in rows.iter_mut().zip(counts) {
        row["LivePlayers"] = json!(count);
        row["PrivatePlayers"] = match &private {
            Some(p) => json!(p.get(&i64_at(row, "RoomId")).copied().unwrap_or(0)),
            None => Value::Null,
        };
    }
}

/// A room's live player counts, fetched fresh — for the room page, which may
/// sit open a while after the list it came from was loaded:
/// `{ players, private }`, `players` being the public copies (exact, null if
/// it couldn't be read) and `private` the private ones the hub can place (null
/// while it isn't connected).
#[tauri::command]
pub async fn stella_room_players(room_id: i64) -> Value {
    let players = fetch_room_players(room_id).await;
    let private = crate::stella_hub::private_counts().map(|p| p.get(&room_id).copied().unwrap_or(0));
    json!({ "players": players, "private": private })
}

/// Before a friend's JOIN: is the copy of the room they are in full?
///
/// The game asks Stella to put the player in the friend's copy, and when that
/// copy is at its limit Stella answers "Room Full" and the game sends them to
/// their dorm instead. Each copy in `/match/room/{id}/instances` carries
/// Stella's own `isFull` (checked 2026-10-03: a GoldenTrophy copy with 4 of its
/// 4 players said true, one with 3 said false), so the launcher can say so
/// first. Answers `{ known: false }` when the friend isn't in a listed copy (a
/// private copy is never listed) or the list couldn't be read: then nothing is
/// known and the JOIN goes ahead as before.
#[tauri::command]
pub async fn stella_join_check(player_id: i64, room_id: i64) -> Value {
    if player_id <= 0 || room_id <= 0 {
        return json!({ "known": false });
    }
    match api_get(&format!("/match/room/{room_id}/instances")).await {
        Ok(data) => join_check_of(&data, player_id),
        Err(_) => json!({ "known": false }),
    }
}

// ─── Request to join ────────────────────────────────────────────────────────
//
// A friend in a private copy of a room can't be joined outright: Stella's
// matchmaker answers the game's `+join:` (`matchmake/player/{id}`) with
// RoomDoesNotExist, which the game shows as "Room does not exist". The game
// asks instead. Its "Request to Join" is `POST /api/messages/v2/send` with the
// form `ToPlayerId=<friend>&Type=10&Data=`, answered
// `{"Success":true,"Message":"sent"}`; the friend's game answers with an
// invite (message type 6, Data `{"InviteId": <room copy>, "Name",
// "InviteMode"}`) and the asking game goes in through
// `matchmake/invite/{InviteId}` (all from the user's capture, 2026-10-04).
// The game signs that request (X-RNSIG) as it signs every write; Stella
// doesn't check it, as the unsigned room and photo cheers show.
//
// The invite goes to the game, so the request is sent only once the game is
// in: when the launcher starts it for this, it waits for the game's log to say
// it has landed in a room (`request_join_after_launch`).

/// The game's message type for "Request to Join".
const REQUEST_JOIN_MESSAGE: i64 = 10;

/// How long a launch may take to land the player in a room before the
/// request is given up on: a first start with a cold cache can take a while.
const LAND_LIMIT: Duration = Duration::from_secs(5 * 60);

/// After landing, the game still connects to Stella's live hub, which is
/// where the friend's invite arrives.
const AFTER_LANDING: Duration = Duration::from_secs(4);

/// Ask `player_id` to let the signed-in player into their room.
async fn send_join_request(player_id: i64) -> Result<(), String> {
    if player_id <= 0 {
        return Err("No such player.".into());
    }
    let form = [
        ("ToPlayerId", player_id.to_string()),
        ("Type", REQUEST_JOIN_MESSAGE.to_string()),
        ("Data", String::new()),
    ];
    let v = api_request_with(reqwest::Method::POST, "/api/messages/v2/send", Body::Form(&form)).await?;
    if v.get("Success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        let why = v.get("Message").and_then(Value::as_str).unwrap_or("no reason given");
        Err(format!("Stella didn't send the join request ({why})."))
    }
}

/// Ask a friend to let you in, now: the game is already running.
#[tauri::command]
pub async fn stella_request_join(player_id: i64) -> Result<(), String> {
    send_join_request(player_id).await
}

// ─── Notifications ──────────────────────────────────────────────────────────
//
// Stella's notifications are the game's messages: `GET /api/messages/v2/get`
// answers `[{ Id, FromPlayerId, Type, Data, RoomId, SentTime, PlayerEventId }]`,
// and new ones are pushed to the hub as they come (stella_hub). The types are
// Rec Room's, the same numbers Vanilla's website uses (6 an invite, 10 a join
// request, 40 an accepted friend request, ...). Stella reuses a deleted
// message's `Id` (the game deletes what it has handled; seen 2026-10-04, when
// every new message was 7), so a row's `id` here is its `Id` and `SentTime`
// together, which is what the page remembers as read. Nothing is deleted from
// here: that would take it from the game too.

/// The signed-in player's notifications, newest first, shaped as Vanilla's
/// are (`{ id, type, senderId, senderName, senderDisplay, senderAvatar,
/// roomId, message, sentTime }`). `message` is a text message's text, or an
/// invite's room name.
#[tauri::command]
pub async fn stella_notifications() -> Result<Value, String> {
    let raw = api_get("/api/messages/v2/get").await?;
    let items = raw.as_array().cloned().unwrap_or_default();
    let mut ids: Vec<i64> = items
        .iter()
        .filter_map(|n| n.get("FromPlayerId").and_then(Value::as_i64))
        .filter(|&id| id > 0)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let people = cached_accounts(&ids).await;
    let mut out: Vec<Value> = items.iter().map(|n| notification_row(n, &people)).collect();
    out.sort_by(|a, b| b["sentTime"].as_str().unwrap_or("").cmp(a["sentTime"].as_str().unwrap_or("")));
    Ok(Value::Array(out))
}

/// The game's message type for a text message.
const TEXT_MESSAGE: i64 = 30;
/// The game's message type for an invite to a room.
pub(crate) const INVITE_MESSAGE: i64 = 6;

fn notification_row(n: &Value, people: &People) -> Value {
    let kind = n.get("Type").and_then(Value::as_i64).unwrap_or(-1);
    let from = n.get("FromPlayerId").and_then(Value::as_i64).filter(|&id| id > 0);
    let sender = from.and_then(|id| people.get(&id));
    let data = n.get("Data").and_then(Value::as_str).unwrap_or("");
    let message = match kind {
        TEXT_MESSAGE => Some(data.chars().take(300).collect::<String>()),
        INVITE_MESSAGE => serde_json::from_str::<Value>(data)
            .ok()
            .and_then(|d| d.get("Name").and_then(Value::as_str).map(str::to_string))
            .map(|name| name.trim_start_matches(['^', '@']).chars().take(80).collect()),
        _ => None,
    };
    let sent = n.get("SentTime").and_then(Value::as_str).unwrap_or("");
    let id = n.get("Id").map(|v| v.to_string()).unwrap_or_default();
    json!({
        "id": format!("{id}-{sent}"),
        "type": kind,
        "senderId": from,
        "senderName": sender.map(|p| str_at(p, "username")),
        "senderDisplay": sender.map(|p| str_at(p, "displayName")).filter(|s| !s.is_empty()),
        "senderAvatar": sender.map(|p| img_url(str_at(p, "profileImage"), 256)),
        "roomId": n.get("RoomId").and_then(Value::as_i64),
        "message": message,
        "sentTime": sent,
    })
}

/// How long the hub may take to connect before asking.
const HUB_WAIT: Duration = Duration::from_secs(10);

/// With the game closed: ask `player_id` to let the player in. Their answer
/// is an invite, which arrives on the launcher's own hub connection and is
/// shown as a pop-up (`stella-invite`, see stella_hub); once invited, a plain
/// `+join:` gets into their private room (the user found, 2026-10-04). So the
/// hub must be listening first. Answers `{ listening, sent }`: not `listening`
/// means it couldn't be reached and nothing was sent, as the invite would go
/// unheard.
#[tauri::command]
pub async fn stella_ask_to_join(app: tauri::AppHandle, player_id: i64) -> Result<Value, String> {
    crate::stella_hub::ensure_running(app);
    let started = std::time::Instant::now();
    while !crate::stella_hub::listening() {
        if started.elapsed() > HUB_WAIT {
            return Ok(json!({ "listening": false, "sent": false }));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    send_join_request(player_id).await?;
    Ok(json!({ "listening": true, "sent": true }))
}

/// The game's own log. Unity moves the last one to `Player-prev.log` and
/// starts this afresh at each launch.
fn game_log_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("USERPROFILE").ok()?;
    Some(
        std::path::Path::new(&base)
            .join("AppData")
            .join("LocalLow")
            .join("Against Gravity")
            .join("Rec Room")
            .join("Player.log"),
    )
}

/// Where the game's log ends now. Taken before a launch, so that waiting for
/// it to land reads only what that launch writes.
pub fn game_log_mark() -> u64 {
    game_log_path()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len())
}

/// What the game logs on arriving in a room, its dorm included:
/// `Player joined @abod124's Dorm in scene "Dorm Room" with playerCount 1`.
const LANDED_LINE: &[u8] = b"Player joined ";

/// Follows the game's log from a mark, a chunk at a time.
struct LogFollower {
    offset: u64,
    /// The end of the last chunk, so a line split across two reads is found.
    tail: Vec<u8>,
}

impl LogFollower {
    fn new(mark: u64) -> Self {
        Self { offset: mark, tail: Vec::new() }
    }

    /// Whether the log, now `len` bytes long, has said the player landed
    /// since the last look. `read` gives the bytes from an offset to the end.
    /// A log shorter than the mark is a new one (the game started afresh), so
    /// it is read from its start.
    fn landed(&mut self, len: u64, read: impl FnOnce(u64) -> Vec<u8>) -> bool {
        if len < self.offset {
            self.offset = 0;
            self.tail.clear();
        }
        if len == self.offset {
            return false;
        }
        let chunk = read(self.offset);
        self.offset += chunk.len() as u64;
        let mut hay = std::mem::take(&mut self.tail);
        hay.extend_from_slice(&chunk);
        let found = hay.windows(LANDED_LINE.len()).any(|w| w == LANDED_LINE);
        let keep = hay.len().min(LANDED_LINE.len());
        self.tail = hay[hay.len() - keep..].to_vec();
        found
    }
}

/// Bytes of `path` from `offset` to its end (none if it can't be read).
fn read_from(path: &std::path::Path, offset: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut out = Vec::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        if f.seek(SeekFrom::Start(offset)).is_ok() {
            let _ = f.read_to_end(&mut out);
        }
    }
    out
}

/// Wait for the game, just launched, to land in a room. False if it quits
/// first or takes longer than [`LAND_LIMIT`].
async fn wait_until_landed(mark: u64) -> bool {
    let Some(path) = game_log_path() else { return false };
    let mut follower = LogFollower::new(mark);
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let len = std::fs::metadata(&path).map_or(0, |m| m.len());
        if follower.landed(len, |at| read_from(&path, at)) {
            return true;
        }
        // The process is up before launch_game returns; a little slack all
        // the same before its absence counts as the game having quit.
        if started.elapsed() > Duration::from_secs(20) && !crate::game::rec_room_running() {
            return false;
        }
        if started.elapsed() > LAND_LIMIT {
            return false;
        }
    }
}

/// Ask `player_id` to let the player in once the game, being launched now,
/// has landed. `mark` is [`game_log_mark`] from before the launch. The outcome
/// goes to the page as `stella-join-request` `{ playerId, ok, error }`.
pub fn request_join_after_launch(app: tauri::AppHandle, player_id: i64, mark: u64) {
    use tauri::Emitter;
    tauri::async_runtime::spawn(async move {
        // Make sure of a session now, while the window is likely still up: a
        // launch may hide it, and a hidden launcher doesn't sign in.
        let ready = ensure_session().await.map(|_| ());
        let result = match ready {
            Err(e) => Err(e),
            Ok(()) if !wait_until_landed(mark).await => {
                Err("The game didn't get to a room, so no join request was sent.".into())
            }
            Ok(()) => {
                tokio::time::sleep(AFTER_LANDING).await;
                send_join_request(player_id).await
            }
        };
        let _ = app.emit(
            "stella-join-request",
            json!({ "playerId": player_id, "ok": result.is_ok(), "error": result.err() }),
        );
    });
}

/// The game's currency number for tokens (`RecCenterTokens`, after 1 for
/// `LaserTagTickets`, in the game's own list).
const TOKEN_CURRENCY: i64 = 2;

/// The signed-in player's tokens, for the account menu. Stella answers the
/// game's `/api/storefronts/v4/balance/{currency}` with
/// `[{ Balance, CurrencyType, Platform }]` (checked 2026-10-03; the v2 path the
/// game also knows is a 404 there).
#[tauri::command]
pub async fn stella_tokens() -> Result<i64, String> {
    let data = api_get(&format!("/api/storefronts/v4/balance/{TOKEN_CURRENCY}")).await?;
    token_balance(&data).ok_or_else(|| "Stella sent no token balance.".into())
}

/// The token rows of a balance answer, added up (one per platform). `None`
/// when there are none, so a changed answer reads as unknown rather than 0.
fn token_balance(data: &Value) -> Option<i64> {
    let rows: Vec<i64> = data
        .as_array()?
        .iter()
        .filter(|r| r.get("CurrencyType").and_then(Value::as_i64) == Some(TOKEN_CURRENCY))
        .filter_map(|r| r.get("Balance").and_then(Value::as_i64))
        .collect();
    (!rows.is_empty()).then(|| rows.iter().sum())
}

/// [`stella_join_check`]'s answer from an instance list. Stella's `isFull` is
/// taken as it is: it is what the game is told when it asks to join.
fn join_check_of(data: &Value, player_id: i64) -> Value {
    let instance = data.as_array().into_iter().flatten().find(|i| {
        i.get("playerIds")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|p| p.as_i64() == Some(player_id)))
    });
    let Some(instance) = instance else {
        return json!({ "known": false });
    };
    json!({
        "known": true,
        "full": instance.get("isFull").and_then(Value::as_bool).unwrap_or(false),
        "players": instance["playerIds"].as_array().map_or(0, Vec::len),
    })
}

/// People search. Stella's `/account/search` takes the text as `name` (a
/// `query` parameter is accepted and silently matches nothing), ignores
/// `skip`/`take`, and returns its best 50 matches with an exact username first.
/// So the whole answer is one page of at most 50, sliced here. Each result
/// carries the player's presence where the live hub has it.
///
/// Stella has no list of everyone, so with no text the page lists the players
/// online now instead ([`browse_online`]).
///
/// This is also how a profile opened by name (a photo's uploader, a tagged
/// player) finds the account behind it.
pub async fn fetch_people(app: &tauri::AppHandle, skip: i64, take: i64, query: &str) -> Value {
    let query = query.trim();
    let (skip, take) = (skip.max(0) as usize, take.max(1) as usize);
    if query.is_empty() {
        return browse_online(app, skip, take).await;
    }

    let path = format!("/account/search?name={}", urlenc(query));
    let data = match api_get(&path).await {
        Ok(v) => v,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    let rows = data.as_array().cloned().unwrap_or_default();
    let total = rows.len() as i64;
    let mut shaped: Vec<Value> = rows
        .iter()
        .skip(skip)
        .take(take)
        .map(|p| {
            let mut row = person_row(p);
            if let Some((online, room, private)) = crate::stella_hub::presence_for(i64_at(p, "accountId")) {
                set_presence(&mut row, online, &room, private);
            }
            row
        })
        .collect();
    attach_levels(&mut shaped).await;

    json!({
        "success": true,
        "data": { "Results": shaped, "TotalResults": total, "TotalKnown": true }
    })
}

/// Fill a people row's presence: the dot, and where they are.
fn set_presence(row: &mut Value, online: bool, room: &str, private: bool) {
    row["isOnline"] = json!(online);
    row["roomName"] = json!(room);
    row["roomPrivate"] = json!(private);
}

/// People before anything is typed: everyone the live hub has heard is
/// online (public status only, see `stella_hub::online_players`), friends
/// first, then by name, with the room each is in. `partial` is set while the
/// hub hasn't listened long enough to have heard everyone, for the page to
/// ask again; `note` says why a list is empty.
async fn browse_online(app: &tauri::AppHandle, skip: usize, take: usize) -> Value {
    use crate::stella_hub::{online_players, Online};

    let me = match my_account_id().await {
        Ok(id) => id,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    // Opened just after signing in: give the connection a moment to come up
    // rather than show an empty list.
    let mut online = online_players(app.clone());
    let started = std::time::Instant::now();
    while matches!(online, Online::Connecting) && started.elapsed() < Duration::from_secs(8) {
        tokio::time::sleep(Duration::from_millis(400)).await;
        online = online_players(app.clone());
    }
    let empty = |note: &str, partial: bool| {
        json!({
            "success": true,
            "data": { "Results": [], "TotalResults": 0, "TotalKnown": true },
            "note": note,
            "partial": partial,
        })
    };
    let (players, settled) = match online {
        Online::Players { players, settled } => (players, settled),
        Online::Connecting => return empty("Finding who's online…", true),
        Online::Paused => return empty("The game shows who's online while it runs. Search for a player by name.", false),
        Online::Down => return empty("Couldn't reach Stella's live player list. Search for a player by name.", false),
        Online::Idle => return empty("Open Stella to see who's online.", false),
    };
    let players: Vec<_> = players.into_iter().filter(|p| p.id != me).collect();
    if players.is_empty() {
        return if settled { empty("Nobody else is online right now.", false) } else { empty("Finding who's online…", true) };
    }

    let ids: Vec<i64> = players.iter().map(|p| p.id).collect();
    let accounts = cached_accounts(&ids).await;
    // Friends first, then by display name. A player whose account couldn't be
    // read is left out: there would be no name to show.
    let mut rows: Vec<(bool, String, Value)> = players
        .iter()
        .filter_map(|p| {
            let mut row = person_row(accounts.get(&p.id)?);
            set_presence(&mut row, true, &p.room_name, p.private);
            let name = format!("{}\0{}", str_at(&row, "displayName"), str_at(&row, "userName")).to_lowercase();
            Some((!p.friend, name, row))
        })
        .collect();
    rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let total = rows.len() as i64;
    let mut page: Vec<Value> = rows.into_iter().skip(skip).take(take).map(|(_, _, row)| row).collect();
    attach_levels(&mut page).await;
    json!({
        "success": true,
        "data": { "Results": page, "TotalResults": total, "TotalKnown": true },
        "partial": !settled,
    })
}

/// Accounts looked up for the online list, kept a while: the same few hundred
/// players come and go all evening, and paging or coming back to the tab
/// shouldn't look them all up again.
type AccountCache = std::collections::HashMap<i64, (Value, std::time::Instant)>;
static ACCOUNT_CACHE: Mutex<Option<AccountCache>> = Mutex::new(None);
const ACCOUNT_TTL: Duration = Duration::from_secs(30 * 60);
/// Accounts asked for in one `/account/bulk` call.
const ACCOUNT_BATCH: usize = 25;

/// `ids`' accounts: from [`ACCOUNT_CACHE`], and the rest looked up in
/// batches, side by side.
async fn cached_accounts(ids: &[i64]) -> People {
    let mut found = People::new();
    let mut missing = Vec::new();
    {
        let guard = ACCOUNT_CACHE.lock().ok();
        let cache = guard.as_ref().and_then(|g| g.as_ref());
        for &id in ids {
            match cache.and_then(|c| c.get(&id)).filter(|(_, at)| at.elapsed() < ACCOUNT_TTL) {
                Some((account, _)) => {
                    found.insert(id, account.clone());
                }
                None => missing.push(id),
            }
        }
    }
    let batches = futures_util::future::join_all(missing.chunks(ACCOUNT_BATCH).map(resolve_accounts)).await;
    let fetched: Vec<(i64, Value)> = batches.into_iter().flatten().collect();
    if let Ok(mut guard) = ACCOUNT_CACHE.lock() {
        let cache = guard.get_or_insert_with(Default::default);
        cache.retain(|_, (_, at)| at.elapsed() < ACCOUNT_TTL);
        let now = std::time::Instant::now();
        cache.extend(fetched.iter().map(|(id, account)| (*id, (account.clone(), now))));
    }
    found.extend(fetched);
    found
}

/// A Stella account or room id as the page sent it, which must be a plain
/// number. It goes into an API path, sent with the player's token, so
/// anything else is refused rather than encoded: a `..` there would ask for a
/// different path on the API.
fn numeric_id(id: &str) -> Result<i64, Value> {
    id.trim()
        .parse::<i64>()
        .ok()
        .filter(|&id| id > 0)
        .ok_or_else(|| json!({ "success": false, "error": "That isn't a Stella id." }))
}

/// Rooms a given account owns.
pub async fn fetch_user_rooms(user_id: &str, skip: i64, take: i64) -> Value {
    let user_id = match numeric_id(user_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let skip = skip.max(0);
    let path = format!("/roomserver/rooms/ownedby/{user_id}?skip={skip}&take={take}");
    let data = match api_get(&path).await {
        Ok(v) => v,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    let (rows, total) = rooms_of(data);
    let ids: Vec<i64> = rows.iter().map(|r| i64_at(r, "CreatorAccountId")).filter(|&i| i != 0).collect();
    let creators = resolve_accounts(&ids).await;
    let mut shaped: Vec<Value> = rows.iter().map(|r| room_row(r, &creators)).collect();
    attach_live_players(&mut shaped).await;
    let total_results = total.unwrap_or(skip + shaped.len() as i64);
    json!({
        "success": true,
        "data": { "Results": shaped, "TotalResults": total_results, "TotalKnown": true }
    })
}

/// Reshape one Stella photo into the feed card's shape, with its uploader
/// resolved and an absolute image URL.
fn photo_row(p: &Value, creators: &People) -> Value {
    let image = str_at(p, "ImageName");
    let creator_id = i64_at(p, "PlayerId");
    let creator = creators.get(&creator_id);
    json!({
        "Id": i64_at(p, "Id"),
        "ImageName": image,
        "ThumbUrl": img_url(image, 1024),
        "RoomId": i64_at(p, "RoomId"),
        "RoomName": "",
        "CheerCount": i64_at(p, "CheerCount"),
        // Stella has no comments (no comment route answers, and every photo's
        // count is 0, checked 2026-10-03), so no count is passed on and the
        // card and photo page leave the stat out, as for Vanilla.
        "CommentCount": Value::Null,
        "Description": p.get("Description").cloned().unwrap_or(Value::Null),
        "CreatedAt": str_at(p, "CreatedAt"),
        "CreatorPlayerId": creator_id,
        "CreatorUsername": creator.map(|c| str_at(c, "username")).unwrap_or(""),
        "CreatorDisplayName": creator.map(|c| str_at(c, "displayName")).unwrap_or(""),
        "CreatorAvatarUrl": creator.map(|c| img_url(str_at(c, "profileImage"), 128)).unwrap_or_default(),
    })
}

/// Photos taken in a room. Unlike Radium/Vanilla (which scan a global feed),
/// Stella has a direct per-room endpoint.
pub async fn fetch_room_photos(room_id: &str, skip: i64, take: i64) -> Value {
    let room_id = match numeric_id(room_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let skip = skip.max(0);
    let path = format!("/api/images/v4/room/{room_id}?skip={skip}&take={take}");
    let data = match api_get(&path).await {
        Ok(v) => v,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    let rows = data.as_array().cloned().unwrap_or_default();
    let ids: Vec<i64> = {
        let mut set = std::collections::BTreeSet::new();
        for p in &rows {
            let id = i64_at(p, "PlayerId");
            if id != 0 {
                set.insert(id);
            }
        }
        set.into_iter().collect()
    };
    let creators = resolve_accounts(&ids).await;
    let shaped: Vec<Value> = rows.iter().map(|p| photo_row(p, &creators)).collect();
    let shown = skip + shaped.len() as i64;
    let (total, known) = if (shaped.len() as i64) < take { (shown, true) } else { (shown + 1, false) };
    json!({
        "success": true,
        "data": { "Results": shaped, "TotalResults": total, "TotalKnown": known }
    })
}

/// Photos a given account has taken.
pub async fn fetch_user_photos(user_id: &str, skip: i64, take: i64) -> Value {
    let user_id = match numeric_id(user_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let skip = skip.max(0);
    let path = format!("/api/images/v5/player/{user_id}?skip={skip}&take={take}");
    let data = match api_get(&path).await {
        Ok(v) => v,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    let rows = data.as_array().cloned().unwrap_or_default();
    let ids: Vec<i64> = rows.iter().map(|p| i64_at(p, "PlayerId")).filter(|&i| i != 0).collect();
    let creators = resolve_accounts(&ids).await;
    let shaped: Vec<Value> = rows.iter().map(|p| photo_row(p, &creators)).collect();
    let shown = skip + shaped.len() as i64;
    let (total, known) = if (shaped.len() as i64) < take { (shown, true) } else { (shown + 1, false) };
    json!({
        "success": true,
        "data": { "Results": shaped, "TotalResults": total, "TotalKnown": known }
    })
}

/// The cheers a player has been given, by kind, from their reputation row:
/// what the game's own profile shows under the name. Null when the row is
/// missing (the read failed, or a system account).
fn cheers_of(reputation: Option<&Value>) -> Value {
    match reputation {
        Some(r) => json!({
            "general": i64_at(r, "CheerGeneral"),
            "helpful": i64_at(r, "CheerHelpful"),
            "creative": i64_at(r, "CheerCreative"),
            "greatHost": i64_at(r, "CheerGreatHost"),
            "sportsman": i64_at(r, "CheerSportsman"),
        }),
        None => Value::Null,
    }
}

/// `/api/relationships/mutualfriends` rows (`{AccountId, Username,
/// DisplayName, ProfileImage}`) in the shape the page draws people in.
fn mutual_friends_of(rows: &Value) -> Vec<Value> {
    rows.as_array()
        .map(|rows| {
            rows.iter()
                .filter(|r| i64_at(r, "AccountId") != 0)
                .map(|r| {
                    json!({
                        "id": i64_at(r, "AccountId"),
                        "userName": str_at(r, "Username"),
                        "displayName": str_at(r, "DisplayName"),
                        "AvatarUrl": img_url(str_at(r, "ProfileImage"), 128),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Profile details for one account: bio, subscriber count, level, avatar and
/// banner, and what the game's profile shows besides — the display emoji, a
/// Junior account, cheers by kind, and the friends you share.
///
/// Fills the person detail view. Friends and visit counts aren't exposed for an
/// arbitrary account on Stella (friends is only the signed-in user's own list),
/// so those come back empty and the UI hides them — like Vanilla.
pub async fn user_details(account_id: i64) -> Value {
    // Five separate reads, side by side rather than one after another.
    let (account_path, bio_path, reputation_path, mutual_path) = (
        format!("/account/bulk?id={account_id}"),
        format!("/account/{account_id}/bio"),
        format!("/api/playerReputation/v2/bulk?id={account_id}"),
        format!("/api/relationships/mutualfriends?id={account_id}"),
    );
    let ids = [account_id];
    // Asked about yourself, "mutual friends" answers your whole friends list,
    // so your own profile doesn't ask.
    let mutual = async {
        if my_account_id().await.ok() == Some(account_id) {
            return Vec::new();
        }
        api_get(&mutual_path).await.map(|v| mutual_friends_of(&v)).unwrap_or_default()
    };
    let (account, bio, reputation, levels, mutual) = tokio::join!(
        api_get(&account_path),
        api_get(&bio_path),
        api_get(&reputation_path),
        player_levels(&ids),
        mutual,
    );

    // Core record (display name, images). A system/placeholder account id (e.g.
    // 1, the owner of the official rooms) returns an empty array here.
    let account = account.ok().and_then(|v| v.as_array().and_then(|a| a.first().cloned()));
    let (username, display, avatar, banner) = match account.as_ref() {
        Some(a) => (
            str_at(a, "username").to_string(),
            str_at(a, "displayName").to_string(),
            img_url(str_at(a, "profileImage"), 256),
            {
                let b = str_at(a, "bannerImage");
                if b.is_empty() { String::new() } else { img_url(b, 1000) }
            },
        ),
        None => (String::new(), String::new(), String::new(), String::new()),
    };
    // When the account was made, for the profile's JOINED tile. Some old
    // accounts carry year 1 here; the page leaves those out.
    let created_at = account.as_ref().map(|a| str_at(a, "createdAt").to_string()).unwrap_or_default();

    // Shown beside the name, as the game does. Capped: it is meant to be one
    // emoji, and it goes on the page as text.
    let emoji: String = account
        .as_ref()
        .map(|a| str_at(a, "displayEmoji").trim().chars().take(8).collect())
        .unwrap_or_default();
    let junior = account
        .as_ref()
        .and_then(|a| a.get("isJunior"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let bio = bio.ok().map(|v| str_at(&v, "bio").to_string()).unwrap_or_default();

    // Reputation carries the subscriber count and the cheers. It answers 500
    // for some system accounts, so a failure just leaves those blank.
    let reputation = reputation.ok().and_then(|v| v.as_array().and_then(|a| a.first().cloned()));
    let subscribers = reputation
        .as_ref()
        .map(|r| i64_at(r, "SubscriberCount").to_string())
        .unwrap_or_default();

    json!({
        "success": true,
        "friends": "",
        "subscribers": subscribers,
        "visits": "",
        "status": "",
        // Null where unknown, which hides the LEVEL tile.
        "level": levels.get(&account_id),
        "createdAt": created_at,
        "bio": bio,
        "banner": banner,
        "avatar": avatar,
        "userName": username,
        "displayName": display,
        "emoji": emoji,
        "junior": junior,
        "cheers": cheers_of(reputation.as_ref()),
        "mutualFriends": mutual,
    })
}

// ─── Inventions ─────────────────────────────────────────────────────────────
//
// `/api/inventions/v1/fromcreators?id=<account>&skip=&take=` lists what a
// player has made, newest first (measured 2026-10-04: creator 2702's 16, and
// skip/take page it). `creatorId=`, `creatorIds=` and `accountId=` are taken
// too and always answer `[]` — the same trap as account search's `query=`.
// Rows are the stock Rec Room invention. Stella leaves CheerCount at 0 on
// every one sampled, so downloads is the number that means something.

/// One invention in the shape the profile's grid draws.
fn invention_row(i: &Value) -> Value {
    let image = str_at(i, "ImageName");
    let flag = |key: &str| i.get(key).and_then(Value::as_bool).unwrap_or(false);
    // The game's placeholder, which is no description at all.
    let description = match str_at(i, "Description").trim() {
        "No description yet" => "",
        text => text,
    };
    json!({
        "Id": i64_at(i, "InventionId"),
        "Name": str_at(i, "Name"),
        "Description": description,
        "ImageName": image,
        "ThumbUrl": img_url(image, 1024),
        "Downloads": i64_at(i, "NumDownloads"),
        "CheerCount": i64_at(i, "CheerCount"),
        "Price": i64_at(i, "Price"),
        "Certified": flag("IsCertifiedInvention"),
        "Featured": flag("IsFeatured"),
        "CreatedAt": str_at(i, "CreatedAt"),
        "CreatorPlayerId": i64_at(i, "CreatorPlayerId"),
    })
}

/// Whether the game would show this invention to other players.
fn invention_listed(i: &Value) -> bool {
    i.get("IsPublished").and_then(Value::as_bool).unwrap_or(true)
        && !i.get("HideFromPlayer").and_then(Value::as_bool).unwrap_or(false)
}

/// Inventions a given account has published.
pub async fn fetch_user_inventions(user_id: &str, skip: i64, take: i64) -> Value {
    let user_id = match numeric_id(user_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let skip = skip.max(0);
    let path = format!("/api/inventions/v1/fromcreators?id={user_id}&skip={skip}&take={take}");
    let data = match api_get(&path).await {
        Ok(v) => v,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    let rows = data.as_array().cloned().unwrap_or_default();
    // Paged by what the server sent, before the unlisted ones are dropped:
    // a short page is the last one, however many of it are shown.
    let last_page = (rows.len() as i64) < take;
    let shaped: Vec<Value> = rows.iter().filter(|i| invention_listed(i)).map(invention_row).collect();
    let shown = skip + shaped.len() as i64;
    let (total, known) = if last_page { (shown, true) } else { (shown + 1, false) };
    json!({
        "success": true,
        "data": { "Results": shaped, "TotalResults": total, "TotalKnown": known }
    })
}

/// The account whose username is exactly `name` (any case), via people search.
pub async fn account_id_for_name(name: &str) -> Option<i64> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let data = api_get(&format!("/account/search?name={}", urlenc(name))).await.ok()?;
    data.as_array()?
        .iter()
        .find(|p| str_at(p, "username").eq_ignore_ascii_case(name))
        .map(|p| i64_at(p, "accountId"))
        .filter(|&id| id != 0)
}

// ─── Friends: favorites and requests ────────────────────────────────────────
//
// The game's own calls (paths from its metadata):
// `/api/relationships/v1/favorite` and `…/v1/unfavorite` star and unstar a
// friend, and `/api/relationships/v2/acceptfriendrequest` accepts a request,
// each naming the player as `?id=`. They are GETs: POST and PUT are a 405.
// Each answers the player's relationship row as it now stands, e.g.
// `{Favorited: 1, IsFavorited: 1, IsFriend: true, RelationshipType: 3, …}`
// (measured 2026-10-05 on one of the user's friends, with their go-ahead:
// starred and unstarred again). In the friends list
// (`/api/relationships/v2/get`) `RelationshipType` 2 is a request someone
// sent the player, 1 one the player sent (the player confirmed which way 1
// goes), 3 a friend. Accepting is untested: there was no request to accept.

/// Make a relationship call about one player, answering their row as it now
/// stands.
pub(crate) async fn relationship_call(path: &str, player_id: i64) -> Result<Value, String> {
    if player_id <= 0 {
        return Err("No such player.".into());
    }
    api_get(&format!("{path}?id={player_id}")).await
}

// ─── Room cheer and favorite ────────────────────────────────────────────────
//
// `/roomserver/rooms/{id}/interactionby/me` answers `{Cheered, Favorited,
// LastVisitedAt}` for the signed-in player; PUT on `…/me/cheer` or
// `…/me/favorite` sets one and DELETE clears it, each answering the new state
// (POST is a 405). Measured on RecCenter on 2026-10-01, with the user's
// go-ahead: the room's counts moved by one and back.

fn interaction_of(v: &Value) -> Value {
    json!({
        "cheered": v.get("Cheered").and_then(Value::as_bool).unwrap_or(false),
        "favorited": v.get("Favorited").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Whether the signed-in player has cheered and favorited a room.
#[tauri::command]
pub async fn stella_room_interaction(room_id: i64) -> Result<Value, String> {
    let v = api_get(&format!("/roomserver/rooms/{room_id}/interactionby/me")).await?;
    Ok(interaction_of(&v))
}

/// Cheer or favorite a room (`on`), or take it back. `kind` is "cheer" or
/// "favorite". Answers the room's new `{ cheered, favorited }`.
#[tauri::command]
pub async fn stella_set_room_interaction(room_id: i64, kind: String, on: bool) -> Result<Value, String> {
    if kind != "cheer" && kind != "favorite" {
        return Err(format!("Unknown room interaction: {kind}"));
    }
    let method = if on { reqwest::Method::PUT } else { reqwest::Method::DELETE };
    let v = api_request(method, &format!("/roomserver/rooms/{room_id}/interactionby/me/{kind}")).await?;
    Ok(interaction_of(&v))
}

// ─── Photo cheers ───────────────────────────────────────────────────────────
//
// `GET /api/images/v5/cheered/bulk?id=1&id=2` answers `[{SavedImageId,
// IsCheered}]` for the signed-in player. `POST /api/images/v1/cheer` with the
// JSON `{"SavedImageId": id, "Cheer": bool}` sets or clears one and answers
// `{}`. The names must be spelled exactly so: any other spelling (`imageId`,
// `ImageId`, `savedImageId`) is a 404, as is clearing a cheer that isn't
// there, and a form body is a 500. Measured 2026-10-03 on one of Coach's
// photos, with the user's go-ahead: its count went 6, 7, 6.

/// Ids per cheered-state request, to keep the query string short.
const CHEER_BATCH: usize = 50;

/// Which of these photos the signed-in player has cheered.
#[tauri::command]
pub async fn stella_cheered_photos(ids: Vec<i64>) -> Result<Vec<i64>, String> {
    let ids: Vec<i64> = ids.into_iter().filter(|&id| id > 0).collect();
    let mut cheered = Vec::new();
    for chunk in ids.chunks(CHEER_BATCH) {
        let query = chunk.iter().map(|id| format!("id={id}")).collect::<Vec<_>>().join("&");
        let data = api_get(&format!("/api/images/v5/cheered/bulk?{query}")).await?;
        cheered.extend(cheered_of(&data));
    }
    Ok(cheered)
}

fn cheered_of(data: &Value) -> Vec<i64> {
    data.as_array()
        .into_iter()
        .flatten()
        .filter(|r| r.get("IsCheered").and_then(Value::as_bool) == Some(true))
        .filter_map(|r| r.get("SavedImageId").and_then(Value::as_i64))
        .collect()
}

/// Cheer a photo (`on`), or take the cheer back.
#[tauri::command]
pub async fn stella_set_photo_cheer(photo_id: i64, on: bool) -> Result<bool, String> {
    if photo_id <= 0 {
        return Err("No such photo.".into());
    }
    let body = json!({ "SavedImageId": photo_id, "Cheer": on });
    api_request_with(reqwest::Method::POST, "/api/images/v1/cheer", Body::Json(&body)).await?;
    Ok(on)
}

/// Room tag/category filters.
pub async fn fetch_filters() -> Value {
    match api_get("/api/rooms/v1/filters").await {
        Ok(data) => json!({ "success": true, "data": data }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

/// Minimal percent-encoding for a query-string value.
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {

    /// Cloudflare's error page, as the start of it was logged on 2026-10-05,
    /// is logged by its title rather than its markup.
    #[test]
    fn a_web_page_reply_is_logged_by_its_title() {
        let page = b"<!DOCTYPE html>\n<!--[if lt IE 7]> <html class=\"no-js ie6 oldie\" lang=\"en-US\"> <![endif]-->\n\
            <html class=\"no-js\" lang=\"en-US\"> <head>\n<title>stellaonline.org | 520: Web server is returning an unknown error</title>\n\
            <meta charset=\"UTF-8\" /></head><body>...</body></html>";
        assert_eq!(
            super::quoted_reply(page),
            ": a web page, \"stellaonline.org | 520: Web server is returning an unknown error\""
        );
        // Anything else is quoted as before.
        assert_eq!(super::quoted_reply(br#"{"error":"nope"}"#), r#": {"error":"nope"}"#);
        assert_eq!(super::quoted_reply(b"<html><body>no title</body></html>"), ": <html><body>no title</body></html>");
        assert_eq!(super::quoted_reply(b""), "");
    }

    #[test]
    fn only_the_edge_reporting_a_failed_server_is_retried() {
        use reqwest::StatusCode;
        for code in [502, 503, 504, 520, 521, 522, 523, 524, 525, 526, 527] {
            assert!(super::edge_failure(StatusCode::from_u16(code).unwrap()), "{code}");
        }
        for code in [400, 401, 403, 404, 405, 429, 500, 501, 530] {
            assert!(!super::edge_failure(StatusCode::from_u16(code).unwrap()), "{code}");
        }
    }

    #[test]
    fn logged_addresses_leave_out_the_steam_id() {
        assert_eq!(
            super::for_log("https://auth.stellaonline.org/auth/cachedlogin/forplatformid/0/76561198000000000"),
            "https://auth.stellaonline.org/auth/cachedlogin/forplatformid/0/<steam id>"
        );
        assert_eq!(super::for_log("https://x.stellaonline.org/a?b=1"), "https://x.stellaonline.org/a?b=1");
    }

    #[test]
    fn service_urls_join_without_doubling_the_segment() {
        use super::join_service;
        assert_eq!(
            join_service("https://auth.stellaonline.org/auth", "auth", "/auth/connect/token"),
            "https://auth.stellaonline.org/auth/connect/token"
        );
        assert_eq!(
            join_service("https://accounts.stellaonline.org/", "account", "/account/bulk?id=1"),
            "https://accounts.stellaonline.org/account/bulk?id=1"
        );
        assert_eq!(
            join_service("https://api.stellaonline.org/", "api", "/api/relationships/v2/get"),
            "https://api.stellaonline.org/api/relationships/v2/get"
        );
        assert_eq!(
            join_service("https://match.stellaonline.org/match", "match", "/match/room/1/instances"),
            "https://match.stellaonline.org/match/room/1/instances"
        );
        assert_eq!(
            join_service("https://notify.stellaonline.org/notify", "notify", "/notify/hub/v1"),
            "https://notify.stellaonline.org/notify/hub/v1"
        );
    }

    #[test]
    fn only_stella_hosts_come_from_the_name_server() {
        use super::on_stella;
        assert!(on_stella("https://auth.stellaonline.org/auth"));
        assert!(on_stella("https://stellaonline.org"));
        assert!(!on_stella("http://auth.stellaonline.org/auth"));
        assert!(!on_stella("https://stellaonline.org.evil.com/"));
        assert!(!on_stella("https://google.com/generate_204"));
        // Read by a URL parser as host `evil.com`, path `/.stellaonline.org`.
        assert!(!on_stella(r"https://evil.com\.stellaonline.org/auth"));
        assert!(!on_stella("https://auth.stellaonline.org@evil.com/auth"));
        assert!(!on_stella("https://user@auth.stellaonline.org/auth"));
    }

    #[test]
    fn notification_rows_name_the_sender_and_an_invites_room() {
        let mut people = People::new();
        people.insert(4034, json!({ "accountId": 4034, "username": "Fangame300", "displayName": "", "profileImage": "pic" }));
        // As Stella sent them, 2026-10-04.
        let invite = json!({ "Id": 7, "FromPlayerId": 4034, "Type": 6, "RoomId": 6389,
            "Data": "{\"InviteId\":70172316,\"Name\":\"@Fangame300\\u0027s Dorm\",\"InviteMode\":22}",
            "SentTime": "2026-10-04T10:37:38.005231Z" });
        let row = notification_row(&invite, &people);
        assert_eq!(row["id"], "7-2026-10-04T10:37:38.005231Z");
        assert_eq!(row["type"], 6);
        assert_eq!(row["senderName"], "Fangame300");
        assert_eq!(row["senderDisplay"], Value::Null);
        assert_eq!(row["senderAvatar"], "https://img.stellaonline.org/img/pic?width=256");
        assert_eq!(row["message"], "Fangame300's Dorm");
        // A new friend: no text, and an unknown sender has no name.
        let friend = json!({ "Id": 2, "FromPlayerId": 519, "Data": "0", "Type": 40, "RoomId": 0,
            "SentTime": "2026-10-01T19:09:54.045Z", "PlayerEventId": 1 });
        let row = notification_row(&friend, &people);
        assert_eq!(row["message"], Value::Null);
        assert_eq!(row["senderId"], 519);
        assert_eq!(row["senderName"], Value::Null);
    }

    #[test]
    fn log_follower_finds_the_landing_only_after_the_mark() {
        let old = b"...Player joined @abod124's Dorm in scene \"Dorm Room\"...
".to_vec();
        let mut f = LogFollower::new(old.len() as u64);
        // Nothing new yet, then more of the old game's lines.
        assert!(!f.landed(old.len() as u64, |_| unreachable!()));
        let mut log = old.clone();
        log.extend_from_slice(b"[Log] loading
");
        assert!(!f.landed(log.len() as u64, |at| log[at as usize..].to_vec()));
        log.extend_from_slice(b"[Log] Player joined ^RecCenter
");
        assert!(f.landed(log.len() as u64, |at| log[at as usize..].to_vec()));
    }

    #[test]
    fn log_follower_reads_a_new_log_from_its_start() {
        let mut f = LogFollower::new(5_000_000);
        let fresh = b"Mono path[0]
[Log] Player joined @x's Dorm
".to_vec();
        assert!(f.landed(fresh.len() as u64, |at| fresh[at as usize..].to_vec()));
    }

    #[test]
    fn log_follower_finds_a_line_split_between_reads() {
        let mut f = LogFollower::new(0);
        let log = b"[Log] Player joined ^RecCenter
".to_vec();
        let cut = 12; // inside "Player joined "
        assert!(!f.landed(cut as u64, |_| log[..cut].to_vec()));
        assert!(f.landed(log.len() as u64, |at| log[at as usize..].to_vec()));
    }

    #[test]
    fn cheered_of_keeps_the_cheered_ids() {
        let rows = json!([
            { "SavedImageId": 27372, "IsCheered": true },
            { "SavedImageId": 89811, "IsCheered": false },
            { "SavedImageId": 5, "IsCheered": true }
        ]);
        assert_eq!(cheered_of(&rows), vec![27372, 5]);
        assert!(cheered_of(&json!({})).is_empty());
    }

    #[test]
    fn token_balance_reads_the_token_rows() {
        assert_eq!(token_balance(&json!([{ "Balance": 0, "CurrencyType": 2, "Platform": -1 }])), Some(0));
        assert_eq!(
            token_balance(&json!([
                { "Balance": 150, "CurrencyType": 2, "Platform": -1 },
                { "Balance": 9, "CurrencyType": 1, "Platform": -1 },
                { "Balance": 25, "CurrencyType": 2, "Platform": 0 }
            ])),
            Some(175)
        );
        assert_eq!(token_balance(&json!([{ "Balance": 9, "CurrencyType": 1 }])), None);
        assert_eq!(token_balance(&json!({ "Message": "nope" })), None);
    }

    #[test]
    fn join_check_finds_the_friends_copy() {
        let list = json!([
            { "isFull": true, "playerIds": [3372, 67086, 70095, 70754], "roomId": 3, "roomInstanceId": 1 },
            { "isFull": false, "playerIds": [32183, 51504, 66781], "roomId": 3, "roomInstanceId": 2 }
        ]);
        assert_eq!(join_check_of(&list, 70095), json!({ "known": true, "full": true, "players": 4 }));
        assert_eq!(join_check_of(&list, 51504)["full"], json!(false));
        // Not in any listed copy (a private one): nothing is known.
        assert_eq!(join_check_of(&list, 5), json!({ "known": false }));
        assert_eq!(join_check_of(&json!({ "error": 1 }), 5), json!({ "known": false }));
    }

    #[test]
    fn hot_is_rebuilt_from_both_lists() {
        let room = |id: i64, visits: i64| json!({ "RoomId": id, "Stats": { "VisitCount": visits } });
        let official = vec![room(1, 500), room(2, 40)];
        // Stella's paging repeats a room now and then.
        let community = vec![room(10, 90), room(11, 40), room(10, 90), room(12, 5)];
        let ids: Vec<i64> = merge_hot(official, community).iter().map(|r| i64_at(r, "RoomId")).collect();
        // By visits; the tie (2 and 11) keeps official first; 10 once.
        assert_eq!(ids, vec![1, 10, 2, 11, 12]);
    }

    #[test]
    fn tag_filter_matches_stellas() {
        let rooms = vec![
            json!({ "RoomId": 9, "IsRRO": true, "Tags": [{ "Tag": "recroomoriginal", "Type": 2 }] }),
            json!({ "RoomId": 100, "IsRRO": false, "Tags": [{ "Tag": "pvp", "Type": 0 }] }),
            json!({ "RoomId": 101, "IsRRO": false, "Tags": [{ "Tag": "PVP", "Type": 0 }, { "Tag": "quest", "Type": 0 }] }),
            json!({ "RoomId": 102, "IsRRO": false }),
        ];
        let ids = |tag: &str| filter_tag(&rooms, tag).iter().map(|r| i64_at(r, "RoomId")).collect::<Vec<_>>();
        assert_eq!(ids(""), vec![9, 100, 101, 102]);
        assert_eq!(ids("rro"), vec![9]);
        assert_eq!(ids("community"), vec![100, 101, 102]);
        assert_eq!(ids("pvp"), vec![100, 101]);
        assert_eq!(ids("quest"), vec![101]);
        assert!(ids("horror").is_empty());
    }

    #[tokio::test]
    async fn unreachable_retries_then_names_the_cause() {
        // Port 1 on loopback: nothing listens, so every attempt is refused.
        let req = reqwest::Client::new().get("http://127.0.0.1:1/").timeout(Duration::from_secs(5));
        let started = std::time::Instant::now();
        let err = send_with_retry(req).await.unwrap_err();
        assert!(err.starts_with("Couldn't reach Stella ("), "{err}");
        assert!(!err.contains("error sending request"), "{err}");
        // Three attempts, with 0.4 s and 0.8 s between them.
        assert!(started.elapsed() >= Duration::from_millis(1100), "{:?}", started.elapsed());
    }
    use super::*;

    /// A device id that signed in is kept, read back, and anything that isn't
    /// one is ignored rather than sent.
    #[test]
    fn a_device_id_is_kept_once_it_works() {
        let dir = std::env::temp_dir().join(format!("stella-device-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join(DEVICE_ID_FILE_NAME);
        let id = "0123456789abcdef0123456789abcdef01234567";

        assert_eq!(read_device_id_file(&path), None);
        write_device_id_file(&path, id).unwrap();
        assert_eq!(read_device_id_file(&path).as_deref(), Some(id));
        // Written again unchanged, and read back past a stray newline.
        write_device_id_file(&path, id).unwrap();
        std::fs::write(&path, format!("{id}\r\n")).unwrap();
        assert_eq!(read_device_id_file(&path).as_deref(), Some(id));
        for junk in ["", "not an id", &id[..39], &format!("{id}0"), "0123456789abcdef0123456789abcdef0123456z"] {
            std::fs::write(&path, junk).unwrap();
            assert_eq!(read_device_id_file(&path), None, "{junk}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Stella's "wrong device" answer is told apart from other refusals.
    #[test]
    fn a_rejected_device_is_recognised() {
        assert!(is_device_rejection(br#"{"error":"access_denied","error_description":"platform verification failed"}"#));
        assert!(is_device_rejection(b"Platform Verification Failed"));
        assert!(!is_device_rejection(br#"{"error":"access_denied","error_description":"missing fields"}"#));
        assert!(!is_device_rejection(b"<html>Sorry, you have been blocked</html>"));
    }

    /// Only a plain positive number reaches an API path.
    #[test]
    fn ids_must_be_numbers() {
        assert_eq!(numeric_id("70541").ok(), Some(70541));
        assert_eq!(numeric_id(" 9 ").ok(), Some(9));
        for bad in ["", "..", "../account/me", "9/../../me", "-1", "0", "1.5", "9?x=1"] {
            assert!(numeric_id(bad).is_err(), "{bad}");
        }
    }

    /// A 401 for a token that has since been replaced leaves the new session.
    #[test]
    fn a_late_401_keeps_a_fresh_session() {
        let session = |token: &str| Session { access_token: token.into(), expires_at: now_secs() + 3600, account_id: 1 };
        *SESSION.lock().unwrap() = Some(session("fresh"));
        forget_session_if("expired");
        assert!(SESSION.lock().unwrap().is_some());
        forget_session_if("fresh");
        assert!(SESSION.lock().unwrap().is_none());

        // With no session and Stella not in use (hidden, or another network
        // picked), nothing signs in: no Steam, just the answer.
        assert!(!in_use());
        let refused = tokio::runtime::Runtime::new().unwrap().block_on(ensure_session());
        assert_eq!(refused.unwrap_err(), NOT_IN_USE);
    }

    /// Rooms come out of every envelope the endpoints use.
    #[test]
    fn rooms_come_out_of_each_envelope() {
        let (rooms, total) = rooms_of(json!({ "TotalResults": 159, "Results": [{ "RoomId": 1 }] }));
        assert_eq!((rooms.len(), total), (1, Some(159)));
        let (rooms, total) = rooms_of(json!([{ "RoomId": 1 }, { "RoomId": 2 }]));
        assert_eq!((rooms.len(), total), (2, None));
        let (rooms, total) = rooms_of(json!({ "error": "x" }));
        assert_eq!((rooms.len(), total), (0, None));
    }

    #[test]
    fn invention_row_drops_the_placeholder_description() {
        let i = json!({
            "InventionId": 406, "CreatorPlayerId": 2702, "Name": "gun thing",
            "Description": "No description yet", "ImageName": "InventionThumbnail-2702-x",
            "NumDownloads": 52, "CheerCount": 0, "Price": 0, "IsCertifiedInvention": false,
            "CreatedAt": "2026-05-04T19:43:48.639Z"
        });
        let row = invention_row(&i);
        assert_eq!(row["Id"], 406);
        assert_eq!(row["Description"], "");
        assert_eq!(row["Downloads"], 52);
        assert_eq!(row["Certified"], false);
        assert_eq!(row["ThumbUrl"], "https://img.stellaonline.org/img/InventionThumbnail-2702-x?width=1024");
        let described = invention_row(&json!({ "Description": "  it shoots things " }));
        assert_eq!(described["Description"], "it shoots things");
    }

    #[test]
    fn unpublished_and_hidden_inventions_are_left_out() {
        assert!(invention_listed(&json!({ "IsPublished": true, "HideFromPlayer": false })));
        assert!(invention_listed(&json!({})));
        assert!(!invention_listed(&json!({ "IsPublished": false })));
        assert!(!invention_listed(&json!({ "IsPublished": true, "HideFromPlayer": true })));
    }

    #[test]
    fn mutual_friends_and_cheers_are_reshaped() {
        let rows = json!([
            { "AccountId": 66620, "Username": "TopazRaptor1251", "DisplayName": "Topaz", "ProfileImage": "ProfileThumbnail-66620-x" },
            { "AccountId": 0, "Username": "nobody" }
        ]);
        let people = mutual_friends_of(&rows);
        assert_eq!(people.len(), 1);
        assert_eq!(people[0]["id"], 66620);
        assert_eq!(people[0]["userName"], "TopazRaptor1251");
        assert_eq!(people[0]["AvatarUrl"], "https://img.stellaonline.org/img/ProfileThumbnail-66620-x?width=128");
        assert!(mutual_friends_of(&json!({ "error": "x" })).is_empty());

        let rep = json!({ "CheerGeneral": 429, "CheerHelpful": 79, "CheerCreative": 107, "CheerGreatHost": 104, "CheerSportsman": 103 });
        let cheers = cheers_of(Some(&rep));
        assert_eq!(cheers["general"], 429);
        assert_eq!(cheers["greatHost"], 104);
        assert!(cheers_of(None).is_null());
    }

    #[test]
    fn img_url_handles_empty_and_default() {
        assert_eq!(img_url("", 512), "");
        assert_eq!(img_url("DefaultProfileImage", 256), "");
        assert_eq!(img_url("RoomThumbnail-reccenter", 512), "https://img.stellaonline.org/img/RoomThumbnail-reccenter?width=512");
    }

    #[test]
    fn room_row_flattens_stats_and_builds_thumb() {
        let r = json!({
            "RoomId": 9, "Name": "RecCenter", "Description": "hi",
            "ImageName": "RoomThumbnail-reccenter", "CreatorAccountId": 1,
            "Stats": { "CheerCount": 313, "FavoriteCount": 1698, "VisitCount": 491859, "VisitorCount": 4 }
        });
        let out = room_row(&r, &People::new());
        assert_eq!(out["RoomId"], 9);
        assert_eq!(out["CheerCount"], 313);
        assert_eq!(out["VisitCount"], 491859);
        assert_eq!(out["ActivePlayerCount"], 4);
        assert_eq!(out["ThumbUrl"], "https://img.stellaonline.org/img/RoomThumbnail-reccenter?width=512");
        assert_eq!(out["CreatorUsername"], "Unknown");
    }

    #[test]
    fn person_row_maps_fields() {
        let p = json!({ "accountId": 70541, "username": "abod124", "displayName": "abod124", "profileImage": "ProfileThumbnail-70541-x" });
        let out = person_row(&p);
        assert_eq!(out["id"], 70541);
        assert_eq!(out["userName"], "abod124");
        assert_eq!(out["AvatarUrl"], "https://img.stellaonline.org/img/ProfileThumbnail-70541-x?width=256");
        assert!(out["isOnline"].is_null());
    }

    /// A lookup answer shaped like Stella's (2026-10-06): two accounts on one
    /// Steam account, the newer one listed second.
    fn two_accounts() -> Value {
        json!([
            { "platform": 0, "platformId": "1", "accountId": 70541, "lastLoginTime": "2026-10-06T20:25:43.209Z",
              "requirePassword": false, "account": { "accountId": 70541, "username": "abod124", "displayName": "abod124" } },
            { "platform": 0, "platformId": "1", "accountId": 72407, "lastLoginTime": "2026-10-06T20:25:23.984Z",
              "requirePassword": false, "account": { "accountId": 72407, "username": "CarefreeSalmon", "displayName": "HelpfulBee5763", "isJunior": true } },
        ])
    }

    #[test]
    fn linked_accounts_are_read_from_the_lookup() {
        let accounts = parse_linked_accounts(&two_accounts());
        assert_eq!(accounts.iter().map(|a| a.account_id).collect::<Vec<_>>(), [70541, 72407]);
        assert_eq!(accounts[1].account["username"], "CarefreeSalmon");
        // No account yet: Stella answers an empty list.
        assert!(parse_linked_accounts(&json!([])).is_empty());
        // Rows without an id, and repeats, are left out; a row without the
        // embedded account still counts.
        let odd = json!([{ "accountId": 0 }, { "lastLoginTime": "x" }, { "accountId": 5 }, { "accountId": 5 }]);
        let accounts = parse_linked_accounts(&odd);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account, json!({}));
        assert!(parse_linked_accounts(&json!({ "error": "nope" })).is_empty());
    }

    #[test]
    fn the_account_played_last_is_used_until_one_is_picked() {
        let accounts = parse_linked_accounts(&two_accounts());
        assert_eq!(pick_account(&accounts, None).map(|a| a.account_id), Some(70541));
        // Picked in the launcher: that one, while it is still on the Steam account.
        assert_eq!(pick_account(&accounts, Some(72407)).map(|a| a.account_id), Some(72407));
        assert_eq!(pick_account(&accounts, Some(99)).map(|a| a.account_id), Some(70541));
        assert!(pick_account(&[], Some(72407)).is_none());
    }

    #[test]
    fn an_account_with_a_password_is_passed_over_by_default() {
        let mut accounts = parse_linked_accounts(&two_accounts());
        accounts[0].require_password = true;
        assert_eq!(default_account(&accounts).map(|a| a.account_id), Some(72407));
        accounts[1].require_password = true;
        assert_eq!(default_account(&accounts).map(|a| a.account_id), Some(70541));
        // Picked on purpose, it is still used.
        accounts[1].require_password = false;
        assert_eq!(pick_account(&accounts, Some(70541)).map(|a| a.account_id), Some(70541));
    }

    #[test]
    fn sign_in_times_sort_in_time_order() {
        assert!(login_time_key("2026-10-06T20:25:43.2Z") < login_time_key("2026-10-06T20:25:43.209Z"));
        assert!(login_time_key("2026-10-06T20:25:43Z") < login_time_key("2026-10-06T20:25:43.001Z"));
        assert!(login_time_key("2026-10-05T23:59:59.999Z") < login_time_key("2026-10-06T00:00:00Z"));
        assert!(login_time_key("") < login_time_key("2020-01-01T00:00:00Z"));
    }

    #[test]
    fn jwt_expiry_reads_exp() {
        // {"exp":1790873724} base64url, no padding
        let payload = base64url_encode(br#"{"exp":1790873724}"#);
        let token = format!("hdr.{payload}.sig");
        assert_eq!(jwt_expiry(&token), 1790873724);
        assert_eq!(jwt_expiry("not-a-jwt"), 0);
    }

    fn base64url_encode(bytes: &[u8]) -> String {
        const C: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            let take = chunk.len() + 1;
            for i in 0..take {
                out.push(C[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            }
        }
        out
    }
}



