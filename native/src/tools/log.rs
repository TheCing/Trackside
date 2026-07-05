//! Append-to-file logging. Replaces the per-module `OpenOptions::new().append()` +
//! `writeln!` boilerplate that used to live in ~24 modules.

use std::io::Write;

/// Append a line to the default Heaven log (`trackside-native.log`).
pub fn log(msg: &str) {
    log_to("trackside-native.log", msg);
}

/// Append a line to the named log file under the Heaven logs directory. Silent on failure
/// (logging must never take down a hook).
pub fn log_to(file: &str, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file(file))
    {
        let _ = writeln!(f, "{msg}");
    }
}
