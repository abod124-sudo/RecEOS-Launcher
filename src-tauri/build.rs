use std::path::PathBuf;

fn main() {
    // Stella sign-in loads the Steamworks SDK (stella_api.rs), whose
    // steam_api64.dll must sit next to the launcher executable at runtime. The
    // bundler ships it to the install directory via `bundle.resources`; for a
    // dev/`cargo` build, copy it beside the built exe so sign-in works there too
    // (and survives `cargo clean`, unlike a one-off manual copy).
    #[cfg(target_os = "windows")]
    {
        if let Ok(out_dir) = std::env::var("OUT_DIR") {
            // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out; the exe lives
            // at target/<profile>/, three levels up.
            let exe_dir = PathBuf::from(&out_dir)
                .ancestors()
                .nth(3)
                .map(|p| p.to_path_buf());
            let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("steam_api64.dll");
            if let (Some(dir), true) = (exe_dir, src.exists()) {
                let dst = dir.join("steam_api64.dll");
                let _ = std::fs::copy(&src, &dst);
            }
            println!("cargo:rerun-if-changed=steam_api64.dll");
        }
    }

    // Only Stella's sign-in uses the Steam API, so it is delay-loaded: the
    // launcher starts, and Radium and Vanilla work, even with the DLL gone
    // (quarantined, or an exe copied out on its own). Linked normally, a
    // missing DLL stopped the launcher before it could show anything.
    // `stella_api::load_steam_api` loads it ahead of the first Steam call.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "linux" {
        // Deb resources live in /usr/lib/Rec EOS Launcher; AppImage uses the
        // same layout below AppDir/usr. A dev build uses the adjacent copy.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN:$ORIGIN/..:$ORIGIN/../lib/Rec EOS Launcher");
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/libsteam_api.so");
        println!("cargo:rerun-if-changed=resources/libsteam_api.so");
        if source.is_file() {
            if let Ok(out) = std::env::var("OUT_DIR") {
                if let Some(profile) = PathBuf::from(out).ancestors().nth(3) {
                    std::fs::copy(source, profile.join("libsteam_api.so")).expect("copy Linux Steam library");
                    let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/receos-proton-helper.exe");
                    if helper.is_file() {
                        std::fs::copy(helper, profile.join("receos-proton-helper.exe")).expect("copy Proton helper");
                    }
                }
            }
        }
        println!("cargo:rerun-if-changed=resources/receos-proton-helper.exe");
    }
    if target_os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-arg=/DELAYLOAD:steam_api64.dll");
        println!("cargo:rustc-link-arg=delayimp.lib");
    }

    tauri_build::build()
}
