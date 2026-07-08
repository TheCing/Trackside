//! Heaven — UI tempo.
//!
//! The game animates almost its entire interface (menu opens, panel slides, screen
//! transitions, button feedback) through **DOTween**. We speed those up.
//!
//! We hook DOTween's per-frame driver `DG.Tweening.Core.TweenManager.Update(type,
//! deltaTime, independentTime)` and scale the time deltas it receives. This intercepts
//! the CONSUMER of the clock, so it's immune to anything that resets DOTween's global
//! `timeScale` mid-session — e.g. certain event popups (the "mood" event) reset it to 1,
//! which defeated the earlier approach of writing the global field directly.
//!
//!   tempo 1.0 = stock · >1 = snappier UI (10x ≈ near-instant).

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;

// Requested tempo, stored as f32 bits. 1.0 = stock.
static TEMPO: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0f32
static TRAMP: AtomicUsize = AtomicUsize::new(0);
static DETOUR: OnceLock<RawDetour> = OnceLock::new();

pub fn tempo() -> f32 {
    f32::from_bits(TEMPO.load(Ordering::Relaxed))
}

/// Set the UI tempo (clamped to a sane 1..=10x).
pub fn set_tempo(t: f32) {
    TEMPO.store(t.clamp(1.0, 10.0).to_bits(), Ordering::Relaxed);
}

/// Apply persisted settings to the UI-tempo module at boot.
pub fn apply(s: &crate::settings::Settings) {
    set_tempo(s.ui_tempo);
}

/// Kept for the overlay's per-frame call site — the hook now does the work, so this is a
/// no-op (no global field to re-assert).
pub fn enforce() {}

// TweenManager.Update is a STATIC method → (updateType, deltaTime, independentTime, MethodInfo*).
type UpdateFn = unsafe extern "C" fn(i32, f32, f32, *mut c_void);

unsafe extern "C" fn update_hook(update_type: i32, mut dt: f32, mut idt: f32, mi: *mut c_void) {
    let t = TRAMP.load(Ordering::Relaxed);
    if t == 0 {
        return;
    }
    // This is the SINGLE detour on TweenManager.Update (a static, main-thread per-frame tick) and the
    // one main-thread driver for ALL Heaven pumps. hunter previously stacked a SECOND RawDetour on this
    // exact method for the same pumps — two detours on one 5-byte prologue corrupt each other's
    // trampolines and produced the intermittent AV/C++-exception crash stamped "after-tween". Now
    // consolidated here: one detour, every pump. All pumps are idempotent/guarded and safe on the main
    // thread (the only place RequestBase.Send / SoftwareReset may run).
    let pump_t0 = std::time::Instant::now();
    crate::crashlog::step("tween:hunter-pump");
    crate::hunter::frame_pump(); // opponent-hunter jittered auto-roll
    crate::crashlog::step("tween:padder-pump");
    crate::padder::pump(); // Team Trials team-edit apply (main thread = safe for RequestBase.Send)
    crate::crashlog::step("tween:reset-pump");
    crate::reset::poll(); // soft-reset main-thread execution point (guarded, no-op if idle)
    crate::crashlog::step("tween:affinity-pump");
    crate::affinity::poll(); // affinity-badge "is a dialog open" gate sample
    crate::crashlog::step("tween:pruner-pump");
    crate::pruner::pump(); // follower-pruner reads/removals (main thread; guarded, no-op if idle)
    crate::crashlog::step("tween:roomfinder-pump");
    crate::roomfinder::pump(); // room-match finder read/refresh/auto-join cycle (guarded, no-op if idle)
    crate::crashlog::step("tween:skillbuyer-pump");
    crate::skill_buyer::pump(); // Apply Optimal scan/selection driver (guarded, no-op if idle)
    #[cfg(feature = "banner")]
    {
        crate::crashlog::step("tween:icondump-pump");
        crate::icon_dump::pump(); // texture-inventory harvest (guarded, no-op if idle)
    }
    // Diagnostic: how long Heaven's own main-thread pumps take this frame (proves we're not the stall).
    crate::loadprof::pump(pump_t0.elapsed().as_secs_f64() * 1000.0);
    crate::crashlog::step("tween:scale-orig");
    let s = tempo();
    if s != 1.0 {
        // Scale BOTH the frame delta AND the time-independent delta so the WHOLE UI speeds up
        // uniformly. Many menu/transition/loader tweens advance on the timeScale-INDEPENDENT clock;
        // leaving that at real time made high speed feel only "half fast". Scaling both makes the
        // multiplier match across every animation.
        dt *= s;
        idt *= s;
    }
    let f: UpdateFn = std::mem::transmute(t);
    f(update_type, dt, idt, mi);
    crate::crashlog::step("idle:after-tween");
}

pub fn install() -> Result<&'static str, String> {
    let k = il2cpp::class("DG.Tweening.Core.TweenManager");
    if k.is_null() {
        return Err("TweenManager not found".into());
    }
    let m = il2cpp::method(k, "Update", 3);
    if m.is_null() {
        return Err("Update: not found".into());
    }
    let target = il2cpp::method_pointer(m);
    if target.is_null() {
        return Err("Update: null ptr".into());
    }
    unsafe {
        // Heaven OWNS the UI tempo. If a co-resident mod (e.g. a localization loader) detoured
        // Update FIRST, we CHAIN on top instead of yielding: our detour scales dt/idt, then its
        // trampoline jumps to whatever hooked Update before us, which finally calls the real
        // Update. So the tempo works regardless of load order — no external config needed for
        // Heaven to win the speed. retour relocates the existing jmp prologue into our trampoline.
        let chained = il2cpp::is_detoured(target);
        let d = RawDetour::new(target as *const (), update_hook as *const ())
            .map_err(|e| format!("Update: {e}"))?;
        d.enable().map_err(|e| format!("Update enable: {e}"))?;
        TRAMP.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
        let _ = DETOUR.set(d);
        Ok(if chained {
            "OK (chained on top — Trackside owns speed)"
        } else {
            "OK"
        })
    }
}
