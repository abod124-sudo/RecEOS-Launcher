//! Signing in to Rec EOS, and what that opens: friends and where they are,
//! friend requests, game invites, chats, and cheering or favoriting rooms.
//!
//! It all goes through the Rec EOS server's launcher API (`/launcher/*`, the
//! Rec EOS repo's server/src/launcher.ts). The player signs in with their Rec
//! EOS username and password (the one Discord's /password sets); the password
//! goes to the server once and is never kept. The token the server answers
//! with is kept encrypted for this Windows user (DPAPI, as the Vanilla session
//! is), and is all later calls send.
//!
//! The launcher only reads presence; it never reports any, so being signed in
//! here does not show the player as online.
//!
//! Answers are reshaped into what the page already draws for Stella (friends,
//! notifications) so the same cards, bell and pop-ups serve both.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use zeroize::Zeroizing;

use crate::receos::USER_AGENT;
use crate::receos_api::image_url;
use crate::vanilla_auth::store;

const TOKEN_FILE: &str = "receos-session.bin";

/// The answer while nobody is signed in. The page matches it.
pub const SIGNED_OUT: &str = "Sign in to Rec EOS.";

static TOKEN: Mutex<Option<Zeroizing<String>>> = Mutex::new(None);
static LOADED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The server. In a debug build `RECEOS_API_ROOT` may point at a local test
/// server (http://127.0.0.1:...); a release build always uses the real one.
fn api_root() -> String {
    #[cfg(debug_assertions)]
    if let Ok(root) = std::env::var("RECEOS_API_ROOT") {
        let root = root.trim_end_matches('/').to_string();
        if root.starts_with("http://127.0.0.1:") || root.starts_with("https://") {
            return root;
        }
    }
    crate::receos::API_ROOT.to_string()
}

fn http() -> reqwest::Client {
    let b = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20));
    #[cfg(not(debug_assertions))]
    let b = b.https_only(true);
    b.build().unwrap_or_default()
}

fn token_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_local_data_dir().ok().map(|d| d.join(TOKEN_FILE))
}

fn token(app: &AppHandle) -> Option<Zeroizing<String>> {
    if !LOADED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        let saved = token_path(app).and_then(|p| store::load(&p));
        if let Ok(mut t) = TOKEN.lock() {
            *t = saved.filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }
    TOKEN.lock().ok().and_then(|t| t.clone())
}

fn set_token(app: &AppHandle, value: Option<&str>) {
    if let Ok(mut t) = TOKEN.lock() {
        *t = value.map(|v| Zeroizing::new(v.to_string()));
    }
    LOADED.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(path) = token_path(app) {
        match value {
            Some(v) => {
                let _ = store::save(&path, v.as_bytes());
            }
            None => store::delete(&path),
        }
    }
}

/// Whether someone is signed in (as far as the launcher knows).
pub fn signed_in(app: &AppHandle) -> bool {
    token(app).is_some()
}

/// A call to the launcher API. A 401 means the token is no longer good (a
/// logout elsewhere, a password change): it is forgotten.
async fn call(app: &AppHandle, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    let Some(tok) = token(app) else { return Err(SIGNED_OUT.into()) };
    let mut req = http()
        .request(method, format!("{}{}", api_root(), path))
        .header("Authorization", format!("Bearer {}", tok.as_str()));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await.map_err(|e| format!("Couldn't reach Rec EOS: {e}"))?;
    let status = res.status();
    let data: Value = res.json().await.unwrap_or(Value::Null);
    if status == reqwest::StatusCode::UNAUTHORIZED {
        set_token(app, None);
        return Err(SIGNED_OUT.into());
    }
    if !status.is_success() {
        return Err(data["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("Rec EOS answered {status}.")));
    }
    Ok(data)
}

fn player_json(a: &Value) -> Value {
    json!({
        "id": a["accountId"].as_i64(),
        "userName": a["username"].as_str().unwrap_or(""),
        "displayName": a["displayName"].as_str().filter(|s| !s.is_empty()).or(a["username"].as_str()).unwrap_or(""),
        "AvatarUrl": image_url(a["profileImage"].as_str().unwrap_or("")),
        "level": a["level"].as_i64(),
    })
}

// ─── Sign-in ────────────────────────────────────────────────────────────────

/// `{ authenticated, player }`. Asks the server, so a token it no longer takes
/// is noticed here.
#[tauri::command]
pub async fn receos_auth_status(app: AppHandle) -> Value {
    if !signed_in(&app) {
        return json!({ "authenticated": false });
    }
    match call(&app, reqwest::Method::GET, "/launcher/me", None).await {
        Ok(me) => json!({ "authenticated": true, "player": player_json(&me["account"]), "roles": me["roles"] }),
        Err(e) if e == SIGNED_OUT => json!({ "authenticated": false, "reason": "expired" }),
        // Unreachable: still signed in, as far as anyone knows.
        Err(e) => json!({ "authenticated": false, "hasSession": true, "error": e }),
    }
}

#[tauri::command]
pub async fn receos_login(app: AppHandle, username: String, password: String) -> Result<Value, String> {
    let password = Zeroizing::new(password);
    let username = username.trim().trim_start_matches('@').to_string();
    if username.is_empty() || password.is_empty() {
        return Err("Enter your Rec EOS username and password.".into());
    }
    let res = http()
        .post(format!("{}/launcher/login", api_root()))
        .json(&json!({ "username": username, "password": password.as_str() }))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Rec EOS: {e}"))?;
    let status = res.status();
    let data: Value = res.json().await.unwrap_or(Value::Null);
    // A server without the launcher API answers like any unknown game call:
    // the game's own sign-in check ("unauthorized"), or not found.
    if status == reqwest::StatusCode::NOT_FOUND || data["error"].as_str() == Some("unauthorized") {
        return Err("Rec EOS's server doesn't take launcher sign-ins yet. Try again after its next update.".into());
    }
    let Some(tok) = data["token"].as_str().filter(|t| t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())) else {
        return Err(data["error"].as_str().unwrap_or("Couldn't sign in.").to_string());
    };
    set_token(&app, Some(tok));
    crate::applog::backend("info", "Rec EOS", format!("Signed in as @{}.", data["account"]["username"].as_str().unwrap_or("")));
    Ok(json!({ "player": player_json(&data["account"]), "roles": data["roles"] }))
}

#[tauri::command]
pub async fn receos_logout(app: AppHandle) -> Value {
    let _ = call(&app, reqwest::Method::POST, "/launcher/logout", Some(json!({}))).await;
    set_token(&app, None);
    json!({ "success": true })
}

// ─── Friends ────────────────────────────────────────────────────────────────

/// The friends list in `stella_friends`' shape: `{ success, loaded, connected,
/// paused, friends, requests }`, each friend `{ id, userName, displayName,
/// AvatarUrl, status, roomId, roomName, private, favorite }`. Online first,
/// then favorites, then by name.
#[tauri::command]
pub async fn receos_friends(app: AppHandle) -> Value {
    let data = match call(&app, reqwest::Method::GET, "/launcher/friends", None).await {
        Ok(d) => d,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    let rows = data["friends"].as_array().cloned().unwrap_or_default();
    let mut friends = Vec::new();
    let mut requests = Vec::new();
    for r in &rows {
        let mut row = player_json(r);
        match r["relationship"].as_i64() {
            Some(3) => {
                let online = r["online"].as_bool() == Some(true);
                row["status"] = json!(if online { "online" } else { "offline" });
                row["roomId"] = json!(r["room"]["roomId"].as_i64().unwrap_or(0));
                row["roomName"] = json!(r["room"]["name"].as_str().unwrap_or(""));
                row["private"] = json!(r["room"]["isPrivate"].as_bool() == Some(true));
                row["favorite"] = json!(r["favorited"].as_bool() == Some(true));
                friends.push(row);
            }
            Some(2) => requests.push(row),
            _ => {}
        }
    }
    let name = |v: &Value| v["displayName"].as_str().unwrap_or("").to_lowercase();
    friends.sort_by(|a, b| {
        (a["status"] != "online")
            .cmp(&(b["status"] != "online"))
            .then_with(|| b["favorite"].as_bool().cmp(&a["favorite"].as_bool()))
            .then_with(|| name(a).cmp(&name(b)))
    });
    requests.sort_by_key(|r| name(r));
    json!({ "success": true, "loaded": true, "connected": true, "paused": false, "friends": friends, "requests": requests })
}

async fn relationship(app: &AppHandle, action: &str, id: i64) -> Result<(), String> {
    if id <= 0 {
        return Err("Which player?".into());
    }
    call(app, reqwest::Method::POST, &format!("/launcher/relationships/{action}?id={id}"), Some(json!({}))).await.map(|_| ())
}

#[tauri::command]
pub async fn receos_set_friend_favorite(app: AppHandle, id: i64, on: bool) -> Result<(), String> {
    relationship(&app, if on { "favorite" } else { "unfavorite" }, id).await
}

#[tauri::command]
pub async fn receos_accept_friend_request(app: AppHandle, id: i64) -> Result<(), String> {
    relationship(&app, "acceptfriendrequest", id).await
}

#[tauri::command]
pub async fn receos_send_friend_request(app: AppHandle, id: i64) -> Result<(), String> {
    relationship(&app, "sendfriendrequest", id).await
}

#[tauri::command]
pub async fn receos_remove_friend(app: AppHandle, id: i64) -> Result<(), String> {
    relationship(&app, "removefriend", id).await
}

// ─── Notifications ──────────────────────────────────────────────────────────

/// The game's message types the bell shows: Rec Room's own numbers.
const INVITE: i64 = 6;

fn notification_row(m: &Value, players: &Value) -> Value {
    let kind = m["type"].as_i64().unwrap_or(-1);
    let from = m["fromId"].as_i64().filter(|&id| id > 0);
    let sender = from.and_then(|id| players.get(id.to_string()));
    let data = m["data"].as_str().unwrap_or("");
    let message = match kind {
        INVITE => serde_json::from_str::<Value>(data)
            .ok()
            .and_then(|d| d.get("name").or_else(|| d.get("Name")).and_then(Value::as_str).map(str::to_string))
            .map(|n| n.trim_start_matches(['^', '@']).chars().take(80).collect::<String>()),
        _ => None,
    };
    let sent = m["sentAt"].as_str().unwrap_or("");
    json!({
        "id": format!("{}-{sent}", m["id"]),
        "messageId": m["id"].as_i64(),
        "type": kind,
        "senderId": from,
        "senderName": sender.map(|p| p["username"].as_str().unwrap_or("")),
        "senderDisplay": sender.and_then(|p| p["displayName"].as_str()).filter(|s| !s.is_empty()),
        "senderAvatar": sender.map(|p| image_url(p["profileImage"].as_str().unwrap_or(""))),
        "roomId": m["roomId"].as_i64(),
        "message": message,
        "sentTime": sent,
    })
}

/// The bell's list: the game's inbox (invites, friend requests and answers),
/// newest first, in the shape `stella_notifications` returns.
#[tauri::command]
pub async fn receos_notifications(app: AppHandle) -> Result<Value, String> {
    let data = call(&app, reqwest::Method::GET, "/launcher/updates?afterMessageId=0&afterChatMessageId=0", None).await?;
    let players = &data["players"];
    let mut out: Vec<Value> = data["messages"].as_array().cloned().unwrap_or_default().iter().map(|m| notification_row(m, players)).collect();
    out.sort_by(|a, b| b["sentTime"].as_str().unwrap_or("").cmp(a["sentTime"].as_str().unwrap_or("")));
    Ok(Value::Array(out))
}

/// What arrived since the cursors: `{ notifications, chat, messageCursor,
/// chatCursor }`. Without cursors only the cursors come back, so the first
/// look after starting doesn't pop up old things.
#[tauri::command]
pub async fn receos_updates(app: AppHandle, after_message_id: Option<i64>, after_chat_message_id: Option<i64>) -> Result<Value, String> {
    let mut path = "/launcher/updates".to_string();
    let mut sep = '?';
    if let Some(m) = after_message_id {
        path.push_str(&format!("{sep}afterMessageId={m}"));
        sep = '&';
    }
    if let Some(c) = after_chat_message_id {
        path.push_str(&format!("{sep}afterChatMessageId={c}"));
    }
    let data = call(&app, reqwest::Method::GET, &path, None).await?;
    let players = data["players"].clone();
    let notifications: Vec<Value> = data["messages"].as_array().cloned().unwrap_or_default().iter().map(|m| notification_row(m, &players)).collect();
    let chat: Vec<Value> = data["chat"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|c| chat_message(c, &players))
        .collect();
    Ok(json!({
        "notifications": notifications,
        "chat": chat,
        "messageCursor": data["messageCursor"],
        "chatCursor": data["chatCursor"],
    }))
}

// ─── Chats ──────────────────────────────────────────────────────────────────

/// The text of a chat message: the game sends `{Type, Version, Data}` and a
/// text message is Type 0. Other kinds (a photo, a room link) get a stand-in.
fn chat_text(contents: &str) -> (String, bool) {
    match serde_json::from_str::<Value>(contents) {
        Ok(v) if v["Type"].as_i64() == Some(0) => (v["Data"].as_str().unwrap_or("").to_string(), true),
        Ok(v) if v.is_object() => ("[Sent something you can see in the game]".to_string(), false),
        _ => (contents.to_string(), true),
    }
}

fn chat_message(c: &Value, players: &Value) -> Value {
    let (text, is_text) = chat_text(c["contents"].as_str().unwrap_or(""));
    let sender = c["senderPlayerId"].as_i64();
    let who = sender.and_then(|id| players.get(id.to_string()));
    json!({
        "id": c["chatMessageId"].as_i64(),
        "threadId": c["chatThreadId"].as_i64(),
        "senderId": sender,
        "senderName": who.map(|p| p["username"].as_str().unwrap_or("")),
        "senderDisplay": who.and_then(|p| p["displayName"].as_str()),
        "text": text.chars().take(4000).collect::<String>(),
        "isText": is_text,
        "sentTime": c["timeSent"].as_str().unwrap_or(""),
    })
}

/// Names and pictures for these players, by id.
async fn players(app: &AppHandle, ids: &[i64]) -> Value {
    let mut ids: Vec<i64> = ids.iter().copied().filter(|&i| i > 0).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return json!({});
    }
    let q: Vec<String> = ids.iter().take(100).map(|i| format!("id={i}")).collect();
    let data = call(app, reqwest::Method::GET, &format!("/launcher/players?{}", q.join("&")), None).await.unwrap_or(Value::Null);
    let mut out = serde_json::Map::new();
    for p in data["players"].as_array().cloned().unwrap_or_default() {
        if let Some(id) = p["accountId"].as_i64() {
            out.insert(id.to_string(), p);
        }
    }
    Value::Object(out)
}

fn thread_json(t: &Value, me: Option<i64>, players: &Value) -> Value {
    let members: Vec<i64> = t["playerIds"].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    let others: Vec<Value> = members
        .iter()
        .filter(|&&id| Some(id) != me)
        .filter_map(|id| players.get(id.to_string()))
        .map(player_json)
        .collect();
    let latest = t.get("latestMessage").filter(|v| v.is_object()).or_else(|| t["messages"].as_array().and_then(|m| m.first()));
    let latest = latest.map(|m| chat_message(m, players));
    let last_read = t["lastReadMessageId"].as_i64().unwrap_or(0);
    let unread = latest.as_ref().is_some_and(|m| m["id"].as_i64().unwrap_or(0) > last_read && m["senderId"].as_i64() != me);
    json!({
        "id": t["chatThreadId"].as_i64(),
        "name": t["chatThreadName"].as_str().unwrap_or(""),
        "members": others,
        "latest": latest,
        "lastRead": last_read,
        "unread": unread,
    })
}

async fn my_id(app: &AppHandle) -> Option<i64> {
    call(app, reqwest::Method::GET, "/launcher/me", None).await.ok().and_then(|m| m["account"]["accountId"].as_i64())
}

/// The player's chats, newest first: `[{ id, name, members, latest, unread }]`.
#[tauri::command]
pub async fn receos_chat_threads(app: AppHandle) -> Result<Value, String> {
    let threads = call(&app, reqwest::Method::GET, "/launcher/chat/threads?max=50&withPlayers=1", None).await?;
    // Servers with withPlayers answer { me, threads, players } in one request; older ones a bare list, and then
    // the names and our own id are two more requests.
    if let Some(list) = threads["threads"].as_array() {
        let me = threads["me"].as_i64();
        let people = &threads["players"];
        return Ok(Value::Array(list.iter().map(|t| thread_json(t, me, people)).collect()));
    }
    let list = threads.as_array().cloned().unwrap_or_default();
    let mut ids: Vec<i64> = list
        .iter()
        .flat_map(|t| t["playerIds"].as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_i64())
        .collect();
    let me = my_id(&app).await;
    ids.retain(|&i| Some(i) != me);
    let people = players(&app, &ids).await;
    Ok(Value::Array(list.iter().map(|t| thread_json(t, me, &people)).collect()))
}

/// A page of one chat's messages, oldest first. `before`: the oldest one shown.
#[tauri::command]
pub async fn receos_chat_messages(app: AppHandle, thread_id: i64, before: Option<i64>) -> Result<Value, String> {
    let mut path = format!("/launcher/chat/threads/{thread_id}/messages?count=50");
    if let Some(b) = before.filter(|&b| b > 0) {
        path.push_str(&format!("&mode=0&ref={b}"));
    }
    let msgs = call(&app, reqwest::Method::GET, &path, None).await?;
    let list = msgs.as_array().cloned().unwrap_or_default();
    let ids: Vec<i64> = list.iter().filter_map(|m| m["senderPlayerId"].as_i64()).collect();
    let people = players(&app, &ids).await;
    Ok(Value::Array(list.iter().map(|m| chat_message(m, &people)).collect()))
}

fn text_contents(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Type a message first.".into());
    }
    if text.chars().count() > 1000 {
        return Err("That message is too long.".into());
    }
    Ok(json!({ "Type": 0, "Version": 1, "Data": text }).to_string())
}

#[tauri::command]
pub async fn receos_chat_send(app: AppHandle, thread_id: i64, text: String) -> Result<Value, String> {
    let contents = text_contents(&text)?;
    let sent = call(&app, reqwest::Method::POST, &format!("/launcher/chat/threads/{thread_id}/messages"), Some(json!({ "contents": contents }))).await?;
    Ok(chat_message(&sent["chatMessage"], &json!({})))
}

/// The chat with exactly these players (made if there is none), with an
/// optional first message. Answers the thread as `receos_chat_threads` does.
#[tauri::command]
pub async fn receos_chat_open(app: AppHandle, player_ids: Vec<i64>, text: Option<String>) -> Result<Value, String> {
    let mut body = json!({ "ids": player_ids, "messageCount": 1 });
    if let Some(t) = text.filter(|t| !t.trim().is_empty()) {
        body["contents"] = json!(text_contents(&t)?);
    }
    let data = call(&app, reqwest::Method::POST, "/launcher/chat/withmembers", Some(body)).await?;
    let me = my_id(&app).await;
    let ids: Vec<i64> = data["thread"]["playerIds"].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    let people = players(&app, &ids).await;
    Ok(thread_json(&data["thread"], me, &people))
}

#[tauri::command]
pub async fn receos_chat_read(app: AppHandle, thread_id: i64, message_id: i64) -> Result<(), String> {
    call(&app, reqwest::Method::POST, &format!("/launcher/chat/threads/{thread_id}/read"), Some(json!({ "messageId": message_id }))).await.map(|_| ())
}

// ─── Rooms ──────────────────────────────────────────────────────────────────

/// `{ Cheered, Favorited }` for a room, as `stella_room_interaction` answers.
#[tauri::command]
pub async fn receos_room_interaction(app: AppHandle, room_id: i64) -> Result<Value, String> {
    call(&app, reqwest::Method::GET, &format!("/launcher/rooms/{room_id}/interaction"), None).await
}

#[tauri::command]
pub async fn receos_set_room_interaction(app: AppHandle, room_id: i64, cheer: Option<bool>, favorite: Option<bool>) -> Result<(), String> {
    let mut body = json!({});
    if let Some(c) = cheer {
        body["cheer"] = json!(c);
    }
    if let Some(f) = favorite {
        body["favorite"] = json!(f);
    }
    call(&app, reqwest::Method::PUT, &format!("/launcher/rooms/{room_id}/interaction"), Some(body)).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_text_reads_the_games_shape() {
        assert_eq!(chat_text(r#"{"Type":0,"Version":1,"Data":"hi"}"#), ("hi".to_string(), true));
        assert!(!chat_text(r#"{"Type":3,"Data":"x"}"#).1);
        assert_eq!(chat_text("plain").0, "plain");
    }

    #[test]
    fn a_text_message_is_wrapped_for_the_game() {
        let c = text_contents("  hello ").unwrap();
        let v: Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["Type"], 0);
        assert_eq!(v["Data"], "hello");
        assert!(text_contents("   ").is_err());
        assert!(text_contents(&"x".repeat(1001)).is_err());
    }

    #[test]
    fn an_invite_names_its_room() {
        let players = json!({ "9002": { "username": "Bob", "displayName": "Bob D", "profileImage": "" } });
        let n = notification_row(&json!({ "id": 5, "fromId": 9002, "type": 6, "data": "{\"inviteId\":1,\"name\":\"^RecCenter\"}", "roomId": 3, "sentAt": "t" }), &players);
        assert_eq!(n["message"], "RecCenter");
        assert_eq!(n["senderName"], "Bob");
        assert_eq!(n["messageId"], 5);
    }

    #[test]
    fn unread_is_only_someone_elses_newer_message() {
        let t = json!({ "chatThreadId": 1, "playerIds": [1, 2], "lastReadMessageId": 4,
            "latestMessage": { "chatMessageId": 5, "senderPlayerId": 2, "contents": "{\"Type\":0,\"Data\":\"x\"}" } });
        assert_eq!(thread_json(&t, Some(1), &json!({}))["unread"], true);
        assert_eq!(thread_json(&t, Some(2), &json!({}))["unread"], false);
    }
}
