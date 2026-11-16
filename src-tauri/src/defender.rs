use serde_json::{json, Value};
use std::path::Path;
use tokio::process::Command;

use crate::config::{self, Network};
use crate::download;

/// Resolve the full path to `powershell.exe`.
///
/// Built from `SystemRoot` rather than assuming `C:\Windows`; falls back to the
/// bare name, resolved through `PATH`, only if that file isn't there.
pub(crate) fn get_powershell_path() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let path = format!(
        r"{}\System32\WindowsPowerShell\v1.0\powershell.exe",
        root.trim_end_matches('\\')
    );
    if Path::new(&path).exists() {
        path
    } else {
        "powershell".to_string()
    }
}

fn is_path_safe(path: &str) -> bool {
    // Control characters, and the characters Windows forbids in a path anyway.
    // The path never appears in a command line or a script as text (see
    // `elevated_script`), so nothing here is about quoting.
    !path
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '<' | '>' | '|' | '\r' | '\n'))
}

/// What `ShellExecuteExW` fails with when the UAC prompt is declined.
/// Windows' own `ERROR_CANCELLED`.
#[cfg(windows)]
const ELEVATION_CANCELLED: u32 = 1223;

/// `s` as a PowerShell single-quoted string literal.
///
/// PowerShell ends a single-quoted string at any of five quote characters, not
/// only the ASCII one: ‘ ’ ‚ and ‛ count too. Doubling only `'` left a folder
/// like `O’Brien` able to close the string early — a syntax error at best, and
/// in a script that then runs elevated. Inside a single-quoted string nothing
/// else is special — no `$` expansion, no backtick escapes — so with every
/// quote doubled the folder is plain data, whatever it contains.
fn ps_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// `cmdlet` (`Add-MpPreference` or `Remove-MpPreference`) for every folder in
/// `dirs`, in one call, each folder a single-quoted literal (see [`ps_quote`]).
///
/// The folders are written out in plain text on purpose. They used to be
/// base64 that the script decoded at run time, handed over as
/// `-EncodedCommand` by a hidden PowerShell that started another one: exactly
/// what malware turning Defender off looks like, and Defender flagged it as
/// such when the button was pressed. Readable, the command is also what the
/// UAC prompt's "Show more details" shows the user.
fn exclusion_call(cmdlet: &str, dirs: &[&str]) -> String {
    let paths = dirs.iter().map(|dir| ps_quote(dir)).collect::<Vec<_>>().join(", ");
    format!("{} -ExclusionPath @({})", cmdlet, paths)
}

/// What the elevated PowerShell runs: `cmdlet` for `dirs`, after quietly
/// taking `stale` out of the exclusions.
///
/// `stale` is the folders a client was excluded at before it moved. Their
/// removal is wrapped so it can't fail the call: one Defender no longer lists
/// is no reason to refuse the new exclusion. The call that matters is last,
/// because its success is what the process's exit code reports.
fn exclusion_script(cmdlet: &str, dirs: &[&str], stale: &[&str]) -> String {
    let main = exclusion_call(cmdlet, dirs);
    if stale.is_empty() {
        return main;
    }
    format!(
        "try {{ {} -ErrorAction Stop }} catch {{}}; {}",
        exclusion_call("Remove-MpPreference", stale),
        main
    )
}

/// The command line the elevated PowerShell is started with to run `inner`.
///
/// `$ErrorActionPreference = 'Stop'` makes a cmdlet error end the script, so
/// PowerShell exits non-zero and the launcher doesn't record a failed change
/// as done. Folders can't hold `"` (see [`is_path_safe`]) and the script never
/// ends in a backslash, so the double quotes around it are the only ones the
/// command line has.
#[cfg_attr(not(windows), allow(dead_code))]
fn elevated_params(inner: &str) -> String {
    format!("-NoProfile -NonInteractive -Command \"$ErrorActionPreference = 'Stop'; {}\"", inner)
}

/// Start PowerShell as administrator with `params` — one UAC prompt — wait
/// for it, and answer its exit code, or the Win32 error that kept it from
/// starting ([`ELEVATION_CANCELLED`] when the prompt was declined).
///
/// The launcher asks Windows for the elevated process itself, through
/// `ShellExecuteExW`'s `runas` verb, rather than having a hidden PowerShell
/// call `Start-Process -Verb RunAs` on a second one. The window opens
/// minimised and unfocused: visible in the taskbar for the second it runs,
/// rather than hidden.
#[cfg(windows)]
fn shell_execute_elevated(file: &str, params: &str) -> Result<u32, u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWMINNOACTIVE;

    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (verb, file, params) = (wide("runas"), wide(file), wide(params));

    // SAFETY: a zeroed SHELLEXECUTEINFOW is valid (null pointers, no flags);
    // the strings outlive the call, and the process handle it hands back is
    // waited on and closed here.
    unsafe {
        let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        // NO_UI: a failure comes back as an error for the page to show,
        // not as a Windows error dialog of its own.
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = params.as_ptr();
        info.nShow = SW_SHOWMINNOACTIVE;
        if ShellExecuteExW(&mut info) == 0 {
            return Err(GetLastError());
        }
        if info.hProcess.is_null() {
            return Err(0);
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let got = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        if got == 0 { Err(GetLastError()) } else { Ok(code) }
    }
}

/// Run `inner` as administrator, through one UAC prompt. The error is what to
/// tell the user.
///
/// The wait runs on a blocking thread: it lasts as long as the user takes to
/// answer the UAC prompt, too long to hold a tokio worker that other commands
/// are queued behind.
#[cfg(windows)]
async fn run_elevated(inner: &str) -> Result<(), String> {
    let powershell_path = get_powershell_path();
    let params = elevated_params(inner);
    let result = tokio::task::spawn_blocking(move || shell_execute_elevated(&powershell_path, &params))
        .await
        .map_err(|e| e.to_string())?;
    match result {
        Ok(0) => Ok(()),
        Ok(code) => Err(format!(
            "Windows Defender did not accept the change (exit code {}).",
            code
        )),
        Err(ELEVATION_CANCELLED) => {
            Err("The administrator prompt was declined, so nothing was changed.".to_string())
        }
        Err(error) => Err(format!("Windows couldn't start the administrator prompt (error {}).", error)),
    }
}

#[cfg(not(windows))]
async fn run_elevated(_inner: &str) -> Result<(), String> {
    Err("Windows Defender is only available on Windows.".to_string())
}

/// The network a command is about: the one the page names, as for every other
/// install command, rather than the one last saved to config — the page
/// records the result against the network it is showing.
fn network_of(cfg: &config::Config, network: Option<String>) -> Network {
    network.map_or_else(|| cfg.network(), |n| Network::parse(Some(&n)))
}

/// `dirs` without any folder another network has excluded too: taking it out
/// for this one would take it out for that one as well.
fn not_held_by_others(cfg: &config::Config, network: Network, dirs: &[String]) -> Vec<String> {
    dirs.iter()
        .filter(|dir| {
            !Network::ALL
                .into_iter()
                .filter(|&other| other != network)
                .any(|other| cfg.defender_dirs_for(other).iter().any(|d| config::same_dir(d, dir)))
        })
        .cloned()
        .collect()
}

/// Exclude `network`'s folders (see [`config::exclusion_dirs_for`]) from
/// Windows Defender through one UAC prompt, and record them as excluded.
///
/// Folders on record from before the client moved are taken out in the same
/// prompt, so the exclusion moves with the client rather than staying behind
/// on a folder the launcher no longer uses. Answers `{ success, excluded,
/// movedFrom: [folders taken out] }` or `{ success: false, error }`.
#[tauri::command]
pub async fn add_defender_exclusion(app: tauri::AppHandle, network: Option<String>) -> Value {
    if !cfg!(windows) { return serde_json::json!({"success": false, "error": "Windows Defender is only available on Windows."}); }
    let cfg = config::current(&app);
    let network = network_of(&cfg, network);
    let dirs = config::exclusion_dirs_for(&cfg, network, &config::app_data_dir(&app));
    let stale: Vec<String> = cfg
        .defender_dirs_for(network)
        .iter()
        .filter(|old| !dirs.iter().any(|dir| config::same_dir(dir, old)))
        .cloned()
        .collect();
    let stale = not_held_by_others(&cfg, network, &stale);

    if !dirs.iter().chain(&stale).all(|d| is_path_safe(d)) {
        return json!({ "success": false, "error": "Invalid characters in client path." });
    }

    // Refuse to hand Defender a directory broad enough that excluding it would
    // disable real-time protection for most of the disk. The install folder is
    // user-chosen through a folder picker, so "C:\\" is two clicks away.
    if let Some(broad) = dirs.iter().find(|d| download::is_overly_broad_dir(d)) {
        return json!({
            "success": false,
            "error": format!(
                "Refusing to change antivirus settings for '{}': that folder is too broad. \
                 Point the client install folder at a dedicated directory first.",
                broad
            )
        });
    }

    let dir_refs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let stale_refs: Vec<&str> = stale.iter().map(String::as_str).collect();
    if let Err(error) = run_elevated(&exclusion_script("Add-MpPreference", &dir_refs, &stale_refs)).await {
        return json!({ "success": false, "error": error });
    }

    // Defender has the folders now, whatever happens here. A record that
    // couldn't be saved only means they read as not excluded, and the next
    // launch offers to exclude them again, which is harmless.
    let recorded = config::update(&app, |cfg| cfg.set_defender_dirs(network, dirs.clone()));
    json!({
        "success": true,
        "excluded": recorded.is_ok(),
        "dirs": if recorded.is_ok() { dirs } else { Vec::new() },
        "movedFrom": stale,
    })
}

/// Take the folders on record for `network` back out of Windows Defender's
/// exclusions, through one UAC prompt. Exactly those: after the client has
/// moved, they are not the folders it is in now. Answers `{ success,
/// excluded: false }` or `{ success: false, error }`.
#[tauri::command]
pub async fn remove_defender_exclusion(app: tauri::AppHandle, network: Option<String>) -> Value {
    if !cfg!(windows) { return serde_json::json!({"success": false, "error": "Windows Defender is only available on Windows."}); }
    let cfg = config::current(&app);
    let network = network_of(&cfg, network);
    let targets = not_held_by_others(&cfg, network, cfg.defender_dirs_for(network));

    if !targets.iter().all(|d| is_path_safe(d)) {
        return json!({ "success": false, "error": "Invalid characters in a recorded folder path." });
    }
    if !targets.is_empty() {
        let refs: Vec<&str> = targets.iter().map(String::as_str).collect();
        if let Err(error) = run_elevated(&exclusion_call("Remove-MpPreference", &refs)).await {
            return json!({ "success": false, "error": error });
        }
    }

    match config::update(&app, |cfg| cfg.set_defender_dirs(network, Vec::new())) {
        Ok(()) => json!({ "success": true, "excluded": false }),
        Err(e) => json!({
            "success": false,
            "error": format!("The exclusion was removed, but the launcher couldn't record that. {}", e)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_powershell_quote_character_is_doubled() {
        assert_eq!(ps_quote("C:\\plain"), "'C:\\plain'");
        assert_eq!(ps_quote("it's"), "'it''s'");
        assert_eq!(ps_quote("O\u{2019}Brien"), "'O\u{2019}\u{2019}Brien'");
        assert_eq!(ps_quote("\u{2018}\u{201A}\u{201B}"), "'\u{2018}\u{2018}\u{201A}\u{201A}\u{201B}\u{201B}'");
    }

    /// None of the patterns Defender reads as tampering: no encoded command,
    /// no run-time decoding, no PowerShell starting another one hidden.
    #[test]
    fn the_command_is_plain_readable_powershell() {
        let inner = exclusion_script("Add-MpPreference", &["C:\\new"], &["C:\\old"]);
        let params = elevated_params(&inner);
        for bad in ["EncodedCommand", "FromBase64String", "Start-Process", "RunAs", "Hidden", "Bypass"] {
            assert!(!params.contains(bad), "{bad} in {params}");
        }
        assert!(params.contains("Add-MpPreference -ExclusionPath @('C:\\new')"));
    }

    /// The folders a moved client was excluded at go first, and can't fail
    /// the call: the new exclusion is last, so it is what the exit code says.
    #[test]
    fn a_moved_clients_old_folders_go_quietly_before_the_new_ones() {
        let inner = exclusion_script("Add-MpPreference", &["C:\\new"], &["C:\\old"]);
        assert!(inner.starts_with("try { Remove-MpPreference -ExclusionPath @("));
        assert!(inner.contains("-ErrorAction Stop } catch {}; Add-MpPreference -ExclusionPath @("));
        assert!(inner.ends_with(&exclusion_call("Add-MpPreference", &["C:\\new"])));
        // Nothing to take out: the one call, as before.
        assert_eq!(
            exclusion_script("Add-MpPreference", &["C:\\new"], &[]),
            exclusion_call("Add-MpPreference", &["C:\\new"])
        );
    }

    /// Run the exact command line the elevated PowerShell gets, unelevated,
    /// with `Add-MpPreference` / `Remove-MpPreference` swapped for functions
    /// that print the folders they were handed, one call per line.
    #[cfg(windows)]
    fn run_like_elevated(inner: &str) -> (Option<i32>, String) {
        use std::os::windows::process::CommandExt;
        let fakes = "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
            function Add-MpPreference { param($ExclusionPath) [Console]::Out.Write('add:' + ($ExclusionPath -join '|') + \"`n\") }; \
            function Remove-MpPreference { param($ExclusionPath) [Console]::Out.Write('remove:' + ($ExclusionPath -join '|') + \"`n\") }; ";
        // The fakes go in as their own leading statements; `inner` is untouched.
        let params = elevated_params(&format!("{}{}", fakes.replace('"', "\\\""), inner));
        let out = std::process::Command::new(get_powershell_path())
            .raw_arg(&params)
            .output()
            .expect("powershell runs");
        (out.status.code(), String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// The round trip through the real command line: PowerShell hands the
    /// cmdlet exactly the folders that went in, however hostile their
    /// characters, and none of them runs as code.
    #[cfg(windows)]
    #[test]
    fn hostile_folder_names_reach_the_cmdlet_exactly() {
        let dir = "C:\\Users\\O\u{2019}Brien\\$(calc)'; Remove-Item C:\\nope -Recurse #\\client";
        let other = "C:\\data\\it's $env:TEMP `n (x) & \u{2018}\u{201A}\u{201B} stella\\";
        let (code, out) = run_like_elevated(&exclusion_call("Add-MpPreference", &[dir, other]));
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(out, format!("add:{}|{}\n", dir, other));
    }

    /// Stella's exclusion covers two folders and a moved client takes the old
    /// one out, all in one elevated process — one UAC prompt — old first.
    #[cfg(windows)]
    #[test]
    fn several_folders_and_a_move_go_through_one_process() {
        let inner = exclusion_script("Add-MpPreference", &["C:\\a", "C:\\b"], &["C:\\old"]);
        let (code, out) = run_like_elevated(&inner);
        assert_eq!(code, Some(0));
        assert_eq!(out, "remove:C:\\old\nadd:C:\\a|C:\\b\n");
    }

    /// A cmdlet that fails makes the process fail, so the launcher doesn't
    /// record the exclusion as added.
    #[cfg(windows)]
    #[test]
    fn a_failed_cmdlet_is_not_reported_as_success() {
        use std::os::windows::process::CommandExt;
        let inner = "Write-Error 'Defender said no'";
        let out = std::process::Command::new(get_powershell_path())
            .raw_arg(elevated_params(inner))
            .output()
            .expect("powershell runs");
        assert_ne!(out.status.code(), Some(0));
    }
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct AntivirusProduct {
    pub name: String,
    #[serde(rename = "isDefender")]
    pub is_defender: bool,
}

/// Longest [`detect_antivirus`] waits for Windows to list its antivirus
/// products. A healthy machine answers in well under a second.
#[cfg(target_os = "windows")]
const DETECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Query the system's antivirus products.
/// Classifies products as Microsoft Defender or third-party.
#[tauri::command]
pub async fn detect_antivirus() -> Vec<AntivirusProduct> {
    #[cfg(target_os = "windows")]
    {
        let powershell_path = get_powershell_path();
        // Emit one "displayName|productState" line per registered AV. productState
        // is a hex bitmask whose middle byte encodes real-time-protection status
        // ("00" = off); the Rust side uses it to drop disabled/stale entries.
        // UTF-8 out, so a product name outside ASCII isn't mangled by the
        // console's legacy code page on its way to `from_utf8_lossy` below.
        let ps_command = r#"
            [Console]::OutputEncoding = [Text.Encoding]::UTF8
            $result = @()
            try {
                $avs = Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntiVirusProduct -ErrorAction SilentlyContinue
                if (-not $avs) {
                    $avs = Get-WmiObject -Namespace root/SecurityCenter2 -Class AntiVirusProduct -ErrorAction SilentlyContinue
                }
                foreach ($av in $avs) {
                    $name = "$($av.displayName)".Trim()
                    if ([string]::IsNullOrWhiteSpace($name)) { continue }
                    $state = '{0:x6}' -f [int]$av.productState
                    $result += ($name + '|' + $state)
                }
            } catch {}
            if ($result.Count -eq 0) {
                if (Get-Service -Name WinDefend -ErrorAction SilentlyContinue) {
                    $result += "Windows Defender|001000"
                }
            }
            $result | Write-Output
        "#;

        // Bounded: PLAY waits on this before anything else, and a Security
        // Center query that never answers (a broken WMI repository does that)
        // left the button dead until the launcher was restarted. The process
        // is killed when the wait gives up, and the answer falls back to the
        // same "Windows Defender" as a query that failed outright.
        let run = Command::new(&powershell_path)
            .args(["-NoProfile", "-NonInteractive", "-Command", ps_command])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .kill_on_drop(true)
            .output();
        let output = match tokio::time::timeout(DETECT_TIMEOUT, run).await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::from(std::io::ErrorKind::TimedOut)),
        };
        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let mut products = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for line in stdout.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    // Split "name|productState" from the right, since a display
                    // name could (rarely) contain a '|' itself.
                    let (name, state) = match line.rsplit_once('|') {
                        Some((n, s)) => (n.trim(), s.trim()),
                        None => (line, ""),
                    };
                    if name.is_empty() {
                        continue;
                    }

                    // Skip products whose real-time protection is off ("00" in the
                    // middle byte). This drops a passive Defender when a
                    // third-party AV is active, plus disabled/stale entries.
                    // `get` rather than slicing: a byte range that splits a
                    // character would panic, and panics abort in release.
                    if state.len() == 6 && state.get(2..4) == Some("00") {
                        continue;
                    }

                    let lower_name = name.to_lowercase();

                    // Dedup: SecurityCenter2 can list the same product more than once.
                    if !seen.insert(lower_name.clone()) {
                        continue;
                    }

                    // Match Microsoft Defender precisely. A bare `contains("defender")`
                    // wrongly flags third-party products like *Bitdefender*, hiding
                    // them from the third-party AV warning.
                    let is_defender = lower_name.contains("windows defender")
                        || lower_name.contains("microsoft defender")
                        || lower_name.contains("microsoft security essentials");

                    products.push(AntivirusProduct {
                        name: name.to_string(),
                        is_defender,
                    });
                }
                if products.is_empty() {
                    products.push(AntivirusProduct {
                        name: "Windows Defender".to_string(),
                        is_defender: true,
                    });
                }
                products
            }
            Err(_) => vec![AntivirusProduct {
                name: "Windows Defender".to_string(),
                is_defender: true,
            }],
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        vec![]
    }
}

