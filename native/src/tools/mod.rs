//! Heaven — `tools`: shared cross-cutting utilities.
//!
//! One home for the small helpers that were previously copy-pasted into nearly every
//! module (log-file writing, a process-monotonic clock). Add future shared tooling here
//! (dump/format helpers, seeded RNG for tests, etc.) so it lives in one clearly-named place
//! instead of being duplicated per feature.
//!
//! NOTE: the crash breadcrumb/SEH filter (`crashlog`) is its own subsystem, not a helper —
//! it deliberately stays out of `tools`.

pub mod log;
pub mod time;

pub use log::{log, log_to};
pub use time::{clock, now_ms};
