//! Rival-race entry cut-in skip.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

use crate::hooks::{in_heaven, ReentryGuard};
use crate::skip::event::fire_action;
use crate::skip::{is_rival_enabled, rr_log, Invokable};

type BoolMethodFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;

// Skip the full-screen rival ENTRY cut-in (the 2D "RIVAL <name>" card shown before a rival
// race). It is played by SingleModeRaceEntryViewController.<PlayRivalEntryCoroutine>d__103.
// On its FIRST MoveNext (state 0) we set the state field to -1 so the body falls through to
// the default case and renders nothing, then call DestroyRivalEntry() to clear any partial
// visuals and invoke the coroutine's endAction so the flow proceeds straight to the race.
// (Driving the coroutine to completion does NOT work here — its first step yields on the
// rival model/asset load, never advancing the on-screen card; this early-skip does.)
crate::skip_hook_slot!(TR_RIVALMN, D_RIVALMN);
pub(crate) static DESTROY_RIVAL_ENTRY: OnceLock<Invokable> = OnceLock::new(); // PartsRivalEntryAnimation.DestroyRivalEntryWithUnload
const O_RIVAL_STATE: usize = 0x10; // <>1__state
const O_RIVAL_ENDACTION: usize = 0x20; // endAction (System.Action)
// 2026-07-01 update: coroutine moved to Gallop.PartsRivalEntryAnimation.d__11; a new
// itemIconList field @0x28 pushed <>4__this from 0x28 -> 0x30.
const O_RIVAL_THIS: usize = 0x30; // <>4__this (PartsRivalEntryAnimation)
pub(crate) unsafe extern "C" fn on_rival_movenext(this: *mut c_void, m: *mut c_void) -> bool {
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
