//! Heaven — crash detector. Installs a last-chance unhandled-exception filter that, when the
//! game crashes, writes `heaven-crash.log` with the exception code, the faulting address, WHICH
//! module that address is in (ours = `heaven_overlay.dll`, the game = `GameAssembly.dll`, …) and
//! the last "breadcrumb" — the hook that was executing. That pinpoints which feature crashed.
//!
//! The breadcrumb is a single cheap atomic the risky hooks stamp on entry (no I/O on the hot
//! path), so the only cost during normal play is one relaxed store per hooked call.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

// Minimal SEH structs (manual FFI — avoids depending on windows-sys's Diagnostics module layout).
#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut c_void,
    num_params: u32,
    info: [usize; 15], // repr(C) pads `num_params` so this lands at offset 32, matching Win32
}
#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut c_void,
}
type TopFilter = Option<unsafe extern "system" fn(*const ExceptionPointers) -> i32>;
#[link(name = "kernel32")]
extern "system" {
    fn SetUnhandledExceptionFilter(filter: TopFilter) -> TopFilter;
}

static BREADCRUMB: AtomicU32 = AtomicU32::new(0);

/// Stamp the current execution point (called on entry to risky hooks). Cheap relaxed store.
#[inline]
pub fn crumb(code: u32) {
    BREADCRUMB.store(code, Ordering::Relaxed);
}

fn crumb_name(c: u32) -> &'static str {
    match c {
        0 => "none (crashed outside our hooks)",
        1 => "boot: graphics::install",
        2 => "boot: display::install",
        3 => "boot: display::install_window",
        4 => "boot: cyspring::install",
        11 => "display::on_get_width (Gallop.Screen.get_Width)",
        12 => "display::on_get_height (Gallop.Screen.get_Height)",
        13 => "display::on_set_resolution (UnityEngine.Screen.SetResolution)",
        14 => "display::on_resize_ui (UIManager.ChangeResizeUIForPC)",
        15 => "display::apply_ui_scale (CanvasScaler array)",
        16 => "display::recreate_RT (CreateRenderTextureFromScreen)",
        21 => "graphics::on_apply_quality (ApplyGraphicsQuality)",
        31 => "cyspring::on_init (CySpringController.Init)",
        _ => "?",
    }
}

fn write_crash(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-crash.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Resolve which loaded module an address belongs to → (file name, offset from module base).
unsafe fn module_for(addr: usize) -> (String, usize) {
    let mut hmod: HMODULE = std::ptr::null_mut();
    let ok = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        addr as *const u16,
        &mut hmod,
    );
    if ok == 0 || hmod.is_null() {
        return ("<unknown>".into(), addr);
    }
    let mut buf = [0u16; 260];
    let n = GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32) as usize;
    let path = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
    let name = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string();
    (name, addr.wrapping_sub(hmod as usize))
}

unsafe extern "system" fn handler(info: *const ExceptionPointers) -> i32 {
    const CONTINUE_SEARCH: i32 = 0; // let Windows / WER crash normally after we log
    if info.is_null() {
        return CONTINUE_SEARCH;
    }
    let rec = (*info).record;
    if rec.is_null() {
        return CONTINUE_SEARCH;
    }
    let code = (*rec).code;
    let addr = (*rec).address as usize;
    let (module, off) = module_for(addr);
    let bc = BREADCRUMB.load(Ordering::Relaxed);

    let mut extra = String::new();
    // 0xC0000005 = access violation → record read/write/execute + the bad data address.
    if code == 0xC000_0005 && (*rec).num_params >= 2 {
        let kind = match (*rec).info[0] {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "?",
        };
        let at = (*rec).info[1];
        extra = format!("\n  access violation: {kind} at 0x{at:016x}");
    }

    write_crash(&format!(
        "\n=== CRASH ===\n  code   : 0x{code:08x}\n  at     : 0x{addr:016x}  ({module} + 0x{off:x})\n  hook   : [{bc}] {}{extra}\n=============",
        crumb_name(bc)
    ));
    CONTINUE_SEARCH
}

/// Arm the crash detector. Re-armed a few times because the game's own crash handler
/// (Unity) installs later and would otherwise replace ours.
pub fn install() {
    unsafe {
        SetUnhandledExceptionFilter(Some(handler));
    }
    write_crash("--- heaven crash detector armed ---");
    std::thread::spawn(|| {
        for delay in [2u64, 6, 12] {
            std::thread::sleep(std::time::Duration::from_secs(delay));
            unsafe {
                SetUnhandledExceptionFilter(Some(handler));
            }
        }
    });
}
