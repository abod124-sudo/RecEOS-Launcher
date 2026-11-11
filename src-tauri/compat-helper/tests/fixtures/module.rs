//! Harmless DLL used only by the controlled process-loading regression tests.
#[no_mangle]
pub extern "system" fn fixture_marker() -> u32 {
    123
}

#[cfg(exit_during_load)]
#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _: *mut core::ffi::c_void,
    reason: u32,
    _: *mut core::ffi::c_void,
) -> i32 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn TerminateProcess(process: *mut core::ffi::c_void, code: u32) -> i32;
    }
    if reason == 1 {
        // Exit before LoadLibraryW can return, independent of the scheduler.
        // The former sleep/other-thread handshake could let initialization
        // finish first on a busy runner, making success a valid result.
        std::fs::write("exit-during-load.started", b"terminating in DllMain").unwrap();
        TerminateProcess(GetCurrentProcess(), 42);
        // Successful self-termination does not return. Still exit from this
        // initialization call if an emulator unexpectedly returns an error.
        std::process::abort();
    }
    1
}
