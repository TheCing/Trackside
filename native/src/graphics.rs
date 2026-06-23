//! Heaven — 3D model quality unlock + texture/shadow enhancement (cosmetic).
//!
//! `Gallop.GraphicSettings.ApplyGraphicsQuality(quality, force)` selects the character render
//! quality tier (and re-applies the relevant `UnityEngine.QualitySettings`). We hook it and:
//!
//!   • Quality unlock — force the full-quality toon tier with `force = true`, so models always
//!     render at full quality regardless of the device / menu cap.
//!
//!   • Texture & shadow enhancement — right after the game applies its tier, re-assert higher
//!     `QualitySettings` (anisotropic filtering ForceEnable, a larger LOD bias so models stay
//!     detailed, max shadow resolution). Re-asserting inside the same hook means the game's own
//!     quality pass can't stomp it.
//!
//! Both apply when the game next re-applies its graphics quality (scene / character load). No
//! gameplay effect, so it ships in every build.
//!
//! (A render super-sampling option was tried via the `GetVirtualResolution*3D` getters but
//! resizing the internal 3D resolution leaves the character mis-framed inside its display quad,
//! so it's intentionally left out until the render texture + display rect are rebuilt together.)

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;

/// `GraphicsQuality` enum value we force. `ToonFull` (3) is the full-quality toon tier.
const QUALITY_FULL: i32 = 3;
// QualitySettings values for the enhancement pass.
const ANISO_FORCE_ENABLE: i32 = 2; // AnisotropicFiltering.ForceEnable
const SHADOW_RES_VERY_HIGH: i32 = 3; // ShadowResolution.VeryHigh
const LOD_BIAS: f32 = 2.0; // >1 keeps higher-detail LODs in use

static QUALITY_ON: AtomicBool = AtomicBool::new(false);
static EXTRAS_ON: AtomicBool = AtomicBool::new(false);
static LOW_SPEC: AtomicBool = AtomicBool::new(false);

static TR_AGQ: AtomicUsize = AtomicUsize::new(0);
static D_AGQ: OnceLock<RawDetour> = OnceLock::new();

// Resolved `UnityEngine.QualitySettings` static setters (Method handles).
static QS_ANISO: AtomicUsize = AtomicUsize::new(0); // set_anisotropicFiltering(int)
static QS_LOD: AtomicUsize = AtomicUsize::new(0); // set_lodBias(float)
static QS_SHADOWRES: AtomicUsize = AtomicUsize::new(0); // set_shadowResolution(int)
static QS_ANTIALIAS: AtomicUsize = AtomicUsize::new(0); // set_antiAliasing(int)
static QS_SHADOWS: AtomicUsize = AtomicUsize::new(0); // set_shadows(int) ShadowQuality
static QS_TEXLIMIT: AtomicUsize = AtomicUsize::new(0); // set_masterTextureLimit(int)
static QS_PIXELLIGHTS: AtomicUsize = AtomicUsize::new(0); // set_pixelLightCount(int)

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

pub fn quality_unlocked() -> bool {
    QUALITY_ON.load(Ordering::Relaxed)
}
pub fn set_quality_unlocked(on: bool) {
    QUALITY_ON.store(on, Ordering::Relaxed);
}

pub fn extras_enabled() -> bool {
    EXTRAS_ON.load(Ordering::Relaxed)
}
pub fn set_extras_enabled(on: bool) {
    EXTRAS_ON.store(on, Ordering::Relaxed);
}

pub fn low_spec() -> bool {
    LOW_SPEC.load(Ordering::Relaxed)
}
pub fn set_low_spec(on: bool) {
    LOW_SPEC.store(on, Ordering::Relaxed);
}

/// Drive every `QualitySettings` knob to its cheapest value (potato mode).
unsafe fn apply_low() {
    set_i32(&QS_ANISO, 0); // AnisotropicFiltering.Disable
    set_f32(&QS_LOD, 0.4); // drop to low-detail LODs aggressively
    set_i32(&QS_SHADOWRES, 0); // ShadowResolution.Low
    set_i32(&QS_ANTIALIAS, 0); // no MSAA
    set_i32(&QS_SHADOWS, 0); // ShadowQuality.Disable
    set_i32(&QS_TEXLIMIT, 1); // half-res textures
    set_i32(&QS_PIXELLIGHTS, 0); // no per-pixel lights
}

/// Call a resolved static `QualitySettings` setter that takes a single i32.
unsafe fn set_i32(slot: &AtomicUsize, value: i32) {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(i32, *const c_void) = std::mem::transmute(p);
    f(value, m as *const c_void);
}

/// Call a resolved static `QualitySettings` setter that takes a single f32.
unsafe fn set_f32(slot: &AtomicUsize, value: f32) {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(f32, *const c_void) = std::mem::transmute(p);
    f(value, m as *const c_void);
}

/// Re-assert the enhanced QualitySettings (called right after the game applies its tier).
unsafe fn apply_extras() {
    set_i32(&QS_ANISO, ANISO_FORCE_ENABLE);
    set_f32(&QS_LOD, LOD_BIAS);
    set_i32(&QS_SHADOWRES, SHADOW_RES_VERY_HIGH);
}

// ApplyGraphicsQuality(this, quality: i32, force: bool, MethodInfo*)
unsafe extern "C" fn on_apply_quality(this: *mut c_void, quality: i32, force: bool, method: *mut c_void) {
    crate::crashlog::crumb(21);
    let t = TR_AGQ.load(Ordering::Relaxed);
    if t == 0 {
        return;
    }
    let orig: unsafe extern "C" fn(*mut c_void, i32, bool, *mut c_void) = std::mem::transmute(t);
    if LOW_SPEC.load(Ordering::Relaxed) {
        orig(this, 0, true, method); // force the lowest toon tier (Toon1280)
        apply_low();
    } else if QUALITY_ON.load(Ordering::Relaxed) {
        orig(this, QUALITY_FULL, true, method); // force the full-quality tier
        if EXTRAS_ON.load(Ordering::Relaxed) {
            apply_extras();
        }
    } else {
        orig(this, quality, force, method);
        if EXTRAS_ON.load(Ordering::Relaxed) {
            apply_extras();
        }
    }
}

/// Resolve `GraphicSettings` + `QualitySettings` and install the quality hook.
pub fn install() -> Result<(), String> {
    let k = il2cpp::class("Gallop.GraphicSettings");
    if k.is_null() {
        return Err("GraphicSettings not found".into());
    }
    unsafe {
        il2cpp::hook_method(k, "ApplyGraphicsQuality", 2, on_apply_quality as *const (), &TR_AGQ, &D_AGQ)?;
    }
    // Resolve the QualitySettings static setters for the enhancement pass (best-effort).
    let qs = il2cpp::class("UnityEngine.QualitySettings");
    if !qs.is_null() {
        QS_ANISO.store(il2cpp::method(qs, "set_anisotropicFiltering", 1) as usize, Ordering::Relaxed);
        QS_LOD.store(il2cpp::method(qs, "set_lodBias", 1) as usize, Ordering::Relaxed);
        QS_SHADOWRES.store(il2cpp::method(qs, "set_shadowResolution", 1) as usize, Ordering::Relaxed);
        QS_ANTIALIAS.store(il2cpp::method(qs, "set_antiAliasing", 1) as usize, Ordering::Relaxed);
        QS_SHADOWS.store(il2cpp::method(qs, "set_shadows", 1) as usize, Ordering::Relaxed);
        QS_TEXLIMIT.store(il2cpp::method(qs, "set_masterTextureLimit", 1) as usize, Ordering::Relaxed);
        QS_PIXELLIGHTS.store(il2cpp::method(qs, "set_pixelLightCount", 1) as usize, Ordering::Relaxed);
    }
    log("[graphics] ApplyGraphicsQuality hooked (quality unlock + texture/shadow extras ready)");
    Ok(())
}
