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

/// Exit code the non-elevated wrapper reports when the elevated process never
/// started: the UAC prompt was declined, or Windows refused to launch it.
/// Windows' own `ERROR_CANCELLED`.
const ELEVATION_CANCELLED: i32 = 1223;

/// UTF-16LE base64, the encoding PowerShell's `-EncodedCommand` takes.
fn utf16_base64(s: &str) -> String {
    let bytes: Vec<u8> = s.encode_utf16().flat_map(u16::to_le_bytes).collect();
    crate::frost::base64(&bytes)
}

/// `s` as a PowerShell single-quoted string literal.
///
/// PowerShell ends a single-quoted string at any of five quote characters, not
/// only the ASCII one: ‘ ’ ‚ and ‛ count too. Doubling only `'` left a folder
/// like `O’Brien` able to close the string early — a syntax error at best, and
/// in a script that then runs elevated.
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
/// `dirs`, in one call. Each path is decoded from base64 at run time, so no
/// character a folder name can hold — quotes of any kind, `$`, backticks — is
/// ever parsed as PowerShell.
fn exclusion_call(cmdlet: &str, dirs: &[&str]) -> String {
    let paths = dirs
        .iter()
        .map(|dir| {
            format!(
                "[Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{}'))",
                utf16_base64(dir)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
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

/// The script the non-elevated PowerShell runs to run `inner` in an elevated
/// one, so there is one UAC prompt however much `inner` does.
///
/// No folder ever appears in either script as text. The elevated script is
/// handed over as `-EncodedCommand`, and inside it each path is decoded from
/// base64 at run time (see [`exclusion_call`]).
///
/// `$ErrorActionPreference = 'Stop'` plus the `catch` is what makes a declined
/// UAC prompt a failure. Without them Start-Process's error ended only that
/// statement, `$p` stayed null, and `exit $null` exited 0 — so declining the
/// prompt reported the exclusion as added and the launcher saved it as done.
fn elevated_script(powershell_path: &str, inner: &str) -> String {
    format!(
        "$ErrorActionPreference = 'Stop'; \
         try {{ \
           $p = Start-Process -FilePath {} -ArgumentList '-NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru; \
           exit $p.ExitCode \
         }} catch {{ exit {} }}",
        ps_quote(powershell_path),
        utf16_base64(inner),
        ELEVATION_CANCELLED
    )
}

/// Run `inner` as administrator, through one UAC prompt. The error is what to
/// tell the user.
///
/// Awaited, not blocked on: this waits for an elevated PowerShell process and,
/// with it, for the user to answer a UAC prompt — which could be a long time
/// to hold a tokio worker that other commands are queued behind.
async fn run_elevated(inner: &str) -> Result<(), String> {
    let powershell_path = get_powershell_path();
    let script = elevated_script(&powershell_path, inner);

    let mut command = Command::new(&powershell_path);
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    #[cfg(target_os = "windows")]
    {
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    match command.output().await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(match output.status.code() {
            Some(ELEVATION_CANCELLED) => {
                "The administrator prompt was declined, so nothing was changed.".to_string()
            }
            code => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let detail = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
                format!(
                    "Windows Defender did not accept the change (exit code {}). {}",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()),
                    detail
                )
                .trim()
                .to_string()
            }
        }),
        Err(e) => Err(e.to_string()),
    }
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

    #[test]
    fn the_folder_never_appears_in_the_script_as_text() {
        let dir = "C:\\Users\\O\u{2019}Brien\\$(calc)'; Remove-Item C:\\ -Recurse #\\client";
        let old = "C:\\old\\it's $(notepad)";
        let inner = exclusion_script("Add-MpPreference", &[dir, "C:\\data\\stella"], &[old]);
        for script in [inner.clone(), elevated_script("C:\\ps.exe", &inner)] {
            assert!(!script.contains("Brien"));
            assert!(!script.contains("calc"));
            assert!(!script.contains("Remove-Item"));
            assert!(!script.contains("stella"));
            assert!(!script.contains("notepad"));
        }
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

    /// Stella's exclusion covers two folders in one elevated call: both are
    /// handed to one `-ExclusionPath` array, so there is still one UAC prompt.
    #[cfg(windows)]
    #[test]
    fn several_folders_go_to_one_exclusion_call() {
        let dirs = ["C:\\data\\client-stella", "C:\\data\\it's stella"];
        let script = elevated_script("C:\\ps.exe", &exclusion_call("Add-MpPreference", &dirs));
        // Pull the elevated command back out and decode it, as PowerShell would.
        let encoded = script.split("-EncodedCommand ").nth(1).unwrap().split('\'').next().unwrap();
        let script = format!(
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
             [Console]::Out.Write([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{}')))",
            encoded
        );
        let out = std::process::Command::new(get_powershell_path())
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &utf16_base64(&script)])
            .output()
            .expect("powershell runs");
        let inner = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(inner.starts_with("Add-MpPreference -ExclusionPath @("));
        assert_eq!(inner.matches("FromBase64String").count(), 2);

        // And each path decodes to exactly what went in.
        let decode_all = inner.replace("Add-MpPreference -ExclusionPath ", "[Console]::OutputEncoding = [Text.Encoding]::UTF8; [Console]::Out.Write((");
        let decode_all = format!("{}) -join '|')", decode_all);
        let out = std::process::Command::new(get_powershell_path())
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &utf16_base64(&decode_all)])
            .output()
            .expect("powershell runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), dirs.join("|"));
    }

    /// The real round trip: PowerShell decodes the path to exactly the string
    /// that went in, however hostile its characters. Not elevated — the same
    /// decoding, printed instead of handed to Defender.
    #[cfg(windows)]
    #[test]
    fn powershell_decodes_the_path_exactly() {
        let dir = "C:\\Users\\O\u{2019}Brien\\it's $env:TEMP `n (x) & client";
        let script = format!(
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
             [Console]::Out.Write([Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('{}')))",
            utf16_base64(dir)
        );
        let out = std::process::Command::new(get_powershell_path())
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &utf16_base64(&script)])
            .output()
            .expect("powershell runs");
        assert_eq!(String::from_utf8_lossy(&out.stdout), dir);
    }

    /// A launch that fails before anything elevated runs — which is what a
    /// declined UAC prompt is — must come back as a failure, not exit 0.
    #[cfg(windows)]
    #[test]
    fn a_failed_elevation_is_not_reported_as_success() {
        let script = elevated_script("C:\\radium-no-such-dir\\missing.exe", &exclusion_call("Add-MpPreference", &["C:\\x"]));
        let status = std::process::Command::new(get_powershell_path())
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .expect("powershell runs");
        assert_eq!(status.code(), Some(ELEVATION_CANCELLED));
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

