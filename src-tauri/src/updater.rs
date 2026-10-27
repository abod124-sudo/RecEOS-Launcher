use serde_json::json;
use std::env;
use std::process::Command;


const GITHUB_OWNER: &str = "abod124-sudo";
/// Renamed from Radium-Launcher for 4.0.0. A repository under the old name is
/// kept as a bridge: it holds the 4.0.0 release, because every version before
/// it looks only there and refuses installers from any other repository (a
/// renamed repository's API answer names its assets under the new name). Once
/// on 4.0.0, a launcher checks here.
const GITHUB_REPO: &str = "RecEOS-Launcher";

/// Check GitHub for a newer release of the launcher.
///
/// Returns a JSON object with update information including whether an update
/// is available, version strings, and download URLs.
#[tauri::command]
pub async fn check_for_update(app: tauri::AppHandle) -> serde_json::Value {
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/latest",
        GITHUB_OWNER, GITHUB_REPO
    );

    // Same reasoning as `download_update`: this answer names the URL and the
    // digest the installer is then fetched and checked against, so it is not
    // read over a channel a redirect could drop to plain http.
    let client = match reqwest::Client::builder()
        .https_only(true)
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "hasUpdate": false,
                "error": format!("Failed to create HTTP client: {}", e)
            });
        }
    };

    let response = match client
        .get(&url)
        .header("User-Agent", "Radium-Launcher-Updater")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return json!({
                "hasUpdate": false,
                "error": format!("Failed to fetch release info: {}", e)
            });
        }
    };

    // A refusal came back as JSON too — `{"message": "API rate limit
    // exceeded ..."}` — with no `tag_name`, which compared as "no newer
    // version" and told the user they were up to date. Only a 404 means that:
    // the repository has no published release yet.
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return json!({
            "hasUpdate": false,
            "currentVersion": app.package_info().version.to_string(),
        });
    }
    if !status.is_success() {
        let reason = if status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            "GitHub is limiting requests right now; try again later".to_string()
        } else {
            format!("GitHub answered HTTP {}", status.as_u16())
        };
        return json!({ "hasUpdate": false, "error": reason });
    }

    // Capped like every other API reply; see `server::MAX_API_BYTES`.
    let release: serde_json::Value = match crate::server::read_capped(response, crate::server::MAX_API_BYTES)
        .await
        .and_then(|body| serde_json::from_slice(&body).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => {
            return json!({
                "hasUpdate": false,
                "error": format!("Failed to parse release JSON: {}", e)
            });
        }
    };

    let current_version = app.package_info().version.to_string();

    let latest_version = release["tag_name"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let has_update = crate::download::version_gt(&latest_version, &current_version);

    let release_url = release["html_url"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let release_notes = release["body"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // Find the installer asset: name contains "setup" and ends with ".exe" (case-insensitive)
    let asset = release["assets"].as_array().and_then(|assets| {
        assets.iter().find(|asset| {
            if let Some(name) = asset["name"].as_str() {
                let lower = name.to_lowercase();
                lower.contains("setup") && lower.ends_with(".exe")
            } else {
                false
            }
        })
    });

    // The installer's signature, published beside it as `<installer>.sig` by
    // `npm run build` (scripts/sign-update.mjs). Without one the release is
    // offered as a page to download from, as a release with no installer is,
    // and never installed from here: see `download_update`.
    let signature_url = asset
        .and_then(|a| a["name"].as_str())
        .and_then(|name| {
            let sig_name = format!("{name}.sig");
            release["assets"]
                .as_array()?
                .iter()
                .find(|s| s["name"].as_str() == Some(sig_name.as_str()))
        })
        .and_then(|s| s["browser_download_url"].as_str())
        .filter(|url| is_official_release_asset(url))
        .unwrap_or("")
        .to_string();
    let signed = !signature_url.is_empty();

    let download_url = asset
        .and_then(|a| a["browser_download_url"].as_str())
        .filter(|_| signed)
        .unwrap_or("")
        .to_string();

    // GitHub publishes a content digest for release assets as `sha256:<hex>`.
    // It is carried through to `download_update`, which refuses to run an
    // installer whose bytes don't match it — the installer is executed and then
    // elevated by NSIS, so "it came over TLS" is a weaker claim than we want to
    // rely on alone. Empty when the API doesn't provide one.
    let download_digest = asset
        .and_then(|a| a["digest"].as_str())
        .unwrap_or("")
        .to_string();

    // What `download_update` will accept: this installer, checked against this
    // digest — not whatever the page hands back.
    *OFFERED.lock().unwrap_or_else(|e| e.into_inner()) = (has_update && !download_url.is_empty()).then(|| Offer {
        url: download_url.clone(),
        digest: download_digest.clone(),
        signature_url: signature_url.clone(),
    });

    json!({
        "hasUpdate": has_update,
        "currentVersion": current_version,
        "latestVersion": latest_version,
        "releaseUrl": release_url,
        "downloadUrl": download_url,
        "downloadDigest": download_digest,
        // False for a release published without its installer's signature,
        // which is offered as its page instead (`downloadUrl` is empty).
        "signed": signed,
        "releaseNotes": release_notes
    })
}

/// Whether `url` is a release asset of this repository on GitHub.
///
/// Checked on the parsed URL, not the string. A prefix test on the raw string
/// passed `.../releases/download/../../../../other/repo/releases/download/x.exe`,
/// which the URL parser then normalizes into another repository's release, so
/// the "official" installer could have come from anyone's repo. `%2e%2e` is
/// normalized the same way, so comparing after parsing covers both spellings.
fn is_official_release_asset(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return false;
    }
    let Some(segments) = parsed.path_segments() else {
        return false;
    };
    let segments: Vec<&str> = segments.collect();
    // owner / repo / releases / download / <tag> / <asset>
    segments.len() == 6
        && segments[0].eq_ignore_ascii_case(GITHUB_OWNER)
        && segments[1].eq_ignore_ascii_case(GITHUB_REPO)
        && segments[2] == "releases"
        && segments[3] == "download"
        && segments[4..].iter().all(|s| !s.is_empty())
}

/// The installer the last update check offered, the digest GitHub published
/// for it, and where its signature is.
struct Offer {
    url: String,
    digest: String,
    signature_url: String,
}

/// The public half of the key every launcher installer is signed with, as
/// `tauri signer generate` writes it (`radium-launcher-updates.key.pub`): the
/// base64 of a minisign public key file. Key id C37ABF0654950DCF.
///
/// The private half never leaves the developer's PC
/// (`%USERPROFILE%\.tauri\radium-launcher-updates.key`); `npm run build` signs
/// the installer with it (scripts/sign-update.mjs). Everything else about an
/// update — the release, its installer, the digest GitHub publishes — comes
/// from the GitHub account, so anyone who got into that account could publish
/// an installer and a digest that agree. They could not sign it.
///
/// Replacing this key strands every launcher that has the old one: they can
/// no longer update themselves, and need the new version installed by hand.
const UPDATE_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEMzN0FCRjA2NTQ5NTBEQ0YKUldUUERaVlVCcjk2d3o5R25ZYVM5cGYxVDBxMDZMcXJwcjhINENXN0FWVjU0eERBTEVWUHEzRlMK";

/// Largest signature file accepted. A real one is about 400 bytes.
const MAX_SIGNATURE_BYTES: u64 = 4096;

/// Check `installer` against `signature`, the text of its `.sig` file as
/// `tauri signer sign` writes it (base64 of a minisign signature file), and
/// [`UPDATE_PUBLIC_KEY`]. The error is what to tell the user.
fn verify_update_signature(installer: &[u8], signature: &str) -> Result<(), String> {
    use base64::Engine;
    let decode = |text: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    };
    let key = decode(UPDATE_PUBLIC_KEY)
        .and_then(|text| minisign_verify::PublicKey::decode(&text).ok())
        .ok_or("The launcher's update key is unreadable. Download the update from its release page instead.")?;
    let signature = decode(signature)
        .and_then(|text| minisign_verify::Signature::decode(&text).ok())
        .ok_or("The update's signature is unreadable, so it was not installed.")?;
    key.verify(installer, &signature, false).map_err(|_| {
        "The update isn't signed with the launcher's key, so it was not installed. \
         Download it from the release page only if you trust where it came from."
            .to_string()
    })
}

/// Fetch an installer's `.sig` file.
async fn fetch_signature(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Failed to download the update's signature: {}", e))?;
    if !response.status().is_success() {
        return Err(format!("The update's signature couldn't be downloaded (HTTP {}).", response.status().as_u16()));
    }
    let bytes = crate::server::read_capped(response, MAX_SIGNATURE_BYTES)
        .await
        .map_err(|_| "The update's signature is far larger than a signature should be.".to_string())?;
    String::from_utf8(bytes).map_err(|_| "The update's signature is unreadable, so it was not installed.".to_string())
}

/// Set by [`check_for_update`], required by [`download_update`].
///
/// The page used to name both the URL and the digest to check it against. The
/// URL was held to this repository's releases, but any of them would do — an
/// old one included — and the digest was whatever the page sent, or nothing,
/// which skipped the check. Now only the installer the backend itself was just
/// told is the latest can be downloaded, and it is checked against the digest
/// that came with it.
static OFFERED: std::sync::Mutex<Option<Offer>> = std::sync::Mutex::new(None);

/// Largest installer this will download. The real one is around ten
/// megabytes; the body is held in memory while it is checked.
const MAX_INSTALLER_BYTES: u64 = 256 * 1024 * 1024;

/// Filename prefix for the downloaded launcher installer. Each run appends a
/// unique suffix (see [`download_update`]), so old ones accumulate in temp.
const INSTALLER_PREFIX: &str = "RadiumLauncherSetup_update_";

/// Delete installers left behind by previous updates.
///
/// Each update writes a uniquely-named installer to temp and then exits the app
/// to run it, so nothing ever cleaned them up — one abandoned executable per
/// update, forever. Only files matching our own prefix are considered, and only
/// ones older than an hour, so the installer this run is about to write (and any
/// installer a concurrently-updating instance is still executing) is left alone.
fn clean_stale_installers(temp_dir: &std::path::Path) {
    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(INSTALLER_PREFIX) || !name.ends_with(".exe") {
            continue;
        }
        let is_stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|age| age > MAX_AGE)
            .unwrap_or(false);
        if is_stale {
            // Best-effort: a file still locked by a running installer stays.
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Download the update installer and launch it.
///
/// Downloads the file from the given URL to the system temp directory as
/// "RadiumLauncherSetup_update.exe", spawns the installer as a detached process,
/// waits briefly, then exits the current application.
///
/// If `place_on_desktop` is true, the `/DESKTOP` flag is passed to the installer
/// so it re-creates the desktop shortcut even when running in update mode.
#[tauri::command]
pub async fn download_update(
    app: tauri::AppHandle,
    url: String,
    place_on_desktop: bool,
) -> Result<serde_json::Value, String> {
    // Security check: restrict downloads to trusted official release URLs
    if !is_official_release_asset(&url) {
        return Err("Untrusted update download URL.".into());
    }
    // And to the one the last check offered. See [`OFFERED`].
    let (digest, signature_url) = {
        let offered = OFFERED.lock().unwrap_or_else(|e| e.into_inner());
        match offered.as_ref() {
            Some(offer) if offer.url == url => (offer.digest.clone(), offer.signature_url.clone()),
            _ => return Err("That isn't the update the launcher last found. Check for updates again.".into()),
        }
    };
    if !is_official_release_asset(&signature_url) {
        return Err("The update isn't signed, so it can't be installed from here. Download it from its release page instead.".into());
    }

    // Unique per run. A fixed name here is a file another process running as
    // this user can replace in the window between writing the installer and
    // executing it — and what gets executed is an installer, elevated by NSIS.
    // The nanosecond clock plus the pid is enough to make the target
    // unpredictable without pulling in a rand dependency.
    let unique = format!(
        "{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let temp_dir = env::temp_dir();
    clean_stale_installers(&temp_dir);
    let installer_path = temp_dir.join(format!("{}{}.exe", INSTALLER_PREFIX, unique));

    // Download the installer. Without timeouts a stalled connection would
    // leave the update modal stuck on "Downloading..." forever.
    //
    // `https_only` covers the redirects, which `is_official_release_asset`
    // cannot see: GitHub bounces a release asset to its object storage, and
    // without this a hop to plain http would be followed and the bytes could
    // be rewritten in transit. What arrives here is executed, so the transport
    // is held to that even though the signature below is what decides.
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Failed to download update: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Update download failed: HTTP {}", response.status()));
    }
    if response.content_length().is_some_and(|len| len > MAX_INSTALLER_BYTES) {
        return Err("The update is far larger than an installer should be. Nothing was installed.".into());
    }

    let bytes = crate::server::read_capped(response, MAX_INSTALLER_BYTES)
        .await
        .map_err(|e| format!("Failed to read update bytes: {}", e))?;

    // Verify before anything is written where it could be executed. GitHub
    // publishes the asset digest alongside the download URL; when it is
    // present, bytes that don't match it are not an installer we are willing to
    // run. When it is absent — an older API response, a release published
    // before digests existed — this falls through to the signature, which is
    // required and is the check that doesn't rest on the GitHub account.
    if !digest.trim().is_empty() {
        let actual = crate::download::sha256_of(&bytes);
        if !crate::download::digest_matches(&actual, &digest) {
            return Err(format!(
                "The downloaded update does not match the digest GitHub published \
                 for it (expected {}, got {}). Nothing was installed.",
                digest, actual
            ));
        }
    }

    // And the signature, which only the developer's own key can make (see
    // [`UPDATE_PUBLIC_KEY`]). Required, unlike the digest: an installer that
    // doesn't verify is never written where it could be run.
    let signature = fetch_signature(&client, &signature_url).await?;
    verify_update_signature(&bytes, &signature)?;

    // A new file only. The name is unpredictable, but if something did put a
    // file there first, that file is not the one that was just checked.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&installer_path)
            .map_err(|e| format!("Failed to write installer to disk: {}", e))?;
        file.write_all(&bytes)
            .map_err(|e| format!("Failed to write installer to disk: {}", e))?;
    }

    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;

    // Build the installer command, optionally requesting a desktop shortcut
    let mut cmd = Command::new(&installer_path);
    if place_on_desktop {
        cmd.arg("/DESKTOP");
    }

    // Spawn the installer as a detached process
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x00000008) // DETACHED_PROCESS
        .spawn()
        .map_err(|e| format!("Failed to launch installer: {}", e))?;

    #[cfg(not(target_os = "windows"))]
    cmd.spawn()
        .map_err(|e| format!("Failed to launch installer: {}", e))?;

    // Wait briefly to let the installer start, then exit the app
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    app.exit(0);
    Ok(json!({ "success": true }))
}


/// Return the current application version string.
#[tauri::command(async)]
pub fn get_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

#[cfg(test)]
mod signature_tests {
    use super::verify_update_signature;

    /// `tauri signer sign` with the real key over the bytes `hello radium`,
    /// as written to the `.sig` file (2026-10-07).
    const SIGNED: &[u8] = b"hello radium";
    const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUUERaVlVCcjk2dzFHU3JZV1NqQ2pSbk4xTzFtR1RHZHljcTJjeTBTSlI4R0VjRmxsVy9OWTdFMjlDZFY0MFN6QmVDc0s0Ulc1bmgzL3RVUHBhVTFUbzdiNDlaQkNDK3c4PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkxMzkyNTIwCWZpbGU6cmFkaXVtLXNpZy10ZXN0LmJpbgpWVStwLzRraDlNaGR2NGlKN2dmUENJd0FKWXNDdkhFQ3RWSklmWG4rbU5iWW0wVjROZFhoV3l0NkFqR1V6Q1ByaGVpK1l1UE1mK1hrbDdhYmxDRVdDZz09Cg==";

    #[test]
    fn what_the_signer_signed_verifies() {
        assert_eq!(verify_update_signature(SIGNED, SIGNATURE), Ok(()));
        // As it arrives from a file, with its line break.
        assert_eq!(verify_update_signature(SIGNED, &format!("{SIGNATURE}\n")), Ok(()));
    }

    #[test]
    fn anything_else_is_refused() {
        // One byte changed.
        assert!(verify_update_signature(b"hello radiun", SIGNATURE).is_err());
        assert!(verify_update_signature(b"", SIGNATURE).is_err());
        // Not a signature at all, or someone else's.
        assert!(verify_update_signature(SIGNED, "").is_err());
        assert!(verify_update_signature(SIGNED, "not base64 !!").is_err());
        let other = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVSb2JoZmRxTWlVMGlMK3V6VGxUeldvY0pnUzNEa3FDUjlBV29SdmF5U0FBUGNoRkNkdXFJODZmZVlUc0NyelJFZGN1VjJTRG9YMHpMbkZhN1ZPNlNvRlBXUDUxU0l5aWdNPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNjk5OTk5OTk5CWZpbGU6eAo3bmZ3K2U3VjlVWVNZRDFMZ2N0eUd6NmhHM3pJRU5ld0tJSkJFN3Z2QW1OVEdHTnBRQkp6c3lSS0RZTDZFQVNLY2dZR0tXdDJMUzRWa09lckJJb3VEZz09Cg==";
        assert!(verify_update_signature(SIGNED, other).is_err());
    }

    /// Before uploading a release: the installer verifies against its .sig the
    /// way the launcher will check it.
    ///
    /// `RADIUM_INSTALLER="<path to ...-setup.exe>" cargo test -- --ignored a_built_installer`
    #[test]
    #[ignore = "checks a built installer named by RADIUM_INSTALLER"]
    fn a_built_installer_verifies_against_its_signature() {
        let installer = std::env::var("RADIUM_INSTALLER").expect("set RADIUM_INSTALLER to a built -setup.exe");
        let bytes = std::fs::read(&installer).expect("the installer");
        let signature = std::fs::read_to_string(format!("{installer}.sig")).expect("its .sig (npm run sign:update)");
        assert_eq!(verify_update_signature(&bytes, &signature), Ok(()));
    }
}

#[cfg(test)]
mod release_url_tests {
    use super::is_official_release_asset;

    #[test]
    fn a_real_release_asset_is_accepted() {
        assert!(is_official_release_asset(
            "https://github.com/abod124-sudo/RecEOS-Launcher/releases/download/v4.0.0/Rec.EOS.Launcher_4.0.0_x64-setup.exe"
        ));
    }

    #[test]
    fn dot_segments_cannot_walk_into_another_repo() {
        for url in [
            "https://github.com/abod124-sudo/RecEOS-Launcher/releases/download/../../../../evil/repo/releases/download/v1/setup.exe",
            "https://github.com/abod124-sudo/RecEOS-Launcher/releases/download/%2e%2e/%2e%2e/%2e%2e/%2e%2e/evil/repo/releases/download/v1/setup.exe",
            "https://github.com/abod124-sudo/RecEOS-Launcher/releases/download/v1/../../../../../evil/x/releases/download/v1/setup.exe",
        ] {
            assert!(!is_official_release_asset(url), "{url} should be refused");
        }
    }

    #[test]
    fn anything_but_the_repo_on_github_is_refused() {
        for url in [
            "http://github.com/abod124-sudo/RecEOS-Launcher/releases/download/v1/setup.exe",
            "https://github.com.evil.example/abod124-sudo/RecEOS-Launcher/releases/download/v1/setup.exe",
            "https://evil.example@github.com/abod124-sudo/RecEOS-Launcher/releases/download/v1/setup.exe",
            "https://github.com:8443/abod124-sudo/RecEOS-Launcher/releases/download/v1/setup.exe",
            "https://github.com/someone/RecEOS-Launcher/releases/download/v1/setup.exe",
            "https://github.com/abod124-sudo/RecEOS-Launcher/releases/download/v1/",
            "https://github.com/abod124-sudo/RecEOS-Launcher/archive/v1.zip",
            // The bridge repository under the old name is for older versions only.
            "https://github.com/abod124-sudo/Radium-Launcher/releases/download/v4.0.0/setup.exe",
            "not a url",
        ] {
            assert!(!is_official_release_asset(url), "{url} should be refused");
        }
    }
}

#[cfg(test)]
mod installer_cleanup_tests {
    use super::{clean_stale_installers, INSTALLER_PREFIX};
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radium-updater-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Backdate a file so the age check sees it as stale.
    fn age(path: &std::path::Path, secs: u64) {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        let f = fs::File::options().write(true).open(path).expect("open");
        f.set_modified(when).expect("set mtime");
    }

    #[test]
    fn a_stale_installer_is_removed() {
        let dir = temp_dir("stale");
        let stale = dir.join(format!("{}12345_678.exe", INSTALLER_PREFIX));
        fs::write(&stale, b"stub").expect("write");
        age(&stale, 60 * 60 * 24);

        clean_stale_installers(&dir);
        assert!(!stale.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    /// The installer this run is about to write, and one a concurrently
    /// updating instance may still be executing, are both recent.
    #[test]
    fn a_recent_installer_is_left_alone() {
        let dir = temp_dir("recent");
        let fresh = dir.join(format!("{}999_111.exe", INSTALLER_PREFIX));
        fs::write(&fresh, b"stub").expect("write");

        clean_stale_installers(&dir);
        assert!(fresh.exists(), "a running installer must not be deleted");

        let _ = fs::remove_dir_all(&dir);
    }

    /// Temp is shared with the rest of the system, so nothing outside our own
    /// naming scheme may be touched however old it is.
    #[test]
    fn unrelated_temp_files_are_never_touched() {
        let dir = temp_dir("unrelated");
        let others = [
            "important.exe",
            "RadiumLauncherSetup.exe",
            "SomeOtherApp_update_1.exe",
        ];
        for name in others {
            let p = dir.join(name);
            fs::write(&p, b"stub").expect("write");
            age(&p, 60 * 60 * 24 * 30);
        }
        // Our prefix but not an executable.
        let log = dir.join(format!("{}1_2.log", INSTALLER_PREFIX));
        fs::write(&log, b"stub").expect("write");
        age(&log, 60 * 60 * 24 * 30);

        clean_stale_installers(&dir);

        for name in others {
            assert!(dir.join(name).exists(), "{} must survive", name);
        }
        assert!(log.exists(), "only .exe files match");

        let _ = fs::remove_dir_all(&dir);
    }
}
