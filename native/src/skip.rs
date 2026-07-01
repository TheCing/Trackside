//! Heaven Plan B — B3: native SuperSkip (port of core/modules/native_skip.js).
//!
//! This pass covers the two "bread and butter" skips:
//!   1) TRAINING cut-in  → SingleModeTrainingCutInHelper.SkipRuntime()
//!   2) EVENTS/recreation/Rest → StoryViewController.SkipStory() (guarded by
//!      trainCutt / TimelineController.IsPlaying + a 1200ms debounce)
//! Race-result auto-advance (part 3) lands in a follow-up (skip part 3).
//!
//! Every method we INVOKE is called via its compiled methodPointer with the
//! trailing hidden MethodInfo* arg. Every method we HOOK guards against logical
//! recursion with hooks::ReentryGuard / in_heaven() — the native equivalent of
//! the JS busy flags, and the thing that makes this safe where Frida crashed.

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use retour::RawDetour;

use crate::hooks::{in_heaven, ReentryGuard};
use crate::il2cpp;

// Debounce only swallows a same-timeline re-fire of OnStartPlayingTimeline (a few frames). It must
// stay SHORT because it's WALL-clock, not game time — at high UI speed (e.g. 10x) distinct events
// compress into a small real-time window, and a long debounce was dropping the 2nd/3rd event ("some
// texts don't skip"). The IS_PLAYING check below is the real guard against re-skipping a finished one.
const EVENT_DEBOUNCE_MS: u64 = 100;

// ── enable flags — SuperSkip is split into Training and Events so the overlay
//    can toggle each leg independently (Races = race-result auto, further down).
static SKIP_ENABLED: AtomicBool = AtomicBool::new(true); // TRAINING cut-ins
static EVENT_ENABLED: AtomicBool = AtomicBool::new(true); // EVENT / story timelines
static SHOP_ENABLED: AtomicBool = AtomicBool::new(true); // PRO SHOP buy/use animations
static RIVAL_ENABLED: AtomicBool = AtomicBool::new(true); // rival-race entry "RIVAL <name>" intro

// Training
pub fn set_enabled(on: bool) {
    SKIP_ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_enabled() -> bool {
    SKIP_ENABLED.load(Ordering::Relaxed)
}
pub fn set_train_enabled(on: bool) {
    SKIP_ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_train_enabled() -> bool {
    SKIP_ENABLED.load(Ordering::Relaxed)
}
// Events
pub fn set_event_enabled(on: bool) {
    EVENT_ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_event_enabled() -> bool {
    EVENT_ENABLED.load(Ordering::Relaxed)
}
// Pro Shop (buy/use performance animations)
pub fn set_shop_enabled(on: bool) {
    SHOP_ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_shop_enabled() -> bool {
    SHOP_ENABLED.load(Ordering::Relaxed)
}
// Rival-race entry intro ("RIVAL <name>" splash before a rival race)
pub fn set_rival_enabled(on: bool) {
    RIVAL_ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_rival_enabled() -> bool {
    RIVAL_ENABLED.load(Ordering::Relaxed)
}

/// One-line snapshot of the skip subsystem for the diagnostics report — enable flags, the live gate
/// flags, and the run counters. If a skip "doesn't work", this shows whether it's disabled, gated
/// (stuck in team-trials / window state), or never firing (counter at 0 ⇒ the hook isn't being hit,
/// usually because another mod detoured the method first — see the install results in the report).
pub fn diag() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    format!(
        "enabled: train={} event={} shop={} rival={} race_result={}\n  \
         gates: in_team_trials={} window_open={} busy={} driving={}\n  \
         counts: train_skips={} event_skips={} rr_presses={}",
        SKIP_ENABLED.load(Relaxed),
        EVENT_ENABLED.load(Relaxed),
        SHOP_ENABLED.load(Relaxed),
        RIVAL_ENABLED.load(Relaxed),
        RACE_RESULT_ENABLED.load(Relaxed),
        IN_TEAM_TRIALS.load(Relaxed),
        WINDOW_OPEN.load(Relaxed),
        BUSY.load(Relaxed),
        DRIVING.load(Relaxed),
        TRAIN_SKIPS.load(Relaxed),
        EVENT_SKIPS.load(Relaxed),
        RR_PRESSES.load(Relaxed),
    )
}

// ── method ABIs (this, MethodInfo*) ─────────────────────────────────────────
type VoidM = unsafe extern "C" fn(*mut c_void, *mut c_void);
type PtrM = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type BoolM = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;

/// A resolved invokable method: (compiled code ptr, MethodInfo*).
#[derive(Clone, Copy)]
struct Invokable {
    code: usize,
    mi: usize,
}
impl Invokable {
    const NONE: Invokable = Invokable { code: 0, mi: 0 };
    fn ok(&self) -> bool {
        self.code != 0
    }
    unsafe fn call_void(&self, this: *mut c_void) {
        if self.code != 0 {
            let f: VoidM = std::mem::transmute(self.code);
            f(this, self.mi as *mut c_void);
        }
    }
    unsafe fn call_ptr(&self, this: *mut c_void) -> *mut c_void {
        if self.code == 0 {
            return std::ptr::null_mut();
        }
        let f: PtrM = std::mem::transmute(self.code);
        f(this, self.mi as *mut c_void)
    }
    unsafe fn call_bool(&self, this: *mut c_void) -> bool {
        if self.code == 0 {
            return false;
        }
        let f: BoolM = std::mem::transmute(self.code);
        f(this, self.mi as *mut c_void)
    }
    /// Call a STATIC 0-arg method returning a pointer: ABI is `f(MethodInfo*)`.
    unsafe fn call_ptr_static(&self) -> *mut c_void {
        if self.code == 0 {
            return std::ptr::null_mut();
        }
        let f: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(self.code);
        f(self.mi as *mut c_void)
    }
    /// SteamInputBlockManager.PlayClose(Action onComplete, bool immediate): lift the input
    /// block. null completion callback, `flag` controls immediate vs animated.
    unsafe fn call_play_close(&self, this: *mut c_void, flag: bool) {
        if self.code == 0 {
            return;
        }
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void, bool, *mut c_void) = std::mem::transmute(self.code);
        f(this, std::ptr::null_mut(), flag, self.mi as *mut c_void);
    }
}

fn resolve(klass: il2cpp::Class, name: &str, argc: i32) -> Invokable {
    resolve_m(il2cpp::method(klass, name, argc))
}

fn resolve_m(m: il2cpp::Method) -> Invokable {
    if m.is_null() {
        return Invokable::NONE;
    }
    let code = il2cpp::method_pointer(m) as usize;
    Invokable { code, mi: m as usize }
}

// Invokables (set in install).
static SKIP_RUNTIME: OnceLock<Invokable> = OnceLock::new(); // training
static SKIP_STORY: OnceLock<Invokable> = OnceLock::new(); // events
static GET_TL: OnceLock<Invokable> = OnceLock::new(); // StoryViewController.get_TimelineController
static IS_PLAYING: OnceLock<Invokable> = OnceLock::new(); // StoryTimelineController.get_IsPlaying
static TRAIN_CUTT: OnceLock<Invokable> = OnceLock::new(); // get_IsPlayingOrWillPlayTrainingCutt

// Stats.
static TRAIN_SKIPS: AtomicU64 = AtomicU64::new(0);
static EVENT_SKIPS: AtomicU64 = AtomicU64::new(0);
pub fn stats() -> (u64, u64) {
    (TRAIN_SKIPS.load(Ordering::Relaxed), EVENT_SKIPS.load(Ordering::Relaxed))
}

fn clock() -> &'static Instant {
    static CLOCK: OnceLock<Instant> = OnceLock::new();
    CLOCK.get_or_init(Instant::now)
}

// ── trampolines (keep detours alive + call originals) ───────────────────────
macro_rules! hook_slot {
    ($tramp:ident, $det:ident) => {
        static $tramp: AtomicUsize = AtomicUsize::new(0);
        static $det: OnceLock<RawDetour> = OnceLock::new();
    };
}
hook_slot!(TR_START, D_START);
hook_slot!(TR_PLAY, D_PLAY);
hook_slot!(TR_MAIN, D_MAIN);
hook_slot!(TR_TIMELINE, D_TIMELINE);
hook_slot!(TR_TAGIN, D_TAGIN); // SingleModeMainViewTagTrainingCutInPlayer.PlayCutIn
hook_slot!(TR_TAGOUT, D_TAGOUT); // .PlayCutInOut
hook_slot!(TR_PHOTO_PLAY, D_PHOTO_PLAY); // PhotoStudioCuttController.PlayCutIn
hook_slot!(TR_PHOTO_ASYNC, D_PHOTO_ASYNC); // .PlayCutInAsync
hook_slot!(TR_PHOTO_END, D_PHOTO_END); // .OnEndCutIn
// True while the Photo Studio is replaying a cut. It reuses SingleModeTrainingCutInHelper
// (PhotoStudioCuttController._cutInHelperList@0x18), so those helpers fire our OnPlayCutIn
// hook — without this flag the training-skip would swallow the photo-studio animation too.
static PHOTO_CUT_ACTIVE: AtomicBool = AtomicBool::new(false);

#[inline]
unsafe fn call_orig(tramp: &AtomicUsize, this: *mut c_void, method: *mut c_void) {
    let t = tramp.load(Ordering::Relaxed);
    if t != 0 {
        let f: VoidM = std::mem::transmute(t);
        f(this, method);
    }
}

// ── TRAINING: run SkipRuntime after a cut-in start. ─────────────────────────
fn do_training_skip(this: *mut c_void) {
    if !is_enabled() || in_heaven() || this.is_null() {
        return;
    }
    if PHOTO_CUT_ACTIVE.load(Ordering::Relaxed) {
        return; // Photo Studio cut recreation — must play normally, never skip it
    }
    if let Some(sr) = SKIP_RUNTIME.get() {
        if sr.ok() {
            let _g = ReentryGuard::enter();
            unsafe { sr.call_void(this) };
            TRAIN_SKIPS.fetch_add(1, Ordering::Relaxed);
        }
    }
}
unsafe extern "C" fn on_start_cutin(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_START, this, m);
    do_training_skip(this);
}
unsafe extern "C" fn on_play_cutin(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_PLAY, this, m);
    do_training_skip(this);
}
unsafe extern "C" fn on_play_main_cutin(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_MAIN, this, m);
    do_training_skip(this);
}

// ── PHOTO STUDIO: pause the training-skip while a recreated cut plays ────────
// PhotoStudioCuttController replays training cut-ins through the SAME
// SingleModeTrainingCutInHelper instances (its _cutInHelperList@0x18), so those
// helpers fire on_play_cutin above and SkipRuntime() would skip the photo cut too.
// We flag the play window (both the sync PlayCutIn and the async coroutine entry,
// whichever the view controller uses) and clear it on OnEndCutIn.
type Photo3Fn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void);
type Photo1RetFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;

unsafe extern "C" fn on_photo_play_cut(
    this: *mut c_void,
    model: *mut c_void,
    on_end: *mut c_void,
    on_clean: *mut c_void,
    m: *mut c_void,
) {
    PHOTO_CUT_ACTIVE.store(true, Ordering::Relaxed);
    rr_log("[photo] cut start -> training-skip paused");
    let t = TR_PHOTO_PLAY.load(Ordering::Relaxed);
    if t != 0 {
        let f: Photo3Fn = std::mem::transmute(t);
        f(this, model, on_end, on_clean, m);
    }
}
unsafe extern "C" fn on_photo_play_cut_async(
    this: *mut c_void,
    model: *mut c_void,
    m: *mut c_void,
) -> *mut c_void {
    PHOTO_CUT_ACTIVE.store(true, Ordering::Relaxed);
    let t = TR_PHOTO_ASYNC.load(Ordering::Relaxed);
    if t != 0 {
        let f: Photo1RetFn = std::mem::transmute(t);
        return f(this, model, m);
    }
    std::ptr::null_mut()
}
unsafe extern "C" fn on_photo_end_cut(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_PHOTO_END, this, m);
    PHOTO_CUT_ACTIVE.store(false, Ordering::Relaxed);
    rr_log("[photo] cut end -> training-skip resumed");
}

// ── EVENTS: SkipStory on OnStartPlayingTimeline (guarded + debounced). ──────
static LAST_EVENT_SKIP_MS: AtomicU64 = AtomicU64::new(0);
fn try_event_skip(this: *mut c_void) {
    if !is_event_enabled() || in_heaven() || this.is_null() {
        return;
    }
    let now = clock().elapsed().as_millis() as u64;
    let dt = now.wrapping_sub(LAST_EVENT_SKIP_MS.load(Ordering::Relaxed));
    // Guard the whole critical section: any re-entry into our hooks passes thru.
    let _g = ReentryGuard::enter();
    unsafe {
        // Don't skip while a training cut-in is (or will be) playing.
        if let Some(tc) = TRAIN_CUTT.get() {
            if tc.ok() && tc.call_bool(this) {
                rr_log("[event] ignored (training cut-in playing)");
                return;
            }
        }
        // Resolve the playing timeline (its pointer is the dedup key we log, so we can tell a
        // genuine distinct event from a same-timeline re-fire).
        let mut tl_key = 0usize;
        if let (Some(gtl), Some(isp)) = (GET_TL.get(), IS_PLAYING.get()) {
            if gtl.ok() && isp.ok() {
                let tl = gtl.call_ptr(this);
                if tl.is_null() || !isp.call_bool(tl) {
                    rr_log("[event] ignored (no timeline / not playing)");
                    return;
                }
                tl_key = tl as usize;
            }
        }
        // Short re-fire debounce: swallow OnStartPlayingTimeline firing twice for the SAME timeline
        // start (a few render frames). MUST stay short — it's wall-clock, so a long window dropped
        // distinct back-to-back events at high UI speed (the "some texts don't skip" report).
        if dt < EVENT_DEBOUNCE_MS {
            rr_log(&format!("[event] ignored (debounce {dt}ms) tl={tl_key:#x}"));
            return;
        }
        if let Some(ss) = SKIP_STORY.get() {
            if ss.ok() {
                ss.call_void(this);
                LAST_EVENT_SKIP_MS.store(now, Ordering::Relaxed);
                EVENT_SKIPS.fetch_add(1, Ordering::Relaxed);
                rr_log(&format!("[event] SkipStory() tl={tl_key:#x} dt={dt}ms"));
            }
        }
    }
}
unsafe extern "C" fn on_start_timeline(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_TIMELINE, this, m);
    try_event_skip(this);
}

// ── TAG (friendship/rainbow) TRAINING cut-in splash ─────────────────────────
// The "FRIENDSHIP TRAINING!" splash is SingleModeMainViewTagTrainingCutInPlayer.
// PlayCutIn(List<SupportCardData>, Action onDone). We skip the ~1.5s animation by
// firing the onDone callback immediately (so the turn proceeds with no splash).
// Gated by the training-skip toggle.
type TagInFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void);
type TagOutFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);

// Strategy: let the ORIGINAL PlayCutIn run (so its setup + the later PlayCutInOut work,
// no freeze) but fire its onDone callback EARLY (deferred to the next frame, on a clean
// stack) so the flow advances immediately instead of waiting ~1.1s for the splash. The
// original's own later onDone re-fires — assumed idempotent (it just unblocks the
// execute coroutine). Net: friendship training skips ~as fast as a normal one.
static ACTION_INVOKE: OnceLock<Invokable> = OnceLock::new(); // System.Action.Invoke
static SET_ACTIVE: OnceLock<Invokable> = OnceLock::new(); // GameObject.SetActive(bool)
static PENDING_TAG_CB: AtomicUsize = AtomicUsize::new(0);
const O_TAG_ROOT: usize = 0x60; // SingleModeMainViewTagTrainingCutInPlayer._tagCutInRootObject

// GameObject.SetActive takes a bool arg → fn(this, bool, MethodInfo*).
type SetActiveFn = unsafe extern "C" fn(*mut c_void, bool, *mut c_void);

unsafe fn hide_tag_visual(this: *mut c_void) {
    if this.is_null() {
        return;
    }
    let go = *((this as usize + O_TAG_ROOT) as *const *mut c_void);
    if go.is_null() {
        return;
    }
    if let Some(sa) = SET_ACTIVE.get() {
        if sa.ok() {
            let f: SetActiveFn = std::mem::transmute(sa.code);
            f(go, false, sa.mi as *mut c_void);
        }
    }
}

unsafe extern "C" fn on_tag_play_cutin(this: *mut c_void, cards: *mut c_void, cb: *mut c_void, m: *mut c_void) {
    let t = TR_TAGIN.load(Ordering::Relaxed);
    if t != 0 {
        let f: TagInFn = std::mem::transmute(t);
        f(this, cards, cb, m); // full original flow — keeps state valid for PlayCutInOut
    }
    if is_enabled() && !in_heaven() && !cb.is_null() {
        hide_tag_visual(this); // hide the "FRIENDSHIP TRAINING!" splash content (no flicker)
        PENDING_TAG_CB.store(cb as usize, Ordering::Relaxed); // fire onDone early next frame
        TRAIN_SKIPS.fetch_add(1, Ordering::Relaxed);
    }
}
unsafe extern "C" fn on_tag_play_cutin_out(this: *mut c_void, cb: *mut c_void, m: *mut c_void) {
    let t = TR_TAGOUT.load(Ordering::Relaxed);
    if t != 0 {
        let f: TagOutFn = std::mem::transmute(t);
        f(this, cb, m);
    }
    if is_enabled() && !in_heaven() && !cb.is_null() {
        PENDING_TAG_CB.store(cb as usize, Ordering::Relaxed);
    }
}

unsafe fn fire_action(cb: usize) {
    if cb == 0 {
        return;
    }
    if let Some(inv) = ACTION_INVOKE.get() {
        if inv.ok() {
            let _g = ReentryGuard::enter();
            inv.call_void(cb as *mut c_void);
        }
    }
}

/// Fire all deferred callbacks (friendship onDone + shop perf callbacks) on a clean
/// main-thread frame (from the ButtonCommon.Update tick), avoiding re-entrancy.
fn pump_pending_tag_cb() {
    unsafe { fire_action(PENDING_TAG_CB.swap(0, Ordering::Relaxed)) };
    // Close the skipped use-item performance's dialogs BEFORE its callbacks reopen the list.
    if SHOP_TEARDOWN.swap(false, Ordering::Relaxed) {
        let _g = ReentryGuard::enter();
        shop_teardown();
    }
    let cbs: Vec<usize> = shop_pending().lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
    for c in cbs {
        unsafe { fire_action(c) };
    }
    // After a use-item performance was driven to completion (coroutine), auto-close the item
    // list so the player lands on the career screen instead of back on the list.
    if SHOP_CLOSE_LIST.swap(false, Ordering::Relaxed) {
        let _g = ReentryGuard::enter();
        let _ = shop_close_item_list();
        // A NORMAL-item buy shows the exchange-complete dialog via this same use-performance
        // coroutine, so it's THIS path (not the buy-path) that dismisses the dialog. If a buy is
        // pending, arm Back right now instead of letting the buy-path wait out its timeout.
        if SHOP_CLOSE_BUY_UNTIL.swap(0, Ordering::Relaxed) != 0 {
            SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
            rr_log(&format!("[shop {}ms] dialog dismissed during buy; back-press armed", now_ms()));
        }
    }
    // After a BUY (exchange), the "Exchange Complete / choose how many" dialog appears a few
    // frames LATER — so we retry closing the forefront dialog over a short window until it shows.
    let close_until = SHOP_CLOSE_BUY_UNTIL.load(Ordering::Relaxed);
    if close_until != 0 {
        if now_ms() < close_until {
            // Normal item: the exchange-complete dialog appears → close it, then arm Back.
            let _g = ReentryGuard::enter();
            if shop_close_item_list() {
                SHOP_CLOSE_BUY_UNTIL.store(0, Ordering::Relaxed);
                SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
                rr_log(&format!("[shop {}ms] buy dialog closed; back-press armed", now_ms()));
            }
        } else {
            // Window expired with no dialog ever appearing → it was an AUTO-REDEEM item (stat
            // books etc. apply instantly, no "choose how many" dialog). Just arm Back so the
            // player still lands where their manual Back would.
            SHOP_CLOSE_BUY_UNTIL.store(0, Ordering::Relaxed);
            SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
            rr_log(&format!("[shop {}ms] auto-redeem (no dialog); back-press armed", now_ms()));
        }
    }
}

/// Raw click on a ButtonCommon (no whitelist/dedup) — reuses the race-result click primitives to
/// auto-press the shop "BackButton" after a buy. Returns true once it actually clicked.
unsafe fn click_now(this: *mut c_void) -> bool {
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

/// Dismiss the forefront dialog after a shop item-use (the `DialogSingleModeScenarioFreeUserItemList`
/// the drive returns you to). Uses the proper DialogCommon.Close (clears blur + state — NOT
/// ForceDestroy). GetForeFrontDialog returns the DialogCommon *container*; right after a use it
/// is the item list, so closing it once lands on the underlying career screen. Logs the class it
/// closes so diagnostics show whether it hit the list or something unexpected.
/// Returns true if it closed a dialog. Quiet on the no-dialog/null cases so the BUY retry loop
/// doesn't spam the log every frame while waiting for the dialog to appear.
fn shop_close_item_list() -> bool {
    let (Some(gf), Some(close)) = (GET_FOREFRONT.get(), DIALOG_CLOSE.get()) else {
        return false;
    };
    if !gf.ok() || !close.ok() {
        return false;
    }
    let top = unsafe { gf.call_ptr_static() };
    if top.is_null() {
        return false;
    }
    let nm = il2cpp::object_class_name(top);
    if nm.contains("Dialog") {
        rr_log(&format!("[shop {}ms] close-list: Close {nm}", now_ms()));
        unsafe { close.call_void(top) };
        true
    } else {
        false
    }
}

// ── PRO SHOP (scenario free shop) buy/use performance skip ──────────────────
// SingleModeScenarioFreeShopViewController.PlayUseItemPerformanceCore(items, Action,
// Action) plays the buy/use flourish. The item effect is already applied by the server
// exchange/use request BEFORE this, so the performance is purely visual — skip it and
// fire its callbacks (deferred) to continue. One hook covers both buying and using.
static SHOP_PENDING: OnceLock<Mutex<Vec<usize>>> = OnceLock::new();
fn shop_pending() -> &'static Mutex<Vec<usize>> {
    SHOP_PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

// When we skip the inventory use-item performance, its coroutine (which we skipped)
// would normally ForceDestroy the open effect-list + user-item-list dialogs and reopen
// the list. We replicate that teardown next frame: get the forefront dialog and, while
// it's one of our two shop dialogs, ForceDestroy it; then the deferred callbacks reopen
// the list. Resolved in install(); the use-perf hook only arms if both are available.
static SHOP_TEARDOWN: AtomicBool = AtomicBool::new(false);
static PENDING_PARTS: AtomicUsize = AtomicUsize::new(0); // the use-item Parts to Release
static GET_FOREFRONT: OnceLock<Invokable> = OnceLock::new(); // DialogManager.GetForeFrontDialog (static)
static FORCE_DESTROY: OnceLock<Invokable> = OnceLock::new(); // DialogCommon.ForceDestroy (instance)
static DIALOG_CLOSE: OnceLock<Invokable> = OnceLock::new(); // DialogCommon.Close (proper dismiss: clears blur + state)
static PARTS_RELEASE: OnceLock<Invokable> = OnceLock::new(); // PartsSingleModeScenarioFreeUseItemPerformance.Release
static PLAY_CLOSE: OnceLock<Invokable> = OnceLock::new(); // SteamInputBlockManager.PlayClose (lift the input block)
static SIB_MGR: AtomicUsize = AtomicUsize::new(0); // captured SteamInputBlockManager instance
static SHOP_USE_CB: AtomicUsize = AtomicUsize::new(0); // the use-perf completion callback (continues SingleMode)

// Capture the SteamInputBlockManager instance from its normal PlayClose calls (it's a
// persistent manager; instance method, simple ABI). We need it to PlayClose the input
// block the skipped coroutine would otherwise have lifted (else input stays blocked).
hook_slot!(TR_SIBCLOSE, D_SIBCLOSE);
type SibCloseFn = unsafe extern "C" fn(*mut c_void, *mut c_void, bool, *mut c_void);
unsafe extern "C" fn on_sib_close(this: *mut c_void, action: *mut c_void, flag: bool, m: *mut c_void) {
    SIB_MGR.store(this as usize, Ordering::Relaxed);
    let t = TR_SIBCLOSE.load(Ordering::Relaxed);
    if t != 0 {
        let f: SibCloseFn = std::mem::transmute(t);
        f(this, action, flag, m);
    }
}

/// Replicate the skipped use-item coroutine's teardown: the normal coroutine ForceDestroys
/// the two open dialog CONTAINERS (the effect-list + user-item-list) and Releases the Parts.
/// GetForeFrontDialog returns the DialogCommon *container* (class "DialogCommon"), not the
/// content — so we destroy the forefront while it's a dialog container, capped at 2 (the
/// exact count the normal flow destroys), then Release the Parts. Deferred to a clean frame
/// (the ButtonCommon.Update pump) so it isn't re-entrant with the use-item button callback.
fn shop_teardown() {
    rr_log("[shop] teardown START");
    if let (Some(gf), Some(close)) = (GET_FOREFRONT.get(), DIALOG_CLOSE.get()) {
        if gf.ok() && close.ok() {
            for _ in 0..2 {
                let top = unsafe { gf.call_ptr_static() };
                if top.is_null() {
                    rr_log("[shop] forefront null");
                    break;
                }
                let nm = il2cpp::object_class_name(top);
                if nm.contains("Dialog") {
                    rr_log(&format!("[shop] Close {nm}"));
                    unsafe { close.call_void(top) };
                } else {
                    rr_log(&format!("[shop] stop at {nm}"));
                    break;
                }
            }
        } else {
            rr_log("[shop] gf/close not ok");
        }
    }
    // Fire the use-perf completion callback to CONTINUE the SingleMode flow (without it the
    // flow stays paused waiting for the performance → input dead). After dialogs are closed.
    let cb = SHOP_USE_CB.swap(0, Ordering::Relaxed);
    if cb != 0 {
        rr_log("[shop] fire completion callback");
        unsafe { fire_action(cb) };
    }
    // Then lift the input block (drain a few — the use flow stacks several).
    let mgr = SIB_MGR.load(Ordering::Relaxed);
    rr_log(&format!("[shop] sib_mgr={:#x} play_close_ok={}", mgr, PLAY_CLOSE.get().map(|i| i.ok()).unwrap_or(false)));
    if mgr != 0 {
        if let Some(pc) = PLAY_CLOSE.get() {
            if pc.ok() {
                for i in 0..6 {
                    unsafe { pc.call_play_close(mgr as *mut c_void, true) };
                    rr_log(&format!("[shop] PlayClose #{i}"));
                }
            }
        }
    }
    // NOTE: do NOT call Parts.Release() here — it blocks (it waits for the performance we
    // skipped to finish → deadlock/hang). Closing the dialogs + lifting the input block is
    // enough; the Parts is torn down with its dialog.
    let _ = PENDING_PARTS.swap(0, Ordering::Relaxed);
    rr_log("[shop] teardown END");
}
// ── Drive the use-item performance coroutine to completion in one frame ──────
// External replication of the coroutine's effects is impossible (the SingleMode continuation
// + input-unblock live INSIDE the coroutine; the callback arg is null). So instead we let the
// game's own coroutine run, but on its first MoveNext we pump it to the end synchronously: it
// performs its own cleanup (Close/PlayClose/ForceDestroy) + continuation, just without the
// inter-step visual waits — so nothing renders. The game does everything correctly.
hook_slot!(TR_MOVENEXT, D_MOVENEXT);
static DRIVING: AtomicBool = AtomicBool::new(false);
// Set after a shop action (inventory USE coroutine drive, or a BUY performance skip) → the next
// ButtonCommon pump auto-closes the leftover dialog so the player lands back on the underlying
// screen instead of on the item/buy dialog.
static SHOP_CLOSE_LIST: AtomicBool = AtomicBool::new(false);
// TIME deadlines (ms since `clock()` start), NOT frame counts — on_button_update / the pump run
// once PER ButtonCommon, so a frame counter burns out in a few real frames. A real-time window
// survives the dialog→shop transition. After a BUY we retry closing the exchange dialog until
// this deadline; once closed we arm the BackButton auto-press until its own deadline.
static SHOP_CLOSE_BUY_UNTIL: AtomicU64 = AtomicU64::new(0);
static SHOP_PRESS_BACK_UNTIL: AtomicU64 = AtomicU64::new(0);
fn now_ms() -> u64 {
    clock().elapsed().as_millis() as u64
}

// ── career heartbeat ─────────────────────────────────────────────────────────
// The race-result auto-advance is a CAREER (single-mode) feature, but its press targets are
// generic button names that also exist in other modes' menus — so if it armed in a non-career
// race it kept auto-pressing menu buttons there (nothing clears the window outside career, since
// only the single-mode ChangeMainView does). Fix: a positive gate. Every SingleMode (career-only)
// hook stamps this heartbeat; the result skip only ARMS while the heartbeat is fresh — so it can
// never arm or leak outside a career run.
static LAST_CAREER_MS: AtomicU64 = AtomicU64::new(0);
/// Mark that a career (single-mode) hook just fired. Called from the SingleMode-only detours.
fn mark_career() {
    // Log only the OPEN transition (gate was closed → now in career), so the log isn't spammed by
    // the many ChangeMainView calls within a run.
    if LAST_CAREER_MS.swap(now_ms(), Ordering::Relaxed) == 0 {
        rr_log("[race-result] career detected (ChangeMainView) -> gate OPEN (skip may arm now)");
    }
}
/// True if a career hook fired recently enough to still be in this career run (window spans a full
/// race + the pre-race menu, so a legit career result is never blocked; goes stale after you leave).
fn career_fresh() -> bool {
    LAST_CAREER_MS.load(Ordering::Relaxed) != 0
        && now_ms().saturating_sub(LAST_CAREER_MS.load(Ordering::Relaxed)) < 360_000
}
type BoolMethodFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;
unsafe extern "C" fn on_movenext(this: *mut c_void, m: *mut c_void) -> bool {
    let t = TR_MOVENEXT.load(Ordering::Relaxed);
    if t == 0 {
        return false;
    }
    let f: BoolMethodFn = std::mem::transmute(t);
    if !is_shop_enabled() || in_heaven() || DRIVING.load(Ordering::Relaxed) {
        return f(this, m); // normal single step (or a step during our own drive)
    }
    DRIVING.store(true, Ordering::Relaxed);
    let _g = ReentryGuard::enter();
    let mut n = 0u32;
    while f(this, m) {
        n += 1;
        if n > 2000 {
            break;
        }
    }
    DRIVING.store(false, Ordering::Relaxed);
    // We just used an item; queue closing the item-list dialog (lands past "Close", like the
    // race-result auto-advance). Deferred to a clean frame via the ButtonCommon.Update pump so
    // it isn't re-entrant with this coroutine. Trade-off (accepted per request): auto-closing
    // after each use means you can't chain several uses without reopening the list.
    SHOP_CLOSE_LIST.store(true, Ordering::Relaxed);
    rr_log(&format!("[shop] use-perf coroutine driven in {n} steps; close-list queued"));
    false
}

// Skip the full-screen rival ENTRY cut-in (the 2D "RIVAL <name>" card shown before a rival
// race). It is played by SingleModeRaceEntryViewController.<PlayRivalEntryCoroutine>d__103.
// On its FIRST MoveNext (state 0) we set the state field to -1 so the body falls through to
// the default case and renders nothing, then call DestroyRivalEntry() to clear any partial
// visuals and invoke the coroutine's endAction so the flow proceeds straight to the race.
// (Driving the coroutine to completion does NOT work here — its first step yields on the
// rival model/asset load, never advancing the on-screen card; this early-skip does.)
hook_slot!(TR_RIVALMN, D_RIVALMN);
static DESTROY_RIVAL_ENTRY: OnceLock<Invokable> = OnceLock::new(); // PartsRivalEntryAnimation.DestroyRivalEntryWithUnload
const O_RIVAL_STATE: usize = 0x10; // <>1__state
const O_RIVAL_ENDACTION: usize = 0x20; // endAction (System.Action)
// 2026-07-01 update: coroutine moved to Gallop.PartsRivalEntryAnimation.d__11; a new
// itemIconList field @0x28 pushed <>4__this from 0x28 -> 0x30.
const O_RIVAL_THIS: usize = 0x30; // <>4__this (PartsRivalEntryAnimation)
unsafe extern "C" fn on_rival_movenext(this: *mut c_void, m: *mut c_void) -> bool {
    let t = TR_RIVALMN.load(Ordering::Relaxed);
    if t == 0 {
        return false;
    }
    let f: BoolMethodFn = std::mem::transmute(t);
    if !is_rival_enabled() || in_heaven() || this.is_null() {
        return f(this, m);
    }
    // Only intercept the very first step; later steps (and our forced -1) run normally.
    let state = *((this as usize + O_RIVAL_STATE) as *const i32);
    if state != 0 {
        return f(this, m);
    }
    let ctrl = *((this as usize + O_RIVAL_THIS) as *const *mut c_void);
    let end_action = *((this as usize + O_RIVAL_ENDACTION) as *const usize);
    *((this as usize + O_RIVAL_STATE) as *mut i32) = -1; // body -> default -> returns false, no visuals
    let _ = f(this, m);
    if !ctrl.is_null() {
        if let Some(inv) = DESTROY_RIVAL_ENTRY.get() {
            if inv.ok() {
                let _g = ReentryGuard::enter();
                inv.call_void(ctrl);
            }
        }
    }
    fire_action(end_action); // proceed to the race
    rr_log("[rival] skipped entry cut-in");
    false
}

hook_slot!(TR_SHOPPERF, D_SHOPPERF);
type ShopPerfFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void);

unsafe extern "C" fn on_shop_perf(this: *mut c_void, items: *mut c_void, cb1: *mut c_void, cb2: *mut c_void, m: *mut c_void) {
    if is_shop_enabled() && !in_heaven() {
        if let Ok(mut q) = shop_pending().lock() {
            if !cb1.is_null() {
                q.push(cb1 as usize);
            }
            if !cb2.is_null() {
                q.push(cb2 as usize);
            }
        }
        EVENT_SKIPS.fetch_add(1, Ordering::Relaxed);
        // Same as the inventory-use path: after the buy callbacks fire on the pump, auto-close
        // the leftover buy/result dialog so the player lands back on the shop without a manual
        // dismiss. The purchase is already committed server-side before this performance runs,
        // so closing the dialog afterwards is safe.
        SHOP_CLOSE_LIST.store(true, Ordering::Relaxed);
        rr_log("[shop] buy perf skipped; close-list queued");
        return; // skip the visual; callbacks fire next frame
    }
    let t = TR_SHOPPERF.load(Ordering::Relaxed);
    if t != 0 {
        let f: ShopPerfFn = std::mem::transmute(t);
        f(this, items, cb1, cb2, m);
    }
}

// A plain BUY (exchange, no use) does NOT fire PlayUseItemPerformanceCore — it lands on the
// "Exchange Complete / Choose how many to use" dialog (DialogSingleModeScenarioFreeUseItemEffectList)
// that the user wants to skip. The controller shows that dialog via OnUseItemExchangeCompleteDialog
// (the BUY path: OnClickShopItemExchange → CallbackSendSingleModeFreeItemExchangeRequest →
// OnUseItemExchangeCompleteDialog). We let it run, then queue the same forefront close as the use
// path, so after buying we auto-dismiss that dialog (= press Close) and land back on the shop. This
// is BUY-specific (inventory use goes through OnClickUserItem), so it never blocks using items.
hook_slot!(TR_EXCHCOMPLETE, D_EXCHCOMPLETE);
type ExchDlgFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void);
// Hooks CallbackSendSingleModeFreeItemExchangeRequest (the BUY/exchange completion, confirmed by
// trace) — the dialog appears right after. Arm the frame countdown so the pump closes it once it's
// up. BUY path only (inventory use never goes through the exchange request).
unsafe extern "C" fn on_exch_request(
    this: *mut c_void, a1: *mut c_void, a2: *mut c_void, a3: *mut c_void, m: *mut c_void,
) {
    let t = TR_EXCHCOMPLETE.load(Ordering::Relaxed);
    if t != 0 {
        let f: ExchDlgFn = std::mem::transmute(t);
        f(this, a1, a2, a3, m); // run the original first (it shows the exchange-complete dialog)
    }
    if is_shop_enabled() && !in_heaven() {
        SHOP_CLOSE_BUY_UNTIL.store(now_ms() + 1000, Ordering::Relaxed); // 1s to catch the dialog; else auto-redeem
        rr_log(&format!("[shop {}ms] exchange request done; buy-close armed", now_ms()));
    }
}

// The USE flow doesn't show its flourish via PlayUseItemPerformanceCore (that's the
// BUY path) — instead it pops the "Use <item>" chara-message card via
// PlayCharaMessage(Queue<Trigger>). The arg is a queue of message data, NOT a
// callback, and the routine just starts a display coroutine that self-advances —
// so skipping it (return without starting the coroutine) drops the popup with no
// continuation left dangling. Covers shop-enter greeting + exchange + use messages.
hook_slot!(TR_CHARAMSG, D_CHARAMSG);
type CharaMsgFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);

unsafe extern "C" fn on_chara_msg(this: *mut c_void, q: *mut c_void, m: *mut c_void) {
    if is_shop_enabled() && !in_heaven() {
        EVENT_SKIPS.fetch_add(1, Ordering::Relaxed);
        return; // skip the chara-message popup
    }
    let t = TR_CHARAMSG.load(Ordering::Relaxed);
    if t != 0 {
        let f: CharaMsgFn = std::mem::transmute(t);
        f(this, q, m);
    }
}

// The actual USE-item animation (chara cheering + "Use <item> / Stat Up" card +
// param-up flourish) is played by PartsSingleModeScenarioFreeUseItemPerformance,
// shared by BOTH the buy→use-now path AND the inventory "use item" dialog (which
// reaches it directly after the effect-list Decide, bypassing the controller's
// PlayUseItemPerformanceCore). Hooking it here covers both. The last two args are
// System.Action callbacks (onCompleteInitializeFlash + completion) — defer-fire
// them so the flow advances exactly as it would after the animation finishes.
hook_slot!(TR_USEPERF, D_USEPERF);
type UsePerfFn = unsafe extern "C" fn(
    *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void,
);
unsafe extern "C" fn on_use_perf(
    this: *mut c_void,
    a0: *mut c_void,
    a1: *mut c_void,
    a2: *mut c_void,
    a3: *mut c_void,
    a4: *mut c_void,
    a5: *mut c_void,
    m: *mut c_void,
) {
    if is_shop_enabled() && !in_heaven() {
        // Fire BOTH callbacks (onCompleteInitializeFlash + final completion): the flow needs
        // both to fully continue + re-enable input. Now that Close() tears the dialogs down
        // first, firing both no longer rebuilds/desyncs them.
        // a5 = completion callback (continues the SingleMode flow). Fire it in teardown
        // AFTER closing dialogs and BEFORE the final PlayClose. a4 (mid-flash) is dropped.
        // a5 (the "callback") is NULL on the inventory path; a4 (onCompleteInitializeFlash)
        // is the non-null one. Fire whichever is non-null as the continuation.
        let cb = if !a5.is_null() { a5 } else { a4 };
        SHOP_USE_CB.store(cb as usize, Ordering::Relaxed);
        PENDING_PARTS.store(this as usize, Ordering::Relaxed);
        rr_log(&format!("[shop] skip PlayUseItemPerformance a4={:#x} a5={:#x}", a4 as usize, a5 as usize));
        SHOP_TEARDOWN.store(true, Ordering::Relaxed); // close dialogs + Release next frame
        EVENT_SKIPS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let t = TR_USEPERF.load(Ordering::Relaxed);
    if t != 0 {
        let f: UsePerfFn = std::mem::transmute(t);
        f(this, a0, a1, a2, a3, a4, a5, m);
    }
}

// Variant for items without the param-up panel: (Transform, List<info>, Action, Action).
hook_slot!(TR_USEPERFD, D_USEPERFD);
type UsePerfDFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void);
unsafe extern "C" fn on_use_perf_default(
    this: *mut c_void,
    a0: *mut c_void,
    a1: *mut c_void,
    a2: *mut c_void,
    a3: *mut c_void,
    m: *mut c_void,
) {
    if is_shop_enabled() && !in_heaven() {
        // a3 (callback) may be null; a2 (onInit) is the other. Fire whichever is non-null.
        let cb = if !a3.is_null() { a3 } else { a2 };
        SHOP_USE_CB.store(cb as usize, Ordering::Relaxed);
        PENDING_PARTS.store(this as usize, Ordering::Relaxed); rr_log("[shop] skip PlayDefaultUseItemPerformance");
        SHOP_TEARDOWN.store(true, Ordering::Relaxed); // close dialogs + Release next frame
        EVENT_SKIPS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let t = TR_USEPERFD.load(Ordering::Relaxed);
    if t != 0 {
        let f: UsePerfDFn = std::mem::transmute(t);
        f(this, a0, a1, a2, a3, m);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// B3b — RACE-RESULT AUTO-ADVANCE  (EXPERIMENTAL, default OFF)
// Port of native_skip.js part 3. After "View Results" (+ the unavoidable 1st
// tap), the result screens auto-press their own buttons to the next turn.
// Untested in this native form → gated behind RACE_RESULT_ENABLED; enabling it
// never touches the proven training/event core.
// ═══════════════════════════════════════════════════════════════════════════
// Default ON in builds with feature `races_on`, OFF otherwise.
static RACE_RESULT_ENABLED: AtomicBool = AtomicBool::new(cfg!(feature = "races_on"));
pub fn set_race_result_enabled(on: bool) {
    RACE_RESULT_ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_race_result_enabled() -> bool {
    RACE_RESULT_ENABLED.load(Ordering::Relaxed)
}

// TEAM TRIALS guard. The race-result auto-advance is a CAREER (single-mode) feature,
// but its anchor button ("RaceSkipButton") and press targets also exist on the Team
// Trials result screen — so without this it auto-pressed buttons there and got the TT
// result UI stuck (the long-standing v2.2 bug). htt.rs sets this true when a Team
// Trials result is built; the career view-manager (ChangeMainView, single-mode only)
// clears it when we're back in career. While set, race-result never fires.
static IN_TEAM_TRIALS: AtomicBool = AtomicBool::new(false);
pub fn set_in_team_trials(on: bool) {
    IN_TEAM_TRIALS.store(on, Ordering::Relaxed);
}

/// Race-result auto-advance only fires when the player WON (finished 1st).
/// Anything else — lost, or placement not yet known — means NO auto-advance, so
/// the player handles it manually (e.g. to retry). Reset per race in race.rs
/// (ImportDirect), so a retry or the next race is re-evaluated from scratch.
fn rr_should_advance() -> bool {
    if !RACE_RESULT_ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    // Never auto-advance during Team Trials (career-only feature).
    if IN_TEAM_TRIALS.load(Ordering::Relaxed) {
        return false;
    }
    // Positive career gate: only ever auto-press inside a career (single-mode) run, so a stale armed
    // window can't leak presses into other modes' menus.
    if !career_fresh() {
        return false;
    }
    // Auto-advance when the player WON (placement 1), OR when no race retries remain
    // (`available_continue_num` == 0): a retry isn't possible, so don't hold the result
    // screen on a loss. Placement + continues come from the response hooks (race_net in
    // public, the response hook in private). continues == -1 means "unknown" → fall back
    // to the win-only gate. Both builds ship `raceread`.
    #[cfg(feature = "raceread")]
    {
        let won = crate::race::player_finish_order() == 1;
        let no_retries_left = crate::race::continues_available() == 0;
        won || no_retries_left
    }
}

const PRESS_GAP_MS: u64 = 130;
const MULTI_MAX: u32 = 4;
// EXACT whitelist + exact match (substring matching caused mis-presses). The
// (substring matching caused mis-presses, so we match the exact button names).
fn is_press_target(name: &str) -> bool {
    [
        "ButtonCommon",
        "ButtonCenter",
        "NextButton",
        "ScreenTap",
        "SingleModeNextButton",
        "TouchSprite",
        // Debut / first-race completion (and other special result screens) advance via
        // a "Continue" button. Safe to press: auto_press only runs when rr_should_advance
        // (won, or no retries left), so a retry-eligible loss never auto-continues.
        "ContinueButton",
    ]
    .contains(&name)
}
fn is_multi(name: &str) -> bool {
    name == "ScreenTap"
        || name == "TouchSprite"
        || name == "ButtonCommon"
        || name == "ButtonCenter"
}

/// Gate the auto-press by context. When the skip is advancing a LOSS only because race
/// retries are exhausted (`continues == 0`), the result screen's retry is a paid
/// "buy an alarm clock" purchase — and the generic dialog taps (ButtonCenter / ScreenTap /
/// ButtonCommon / TouchSprite) or ContinueButton can land on that purchase confirmation /
/// the server round-trip, spending Carats and desyncing the game (bounce to title). In that
/// one case we press ONLY the explicit result-advance button. A win (or a loss with retries
/// still left) uses the full whitelist as before — those screens have no purchase dialog.
fn press_allowed(name: &str) -> bool {
    #[cfg(feature = "raceread")]
    {
        let won = crate::race::player_finish_order() == 1;
        let retries_exhausted = crate::race::continues_available() == 0;
        if !won && retries_exhausted {
            return name == "NextButton" || name == "SingleModeNextButton";
        }
    }
    is_press_target(name)
}

/// Append a line to the native engine log (race-result diagnostics).
fn rr_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

// Single global busy flag (mirrors native_skip.js `busy`): set while WE invoke a
// button/dialog method so the Update/Push detours skip during our own calls.
static BUSY: AtomicBool = AtomicBool::new(false);
static WINDOW_OPEN: AtomicBool = AtomicBool::new(false);
static RR_PRESSES: AtomicU64 = AtomicU64::new(0);

// Resolved (methodPointer code, MethodInfo*) pairs — called DIRECTLY like the
// JS NativeFunctions (no runtime_invoke), passing the trailing MethodInfo arg.
static GETNAME_C: AtomicUsize = AtomicUsize::new(0);
static GETNAME_MI: AtomicUsize = AtomicUsize::new(0);
static OPC_C: AtomicUsize = AtomicUsize::new(0);
static OPC_MI: AtomicUsize = AtomicUsize::new(0);
static ISLOCK_C: AtomicUsize = AtomicUsize::new(0);
static ISLOCK_MI: AtomicUsize = AtomicUsize::new(0);
static CLOSE_C: AtomicUsize = AtomicUsize::new(0);
static CLOSE_MI: AtomicUsize = AtomicUsize::new(0);
static CUR_C: AtomicUsize = AtomicUsize::new(0);
static CUR_MI: AtomicUsize = AtomicUsize::new(0);
static CTOR_C: AtomicUsize = AtomicUsize::new(0);
static CTOR_MI: AtomicUsize = AtomicUsize::new(0);
static C_PED: AtomicUsize = AtomicUsize::new(0);

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
fn clear_rr_caches() {
    if let Ok(mut m) = name_cache().lock() { m.clear(); }
    if let Ok(mut m) = press_state().lock() { m.clear(); }
    if let Ok(mut m) = done_dlg().lock() { m.clear(); }
    if let Ok(mut m) = logged_names().lock() { m.clear(); }
    RR_NEXT_LIFT.store(0, Ordering::Relaxed);
}

// Direct-call ABIs (this, …, MethodInfo*).
type RetPtr = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void; // get_name
type RetBool = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool; // IsLock
type Void2 = unsafe extern "C" fn(*mut c_void, *mut c_void); // Close
type Click = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void); // OnPointerClick(this, ped, mi)
type CurStatic = unsafe extern "C" fn(*mut c_void) -> *mut c_void; // EventSystem.get_current(mi)
type Ctor1 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void); // ctor(this, es, mi)

/// GameObject/component name of a button (cached), via direct get_name call.
fn button_name(this: *mut c_void) -> String {
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
unsafe fn make_pointer_event() -> *mut c_void {
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

// Stuck-advance-button input-block lift. The victory-concert result screen (and similar) keeps a
// SteamInputBlock(Clone) up that never lifts on its own, leaving Next/Continue locked forever so
// the skip stalls. 0 = not timing yet; otherwise the earliest ms at which we may PlayClose. Reset
// on a successful press and on race end (clear_rr_caches).
static RR_NEXT_LIFT: AtomicU64 = AtomicU64::new(0);
const RR_LIFT_GRACE_MS: u64 = 600; // stay locked this long before lifting (don't fight normal transient locks)
const RR_LIFT_THROTTLE_MS: u64 = 400; // re-lift at most this often while still stuck

fn is_advance_button(name: &str) -> bool {
    name == "NextButton" || name == "SingleModeNextButton" || name == "ContinueButton"
}

/// Lift the SteamInputBlock the skipped result coroutine would have lifted (the same PlayClose the
/// shop-skip uses) so a stuck-locked advance button unlocks. No-op until SIB_MGR is captured.
fn lift_input_block() {
    let mgr = SIB_MGR.load(Ordering::Relaxed);
    if mgr == 0 {
        return;
    }
    if let Some(pc) = PLAY_CLOSE.get() {
        if pc.ok() {
            unsafe { pc.call_play_close(mgr as *mut c_void, true) };
            rr_log("[race-result] lifted stuck input block (advance button locked)");
        }
    }
}

fn auto_press(this: *mut c_void) {
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
fn auto_close(dlg: *mut c_void) {
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

pub fn race_result_stats() -> (bool, u64) {
    (WINDOW_OPEN.load(Ordering::Relaxed), RR_PRESSES.load(Ordering::Relaxed))
}

// Detours for race-result.
type Void3 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type Push1 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
type Push2 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
hook_slot!(TR_UPDATE, D_UPDATE);
hook_slot!(TR_ONPC, D_ONPC);
hook_slot!(TR_CMV, D_CMV);
hook_slot!(TR_HOME, D_HOME); // HomeViewController.PlayInView — reached the lobby = left career
hook_slot!(TR_PUSH1, D_PUSH1);
hook_slot!(TR_PUSH2, D_PUSH2);

unsafe extern "C" fn on_button_update(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_UPDATE, this, m);
    pump_pending_tag_cb(); // fire deferred friendship-splash onDone on a clean frame
    // After a buy auto-closes its dialog, auto-press the shop "BackButton" so the player lands
    // where their manual Back would (this Update fires per ButtonCommon, so we catch BackButton
    // when it's this one). Frame-windowed + shop-gated so it never fires elsewhere.
    if now_ms() < SHOP_PRESS_BACK_UNTIL.load(Ordering::Relaxed) && is_shop_enabled() && !in_heaven() {
        if button_name(this) == "BackButton" {
            let _g = ReentryGuard::enter();
            if click_now(this) {
                SHOP_PRESS_BACK_UNTIL.store(0, Ordering::Relaxed);
                rr_log(&format!("[shop {}ms] auto-pressed BackButton", now_ms()));
            }
        }
    }
    // Main-thread per-frame tick: let the the full build recompute its box geometry here
    // (Unity UI calls are only safe on the game thread, not the render thread).
    if rr_should_advance() && WINDOW_OPEN.load(Ordering::Relaxed) && !in_heaven() {
        auto_press(this);
    }
}
unsafe extern "C" fn on_pointer_click(this: *mut c_void, evt: *mut c_void, m: *mut c_void) {
    let t = TR_ONPC.load(Ordering::Relaxed);
    if t != 0 {
        let f: Void3 = std::mem::transmute(t);
        f(this, evt, m);
    }
    if !RACE_RESULT_ENABLED.load(Ordering::Relaxed) || in_heaven() {
        return;
    }
    // Only the in-race skip button is the anchor.
    if !button_name(this).contains("RaceSkipButton") {
        return;
    }
    // Log EVERY RaceSkipButton click + the gate decision, so the arm/disarm behaviour can be
    // verified from heaven-native.log without having to reproduce the rare menu-bug live.
    let tt = IN_TEAM_TRIALS.load(Ordering::Relaxed);
    let cf = career_fresh();
    let already = WINDOW_OPEN.load(Ordering::Relaxed);
    let age = now_ms().saturating_sub(LAST_CAREER_MS.load(Ordering::Relaxed));
    if !already && !tt && cf {
        WINDOW_OPEN.store(true, Ordering::Relaxed);
        clear_rr_caches();
        #[cfg(feature = "raceread")]
        let fo = crate::race::player_finish_order();
        rr_log(&format!(
            "[race-result] RaceSkipButton -> ARMED (career age={age}ms, finish_order={fo} -> {})",
            if fo == 1 { "SKIP" } else { "MANUAL" }
        ));
    } else {
        rr_log(&format!(
            "[race-result] RaceSkipButton -> NOT armed (career_fresh={cf} age={age}ms, team_trials={tt}, already_open={already})"
        ));
    }
}
unsafe extern "C" fn on_change_main_view(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_CMV, this, m);
    // ChangeMainView is single-mode only → we're in career; refresh the heartbeat + clear TT guard.
    mark_career();
    IN_TEAM_TRIALS.store(false, Ordering::Relaxed);
    if WINDOW_OPEN.swap(false, Ordering::Relaxed) {
        clear_rr_caches();
        rr_log("[race-result] back in career (ChangeMainView) -> window disarmed");
    }
}
// HomeViewController.PlayInView — the lobby (Home) is animating in, i.e. we've LEFT career. Hard-clear
// the career heartbeat + disarm the race-result skip, so it can never leak into non-career modes
// (you always pass through Home to reach Daily / Champions / Legend / Story). Returns the coroutine.
unsafe extern "C" fn on_home_in(this: *mut c_void, m: *mut c_void) -> *mut c_void {
    let t = TR_HOME.load(Ordering::Relaxed);
    let rv = if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(t);
        f(this, m)
    } else {
        std::ptr::null_mut()
    };
    LAST_CAREER_MS.store(0, Ordering::Relaxed); // career heartbeat → stale immediately
    let was_open = WINDOW_OPEN.swap(false, Ordering::Relaxed);
    if was_open {
        clear_rr_caches();
    }
    rr_log(&format!("[race-result] Home reached -> gate CLOSED (heartbeat cleared, was_armed={was_open})"));
    rv
}
unsafe extern "C" fn on_push1(this: *mut c_void, a: *mut c_void, m: *mut c_void) -> *mut c_void {
    let t = TR_PUSH1.load(Ordering::Relaxed);
    let rv = if t != 0 {
        let f: Push1 = std::mem::transmute(t);
        f(this, a, m)
    } else {
        std::ptr::null_mut()
    };
    if rr_should_advance() && WINDOW_OPEN.load(Ordering::Relaxed) && !in_heaven() {
        auto_close(rv);
    }
    rv
}
unsafe extern "C" fn on_push2(
    this: *mut c_void,
    a: *mut c_void,
    b: *mut c_void,
    m: *mut c_void,
) -> *mut c_void {
    let t = TR_PUSH2.load(Ordering::Relaxed);
    let rv = if t != 0 {
        let f: Push2 = std::mem::transmute(t);
        f(this, a, b, m)
    } else {
        std::ptr::null_mut()
    };
    if rr_should_advance() && WINDOW_OPEN.load(Ordering::Relaxed) && !in_heaven() {
        auto_close(rv);
    }
    rv
}

/// Install race-result auto-advance (faithful port of native_skip.js part 3).
/// Independent of training/events. Returns Ok(note) describing what resolved.
pub fn install_race_result() -> Result<String, String> {
    let mut note = String::new();
    let btn = il2cpp::class("Gallop.ButtonCommon");
    if btn.is_null() {
        return Err("anchor miss".into());
    }
    let cvm = il2cpp::class("Gallop.SingleModeChangeViewManager");
    if cvm.is_null() {
        return Err("view-mgr miss".into());
    }
    let dm = il2cpp::class("Gallop.DialogManager");
    let dc = il2cpp::class("Gallop.DialogCommon");
    let obj = il2cpp::class("UnityEngine.Object");
    let es = il2cpp::class("UnityEngine.EventSystems.EventSystem");
    let ped = il2cpp::class("UnityEngine.EventSystems.PointerEventData");

    // Resolve (code, MethodInfo) pairs.
    let resolve = |cs: &AtomicUsize, ms: &AtomicUsize, k: il2cpp::Class, n: &str, argc: i32| -> bool {
        if k.is_null() {
            return false;
        }
        let m = il2cpp::method(k, n, argc);
        if m.is_null() {
            return false;
        }
        cs.store(il2cpp::method_pointer(m) as usize, Ordering::Relaxed);
        ms.store(m as usize, Ordering::Relaxed);
        true
    };
    if !resolve(&GETNAME_C, &GETNAME_MI, obj, "get_name", 0) {
        note.push_str("name miss; ");
    }
    if !resolve(&OPC_C, &OPC_MI, btn, "OnPointerClick", 1) {
        return Err("click miss".into());
    }
    resolve(&ISLOCK_C, &ISLOCK_MI, btn, "IsLock", 0);
    if !dc.is_null() {
        resolve(&CLOSE_C, &CLOSE_MI, dc, "Close", 0);
    }
    resolve(&CUR_C, &CUR_MI, es, "get_current", 0);
    if !ped.is_null() {
        C_PED.store(ped as usize, Ordering::Relaxed);
        resolve(&CTOR_C, &CTOR_MI, ped, ".ctor", 1);
    }
    if CTOR_C.load(Ordering::Relaxed) == 0 {
        note.push_str("evt ctor miss; ");
    }

    unsafe {
        install_one(btn, "Update", 0, on_button_update as *const (), &TR_UPDATE, &D_UPDATE)?;
        install_one(btn, "OnPointerClick", 1, on_pointer_click as *const (), &TR_ONPC, &D_ONPC)?;
        install_one(cvm, "ChangeMainView", 0, on_change_main_view as *const (), &TR_CMV, &D_CMV)?;
        // Home (lobby) hard-clear — non-fatal: if it misses, the 6-min heartbeat window still bounds it.
        let home = il2cpp::class("Gallop.HomeViewController");
        if home.is_null() {
            note.push_str("home view miss (heartbeat-only gate); ");
        } else if install_one(home, "PlayInView", 0, on_home_in as *const (), &TR_HOME, &D_HOME).is_err() {
            note.push_str("home hook miss; ");
        }
        if dm.is_null() {
            note.push_str("dialog-mgr miss (no auto-close); ");
        } else {
            if install_one(dm, "PushDialog", 1, on_push1 as *const (), &TR_PUSH1, &D_PUSH1).is_err() {
                note.push_str("push1 miss; ");
            }
            if install_one(dm, "PushDialogSequence", 2, on_push2 as *const (), &TR_PUSH2, &D_PUSH2).is_err() {
                note.push_str("push2 miss; ");
            }
        }
    }
    Ok(note)
}

// ── install ─────────────────────────────────────────────────────────────────
unsafe fn install_one(
    klass: il2cpp::Class,
    name: &str,
    argc: i32,
    detour_fn: *const (),
    tramp: &AtomicUsize,
    keep: &OnceLock<RawDetour>,
) -> Result<(), String> {
    let m = il2cpp::method(klass, name, argc);
    if m.is_null() {
        return Err(format!("{name} miss"));
    }
    let target = il2cpp::method_pointer(m);
    if target.is_null() {
        return Err(format!("{name} ptr null"));
    }
    if il2cpp::is_detoured(target) {
        return Err(format!("{name}: already detoured (skipped)"));
    }
    let d = RawDetour::new(target as *const (), detour_fn).map_err(|e| format!("{name}: {e}"))?;
    d.enable().map_err(|e| format!("{name} enable: {e}"))?;
    tramp.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
    let _ = keep.set(d);
    Ok(())
}

/// Returns (training_ok, events_ok, notes).
pub fn install() -> (bool, bool, String) {
    let mut notes = String::new();
    let mut training_ok = false;
    let mut events_ok = false;

    // ── TRAINING ──  (all IL2CPP names obfuscated → no `strings` leak)
    let helper = il2cpp::class("Gallop.SingleModeTrainingCutInHelper");
    if helper.is_null() {
        notes.push_str("train helper miss; ");
    } else {
        let _ = SKIP_RUNTIME.set(resolve(helper, "SkipRuntime", 0));
        if !SKIP_RUNTIME.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("train skip miss; ");
        } else {
            let mut any = false;
            unsafe {
                for r in [
                    install_one(helper, "OnStartCutIn", 0, on_start_cutin as *const (), &TR_START, &D_START),
                    install_one(helper, "OnPlayCutIn", 0, on_play_cutin as *const (), &TR_PLAY, &D_PLAY),
                    install_one(helper, "OnPlayMainCutIn", 0, on_play_main_cutin as *const (), &TR_MAIN, &D_MAIN),
                ] {
                    match r {
                        Ok(()) => any = true,
                        Err(e) => notes.push_str(&format!("{e}; ")),
                    }
                }
            }
            training_ok = any;
        }
    }

    // ── TAG (friendship/rainbow) TRAINING splash ── skip the "FRIENDSHIP TRAINING!"
    //    cut-in by firing its onDone early (deferred). Shares the training-skip toggle.
    let _ = ACTION_INVOKE.set(resolve(il2cpp::class("System.Action"), "Invoke", 0));
    let _ = SET_ACTIVE.set(resolve(il2cpp::class("UnityEngine.GameObject"), "SetActive", 1));
    let tag = il2cpp::class("Gallop.SingleModeMainViewTagTrainingCutInPlayer");
    if tag.is_null() {
        notes.push_str("tag cutin miss; ");
    } else if !ACTION_INVOKE.get().map(|i| i.ok()).unwrap_or(false) {
        notes.push_str("action.invoke miss; ");
    } else {
        unsafe {
            if let Err(e) = install_one(tag, "PlayCutIn", 2, on_tag_play_cutin as *const (), &TR_TAGIN, &D_TAGIN) {
                notes.push_str(&format!("{e}; "));
            }
            let _ = install_one(tag, "PlayCutInOut", 1, on_tag_play_cutin_out as *const (), &TR_TAGOUT, &D_TAGOUT);
        }
    }

    // ── PHOTO STUDIO cut-recreation guard ── pause the training-skip while the Photo
    //    Studio replays a cut (it reuses SingleModeTrainingCutInHelper, so our OnPlayCutIn
    //    hook would otherwise skip the animation the user is trying to view/capture).
    //    Independent of the training toggle — the flag only ever gates a photo-studio cut.
    let photo = il2cpp::class("Gallop.PhotoStudioCuttController");
    if photo.is_null() {
        notes.push_str("photo cut ctrl miss; ");
    } else {
        unsafe {
            if let Err(e) = install_one(photo, "PlayCutIn", 3, on_photo_play_cut as *const (), &TR_PHOTO_PLAY, &D_PHOTO_PLAY) {
                notes.push_str(&format!("photo play: {e}; "));
            }
            if let Err(e) = install_one(photo, "PlayCutInAsync", 1, on_photo_play_cut_async as *const (), &TR_PHOTO_ASYNC, &D_PHOTO_ASYNC) {
                notes.push_str(&format!("photo async: {e}; "));
            }
            if let Err(e) = install_one(photo, "OnEndCutIn", 0, on_photo_end_cut as *const (), &TR_PHOTO_END, &D_PHOTO_END) {
                notes.push_str(&format!("photo end: {e}; "));
            }
        }
    }

    // ── PRO SHOP (scenario free shop) buy/use animation skip ──
    let shop = il2cpp::class("Gallop.SingleModeScenarioFreeShopViewController");
    if shop.is_null() {
        notes.push_str("shop ctrl miss; ");
    } else {
        // Chara-message popup skip (the USE flow's "Use <item>" card) needs no
        // callback plumbing, so install it whether or not Action.Invoke resolved.
        unsafe {
            if let Err(e) = install_one(shop, "PlayCharaMessage", 1, on_chara_msg as *const (), &TR_CHARAMSG, &D_CHARAMSG) {
                notes.push_str(&format!("{e}; "));
            }
        }
        // The BUY performance skip defers the original callbacks, so it needs Action.Invoke.
        if !ACTION_INVOKE.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("shop: action.invoke miss; ");
        } else {
            unsafe {
                if let Err(e) = install_one(shop, "PlayUseItemPerformanceCore", 3, on_shop_perf as *const (), &TR_SHOPPERF, &D_SHOPPERF) {
                    notes.push_str(&format!("{e}; "));
                }
            }
        }
        // Post-BUY auto-close: arm the close countdown when the exchange completes (trace-confirmed
        // CallbackSendSingleModeFreeItemExchangeRequest fires right before the dialog appears).
        unsafe {
            if let Err(e) = install_one(shop, "CallbackSendSingleModeFreeItemExchangeRequest", 3,
                                        on_exch_request as *const (), &TR_EXCHCOMPLETE, &D_EXCHCOMPLETE) {
                notes.push_str(&format!("shop exch-req: {e}; "));
            }
        }
    }

    // ── PRO SHOP use-item animation (the chara-cheer "Use <item> / Stat Up" card) ──
    // Shared by buy→use-now AND the inventory "use item" dialog (which reaches the Parts
    // performance directly after the effect-list Decide). Skipping the performance drops
    // its coroutine, which normally ForceDestroys the open dialogs — so we replicate that
    // teardown in pump_pending_tag_cb (shop_teardown). Only arm if the teardown primitives
    // resolved AND GetForefrontDialog is static (no manager instance to call it on),
    // otherwise leave the animation playing rather than risk the modal-deadlock freeze.
    // Inventory use-item animation skip — drive the performance coroutine to completion on its
    // first MoveNext: the game runs its own cleanup (close dialogs, lift the input block) +
    // SingleMode continuation, we just collapse the inter-step visual waits so nothing renders.
    // External replication was impossible — that continuation lives inside the coroutine (the
    // callback arg is null). We do NOT skip PlayUseItemPerformance (it must run to start it).
    {
        let coro = il2cpp::nested_class(
            "Gallop.PartsSingleModeScenarioFreeUseItemPerformance",
            // 2026-07-01 update: same class, coroutine index renumbered d__14 -> d__13.
            "<PlayUseItemPerformanceCoroutine>d__13",
        );
        if coro.is_null() {
            notes.push_str("useperf coro miss; ");
        } else {
            unsafe {
                if let Err(e) = install_one(coro, "MoveNext", 0, on_movenext as *const (), &TR_MOVENEXT, &D_MOVENEXT) {
                    notes.push_str(&format!("coro movenext: {e}; "));
                }
            }
        }
        // Resolve the dialog primitives the post-use auto-close needs (close the item list so
        // the player lands on the career screen). GetForeFrontDialog is a static on DialogManager;
        // Close is the proper instance dismiss on DialogCommon.
        let dmgr = il2cpp::class("Gallop.DialogManager");
        let dcom = il2cpp::class("Gallop.DialogCommon");
        if !dmgr.is_null() {
            let _ = GET_FOREFRONT.set(resolve(dmgr, "GetForeFrontDialog", 0));
        }
        if !dcom.is_null() {
            let _ = DIALOG_CLOSE.set(resolve(dcom, "Close", 0));
        }
        if !GET_FOREFRONT.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("shop close-list: forefront miss; ");
        }
        if !DIALOG_CLOSE.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("shop close-list: close miss; ");
        }
    }

    // ── RIVAL-RACE entry intro ("RIVAL <name>" card) — skip its coroutine on the first step ──
    {
        // 2026-07-01 update: the rival entry moved out of SingleModeRaceEntryViewController into
        // Gallop.PartsRivalEntryAnimation, and DestroyRivalEntry was split — DestroyRivalEntryWithUnload
        // is the full teardown (unloads the zekken assets + calls DestroyRivalEntryAnimationObj).
        let entry = il2cpp::class("Gallop.PartsRivalEntryAnimation");
        if entry.is_null() {
            notes.push_str("rival entry cls miss; ");
        } else {
            let _ = DESTROY_RIVAL_ENTRY.set(resolve(entry, "DestroyRivalEntryWithUnload", 0));
            if !DESTROY_RIVAL_ENTRY.get().map(|i| i.ok()).unwrap_or(false) {
                notes.push_str("rival destroy miss; ");
            }
        }
        let rcoro = il2cpp::nested_class(
            "Gallop.PartsRivalEntryAnimation",
            "<PlayRivalEntryCoroutine>d__11",
        );
        if rcoro.is_null() {
            notes.push_str("rival coro miss; ");
        } else {
            unsafe {
                if let Err(e) = install_one(rcoro, "MoveNext", 0, on_rival_movenext as *const (), &TR_RIVALMN, &D_RIVALMN) {
                    notes.push_str(&format!("rival movenext: {e}; "));
                }
            }
        }
    }

    // ── EVENTS ──
    let view = il2cpp::class("Gallop.StoryViewController");
    let story = il2cpp::class("Gallop.StoryTimelineController");
    if view.is_null() {
        notes.push_str("story view miss; ");
    } else {
        let _ = SKIP_STORY.set(resolve(view, "SkipStory", 0));
        let _ = GET_TL.set(resolve(view, "get_TimelineController", 0));
        let _ = TRAIN_CUTT.set(resolve(view, "get_IsPlayingOrWillPlayTrainingCutt", 0));
        if !story.is_null() {
            let _ = IS_PLAYING.set(resolve(story, "get_IsPlaying", 0));
        }
        if !SKIP_STORY.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("story skip miss; ");
        } else {
            unsafe {
                match install_one(view, "OnStartPlayingTimeline", 0,
                                  on_start_timeline as *const (), &TR_TIMELINE, &D_TIMELINE) {
                    Ok(()) => events_ok = true,
                    Err(e) => notes.push_str(&format!("{e}; ")),
                }
            }
        }
    }

    (training_ok, events_ok, notes)
}
