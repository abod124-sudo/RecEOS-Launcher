//! Vanilla's own launcher settings, shown in Rec EOS (Settings → Vanilla).
//!
//! Vanilla's PC build runs through Vanilla's launcher, which keeps a handful of
//! game options in `%APPDATA%\VanillaLauncher\settings.json`. Rec EOS shows
//! them so nobody has to open that launcher to change them.
//!
//! The launcher is always the owner of that file:
//! * **Launcher closed:** only the one value in the file changes (the rest of
//!   the file is left byte for byte), and the launcher reads it on its next start.
//! * **Launcher open:** it holds its settings in memory and rewrites the file
//!   on its next save (even a window resize), so an edit to the file would be
//!   undone. The switch is flipped in the launcher's own Settings instead, and
//!   the launcher saves it itself.

use serde_json::{json, Value};
use std::path::PathBuf;

/// The PowerShell every Vanilla-launcher automation starts with (Play in
/// `game.rs`, the install sign-off in `vanilla_game.rs`, the Settings switches
/// here), so the lessons learned about that window live in one place:
/// * `Find-LauncherWindow` — the real window, by its Tauri class. Not
///   `Process.MainWindowHandle`: after a launch that points at a hidden helper.
/// * `Restore-LauncherWindow` — a minimised window shows UI Automation nothing
///   (restored without taking focus), and the launcher saves the 160x28 size
///   Windows reports while minimised, so a tiny window is resized to 900x532.
/// * `Test-LauncherUp` — the launcher itself, not the `--anticheat-guard`
///   processes, which share its exe name and outlive their games.
/// * `Show-Launcher` — the launcher works behind Rec EOS (kept there by
///   `game::VanillaLauncherBehind`) and comes forward only when it needs the
///   user (sign-in, a choice, an error).
/// * `Get-LauncherElements`, `Clickable-Ancestor`, `Say`.
///
/// `__LAUNCHER_PATH__` (used by `Start-Launcher`) is filled in by the caller.
#[cfg(target_os = "windows")]
pub(crate) const UIA_PRELUDE: &str = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$sig = @'
using System;using System.Text;using System.Runtime.InteropServices;
public class VlaWin {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct Rect { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out Rect r);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int hgt, uint flags);
}
'@
Add-Type -TypeDefinition $sig
$AE = [System.Windows.Automation.AutomationElement]
$T  = [System.Windows.Automation.Condition]::TrueCondition
$D  = [System.Windows.Automation.TreeScope]::Descendants
$IP = [System.Windows.Automation.InvokePattern]::Pattern
$TP = [System.Windows.Automation.TogglePattern]::Pattern
$RW = [System.Windows.Automation.TreeWalker]::RawViewWalker
function Say($s) { [Console]::Out.WriteLine($s); [Console]::Out.Flush() }
function Test-LauncherUp {
  @(Get-CimInstance Win32_Process -Filter "Name='vanillalauncher.exe'" -ErrorAction SilentlyContinue |
    Where-Object { "$($_.CommandLine)" -notmatch '--anticheat-guard' }).Count -gt 0
}
function Start-Launcher {
  $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--force-renderer-accessibility'
  Start-Process -FilePath '__LAUNCHER_PATH__'
}
function Find-LauncherWindow {
  $script:vlaPids = @(Get-Process vanillalauncher -ErrorAction SilentlyContinue | ForEach-Object Id)
  $script:found = [IntPtr]::Zero
  if (-not $script:vlaPids) { return [IntPtr]::Zero }
  [VlaWin]::EnumWindows([VlaWin+EnumProc]{ param($h,$l)
    $wp = 0; [VlaWin]::GetWindowThreadProcessId($h, [ref]$wp) | Out-Null
    if ($script:vlaPids -contains [int]$wp) {
      $c = New-Object System.Text.StringBuilder 64; [VlaWin]::GetClassName($h, $c, 64) | Out-Null
      if ($c.ToString() -eq 'Tauri Window') { $script:found = $h; return $false }
    }
    return $true
  }, [IntPtr]::Zero) | Out-Null
  return $script:found
}
function Restore-LauncherWindow($h) {
  if ([VlaWin]::IsIconic($h)) { [VlaWin]::ShowWindow($h, 4) | Out-Null; Start-Sleep -Milliseconds 800 }   # SW_SHOWNOACTIVATE
  $r = New-Object VlaWin+Rect
  if (-not [VlaWin]::IsIconic($h) -and [VlaWin]::GetWindowRect($h, [ref]$r) -and (($r.R - $r.L) -lt 400 -or ($r.B - $r.T) -lt 300)) {
    [VlaWin]::SetWindowPos($h, [IntPtr]::Zero, [Math]::Max(0, $r.L), [Math]::Max(0, $r.T), 900, 532, 0x0014) | Out-Null   # NOZORDER|NOACTIVATE
    Start-Sleep -Milliseconds 800
  }
}
# `launcher:shown` tells Rec EOS's watcher (game::VanillaLauncherBehind) to stop
# sending the launcher to the back.
function Show-Launcher($h) {
  if (-not $script:launcherShown) { $script:launcherShown = $true; Say 'launcher:shown' }
  if ($h -and $h -ne [IntPtr]::Zero) { [VlaWin]::ShowWindow($h, 9) | Out-Null; [VlaWin]::SetForegroundWindow($h) | Out-Null }
}
function Get-LauncherElements($h) {
  $script:kids = New-Object System.Collections.Generic.List[IntPtr]
  [VlaWin]::EnumChildWindows($h, [VlaWin+EnumProc]{ param($k,$l) $script:kids.Add($k); return $true }, [IntPtr]::Zero) | Out-Null
  foreach ($k in $script:kids) { try { $a = $AE::FromHandle($k).FindAll($D, $T); if ($a.Count -gt 5) { return $a } } catch {} }
  return @()
}
function Clickable-Ancestor($e) {
  $p = $RW.GetParent($e)
  for ($i = 0; $p -and $i -lt 3; $i++) { try { $null = $p.GetCurrentPattern($IP); return $p } catch {}; $p = $RW.GetParent($p) }
  return $null
}
"#;

/// Rec EOS's name for each setting, its key in `settings.json`, and the start
/// of its switch's label in the launcher's Settings.
const SETTINGS: [(&str, &str, &str); 5] = [
    ("discordRpc", "discordRpcEnabled", "Discord Rich Presence"),
    ("appIdFix", "appIdFixEnabled", "Enable AppId Fix"),
    ("darkUi", "darkUiEnabled", "Enable Dark UI"),
    ("newConfigUi", "newConfigUiEnabled", "Enable New Config UI"),
    ("questGraphics", "questGraphicsEnabled", "Quest Graphics"),
];

fn launcher_data_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("VanillaLauncher"))
}

fn settings_path() -> Option<PathBuf> {
    Some(launcher_data_dir()?.join("settings.json"))
}

/// What Settings → Vanilla shows: whether the launcher is installed, each
/// setting's value (missing counts as off), and where the game installs.
fn summary() -> Value {
    let installed = crate::game::find_vanilla_launcher().is_some();
    let parsed: Option<Value> = settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    let mut values = serde_json::Map::new();
    for (name, key, _) in SETTINGS {
        let on = parsed.as_ref().and_then(|v| v.get(key)).and_then(Value::as_bool).unwrap_or(false);
        values.insert(name.to_string(), Value::Bool(on));
    }
    let install_dir = parsed
        .as_ref()
        .and_then(|v| v.get("installDirectory"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| launcher_data_dir().map(|d| d.join("Installs").to_string_lossy().to_string()));
    json!({
        "installed": installed,
        "hasSettings": parsed.is_some(),
        "values": values,
        "installDirectory": install_dir,
        // The game's own folder inside it (what Settings → Game folders shows).
        "gameDirectory": crate::game::vanilla_launcher_game_dir(),
    })
}

#[tauri::command(async)]
pub fn vanilla_launcher_settings() -> Value {
    summary()
}

/// Turn one of Vanilla's launcher settings on or off. Returns the new summary.
#[tauri::command(async)]
pub fn set_vanilla_launcher_setting(name: String, value: bool) -> Result<Value, String> {
    let (_, key, label) = SETTINGS
        .iter()
        .find(|(n, _, _)| *n == name)
        .copied()
        .ok_or_else(|| "Unknown Vanilla setting.".to_string())?;
    if crate::game::find_vanilla_launcher().is_none() {
        return Err("Vanilla's launcher isn't installed.".into());
    }

    match flip_in_open_launcher(label, value)? {
        Flip::Done => {}
        Flip::NotRunning => write_to_file(key, value)?,
    }

    let now = summary();
    if now["values"][name.as_str()].as_bool() != Some(value) {
        return Err("Vanilla's launcher didn't save that change. Try again from its own Settings.".into());
    }
    Ok(now)
}

/// Change one `true`/`false` in the file, leaving everything else as it is.
fn write_to_file(key: &str, value: bool) -> Result<(), String> {
    let path = settings_path().ok_or("Couldn't find Vanilla's launcher settings.")?;
    let text = std::fs::read_to_string(&path)
        .map_err(|_| "Open Vanilla's launcher once first, so it creates its settings.".to_string())?;
    let new_text = match set_bool_in_json_text(&text, key, value) {
        Some(t) => t,
        // Not there yet (or written oddly): add it through a real parse.
        None => {
            let mut v: Value =
                serde_json::from_str(&text).map_err(|_| "Vanilla's launcher settings file is damaged.".to_string())?;
            let obj = v.as_object_mut().ok_or("Vanilla's launcher settings file is damaged.")?;
            obj.insert(key.to_string(), Value::Bool(value));
            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?
        }
    };
    // Written beside it and swapped in, so a crash never leaves half a file.
    let tmp = path.with_extension("json.receos-tmp");
    std::fs::write(&tmp, new_text).map_err(|e| format!("Couldn't save Vanilla's settings: {}", e))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Couldn't save Vanilla's settings: {}", e)
    })
}

/// `"key": true` → `"key": false` (or the other way) in JSON text, touching
/// nothing else. `None` if the key isn't there with a plain boolean value.
fn set_bool_in_json_text(text: &str, key: &str, value: bool) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let after_key = text.find(&needle)? + needle.len();
    let rest = &text[after_key..];
    let colon = rest.find(':')?;
    if !rest[..colon].trim().is_empty() {
        return None;
    }
    let after_colon = &rest[colon + 1..];
    let ws = after_colon.len() - after_colon.trim_start().len();
    let v = &after_colon[ws..];
    let old_len = if v.starts_with("true") {
        4
    } else if v.starts_with("false") {
        5
    } else {
        return None;
    };
    let start = after_key + colon + 1 + ws;
    Some(format!("{}{}{}", &text[..start], value, &text[start + old_len..]))
}

enum Flip {
    Done,
    NotRunning,
}

/// If Vanilla's launcher is open, flip the switch in its Settings (opening
/// that panel and closing it again if it wasn't open).
#[cfg(target_os = "windows")]
fn flip_in_open_launcher(label: &str, value: bool) -> Result<Flip, String> {
    use std::os::windows::process::CommandExt;

    let launcher = crate::game::find_vanilla_launcher().unwrap_or_default();
    let script = format!("{}{}", UIA_PRELUDE, FLIP_PS)
        .replace("__LAUNCHER_PATH__", &launcher.replace('\'', "''"))
        .replace("__LABEL__", &label.replace('\'', "''"))
        .replace("__WANT__", if value { "On" } else { "Off" });
    let output = std::process::Command::new(crate::defender::get_powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output()
        .map_err(|e| format!("Couldn't reach Vanilla's launcher: {}", e))?;
    let out = String::from_utf8_lossy(&output.stdout);
    match out.lines().map(str::trim).rev().find(|l| !l.is_empty()) {
        Some("not_running") => Ok(Flip::NotRunning),
        Some("ok") => Ok(Flip::Done),
        Some("no_settings") => Err("Sign in to Vanilla's launcher first, then try again.".into()),
        _ => Err("Couldn't change that in Vanilla's launcher. Try again from its own Settings.".into()),
    }
}

#[cfg(not(target_os = "windows"))]
fn flip_in_open_launcher(_label: &str, _value: bool) -> Result<Flip, String> {
    Ok(Flip::NotRunning)
}

/// Finds the launcher's window (by its Tauri window class, as the Play
/// automation does), opens its Settings panel if needed, sets the switch whose
/// label starts with `__LABEL__` to `__WANT__`, and puts the panel back.
/// Prints `not_running`, `no_settings` (signed out: no Settings button) or `ok`.
#[cfg(target_os = "windows")]
const FLIP_PS: &str = r#"
$script:top = Find-LauncherWindow
if ($script:top -eq [IntPtr]::Zero) { Write-Output 'not_running'; exit }
Restore-LauncherWindow $script:top
function All { Get-LauncherElements $script:top }
function SettingsButton { All | Where-Object { $_.Current.Name -eq 'Settings' -and $_.Current.ControlType.ProgrammaticName -eq 'ControlType.Button' } | Select-Object -First 1 }
function Find-Toggle { All | Where-Object { "$($_.Current.Name)".StartsWith('__LABEL__') -and $_.Current.ControlType.ProgrammaticName -eq 'ControlType.Button' } | Select-Object -First 1 }

$btn = $null
for ($i = 0; $i -lt 10 -and -not $btn; $i++) { $btn = SettingsButton; if (-not $btn) { Start-Sleep -Milliseconds 500 } }
if (-not $btn) { Write-Output 'no_settings'; exit }
$opened = $false
if ($btn.GetCurrentPattern($TP).Current.ToggleState -ne 'On') { $btn.GetCurrentPattern($TP).Toggle(); $opened = $true; Start-Sleep -Milliseconds 700 }

$result = 'failed'
$sw = Find-Toggle
if ($sw) {
  if ($sw.GetCurrentPattern($TP).Current.ToggleState -ne '__WANT__') { $sw.GetCurrentPattern($TP).Toggle(); Start-Sleep -Milliseconds 700 }
  $sw = Find-Toggle
  if ($sw -and $sw.GetCurrentPattern($TP).Current.ToggleState -eq '__WANT__') { $result = 'ok' }
}
if ($opened) { $b = SettingsButton; if ($b -and $b.GetCurrentPattern($TP).Current.ToggleState -eq 'On') { $b.GetCurrentPattern($TP).Toggle() } }
Write-Output $result
"#;

// ── Vanilla's launcher updates ─────────────────────────────────────────────

/// The installed launcher's version, from its exe's version resource
/// ("1.0.17"), or `None` if it isn't installed or has none.
#[cfg(target_os = "windows")]
fn installed_launcher_version() -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    let exe = crate::game::find_vanilla_launcher()?;
    let wide: Vec<u16> = std::ffi::OsStr::new(&exe).encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is NUL-terminated; `buf` is sized by the size call and
    // outlives the pointer VerQueryValueW returns into it, which is only read
    // within these bounds as a VS_FIXEDFILEINFO.
    unsafe {
        let size = GetFileVersionInfoSizeW(wide.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(wide.as_ptr(), 0, size, buf.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let root: [u16; 2] = [b'\\' as u16, 0];
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len: u32 = 0;
        if VerQueryValueW(buf.as_ptr().cast(), root.as_ptr(), &mut ptr, &mut len) == 0
            || ptr.is_null()
            || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let info = &*(ptr as *const VS_FIXEDFILEINFO);
        let (ms, ls) = (info.dwProductVersionMS, info.dwProductVersionLS);
        Some(format!("{}.{}.{}", ms >> 16, ms & 0xffff, ls >> 16))
    }
}
#[cfg(not(target_os = "windows"))]
fn installed_launcher_version() -> Option<String> {
    None
}

/// The latest launcher version Vanilla publishes (its signed update feed).
async fn latest_launcher_version() -> Result<String, String> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(crate::download::VANILLA_MANIFEST_URL)
        .header("User-Agent", "RecEOS-Launcher")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Vanilla's update server: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Vanilla's update server answered HTTP {}.", resp.status().as_u16()));
    }
    let body = crate::server::read_capped(resp, crate::server::MAX_API_BYTES)
        .await
        .map_err(|e| format!("Couldn't read Vanilla's update feed: {}", e))?;
    let v: Value = serde_json::from_slice(&body).map_err(|_| "Vanilla's update feed was unreadable.".to_string())?;
    v.get("version")
        .and_then(Value::as_str)
        .map(|s| s.trim_start_matches('v').to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Vanilla's update feed has no version.".to_string())
}

/// `{ installed, installedVersion, latestVersion, updateAvailable }`.
#[tauri::command]
pub async fn vanilla_launcher_update_status() -> Value {
    let installed = installed_launcher_version();
    match latest_launcher_version().await {
        Ok(latest) => json!({
            "success": true,
            "installed": crate::game::find_vanilla_launcher().is_some(),
            "installedVersion": installed.clone().unwrap_or_default(),
            "latestVersion": latest,
            "updateAvailable": installed.as_deref().is_some_and(|i| crate::download::version_gt(&latest, i)),
        }),
        Err(e) => json!({
            "success": false,
            "installed": crate::game::find_vanilla_launcher().is_some(),
            "installedVersion": installed.unwrap_or_default(),
            "error": e,
        }),
    }
}

/// Close Vanilla's launcher, and the `--anticheat-guard` processes it leaves
/// behind after games end, so its installer can replace the files. Only
/// called with no game running, when every guard is a leftover.
#[cfg(target_os = "windows")]
pub(crate) fn close_vanilla_launcher() {
    use std::os::windows::process::CommandExt;
    let script = "Get-CimInstance Win32_Process -Filter \"Name='vanillalauncher.exe'\" | \
                  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }";
    let _ = std::process::Command::new(crate::defender::get_powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .output();
}
#[cfg(not(target_os = "windows"))]
pub(crate) fn close_vanilla_launcher() {}

/// Update Vanilla's launcher: close it (never while a game runs), then fetch,
/// verify and open the latest official installer — the same path as the
/// first install (`download::install_vanilla_launcher`).
#[tauri::command]
pub async fn update_vanilla_launcher(app: tauri::AppHandle) -> Result<Value, String> {
    if crate::game::find_vanilla_launcher().is_none() {
        return Err("Vanilla's launcher isn't installed.".into());
    }
    if crate::game::game_running(&app) {
        return Err("Close the game before updating Vanilla's launcher.".into());
    }
    // Checked before closing anything: a game download may be driving the
    // launcher right now (its sign-off), and killing it would strand that.
    if crate::download::client_busy() {
        return Err("Wait for the current download or file check to finish first.".into());
    }
    tokio::task::spawn_blocking(close_vanilla_launcher)
        .await
        .map_err(|e| e.to_string())?;
    crate::download::install_vanilla_launcher(app).await
}

/// Show where Vanilla installs its game.
#[tauri::command(async)]
pub fn open_vanilla_install_folder() -> Result<(), String> {
    let dir = summary()["installDirectory"].as_str().map(PathBuf::from).ok_or("No install folder yet.")?;
    let target = if dir.join("Vanilla").is_dir() { dir.join("Vanilla") } else { dir };
    if !target.is_dir() {
        return Err("Vanilla's install folder doesn't exist yet.".into());
    }
    crate::download::reveal_folder(&target);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::set_bool_in_json_text;

    const FILE: &str = "{\n  \"darkUiEnabled\": false,\n  \"appIdFixEnabled\": true,\n  \"windowWidth\": 900\n}";

    #[test]
    fn flips_only_that_value() {
        let out = set_bool_in_json_text(FILE, "darkUiEnabled", true).unwrap();
        assert_eq!(out, FILE.replace("\"darkUiEnabled\": false", "\"darkUiEnabled\": true"));
        let out = set_bool_in_json_text(FILE, "appIdFixEnabled", false).unwrap();
        assert_eq!(out, FILE.replace("\"appIdFixEnabled\": true", "\"appIdFixEnabled\": false"));
        assert_eq!(set_bool_in_json_text(FILE, "appIdFixEnabled", true).unwrap(), FILE);
    }

    #[test]
    fn leaves_missing_or_non_boolean_keys_to_a_real_parse() {
        assert_eq!(set_bool_in_json_text(FILE, "questGraphicsEnabled", true), None);
        assert_eq!(set_bool_in_json_text(FILE, "windowWidth", true), None);
    }
}
