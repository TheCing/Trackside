//! Breeding trace — feed the Trackside Dashboard's Breed Optimizer passively.
//!
//! The dashboard needs two halves:
//!
//!   * **mine**     — your trained umas. Arrives as `trained_chara_array` (the Veteran
//!                    List packet we already read for the data.json export).
//!   * **rentable** — friends' BORROWABLE parents, i.e. the rental half of the
//!                    optimizer. Arrives as `succession_trained_chara_data` on the
//!                    career-start (`pre_single_mode/index`) response, together with
//!                    `summary_user_info_array` (the friends' names).
//!
//! Upstream's dashboard could only reach the rentable half by attaching Frida to the
//! game to lift your auth key and then calling the server's API as your client. We
//! already see the same data go past on the wire, so we just read it — no injection,
//! no credentials. This is what makes dropping the Frida path free.
//!
//! We write the dashboard's own trace format (one JSON object per line):
//!
//!   {"ts":…,"direction":"RES","endpoint":"load/index",             "data":{"data":{"trained_chara":[…]}}}
//!   {"ts":…,"direction":"RES","endpoint":"pre_single_mode/index",  "data":{"data":{"succession_trained_chara_data":{…}}}}
//!
//! straight into the dashboard's `breeding/` folder, so it's picked up with no import
//! step (`breeding.find_trace()` globs that folder for the newest `*.jsonl`).
//!
//! The two packets arrive at different times, so we hold each half and rewrite the
//! file whenever either updates. We only write once we have **mine**: a trace with an
//! empty `trained_chara` would still satisfy the dashboard's "is it set up?" check and
//! leave the user staring at an empty inventory with the setup dialog skipped. The
//! dashboard merges halves across trace files, so a rentals-only session still lands
//! once the Veteran List has been seen even once.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

static MINE: OnceLock<Mutex<Option<Value>>> = OnceLock::new();
static RENTALS: OnceLock<Mutex<Option<Value>>> = OnceLock::new();
/// FNV-1a of the last file we wrote — skip identical rewrites (this runs per packet).
static LAST_HASH: AtomicU64 = AtomicU64::new(0);
static ENABLED: AtomicBool = AtomicBool::new(true);

fn mine_slot() -> &'static Mutex<Option<Value>> {
    MINE.get_or_init(|| Mutex::new(None))
}
fn rentals_slot() -> &'static Mutex<Option<Value>> {
    RENTALS.get_or_init(|| Mutex::new(None))
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Apply persisted settings at boot.
pub fn apply(s: &crate::settings::Settings) {
    set_enabled(s.breeding_trace);
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Your trained umas (`trained_chara_array`). The dashboard's own data.json import
/// feeds these same entries in as `trained_chara`, so the shapes are interchangeable.
pub fn set_mine(entries: Vec<Value>) {
    if entries.is_empty() {
        return;
    }
    if let Ok(mut g) = mine_slot().lock() {
        // Largest roster wins: the partner pickers fire the same packet with a handful
        // of umas, and a partial pass must not shrink the inventory (same guard the
        // veterans export uses).
        let prev = g
            .as_ref()
            .and_then(|v| v.as_array().map(|a| a.len()))
            .unwrap_or(0);
        if entries.len() < prev {
            return;
        }
        *g = Some(Value::Array(entries));
    }
    write_if_ready();
}

/// Friends' borrowable parents (`succession_trained_chara_data`, verbatim).
pub fn set_rentals(block: Value) {
    let n = block
        .get("succession_trained_chara_array")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if n == 0 {
        return;
    }
    if let Ok(mut g) = rentals_slot().lock() {
        *g = Some(block);
    }
    crate::tools::log(&format!("[breeding] {n} borrowable parents captured"));
    write_if_ready();
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Compose and write the trace — but only once `mine` is present (see module docs).
fn write_if_ready() {
    if !enabled() {
        return;
    }
    let mine = match mine_slot().lock() {
        Ok(g) => match g.as_ref() {
            Some(v) => v.clone(),
            None => return, // rentals alone would make a trace that fools the setup check
        },
        Err(_) => return,
    };
    let rentals = rentals_slot()
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| json!({"succession_trained_chara_array": [], "summary_user_info_array": []}));

    let ts = now_secs();
    let mut out = String::new();
    for (endpoint, inner) in [
        ("load/index", json!({ "trained_chara": mine })),
        ("pre_single_mode/index", json!({ "succession_trained_chara_data": rentals })),
    ] {
        let rec = json!({
            "ts": ts,
            "direction": "RES",
            "endpoint": endpoint,
            "data": { "data": inner, "data_headers": { "result_code": 1 } },
        });
        match serde_json::to_string(&rec) {
            Ok(s) => {
                out.push_str(&s);
                out.push('\n');
            }
            Err(_) => return,
        }
    }

    // Hash the payload, not the file: `ts` changes every call, so hash the records
    // without it by checking the body we'd write minus the timestamps.
    let stable = out
        .split('\n')
        .map(|l| {
            l.find(",\"direction\"")
                .map(|i| &l[i..])
                .unwrap_or(l)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let h = fnv1a(stable.as_bytes());
    if LAST_HASH.swap(h, Ordering::Relaxed) == h {
        return; // nothing changed since the last write
    }

    let dir = crate::paths::dashboard_data_dir().join("breeding");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Stable filename: the dashboard takes the NEWEST *.jsonl, and rewriting one file
    // keeps us newest without littering a file per career start.
    let path = dir.join("capture_trackside.jsonl");
    match std::fs::write(&path, out.as_bytes()) {
        Ok(_) => {
            let n_mine = mine_slot()
                .lock()
                .ok()
                .and_then(|g| g.as_ref().and_then(|v| v.as_array().map(|a| a.len())))
                .unwrap_or(0);
            let n_rent = rentals_slot()
                .lock()
                .ok()
                .and_then(|g| {
                    g.as_ref()
                        .and_then(|v| v.get("succession_trained_chara_array"))
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                })
                .unwrap_or(0);
            crate::tools::log(&format!(
                "[breeding] trace written ({n_mine} yours, {n_rent} borrowable) -> {}",
                path.display()
            ));
        }
        Err(e) => crate::tools::log(&format!("[breeding] trace write failed: {e}")),
    }
}

/// Live status for the menu.
pub fn status() -> String {
    let n_mine = mine_slot()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|v| v.as_array().map(|a| a.len())))
        .unwrap_or(0);
    let n_rent = rentals_slot()
        .lock()
        .ok()
        .and_then(|g| {
            g.as_ref()
                .and_then(|v| v.get("succession_trained_chara_array"))
                .and_then(|v| v.as_array())
                .map(|a| a.len())
        })
        .unwrap_or(0);
    match (n_mine, n_rent) {
        (0, 0) => "Waiting — open the Veteran List, then a career start screen.".into(),
        (m, 0) => format!("{m} of yours captured. Open a career start screen for borrowable parents."),
        (0, r) => format!("{r} borrowable parents held — open the Veteran List to write the trace."),
        (m, r) => format!("{m} yours + {r} borrowable — sent to the dashboard."),
    }
}
