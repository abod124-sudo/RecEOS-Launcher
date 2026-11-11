// Shared by the Windows launcher and the Proton compatibility helper.
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, WaitForSingleObject,
};

/// The module the patch hooks.
const GAME_MODULE: &str = "GameAssembly.dll";

/// How long the game may take to load its code. A first start on a slow
/// disk is the long case.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the game's modules are looked at while waiting. Short, since
/// the game starts running its code straight after the module loads.
const POLL: Duration = Duration::from_millis(5);

/// How long loading the patch may take.
const LOAD_TIMEOUT_MS: u32 = 30_000;

pub fn load_when_ready(child: &mut std::process::Child, patch: &Path) -> Result<(), String> {
    let started = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "The game exited before it finished starting ({}).",
                status
            ));
        }
        if has_module(child.id(), GAME_MODULE) {
            break;
        }
        if started.elapsed() > READY_TIMEOUT {
            return Err("The game took too long to start.".into());
        }
        std::thread::sleep(POLL);
    }
    // The handle from CreateProcess carries full access to the process.
    load_library(child.as_raw_handle() as HANDLE, child.id(), patch)
}

/// Whether process `pid` has a module named `name` loaded.
fn has_module(pid: u32, name: &str) -> bool {
    any_module(pid, |entry| {
        let len = entry
            .szModule
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szModule.len());
        String::from_utf16_lossy(&entry.szModule[..len]).eq_ignore_ascii_case(name)
    })
}

fn any_module(pid: u32, mut matches: impl FnMut(&MODULEENTRY32W) -> bool) -> bool {
    // SAFETY: the snapshot handle is checked before use and closed on
    // every path. `entry` is zeroed with `dwSize` set, as Module32FirstW
    // requires, and both calls get that same struct.
    unsafe {
        // Fails with ERROR_BAD_LENGTH while the loader is mid-change in a
        // starting process; the caller simply asks again.
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: MODULEENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut found = false;
        if Module32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if matches(&entry) {
                    found = true;
                    break;
                }
                if Module32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        found
    }
}

fn file_identity(file: &std::fs::File) -> Option<(u32, u32, u32)> {
    // SAFETY: the file owns a live handle; the output structure is initialized.
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        (GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) != 0).then_some((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
}

fn loaded_patch(process: HANDLE, pid: u32, dll: &Path, thread_exit: u32) -> bool {
    let Some(expected) = std::fs::File::open(dll)
        .ok()
        .and_then(|file| file_identity(&file))
    else {
        return false;
    };
    any_module(pid, |entry| {
        // Thread exit codes can also be process exit/error codes. Require the
        // actual module base, including the rare valid zero low 32 bits.
        if entry.modBaseAddr as usize as u32 != thread_exit {
            return false;
        }
        let mut wide = vec![0u16; 32768];
        // SAFETY: process is the owned child's live handle; hModule comes
        // from its snapshot, and wide has the supplied writable capacity.
        let length = unsafe {
            K32GetModuleFileNameExW(process, entry.hModule, wide.as_mut_ptr(), wide.len() as u32)
        } as usize;
        if length == 0 || length >= wide.len() {
            return false;
        }
        let path = std::ffi::OsString::from_wide(&wide[..length]);
        // Compare file identities so aliases, Unicode and long paths work,
        // while a different DLL with the same basename cannot pass.
        std::fs::File::open(path)
            .ok()
            .and_then(|file| file_identity(&file))
            == Some(expected)
    })
}

/// Have `process` (whose id is `pid`) load the DLL at `dll` with
/// `LoadLibraryW`.
fn load_library(process: HANDLE, pid: u32, dll: &Path) -> Result<(), String> {
    let wide: Vec<u16> = dll
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let size = wide.len() * std::mem::size_of::<u16>();
    let kernel32: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();

    // SAFETY: every handle and allocation is checked before use and
    // released on every path. `LoadLibraryW` is resolved in this process;
    // kernel32 is mapped at the same address in every process of a boot
    // session, so the address is valid in the game too, and its signature
    // (one pointer in, a handle out) matches a thread start routine's.
    // The remote buffer holds the NUL-terminated path the call reads.
    unsafe {
        let load_library = GetProcAddress(
            GetModuleHandleW(kernel32.as_ptr()),
            c"LoadLibraryW".as_ptr().cast(),
        )
        .ok_or("LoadLibraryW was not found.")?;

        let remote = VirtualAllocEx(
            process,
            std::ptr::null(),
            size,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            return Err(format!(
                "Couldn't reserve memory in the game ({}).",
                std::io::Error::last_os_error()
            ));
        }
        let result = (|| {
            let mut written = 0usize;
            if WriteProcessMemory(process, remote, wide.as_ptr().cast(), size, &mut written) == 0
                || written != size
            {
                return Err(format!(
                    "Couldn't pass the patch path to the game ({}).",
                    std::io::Error::last_os_error()
                ));
            }
            let start: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 =
                std::mem::transmute(load_library);
            let thread = CreateRemoteThread(
                process,
                std::ptr::null(),
                0,
                Some(start),
                remote,
                0,
                std::ptr::null_mut(),
            );
            if thread.is_null() {
                return Err(format!(
                    "Couldn't start the patch in the game ({}).",
                    std::io::Error::last_os_error()
                ));
            }
            let waited = WaitForSingleObject(thread, LOAD_TIMEOUT_MS);
            let mut code = 0u32;
            let got_code = GetExitCodeThread(thread, &mut code) != 0;
            CloseHandle(thread);
            if waited != WAIT_OBJECT_0 {
                return Err("The patch took too long to load.".into());
            }
            // A nonzero thread/process exit code alone does not prove that
            // LoadLibraryW loaded the accepted file. Confirm both its file
            // identity and the returned module handle against the child.
            let loaded = got_code
                && (0..20).any(|attempt| {
                    if attempt != 0 {
                        std::thread::sleep(POLL);
                    }
                    loaded_patch(process, pid, dll, code)
                });
            if !loaded {
                return Err(
                    "The verified patch was not loaded, or the game exited during loading. Update the patch and retry.".into(),
                );
            }
            Ok(())
        })();
        VirtualFreeEx(process, remote, 0, MEM_RELEASE);
        result
    }
}
