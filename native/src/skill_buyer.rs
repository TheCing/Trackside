//! Apply Optimal + live skill-list capture for the end-of-career skill-learn screen.
//!
//! Two jobs:
//!   1. Capture the game's ACTUAL offered-skill list (via
//!      `SingleModeSkillLearningViewController._itemList` → each item's `GetSkillId()`),
//!      so the advisor recommends from ground truth instead of a static reconstruction.
//!      The reconstruction structurally can't see inherited (green) skills, the unique,
//!      or unhinted whites — the live list has all of them.
//!   2. Apply Optimal: click the recommended skills' + buttons; player confirms with the
//!      game's own Decide. (Selection driver still pending — see `driver_ready`.)
//!
//! Class layout is from the in-game scan (trackside-skill-learn-scan.txt):
//!   Gallop.SingleModeSkillLearningViewController
//!       [0x48] _itemList: List<PartsSingleModeSkillLearningListItem>
//!       [0x58] <RemainingPoint>k__BackingField: Int32
//!   Gallop.PartsSingleModeSkillLearningListItem
//!       fn GetSkillId()/0 -> Int32
//!       [0x68] _needPoint: Gallop.TextCommon   (displayed SP cost)
//!       [0xa8] _hintLvText: Gallop.TextCommon
//!   List<T>: _items @0x10 (T[]), _size @0x18; array data @0x20 (8-byte refs)
//!
//! Threading: all IL2CPP reads happen in `pump()` / the Setup detour, i.e. the game main
//! thread (via ui_tempo's single TweenManager.Update detour). UI sets atomic flags; the
//! advisor worker only reads the plain snapshot Vec.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::il2cpp;
use crate::tt_il2cpp::{rd_i32, rd_ptr};

const VIEW_CLASS_NAME: &str = "Gallop.SingleModeSkillLearningViewController";
const ITEM_CLASS_NAME: &str = "Gallop.PartsSingleModeSkillLearningListItem";
const OFF_ITEM_LIST: usize = 0x48;
const OFF_NEED_POINT: usize = 0x68;
const OFF_HINT_TEXT: usize = 0xa8;

// Live view instance (captured in the Setup detour, cleared on PlayOutView).
static INSTANCE: AtomicUsize = AtomicUsize::new(0);
static SETUP_TRAMP: AtomicUsize = AtomicUsize::new(0);
static OUT_TRAMP: AtomicUsize = AtomicUsize::new(0);
static SETUP_DETOUR: OnceLock<retour::RawDetour> = OnceLock::new();
static OUT_DETOUR: OnceLock<retour::RawDetour> = OnceLock::new();

// Snapshot of the skill_ids the game is currently offering (main-thread producer, worker
// consumer). Empty when not on the screen.
static OFFERED: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();

static SCAN_REQUESTED: AtomicBool = AtomicBool::new(false);
static APPLY_REQUESTED: AtomicBool = AtomicBool::new(false);
static PENDING: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();
static STATUS: OnceLock<Mutex<String>> = OnceLock::new();

fn offered() -> &'static Mutex<Vec<i32>> {
    OFFERED.get_or_init(|| Mutex::new(Vec::new()))
}
fn pending() -> &'static Mutex<Vec<i32>> {
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}
fn status_slot() -> &'static Mutex<String> {
    STATUS.get_or_init(|| Mutex::new(String::new()))
}
fn set_status(s: impl Into<String>) {
    if let Ok(mut g) = status_slot().lock() {
        *g = s.into();
    }
}
pub fn status() -> String {
    status_slot().lock().map(|s| s.clone()).unwrap_or_default()
}

/// Skill ids the game is currently offering on the learn screen (empty when off-screen or
/// not yet captured). The advisor uses this as its candidate gate when non-empty.
pub fn offered_skill_ids() -> Vec<i32> {
    offered().lock().map(|g| g.clone()).unwrap_or_default()
}

// Live SP-remaining snapshot (RemainingPoint @0x58), set each frame in pump(). i32::MIN =
// not on the screen. Reactive: shrinks as the player clicks +, grows on −.
static REMAINING: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(i32::MIN);
static BUDGET_SP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// SP the player has committed on the live screen right now (budget − remaining). None when
/// off-screen → the window falls back to the recommendation's own `spent`.
pub fn live_spent() -> Option<i32> {
    let rem = REMAINING.load(Ordering::Relaxed);
    if rem == i32::MIN {
        return None;
    }
    let budget = BUDGET_SP.load(Ordering::Relaxed);
    Some((budget - rem).max(0))
}

/// The player's rating RIGHT NOW as they hand-pick skills: baseline (captured chara) plus
/// the grade of everything currently marked for purchase on the live screen. None off-screen.
pub fn live_current_rating() -> Option<i32> {
    let ids = live_selected_ids();
    if ids.is_empty() && REMAINING.load(Ordering::Relaxed) == i32::MIN {
        return None;
    }
    Some(crate::skill_advisor::rating_with_pending(&ids))
}

/// Skill ids currently marked-for-purchase on the live screen (selected level > 0). Read
/// each frame in pump(); empty when off-screen.
pub fn live_selected_ids() -> Vec<i32> {
    SELECTED.get_or_init(|| Mutex::new(Vec::new())).lock().map(|g| g.clone()).unwrap_or_default()
}
static SELECTED: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();

/// True when we're live on the skill screen with a captured item list.
pub fn on_learn_screen() -> bool {
    INSTANCE.load(Ordering::Relaxed) != 0 && !offered_skill_ids().is_empty()
}

// ── live selection tracking (drives the reactive rating) ─────────────────────
// The nested SkillInfo layout is unknown, so instead of reading selection state we COUNT
// the clicks: every + / − goes through OnClickPlusListItem / OnClickMinusListItem (the
// player's taps AND our Apply driver, which calls the same detoured method). ClearSelected
// covers the Reset button. Counts per item index translate to tier skill_ids via the
// chain tables, and that list feeds skill_advisor::rating_with_pending.

// Keyed by the LIST-ITEM object pointer (the method's actual argument), stable for the
// screen session and cleared on Setup/PlayOut/Reset.
static CLICKS: OnceLock<Mutex<std::collections::HashMap<usize, i32>>> = OnceLock::new();
static PLUS_TRAMP: AtomicUsize = AtomicUsize::new(0);
static MINUS_TRAMP: AtomicUsize = AtomicUsize::new(0);
static CLEARSEL_TRAMP: AtomicUsize = AtomicUsize::new(0);
static PLUS_DETOUR: OnceLock<retour::RawDetour> = OnceLock::new();
static MINUS_DETOUR: OnceLock<retour::RawDetour> = OnceLock::new();
static CLEARSEL_DETOUR: OnceLock<retour::RawDetour> = OnceLock::new();

fn clicks() -> &'static Mutex<std::collections::HashMap<usize, i32>> {
    CLICKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn clear_selection_state() {
    if let Ok(mut c) = clicks().lock() {
        c.clear();
    }
    if let Ok(mut s) = SELECTED.get_or_init(|| Mutex::new(Vec::new())).lock() {
        s.clear();
    }
}

/// Translate the per-item click counts into pending tier skill_ids. Main thread only —
/// only touches item pointers while the screen is live (cleared on PlayOutView).
unsafe fn rebuild_selected() {
    if live_instance().is_null() {
        return;
    }
    let counts: Vec<(usize, i32)> = clicks()
        .lock()
        .map(|c| c.iter().map(|(k, v)| (*k, *v)).filter(|(_, v)| *v > 0).collect())
        .unwrap_or_default();
    let item_k = il2cpp::class(ITEM_CLASS_NAME);
    let get_id = il2cpp::method(item_k, "GetSkillId", 0);
    let mut out = Vec::new();
    for (item_ptr, n) in counts {
        let sid = item_skill_id(item_ptr as *mut c_void, get_id);
        if sid > 0 {
            out.extend(crate::skill_advisor::tier_ids_for_clicks(sid, n));
        }
    }
    if let Ok(mut s) = SELECTED.get_or_init(|| Mutex::new(Vec::new())).lock() {
        *s = out;
    }
}

unsafe extern "C" fn on_click_plus(this: *mut c_void, item: *mut c_void, mi: *mut c_void) {
    let t = PLUS_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) = std::mem::transmute(t);
        orig(this, item, mi);
    }
    if !item.is_null() {
        if let Ok(mut c) = clicks().lock() {
            *c.entry(item as usize).or_insert(0) += 1;
        }
        rebuild_selected();
    }
}

unsafe extern "C" fn on_click_minus(this: *mut c_void, item: *mut c_void, mi: *mut c_void) {
    let t = MINUS_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) = std::mem::transmute(t);
        orig(this, item, mi);
    }
    if !item.is_null() {
        if let Ok(mut c) = clicks().lock() {
            let e = c.entry(item as usize).or_insert(0);
            *e = (*e - 1).max(0);
        }
        rebuild_selected();
    }
}

unsafe extern "C" fn on_clear_selected(this: *mut c_void, mi: *mut c_void) {
    let t = CLEARSEL_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(t);
        orig(this, mi);
    }
    clear_selection_state();
}

// ── instance capture ─────────────────────────────────────────────────────────

unsafe extern "C" fn on_setup(this: *mut c_void, mi: *mut c_void) {
    INSTANCE.store(this as usize, Ordering::Relaxed);
    clear_selection_state();
    crate::tools::debug("[skill_buyer] learn screen opened (Setup) — instance captured");
    let t = SETUP_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(t);
        orig(this, mi);
    }
}

unsafe extern "C" fn on_playout(this: *mut c_void, mi: *mut c_void) {
    INSTANCE.store(0, Ordering::Relaxed);
    if let Ok(mut g) = offered().lock() {
        g.clear();
    }
    clear_selection_state();
    let t = OUT_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(t);
        orig(this, mi);
    }
}

pub fn install() -> String {
    let k = il2cpp::class(VIEW_CLASS_NAME);
    if k.is_null() {
        return "view class not found (game update? re-scan)".into();
    }
    let mut ok = 0;
    // Per-hook outcome logging — a partial failure here means a silently degraded feature
    // (e.g. clicks not tracked), so name exactly which method didn't resolve.
    let mut hook = |name: &str, argc: i32, det: *const (), tr: &AtomicUsize, d: &OnceLock<retour::RawDetour>| {
        match unsafe { il2cpp::hook_method(k, name, argc, det, tr, d) } {
            Ok(()) => {
                ok += 1;
                crate::tools::debug(&format!("[skill_buyer] hooked {name}"));
            }
            Err(e) => crate::tools::warn(&format!("[skill_buyer] hook {name} FAILED: {e}")),
        }
    };
    hook("Setup", 0, on_setup as *const (), &SETUP_TRAMP, &SETUP_DETOUR);
    hook("PlayOutView", 0, on_playout as *const (), &OUT_TRAMP, &OUT_DETOUR);
    // Selection tracking: + / − / Reset all pass through these (player taps AND our Apply
    // driver) — they drive the reactive rating in the optimizer window.
    hook("OnClickPlusListItem", 1, on_click_plus as *const (), &PLUS_TRAMP, &PLUS_DETOUR);
    hook("OnClickMinusListItem", 1, on_click_minus as *const (), &MINUS_TRAMP, &MINUS_DETOUR);
    hook("ClearSelected", 0, on_clear_selected as *const (), &CLEARSEL_TRAMP, &CLEARSEL_DETOUR);
    format!("OK ({ok}/5 hooks; live capture + selection tracking)")
}

// ── live item-list read (main thread only) ───────────────────────────────────

/// Call `PartsSingleModeSkillLearningListItem.GetSkillId()` on one item.
unsafe fn item_skill_id(item: *mut c_void, get_skill_id: il2cpp::Method) -> i32 {
    if item.is_null() || get_skill_id.is_null() {
        return 0;
    }
    let p = il2cpp::method_pointer(get_skill_id);
    if p.is_null() {
        return 0;
    }
    let f: extern "C" fn(*mut c_void, *const c_void) -> i32 = std::mem::transmute(p);
    f(item, get_skill_id as *const c_void)
}

/// Walk the live `_itemList` and return the offered skill ids. Main thread only.
unsafe fn read_offered() -> Vec<i32> {
    let inst = live_instance();
    if inst.is_null() {
        return Vec::new();
    }
    let list = rd_ptr(inst, OFF_ITEM_LIST);
    if list.is_null() {
        return Vec::new();
    }
    let items = rd_ptr(list, 0x10); // T[]
    let size = rd_i32(list, 0x18);
    if items.is_null() || size <= 0 || size > 4096 {
        return Vec::new();
    }
    let item_k = il2cpp::class(ITEM_CLASS_NAME);
    let get_id = il2cpp::method(item_k, "GetSkillId", 0);
    let mut out = Vec::with_capacity(size as usize);
    for i in 0..size as usize {
        // IL2CPP array payload starts at 0x20, 8-byte reference slots.
        let item = *((items as usize + 0x20 + i * 8) as *const *mut c_void);
        let sid = item_skill_id(item, get_id);
        if sid > 0 {
            out.push(sid);
        }
    }
    out
}

// ── pump / scan / apply ──────────────────────────────────────────────────────

pub fn pump() {
    // Refresh the offered snapshot each frame we're on the screen (cheap: ~50 int calls).
    // live_instance() validates INSTANCE is still a live learn-screen object before ANY deref,
    // so a stale/freed pointer can't crash the pump (the 0xf4 use-after-free).
    let inst = unsafe { live_instance() };
    if !inst.is_null() {
        let mut ids = unsafe { read_offered() };
        if !ids.is_empty() {
            // Sorted set compare: the game rebinds/reorders _itemList entries as you interact
            // with the shop, and an ORDER-sensitive compare here kept "detecting changes" and
            // stomping the recommendation right after every Recommend (the un-rerunnable bug).
            ids.sort_unstable();
            ids.dedup();
            let n = ids.len();
            let changed = offered()
                .lock()
                .map(|mut g| {
                    if *g != ids {
                        *g = ids;
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            if changed {
                crate::tools::debug(&format!("[skill_buyer] offered list changed -> {n} skills; recomputing"));
                // Real set change (first capture / skill bought) → recompute seamlessly
                // instead of leaving a cleared result behind.
                crate::skill_advisor::request_recommend();
            }
        }
        // Live reactive snapshot: RemainingPoint (SP left) drives the SP-spent bar in real
        // time as the player clicks + / −. Budget = remaining at first sight (nothing picked
        // yet is the common case; if they've already spent, it self-corrects upward on −).
        let rem = unsafe { rd_i32(inst, 0x58) };
        REMAINING.store(rem, Ordering::Relaxed);
        let prev_budget = BUDGET_SP.load(Ordering::Relaxed);
        if rem > prev_budget {
            BUDGET_SP.store(rem, Ordering::Relaxed);
        }
    } else {
        REMAINING.store(i32::MIN, Ordering::Relaxed);
    }
    if SCAN_REQUESTED.swap(false, Ordering::Relaxed) {
        unsafe { run_scan() };
    }
    if APPLY_REQUESTED.swap(false, Ordering::Relaxed) {
        run_apply();
    }
    // Apply driver runs as a paced state machine across frames (never a burst).
    drive_apply();
}

pub fn request_apply(skill_ids: Vec<i32>) {
    if let Ok(mut g) = pending().lock() {
        *g = skill_ids;
    }
    APPLY_REQUESTED.store(true, Ordering::Relaxed);
}

pub fn request_scan() {
    SCAN_REQUESTED.store(true, Ordering::Relaxed);
    set_status("Scanning… (log lands in trackside-logs)");
}

/// True once the Apply SELECTION DRIVER can run: IL2CPP up, view class + click method
/// resolved. The `ready()` guard keeps this safe in the preview host (no game runtime).
pub fn driver_ready() -> bool {
    if !il2cpp::ready() {
        return false;
    }
    let k = il2cpp::class(VIEW_CLASS_NAME);
    !k.is_null() && !il2cpp::method(k, "OnClickPlusListItem", 1).is_null()
}

// Apply driver state: a queue of (item_index) clicks, fired one per N frames so the game's
// UI updates (and RemainingPoint decrements) between clicks — a burst would race the game.
static APPLY_QUEUE: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();
static APPLY_COOLDOWN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
const APPLY_GAP_FRAMES: i32 = 6; // ~100ms at 60fps — smooth, game keeps up

fn run_apply() {
    let ids = pending().lock().map(|g| g.clone()).unwrap_or_default();
    crate::tools::debug(&format!("[skill_buyer] Apply Optimal: {} recommended tier ids", ids.len()));
    if ids.is_empty() {
        set_status("Nothing to apply.");
        return;
    }
    if !driver_ready() {
        crate::tools::warn("[skill_buyer] Apply aborted — learn-screen controller not resolved");
        set_status("Learn-screen controller not resolved (game update? re-scan).");
        return;
    }
    // Map each recommended skill_id → its live list index, then queue one click per tier.
    // A chain skill (e.g. ○→◎) needs its + pressed once per tier bought, so `ids` already
    // contains every tier from the recommendation's chain; duplicates on the same index
    // become repeated clicks on that item (the ◎-via-○ double-press).
    let index_of = unsafe { build_index_map() };
    let mut queue: Vec<i32> = Vec::new();
    let mut hit = 0;
    for sid in &ids {
        if let Some(&idx) = index_of.get(sid) {
            queue.push(idx);
            hit += 1;
        } else {
            // A chain's lower tier shares its group's item — click that item again.
            if let Some(idx) = unsafe { group_index_for(sid, &index_of) } {
                queue.push(idx);
                hit += 1;
            }
        }
    }
    if queue.is_empty() {
        crate::tools::warn(&format!("[skill_buyer] Apply: none of {} recommended skills are on the live list", ids.len()));
        set_status("None of the recommended skills are on the live list right now.");
        return;
    }
    crate::tools::debug(&format!("[skill_buyer] Apply: queued {} clicks across item indices {:?}", queue.len(), queue));
    if let Ok(mut q) = APPLY_QUEUE.get_or_init(|| Mutex::new(Vec::new())).lock() {
        *q = queue;
    }
    APPLY_COOLDOWN.store(0, Ordering::Relaxed);
    set_status(format!("Applying {hit}\u{2026} then press Decide."));
}

/// Fire one queued + click if the cooldown elapsed. Main thread (pump).
fn drive_apply() {
    let cd = APPLY_COOLDOWN.fetch_sub(1, Ordering::Relaxed);
    if cd > 0 {
        return;
    }
    APPLY_COOLDOWN.store(APPLY_GAP_FRAMES, Ordering::Relaxed);
    let idx = {
        let Ok(mut q) = APPLY_QUEUE.get_or_init(|| Mutex::new(Vec::new())).lock() else { return };
        if q.is_empty() {
            return;
        }
        q.remove(0)
    };
    unsafe { click_plus(idx) };
    if APPLY_QUEUE.get_or_init(|| Mutex::new(Vec::new())).lock().map(|q| q.is_empty()).unwrap_or(true) {
        set_status("Applied — press the game's Decide to confirm.");
    }
}

/// skill_id → live `_itemList` index (via each item's GetSkillId).
unsafe fn build_index_map() -> std::collections::HashMap<i32, i32> {
    let mut out = std::collections::HashMap::new();
    let inst = INSTANCE.load(Ordering::Relaxed) as *mut c_void;
    if inst.is_null() {
        return out;
    }
    let list = rd_ptr(inst, OFF_ITEM_LIST);
    let items = rd_ptr(list, 0x10);
    let size = rd_i32(list, 0x18);
    if items.is_null() || size <= 0 || size > 4096 {
        return out;
    }
    let item_k = il2cpp::class(ITEM_CLASS_NAME);
    let get_id = il2cpp::method(item_k, "GetSkillId", 0);
    for i in 0..size as usize {
        let item = *((items as usize + 0x20 + i * 8) as *const *mut c_void);
        let sid = item_skill_id(item, get_id);
        if sid > 0 {
            out.entry(sid).or_insert(i as i32);
        }
    }
    out
}

/// For a chain lower-tier skill_id not directly listed, find the item index whose skill
/// shares its group (the game lists one upgradable item per chain group).
unsafe fn group_index_for(sid: &i32, index_of: &std::collections::HashMap<i32, i32>) -> Option<i32> {
    let gid = crate::skill_advisor::group_of(*sid)?;
    index_of
        .iter()
        .find(|(other, _)| crate::skill_advisor::group_of(**other) == Some(gid))
        .map(|(_, &idx)| idx)
}

/// INSTANCE, but only if it STILL points at a live `SingleModeSkillLearningViewController`.
///
/// The learn-screen controller is captured on Setup and meant to be cleared on PlayOutView,
/// but if that clear is ever missed (e.g. the screen tears down via a path we don't hook, or
/// a game update shifts the method), INSTANCE dangles. The per-frame pump then walks a freed
/// object graph and derefs a garbage list-item → the `0xc0000005` read @0xf4 use-after-free
/// crash. Guard: every IL2CPP object stores its `Il2CppClass*` at offset 0, so compare it to
/// the resolved view class; on mismatch the pointer is stale/reused → clear it and bail. Cheap
/// (one pointer read + compare) and runs before any deeper deref.
unsafe fn live_instance() -> *mut c_void {
    let inst = INSTANCE.load(Ordering::Relaxed) as *mut c_void;
    if inst.is_null() {
        return std::ptr::null_mut();
    }
    let k = il2cpp::class(VIEW_CLASS_NAME);
    if k.is_null() {
        return std::ptr::null_mut();
    }
    // klass pointer at offset 0 of the object; a live controller's matches the view class.
    if rd_ptr(inst, 0) != k as *mut c_void {
        INSTANCE.store(0, Ordering::Relaxed);
        clear_selection_state();
        return std::ptr::null_mut();
    }
    inst
}

/// Resolve the live `_itemList[index]` object pointer (or null if out of range).
unsafe fn item_at(index: i32) -> *mut c_void {
    let inst = live_instance();
    if inst.is_null() || index < 0 {
        return std::ptr::null_mut();
    }
    let list = rd_ptr(inst, OFF_ITEM_LIST);
    let items = rd_ptr(list, 0x10);
    let size = rd_i32(list, 0x18);
    if items.is_null() || index >= size {
        return std::ptr::null_mut();
    }
    *((items as usize + 0x20 + index as usize * 8) as *const *mut c_void)
}

/// Invoke `OnClickPlusListItem(item)` on the live controller — the exact path the game's own
/// + button takes. The parameter is the LIST-ITEM OBJECT, not an index (a v1 build passed the
/// raw index and the game deref'd it as a pointer → read @0xf4 off near-null → crash).
unsafe fn click_plus(index: i32) {
    let inst = INSTANCE.load(Ordering::Relaxed) as *mut c_void;
    if inst.is_null() {
        return;
    }
    let item = item_at(index);
    if item.is_null() {
        return; // out of range or list not ready — never pass a bad pointer to the game
    }
    let k = il2cpp::class(VIEW_CLASS_NAME);
    let m = il2cpp::method(k, "OnClickPlusListItem", 1);
    if m.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(*mut c_void, *mut c_void, *const c_void) = std::mem::transmute(p);
    f(inst, item, m as *const c_void);
}

/// Read a `Gallop.TextCommon.get_text()` at `base + off` (best-effort; "" on any miss).
unsafe fn read_text_field(base: *mut c_void, off: usize) -> String {
    let tc = rd_ptr(base, off);
    if tc.is_null() {
        return String::new();
    }
    let k = il2cpp::class("Gallop.TextCommon");
    for getter in ["get_text", "get_Text"] {
        let m = il2cpp::method(k, getter, 0);
        if m.is_null() {
            continue;
        }
        let p = il2cpp::method_pointer(m);
        if p.is_null() {
            continue;
        }
        let f: extern "C" fn(*mut c_void, *const c_void) -> *mut c_void = std::mem::transmute(p);
        let s = f(tc, m as *const c_void);
        if !s.is_null() {
            return il2cpp::read_string(s);
        }
    }
    String::new()
}

/// Dump: (a) the LIVE offered items with real skill_id + cost + hint text, and (b) the
/// learn-screen + icon-utility class members (incl. nested types). Everything needed to
/// finish the selection driver and validate the live-list offsets.
unsafe fn run_scan() {
    const LOG: &str = "trackside-skill-learn-scan.txt";
    let mut out = String::new();
    // Format marker FIRST — instantly distinguishes which build wrote the log (an old
    // in-memory DLL kept producing v1 logs after a newer file was deployed underneath it).
    out.push_str(&format!(
        "==== TRACKSIDE SKILL SCAN v2 (live capture) — overlay v{} ====\n\n",
        env!("CARGO_PKG_VERSION")
    ));

    out.push_str("==== LIVE OFFERED ITEMS (this capture) ====\n");
    let inst = INSTANCE.load(Ordering::Relaxed) as *mut c_void;
    if inst.is_null() {
        out.push_str("(no live view captured — open the skill-learn screen, then Scan)\n");
    } else {
        let remaining = rd_i32(inst, 0x58);
        out.push_str(&format!("RemainingPoint = {remaining}\n"));
        let list = rd_ptr(inst, OFF_ITEM_LIST);
        let items = rd_ptr(list, 0x10);
        let size = rd_i32(list, 0x18);
        out.push_str(&format!("item count = {size}\n"));
        let item_k = il2cpp::class(ITEM_CLASS_NAME);
        let get_id = il2cpp::method(item_k, "GetSkillId", 0);
        for i in 0..size.clamp(0, 4096) as usize {
            let item = *((items as usize + 0x20 + i * 8) as *const *mut c_void);
            let sid = item_skill_id(item, get_id);
            let cost = read_text_field(item, OFF_NEED_POINT);
            let hint = read_text_field(item, OFF_HINT_TEXT);
            out.push_str(&format!("  [{i:3}] skill_id={sid:>7}  cost='{cost}'  hint='{hint}'\n"));
        }
    }

    out.push_str("\n==== CLASS MEMBERS ====\n");
    for pat in ["SkillLearning", "SkillLearn", "GainSkillInfo", "MasterSkillData"] {
        for (full, k) in il2cpp::find_classes(pat) {
            out.push_str(&format!("\n-- {full}\n"));
            if k.is_null() {
                continue;
            }
            for m in il2cpp::class_methods(k) {
                out.push_str(&format!("   fn {m}\n"));
            }
            for (name, off, ty) in il2cpp::class_fields(k) {
                out.push_str(&format!("   [{off:#x}] {name}: {ty}\n"));
            }
        }
    }
    crate::tools::log_to(LOG, &out);
    let n = offered_skill_ids().len();
    set_status(format!("Scan written ({n} live skills). trackside-logs/{LOG}"));
}
