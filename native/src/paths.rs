//! Portable path resolution — the DLL no longer hardcodes the developer's
//! machine paths. Two kinds of paths:
//!
//!  - **MOD-local files** (settings, logs): live next to `trackside.dll`
//!    (i.e. inside the game folder), found via `GetModuleFileNameW`. Always
//!    correct on any machine, no config.
//!
//!  - **Team Trials captures**: go into the **Heaven dashboard's own data
//!    folder**. The dashboard publishes its `data/` path on startup to
//!    `%LOCALAPPDATA%\Heaven\datadir.txt`; we read that so captures land right
//!    next to the user's existing history. If the dashboard has never run, we
//!    fall back to `%LOCALAPPDATA%\Heaven\data` (the dashboard's importer also
//!    checks that location). The dashboard is an external app with its own
//!    branding, so this path intentionally keeps the Heaven name.
//!
//! FORK MIGRATION: installs upgrading from Heaven have their state under the
//! old `heaven-*` names. `local_file_migrated` renames the old file to the new
//! name once (first run), so settings/state survive the rebrand.

#![allow(dead_code)]

use std::path::PathBuf;

use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Directory that contains our overlay DLL (the game folder). Falls back to the
/// current dir if the module can't be located. We look up BOTH `trackside.dll`
/// (normal) and `heaven_overlay.dll` (when loaded by a Heaven-style proxy that
/// loads the overlay under the old name) so paths resolve either way.
pub fn dll_dir() -> PathBuf {
    unsafe {
        let mut h = GetModuleHandleW(wide("trackside.dll").as_ptr());
        if h.is_null() {
            h = GetModuleHandleW(wide("heaven_overlay.dll").as_ptr());
        }
        if h.is_null() {
            return PathBuf::from(".");
        }
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(h as _, buf.as_mut_ptr(), buf.len() as u32);
        if n == 0 {
            return PathBuf::from(".");
        }
        let full = PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
        full.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
    }
}

/// A MOD-local file next to the DLL (e.g. the settings JSON).
pub fn local_file(name: &str) -> PathBuf {
    dll_dir().join(name)
}

/// A MOD-local file that MIGRATES from a pre-fork (Heaven) name: if the new
/// file doesn't exist yet but the old one does, the old file is renamed to the
/// new name (one-time, first run after the upgrade). Always returns the NEW path.
pub fn local_file_migrated(name: &str, old_name: &str) -> PathBuf {
    let new = dll_dir().join(name);
    if !new.exists() {
        let old = dll_dir().join(old_name);
        if old.exists() {
            let _ = std::fs::rename(&old, &new);
        }
    }
    new
}

/// A MOD-local DIRECTORY that migrates from a pre-fork (Heaven) name, same rules
/// as `local_file_migrated`. Returns the NEW path (not created if absent).
pub fn local_dir_migrated(name: &str, old_name: &str) -> PathBuf {
    let new = dll_dir().join(name);
    if !new.exists() {
        let old = dll_dir().join(old_name);
        if old.is_dir() {
            let _ = std::fs::rename(&old, &new);
        }
    }
    new
}

/// A MOD log file under `<dll dir>/trackside-logs/`. Ensures the folder exists
/// (migrating a pre-fork `heaven-logs/` folder in place if present).
pub fn log_file(name: &str) -> PathBuf {
    let dir = local_dir_migrated("trackside-logs", "heaven-logs");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(name)
}

fn localappdata_heaven() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("Heaven")
}

/// The Heaven dashboard's `data/` directory. Prefers the path the dashboard
/// published (`%LOCALAPPDATA%\Heaven\datadir.txt`); falls back to
/// `%LOCALAPPDATA%\Heaven\data`.
pub fn heaven_data_dir() -> PathBuf {
    let pointer = localappdata_heaven().join("datadir.txt");
    if let Ok(s) = std::fs::read_to_string(&pointer) {
        let p = PathBuf::from(s.trim());
        if !s.trim().is_empty() && p.is_dir() {
            return p;
        }
    }
    localappdata_heaven().join("data")
}

/// Where the MOD writes Team Trials captures: `<heaven data>/htt/native`.
pub fn tt_capture_dir() -> PathBuf {
    heaven_data_dir().join("htt").join("native")
}
