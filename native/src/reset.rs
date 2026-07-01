//! Soft game reset — reloads Umamusume back to the title screen without killing the process, by
//! calling `Gallop.GameSystem.SoftwareReset()` on its singleton. Handy after changing settings or to
//! recover from a soft-lock while keeping the Steam session alive.
//!
//! Everything is resolved BY NAME at boot (so a game patch that shifts RVAs doesn't break it). The
//! overlay button (render thread) only sets a flag via `request()`; the actual managed call runs from
//! `poll()`, which is driven on the game's MAIN thread by hunter's per-frame `TweenManager.Update`
//! pump. Never call IL2CPP from the render thread.

#![allow(dead_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::il2cpp;

// Resolved once at boot. Method code pointers + their MethodInfo* (il2cpp static/instance methods take
// the MethodInfo as a trailing arg; for a 0-arg static it's the only arg, matching the padder pattern).
static GET_INSTANCE_FN: AtomicUsize = AtomicUsize::new(0);
static GET_INSTANCE_M: AtomicUsize = AtomicUsize::new(0);
static SOFT_RESET_FN: AtomicUsize = AtomicUsize::new(0);
static SOFT_RESET_M: AtomicUsize = AtomicUsize::new(0);
static IS_EXEC_FN: AtomicUsize = AtomicUsize::new(0);
static IS_EXEC_M: AtomicUsize = AtomicUsize::new(0);

static READY: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// True once the GameSystem methods resolved — the overlay uses this to enable/disable the button.
pub fn is_ready() -> bool {
    READY.load(Ordering::Relaxed)
}

/// Request a soft reset. Cheap + non-blocking; safe to call from the render thread. The reset itself
/// fires on the next main-thread pump.
pub fn request() {
    REQUESTED.store(true, Ordering::SeqCst);
}

/// Resolve `Gallop.GameSystem` + its singleton getter and reset methods. Run on an IL2CPP-attached
/// thread (boot). Idempotent-ish (safe to call once).
pub fn install() -> String {
    let gs = il2cpp::class("Gallop.GameSystem");
    if gs.is_null() {
        return "GameSystem class missing".into();
    }
    // get_Instance is inherited from MonoSingleton<GameSystem>; class_get_method_from_name walks the
    // hierarchy and returns the concrete instantiation's method.
    let gi = il2cpp::method(gs, "get_Instance", 0);
    let sr = il2cpp::method(gs, "SoftwareReset", 0);
    let ie = il2cpp::method(gs, "IsExecutingSoftwareReset", 0);
    if gi.is_null() || sr.is_null() {
        return format!(
            "methods missing (get_Instance={} SoftwareReset={})",
            !gi.is_null(),
            !sr.is_null()
        );
    }
    let gip = il2cpp::method_pointer(gi);
    let srp = il2cpp::method_pointer(sr);
    if gip.is_null() || srp.is_null() {
        return "method pointers null".into();
    }
    GET_INSTANCE_FN.store(gip as usize, Ordering::Relaxed);
    GET_INSTANCE_M.store(gi as usize, Ordering::Relaxed);
    SOFT_RESET_FN.store(srp as usize, Ordering::Relaxed);
    SOFT_RESET_M.store(sr as usize, Ordering::Relaxed);
    if !ie.is_null() {
        let iep = il2cpp::method_pointer(ie);
        if !iep.is_null() {
            IS_EXEC_FN.store(iep as usize, Ordering::Relaxed);
            IS_EXEC_M.store(ie as usize, Ordering::Relaxed);
        }
    }
    READY.store(true, Ordering::Relaxed);
    "OK".into()
}

/// Main-thread pump (called from hunter's TweenManager.Update tick). If a reset was requested and none
/// is already in progress, calls GameSystem.SoftwareReset() exactly once.
pub fn poll() {
    if !REQUESTED.swap(false, Ordering::SeqCst) {
        return;
    }
    if !READY.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let gip = GET_INSTANCE_FN.load(Ordering::Relaxed);
        let gi_m = GET_INSTANCE_M.load(Ordering::Relaxed);
        if gip == 0 || gi_m == 0 {
            return;
        }
        // static T get_Instance(MethodInfo*)
        let get_inst: extern "C" fn(*const c_void) -> *mut c_void = std::mem::transmute(gip);
        let inst = get_inst(gi_m as *const c_void);
        if inst.is_null() {
            return;
        }

        // Skip if a soft reset is already running (avoids re-entrancy / double reload).
        let ie = IS_EXEC_FN.load(Ordering::Relaxed);
        let ie_m = IS_EXEC_M.load(Ordering::Relaxed);
        if ie != 0 && ie_m != 0 {
            // static bool IsExecutingSoftwareReset(MethodInfo*)
            let is_exec: extern "C" fn(*const c_void) -> bool = std::mem::transmute(ie);
            if is_exec(ie_m as *const c_void) {
                return;
            }
        }

        let srp = SOFT_RESET_FN.load(Ordering::Relaxed);
        let sr_m = SOFT_RESET_M.load(Ordering::Relaxed);
        if srp == 0 || sr_m == 0 {
            return;
        }
        // void SoftwareReset(this, MethodInfo*)
        let soft_reset: extern "C" fn(*mut c_void, *const c_void) = std::mem::transmute(srp);
        soft_reset(inst, sr_m as *const c_void);
    }
}
