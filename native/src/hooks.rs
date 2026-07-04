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
