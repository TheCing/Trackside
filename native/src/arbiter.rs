//! Feature arbiter — visibility + auto-disable over hook coexistence.
//!
//! `il2cpp::hook_method` REFUSES to double-hook a method another mod detoured first
//! ("already detoured (skipped)") → Trackside yields, no crash, but the feature is lost.
//! The arbiter records every hook outcome keyed by `Class.method`, so Heaven can:
//!   - SHOW which features it ceded to a co-resident mod (boot log + overlay), and
//!   - AUTO-DISABLE its own duplicate tweaks (`is_ceded`) so the menu is honest and
//!     two mods never fight over / double-apply the same game state (e.g. UI speed).

use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Heaven,
    External,
    Chained,
    Missing,
}

// (key = "Class.method", owner). Mutex is const-constructible → no lazy init needed.
static RECORDS: Mutex<Vec<(String, Owner)>> = Mutex::new(Vec::new());

/// Record a hook outcome under a unique `Class.method` key (last write wins).
pub fn record(key: &str, owner: Owner) {
    if let Ok(mut v) = RECORDS.lock() {
        if let Some(e) = v.iter_mut().find(|(k, _)| k == key) {
            e.1 = owner;
        } else {
            v.push((key.to_string(), owner));
        }
    }
}

/// Classify a `hook_method` result and record it under `key`.
pub fn note(key: &str, res: &Result<(), String>) {
    let owner = match res {
        Ok(()) => Owner::Heaven,
        Err(e) if e.contains("already detoured") => Owner::External,
        Err(_) => Owner::Missing,
    };
    record(key, owner);
}

/// True if a method matching `suffix` was ceded to another mod (it's hooked by them,
/// not us). `suffix` is matched against the end of the key so callers can pass either
/// the full "Class.method" or just the distinctive part.
pub fn is_ceded(suffix: &str) -> bool {
    RECORDS
        .lock()
        .map(|v| {
            v.iter()
                .any(|(k, o)| *o == Owner::External && k.ends_with(suffix))
        })
        .unwrap_or(false)
}

pub fn is_chained(suffix: &str) -> bool {
    RECORDS
        .lock()
        .map(|v| {
            v.iter()
                .any(|(k, o)| *o == Owner::Chained && k.ends_with(suffix))
        })
        .unwrap_or(false)
}

/// One-line summary for the boot log / overlay.
pub fn report() -> String {
    let v = match RECORDS.lock() {
        Ok(v) => v,
        Err(e) => e.into_inner(),
    };
    let owned = v.iter().filter(|(_, o)| *o == Owner::Heaven).count();
    let ext: Vec<&str> = v
        .iter()
        .filter(|(_, o)| *o == Owner::External)
        .map(|(k, _)| k.as_str())
        .collect();
    if ext.is_empty() {
        format!("{owned} hook(s) owned by Trackside, none ceded")
    } else {
        format!(
            "{owned} owned by Trackside, {} ceded to a co-resident mod: [{}]",
            ext.len(),
            ext.join(", ")
        )
    }
}
