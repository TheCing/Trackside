//! umas — export the player's trained "veteran" umas to JSON, local-only, under
//! `heaven_umas/` (next to the game). This is the data the Hakuraku "veterans" page
//! consumes. Mirrors the horseACT plugin (ayaliz/horseACT), included natively with
//! the author's permission so Heaven covers it itself — and users don't need a
//! second mod that hooks the same race/trained-chara subsystem (which conflicts).
//!
//! Mechanism (same as horseACT): scan the game assembly for ANY method that takes a
//! `TrainedChara[]` parameter (the veteran/legacy list load/apply), hook it, and when
//! it fires dump that array via the generic IL2CPP→JSON walker in `race_export`
//! (the same reflection dump horseACT uses → Hakuraku-compatible output).

#![allow(static_mut_refs)]

use core::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::htt_il2cpp as h;
use crate::htt_il2cpp::{RawMethod, RawObject};

static ENABLED: AtomicBool = AtomicBool::new(false);
static ORIG: AtomicUsize = AtomicUsize::new(0);
static DETOUR: OnceLock<RawDetour> = OnceLock::new();
// Human-readable "Class.method (params=N)" of the method we hooked — logged when it
// fires so we can confirm which method it actually is and its real arity.
static METHOD_DESC: OnceLock<String> = OnceLock::new();

/// Mirror the persisted toggle into the fast path. Called by settings.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

fn ulog(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    CStr::from_ptr(p).to_string_lossy().into_owned()
}

// Instance method: (this, TrainedChara[] array, hidden MethodInfo*).
type VeteranFn = unsafe extern "C" fn(*mut RawObject, *mut RawObject, *const c_void);

unsafe extern "C" fn veteran_hook(this: *mut RawObject, arr: *mut RawObject, method: *const c_void) {
    let orig = ORIG.load(Ordering::Relaxed);
    if orig != 0 {
        let f: VeteranFn = std::mem::transmute(orig);
        f(this, arr, method);
    }
    if !ENABLED.load(Ordering::Relaxed) || arr.is_null() {
        return;
    }
    // WorkTrainedCharaData.UpdateAll fires once when the roster updates → dumping the whole
    // array here is safe (unlike the per-race ChampionsRaceInfo setter we used to hit).
    save(arr as usize);
}

fn save(arr_addr: usize) {
    // Reuse the generic IL2CPP→JSON walker (race_export) — same format horseACT emits.
    let json = crate::race_export::dump_object_json(arr_addr);
    if json.is_empty() || json == "null" || json == "<err>" {
        ulog("[umas] veterans dump empty/failed");
        return;
    }
    let dir = crate::paths::dll_dir().join("heaven_umas");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("veterans.json");
    match std::fs::write(&path, json.as_bytes()) {
        Ok(_) => ulog(&format!("[umas] veterans.json saved ({} bytes)", json.len())),
        Err(e) => ulog(&format!("[umas] write failed: {e}")),
    }
}

/// Find a method taking a `TrainedChara[]` parameter and detour it.
/// Must run on an IL2CPP-attached thread (boot thread, before detach).
pub fn install() -> String {
    unsafe {
        if !h::init() {
            return "il2cpp init failed".into();
        }
        let image = find_game_image();
        if image.is_null() {
            return "game image not found".into();
        }
        let (cnt, get_class, get_methods, param_count, get_param, from_type, class_name) = match (
            h::IMAGE_GET_CLASS_COUNT,
            h::IMAGE_GET_CLASS,
            h::CLASS_GET_METHODS,
            h::METHOD_GET_PARAM_COUNT,
            h::METHOD_GET_PARAM,
            h::CLASS_FROM_TYPE,
            h::CLASS_GET_NAME,
        ) {
            (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f), Some(g)) => (a, b, c, d, e, f, g),
            _ => return "il2cpp enum fns unavailable".into(),
        };

        let n = cnt(image);
        // Pick the veteran-roster method exactly like horseACT: prefer the method whose
        // class is "WorkTrainedCharaData" and whose name contains "UpdateAll" (that's the
        // full trained-uma roster apply — fires once when the roster updates). Fall back to
        // the first method taking a TrainedChara[] only if that exact one isn't found.
        // (The naive first-match was ChampionsRaceInfo.set_TrainedCharaArray — a per-race
        // setter that fires many times; dumping on each froze the game.)
        let mut target: *mut RawMethod = std::ptr::null_mut();
        let mut fallback: *mut RawMethod = std::ptr::null_mut();
        let mut fallback_desc = String::new();
        'outer: for ci in 0..n {
            let klass = get_class(image, ci);
            if klass.is_null() {
                continue;
            }
            let cname = h::class_name(klass);
            let mut iter: *mut c_void = std::ptr::null_mut();
            loop {
                let m = get_methods(klass, &mut iter);
                if m.is_null() {
                    break;
                }
                let pc = param_count(m);
                let mut takes_tc = false;
                for pi in 0..pc {
                    let ty = get_param(m, pi);
                    if ty.is_null() {
                        continue;
                    }
                    let pclass = from_type(ty);
                    if pclass.is_null() {
                        continue;
                    }
                    let pname = cstr(class_name(pclass));
                    if pname.contains("TrainedChara") && pname.contains("[]") {
                        takes_tc = true;
                        break;
                    }
                }
                if !takes_tc {
                    continue;
                }
                let mname = h::METHOD_GET_NAME.map(|f| cstr(f(m))).unwrap_or_default();
                if cname.contains("WorkTrainedCharaData") && mname.contains("UpdateAll") {
                    target = m;
                    let _ = METHOD_DESC.set(format!("{cname}.{mname} (params={pc})"));
                    break 'outer;
                }
                if fallback.is_null() {
                    fallback = m;
                    fallback_desc = format!("{cname}.{mname} (params={pc})");
                }
            }
        }
        let found = if target.is_null() {
            if !fallback.is_null() {
                let _ = METHOD_DESC.set(format!("(fallback) {fallback_desc}"));
            }
            fallback
        } else {
            target
        };

        if found.is_null() {
            return "no TrainedChara[] method found".into();
        }
        let fnptr = h::method_addr(found);
        if fnptr == 0 {
            return "method pointer null".into();
        }
        if crate::il2cpp::is_detoured(fnptr as *const c_void) {
            return "already detoured (skipped)".into();
        }
        match RawDetour::new(fnptr as *const (), veteran_hook as *const ()) {
            Ok(d) => {
                if d.enable().is_err() {
                    return "detour enable failed".into();
                }
                ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = DETOUR.set(d);
                "hooked TrainedChara[] method (veterans export)".into()
            }
            Err(e) => format!("detour failed: {e}"),
        }
    }
}

use crate::htt::find_game_image;
