//! Heaven — display & window QoL. Four independent, cosmetic/QoL tweaks:
//!
//!   #3 Always-on-top + block-minimize — pure Win32 on the game window.
//!   #2 Borderless / fullscreen mode    — hook `UnityEngine.Screen.SetResolution_Injected`
//!                                         and substitute the requested full-screen mode.
//!   #1 Render scale (super-sampling)    — hook `Gallop.Screen.get_Width/get_Height` to return
//!                                         a scaled internal resolution (scales the WHOLE
//!                                         pipeline consistently), and recreate the 3D render
//!                                         texture on resize via `UIManager.ChangeResizeUIForPC`
//!                                         (this is the piece a per-component scale was missing).
//!   #4 UI scale                          — in the same resize hook, set `CanvasScaler.scaleFactor`
//!                                         on every canvas scaler the UIManager owns.
//!
//! Everything defaults to OFF / 1.0 and resolves defensively: a missing class/method is logged
//! and skipped, never fatal. No gameplay effect → ships in every build.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;

fn log(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

// ── shared settings ──────────────────────────────────────────────────────────
static ALWAYS_ON_TOP: AtomicBool = AtomicBool::new(false);
static BLOCK_MINIMIZE: AtomicBool = AtomicBool::new(false);
static DISPLAY_MODE: AtomicI32 = AtomicI32::new(0); // 0 = off, 1 = borderless, 2 = exclusive, 3 = windowed
static RENDER_SCALE: AtomicU32 = AtomicU32::new(0x3f80_0000); // f32 1.0
static UI_SCALE: AtomicU32 = AtomicU32::new(0x3f80_0000); // f32 1.0
static LOW_SPEC: AtomicBool = AtomicBool::new(false);

pub fn set_low_spec(on: bool) {
    LOW_SPEC.store(on, Ordering::Relaxed);
}

pub fn always_on_top() -> bool { ALWAYS_ON_TOP.load(Ordering::Relaxed) }
pub fn block_minimize() -> bool { BLOCK_MINIMIZE.load(Ordering::Relaxed) }
pub fn display_mode() -> i32 { DISPLAY_MODE.load(Ordering::Relaxed) }
pub fn render_scale() -> f32 { f32::from_bits(RENDER_SCALE.load(Ordering::Relaxed)) }
pub fn ui_scale() -> f32 { f32::from_bits(UI_SCALE.load(Ordering::Relaxed)) }

pub fn set_block_minimize(on: bool) { BLOCK_MINIMIZE.store(on, Ordering::Relaxed); }
pub fn set_display_mode(m: i32) { DISPLAY_MODE.store(m, Ordering::Relaxed); }
pub fn set_render_scale(s: f32) { RENDER_SCALE.store(s.clamp(1.0, 2.0).to_bits(), Ordering::Relaxed); }
pub fn set_ui_scale(s: f32) { UI_SCALE.store(s.clamp(0.7, 1.5).to_bits(), Ordering::Relaxed); }

// ════════════════════════════ #3 Win32: window ═══════════════════════════════
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, FindWindowW, GetWindowThreadProcessId, SetWindowPos, SetWindowsHookExW,
    HCBT_MINMAX, HHOOK, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SW_RESTORE, WH_CBT,
};

static GAME_HWND: AtomicUsize = AtomicUsize::new(0);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn find_hwnd() -> HWND {
    let cur = GAME_HWND.load(Ordering::Relaxed);
    if cur != 0 {
        return cur as HWND;
    }
    let cls = wide("UnityWndClass");
    // Global title is "Umamusume"; FindWindow is case-insensitive on the title.
    for title in ["umamusume", "UmamusumePrettyDerby_Jpn"] {
        let t = wide(title);
        let h = unsafe { FindWindowW(cls.as_ptr(), t.as_ptr()) };
        if !h.is_null() {
            GAME_HWND.store(h as usize, Ordering::Relaxed);
            return h;
        }
    }
    std::ptr::null_mut()
}

pub fn set_always_on_top(on: bool) {
    ALWAYS_ON_TOP.store(on, Ordering::Relaxed);
    // SetWindowPos sends synchronous messages to the window's UI thread. Calling it from the
    // overlay's render/Present thread deadlocks the game, so apply it from a worker thread.
    std::thread::spawn(move || {
        let h = find_hwnd();
        if h.is_null() {
            return;
        }
        let insert_after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
        unsafe {
            SetWindowPos(h, insert_after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    });
}

static mut HCBT: HHOOK = std::ptr::null_mut();
unsafe extern "system" fn cbt_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // HCBT_MINMAX: a window is being minimized/maximized. Block minimize when enabled.
    if ncode == HCBT_MINMAX as i32 && lparam as i32 != SW_RESTORE && BLOCK_MINIMIZE.load(Ordering::Relaxed) {
        return 1; // non-zero = swallow the operation
    }
    CallNextHookEx(HCBT, ncode, wparam, lparam)
}

/// Install the CBT hook on the game window's UI thread (for block-minimize) and apply
/// always-on-top if it was persisted on. Best-effort; safe to call once at boot.
pub fn install_window() {
    let h = find_hwnd();
    if h.is_null() {
        log("[display] game window not found (window QoL deferred)");
        return;
    }
    unsafe {
        let tid = GetWindowThreadProcessId(h, std::ptr::null_mut());
        if tid != 0 {
            // A thread-specific CBT hook avoids touching other processes.
            let hh = SetWindowsHookExW(WH_CBT, Some(cbt_proc), std::ptr::null_mut(), tid);
            if !hh.is_null() {
                HCBT = hh;
            }
        }
    }
    if ALWAYS_ON_TOP.load(Ordering::Relaxed) {
        set_always_on_top(true);
    }
    log("[display] window QoL installed");
}

// ════════════════════ #2 UnityEngine.Screen.SetResolution ════════════════════
#[repr(C)]
struct RefreshRate {
    numerator: u32,
    denominator: u32,
}

static TR_SETRES: AtomicUsize = AtomicUsize::new(0);
static D_SETRES: OnceLock<RawDetour> = OnceLock::new();

// SetResolution_Injected(width, height, FullScreenMode, RefreshRate*) — a raw icall (no MethodInfo).
unsafe extern "C" fn on_set_resolution(w: i32, h: i32, mode: i32, refresh: *const RefreshRate) {
    crate::crashlog::crumb(13);
    let t = TR_SETRES.load(Ordering::Relaxed);
    if t == 0 {
        return;
    }
    let orig: unsafe extern "C" fn(i32, i32, i32, *const RefreshRate) = std::mem::transmute(t);
    // Our display mode → Unity FullScreenMode (Exclusive=0, FullScreenWindow=1, Windowed=3).
    let m = match DISPLAY_MODE.load(Ordering::Relaxed) {
        1 => 1, // Borderless → FullScreenWindow
        2 => 0, // Exclusive
        3 => 3, // Windowed
        _ => mode,
    };
    orig(w, h, m, refresh);
}

// NOTE: render scale (#1) was implemented by hooking `Gallop.Screen.get_Width/get_Height`, but
// those are tiny thunk getters and `retour`'s relocated trampoline faults when called (access
// violation in trampoline memory). Removed — the lever is not safe with our hooker.

// ════════════════════ #4 Gallop.UIManager.ChangeResizeUIForPC (UI scale) ══════════════════
static TR_RESIZE: AtomicUsize = AtomicUsize::new(0);
static D_RESIZE: OnceLock<RawDetour> = OnceLock::new();
// Resolved UIManager / CanvasScaler members (Method handles).
static M_CANVAS_LIST: AtomicUsize = AtomicUsize::new(0); // UIManager.GetCanvasScalerList() -> Array
static M_SET_SCALEFACTOR: AtomicUsize = AtomicUsize::new(0); // CanvasScaler.set_scaleFactor(float)

unsafe fn apply_ui_scale(_uimgr: *mut c_void) {
    // DISABLED 2026-06-14 — this crashed the game (0xC0000005, crash breadcrumb 15) when UI
    // scale was ≠ 1.0. Root cause: `GetCanvasScalerList()` returns a `List<CanvasScaler>`,
    // NOT a raw Il2CppArray, so reading max_length@0x18 / elements@0x20 off the List object
    // dereferenced garbage pointers. UI scale is a minor QoL; disabled until the List<T>
    // layout is verified (backing array `_items`@+0x10, `_size`@+0x18, then iterate that
    // array's elements@+0x20). The ChangeResizeUIForPC hook still calls the original
    // untouched, so window resizing is unaffected — only the scale application is off.
}

// ChangeResizeUIForPC(this, width, height, MethodInfo*) — runs on the game's main thread.
unsafe extern "C" fn on_resize_ui(this: *mut c_void, w: i32, h: i32, method: *mut c_void) {
    crate::crashlog::crumb(14);
    let t = TR_RESIZE.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void, i32, i32, *mut c_void) = std::mem::transmute(t);
        orig(this, w, h, method);
    }
    apply_ui_scale(this);
}

// ════════════════════════════════ install ════════════════════════════════════
pub fn install() -> Result<(), String> {
    let mut notes: Vec<&str> = Vec::new();

    // #2 — UnityEngine.Screen.SetResolution_Injected (raw icall).
    let setres = il2cpp::resolve_icall("UnityEngine.Screen::SetResolution_Injected(System.Int32,System.Int32,UnityEngine.FullScreenMode,UnityEngine.RefreshRate)");
    if !setres.is_null() && !unsafe { il2cpp::is_detoured(setres) } {
        if let Ok(d) = unsafe { RawDetour::new(setres as *const (), on_set_resolution as *const ()) } {
            if unsafe { d.enable() }.is_ok() {
                TR_SETRES.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = D_SETRES.set(d);
                notes.push("fullscreen");
            }
        }
    }

    // #4 — Gallop.UIManager.ChangeResizeUIForPC + member methods (UI scale).
    let uimgr = il2cpp::class("Gallop.UIManager");
    if !uimgr.is_null() {
        M_CANVAS_LIST.store(il2cpp::method(uimgr, "GetCanvasScalerList", 0) as usize, Ordering::Relaxed);
        if let Some(cs) = {
            let c = il2cpp::class("UnityEngine.UI.CanvasScaler");
            if c.is_null() { None } else { Some(c) }
        } {
            M_SET_SCALEFACTOR.store(il2cpp::method(cs, "set_scaleFactor", 1) as usize, Ordering::Relaxed);
        }
        unsafe {
            if il2cpp::hook_method(uimgr, "ChangeResizeUIForPC", 2, on_resize_ui as *const (), &TR_RESIZE, &D_RESIZE).is_ok() {
                notes.push("uiscale");
            }
        }
    }

    if notes.is_empty() {
        return Err("no display hooks installed".into());
    }
    log(&format!("[display] hooks: {}", notes.join(", ")));
    Ok(())
}
