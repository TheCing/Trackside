//! Heaven Plan B — detour layer (B1).
//!
//! Replaces Frida's Interceptor with retour trampolining detours installed on
//! the game's compiled methodPointers. The whole reason Plan A crashed was
//! Frida's "breakpoint triggered" re-entrancy: calling a NativeFunction whose
//! path re-enters a hooked method = hard crash. Native detours don't have that
//! limitation, but we still guard against LOGICAL recursion (our hook calling a
//! method it itself hooks) with a thread-local flag.
//!
//! B1 goal: hook ONE hot method (ButtonCommon.Update), call the trampoline, and
//! prove (a) the game keeps running and (b) a guarded self-call into the hooked
//! address re-enters our hook and is short-circuited — no crash, no recursion.

#![allow(dead_code)]

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use crate::il2cpp;

thread_local! {
    /// Set while WE are invoking a method we also hook → the re-entered hook
    /// body sees this and passes straight through to the original.
    static IN_HEAVEN: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard: set IN_HEAVEN for the duration of a Heaven-initiated call into a
/// method we also hook. Shared by skip.rs etc. — any re-entered Heaven hook sees
/// `in_heaven()` and passes straight through to the original.
pub struct ReentryGuard;
impl ReentryGuard {
    pub fn enter() -> Self {
        IN_HEAVEN.with(|f| f.set(true));
        ReentryGuard
    }
}
impl Drop for ReentryGuard {
    fn drop(&mut self) {
        IN_HEAVEN.with(|f| f.set(false));
    }
}
pub fn in_heaven() -> bool {
    IN_HEAVEN.with(|f| f.get())
}

// IL2CPP instance void method with no managed params compiles to:
//   void (*)(Il2CppObject* this, MethodInfo* method)
type VoidMethod1 = unsafe extern "C" fn(*mut c_void, *mut c_void);

// ── B1: ButtonCommon.Update hook ────────────────────────────────────────────
static UPDATE_TRAMP: AtomicUsize = AtomicUsize::new(0);    // trampoline → original code
static UPDATE_TARGET: AtomicUsize = AtomicUsize::new(0);   // hooked methodPointer (re-enters us)
static UPDATE_CALLS: AtomicU64 = AtomicU64::new(0);        // detour hit counter
static UPDATE_REENTRY_OK: AtomicU64 = AtomicU64::new(0);   // guarded self-call hits
static GUARD_TEST_DONE: OnceLock<()> = OnceLock::new();
// Keep the detour alive for the process lifetime.
static UPDATE_DETOUR: OnceLock<RawDetour> = OnceLock::new();

unsafe extern "C" fn update_hook(this: *mut c_void, method: *mut c_void) {
    UPDATE_CALLS.fetch_add(1, Ordering::Relaxed);

    // Re-entrancy short-circuit: if we re-entered via our own call, just run
    // the original and return — never recurse into Heaven logic.
    if in_heaven() {
        UPDATE_REENTRY_OK.fetch_add(1, Ordering::Relaxed);
        call_original(this, method);
        return;
    }

    // ── B1 one-shot guard test: call the HOOKED address from inside the hook.
    //    It re-enters update_hook; the guard short-circuits it → no crash, no
    //    infinite recursion. This is exactly the pattern that crashed Plan A in
    //    Frida. Runs exactly once. ──
    if GUARD_TEST_DONE.get().is_none() {
        let _ = GUARD_TEST_DONE.set(());
        let hooked = UPDATE_TARGET.load(Ordering::Relaxed);
        if hooked != 0 {
            let _g = ReentryGuard::enter();
            let f: VoidMethod1 = std::mem::transmute(hooked);
            f(this, method); // re-enters update_hook; in_heaven()==true → passthrough
        }
    }

    call_original(this, method);
}

#[inline]
unsafe fn call_original(this: *mut c_void, method: *mut c_void) {
    let t = UPDATE_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let f: VoidMethod1 = std::mem::transmute(t);
        f(this, method);
    }
}

/// Snapshot of B1 counters for logging.
pub fn b1_stats() -> (u64, u64) {
    (UPDATE_CALLS.load(Ordering::Relaxed), UPDATE_REENTRY_OK.load(Ordering::Relaxed))
}

/// Install the B1 hook on ButtonCommon.Update. Call after il2cpp::init +
/// thread attach. Returns Err with a reason on failure.
pub fn install_b1() -> Result<(), String> {
    let k = il2cpp::class("Gallop.ButtonCommon");
    if k.is_null() {
        return Err("anchor class miss".into());
    }
    let m = il2cpp::method(k, "Update", 0);
    if m.is_null() {
        return Err("anchor method miss".into());
    }
    let target = il2cpp::method_pointer(m);
    if target.is_null() {
        return Err("method ptr null".into());
    }
    unsafe {
        let detour = RawDetour::new(target as *const (), update_hook as *const ())
            .map_err(|e| format!("RawDetour::new: {e}"))?;
        detour.enable().map_err(|e| format!("detour.enable: {e}"))?;
        UPDATE_TRAMP.store(detour.trampoline() as *const () as usize, Ordering::Relaxed);
        UPDATE_TARGET.store(target as usize, Ordering::Relaxed);
        let _ = UPDATE_DETOUR.set(detour);
    }
    Ok(())
}
