//! Native Linux integration. Proton prefixes belong to this launcher, not to
//! Steam's installed games; uninstalling a client never removes its prefix.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{self, Network};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProtonSettings {
    pub proton_dir: String,
    pub compat_data_dir: String,
    /// Run Direct3D through WineD3D (OpenGL) instead of DXVK (Vulkan), for
    /// systems without a Vulkan 1.3 driver. Slower, but starts where DXVK can't.
    pub opengl: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LinuxSettings {
    pub steam_dir: String,
    pub networks: BTreeMap<String, ProtonSettings>,
}

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

pub fn init(app: tauri::AppHandle) {
    let _ = APP.set(app);
}

pub fn network_name(network: Network) -> &'static str {
    match network {
        Network::Radium => "radium",
        Network::RecEos => "receos",
        Network::Stella => "stella",
        Network::Vanilla => "vanilla",
    }
}

#[tauri::command]
pub fn get_platform_capabilities() -> Value {
    json!({"os": std::env::consts::OS, "linux": cfg!(target_os = "linux"),
        "windowsSecurity": cfg!(windows), "vanillaAutomation": cfg!(windows),
        "protonRequired": cfg!(target_os = "linux"), "linuxGameplayValidated": false})
}

#[tauri::command]
pub async fn pick_linux_directory() -> Option<String> {
    rfd::AsyncFileDialog::new()
        .pick_folder()
        .await
        .map(|p| p.path().to_string_lossy().to_string())
}

fn unbundle_environment(command: &mut Command) {
    for key in [
        "LD_LIBRARY_PATH",
        "LD_PRELOAD",
        "GTK_PATH",
        "GTK_EXE_PREFIX",
        "GTK_DATA_PREFIX",
        "GIO_EXTRA_MODULES",
        "GSETTINGS_SCHEMA_DIR",
        "GI_TYPELIB_PATH",
        "GDK_PIXBUF_MODULE_FILE",
        "GDK_PIXBUF_MODULEDIR",
        "WEBKIT_EXEC_PATH",
        "GST_PLUGIN_PATH",
        "GST_PLUGIN_SYSTEM_PATH",
        "GST_PLUGIN_SCANNER",
        "QT_PLUGIN_PATH",
        "QT_QPA_PLATFORM_PLUGIN_PATH",
        "PYTHONHOME",
        "PYTHONPATH",
    ] {
        command.env_remove(key);
    }
}

/// AppRun's libraries are for this application, not host tools built against
/// a newer GLib. Its multimedia hook also redirects Python's standard library,
/// which breaks Proton's interpreter. Retain display, D-Bus and XDG variables.
pub(crate) fn system_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    if std::env::var_os("APPDIR").is_some() || std::env::var_os("APPIMAGE").is_some() {
        unbundle_environment(&mut command);
    }
    command
}

#[cfg(target_os = "linux")]
pub fn tray_host_available() -> bool {
    use gio::glib::variant::ToVariant;
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else {
        return false;
    };
    // Constructing an AppIndicator succeeds even without a panel. Require a
    // registered host before hiding, and recheck so a late-starting panel or
    // a disappearing desktop host is handled on the next close request.
    bus.call_sync(
        Some("org.kde.StatusNotifierWatcher"),
        "/StatusNotifierWatcher",
        "org.freedesktop.DBus.Properties",
        "Get",
        Some(
            &(
                "org.kde.StatusNotifierWatcher",
                "IsStatusNotifierHostRegistered",
            )
                .to_variant(),
        ),
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        500,
        None::<&gio::Cancellable>,
    )
    .ok()
    .and_then(|value| value.get::<(gio::glib::Variant,)>())
    .and_then(|(value,)| value.get::<bool>())
    .unwrap_or(false)
}

fn allowed_linux_url(url: &str) -> bool {
    if url.len() > 4096 || url.chars().any(char::is_control) {
        return false;
    }
    if url == "steam://" {
        return true;
    }
    if let Some(id) = url.strip_prefix("steam://install/") {
        return !id.is_empty() && id.bytes().all(|c| c.is_ascii_digit());
    }
    tauri::Url::parse(url).is_ok_and(|parsed| {
        parsed.scheme() == "https"
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
    })
}

#[tauri::command(async)]
pub fn open_linux_url(url: String) -> Result<(), String> {
    if !cfg!(target_os = "linux") || !allowed_linux_url(&url) {
        return Err("Only HTTPS websites and approved Steam links can be opened on Linux.".into());
    }
    let child = system_command("xdg-open")
        .arg(url)
        .spawn()
        .map_err(|e| format!("Could not open the link. Install xdg-utils: {e}"))?;
    reap(child);
    Ok(())
}

// Steam's VDF files escape backslashes and quotes. This reads scalar fields
// only; section nesting is immaterial for library paths and tool app IDs.
fn vdf_values(text: &str, key: &str) -> Vec<String> {
    let pattern = format!(r#""{}"\s+"((?:\\.|[^"\\])*)""#, regex::escape(key));
    regex::Regex::new(&pattern)
        .unwrap()
        .captures_iter(text)
        .map(|c| {
            let mut chars = c[1].chars();
            let mut value = String::new();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    value.push(chars.next().unwrap_or('\\'));
                } else {
                    value.push(ch);
                }
            }
            value
        })
        .collect()
}

fn libraries(steam: &Path) -> Vec<PathBuf> {
    let mut roots = vec![steam.to_path_buf()];
    if let Ok(vdf) = std::fs::read_to_string(steam.join("steamapps/libraryfolders.vdf")) {
        for path in vdf_values(&vdf, "path").into_iter().map(PathBuf::from) {
            if path.is_absolute() && path.join("steamapps").is_dir() && !roots.contains(&path) {
                roots.push(path);
            }
        }
    }
    roots
}

fn proton_version(path: &Path) -> Option<Vec<u32>> {
    let name = path.file_name()?.to_str()?;
    let version = name.strip_prefix("Proton ")?;
    // Experimental, beta, hotfix and custom tools are opt-in, never the default.
    if !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    Some(version.split('.').map(|p| p.parse().unwrap_or(0)).collect())
}

fn installed_protons(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut tools = Vec::new();
    for root in roots {
        for parent in [
            root.join("steamapps/common"),
            root.join("compatibilitytools.d"),
        ] {
            if let Ok(entries) = std::fs::read_dir(parent) {
                tools.extend(
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.join("proton").is_file()),
                );
            }
        }
    }
    tools.sort_by_key(|p| std::cmp::Reverse(proton_version(p)));
    tools
}

fn runtime_entry(proton: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let manifest = std::fs::read_to_string(proton.join("toolmanifest.vdf"))
        .map_err(|_| "This Proton directory has no toolmanifest.vdf. Select an installed Valve Proton release.".to_string())?;
    let app_id = vdf_values(&manifest, "require_tool_appid").into_iter().next()
        .ok_or("Proton does not declare a required Steam Linux Runtime. Select an installed Valve Proton release.")?;
    if !app_id.bytes().all(|b| b.is_ascii_digit()) || app_id.is_empty() {
        return Err("Invalid Steam Linux Runtime app ID in Proton's manifest.".into());
    }
    for root in roots {
        let manifest = root.join(format!("steamapps/appmanifest_{app_id}.acf"));
        if let Ok(text) = std::fs::read_to_string(manifest) {
            if let Some(dir) = vdf_values(&text, "installdir").first() {
                // An install name, never a path outside this Steam library.
                if dir.contains(['/', '\\']) || dir == "." || dir == ".." {
                    continue;
                }
                let entry = root
                    .join("steamapps/common")
                    .join(dir)
                    .join("_v2-entry-point");
                if entry.is_file() {
                    return Ok(entry);
                }
            }
        }
    }
    Err(format!(
        "Install Steam Linux Runtime (app {app_id}) in Steam, then retry."
    ))
}

/// The Proton verb games are started with; see [`Runtime::spawn`].
pub const LAUNCH_VERB: &str = "waitforexitandrun";

pub struct Runtime {
    pub steam: PathBuf,
    pub proton: PathBuf,
    pub compat_data: PathBuf,
    pub entry: PathBuf,
    pub network: Network,
    pub opengl: bool,
}

/// Canonicalize existing ancestors too, so a not-yet-created child below a
/// symlink cannot conceal a Steam prefix or a collision with another network.
pub(crate) fn resolved_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Use an absolute directory without '..' components.".into());
    }
    let mut ancestor = path;
    let mut tail = Vec::new();
    loop {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tail.push(
                    ancestor
                        .file_name()
                        .ok_or("Invalid directory")?
                        .to_os_string(),
                );
                ancestor = ancestor.parent().ok_or("Invalid directory")?;
            }
            Err(error) => {
                return Err(format!(
                    "Cannot inspect path {}: {error}",
                    ancestor.display()
                ))
            }
        }
    }
    let mut resolved = std::fs::canonicalize(ancestor)
        .map_err(|error| format!("Cannot inspect path {}: {error}", ancestor.display()))?;
    for component in tail.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn compat_data_dir(app: &tauri::AppHandle, network: Network) -> Result<PathBuf, String> {
    let cfg = config::current(app);
    let selected = cfg.linux.networks.get(network_name(network));
    let path = selected
        .filter(|s| !s.compat_data_dir.is_empty())
        .map(|s| PathBuf::from(&s.compat_data_dir))
        .unwrap_or_else(|| {
            config::app_data_dir(app)
                .join("proton")
                .join(network_name(network))
        });
    if !path.is_absolute() {
        return Err("The compatibility-data directory must be an absolute path.".into());
    }
    resolved_path(&path)
}

fn steam_dir(settings: &LinuxSettings) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let candidates = if settings.steam_dir.is_empty() {
        vec![
            home.join(".local/share/Steam"),
            home.join(".steam/steam"),
            home.join(".steam/root"),
        ]
    } else {
        vec![PathBuf::from(&settings.steam_dir)]
    };
    candidates.into_iter().find(|p| p.is_absolute() && p.join("steamapps").is_dir())
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or_else(|| "Install native Linux Steam and open it once. Flatpak Steam is not supported in this release.".into())
}

pub fn resolve(app: &tauri::AppHandle, network: Network) -> Result<Runtime, String> {
    let cfg = config::current(app);
    let steam = steam_dir(&cfg.linux)?;
    let roots = libraries(&steam);
    let selected = cfg.linux.networks.get(network_name(network));
    let proton = if let Some(s) = selected.filter(|s| !s.proton_dir.is_empty()) {
        PathBuf::from(&s.proton_dir)
    } else {
        installed_protons(&roots).into_iter().find(|p| proton_version(p).is_some())
            .ok_or("Install a stable Valve Proton release from Steam's Tools library, or select its directory in Settings.")?
    };
    if !proton.is_absolute() || !proton.join("proton").is_file() {
        return Err("The selected Proton directory must contain the proton executable.".into());
    }
    let entry = runtime_entry(&proton, &roots)?;
    let compat_data = compat_data_dir(app, network)?;
    let prefix = resolved_path(&compat_data.join("pfx"))?;
    if compat_data.parent().is_none() || compat_data.file_name().is_some_and(|n| n == "pfx") {
        return Err(
            "Choose a compatibility-data directory, not the filesystem root or a pfx directory."
                .into(),
        );
    }
    // Never reuse an official Steam game's prefix, or share one across networks.
    for root in &roots {
        let steam_prefixes = resolved_path(&root.join("steamapps/compatdata"))?;
        if compat_data.starts_with(&steam_prefixes)
            || steam_prefixes.starts_with(&compat_data)
            || prefix.starts_with(&steam_prefixes)
        {
            return Err("Choose a launcher-owned compatibility directory outside Steam's steamapps/compatdata.".into());
        }
    }
    for other in Network::ALL.into_iter().filter(|&n| n != network) {
        let other_path = compat_data_dir(app, other)?;
        let other_prefix = resolved_path(&other_path.join("pfx"))?;
        if compat_data.starts_with(&other_path)
            || other_path.starts_with(&compat_data)
            || prefix.starts_with(&other_prefix)
            || other_prefix.starts_with(&prefix)
        {
            return Err("Each revival needs its own separate compatibility-data directory.".into());
        }
    }
    #[cfg(target_os = "linux")]
    {
        let clients: Vec<_> = Network::ALL
            .into_iter()
            .map(|n| PathBuf::from(config::get_client_dir_for(app, &cfg, n)))
            .collect();
        prefixes_outside_clients(std::slice::from_ref(&compat_data), &clients)?;
        writable_compat_directory(&compat_data)?;
        writable_compat_directory(&prefix)?;
    }
    Ok(Runtime {
        steam,
        proton,
        compat_data,
        entry,
        network,
        opengl: selected.is_some_and(|s| s.opengl),
    })
}

#[cfg(target_os = "linux")]
fn prefixes_outside_clients(prefixes: &[PathBuf], clients: &[PathBuf]) -> Result<(), String> {
    for client in clients {
        let client = resolved_path(client)?;
        for directory in prefixes {
            for prefix in [
                resolved_path(directory)?,
                resolved_path(&directory.join("pfx"))?,
            ] {
                if prefix.starts_with(&client) {
                    return Err(format!(
                        "Proton data {} is inside game client {}. Move the compatibility-data directory outside all game client folders and update Linux / Proton settings before launching, updating or uninstalling. Prefixes must be retained.",
                        prefix.display(), client.display()
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Cleanup must protect prefixes even without an installed Steam/Proton.
/// Also retain the default prefixes after a user selects custom locations.
#[cfg(target_os = "linux")]
pub(crate) fn guard_client_cleanup(app: &tauri::AppHandle, client: &Path) -> Result<(), String> {
    let mut prefixes = Vec::new();
    for network in Network::ALL {
        prefixes.push(compat_data_dir(app, network)?);
        prefixes.push(
            config::app_data_dir(app)
                .join("proton")
                .join(network_name(network)),
        );
    }
    prefixes_outside_clients(&prefixes, &[client.to_path_buf()])
}

// Check the nearest existing directory without creating a prefix during a
// status query. access(2) with AT_EACCESS includes ACLs and mount permissions,
// unlike testing Unix mode bits. Check pfx separately because it may have
// different permissions from its compatibility-data parent.
#[cfg(target_os = "linux")]
fn writable_compat_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let problem = || {
        format!(
        "Cannot write to Proton compatibility directory {}. Choose a directory you own with read, write and search permissions.",
        path.display()
    )
    };
    let mut existing = path;
    loop {
        match std::fs::metadata(existing) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(format!(
                        "Proton compatibility path {} is not a directory.",
                        existing.display()
                    ));
                }
                let name = std::ffi::CString::new(existing.as_os_str().as_bytes())
                    .map_err(|_| problem())?;
                // SAFETY: name is a valid NUL-terminated path; this call only
                // checks access using the launcher's effective credentials.
                let accessible = unsafe {
                    libc::faccessat(
                        libc::AT_FDCWD,
                        name.as_ptr(),
                        libc::R_OK | libc::W_OK | libc::X_OK,
                        libc::AT_EACCESS,
                    )
                };
                return if accessible == 0 {
                    Ok(())
                } else {
                    Err(problem())
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or_else(problem)?;
            }
            Err(_) => return Err(problem()),
        }
    }
}

impl Runtime {
    pub(crate) fn command(&self, verb: &str) -> Result<Command, String> {
        #[cfg(target_os = "linux")]
        {
            writable_compat_directory(&self.compat_data)?;
            writable_compat_directory(&self.compat_data.join("pfx"))?;
        }
        std::fs::create_dir_all(&self.compat_data)
            .map_err(|e| format!("Cannot create Proton compatibility directory: {e}"))?;
        let app_id = if self.network == Network::Radium {
            "92"
        } else {
            "471710"
        };
        let mut cmd = system_command(&self.entry);
        cmd.args(["--verb=run", "--"])
            .arg(self.proton.join("proton"))
            .arg(verb)
            .env("STEAM_COMPAT_DATA_PATH", &self.compat_data)
            .env("STEAM_COMPAT_CLIENT_INSTALL_PATH", &self.steam)
            .env("STEAM_COMPAT_APP_ID", app_id)
            .env("SteamAppId", app_id)
            .env("SteamGameId", app_id);
        cmd.env(
            "STEAM_COMPAT_LIBRARY_PATHS",
            std::env::join_paths(libraries(&self.steam)).map_err(|e| e.to_string())?,
        );
        cmd.env(
            "STEAM_COMPAT_TOOL_PATHS",
            std::env::join_paths([
                self.proton.as_path(),
                self.entry.parent().ok_or("Runtime directory missing")?,
            ])
            .map_err(|e| e.to_string())?,
        );
        self.expose_paths(&mut cmd, &[])?;
        // AppImage sets these for its bundled GTK libraries; they must not
        // leak into native Steam or the Steam Linux Runtime container.
        for key in [
            "LD_LIBRARY_PATH",
            "LD_PRELOAD",
            "GTK_PATH",
            "GTK_EXE_PREFIX",
            "GTK_DATA_PREFIX",
            "GIO_EXTRA_MODULES",
        ] {
            cmd.env_remove(key);
        }
        let native_override = match self.network {
            Network::RecEos => Some("version=n,b"),
            // Vanilla's Windows client uses its bundled winhttp loader to
            // connect to Vanilla. Keep the official launcher/authentication.
            Network::Vanilla => Some("winhttp=n,b"),
            _ => None,
        };
        if let Some(native_override) = native_override {
            let existing = std::env::var("WINEDLLOVERRIDES").unwrap_or_default();
            cmd.env("WINEDLLOVERRIDES", format!("{existing};{native_override}"));
        }
        if self.opengl {
            cmd.env("PROTON_USE_WINED3D", "1");
        }
        Ok(cmd)
    }

    /// Starts `exe` in this prefix the way Steam starts a game: Proton's
    /// `waitforexitandrun` lets the prefix's previous Wine session finish
    /// shutting down first. Started straight after a crash or a Stop with
    /// plain `run`, the game met the old session's leftovers, and Rec EOS's
    /// mod loader failed to open its console (seen 2026-11-13).
    pub fn spawn(&self, exe: &Path, args: &[String], work_dir: &Path) -> Result<Child, String> {
        let mut cmd = self.command(LAUNCH_VERB)?;
        self.expose_paths(&mut cmd, &[work_dir])?;
        cmd.env("STEAM_COMPAT_INSTALL_PATH", work_dir);
        cmd.arg(exe).args(args).current_dir(work_dir);
        cmd.spawn()
            .map_err(|e| format!("Could not start Proton: {e}"))
    }

    pub fn windows_path(&self, path: &Path) -> Result<String, String> {
        let mut cmd = self.command("getcompatpath")?;
        self.expose_paths(&mut cmd, &[path.parent().unwrap_or(path)])?;
        let output = cmd
            .arg(path)
            .output()
            .map_err(|e| format!("Proton could not translate this path: {e}"))?;
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() || value.is_empty() || value.contains(['\n', '\r', '\0']) {
            return Err("Proton could not translate the selected file path. Check the runtime and prefix permissions.".into());
        }
        Ok(value)
    }

    pub fn native_path(&self, path: &str) -> Option<PathBuf> {
        windows_to_native(&self.compat_data.join("pfx"), path)
    }

    pub(crate) fn expose_paths(
        &self,
        command: &mut Command,
        paths: &[&Path],
    ) -> Result<(), String> {
        let mut shared: Vec<PathBuf> = std::env::var_os("PRESSURE_VESSEL_FILESYSTEMS_RW")
            .map(|v| std::env::split_paths(&v).collect())
            .unwrap_or_default();
        shared.push(self.compat_data.clone());
        shared.extend(paths.iter().map(|p| p.to_path_buf()));
        command.env(
            "PRESSURE_VESSEL_FILESYSTEMS_RW",
            std::env::join_paths(shared).map_err(|_| {
                "Proton shared paths cannot contain ':'. Choose another directory.".to_string()
            })?,
        );
        Ok(())
    }
}

// /usr resources are outside Steam's container. Stage a fresh helper inside
// the exposed compatibility directory without following destination symlinks.
pub fn stage_helper(runtime: &Runtime, source: &Path) -> Result<PathBuf, String> {
    use std::io::Write;
    std::fs::create_dir_all(&runtime.compat_data).map_err(|e| e.to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    let directory = runtime
        .compat_data
        .join(format!("receos-helper-{}-{nonce}", std::process::id()));
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory).map_err(|e| e.to_string())?;
    let target = directory.join("receos-proton-helper.exe");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&target).map_err(|e| e.to_string())?;
    let result = std::fs::read(source).and_then(|bytes| file.write_all(&bytes));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&target);
        return Err(error.to_string());
    }
    Ok(target)
}

pub struct HelperFiles(pub PathBuf);

impl Drop for HelperFiles {
    fn drop(&mut self) {
        if let Some(directory) = self.0.parent() {
            for name in [
                "request.json",
                "response.json",
                "response.partial",
                "receos-proton-helper.exe",
            ] {
                let _ = std::fs::remove_file(directory.join(name));
            }
            let _ = std::fs::remove_dir(directory);
        }
    }
}

pub fn helper_reply(path: &Path) -> Result<Option<Value>, String> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Cannot read the Proton helper result: {e}")),
    };
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Invalid Proton helper result file.".into());
    }
    let mut bytes = Vec::new();
    file.take(65537)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 65536 {
        return Err("Proton helper result is too large.".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("Invalid Proton helper result: {e}"))
}

pub fn reap(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

#[tauri::command(async)]
pub fn get_linux_runtime_status(app: tauri::AppHandle, network: Option<String>) -> Value {
    if !cfg!(target_os = "linux") {
        return json!({"required": false});
    }
    let network = Network::parse(network.as_deref());
    let cfg = config::current(&app);
    let steam = steam_dir(&cfg.linux).ok();
    let tools: Vec<Value> = steam
        .as_ref()
        .map(|s| installed_protons(&libraries(s)))
        .unwrap_or_default()
        .into_iter()
        .map(
            |p| json!({ "name": tool_name(&p), "path": p, "stable": proton_version(&p).is_some() }),
        )
        .collect();
    // The prefix shown even when Steam or Proton is missing, so the page can
    // still say where this network's Windows files live.
    let compat_data = compat_data_dir(&app, network).ok();
    let base = json!({
        "required": true,
        "steamDir": steam,
        "compatDataDir": compat_data,
        "defaultCompatDataDir": config::app_data_dir(&app).join("proton").join(network_name(network)),
        "installedProtons": tools,
        "gameplayValidated": false,
    });
    let mut status = base;
    match resolve(&app, network) {
        Ok(r) => {
            status["ready"] = json!(true);
            status["protonDir"] = json!(r.proton);
            status["protonName"] = json!(tool_name(&r.proton));
            status["runtimeName"] = json!(r.entry.parent().map(tool_name));
            status["compatDataDir"] = json!(r.compat_data);
        }
        Err(e) => {
            status["ready"] = json!(false);
            status["error"] = json!(e);
        }
    }
    status
}

/// Opens a network's Proton prefix folder in the file manager. Only the
/// configured prefix, never a path from the page.
#[tauri::command(async)]
pub fn open_proton_prefix(app: tauri::AppHandle, network: String) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("Proton prefixes exist on Linux only.".into());
    }
    let dir = compat_data_dir(&app, Network::parse(Some(&network)))?;
    if !dir.is_dir() {
        return Err(
            "This revival has no Proton files yet. They are created the first time you play."
                .into(),
        );
    }
    crate::download::reveal_folder(&dir);
    Ok(())
}

/// A Steam tool's display name: its folder name ("Proton 11.0").
fn tool_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The last game launch through Proton, for explaining a game that closes
/// right after starting. Cleared by Stop, which is a deliberate close.
static LAST_LAUNCH: std::sync::Mutex<Option<(std::time::Instant, Network)>> =
    std::sync::Mutex::new(None);

/// How soon after launch a close counts as "closed right after starting".
const EARLY_EXIT: std::time::Duration = std::time::Duration::from_secs(45);

pub fn note_launch(network: Network) {
    if let Ok(mut last) = LAST_LAUNCH.lock() {
        *last = Some((std::time::Instant::now(), network));
    }
}

fn forget_launch() {
    if let Ok(mut last) = LAST_LAUNCH.lock() {
        *last = None;
    }
}

/// Called when the game is seen to close. When that happens within
/// `EARLY_EXIT` of launching (and not through Stop), explains the most likely
/// cause, reading the game's own log for the graphics failure Proton gives on
/// a system without a usable Vulkan driver.
pub fn early_exit_hint() -> Option<String> {
    let (launched, network) = LAST_LAUNCH.lock().ok()?.take()?;
    if launched.elapsed() > EARLY_EXIT {
        return None;
    }
    let app = APP.get()?;
    let opengl = config::current(app)
        .linux
        .networks
        .get(network_name(network))
        .is_some_and(|s| s.opengl);
    let log = rec_room_data_dir(network)
        .and_then(|dir| std::fs::read_to_string(dir.join("Player.log")).ok())
        .unwrap_or_default();
    Some(early_exit_message(&log, opengl))
}

fn early_exit_message(player_log: &str, opengl: bool) -> String {
    let no_vulkan = player_log.contains("d3d11: failed to create factory");
    match (no_vulkan, opengl) {
        (true, false) => "The game couldn't start its graphics: Proton found no Vulkan 1.3 driver. \
            Install your graphics card's Vulkan driver, or turn on OpenGL mode in Settings > Proton."
            .into(),
        (true, true) => "The game couldn't start its graphics, even in OpenGL mode. \
            Check that your graphics drivers are installed."
            .into(),
        (false, false) => "The game closed right after starting. If it keeps happening, \
            try OpenGL mode in Settings > Proton, or check the Logs page."
            .into(),
        (false, true) => "The game closed right after starting. Check the Logs page, \
            or turn OpenGL mode off in Settings > Proton."
            .into(),
    }
}

fn legacy_batch_args(path: String) -> Result<Vec<String>, String> {
    // CALL receives one path argument. Embedding quotes in /c's command
    // argument is escaped again by Proton's Steam shim and fails with spaces.
    // cmd still expands metacharacters, so reject them before starting it.
    if path.contains(['%', '!', '"', '&', '|', '<', '>', '^', '\r', '\n']) {
        return Err("Move this legacy batch client to a directory without cmd metacharacters, or select its game executable.".into());
    }
    Ok(vec![
        "/d".into(),
        "/s".into(),
        "/c".into(),
        "call".into(),
        path,
    ])
}

pub fn launch(
    app: &tauri::AppHandle,
    network: Network,
    exe: &Path,
    args: &[String],
) -> Result<(), String> {
    let runtime = resolve(app, network)?;
    if !steam_running() {
        return Err("Start native Linux Steam and sign in before playing.".into());
    }
    let dir = exe
        .parent()
        .ok_or("Game executable has no parent directory.")?;
    let mut args = args.to_vec();
    let target = if exe
        .extension()
        .is_some_and(|s| s.eq_ignore_ascii_case("bat"))
    {
        let path = runtime.windows_path(exe)?;
        args = legacy_batch_args(path)?;
        PathBuf::from("cmd.exe")
    } else {
        exe.to_path_buf()
    };
    reap(runtime.spawn(&target, &args, dir)?);
    Ok(())
}

// Registry strings in a Wine prefix use doubled backslashes. Resolve drive
// mappings rather than assuming that a user's prefix maps every path to Z:.
fn windows_to_native(prefix: &Path, path: &str) -> Option<PathBuf> {
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return None;
    }
    let drive = format!("{}:", (bytes[0] as char).to_ascii_lowercase());
    let mapped = prefix.join("dosdevices").join(drive);
    let base = std::fs::canonicalize(mapped).ok()?;
    let tail = path[3..].replace('\\', "/");
    if tail.split('/').any(|part| part == "..") {
        return None;
    }
    Some(base.join(tail))
}

fn registry_value(prefix: &Path, section: &str, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(prefix.join("user.reg")).ok()?;
    let section_header = format!("[{section}]");
    let mut active = false;
    for line in text.lines() {
        if line.starts_with('[') {
            active = line.starts_with(&section_header);
            continue;
        }
        if active {
            let start = format!("\"{name}\"=\"");
            if let Some(value) = line.strip_prefix(&start).and_then(|s| s.strip_suffix('"')) {
                return Some(value.replace("\\\\", "\\").replace("\\\"", "\""));
            }
        }
    }
    None
}

pub fn vanilla_data_dir() -> Option<PathBuf> {
    let app = APP.get()?;
    let prefix = compat_data_dir(app, Network::Vanilla).ok()?.join("pfx");
    let appdata = registry_value(
        &prefix,
        r"Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders",
        "AppData",
    )?;
    Some(windows_to_native(&prefix, &appdata)?.join("VanillaLauncher"))
}

pub fn vanilla_launcher() -> Option<String> {
    let app = APP.get()?;
    let prefix = compat_data_dir(app, Network::Vanilla).ok()?.join("pfx");
    // The official MSI installs per machine into Program Files. Both forms
    // are inside this prefix; never inspect the host's Windows environment.
    for dir in ["Program Files", "Program Files (x86)"] {
        let exe = prefix
            .join("drive_c")
            .join(dir)
            .join("Vanilla Launcher/vanillalauncher.exe");
        if exe.is_file() {
            return Some(exe.to_string_lossy().to_string());
        }
    }
    None
}

pub fn vanilla_game_dir() -> Option<String> {
    let app = APP.get()?;
    let data = vanilla_data_dir()?;
    let settings: Value = std::fs::read_to_string(data.join("settings.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let root = settings["installDirectory"]
        .as_str()
        .filter(|s| !s.is_empty())
        .and_then(|p| {
            windows_to_native(&compat_data_dir(app, Network::Vanilla).ok()?.join("pfx"), p)
        })
        .unwrap_or_else(|| data.join("Installs"));
    Some(root.join("Vanilla").to_string_lossy().to_string())
}

pub fn vanilla_install_root(path: &str) -> Option<String> {
    let app = APP.get()?;
    let prefix = compat_data_dir(app, Network::Vanilla).ok()?.join("pfx");
    windows_to_native(&prefix, path).map(|p| p.to_string_lossy().to_string())
}

pub fn rec_room_data_dir(network: Network) -> Option<PathBuf> {
    let app = APP.get()?;
    let prefix = compat_data_dir(app, network).ok()?.join("pfx");
    let local = registry_value(
        &prefix,
        r"Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders",
        "Local AppData",
    )?;
    Some(
        windows_to_native(&prefix, &local)?
            .parent()?
            .join("LocalLow/Against Gravity/Rec Room"),
    )
}

pub fn required_steam_app() -> bool {
    let Some(app) = APP.get() else { return false };
    let cfg = config::current(app);
    let Ok(steam) = steam_dir(&cfg.linux) else {
        return false;
    };
    libraries(&steam).iter().any(|root| {
        std::fs::read_to_string(root.join("steamapps/appmanifest_92.acf"))
            .ok()
            .is_some_and(|text| {
                vdf_values(&text, "StateFlags")
                    .iter()
                    .any(|s| s.parse::<u32>().is_ok_and(|v| v & 4 != 0))
            })
    })
}

#[cfg(target_os = "linux")]
mod processes {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    pub struct Process {
        pub pid: u32,
        start: String,
        pub exe: PathBuf,
    }

    fn owned(path: &Path) -> bool {
        // SAFETY: getuid has no preconditions.
        path.metadata()
            .is_ok_and(|m| m.uid() == unsafe { libc::getuid() })
    }

    fn start_time(dir: &Path) -> Option<String> {
        let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(19)
            .map(str::to_string)
    }

    fn mapped_image(dir: &Path, images: &[&str]) -> Option<PathBuf> {
        let maps = std::fs::read_to_string(dir.join("maps")).ok()?;
        for line in maps.lines() {
            let mut fields = line;
            for _ in 0..5 {
                fields = fields.trim_start();
                fields = &fields[fields.find(char::is_whitespace).unwrap_or(fields.len())..];
            }
            let path = fields
                .trim_start()
                .trim_end_matches(" (deleted)")
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n")
                .replace("\\134", "\\");
            let path = PathBuf::from(path);
            if path.is_absolute()
                && path
                    .file_name()
                    .is_some_and(|n| images.iter().any(|i| n.eq_ignore_ascii_case(i)))
            {
                return Some(path);
            }
        }
        None
    }

    fn candidate_comm(comm: &str, images: &[&str]) -> bool {
        let name = comm.trim_end();
        images
            .iter()
            .any(|image| name.eq_ignore_ascii_case(&image[..image.len().min(15)]))
            || ["wine", "wine64", "wine-preloader", "wine64-preloade"].contains(&name)
    }

    fn belongs(exe: &Path, prefix: &Path, scopes: &[(PathBuf, String)]) -> bool {
        scopes.iter().any(|(expected, client)| {
            expected == prefix && config::path_is_inside_dir(&exe.to_string_lossy(), client)
        })
    }

    pub fn list(images: &[&str], scoped: bool) -> Vec<Process> {
        let mut found = Vec::new();
        let mut scopes = Vec::new();
        if scoped {
            let Some(app) = APP.get() else { return found };
            let cfg = config::current(app);
            for network in Network::ALL {
                if let Ok(prefix) = compat_data_dir(app, network)
                    .and_then(|p| std::fs::canonicalize(p.join("pfx")).map_err(|e| e.to_string()))
                {
                    let client = if network == Network::Vanilla {
                        vanilla_game_dir().unwrap_or_default()
                    } else {
                        config::get_client_dir_for(app, &cfg, network)
                    };
                    scopes.push((prefix, client));
                }
            }
        }
        let Some(entries) = std::fs::read_dir("/proc").ok() else {
            return found;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if !owned(&dir) {
                continue;
            }
            if !std::fs::read_to_string(dir.join("comm"))
                .is_ok_and(|comm| candidate_comm(&comm, images))
            {
                continue;
            }
            let Some(start) = start_time(&dir) else {
                continue;
            };
            let Some(exe) = mapped_image(&dir, images) else {
                continue;
            };
            if scoped {
                let Ok(env) = std::fs::read(dir.join("environ")) else {
                    continue;
                };
                let prefix = env
                    .split(|&c| c == 0)
                    .find_map(|s| s.strip_prefix(b"WINEPREFIX="))
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .map(PathBuf::from);
                let Some(prefix) = prefix.and_then(|p| std::fs::canonicalize(p).ok()) else {
                    continue;
                };
                if !belongs(&exe, &prefix, &scopes) {
                    continue;
                }
            }
            found.push(Process { pid, start, exe });
        }
        found
    }

    pub fn stop(process: Process) -> bool {
        // pidfds bind the signal to this process even if the numeric PID is
        // reused between checking the image and sending the signal.
        let dir = PathBuf::from(format!("/proc/{}", process.pid));
        unsafe {
            let fd = libc::syscall(libc::SYS_pidfd_open, process.pid, 0) as i32;
            if fd < 0 {
                return false;
            }
            let image = process
                .exe
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let same = owned(&dir)
                && start_time(&dir).as_deref() == Some(process.start.as_str())
                && mapped_image(&dir, &[&image]).as_ref() == Some(&process.exe);
            let sent = same
                && libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd,
                    libc::SIGTERM,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                ) == 0;
            if sent {
                for _ in 0..10 {
                    if start_time(&dir).as_deref() != Some(process.start.as_str()) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if start_time(&dir).as_deref() == Some(process.start.as_str()) {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd,
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
            libc::close(fd);
            sent
        }
    }

    pub fn stop_helper(prefix: &Path, image: &Path) {
        let Ok(prefix) = std::fs::canonicalize(prefix) else {
            return;
        };
        let Some(name) = image.file_name().and_then(|s| s.to_str()) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if !owned(&dir) || mapped_image(&dir, &[name]).as_deref() != Some(image) {
                continue;
            }
            let matches = std::fs::read(dir.join("environ")).ok().is_some_and(|env| {
                env.split(|&c| c == 0)
                    .find_map(|s| s.strip_prefix(b"WINEPREFIX="))
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .and_then(|s| std::fs::canonicalize(s).ok())
                    .as_ref()
                    == Some(&prefix)
            });
            if matches {
                if let Some(start) = start_time(&dir) {
                    stop(Process {
                        pid,
                        start,
                        exe: image.to_path_buf(),
                    });
                }
            }
        }
    }

    pub fn steam_running() -> bool {
        std::fs::read_dir("/proc").is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                owned(&entry.path())
                    && std::fs::read_link(entry.path().join("exe"))
                        .ok()
                        .is_some_and(|p| p.file_name().is_some_and(|n| n == "steam"))
            })
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn comm_truncation_does_not_hide_new_clients() {
            assert!(candidate_comm("Recroom_Release\n", &crate::game::GAME_EXES));
            assert!(candidate_comm("RecRoom.exe\n", &crate::game::GAME_EXES));
            assert!(!candidate_comm("steam\n", &crate::game::GAME_EXES));
        }
        #[test]
        fn both_prefix_and_client_image_must_match() {
            let scopes = vec![(
                PathBuf::from("/home/me/prefix/pfx"),
                "/home/me/Client".into(),
            )];
            assert!(belongs(
                Path::new("/home/me/Client/RecRoom.exe"),
                Path::new("/home/me/prefix/pfx"),
                &scopes
            ));
            assert!(!belongs(
                Path::new("/home/me/Client/RecRoom.exe"),
                Path::new("/home/me/Steam/pfx"),
                &scopes
            ));
            assert!(!belongs(
                Path::new("/home/me/client/RecRoom.exe"),
                Path::new("/home/me/prefix/pfx"),
                &scopes
            ));
            assert!(!belongs(
                Path::new("/home/me/Client2/RecRoom.exe"),
                Path::new("/home/me/prefix/pfx"),
                &scopes
            ));
        }
        #[test]
        fn mapped_pe_image_retains_spaces() {
            let root = super::super::tests::temp("maps");
            std::fs::write(root.join("maps"), "100-200 r-xp 0 00:00 1 /usr/bin/wine64\n200-300 r--p 0 00:00 2 /home/me/My\\040 Games/RecRoom.exe\n").unwrap();
            assert_eq!(
                mapped_image(&root, &crate::game::GAME_EXES).unwrap(),
                PathBuf::from("/home/me/My  Games/RecRoom.exe")
            );
            assert!(mapped_image(&root, &["steam.exe"]).is_none());
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

pub fn game_running(images: &[&str], scoped: bool) -> bool {
    #[cfg(target_os = "linux")]
    {
        !processes::list(images, scoped).is_empty()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (images, scoped);
        false
    }
}

pub fn stop_games() -> bool {
    forget_launch();
    #[cfg(target_os = "linux")]
    {
        processes::list(&crate::game::GAME_EXES, true)
            .into_iter()
            .map(processes::stop)
            .fold(true, |all, stopped| stopped && all)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub fn stop_helper(runtime: &Runtime, image: &Path) {
    #[cfg(target_os = "linux")]
    processes::stop_helper(&runtime.compat_data.join("pfx"), image);
    #[cfg(not(target_os = "linux"))]
    let _ = (runtime, image);
}

pub fn steam_running() -> bool {
    #[cfg(target_os = "linux")]
    {
        processes::steam_running()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn missing_directories_are_resolved_but_dangling_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = temp("missing-prefix-path");
        assert_eq!(
            resolved_path(&root.join("not created/pfx")).unwrap(),
            std::fs::canonicalize(&root)
                .unwrap()
                .join("not created/pfx")
        );
        symlink(root.join("missing target"), root.join("dangling prefix")).unwrap();
        assert!(resolved_path(&root.join("dangling prefix/pfx")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_protects_nested_prefixes_and_prefix_symlink_targets() {
        use std::os::unix::fs::symlink;
        let root = temp("prefix-retention");
        let client = root.join("Games with spaces/Rec EOS");
        let nested = client.join("RecRoom_Data/compat data");
        std::fs::create_dir_all(nested.join("pfx/drive_c/users/steamuser")).unwrap();
        let save = nested.join("pfx/drive_c/users/steamuser/save.dat");
        std::fs::write(&save, b"preserved save").unwrap();
        assert!(prefixes_outside_clients(
            std::slice::from_ref(&nested),
            std::slice::from_ref(&client)
        )
        .unwrap_err()
        .contains("Move the compatibility-data directory"));
        let external = root.join("external prefix");
        std::fs::create_dir_all(&external).unwrap();
        symlink(nested.join("pfx"), external.join("pfx")).unwrap();
        assert!(prefixes_outside_clients(&[external], &[client]).is_err());
        assert_eq!(std::fs::read(save).unwrap(), b"preserved save");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_allows_sibling_prefixes_and_game_files_inside_a_prefix() {
        let root = temp("prefix-siblings");
        let prefix = root.join("client-other/proton");
        std::fs::create_dir_all(&prefix).unwrap();
        prefixes_outside_clients(std::slice::from_ref(&prefix), &[root.join("client")]).unwrap();
        // Vanilla and user-selected Windows installs can live below drive_c;
        // clearing their game files does not remove the containing prefix.
        prefixes_outside_clients(
            std::slice::from_ref(&prefix),
            &[prefix.join("pfx/drive_c/Games/Rec EOS")],
        )
        .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn compatibility_paths_check_directories_and_do_not_create_them() {
        let root = temp("prefix-access");
        let missing = root.join("new prefix with spaces/child");
        writable_compat_directory(&missing).unwrap();
        assert!(!missing.exists());
        std::fs::write(root.join("file"), b"not a directory").unwrap();
        assert!(writable_compat_directory(&root.join("file"))
            .unwrap_err()
            .contains("not a directory"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unwritable_compatibility_data_and_prefix_are_rejected() {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        const CHILD_PATH: &str = "RECEOS_TEST_UNWRITABLE_PREFIX";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = PathBuf::from(path);
            assert!(writable_compat_directory(&path)
                .unwrap_err()
                .contains("permissions"));
            assert!(writable_compat_directory(&path.join("new child")).is_err());
            assert!(resolved_path(&path.join("new child")).is_err());
            return;
        }
        let root = temp("prefix-permissions");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        for path in [root.join("compat data"), root.join("other/pfx")] {
            std::fs::create_dir_all(&path).unwrap();
            for mode in [0o500, 0o400] {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
                if unsafe { libc::geteuid() } == 0 {
                    // Root can write mode-0500 directories. Exercise the real
                    // unprivileged access check even in root-owned CI containers.
                    let status = Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "linux::tests::unwritable_compatibility_data_and_prefix_are_rejected",
                        ])
                        .env(CHILD_PATH, &path)
                        .uid(65534)
                        .gid(65534)
                        .status()
                        .unwrap();
                    assert!(status.success());
                } else {
                    assert!(writable_compat_directory(&path)
                        .unwrap_err()
                        .contains("permissions"));
                    assert!(writable_compat_directory(&path.join("new child")).is_err());
                    if mode == 0o400 {
                        assert!(resolved_path(&path.join("new child")).is_err());
                    }
                }
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn system_tools_use_host_libraries_and_keep_the_desktop_session() {
        let mut command = Command::new("notify-send");
        command
            .env("LD_LIBRARY_PATH", "/app/usr/lib")
            .env("GIO_EXTRA_MODULES", "/app/gio")
            .env("GSETTINGS_SCHEMA_DIR", "/app/schemas")
            .env("DISPLAY", ":17")
            .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/session")
            .env("PYTHONHOME", "/app/usr")
            .env("PYTHONPATH", "/app/python")
            .env("PYTHONDONTWRITEBYTECODE", "1");
        unbundle_environment(&mut command);
        let env: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(env[std::ffi::OsStr::new("LD_LIBRARY_PATH")], None);
        assert_eq!(env[std::ffi::OsStr::new("GIO_EXTRA_MODULES")], None);
        assert_eq!(env[std::ffi::OsStr::new("GSETTINGS_SCHEMA_DIR")], None);
        assert_eq!(env[std::ffi::OsStr::new("PYTHONHOME")], None);
        assert_eq!(env[std::ffi::OsStr::new("PYTHONPATH")], None);
        assert_eq!(
            env[std::ffi::OsStr::new("PYTHONDONTWRITEBYTECODE")].unwrap(),
            "1"
        );
        assert_eq!(
            env[std::ffi::OsStr::new("DISPLAY")],
            Some(std::ffi::OsStr::new(":17"))
        );
        assert_eq!(
            env[std::ffi::OsStr::new("DBUS_SESSION_BUS_ADDRESS")],
            Some(std::ffi::OsStr::new("unix:path=/session"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_python_can_import_its_standard_library_after_appimage_cleanup() {
        let root = temp("appimage-python");
        let mut command = Command::new("python3");
        command
            .env("PYTHONHOME", root.join("bundled-python"))
            .env("PYTHONPATH", root.join("bundled-modules"));
        unbundle_environment(&mut command);
        let output = command
            .args(["-c", "import encodings; print('host-python')"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"host-python\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_batch_paths_retain_spaces_and_reject_cmd_expansion() {
        let path = r"Z:\home\me\My Games\launch game.bat";
        let args = legacy_batch_args(path.into()).unwrap();
        assert_eq!(args.last().unwrap(), path);
        assert_eq!(args[3], "call");
        for character in ['%', '!', '"', '&', '|', '<', '>', '^', '\r', '\n'] {
            assert!(legacy_batch_args(format!(r"Z:\Games\bad{character}path\launch.bat")).is_err());
        }
    }
    #[test]
    fn desktop_url_opener_keeps_the_existing_scheme_restrictions() {
        for valid in [
            "https://rec-eos.com/",
            "https://github.com/abod124-sudo/RecEOS-Launcher/releases/latest",
            "steam://",
            "steam://install/92",
        ] {
            assert!(allowed_linux_url(valid), "{valid}");
        }
        for invalid in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "http://example.com/",
            "steam://run/92",
            "steam://install/92;other",
            "https://user:password@example.com",
            "https://example.com/\nother",
        ] {
            assert!(!allowed_linux_url(invalid), "{invalid}");
        }
    }
    pub(super) fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "receos-linux-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    #[test]
    fn discovers_additional_library_and_matching_runtime() {
        let root = temp("libraries");
        let library = root.join("My Games");
        std::fs::create_dir_all(root.join("steamapps")).unwrap();
        let proton = library.join("steamapps/common/Proton 10.0");
        let runtime = library.join("steamapps/common/SteamLinuxRuntime_sniper");
        std::fs::create_dir_all(&proton).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                "\"libraryfolders\" {{ \"1\" {{ \"path\" \"{}\" }} }}",
                library.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        std::fs::write(proton.join("proton"), "").unwrap();
        std::fs::write(
            proton.join("toolmanifest.vdf"),
            "\"manifest\" { \"require_tool_appid\" \"1628350\" }",
        )
        .unwrap();
        std::fs::write(
            library.join("steamapps/appmanifest_1628350.acf"),
            "\"AppState\" { \"installdir\" \"SteamLinuxRuntime_sniper\" }",
        )
        .unwrap();
        std::fs::write(runtime.join("_v2-entry-point"), "").unwrap();
        let roots = libraries(&root);
        assert_eq!(installed_protons(&roots), [proton.clone()]);
        assert_eq!(
            runtime_entry(&proton, &roots).unwrap(),
            runtime.join("_v2-entry-point")
        );
        std::fs::remove_file(runtime.join("_v2-entry-point")).unwrap();
        assert!(runtime_entry(&proton, &roots)
            .unwrap_err()
            .contains("1628350"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn invalid_prefix_paths_are_rejected() {
        assert!(resolved_path(Path::new("relative/prefix")).is_err());
        let root = temp("prefix");
        assert!(resolved_path(&root.join("../escape")).is_err());
        assert!(resolved_path(&root.join("new/prefix"))
            .unwrap()
            .ends_with("new/prefix"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_ancestors_and_wine_drive_paths_resolve() {
        let root = temp("symlink");
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.join("alias")).unwrap();
        assert_eq!(
            resolved_path(&root.join("alias/new")).unwrap(),
            real.join("new")
        );
        let pfx = root.join("pfx");
        std::fs::create_dir_all(pfx.join("dosdevices")).unwrap();
        std::os::unix::fs::symlink(&real, pfx.join("dosdevices/c:")).unwrap();
        assert_eq!(
            windows_to_native(&pfx, r"C:\Program Files\Vanilla Launcher"),
            Some(real.join("Program Files/Vanilla Launcher"))
        );
        assert!(windows_to_native(&pfx, r"C:\..\outside").is_none());
        assert!(windows_to_native(&pfx, r"\\server\share").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn vdf_paths_and_runtime_ids() {
        assert_eq!(
            vdf_values(
                r#""libraryfolders" { "1" { "path" "/media/My Games/Steam" } }"#,
                "path"
            ),
            ["/media/My Games/Steam"]
        );
        assert_eq!(
            vdf_values(
                r#""manifest" { "require_tool_appid" "1628350" }"#,
                "require_tool_appid"
            ),
            ["1628350"]
        );
    }
    #[test]
    fn stable_proton_ordering() {
        assert!(proton_version(Path::new("Proton 10.0")) > proton_version(Path::new("Proton 9.0")));
        assert!(proton_version(Path::new("Proton Experimental")).is_none());
        assert!(proton_version(Path::new("GE-Proton10-1")).is_none());
    }
    #[test]
    fn old_config_and_per_network_settings_roundtrip() {
        let old: config::Config = serde_json::from_str("{}").unwrap();
        assert!(old.linux.networks.is_empty());
        let mut settings = LinuxSettings::default();
        settings.networks.insert(
            "stella".into(),
            ProtonSettings {
                proton_dir: "/media/Steam/Proton 10.0".into(),
                compat_data_dir: "/home/user/My Games/stella".into(),
                opengl: true,
            },
        );
        let decoded: LinuxSettings =
            serde_json::from_value(serde_json::to_value(&settings).unwrap()).unwrap();
        assert_eq!(
            decoded.networks["stella"].compat_data_dir,
            settings.networks["stella"].compat_data_dir
        );
        assert!(decoded.networks["stella"].opengl);
        // Configs saved before OpenGL mode existed keep Vulkan.
        let older: LinuxSettings =
            serde_json::from_str(r#"{"networks":{"receos":{"protonDir":""}}}"#).unwrap();
        assert!(!older.networks["receos"].opengl);
    }

    #[test]
    fn early_exit_messages_name_the_cause() {
        let no_vulkan = "GfxDevice: creating device client; threaded=1\nd3d11: failed to create factory (80004005).\n";
        assert!(early_exit_message(no_vulkan, false).contains("OpenGL mode"));
        assert!(early_exit_message(no_vulkan, false).contains("Vulkan"));
        assert!(!early_exit_message(no_vulkan, true).contains("turn on OpenGL"));
        assert!(early_exit_message("", false).contains("try OpenGL mode"));
        assert!(early_exit_message("", true).contains("turn OpenGL mode off"));
    }

    #[test]
    fn stop_is_not_reported_as_an_early_exit() {
        note_launch(Network::RecEos);
        forget_launch();
        assert!(LAST_LAUNCH.lock().unwrap().is_none());
    }
    #[test]
    fn proton_command_preserves_arguments_and_isolates_environment() {
        let root = temp("command");
        let mut runtime = Runtime {
            steam: root.join("Steam & Games"),
            proton: root.join("Proton 10.0"),
            compat_data: root.join("prefix"),
            entry: root.join("runtime/_v2-entry-point"),
            network: Network::RecEos,
            opengl: false,
        };
        let mut command = runtime.command("run").unwrap();
        command
            .arg(root.join("My Game & Mods/RecRoom.exe"))
            .arg("+roomname:Test_Room");
        let args: Vec<_> = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[..3],
            &[
                "--verb=run",
                "--",
                runtime.proton.join("proton").to_string_lossy().as_ref()
            ]
        );
        assert_eq!(args[3], "run");
        assert!(
            args[4].ends_with("My Game & Mods/RecRoom.exe")
                || args[4].ends_with(r"My Game & Mods\RecRoom.exe")
        );
        assert_eq!(args[5], "+roomname:Test_Room");
        let env: BTreeMap<_, _> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|s| s.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(env["SteamAppId"].as_deref(), Some("471710"));
        assert!(env["WINEDLLOVERRIDES"]
            .as_deref()
            .unwrap()
            .ends_with("version=n,b"));
        assert_eq!(env["LD_LIBRARY_PATH"], None);
        runtime.network = Network::Vanilla;
        let vanilla = runtime.command("run").unwrap();
        let overrides = vanilla
            .get_envs()
            .find(|(name, _)| *name == "WINEDLLOVERRIDES")
            .and_then(|(_, value)| value)
            .unwrap()
            .to_string_lossy();
        assert!(overrides.ends_with("winhttp=n,b"));
        assert!(vanilla
            .get_envs()
            .all(|(name, _)| name != "PROTON_USE_WINED3D"));
        runtime.opengl = true;
        let opengl = runtime.command("run").unwrap();
        assert!(opengl
            .get_envs()
            .any(|(name, value)| name == "PROTON_USE_WINED3D" && value == Some("1".as_ref())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_helpers_are_fresh_and_do_not_overwrite_previous_files() {
        let root = temp("helper-stage");
        let source = root.join("installed-helper.exe");
        std::fs::write(&source, b"bundled helper").unwrap();
        let runtime = Runtime {
            steam: root.clone(),
            proton: root.clone(),
            compat_data: root.join("prefix"),
            entry: root.clone(),
            network: Network::Stella,
            opengl: false,
        };
        let first = stage_helper(&runtime, &source).unwrap();
        std::fs::write(&first, b"previous launch").unwrap();
        let second = stage_helper(&runtime, &source).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(second).unwrap(), b"bundled helper");
        assert_eq!(std::fs::read(first).unwrap(), b"previous launch");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn helper_results_are_bounded_and_require_complete_json() {
        let root = temp("helper-reply");
        let path = root.join("response.json");
        assert!(helper_reply(&path).unwrap().is_none());
        std::fs::write(&path, br#"{"version":1,"success":false}"#).unwrap();
        assert_eq!(helper_reply(&path).unwrap().unwrap()["success"], false);
        std::fs::write(&path, b"{").unwrap();
        assert!(helper_reply(&path).is_err());
        std::fs::write(&path, vec![b' '; 65537]).unwrap();
        assert!(helper_reply(&path).unwrap_err().contains("too large"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn helper_results_reject_symlinks_and_non_files() {
        let root = temp("helper-reply-symlink");
        let target = root.join("target.json");
        std::fs::write(&target, br#"{"version":1,"success":true}"#).unwrap();
        let link = root.join("response.json");
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert!(helper_reply(&link).is_err());
        assert!(helper_reply(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
