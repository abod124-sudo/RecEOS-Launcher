use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::{json, Value};
use tauri::Emitter;
use tauri::Manager;

use crate::config;

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Game executables the launcher recognises, newest client first.
/// (The new recroom.baby client uses `Recroom_Release.exe`; the legacy client
/// used `RecRoom.exe`.) The legacy screen-mode script is the last resort.
pub const GAME_EXES: [&str; 2] = ["Recroom_Release.exe", "RecRoom.exe"];

/// Every launch target, in preference order.
const LAUNCH_TARGETS: [&str; 3] = [
    "Recroom_Release.exe",
    "RecRoom.exe",
    "RecRoom_ScreenMode.bat",
];

/// Locate the game executable inside `dir` — recursive to depth 4, preferring
/// the newest known client, then the legacy exe, then the screen-mode script.
///
/// All three names are matched in a single walk. Searching for one name at a
/// time meant that on the common install (which has `RecRoom.exe`, not
/// `Recroom_Release.exe`) the whole tree was walked to exhaustion for the name
/// that isn't there before the second pass found the one that is — a full scan
/// of a multi-gigabyte install on every `check_install`, which runs on every
/// settings autosave. Preference is applied per directory level, so a
/// `Recroom_Release.exe` nested one level down still loses to nothing: the
/// shallowest directory containing any target wins, and within it the
/// most-preferred name.
pub fn find_game_exe(dir: &str) -> Option<String> {
    find_launch_target(Path::new(dir), &LAUNCH_TARGETS, 0)
}

/// What Stella launches. Its client carries both exes, but
/// `Recroom_Release.exe` there is the EasyAntiCheat bootstrapper, which starts
/// the game under anti-cheat where the patch can't be loaded. `RecRoom.exe` is
/// the game itself.
const STELLA_LAUNCH_TARGETS: [&str; 1] = ["RecRoom.exe"];

/// [`find_game_exe`] for `network`'s client.
pub fn find_game_exe_for(network: config::Network, dir: &str) -> Option<String> {
    match network {
        // Rec EOS likewise: Recroom_Release.exe is Easy Anti-Cheat, which
        // refuses BepInEx.
        config::Network::Stella | config::Network::RecEos => {
            find_launch_target(Path::new(dir), &STELLA_LAUNCH_TARGETS, 0)
        }
        _ => find_game_exe(dir),
    }
}

fn find_launch_target(dir: &Path, targets: &[&str], depth: u32) -> Option<String> {
    if depth > 4 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;

    // Index this level in one pass, then decide — a second read_dir per name
    // would put the per-name cost straight back.
    let mut found: Vec<Option<std::path::PathBuf>> = vec![None; targets.len()];
    let mut subdirs: Vec<std::path::PathBuf> = Vec::new();

    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            subdirs.push(entry.path());
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        for (i, target) in targets.iter().enumerate() {
            if found[i].is_none() && name.eq_ignore_ascii_case(target) {
                found[i] = Some(entry.path());
            }
        }
    }

    if let Some(hit) = found.into_iter().flatten().next() {
        return Some(hit.to_string_lossy().to_string());
    }

    for subdir in subdirs {
        if let Some(hit) = find_launch_target(&subdir, targets, depth + 1) {
            return Some(hit);
        }
    }

    None
}

/// Call `visit` with the id of every running process whose image name is one
/// of `images`, until it returns true. Returns whether one did.
///
/// Walks the kernel's process snapshot directly rather than shelling out to
/// `tasklist`. The old implementation cost ~76ms per call — essentially all of
/// it process-creation overhead, which is why filtering by image name made no
/// difference — and this runs every 2 seconds for the whole session from the
/// game monitor. A snapshot walk over ~320 processes is about 1ms, spawns
/// nothing, and keeps the call off the blocking path of the async commands that
/// use it. It also stops the launcher from creating a hidden console process
/// twice a second, which is exactly the pattern antivirus heuristics flag.
#[cfg(target_os = "windows")]
fn find_process(images: &[&str], mut visit: impl FnMut(u32) -> bool) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: the snapshot handle is checked against INVALID_HANDLE_VALUE
    // before use and closed on every exit path. `entry` is zeroed and has its
    // `dwSize` set before the first call, which is what Process32FirstW
    // requires; both iteration calls receive that same initialised struct.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }

        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut found = false;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if image_name_matches(&entry.szExeFile, images) && visit(entry.th32ProcessID) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        let _ = CloseHandle(snapshot);
        found
    }
}

/// Returns true if any process with one of the given image names is running.
#[cfg(target_os = "windows")]
fn any_process_running(images: &[&str]) -> bool {
    find_process(images, |_| true)
}

/// Whether any process with one of the given image names is running *from
/// inside* one of `dirs`.
///
/// The name alone is not enough: official Rec Room's own client is
/// `RecRoom.exe` too. Matched by name only, playing it made the launcher
/// report its game as running — blocking downloads, uninstalls and network
/// switches, and turning PLAY into a Stop Game that closed the other game.
#[cfg(target_os = "windows")]
fn client_running(images: &[&str], dirs: &[String]) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    find_process(images, |pid| {
        // SAFETY: a null handle is checked for, and a real one is closed
        // straight after the query.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let inside = process_image_path(handle).is_some_and(|exe| in_client_dirs(&exe, dirs));
            let _ = CloseHandle(handle);
            inside
        }
    })
}

/// The full path of the executable behind a process handle, which must carry
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
///
/// # Safety
/// `handle` must be a live process handle.
#[cfg(target_os = "windows")]
unsafe fn process_image_path(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};
    // The longest path Windows has.
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    if QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
}

/// Whether an executable's path lies in one of the launcher's client folders.
fn in_client_dirs(exe: &str, dirs: &[String]) -> bool {
    dirs.iter().any(|dir| config::path_is_inside_dir(exe, dir))
}

/// Every network's client folder: the only places a game this launcher
/// installed, and so may watch or stop, can be running from. Plus Vanilla's
/// own launcher's game folder: Vanilla's PC build runs only from there (Play
/// starts it through that launcher), so that's where to watch and stop it.
fn client_dirs(app: &tauri::AppHandle) -> Vec<String> {
    let cfg = config::current(app);
    let mut dirs: Vec<String> = config::Network::ALL
        .into_iter()
        .map(|network| config::get_client_dir_for(app, &cfg, network))
        .collect();
    dirs.extend(vanilla_launcher_game_dir());
    dirs
}

/// The folder Vanilla's own launcher installs its game into:
/// `<installDirectory>\Vanilla`, from the launcher's settings. Only that exact
/// folder — never the whole install directory, which a user could point at a
/// broad folder that also holds official Rec Room.
pub(crate) fn vanilla_launcher_game_dir() -> Option<String> {
    let data = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("VanillaLauncher");
    let settings = std::fs::read_to_string(data.join("settings.json")).ok();
    vanilla_game_candidates(settings.as_deref(), &data)
        .into_iter()
        .next()
        .map(|dir| dir.to_string_lossy().to_string())
}

/// Whether a `PROCESSENTRY32W` image name matches any of `images`.
///
/// The field is a fixed-size UTF-16 buffer padded with NULs, so it is truncated
/// at the first NUL before comparing — the whole 260-wchar buffer would never
/// match anything.
#[cfg(target_os = "windows")]
fn image_name_matches(sz_exe_file: &[u16], images: &[&str]) -> bool {
    let len = sz_exe_file
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(sz_exe_file.len());
    let name = &sz_exe_file[..len];
    images.iter().any(|img| utf16_eq_ignore_ascii_case(name, img))
}

/// Case-insensitive comparison of a UTF-16 slice against an ASCII string,
/// without allocating.
///
/// Decoding each name into a `String` instead cost 300-odd allocations per
/// poll, which was most of the walk's runtime. Comparing lengths up front is
/// valid because every image name we look for is ASCII, so one byte is one
/// UTF-16 unit; a non-ASCII `ascii` argument simply fails to match, which is
/// the safe answer.
#[cfg(target_os = "windows")]
fn utf16_eq_ignore_ascii_case(utf16: &[u16], ascii: &str) -> bool {
    if utf16.len() != ascii.len() {
        return false;
    }
    utf16
        .iter()
        .zip(ascii.bytes())
        .all(|(&u, a)| u < 128 && (u as u8).eq_ignore_ascii_case(&a))
}

#[cfg(not(target_os = "windows"))]
fn any_process_running(_images: &[&str]) -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn client_running(_images: &[&str], _dirs: &[String]) -> bool {
    false
}

/// Terminate every running process with one of the given image names that
/// runs from inside one of `dirs` — see [`client_running`] for why the folder
/// matters. Returns the number that were asked to stop.
///
/// Walks the same snapshot as [`client_running`] instead of spawning
/// `taskkill.exe` once per name. `taskkill` was resolved by bare name, so it
/// came out of whatever the process search order turned up first, and three
/// spawns cost ~75 ms each of pure process-creation overhead to do what two
/// handle calls do. This also stops Stop Game from creating hidden console
/// processes, which is the pattern antivirus heuristics flag.
#[cfg(target_os = "windows")]
fn terminate_client_processes(images: &[&str], dirs: &[String]) -> u32 {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    let mut killed = 0;
    find_process(images, |pid| {
        // SAFETY: a null handle (access denied, or the process exited between
        // the snapshot and here) is skipped; a real one is closed straight
        // after. One handle both reads the path and terminates, so the process
        // checked is the process stopped even if its id is reused meanwhile.
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if !handle.is_null() {
                let ours = process_image_path(handle).is_some_and(|exe| in_client_dirs(&exe, dirs));
                if ours && TerminateProcess(handle, 1) != 0 {
                    killed += 1;
                }
                let _ = CloseHandle(handle);
            }
        }
        false
    });
    killed
}

#[cfg(not(target_os = "windows"))]
fn terminate_client_processes(_images: &[&str], _dirs: &[String]) -> u32 {
    0
}

/// Read a `REG_DWORD` value, or `None` if the key, the value or the type isn't
/// there.
///
/// Replaces two `reg.exe query` spawns whose stdout was then parsed by
/// splitting on whitespace and guessing at hex vs decimal. `reg` was resolved
/// by bare name — so it came from wherever the process search order found one
/// — each call cost a process creation, and the output is localised on some
/// Windows installs, which the "find the line containing the value name" parse
/// quietly depended on not being.
#[cfg(target_os = "windows")]
fn reg_dword(hive: windows_sys::Win32::System::Registry::HKEY, path: &str, value: &str) -> Option<u32> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_REG_DWORD};

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (path, value) = (wide(path), wide(value));
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both strings are NUL-terminated; `data` is exactly the four bytes
    // `size` promises, and RRF_RT_REG_DWORD makes the call fail rather than
    // write anything else into it.
    let rc = unsafe {
        RegGetValueW(
            hive,
            path.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (rc == ERROR_SUCCESS).then_some(data)
}

// ─── Tauri Commands ───────────────────────────────────────────────────────────

/// Whether a game this launcher installed — either network's — is running.
pub fn game_running(app: &tauri::AppHandle) -> bool {
    client_running(&GAME_EXES, &client_dirs(app))
}

/// Checks whether a game this launcher installed is currently running.
#[tauri::command(async)]
pub fn check_game_running(app: tauri::AppHandle) -> bool {
    game_running(&app)
}

/// Whether any Rec Room client is running, wherever it was started from. Used
/// by Stella's friends connection, which must not run alongside the game's.
pub fn rec_room_running() -> bool {
    any_process_running(&GAME_EXES)
}

/// Checks whether `steam.exe` is currently running.
#[tauri::command(async)]
pub fn check_steam() -> bool {
    any_process_running(&["steam.exe"])
}

/// Returns true if the required Rec Room Steam app (appid 92) is installed,
/// by reading the Steam per-user registry key. Returns true on non-Windows so
/// the check never blocks there.
#[tauri::command(async)]
pub fn check_required_steam_app() -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Registry::HKEY_CURRENT_USER;
        reg_dword(
            HKEY_CURRENT_USER,
            r"Software\Valve\Steam\Apps\92",
            "Installed",
        ) == Some(1)
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}

/// Start the game. On the blocking pool: a Stella launch waits for the game to
/// load its code before the patch goes in (up to two minutes on a slow first
/// start) and then for the patch to load, and a sync command marked `async`
/// runs on one of the async runtime's few worker threads, which every other
/// command, download and connection shares.
#[tauri::command]
pub async fn launch_game(
    app: tauri::AppHandle,
    config: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let result = tokio::task::spawn_blocking(move || launch_game_impl(app, config))
        .await
        .unwrap_or_else(|e| Err(format!("The launch failed: {}", e)));
    match result {
        Ok(val) => Ok(val),
        Err(err) => Ok(json!({ "success": false, "error": err })),
    }
}

/// Spawn a legacy `.bat` launch script via `cmd /c start`. The path is quoted
/// explicitly (via `raw_arg`) so a client directory containing characters like
/// `&` — legal in Windows folder names — can't break cmd's parsing.
#[cfg(target_os = "windows")]
fn spawn_bat(exe_path: &str, work_dir: &str) -> std::io::Result<std::process::Child> {
    use std::os::windows::process::CommandExt;
    let line = format!("start \"\" \"{}\"", exe_path);
    let mut cmd = Command::new("cmd.exe");
    cmd.raw_arg("/c")
        .raw_arg(line)
        .current_dir(work_dir)
        .creation_flags(0x00000008); // DETACHED_PROCESS
    cmd.spawn()
}

/// There is no `cmd.exe` off Windows, and a `.bat` is not a thing to run there.
/// Failing explicitly beats calling a binary that cannot exist and reporting
/// whatever the OS says about the missing file.
#[cfg(not(target_os = "windows"))]
fn spawn_bat(_exe_path: &str, _work_dir: &str) -> std::io::Result<std::process::Child> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Launching a .bat client is only supported on Windows.",
    ))
}

/// A room name as Rec Room allows them: 1–64 letters, digits, `_` and `-`.
fn valid_room_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Locate Vanilla's own launcher executable. Its installer puts it per-machine
/// under Program Files, so look there (and the 32-bit Program Files) for
/// `vanillalauncher.exe`. Returns the full path if it's present.
#[cfg(target_os = "windows")]
pub fn find_vanilla_launcher() -> Option<String> {
    const REL: &str = r"Vanilla Launcher\vanillalauncher.exe";
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(base) = std::env::var(var) {
            let p = Path::new(&base).join(REL);
            if p.is_file() {
                return Some(p.to_string_lossy().to_string());
            }
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
pub fn find_vanilla_launcher() -> Option<String> {
    None
}

/// EXPERIMENTAL auto-play for Vanilla. Opens Vanilla's own launcher and then,
/// through Windows UI Automation, presses its real "Play" button — so the
/// launcher (signed in) does the actual launch and mints the anti-cheat session
/// itself. Nothing is forged; this only automates a click the user would make.
///
/// It is best-effort: the launcher has several screens (sign-in, download,
/// update, locate, play), and we only press a button named "Play" that is
/// enabled and on-screen. If none appears within the window, the launcher is
/// simply left open for the user to finish in (the same as the plain hand-off),
/// and `autoplay` comes back `no_play`. WebView2 exposes its DOM to UI
/// Automation only when accessibility is active, so the launcher is started
/// with `--force-renderer-accessibility`.
///
/// Caveats worth keeping in mind: this drives another app's UI, so it can break
/// when that UI changes, and an anti-cheat launcher could in principle treat
/// outside automation as tampering — so it's experimental and should be tested
/// on a throwaway account first.
#[tauri::command]
pub async fn autoplay_vanilla(app: tauri::AppHandle, mode: Option<String>) -> Result<Value, String> {
    let Some(launcher) = find_vanilla_launcher() else {
        return Ok(json!({ "success": false, "autoplay": "no_launcher" }));
    };
    // Only two modes exist in Vanilla's launcher; anything else is Screen.
    let vr = mode.as_deref() == Some("vr");

    #[cfg(target_os = "windows")]
    {
        let events = app.clone();
        let (status, mode_set) =
            tokio::task::spawn_blocking(move || run_vanilla_play_automation(&events, &launcher, vr))
                .await
                .map_err(|e| format!("Auto-play task failed: {}", e))??;
        // Launch pressed: same "starting" state as our own launches, so Home
        // shows STOP and the monitor waits for the game to appear before it
        // would report it closed (see `start_game_monitor`).
        if status == "invoked" {
            GAME_SEEN_SINCE_LAUNCH.store(false, Ordering::SeqCst);
            LAUNCH_GRACE_POLLS.store(GRACE_POLLS_AFTER_LAUNCH, Ordering::SeqCst);
            GAME_RUNNING_STATE.store(true, Ordering::SeqCst);
            let _ = app.emit("game-state", json!({ "running": true }));
        }
        Ok(json!({
            "success": status != "no_launcher",
            "autoplay": status,
            "modeSet": mode_set,
        }))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, launcher, vr);
        Err("Auto-play is only supported on Windows.".into())
    }
}

/// Folders where Vanilla's launcher keeps the game: `<installDirectory>\Vanilla`
/// (its layout), then `<installDirectory>` itself. `installDirectory` falls back
/// to `<launcher data>\Installs` when the settings are missing or unreadable.
fn vanilla_game_candidates(settings: Option<&str>, launcher_data: &Path) -> Vec<std::path::PathBuf> {
    let base = settings
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("installDirectory")?.as_str().map(str::to_string))
        .filter(|s| !s.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| launcher_data.join("Installs"));
    vec![base.join("Vanilla"), base]
}

/// Final results the automation script can end with.
#[cfg(target_os = "windows")]
const VANILLA_AUTOPLAY_RESULTS: [&str; 7] = [
    "invoked",
    "needs_mode",
    "no_play",
    "no_launcher",
    "needs_client",
    "needs_update",
    "needs_busy",
];

/// Start Vanilla's launcher if it isn't already running and press its Play
/// button via UI Automation. While it runs, each launcher screen it recognises
/// (`signin`, `client`, `update`, `busy`) is sent to the UI as a
/// `vanilla-autoplay` event. Returns `"invoked"`, `"needs_client"` (signed in
/// but the game isn't installed in the launcher), `"needs_update"`,
/// `"needs_busy"` (the launcher is downloading/updating the game), or
/// `"no_play"` (no Play button in time — e.g. never signed in). Blocking: polls
/// for up to ~45s, or ~5 min while a sign-in screen is up.
#[cfg(target_os = "windows")]
fn run_vanilla_play_automation(
    app: &tauri::AppHandle,
    launcher: &str,
    vr: bool,
) -> Result<(String, bool), String> {
    use std::io::{BufRead, BufReader};
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    // Single-quoted PowerShell string; double any quote in the path.
    let safe_path = launcher.replace('\'', "''");
    let script = format!("{}{}", crate::vanilla_launcher::UIA_PRELUDE, VANILLA_PLAY_PS)
        .replace("__LAUNCHER_PATH__", &safe_path)
        .replace("__MODE__", if vr { "vr" } else { "screen" });

    let mut child = Command::new(crate::defender::get_powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .spawn()
        .map_err(|e| format!("Couldn't run the launcher automation: {}", e))?;

    let mut result = "no_play".to_string();
    let mut mode_set = false;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let line = line.trim();
            if let Some(state) = line.strip_prefix("state:") {
                let _ = app.emit("vanilla-autoplay", state);
            } else if line == "mode:set" {
                mode_set = true;
            } else if VANILLA_AUTOPLAY_RESULTS.contains(&line) {
                result = line.to_string();
            }
        }
    }
    let _ = child.wait();
    Ok((result, mode_set))
}

/// The UI Automation routine, run via PowerShell. Starts the launcher (with
/// WebView2 accessibility forced on) if it isn't up, then for ~45s looks across
/// the launcher's WebView2 child windows for its Launch control, picks the
/// wanted Screen/VR mode, and presses Launch. `__LAUNCHER_PATH__` and
/// `__MODE__` are filled in by [`run_vanilla_play_automation`].
#[cfg(target_os = "windows")]
const VANILLA_PLAY_PS: &str = r#"
# Helpers (Find-LauncherWindow, Restore-LauncherWindow, …) come from
# vanilla_launcher::UIA_PRELUDE, prepended by run_vanilla_play_automation.
if (-not (Test-LauncherUp)) { Start-Launcher }

# 45s to reach a Play button; extended to 5 min once a sign-in screen shows, so
# the user can sign in and we press Play right after.
$deadline = (Get-Date).AddSeconds(45)
$status = 'no_play'
$top = [IntPtr]::Zero
$last = ''; $seen = 0

# Play mode wanted by Rec EOS. The launcher has a Screen/VR choice once the game
# is installed; we pick it (a switch named for the mode, or a Play button that
# names it) before pressing Play, and never press a Play that names the other mode.
$want = '__MODE__'
$vrRx = '(?i)\b(vr|virtual reality)\b'
$scRx = '(?i)\b(screen|desktop|flat|2d)\b'
if ($want -eq 'vr') { $wantRx = $vrRx; $otherRx = $scRx } else { $wantRx = $scRx; $otherRx = $vrRx }
$modeCtlRx = if ($want -eq 'vr') { '(?i)^(vr|vr mode|virtual reality)$' } else { '(?i)^(screen|screen mode|desktop|desktop mode|flat)$' }
$playRx = '(?i)^[^a-z]*(play|launch)\b[^a-z]*((in\s+)?(vr|virtual reality|screen|desktop|flat|2d)(\s+mode)?)?[^a-z]*$'
$ctlTypes = 'ControlType.Button','ControlType.RadioButton','ControlType.TabItem','ControlType.ListItem','ControlType.MenuItem','ControlType.CheckBox'
$modeSet = $false; $modeTries = 0; $otherOnly = 0
# The launcher's game page (seen with the game installed): a clickable
# `launch-bar` group holding the text "Launch", a "Launch mode" group with VR /
# Screen buttons (the chosen one has class `is-selected`), and Uninstall. The
# text itself isn't clickable, so Clickable-Ancestor presses the bar.
$script:vrOff = $false
function Pick-Mode($e) {
  if ($script:vrOff) {
    try { $p = $e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern); if ($p.Current.ToggleState -ne 'Off') { $p.Toggle() }; return $true } catch { return $false }
  }
  try { $p = $e.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern); if (-not $p.Current.IsSelected) { $p.Select() }; return $true } catch {}
  try { $p = $e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern); if ($p.Current.ToggleState -ne 'On') { $p.Toggle() }; return $true } catch {}
  try { $e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke(); return $true } catch {}
  return $false
}
while ((Get-Date) -lt $deadline) {
  $top = Find-LauncherWindow
  if ($top -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 1000; continue }
  Restore-LauncherWindow $top
  $playWant = $null; $playPlain = $null; $playOther = $null; $modeCtl = $null
  $signin = $false; $client = $false; $update = $false; $busy = $false; $loading = $false
  $wantSelected = $false; $markerSeen = $false
  foreach ($all in @(,(Get-LauncherElements $top))) {
    try {
      foreach ($e in $all) {
        $c = $e.Current
        $n = "$($c.Name)".Trim()
        $type = $c.ControlType.ProgrammaticName
        $usable = $c.IsEnabled -and -not $c.IsOffscreen
        if ($type -eq 'ControlType.Button' -and $usable -and $n -match $playRx) {
          if ($n -match $wantRx) { $playWant = $e } elseif ($n -match $otherRx) { $playOther = $e } else { $playPlain = $e }
        }
        elseif ($type -eq 'ControlType.Text' -and $usable -and $n -match '(?i)^(launch|play)$' -and -not $playPlain) {
          $bar = Clickable-Ancestor $e
          if ($bar) { $playPlain = $bar }
        }
        elseif ($usable -and $ctlTypes -contains $type -and $n -match $modeCtlRx) {
          $modeCtl = $e
          if ("$($c.ClassName)" -match '(^|\s)is-selected(\s|$)') { $wantSelected = $true }
        }
        if ($type -eq 'ControlType.Button' -and $n -match '(?i)^(vr|screen)$' -and "$($c.ClassName)" -match '(^|\s)is-selected(\s|$)') { $markerSeen = $true }
        # A lone "VR" on/off switch: for Screen, the "switch" is turning it off.
        elseif ($usable -and $want -ne 'vr' -and $type -eq 'ControlType.CheckBox' -and $n -match '(?i)^(vr|vr mode)$') { $modeCtl = $e; $script:vrOff = $true }
        # Login screen: Username/Password boxes + "Login" button. With "Remember
        # me" it shows ~3s under a "Loading" group while it signs in by itself.
        if ($c.IsPassword -or $n -match '(?i)^(sign|log)\s?in\b|^login$|^continue with') { $signin = $true }
        if ($n -eq 'Loading') { $loading = $true }
        if ($n -match '(?i)^(locate installation|download|install)$') { $client = $true }
        if ($n -match '(?i)^update( now| game)?$') { $update = $true }
        if ($n -match '(?i)^(downloading|installing|updating|verifying|extracting)\b') { $busy = $true }
      }
    } catch {}
  }
  # Select the wanted mode first (then press Play on the next pass, once the
  # launcher has updated), unless a Play button already names it.
  # When the launcher marks the chosen mode (`is-selected`), that is the truth
  # each pass; a switch without a marker counts as set once picked.
  if ($markerSeen) { $modeSet = $wantSelected }
  if (-not $playWant -and $modeCtl -and -not $modeSet -and $modeTries -lt 3) {
    $modeTries++
    if ((Pick-Mode $modeCtl) -and -not $markerSeen) { $modeSet = $true }
    Start-Sleep -Milliseconds 700
    continue
  }
  # Mode switch present but the pick never took: don't launch the wrong mode.
  if ($modeCtl -and -not $modeSet) { Show-Launcher $top; $status = 'needs_mode'; break }
  $play = $null
  if ($playWant) { $play = $playWant; $modeSet = $true }
  elseif ($playPlain) { $play = $playPlain }
  if ($play) {
    try {
      # Not minimised afterwards: the launcher would save the minimised size
      # (160x28) as its window size. The game's window covers it anyway.
      $play.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
      if ($modeSet) { Say 'mode:set' }
      $status = 'invoked'
      break
    } catch {}
  }
  # Only a Play for the other mode, and no way to switch: leave it to the user.
  if ($playOther -and -not $play) {
    $otherOnly++
    if ($otherOnly -ge 3) { Show-Launcher $top; $status = 'needs_mode'; break }
  }
  $state = if ($loading) { '' } elseif ($signin) { 'signin' } elseif ($busy) { 'busy' } elseif ($update) { 'update' } elseif ($client) { 'client' } else { '' }
  if ($state -eq $last) { $seen++ } else { $seen = 1; $last = $state }
  # Act on a screen only once it has held (skips loading flicker); the login
  # form gets longer, since it's also shown briefly during auto sign-in.
  $need = if ($state -eq 'signin') { 4 } else { 2 }
  if ($state -and $seen -eq $need) {
    Say "state:$state"
    Show-Launcher $top
    if ($state -eq 'signin') { $deadline = (Get-Date).AddMinutes(5) }
    else { $status = "needs_$state"; break }
  }
  Start-Sleep -Milliseconds 1000
}
if ($status -eq 'no_play') { Show-Launcher $top }
Say $status
"#;

fn launch_game_impl(
    app: tauri::AppHandle,
    config: serde_json::Value,
) -> Result<serde_json::Value, String> {
    // 1. Check if already running
    if game_running(&app) {
        return Err("Game already running.".into());
    }

    // 2. Play mode (legacy clients accept a +mode arg; the new client ignores it)
    let play_mode = config
        .get("playMode")
        .and_then(|v| v.as_str())
        .unwrap_or("screen");

    // 3. Resolve client directory + game executable for the selected network
    let network = config::Network::parse(config.get("network").and_then(|v| v.as_str()));

    // Vanilla's PC build is an anti-cheat'd client that only runs when
    // Vanilla's own launcher starts it: the launcher (with the user signed in)
    // creates a one-time signed session the game checks for on startup, and
    // starting RecRoom.exe any other way fails with an anti-cheat violation.
    // So "Play" on Vanilla hands off to that launcher instead of spawning the
    // game — it opens, the user plays from there, and Rec EOS never starts the
    // Vanilla game itself. (When Vanilla give us a launch command we can pass
    // it here for a one-click start.)
    if network == config::Network::Vanilla {
        let Some(launcher) = find_vanilla_launcher() else {
            return Err(
                "Vanilla's launcher isn't installed. Install the Vanilla Launcher first, \
                 then press Play again."
                    .into(),
            );
        };
        let work_dir = Path::new(&launcher)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut cmd = Command::new(&launcher);
        cmd.current_dir(&work_dir);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x00000008); // DETACHED_PROCESS
        }
        cmd.spawn()
            .map_err(|e| format!("Couldn't open Vanilla's launcher: {}", e))?;
        return Ok(json!({ "success": true, "handoff": "vanilla-launcher" }));
    }

    let cfg = config::current(&app);
    let client_dir = config::get_client_dir_for(&app, &cfg, network);

    // A download, file check or repair is reading or rewriting these files.
    if crate::download::client_busy() {
        return Err("Wait for the download or file check to finish before playing.".into());
    }

    // Half a client, from an install that was cut off: whatever exe is in
    // there is not one to run. See `download::INCOMPLETE_MARKER`.
    if crate::download::install_incomplete(&client_dir) {
        return Err("The last install of the client didn't finish. Download it again from Home.".into());
    }

    // Prefer the saved exe path; otherwise search the client dir for a known exe.
    let mut exe_path = match network {
        // The frontend passes Radium's exe path as a flat config field.
        config::Network::Radium => config
            .get("gameExePath")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        // Vanilla's and Stella's install state live in their own config
        // sub-objects, so read them from disk rather than from the frontend's
        // flat (Radium) field.
        config::Network::Vanilla => cfg.vanilla.game_exe_path.clone(),
        config::Network::Stella => cfg.stella.game_exe_path.clone(),
        config::Network::RecEos => cfg.receos.game_exe_path.clone(),
    };
    // Stella only ever launches the game itself; see `STELLA_LAUNCH_TARGETS`.
    let wrong_stella_exe = matches!(network, config::Network::Stella | config::Network::RecEos)
        && !Path::new(&exe_path)
            .file_name()
            .is_some_and(|f| f.eq_ignore_ascii_case(STELLA_LAUNCH_TARGETS[0]));
    // The exe path arrives from the frontend for Radium, so it is confirmed to
    // live inside the resolved client directory before anything is spawned —
    // the same containment check `check_install` applies. Without it, anything
    // that could reach the IPC layer (a compromised API feeding the room and
    // photo views, say) could name any executable on disk and have the
    // launcher run it.
    if exe_path.is_empty()
        || wrong_stella_exe
        || !Path::new(&exe_path).exists()
        || !config::path_is_inside_dir(&exe_path, &client_dir)
    {
        exe_path = find_game_exe_for(network, &client_dir).unwrap_or_default();
    }
    if exe_path.is_empty() {
        return Err(format!(
            "Game executable not found in: {}\n\nPlease download the client first.",
            client_dir
        ));
    }
    // Without the mod the game would go looking for Rec Room's own servers.
    if network == config::Network::RecEos && !crate::receos::mod_installed(&client_dir) {
        return Err("Rec EOS isn't installed in this client yet. Press UPDATE on Home.".into());
    }

    let exe = Path::new(&exe_path);
    let work_dir = exe
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let file_lower = exe
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // 6. Spawn the process (DETACHED_PROCESS = 0x00000008)
    #[cfg(target_os = "windows")]
    use std::os::windows::process::CommandExt;

    let mode_arg = if play_mode == "vr" { "+mode:vr" } else { "+mode:screen" };
    let is_bat = file_lower.ends_with(".bat");
    // Stella starts the game and loads its patch into it, which takes a moment
    // longer than a plain spawn and fails the launch if the patch can't go in.
    // Stella can start straight into a friend's room: `+join:<account id>` is
    // the connect string the game publishes to Steam for "Join Game" (read off
    // its rich presence, 2026-10-02), and the game itself does the joining —
    // the server decides whether that player can be joined.
    let join_arg = config
        .get("joinPlayerId")
        .and_then(|v| v.as_i64())
        .filter(|&id| id > 0 && matches!(network, config::Network::Stella | config::Network::RecEos))
        .map(|id| format!("+join:{id}"));
    // Likewise a room, by name: `+roomname:` sits beside `+join:` in the
    // game's own list of launch arguments. Only a plain room name is passed
    // on (letters, digits, `_`, `-`), never anything that could read as more.
    let room_arg = config
        .get("joinRoomName")
        .and_then(|v| v.as_str())
        .map(|n| n.trim().trim_start_matches('^'))
        .filter(|n| matches!(network, config::Network::Stella | config::Network::RecEos) && valid_room_name(n))
        .map(|n| format!("+roomname:{n}"));
    // A friend in a private copy of a room can't be joined that way (Stella
    // answers "Room does not exist"); the game starts plainly instead, and once
    // it has landed the launcher sends them a join request, as the game's own
    // "Request to Join" does. See `stella_api::request_join_after_launch`.
    let request_join = config
        .get("requestJoinPlayerId")
        .and_then(|v| v.as_i64())
        .filter(|&id| id > 0 && network == config::Network::Stella);
    let log_mark = request_join.map(|_| crate::stella_api::game_log_mark());
    let stella_pid = if network == config::Network::Stella {
        let mut args = vec![mode_arg];
        // A player to join wins over a room; the frontend only sends one.
        if let Some(join) = join_arg.as_ref().or(room_arg.as_ref()) {
            args.push(join.as_str());
        }
        Some(crate::stella::launch(&app, exe, &work_dir, &args)?)
    } else {
        None
    };
    let child = if let Some(pid) = stella_pid {
        pid
    } else if network == config::Network::RecEos {
        // The game itself, with the mode and a room or player to start in.
        let mut cmd = Command::new(exe);
        cmd.arg(mode_arg);
        if let Some(target) = join_arg.as_ref().or(room_arg.as_ref()) {
            cmd.arg(target);
        }
        cmd.current_dir(&work_dir);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x00000008); // DETACHED_PROCESS
        cmd.spawn()
            .map_err(|e| format!("Failed to launch game executable: {}", e))?
            .id()
    } else if is_bat {
        spawn_bat(&exe_path, &work_dir)
            .map_err(|e| format!("Failed to launch batch file: {}", e))?
            .id()
    } else {
        let mut cmd = Command::new(exe);
        // Legacy RecRoom.exe accepts a +mode argument; the new Recroom_Release.exe
        // is launched plain.
        if file_lower == "recroom.exe" {
            cmd.arg(mode_arg);
        }
        cmd.current_dir(&work_dir);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(0x00000008); // DETACHED_PROCESS
        cmd.spawn()
            .map_err(|e| format!("Failed to launch game executable: {}", e))?
            .id()
    };

    // A .bat launch goes through `cmd /c start`, so `child` is the transient
    // cmd.exe wrapper (which exits immediately), not the game — its PID is
    // meaningless. Only report a PID for a direct executable launch.
    let pid_value = if is_bat && stella_pid.is_none() { Value::Null } else { Value::from(child) };

    // 7. Enter the post-launch "grace" state and mark the game as running.
    //
    // The background monitor must not report "closed" until the game process
    // has actually appeared: a `.bat`/`start` launch (and slow first-time Unity
    // startup) can take several seconds, during which no game process exists yet.
    // Without this, the monitor's first poll would see no process, flip the
    // state back to not-running, and fire a spurious "Game closed". See
    // `start_game_monitor`.
    GAME_SEEN_SINCE_LAUNCH.store(false, Ordering::SeqCst);
    LAUNCH_GRACE_POLLS.store(GRACE_POLLS_AFTER_LAUNCH, Ordering::SeqCst);
    GAME_RUNNING_STATE.store(true, Ordering::SeqCst);
    let _ = app.emit("game-state", json!({ "running": true }));

    if let (Some(player_id), Some(mark)) = (request_join, log_mark) {
        crate::stella_api::request_join_after_launch(app.clone(), player_id, mark);
    }

    // 8. Optionally minimize the launcher
    let minimize = config
        .get("minimizeOnLaunch")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if minimize {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.minimize();
        }
    }

    // 9. Optionally close the launcher
    let close = config
        .get("closeOnLaunch")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if close || cfg.close_on_launch {
        if cfg.run_in_background {
            // Tray mode: the setting reads "Hide launcher when game starts",
            // and quitting here would also stop the notification pop-ups
            // the tray exists to keep running.
            crate::background::hide_main(&app);
        } else {
            app.exit(0);
        }
    }

    Ok(json!({ "success": true, "pid": pid_value }))
}

/// Forcibly stops the game this launcher started: every recognised game
/// process running from one of its client folders.
///
/// Covers both the current client (`Recroom_Release.exe`) and the legacy one
/// (`RecRoom.exe`); see [`GAME_EXES`]. A Rec Room running from anywhere else —
/// official Rec Room through Steam — is not the launcher's to close.
#[tauri::command(async)]
pub fn kill_game(app: tauri::AppHandle) -> bool {
    // Off Windows nothing is found and `game_running` already reports false,
    // so nothing can ask for this in the first place.
    #[cfg(target_os = "windows")]
    {
        terminate_client_processes(&GAME_EXES, &client_dirs(&app));
        true
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        false
    }
}

/// Queries the Windows registry to determine whether Smart App Control is
/// enabled. Returns `{ enabled: bool, state: i32 }`.
///
/// `state` is Windows' own value: 0 off, 1 enforcing, 2 evaluation. `-1` means
/// the value isn't there at all, which is every Windows build before 11 22H2
/// — and every platform that isn't Windows, where there is no such feature to
/// ask about rather than a query to run.
#[tauri::command(async)]
pub fn check_smart_app_control() -> serde_json::Value {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE;
        match reg_dword(
            HKEY_LOCAL_MACHINE,
            r"SYSTEM\CurrentControlSet\Control\CI\Policy",
            "VerifiedAndReputablePolicyState",
        ) {
            Some(state) => json!({ "enabled": state == 1 || state == 2, "state": state }),
            None => json!({ "enabled": false, "state": -1 }),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        json!({ "enabled": false, "state": -1 })
    }
}

// ─── Background Game Monitor ─────────────────────────────────────────────────

/// Tracks the last known running state so the monitor only emits on transitions.
static GAME_RUNNING_STATE: AtomicBool = AtomicBool::new(false);

/// Whether the game process has been observed at least once since the last
/// launch. Until it has, the monitor treats "not running" as "not started yet"
/// rather than "closed" (see `LAUNCH_GRACE_POLLS`).
static GAME_SEEN_SINCE_LAUNCH: AtomicBool = AtomicBool::new(false);

/// Remaining monitor polls during which a premature "closed" is suppressed
/// after a launch. Decremented once per poll; set by `launch_game`.
static LAUNCH_GRACE_POLLS: AtomicU64 = AtomicU64::new(0);

/// Length of the post-launch grace window, in monitor polls. At the 2s poll
/// interval this is ~16s — enough for a slow `.bat`/`start` launch or a cold
/// Unity start to make the game process appear in the process snapshot.
const GRACE_POLLS_AFTER_LAUNCH: u64 = 8;

/// Decide whether the monitor should emit a `game-state` transition this poll.
///
/// Returns `false` (no emit) when the state is unchanged, and also when a
/// "closed" transition would fire prematurely — i.e. the process hasn't been
/// seen yet and we're still inside the post-launch grace window.
fn should_emit_game_state(
    running: bool,
    previous: bool,
    seen_since_launch: bool,
    grace_polls: u64,
) -> bool {
    if running == previous {
        return false;
    }
    if !running && !seen_since_launch && grace_polls > 0 {
        return false;
    }
    true
}

/// Spawns a background tokio task that polls `RecRoom.exe` every 2 seconds and
/// emits `game-state` events to the frontend whenever the running state changes.
pub fn start_game_monitor(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // Graceful shutdown: break if the main window is gone
            if app.get_webview_window("main").is_none() {
                break;
            }

            let running = game_running(&app);
            if running {
                GAME_SEEN_SINCE_LAUNCH.store(true, Ordering::SeqCst);
            }

            // Consume one grace poll if we're inside the post-launch window.
            let grace = LAUNCH_GRACE_POLLS.load(Ordering::SeqCst);
            if grace > 0 {
                LAUNCH_GRACE_POLLS.store(grace - 1, Ordering::SeqCst);
            }

            // Right after a launch the game process can take a few seconds to
            // appear; don't flip to not-running until it's been seen at least
            // once or the grace window has elapsed (see should_emit_game_state).
            let previous = GAME_RUNNING_STATE.load(Ordering::SeqCst);
            let seen = GAME_SEEN_SINCE_LAUNCH.load(Ordering::SeqCst);
            if should_emit_game_state(running, previous, seen, grace) {
                GAME_RUNNING_STATE.store(running, Ordering::SeqCst);
                let _ = app.emit("game-state", json!({ "running": running }));
            }
        }
    });
}

#[cfg(test)]
mod game_monitor_tests {
    use super::should_emit_game_state;

    #[test]
    fn no_emit_when_state_unchanged() {
        assert!(!should_emit_game_state(true, true, true, 0));
        assert!(!should_emit_game_state(false, false, false, 0));
    }

    #[test]
    fn suppresses_premature_close_during_grace() {
        // Just launched: previous=running(true), process not seen yet, grace left.
        // A "not running" poll must NOT emit "closed".
        assert!(!should_emit_game_state(false, true, false, 8));
    }

    #[test]
    fn emits_close_once_process_has_been_seen() {
        // The process appeared earlier (seen=true), then genuinely exited: emit,
        // even if grace polls remain.
        assert!(should_emit_game_state(false, true, true, 5));
    }

    #[test]
    fn emits_close_when_grace_elapsed_without_appearing() {
        // Launch that never produced a process: after the grace window, report it.
        assert!(should_emit_game_state(false, true, false, 0));
    }

    #[test]
    fn emits_running_transition_immediately() {
        // Process appearing is always emitted (grace only guards the close edge).
        assert!(should_emit_game_state(true, false, false, 8));
        assert!(should_emit_game_state(true, false, true, 0));
    }
}

#[cfg(all(test, target_os = "windows"))]
mod process_snapshot_tests {
    use super::{any_process_running, client_running, image_name_matches};

    /// A process counts as the launcher's game only when it runs from one of
    /// the client folders. Official Rec Room is `RecRoom.exe` too, and matching
    /// by name alone made Stop Game close it.
    #[test]
    fn a_process_counts_only_when_it_runs_from_a_client_folder() {
        let exe = std::env::current_exe().expect("current exe");
        let name = exe.file_name().expect("name").to_string_lossy().to_string();
        let own_dir = exe.parent().expect("dir").to_string_lossy().to_string();
        let elsewhere = std::env::temp_dir().join("radium-no-such-client").to_string_lossy().to_string();

        assert!(client_running(&[&name], &[elsewhere.clone(), own_dir]), "found in its own folder");
        assert!(!client_running(&[&name], &[elsewhere]), "running, but not from a client folder");
        assert!(!client_running(&[&name], &[]), "no client folders, nothing is ours");
    }

    /// The snapshot walk must find a process that is definitely running: this
    /// test binary. Guards against the whole enumeration silently returning
    /// false — the failure mode that would make the launcher believe the game
    /// had closed the moment it started.
    #[test]
    fn the_snapshot_finds_this_test_process() {
        let exe = std::env::current_exe().expect("current exe");
        let name = exe
            .file_name()
            .expect("exe file name")
            .to_string_lossy()
            .to_string();

        assert!(
            any_process_running(&[&name]),
            "expected to find this test process ({}) in the snapshot",
            name
        );
    }

    #[test]
    fn a_process_that_is_not_running_is_not_reported() {
        assert!(!any_process_running(&[
            "radium-launcher-no-such-process-9f3a1c.exe"
        ]));
    }

    #[test]
    fn an_empty_image_list_matches_nothing() {
        assert!(!any_process_running(&[]));
    }

    /// The image name arrives as a fixed 260-wchar buffer padded with NULs, so
    /// it has to be truncated at the first NUL before comparing. Comparison is
    /// case-insensitive because Windows process names are.
    #[test]
    fn a_nul_padded_image_name_is_truncated_before_comparing() {
        let mut buf = [0u16; 260];
        for (slot, ch) in buf.iter_mut().zip("RecRoom.exe".encode_utf16()) {
            *slot = ch;
        }

        assert!(image_name_matches(&buf, &["RecRoom.exe"]));
        assert!(image_name_matches(&buf, &["recroom.EXE"]));
        assert!(image_name_matches(&buf, &["Recroom_Release.exe", "RecRoom.exe"]));
        assert!(!image_name_matches(&buf, &["RecRoom"]));
        assert!(!image_name_matches(&buf, &["steam.exe"]));
    }

    /// A buffer with no NUL at all must not read past its end.
    #[test]
    fn an_unterminated_image_name_does_not_overrun() {
        let buf = [b'a' as u16; 260];
        assert!(!image_name_matches(&buf, &["a.exe"]));
        assert!(image_name_matches(&buf, &["a".repeat(260).as_str()]));
    }
}

#[cfg(test)]
mod vanilla_game_folder_tests {
    use super::vanilla_game_candidates;
    use std::path::{Path, PathBuf};

    #[test]
    fn uses_the_launchers_saved_install_directory() {
        let s = r#"{ "installDirectory": "D:\\Games\\Vanilla Installs", "installedVersions": {} }"#;
        let got = vanilla_game_candidates(Some(s), Path::new("C:\\data"));
        assert_eq!(
            got,
            vec![
                PathBuf::from("D:\\Games\\Vanilla Installs").join("Vanilla"),
                PathBuf::from("D:\\Games\\Vanilla Installs"),
            ]
        );
    }

    #[test]
    fn falls_back_to_installs_when_settings_are_missing_or_bad() {
        let data = Path::new("C:\\data");
        let want = vec![data.join("Installs").join("Vanilla"), data.join("Installs")];
        assert_eq!(vanilla_game_candidates(None, data), want);
        assert_eq!(vanilla_game_candidates(Some("not json"), data), want);
        assert_eq!(vanilla_game_candidates(Some(r#"{"installDirectory":"  "}"#), data), want);
    }
}

#[cfg(test)]
mod tests {
    use super::valid_room_name;

    #[test]
    fn room_names_are_plain() {
        assert!(valid_room_name("RecCenter"));
        assert!(valid_room_name("ink383s_infinteinkzone"));
        assert!(valid_room_name("Rec-Pulse"));
        assert!(!valid_room_name(""));
        assert!(!valid_room_name("^RecCenter"));
        assert!(!valid_room_name("Rec Center"));
        assert!(!valid_room_name("RecCenter +join:1"));
        assert!(!valid_room_name("@someone's Dorm"));
        assert!(!valid_room_name(&"a".repeat(65)));
    }
}
