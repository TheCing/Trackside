//! race_export — dump each race to a JSON the user can upload to a web race
//! visualizer (the `RaceInfo` object incl. its `<SimDataBase64>k__BackingField`).
//!
//! Mechanism: `race::compute_header` already detects a NEW race (the `RaceInfo`
//! pointer changing) and hands us the live `RaceInfo` object — so we don't add a
//! hot hook. On a new race we walk the object graph by IL2CPP reflection into a
//! `serde_json::Value` (using the field/type/array exports already resolved in
//! `htt_il2cpp`), then write it to disk grouped by `<RaceType>k__BackingField`.
//!
//! The serializer mirrors the on-disk schema the visualizer expects: every
//! instance field is emitted under its raw managed name (`<X>k__BackingField`),
//! enums resolve to their member name, `Obscured*` numeric values are decrypted,
//! and arrays/strings/nested objects recurse. The heavy serialize + file write
//! runs on a worker thread; only the managed-memory walk touches the game thread
//! (which is IL2CPP-attached when `compute_header` runs).

#![allow(static_mut_refs)]

use core::ffi::c_void;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::Value;

use crate::htt_il2cpp as h;


// The race viewer (hakuraku) shows an "outdated, go download the other tool" notice
// unless the file declares this version key. We emit it so the viewer treats our
// output as current and never points the user elsewhere. Bump to match the viewer's
// expected current release.
const VIEWER_VERSION: &str = "1.1.4";

/// Walk an arbitrary managed object to a JSON string (for one-shot RE/census of
/// unknown object layouts, e.g. the career acquired-skill list). Safe to call from
/// an attached thread; returns "<err>" on failure.
pub fn dump_object_json(addr: usize) -> String {
    if addr == 0 {
        return "null".into();
    }
    std::panic::catch_unwind(move || unsafe {
        let mut visited: HashSet<usize> = HashSet::new();
        let val = crate::il2cpp_json::convert_object(addr as *mut c_void, 0, &mut visited);
        serde_json::to_string(&val).unwrap_or_default()
    })
    .unwrap_or_else(|_| "<err>".into())
}

/// Stamp the viewer version key onto the root object.
fn stamp_version(v: &mut Value) {
    if let Value::Object(map) = v {
        map.insert("horseACT_version".to_string(), Value::String(VIEWER_VERSION.to_string()));
    }
}

// Runtime mirror of the settings toggle. The RaceInfo getter we hook fires very
// often, so the hot path checks this atomic instead of locking the settings cache.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Mirror the persisted toggle into the fast path. Called by settings.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Apply persisted settings to the race export module at boot.
pub fn apply(s: &crate::settings::Settings) {
    set_enabled(s.race_export);
}

// Cached `<SimDataBase64>k__BackingField` offset on RaceInfo. usize::MAX = unknown.
static SIM_OFFSET: AtomicUsize = AtomicUsize::new(usize::MAX);
// Dedup: last race we dumped (RaceInfo ptr + its SimData ptr). A race is "new" when
// either changes (the game reuses a RaceInfo address but swaps the SimData on a re-run).
static LAST_RI: AtomicUsize = AtomicUsize::new(0);
static LAST_SIM: AtomicUsize = AtomicUsize::new(0);
// Last RaceInfo we logged a diagnostic line for (so the log shows one line per race).
static LAST_DIAG: AtomicUsize = AtomicUsize::new(0);

fn elog(msg: &str) {
    crate::tools::log(msg);
}


// ── new-race entry point (called from race::compute_header) ───────────────────

/// Called every time `compute_header` sees the live `RaceInfo`. Cheap pointer
/// compares; only walks + saves when a genuinely new race is detected and the
/// export toggle is on. Safe to call from the (attached) game thread.
pub fn maybe_dump(ri: *mut c_void) {
    if ri.is_null() || !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let addr = ri as usize;
    let _ = std::panic::catch_unwind(move || unsafe { dump_inner(addr as *mut c_void) });
}

unsafe fn dump_inner(ri: *mut c_void) {
    let klass = match h::OBJECT_GET_CLASS {
        Some(f) => f(ri),
        None => return,
    };
    if klass.is_null() {
        return;
    }

    // Resolve + cache the SimData field offset once. We key on `<SimData>k__BackingField`
    // (the deserialized RaceSimulateData OBJECT, offset 0xe8) rather than the base64 STRING
    // (`<SimDataBase64>`, 0xe0): the object is populated on BOTH the 3D path (SetAndDeserializeBase64)
    // AND the simulated/skipped path (SetupSimulateData attaches an already-deserialized SimData,
    // where the base64 string can be empty). Using the object makes "race ready" fire for skips too.
    let mut sim_off = SIM_OFFSET.load(Ordering::Relaxed);
    if sim_off == usize::MAX {
        sim_off = h::field_offset(klass, "<SimData>k__BackingField").unwrap_or(0);
        SIM_OFFSET.store(sim_off, Ordering::Relaxed);
    }
    let sim_ptr = if sim_off != 0 {
        *((ri as usize + sim_off) as *const usize)
    } else {
        0
    };
    // One line per distinct RaceInfo so the log shows the trigger firing + whether the
    // SimData object has populated yet (diagnostic; cheap, ~1 line per race).
    if (ri as usize) != LAST_DIAG.load(Ordering::Relaxed) {
        LAST_DIAG.store(ri as usize, Ordering::Relaxed);
        elog(&format!("[race-export] trigger: ri={ri:p} sim_off={sim_off:#x} sim_ptr={sim_ptr:#x}"));
    }
    // If we know the SimData slot and it's still empty, the race isn't ready —
    // don't dump (and don't mark it seen, so we retry on the next call).
    if sim_off != 0 && sim_ptr == 0 {
        return;
    }

    let last_ri = LAST_RI.load(Ordering::Relaxed);
    let last_sim = LAST_SIM.load(Ordering::Relaxed);
    let is_new = (ri as usize) != last_ri || (sim_off != 0 && sim_ptr != last_sim);
    if !is_new {
        return;
    }
    LAST_RI.store(ri as usize, Ordering::Relaxed);
    LAST_SIM.store(sim_ptr, Ordering::Relaxed);

    let mut visited: HashSet<usize> = HashSet::new();
    let val = crate::il2cpp_json::convert_object(ri, 0, &mut visited);
    let base = crate::paths::dll_dir().join("heaven-races");
    // Hand off the (pure-Rust) serialize + disk write to a worker thread so the
    // game thread isn't blocked on I/O.
    std::thread::spawn(move || save(val, base));
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' })
        .collect::<String>()
        .trim()
        .to_string()
}

fn folder_for(race_type: &str) -> String {
    match race_type {
        "Single" => "Career".into(),
        "RoomMatch" => "Room match".into(),
        "Champions" => "Champions meeting".into(),
        "Practice" => "Practice room".into(),
        "Stadium" | "TeamStadium" | "Daily" => "Team trials".into(),
        "" => "Other".into(),
        // Any unknown race type self-groups into a folder named after it, so new
        // categories are captured (and surfaced) without a code change.
        other => sanitize(other),
    }
}

/// Find the winner (FinishOrder == 0) across the known horse arrays → (name, raw time).
fn winner_of(v: &Value) -> Option<(String, f64)> {
    for field in ["<RaceHorse>k__BackingField", "<PlayerTeamMemberArray>k__BackingField"] {
        if let Some(arr) = v.get(field).and_then(|x| x.as_array()) {
            if let Some(w) = arr
                .iter()
                .find(|h| h.get("FinishOrder").and_then(|x| x.as_i64()) == Some(0))
            {
                let name = w
                    .get("<charaName>k__BackingField")
                    .and_then(|x| x.as_str())
                    .unwrap_or("race")
                    .to_string();
                let time = w.get("FinishTimeRaw").and_then(|x| x.as_f64()).unwrap_or(0.0);
                return Some((name, time));
            }
        }
    }
    None
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn write_json(value: Value, dir: PathBuf, filename: String) {
    if let Err(e) = std::fs::create_dir_all(&dir) {
        elog(&format!("[race-export] mkdir failed: {e}"));
        return;
    }
    let path = dir.join(filename);
    match serde_json::to_string_pretty(&value) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(_) => elog(&format!("[race-export] saved {}", path.display())),
            Err(e) => elog(&format!("[race-export] write failed: {e}")),
        },
        Err(e) => elog(&format!("[race-export] serialize failed: {e}")),
    }
}

fn save(mut value: Value, base: PathBuf) {
    // The visualizer needs the replay blob; skip empty races.
    match value.get("<SimDataBase64>k__BackingField") {
        Some(Value::String(s)) if !s.is_empty() => {}
        _ => {
            elog("[race-export] skipped: SimDataBase64 missing/empty");
            return;
        }
    }
    stamp_version(&mut value);
    let race_type = value
        .get("<RaceType>k__BackingField")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let dir = base.join(folder_for(&race_type));
    let stamp = now_ms();
    let filename = match winner_of(&value) {
        Some((name, time)) => format!("{}-{:.4}s-{}.json", sanitize(&name), time, stamp),
        None => format!("race-{stamp}.json"),
    };
    write_json(value, dir, filename);
}

/// Team Trials export. The Team Trials result never goes through `RaceInfo`
/// (the races are auto-resolved, so `get_RaceTrackId` never fires) — but Heaven
/// already hooks `TeamStadiumResult..ctor` for the dashboard capture, so we reuse
/// that hook's response object here: walk the whole result payload to JSON and
/// drop it under "Team trials". No SimData gate — the TT payload carries its own
/// per-race replay blobs (`race_scenario`).
pub fn dump_team_trials(response: *mut c_void) {
    if response.is_null() || !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let addr = response as usize;
    let _ = std::panic::catch_unwind(move || unsafe {
        let mut visited: HashSet<usize> = HashSet::new();
        let mut val = crate::il2cpp_json::convert_object(addr as *mut c_void, 0, &mut visited);
        // Sanity: only dump if it actually looks like a Team Trials result.
        let looks_tt = val.get("race_result_array").is_some()
            || val
                .get("data")
                .and_then(|d| d.get("race_result_array"))
                .is_some();
        if !looks_tt {
            elog("[race-export] TT: no race_result_array; skipped");
            return;
        }
        stamp_version(&mut val);
        let dir = crate::paths::dll_dir().join("heaven-races").join("Team trials");
        let stamp = now_ms();
        std::thread::spawn(move || write_json(val, dir, format!("TT-{stamp}.json")));
    });
}
