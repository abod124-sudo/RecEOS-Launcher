//! One-click GE-Proton, from Settings › Proton on Linux.
//!
//! Valve's Proton can't decode H.264, so Rec EOS's loading banner (an MP4)
//! plays Proton's colour-bars placeholder; GE-Proton ships the codecs. The
//! version is pinned rather than "latest": GE-Proton11-12 is built on Proton
//! 11, the Proton Rec EOS was confirmed to start under, and its archive is
//! checked against a SHA-512 written here, so a replaced release asset is
//! refused instead of unpacked. To move to a newer GE-Proton, change all five
//! constants together (the hash is in the release's `.sha512sum` asset).
//!
//! It goes where Steam keeps custom tools, `<Steam>/compatibilitytools.d`
//! (Flatpak Steam's too), so Steam lists it as well. Automatic then picks it
//! (see `automatic_proton` in linux.rs). Nothing is replaced: an existing
//! folder of the same name is left alone.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use tauri::Emitter;

pub const GE_PROTON_TAG: &str = "GE-Proton11-12";
/// The archive's top folder; installed under [`GE_PROTON_TAG`].
const ARCHIVE_DIR: &str = "GE-Proton11-12-x86_64";
const ARCHIVE_URL: &str = "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/GE-Proton11-12/GE-Proton11-12-x86_64.tar.gz";
const ARCHIVE_SIZE: u64 = 566_859_228;
const ARCHIVE_SHA512: &str = "2e37447902529c9fc39138dc0b3f24aed3513b59a190b825e6b4a668e49e2033bdf6b8a7998e8686e067bbb3f61f3a6633e2408f0cc96f6dad983bd18e2db31e";

/// No data for this long fails the download instead of hanging.
const STALL: std::time::Duration = std::time::Duration::from_secs(60);

static INSTALLING: AtomicBool = AtomicBool::new(false);

struct Busy;
impl Drop for Busy {
    fn drop(&mut self) {
        INSTALLING.store(false, Ordering::SeqCst);
    }
}

/// The working folder beside the destination, removed on every way out.
struct Work(PathBuf);
impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn progress(app: &tauri::AppHandle, phase: &str, done: u64) {
    let _ = app.emit(
        "ge-proton-progress",
        json!({ "phase": phase, "downloaded": done, "total": ARCHIVE_SIZE }),
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Installs the pinned GE-Proton into the Steam that Settings › Proton uses
/// and returns its name. Already installed is success.
#[tauri::command]
pub async fn install_ge_proton(app: tauri::AppHandle) -> Result<String, String> {
    if !cfg!(target_os = "linux") || std::env::consts::ARCH != "x86_64" {
        return Err("GE-Proton can be installed on 64-bit Linux only.".into());
    }
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("GE-Proton is already being installed.".into());
    }
    let _busy = Busy;
    let steam = crate::linux::steam_folder(&app)?;
    let tools = steam.join("compatibilitytools.d");
    let target = tools.join(GE_PROTON_TAG);
    if target.join("proton").is_file() {
        return Ok(GE_PROTON_TAG.into());
    }
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err(format!(
            "{} already exists but isn't a complete GE-Proton. Remove it, then try again.",
            target.display()
        ));
    }
    std::fs::create_dir_all(&tools)
        .map_err(|e| format!("Can't create Steam's compatibilitytools.d folder: {e}"))?;
    // Beside the destination, so the final move is a rename on one disk.
    let work = Work(tools.join(format!(".receos-{GE_PROTON_TAG}-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&work.0);
    std::fs::create_dir(&work.0).map_err(|e| format!("Can't prepare the download folder: {e}"))?;
    let archive = work.0.join("archive.tar.gz");

    download(&app, &archive).await?;

    progress(&app, "unpack", ARCHIVE_SIZE);
    let unpacked = work.0.join("unpacked");
    std::fs::create_dir(&unpacked).map_err(|e| e.to_string())?;
    let status = {
        let (archive, unpacked) = (archive.clone(), unpacked.clone());
        tokio::task::spawn_blocking(move || {
            crate::linux::system_command("tar")
                .arg("--no-same-owner")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(&unpacked)
                .status()
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Couldn't run tar to unpack GE-Proton: {e}"))?
    };
    if !status.success() {
        return Err("GE-Proton couldn't be unpacked. Check there's about 1.5 GB free, then try again.".into());
    }
    let _ = std::fs::remove_file(&archive);
    let tool = unpacked.join(ARCHIVE_DIR);
    if !complete(&tool) {
        return Err("The GE-Proton download didn't contain the expected files.".into());
    }
    std::fs::rename(&tool, &target)
        .map_err(|e| format!("Couldn't move GE-Proton into place: {e}"))?;
    progress(&app, "done", ARCHIVE_SIZE);
    Ok(GE_PROTON_TAG.into())
}

fn complete(tool: &Path) -> bool {
    tool.join("proton").is_file() && tool.join("toolmanifest.vdf").is_file()
}

async fn download(app: &tauri::AppHandle, archive: &Path) -> Result<(), String> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha512};
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Couldn't start the download: {e}"))?;
    let response = client
        .get(ARCHIVE_URL)
        .header("User-Agent", "RecEOS-Launcher")
        .send()
        .await
        .map_err(|e| format!("Couldn't reach GitHub to download GE-Proton: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("GitHub refused the GE-Proton download ({}).", response.status()));
    }
    if response.content_length().is_some_and(|n| n != ARCHIVE_SIZE) {
        return Err("The GE-Proton download isn't the expected file; nothing was installed.".into());
    }
    let mut file = tokio::fs::File::create(archive)
        .await
        .map_err(|e| format!("Can't write the GE-Proton download: {e}"))?;
    let mut hash = Sha512::new();
    let mut received: u64 = 0;
    let mut last_emit = std::time::Instant::now();
    let mut stream = response.bytes_stream();
    progress(app, "download", 0);
    loop {
        let chunk = match tokio::time::timeout(STALL, stream.next()).await {
            Ok(Some(chunk)) => chunk.map_err(|e| format!("The GE-Proton download was interrupted: {e}"))?,
            Ok(None) => break,
            Err(_) => return Err("The GE-Proton download stalled. Check your connection and try again.".into()),
        };
        received += chunk.len() as u64;
        if received > ARCHIVE_SIZE {
            return Err("The GE-Proton download is larger than expected; nothing was installed.".into());
        }
        hash.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("Can't write the GE-Proton download (is the disk full?): {e}"))?;
        if last_emit.elapsed() >= std::time::Duration::from_millis(250) {
            progress(app, "download", received);
            last_emit = std::time::Instant::now();
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    if received != ARCHIVE_SIZE || hex(&hash.finalize()) != ARCHIVE_SHA512 {
        return Err("The GE-Proton download didn't match its checksum, so it wasn't installed. Try again.".into());
    }
    progress(app, "download", received);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_release_is_consistent() {
        assert!(ARCHIVE_URL.starts_with("https://github.com/GloriousEggroll/proton-ge-custom/releases/download/"));
        assert!(ARCHIVE_URL.contains(&format!("/{GE_PROTON_TAG}/{ARCHIVE_DIR}.tar.gz")));
        assert_eq!(ARCHIVE_DIR, format!("{GE_PROTON_TAG}-x86_64"));
        assert_eq!(ARCHIVE_SHA512.len(), 128);
        assert!(ARCHIVE_SHA512.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(hex(&[0x00, 0xab, 0x0f]), "00ab0f");
    }
}
