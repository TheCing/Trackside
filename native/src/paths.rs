//! Portable path resolution — the DLL no longer hardcodes the developer's
//! machine paths. Two kinds of paths:
//!
//!  - **MOD-local files** (settings, logs): live next to `heaven_overlay.dll`
//!    (i.e. inside the game folder), found via `GetModuleFileNameW`. Always
//!    correct on any machine, no config.
//!
//!  - **Team Trials captures**: go into the **Heaven dashboard's own data
//!    folder**. The dashboard publishes its `data/` path on startup to
//!    `%LOCALAPPDATA%\Heaven\datadir.txt`; we read that so captures land right
//!    next to the user's existing history. If the dashboard has never run, we
//!    fall back to `%LOCALAPPDATA%\Heaven\data` (the dashboard's importer also
//!    checks that location).

#![allow(dead_code)]

use std::path::PathBuf;

use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Directory that contains `heaven_overlay.dll` (the game folder). Falls back to
/// the current dir if the module can't be located.
pub fn dll_dir() -> PathBuf {
    unsafe {
        let h = GetModuleHandleW(wide("heaven_overlay.dll").as_ptr());
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

/// A MOD log file under `<dll dir>/heaven-logs/`. Ensures the folder exists.
pub fn log_file(name: &str) -> PathBuf {
    let dir = dll_dir().join("heaven-logs");
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
