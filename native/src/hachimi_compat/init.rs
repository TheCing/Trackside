//! Entry point — called from boot once il2cpp is ready.
//!
//! Enumerates `heaven_plugins/*.dll`, LoadLibrary's each, and calls any
//! `hachimi_init` export with our compatible vtable.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};

use super::il2cpp_api::api;
use super::vtable::VTABLE;
use super::{plog, sym, HachimiInitFn, InitResult, SDK_VERSION};

// How many external SDK plugins successfully initialised this run. Used by the native companion
// feed to step aside (an external plugin like CarrotBlender feeds the same UDP channel; running
// both would double-send and corrupt the stream).
static SDK_LOADED: AtomicU32 = AtomicU32::new(0);
pub fn sdk_plugins_loaded() -> u32 {
    SDK_LOADED.load(Ordering::Relaxed)
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// For every `*.dll` in `heaven_plugins/` exporting `hachimi_init`, hand it our
/// compatible vtable so it installs its hooks. DLLs without that export are
/// self-contained mods already started by the early loader — left alone.
pub fn init_plugins() -> String {
    let dir: PathBuf = crate::paths::local_dir_migrated("trackside_plugins", "heaven_plugins");
    if !dir.is_dir() {
        return "no trackside_plugins/ (skipped)".into();
    }
    if api().is_none() {
        return "il2cpp api unavailable (skipped)".into();
    }
    let mut dlls: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("dll")).unwrap_or(false))
            .collect(),
        Err(e) => return format!("read_dir failed: {e}"),
    };
    dlls.sort();
    if dlls.is_empty() {
        return "trackside_plugins/ empty".into();
    }

    let mut inited = 0u32;
    let mut notes = String::new();
    for path in &dlls {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let w = wide(path.as_os_str());
        let mut handle = unsafe { GetModuleHandleW(w.as_ptr()) };
        if handle.is_null() {
            handle = unsafe { LoadLibraryW(w.as_ptr()) };
        }
        if handle.is_null() {
            notes.push_str(&format!(" [{name}: load FAIL]"));
            continue;
        }
        let init: Option<HachimiInitFn> = unsafe { sym(handle, b"hachimi_init\0") };
        let Some(init) = init else {
            continue; // self-contained mod, not an SDK plugin
        };
        plog(&format!("calling hachimi_init: {name} (host v{SDK_VERSION})"));
        let res = unsafe { init(&VTABLE, SDK_VERSION) };
        if res == InitResult::Ok {
            inited += 1;
            plog(&format!("init OK: {name}"));
            notes.push_str(&format!(" [{name}: OK]"));
        } else {
            plog(&format!("init ERROR: {name}"));
            notes.push_str(&format!(" [{name}: ERROR]"));
        }
    }
    SDK_LOADED.store(inited, Ordering::Relaxed);
    if inited == 0 && notes.is_empty() {
        "no SDK plugins (none export hachimi_init)".into()
    } else {
        format!("{inited} initialised{notes}")
    }
}
