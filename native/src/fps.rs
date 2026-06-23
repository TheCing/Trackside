//! Heaven Plan B — B5: native FPS control (port of core/modules/fps_unlock.js v3).
//!
//! Two problems make a one-shot set fail: the game re-sets BOTH
//! Application.targetFrameRate AND QualitySettings.vSyncCount every frame. vSync
//! takes precedence over targetFrameRate, so if the game keeps vSyncCount>0 you
//! stay capped (e.g. 30) no matter the target. We therefore HOOK both setters
//! (clamp-guard) and, while a cap is active, force vSyncCount=0 and the target
//! to our value — the game can no longer override us.
//!
//!   value:  0 = off (no override)   -1 = uncapped   N>0 = cap at N
//! Both are static methods → compiled signature `void f(i32 value, MethodInfo*)`.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;

type SetIntStatic = unsafe extern "C" fn(i32, *mut c_void);

static TARGET_TRAMP: AtomicUsize = AtomicUsize::new(0);
static TARGET_MI: AtomicUsize = AtomicUsize::new(0);
static VSYNC_TRAMP: AtomicUsize = AtomicUsize::new(0);
static VSYNC_MI: AtomicUsize = AtomicUsize::new(0);
static CURRENT: AtomicI32 = AtomicI32::new(0); // requested cap (0 = off)

static TARGET_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static VSYNC_DETOUR: OnceLock<RawDetour> = OnceLock::new();

pub fn current() -> i32 {
    CURRENT.load(Ordering::Relaxed)
}

#[inline]
unsafe fn call_tramp(tramp: &AtomicUsize, mi: &AtomicUsize, value: i32) {
    let t = tramp.load(Ordering::Relaxed);
    if t != 0 {
        let f: SetIntStatic = std::mem::transmute(t);
        f(value, mi.load(Ordering::Relaxed) as *mut c_void);
    }
}

/// Clamp-guard on Application.set_targetFrameRate: while a cap is active, every
/// call (ours or the game's) is forced to our value. cap 0 → pass through.
unsafe extern "C" fn target_hook(incoming: i32, mi: *mut c_void) {
    let cap = CURRENT.load(Ordering::Relaxed);
    let value = if cap == 0 { incoming } else { cap };
    let t = TARGET_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let f: SetIntStatic = std::mem::transmute(t);
        f(value, mi);
    }
}

/// Clamp-guard on QualitySettings.set_vSyncCount: while a cap is active, force
/// vSync OFF (0) so the target frame rate actually applies. cap 0 → pass through.
unsafe extern "C" fn vsync_hook(incoming: i32, mi: *mut c_void) {
    let cap = CURRENT.load(Ordering::Relaxed);
    let value = if cap == 0 { incoming } else { 0 };
    let t = VSYNC_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let f: SetIntStatic = std::mem::transmute(t);
        f(value, mi);
    }
}

/// Apply an FPS cap. 0 = off, -1 = uncapped (+vSync off), N = cap at N (+vSync off).
pub fn set_cap(value: i32) {
    CURRENT.store(value, Ordering::Relaxed);
    if value == 0 {
        return; // hooks now pass the game's own values through
    }
    // Apply immediately via the trampolines (the hooks keep enforcing after).
    unsafe {
        call_tramp(&VSYNC_TRAMP, &VSYNC_MI, 0);
        call_tramp(&TARGET_TRAMP, &TARGET_MI, value);
    }
}

unsafe fn hook(
    klass: il2cpp::Class,
    name: &str,
    det: *const (),
    tramp: &AtomicUsize,
    mi: &AtomicUsize,
    keep: &OnceLock<RawDetour>,
) -> Result<(), String> {
    let m = il2cpp::method(klass, name, 1);
    if m.is_null() {
        return Err(format!("{name} miss"));
    }
    mi.store(m as usize, Ordering::Relaxed);
    let target = il2cpp::method_pointer(m);
    if target.is_null() {
        return Err(format!("{name} ptr null"));
    }
    if il2cpp::is_detoured(target) {
        return Err(format!("{name}: already detoured (skipped)"));
    }
    let d = RawDetour::new(target as *const (), det).map_err(|e| format!("{name}: {e}"))?;
    d.enable().map_err(|e| format!("{name} enable: {e}"))?;
    tramp.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
    let _ = keep.set(d);
    Ok(())
}

/// Resolve setters + install both clamp-guards. Call after il2cpp::init.
pub fn install() -> Result<(), String> {
    let app = il2cpp::class("UnityEngine.Application");
    if app.is_null() {
        return Err("app miss".into());
    }
    unsafe {
        hook(app, "set_targetFrameRate", target_hook as *const (), &TARGET_TRAMP, &TARGET_MI, &TARGET_DETOUR)?;
    }
    // vSync is optional but important — without disabling it the target is ignored.
    let q = il2cpp::class("UnityEngine.QualitySettings");
    if !q.is_null() {
        unsafe {
            let _ = hook(q, "set_vSyncCount", vsync_hook as *const (), &VSYNC_TRAMP, &VSYNC_MI, &VSYNC_DETOUR);
        }
    }
    Ok(())
}
