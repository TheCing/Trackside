//! Event/story skip + friendship "TAG" splash skip + deferred pump.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use crate::hooks::{in_heaven, ReentryGuard};
use crate::skip::{
    call_orig, clock, is_enabled, is_event_enabled, now_ms, rr_log, Invokable, EVENT_SKIPS, TRAIN_SKIPS,
};

// Debounce only swallows a same-timeline re-fire of OnStartPlayingTimeline (a few frames). It must
// stay SHORT because it's WALL-clock, not game time — at high UI speed (e.g. 10x) distinct events
// compress into a small real-time window, and a long debounce was dropping the 2nd/3rd event ("some
// texts don't skip"). The IS_PLAYING check below is the real guard against re-skipping a finished one.
const EVENT_DEBOUNCE_MS: u64 = 100;

// Invokables (set in install).
pub(crate) static SKIP_STORY: OnceLock<Invokable> = OnceLock::new(); // events
pub(crate) static GET_TL: OnceLock<Invokable> = OnceLock::new(); // StoryViewController.get_TimelineController
pub(crate) static IS_PLAYING: OnceLock<Invokable> = OnceLock::new(); // StoryTimelineController.get_IsPlaying
pub(crate) static TRAIN_CUTT: OnceLock<Invokable> = OnceLock::new(); // get_IsPlayingOrWillPlayTrainingCutt

crate::skip_hook_slot!(TR_TIMELINE, D_TIMELINE);
crate::skip_hook_slot!(TR_TAGIN, D_TAGIN); // SingleModeMainViewTagTrainingCutInPlayer.PlayCutIn
crate::skip_hook_slot!(TR_TAGOUT, D_TAGOUT); // .PlayCutInOut

// ── "Goal Complete" event guard ─────────────────────────────────────────────
// The "All goals achieved / GOAL COMPLETE" event (SingleModeConfirmCompleteViewController, shown
// before the URA-Finale race) plays a story. SkipStory'ing THAT one deadlocks the game — confirmed:
// turning the event-skip OFF lets it pass. So while that screen is up, the event-skip stands down
// (== "Events OFF" but only there, automatic). Set on ConfirmComplete.PlayInView, cleared at Home.
pub(crate) static CAREER_END: AtomicBool = AtomicBool::new(false);
crate::skip_hook_slot!(TR_CONFIRM, D_CONFIRM); // SingleModeConfirmCompleteViewController.PlayInView

/// PlayInView of the "Goal Complete" screen — suspend the event-skip until Home.
pub(crate) unsafe extern "C" fn on_confirm_complete_in(this: *mut c_void, m: *mut c_void) -> *mut c_void {
    CAREER_END.store(true, Ordering::Relaxed);
    rr_log("[event] Goal Complete screen — event-skip suspended (anti-deadlock)");
    let t = TR_CONFIRM.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(t);
        f(this, m)
    } else {
        std::ptr::null_mut()
    }
}

// ── EVENTS: SkipStory on OnStartPlayingTimeline (guarded + debounced). ──────
static LAST_EVENT_SKIP_MS: AtomicU64 = AtomicU64::new(0);
fn try_event_skip(this: *mut c_void) {
    if !is_event_enabled() || in_heaven() || this.is_null() {
        return;
    }
    if CAREER_END.load(Ordering::Relaxed) {
        rr_log("[event] ignored (Goal Complete guard)");
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
pub(crate) unsafe extern "C" fn on_start_timeline(this: *mut c_void, m: *mut c_void) {
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
pub(crate) static ACTION_INVOKE: OnceLock<Invokable> = OnceLock::new(); // System.Action.Invoke
pub(crate) static SET_ACTIVE: OnceLock<Invokable> = OnceLock::new(); // GameObject.SetActive(bool)
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

pub(crate) unsafe extern "C" fn on_tag_play_cutin(this: *mut c_void, cards: *mut c_void, cb: *mut c_void, m: *mut c_void) {
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
pub(crate) unsafe extern "C" fn on_tag_play_cutin_out(this: *mut c_void, cb: *mut c_void, m: *mut c_void) {
    let t = TR_TAGOUT.load(Ordering::Relaxed);
    if t != 0 {
        let f: TagOutFn = std::mem::transmute(t);
        f(this, cb, m);
    }
    if is_enabled() && !in_heaven() && !cb.is_null() {
        PENDING_TAG_CB.store(cb as usize, Ordering::Relaxed);
    }
}

pub(crate) unsafe fn fire_action(cb: usize) {
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
pub(crate) fn pump_pending_tag_cb() {
    use crate::skip::shop;
    unsafe { fire_action(PENDING_TAG_CB.swap(0, Ordering::Relaxed)) };
    // Close the skipped use-item performance's dialogs BEFORE its callbacks reopen the list.
    if shop::SHOP_TEARDOWN.swap(false, Ordering::Relaxed) {
        let _g = ReentryGuard::enter();
        shop::shop_teardown();
    }
    let cbs: Vec<usize> = shop::shop_pending().lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default();
    for c in cbs {
        unsafe { fire_action(c) };
    }
    // After a use-item performance was driven to completion (coroutine), auto-close the item
    // list so the player lands on the career screen instead of back on the list.
    if shop::SHOP_CLOSE_LIST.swap(false, Ordering::Relaxed) {
        let _g = ReentryGuard::enter();
        let _ = shop::shop_close_item_list();
        // A NORMAL-item buy shows the exchange-complete dialog via this same use-performance
        // coroutine, so it's THIS path (not the buy-path) that dismisses the dialog. If a buy is
        // pending, arm Back right now instead of letting the buy-path wait out its timeout.
        if shop::SHOP_CLOSE_BUY_UNTIL.swap(0, Ordering::Relaxed) != 0 {
            shop::SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
            rr_log(&format!("[shop {}ms] dialog dismissed during buy; back-press armed", now_ms()));
        }
    }
    // After a BUY (exchange), the "Exchange Complete / choose how many" dialog appears a few
    // frames LATER — so we retry closing the forefront dialog over a short window until it shows.
    let close_until = shop::SHOP_CLOSE_BUY_UNTIL.load(Ordering::Relaxed);
    if close_until != 0 {
        if now_ms() < close_until {
            // Normal item: the exchange-complete dialog appears → close it, then arm Back.
            let _g = ReentryGuard::enter();
            if shop::shop_close_item_list() {
                shop::SHOP_CLOSE_BUY_UNTIL.store(0, Ordering::Relaxed);
                shop::SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
                rr_log(&format!("[shop {}ms] buy dialog closed; back-press armed", now_ms()));
            }
        } else {
            // Window expired with no dialog ever appearing → it was an AUTO-REDEEM item (stat
            // books etc. apply instantly, no "choose how many" dialog). Just arm Back so the
            // player still lands where their manual Back would.
            shop::SHOP_CLOSE_BUY_UNTIL.store(0, Ordering::Relaxed);
            shop::SHOP_PRESS_BACK_UNTIL.store(now_ms() + 5000, Ordering::Relaxed);
            rr_log(&format!("[shop {}ms] auto-redeem (no dialog); back-press armed", now_ms()));
        }
    }
}
