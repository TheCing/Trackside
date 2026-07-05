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
//!
//! The feature is split into focused submodules; this file holds the shared
//! infra (enable flags, the Invokable ABI, the hook-slot macro, the career
//! heartbeat, stats) plus the two install entry points (`install` /
//! `install_race_result`) that boot.rs calls. The submodules own their own
//! detour fns, hook slots and resolved-method statics:
//!   - `train`  — training cut-in skip + photo-studio guard
//!   - `event`  — event/story skip + friendship TAG splash + deferred pump
//!   - `shop`   — pro-shop buy/use performance skip
//!   - `rival`  — rival entry cut-in skip
//!   - `result` — race-result auto-advance + SteamInputBlock lifter
//! The generic click engine lives in the sibling `crate::ui_input` module.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use retour::RawDetour;

use crate::il2cpp;

pub(crate) mod event;
pub(crate) mod result;
pub(crate) mod rival;
pub(crate) mod shop;
pub(crate) mod train;

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
pub fn set_scene_enabled(_on: bool) {}
pub fn is_scene_enabled() -> bool {
    false
}

/// Apply persisted settings to the skip subsystem at boot.
pub fn apply(s: &crate::settings::Settings) {
    set_train_enabled(s.skip_training);
    set_event_enabled(s.skip_events);
    set_shop_enabled(s.skip_shop);
    set_rival_enabled(s.skip_rival);
    set_scene_enabled(s.skip_scene_cutt);
    set_race_result_enabled(s.race_result);
}

fn scene_driving() -> bool {
    false
}

pub fn diag() -> String {
    use std::sync::atomic::Ordering::Relaxed;
    format!(
        "enabled: train={} event={} shop={} rival={} scene={} race_result={}\n  \
         gates: in_team_trials={} window_open={} busy={} driving={} scene_driving={}\n  \
         counts: train_skips={} event_skips={} rr_presses={}",
        SKIP_ENABLED.load(Relaxed),
        EVENT_ENABLED.load(Relaxed),
        SHOP_ENABLED.load(Relaxed),
        RIVAL_ENABLED.load(Relaxed),
        is_scene_enabled(),
        result::RACE_RESULT_ENABLED.load(Relaxed),
        result::IN_TEAM_TRIALS.load(Relaxed),
        result::WINDOW_OPEN.load(Relaxed),
        crate::ui_input::BUSY.load(Relaxed),
        shop::DRIVING.load(Relaxed),
        scene_driving(),
        TRAIN_SKIPS.load(Relaxed),
        EVENT_SKIPS.load(Relaxed),
        result::RR_PRESSES.load(Relaxed),
    )
}

// ── method ABIs (this, MethodInfo*) ─────────────────────────────────────────
pub(crate) type VoidM = unsafe extern "C" fn(*mut c_void, *mut c_void);
pub(crate) type PtrM = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
pub(crate) type BoolM = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;

/// A resolved invokable method: (compiled code ptr, MethodInfo*).
#[derive(Clone, Copy)]
pub(crate) struct Invokable {
    pub(crate) code: usize,
    pub(crate) mi: usize,
}
impl Invokable {
    pub(crate) const NONE: Invokable = Invokable { code: 0, mi: 0 };
    pub(crate) fn ok(&self) -> bool {
        self.code != 0
    }
    pub(crate) unsafe fn call_void(&self, this: *mut c_void) {
        if self.code != 0 {
            let f: VoidM = std::mem::transmute(self.code);
            f(this, self.mi as *mut c_void);
        }
    }
    pub(crate) unsafe fn call_ptr(&self, this: *mut c_void) -> *mut c_void {
        if self.code == 0 {
            return std::ptr::null_mut();
        }
        let f: PtrM = std::mem::transmute(self.code);
        f(this, self.mi as *mut c_void)
    }
    pub(crate) unsafe fn call_bool(&self, this: *mut c_void) -> bool {
        if self.code == 0 {
            return false;
        }
        let f: BoolM = std::mem::transmute(self.code);
        f(this, self.mi as *mut c_void)
    }
    /// Call a STATIC 0-arg method returning a pointer: ABI is `f(MethodInfo*)`.
    pub(crate) unsafe fn call_ptr_static(&self) -> *mut c_void {
        if self.code == 0 {
            return std::ptr::null_mut();
        }
        let f: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(self.code);
        f(self.mi as *mut c_void)
    }
    /// SteamInputBlockManager.PlayClose(Action onComplete, bool immediate): lift the input
    /// block. null completion callback, `flag` controls immediate vs animated.
    pub(crate) unsafe fn call_play_close(&self, this: *mut c_void, flag: bool) {
        if self.code == 0 {
            return;
        }
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void, bool, *mut c_void) = std::mem::transmute(self.code);
        f(this, std::ptr::null_mut(), flag, self.mi as *mut c_void);
    }
}

pub(crate) fn resolve(klass: il2cpp::Class, name: &str, argc: i32) -> Invokable {
    resolve_m(il2cpp::method(klass, name, argc))
}

pub(crate) fn resolve_m(m: il2cpp::Method) -> Invokable {
    if m.is_null() {
        return Invokable::NONE;
    }
    let code = il2cpp::method_pointer(m) as usize;
    Invokable { code, mi: m as usize }
}

// Stats.
pub(crate) static TRAIN_SKIPS: AtomicU64 = AtomicU64::new(0);
pub(crate) static EVENT_SKIPS: AtomicU64 = AtomicU64::new(0);
pub fn stats() -> (u64, u64) {
    (TRAIN_SKIPS.load(Ordering::Relaxed), EVENT_SKIPS.load(Ordering::Relaxed))
}

pub(crate) fn clock() -> &'static Instant {
    crate::tools::clock()
}

pub(crate) fn now_ms() -> u64 {
    crate::tools::now_ms()
}

/// Append a line to the native engine log (race-result diagnostics).
pub(crate) fn rr_log(msg: &str) {
    crate::tools::log(msg);
}

// ── trampolines (keep detours alive + call originals) ───────────────────────
#[macro_export]
macro_rules! skip_hook_slot {
    ($tramp:ident, $det:ident) => {
        pub(crate) static $tramp: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        pub(crate) static $det: std::sync::OnceLock<retour::RawDetour> = std::sync::OnceLock::new();
    };
}

#[inline]
pub(crate) unsafe fn call_orig(tramp: &AtomicUsize, this: *mut c_void, method: *mut c_void) {
    let t = tramp.load(Ordering::Relaxed);
    if t != 0 {
        let f: VoidM = std::mem::transmute(t);
        f(this, method);
    }
}

// ── career heartbeat ─────────────────────────────────────────────────────────
// The race-result auto-advance is a CAREER (single-mode) feature, but its press targets are
// generic button names that also exist in other modes' menus — so if it armed in a non-career
// race it kept auto-pressing menu buttons there (nothing clears the window outside career, since
// only the single-mode ChangeMainView does). Fix: a positive gate. Every SingleMode (career-only)
// hook stamps this heartbeat; the result skip only ARMS while the heartbeat is fresh — so it can
// never arm or leak outside a career run.
pub(crate) static LAST_CAREER_MS: AtomicU64 = AtomicU64::new(0);
/// Mark that a career (single-mode) hook just fired. Called from the SingleMode-only detours.
pub(crate) fn mark_career() {
    // Log only the OPEN transition (gate was closed → now in career), so the log isn't spammed by
    // the many ChangeMainView calls within a run.
    if LAST_CAREER_MS.swap(now_ms(), Ordering::Relaxed) == 0 {
        rr_log("[race-result] career detected (ChangeMainView) -> gate OPEN (skip may arm now)");
    }
}
/// True if a career hook fired recently enough to still be in this career run (window spans a full
/// race + the pre-race menu, so a legit career result is never blocked; goes stale after you leave).
pub(crate) fn career_fresh() -> bool {
    LAST_CAREER_MS.load(Ordering::Relaxed) != 0
        && now_ms().saturating_sub(LAST_CAREER_MS.load(Ordering::Relaxed)) < 360_000
}

// TEAM TRIALS guard. The race-result auto-advance is a CAREER (single-mode) feature,
// but its anchor button ("RaceSkipButton") and press targets also exist on the Team
// Trials result screen — so without this it auto-pressed buttons there and got the TT
// result UI stuck (the long-standing v2.2 bug). htt.rs sets this true when a Team
// Trials result is built; the career view-manager (ChangeMainView, single-mode only)
// clears it when we're back in career. While set, race-result never fires.
pub fn set_in_team_trials(on: bool) {
    result::IN_TEAM_TRIALS.store(on, Ordering::Relaxed);
}

pub fn set_race_result_enabled(on: bool) {
    result::RACE_RESULT_ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_race_result_enabled() -> bool {
    result::RACE_RESULT_ENABLED.load(Ordering::Relaxed)
}

pub fn race_result_stats() -> (bool, u64) {
    (
        result::WINDOW_OPEN.load(Ordering::Relaxed),
        result::RR_PRESSES.load(Ordering::Relaxed),
    )
}

// ── install ─────────────────────────────────────────────────────────────────
pub(crate) unsafe fn install_one(
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

/// Install race-result auto-advance (faithful port of native_skip.js part 3).
/// Independent of training/events. Returns Ok(note) describing what resolved.
pub fn install_race_result() -> Result<String, String> {
    result::install_race_result()
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
        let _ = train::SKIP_RUNTIME.set(resolve(helper, "SkipRuntime", 0));
        if !train::SKIP_RUNTIME.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("train skip miss; ");
        } else {
            let mut any = false;
            unsafe {
                for r in [
                    install_one(helper, "OnStartCutIn", 0, train::on_start_cutin as *const (), &train::TR_START, &train::D_START),
                    install_one(helper, "OnPlayCutIn", 0, train::on_play_cutin as *const (), &train::TR_PLAY, &train::D_PLAY),
                    install_one(helper, "OnPlayMainCutIn", 0, train::on_play_main_cutin as *const (), &train::TR_MAIN, &train::D_MAIN),
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
    let _ = event::ACTION_INVOKE.set(resolve(il2cpp::class("System.Action"), "Invoke", 0));
    let _ = event::SET_ACTIVE.set(resolve(il2cpp::class("UnityEngine.GameObject"), "SetActive", 1));
    let tag = il2cpp::class("Gallop.SingleModeMainViewTagTrainingCutInPlayer");
    if tag.is_null() {
        notes.push_str("tag cutin miss; ");
    } else if !event::ACTION_INVOKE.get().map(|i| i.ok()).unwrap_or(false) {
        notes.push_str("action.invoke miss; ");
    } else {
        unsafe {
            if let Err(e) = install_one(tag, "PlayCutIn", 2, event::on_tag_play_cutin as *const (), &event::TR_TAGIN, &event::D_TAGIN) {
                notes.push_str(&format!("{e}; "));
            }
            let _ = install_one(tag, "PlayCutInOut", 1, event::on_tag_play_cutin_out as *const (), &event::TR_TAGOUT, &event::D_TAGOUT);
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
            if let Err(e) = install_one(photo, "PlayCutIn", 3, train::on_photo_play_cut as *const (), &train::TR_PHOTO_PLAY, &train::D_PHOTO_PLAY) {
                notes.push_str(&format!("photo play: {e}; "));
            }
            if let Err(e) = install_one(photo, "PlayCutInAsync", 1, train::on_photo_play_cut_async as *const (), &train::TR_PHOTO_ASYNC, &train::D_PHOTO_ASYNC) {
                notes.push_str(&format!("photo async: {e}; "));
            }
            if let Err(e) = install_one(photo, "OnEndCutIn", 0, train::on_photo_end_cut as *const (), &train::TR_PHOTO_END, &train::D_PHOTO_END) {
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
            if let Err(e) = install_one(shop, "PlayCharaMessage", 1, shop::on_chara_msg as *const (), &shop::TR_CHARAMSG, &shop::D_CHARAMSG) {
                notes.push_str(&format!("{e}; "));
            }
        }
        // The BUY performance skip defers the original callbacks, so it needs Action.Invoke.
        if !event::ACTION_INVOKE.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("shop: action.invoke miss; ");
        } else {
            unsafe {
                if let Err(e) = install_one(shop, "PlayUseItemPerformanceCore", 3, shop::on_shop_perf as *const (), &shop::TR_SHOPPERF, &shop::D_SHOPPERF) {
                    notes.push_str(&format!("{e}; "));
                }
            }
        }
        // Post-BUY auto-close: arm the close countdown when the exchange completes (trace-confirmed
        // CallbackSendSingleModeFreeItemExchangeRequest fires right before the dialog appears).
        unsafe {
            if let Err(e) = install_one(shop, "CallbackSendSingleModeFreeItemExchangeRequest", 3,
                                        shop::on_exch_request as *const (), &shop::TR_EXCHCOMPLETE, &shop::D_EXCHCOMPLETE) {
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
                if let Err(e) = install_one(coro, "MoveNext", 0, shop::on_movenext as *const (), &shop::TR_MOVENEXT, &shop::D_MOVENEXT) {
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
            let _ = shop::GET_FOREFRONT.set(resolve(dmgr, "GetForeFrontDialog", 0));
        }
        if !dcom.is_null() {
            let _ = shop::DIALOG_CLOSE.set(resolve(dcom, "Close", 0));
        }
        if !shop::GET_FOREFRONT.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("shop close-list: forefront miss; ");
        }
        if !shop::DIALOG_CLOSE.get().map(|i| i.ok()).unwrap_or(false) {
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
            let _ = rival::DESTROY_RIVAL_ENTRY.set(resolve(entry, "DestroyRivalEntryWithUnload", 0));
            if !rival::DESTROY_RIVAL_ENTRY.get().map(|i| i.ok()).unwrap_or(false) {
                notes.push_str("rival destroy miss; ");
            }
        }
        let rcoro = il2cpp::nested_class(
            "Gallop.PartsRivalEntryAnimation",
            "<PlayRivalEntryCoroutine>d__11",
        );
        // The skip is now context-gated in rival::on_rival_movenext: it is SUPPRESSED when the
        // coroutine's endAction targets a paddock view controller (the URA/scenario-finals rival
        // intro is embedded in the paddock as the "VsUniqueNpcEntry" step, and firing that
        // continuation early corrupted the paddock — default 9999 stats). Normal rival races (entry
        // card outside the paddock) still skip. (Root-caused 2026-07-05 from a live field dump.)
        if rcoro.is_null() {
            notes.push_str("rival coro miss; ");
        } else {
            unsafe {
                if let Err(e) = install_one(rcoro, "MoveNext", 0, rival::on_rival_movenext as *const (), &rival::TR_RIVALMN, &rival::D_RIVALMN) {
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
        let _ = event::SKIP_STORY.set(resolve(view, "SkipStory", 0));
        let _ = event::GET_TL.set(resolve(view, "get_TimelineController", 0));
        let _ = event::TRAIN_CUTT.set(resolve(view, "get_IsPlayingOrWillPlayTrainingCutt", 0));
        if !story.is_null() {
            let _ = event::IS_PLAYING.set(resolve(story, "get_IsPlaying", 0));
        }
        if !event::SKIP_STORY.get().map(|i| i.ok()).unwrap_or(false) {
            notes.push_str("story skip miss; ");
        } else {
            unsafe {
                match install_one(view, "OnStartPlayingTimeline", 0,
                                  event::on_start_timeline as *const (), &event::TR_TIMELINE, &event::D_TIMELINE) {
                    Ok(()) => events_ok = true,
                    Err(e) => notes.push_str(&format!("{e}; ")),
                }
            }
        }
    }

    // ── Goal-Complete FREEZE guard ── suppress the event skip while the "All goals achieved" screen is
    // up (SkipStory there hangs the game in a DialogManager z-order loop). Hook the SAFE (void) BeginView.
    {
        let ccc = il2cpp::class("Gallop.SingleModeConfirmCompleteViewController");
        if ccc.is_null() {
            notes.push_str("goal-complete vc miss; ");
        } else {
            unsafe {
                // RegisterDownload fires EARLIEST (asset preload, before the corrupting story) — primary arm.
                if let Err(e) = install_one(ccc, "RegisterDownload", 1, event::on_goal_complete_register as *const (),
                                            &event::TR_GCREG, &event::D_GCREG) {
                    notes.push_str(&format!("goal-complete reg: {e}; "));
                }
                // BeginView backup.
                if let Err(e) = install_one(ccc, "BeginView", 0, event::on_goal_complete_begin as *const (),
                                            &event::TR_GOALBEGIN, &event::D_GOALBEGIN) {
                    notes.push_str(&format!("goal-complete begin: {e}; "));
                }
            }
        }
    }

    (training_ok, events_ok, notes)
}
