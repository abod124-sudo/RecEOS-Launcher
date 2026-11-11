//! Windows-only, non-GUI helper. This process runs inside the selected
//! Proton prefix, so the existing Windows patch loader can target its child.
#[cfg(windows)]
#[path = "../../src/stella_inject.rs"]
mod inject;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Request {
    version: u32,
    exe: std::path::PathBuf,
    patch: std::path::PathBuf,
    sha256: String,
    args: Vec<String>,
}

#[cfg(windows)]
fn launch(request: Request) -> Result<std::process::Child, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    if request.version != 1
        || !request.exe.is_absolute()
        || !request.patch.is_absolute()
        || request
            .exe
            .file_name()
            .is_none_or(|s| !s.eq_ignore_ascii_case("RecRoom.exe"))
    {
        return Err("Invalid Stella helper launch request.".into());
    }
    if request.args.len() > 3
        || request
            .args
            .iter()
            .any(|s| s.len() > 128 || s.contains(['\0', '\n', '\r']))
    {
        return Err("Invalid game arguments.".into());
    }
    // Deny writes/deletion for the entire verification and injection window.
    let mut held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&request.patch)
        .map_err(|e| format!("Cannot open verified patch: {e}"))?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let len = held.read(&mut buf).map_err(|e| e.to_string())?;
        if len == 0 {
            break;
        }
        hash.update(&buf[..len]);
    }
    if format!("{:x}", hash.finalize()) != request.sha256 {
        return Err("Stella patch changed after verification. Update the patch and retry.".into());
    }
    let mut child = std::process::Command::new(&request.exe)
        .args(&request.args)
        .current_dir(request.exe.parent().ok_or("Game directory missing")?)
        .spawn()
        .map_err(|e| format!("Cannot start Stella: {e}"))?;
    if let Err(error) = inject::load_when_ready(&mut child, &request.patch) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(child)
}

fn request_bytes(path: Option<&std::path::Path>) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut input: Box<dyn Read> = match path {
        Some(path) => Box::new(std::fs::File::open(path).map_err(|e| e.to_string())?),
        None => Box::new(std::io::stdin()),
    };
    let mut bytes = Vec::new();
    input.by_ref().take(65537).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() > 65536 { return Err("Helper request too large.".into()); }
    Ok(bytes)
}

fn write_reply(path: &std::path::Path, reply: &serde_json::Value) -> Result<(), String> {
    use std::io::Write;
    // A private launch directory and exclusive creation prevent stale replies
    // or a symlink destination from being accepted. Publish only a full JSON.
    let pending = path.with_extension("partial");
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&pending).map_err(|e| e.to_string())?;
    file.write_all(reply.to_string().as_bytes()).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    if path.exists() { return Err("Helper response already exists.".into()); }
    std::fs::rename(pending, path).map_err(|e| e.to_string())
}

fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let files = if arguments.len() == 4 && arguments[0] == "--request-file" && arguments[2] == "--response-file" {
        Some((std::path::PathBuf::from(&arguments[1]), std::path::PathBuf::from(&arguments[3])))
    } else { None };
    let result = (|| -> Result<std::process::Child, String> {
        if !arguments.is_empty() && files.is_none() { return Err("Invalid helper arguments.".into()); }
        if files.as_ref().is_some_and(|(input, output)| !input.is_absolute() || !output.is_absolute() || input == output) {
            return Err("Helper protocol requires separate absolute file paths.".into());
        }
        let bytes = request_bytes(files.as_ref().map(|(input, _)| input.as_path()))?;
        let request: Request = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        #[cfg(windows)]
        {
            launch(request)
        }
        #[cfg(not(windows))]
        {
            let _ = request;
            Err("This helper must be built for Windows x64 and run inside Proton.".into())
        }
    })();
    let reply = match &result {
        Ok(child) => serde_json::json!({"version": 1, "success": true, "windowsPid": child.id()}),
        Err(error) => serde_json::json!({"version": 1, "success": false, "error": error}),
    };
    if let Some((_, output)) = &files {
        if let Err(error) = write_reply(output, &reply) {
            if let Ok(mut child) = result { let _ = child.kill(); let _ = child.wait(); }
            eprintln!("Cannot publish verified helper result: {error}");
            std::process::exit(1);
        }
    }
    println!("RECEOS-PROTON-HELPER {reply}");
    if reply["success"] != true {
        std::process::exit(1);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn unknown_fields_are_not_accepted() {
        assert!(serde_json::from_str::<Request>(r#"{"version":1,"exe":"C:\\RecRoom.exe","patch":"C:\\patch.dll","sha256":"x","args":[],"command":"other"}"#).is_err());
    }
    #[test]
    fn an_unverified_patch_never_starts_the_game() {
        let root = std::env::temp_dir().join(format!("receos-helper-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let patch = root.join("patch.dll");
        std::fs::write(&patch, b"modified patch").unwrap();
        let request = Request {
            version: 1,
            exe: root.join("RecRoom.exe"),
            patch,
            sha256: "0".repeat(64),
            args: vec!["+mode:screen".into()],
        };
        assert!(launch(request)
            .unwrap_err()
            .contains("changed after verification"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn invalid_launch_targets_are_rejected() {
        let request = Request {
            version: 1,
            exe: "C:\\Windows\\System32\\cmd.exe".into(),
            patch: "C:\\patch.dll".into(),
            sha256: "0".repeat(64),
            args: vec![],
        };
        assert!(launch(request)
            .unwrap_err()
            .contains("Invalid Stella helper"));
    }
}
