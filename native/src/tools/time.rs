//! A process-monotonic clock, shared by the modules that used to each carry their own
//! `static CLOCK`/`now_ms()` copy. All deltas stay correct (the anchor is fixed for the
//! process); only the absolute epoch is unified.

use std::sync::OnceLock;
use std::time::Instant;

/// Process-monotonic anchor, set on first use.
pub fn clock() -> &'static Instant {
    static CLOCK: OnceLock<Instant> = OnceLock::new();
    CLOCK.get_or_init(Instant::now)
}

/// Milliseconds elapsed since the process clock anchor.
pub fn now_ms() -> u64 {
    clock().elapsed().as_millis() as u64
}
