//! Heaven Plan B — B4: native race reader (port of core/modules/raceread.js).
//!
//! Hooks the client-side replay path of RaceSimulateReader to read live horse
//! frames + the player index, and publishes a RaceState into the shared store.
//! All reads are raw fixed-offset memory (no managed invokes) → low risk.
//!
//! HorseFrameData: Distance@0x10 Lane@0x14 Speed@0x18 Hp@0x1c (f32)
//!                 Temptation@0x20 BlockFront@0x21 (i8)
//!
//! Finish placement (for the race-result Top-1 skip gate): resolved at
//! `_ImportPostProcess` (fires even on SKIP). The player's sim horse index comes
//! from the msgpack race response (frame_order - 1, published by the response hook
//! via `set_net_player`); their FinishOrder is read out of the sim's own result
//! array. See docs (local) for the full data reference.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use retour::RawDetour;

use crate::data::RaceHorse;
use crate::htt_il2cpp as h;
use crate::il2cpp;
use crate::ipc;

const EMIT_MS: u64 = 500; // ~2 Hz frame publish

fn clock() -> &'static Instant {
    crate::tools::clock()
}

fn rlog(msg: &str) {
    crate::tools::log(msg);
}

#[inline]
unsafe fn rf32(base: *mut c_void, off: usize) -> f64 {
    ((base as usize + off) as *const f32).read_unaligned() as f64
}
#[inline]
unsafe fn ri8(base: *mut c_void, off: usize) -> i64 {
    ((base as usize + off) as *const i8).read_unaligned() as i64
}

fn read_horse_frame(retval: *mut c_void, idx: i64) -> RaceHorse {
    unsafe {
        RaceHorse {
            idx,
            dist: (rf32(retval, 0x10) * 10.0).round() / 10.0,
            lane: (rf32(retval, 0x14) * 100.0).round() / 100.0,
            speed: (rf32(retval, 0x18) * 100.0).round() / 100.0,
            hp: (rf32(retval, 0x1c) * 100.0).round() / 100.0,
            max_hp: 0.0, // not exposed by the frame struct (matches raceread.js)
            tempt: ri8(retval, 0x20),
            block: ri8(retval, 0x21),
        }
    }
}

static PLAYER_INDEX: AtomicI32 = AtomicI32::new(-1);
static LAST_EMIT: AtomicU64 = AtomicU64::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static BUFFER: OnceLock<Mutex<HashMap<i64, RaceHorse>>> = OnceLock::new();
fn buffer() -> &'static Mutex<HashMap<i64, RaceHorse>> {
    BUFFER.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn stats() -> (i32, u64) {
    (PLAYER_INDEX.load(Ordering::Relaxed), FRAMES.load(Ordering::Relaxed))
}

// ── player finish placement (for the race-result Top-1 skip gate) ────────────
// 1 = won (1st), N = N-th, 0 = not yet known. Set per race at _ImportPostProcess.
static PLAYER_FINISH_ORDER: AtomicI32 = AtomicI32::new(0);
/// 1 = the player won this race (top-1). 0 = not finished / not yet known.
pub fn player_finish_order() -> i32 {
    PLAYER_FINISH_ORDER.load(Ordering::Relaxed)
}

// ── remaining race retries ("continues") ────────────────────────────────────
// `available_continue_num` from the career response: how many times you can still
// retry a lost race. -1 = unknown. Lets the race-result skip auto-advance even on a
// LOSS once no retries remain (no point holding for a retry you can't do).
static CONTINUES: AtomicI32 = AtomicI32::new(-1);
/// Remaining race retries. -1 until a career response reports it.
pub fn continues_available() -> i32 {
    CONTINUES.load(Ordering::Relaxed)
}
/// Published by the response hooks when a career payload reports available_continue_num.
pub fn set_continues_available(n: i32) {
    CONTINUES.store(n, Ordering::Relaxed);
}

// Player's horse identity from the msgpack race response (published by the response
// DecompressResponse hook). The player is the only horse with viewer_id != 0;
// its `frame_order - 1` is the sim horse index used to index the result array.
static NET_PLAYER_FRAMEORDER: AtomicI32 = AtomicI32::new(-1);
/// Called by the response hook when a race_horse_data payload is seen.
pub fn set_net_player(_arr_idx: i32, frame_order: i32, _horses: i32) {
    NET_PLAYER_FRAMEORDER.store(frame_order, Ordering::Relaxed);
}

// ── race header (track + distance + distance-type) for the freecam telemetry HUD ──
// Computed once per race on the game main thread (RaceManager.GetPlayerHorseIndex hook).
static M_RACEINFO: AtomicUsize = AtomicUsize::new(0);
static MI_RACEINFO: AtomicUsize = AtomicUsize::new(0);
static M_TRACKID: AtomicUsize = AtomicUsize::new(0);
static MI_TRACKID: AtomicUsize = AtomicUsize::new(0);
static M_CDIST: AtomicUsize = AtomicUsize::new(0);
static MI_CDIST: AtomicUsize = AtomicUsize::new(0);
static M_CDTYPE: AtomicUsize = AtomicUsize::new(0);
static MI_CDTYPE: AtomicUsize = AtomicUsize::new(0);
static M_GRADE: AtomicUsize = AtomicUsize::new(0);
static MI_GRADE: AtomicUsize = AtomicUsize::new(0);
static COURSE_DIST: AtomicI32 = AtomicI32::new(0);
static TRACK_ID: AtomicI32 = AtomicI32::new(0);
static RACE_GRADE: AtomicI32 = AtomicI32::new(0);
/// Race grade value (100=G1, 200=G2, 300=G3, 400=OP, …). 0 until known.
pub fn race_grade() -> i32 {
    RACE_GRADE.load(Ordering::Relaxed)
}
/// Current racecourse id (e.g. 10009 = Hanshin). 0 until known. For per-course freecam poses.
pub fn track_id() -> i32 {
    TRACK_ID.load(Ordering::Relaxed)
}
static HEADER: OnceLock<Mutex<String>> = OnceLock::new();
fn header_slot() -> &'static Mutex<String> {
    HEADER.get_or_init(|| Mutex::new(String::new()))
}
/// e.g. "Hanshin 1600m Mile". Empty until a race is loaded.
pub fn race_header() -> String {
    header_slot().lock().map(|s| s.clone()).unwrap_or_default()
}
/// Total course distance in metres (for progress %). 0 until known.
pub fn course_distance() -> i32 {
    COURSE_DIST.load(Ordering::Relaxed)
}

fn track_name(id: i32) -> String {
    match id {
        10001 => "Sapporo",
        10002 => "Hakodate",
        10003 => "Niigata",
        10004 => "Fukushima",
        10005 => "Nakayama",
        10006 => "Tokyo",
        10007 => "Chukyo",
        10008 => "Kyoto",
        10009 => "Hanshin",
        10010 => "Kokura",
        10101 => "Ooi",
        _ => return format!("Track {id}"),
    }
    .to_string()
}
fn dist_type(t: i32) -> &'static str {
    match t {
        1 => "Short",
        2 => "Mile",
        3 => "Medium",
        4 => "Long",
        _ => "",
    }
}

unsafe fn call_i32(fnp: &AtomicUsize, mip: &AtomicUsize, this: *mut c_void) -> i32 {
    let p = fnp.load(Ordering::Relaxed);
    if p == 0 || this.is_null() {
        return 0;
    }
    let f: unsafe extern "C" fn(*mut c_void, *const c_void) -> i32 = std::mem::transmute(p);
    f(this, mip.load(Ordering::Relaxed) as *const c_void)
}
unsafe fn call_obj(fnp: &AtomicUsize, mip: &AtomicUsize, this: *mut c_void) -> *mut c_void {
    let p = fnp.load(Ordering::Relaxed);
    if p == 0 || this.is_null() {
        return std::ptr::null_mut();
    }
    let f: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void = std::mem::transmute(p);
    f(this, mip.load(Ordering::Relaxed) as *const c_void)
}

// Last RaceInfo pointer we built a header from. Recompute when it CHANGES (a new race) —
// don't rely on a reset hook (ImportDirect may not be installed on every build, which left
// the header stuck on the first race's track/distance all session).
static LAST_RACEINFO: AtomicUsize = AtomicUsize::new(0);

/// Build the race header (main thread). `mgr` = RaceManager instance. Recomputes whenever the
/// RaceInfo pointer changes (= a new race loaded).
fn compute_header(mgr: *mut c_void) {
    unsafe {
        let ri = call_obj(&M_RACEINFO, &MI_RACEINFO, mgr);
        if ri.is_null() {
            return;
        }
        let same = ri as usize == LAST_RACEINFO.load(Ordering::Relaxed);
        if same && header_slot().lock().map(|s| !s.is_empty()).unwrap_or(false) {
            return; // same race, header already built
        }
        let dist = call_i32(&M_CDIST, &MI_CDIST, ri);
        if dist <= 0 {
            return; // RaceInfo not ready yet — try again next call
        }
        LAST_RACEINFO.store(ri as usize, Ordering::Relaxed);
        let tid = call_i32(&M_TRACKID, &MI_TRACKID, ri);
        let dtype = call_i32(&M_CDTYPE, &MI_CDTYPE, ri);
        let grade = call_i32(&M_GRADE, &MI_GRADE, ri);
        COURSE_DIST.store(dist, Ordering::Relaxed);
        TRACK_ID.store(tid, Ordering::Relaxed);
        RACE_GRADE.store(grade, Ordering::Relaxed);
        let h = format!("{} {}m {}", track_name(tid), dist, dist_type(dtype));
        rlog(&format!("[race] header: {h}  (trackId={tid} type={dtype})"));
        if let Ok(mut s) = header_slot().lock() {
            *s = h;
        }
    }
}

/// Read `RaceSimulateData._horseResultDataArray[idx].FinishOrder` (0-based,
/// 0 = 1st). The array is a managed ref array: data at base+0x20 holds element
/// pointers; each element's FinishOrder is at +0x10. Returns -1 if unavailable.
unsafe fn sim_finish_order(res_arr: *mut c_void, idx: i32) -> i32 {
    if res_arr.is_null() || idx < 0 {
        return -1;
    }
    let alen = match h::ARRAY_LENGTH {
        Some(f) => f,
        None => return -1,
    };
    if idx >= alen(res_arr) as i32 {
        return -1;
    }
    let p = ((res_arr as usize + 0x20 + idx as usize * 8) as *const usize).read_unaligned();
    if p == 0 {
        return -1;
    }
    ((p + 0x10) as *const i32).read_unaligned()
}

// ── detour slots ────────────────────────────────────────────────────────────
macro_rules! slot {
    ($t:ident, $d:ident) => {
        static $t: AtomicUsize = AtomicUsize::new(0);
        static $d: OnceLock<RawDetour> = OnceLock::new();
    };
}
slot!(TR_FRAME, D_FRAME);
slot!(TR_IMPORT, D_IMPORT);
slot!(TR_POST, D_POST);
slot!(TR_PLAYER, D_PLAYER);
slot!(TR_RTEXP, D_RTEXP);

type VoidM = unsafe extern "C" fn(*mut c_void, *mut c_void);
type FrameM = unsafe extern "C" fn(*mut c_void, i32, *mut c_void) -> *mut c_void;
type IntM = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;

unsafe fn call_void(tr: &AtomicUsize, this: *mut c_void, m: *mut c_void) {
    let t = tr.load(Ordering::Relaxed);
    if t != 0 {
        let f: VoidM = std::mem::transmute(t);
        f(this, m);
    }
}

// GetFrameDataFromCache(reader, horseIndex) → HorseFrameData*  (live telemetry)
unsafe extern "C" fn on_frame(reader: *mut c_void, horse_idx: i32, m: *mut c_void) -> *mut c_void {
    let t = TR_FRAME.load(Ordering::Relaxed);
    let retval = if t != 0 {
        let f: FrameM = std::mem::transmute(t);
        f(reader, horse_idx, m)
    } else {
        std::ptr::null_mut()
    };
    if retval.is_null() {
        return retval;
    }
    let idx = horse_idx as i64;
    if let Ok(mut buf) = buffer().lock() {
        buf.insert(idx, read_horse_frame(retval, idx));
    }
    if horse_idx == 0 {
        let now = clock().elapsed().as_millis() as u64;
        if now.wrapping_sub(LAST_EMIT.load(Ordering::Relaxed)) >= EMIT_MS {
            LAST_EMIT.store(now, Ordering::Relaxed);
            FRAMES.fetch_add(1, Ordering::Relaxed);
            let time = rf32(reader, 0x18);
            let mut horses: Vec<RaceHorse> = buffer()
                .lock()
                .map(|b| b.values().cloned().collect())
                .unwrap_or_default();
            horses.sort_by_key(|h| h.idx);
            let pidx = PLAYER_INDEX.load(Ordering::Relaxed) as i64;
            ipc::with_race(|r| {
                r.active = true;
                r.time = (time * 100.0).round() / 100.0;
                r.player_index = pidx;
                r.horse_count = horses.len() as i64;
                r.horses = horses.clone();
            });
        }
        if let Ok(mut buf) = buffer().lock() {
            buf.clear();
        }
    }
    retval
}

// ImportDirect — race start (reset). NOTE: this hook may fail to resolve on some
// builds (argc mismatch); the placement is also reset defensively in on_post.
unsafe extern "C" fn on_import(this: *mut c_void, m: *mut c_void) {
    LAST_EMIT.store(0, Ordering::Relaxed);
    PLAYER_INDEX.store(-1, Ordering::Relaxed);
    PLAYER_FINISH_ORDER.store(0, Ordering::Relaxed);
    COURSE_DIST.store(0, Ordering::Relaxed);
    if let Ok(mut s) = header_slot().lock() {
        s.clear(); // recompute the header for the new race
    }
    if let Ok(mut buf) = buffer().lock() {
        buf.clear();
    }
    ipc::with_race(|r| {
        *r = Default::default();
        r.active = true;
    });
    call_void(&TR_IMPORT, this, m);
}

// _ImportPostProcess — sim info (horse count, duration) + finish placement.
// Fires even when the race is SKIPPED (the sim is imported regardless), which is
// why the placement is resolved here rather than from the live RaceManager (the
// latter isn't even instantiated for a skipped career race).
unsafe extern "C" fn on_post(reader: *mut c_void, m: *mut c_void) {
    call_void(&TR_POST, reader, m);
    let sim = ((reader as usize + 0x10) as *const usize).read_unaligned() as *mut c_void;
    if !sim.is_null() {
        let horse_count = ((sim as usize + 0x30) as *const i32).read_unaligned() as i64;
        let duration = rf32(sim, 0x44);
        ipc::with_race(|r| {
            r.horse_count = horse_count;
            r.duration = (duration * 100.0).round() / 100.0;
        });
    }
    // Winner index (RaceSimulateReader.TopFinishHorseIndex@0x30) — info only.
    let top = ((reader as usize + 0x30) as *const i32).read_unaligned();

    // Resolve the player's placement. Verified live:
    //   • player's sim horse index = race_horse_data frame_order - 1
    //   • _horseResultDataArray is a ref array; FinishOrder is 0-based (0 = 1st)
    //   • so place = FinishOrder + 1   (place == 1 → won → race-result skip)
    let res_arr = if sim.is_null() {
        std::ptr::null_mut()
    } else {
        ((sim as usize + 0x20) as *const usize).read_unaligned() as *mut c_void
    };
    // Fresh per race (the ImportDirect reset may not install): clear any stale
    // placement so an unresolved race can't inherit a previous SKIP.
    PLAYER_FINISH_ORDER.store(0, Ordering::Relaxed);
    let fo_i = NET_PLAYER_FRAMEORDER.load(Ordering::Relaxed);
    let sim_idx = if fo_i > 0 { fo_i - 1 } else { -1 };
    let fin0 = sim_finish_order(res_arr, sim_idx); // 0-based FinishOrder, -1 if N/A
    if fin0 >= 0 {
        let place = fin0 + 1;
        PLAYER_FINISH_ORDER.store(place, Ordering::Relaxed);
        rlog(&format!(
            "[race] FINISH: frameOrder={fo_i} simIdx={sim_idx} top={top} -> place={place} ({})",
            if place == 1 { "WON -> SKIP" } else { "MANUAL" }
        ));
    }
}

// RaceManager.GetPlayerHorseIndex → int  (player index for the live race panel)
unsafe extern "C" fn on_player(this: *mut c_void, m: *mut c_void) -> i32 {
    let t = TR_PLAYER.load(Ordering::Relaxed);
    let idx = if t != 0 {
        let f: IntM = std::mem::transmute(t);
        f(this, m)
    } else {
        -1
    };
    if idx >= 0 && PLAYER_INDEX.load(Ordering::Relaxed) != idx {
        PLAYER_INDEX.store(idx, Ordering::Relaxed);
        ipc::with_race(|r| r.player_index = idx as i64);
    }
    // Build the race header (track/distance/type) once per race — `this` is RaceManager.
    compute_header(this);
    idx
}

// RaceInfo.get_RaceTrackId → int. Fires whenever the race's track id is read —
// including SKIPPED races (where RaceManager is never built), so it's the reliable
// "a race exists" signal for the JSON export. We forward `this` (the RaceInfo) to
// the exporter (which no-ops unless enabled + a genuinely new race), then return
// the original value. Heaven's own track-id reads route through here transparently.
unsafe extern "C" fn on_rt_export(this: *mut c_void, m: *mut c_void) -> i32 {
    crate::race_export::maybe_dump(this);
    let t = TR_RTEXP.load(Ordering::Relaxed);
    if t != 0 {
        let f: IntM = std::mem::transmute(t);
        f(this, m)
    } else {
        0
    }
}

unsafe fn hook(
    klass: il2cpp::Class,
    name: &str,
    argc: i32,
    det: *const (),
    tr: &AtomicUsize,
    keep: &OnceLock<RawDetour>,
) -> bool {
    let m = il2cpp::method(klass, name, argc);
    if m.is_null() {
        return false;
    }
    let target = il2cpp::method_pointer(m);
    if target.is_null() {
        return false;
    }
    if il2cpp::is_detoured(target) {
        return false;
    }
    match RawDetour::new(target as *const (), det) {
        Ok(d) => {
            if d.enable().is_ok() {
                tr.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = keep.set(d);
                return true;
            }
            false
        }
        Err(_) => false,
    }
}

/// Install the race reader hooks. Returns a short note of what resolved.
pub fn install() -> String {
    unsafe {
        h::init();
    }
    let reader = il2cpp::class("Gallop.RaceSimulateReader");
    let mgr = il2cpp::class("Gallop.RaceManager");
    if reader.is_null() {
        return "reader miss".into();
    }
    let mut got = Vec::new();
    unsafe {
        if hook(reader, "GetFrameDataFromCache", 1, on_frame as *const (), &TR_FRAME, &D_FRAME) {
            got.push("frame");
        }
        if hook(reader, "ImportDirect", 0, on_import as *const (), &TR_IMPORT, &D_IMPORT) {
            got.push("import");
        }
        if hook(reader, "_ImportPostProcess", 0, on_post as *const (), &TR_POST, &D_POST) {
            got.push("post");
        }
        if !mgr.is_null()
            && hook(mgr, "GetPlayerHorseIndex", 0, on_player as *const (), &TR_PLAYER, &D_PLAYER)
        {
            got.push("player");
        }
        // Race-header method pointers: RaceManager.get_RaceInfo → RaceInfo getters.
        if !mgr.is_null() {
            let m = il2cpp::method(mgr, "get_RaceInfo", 0);
            if !m.is_null() {
                M_RACEINFO.store(il2cpp::method_pointer(m) as usize, Ordering::Relaxed);
                MI_RACEINFO.store(m as usize, Ordering::Relaxed);
            }
        }
        let ri = il2cpp::class("Gallop.RaceInfo");
        for (name, fnp, mip) in [
            ("get_RaceTrackId", &M_TRACKID, &MI_TRACKID),
            ("get_CourseDistance", &M_CDIST, &MI_CDIST),
            ("get_CourseDistanceType", &M_CDTYPE, &MI_CDTYPE),
            ("get_Grade", &M_GRADE, &MI_GRADE),
        ] {
            let m = il2cpp::method(ri, name, 0);
            if !m.is_null() {
                fnp.store(il2cpp::method_pointer(m) as usize, Ordering::Relaxed);
                mip.store(m as usize, Ordering::Relaxed);
            }
        }
        if M_RACEINFO.load(Ordering::Relaxed) != 0 {
            got.push("header");
        }
        // Per-race JSON export: detour RaceInfo.get_RaceTrackId (fires on every race,
        // incl. skipped ones). The exporter no-ops unless its toggle is on.
        if !ri.is_null()
            && hook(ri, "get_RaceTrackId", 0, on_rt_export as *const (), &TR_RTEXP, &D_RTEXP)
        {
            got.push("export");
        }
    }
    format!("hooks=[{}]", got.join(","))
}
