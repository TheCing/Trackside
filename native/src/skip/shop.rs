//! Pro-Shop (scenario free shop) buy/use performance skip.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::hooks::in_heaven;
use crate::il2cpp;
use crate::skip::event::fire_action;
use crate::skip::{is_shop_enabled, now_ms, rr_log, Invokable, EVENT_SKIPS};

type BoolMethodFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;

// ── PRO SHOP (scenario free shop) buy/use performance skip ──────────────────
// SingleModeScenarioFreeShopViewController.PlayUseItemPerformanceCore(items, Action,
// Action) plays the buy/use flourish. The item effect is already applied by the server
// exchange/use request BEFORE this, so the performance is purely visual — skip it and
// fire its callbacks (deferred) to continue. One hook covers both buying and using.
static SHOP_PENDING: OnceLock<Mutex<Vec<usize>>> = OnceLock::new();
pub(crate) fn shop_pending() -> &'static Mutex<Vec<usize>> {
    SHOP_PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

// When we skip the inventory use-item performance, its coroutine (which we skipped)
// would normally ForceDestroy the open effect-list + user-item-list dialogs and reopen
// the list. We replicate that teardown next frame: get the forefront dialog and, while
// it's one of our two shop dialogs, ForceDestroy it; then the deferred callbacks reopen
// the list. Resolved in install(); the use-perf hook only arms if both are available.
pub(crate) static SHOP_TEARDOWN: AtomicBool = AtomicBool::new(false);
static PENDING_PARTS: AtomicUsize = AtomicUsize::new(0); // the use-item Parts to Release
pub(crate) static GET_FOREFRONT: OnceLock<Invokable> = OnceLock::new(); // DialogManager.GetForeFrontDialog (static)
static FORCE_DESTROY: OnceLock<Invokable> = OnceLock::new(); // DialogCommon.ForceDestroy (instance)
pub(crate) static DIALOG_CLOSE: OnceLock<Invokable> = OnceLock::new(); // DialogCommon.Close (proper dismiss: clears blur + state)
static PARTS_RELEASE: OnceLock<Invokable> = OnceLock::new(); // PartsSingleModeScenarioFreeUseItemPerformance.Release
pub(crate) static PLAY_CLOSE: OnceLock<Invokable> = OnceLock::new(); // SteamInputBlockManager.PlayClose (lift the input block)
pub(crate) static SIB_MGR: AtomicUsize = AtomicUsize::new(0); // captured SteamInputBlockManager instance
static SHOP_USE_CB: AtomicUsize = AtomicUsize::new(0); // the use-perf completion callback (continues SingleMode)

// Capture the SteamInputBlockManager instance from its normal PlayClose calls (it's a
// persistent manager; instance method, simple ABI). We need it to PlayClose the input
// block the skipped coroutine would otherwise have lifted (else input stays blocked).
crate::skip_hook_slot!(TR_SIBCLOSE, D_SIBCLOSE);
type SibCloseFn = unsafe extern "C" fn(*mut c_void, *mut c_void, bool, *mut c_void);
pub(crate) unsafe extern "C" fn on_sib_close(this: *mut c_void, action: *mut c_void, flag: bool, m: *mut c_void) {
    SIB_MGR.store(this as usize, Ordering::Relaxed);
    let t = TR_SIBCLOSE.load(Ordering::Relaxed);
    if t != 0 {
        let f: SibCloseFn = std::mem::transmute(t);
        f(this, action, flag, m);
    }
}

/// Dismiss the forefront dialog after a shop item-use (the `DialogSingleModeScenarioFreeUserItemList`
/// the drive returns you to). Uses the proper DialogCommon.Close (clears blur + state — NOT
/// ForceDestroy). GetForeFrontDialog returns the DialogCommon *container*; right after a use it
/// is the item list, so closing it once lands on the underlying career screen. Logs the class it
/// closes so diagnostics show whether it hit the list or something unexpected.
/// Returns true if it closed a dialog. Quiet on the no-dialog/null cases so the BUY retry loop
/// doesn't spam the log every frame while waiting for the dialog to appear.
pub(crate) fn shop_close_item_list() -> bool {
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

/// Replicate the skipped use-item coroutine's teardown: the normal coroutine ForceDestroys
/// the two open dialog CONTAINERS (the effect-list + user-item-list) and Releases the Parts.
/// GetForeFrontDialog returns the DialogCommon *container* (class "DialogCommon"), not the
/// content — so we destroy the forefront while it's a dialog container, capped at 2 (the
/// exact count the normal flow destroys), then Release the Parts. Deferred to a clean frame
/// (the ButtonCommon.Update pump) so it isn't re-entrant with the use-item button callback.
pub(crate) fn shop_teardown() {
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
crate::skip_hook_slot!(TR_MOVENEXT, D_MOVENEXT);
pub(crate) static DRIVING: AtomicBool = AtomicBool::new(false);
// Set after a shop action (inventory USE coroutine drive, or a BUY performance skip) → the next
// ButtonCommon pump auto-closes the leftover dialog so the player lands back on the underlying
// screen instead of on the item/buy dialog.
pub(crate) static SHOP_CLOSE_LIST: AtomicBool = AtomicBool::new(false);
// TIME deadlines (ms since `clock()` start), NOT frame counts — on_button_update / the pump run
// once PER ButtonCommon, so a frame counter burns out in a few real frames. A real-time window
// survives the dialog→shop transition. After a BUY we retry closing the exchange dialog until
// this deadline; once closed we arm the BackButton auto-press until its own deadline.
pub(crate) static SHOP_CLOSE_BUY_UNTIL: AtomicU64 = AtomicU64::new(0);
pub(crate) static SHOP_PRESS_BACK_UNTIL: AtomicU64 = AtomicU64::new(0);

pub(crate) unsafe extern "C" fn on_movenext(this: *mut c_void, m: *mut c_void) -> bool {
    let t = TR_MOVENEXT.load(Ordering::Relaxed);
    if t == 0 {
        return false;
    }
    let f: BoolMethodFn = std::mem::transmute(t);
    if !is_shop_enabled() || in_heaven() || DRIVING.load(Ordering::Relaxed) {
        return f(this, m); // normal single step (or a step during our own drive)
    }
    DRIVING.store(true, Ordering::Relaxed);
    let _g = crate::hooks::ReentryGuard::enter();
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

crate::skip_hook_slot!(TR_SHOPPERF, D_SHOPPERF);
type ShopPerfFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void);

pub(crate) unsafe extern "C" fn on_shop_perf(this: *mut c_void, items: *mut c_void, cb1: *mut c_void, cb2: *mut c_void, m: *mut c_void) {
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
crate::skip_hook_slot!(TR_EXCHCOMPLETE, D_EXCHCOMPLETE);
type ExchDlgFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void);
// Hooks CallbackSendSingleModeFreeItemExchangeRequest (the BUY/exchange completion, confirmed by
// trace) — the dialog appears right after. Arm the frame countdown so the pump closes it once it's
// up. BUY path only (inventory use never goes through the exchange request).
pub(crate) unsafe extern "C" fn on_exch_request(
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
crate::skip_hook_slot!(TR_CHARAMSG, D_CHARAMSG);
type CharaMsgFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);

pub(crate) unsafe extern "C" fn on_chara_msg(this: *mut c_void, q: *mut c_void, m: *mut c_void) {
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
crate::skip_hook_slot!(TR_USEPERF, D_USEPERF);
type UsePerfFn = unsafe extern "C" fn(
    *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void,
);
pub(crate) unsafe extern "C" fn on_use_perf(
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
crate::skip_hook_slot!(TR_USEPERFD, D_USEPERFD);
type UsePerfDFn =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void);
pub(crate) unsafe extern "C" fn on_use_perf_default(
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
