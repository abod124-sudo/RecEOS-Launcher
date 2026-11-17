//! Rec EOS's live channel for the launcher: one WebSocket to the server's
//! `GET /launcher/live` (the Rec EOS repo's server/src/social/live.ts), held
//! while someone is signed in. The server sends `{"t":"sync","why":...}` when
//! something changed for this account (a friend request, a message, a chat, a
//! friend coming online or changing rooms), and the page refetches only then,
//! instead of asking for the friends list every few seconds.
//!
//! The page hears it as the `receos-live` event:
//! - `{ kind: "connected" }`: (re)connected; anything missed while down is
//!   fetched once.
//! - `{ kind: "sync", why }`: something changed.
//! - `{ kind: "down" }`: not connected; the page goes back to its own timers.
//!
//! The socket sends "ping" every 40 s (Cloudflare drops a WebSocket quiet for
//! about 100 s; the server answers "pong") and reconnects with a growing,
//! jittered wait when it drops.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, protocol::frame::coding::CloseCode, Message};

use crate::receos::USER_AGENT;
use crate::{applog, receos_social};

const EVENT: &str = "receos-live";
const PING_EVERY: Duration = Duration::from_secs(40);
/// No answer to anything (not even our pings' "pong") for this long: the
/// connection is dead even if the OS hasn't noticed.
const SILENT_FOR: Duration = Duration::from_secs(100);
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);
/// A connection that lasted this long worked: the next wait starts small
/// again. One dropped sooner (a proxy closing it at once) keeps backing off,
/// or every reconnect's catch-up fetch would come round every 2 s.
const STABLE_FOR: Duration = Duration::from_secs(60);
/// Replaced by the account's newer launchers (the server keeps 4 per account),
/// or a server without the channel yet: try again much later.
const RETRY_REPLACED: Duration = Duration::from_secs(10 * 60);
const RETRY_MISSING: Duration = Duration::from_secs(30 * 60);

/// Bumped by every start and stop: a connection from an older one ends.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static RUNNING: Mutex<Option<u64>> = Mutex::new(None);
static STOP: Notify = Notify::const_new();

/// Holds the channel open while signed in. Calling it again while it runs does nothing.
#[tauri::command]
pub fn receos_live_start(app: AppHandle) {
    start(&app);
}

pub fn start(app: &AppHandle) {
    if !receos_social::signed_in(app) {
        return;
    }
    let gen = {
        let Ok(mut running) = RUNNING.lock() else { return };
        if running.is_some() {
            return;
        }
        let gen = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        *running = Some(gen);
        gen
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        run(&app, gen).await;
        // Turned down (not stopped by a sign-out or a newer start, which say so themselves).
        if current(gen) {
            let _ = app.emit(EVENT, json!({ "kind": "down" }));
        }
        if let Ok(mut running) = RUNNING.lock() {
            if *running == Some(gen) {
                *running = None;
            }
        }
    });
}

/// Signed out: the channel closes now.
pub fn stop() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut running) = RUNNING.lock() {
        *running = None;
    }
    STOP.notify_waiters();
}

fn current(gen: u64) -> bool {
    GENERATION.load(Ordering::SeqCst) == gen
}

/// A wait that ends early when stopped. False when stopped.
async fn pause(gen: u64, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => current(gen),
        _ = STOP.notified() => false,
    }
}

/// Up to a quarter more, so a server restart isn't met by every launcher at once.
fn jitter(d: Duration) -> Duration {
    let mut b = [0u8; 2];
    let _ = getrandom::getrandom(&mut b);
    let quarter = d.as_millis() as u64 / 4;
    d + Duration::from_millis(if quarter == 0 { 0 } else { u16::from_le_bytes(b) as u64 % quarter })
}

/// How a connection ended, and so how long to wait before the next.
enum Ended {
    /// Was open this long, then dropped.
    Dropped(Duration),
    /// Couldn't connect: wait longer each time.
    Failed(String),
    /// The server doesn't want this one now.
    Later(Duration, String),
    /// Signed out, or the token was turned down.
    Stop,
}

async fn run(app: &AppHandle, gen: u64) {
    let mut wait = RETRY_FIRST;
    let mut failures = 0u32;
    while current(gen) {
        let ended = connect(app, gen).await;
        if !current(gen) {
            return;
        }
        let _ = app.emit(EVENT, json!({ "kind": "down" }));
        let delay = match ended {
            Ended::Stop => return,
            Ended::Dropped(lasted) if lasted >= STABLE_FOR => {
                failures = 0;
                wait = RETRY_FIRST;
                RETRY_FIRST
            }
            Ended::Dropped(_) => {
                let d = wait;
                wait = (wait * 2).min(RETRY_MAX);
                d
            }
            Ended::Failed(why) => {
                failures += 1;
                if failures == 1 || failures % 10 == 0 {
                    applog::backend("warn", "server", format!("Rec EOS live updates: {why} (tried {failures} times; checking every few minutes meanwhile)."));
                }
                let d = wait;
                wait = (wait * 2).min(RETRY_MAX);
                d
            }
            Ended::Later(d, why) => {
                applog::backend("info", "server", format!("Rec EOS live updates paused: {why}."));
                d
            }
        };
        if !pause(gen, jitter(delay)).await {
            return;
        }
    }
}

/// `https://…` → `wss://…/launcher/live` (`http://127.0.0.1:…` in a debug build: `ws://`).
fn live_url() -> String {
    let root = receos_social::api_root();
    let root = if let Some(rest) = root.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = root.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        root
    };
    format!("{root}/launcher/live")
}

async fn connect(app: &AppHandle, gen: u64) -> Ended {
    let Some(token) = receos_social::session_token(app) else { return Ended::Stop };
    let url = live_url();
    let mut req = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => return Ended::Later(RETRY_MISSING, format!("bad address {url}: {e}")),
    };
    let headers = req.headers_mut();
    if let Ok(v) = USER_AGENT.parse() {
        headers.insert("User-Agent", v);
    }
    match format!("Bearer {}", token.as_str()).parse() {
        Ok(v) => headers.insert("Authorization", v),
        Err(_) => return Ended::Stop,
    };
    drop(token);
    let connecting = tokio_tungstenite::connect_async(req);
    let result = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(20), connecting) => r,
        _ = STOP.notified() => return Ended::Stop,
    };
    let ws = match result {
        Err(_) => return Ended::Failed("timed out connecting".into()),
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(r))) => {
            return match r.status().as_u16() {
                // The token is no longer good; the next API call signs out.
                401 => Ended::Stop,
                404 => Ended::Later(RETRY_MISSING, "the server has no live channel".into()),
                s => Ended::Failed(format!("HTTP {s}")),
            };
        }
        Ok(Err(e)) => return Ended::Failed(e.to_string()),
        Ok(Ok((ws, _))) => ws,
    };
    if !current(gen) {
        return Ended::Stop;
    }
    let _ = app.emit(EVENT, json!({ "kind": "connected" }));
    let (mut tx, mut rx) = ws.split();
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let opened = Instant::now();
    let mut heard = opened;
    loop {
        tokio::select! {
            _ = STOP.notified() => {
                let _ = tx.send(Message::Close(None)).await;
                return Ended::Stop;
            }
            _ = ping.tick() => {
                if !current(gen) {
                    let _ = tx.send(Message::Close(None)).await;
                    return Ended::Stop;
                }
                if heard.elapsed() > SILENT_FOR {
                    return Ended::Dropped(opened.elapsed());
                }
                if tx.send(Message::Text("ping".into())).await.is_err() {
                    return Ended::Dropped(opened.elapsed());
                }
            }
            msg = rx.next() => {
                heard = Instant::now();
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if !current(gen) {
                            return Ended::Stop;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(&text) {
                            if v["t"] == "sync" {
                                let why = v["why"].as_str().unwrap_or("").chars().take(40).collect::<String>();
                                let _ = app.emit(EVENT, json!({ "kind": "sync", "why": why }));
                            }
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        return match frame.map(|f| f.code) {
                            Some(CloseCode::Policy) => Ended::Later(RETRY_REPLACED, "newer launchers on this account hold the connection".into()),
                            Some(CloseCode::Again) => Ended::Failed("the server is busy".into()),
                            _ => Ended::Dropped(opened.elapsed()),
                        };
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => return Ended::Dropped(opened.elapsed()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_adds_at_most_a_quarter() {
        for _ in 0..100 {
            let d = jitter(Duration::from_secs(8));
            assert!(d >= Duration::from_secs(8) && d < Duration::from_secs(10));
        }
    }
}
