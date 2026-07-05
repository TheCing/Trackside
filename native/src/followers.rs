//! Auto-unfollow — bulk-remove followers via REAL UI clicks (Vía A: human-paced, NO direct API).
//!
//! It drives the game's own button flow exactly as a human would, one follower per cycle:
//!   1) click the first follower list item   (ButtonCommon "utx_frm_list_base_00_sl")
//!   2) click "Remove Follower"               (ButtonCommon "ButtonM01")
//!   3) click the confirm dialog's Close      (ButtonCommon "ButtonCenter")
//!   4) click the "removed" notice's Close    (ButtonCommon "ButtonCenter")
//!   5) reload the list so the next follower moves to the top — FriendViewController.OnSelectTab
//!      to Recommended then back to Followers (the removed one stays PINNED at the top otherwise,
//!      and re-running the flow on it would re-FOLLOW them, per the user's own finding).
//!
//! Each step is a real ButtonCommon click via ui_input, spaced by a human-like throttle + jitter, so
//! the server sees the same request stream a person clicking by hand would — no burst, no advantage.
//! Driven from the ButtonCommon.Update pump (result.rs::on_button_update). Default OFF, manual toggle.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::OnceLock;

use crate::skip::{now_ms, rr_log, Invokable};
use crate::ui_input::{button_name, click_now};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: AtomicU8 = AtomicU8::new(0);
static NEXT_AT: AtomicU64 = AtomicU64::new(0);
static REMOVED: AtomicU64 = AtomicU64::new(0);
static FVC: AtomicUsize = AtomicUsize::new(0); // captured FriendViewController instance
pub(crate) static ON_SELECT_TAB: OnceLock<Invokable> = OnceLock::new(); // FriendViewController.OnSelectTab(int)

// Target ButtonCommon names (from a live capture of the manual flow).
const B_FOLLOWER: &str = "utx_frm_list_base_00_sl";
const B_REMOVE: &str = "ButtonM01";
const B_CLOSE: &str = "ButtonCenter";
// Tab indices on the Friends screen: 0=Following, 1=Followers, 2=Recommended.
const TAB_FOLLOWERS: i32 = 1;
const TAB_RECOMMENDED: i32 = 2;

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if on {
        STATE.store(0, Ordering::Relaxed);
        NEXT_AT.store(0, Ordering::Relaxed);
        rr_log("[unfollow] STARTED (auto-remove followers)");
    } else {
        rr_log(&format!("[unfollow] STOPPED (removed {} this session)", REMOVED.load(Ordering::Relaxed)));
    }
}
pub fn removed_count() -> u64 {
    REMOVED.load(Ordering::Relaxed)
}

// Human-like step delay: 450–750 ms, varied per-removal so the cadence isn't a fixed metronome.
fn step_delay() -> u64 {
    let seed = REMOVED.load(Ordering::Relaxed).wrapping_add(STATE.load(Ordering::Relaxed) as u64);
    450 + ((seed.wrapping_mul(2654435761) >> 11) % 300)
}
fn advance(next: u8) {
    STATE.store(next, Ordering::Relaxed);
    NEXT_AT.store(now_ms() + step_delay(), Ordering::Relaxed);
}

unsafe fn select_tab(idx: i32) -> bool {
    let fvc = FVC.load(Ordering::Relaxed);
    if fvc == 0 {
        return false;
    }
    if let Some(inv) = ON_SELECT_TAB.get() {
        if inv.ok() {
            let f: unsafe extern "C" fn(*mut c_void, i32, *mut c_void) = std::mem::transmute(inv.code);
            f(fvc as *mut c_void, idx, inv.mi as *mut c_void);
            return true;
        }
    }
    false
}

/// Resolve OnSelectTab + hook SetScrollView to capture the FriendViewController instance. Returns a
/// status note for the boot log. Non-fatal: if it misses, the toggle just does nothing.
pub fn install() -> String {
    let mut notes = String::new();
    let fvc = crate::il2cpp::class("Gallop.FriendViewController");
    if fvc.is_null() {
        return "FriendViewController miss".into();
    }
    let _ = ON_SELECT_TAB.set(crate::skip::resolve(fvc, "OnSelectTab", 1));
    if !ON_SELECT_TAB.get().map(|i| i.ok()).unwrap_or(false) {
        notes.push_str("OnSelectTab miss; ");
    }
    unsafe {
        if let Err(e) = crate::skip::install_one(fvc, "SetScrollView", 0, on_fvc_scroll as *const (), &TR_FVC, &D_FVC) {
            notes.push_str(&format!("SetScrollView: {e}; "));
        }
    }
    if notes.is_empty() {
        notes.push_str("ok");
    }
    notes
}

// Capture the FriendViewController instance (hook on SetScrollView, which runs when the friend list
// is (re)built). We need it to call OnSelectTab for the list reload.
crate::skip_hook_slot!(TR_FVC, D_FVC);
pub(crate) unsafe extern "C" fn on_fvc_scroll(this: *mut c_void, m: *mut c_void) {
    FVC.store(this as usize, Ordering::Relaxed);
    crate::skip::call_orig(&TR_FVC, this, m);
}

/// Pump one step. Called from on_button_update per ButtonCommon. Self-gates on the enable flag and the
/// throttle; steps 0–3 fire only when the matching button is the one being updated, steps 4–5 fire the
/// list reload on any pump tick.
pub(crate) fn pump(this: *mut c_void) {
    if !is_enabled() || now_ms() < NEXT_AT.load(Ordering::Relaxed) {
        return;
    }
    match STATE.load(Ordering::Relaxed) {
        0 => {
            if button_name(this) == B_FOLLOWER && unsafe { click_now(this) } {
                rr_log("[unfollow] step: opened first follower");
                advance(1);
            }
        }
        1 => {
            if button_name(this) == B_REMOVE && unsafe { click_now(this) } {
                advance(2);
            }
        }
        2 => {
            if button_name(this) == B_CLOSE && unsafe { click_now(this) } {
                advance(3);
            }
        }
        3 => {
            if button_name(this) == B_CLOSE && unsafe { click_now(this) } {
                REMOVED.fetch_add(1, Ordering::Relaxed);
                rr_log(&format!("[unfollow] removed #{}", REMOVED.load(Ordering::Relaxed)));
                advance(4);
            }
        }
        4 => {
            // reload: go to Recommended (drops the just-removed pinned entry)…
            if unsafe { select_tab(TAB_RECOMMENDED) } {
                advance(5);
            }
        }
        5 => {
            // …and back to Followers, so the NEXT follower is now the top item.
            if unsafe { select_tab(TAB_FOLLOWERS) } {
                advance(0);
            }
        }
        _ => {}
    }
}
