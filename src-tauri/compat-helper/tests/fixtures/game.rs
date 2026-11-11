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
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
