//! Bug reports, filed from the Report a Problem dialog on the Logs page.
//!
//! The dialog's review step shows exactly what is sent, so both it
//! ([`bug_report_preview`]) and the send ([`submit_bug_report`]) build their
//! facts with the one [`Facts::gather`]: what the person reads is what goes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::{applog, config, download};

/// Description length limits, in characters. The dialog enforces the same.
const MIN_DESCRIPTION: usize = 10;
const MAX_DESCRIPTION: usize = 1500;

/// Where reports are sent: the relay Worker in relay/, never Discord itself.
const REPORT_URL: &str = "https://receos-bug-relay.receos-api.workers.dev/report";

/// Seconds between two reports.
const COOLDOWN_SECS: u64 = 60;

static LAST_SUBMISSION_TIME: AtomicU64 = AtomicU64::new(0);

/// The longest a quoted log line may be in the embed. Discord refuses a whole
/// message whose field runs past 1024 characters; this leaves room for the
/// code fence around it.
const MAX_LINKED_LINE: usize = 900;

/// How many of the newest log lines the review step shows.
const PREVIEW_LINES: usize = 300;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Seconds left before another report may be sent.
fn cooldown_left() -> u64 {
    (LAST_SUBMISSION_TIME.load(Ordering::SeqCst) + COOLDOWN_SECS).saturating_sub(now_secs())
}

/// Human-readable reachability for a tri-state ping result. `None` means the
/// frontend hadn't polled yet, which must not be reported as offline.
fn online_label(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "online",
        Some(false) => "OFFLINE",
        None => "not checked",
    }
}

/// The installed client's build health. Mirrors `check_install`'s rule: a
/// Radium client whose recorded build id differs from the one this launcher
/// requires is outdated and must be re-downloaded. Vanilla installs come from
/// a user-supplied zip, and Stella's feed has no version, so neither has a
/// build to track and neither is called outdated.
fn client_status_label(network: config::Network, is_installed: bool, client_build: &str) -> &'static str {
    if !is_installed {
        "Not installed"
    } else if network != config::Network::Radium {
        "Installed (no build tracking)"
    } else if client_build != download::REQUIRED_CLIENT_BUILD {
        "OUTDATED — re-download required"
    } else {
        "Up to date"
    }
}

/// "Windows (64-bit)" and the like.
fn system_label() -> String {
    let os = match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "64-bit",
        "x86" => "32-bit",
        "aarch64" => "ARM64",
        other => other,
    };
    format!("{os} ({arch})")
}

/// A short label from the page that is safe to put in the embed as-is.
fn is_plain_label(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= 48
        && s.chars().all(|c| c.is_alphanumeric() || " &()-.'+".contains(c))
}

fn network_label(network: config::Network) -> &'static str {
    match network {
        config::Network::Radium => "Radium",
        config::Network::Vanilla => "Vanilla",
        config::Network::Stella => "Stella",
        config::Network::RecEos => "Rec EOS",
    }
}

/// The kinds of problem the dialog offers, by id. Anything else is "other":
/// the id lands in the embed and in the message that pings the channel, where
/// an `@here` or an overlong string would ride along.
fn category_name(id: &str) -> &'static str {
    match id {
        "launch" => "Game won't launch",
        "download" => "Download or install",
        "account" => "Sign-in or account",
        "looks" => "Looks wrong",
        "launcher" => "Launcher problem",
        _ => "Something else",
    }
}

/// Severity: its label and the embed's colour.
fn severity(id: &str) -> (&'static str, u32) {
    match id {
        "critical" => ("Critical — crashes or freezes", 0xE5484D),
        "high" => ("High — can't play", 0xF76B15),
        "low" => ("Low — minor", 0x30A46C),
        _ => ("Medium — something's broken", 0xFFC53D),
    }
}

/// The signed-in Windows user's profile folder (`C:\Users\<name>`), if known.
fn user_profile_dir() -> Option<String> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|p| p.trim_end_matches(['\\', '/']).to_string())
        // Too short to be a profile path; replacing it would mangle the text.
        .filter(|p| p.len() >= 4)
}

/// `text` with the user's profile folder replaced by `%USERPROFILE%`.
///
/// Reports go to a Discord channel, and the log lines and install path they
/// carry spell out `C:\Users\<name>\...` — the person's Windows account name,
/// which a report needs no more than it needs their password. Matched
/// case-insensitively (Windows paths are) and in the three spellings that
/// reach the log: backslashes, forward slashes and JSON-escaped backslashes.
fn redact_profile_path(text: &str, profile: &str) -> String {
    let mut out = text.to_string();
    for needle in [
        profile.to_string(),
        profile.replace('\\', "/"),
        profile.replace('\\', "\\\\"),
    ] {
        out = replace_path_ignore_ascii_case(&out, &needle, "%USERPROFILE%");
    }
    out
}

/// [`redact_profile_path`] with this machine's profile folder.
fn redact(text: &str) -> String {
    match user_profile_dir() {
        Some(p) => redact_profile_path(text, &p),
        None => text.to_string(),
    }
}

/// Replace every ASCII-case-insensitive occurrence of the path `needle` in
/// `haystack` that ends where a path component ends — so `C:\Users\Jane` is
/// not matched inside `C:\Users\Janet`.
///
/// Compared byte by byte, but only ever cut at a match's first and last byte:
/// non-ASCII bytes must match exactly, so a match begins and ends on the same
/// character boundaries it has in `needle`, and slicing there cannot panic.
fn replace_path_ignore_ascii_case(haystack: &str, needle: &str, with: &str) -> String {
    let (hay, pat) = (haystack.as_bytes(), needle.as_bytes());
    if pat.is_empty() || pat.len() > hay.len() {
        return haystack.to_string();
    }
    let ends_component = |at: usize| {
        hay.get(at)
            // A '.' ends it: after a profile path that is far more often the
            // end of a log sentence than the rest of a longer account name.
            .map(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_') || *b >= 0x80))
            .unwrap_or(true)
    };
    let mut out = String::with_capacity(haystack.len());
    let (mut last, mut i) = (0, 0);
    while i + pat.len() <= hay.len() {
        if hay[i..i + pat.len()].eq_ignore_ascii_case(pat) && ends_component(i + pat.len()) {
            out.push_str(&haystack[last..i]);
            out.push_str(with);
            i += pat.len();
            last = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&haystack[last..]);
    out
}

/// The last `n` lines of `text`.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// `text` cut to `max` characters, with an ellipsis if anything was cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// A short code naming one report, in the Discord post and in the sender's
/// own log, so a post can be matched to its session. Not a secret and not unique by
/// construction; it only has to tell one day's reports apart.
fn reference() -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
    let mut n = nanos ^ (std::process::id() as u64).rotate_left(32);
    let mut code = String::from("RL-");
    for _ in 0..5 {
        code.push(ALPHABET[(n % 32) as usize] as char);
        n /= 32;
    }
    code
}

/// Everything a report says about the launcher, besides what the person
/// wrote. Client build, version and options are read from config rather than
/// trusted from the page, and for the network the report was filed from.
struct Facts {
    launcher_version: String,
    network: config::Network,
    os: String,
    installed: bool,
    running: bool,
    download_state: &'static str,
    play_mode: &'static str,
    error_count: u64,
    api: Option<bool>,
    cdn: Option<bool>,
    client_version: String,
    client_build: String,
    client_status: &'static str,
    install_dir: String,
    theme: String,
    av_excluded: bool,
    minimize_on_launch: bool,
    close_on_launch: bool,
}

/// A label and value, for the review step's list.
#[derive(Serialize)]
pub struct Fact {
    label: &'static str,
    value: String,
}

impl Facts {
    fn gather(app: &tauri::AppHandle, diagnostics: &Value) -> Facts {
        let cfg = config::current(app);
        let flag = |k: &str| -> bool { diagnostics.get(k).and_then(Value::as_bool).unwrap_or(false) };
        let network = cfg.network();
        let installed = flag("isInstalled");
        let client_build = cfg.client_build_for(network);
        let install_dir = cfg.install_dir_for(network);
        Facts {
            // This build's own version, not the page's word for it.
            launcher_version: format!("v{}", app.package_info().version),
            network,
            os: system_label(),
            installed,
            running: flag("isGameRunning"),
            // Only the phases there are: this lands in the embed as-is.
            download_state: match diagnostics.get("downloadState").and_then(Value::as_str) {
                Some("downloading") => "Downloading",
                Some("paused") => "Paused",
                Some("cancelling") => "Cancelling",
                _ => "None",
            },
            // The two modes there are. config.json is hand-editable, and a
            // long value would push an embed field past Discord's limit.
            play_mode: if cfg.play_mode == "vr" { "VR" } else { "Screen" },
            error_count: diagnostics.get("errorCount").and_then(Value::as_u64).unwrap_or(0),
            api: diagnostics.get("apiOnline").and_then(Value::as_bool),
            cdn: diagnostics.get("cdnOnline").and_then(Value::as_bool),
            client_version: match cfg.client_version_for(network) {
                "" => String::new(),
                v => format!("v{v}"),
            },
            client_build: match client_build {
                "" => "unrecorded".to_string(),
                b => b.to_string(),
            },
            client_status: client_status_label(network, installed, client_build),
            install_dir: if install_dir.is_empty() {
                "Default".to_string()
            } else {
                redact(install_dir)
            },
            theme: {
                // The skin's name as the Settings menu shows it, when the page
                // sends a plausible one; the config id otherwise.
                let skin = diagnostics
                    .get("themeName")
                    .and_then(Value::as_str)
                    .filter(|n| is_plain_label(n))
                    .map(str::to_string)
                    .unwrap_or_else(|| cfg.theme.clone());
                if cfg.glass.enabled { format!("Liquid Glass (over {skin})") } else { skin }
            },
            av_excluded: cfg.defender_excluded_for(network),
            minimize_on_launch: cfg.minimize_on_launch,
            close_on_launch: cfg.close_on_launch,
        }
    }

    fn yes_no(b: bool) -> &'static str {
        if b { "Yes" } else { "No" }
    }

    fn client_line(&self) -> String {
        if self.installed {
            let version = if self.client_version.is_empty() { String::new() } else { format!("{} · ", self.client_version) };
            format!("{version}build {} · {}", self.client_build, self.client_status)
        } else {
            self.client_status.to_string()
        }
    }

    fn servers_line(&self) -> String {
        format!("API {} · CDN {}", online_label(self.api), online_label(self.cdn))
    }

    /// The list the review step shows.
    fn rows(&self) -> Vec<Fact> {
        let row = |label, value: String| Fact { label, value };
        vec![
            row("Launcher", self.launcher_version.clone()),
            row("Network", network_label(self.network).to_string()),
            row("System", self.os.clone()),
            row("Game client", self.client_line()),
            row("Install folder", self.install_dir.clone()),
            row("Game running", Self::yes_no(self.running).to_string()),
            row("Download", self.download_state.to_string()),
            row("Play mode", self.play_mode.to_string()),
            row("Servers", self.servers_line()),
            row("Errors logged", self.error_count.to_string()),
            row("Theme", self.theme.clone()),
            row("Antivirus exclusion", if self.av_excluded { "Added" } else { "Not added" }.to_string()),
        ]
    }

    /// The block at the top of the attached log, so the file stands alone
    /// when it is read outside Discord.
    fn log_header(&self, reference: &str, category: &str, severity: &str) -> String {
        format!(
            "===== REC EOS LAUNCHER — BUG REPORT {reference} =====\r\n\
             Problem  : {category} ({severity})\r\n\
             Launcher : {} on {}\r\n\
             Network  : {}\r\n\
             Client   : {}\r\n\
             Install  : {}\r\n\
             Runtime  : running={} download={} mode={}\r\n\
             Servers  : {}\r\n\
             Errors   : {} logged this session\r\n\
             Theme    : {}\r\n\
             ==========================================================\r\n\r\n",
            self.launcher_version,
            self.os,
            network_label(self.network),
            self.client_line(),
            self.install_dir,
            Self::yes_no(self.running),
            self.download_state,
            self.play_mode,
            self.servers_line(),
            self.error_count,
            self.theme,
        )
    }
}

// ─── Review ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviousPreview {
    lines: usize,
    ending: applog::Ending,
    ending_text: &'static str,
    preview: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    facts: Vec<Fact>,
    /// This session's log, newest lines, redacted as it would be sent.
    log_preview: String,
    previous: Option<PreviousPreview>,
    cooldown: u64,
}

/// What the review step shows: every fact the report carries, and the logs it
/// can attach, redacted exactly as they would be sent.
#[tauri::command]
pub async fn bug_report_preview(app: tauri::AppHandle, diagnostics: Value, logs: String) -> Preview {
    let facts = Facts::gather(&app, &diagnostics).rows();
    tokio::task::spawn_blocking(move || {
        let previous = applog::previous_session().map(|(text, ending)| PreviousPreview {
            lines: text.lines().filter(|l| !l.trim().is_empty()).count(),
            ending,
            ending_text: ending.describe(),
            preview: redact(&last_lines(&text, PREVIEW_LINES)),
        });
        Preview {
            facts,
            log_preview: redact(&last_lines(&logs, PREVIEW_LINES)),
            previous,
            cooldown: cooldown_left(),
        }
    })
    .await
    .unwrap_or_else(|_| Preview { facts: Vec::new(), log_preview: String::new(), previous: None, cooldown: 0 })
}

// ─── Send ───────────────────────────────────────────────────────────────────

/// What the dialog sends.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    description: String,
    category: String,
    severity: String,
    /// This session's log, as the Logs page holds it. Empty to leave it out.
    logs: String,
    attach_log: bool,
    attach_previous: bool,
    /// The log line the report was started from ("Report this"), if any.
    linked_line: Option<String>,
    diagnostics: Value,
}

#[derive(Serialize)]
pub struct Sent {
    reference: String,
}

#[tauri::command]
pub async fn submit_bug_report(app: tauri::AppHandle, report: Report) -> Result<Sent, String> {
    let len = report.description.trim().chars().count();
    if len < MIN_DESCRIPTION {
        return Err(format!("Describe the problem in at least {MIN_DESCRIPTION} characters."));
    }
    if len > MAX_DESCRIPTION {
        return Err(format!("Keep the description under {MAX_DESCRIPTION} characters."));
    }

    // The slot is claimed before the send rather than stamped after it: two
    // clicks landing together each read the old time, both passed, and both
    // reports went out. A send that fails gives the slot back.
    let now = now_secs();
    let last = LAST_SUBMISSION_TIME.load(Ordering::SeqCst);
    if now < last + COOLDOWN_SECS {
        return Err(format!("Please wait {} seconds before sending another report.", last + COOLDOWN_SECS - now));
    }
    if LAST_SUBMISSION_TIME
        .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("Another report is being sent. Please wait a minute.".into());
    }
    let result = send(app, report).await;
    if result.is_err() {
        let _ = LAST_SUBMISSION_TIME.compare_exchange(now, last, Ordering::SeqCst, Ordering::SeqCst);
    }
    result
}

/// Seconds before another report may be sent, for a dialog opened during the
/// cooldown.
#[tauri::command]
pub fn bug_report_cooldown() -> u64 {
    cooldown_left()
}

/// Windows line endings throughout, so an attachment opens cleanly in any
/// Windows editor.
fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Text that goes into a code block in the embed: no backtick may close the
/// fence early.
fn fence_safe(text: &str) -> String {
    text.replace('`', "ˋ")
}

async fn send(app: tauri::AppHandle, report: Report) -> Result<Sent, String> {
    let facts = Facts::gather(&app, &report.diagnostics);
    let reference = reference();
    let category = category_name(report.category.to_lowercase().as_str());
    let (severity_name, color) = severity(report.severity.to_lowercase().as_str());

    // No pings from the description, and no Windows account name.
    let description = redact(
        &report
            .description
            .trim()
            .replace("@everyone", "`@everyone`")
            .replace("@here", "`@here`"),
    );

    // Attachments: this session's log (with the facts on top), and the one
    // before it when asked for. Read here rather than from the page, so a
    // report can't attach a file the launcher didn't write.
    let mut files: Vec<(&str, String)> = Vec::new();
    let mut attached: Vec<String> = Vec::new();
    if report.attach_log {
        let lines = report.logs.lines().filter(|l| !l.trim().is_empty()).count();
        let body = if report.logs.trim().is_empty() {
            "(no log lines this session)\r\n".to_string()
        } else {
            applog::tail(&report.logs, applog::MAX_READ_BYTES).to_string()
        };
        files.push(("logs.txt", crlf(&redact(&(facts.log_header(&reference, category, severity_name) + &body)))));
        attached.push(format!("logs.txt — this session, {lines} lines"));
    }
    if report.attach_previous {
        let previous = tokio::task::spawn_blocking(applog::previous_session).await.ok().flatten();
        if let Some((text, ending)) = previous {
            attached.push(format!("previous-session.txt — {}", ending.describe()));
            files.push(("previous-session.txt", crlf(&redact(&text))));
        }
    }

    let mut fields = vec![
        serde_json::json!({ "name": "Severity", "value": severity_name, "inline": true }),
        serde_json::json!({ "name": "Network", "value": network_label(facts.network), "inline": true }),
        serde_json::json!({ "name": "Launcher", "value": format!("{} · {}", facts.launcher_version, facts.os), "inline": true }),
        serde_json::json!({
            "name": "Game",
            "value": format!(
                "Client: {}\nRunning: {} · Download: {} · Mode: {}",
                facts.client_line(), Facts::yes_no(facts.running), facts.download_state, facts.play_mode
            ),
            "inline": false
        }),
        serde_json::json!({ "name": "Servers", "value": facts.servers_line(), "inline": true }),
        serde_json::json!({ "name": "Errors logged", "value": facts.error_count.to_string(), "inline": true }),
        serde_json::json!({ "name": "Theme", "value": clip(&facts.theme, 200), "inline": true }),
        serde_json::json!({
            "name": "Options",
            "value": format!(
                "AV exclusion: {} · Minimize on launch: {} · Close on launch: {} · Install folder: {}",
                if facts.av_excluded { "added" } else { "not added" },
                Facts::yes_no(facts.minimize_on_launch),
                Facts::yes_no(facts.close_on_launch),
                if facts.install_dir == "Default" { "default" } else { "custom" },
            ),
            "inline": false
        }),
    ];
    if let Some(line) = report.linked_line.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
        fields.push(serde_json::json!({
            "name": "Reported from this log line",
            "value": format!("```\n{}\n```", fence_safe(&clip(&redact(line), MAX_LINKED_LINE))),
            "inline": false
        }));
    }
    fields.push(serde_json::json!({
        "name": "Attached",
        "value": if attached.is_empty() { "Nothing — the logs were left out".to_string() } else { attached.join("\n") },
        "inline": false
    }));

    let payload = serde_json::json!({
        // The relay adds the @everyone ping itself.
        "content": format!("New bug report {reference} [{severity_name}]"),
        "embeds": [{
            "title": format!("{category} · {reference}"),
            "description": description,
            "color": color,
            "fields": fields,
            "footer": { "text": format!("Rec EOS Launcher {}", facts.launcher_version) }
        }]
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| format!("Couldn't start the upload: {e}"))?;

    // Reports go through our relay (the relay/ folder), which holds the
    // Discord webhook as a secret. Never put a webhook address in the app:
    // anyone can read it out of the .exe.
    #[allow(unused_mut)]
    let mut url = REPORT_URL;

    // Development builds can send somewhere harmless instead, to test the
    // dialog end to end without posting to the real channel.
    #[cfg(debug_assertions)]
    let dev_url = std::env::var("RADIUM_REPORT_URL").ok();
    #[cfg(debug_assertions)]
    if let Some(u) = dev_url.as_deref() {
        url = u;
    }

    let payload = serde_json::to_string(&payload).map_err(|e| format!("Couldn't build the report: {e}"))?;
    let mut form = reqwest::multipart::Form::new().part(
        "payload_json",
        reqwest::multipart::Part::text(payload)
            .mime_str("application/json")
            .map_err(|e| e.to_string())?,
    );
    for (i, (name, body)) in files.into_iter().enumerate() {
        form = form.part(
            format!("files[{i}]"),
            reqwest::multipart::Part::text(body)
                .file_name(name)
                .mime_str("text/plain")
                .map_err(|e| e.to_string())?,
        );
    }

    let response = client
        .post(url)
        .multipart(form)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "The report server took too long to answer. Check your connection and try again.".to_string()
            } else {
                "Couldn't reach the report server. Check your connection and try again.".to_string()
            }
        })?;
    let status = response.status();
    if status.as_u16() == 429 {
        return Err("The report server is busy. Try again in a minute.".into());
    }
    if !status.is_success() {
        return Err(format!("The report server refused the report (HTTP {}).", status.as_u16()));
    }
    Ok(Sent { reference })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_path_is_redacted_in_every_spelling() {
        let profile = r"C:\Users\Jane Doe";
        let text = concat!(
            r"Install dir: C:\Users\Jane Doe\AppData\Roaming\com.radium.launcher\client",
            "\n",
            r"Exe: c:\users\jane doe\x\RecRoom.exe | C:/Users/Jane Doe/y | C:\\Users\\Jane Doe\\z",
        );
        let out = redact_profile_path(text, profile);
        assert!(!out.to_lowercase().contains("jane"), "{out}");
        assert!(out.contains(r"%USERPROFILE%\AppData\Roaming"));
        assert!(out.contains("%USERPROFILE%/y"));
        assert!(out.contains(r"%USERPROFILE%\\z"));
    }

    #[test]
    fn redaction_leaves_other_text_and_multibyte_characters_alone() {
        let profile = r"C:\Users\Zoë";
        let text = r"é C:\Users\Zoë\x — C:\Users\Zoey stays, D:\Users\Zoë stays";
        let out = redact_profile_path(text, profile);
        assert_eq!(out, r"é %USERPROFILE%\x — C:\Users\Zoey stays, D:\Users\Zoë stays");
        assert_eq!(replace_path_ignore_ascii_case("abc", "", "x"), "abc");
        assert_eq!(replace_path_ignore_ascii_case("ab", "abc", "x"), "ab");
    }

    #[test]
    fn another_account_sharing_the_name_prefix_is_not_touched() {
        let out = redact_profile_path(r"C:\Users\Janet\x and C:\Users\Jane.", r"C:\Users\Jane");
        assert_eq!(out, r"C:\Users\Janet\x and %USERPROFILE%.");
    }

    #[test]
    fn online_label_is_tri_state() {
        assert_eq!(online_label(Some(true)), "online");
        assert_eq!(online_label(Some(false)), "OFFLINE");
        // Not-yet-polled must never read as offline.
        assert_eq!(online_label(None), "not checked");
    }

    #[test]
    fn client_status_reflects_build_health() {
        use config::Network::{Radium, Vanilla};
        assert_eq!(client_status_label(Radium, false, ""), "Not installed");
        assert_eq!(client_status_label(Radium, false, download::REQUIRED_CLIENT_BUILD), "Not installed");
        // Installed but with a stale/blank build id → flagged outdated.
        assert_eq!(client_status_label(Radium, true, ""), "OUTDATED — re-download required");
        assert_eq!(client_status_label(Radium, true, "recroom-baby-2015"), "OUTDATED — re-download required");
        assert_eq!(client_status_label(Radium, true, download::REQUIRED_CLIENT_BUILD), "Up to date");
        // Vanilla has no build to compare, as in `check_install`.
        assert_eq!(client_status_label(Vanilla, true, ""), "Installed (no build tracking)");
        assert_eq!(client_status_label(Vanilla, false, ""), "Not installed");
    }

    #[test]
    fn unknown_categories_and_severities_fall_back_to_fixed_labels() {
        assert_eq!(category_name("launch"), "Game won't launch");
        assert_eq!(category_name("@everyone"), "Something else");
        assert_eq!(severity("critical").0, "Critical — crashes or freezes");
        assert_eq!(severity("x".repeat(5000).as_str()).0, "Medium — something's broken");
    }

    #[test]
    fn references_are_short_and_unambiguous() {
        let r = reference();
        assert_eq!(r.len(), 8);
        assert!(r.starts_with("RL-"));
        // No 0/O or 1/I to misread.
        assert!(r[3..].chars().all(|c| !"01OI".contains(c)), "{r}");
    }

    #[test]
    fn only_plain_labels_from_the_page_reach_the_embed() {
        assert!(is_plain_label("Black & White (Inverted)"));
        assert!(is_plain_label("Windows XP"));
        assert!(!is_plain_label("@everyone"));
        assert!(!is_plain_label(""));
        assert!(!is_plain_label(&"x".repeat(49)));
        assert!(system_label().contains('('));
    }

    #[test]
    fn quoted_lines_cannot_close_their_code_block() {
        assert_eq!(fence_safe("a ``` b"), "a ˋˋˋ b");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(last_lines("1\n2\n3\n4", 2), "3\n4");
        assert_eq!(last_lines("1", 5), "1");
        assert_eq!(crlf("a\nb\r\nc"), "a\r\nb\r\nc");
    }
}
