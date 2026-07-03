//! Heaven — the single `Gallop.HttpHelper::DecompressResponse` hook.
//!
//! One detour reads every decrypted + lz4-decompressed msgpack API response and fans it out:
//!   - to the companion-overlay bridge (`uma_bridge`), for ALL responses;
//!   - the player-horse identity (the one with `viewer_id != 0`) → `race::set_net_player`
//!     (+ freecam auto-follow), so the race-result Top-1 skip knows if you WON;
//!   - remaining race retries (`available_continue_num`) → `race::set_continues_available`;
//!   - (full build only) extra career payloads handled by additional consumers.
//!
//! Read-only: it calls the original, reads the decompressed result, and returns it UNCHANGED. If a
//! co-resident mod already detoured DecompressResponse (e.g. a spark collector) we CHAIN on top —
//! both hooks are read-only, so the response passes through both. This replaces the former duplicate
//! response hooks that were previously split across separate modules.

#![allow(dead_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;
use rmpv::Value;

use crate::htt_il2cpp as h;
use crate::msgpack::{as_arr, contains, find_key, map_get};

fn log(msg: &str) {
    crate::tools::log(msg);
}

static INSTALLED: AtomicBool = AtomicBool::new(false);
static ORIG: AtomicUsize = AtomicUsize::new(0);
static DETOUR: OnceLock<RawDetour> = OnceLock::new();

type DecompStaticFn = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;
type DecompInstFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> *mut c_void;

unsafe fn on_response(ret: *mut c_void) {
    if ret.is_null() {
        return;
    }
    let len = h::array_len(ret as *mut h::RawObject);
    if len == 0 || len > 50 * 1024 * 1024 {
        return;
    }
    let data = (ret as *mut u8).add(0x20);
    let slice = std::slice::from_raw_parts(data, len);
    // Feed the plain response to the companion-overlay bridge (all responses, before our filter).
    crate::uma_bridge::send_response(slice);

    let has_race = contains(slice, b"race_horse_data");
    let has_cont = contains(slice, b"available_continue_num");
    // These payloads only matter to a full-build consumer, and only while it's on
    // (the `is_enabled()` short-circuit avoids scanning every response when it's off).
        && (contains(slice, b"choice_array") || contains(slice, b"choice_reward_array"));
    #[cfg(not(feature = "oracle"))]
    let has_event = false;

    if !has_race && !has_cont && !has_event {
        return;
    }
    let bytes = slice.to_vec();
    if has_race {
        parse_race(&bytes);
    }
    if has_cont {
        parse_continues(&bytes);
    }
}

/// Find the player's horse in `race_horse_data` (the one with `viewer_id != 0`; NPCs are all 0)
/// and publish its array index + `frame_order` for the race module.
fn parse_race(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let val = match rmpv::decode::read_value(&mut cur) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut arrs: Vec<&Value> = Vec::new();
    find_key(&val, "race_horse_data", &mut arrs);
    for a in arrs {
        if let Some(list) = as_arr(a) {
            for (i, hh) in list.iter().enumerate() {
                let vid = map_get(hh, "viewer_id").and_then(|x| x.as_i64()).unwrap_or(0);
                if vid != 0 {
                    let fo = map_get(hh, "frame_order").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                    log(&format!(
                        "[response] race player: arrIdx={i} frame_order={fo} viewer={vid} horses={}",
                        list.len()
                    ));
                    crate::race::set_net_player(i as i32, fo, list.len() as i32);
                    // Auto-frame the player's Uma at race start (freecam build only).
                    #[cfg(feature = "freecam")]
                    crate::freecam::auto_follow_player(fo);
                    return;
                }
            }
        }
    }
}

/// Read `available_continue_num` (remaining race retries) and publish it so the race-result skip
/// can auto-advance once no retries remain.
fn parse_continues(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let val = match rmpv::decode::read_value(&mut cur) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut hits: Vec<&Value> = Vec::new();
    find_key(&val, "available_continue_num", &mut hits);
    if let Some(n) = hits.first().and_then(|v| v.as_i64()) {
        crate::race::set_continues_available(n as i32);
    }
}

unsafe extern "C" fn hook_static(arg0: *mut c_void, m: *const c_void) -> *mut c_void {
    let ret = {
        let t = ORIG.load(Ordering::Relaxed);
        if t != 0 {
            let f: DecompStaticFn = std::mem::transmute(t);
            f(arg0, m)
        } else {
            std::ptr::null_mut()
        }
    };
    on_response(ret);
    ret
}

unsafe extern "C" fn hook_inst(this: *mut c_void, arg0: *mut c_void, m: *const c_void) -> *mut c_void {
    let ret = {
        let t = ORIG.load(Ordering::Relaxed);
        if t != 0 {
            let f: DecompInstFn = std::mem::transmute(t);
            f(this, arg0, m)
        } else {
            std::ptr::null_mut()
        }
    };
    on_response(ret);
    ret
}

/// Install the DecompressResponse hook. Idempotent. Called once at boot.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if !h::init() {
            log("[response] il2cpp init failed");
            return;
        }
        let image = h::find_game_image();
        if image.is_null() {
            log("[response] game image not found");
            return;
        }
        let ns = std::ffi::CString::new("Gallop").unwrap();
        let cn = std::ffi::CString::new("HttpHelper").unwrap();
        let klass = match h::CLASS_FROM_NAME {
            Some(f) => f(image, ns.as_ptr(), cn.as_ptr()),
            None => std::ptr::null_mut(),
        };
        if klass.is_null() {
            log("[response] Gallop.HttpHelper not found");
            return;
        }
        let mname = std::ffi::CString::new("DecompressResponse").unwrap();
        let method = match h::CLASS_GET_METHOD_FROM_NAME {
            Some(f) => f(klass, mname.as_ptr(), 1),
            None => std::ptr::null_mut(),
        };
        if method.is_null() {
            log("[response] DecompressResponse(1) not found");
            return;
        }
        let is_static = match h::METHOD_GET_FLAGS {
            Some(f) => (f(method, std::ptr::null_mut()) & h::METHOD_ATTRIBUTE_STATIC) != 0,
            None => true,
        };
        let fnptr = h::method_addr(method);
        if fnptr == 0 {
            log("[response] method pointer null");
            return;
        }
        // If another mod (e.g. a spark collector) detoured DecompressResponse first, CHAIN on top
        // instead of yielding. Both hooks are read-only — each calls the original, reads the
        // decompressed result, and returns it UNCHANGED — so they coexist: the response passes
        // through both. retour relocates the existing jmp prologue into our trampoline.
        let chained = crate::il2cpp::is_detoured(fnptr as *const c_void);
        let det = if is_static { hook_static as *const () } else { hook_inst as *const () };
        match RawDetour::new(fnptr as *const (), det) {
            Ok(d) => {
                if d.enable().is_err() {
                    log("[response] detour enable failed");
                    return;
                }
                ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = DETOUR.set(d);
                if chained {
                    log("[response] already detoured (another mod) — chaining on top");
                }
                log(&format!("[response] hooked Gallop.HttpHelper::DecompressResponse (static={is_static})"));
            }
            Err(e) => log(&format!("[response] detour failed: {e}")),
        }
    }
}
