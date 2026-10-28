//! Vanilla's game files, downloaded and updated by Rec EOS.
//!
//! Vanilla's PC build only runs when Vanilla's own launcher starts it (its
//! anti-cheat), but the files themselves are public: a manifest at
//! `cdn.vanillarec.net/AppStorage/1/manifest.json` lists every file with its
//! size and SHA-256, and each one is served at `…/AppStorage/1/files/<path>`.
//! Rec EOS downloads them into the folder Vanilla's launcher installs to
//! (`<installDirectory>\Vanilla`), checking every file against the manifest.
//!
//! Rec EOS never writes the launcher's own records (the one exception is
//! Uninstall, which removes the game's entry exactly as the launcher's own
//! Uninstall does; see [`uninstall`]). Once every file is in
//! place, it presses the launcher's Download / Update / Resume: the launcher
//! re-checks the files, finds them all present (it reuses files already on
//! disk — tested), downloads nothing, and writes `.launchermanifest.json` and
//! its `installedVersions` entry itself. So the launcher's integrity check
//! only ever reads what the launcher wrote. See [`sign_off`].
//!
//! Rules this keeps:
//! * Manifest and files come only from `https://cdn.vanillarec.net`, with no
//!   redirects.
//! * Manifest paths are untrusted: anything absolute, with `..`, a drive or
//!   a `:` is refused, and every path must stay inside the game folder.
//! * A file is written to a temporary name, checked against its SHA-256, and
//!   only then renamed into place; game files are never altered.
//! * On an update, only files the previous manifest listed and the new one
//!   doesn't are deleted — nothing else.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};

const CDN_HOST: &str = "cdn.vanillarec.net";
const MANIFEST_URL: &str = "https://cdn.vanillarec.net/AppStorage/1/manifest.json";
const FILES_BASE: &str = "https://cdn.vanillarec.net/AppStorage/1/files/";
/// Vanilla's id in the launcher's `installedVersions`.
const APP_ID: &str = "1";
/// The launcher's own record of what it installed, beside the game.
const LAUNCHER_MANIFEST: &str = ".launchermanifest.json";
/// Suffix of Rec EOS's in-progress files (never a name the game uses).
const TMP_SUFFIX: &str = ".receos-tmp";
/// The manifest is ~650 KB today; anything wildly larger isn't one.
const MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;
/// The whole game is ~7.7 GB; refuse a manifest claiming far more.
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Files downloaded at once.
const CONCURRENCY: usize = 8;
/// Attempts per file before the download stops with an error.
const ATTEMPTS: usize = 3;
/// Left free on the drive beyond what the download needs.
const SPACE_MARGIN: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct ManifestFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    pub version: String,
    pub files: Vec<ManifestFile>,
}

/// Parse a manifest, refusing anything malformed rather than skipping it.
pub(crate) fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let v: Value = serde_json::from_str(text).map_err(|_| "Vanilla's file list isn't valid JSON.".to_string())?;
    let version = v
        .get("version")
        .or_else(|| v.get("Version"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("Vanilla's file list has no version.")?
        .to_string();
    let arr = v.get("files").and_then(Value::as_array).ok_or("Vanilla's file list has no files.")?;
    let mut files = Vec::with_capacity(arr.len());
    let mut total: u64 = 0;
    for f in arr {
        let path = f.get("path").and_then(Value::as_str).ok_or("A file in Vanilla's list has no path.")?;
        let sha = f.get("sha256").and_then(Value::as_str).ok_or("A file in Vanilla's list has no hash.")?;
        let size = f.get("size").and_then(Value::as_u64).ok_or("A file in Vanilla's list has no size.")?;
        if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("Vanilla's list has a bad hash for {}.", path));
        }
        if safe_relative_path(path).is_none() {
            return Err(format!("Vanilla's list has an unsafe file path: {}", path));
        }
        total = total.saturating_add(size);
        files.push(ManifestFile { path: path.to_string(), sha256: sha.to_ascii_lowercase(), size });
    }
    if files.is_empty() {
        return Err("Vanilla's file list is empty.".into());
    }
    if total > MAX_TOTAL_BYTES {
        return Err("Vanilla's file list claims an impossible size.".into());
    }
    Ok(Manifest { version, files })
}

/// A manifest path as a relative path that can only point inside the game
/// folder: `/`-separated, no empty / `.` / `..` parts, no drive, no `:`, no `\`.
/// Also refused, because Windows wouldn't write where the path says: a part
/// ending in `.` or space (Windows drops it, so `a.` is `a`), and the reserved
/// device names (`CON`, `NUL`, `COM1`…, with or without an extension).
pub(crate) fn safe_relative_path(p: &str) -> Option<PathBuf> {
    if p.is_empty() || p.starts_with('/') || p.contains('\\') || p.contains(':') || p.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for seg in p.split('/') {
        if seg.is_empty() || seg.ends_with('.') || seg.ends_with(' ') || is_reserved_name(seg) {
            return None;
        }
        if seg.chars().any(|c| (c as u32) < 32 || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')) {
            return None;
        }
        out.push(seg);
    }
    // Belt and braces: the result must be plain, relative components.
    if !out.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(out)
}

/// Windows' reserved device names, which open a device instead of a file.
fn is_reserved_name(seg: &str) -> bool {
    let stem = seg.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0')
}

/// `FILES_BASE` + the path with each segment percent-encoded.
fn file_url(path: &str) -> String {
    let mut url = String::from(FILES_BASE);
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            url.push('/');
        }
        for b in seg.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                url.push(b as char);
            } else {
                url.push_str(&format!("%{:02X}", b));
            }
        }
    }
    url
}

/// Vanilla's entry in the launcher's `installedVersions`, if it has one. The
/// launcher sometimes records it as `""` (seen when it signs off files that
/// were already on disk), so presence is what counts, not the value.
fn launcher_entry() -> Option<String> {
    let path = PathBuf::from(std::env::var_os("APPDATA")?).join("VanillaLauncher").join("settings.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("installedVersions")?.get(APP_ID)?.as_str().map(str::to_string)
}

/// The installed game version: the launcher's recorded one, else the version
/// in the manifest the launcher wrote beside the game.
pub(crate) fn installed_version() -> Option<String> {
    let entry = launcher_entry()?;
    if !entry.trim().is_empty() {
        return Some(entry);
    }
    let text = std::fs::read_to_string(game_dir()?.join(LAUNCHER_MANIFEST)).ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .get("version")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// Where the game goes: Vanilla's launcher's game folder.
pub(crate) fn game_dir() -> Option<PathBuf> {
    crate::game::vanilla_launcher_game_dir().map(PathBuf::from)
}

/// The game counts as installed once Vanilla's launcher has an entry for it
/// and its exe is there.
pub(crate) fn game_installed() -> bool {
    launcher_entry().is_some() && game_dir().is_some_and(|d| d.join("RecRoom.exe").is_file())
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(crate::server::USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .build()
        .map_err(|e| format!("Couldn't start the download: {}", e))
}

async fn fetch_manifest(client: &reqwest::Client) -> Result<Manifest, String> {
    let resp = client
        .get(MANIFEST_URL)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Vanilla's file server: {}", e))?;
    if resp.url().host_str() != Some(CDN_HOST) {
        return Err("Vanilla's file server answered from somewhere unexpected.".into());
    }
    if !resp.status().is_success() {
        return Err(format!("Vanilla's file server answered {}.", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("Couldn't read Vanilla's file list: {}", e))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err("Vanilla's file list is too large.".into());
    }
    parse_manifest(&String::from_utf8_lossy(&bytes))
}

/// `{ success, installedVersion, latestVersion, hasUpdate, versionKnown }`
/// for the Home update check.
pub(crate) async fn check_update() -> Value {
    let installed = installed_version();
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return json!({ "success": false, "error": e }),
    };
    match fetch_manifest(&client).await {
        Ok(m) => json!({
            "success": true,
            "installedVersion": installed.clone().unwrap_or_default(),
            "latestVersion": m.version,
            "versionKnown": installed.is_some(),
            "hasUpdate": installed.as_deref().is_some_and(|v| v != m.version),
            "patchNotes": "",
        }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

fn sha256_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn emit(app: &AppHandle, v: Value) {
    let _ = app.emit("download-progress", v);
}

/// Free space on the drive holding `dir`, if Windows can say.
#[cfg(target_os = "windows")]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut avail: u64 = 0;
    // SAFETY: `wide` is NUL-terminated; the out pointer is a valid u64.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(avail)
}
#[cfg(not(target_os = "windows"))]
fn free_space(_dir: &Path) -> Option<u64> {
    None
}

// ── Files Rec EOS already checked ─────────────────────────────────────────
//
// Hashing all 7.7 GB on every Resume or update is slow, so a file whose hash
// matched is remembered with its size and modified time; while both are
// unchanged it isn't hashed again. Kept in Rec EOS's own data folder, never in
// the launcher's. Vanilla's launcher still checks every file itself before
// launching, so a stale entry can't get a bad file played.

const VERIFIED_FILE: &str = "vanilla-verified.json";

/// path → (size, modified time in ns, sha256).
type Verified = std::collections::HashMap<String, (u64, u128, String)>;

fn verified_path(app: &AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    app.path().app_local_data_dir().ok().map(|d| d.join(VERIFIED_FILE))
}

fn load_verified(app: &AppHandle) -> Verified {
    let Some(p) = verified_path(app) else { return Verified::new() };
    let Ok(text) = std::fs::read_to_string(p) else { return Verified::new() };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Verified::new() };
    let mut out = Verified::new();
    if let Some(obj) = v.as_object() {
        for (k, e) in obj {
            let (Some(size), Some(mtime), Some(sha)) = (
                e.get(0).and_then(Value::as_u64),
                e.get(1).and_then(Value::as_str).and_then(|s| s.parse::<u128>().ok()),
                e.get(2).and_then(Value::as_str),
            ) else {
                continue;
            };
            out.insert(k.clone(), (size, mtime, sha.to_string()));
        }
    }
    out
}

fn save_verified(app: &AppHandle, v: &Verified) {
    let Some(p) = verified_path(app) else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let obj: serde_json::Map<String, Value> =
        v.iter().map(|(k, (s, m, h))| (k.clone(), json!([s, m.to_string(), h]))).collect();
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, Value::Object(obj).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn mtime_ns(m: &std::fs::Metadata) -> u128 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Record a file that just matched its hash.
fn remember(verified: &std::sync::Mutex<Verified>, dest: &Path, f: &ManifestFile) {
    if let Ok(m) = dest.metadata() {
        verified.lock().unwrap().insert(f.path.clone(), (m.len(), mtime_ns(&m), f.sha256.clone()));
    }
}

/// Which manifest files still need downloading: missing, the wrong size, or
/// (same size) the wrong hash. Files remembered as checked, unchanged since,
/// aren't hashed again; the rest hash on a few threads, reporting progress.
fn files_needed(
    app: &AppHandle,
    root: &Path,
    files: &[ManifestFile],
    verified: &std::sync::Mutex<Verified>,
) -> Result<Vec<usize>, String> {
    // One look at each file: missing / wrong size → needed; remembered and
    // unchanged → fine; otherwise it gets hashed.
    let mut need = Vec::new();
    let mut to_hash: Vec<usize> = Vec::new();
    {
        let known = verified.lock().unwrap();
        for (i, f) in files.iter().enumerate() {
            let Some(rel) = safe_relative_path(&f.path) else { return Err(format!("Unsafe path {}", f.path)) };
            match root.join(rel).metadata() {
                Ok(m) if m.is_file() && m.len() == f.size => {
                    let same = known
                        .get(&f.path)
                        .is_some_and(|(s, t, h)| *s == m.len() && *t == mtime_ns(&m) && *h == f.sha256);
                    if !same {
                        to_hash.push(i);
                    }
                }
                _ => need.push(i),
            }
        }
    }
    let total_check: u64 = to_hash.iter().map(|&i| files[i].size).sum();
    let checked = AtomicU64::new(0);
    let next = AtomicU64::new(0);
    let need = std::sync::Mutex::new(need);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| loop {
                if crate::download::cancel_requested() {
                    return;
                }
                let Some(&i) = to_hash.get(next.fetch_add(1, Ordering::SeqCst) as usize) else { return };
                let f = &files[i];
                let Some(rel) = safe_relative_path(&f.path) else { return };
                let dest = root.join(rel);
                if sha256_file(&dest).as_deref() == Some(f.sha256.as_str()) {
                    remember(verified, &dest, f);
                } else {
                    need.lock().unwrap().push(i);
                }
                checked.fetch_add(f.size, Ordering::SeqCst);
            });
        }
        // Progress while the threads work.
        let mut last = Instant::now();
        while (next.load(Ordering::SeqCst) as usize) < to_hash.len() && !crate::download::cancel_requested() {
            std::thread::sleep(Duration::from_millis(150));
            if last.elapsed() >= Duration::from_millis(500) && total_check > 0 {
                last = Instant::now();
                let pct = (checked.load(Ordering::SeqCst) as f64 / total_check as f64 * 100.0).min(99.0) as i64;
                emit(app, json!({ "phase": "download", "pct": pct, "downloaded": 0, "total": 0, "speed": 0, "eta": -1,
                    "status": "Checking the files already downloaded..." }));
            }
        }
    });
    if crate::download::cancel_requested() {
        return Err(crate::download::CANCELLED.into());
    }
    let mut v = need.into_inner().unwrap();
    v.sort_unstable();
    Ok(v)
}

enum Stop {
    Paused,
    Cancelled,
}

/// Where a file is downloaded to before it's checked: a hidden name beside it.
fn tmp_path(root: &Path, f: &ManifestFile) -> Option<PathBuf> {
    let dest = root.join(safe_relative_path(&f.path)?);
    let name = dest.file_name()?.to_string_lossy().to_string();
    Some(dest.with_file_name(format!(".{}{}", name, TMP_SUFFIX)))
}

/// Size of a partly downloaded file, if there is a usable one.
fn partial_len(root: &Path, f: &ManifestFile) -> u64 {
    tmp_path(root, f)
        .and_then(|t| t.metadata().ok())
        .map(|m| m.len())
        .filter(|&n| n <= f.size)
        .unwrap_or(0)
}

/// Hash the bytes already in a partial file, so a resumed download can carry on.
fn hash_prefix(path: &Path) -> Option<Sha256> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            return Some(h);
        }
        h.update(&buf[..n]);
    }
}

/// Download one file to a temporary name, check it, then move it into place.
/// A partial file from a pause, stall or earlier run is continued (a byte
/// range request); `downloaded` already counts its bytes.
async fn download_one(
    client: &reqwest::Client,
    root: &Path,
    f: &ManifestFile,
    downloaded: &AtomicU64,
    verified: &std::sync::Mutex<Verified>,
) -> Result<(), Result<Stop, String>> {
    let rel = safe_relative_path(&f.path).ok_or_else(|| Err(format!("Unsafe path {}", f.path)))?;
    let dest = root.join(&rel);
    let parent = dest.parent().unwrap_or(root).to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|e| Err(format!("Couldn't create {}: {}", parent.display(), e)))?;
    let tmp = tmp_path(root, f).ok_or_else(|| Err(format!("Unsafe path {}", f.path)))?;

    // Throw away a partial file (it's bad or the server won't continue it).
    let discard = |have: u64| {
        let _ = std::fs::remove_file(&tmp);
        downloaded.fetch_sub(have, Ordering::SeqCst);
    };

    let mut last_err = String::new();
    for attempt in 0..ATTEMPTS {
        let mut have = partial_len(root, f);
        let result: Result<(), Result<Stop, String>> = async {
            // Carry on from what's there: hash those bytes first.
            let mut hasher = if have > 0 {
                let t = tmp.clone();
                match tokio::task::spawn_blocking(move || hash_prefix(&t)).await.ok().flatten() {
                    Some(h) => h,
                    None => {
                        discard(have);
                        have = 0;
                        Sha256::new()
                    }
                }
            } else {
                Sha256::new()
            };
            if have < f.size {
                let mut req = client.get(file_url(&f.path));
                if have > 0 {
                    req = req.header(reqwest::header::RANGE, format!("bytes={}-", have));
                }
                // Only the wait for the answer is limited; the body is bounded by
                // the stall cutoff below, so a slow link never runs out of time.
                let resp = tokio::time::timeout(crate::download::RESPONSE_TIMEOUT, req.send())
                    .await
                    .map_err(|_| Err(format!("{}: Vanilla's file server didn't answer", f.path)))?
                    .map_err(|e| Err(format!("{}: {}", f.path, e)))?;
                if resp.url().host_str() != Some(CDN_HOST) {
                    return Err(Err("Vanilla's file server answered from somewhere unexpected.".into()));
                }
                let status = resp.status();
                if have > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
                    // The server sent the whole file instead: start over.
                    discard(have);
                    have = 0;
                    hasher = Sha256::new();
                }
                if !status.is_success() {
                    return Err(Err(format!("{}: server answered {}", f.path, status.as_u16())));
                }
                let mut file = if have > 0 {
                    tokio::fs::OpenOptions::new().append(true).open(&tmp).await
                } else {
                    tokio::fs::File::create(&tmp).await
                }
                .map_err(|e| Err(format!("Couldn't write {}: {}", tmp.display(), e)))?;
                let mut stream = resp.bytes_stream();
                loop {
                    if crate::download::cancel_requested() {
                        return Err(Ok(Stop::Cancelled));
                    }
                    if crate::download::pause_requested() {
                        tokio::io::AsyncWriteExt::flush(&mut file).await.ok();
                        return Err(Ok(Stop::Paused));
                    }
                    let next = tokio::time::timeout(crate::download::STALL_TIMEOUT, stream.next())
                        .await
                        .map_err(|_| Err(format!("{}: the download stalled", f.path)))?;
                    let Some(chunk) = next else { break };
                    let chunk = chunk.map_err(|e| Err(format!("{}: {}", f.path, e)))?;
                    if have + chunk.len() as u64 > f.size {
                        discard(have);
                        have = 0;
                        return Err(Err(format!("{}: the server sent more than the list says", f.path)));
                    }
                    hasher.update(&chunk);
                    tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                        .await
                        .map_err(|e| Err(format!("Couldn't write {}: {}", tmp.display(), e)))?;
                    have += chunk.len() as u64;
                    downloaded.fetch_add(chunk.len() as u64, Ordering::SeqCst);
                }
                tokio::io::AsyncWriteExt::flush(&mut file).await.map_err(|e| Err(e.to_string()))?;
                drop(file);
            }
            if have != f.size || hex(&hasher.finalize()) != f.sha256 {
                discard(have);
                have = 0;
                return Err(Err(format!("{} didn't match Vanilla's file list", f.path)));
            }
            if dest.exists() {
                let _ = std::fs::remove_file(&dest);
            }
            std::fs::rename(&tmp, &dest).map_err(|e| Err(format!("Couldn't place {}: {}", f.path, e)))?;
            remember(verified, &dest, f);
            Ok(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            // A pause keeps the partial file for Resume; a cancel drops it.
            Err(Ok(Stop::Paused)) => return Err(Ok(Stop::Paused)),
            Err(Ok(Stop::Cancelled)) => {
                discard(have);
                return Err(Ok(Stop::Cancelled));
            }
            // Network trouble keeps what arrived; the next attempt continues it.
            Err(Err(e)) => {
                last_err = e;
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    Err(Err(last_err))
}

/// Remove Rec EOS temp files that aren't a partial of a file this manifest
/// lists (left by an older version, or all of them with `keep = None`).
fn clean_tmp(root: &Path, keep: Option<&std::collections::HashSet<PathBuf>>) {
    fn walk(dir: &Path, depth: usize, keep: Option<&std::collections::HashSet<PathBuf>>) {
        if depth > 12 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, depth + 1, keep);
            } else if p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(TMP_SUFFIX))
                && !keep.is_some_and(|k| k.contains(&p))
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    walk(root, 0, keep);
}

/// Delete files the previous manifest (the launcher's own record) listed and
/// the new one doesn't. Nothing outside that list is ever touched.
fn remove_dropped(root: &Path, new: &Manifest) -> usize {
    let Ok(text) = std::fs::read_to_string(root.join(LAUNCHER_MANIFEST)) else { return 0 };
    let Ok(old) = parse_manifest(&text) else { return 0 };
    let keep: std::collections::HashSet<&str> = new.files.iter().map(|f| f.path.as_str()).collect();
    let mut removed = 0;
    for f in &old.files {
        if keep.contains(f.path.as_str()) {
            continue;
        }
        if let Some(rel) = safe_relative_path(&f.path) {
            if std::fs::remove_file(root.join(rel)).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

/// Download (or update) Vanilla's game, then have Vanilla's launcher sign it
/// off. Shares the client download's guard, cancel and pause with the rest of
/// the launcher, and reports on the same `download-progress` event.
pub(crate) async fn download(app: AppHandle) -> Result<Value, String> {
    let _guard = crate::download::claim_client_task()?;
    crate::download::reset_cancel();
    crate::download::reset_pause();

    if crate::game::find_vanilla_launcher().is_none() {
        return Err("Install Vanilla's launcher first (GET VANILLA).".into());
    }
    if crate::game::game_running(&app) {
        return Err("Close the game first.".into());
    }
    let root = game_dir().ok_or("Couldn't find Vanilla's game folder.")?;
    std::fs::create_dir_all(&root).map_err(|e| format!("Couldn't create {}: {}", root.display(), e))?;

    emit(&app, json!({ "phase": "download", "pct": -1, "downloaded": 0, "total": 0, "speed": 0, "eta": -1,
        "status": "Getting Vanilla's file list..." }));
    let client = http_client()?;
    let manifest = fetch_manifest(&client).await?;

    // Partial files of this version's files are kept (they resume); any
    // other Rec EOS temp file is left over from something older.
    let keep: std::collections::HashSet<PathBuf> = manifest.files.iter().filter_map(|f| tmp_path(&root, f)).collect();
    clean_tmp(&root, Some(&keep));

    // What isn't already there and right.
    let verified = Arc::new(std::sync::Mutex::new(load_verified(&app)));
    let check_app = app.clone();
    let check_root = root.clone();
    let check_files = manifest.files.clone();
    let check_verified = verified.clone();
    let needed = tokio::task::spawn_blocking(move || files_needed(&check_app, &check_root, &check_files, &check_verified))
        .await
        .map_err(|e| format!("File check failed: {}", e))?;
    save_verified(&app, &verified.lock().unwrap());
    let needed = needed?;
    let need_bytes: u64 = needed.iter().map(|&i| manifest.files[i].size).sum();
    // Bytes already in partial files count as done (they're continued).
    let resumed_bytes: u64 = needed.iter().map(|&i| partial_len(&root, &manifest.files[i])).sum();

    let still_to_fetch = need_bytes.saturating_sub(resumed_bytes);
    if let Some(free) = free_space(&root) {
        if free < still_to_fetch + SPACE_MARGIN {
            return Err(format!(
                "Not enough disk space: Vanilla needs {} more, and the drive has {} free.",
                crate::download::human_bytes(still_to_fetch + SPACE_MARGIN),
                crate::download::human_bytes(free)
            ));
        }
    }

    // Download what's missing, a few files at a time.
    let downloaded = Arc::new(AtomicU64::new(resumed_bytes));
    let done = Arc::new(AtomicBool::new(false));
    let reporter = {
        let app = app.clone();
        let downloaded = downloaded.clone();
        let done = done.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            while !done.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let got = downloaded.load(Ordering::SeqCst);
                let secs = start.elapsed().as_secs_f64().max(0.001);
                // Speed counts only this session's bytes, not resumed ones.
                let speed = got.saturating_sub(resumed_bytes) as f64 / secs;
                let pct = if need_bytes > 0 { (got as f64 / need_bytes as f64 * 100.0).min(99.0) as i64 } else { 99 };
                let eta = if speed > 0.0 { (need_bytes.saturating_sub(got) as f64 / speed) as i64 } else { -1 };
                emit(&app, json!({ "phase": "download", "pct": pct, "downloaded": got, "total": need_bytes,
                    "speed": speed as u64, "eta": eta, "resumable": true }));
            }
        })
    };

    // Each task owns its copies (the client is a cheap handle), so the futures
    // borrow nothing and can move between the runtime's threads.
    let tasks: Vec<_> = needed
        .iter()
        .map(|&i| (client.clone(), root.clone(), manifest.files[i].clone(), downloaded.clone(), verified.clone()))
        .collect();
    let results: Vec<Result<(), Result<Stop, String>>> = futures_util::stream::iter(tasks.into_iter().map(
        |(client, root, f, downloaded, verified)| async move {
            if crate::download::cancel_requested() {
                return Err(Ok(Stop::Cancelled));
            }
            if crate::download::pause_requested() {
                return Err(Ok(Stop::Paused));
            }
            download_one(&client, &root, &f, &downloaded, &verified).await
        },
    ))
    .buffer_unordered(CONCURRENCY)
    .collect()
    .await;
    done.store(true, Ordering::SeqCst);
    let _ = reporter.await;
    save_verified(&app, &verified.lock().unwrap());

    if results.iter().any(|r| matches!(r, Err(Ok(Stop::Cancelled)))) || crate::download::cancel_requested() {
        // A cancel starts fresh next time: drop the partial files.
        clean_tmp(&root, None);
        return Err(crate::download::CANCELLED.into());
    }
    if results.iter().any(|r| matches!(r, Err(Ok(Stop::Paused)))) || crate::download::pause_requested() {
        // Partial files stay, so Resume carries on from them.
        let got = downloaded.load(Ordering::SeqCst);
        let pct = if need_bytes > 0 { (got as f64 / need_bytes as f64 * 100.0).min(99.0) as i64 } else { -1 };
        emit(&app, json!({ "phase": "paused", "pct": pct, "downloaded": got, "total": need_bytes }));
        return Err("Paused".into());
    }
    if let Some(Err(Err(e))) = results.into_iter().find(|r| r.is_err()) {
        return Err(format!("{} Press DOWNLOAD to continue — what's downloaded so far is kept.", e));
    }

    let removed = remove_dropped(&root, &manifest);

    // Hand over to Vanilla's launcher to check the files and record the install.
    emit(&app, json!({ "phase": "extract", "pct": -1, "status": "Vanilla's launcher is checking the files..." }));
    let signed = sign_off(&app, &manifest.version).await;
    Ok(json!({
        "success": true,
        "version": manifest.version,
        "downloadedFiles": needed.len(),
        "removedFiles": removed,
        "signedOff": signed.as_deref() == Ok("done"),
        "signOff": match &signed { Ok(s) => s.clone(), Err(e) => e.clone() },
        "exePath": root.join("RecRoom.exe").to_string_lossy(),
    }))
}

/// Verify Files for Vanilla: hash every game file against Vanilla's file list
/// (none skipped, unlike a download's check), reporting `verify` progress.
/// Answers like `verify::verify_client`: `{status: ok | damaged, checked,
/// damaged: [{path, problem}], damagedBytes}`. Files that pass are remembered
/// as checked and the ones that fail are forgotten, so the repair (a normal
/// [`download`]) fetches exactly those.
pub(crate) async fn verify(app: AppHandle) -> Result<Value, String> {
    let _guard = crate::download::claim_client_task()?;
    crate::download::reset_cancel();
    if crate::game::game_running(&app) {
        return Err("Close the game before checking its files.".into());
    }
    let root = game_dir().filter(|d| d.is_dir()).ok_or("There's no installed game to check.")?;
    emit(&app, json!({ "phase": "verify", "pct": -1, "done": 0, "totalEntries": 0, "entry": "Getting Vanilla's file list...", "eta": -1 }));
    let manifest = fetch_manifest(&http_client()?).await?;

    let verified = Arc::new(std::sync::Mutex::new(load_verified(&app)));
    let (check_app, check_root, files, check_verified) = (app.clone(), root.clone(), manifest.files.clone(), verified.clone());
    let damaged = tokio::task::spawn_blocking(move || check_every_file(&check_app, &check_root, &files, &check_verified))
        .await
        .map_err(|e| format!("File check failed: {}", e))?;
    save_verified(&app, &verified.lock().unwrap());
    let damaged = damaged?;

    let damaged_bytes: u64 = damaged.iter().map(|(i, _)| manifest.files[*i].size).sum();
    let list: Vec<Value> = damaged.iter().map(|(i, p)| json!({ "path": manifest.files[*i].path, "problem": p })).collect();
    Ok(json!({
        "status": if list.is_empty() { "ok" } else { "damaged" },
        "checked": manifest.files.len(),
        "damaged": list,
        "damagedBytes": damaged_bytes,
    }))
}

/// Hash every file on a few threads. Returns the failing ones (index into
/// `files`, with `missing` / `size` / `content`), sorted.
fn check_every_file(
    app: &AppHandle,
    root: &Path,
    files: &[ManifestFile],
    verified: &std::sync::Mutex<Verified>,
) -> Result<Vec<(usize, &'static str)>, String> {
    let total: u64 = files.iter().map(|f| f.size).sum::<u64>().max(1);
    let hashed = AtomicU64::new(0);
    let next = AtomicU64::new(0);
    let done = AtomicU64::new(0);
    let last_name = std::sync::Mutex::new(String::new());
    let bad = std::sync::Mutex::new(Vec::new());
    let started = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| loop {
                if crate::download::cancel_requested() {
                    return;
                }
                let i = next.fetch_add(1, Ordering::SeqCst) as usize;
                let Some(f) = files.get(i) else { return };
                *last_name.lock().unwrap() = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
                let problem = match safe_relative_path(&f.path).map(|rel| root.join(rel)) {
                    None => Some("missing"),
                    Some(dest) => match dest.metadata() {
                        Ok(m) if m.is_file() && m.len() == f.size => {
                            if sha256_file(&dest).as_deref() == Some(f.sha256.as_str()) {
                                remember(verified, &dest, f);
                                None
                            } else {
                                Some("content")
                            }
                        }
                        Ok(m) if m.is_file() => Some("size"),
                        _ => Some("missing"),
                    },
                };
                // BepInEx rewrites its cache every time the game starts, so a
                // changed cache file isn't damage (a missing one still is).
                let problem = problem.filter(|p| !(*p != "missing" && f.path.starts_with("BepInEx/cache/")));
                if let Some(p) = problem {
                    verified.lock().unwrap().remove(&f.path);
                    bad.lock().unwrap().push((i, p));
                }
                hashed.fetch_add(f.size, Ordering::SeqCst);
                done.fetch_add(1, Ordering::SeqCst);
            });
        }
        while (done.load(Ordering::SeqCst) as usize) < files.len() && !crate::download::cancel_requested() {
            std::thread::sleep(Duration::from_millis(400));
            let h = hashed.load(Ordering::SeqCst);
            let pct = (h as f64 / total as f64 * 100.0).min(99.0) as i64;
            let secs = started.elapsed().as_secs_f64();
            let eta = if h > 0 { (secs / h as f64 * total.saturating_sub(h) as f64) as i64 } else { -1 };
            emit(app, json!({ "phase": "verify", "pct": pct, "done": done.load(Ordering::SeqCst),
                "totalEntries": files.len(), "entry": last_name.lock().unwrap().clone(), "eta": eta }));
        }
    });
    if crate::download::cancel_requested() {
        return Err(crate::download::CANCELLED.into());
    }
    let mut v = bad.into_inner().unwrap();
    v.sort_unstable();
    Ok(v)
}

/// Uninstall Vanilla's game the way Vanilla's own launcher does (its Uninstall
/// button deletes the game folder and drops the game from `installedVersions`):
/// close the launcher first, so it can't write back the record it holds in
/// memory, then remove the entry and delete `<installDirectory>\Vanilla`.
/// The entry goes first: if the delete then fails part way, the game reads as
/// not installed (a download fills in what's missing) rather than installed
/// with files gone. Refuses a folder that doesn't look like Vanilla's game.
pub(crate) async fn uninstall(app: &AppHandle) -> Result<(), String> {
    if crate::game::game_running(app) {
        return Err("Cannot uninstall while the game is running.".into());
    }
    let _guard = crate::download::claim_client_task()
        .map_err(|_| "Cannot uninstall while a download or file check is in progress.".to_string())?;
    tokio::task::spawn_blocking(crate::vanilla_launcher::close_vanilla_launcher)
        .await
        .map_err(|e| e.to_string())?;
    // Killed processes let go of their files only once they've exited.
    for _ in 0..40 {
        if !crate::game::any_vanilla_launcher_process() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let root = game_dir().filter(|d| d.exists());
    if let Some(root) = &root {
        let looks_like_game = root.file_name().is_some_and(|n| n.eq_ignore_ascii_case("Vanilla"))
            && (root.join("RecRoom.exe").exists() || root.join(LAUNCHER_MANIFEST).exists() || root.join("RecRoom_Data").exists());
        if !looks_like_game {
            return Err(format!("Nothing was removed: '{}' doesn't look like Vanilla's game folder.", root.display()));
        }
    }

    forget_launcher_entry()?;
    if let Some(p) = verified_path(app) {
        let _ = std::fs::remove_file(p);
    }

    if let Some(root) = root {
        // A file still briefly held (antivirus scanning it, a process just
        // exiting) gets a few more tries.
        let dir = root.clone();
        tokio::task::spawn_blocking(move || remove_tree(&dir))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("Couldn't delete everything in {}: {}. Close anything using it and press Uninstall again.", root.display(), e))?;
    }
    Ok(())
}

/// Delete `dir` and everything in it. `std::fs::remove_dir_all` stops at the
/// first file it can't delete (seen here: a sharing violation on a file that
/// nothing held a moment later), leaving almost everything; this deletes what
/// it can, retries the rest a few times, and names a file that still won't go.
/// Doesn't follow links out of the folder (a link is deleted, not entered).
fn remove_tree(dir: &Path) -> Result<(), String> {
    fn pass(dir: &Path, failed: &mut Vec<(PathBuf, std::io::Error)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() {
                pass(&p, failed);
                if let Err(err) = std::fs::remove_dir(&p) {
                    if p.exists() && failed.is_empty() {
                        failed.push((p, err));
                    }
                }
            } else {
                // A read-only file can't be deleted until that's cleared.
                if let Ok(m) = std::fs::symlink_metadata(&p) {
                    let mut perm = m.permissions();
                    if perm.readonly() {
                        #[allow(clippy::permissions_set_readonly_false)]
                        perm.set_readonly(false);
                        let _ = std::fs::set_permissions(&p, perm);
                    }
                }
                let removed = if ft.is_symlink() && ft.is_dir() { std::fs::remove_dir(&p) } else { std::fs::remove_file(&p) };
                if let Err(err) = removed {
                    failed.push((p, err));
                }
            }
        }
    }
    let mut last: Option<(PathBuf, std::io::Error)> = None;
    for attempt in 0..8 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(1000));
        }
        let mut failed = Vec::new();
        pass(dir, &mut failed);
        let _ = std::fs::remove_dir(dir);
        if !dir.exists() {
            return Ok(());
        }
        last = failed.into_iter().next();
    }
    Err(match last {
        Some((p, e)) => format!("{} ({})", e, p.strip_prefix(dir).unwrap_or(&p).display()),
        None => "the folder is still there".into(),
    })
}

/// Remove Vanilla's entry from the launcher's `installedVersions` (and only
/// that), as the launcher's own Uninstall does.
fn forget_launcher_entry() -> Result<(), String> {
    if let Some(path) = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("VanillaLauncher").join("settings.json")) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut v) = serde_json::from_str::<Value>(&text) {
                let removed = v.get_mut("installedVersions").and_then(Value::as_object_mut).is_some_and(|m| m.remove(APP_ID).is_some());
                if removed {
                    let tmp = path.with_extension("json.receos-tmp");
                    let pretty = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
                    std::fs::write(&tmp, pretty).and_then(|_| std::fs::rename(&tmp, &path))
                        .map_err(|e| format!("Couldn't update Vanilla's launcher, so nothing was removed: {}", e))?;
                }
            }
        }
    }
    Ok(())
}

/// Have Vanilla's launcher check the files and record the install: open it,
/// press its Download / Update / Resume, and wait until it offers Launch.
/// Never presses Cancel (the launcher's Cancel deletes the install folder).
/// Returns `done`, `needs_signin`, or an error.
async fn sign_off(app: &AppHandle, version: &str) -> Result<String, String> {
    let Some(launcher) = crate::game::find_vanilla_launcher() else {
        return Err("Vanilla's launcher isn't installed.".into());
    };
    // Only the launcher's own record counts here — not the version in its
    // manifest file, which can already say this version while the record is
    // still "" and the launcher is about to ask for an update.
    if launcher_entry().as_deref() == Some(version) {
        return Ok("done".into());
    }
    #[cfg(target_os = "windows")]
    {
        let app = app.clone();
        let version = version.to_string();
        tokio::task::spawn_blocking(move || run_sign_off(&app, &launcher, &version))
            .await
            .map_err(|e| format!("Couldn't reach Vanilla's launcher: {}", e))?
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, launcher);
        Err("Only on Windows.".into())
    }
}

#[cfg(target_os = "windows")]
fn run_sign_off(app: &AppHandle, launcher: &str, version: &str) -> Result<String, String> {
    use std::io::{BufRead, BufReader};
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    // The version is compared inside a single-quoted PowerShell string; it comes
    // from Vanilla's manifest, so keep only characters a version uses.
    let version: String = version.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+')).collect();
    let script = format!("{}{}", crate::vanilla_launcher::UIA_PRELUDE, SIGN_OFF_PS)
        .replace("__LAUNCHER_PATH__", &launcher.replace('\'', "''"))
        .replace("__VERSION__", &version);
    let mut child = std::process::Command::new(crate::defender::get_powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .spawn()
        .map_err(|e| format!("Couldn't reach Vanilla's launcher: {}", e))?;
    let behind = crate::game::VanillaLauncherBehind::start(app, true);
    let mut result = "timeout".to_string();
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let line = line.trim();
            if let Some(p) = line.strip_prefix("pct:") {
                let pct: i64 = p.parse().unwrap_or(-1);
                emit(app, json!({ "phase": "extract", "pct": pct, "status": "Vanilla's launcher is checking the files..." }));
            } else if line == "launcher:shown" {
                behind.mark_shown();
            } else if line == "state:restart" {
                emit(app, json!({ "phase": "extract", "pct": -1, "status": "Restarting Vanilla's launcher..." }));
            } else if line == "state:signin" {
                emit(app, json!({ "phase": "extract", "pct": -1, "status": "Sign in to Vanilla's launcher to finish." }));
                let _ = app.emit("vanilla-autoplay", "signin");
            } else if matches!(line, "done" | "needs_signin" | "timeout" | "no_launcher") {
                result = line.to_string();
            }
        }
    }
    let _ = child.wait();
    match result.as_str() {
        "done" => Ok("done".into()),
        "needs_signin" => Ok("needs_signin".into()),
        "no_launcher" => Err("Vanilla's launcher didn't open.".into()),
        _ => Err("Vanilla's launcher didn't finish checking the files in time. Press PLAY to try again.".into()),
    }
}

/// UI Automation: open Vanilla's launcher (found by its Tauri window class,
/// restored if minimised, given back a usable size if tiny), then on its game
/// page press the launch bar while it reads Download / Update, or the Resume
/// button while Paused — never Cancel — and wait (up to 20 min) for Launch.
/// Prints `pct:N` while it works, `state:signin` at a sign-in screen, and
/// finally `done`, `needs_signin`, `timeout` or `no_launcher`.
#[cfg(target_os = "windows")]
const SIGN_OFF_PS: &str = r#"
# Helpers (Find-LauncherWindow, Restore-LauncherWindow, …) come from
# vanilla_launcher::UIA_PRELUDE, prepended by run_sign_off.
if (-not (Test-LauncherUp)) { Start-Launcher }

$deadline = (Get-Date).AddMinutes(20)
$pressedAt = [DateTime]::MinValue
$signinSaid = $false; $result = 'timeout'; $sawWindow = $false; $lastPct = -2
$restarted = $false; $stuckSince = $null; $signinSeen = 0
while ((Get-Date) -lt $deadline) {
  $top = Find-LauncherWindow
  if ($top -eq [IntPtr]::Zero) { if (-not $sawWindow -and ((Get-Date) - $deadline.AddMinutes(-20)).TotalSeconds -gt 60) { $result = 'no_launcher'; break }; Start-Sleep 1; continue }
  $sawWindow = $true
  Restore-LauncherWindow $top
  $all = @(Get-LauncherElements $top)
  $launchText = $null; $dlText = $null; $resume = $null; $pct = -1; $signin = $false; $loading = $false; $modes = $false; $paused = $false
  foreach ($e in $all) {
    $c = $e.Current; $n = "$($c.Name)".Trim(); $type = $c.ControlType.ProgrammaticName
    if ($type -eq 'ControlType.Text' -and $n -eq 'Paused') { $paused = $true }
    if ($type -eq 'ControlType.Text' -and $n -match '^(?i)launch$') { $launchText = $e }
    if ($type -eq 'ControlType.Text' -and $n -match '^(?i)(download|update)$') { $dlText = $e }
    if ($type -eq 'ControlType.Button' -and $n -eq 'Resume') { $resume = $e }
    if ($type -eq 'ControlType.Button' -and $n -match '^(VR|Screen)$') { $modes = $true }
    if ($n -match '^(\d{1,3})%') { $pct = [int]$Matches[1] }
    if ($c.IsPassword -or $n -match '(?i)^login$') { $signin = $true }
    if ($n -eq 'Loading') { $loading = $true }
  }
  # Done only when it offers Launch AND has recorded a real version: after a
  # sign-off of files already on disk it can record "" and then, a moment
  # later, decide that's an update — so keep going (pressing Update) until then.
  if ($launchText -and $modes) {
    $ver = ''
    try { $ver = "$(((Get-Content "$env:APPDATA\VanillaLauncher\settings.json" -Raw | ConvertFrom-Json).installedVersions).'1')" } catch {}
    if ($ver.Trim() -eq '__VERSION__') { $result = 'done'; break }
  }
  # The login form also flashes up during auto sign-in, so it has to hold.
  if ($signin -and -not $loading) { $signinSeen++ } else { $signinSeen = 0 }
  if ($signinSeen -ge 4) {
    if (-not $signinSaid) { Say 'state:signin'; $signinSaid = $true; Show-Launcher $top }
    Start-Sleep 1; continue
  }
  if ($signin) { Start-Sleep 1; continue }
  if ($pct -ge 0 -and $pct -ne $lastPct) { Say "pct:$pct"; $lastPct = $pct }
  # A download the launcher itself paused: its whole progress bar is marked
  # disabled for accessibility, so Resume can't be pressed from here. That
  # paused state lives only in the launcher's memory, so restart it (once);
  # it then offers Download again and reuses every file already on disk.
  if ($paused -and -not $restarted -and -not ($resume -and $resume.Current.IsEnabled)) {
    if (-not $stuckSince) { $stuckSince = Get-Date }
    if (((Get-Date) - $stuckSince).TotalSeconds -gt 4) {
      Say 'state:restart'
      $wp = 0; [VlaWin]::GetWindowThreadProcessId($top, [ref]$wp) | Out-Null
      if ($wp) { Stop-Process -Id ([int]$wp) -Force -ErrorAction SilentlyContinue }
      Start-Sleep 3
      Start-Launcher
      $restarted = $true; $pressedAt = [DateTime]::MinValue
      Start-Sleep 5
      continue
    }
  } else { $stuckSince = $null }
  # Press once, then give the launcher time to start before pressing again.
  if (((Get-Date) - $pressedAt).TotalSeconds -gt 20) {
    if ($resume -and $resume.Current.IsEnabled) { try { $resume.GetCurrentPattern($IP).Invoke(); $pressedAt = Get-Date } catch {} }
    elseif ($dlText) { $bar = Clickable-Ancestor $dlText; if ($bar) { try { $bar.GetCurrentPattern($IP).Invoke(); $pressedAt = Get-Date } catch {} } }
  }
  Start-Sleep 1
}
if ($result -eq 'timeout' -and $signinSaid) { $result = 'needs_signin' }
Say $result
"#;

#[cfg(test)]
mod tests {
    use super::{file_url, parse_manifest, remove_tree, safe_relative_path};

    #[test]
    fn remove_tree_deletes_nested_and_read_only_files() {
        let root = std::env::temp_dir().join(format!("receos-rt-{}", std::process::id()));
        let deep = root.join("RecRoom_Data").join("Managed");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("RecRoom.exe"), b"x").unwrap();
        let ro = deep.join("ro.dll");
        std::fs::write(&ro, b"y").unwrap();
        let mut perm = std::fs::metadata(&ro).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&ro, perm).unwrap();
        remove_tree(&root).expect("removes everything");
        assert!(!root.exists());
    }

    #[test]
    fn manifest_paths_must_stay_inside_the_game_folder() {
        for bad in ["", "/abs", "..", "a/../b", "./a", "C:/x", "a\\b", "a//b", "a/", "x:y", "a/.",
                    "RecRoom_Data/CON", "nul.txt", "a/com1.dll", "LPT9", "a./b", "a /b", "x?.dll", "a*b"] {
            assert!(safe_relative_path(bad).is_none(), "{bad} should be refused");
        }
        for good in ["RecRoom.exe", ".doorstop_version", "BepInEx/core/0Harmony.dll", "RecRoom_Data/a b.assets",
                     "console.dll", "COM0.txt", "auxiliary/x", "nulls.bin"] {
            assert!(safe_relative_path(good).is_some(), "{good} should be allowed");
        }
    }

    #[test]
    fn file_urls_encode_each_segment() {
        assert_eq!(file_url("a b/c#d.dll"), "https://cdn.vanillarec.net/AppStorage/1/files/a%20b/c%23d.dll");
        assert_eq!(file_url(".doorstop_version"), "https://cdn.vanillarec.net/AppStorage/1/files/.doorstop_version");
    }

    #[test]
    fn manifests_are_parsed_strictly() {
        let ok = r#"{"version":"1.0.14","files":[{"path":"RecRoom.exe","sha256":"2744735fee2ee237e29f592f88ab669dc8d478fca31e6da4caf9a4a1b3abb7f1","size":5}]}"#;
        let m = parse_manifest(ok).unwrap();
        assert_eq!(m.version, "1.0.14");
        assert_eq!(m.files.len(), 1);
        assert!(parse_manifest(&ok.replace("RecRoom.exe", "../evil.exe")).is_err());
        assert!(parse_manifest(&ok.replace("2744735f", "zz44735f")).is_err());
        assert!(parse_manifest(r#"{"version":"1","files":[]}"#).is_err());
        assert!(parse_manifest("not json").is_err());
    }
}
