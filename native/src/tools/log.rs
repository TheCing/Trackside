//! Append-to-file logging with wall-clock timestamps + levels.
//!
//! Two tiers:
//!   * `log` / `warn` / `error` — ALWAYS written to `trackside-native.log`, timestamped and
//!     level-tagged so events can be correlated in time and filtered by severity.
//!   * `debug` — only written when the **Verbose diagnostics** toggle is on. This is the tier
//!     that makes the toggle actually do something: flip it on, reproduce, send the log.
//!
//! `log_to` stays RAW (no prefix) for structured one-shot dumps (scan reports, the diag
//! report, loadprof CSV) where a per-line timestamp would corrupt the format.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

// ── wall-clock timestamp (Win32, dependency-free, Windows-only target) ──────────
#[repr(C)]
#[derive(Default)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}
extern "system" {
    fn GetLocalTime(t: *mut SystemTime);
}

/// `HH:MM:SS.mmm` local time — matches what the player sees on their clock, so a report
/// ("it froze around 3:42") lines up with the log.
fn timestamp() -> String {
    let mut t = SystemTime::default();
    unsafe { GetLocalTime(&mut t) };
    format!("{:02}:{:02}:{:02}.{:03}", t.hour, t.minute, t.second, t.milliseconds)
}

// Monotonic line counter — a stable ordering key even if two lines land in the same
// millisecond, and a quick "how much happened" gauge.
static SEQ: AtomicUsize = AtomicUsize::new(0);

/// Raw append — no timestamp/level. For structured dumps. Silent on failure (logging must
/// never take down a hook).
pub fn log_to(file: &str, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(crate::paths::log_file(file)) {
        let _ = writeln!(f, "{msg}");
    }
}

/// Write one timestamped, level-tagged line to the native log.
fn native(level: &str, msg: &str) {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    log_to("trackside-native.log", &format!("[{}] {level} #{n:<6} {msg}", timestamp()));
}

/// INFO — always written. (Every existing `tools::log(..)` call site routes here, so they all
/// gain timestamps/levels for free.)
pub fn log(msg: &str) {
    native("INFO ", msg);
}

/// WARN — always written. Recoverable oddities (a resolve miss with a fallback, a skipped item).
pub fn warn(msg: &str) {
    native("WARN ", msg);
}

/// ERROR — always written. A feature genuinely failed (hook didn't install, a call errored).
pub fn error(msg: &str) {
    native("ERROR", msg);
}

/// DEBUG — written ONLY when Verbose diagnostics is on. Detailed per-event tracing: packet
/// types, screen captures, per-click Apply steps, recommend inputs/outputs. Cheap when off
/// (one relaxed atomic load), so it's safe to sprinkle at event points — but keep it OUT of
/// per-frame hot loops.
pub fn debug(msg: &str) {
    if crate::diag::enabled() {
        native("DBG  ", msg);
    }
}

/// Is verbose (debug) logging on? For callers that want to skip building an expensive message.
pub fn debug_enabled() -> bool {
    crate::diag::enabled()
}
