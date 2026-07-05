//! Trackside loader — a `version.dll` proxy.
//!
//! Dropped into the game folder, the EXE's import of `version.dll` resolves to
//! this DLL instead of `C:\Windows\System32\version.dll`. We do three things in
//! `DllMain` (mirroring the behavior of the loader this fork replaces, so the
//! boot order the overlay expects is preserved):
//!
//!   1. apply a staged self-update: `trackside.dll.new` → `trackside.dll`
//!      (the overlay downloads updates but can't replace itself while loaded);
//!   2. early-load every DLL in `trackside_plugins/` (self-contained proxy-style
//!      mods need to install their hooks before IL2CPP boots; SDK-style plugins
//!      get their `hachimi_init` called later by the overlay itself);
//!   3. load `trackside.dll` (the overlay).
//!
//! Every real `version.dll` export is forwarded to the genuine DLL in
//! System32, resolved lazily on first call.

#![allow(non_snake_case, clippy::missing_safety_doc)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{
    DisableThreadLibraryCalls, GetModuleFileNameW, GetProcAddress, LoadLibraryW,
};
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

// ── forward target ────────────────────────────────────────────────────────────

/// Our own directory (the game folder), captured in DllMain.
static OWN_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The DLL we forward the version APIs to. CHAIN-COMPATIBLE: if the game folder
/// has a `heaven_version.dll` (the next proxy in a stacked install — e.g. Hachimi's
/// own version-proxy, renamed so it can sit behind the master loader), we forward
/// to it so its DllMain boots its mod and the chain stays intact. Otherwise we
/// forward straight to the genuine `C:\Windows\System32\version.dll` (loaded by
/// FULL path — a bare "version.dll" would resolve back to us).
static REAL: OnceLock<usize> = OnceLock::new();

fn real_dll() -> HMODULE {
    let h = *REAL.get_or_init(|| unsafe {
        if let Some(dir) = OWN_DIR.get() {
            let chained = dir.join("heaven_version.dll");
            if chained.exists() {
                let mut w: Vec<u16> = chained.as_os_str().to_string_lossy().encode_utf16().collect();
                w.push(0);
                let h = LoadLibraryW(w.as_ptr());
                if !h.is_null() {
                    return h as usize;
                }
            }
        }
        let mut buf = [0u16; 512];
        let n = GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) as usize;
        let mut path: Vec<u16> = buf[..n].to_vec();
        path.extend("\\version.dll".encode_utf16());
        path.push(0);
        LoadLibraryW(path.as_ptr()) as usize
    });
    h as HMODULE
}

/// Resolve an export from the real DLL. Panics never — a missing export returns
/// a null fn and the forward simply fails the call (matches a broken system DLL).
unsafe fn real_fn(name: &[u8]) -> *const c_void {
    let h = real_dll();
    if h.is_null() {
        return std::ptr::null();
    }
    GetProcAddress(h, name.as_ptr()).map(|f| f as *const c_void).unwrap_or(std::ptr::null())
}

/// Define a forwarding export: same name, `extern "system"`, lazy-resolved.
macro_rules! forward {
    ($name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty) => {
        #[no_mangle]
        pub unsafe extern "system" fn $name($($arg: $ty),*) -> $ret {
            static SLOT: OnceLock<usize> = OnceLock::new();
            let p = *SLOT.get_or_init(|| real_fn(concat!(stringify!($name), "\0").as_bytes()) as usize);
            if p == 0 {
                return std::mem::zeroed();
            }
            let f: unsafe extern "system" fn($($ty),*) -> $ret = std::mem::transmute(p);
            f($($arg),*)
        }
    };
}

type BOOL = i32;
type DWORD = u32;
type PCSTR = *const u8;
type PCWSTR = *const u16;
type PSTR = *mut u8;
type PWSTR = *mut u16;

forward!(GetFileVersionInfoA(f: PCSTR, h: DWORD, len: DWORD, data: *mut c_void) -> BOOL);
forward!(GetFileVersionInfoW(f: PCWSTR, h: DWORD, len: DWORD, data: *mut c_void) -> BOOL);
forward!(GetFileVersionInfoExA(fl: DWORD, f: PCSTR, h: DWORD, len: DWORD, data: *mut c_void) -> BOOL);
forward!(GetFileVersionInfoExW(fl: DWORD, f: PCWSTR, h: DWORD, len: DWORD, data: *mut c_void) -> BOOL);
forward!(GetFileVersionInfoSizeA(f: PCSTR, out: *mut DWORD) -> DWORD);
forward!(GetFileVersionInfoSizeW(f: PCWSTR, out: *mut DWORD) -> DWORD);
forward!(GetFileVersionInfoSizeExA(fl: DWORD, f: PCSTR, out: *mut DWORD) -> DWORD);
forward!(GetFileVersionInfoSizeExW(fl: DWORD, f: PCWSTR, out: *mut DWORD) -> DWORD);
forward!(VerFindFileA(fl: DWORD, file: PCSTR, win: PCSTR, app: PCSTR, cur: PSTR, curlen: *mut u32, dest: PSTR, destlen: *mut u32) -> DWORD);
forward!(VerFindFileW(fl: DWORD, file: PCWSTR, win: PCWSTR, app: PCWSTR, cur: PWSTR, curlen: *mut u32, dest: PWSTR, destlen: *mut u32) -> DWORD);
forward!(VerInstallFileA(fl: DWORD, src: PCSTR, dst: PCSTR, srcdir: PCSTR, dstdir: PCSTR, curdir: PCSTR, tmp: PSTR, tmplen: *mut u32) -> DWORD);
forward!(VerInstallFileW(fl: DWORD, src: PCWSTR, dst: PCWSTR, srcdir: PCWSTR, dstdir: PCWSTR, curdir: PCWSTR, tmp: PWSTR, tmplen: *mut u32) -> DWORD);
forward!(VerLanguageNameA(lang: DWORD, name: PSTR, cch: DWORD) -> DWORD);
forward!(VerLanguageNameW(lang: DWORD, name: PWSTR, cch: DWORD) -> DWORD);
forward!(VerQueryValueA(block: *const c_void, sub: PCSTR, buf: *mut *mut c_void, len: *mut u32) -> BOOL);
forward!(VerQueryValueW(block: *const c_void, sub: PCWSTR, buf: *mut *mut c_void, len: *mut u32) -> BOOL);

// ── boot ──────────────────────────────────────────────────────────────────────

/// The folder this proxy lives in (= the game folder).
fn own_dir(hinst: HMODULE) -> PathBuf {
    unsafe {
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(hinst, buf.as_mut_ptr(), buf.len() as u32) as usize;
        let full = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        full.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Swap a staged overlay update in: `trackside.dll.new` → `trackside.dll`.
/// Best-effort with a couple of retries (AV scanners briefly hold new files).
fn apply_staged_update(dir: &PathBuf) {
    let staged = dir.join("trackside.dll.new");
    if !staged.exists() {
        return;
    }
    let live = dir.join("trackside.dll");
    for _ in 0..5 {
        let _ = std::fs::remove_file(&live);
        if std::fs::rename(&staged, &live).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Early-load self-contained plugin DLLs (before the overlay and IL2CPP), so
/// proxy-style mods get their hooks in on time. SDK-style plugins loaded here
/// just sit idle until the overlay calls their `hachimi_init` later.
fn load_plugins_early(dir: &PathBuf) {
    let plugins = dir.join("trackside_plugins");
    let Ok(rd) = std::fs::read_dir(&plugins) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map(|x| x.eq_ignore_ascii_case("dll")).unwrap_or(false) {
            let mut w: Vec<u16> = p.as_os_str().to_string_lossy().encode_utf16().collect();
            w.push(0);
            unsafe {
                LoadLibraryW(w.as_ptr());
            }
        }
    }
}

fn boot(hinst: HMODULE) {
    let dir = own_dir(hinst);
    let _ = OWN_DIR.set(dir.clone());
    // Wake the next proxy in the chain NOW (not lazily on the first version-API
    // call) so a stacked mod (e.g. Hachimi) boots at the same point it used to.
    let _ = real_dll();
    apply_staged_update(&dir);
    load_plugins_early(&dir);
    let overlay = dir.join("trackside.dll");
    let mut w: Vec<u16> = overlay.as_os_str().to_string_lossy().encode_utf16().collect();
    w.push(0);
    unsafe {
        LoadLibraryW(w.as_ptr());
    }
}

const DLL_PROCESS_ATTACH: u32 = 1;

#[no_mangle]
pub unsafe extern "system" fn DllMain(hinst: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        DisableThreadLibraryCalls(hinst);
        // Synchronous on purpose: the loader we replace also boots in DllMain, and
        // the plugin early-load must beat the game's IL2CPP init on the main thread.
        boot(hinst);
    }
    1
}
