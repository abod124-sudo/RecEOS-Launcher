//! Harmless DLL used only by the controlled process-loading regression tests.
#[no_mangle]
pub extern "system" fn fixture_marker() -> u32 {
    123
}

#[cfg(slow_init)]
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    module: *mut core::ffi::c_void,
    reason: u32,
    _: *mut core::ffi::c_void,
) -> i32 {
    #[link(name = "kernel32")]
    extern "system" {
        fn Sleep(milliseconds: u32);
        fn GetModuleFileNameW(module: *mut core::ffi::c_void, buffer: *mut u16, size: u32) -> u32;
    }
    if reason == 1 {
        use std::os::windows::ffi::OsStringExt;
        let mut buffer = vec![0u16; 32768];
        let length = GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) as usize;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
        std::fs::write(
            path.parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("loading.started"),
            b"ready",
        )
        .unwrap();
        Sleep(3000);
    }
    1
}
