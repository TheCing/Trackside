//! Generic UI click/dialog engine — the reusable primitives the SuperSkip
//! (`crate::skip::result` and `crate::skip::shop`) drive game buttons/dialogs with.
//!
//! Resolved (methodPointer code, MethodInfo*) pairs are called DIRECTLY like the JS
//! NativeFunctions (no runtime_invoke), passing the trailing MethodInfo arg. The
//! result-specific gating (whitelist, win/loss, stuck-lift) lives in `crate::skip::result`;
//! this module only owns the mechanical click/close + the button-name cache.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::il2cpp;
use crate::skip::clock;
use crate::skip::result::{
    is_advance_button, is_multi, is_press_target, lift_input_block, press_allowed, MULTI_MAX,
    PRESS_GAP_MS, RR_LIFT_GRACE_MS, RR_LIFT_THROTTLE_MS, RR_NEXT_LIFT, RR_PRESSES, WINDOW_OPEN,
};

/// Append a line to the native engine log (race-result diagnostics).
fn rr_log(msg: &str) {
    crate::tools::log(msg);
}

// Single global busy flag (mirrors native_skip.js `busy`): set while WE invoke a
// button/dialog method so the Update/Push detours skip during our own calls.
pub(crate) static BUSY: AtomicBool = AtomicBool::new(false);

// Resolved (methodPointer code, MethodInfo*) pairs — called DIRECTLY like the
// JS NativeFunctions (no runtime_invoke), passing the trailing MethodInfo arg.
pub(crate) static GETNAME_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static GETNAME_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static OPC_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static OPC_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ISLOCK_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static ISLOCK_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CLOSE_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CLOSE_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CUR_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CUR_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CTOR_C: AtomicUsize = AtomicUsize::new(0);
pub(crate) static CTOR_MI: AtomicUsize = AtomicUsize::new(0);
pub(crate) static C_PED: AtomicUsize = AtomicUsize::new(0);

static NAME_CACHE: OnceLock<Mutex<HashMap<usize, String>>> = OnceLock::new();
static PRESS_STATE: OnceLock<Mutex<HashMap<usize, (u32, u64)>>> = OnceLock::new();
static DONE_DLG: OnceLock<Mutex<std::collections::HashSet<usize>>> = OnceLock::new();
static LOGGED_NAMES: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
fn name_cache() -> &'static Mutex<HashMap<usize, String>> {
    NAME_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn press_state() -> &'static Mutex<HashMap<usize, (u32, u64)>> {
    PRESS_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}
fn done_dlg() -> &'static Mutex<std::collections::HashSet<usize>> {
    DONE_DLG.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
fn logged_names() -> &'static Mutex<std::collections::HashSet<String>> {
    LOGGED_NAMES.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
/// Clear the click-engine caches (name / press-state / done-dialog / logged-names).
/// Called by crate::skip::result::clear_rr_caches on arm/disarm/race-end.
pub(crate) fn clear_caches() {
    if let Ok(mut m) = name_cache().lock() { m.clear(); }
    if let Ok(mut m) = press_state().lock() { m.clear(); }
    if let Ok(mut m) = done_dlg().lock() { m.clear(); }
    if let Ok(mut m) = logged_names().lock() { m.clear(); }
}

// Direct-call ABIs (this, …, MethodInfo*).
type RetPtr = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void; // get_name
type RetBool = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool; // IsLock
type Void2 = unsafe extern "C" fn(*mut c_void, *mut c_void); // Close
type Click = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void); // OnPointerClick(this, ped, mi)
type CurStatic = unsafe extern "C" fn(*mut c_void) -> *mut c_void; // EventSystem.get_current(mi)
type Ctor1 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void); // ctor(this, es, mi)

/// GameObject/component name of a button (cached), via direct get_name call.
pub(crate) fn button_name(this: *mut c_void) -> String {
    let key = this as usize;
    if let Ok(m) = name_cache().lock() {
        if let Some(n) = m.get(&key) {
            return n.clone();
        }
    }
    let code = GETNAME_C.load(Ordering::Relaxed);
    let name = if code != 0 {
        unsafe {
            let f: RetPtr = std::mem::transmute(code);
            let s = f(this, GETNAME_MI.load(Ordering::Relaxed) as *mut c_void);
            il2cpp::read_string(s)
        }
    } else {
        String::new()
    };
    if let Ok(mut m) = name_cache().lock() {
        m.insert(key, name.clone());
    }
    // Diagnostic: log each distinct button name seen while the result window is
    // open (identifies e.g. the real name of "Next").
    if WINDOW_OPEN.load(Ordering::Relaxed) && !name.is_empty() {
        if let Ok(mut s) = logged_names().lock() {
            if s.insert(name.clone()) {
                rr_log(&format!("[race-result] button seen: \"{name}\" press={}", is_press_target(&name)));
            }
        }
    }
    name
}

/// Synthetic PointerEventData(EventSystem.current), via direct ctor call.
pub(crate) unsafe fn make_pointer_event() -> *mut c_void {
    let ctor = CTOR_C.load(Ordering::Relaxed);
    let ped_cls = C_PED.load(Ordering::Relaxed);
    if ctor == 0 || ped_cls == 0 {
        return std::ptr::null_mut();
    }
    let cur_c = CUR_C.load(Ordering::Relaxed);
    let es = if cur_c != 0 {
        let f: CurStatic = std::mem::transmute(cur_c);
        f(CUR_MI.load(Ordering::Relaxed) as *mut c_void)
    } else {
        std::ptr::null_mut()
    };
    let obj = il2cpp::object_new(ped_cls as il2cpp::Class);
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    let f: Ctor1 = std::mem::transmute(ctor);
    f(obj, es, CTOR_MI.load(Ordering::Relaxed) as *mut c_void);
    obj
}

/// Raw click on a ButtonCommon (no whitelist/dedup) — reuses the race-result click primitives to
/// auto-press the shop "BackButton" after a buy. Returns true once it actually clicked.
pub(crate) unsafe fn click_now(this: *mut c_void) -> bool {
    if this.is_null() || BUSY.load(Ordering::Relaxed) {
        return false;
    }
    let il = ISLOCK_C.load(Ordering::Relaxed);
    if il != 0 {
        let f: RetBool = std::mem::transmute(il);
        if f(this, ISLOCK_MI.load(Ordering::Relaxed) as *mut c_void) {
            return false; // locked → retry next frame
        }
    }
    let opc = OPC_C.load(Ordering::Relaxed);
    if opc == 0 {
        return false;
    }
    let ped = make_pointer_event();
    if ped.is_null() {
        return false;
    }
    BUSY.store(true, Ordering::Relaxed);
    let f: Click = std::mem::transmute(opc);
    f(this, ped, OPC_MI.load(Ordering::Relaxed) as *mut c_void);
    BUSY.store(false, Ordering::Relaxed);
    true
}

pub(crate) fn auto_press(this: *mut c_void) {
    if this.is_null() || BUSY.load(Ordering::Relaxed) {
        return;
    }
    let name = button_name(this);
    if !press_allowed(&name) {
        return;
    }
    let key = this as usize;
    let now = clock().elapsed().as_millis() as u64;
    let max = if is_multi(&name) { MULTI_MAX } else { 1 };
    // Read-only check — do NOT consume an attempt yet (mirrors native_skip.js:
    // the count/lastPress only advance AFTER a successful click). Otherwise a
    // transiently-locked button (e.g. "Next" during the result reveal) burns its
    // single allowed press on a locked frame and never retries.
    {
        let st = match press_state().lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        let (cnt, last) = *st.get(&key).unwrap_or(&(0, 0));
        if cnt >= max || now.wrapping_sub(last) < PRESS_GAP_MS {
            return;
        }
    }
    unsafe {
        // Respect button lock — return WITHOUT consuming the attempt so we retry
        // next frame once it unlocks.
        let il = ISLOCK_C.load(Ordering::Relaxed);
        if il != 0 {
            let f: RetBool = std::mem::transmute(il);
            if f(this, ISLOCK_MI.load(Ordering::Relaxed) as *mut c_void) {
                // Button is locked. Normally it unlocks in a frame or two and we just retry — but
                // the victory-concert result screen leaves a SteamInputBlock up that never lifts on
                // its own, so the advance button (Next / SingleModeNext / Continue) stays locked
                // forever and the skip stalls (window_open=true, no advance). After a short grace
                // (so we never disturb a normal transient lock), lift that block ourselves — the
                // exact PlayClose the skipped flow would have run — so the button unlocks and our
                // next-frame click lands. Only for the explicit advance buttons, and auto_press
                // already runs only when rr_should_advance (won / no retries), so this is safe.
                if is_advance_button(&name) {
                    let next = RR_NEXT_LIFT.load(Ordering::Relaxed);
                    if next == 0 {
                        RR_NEXT_LIFT.store(now + RR_LIFT_GRACE_MS, Ordering::Relaxed);
                    } else if now >= next {
                        lift_input_block();
                        RR_NEXT_LIFT.store(now + RR_LIFT_THROTTLE_MS, Ordering::Relaxed);
                    }
                }
                return;
            }
        }
        let opc = OPC_C.load(Ordering::Relaxed);
        if opc == 0 {
            return;
        }
        let ped = make_pointer_event();
        if ped.is_null() {
            return;
        }
        BUSY.store(true, Ordering::Relaxed);
        let f: Click = std::mem::transmute(opc);
        f(this, ped, OPC_MI.load(Ordering::Relaxed) as *mut c_void);
        BUSY.store(false, Ordering::Relaxed);
    }
    // Click succeeded → now consume the attempt + stamp the time.
    if let Ok(mut st) = press_state().lock() {
        let (cnt, _) = *st.get(&key).unwrap_or(&(0, 0));
        st.insert(key, (cnt + 1, now));
    }
    RR_PRESSES.fetch_add(1, Ordering::Relaxed);
    // We advanced — restart the stuck-lift grace for whatever screen comes next.
    RR_NEXT_LIFT.store(0, Ordering::Relaxed);
}

/// Auto-close a pushed dialog (the JS DialogManager.Push*→Close path).
pub(crate) fn auto_close(dlg: *mut c_void) {
    if dlg.is_null() || BUSY.load(Ordering::Relaxed) {
        return;
    }
    if !il2cpp::object_class_name(dlg).contains("DialogCommon") {
        return;
    }
    {
        let mut d = match done_dlg().lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if !d.insert(dlg as usize) {
            return;
        }
    }
    let code = CLOSE_C.load(Ordering::Relaxed);
    if code == 0 {
        return;
    }
    unsafe {
        BUSY.store(true, Ordering::Relaxed);
        let f: Void2 = std::mem::transmute(code);
        f(dlg, CLOSE_MI.load(Ordering::Relaxed) as *mut c_void);
        BUSY.store(false, Ordering::Relaxed);
    }
}
