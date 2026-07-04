//! Race-result auto-advance (win/loss gating, whitelist, multi-press) +
//! SteamInputBlock lifter. Career-gated, default OFF unless `races_on`.
//!
//! ═══════════════════════════════════════════════════════════════════════════
//! B3b — RACE-RESULT AUTO-ADVANCE  (EXPERIMENTAL, default OFF)
//! Port of native_skip.js part 3. After "View Results" (+ the unavoidable 1st
//! tap), the result screens auto-press their own buttons to the next turn.
//! Untested in this native form → gated behind RACE_RESULT_ENABLED; enabling it
//! never touches the proven training/event core.
//! ═══════════════════════════════════════════════════════════════════════════

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::hooks::{in_heaven, ReentryGuard};
use crate::il2cpp;
use crate::skip::{
    call_orig, career_fresh, install_one, is_shop_enabled, mark_career, now_ms, rr_log,
    LAST_CAREER_MS,
};
use crate::ui_input::{auto_close, auto_press, button_name, click_now};

// Default ON in builds with feature `races_on`, OFF otherwise.
pub(crate) static RACE_RESULT_ENABLED: AtomicBool = AtomicBool::new(cfg!(feature = "races_on"));

// TEAM TRIALS guard (see the note in skip/mod.rs::set_in_team_trials). htt.rs sets this via
// crate::skip::set_in_team_trials; the career view-manager clears it. While set, result never fires.
pub(crate) static IN_TEAM_TRIALS: AtomicBool = AtomicBool::new(false);

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
    // screen on a loss. Placement + continues come from the response hooks (the response hook in
    // public, the response hook in private). continues == -1 means "unknown" → fall back
    // to the win-only gate. Both builds ship `raceread`.
    #[cfg(feature = "raceread")]
    {
        let won = crate::race::player_finish_order() == 1;
        let no_retries_left = crate::race::continues_available() == 0;
        won || no_retries_left
    }
}

pub(crate) const PRESS_GAP_MS: u64 = 130;
pub(crate) const MULTI_MAX: u32 = 4;
// EXACT whitelist + exact match (substring matching caused mis-presses). The
// (substring matching caused mis-presses, so we match the exact button names).
pub(crate) fn is_press_target(name: &str) -> bool {
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
pub(crate) fn is_multi(name: &str) -> bool {
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
pub(crate) fn press_allowed(name: &str) -> bool {
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

// Single global busy flag lives in ui_input (BUSY). Window/press counters below.
pub(crate) static WINDOW_OPEN: AtomicBool = AtomicBool::new(false);
pub(crate) static RR_PRESSES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn clear_rr_caches() {
    crate::ui_input::clear_caches();
    RR_NEXT_LIFT.store(0, Ordering::Relaxed);
}

// Stuck-advance-button input-block lift. The victory-concert result screen (and similar) keeps a
// SteamInputBlock(Clone) up that never lifts on its own, leaving Next/Continue locked forever so
// the skip stalls. 0 = not timing yet; otherwise the earliest ms at which we may PlayClose. Reset
// on a successful press and on race end (clear_rr_caches).
pub(crate) static RR_NEXT_LIFT: AtomicU64 = AtomicU64::new(0);
pub(crate) const RR_LIFT_GRACE_MS: u64 = 600; // stay locked this long before lifting (don't fight normal transient locks)
pub(crate) const RR_LIFT_THROTTLE_MS: u64 = 400; // re-lift at most this often while still stuck

pub(crate) fn is_advance_button(name: &str) -> bool {
    name == "NextButton" || name == "SingleModeNextButton" || name == "ContinueButton"
}

/// Lift the SteamInputBlock the skipped result coroutine would have lifted (the same PlayClose the
/// shop-skip uses) so a stuck-locked advance button unlocks. No-op until SIB_MGR is captured.
pub(crate) fn lift_input_block() {
    use crate::skip::shop::{PLAY_CLOSE, SIB_MGR};
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

// Detours for race-result.
type Void3 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type Push1 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
type Push2 = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
crate::skip_hook_slot!(TR_UPDATE, D_UPDATE);
crate::skip_hook_slot!(TR_ONPC, D_ONPC);
crate::skip_hook_slot!(TR_CMV, D_CMV);
crate::skip_hook_slot!(TR_HOME, D_HOME); // HomeViewController.PlayInView — reached the lobby = left career
crate::skip_hook_slot!(TR_PUSH1, D_PUSH1);
crate::skip_hook_slot!(TR_PUSH2, D_PUSH2);

unsafe extern "C" fn on_button_update(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_UPDATE, this, m);
    crate::skip::event::pump_pending_tag_cb(); // fire deferred friendship-splash onDone on a clean frame
    // After a buy auto-closes its dialog, auto-press the shop "BackButton" so the player lands
    // where their manual Back would (this Update fires per ButtonCommon, so we catch BackButton
    // when it's this one). Frame-windowed + shop-gated so it never fires elsewhere.
    if now_ms() < crate::skip::shop::SHOP_PRESS_BACK_UNTIL.load(Ordering::Relaxed) && is_shop_enabled() && !in_heaven() {
        if button_name(this) == "BackButton" {
            let _g = ReentryGuard::enter();
            if click_now(this) {
                crate::skip::shop::SHOP_PRESS_BACK_UNTIL.store(0, Ordering::Relaxed);
                rr_log(&format!("[shop {}ms] auto-pressed BackButton", now_ms()));
            }
        }
    }
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
pub(crate) fn install_race_result() -> Result<String, String> {
    use crate::ui_input::{
        C_PED, CLOSE_C, CLOSE_MI, CTOR_C, CTOR_MI, CUR_C, CUR_MI, GETNAME_C, GETNAME_MI, ISLOCK_C,
        ISLOCK_MI, OPC_C, OPC_MI,
    };
    use std::sync::atomic::AtomicUsize;
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
