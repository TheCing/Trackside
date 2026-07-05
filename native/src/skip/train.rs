//! Training cut-in skip + Photo Studio guard.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::hooks::{in_heaven, ReentryGuard};
use crate::skip::{call_orig, is_enabled, rr_log, Invokable, TRAIN_SKIPS};

// Invokable (set in install).
pub(crate) static SKIP_RUNTIME: OnceLock<Invokable> = OnceLock::new(); // training

crate::skip_hook_slot!(TR_START, D_START);
crate::skip_hook_slot!(TR_PLAY, D_PLAY);
crate::skip_hook_slot!(TR_MAIN, D_MAIN);
crate::skip_hook_slot!(TR_PHOTO_PLAY, D_PHOTO_PLAY); // PhotoStudioCuttController.PlayCutIn
crate::skip_hook_slot!(TR_PHOTO_ASYNC, D_PHOTO_ASYNC); // .PlayCutInAsync
crate::skip_hook_slot!(TR_PHOTO_END, D_PHOTO_END); // .OnEndCutIn
// True while the Photo Studio is replaying a cut. It reuses SingleModeTrainingCutInHelper
// (PhotoStudioCuttController._cutInHelperList@0x18), so those helpers fire our OnPlayCutIn
// hook — without this flag the training-skip would swallow the photo-studio animation too.
static PHOTO_CUT_ACTIVE: AtomicBool = AtomicBool::new(false);

// ── TRAINING: run SkipRuntime after a cut-in start. ─────────────────────────
fn do_training_skip(this: *mut c_void) {
    if !is_enabled() {
        return;
    }
    // DIAGNOSTIC: log the bail reason. If the rainbow "stops skipping" mid-run, this shows whether it is
    // a stuck re-entry guard (in_heaven) — the prime suspect for the "worked then nothing skips" bug.
    if in_heaven() {
        rr_log("[train] BAILED: in_heaven guard held (stuck? watchdog clears it next frame)");
        return;
    }
    if this.is_null() {
        return;
    }
    if PHOTO_CUT_ACTIVE.load(Ordering::Relaxed) {
        rr_log("[train] bailed: photo-studio cut active");
        return; // Photo Studio cut recreation — must play normally, never skip it
    }
    if let Some(sr) = SKIP_RUNTIME.get() {
        if sr.ok() {
            let _g = ReentryGuard::enter();
            unsafe { sr.call_void(this) };
            TRAIN_SKIPS.fetch_add(1, Ordering::Relaxed);
            rr_log("[train] SkipRuntime() fired");
        }
    }
}
pub(crate) unsafe extern "C" fn on_start_cutin(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_START, this, m);
    do_training_skip(this);
}
pub(crate) unsafe extern "C" fn on_play_cutin(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_PLAY, this, m);
    do_training_skip(this);
}
pub(crate) unsafe extern "C" fn on_play_main_cutin(this: *mut c_void, m: *mut c_void) {
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

pub(crate) unsafe extern "C" fn on_photo_play_cut(
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
pub(crate) unsafe extern "C" fn on_photo_play_cut_async(
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
pub(crate) unsafe extern "C" fn on_photo_end_cut(this: *mut c_void, m: *mut c_void) {
    call_orig(&TR_PHOTO_END, this, m);
    PHOTO_CUT_ACTIVE.store(false, Ordering::Relaxed);
    rr_log("[photo] cut end -> training-skip resumed");
}
