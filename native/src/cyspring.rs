//! Heaven — uncap the character cloth / hair (spring-bone) physics update rate.
//!
//! `Gallop.CySpringController` drives the spring-bone simulation for a character's hair and
//! clothes. Its `UpdateMode` field defaults to a frame-skipping / 60 fps-capped mode so the
//! physics don't cost much on weak hardware — which makes hair look choppy at high frame
//! rates. We hook the controller's `Init()`, let the game's own init run, then (when enabled)
//! force `UpdateMode` to `Normal` so the simulation steps on every rendered frame and stays
//! smooth at whatever FPS the game is running.
//!
//! `Init()` only fires when a controller spawns, so the toggle applies to characters loaded
//! from that point on (the next scene / character (re)load picks it up). Purely cosmetic — no
//! gameplay effect — so it ships in every build.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;

// `CySpringController.UpdateMode` is an i32 enum:
//   0 = Normal (every frame), 1 = 60 fps, 2 = SkipFrame, 3 = SkipFramePostAlways.
const MODE_NORMAL: i32 = 0;
const MODE_SKIP_MOST: i32 = 3; // SkipFramePostAlways — cheapest, for the low-spec mode.

static ENABLED: AtomicBool = AtomicBool::new(false);
static LOW_SPEC: AtomicBool = AtomicBool::new(false);
static UPDATEMODE_OFF: AtomicUsize = AtomicUsize::new(usize::MAX); // field byte offset on the object
static TR_INIT: AtomicUsize = AtomicUsize::new(0);
static D_INIT: OnceLock<RawDetour> = OnceLock::new();

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

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Toggle the uncap. New controllers pick the chosen mode up in their `Init()`; there's no
/// global registry of live controllers to walk, so already-spawned characters keep their
/// current mode until the next (re)load.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn set_low_spec(on: bool) {
    LOW_SPEC.store(on, Ordering::Relaxed);
}

/// Hook: run the game's own `Init`, then overwrite `UpdateMode` with Normal when enabled.
unsafe extern "C" fn on_init(this: *mut c_void, method: *mut c_void) {
    crate::crashlog::crumb(31);
    let t = TR_INIT.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(t);
        orig(this, method);
    }
    if this.is_null() {
        return;
    }
    let off = UPDATEMODE_OFF.load(Ordering::Relaxed);
    if off == usize::MAX {
        return;
    }
    // UpdateMode is a plain i32 enum field at `off` on the controller instance.
    // Low-spec wins: skip most physics frames. Otherwise the uncap toggle sets Normal.
    if LOW_SPEC.load(Ordering::Relaxed) {
        *((this as *mut u8).add(off) as *mut i32) = MODE_SKIP_MOST;
    } else if ENABLED.load(Ordering::Relaxed) {
        *((this as *mut u8).add(off) as *mut i32) = MODE_NORMAL;
    }
}

/// Resolve the class + field and install the `Init` detour. Call once the runtime is ready.
pub fn install() -> Result<(), String> {
    let k = il2cpp::class("Gallop.CySpringController");
    if k.is_null() {
        return Err("CySpringController not found".into());
    }
    match il2cpp::field_offset(k, "<UpdateMode>k__BackingField") {
        Some(off) => UPDATEMODE_OFF.store(off, Ordering::Relaxed),
        None => return Err("UpdateMode field not found".into()),
    }
    unsafe {
        il2cpp::hook_method(k, "Init", 0, on_init as *const (), &TR_INIT, &D_INIT)?;
    }
    log("[cyspring] Init hooked (cloth physics uncap ready)");
    Ok(())
}
