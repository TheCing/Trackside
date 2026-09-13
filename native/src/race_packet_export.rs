//! Race export, packet edition — the horseACT 1.1.5+ format Hakuraku now expects.
//!
//! WHY THIS EXISTS ALONGSIDE `race_export`. That module walks the live IL2CPP `RaceInfo` object,
//! which is what horseACT did up to 1.1.4 and what we cloned. Two horseACT releases since then
//! changed the payload — 1.1.6 "Once again dump trained_chara_data" and 1.1.7 "Dump decks/parent
//! info for CM races" — and the IL2CPP route cannot supply either: `<TrainedCharaData>` is a NULL
//! pointer at the point we capture `RaceInfo` (verified across real exports, and it is a genuine
//! null rather than the walker's `<cycle>`/`<max depth>` markers).
//!
//! The data does exist, in the RESPONSE PACKET — which Trackside already decompresses for every
//! request. One packet carries `race_horse_data_array`, `trained_chara_array` (each entry holding
//! `support_card_list`, i.e. the DECK, and `succession_chara_array`, i.e. the PARENTS with their
//! factors) and `race_scenario` together. That is precisely the shape Hakuraku's `parseNewFormat`
//! reads, so 1.1.5+ almost certainly dumps this packet rather than the game object.
//!
//! KEY ALIASES. Hakuraku's parser looks for `succession_chara_list` and `factor_data_array`; the
//! packet spells them `succession_chara_array` and `factor_info_array`. Rather than guess which
//! spelling the current site wants, we emit BOTH — the packet's own names and the aliases. Extra
//! keys are ignored by every reader, so this is compatible with old and new parsers at no risk.
//!
//! Only the race payload is written. No session or auth fields are copied (see `STRIP`).

#![allow(dead_code)]

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rmpv::Value;
use serde_json::{Map as JsonMap, Value as J};

use crate::msgpack::{contains, find_key, map_get, to_json};

/// The version we now declare. Matches the payload we actually emit — deck and parent info included.
const VIEWER_VERSION: &str = "1.1.7";

/// Session/auth/device fields — never written to disk. Same policy as `career_log`.
const STRIP: &[&str] = &[
    "viewer_id", "device", "device_id", "device_name", "graphics_device_name", "ip_address",
    "platform_os_version", "carrier", "keychain", "button_info", "dmm_viewer_id",
    "dmm_onetime_token", "steam_id", "steam_session_auth_ticket", "steam_session_ticket",
];

/// Payload keys copied into the export.
const KEEP: &[&str] = &[
    "race_horse_data_array", "trained_chara_array", "race_scenario", "race_start_info",
    "race_result_info", "random_seed", "weather", "ground_condition", "race_type",
    "race_instance_id", "program_id", "course_id", "lane_distance_max",
];

static WRITTEN: AtomicU64 = AtomicU64::new(0);
/// Most recent trained-chara payload seen on the wire, aliases already applied.
///
/// CACHE-AND-ATTACH, the same shape horseACT uses (it caches the array from a
/// `RaceUtil.SetTrainedCharaData` hook). The deck/parent data and the race replay arrive by
/// different routes — this packet, and the IL2CPP `RaceInfo` object respectively — so neither
/// export is complete on its own. Caching here lets `race_export` attach it to the replay dump.
static CACHED_TRAINED: Mutex<Option<J>> = Mutex::new(None);
static LAST_SIG: Mutex<Option<u64>> = Mutex::new(None);

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn scrub(v: &mut J) {
    match v {
        J::Object(m) => {
            for k in STRIP {
                m.remove(*k);
            }
            for (_, val) in m.iter_mut() {
                scrub(val);
            }
        }
        J::Array(a) => {
            for val in a.iter_mut() {
                scrub(val);
            }
        }
        _ => {}
    }
}

/// Add the aliases Hakuraku's parser looks for, keeping the packet's own spellings alongside.
///
/// `succession_chara_array` -> `succession_chara_list`, and within each parent
/// `factor_info_array` -> `factor_data_array`. Recursive because trained-chara records nest.
fn add_aliases(v: &mut J) {
    match v {
        J::Object(m) => {
            if let Some(sc) = m.get("succession_chara_array").cloned() {
                m.entry("succession_chara_list").or_insert(sc);
            }
            if let Some(fi) = m.get("factor_info_array").cloned() {
                m.entry("factor_data_array").or_insert(fi);
            }
            let vals: Vec<&mut J> = m.values_mut().collect();
            for val in vals {
                add_aliases(val);
            }
        }
        J::Array(a) => {
            for val in a.iter_mut() {
                add_aliases(val);
            }
        }
        _ => {}
    }
}

/// Cheap signature so the same race isn't written twice (several packets can carry it).
fn signature(v: &J) -> u64 {
    let s = v.get("race_horse_data_array").map(|h| h.to_string()).unwrap_or_default();
    let mut h: u64 = 1469598103934665603;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Called for every decompressed response. Writes an export when the race payload is present.
pub fn note_response(bytes: &[u8]) {
    if crate::friendlyplugins::horseact_active() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| crate::tools::log("[race-packet] horseACT is active - built-in export stands down"));
        return;
    }

    if !crate::settings::race_export() {
        return;
    }
    // Both keys are required: a race without trained-chara data is the old format and is already
    // covered by `race_export`'s RaceInfo dump.
    if !contains(bytes, b"race_horse_data_array") || !contains(bytes, b"trained_chara_array") {
        return;
    }
    let mut cur = std::io::Cursor::new(bytes);
    let Ok(root) = rmpv::decode::read_value(&mut cur) else { return };

    let mut out = JsonMap::new();
    for key in KEEP {
        let mut hits: Vec<&Value> = Vec::new();
        find_key(&root, key, &mut hits);
        // Prefer the richest copy — packets often carry a trimmed duplicate.
        let best = hits.into_iter().max_by_key(|v| match v {
            Value::Array(a) => a.len(),
            _ => 1,
        });
        if let Some(v) = best {
            let mut jv = to_json(v);
            scrub(&mut jv);
            out.insert((*key).to_string(), jv);
        }
    }
    if !out.contains_key("race_horse_data_array") || !out.contains_key("trained_chara_array") {
        return;
    }
    let mut doc = J::Object(out);
    add_aliases(&mut doc);
    if let Some(tc) = doc.get("trained_chara_array") {
        if let Ok(mut c) = CACHED_TRAINED.lock() {
            *c = Some(tc.clone());
        }
    }
    if let J::Object(m) = &mut doc {
        m.insert("horseACT_version".into(), J::String(VIEWER_VERSION.into()));
    }

    let sig = signature(&doc);
    {
        let Ok(mut last) = LAST_SIG.lock() else { return };
        if *last == Some(sig) {
            return; // same race, another packet
        }
        *last = Some(sig);
    }

    let dir = crate::paths::local_dir_migrated("trackside-races", "heaven-races").join("CM");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("cm-{}.json", now_ms()));
    let Ok(line) = serde_json::to_string(&doc) else { return };
    std::thread::spawn(move || {
        if let Ok(mut f) = std::fs::File::create(&path) {
            let _ = f.write_all(line.as_bytes());
        }
    });
    let n = WRITTEN.fetch_add(1, Ordering::Relaxed) + 1;
    crate::tools::log(&format!("[race-packet] wrote CM race export #{n} (horseACT {VIEWER_VERSION})"));
}

pub fn written() -> u64 {
    WRITTEN.load(Ordering::Relaxed)
}

/// The most recent trained-chara array seen, for `race_export` to attach to its replay dump.
pub fn cached_trained_chara() -> Option<J> {
    CACHED_TRAINED.lock().ok().and_then(|c| c.clone())
}
