//! Stella: where its client and patch come from, which patch is trusted, and
//! how the patch gets into the game.
//!
//! Stella ships the stock 2024 Rec Room client plus `Stella2024.dll`, a small
//! patch that hooks `GameAssembly.dll` to send the game to Stella's servers
//! instead of Rec Room's. Nothing in the client loads that DLL — it has no
//! exports and is not a proxy for any DLL the game imports — so, as Stella's
//! own launcher does, this one starts `RecRoom.exe` and loads the patch into
//! it once the game code is in memory.
//!
//! The patch is unsigned and served from a "latest" URL that can change at any
//! time, so where it came from proves nothing. Trust is decided by its SHA-256
//! instead:
//!
//! * a build in [`PINNED_PATCH_SHA256`] was checked by hand and is installed
//!   without asking, alongside the client;
//! * any other build is offered as an UPDATE on Home, and is only installed
//!   when the user presses it;
//! * at launch the file on disk must still hash to the build the user
//!   accepted (`config.stella.patchSha256`), or nothing is injected.
//!
//! Both downloads are refused by Stella's Cloudflare without their launcher's
//! User-Agent, so every request here sends [`USER_AGENT`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::Emitter;

use crate::config;
use crate::download::sha256_of;

/// The full Windows client, a zip of about 4.7 GB (8.6 GB extracted). Served
/// chunked, with no length, ETag or range support.
pub const CLIENT_URL: &str = "https://api.stellaonline.org/download/latestwindowsbuild";

/// The current patch DLL.
pub const PATCH_URL: &str = "https://api.stellaonline.org/download/latestpcpatch";

/// Stella's Cloudflare lets downloads through only with this User-Agent.
pub const USER_AGENT: &str = "Stella Launcher/1.0";

/// Patch builds checked by hand, lowercase hex SHA-256. These install without
/// a prompt; anything else waits for the user to press UPDATE.
///
/// `e11367b7…` is the build served on 2026-10-01: a 1,368,576-byte x64 DLL
/// with no exports that imports only KERNEL32 and hooks GameAssembly.dll with
/// MinHook. Add a new build's hash here once it has been looked at, e.g. with
/// `certutil -hashfile Stella2024.dll SHA256`.
pub const PINNED_PATCH_SHA256: &[&str] =
    &["e11367b7d8fbb9fed83e3874bd087beb862a303f32040fdb57b80a8157fc4574"];

/// Name the patch is stored under, in `<app data>/stella/`.
const PATCH_FILE: &str = "Stella2024.dll";

/// Largest patch this will accept. The real one is 1.3 MB.
const MAX_PATCH_BYTES: u64 = 16 * 1024 * 1024;

/// The event the UPDATE progress bar listens to.
const PROGRESS_EVENT: &str = "stella-patch-progress";

/// One patch update at a time.
static UPDATING: AtomicBool = AtomicBool::new(false);

/// The patch as the last check that offered an update downloaded it, and
/// when.
///
/// Stella serves the patch with no ETag, no Last-Modified and no byte ranges
/// (measured 2026-10-07), so nothing short of the whole file says whether it
/// has changed, and every check downloads it. UPDATE, pressed after the check
/// that offered it, installs these same bytes rather than fetching the file a
/// second time, which also makes what it installs exactly the build that check
/// reported. Kept only while an update is on offer, and taken by the install
/// that uses it.
static OFFERED: Mutex<Option<(Instant, Vec<u8>)>> = Mutex::new(None);

/// How long a check's download stands in for a fresh one.
const OFFER_FRESH: Duration = Duration::from_secs(10 * 60);

struct UpdatingGuard;
impl Drop for UpdatingGuard {
    fn drop(&mut self) {
        UPDATING.store(false, Ordering::SeqCst);
    }
}

pub fn is_pinned(sha256: &str) -> bool {
    PINNED_PATCH_SHA256
        .iter()
        .any(|pin| crate::download::digest_matches(sha256, pin))
}

/// Where the accepted patch lives. Outside the client folder on purpose, so a
/// reinstall of the client can't lose it and a folder picked by the user never
/// holds a file the launcher injects.
pub fn patch_path(app: &tauri::AppHandle) -> PathBuf {
    config::stella_patch_dir(&config::app_data_dir(app)).join(PATCH_FILE)
}

/// Whether `bytes` is a 64-bit Windows DLL — the only thing that can load into
/// the 64-bit game. Catches an HTML error page, a truncated file or a wrong
/// download before anything is written.
pub fn looks_like_x64_dll(bytes: &[u8]) -> bool {
    const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
    const IMAGE_FILE_DLL: u16 = 0x2000;
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    if !bytes.starts_with(b"MZ") {
        return false;
    }
    let Some(pe) = u32_at(0x3c).map(|v| v as usize) else {
        return false;
    };
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0".as_slice()) {
        return false;
    }
    u16_at(pe + 4) == Some(IMAGE_FILE_MACHINE_AMD64)
        && u16_at(pe + 22).is_some_and(|c| c & IMAGE_FILE_DLL != 0)
}

/// Download the current patch, reporting `(downloaded, total)` as it arrives
/// (`total` is 0 when the server doesn't say).
async fn fetch_patch(mut on_progress: impl FnMut(u64, u64)) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;

    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(PATCH_URL)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| crate::stella_api::unreachable_message(&e))?;
    let status = response.status();
    if !status.is_success() {
        let body = crate::server::read_capped(response, 64 * 1024).await.unwrap_or_default();
        crate::applog::backend(
            "error",
            "install",
            format!("Stella's patch server answered GET {PATCH_URL} with HTTP {status}{}", crate::stella_api::quoted_reply(&body)),
        );
        return Err(format!("Stella's patch server answered HTTP {}.", status.as_u16()));
    }
    let total = response.content_length().unwrap_or(0);
    if total > MAX_PATCH_BYTES {
        return Err("Stella's patch is far larger than expected, so it was not downloaded.".into());
    }

    let mut bytes = Vec::with_capacity(total as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("The patch download failed: {}", e))?;
        if bytes.len() as u64 + chunk.len() as u64 > MAX_PATCH_BYTES {
            return Err("Stella's patch is far larger than expected, so it was not downloaded.".into());
        }
        bytes.extend_from_slice(&chunk);
        on_progress(bytes.len() as u64, total);
    }
    if total > 0 && bytes.len() as u64 != total {
        return Err("The patch download ended early.".into());
    }
    if !looks_like_x64_dll(&bytes) {
        return Err("Stella's server sent something that is not a 64-bit Windows DLL.".into());
    }
    Ok(bytes)
}

/// Write the patch over the stored one: to a temporary file first, then
/// renamed over, so a failed write never leaves half a DLL to inject.
fn store_patch(app: &tauri::AppHandle, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let path = patch_path(app);
    let dir = path.parent().ok_or("No folder for the patch.")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Couldn't create {}: {}", dir.display(), e))?;
    let tmp = dir.join(format!("{}.download", PATCH_FILE));
    {
        let mut file =
            std::fs::File::create(&tmp).map_err(|e| format!("Couldn't write the patch: {}", e))?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("Couldn't write the patch: {}", e))?;
    }
    // An antivirus scanning the old file can hold it for a moment.
    let mut attempt = 0;
    loop {
        match std::fs::rename(&tmp, &path) {
            Ok(()) => return Ok(()),
            Err(e) if attempt >= 4 => {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("Couldn't replace the patch: {}", e));
            }
            Err(_) => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

/// Store `bytes` as the accepted patch and record its hash.
fn accept_patch(app: &tauri::AppHandle, bytes: &[u8]) -> Result<String, String> {
    let sha = sha256_of(bytes);
    store_patch(app, bytes)?;
    config::update(app, |cfg| cfg.stella.patch_sha256 = sha.clone())?;
    Ok(sha)
}

/// Whether the stored patch is present and still the build the user accepted.
fn stored_patch_matches(path: &Path, accepted: &str) -> bool {
    if accepted.is_empty() {
        return false;
    }
    std::fs::read(path).is_ok_and(|bytes| crate::download::digest_matches(&sha256_of(&bytes), accepted))
}

/// After the client is installed: fetch the patch, and install it at once if
/// it is a pinned build. Returns whether it was installed; a patch that isn't
/// pinned, or couldn't be fetched, is left for the UPDATE button.
pub async fn adopt_pinned_patch(app: &tauri::AppHandle) -> Result<bool, String> {
    let bytes = fetch_patch(|_, _| {}).await?;
    if !is_pinned(&sha256_of(&bytes)) {
        return Ok(false);
    }
    accept_patch(app, &bytes)?;
    Ok(true)
}

/// Whether Stella's patch needs installing or has a new build.
///
/// Returns `{ success, installed, updateAvailable, latestSha256,
/// installedSha256, latestPinned }`. When Stella can't be reached,
/// `success` is false and `updateAvailable` says only whether the patch is
/// missing — which it would need to be to play at all.
#[tauri::command]
pub async fn stella_patch_status(app: tauri::AppHandle) -> Value {
    let accepted = config::current(&app).stella.patch_sha256.clone();
    let path = patch_path(&app);
    let check_path = path.clone();
    let check_accepted = accepted.clone();
    let installed = tokio::task::spawn_blocking(move || stored_patch_matches(&check_path, &check_accepted))
        .await
        .unwrap_or(false);

    match fetch_patch(|_, _| {}).await {
        Ok(bytes) => {
            let latest = sha256_of(&bytes);
            let update_available = !installed || !crate::download::digest_matches(&latest, &accepted);
            if let Ok(mut offered) = OFFERED.lock() {
                *offered = update_available.then(|| (Instant::now(), bytes));
            }
            json!({
                "success": true,
                "installed": installed,
                "updateAvailable": update_available,
                "latestSha256": latest,
                "installedSha256": accepted,
                "latestPinned": is_pinned(&latest),
            })
        }
        Err(e) => json!({
            "success": false,
            "installed": installed,
            "updateAvailable": !installed,
            "installedSha256": accepted,
            "error": e,
        }),
    }
}

/// Install the current patch: what the UPDATE button runs. Pressing it is the
/// user accepting that build, pinned or not.
///
/// Emits `stella-patch-progress` `{ phase, pct }` for the progress bar.
#[tauri::command]
pub async fn stella_update_patch(app: tauri::AppHandle) -> Value {
    match update_patch(&app).await {
        Ok(sha) => json!({ "success": true, "sha256": sha, "pinned": is_pinned(&sha) }),
        Err(e) => {
            let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "error", "pct": 0 }));
            json!({ "success": false, "error": e })
        }
    }
}

async fn update_patch(app: &tauri::AppHandle) -> Result<String, String> {
    if UPDATING.swap(true, Ordering::SeqCst) {
        return Err("An update is already running.".into());
    }
    let _guard = UpdatingGuard;
    // The running game holds the DLL open, and swapping it underneath would
    // change what the next launch injects mid-session anyway.
    if crate::game::game_running(app) {
        return Err("Close the game before updating.".into());
    }

    let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "download", "pct": 0 }));
    // The build the check that offered this update downloaded, if it was a
    // moment ago (see OFFERED); else the current one, fetched now.
    let offered = OFFERED
        .lock()
        .ok()
        .and_then(|mut offered| offered.take())
        .filter(|(at, _)| at.elapsed() < OFFER_FRESH)
        .map(|(_, bytes)| bytes);
    let bytes = match offered {
        Some(bytes) => bytes,
        None => {
            let mut last_pct = -1i64;
            fetch_patch(|done, total| {
                if total == 0 {
                    return;
                }
                // Held below 100 until the file is in place.
                let pct = ((done as f64 / total as f64) * 99.0) as i64;
                if pct != last_pct {
                    last_pct = pct;
                    let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "download", "pct": pct }));
                }
            })
            .await?
        }
    };

    let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "install", "pct": 99 }));
    let app_for_write = app.clone();
    let sha = tokio::task::spawn_blocking(move || accept_patch(&app_for_write, &bytes))
        .await
        .map_err(|e| format!("The update failed: {}", e))??;
    let _ = app.emit(PROGRESS_EVENT, json!({ "phase": "done", "pct": 100 }));
    Ok(sha)
}

/// Remove the stored patch (uninstall).
pub fn remove_patch(app: &tauri::AppHandle) {
    let _ = std::fs::remove_file(patch_path(app));
}

// ─── Launch ─────────────────────────────────────────────────────────────────

/// Open `path` for reading, shared for reading only: while the handle is open,
/// nothing else can write, rename or delete the file, though anyone may still
/// read it, which is all `LoadLibraryW` does.
fn open_locked(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        options.share_mode(FILE_SHARE_READ);
    }
    options.open(path)
}

/// Read the accepted patch and confirm it is still the build the user
/// accepted. Returns its path, and the file held open (see [`open_locked`]) so
/// it stays that build until the game has loaded it: checked and then loaded
/// by path up to two minutes later, as the game starts, it could have been
/// swapped in between.
fn verified_patch(app: &tauri::AppHandle) -> Result<(PathBuf, std::fs::File), String> {
    use std::io::Read;

    let accepted = config::current(app).stella.patch_sha256.clone();
    let path = patch_path(app);
    if accepted.is_empty() || !path.exists() {
        return Err("Stella's patch isn't installed. Press UPDATE on Home to get it.".into());
    }
    let mut file = open_locked(&path).map_err(|e| format!("Couldn't open Stella's patch: {}", e))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|e| format!("Couldn't read Stella's patch: {}", e))?;
    if !crate::download::digest_matches(&sha256_of(&bytes), &accepted) {
        return Err("Stella's patch file has changed since it was installed, so it was not \
                    loaded. Press UPDATE on Home to download it again."
            .into());
    }
    Ok((path, file))
}

/// Start the Stella client with its patch loaded. Returns the game's pid.
///
/// The patch goes in once `GameAssembly.dll` is in memory: that is the module
/// it hooks, and it is loaded before any of the game's own code runs. If the
/// patch can't be loaded the game is closed again, because an unpatched client
/// would talk to Rec Room's servers rather than Stella's.
#[cfg(target_os = "windows")]
pub fn launch(
    app: &tauri::AppHandle,
    exe: &Path,
    work_dir: &str,
    args: &[&str],
) -> Result<u32, String> {
    use std::os::windows::process::CommandExt;

    // `_held` keeps the patch as it was checked until this returns.
    let (patch, _held) = verified_patch(app)?;
    let mut child = std::process::Command::new(exe)
        .args(args)
        .current_dir(work_dir)
        .creation_flags(0x00000008) // DETACHED_PROCESS
        .spawn()
        .map_err(|e| format!("Failed to launch game executable: {}", e))?;

    match inject::load_when_ready(&mut child, &patch) {
        Ok(()) => Ok(child.id()),
        Err(e) => {
            let _ = child.kill();
            Err(format!("Stella's patch couldn't be loaded, so the game was closed. {}", e))
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn launch(
    _app: &tauri::AppHandle,
    _exe: &Path,
    _work_dir: &str,
    _args: &[&str],
) -> Result<u32, String> {
    Err("Stella can only be launched on Windows.".into())
}

#[cfg(target_os = "linux")]
pub fn launch(app: &tauri::AppHandle, exe: &Path, work_dir: &str, args: &[&str]) -> Result<u32, String> {
    use std::io::Write;
    use std::process::Stdio;
    use tauri::Manager;
    let (patch, _held) = verified_patch(app)?;
    let cfg = crate::config::current(app);
    let runtime = crate::linux::resolve(app, crate::config::Network::Stella)?;
    if !crate::linux::steam_running() { return Err("Start native Linux Steam and sign in before playing Stella.".into()); }
    let helper = app.path().resource_dir().map_err(|e| e.to_string())?.join("receos-proton-helper.exe");
    #[cfg(debug_assertions)]
    let helper = if helper.is_file() { helper } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/receos-proton-helper.exe")
    };
    if !helper.is_file() { return Err("The Proton helper is missing. Reinstall the Linux package, or prepare the helper before a development build.".into()); }
    let request = json!({"version": 1, "exe": runtime.windows_path(exe)?,
        "patch": runtime.windows_path(&patch)?, "sha256": cfg.stella.patch_sha256, "args": args});
    let mut cmd = runtime.command(crate::linux::LAUNCH_VERB)?;
    runtime.expose_paths(&mut cmd, &[Path::new(work_dir), patch.parent().ok_or("Patch directory missing")?])?;
    cmd.env("STEAM_COMPAT_INSTALL_PATH", work_dir);
    let helper = crate::linux::HelperFiles(crate::linux::stage_helper(&runtime, &helper)?);
    let directory = helper.0.parent().ok_or("Helper directory missing")?;
    let input = directory.join("request.json");
    let output = directory.join("response.json");
    let mut options = std::fs::OpenOptions::new();
    use std::os::unix::fs::OpenOptionsExt;
    options.write(true).create_new(true).mode(0o600);
    options.open(&input).and_then(|mut file| file.write_all(request.to_string().as_bytes()))
        .map_err(|e| format!("Cannot prepare the Proton helper request: {e}"))?;
    // Proton redirects Windows stdout and its Steam shim can detach stdin.
    // Exchange bounded files in a fresh private directory instead of pipes.
    cmd.arg(&helper.0).args(["--request-file", &runtime.windows_path(&input)?,
        "--response-file", &runtime.windows_path(&output)?])
        .current_dir(work_dir).stdin(Stdio::null()).stdout(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("Could not start the Proton helper: {e}"))?;
    let result = (|| -> Result<(), String> {
        let started = std::time::Instant::now();
        let reply = loop {
            if let Some(reply) = crate::linux::helper_reply(&output)? { break reply }
            if started.elapsed() > std::time::Duration::from_secs(180)
                || child.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Err("Stella's Proton helper timed out or exited without confirming its patch.".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        if reply["version"] != 1 || reply["success"] != true {
            return Err(reply["error"].as_str().unwrap_or("Stella patch loading failed.").to_string());
        }
        Ok(())
    })();
    if let Err(error) = result {
        crate::linux::stop_helper(&runtime, &helper.0);
        let _ = child.kill();
        crate::linux::reap(child);
        crate::linux::stop_games();
        return Err(format!("Stella's patch could not be loaded, so the game was closed. {error}"));
    }
    crate::linux::reap(child);
    // The helper's Windows PID is not a host PID. Linux's monitor resolves
    // the actual process from the verified client path and selected prefix.
    Ok(0)
}

#[cfg(target_os = "windows")]
#[path = "stella_inject.rs"]
mod inject;

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE header: `MZ`, e_lfanew at 0x3c, then `PE\0\0`, machine and
    /// characteristics at their offsets.
    fn pe(machine: u16, characteristics: u16) -> Vec<u8> {
        let mut b = vec![0u8; 0x100];
        b[0] = b'M';
        b[1] = b'Z';
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        b[0x96..0x98].copy_from_slice(&characteristics.to_le_bytes());
        b
    }

    #[test]
    fn only_a_64_bit_dll_passes() {
        assert!(looks_like_x64_dll(&pe(0x8664, 0x2022)));
        // An x86 DLL can't load into the 64-bit game.
        assert!(!looks_like_x64_dll(&pe(0x014c, 0x2102)));
        // An executable rather than a DLL.
        assert!(!looks_like_x64_dll(&pe(0x8664, 0x0022)));
        // A Cloudflare page, or nothing at all.
        assert!(!looks_like_x64_dll(b"<!DOCTYPE html><html>"));
        assert!(!looks_like_x64_dll(b""));
        // e_lfanew pointing past the end must not panic.
        let mut truncated = pe(0x8664, 0x2022);
        truncated[0x3c..0x40].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(!looks_like_x64_dll(&truncated));
    }

    #[test]
    fn pins_match_case_insensitively() {
        assert!(is_pinned(PINNED_PATCH_SHA256[0]));
        assert!(is_pinned(&PINNED_PATCH_SHA256[0].to_uppercase()));
        assert!(!is_pinned(""));
        assert!(!is_pinned(&"0".repeat(64)));
    }

    /// While the checked patch is held, nothing can write, rename or delete
    /// it, and Windows still loads it as a DLL, which is what the game does.
    #[cfg(windows)]
    #[test]
    fn a_held_patch_cant_be_swapped_but_still_loads() {
        use windows_sys::Win32::Foundation::FreeLibrary;
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

        let dir = std::env::temp_dir().join(format!("stella-held-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Any real DLL will do. A name of its own, so it isn't taken for the
        // system copy already loaded.
        let dll = dir.join("radium-held-patch-test.dll");
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        std::fs::copy(Path::new(&system).join("System32").join("msimg32.dll"), &dll).unwrap();

        let held = open_locked(&dll).unwrap();
        assert!(std::fs::OpenOptions::new().write(true).open(&dll).is_err(), "written while held");
        assert!(std::fs::rename(&dll, dir.join("swapped.dll")).is_err(), "renamed while held");
        assert!(std::fs::remove_file(&dll).is_err(), "deleted while held");
        assert!(std::fs::read(&dll).is_ok(), "read while held");

        let wide: Vec<u16> = dll.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: a NUL-terminated path to a system DLL's copy, freed below.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        assert!(!module.is_null(), "loads while held: {}", std::io::Error::last_os_error());
        // SAFETY: the module loaded just above, and nothing of it is used.
        unsafe { FreeLibrary(module) };

        drop(held);
        let _ = std::fs::remove_file(&dll);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn a_changed_patch_file_is_not_the_accepted_one() {
        let dir = std::env::temp_dir().join(format!("stella-patch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PATCH_FILE);
        std::fs::write(&path, b"accepted build").unwrap();
        let accepted = sha256_of(b"accepted build");

        assert!(stored_patch_matches(&path, &accepted));
        // Nothing accepted yet: not even a matching file counts.
        assert!(!stored_patch_matches(&path, ""));
        std::fs::write(&path, b"swapped build").unwrap();
        assert!(!stored_patch_matches(&path, &accepted));
        std::fs::remove_file(&path).unwrap();
        assert!(!stored_patch_matches(&path, &accepted));
        let _ = std::fs::remove_dir(&dir);
    }
}
