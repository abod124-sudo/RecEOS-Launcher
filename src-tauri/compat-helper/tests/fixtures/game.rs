//! A private test process, not a game. Never contacts Steam or a game service.
#![windows_subsystem = "windows"]
use std::{os::windows::ffi::OsStrExt, path::Path, time::Duration};

fn load(path: &Path) {
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryW(path: *const u16) -> *mut core::ffi::c_void;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    assert!(!unsafe { LoadLibraryW(wide.as_ptr()) }.is_null());
}

fn main() {
    let exe = std::env::current_exe().unwrap();
    let root = exe.parent().unwrap();
    std::fs::write(root.join("fixture.pid"), std::process::id().to_string()).unwrap();
    if std::env::args().any(|arg| arg == "preload-other") {
        load(&root.join("other/patch.dll"));
    }
    load(&root.join("GameAssembly.dll"));
    if std::env::args().any(|arg| arg == "exit-during-load") {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !root.join("loading.started").is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::process::exit(42);
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
