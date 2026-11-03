//! Moves the launcher's data from the folders of its old app identifier
//! (`com.radium.launcher`) to the ones of the new (`com.receos.launcher`).
//!
//! Tauri names the settings folder (`%APPDATA%`) and the folder for logs, the
//! web view's storage and caches (`%LOCALAPPDATA%`) after the identifier, so
//! changing it would otherwise start the launcher from nothing: no settings,
//! no sign-ins, and the game downloaded again.
//!
//! Runs first thing in [`crate::run`], before anything opens those folders.
//! Nothing here is fatal: whatever can't be moved stays where it is, and the
//! next start picks it up.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub const OLD_IDENTIFIER: &str = "com.radium.launcher";
pub const NEW_IDENTIFIER: &str = "com.receos.launcher";

/// Settings keys whose paths are a record of something outside the launcher
/// (a Windows Defender exclusion made for the old folder), which a move does
/// not carry along. Left as they are, the launcher sees the new folder as not
/// excluded and offers to exclude it.
fn is_record_of_the_past(key: &str) -> bool {
    key.to_ascii_lowercase().contains("defender")
}

pub fn migrate_app_identifier() {
    // Only Windows sets these; elsewhere there is nothing to do.
    for var in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            move_identifier_dir(Path::new(&base));
        }
    }
    if let Some(base) = std::env::var_os("APPDATA") {
        rewrite_config_file(&Path::new(&base).join(NEW_IDENTIFIER).join("config.json"));
    }
}

/// `<base>\com.radium.launcher` -> `<base>\com.receos.launcher`.
fn move_identifier_dir(base: &Path) {
    let (old, new) = (base.join(OLD_IDENTIFIER), base.join(NEW_IDENTIFIER));
    if !old.is_dir() {
        return;
    }
    // The common case: a rename, instant whatever the size of the game in it.
    if !new.exists() && fs::rename(&old, &new).is_ok() {
        return;
    }
    // Something in it is open (the old launcher's web view takes a moment to
    // let go), or the new folder already exists: move what can be moved,
    // never replacing a file the new launcher has written.
    let _ = merge_move(&old, &new);
    // Only succeeds if that emptied it.
    let _ = fs::remove_dir(&old);
}

/// Moves `src` to `dst`, merging into a `dst` folder that already exists. A
/// file that is already in `dst` is left alone, and so is its original.
fn merge_move(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            // One file that won't move must not strand the rest.
            let _ = merge_move(&entry.path(), &dst.join(entry.file_name()));
        }
        let _ = fs::remove_dir(src);
        Ok(())
    } else if dst.exists() {
        Ok(())
    } else if fs::rename(src, dst).is_ok() {
        Ok(())
    } else if meta.file_type().is_symlink() {
        // A link is moved as the link; copying would follow it.
        Ok(())
    } else {
        fs::copy(src, dst)?;
        let _ = fs::remove_file(src);
        Ok(())
    }
}

/// Settings that name a folder inside the old identifier's folder (the game
/// installed in the default place is recorded that way) point at the new one.
fn rewrite_config_file(path: &PathBuf) {
    let Ok(text) = fs::read_to_string(path) else { return };
    if !text.to_ascii_lowercase().contains(OLD_IDENTIFIER) {
        return;
    }
    let Ok(mut value) = serde_json::from_str::<Value>(&text) else { return };
    if !rewrite_paths(&mut value) {
        return;
    }
    let Ok(out) = serde_json::to_string_pretty(&value) else { return };
    let tmp = path.with_extension("json.migrating");
    if fs::write(&tmp, out).is_ok() && fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

/// Returns true if anything changed.
fn rewrite_paths(value: &mut Value) -> bool {
    match value {
        Value::String(s) => replace_identifier(s),
        Value::Array(items) => items.iter_mut().fold(false, |any, v| rewrite_paths(v) | any),
        Value::Object(map) => map
            .iter_mut()
            .filter(|(k, _)| !is_record_of_the_past(k))
            .fold(false, |any, (_, v)| rewrite_paths(v) | any),
        _ => false,
    }
}

fn replace_identifier(s: &mut String) -> bool {
    let lower = s.to_ascii_lowercase();
    let Some(pos) = lower.find(OLD_IDENTIFIER) else { return false };
    s.replace_range(pos..pos + OLD_IDENTIFIER.len(), NEW_IDENTIFIER);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("radium-migrate-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn paths_follow_the_move_but_defender_records_do_not() {
        let mut v = json!({
            "installDir": r"C:\Users\me\AppData\Roaming\com.radium.launcher\client",
            "vanilla": { "installDir": r"D:\Games\Rec Room" },
            "defenderDirs": [r"C:\Users\me\AppData\Roaming\com.radium.launcher\client"],
            "receos": { "defender_dirs": [r"C:\Users\me\AppData\Roaming\com.radium.launcher\receos"] },
            "theme": "steam-green"
        });
        assert!(rewrite_paths(&mut v));
        assert_eq!(v["installDir"], r"C:\Users\me\AppData\Roaming\com.receos.launcher\client");
        assert_eq!(v["vanilla"]["installDir"], r"D:\Games\Rec Room");
        assert_eq!(v["defenderDirs"][0], r"C:\Users\me\AppData\Roaming\com.radium.launcher\client");
        assert_eq!(v["receos"]["defender_dirs"][0], r"C:\Users\me\AppData\Roaming\com.radium.launcher\receos");
        // Nothing left to do the second time.
        assert!(!rewrite_paths(&mut v));
    }

    #[test]
    fn a_whole_folder_is_renamed() {
        let base = temp("rename");
        let old = base.join(OLD_IDENTIFIER);
        fs::create_dir_all(old.join("client")).unwrap();
        fs::write(old.join("config.json"), "{}").unwrap();
        fs::write(old.join("client").join("RecRoom.exe"), "x").unwrap();

        move_identifier_dir(&base);

        let new = base.join(NEW_IDENTIFIER);
        assert!(new.join("config.json").is_file());
        assert!(new.join("client").join("RecRoom.exe").is_file());
        assert!(!old.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn merging_never_replaces_what_the_new_launcher_wrote() {
        let base = temp("merge");
        let (old, new) = (base.join(OLD_IDENTIFIER), base.join(NEW_IDENTIFIER));
        fs::create_dir_all(old.join("client")).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("config.json"), "old").unwrap();
        fs::write(old.join("client").join("a.dat"), "a").unwrap();
        fs::write(new.join("config.json"), "new").unwrap();

        move_identifier_dir(&base);

        assert_eq!(fs::read_to_string(new.join("config.json")).unwrap(), "new");
        assert_eq!(fs::read_to_string(new.join("client").join("a.dat")).unwrap(), "a");
        // The old config stays, so the old folder does too.
        assert!(old.join("config.json").is_file());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn nothing_to_move_is_fine() {
        let base = temp("none");
        move_identifier_dir(&base);
        assert!(!base.join(NEW_IDENTIFIER).exists());
        let _ = fs::remove_dir_all(&base);
    }
}
