//! Heaven — re-entrancy guard for Heaven-initiated calls into methods we also hook.
//!
//! Native retour detours don't have Frida's "breakpoint re-entrancy" crash, but we still
//! guard against LOGICAL recursion (our hook calling a method it itself hooks) with a
//! thread-local flag. Any re-entered Heaven hook checks `in_heaven()` and passes straight
//! through to the original.

use std::cell::Cell;

thread_local! {
    /// Set while WE are invoking a method we also hook → the re-entered hook
    /// body sees this and passes straight through to the original.
    static IN_HEAVEN: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard: set IN_HEAVEN for the duration of a Heaven-initiated call into a
/// method we also hook. Shared by skip.rs etc. — any re-entered Heaven hook sees
/// `in_heaven()` and passes straight through to the original.
///
/// Nesting-safe: it SAVES the previous value and RESTORES it on drop (not a blind reset to false).
/// The old version reset to false, so an inner guard dropping mid-way through an outer guard's call
/// re-enabled the hooks early → a skip could fire re-entrantly inside another skip and corrupt state.
pub struct ReentryGuard(bool);
impl ReentryGuard {
    pub fn enter() -> Self {
        ReentryGuard(IN_HEAVEN.with(|f| f.replace(true)))
    }
}
impl Drop for ReentryGuard {
    fn drop(&mut self) {
        IN_HEAVEN.with(|f| f.set(self.0));
    }
}
pub fn in_heaven() -> bool {
    IN_HEAVEN.with(|f| f.get())
}
/// Force-clear the re-entry guard on the current thread. Safety net: a leaked guard (a Heaven call
/// that exited without dropping its ReentryGuard) would leave `in_heaven()` stuck true and silently
/// disable every skip that gates on it. The per-frame button pump calls this when it detects the
/// guard held outside any Heaven call, so the skips self-recover without a game restart.
pub fn clear_in_heaven() {
    IN_HEAVEN.with(|f| f.set(false));
}
