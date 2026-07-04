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
type SetIntIcall = unsafe extern "C" fn(i32); // native icall: NO trailing MethodInfo*

static TARGET_TRAMP: AtomicUsize = AtomicUsize::new(0);
static TARGET_MI: AtomicUsize = AtomicUsize::new(0);
static VSYNC_TRAMP: AtomicUsize = AtomicUsize::new(0);
static VSYNC_MI: AtomicUsize = AtomicUsize::new(0);
static TARGET_ICALL_TRAMP: AtomicUsize = AtomicUsize::new(0);
static VSYNC_ICALL_TRAMP: AtomicUsize = AtomicUsize::new(0);
static CURRENT: AtomicI32 = AtomicI32::new(0); // requested cap (0 = off)

static TARGET_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static VSYNC_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static TARGET_ICALL_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static VSYNC_ICALL_DETOUR: OnceLock<RawDetour> = OnceLock::new();

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

/// Clamp-guard on the NATIVE set_targetFrameRate icall. `Gallop.FrameRateController` resolves this
/// via `il2cpp_resolve_icall` and calls it DIRECTLY, bypassing the managed setter thunk above — that
/// is why in-event overrides (`OverrideByNormalFrameRate` -> 30) started slipping past the clamp
/// after the 2026-07-01 update and the fps dropped to 30. Hooking the icall covers that path too.
/// Signature is `void(i32)` — the icall gets NO trailing MethodInfo*.
unsafe extern "C" fn target_icall_hook(incoming: i32) {
    let cap = CURRENT.load(Ordering::Relaxed);
    let value = if cap == 0 { incoming } else { cap };
    let t = TARGET_ICALL_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let f: SetIntIcall = std::mem::transmute(t);
        f(value);
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

/// Clamp-guard on the NATIVE set_vSyncCount icall. The managed QualitySettings.set_vSyncCount setter was
/// STRIPPED from the il2cpp build (the game stopped calling it, so the linker dropped it) — hooking it by
/// name now misses. The engine icall is still registered, so we guard that instead: while a cap is active
/// every vSync write (ours or the engine's) is forced to 0 so the target frame rate actually applies.
/// Signature is `void(i32)` — the icall gets NO trailing MethodInfo*.
unsafe extern "C" fn vsync_icall_hook(incoming: i32) {
    let cap = CURRENT.load(Ordering::Relaxed);
    let value = if cap == 0 { incoming } else { 0 };
    let t = VSYNC_ICALL_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let f: SetIntIcall = std::mem::transmute(t);
        f(value);
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
        // vSync off — prefer the icall tramp (managed setter is stripped from this build),
        // fall back to the managed tramp if only that one was hookable.
        let vi = VSYNC_ICALL_TRAMP.load(Ordering::Relaxed);
        if vi != 0 {
            let f: SetIntIcall = std::mem::transmute(vi);
            f(0);
        } else {
            call_tramp(&VSYNC_TRAMP, &VSYNC_MI, 0);
        }
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

/// Resolve setters + install the managed clamp-guards AND the native-icall guard. Returns a status
/// string for the boot log (so a future break is visible). Call after il2cpp::init.
pub fn install() -> String {
    let app = il2cpp::class("UnityEngine.Application");
    if app.is_null() {
        return "app class miss".into();
    }
    let mut notes = String::new();
    unsafe {
        match hook(app, "set_targetFrameRate", target_hook as *const (), &TARGET_TRAMP, &TARGET_MI, &TARGET_DETOUR) {
            Ok(()) => notes.push_str("managed=ok"),
            Err(e) => notes.push_str(&format!("managed={e}")),
        }
    }
    // Native icall guard: FrameRateController calls set_targetFrameRate directly via
    // il2cpp_resolve_icall, skipping the managed thunk — without this, event/menu frame-rate
    // overrides bypass the clamp and the fps drops to 30 (2026-07-01 regression). Resolve-by-name
    // → survives future updates (RVAs move, the icall signature doesn't).
    unsafe {
        let icall = il2cpp::resolve_icall("UnityEngine.Application::set_targetFrameRate(System.Int32)");
        if icall.is_null() {
            notes.push_str(" icall=miss");
        } else if il2cpp::is_detoured(icall) {
            notes.push_str(" icall=already-detoured");
        } else {
            match RawDetour::new(icall as *const (), target_icall_hook as *const ()) {
                Ok(d) if d.enable().is_ok() => {
                    TARGET_ICALL_TRAMP.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = TARGET_ICALL_DETOUR.set(d);
                    notes.push_str(" icall=ok");
                }
                Ok(_) => notes.push_str(" icall=enable-fail"),
                Err(e) => notes.push_str(&format!(" icall={e}")),
            }
        }
    }
    // vSync is optional but important — without disabling it the target is ignored. The managed
    // QualitySettings.set_vSyncCount was stripped from this build (present only if a future update
    // restores it), so try it best-effort and DON'T treat a miss as failure — the icall below is the
    // real guard now.
    let q = il2cpp::class("UnityEngine.QualitySettings");
    if !q.is_null() && !il2cpp::method(q, "set_vSyncCount", 1).is_null() {
        unsafe {
            match hook(q, "set_vSyncCount", vsync_hook as *const (), &VSYNC_TRAMP, &VSYNC_MI, &VSYNC_DETOUR) {
                Ok(()) => notes.push_str(" vsync-managed=ok"),
                Err(e) => notes.push_str(&format!(" vsync-managed={e}")),
            }
        }
    } else {
        notes.push_str(" vsync-managed=stripped");
    }
    // Native icall guard (the vSync equivalent of the targetFrameRate icall fix): resolve by name so
    // it survives updates. This is what actually forces vSync off now.
    unsafe {
        let icall = il2cpp::resolve_icall("UnityEngine.QualitySettings::set_vSyncCount(System.Int32)");
        if icall.is_null() {
            notes.push_str(" vsync-icall=miss");
        } else if il2cpp::is_detoured(icall) {
            notes.push_str(" vsync-icall=already-detoured");
        } else {
            match RawDetour::new(icall as *const (), vsync_icall_hook as *const ()) {
                Ok(d) if d.enable().is_ok() => {
                    VSYNC_ICALL_TRAMP.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = VSYNC_ICALL_DETOUR.set(d);
                    notes.push_str(" vsync-icall=ok");
                }
                Ok(_) => notes.push_str(" vsync-icall=enable-fail"),
                Err(e) => notes.push_str(&format!(" vsync-icall={e}")),
            }
        }
    }
    notes
}
