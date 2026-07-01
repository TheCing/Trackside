//! Generic plugin host (Heaven-as-primary coexistence layer).
//!
//! Loads external mod DLLs from a `heaven_plugins/` folder next to the overlay,
//! BEFORE Heaven installs its own IL2CPP hooks. Co-resident mods (e.g. a
//! localization mod) therefore install their detours FIRST, and Heaven layers /
//! chains on top — a deterministic load order, which is what avoids the broken
//! trampoline-chain crashes you get when two inline-hook engines fight at load.
//!
//! Each plugin DLL self-initialises through its own `DllMain` when loaded (that
//! is enough for proxy-style mods whose attach does all the work). If a DLL also
//! exports `heaven_plugin_init`, that is called too — our own ABI for plugins
//! that want an explicit, ordered init hook (NOT another mod's convention).
//!
//! Generic by design: the user drops whatever DLL they want into the folder; no
//! third-party mod is named or referenced here.

use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type InitFn = unsafe extern "C" fn();

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Load every `*.dll` in `<game>/heaven_plugins/` (sorted for a stable order) and
/// return a one-line summary for the boot log. Creates the folder on first run so
/// the user has an obvious place to drop a companion mod DLL into.
pub fn load() -> String {
    let dir: PathBuf = crate::paths::dll_dir().join("heaven_plugins");
    if !dir.is_dir() {
        // Auto-create on first boot; if that fails (e.g. read-only dir) just skip.
        if std::fs::create_dir_all(&dir).is_err() {
            return "no heaven_plugins/ folder (skipped)".into();
        }
        return "heaven_plugins/ created (empty)".into();
    }

    let mut dlls: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("dll"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => return format!("read_dir failed: {e}"),
    };
    dlls.sort();
    if dlls.is_empty() {
        return "heaven_plugins/ empty".into();
    }

    let mut loaded = 0u32;
    let mut notes = String::new();
    for path in &dlls {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let w = wide(path.as_os_str());
        let handle = unsafe { LoadLibraryW(w.as_ptr()) };
        if handle.is_null() {
            notes.push_str(&format!(" [FAIL {name}]"));
            continue;
        }
        loaded += 1;
        // Optional explicit init (our own export name, not a third party's).
        let proc = unsafe { GetProcAddress(handle, c"heaven_plugin_init".as_ptr() as *const u8) };
        match proc {
            Some(p) => {
                let f: InitFn = unsafe { std::mem::transmute(p as *const c_void) };
                unsafe { f() };
                notes.push_str(&format!(" [{name}: loaded+init]"));
            }
            None => notes.push_str(&format!(" [{name}: loaded]")),
        }
    }
    format!("{loaded}/{} loaded{notes}", dlls.len())
}
