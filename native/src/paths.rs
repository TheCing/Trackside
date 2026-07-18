//! Portable path resolution — the DLL no longer hardcodes the developer's
//! machine paths. Two kinds of paths:
//!
//!  - **MOD-local files** (settings, logs): live next to `trackside.dll`
//!    (i.e. inside the game folder), found via `GetModuleFileNameW`. Always
//!    correct on any machine, no config.
//!
//!  - **Team Trials captures**: go into the **Trackside Dashboard's own data
//!    folder**. The dashboard publishes its `data/` path on startup to
//!    `%LOCALAPPDATA%\Trackside\datadir.txt`; we read that so captures land right
//!    next to the user's existing history. If the dashboard has never run, we
//!    fall back to `%LOCALAPPDATA%\Trackside\data` (the dashboard's importer also
//!    checks that location). Both sides must agree on this folder name — see
//!    `safe_store.APPDATA_NAME` / `_publish_data_dir()` in the dashboard. The
//!    pre-rename name was `Heaven`; we still read it if it's the only one there.
//!
//!    This is fiddlier than it looks, because the **Windows Store Python**
//!    redirects the dashboard's `%LOCALAPPDATA%` writes into a per-package
//!    `LocalCache` sandbox — and it's the default `python` on a stock Windows box.
//!    The dashboard cannot see out of that sandbox, but we can see into it, so we
//!    search it too and deliberately distrust the pointer's contents when we find
//!    the dashboard living there. See `resolve_appdata_root` / `data_dir_for`.
//!
//! FORK MIGRATION: installs upgrading from Heaven have their state under the
//! old `heaven-*` names. `local_file_migrated` renames the old file to the new
//! name once (first run), so settings/state survive the rebrand.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

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

/// Folder names to look for, newest first (`Heaven` is the pre-rename name).
const APPDATA_NAMES: [&str; 2] = ["Trackside", "Heaven"];

/// The Windows **Store** Python sandboxes `%LOCALAPPDATA%` writes into
/// `%LOCALAPPDATA%\Packages\PythonSoftwareFoundation.Python.<ver>_<hash>\LocalCache\Local`.
/// The dashboard runs on whatever `python` resolves to, and on a stock Windows box
/// that's usually the Store build — which cannot escape its own redirect. We're a
/// native process with no redirection, so we look inside those sandboxes too.
///
/// Without this the overlay writes captures to the real `%LOCALAPPDATA%` while the
/// dashboard reads its sandbox, and "Import in-game" silently finds nothing.
fn store_python_locals(base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(base.join("Packages")) {
        for e in entries.flatten() {
            if e.file_name()
                .to_string_lossy()
                .starts_with("PythonSoftwareFoundation.Python.")
            {
                out.push(e.path().join("LocalCache").join("Local"));
            }
        }
    }
    out
}

/// True if `p` sits inside a Store-app LocalCache redirect.
fn is_sandboxed(p: &Path) -> bool {
    p.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("LocalCache"))
}

/// Pick the dashboard's AppData root under `base`. Pure so it can be tested.
///
/// `datadir.txt` is the tell: the dashboard writes it on every startup, so the root
/// holding it is the one the dashboard actually uses. We check that BEFORE mere
/// directory existence, because we may have created the real `Trackside\data`
/// ourselves as a fallback on an earlier run — its existence proves nothing.
fn resolve_appdata_root(base: &Path) -> PathBuf {
    let mut roots = vec![base.to_path_buf()];
    roots.extend(store_python_locals(base));

    for name in APPDATA_NAMES {
        for root in &roots {
            let dir = root.join(name);
            if dir.join("datadir.txt").is_file() {
                return dir;
            }
        }
    }
    for name in APPDATA_NAMES {
        for root in &roots {
            let dir = root.join(name);
            if dir.is_dir() {
                return dir;
            }
        }
    }
    base.join(APPDATA_NAMES[0]) // nothing yet → create under the current name
}

fn localappdata_trackside() -> PathBuf {
    let base = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()));
    resolve_appdata_root(&base)
}

/// Resolve the dashboard's `data/` dir under a given root. Pure so it can be tested.
fn data_dir_for(root: &Path) -> PathBuf {
    // Trust the published pointer ONLY when the root isn't sandboxed. Under Store
    // Python the pointer's CONTENTS are a lie — the dashboard writes the path it
    // *thinks* it used (the real `%LOCALAPPDATA%`) while its data actually lands in
    // the sandbox beside the pointer. Worse, that named path often does exist for
    // us (we may have created it), so `is_dir()` alone would happily follow the lie.
    if !is_sandboxed(root) {
        if let Ok(s) = std::fs::read_to_string(root.join("datadir.txt")) {
            let p = PathBuf::from(s.trim());
            if !s.trim().is_empty() && p.is_dir() {
                return p;
            }
        }
    }
    root.join("data")
}

/// The dashboard's `data/` directory.
///
/// Must stay in sync with the dashboard's `safe_store.APPDATA_NAME` and
/// `_publish_data_dir()` — this is a two-sided handshake.
pub fn dashboard_data_dir() -> PathBuf {
    data_dir_for(&localappdata_trackside())
}

/// Where the MOD writes Team Trials captures: `<dashboard data>/htt/native`.
pub fn tt_capture_dir() -> PathBuf {
    dashboard_data_dir().join("htt").join("native")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ts_paths_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn sandbox_of(base: &Path) -> PathBuf {
        base.join("Packages")
            .join("PythonSoftwareFoundation.Python.3.13_qbz5n2kfra8p0")
            .join("LocalCache")
            .join("Local")
    }
    fn publish(root: &Path, contents: &str) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("datadir.txt"), contents).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
    }

    /// Normal Python: dashboard publishes to the real %LOCALAPPDATA%.
    #[test]
    fn real_python_uses_real_root() {
        let base = tmp("real");
        let root = base.join("Trackside");
        publish(&root, root.join("data").to_str().unwrap());
        assert_eq!(resolve_appdata_root(&base), root);
        assert_eq!(data_dir_for(&root), root.join("data"));
    }

    /// Store Python: pointer lives in the sandbox and NAMES a real path we created.
    /// The lie must not be followed — captures belong beside the pointer.
    #[test]
    fn store_python_ignores_the_lying_pointer() {
        let base = tmp("store");
        // we (the overlay) created the real dir on an earlier run — it exists!
        let decoy = base.join("Trackside");
        fs::create_dir_all(decoy.join("data")).unwrap();
        // dashboard's real location: the sandbox
        let sand_root = sandbox_of(&base).join("Trackside");
        publish(&sand_root, decoy.join("data").to_str().unwrap());

        let root = resolve_appdata_root(&base);
        assert_eq!(root, sand_root, "datadir.txt must win over a bare decoy dir");
        assert!(is_sandboxed(&root));
        assert_eq!(
            data_dir_for(&root),
            sand_root.join("data"),
            "must NOT follow the pointer to the decoy"
        );
    }

    /// Pre-rename install: only a Heaven folder exists.
    #[test]
    fn legacy_heaven_still_found() {
        let base = tmp("legacy");
        let root = base.join("Heaven");
        publish(&root, root.join("data").to_str().unwrap());
        assert_eq!(resolve_appdata_root(&base), root);
    }

    /// Trackside wins over Heaven when both are published.
    #[test]
    fn current_name_beats_legacy() {
        let base = tmp("both");
        let new = base.join("Trackside");
        let old = base.join("Heaven");
        publish(&new, new.join("data").to_str().unwrap());
        publish(&old, old.join("data").to_str().unwrap());
        assert_eq!(resolve_appdata_root(&base), new);
    }

    /// Nothing yet → create under the current name, never the legacy one.
    #[test]
    fn fresh_machine_defaults_to_current_name() {
        let base = tmp("fresh");
        assert_eq!(resolve_appdata_root(&base), base.join("Trackside"));
    }

    /// A non-sandboxed pointer to a custom data dir is still honoured.
    #[test]
    fn real_pointer_to_custom_dir_is_honoured() {
        let base = tmp("custom");
        let root = base.join("Trackside");
        let custom = base.join("elsewhere").join("data");
        fs::create_dir_all(&custom).unwrap();
        publish(&root, custom.to_str().unwrap());
        assert_eq!(data_dir_for(&root), custom);
    }

    /// Diagnostic against the REAL machine (env-dependent → #[ignore] by default):
    ///   cargo test --release --lib paths::tests::real_env -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_env_resolution() {
        let base = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap());
        println!("  base            : {}", base.display());
        for s in store_python_locals(&base) {
            println!("  store sandbox   : {}", s.display());
        }
        let root = resolve_appdata_root(&base);
        println!("  resolved root   : {}", root.display());
        println!("  sandboxed?      : {}", is_sandboxed(&root));
        println!("  dashboard data  : {}", data_dir_for(&root).display());
        println!("  tt capture dir  : {}", data_dir_for(&root).join("htt").join("native").display());
    }
}
