//! Player state — feed the dashboard's "Your status" panel.
//!
//! The dashboard's `player_state.py` reads a handful of player-specific fields (name,
//! team class, rank + promotion/demotion thresholds, RP, support bonus, opponent tier,
//! final score / MVP) out of FOUR Team Trials responses:
//!
//!   team_stadium/index              — who you are, team class, rank thresholds
//!   team_stadium/start              — RP, support-card bonus
//!   team_stadium/decide_frame_order — the opponent you drew
//!   team_stadium/all_race_end       — the result: score, win type, MVP, RP/point gains
//!
//! It wants them keyed by endpoint (`extract_state({endpoint: payload})`). Upstream fed
//! that from the mitmproxy capture, which recorded each response separately — the one
//! thing the proxy did that nothing else replaced. The horseACT race export only carries
//! `team_stadium/start`, so 22 of the 27 fields are simply not in it.
//!
//! We see all four go past on the wire, so we write them out per trial and let the
//! dashboard do the extracting. Deliberately storing the WHOLE payload rather than the
//! fields it wants today: `player_state.py` documents "add an entry to FIELD_EXTRACTORS
//! and the dashboard surfaces it automatically", and pre-selecting subtrees here would
//! quietly break that promise.
//!
//! The endpoint name isn't in the response body, so we identify each by a key only it
//! carries. `all_race_end` closes the trial and flushes the file.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// Endpoint → the key that identifies its response. Order matters only for logging.
pub const ENDPOINTS: [(&str, &str); 4] = [
    ("team_stadium/index", "team_stadium_user"),
    ("team_stadium/start", "rp_info"),
    ("team_stadium/decide_frame_order", "opponent_info_copy"),
    ("team_stadium/all_race_end", "total_score_info"),
];

static PENDING: OnceLock<Mutex<Vec<(String, Value)>>> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(true);

fn pending() -> &'static Mutex<Vec<(String, Value)>> {
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub fn apply(s: &crate::settings::Settings) {
    set_enabled(s.player_state);
}

/// Record one endpoint's payload. `payload` is the response's `data` map.
pub fn record(endpoint: &str, payload: Value) {
    if !enabled() {
        return;
    }
    if let Ok(mut g) = pending().lock() {
        // Last write wins per endpoint — a re-request within the same trial supersedes.
        g.retain(|(e, _)| e != endpoint);
        g.push((endpoint.to_string(), payload));
    }
    crate::tools::log(&format!("[player_state] captured {endpoint}"));
    if endpoint == "team_stadium/all_race_end" {
        flush();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Write the trial's payloads out and reset. Called when `all_race_end` lands (the
/// trial is over and every field is now known).
fn flush() {
    let items = match pending().lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return,
    };
    if items.is_empty() {
        return;
    }
    let mut map = serde_json::Map::new();
    for (ep, payload) in &items {
        // Shape it exactly as extract_state() expects: it reads `payload["data"]`.
        map.insert(ep.clone(), json!({ "data": payload }));
    }
    let doc = Value::Object(map);
    let Ok(text) = serde_json::to_string(&doc) else { return };

    let dir = crate::paths::dashboard_data_dir().join("player_state");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("state_{}.json", now_ms()));
    match std::fs::write(&path, text.as_bytes()) {
        Ok(_) => {
            let eps: Vec<&str> = items.iter().map(|(e, _)| e.as_str()).collect();
            crate::tools::log(&format!(
                "[player_state] trial written ({} endpoints: {}) -> {}",
                items.len(),
                eps.join(", "),
                path.display()
            ));
            prune(&dir);
        }
        Err(e) => crate::tools::log(&format!("[player_state] write failed: {e}")),
    }
}

/// Keep the newest 30 files. The dashboard dedups on import, and these payloads are
/// fat (the start response alone is ~350 KB), so an unbounded folder is a slow leak.
fn prune(dir: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<_> = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("state_"))
        .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e.path())))
        .collect();
    if files.len() <= 30 {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    for (_, p) in files.iter().take(files.len() - 30) {
        let _ = std::fs::remove_file(p);
    }
}

pub fn status() -> String {
    let n = pending().lock().map(|g| g.len()).unwrap_or(0);
    if n == 0 {
        "Waiting for a Team Trial.".into()
    } else {
        format!("{n}/4 parts of this trial captured — finishes when the trial ends.")
    }
}
