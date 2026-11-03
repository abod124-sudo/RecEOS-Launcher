//! The launcher's log, kept on disk.
//!
//! Every log line is written by the page (app.js), so the log used to live
//! only in the webview: close the launcher and it was gone, together with
//! whatever explained why it had to be closed. The page now hands its lines to
//! [`log_append`] as well, and each run of the launcher gets a file of its own
//! in `%LOCALAPPDATA%\com.receos.launcher\logs`:
//!
//! ```text
//! launcher.log     this session
//! launcher.1.log   the one before — what a bug report offers to attach
//! launcher.2.log
//! launcher.3.log   the oldest kept
//! ```
//!
//! A session that closes normally ends with [`END_MARKER`]; one that panicked
//! carries a `PANIC` line. Neither means the process was killed, the power
//! went, or Windows shut down under it, which is exactly the session a report
//! most wants — so a missing marker is reported as "didn't close normally".

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// How many earlier sessions are kept beside the current one.
const KEEP_OLD: usize = 3;

/// The most one session writes. A launcher left in the tray for weeks logs
/// little (the pollers are silent), so this is a guard against a loop logging
/// the same failure forever, not a size anyone is expected to reach.
const MAX_SESSION_BYTES: u64 = 8 * 1024 * 1024;

/// The longest single line kept. A line can quote a whole server reply.
const MAX_LINE_BYTES: usize = 16 * 1024;

/// The most lines one [`log_append`] call takes; the page sends a few at a
/// time, every second or so.
const MAX_LINES_PER_CALL: usize = 500;

/// The last line of a session that closed normally.
pub const END_MARKER: &str = "===== Session ended =====";

/// The most of an earlier session handed back to the page or attached to a
/// report: its newest part, the part that explains how it ended.
pub const MAX_READ_BYTES: usize = 2 * 1024 * 1024;

struct Sink {
    dir: PathBuf,
    file: Option<File>,
    written: u64,
    full: bool,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// A poisoned lock only means a writer panicked mid-line; the file is still
/// fine to append to, and the panic hook below must be able to use it.
fn sink() -> MutexGuard<'static, Option<Sink>> {
    SINK.lock().unwrap_or_else(|e| e.into_inner())
}

fn session_path(dir: &Path, age: usize) -> PathBuf {
    if age == 0 {
        dir.join("launcher.log")
    } else {
        dir.join(format!("launcher.{age}.log"))
    }
}

/// Start this session's file, moving the earlier ones down a place.
///
/// Called from `setup`, never earlier: a second copy of the launcher exits in
/// the single-instance plugin before `setup` runs, and must not rotate the
/// file the running one is writing.
pub fn init(dir: PathBuf) {
    let _ = fs::create_dir_all(&dir);
    rotate(&dir);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(session_path(&dir, 0))
        .ok();
    *sink() = Some(Sink { dir, file, written: 0, full: false });
    install_panic_hook();
}

/// launcher.log → .1 → .2 → .3, the oldest dropped. Done from the oldest
/// down, so no rename lands on a file that is still there (which Windows
/// refuses).
fn rotate(dir: &Path) {
    let _ = fs::remove_file(session_path(dir, KEEP_OLD));
    for age in (0..KEEP_OLD).rev() {
        let _ = fs::rename(session_path(dir, age), session_path(dir, age + 1));
    }
}

/// A line as it is written: no control characters (a stray escape or NUL in
/// a server reply would garble the file in a text editor), no line breaks of
/// its own, and no longer than [`MAX_LINE_BYTES`].
fn clean_line(line: &str) -> String {
    let mut out: String = line
        .chars()
        .map(|c| if c == '\t' || !c.is_control() { c } else { ' ' })
        .collect();
    if out.len() > MAX_LINE_BYTES {
        let mut cut = MAX_LINE_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str(" …");
    }
    out
}

fn write_lines(sink: &mut Sink, lines: impl IntoIterator<Item = String>) {
    if sink.full {
        return;
    }
    let Some(file) = sink.file.as_mut() else { return };
    let mut buf = String::new();
    for line in lines {
        buf.push_str(&line);
        buf.push_str("\r\n");
    }
    if sink.written + buf.len() as u64 > MAX_SESSION_BYTES {
        sink.full = true;
        buf = "[log stopped: this session's file is full]\r\n".to_string();
    }
    if file.write_all(buf.as_bytes()).is_ok() {
        sink.written += buf.len() as u64;
    }
}

/// Append lines to this session's file. A message the page wrapped onto
/// several lines arrives with `\n`s; each becomes a line of its own.
pub fn append(lines: &[String]) {
    let mut guard = sink();
    let Some(sink) = guard.as_mut() else { return };
    let split = lines
        .iter()
        .take(MAX_LINES_PER_CALL)
        .flat_map(|l| l.split('\n'))
        .map(clean_line);
    write_lines(sink, split.collect::<Vec<_>>());
}

/// Mark this session as having closed normally. Called on `RunEvent::Exit`.
pub fn end_session() {
    let mut guard = sink();
    if let Some(sink) = guard.as_mut() {
        // Past the size cap too: the marker is what tells the next session
        // this one wasn't cut off.
        sink.full = false;
        write_lines(sink, [END_MARKER.to_string()]);
        if let Some(f) = sink.file.as_mut() {
            let _ = f.flush();
        }
        sink.file = None;
    }
}

/// A Rust panic is written to the log before the process goes down
/// (release builds abort on panic, so nothing after the hook runs).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // try_lock: a panic inside `append` would otherwise deadlock here.
        if let Ok(mut guard) = SINK.try_lock() {
            if let Some(sink) = guard.as_mut() {
                let lines = format!("[--:--:--] PANIC  LAUNCHER  {info}");
                write_lines(sink, lines.split('\n').map(clean_line).collect::<Vec<_>>());
                if let Some(f) = sink.file.as_mut() {
                    let _ = f.flush();
                }
            }
        }
        previous(info);
    }));
}

/// The folder the session files are in, once [`init`] has run.
pub fn dir() -> Option<PathBuf> {
    sink().as_ref().map(|s| s.dir.clone())
}

/// The last `max` bytes of `text`, cut at a line start.
pub fn tail(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    tail.find('\n').map(|i| &tail[i + 1..]).unwrap_or(tail)
}

/// How a session ended, read from its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Ending {
    /// Closed normally: the file ends with [`END_MARKER`].
    Closed,
    /// A panic was logged.
    Crashed,
    /// Neither: killed, frozen and ended from Task Manager, power lost, or
    /// Windows shut down under it.
    Unexpected,
}

impl Ending {
    pub fn describe(self) -> &'static str {
        match self {
            Ending::Closed => "closed normally",
            Ending::Crashed => "crashed",
            Ending::Unexpected => "didn't close normally",
        }
    }
}

pub fn ending_of(text: &str) -> Ending {
    if text.lines().any(|l| l.contains("] PANIC ")) {
        Ending::Crashed
    } else if text.lines().rev().find(|l| !l.trim().is_empty()) == Some(END_MARKER) {
        Ending::Closed
    } else {
        Ending::Unexpected
    }
}

/// The session before this one: its newest [`MAX_READ_BYTES`] and how it
/// ended. `None` when there is none yet, or it logged nothing.
pub fn previous_session() -> Option<(String, Ending)> {
    let path = session_path(&dir()?, 1);
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    if text.trim().is_empty() {
        return None;
    }
    let ending = ending_of(&text);
    Some((tail(&text, MAX_READ_BYTES).to_string(), ending))
}

// ─── Lines from the backend ─────────────────────────────────────────────────
//
// The page owns the log, so a line the backend wants to add goes to it as a
// `backend-log` event and is logged there like any other (shown on the Logs
// page and written to this file). Lines from before the page is listening
// wait in `pending` until it asks for them with [`log_backend_ready`].

// Tests have no page to send it to (see `send_to_page`).
#[cfg_attr(test, allow(dead_code))]
const BACKEND_EVENT: &str = "backend-log";

/// The most lines kept for a page that hasn't started listening yet.
const MAX_PENDING: usize = 200;

/// A line for the page's `addLog(msg, level, source)`.
#[derive(Clone, serde::Serialize)]
pub struct BackendLine {
    level: &'static str,
    source: &'static str,
    msg: String,
}

struct Backend {
    app: Option<tauri::AppHandle>,
    ready: bool,
    pending: Vec<BackendLine>,
    /// When each [`backend_once`] key was last logged.
    seen: std::collections::BTreeMap<String, std::time::Instant>,
}

static BACKEND: Mutex<Backend> = Mutex::new(Backend {
    app: None,
    ready: false,
    pending: Vec::new(),
    seen: std::collections::BTreeMap::new(),
});

fn backend_state() -> MutexGuard<'static, Backend> {
    BACKEND.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where backend lines go. Called from `setup`.
pub fn set_app(app: tauri::AppHandle) {
    backend_state().app = Some(app);
}

/// Log a line from the backend. `level` and `source` are keys of the page's
/// LOG_LEVELS and LOG_SOURCES ("warn", "server", …).
pub fn backend(level: &'static str, source: &'static str, msg: impl Into<String>) {
    let line = BackendLine { level, source, msg: msg.into() };
    {
        let mut state = backend_state();
        if !state.ready {
            if state.pending.len() >= MAX_PENDING {
                state.pending.remove(0);
            }
            state.pending.push(line);
            return;
        }
    }
    send_to_page(line);
}

#[cfg(not(test))]
fn send_to_page(line: BackendLine) {
    use tauri::Emitter;
    let app = backend_state().app.clone();
    if let Some(app) = app {
        let _ = app.emit(BACKEND_EVENT, line);
    }
}

/// Tests have no page. Linking Tauri's event code into the test exe would
/// also stop it starting on Windows (see `stella_api::window_on_screen`).
#[cfg(test)]
fn send_to_page(_line: BackendLine) {}

/// [`backend`], unless the same `key` was logged less than `quiet` ago: for a
/// failure that repeats on every poll or reconnect, which would otherwise
/// bury the lines around it.
pub fn backend_once(
    key: &str,
    quiet: std::time::Duration,
    level: &'static str,
    source: &'static str,
    msg: impl Into<String>,
) {
    {
        let mut state = backend_state();
        let now = std::time::Instant::now();
        if state.seen.get(key).is_some_and(|at| now.duration_since(*at) < quiet) {
            return;
        }
        if state.seen.len() > 200 {
            state.seen.clear();
        }
        state.seen.insert(key.to_string(), now);
    }
    backend(level, source, msg);
}

/// Up to `max` characters of a server's reply, on one line, for quoting in a
/// log line.
pub fn snippet(body: &[u8], max: usize) -> String {
    let text = String::from_utf8_lossy(body);
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > max {
        format!("{}…", one_line.chars().take(max).collect::<String>())
    } else {
        one_line
    }
}

// ─── Commands ───────────────────────────────────────────────────────────────

/// The page is listening for backend lines: hand it the ones that came
/// before, and send the rest as events from now on.
#[tauri::command]
pub fn log_backend_ready() -> Vec<BackendLine> {
    let mut state = backend_state();
    state.ready = true;
    std::mem::take(&mut state.pending)
}

#[tauri::command]
pub async fn log_append(lines: Vec<String>) {
    let _ = tokio::task::spawn_blocking(move || append(&lines)).await;
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviousLog {
    text: String,
    ending: Ending,
}

/// The previous session's log, for the Logs page's "Last session" view.
#[tauri::command]
pub async fn log_previous() -> Option<PreviousLog> {
    tokio::task::spawn_blocking(previous_session)
        .await
        .ok()
        .flatten()
        .map(|(text, ending)| PreviousLog { text, ending })
}

#[tauri::command]
pub async fn log_open_folder() -> bool {
    match dir() {
        Some(dir) if dir.is_dir() => {
            crate::download::reveal_folder(&dir);
            true
        }
        _ => false,
    }
}

/// A file name the page suggests for Save, if it is a plain one.
fn safe_file_name(name: &str) -> Option<&str> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name.ends_with(".txt")
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ok.then_some(name)
}

/// Ask where to save `text` and write it there. `Ok(None)` when the person
/// cancels the dialog.
#[tauri::command]
pub async fn log_save(text: String, file_name: String) -> Result<Option<String>, String> {
    let name = safe_file_name(&file_name).unwrap_or("radium-launcher-log.txt").to_string();
    tokio::task::spawn_blocking(move || {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Save Log")
            .set_file_name(&name)
            .add_filter("Text file", &["txt"])
            .save_file()
        else {
            return Ok(None);
        };
        // CRLF, so the file reads properly in any Windows editor.
        let body = text.replace("\r\n", "\n").replace('\n', "\r\n");
        fs::write(&path, body).map_err(|e| format!("Couldn't save the log: {e}"))?;
        Ok(Some(path.to_string_lossy().to_string()))
    })
    .await
    .map_err(|e| format!("Save failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_three_old_sessions_and_drops_the_oldest() {
        let dir = std::env::temp_dir().join(format!("radium-applog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for age in 0..=KEEP_OLD {
            fs::write(session_path(&dir, age), format!("age {age}")).unwrap();
        }
        rotate(&dir);
        assert!(!session_path(&dir, 0).exists(), "the current file moved to .1");
        for age in 1..=KEEP_OLD {
            let text = fs::read_to_string(session_path(&dir, age)).unwrap();
            assert_eq!(text, format!("age {}", age - 1));
        }
        // Rotating with gaps (a first run, or a deleted file) is fine too.
        fs::remove_file(session_path(&dir, 2)).unwrap();
        rotate(&dir);
        assert!(session_path(&dir, 2).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_lose_control_characters_and_overlong_tails() {
        assert_eq!(clean_line("a\x1b[31mb\0c\td"), "a [31mb c\td");
        let long = "é".repeat(MAX_LINE_BYTES);
        let cut = clean_line(&long);
        assert!(cut.len() <= MAX_LINE_BYTES + " …".len());
        assert!(cut.ends_with(" …"));
    }

    #[test]
    fn how_a_session_ended_is_read_from_its_last_lines() {
        let closed = format!("[10:00:00] INFO   LAUNCHER  started\r\n{END_MARKER}\r\n");
        assert_eq!(ending_of(&closed), Ending::Closed);
        assert_eq!(ending_of("[10:00:00] INFO   LAUNCHER  started\r\n"), Ending::Unexpected);
        let crashed = format!("x\n[--:--:--] PANIC  LAUNCHER  boom\n{END_MARKER}\n");
        assert_eq!(ending_of(&crashed), Ending::Crashed);
    }

    #[test]
    fn a_long_log_keeps_its_newest_whole_lines() {
        assert_eq!(tail("short", 100), "short");
        let log = "old line one\nold line two\nnewest line\n";
        // Cut inside "old line two": that partial line goes too.
        assert_eq!(tail(log, 20), "newest line\n");
        // A cut that would split a multi-byte character moves past it.
        assert_eq!(tail("ééééé\nlast", 6), "last");
    }

    #[test]
    fn only_plain_txt_names_are_suggested() {
        assert_eq!(safe_file_name("radium-log-2026-10-03.txt"), Some("radium-log-2026-10-03.txt"));
        assert_eq!(safe_file_name(r"..\..\x.txt"), None);
        assert_eq!(safe_file_name("x.exe"), None);
        assert_eq!(safe_file_name(".txt"), None);
        assert_eq!(safe_file_name(""), None);
    }

    #[test]
    fn a_quoted_reply_is_one_short_line() {
        assert_eq!(snippet(b"<html>\r\n  <h1>404</h1>\n</html>", 100), "<html> <h1>404</h1> </html>");
        assert_eq!(snippet("ééééé".as_bytes(), 3), "ééé…");
    }

    #[test]
    fn backend_lines_wait_for_the_page_and_repeats_are_left_out() {
        let quiet = std::time::Duration::from_secs(60);
        backend_once("test-repeat", quiet, "warn", "server", "first");
        backend_once("test-repeat", quiet, "warn", "server", "again");
        backend_once("test-other", quiet, "warn", "server", "other");
        let lines: Vec<String> = log_backend_ready().into_iter().map(|l| l.msg).collect();
        assert!(lines.contains(&"first".to_string()));
        assert!(lines.contains(&"other".to_string()));
        assert!(!lines.contains(&"again".to_string()));
    }
}
