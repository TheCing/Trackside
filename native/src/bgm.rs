//! Heaven — silence the game's original title BGM while the custom intro plays.
//!
//! `Gallop.AudioManager` exposes `SetBgmVolume(float, float)` and `GetBgmVolume()`. We grab
//! the singleton (get_Instance), save the current BGM volume, set it to 0 on entering the
//! title scene, and restore it on leaving. Pure native IL2CPP calls via the compiled method
//! pointers (floats ride XMM per the Win64 ABI, which `extern "C"` handles).

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};


use crate::il2cpp;

static GET_INST: AtomicUsize = AtomicUsize::new(0);
static SET_VOL: AtomicUsize = AtomicUsize::new(0);
static GET_VOL: AtomicUsize = AtomicUsize::new(0);
static SET_BUS: AtomicUsize = AtomicUsize::new(0); // SetBusVolume(String, float)
static GET_CUR: AtomicUsize = AtomicUsize::new(0); // GetCurBusVolumeParam(String) -> float
static STOP_VOICE_ALL: AtomicUsize = AtomicUsize::new(0); // StopVoiceAll(float fade)
static SET_VOICE_UNAVAIL: AtomicUsize = AtomicUsize::new(0); // SetVoiceUnavailable(bool)
static SAVED: AtomicU32 = AtomicU32::new(0x3f80_0000); // 1.0 default
static BGM_SAVED: AtomicBool = AtomicBool::new(false); // true once SAVED holds a real read value
static RESOLVED: AtomicBool = AtomicBool::new(false);
static VOICE_TICK: AtomicU32 = AtomicU32::new(0); // throttles the per-frame voice re-kill

fn log(msg: &str) {
    crate::tools::log(msg);
}

/// Resolve the AudioManager methods once (call after the runtime is ready).
pub fn init() {
    let k = il2cpp::class("Gallop.AudioManager");
    if k.is_null() {
        log("[bgm] AudioManager class not found");
        return;
    }
    let gi = il2cpp::method(k, "get_Instance", 0);
    let sv = il2cpp::method(k, "SetBgmVolume", 2);
    let gv = il2cpp::method(k, "GetBgmVolume", 0);
    let sb = il2cpp::method(k, "SetBusVolume", 2);
    let gc = il2cpp::method(k, "GetCurBusVolumeParam", 1);
    let sva = il2cpp::method(k, "StopVoiceAll", 1);
    let svu = il2cpp::method(k, "SetVoiceUnavailable", 1);
    STOP_VOICE_ALL.store(sva as usize, Ordering::Relaxed);
    SET_VOICE_UNAVAIL.store(svu as usize, Ordering::Relaxed);
    GET_INST.store(gi as usize, Ordering::Relaxed);
    SET_VOL.store(sv as usize, Ordering::Relaxed);
    GET_VOL.store(gv as usize, Ordering::Relaxed);
    SET_BUS.store(sb as usize, Ordering::Relaxed);
    GET_CUR.store(gc as usize, Ordering::Relaxed);
    RESOLVED.store(!gi.is_null() && !sv.is_null(), Ordering::Relaxed);
    log(&format!(
        "[bgm] resolve: SetBgmVolume={} SetBusVolume={} StopVoiceAll={} SetVoiceUnavailable={}",
        !sv.is_null(), !sb.is_null(), !sva.is_null(), !svu.is_null()
    ));
}

// Buses muted during the intro so only our song is heard. Master/MasterOut kill ALL game audio
// (incl. the title USM movie + the "Cygames / Pretty Derby" voice); Voice/SE are belt-and-braces.
// Our intro song plays through a separate WASAPI stream (rodio), so it is unaffected.
const INTRO_MUTE_BUSES: &[&str] = &["Master", "MasterOut", "Voice", "SE"];
static SAVED_VOICE: [AtomicU32; 4] = [
    AtomicU32::new(0x3f80_0000), AtomicU32::new(0x3f80_0000),
    AtomicU32::new(0x3f80_0000), AtomicU32::new(0x3f80_0000),
];
static VOICE_SAVED: AtomicBool = AtomicBool::new(false);

/// Low-level: SetBusVolume(this, busName, vol). The 2-arg overload is instance on the singleton
/// (same calling shape as SetBgmVolume). Safe string+float call — no object deref.
fn set_bus(inst: *mut c_void, name: &str, vol: f32) {
    let sbm = SET_BUS.load(Ordering::Relaxed) as *const c_void;
    if sbm.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(sbm as il2cpp::Method);
    if p.is_null() {
        return;
    }
    let s = il2cpp::new_string(name);
    if s.is_null() {
        return;
    }
    let f: extern "C" fn(*mut c_void, *mut c_void, f32, *const c_void) =
        unsafe { std::mem::transmute(p) };
    f(inst, s, vol, sbm);
}

fn get_bus(inst: *mut c_void, name: &str) -> f32 {
    let gcm = GET_CUR.load(Ordering::Relaxed) as *const c_void;
    if gcm.is_null() {
        return 1.0;
    }
    let p = il2cpp::method_pointer(gcm as il2cpp::Method);
    if p.is_null() {
        return 1.0;
    }
    let s = il2cpp::new_string(name);
    if s.is_null() {
        return 1.0;
    }
    let f: extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> f32 =
        unsafe { std::mem::transmute(p) };
    f(inst, s, gcm)
}

/// Mute every game-audio bus (Master/Voice/SE) during the intro so ONLY our song is heard —
/// this kills the title USM movie audio and the "Cygames / Pretty Derby" voice that the BGM
/// mute alone leaves audible. Only ever called while a custom intro is present AND at the title
/// (gated by the caller), so users without an intro hear the game normally.
/// Bus volumes are SAVED once (first call this title visit); the mute itself is RE-FORCED every
/// call — the game re-asserts bus volumes and the CRI title voice cue can start/re-trigger after
/// the first frame, so muting once is not enough (re-issuing the calls each frame at the title
/// is cheap and is what actually keeps it silent).
pub fn mute_voice() {
    let inst = instance();
    if inst.is_null() {
        return;
    }
    // Save each bus's REAL prior volume + zero the buses ONCE. `set_bus`/`get_bus` allocate a
    // managed string (`il2cpp::new_string`) every call — doing that 60x/s from the render thread
    // is what tripped the GC "collect from unknown thread" crash. So the bus mute is one-shot;
    // VOICE_SAVED doubles as the "was saved" flag (cleared in restore_voice). The Master/Voice/SE
    // buses are not re-asserted by the game, so once is enough (the title BGM volume IS re-forced
    // every frame, but that's `force_mute`, which takes a float and never allocates).
    if !VOICE_SAVED.swap(true, Ordering::Relaxed) {
        for (i, name) in INTRO_MUTE_BUSES.iter().enumerate() {
            let cur = get_bus(inst, name);
            SAVED_VOICE[i].store(cur.to_bits(), Ordering::Relaxed);
        }
        for name in INTRO_MUTE_BUSES {
            set_bus(inst, name, 0.0);
        }
        log("[bgm] intro buses muted");
    }
    // Re-kill the CRI title voice cue on a throttle (~7x/s). StopVoiceAll(float)/
    // SetVoiceUnavailable(bool) use cached method pointers and do NOT allocate managed memory, but
    // we still keep render-thread IL2CPP traffic minimal to stay clear of the GC race. The cue
    // never restarts faster than this, so the voice stays silent.
    if VOICE_TICK.fetch_add(1, Ordering::Relaxed) % 8 == 0 {
        call_f32(inst, &STOP_VOICE_ALL, 0.0);
        call_bool(inst, &SET_VOICE_UNAVAIL, true);
    }
}

/// Call an instance method with a single f32 arg: f(this, value, MethodInfo*).
fn call_f32(inst: *mut c_void, slot: &AtomicUsize, v: f32) {
    let m = slot.load(Ordering::Relaxed) as *const c_void;
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m as il2cpp::Method);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(*mut c_void, f32, *const c_void) = unsafe { std::mem::transmute(p) };
    f(inst, v, m);
}

/// Call an instance method with a single bool arg: f(this, value, MethodInfo*).
fn call_bool(inst: *mut c_void, slot: &AtomicUsize, v: bool) {
    let m = slot.load(Ordering::Relaxed) as *const c_void;
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m as il2cpp::Method);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(*mut c_void, bool, *const c_void) = unsafe { std::mem::transmute(p) };
    f(inst, v, m);
}

/// Restore every intro-muted bus to its pre-mute volume when leaving the title. A bus the user
/// had legitimately muted (read 0 at mute time) was stored as 0, so it stays muted — we never
/// force-unmute a bus the user chose to silence.
pub fn restore_voice() {
    // Only restore if we actually muted this visit (clears the flag for the next title entry).
    if !VOICE_SAVED.swap(false, Ordering::Relaxed) {
        return;
    }
    let inst = instance();
    if inst.is_null() {
        return;
    }
    for (i, name) in INTRO_MUTE_BUSES.iter().enumerate() {
        let v = f32::from_bits(SAVED_VOICE[i].load(Ordering::Relaxed));
        set_bus(inst, name, v);
    }
    call_bool(inst, &SET_VOICE_UNAVAIL, false); // re-enable voice for the rest of the game
    log("[bgm] intro buses restored");
}

fn instance() -> *mut c_void {
    let m = GET_INST.load(Ordering::Relaxed) as *const c_void;
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m as il2cpp::Method);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    // static method: only the trailing MethodInfo* arg, returns the singleton Object.
    let f: extern "C" fn(*const c_void) -> *mut c_void = unsafe { std::mem::transmute(p) };
    f(m)
}

pub fn mute() {
    if !RESOLVED.load(Ordering::Relaxed) {
        return;
    }
    let inst = instance();
    if inst.is_null() {
        return;
    }
    // Save the REAL current BGM volume (even if it's already 0 — the user may have muted BGM
    // themselves; force-unmuting them on restore would be wrong). BGM_SAVED records that we have
    // a genuine read so restore() knows it's safe to apply.
    let gvm = GET_VOL.load(Ordering::Relaxed) as *const c_void;
    if !gvm.is_null() {
        let gp = il2cpp::method_pointer(gvm as il2cpp::Method);
        if !gp.is_null() {
            let gf: extern "C" fn(*mut c_void, *const c_void) -> f32 = unsafe { std::mem::transmute(gp) };
            let cur = gf(inst, gvm);
            SAVED.store(cur.to_bits(), Ordering::Relaxed);
            BGM_SAVED.store(true, Ordering::Relaxed);
        }
    }
    set_volume(inst, 0.0);
    log("[bgm] muted");
}

/// Force the BGM volume to 0 with no logging and no save — called every frame while at the
/// title so the game's PlayBgm volume reset can't bring the original track back.
pub fn force_mute() {
    if !RESOLVED.load(Ordering::Relaxed) {
        return;
    }
    let inst = instance();
    if !inst.is_null() {
        set_volume(inst, 0.0);
    }
}

pub fn restore() {
    if !RESOLVED.load(Ordering::Relaxed) {
        return;
    }
    // Nothing genuine was saved (mute() never read a real value) → don't apply the 1.0 default,
    // which could force-unmute a BGM the user had silenced.
    if !BGM_SAVED.swap(false, Ordering::Relaxed) {
        return;
    }
    let inst = instance();
    if inst.is_null() {
        return;
    }
    let v = f32::from_bits(SAVED.load(Ordering::Relaxed));
    set_volume(inst, v);
    log(&format!("[bgm] restored to {v}"));
}

fn set_volume(inst: *mut c_void, vol: f32) {
    let svm = SET_VOL.load(Ordering::Relaxed) as *const c_void;
    if svm.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(svm as il2cpp::Method);
    if p.is_null() {
        return;
    }
    // SetBgmVolume(this, float volume, float fade, MethodInfo*).
    let f: extern "C" fn(*mut c_void, f32, f32, *const c_void) = unsafe { std::mem::transmute(p) };
    f(inst, vol, 0.0, svm);
}
