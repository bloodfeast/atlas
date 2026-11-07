//! Where atlas keeps the user's configuration and data on each OS.
//!
//! | OS | configuration | data (database, memory store, models) |
//! |---|---|---|
//! | Windows | `%APPDATA%\atlas` | `%LOCALAPPDATA%\atlas` |
//! | macOS | `~/Library/Application Support/atlas` | the same folder |
//! | Linux and others | `$XDG_CONFIG_HOME/atlas`, else `~/.config/atlas` | `$XDG_DATA_HOME/atlas`, else `~/.local/share/atlas` |
//!
//! `ATLAS_HOME`, when set, is both folders (tests and portable installs
//! use it). The plugin's own folder is never used for state: Claude Code
//! replaces it on every plugin update, and the plugin data folder it offers
//! is deleted on uninstall, which would take the memory store with it.

use std::path::PathBuf;

/// The environment variable that overrides both folders.
pub const HOME_VAR: &str = "ATLAS_HOME";

/// A folder from the environment. On Windows a value written with `/`
/// (`Z:/home`) is read with `\`, so the paths built on it print with one
/// separator, not `Z:/home\atlas.json`.
fn env_path(name: &str) -> Option<PathBuf> {
    let v = std::env::var_os(name).filter(|v| !v.is_empty())?;
    Some(match v.to_str() {
        Some(s) => native(s, cfg!(windows)),
        None => PathBuf::from(v),
    })
}

/// `s` as a path with the platform's own separator (`\` on Windows).
fn native(s: &str, windows: bool) -> PathBuf {
    PathBuf::from(if windows { s.replace('/', "\\") } else { s.to_string() })
}

/// The user's home folder: `USERPROFILE` on Windows, `HOME` elsewhere.
pub fn home() -> Option<PathBuf> {
    if cfg!(windows) {
        env_path("USERPROFILE")
    } else {
        env_path("HOME")
    }
}

/// The folder holding `atlas.json`, or `None` when no home folder is known.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(h) = env_path(HOME_VAR) {
        return Some(h);
    }
    if cfg!(windows) {
        env_path("APPDATA").or_else(|| home().map(|h| h.join("AppData").join("Roaming"))).map(|d| d.join("atlas"))
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library").join("Application Support").join("atlas"))
    } else {
        env_path("XDG_CONFIG_HOME").or_else(|| home().map(|h| h.join(".config"))).map(|d| d.join("atlas"))
    }
}

/// The folder holding the database, the memory store and the models, or
/// `None` when no home folder is known.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(h) = env_path(HOME_VAR) {
        return Some(h);
    }
    if cfg!(windows) {
        env_path("LOCALAPPDATA").or_else(|| home().map(|h| h.join("AppData").join("Local"))).map(|d| d.join("atlas"))
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library").join("Application Support").join("atlas"))
    } else {
        env_path("XDG_DATA_HOME").or_else(|| home().map(|h| h.join(".local").join("share"))).map(|d| d.join("atlas"))
    }
}

/// The default database: `state/atlas.sqlite` in [`data_dir`], else in the
/// current folder.
pub fn default_db() -> PathBuf {
    data_dir().unwrap_or_default().join("state").join("atlas.sqlite")
}

#[cfg(test)]
mod native_tests {
    #[test]
    fn a_forward_slash_folder_reads_with_the_native_separator() {
        assert_eq!(super::native("Z:/home/atlas", true).to_string_lossy(), "Z:\\home\\atlas");
        #[cfg(windows)]
        assert_eq!(super::native("Z:/home/atlas", true).join("atlas.json").to_string_lossy(), "Z:\\home\\atlas\\atlas.json");
        assert_eq!(super::native("/home/u", false).to_string_lossy(), "/home/u");
    }
}
