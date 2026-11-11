//! Real Windows helper/loader integration against private, harmless fixtures.
//! These checks do not establish Wine/Proton or real game compatibility.
#![cfg(windows)]
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        OnceLock,
    },
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    },
};

static FIXTURES: OnceLock<PathBuf> = OnceLock::new();
static NEXT_CASE: AtomicUsize = AtomicUsize::new(0);

fn fixtures() -> &'static Path {
    FIXTURES
        .get_or_init(|| {
            let root =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/patch-loading-fixtures");
            fs::create_dir_all(&root).unwrap();
            for (source, output, extra) in [
                ("module.rs", "module.dll", vec!["--crate-type", "cdylib"]),
                (
                    "module.rs",
                    "slow.dll",
                    vec!["--crate-type", "cdylib", "--cfg", "slow_init"],
                ),
                ("game.rs", "game.exe", vec![]),
            ] {
                let status = Command::new("rustc")
                    .arg("--edition=2021")
                    .arg(
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("tests/fixtures")
                            .join(source),
                    )
                    .args(extra)
                    .args(["-C", "target-feature=+crt-static"])
                    .arg("-o")
                    .arg(root.join(output))
                    .status()
                    .unwrap();
                assert!(status.success(), "Fixture compiler failed");
            }
            root
        })
        .as_path()
}

struct Case {
    root: PathBuf,
}
impl Case {
    fn new() -> Self {
        let root = fixtures().join(format!(
            "case-{}-{}",
            std::process::id(),
            NEXT_CASE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("verified")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::copy(fixtures().join("game.exe"), root.join("RecRoom.exe")).unwrap();
        fs::copy(fixtures().join("module.dll"), root.join("GameAssembly.dll")).unwrap();
        Self { root }
    }

    fn process(&self) -> HANDLE {
        let pid: u32 = fs::read_to_string(self.root.join("fixture.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return handle;
        }
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        let found =
            unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) } != 0;
        let expected = fs::canonicalize(self.root.join("RecRoom.exe")).ok();
        let image =
            found.then(|| PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize])));
        if expected.is_none() || image.and_then(|path| fs::canonicalize(path).ok()) != expected {
            unsafe {
                CloseHandle(handle);
            }
            return std::ptr::null_mut();
        }
        handle
    }

    fn running(&self) -> bool {
        let handle = self.process();
        if handle.is_null() {
            return false;
        }
        let alive = unsafe { WaitForSingleObject(handle, 0) != WAIT_OBJECT_0 };
        unsafe {
            CloseHandle(handle);
        }
        alive
    }

    fn launch(&self, args: &[&str]) -> (bool, serde_json::Value) {
        let patch = self.root.join("verified/patch.dll");
        let request = serde_json::json!({"version": 1, "exe": self.root.join("RecRoom.exe"),
            "patch": patch, "sha256": format!("{:x}", Sha256::digest(fs::read(&patch).unwrap())), "args": args});
        let mut helper = Command::new(env!("CARGO_BIN_EXE_receos-proton-helper"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        helper
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        // The fixture inherits the helper's stdout pipe. Read the protocol
        // line rather than waiting for EOF while a successful child lives.
        let mut stdout = BufReader::new(helper.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let reply = line
            .trim()
            .strip_prefix("RECEOS-PROTON-HELPER ")
            .expect("Helper reply");
        (
            helper.wait().unwrap().success(),
            serde_json::from_str(reply).unwrap(),
        )
    }

    fn launch_files(&self, args: &[&str]) -> (bool, serde_json::Value) {
        let patch = self.root.join("verified/patch.dll");
        let input = self.root.join("request.json");
        let output = self.root.join("response.json");
        let request = serde_json::json!({"version": 1, "exe": self.root.join("RecRoom.exe"),
            "patch": patch, "sha256": format!("{:x}", Sha256::digest(fs::read(&patch).unwrap())), "args": args});
        fs::write(&input, request.to_string()).unwrap();
        let success = Command::new(env!("CARGO_BIN_EXE_receos-proton-helper"))
            .arg("--request-file").arg(input).arg("--response-file").arg(&output)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .status().unwrap().success();
        (success, serde_json::from_slice(&fs::read(output).unwrap()).unwrap())
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        if self.root.join("fixture.pid").is_file() {
            let handle = self.process();
            if !handle.is_null() {
                unsafe {
                    TerminateProcess(handle, 0);
                    WaitForSingleObject(handle, 5000);
                    CloseHandle(handle);
                }
            }
        }
        if let (Ok(root), Ok(base)) = (fs::canonicalize(&self.root), fs::canonicalize(fixtures())) {
            if root != base && root.starts_with(base) {
                let _ = fs::remove_dir_all(root);
            }
        }
    }
}

#[test]
fn a_valid_patch_loads_before_the_helper_reports_success() {
    let case = Case::new();
    fs::copy(
        fixtures().join("module.dll"),
        case.root.join("verified/patch.dll"),
    )
    .unwrap();
    let (success, reply) = case.launch(&[]);
    assert!(success && reply["success"] == true, "{reply}");
    assert!(
        case.running(),
        "The fixture should remain running after successful loading"
    );
}

#[test]
fn a_failed_load_closes_the_owned_process() {
    let case = Case::new();
    fs::write(
        case.root.join("verified/patch.dll"),
        b"This is not a PE library",
    )
    .unwrap();
    let (success, reply) = case.launch(&[]);
    assert!(!success && reply["success"] == false, "{reply}");
    assert!(
        !case.running(),
        "Failed patch loading left the owned process running"
    );
}

#[test]
fn a_different_loaded_dll_with_the_same_name_does_not_count_as_the_patch() {
    let case = Case::new();
    fs::copy(
        fixtures().join("module.dll"),
        case.root.join("other/patch.dll"),
    )
    .unwrap();
    fs::write(
        case.root.join("verified/patch.dll"),
        b"This is not a PE library",
    )
    .unwrap();
    let (success, reply) = case.launch(&["preload-other"]);
    assert!(
        !success && reply["success"] == false,
        "An unrelated DLL was accepted: {reply}"
    );
    assert!(
        !case.running(),
        "Failed verification left the owned process running"
    );
}

#[test]
fn process_exit_during_loading_cannot_be_reported_as_a_loaded_patch() {
    let case = Case::new();
    fs::copy(
        fixtures().join("slow.dll"),
        case.root.join("verified/patch.dll"),
    )
    .unwrap();
    let (success, reply) = case.launch(&["exit-during-load"]);
    assert!(
        !success && reply["success"] == false,
        "Process exit was accepted as a loaded patch: {reply}"
    );
    assert!(!case.running());
}

#[test]
fn file_protocol_works_without_standard_streams_and_preserves_all_load_checks() {
    for (patch, args, expected) in [
        (Some("module.dll"), vec![], true),
        (None, vec![], false),
        (None, vec!["preload-other"], false),
        (Some("slow.dll"), vec!["exit-during-load"], false),
    ] {
        let case = Case::new();
        fs::copy(fixtures().join("module.dll"), case.root.join("other/patch.dll")).unwrap();
        let target = case.root.join("verified/patch.dll");
        if let Some(patch) = patch { fs::copy(fixtures().join(patch), target).unwrap(); }
        else { fs::write(target, b"This is not a PE library").unwrap(); }
        let (success, reply) = case.launch_files(&args);
        assert_eq!(success, expected, "{reply}");
        assert_eq!(reply["success"], expected, "{reply}");
        assert_eq!(case.running(), expected, "{reply}");
    }
}

#[test]
fn an_unpublishable_result_closes_the_game_and_does_not_overwrite_existing_files() {
    let case = Case::new();
    fs::copy(fixtures().join("module.dll"), case.root.join("verified/patch.dll")).unwrap();
    fs::write(case.root.join("response.json"), b"{\"existing\":true}").unwrap();
    let (success, reply) = case.launch_files(&[]);
    assert!(!success);
    assert_eq!(reply, serde_json::json!({"existing": true}));
    assert!(!case.running());
}
