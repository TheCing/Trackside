//! race_net — player-horse identity from the network race response (feature `racenet`).
//!
//! Hooks `Gallop.HttpHelper::DecompressResponse`, and when a race response goes by, finds
//! the player's horse (the only one with `viewer_id != 0`) and publishes its `frame_order`
//! to `race.rs` via `set_net_player`. That lets the race-result SuperSkip gate know the
//! player's finish placement (so "Races" only auto-advances when you actually WON).
//!
//! Read-only: it parses just the msgpack player-id, stores nothing, and returns the
//! response unchanged.

#![allow(dead_code)]

use core::ffi::{c_void, CStr};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;
use rmpv::Value;

use crate::htt_il2cpp as h;

fn rlog(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

// ── msgpack helpers (rmpv) ──────────────────────────────────────────────────
fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Map(m) = v {
        m.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, val)| val)
    } else {
        None
    }
}

fn as_arr(v: &Value) -> Option<&Vec<Value>> {
    if let Value::Array(a) = v {
        Some(a)
    } else {
        None
    }
}

fn find_key<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Map(m) => {
            for (k, val) in m {
                if k.as_str() == Some(key) {
                    out.push(val);
                }
                find_key(val, key, out);
            }
        }
        Value::Array(a) => {
            for val in a {
                find_key(val, key, out);
            }
        }
        _ => {}
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Find the player's horse in race_horse_data (the one with viewer_id != 0; NPCs
/// are all 0) and publish its frame_order so the race module can resolve placement.
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
                    rlog(&format!("[race_net] player: arrIdx={i} frame_order={fo} horses={}", list.len()));
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

// ── hook on Gallop.HttpHelper::DecompressResponse ──────────────────────────
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
    let has_race = contains(slice, "race_horse_data".as_bytes());
    let has_cont = contains(slice, "available_continue_num".as_bytes());
    if !has_race && !has_cont {
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

/// Read `available_continue_num` (remaining race retries) from a career response and
/// publish it, so the race-result skip can auto-advance once no retries remain.
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

unsafe fn find_game_image() -> *mut h::RawImage {
    let dom = match h::DOMAIN_GET {
        Some(f) => f(),
        None => return std::ptr::null_mut(),
    };
    if dom.is_null() {
        return std::ptr::null_mut();
    }
    let mut count = 0usize;
    let asms = match h::DOMAIN_GET_ASSEMBLIES {
        Some(f) => f(dom, &mut count),
        None => return std::ptr::null_mut(),
    };
    if asms.is_null() {
        return std::ptr::null_mut();
    }
    for i in 0..count {
        let a = *asms.add(i);
        if a.is_null() {
            continue;
        }
        let img = match h::ASSEMBLY_GET_IMAGE {
            Some(f) => f(a),
            None => continue,
        };
        if img.is_null() {
            continue;
        }
        let np = match h::IMAGE_GET_NAME {
            Some(f) => f(img),
            None => continue,
        };
        if np.is_null() {
            continue;
        }
        let nm = CStr::from_ptr(np).to_string_lossy();
        let t = nm.trim_end_matches(".dll");
        if t.eq_ignore_ascii_case("umamusume")
            || t.eq_ignore_ascii_case("Assembly-CSharp")
        {
            return img;
        }
    }
    std::ptr::null_mut()
}

/// Install the DecompressResponse hook (player-id parse only). Idempotent.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if !h::init() {
            rlog("[race_net] il2cpp init failed");
            return;
        }
        let image = find_game_image();
        if image.is_null() {
            rlog("[race_net] game image not found");
            return;
        }
        let ns = std::ffi::CString::new("Gallop").unwrap();
        let cn = std::ffi::CString::new("HttpHelper").unwrap();
        let klass = match h::CLASS_FROM_NAME {
            Some(f) => f(image, ns.as_ptr(), cn.as_ptr()),
            None => std::ptr::null_mut(),
        };
        if klass.is_null() {
            rlog("[race_net] response class not found");
            return;
        }
        let mname = std::ffi::CString::new("DecompressResponse").unwrap();
        let method = match h::CLASS_GET_METHOD_FROM_NAME {
            Some(f) => f(klass, mname.as_ptr(), 1),
            None => std::ptr::null_mut(),
        };
        if method.is_null() {
            rlog("[race_net] response method not found");
            return;
        }
        let is_static = match h::METHOD_GET_FLAGS {
            Some(f) => (f(method, std::ptr::null_mut()) & h::METHOD_ATTRIBUTE_STATIC) != 0,
            None => true,
        };
        let fnptr = h::method_addr(method);
        if fnptr == 0 {
            rlog("[race_net] method pointer null");
            return;
        }
        // If another mod (e.g. a spark collector) detoured DecompressResponse first, CHAIN on
        // top instead of yielding. Both hooks are read-only — each calls the original, reads the
        // decompressed result, and returns it UNCHANGED — so they coexist: the response passes
        // through both. retour relocates the existing jmp prologue into our trampoline, which
        // calls down to the other mod's hook (then the real method). This lets Heaven read the
        // race placement even when a co-resident plugin owns the response hook (was: yielded,
        // which broke the won-race skip for users running such a plugin).
        let chained = crate::il2cpp::is_detoured(fnptr as *const c_void);
        let det = if is_static { hook_static as *const () } else { hook_inst as *const () };
        match RawDetour::new(fnptr as *const (), det) {
            Ok(d) => {
                if d.enable().is_err() {
                    rlog("[race_net] detour enable failed");
                    return;
                }
                ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = DETOUR.set(d);
                if chained {
                    rlog("[race_net] response already detoured (another mod) — chaining on top");
                }
                rlog(&format!("[race_net] hooked response (static={is_static})"));
            }
            Err(e) => rlog(&format!("[race_net] detour failed: {e}")),
        }
    }
}
