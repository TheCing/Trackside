//! pruner — follower-list pruner: trim the oldest-inactive followers when the list nears the
//! 1000 cap, so new padders can follow you again.
//!
//! Flow (all destructive steps are opt-in and previewed):
//!   1. Open the game's follower list, press **Preview prune** — Heaven reads the live list,
//!      sorts by last login (most inactive first), drops whitelisted trainers, and shows the
//!      exact set that WOULD be removed (down to the target size). Nothing is sent yet.
//!   2. Review the dry-run list; pin anyone you want to keep (adds to the whitelist).
//!   3. Press **Start pruning** — removals fire one at a time at a jittered human pace
//!      (same philosophy as the hunter's roll cadence), driven from the game main thread
//!      by `pump()` (riding hunter's TweenManager.Update tick).
//!
//! Safety rails: a persisted whitelist that is never pruned, a hard per-run cap, dry-run
//! before every run, and any read/remove failure stops the run immediately.
//!
//! The IL2CPP boundary lives in `bridge` below and resolves classes/methods by NAME at
//! runtime (non-fatal when this game build renames them). Because the follower screen has
//! not been RE'd yet (unlike TT), `bridge` tries an ordered list of candidate names and the
//! panel exposes a **Scan** action that dumps every Friend/Follow class + its methods to
//! `trackside-logs/trackside-follower-scan.txt` — one live run of that log pins the real names.

#![allow(static_mut_refs)]
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Hard server cap on followers.
pub const FOLLOWER_CAP: usize = 1000;
/// Never remove more than this many in one run, regardless of target math.
pub const MAX_REMOVALS_PER_RUN: usize = 100;

/// One follower as read from the live list.
#[derive(Clone, Serialize, Deserialize)]
pub struct Follower {
    pub viewer_id: i64,
    pub name: String,
    /// Days since last login, when the game exposes it (sort key; None sorts last = kept).
    pub last_login_days: Option<i32>,
}

/// A trainer that must never be pruned.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Pin {
    pub viewer_id: i64,
    pub name: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Store {
    /// Prune down to this many followers.
    target: usize,
    whitelist: Vec<Pin>,
}
impl Default for Store {
    fn default() -> Self {
        Store { target: 900, whitelist: Vec::new() }
    }
}

// ── state ─────────────────────────────────────────────────────────────────────

/// Lifecycle: Idle → (preview requested → computed) Preview → Pruning → back to Idle.
#[derive(Clone, Copy, PartialEq)]
pub enum Phase {
    Idle,
    /// pump() is reading the live list (one frame, usually).
    Reading,
    /// Dry-run list ready; waiting for Start / Cancel.
    Preview,
    Pruning,
}

static PHASE: AtomicUsize = AtomicUsize::new(0); // Phase as usize
static REQ_PREVIEW: AtomicBool = AtomicBool::new(false);
static REQ_SCAN: AtomicBool = AtomicBool::new(false);
static REMOVED: AtomicUsize = AtomicUsize::new(0);
static TOTAL_LIVE: AtomicUsize = AtomicUsize::new(0); // last read follower count
// When the next removal may fire (ms on our clock); u64::MAX = none scheduled.
static NEXT_MS: AtomicU64 = AtomicU64::new(u64::MAX);

fn phase() -> Phase {
    match PHASE.load(Ordering::Relaxed) {
        1 => Phase::Reading,
        2 => Phase::Preview,
        3 => Phase::Pruning,
        _ => Phase::Idle,
    }
}
fn set_phase(p: Phase) {
    PHASE.store(
        match p {
            Phase::Idle => 0,
            Phase::Reading => 1,
            Phase::Preview => 2,
            Phase::Pruning => 3,
        },
        Ordering::Relaxed,
    );
}

fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(load_from_disk()))
}
fn candidates_buf() -> &'static Mutex<Vec<Follower>> {
    static S: OnceLock<Mutex<Vec<Follower>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}
fn status_buf() -> &'static Mutex<String> {
    static S: OnceLock<Mutex<String>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(String::new()))
}
fn set_status(s: String) {
    if let Ok(mut g) = status_buf().lock() {
        *g = s;
    }
}

fn clock() -> &'static Instant {
    static C: OnceLock<Instant> = OnceLock::new();
    C.get_or_init(Instant::now)
}
fn now_ms() -> u64 {
    clock().elapsed().as_millis() as u64
}

/// Human-like delay between removals: 2.5–6.0 s, with an occasional longer rest (~1/8 of the
/// time, +4–9 s). Same xorshift scheme as the hunter's roll cadence — removals are heavier
/// actions than list refreshes, so the band sits a bit wider.
fn next_delay_ms() -> u64 {
    static SEED: AtomicU64 = AtomicU64::new(0);
    let mut s = SEED.load(Ordering::Relaxed);
    if s == 0 {
        s = (clock().elapsed().as_nanos() as u64) | 1;
    }
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    SEED.store(s, Ordering::Relaxed);
    let mut d = 2500 + (s % 3500); // 2.5–6.0 s
    if (s >> 33) % 8 == 0 {
        d += 4000 + ((s >> 5) % 5000); // ~1/8: an extra 4–9 s pause
    }
    d
}

// ── persistence ───────────────────────────────────────────────────────────────

fn json_path() -> std::path::PathBuf {
    crate::paths::local_file_migrated("trackside_follower_pruner.json", "heaven_follower_pruner.json")
}
fn load_from_disk() -> Store {
    match std::fs::read(json_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Store::default(),
    }
}
fn save_to_disk(s: &Store) {
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        let _ = std::fs::write(json_path(), json);
    }
}

fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(crate::paths::log_file("trackside.log"))
    {
        let _ = writeln!(f, "[pruner] {msg}");
    }
}

// ── public API consumed by the overlay UI ─────────────────────────────────────

pub fn current_phase() -> Phase {
    phase()
}
pub fn status() -> String {
    status_buf().lock().map(|s| s.clone()).unwrap_or_default()
}
pub fn removed() -> usize {
    REMOVED.load(Ordering::Relaxed)
}
/// Last live follower count read (0 = never read).
pub fn live_count() -> usize {
    TOTAL_LIVE.load(Ordering::Relaxed)
}

pub fn target() -> usize {
    store().lock().map(|s| s.target).unwrap_or(900)
}
pub fn set_target(t: usize) {
    if let Ok(mut s) = store().lock() {
        s.target = t.min(FOLLOWER_CAP);
        save_to_disk(&s);
    }
}

pub fn whitelist() -> Vec<Pin> {
    store().lock().map(|s| s.whitelist.clone()).unwrap_or_default()
}

/// Case/whitespace-insensitive name compare (unicode-aware — trainer names may be Japanese).
fn name_eq(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Is a follower whitelisted? An id-pin (viewer_id != 0, from the preview) matches by id; a
/// name-pin (viewer_id == 0, added manually) matches by name — so you can protect a trainer
/// up front without ever seeing them in the cull list.
pub fn is_whitelisted(wl: &[Pin], viewer_id: i64, name: &str) -> bool {
    wl.iter().any(|p| {
        if p.viewer_id != 0 {
            p.viewer_id == viewer_id
        } else {
            name_eq(&p.name, name)
        }
    })
}

pub fn pin(viewer_id: i64, name: &str) {
    if let Ok(mut s) = store().lock() {
        if !s.whitelist.iter().any(|p| p.viewer_id == viewer_id && viewer_id != 0) {
            s.whitelist.push(Pin { viewer_id, name: to_owned_trim(name) });
            save_to_disk(&s);
        }
    }
    // A freshly-pinned trainer must leave the pending dry-run set immediately.
    if let Ok(mut c) = candidates_buf().lock() {
        c.retain(|f| f.viewer_id != viewer_id);
    }
}

/// Manually whitelist a trainer by NAME (no viewer_id needed) — protects anyone by name,
/// even players not currently near the cull threshold.
pub fn whitelist_name(name: &str) {
    let name = to_owned_trim(name);
    if name.is_empty() {
        return;
    }
    let lname = name.to_lowercase();
    if let Ok(mut s) = store().lock() {
        // Dedup against existing name-pins (case-insensitive).
        if !s.whitelist.iter().any(|p| p.viewer_id == 0 && p.name.to_lowercase() == lname) {
            s.whitelist.push(Pin { viewer_id: 0, name });
            save_to_disk(&s);
        }
    }
    // Drop any pending dry-run entries that now match the new name-pin.
    if let Ok(mut c) = candidates_buf().lock() {
        c.retain(|f| f.name.trim().to_lowercase() != lname);
    }
}

/// Remove one exact whitelist entry (id + name), so unpinning a name-pin can't wipe others
/// that share `viewer_id == 0`.
pub fn unpin_entry(viewer_id: i64, name: &str) {
    if let Ok(mut s) = store().lock() {
        s.whitelist.retain(|p| !(p.viewer_id == viewer_id && name_eq(&p.name, name)));
        save_to_disk(&s);
    }
}
fn to_owned_trim(s: &str) -> String {
    let t = s.trim();
    if t.len() > 64 { t[..64].to_string() } else { t.to_string() }
}

/// The dry-run set (what Start would remove), oldest-inactive first.
pub fn candidates() -> Vec<Follower> {
    candidates_buf().lock().map(|c| c.clone()).unwrap_or_default()
}

/// Ask pump() to read the live list and compute the dry-run set. UI-thread safe (flag only).
pub fn request_preview() {
    if phase() == Phase::Pruning {
        return;
    }
    set_phase(Phase::Reading);
    REQ_PREVIEW.store(true, Ordering::Relaxed);
    set_status("Reading follower list…".into());
}

/// Begin removing the previewed set. Only valid from Preview. Errors also land in
/// `status()` so the panel shows them persistently.
pub fn start() -> Result<(), String> {
    if phase() != Phase::Preview {
        set_status("Preview first.".into());
        return Err("Preview first.".into());
    }
    let n = candidates_buf().lock().map(|c| c.len()).unwrap_or(0);
    if n == 0 {
        set_status("Nothing to prune (everything pinned?). Preview again.".into());
        set_phase(Phase::Idle);
        return Err("Nothing to prune.".into());
    }
    REMOVED.store(0, Ordering::Relaxed);
    set_phase(Phase::Pruning);
    NEXT_MS.store(now_ms() + 1200, Ordering::Relaxed); // small lead-in before the first removal
    set_status(format!("Pruning… 0/{n}"));
    log(&format!("start: {n} removals queued (target {})", target()));
    Ok(())
}

/// Stop a run (or discard a preview). Never fails.
pub fn stop() {
    let was = phase();
    set_phase(Phase::Idle);
    NEXT_MS.store(u64::MAX, Ordering::Relaxed);
    if was == Phase::Pruning {
        set_status(format!("Stopped after {} removals.", removed()));
    } else {
        set_status(String::new());
    }
}

/// Ask pump() to dump the Friend/Follow class scan (RE aid). UI-thread safe.
pub fn request_scan() {
    REQ_SCAN.store(true, Ordering::Relaxed);
    set_status("Scanning game classes…".into());
}

// ── main-thread pump (rides hunter's TweenManager.Update hook) ────────────────

/// Run on the GAME MAIN THREAD every frame. Executes queued preview reads, the paced
/// removal loop, and queued scans — the only safe place for these IL2CPP calls.
pub fn pump() {
    if REQ_SCAN.swap(false, Ordering::Relaxed) {
        set_status(bridge::scan_dump());
    }
    if REQ_PREVIEW.swap(false, Ordering::Relaxed) {
        do_preview();
    }
    if phase() != Phase::Pruning {
        return;
    }
    let due = NEXT_MS.load(Ordering::Relaxed);
    if due == u64::MAX || now_ms() < due {
        return;
    }
    NEXT_MS.store(u64::MAX, Ordering::Relaxed); // consume; rescheduled below
    // Pop the next candidate and remove it.
    let next = candidates_buf().lock().ok().and_then(|mut c| if c.is_empty() { None } else { Some(c.remove(0)) });
    let Some(f) = next else {
        finish_run();
        return;
    };
    match bridge::remove_follower(f.viewer_id) {
        Ok(_) => {
            let done = REMOVED.fetch_add(1, Ordering::Relaxed) + 1;
            let left = candidates_buf().lock().map(|c| c.len()).unwrap_or(0);
            log(&format!("removed {} ({})", f.name, f.viewer_id));
            if left == 0 {
                finish_run();
            } else {
                let delay = next_delay_ms();
                NEXT_MS.store(now_ms() + delay, Ordering::Relaxed);
                set_status(format!(
                    "Pruning… {done}/{} · next in {:.1}s · removed: {}",
                    done + left,
                    delay as f32 / 1000.0,
                    f.name
                ));
            }
        }
        Err(e) => {
            set_phase(Phase::Idle);
            set_status(format!("Stopped: {e} ({} removed)", removed()));
            log(&format!("remove failed for {} ({}): {e}", f.name, f.viewer_id));
        }
    }
}

fn do_preview() {
    match bridge::read_followers() {
        Ok(mut all) => {
            TOTAL_LIVE.store(all.len(), Ordering::Relaxed);
            let tgt = target();
            if all.len() <= tgt {
                set_phase(Phase::Idle);
                set_status(format!("{} / {} followers — already at or below target {tgt}.", all.len(), FOLLOWER_CAP));
                return;
            }
            let wl = whitelist();
            // Most-inactive first; unknown last-login sorts LAST (kept unless nothing else remains).
            all.sort_by_key(|f| std::cmp::Reverse(f.last_login_days.unwrap_or(-1)));
            let want = (all.len() - tgt).min(MAX_REMOVALS_PER_RUN);
            let picked: Vec<Follower> = all
                .into_iter()
                .filter(|f| !is_whitelisted(&wl, f.viewer_id, &f.name))
                .take(want)
                .collect();
            let n = picked.len();
            if let Ok(mut c) = candidates_buf().lock() {
                *c = picked;
            }
            if n == 0 {
                set_phase(Phase::Idle);
                set_status("Everyone above target is whitelisted — nothing to prune.".into());
            } else {
                set_phase(Phase::Preview);
                set_status(format!(
                    "{} / {} followers — {n} would be removed (target {tgt}). Review below.",
                    live_count(),
                    FOLLOWER_CAP
                ));
            }
        }
        Err(e) => {
            set_phase(Phase::Idle);
            set_status(format!("Read failed: {e}"));
        }
    }
}

fn finish_run() {
    set_phase(Phase::Idle);
    set_status(format!("Done — removed {}.", removed()));
    log(&format!("run complete: {} removed", removed()));
}

/// Boot-time install: resolve what we can, never fatal. (No hooks in v1 — reads gate the flow.)
pub fn install() -> String {
    bridge::install()
}

// ── IL2CPP boundary ───────────────────────────────────────────────────────────

pub(crate) mod bridge {
    //! Runtime-resolved access to the game's follower data + remove action.
    //!
    //! The generic IL2CPP-decode helpers here (`invoke0`, `unbox_i64`, `plain_string`,
    //! `work_data_manager`, `rd_ptr`/`rd_i32`, `dump_class`) are pub(crate): the room
    //! finder's bridge reuses them for the same WorkDataManager/Obscured plumbing.
    //!
    //! STATUS: the follower screen has NOT been RE'd yet (no _research notes exist for it,
    //! unlike TT). Every entry point below walks an ordered candidate-name list and returns
    //! a precise error naming the first step that failed to resolve — run **Scan** on a
    //! machine with the game loaded and `trackside-logs/trackside-follower-scan.txt` gives the
    //! real class/method names to promote into (or reorder within) these lists.

    use core::ffi::c_void;

    use super::Follower;
    use crate::il2cpp;

    // Names CONFIRMED from a live scan (trackside-follower-scan.txt, 2026-07-01):
    //  - WorkDataManager.get_FriendData() -> Gallop.WorkFriendData
    //  - WorkFriendData.GetFollowerList() -> List<WorkFriendData.FriendData>  (a METHOD, not a get_ property)
    //  - FriendData getters: get_ViewerId / get_Name / get_LastLoginUnixTime — all CodeStage
    //    Obscured types (ObscuredLong / ObscuredString), so returns are DECODED, not cast.
    //  - The remove request is Gallop.FriendUnFollowerRequest ("UnFollower" = remove one of YOUR
    //    followers; FriendUnFollowRequest without the "-er" unfollows someone YOU follow — wrong one).
    /// Candidate getters on WorkDataManager for the friend/follow work-data blob.
    const WDM_GETTERS: &[&str] = &["get_FriendData", "get_FollowData", "get_FriendManageData"];
    /// Candidate accessors on WorkFriendData for the FOLLOWER list (people following you).
    const LIST_GETTERS: &[&str] = &["GetFollowerList", "get_FollowerList", "get_FollowerDataList"];
    /// Request classes whose send removes one follower (the same wire call the game's own
    /// remove button makes). Confirmed first; old guesses kept as fallbacks for other builds.
    const REMOVE_REQUESTS: &[&str] = &[
        "Gallop.FriendUnFollowerRequest",
        "Gallop.FriendFollowerDeleteRequest",
        "Gallop.FollowerDeleteRequest",
    ];
    /// Field names for the TARGET follower's viewer id on the remove request.
    /// CRITICAL ORDER: `friend_viewer_id` (the target, @0x88 on FriendUnFollowerRequest) MUST come
    /// before `viewer_id` — the parent RequestCommon.viewer_id (@0x10) is the SENDER's OWN id, and
    /// writing the target there would corrupt the request (wrong/no removal). Confirmed via scan.
    const VIEWER_ID_FIELDS: &[&str] = &["friend_viewer_id", "target_viewer_id", "targetViewerId"];

    fn log(msg: &str) {
        super::log(msg);
    }

    pub fn install() -> String {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return "il2cpp not ready".into();
        }
        // Purely diagnostic: report which candidates resolve in this game build so the boot
        // log shows the pruner's readiness without any behavioural cost.
        let wdm = il2cpp::class("Gallop.WorkDataManager");
        let g = WDM_GETTERS
            .iter()
            .find(|n| !il2cpp::method(wdm, n, 0).is_null())
            .copied()
            .unwrap_or("none");
        let wfd = il2cpp::class("Gallop.WorkFriendData");
        let l = LIST_GETTERS
            .iter()
            .find(|n| !il2cpp::method(wfd, n, 0).is_null())
            .copied()
            .unwrap_or("none");
        let req = REMOVE_REQUESTS
            .iter()
            .find(|n| !il2cpp::class(n).is_null())
            .copied()
            .unwrap_or("none");
        format!("follower pruner: wdm-getter:{g} list:{l} remove-req:{req}")
    }

    /// Invoke a 0-arg method via runtime_invoke — managed exceptions are captured (returns
    /// null) instead of unwinding through our native frames and crashing the game. Value-type
    /// returns come back BOXED (decode with `unbox_i64` / `plain_string`).
    pub(crate) unsafe fn invoke0(this: *mut c_void, klass: il2cpp::Class, method: &str) -> *mut c_void {
        if this.is_null() || klass.is_null() {
            return std::ptr::null_mut();
        }
        let m = il2cpp::method(klass, method, 0);
        if m.is_null() {
            return std::ptr::null_mut();
        }
        il2cpp::runtime_invoke(m, this, &mut [])
    }

    /// Decode a boxed integer return: plain Int64/Int32, or a CodeStage ObscuredLong/ObscuredInt
    /// (plain = hiddenValue ^ currentCryptoKey; field offsets read from metadata, and boxed
    /// value-type field offsets already include the 0x10 object header).
    pub(crate) unsafe fn unbox_i64(boxed: *mut c_void) -> Option<i64> {
        if boxed.is_null() {
            return None;
        }
        let k = il2cpp::object_class(boxed);
        match il2cpp::class_name(k).as_str() {
            "Int64" => Some(*((boxed as usize + 0x10) as *const i64)),
            "Int32" => Some(*((boxed as usize + 0x10) as *const i32) as i64),
            "Boolean" => Some(*((boxed as usize + 0x10) as *const u8) as i64),
            "ObscuredLong" => {
                let ko = il2cpp::field_offset(k, "currentCryptoKey")?;
                let ho = il2cpp::field_offset(k, "hiddenValue")?;
                let key = *((boxed as usize + ko) as *const i64);
                let hid = *((boxed as usize + ho) as *const i64);
                Some(hid ^ key)
            }
            "ObscuredInt" => {
                let ko = il2cpp::field_offset(k, "currentCryptoKey")?;
                let ho = il2cpp::field_offset(k, "hiddenValue")?;
                let key = *((boxed as usize + ko) as *const i32);
                let hid = *((boxed as usize + ho) as *const i32);
                Some((hid ^ key) as i64)
            }
            _ => None,
        }
    }

    /// A returned string, plain or obscured: System.String reads directly; anything else
    /// (ObscuredString) goes through its own ToString(), which returns the decrypted value.
    pub(crate) unsafe fn plain_string(obj: *mut c_void) -> String {
        if obj.is_null() {
            return String::new();
        }
        let k = il2cpp::object_class(obj);
        if il2cpp::class_name(k) == "String" {
            return il2cpp::read_string(obj);
        }
        let s = invoke0(obj, k, "ToString");
        if s.is_null() { String::new() } else { il2cpp::read_string(s) }
    }

    /// WorkDataManager.Instance, or null.
    pub(crate) unsafe fn work_data_manager() -> *mut c_void {
        let k = il2cpp::class("Gallop.WorkDataManager");
        if k.is_null() {
            return std::ptr::null_mut();
        }
        let gi = il2cpp::method(k, "get_Instance", 0);
        let gip = il2cpp::method_pointer(gi);
        if gip.is_null() {
            return std::ptr::null_mut();
        }
        let f: extern "C" fn(*const c_void) -> *mut c_void = std::mem::transmute(gip);
        f(gi as *const c_void)
    }

    #[inline]
    pub(crate) unsafe fn rd_ptr(base: *mut c_void, off: usize) -> *mut c_void {
        if base.is_null() {
            return std::ptr::null_mut();
        }
        *((base as usize + off) as *const *mut c_void)
    }
    #[inline]
    pub(crate) unsafe fn rd_i32(base: *mut c_void, off: usize) -> i32 {
        if base.is_null() {
            return 0;
        }
        *((base as usize + off) as *const i32)
    }

    /// Read the live follower list. MAIN THREAD ONLY (managed calls).
    pub fn read_followers() -> Result<Vec<Follower>, String> {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return Err("IL2CPP runtime not ready".into());
        }
        unsafe {
            let wdm = work_data_manager();
            if wdm.is_null() {
                return Err("WorkDataManager not loaded".into());
            }
            let wdm_class = il2cpp::class("Gallop.WorkDataManager");
            // 1) the friend/follow work-data blob
            let mut blob = std::ptr::null_mut();
            let mut blob_getter = "";
            for g in WDM_GETTERS {
                blob = invoke0(wdm, wdm_class, g);
                if !blob.is_null() {
                    blob_getter = g;
                    break;
                }
            }
            if blob.is_null() {
                return Err("friend work-data not found (open the follower list, then run Scan and send the log)".into());
            }
            // 2) the follower list on the blob
            let blob_class = il2cpp::object_class(blob);
            let mut list = std::ptr::null_mut();
            for g in LIST_GETTERS {
                list = invoke0(blob, blob_class, g);
                if !list.is_null() {
                    break;
                }
            }
            if list.is_null() {
                return Err(format!(
                    "follower list not readable on {} (open the follower list in-game first; if it persists run Scan)",
                    il2cpp::class_full_name(blob_class)
                ));
            }
            log(&format!("read: blob via {blob_getter}, list class {}", il2cpp::object_class_name(list)));
            // 3) walk List<T>: _items @0x10 (T[]), _size @0x18 ; array data @0x20, 8-byte refs
            let items = rd_ptr(list, 0x10);
            let size = rd_i32(list, 0x18);
            if items.is_null() || size < 0 {
                return Err("follower list layout unexpected (run Scan and send the log)".into());
            }
            let mut out = Vec::with_capacity(size as usize);
            for i in 0..size as usize {
                let e = rd_ptr(items, 0x20 + i * 8);
                if e.is_null() {
                    continue;
                }
                if let Some(f) = read_entry(e) {
                    out.push(f);
                }
            }
            if out.is_empty() && size > 0 {
                return Err("entries did not decode (run Scan and send the log)".into());
            }
            Ok(out)
        }
    }

    /// Decode one follower entry (WorkFriendData.FriendData) via its own getters — resolved
    /// from the live instance class, so the nested type works without class_from_name. All
    /// values are Obscured types: invoke0 boxes them, unbox_i64/plain_string decode them.
    unsafe fn read_entry(e: *mut c_void) -> Option<Follower> {
        let k = il2cpp::object_class(e);
        if k.is_null() {
            return None;
        }
        let vid = unbox_i64(invoke0(e, k, "get_ViewerId"))?;
        if vid == 0 {
            return None;
        }
        let name = plain_string(invoke0(e, k, "get_Name"));
        // Last login as a unix time (ObscuredLong) → days ago for the sort/display.
        let last_login_days = unbox_i64(invoke0(e, k, "get_LastLoginUnixTime")).and_then(|unix| {
            if unix <= 0 {
                return None;
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            Some(((now - unix).max(0) / 86_400) as i32)
        });
        Some(Follower { viewer_id: vid, name, last_login_days })
    }

    /// Remove one follower by sending the same request the game's own remove button sends.
    /// MAIN THREAD ONLY.
    pub fn remove_follower(viewer_id: i64) -> Result<(), String> {
        if !il2cpp::ready() {
            return Err("IL2CPP runtime not ready".into());
        }
        unsafe {
            let mut req_class = std::ptr::null_mut();
            let mut req_name = "";
            for n in REMOVE_REQUESTS {
                let k = il2cpp::class(n);
                if !k.is_null() {
                    req_class = k;
                    req_name = n;
                    break;
                }
            }
            if req_class.is_null() {
                return Err("remove-request class not found in this build (run Scan and send the log)".into());
            }
            let req = il2cpp::object_new(req_class);
            if req.is_null() {
                return Err(format!("could not allocate {req_name}"));
            }
            // Set the TARGET follower's viewer id. Field order matters — see VIEWER_ID_FIELDS.
            let off = VIEWER_ID_FIELDS.iter().find_map(|f| il2cpp::field_offset(req_class, f));
            let Some(off) = off else {
                return Err(format!("target viewer-id field not found on {req_name} (run Scan and send the log)"));
            };
            *((req as usize + off) as *mut i64) = viewer_id;
            // Send is inherited from RequestBase`1 and takes 7 args:
            //   (onSuccess: Action<Resp>, onError: Action<...>, 5x bool flags).
            // We pass null callbacks (fire-and-forget) and all bools false — the flags drive UI
            // side-effects (loading spinner / error dialog / caching); false = silent send, which
            // is what we want for an unattended paced loop. runtime_invoke boxes value-type args
            // (bools) as pointers-to-value and captures any managed exception (returns instead of
            // unwinding through native code).
            let m = il2cpp::method(req_class, "Send", 7);
            if m.is_null() {
                return Err(format!("{req_name}.Send(7 args) not found (run Scan and send the log)"));
            }
            let mut flags: [u8; 5] = [0; 5];
            let mut args: [*mut c_void; 7] = [
                std::ptr::null_mut(), // onSuccess
                std::ptr::null_mut(), // onError
                (&mut flags[0]) as *mut u8 as *mut c_void,
                (&mut flags[1]) as *mut u8 as *mut c_void,
                (&mut flags[2]) as *mut u8 as *mut c_void,
                (&mut flags[3]) as *mut u8 as *mut c_void,
                (&mut flags[4]) as *mut u8 as *mut c_void,
            ];
            il2cpp::runtime_invoke(m, req, &mut args);
            Ok(())
        }
    }

    /// One class's methods (with param types) + fields (name/offset/type) into `out`.
    pub(crate) fn dump_class(out: &mut String, full: &str, k: il2cpp::Class) {
        out.push_str(&format!("== {full}\n"));
        // parent chain, so inherited entry points (e.g. a RequestBase Send) are traceable
        let mut p = il2cpp::class_parent(k);
        if !p.is_null() {
            let mut chain = Vec::new();
            while !p.is_null() {
                let n = il2cpp::class_full_name(p);
                if n == "System.Object" {
                    break;
                }
                chain.push(n);
                p = il2cpp::class_parent(p);
            }
            if !chain.is_empty() {
                out.push_str(&format!("   : {}\n", chain.join(" : ")));
            }
        }
        for (name, off, ty) in il2cpp::class_fields(k) {
            out.push_str(&format!("   .{name} : {ty} @0x{off:X}\n"));
        }
        for m in il2cpp::class_methods(k) {
            if let Some((name, _)) = m.split_once('/') {
                let params = il2cpp::method_param_types(k, name);
                if params.is_empty() {
                    out.push_str(&format!("   {m}\n"));
                } else {
                    for p in params {
                        out.push_str(&format!("   {name}({p})\n"));
                    }
                }
            }
        }
        out.push('\n');
    }

    /// Dump every loaded class whose name contains "friend" or "follow" — methods, FIELDS
    /// (name/offset/type) and parent chains, plus a section for each distinct PARENT class
    /// (that's where inherited Send entry points live) — to
    /// `trackside-logs/trackside-follower-scan.txt`. MAIN THREAD ONLY.
    pub fn scan_dump() -> String {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return "Scan failed: IL2CPP runtime not ready".into();
        }
        let mut hits = il2cpp::find_classes("friend");
        hits.extend(il2cpp::find_classes("follow"));
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        hits.dedup_by(|a, b| a.0 == b.0);
        if hits.is_empty() {
            return "Scan found nothing (class enumeration unavailable in this runtime?)".into();
        }
        let mut out = String::new();
        out.push_str("Trackside follower-pruner class scan (v2: + fields, parent chains, parent sections)\n");
        out.push_str("(send this file to pin the follower list / remove-request names)\n\n");
        for (full, k) in &hits {
            dump_class(&mut out, full, *k);
        }
        // Parent classes of the Gallop hits, deduped and not already dumped — the request/task
        // base classes carry the real network entry points.
        let dumped: std::collections::HashSet<String> = hits.iter().map(|(n, _)| n.clone()).collect();
        let mut parents: Vec<(String, il2cpp::Class)> = Vec::new();
        for (full, k) in &hits {
            if !full.starts_with("Gallop.") {
                continue;
            }
            let mut p = il2cpp::class_parent(*k);
            while !p.is_null() {
                let n = il2cpp::class_full_name(p);
                if n == "System.Object" || n.is_empty() {
                    break;
                }
                if !dumped.contains(&n) && !parents.iter().any(|(pn, _)| pn == &n) {
                    parents.push((n.clone(), p));
                }
                p = il2cpp::class_parent(p);
            }
        }
        if !parents.is_empty() {
            out.push_str("──── parent classes (inherited entry points) ────\n\n");
            parents.sort_by(|a, b| a.0.cmp(&b.0));
            for (full, k) in &parents {
                dump_class(&mut out, full, *k);
            }
        }
        let path = crate::paths::log_file("trackside-follower-scan.txt");
        let n_par = parents.len();
        match std::fs::write(&path, out) {
            Ok(_) => format!("Scan: {} classes + {n_par} parents -> {}", hits.len(), path.display()),
            Err(e) => format!("Scan write failed: {e}"),
        }
    }
}
