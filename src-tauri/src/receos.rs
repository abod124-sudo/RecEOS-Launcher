//! Rec EOS: where its client comes from, and how the Rec EOS mod gets into it.
//!
//! Rec EOS runs the stock Rec Room 20220211 build with BepInEx 6 and the
//! RecEOS mod (`BepInEx/plugins/RecEOS.dll`), which moves the game onto Epic
//! Online Services and the Rec EOS servers. The install comes in two parts:
//!
//! * **The base client**, a zip on MediaFire ([`CLIENT_PAGE`]): the game, the
//!   BepInEx loader (Doorstop's `version.dll`, its .NET runtime, `BepInEx/core`
//!   and the generated interop assemblies) and a clean `UserData/RecEOS.json`,
//!   but none of the mod's own files. MediaFire hands out a short-lived direct
//!   link from its file page, so the link is looked up for every download, and
//!   the zip is only accepted if it hashes to [`CLIENT_SHA256`].
//! * **The mod**, from the Rec EOS update server: the same signed release the
//!   mod's in-game updater installs (`GET /v1/version?platform=pc`, files from
//!   `/updates/{sha256}`). The release is signed with the Rec EOS release key
//!   (ECDSA P-256, [`RELEASE_PUBLIC_KEY`]), and only the file names the mod's
//!   own updater allows ([`allowed_file`]) are ever written. This is the
//!   UPDATE button on Home: it fetches only the files that differ, so a game
//!   that already updated itself in-game has nothing to fetch.
//!
//! The game is started as `RecRoom.exe +mode:screen|vr`, never through
//! `Recroom_Release.exe` (Easy Anti-Cheat, which refuses BepInEx).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{Emitter, Manager};

use crate::config::{self, Network};
use crate::download::sha256_of;

/// The Rec EOS game server. `/v1/version`, `/updates/*` and `/web/*` are open
/// without signing in.
pub const API_ROOT: &str = "https://api.rec-eos.com";

/// The MediaFire page of the base client zip. Empty until it is uploaded: the
/// download then explains that there is no download yet.
pub const CLIENT_PAGE: &str = "";

/// SHA-256 of the base client zip (`RecEOS-Base-20220211.zip`), lowercase
/// hex. A download that hashes to anything else is deleted, not installed.
pub const CLIENT_SHA256: &str = "80c8fe07fe7148638cd71edaba47479b38cc83d62de45086d43b04424d7d64b5";

/// The Rec Room build the base client is, shown as its version.
pub const CLIENT_VERSION: &str = "20220211";

/// The Rec EOS release key's public half (SubjectPublicKeyInfo, base64), the
/// same as the mod's `Updater.PublicKey`.
const RELEASE_PUBLIC_KEY: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEc3kYqIamcw3L+Tu7pwi/sTKTQE1pxH27mDExR/S8j1l+KVQKwvxxB+wxnF2/HiRGDVxwGWYI9zAqCH8EFAN1Bw==";

pub const USER_AGENT: &str = "RecEOS Launcher/1.0";

/// Where the mod lives in the client folder; its presence is what "patched" means.
const MOD_DLL: &str = "BepInEx/plugins/RecEOS.dll";

/// Largest single release file this will write. The mod DLL is ~20 MB and the
/// EOS SDK DLL ~19 MB; anything far past that is not a Rec EOS file.
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

const PROGRESS_EVENT: &str = "receos-patch-progress";

static UPDATING: AtomicBool = AtomicBool::new(false);

struct UpdatingGuard;
impl Drop for UpdatingGuard {
    fn drop(&mut self) {
        UPDATING.store(false, Ordering::SeqCst);
    }
}

fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .https_only(true)
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())
}

// ─── The base client on MediaFire ───────────────────────────────────────────

/// Whether `url` is a MediaFire download server (`https://download1234.mediafire.com/...`).
fn is_mediafire_download(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.port().is_none()
        && host.ends_with(".mediafire.com")
        && host.starts_with("download")
}

/// The direct download link on a MediaFire file page: the `downloadButton`
/// anchor's `href`, or its base64 `data-scrambled-url` when the page hides it.
pub(crate) fn mediafire_direct_link(html: &str) -> Option<String> {
    use base64::Engine as _;
    let anchor = regex::Regex::new(r#"(?is)<a\b[^>]*\bid\s*=\s*["']downloadButton["'][^>]*>"#).ok()?;
    let tag = anchor.find(html)?.as_str();
    let attr = |name: &str| {
        regex::Regex::new(&format!(r#"(?is)\b{name}\s*=\s*["']([^"']*)["']"#))
            .ok()?
            .captures(tag)
            .map(|c| c[1].replace("&amp;", "&"))
    };
    let mut candidates = Vec::new();
    if let Some(href) = attr("href") {
        candidates.push(href);
    }
    if let Some(scrambled) = attr("data-scrambled-url") {
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(scrambled.trim()) {
            if let Ok(s) = String::from_utf8(bytes) {
                candidates.push(s);
            }
        }
    }
    candidates.into_iter().find(|u| is_mediafire_download(u))
}

/// The base client's current direct link, looked up from [`CLIENT_PAGE`].
pub async fn resolve_client_url() -> Result<String, String> {
    if CLIENT_PAGE.is_empty() {
        return Err("Rec EOS has no client download yet. Ask in the Rec EOS Discord.".into());
    }
    let html = http()?
        .get(CLIENT_PAGE)
        .header("User-Agent", crate::download::download_user_agent(Network::RecEos))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach MediaFire: {e}"))?
        .error_for_status()
        .map_err(|e| format!("MediaFire answered {e}"))?
        .text()
        .await
        .map_err(|e| format!("Couldn't read MediaFire's page: {e}"))?;
    mediafire_direct_link(&html)
        .ok_or_else(|| "MediaFire's page has no download link. The file may have been taken down.".to_string())
}

// ─── The signed mod release ─────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ReleaseFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    platform: String,
    build: u64,
    #[serde(default)]
    files: Vec<ReleaseFile>,
}

#[derive(Debug, Clone)]
pub(crate) struct Release {
    pub build: u64,
    pub minimum: u64,
    pub files: Vec<ReleaseFile>,
}

/// The file names the mod's own updater (`Updater.AllowedFiles`) may write.
/// A release naming anything else is refused whole, as the mod refuses it.
pub(crate) fn allowed_file(path: &str) -> bool {
    const PATTERNS: [&str; 7] = [
        r"^BepInEx/plugins/RecEOS\.dll$",
        r"^UserLibs/[^/]+\.dll$",
        r"^UserData/RecEOS-images/[^/]+$",
        r"^RecEOS-README\.txt$",
        r"^RecEOS-THIRD-PARTY-NOTICES\.txt$",
        r"^Play\.bat$",
        r"^Play (Screen Mode|VR)\.bat$",
    ];
    let plain = !path.is_empty()
        && !path.contains(['\\', ':'])
        && !path.chars().any(|c| c.is_control())
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..");
    plain
        && PATTERNS
            .iter()
            .any(|p| regex::Regex::new(p).map(|re| re.is_match(path)).unwrap_or(false))
}

/// Check `signature` (raw r‖s, base64url, as WebCrypto's ECDSA writes it) over
/// the manifest text exactly as served.
pub(crate) fn verify_release_signature(manifest: &str, signature: &str) -> Result<(), String> {
    use base64::Engine as _;
    use p256::ecdsa::signature::Verifier;
    use p256::pkcs8::DecodePublicKey;

    let der = base64::engine::general_purpose::STANDARD
        .decode(RELEASE_PUBLIC_KEY)
        .map_err(|e| e.to_string())?;
    let key = p256::ecdsa::VerifyingKey::from_public_key_der(&der).map_err(|e| e.to_string())?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(signature.trim().trim_end_matches('='))
        .map_err(|_| "The release signature is not readable.".to_string())?;
    let sig = p256::ecdsa::Signature::from_slice(&raw).map_err(|_| "The release signature is malformed.".to_string())?;
    key.verify(manifest.as_bytes(), &sig)
        .map_err(|_| "The Rec EOS release is not signed by the Rec EOS release key.".to_string())
}

/// Read a `/v1/version` answer into a checked release: signed, for PC, and
/// naming only allowed files with well-formed hashes.
pub(crate) fn parse_release(body: &Value) -> Result<Option<Release>, String> {
    let latest = body["latest"].as_u64().unwrap_or(0);
    let minimum = body["minimum"].as_u64().unwrap_or(0);
    if latest == 0 {
        // No PC release published at all.
        return Ok(None);
    }
    let manifest = body["manifest"].as_str().ok_or("The release has no manifest.")?;
    let signature = body["signature"].as_str().ok_or("The release has no signature.")?;
    verify_release_signature(manifest, signature)?;
    let m: Manifest = serde_json::from_str(manifest).map_err(|e| format!("The release manifest is unreadable: {e}"))?;
    if m.platform != "pc" {
        return Err("The release is not the PC one.".into());
    }
    if m.build != latest {
        return Err("The release manifest does not match the published build.".into());
    }
    for f in &m.files {
        let hash_ok = f.sha256.len() == 64 && f.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !allowed_file(&f.path) || !hash_ok || f.size > MAX_FILE_BYTES {
            return Err(format!("The release names a file it may not: \"{}\".", f.path));
        }
    }
    if !m.files.iter().any(|f| f.path == MOD_DLL) {
        return Err("The release has no RecEOS.dll.".into());
    }
    Ok(Some(Release { build: m.build, minimum, files: m.files }))
}

pub(crate) async fn fetch_release() -> Result<Release, String> {
    let body: Value = http()?
        .get(format!("{API_ROOT}/v1/version?platform=pc"))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Rec EOS: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Rec EOS answered {e}"))?
        .json()
        .await
        .map_err(|e| format!("Rec EOS's answer was unreadable: {e}"))?;
    parse_release(&body)?.ok_or_else(|| "Rec EOS has not published a PC release yet.".to_string())
}

// ─── What is installed ──────────────────────────────────────────────────────

/// The build stamped into a RecEOS.dll (`AssemblyMetadata("RecEosBuild", N)`),
/// read the way the publish script reads it.
pub(crate) fn stamped_build(dll: &[u8]) -> Option<u64> {
    const KEY: &[u8] = b"\x0bRecEosBuild";
    let at = dll.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    let len = *dll.get(at)? as usize;
    if !(1..=0x7f).contains(&len) {
        return None;
    }
    let digits = dll.get(at + 1..at + 1 + len)?;
    let digits = &digits[..digits.iter().take_while(|b| b.is_ascii_digit()).count()];
    if digits.is_empty() || digits.len() > 18 {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn installed_build(client_dir: &Path) -> Option<u64> {
    std::fs::read(client_dir.join(MOD_DLL)).ok().and_then(|b| stamped_build(&b))
}

fn file_matches(path: &Path, f: &ReleaseFile) -> bool {
    match std::fs::metadata(path) {
        Ok(md) if md.is_file() && md.len() == f.size => {}
        _ => return false,
    }
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).is_ok() && sha256_of(&bytes) == f.sha256
}

/// The release files `client_dir` does not have as published.
fn files_to_fetch(client_dir: &Path, release: &Release) -> Vec<ReleaseFile> {
    release
        .files
        .iter()
        .filter(|f| !file_matches(&client_dir.join(&f.path), f))
        .cloned()
        .collect()
}

fn client_dir(app: &tauri::AppHandle) -> PathBuf {
    let cfg = config::current(app);
    PathBuf::from(config::get_client_dir_for(app, &cfg, Network::RecEos))
}

/// Whether the mod is in the client folder at all: what launching needs.
pub fn mod_installed(client_dir: &str) -> bool {
    Path::new(client_dir).join(MOD_DLL).is_file()
}

/// Whether the installed mod needs files from the current release.
///
/// Returns `{ success, installed, updateAvailable, required, latestBuild,
/// installedBuild, minimumBuild, files }`. `required` means the game can't
/// be played without it: no mod, or a build below the server's minimum.
#[tauri::command]
pub async fn receos_patch_status(app: tauri::AppHandle) -> Value {
    let dir = client_dir(&app);
    let check_dir = dir.clone();
    let (installed, build) = tokio::task::spawn_blocking(move || {
        (check_dir.join(MOD_DLL).is_file(), installed_build(&check_dir))
    })
    .await
    .unwrap_or((false, None));

    match fetch_release().await {
        Ok(release) => {
            let check_dir = dir.clone();
            let check_release = release.clone();
            let need = tokio::task::spawn_blocking(move || files_to_fetch(&check_dir, &check_release))
                .await
                .unwrap_or_default();
            let below_minimum = build.unwrap_or(0) < release.minimum;
            json!({
                "success": true,
                "installed": installed,
                "updateAvailable": !need.is_empty(),
                "required": !installed || below_minimum,
                "latestBuild": release.build,
                "minimumBuild": release.minimum,
                "installedBuild": build,
                "files": need.len(),
                "bytes": need.iter().map(|f| f.size).sum::<u64>(),
            })
        }
        Err(e) => json!({
            "success": false,
            "installed": installed,
            "updateAvailable": !installed,
            "required": !installed,
            "installedBuild": build,
            "error": e,
        }),
    }
}

/// Install the current Rec EOS release: what the UPDATE button runs.
///
/// Emits `receos-patch-progress` `{ phase, pct }` for the progress bar.
#[tauri::command]
pub async fn receos_update_patch(app: tauri::AppHandle) -> Value {
    match update_patch(&app).await {
        Ok(build) => json!({ "success": true, "build": build }),
        Err(e) => {
            let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "error", "pct": 0 }));
            json!({ "success": false, "error": e })
        }
    }
}

pub(crate) async fn update_patch(app: &tauri::AppHandle) -> Result<u64, String> {
    if UPDATING.swap(true, Ordering::SeqCst) {
        return Err("An update is already running.".into());
    }
    let _guard = UpdatingGuard;
    // The running game has RecEOS.dll loaded; it updates itself in-game anyway.
    if crate::game::game_running(app) || crate::game::rec_room_running() {
        return Err("Close the game before updating.".into());
    }
    let dir = client_dir(app);
    if !dir.join("RecRoom.exe").is_file() {
        return Err("Download the Rec EOS client first.".into());
    }

    let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "download", "pct": 0 }));
    let release = fetch_release().await?;
    let check_dir = dir.clone();
    let check_release = release.clone();
    let need = tokio::task::spawn_blocking(move || files_to_fetch(&check_dir, &check_release))
        .await
        .map_err(|e| e.to_string())?;
    if need.is_empty() {
        let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "done", "pct": 100 }));
        return Ok(release.build);
    }

    // Staged outside the client folder: a half-written file in
    // BepInEx/plugins would be loaded (or flagged as a foreign mod).
    let staging = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("receos-staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("Couldn't make a folder for the update: {e}"))?;
    let _cleanup = RemoveDirOnDrop(staging.clone());

    let total: u64 = need.iter().map(|f| f.size).sum::<u64>().max(1);
    let mut done: u64 = 0;
    let mut last_pct = -1i64;
    let client = http()?;
    let mut fetched: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for f in &need {
        if fetched.contains_key(&f.sha256) {
            // Same content under another name (several room images share one).
            done += f.size;
            continue;
        }
        let mut res = client
            .get(format!("{API_ROOT}/updates/{}", f.sha256))
            .send()
            .await
            .map_err(|e| format!("Couldn't download {}: {e}", f.path))?
            .error_for_status()
            .map_err(|e| format!("Couldn't download {}: {e}", f.path))?;
        let mut bytes: Vec<u8> = Vec::with_capacity(f.size as usize);
        while let Some(chunk) = res.chunk().await.map_err(|e| format!("Couldn't download {}: {e}", f.path))? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() as u64 > f.size {
                return Err(format!("{} came back larger than published.", f.path));
            }
            let pct = (((done + bytes.len() as u64) as f64 / total as f64) * 95.0) as i64;
            if pct != last_pct {
                last_pct = pct;
                let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "download", "pct": pct }));
            }
        }
        if bytes.len() as u64 != f.size || sha256_of(&bytes) != f.sha256 {
            return Err(format!("{} didn't match the signed release. Nothing was changed.", f.path));
        }
        let staged = staging.join(&f.sha256);
        std::fs::write(&staged, &bytes).map_err(|e| format!("Couldn't save {}: {e}", f.path))?;
        fetched.insert(f.sha256.clone(), staged);
        done += f.size;
    }

    let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "install", "pct": 97 }));
    let place_dir = dir.clone();
    let installed_count = need.len();
    // Top-level names the mod adds (its readme and notices sit beside the
    // game), so uninstalling the client takes them too.
    let mut top_level: Vec<String> = need
        .iter()
        .filter_map(|f| f.path.split('/').next().map(str::to_string))
        .collect();
    top_level.sort();
    top_level.dedup();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        for f in &need {
            let src = &fetched[&f.sha256];
            let target = place_dir.join(&f.path);
            if !crate::config::path_is_inside_dir(&target.to_string_lossy(), &place_dir.to_string_lossy()) {
                return Err(format!("Refusing to write outside the client folder: {}", f.path));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("Couldn't install {}: {e}", f.path))?;
            }
            // Copied, not moved: one staged file can serve several names, and
            // the staging folder may be on another drive.
            let tmp = target.with_extension("receos-new");
            std::fs::copy(src, &tmp).map_err(|e| format!("Couldn't install {}: {e}", f.path))?;
            if let Err(e) = std::fs::rename(&tmp, &target) {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("Couldn't install {}: {e}", f.path));
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;
    crate::download::add_install_entries(&dir, &top_level);

    let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "done", "pct": 100 }));
    crate::applog::backend("info", "Rec EOS", format!("Installed Rec EOS build {} ({} file(s)).", release.build, installed_count));
    Ok(release.build)
}

struct RemoveDirOnDrop(PathBuf);
impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        // An antivirus scanning the files just written can hold the folder for
        // a moment after they are deleted, so a few tries, off this thread.
        let dir = self.0.clone();
        std::thread::spawn(move || {
            for _ in 0..10 {
                let _ = std::fs::remove_dir_all(&dir);
                if !dir.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });
    }
}

// ─── Server status ──────────────────────────────────────────────────────────

/// `/web/status`: `{ ok, closedBeta, maintenance, online }`.
pub async fn web_status() -> Result<Value, String> {
    http()?
        .get(format!("{API_ROOT}/web/status"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn receos_server_status() -> Value {
    match web_status().await {
        Ok(v) => json!({
            "success": true,
            "online": v["online"].as_u64(),
            "closedBeta": v["closedBeta"].as_bool().unwrap_or(false),
            "maintenance": v["maintenance"],
        }),
        Err(e) => json!({ "success": false, "error": e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("receos_release_fixture.json");

    fn fixture() -> Value {
        serde_json::from_str(FIXTURE).unwrap()
    }

    #[test]
    fn a_published_release_verifies() {
        let body = fixture();
        let release = parse_release(&body).unwrap().unwrap();
        assert_eq!(release.build, body["latest"].as_u64().unwrap());
        assert!(release.files.iter().any(|f| f.path == MOD_DLL));
        assert!(release.files.iter().any(|f| f.path.starts_with("UserLibs/")));
    }

    #[test]
    fn a_changed_manifest_is_refused() {
        let mut body = fixture();
        let tampered = body["manifest"].as_str().unwrap().replacen("RecEOS.dll", "RecEOS.dlx", 1);
        body["manifest"] = Value::from(tampered);
        assert!(parse_release(&body).is_err());
    }

    #[test]
    fn a_changed_signature_is_refused() {
        let mut body = fixture();
        let sig = body["signature"].as_str().unwrap().to_string();
        let flipped = match sig.strip_prefix('A') {
            Some(rest) => format!("B{rest}"),
            None => format!("A{}", &sig[1..]),
        };
        body["signature"] = Value::from(flipped);
        assert!(parse_release(&body).is_err());
    }

    #[test]
    fn no_release_is_not_an_error() {
        assert!(parse_release(&json!({ "platform": "pc", "latest": 0, "minimum": 0 })).unwrap().is_none());
    }

    #[test]
    fn only_the_mods_own_files_are_allowed() {
        for ok in [
            "BepInEx/plugins/RecEOS.dll",
            "UserLibs/EOSSDK-Win64-Shipping.dll",
            "UserData/RecEOS-images/RecCenter.jpg",
            "RecEOS-README.txt",
            "Play VR.bat",
            "Play Screen Mode.bat",
        ] {
            assert!(allowed_file(ok), "{ok}");
        }
        for bad in [
            "BepInEx/plugins/Other.dll",
            "BepInEx/core/BepInEx.Core.dll",
            "version.dll",
            "RecRoom.exe",
            "UserLibs/../version.dll",
            "UserLibs/x/../../version.dll",
            "/UserLibs/a.dll",
            "UserLibs\\a.dll",
            "C:/UserLibs/a.dll",
            "UserData/RecEOS-images/../RecEOS.json",
            "UserData/RecEOS-images/..",
            "",
        ] {
            assert!(!allowed_file(bad), "{bad}");
        }
    }

    #[test]
    fn the_build_stamp_is_read_from_the_dll() {
        let mut dll = b"MZ....junk\x00\x01".to_vec();
        dll.extend_from_slice(b"\x0bRecEosBuild\x0c202610211118\x00\x00");
        assert_eq!(stamped_build(&dll), Some(202610211118));
        assert_eq!(stamped_build(b"MZ no stamp here"), None);
        assert_eq!(stamped_build(b"\x0bRecEosBuild\x00"), None);
    }

    #[test]
    fn the_mediafire_link_is_found() {
        let page = r#"<a class="input popsok" aria-label="Download file"
            href="https://download2391.mediafire.com/abc/def/RecEOS-Base-20220211.zip" id="downloadButton" rel="nofollow">"#;
        assert_eq!(
            mediafire_direct_link(page).as_deref(),
            Some("https://download2391.mediafire.com/abc/def/RecEOS-Base-20220211.zip")
        );

        use base64::Engine as _;
        let scrambled = base64::engine::general_purpose::STANDARD
            .encode("https://download1.mediafire.com/x/y/RecEOS-Base-20220211.zip");
        let page = format!(r#"<a id="downloadButton" href="javascript:void(0)" data-scrambled-url="{scrambled}">"#);
        assert_eq!(
            mediafire_direct_link(&page).as_deref(),
            Some("https://download1.mediafire.com/x/y/RecEOS-Base-20220211.zip")
        );

        for bad in [
            r#"<a id="downloadButton" href="https://evil.example/x.zip">"#,
            r#"<a id="downloadButton" href="http://download1.mediafire.com/x.zip">"#,
            r#"<a id="downloadButton" href="https://download1.mediafire.com.evil.example/x.zip">"#,
            r#"<a id="other" href="https://download1.mediafire.com/x.zip">"#,
        ] {
            assert_eq!(mediafire_direct_link(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_client_pin_is_well_formed_once_set() {
        if CLIENT_SHA256 != "PENDING" {
            assert_eq!(CLIENT_SHA256.len(), 64);
            assert!(CLIENT_SHA256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
    }

    /// Hits the live server: `cargo test -- --ignored the_live_release`.
    #[tokio::test]
    #[ignore = "network"]
    async fn the_live_release_verifies() {
        let release = fetch_release().await.unwrap();
        assert!(release.build > 0);
    }
}
