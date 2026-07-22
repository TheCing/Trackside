//! Independent/Idle Training Career (自主トレ育成 / "SingleModeAutoPlay") capture — accrue setup +
//! result packages for empirical analysis. Cross-platform: the wire schema is identical on Global
//! (Steam) and JP (DMM), so the same content markers work on both. On JP this feeds from
//! `jp_capture` (libnative LZ4); on Global it feeds from the existing `response_hook`
//! (DecompressResponse) and `uma_bridge` (CryptoStream.Write) capture points.
//!
//! The mode is a server-side stochastic auto-raise: the client uploads a config (training policy +
//! race rotation + priority skills), the server rolls an abstract result (races are never physically
//! simulated — `result_time` is always 0), and hands back a source-decomposition (`progress_log_info`)
//! + the final chara at claim. The game retains neither side beyond "last config" + the trained
//! chara, so to study the algorithm across many runs we persist each run's **setup** and **result** as
//! clean JSON as they fly past.
//!
//! Detection is by CONTENT (no IL2CPP): a REQUEST carrying `training_policy_param_rate_set_id` is an
//! idle START (config); a RESPONSE carrying `progress_log_info` is an idle RESULT. Each is written to
//! `<game>/trackside-idle/<epoch_ms>_{setup,result}.json` with a one-line entry in `index.log`.
//! Correlate setup↔result offline by deck / chara / timestamp. Auth-envelope fields are stripped
//! from setups so no session tokens land in the dataset.

#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rmpv::Value;

use crate::msgpack::{as_arr, contains, map_get, to_json};

const SETUP_MARKER: &[u8] = b"training_policy_param_rate_set_id";
const RESULT_MARKER: &[u8] = b"progress_log_info";

/// Request-envelope auth/session fields — stripped from a persisted setup (not game-relevant, and we
/// don't want tokens sitting in a dataset).
const STRIP: &[&str] = &[
    "viewer_id", "device", "device_id", "device_name", "graphics_device_name", "ip_address",
    "platform_os_version", "carrier", "keychain", "locale", "button_info", "dmm_viewer_id",
    "dmm_onetime_token", "steam_id", "steam_session_auth_ticket",
];

static SETUPS: AtomicU64 = AtomicU64::new(0);
static RESULTS: AtomicU64 = AtomicU64::new(0);
static LATEST: Mutex<Option<String>> = Mutex::new(None);

fn dir() -> PathBuf {
    // On Global our DLL sits in the game root, so dll_dir() IS the game folder.
    let d = crate::paths::dll_dir().join("trackside-idle");
    let _ = std::fs::create_dir_all(&d);
    d
}
fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}
/// Decode msgpack to a Value. Responses are pure msgpack from offset 0; requests carry a fixed header
/// before the map (JP ~170 bytes; Global a 4-byte length prefix). We accept only a map whose keys are
/// all STRINGS — real API maps are string-keyed, so this rejects spurious maps decoded from binary
/// header noise (which produced junk "setups" with integer/array keys and could mask a real start).
/// Try the known offsets first, then scan as a fallback (covers both header formats).
fn decode(bytes: &[u8]) -> Option<Value> {
    fn string_keyed(v: &Value) -> bool {
        matches!(v, Value::Map(m) if !m.is_empty() && m.iter().all(|(k, _)| k.as_str().is_some()))
    }
    let mut try_off = |off: usize| -> Option<Value> {
        if off >= bytes.len() {
            return None;
        }
        let mut cur = &bytes[off..];
        match rmpv::decode::read_value(&mut cur) {
            Ok(v) if string_keyed(&v) => Some(v),
            _ => None,
        }
    };
    // 0 = responses; 170 = JP request msgpack after the fixed header.
    if let Some(v) = try_off(0).or_else(|| try_off(170)) {
        return Some(v);
    }
    for off in 0..bytes.len().min(260) {
        if let Some(v) = try_off(off) {
            return Some(v);
        }
    }
    None
}
fn int(v: Option<&Value>) -> i64 {
    v.and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|n| n as i64))).unwrap_or(0)
}
fn append_index(line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir().join("index.log")) {
        let _ = writeln!(f, "{line}");
    }
}

/// Called on every decompressed RESPONSE. Persists idle results (those carrying `progress_log_info`).
pub fn note_response(bytes: &[u8]) {
    if !contains(bytes, RESULT_MARKER) {
        return;
    }
    let Some(v) = decode(bytes) else { return };
    let data = map_get(&v, "data").unwrap_or(&v);

    let ci = map_get(data, "end_info").and_then(|e| map_get(e, "chara_info"));
    let g = |k: &str| int(ci.and_then(|c| map_get(c, k)));
    let (spd, sta, pw, gu, wz, skp, fans) =
        (g("speed"), g("stamina"), g("power"), g("guts"), g("wiz"), g("skill_point"), g("fans"));

    let races_arr = map_get(data, "progress_log_info")
        .and_then(|p| map_get(p, "race_history_array"))
        .and_then(as_arr);
    let races = races_arr.map(|a| a.len()).unwrap_or(0);
    let wins = races_arr
        .map(|a| a.iter().filter(|r| int(map_get(r, "race_history").and_then(|rh| map_get(rh, "result_rank"))) == 1).count())
        .unwrap_or(0);

    let ts = now_ms();
    let json = to_json(&v);
    let _ = std::fs::write(
        dir().join(format!("{ts}_result.json")),
        serde_json::to_vec_pretty(&json).unwrap_or_default(),
    );
    let n = RESULTS.fetch_add(1, Ordering::Relaxed) + 1;
    let summary = format!("result #{n}: spd{spd} sta{sta} pow{pw} gut{gu} wiz{wz} skp{skp} fans{fans} races{races} wins{wins}");
    append_index(&format!("{ts} {summary}"));
    if let Ok(mut l) = LATEST.lock() {
        *l = Some(summary.clone());
    }
    crate::tools::log(&format!("[idle] captured {summary}"));
}

/// Called on every REQUEST (plaintext, before compression/encryption). Persists idle setups (those
/// carrying the training-policy config), auth envelope stripped.
pub fn note_request(bytes: &[u8]) {
    if !contains(bytes, SETUP_MARKER) {
        return;
    }
    let Some(v) = decode(bytes) else { return };

    // Require the real idle-START envelope. The byte-marker alone matches other requests that merely
    // reference the field (and decode() can latch onto binary noise) — persisting those wrote junk
    // "setups" with garbage keys. A genuine start has BOTH the start-request common block and a
    // start_info carrying the policy id.
    let si = map_get(&v, "start_info");
    if map_get(&v, "single_mode_start_request_common").is_none()
        || si.and_then(|s| map_get(s, "training_policy_param_rate_set_id")).is_none()
    {
        return;
    }
    let policy = int(si.and_then(|s| map_get(s, "training_policy_param_rate_set_id")));
    let ground = int(si.and_then(|s| map_get(s, "training_policy_ground_type")));
    let prio = si.and_then(|s| map_get(s, "priority_skill_array")).and_then(as_arr).map(|a| a.len()).unwrap_or(0);
    let races = si.and_then(|s| map_get(s, "race_array")).and_then(as_arr).map(|a| a.len()).unwrap_or(0);

    let mut json = to_json(&v);
    if let serde_json::Value::Object(ref mut m) = json {
        for k in STRIP {
            m.remove(*k);
        }
    }
    let ts = now_ms();
    let _ = std::fs::write(
        dir().join(format!("{ts}_setup.json")),
        serde_json::to_vec_pretty(&json).unwrap_or_default(),
    );
    let n = SETUPS.fetch_add(1, Ordering::Relaxed) + 1;
    let summary = format!("setup #{n}: policy_rate={policy} ground={ground} priority_skills={prio} race_rotation={races}");
    append_index(&format!("{ts} {summary}"));
    crate::tools::log(&format!("[idle] captured {summary}"));
}

/// (setups, results) persisted this session.
pub fn stats() -> (u64, u64) {
    (SETUPS.load(Ordering::Relaxed), RESULTS.load(Ordering::Relaxed))
}
/// One-line summary of the most recent idle result, for the overlay.
pub fn latest() -> Option<String> {
    LATEST.lock().ok().and_then(|g| g.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value;

    fn enc(v: &Value) -> Vec<u8> {
        let mut b = Vec::new();
        rmpv::encode::write_value(&mut b, v).unwrap();
        b
    }
    fn map(pairs: Vec<(&str, Value)>) -> Value {
        Value::Map(pairs.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
    }

    /// Exercises the real detection/parse path against synthetic idle packets — proves the content
    /// markers and key names match, independent of how the optimizer lays out the string literals.
    /// One test (not three) so the shared SETUPS/RESULTS counters aren't raced by parallel tests.
    #[test]
    fn idle_capture_detection() {
        // ── idle RESULT (response carrying progress_log_info) ──
        let race = |rank: i64| map(vec![("race_history", map(vec![("result_rank", Value::from(rank))]))]);
        let result = map(vec![(
            "data",
            map(vec![
                ("progress_log_info", map(vec![("race_history_array", Value::Array(vec![race(1), race(4), race(1)]))])),
                ("end_info", map(vec![("chara_info", map(vec![("speed", Value::from(1200)), ("skill_point", Value::from(600))]))])),
            ]),
        )]);
        let r0 = stats().1;
        note_response(&enc(&result));
        assert_eq!(stats().1, r0 + 1, "idle result (progress_log_info) must be captured");

        // a plain response without the marker must be ignored
        note_response(&enc(&map(vec![("chara_info", Value::from("x"))])));
        assert_eq!(stats().1, r0 + 1, "non-idle response must be ignored");

        // ── idle SETUP (request carrying the start envelope) ──
        let setup = map(vec![
            ("single_mode_start_request_common", map(vec![("dummy", Value::from(1))])),
            ("start_info", map(vec![
                ("training_policy_param_rate_set_id", Value::from(3)),
                ("training_policy_ground_type", Value::from(1)),
                ("priority_skill_array", Value::Array(vec![Value::from(101), Value::from(202)])),
            ])),
        ]);
        let s0 = stats().0;
        note_request(&enc(&setup));
        assert_eq!(stats().0, s0 + 1, "idle setup (training_policy_param_rate_set_id) must be captured");

        // a request with the byte-marker but NOT the real start envelope must be rejected
        note_request(&enc(&map(vec![("training_policy_param_rate_set_id", Value::from(9))])));
        assert_eq!(stats().0, s0 + 1, "non-start request must be rejected");
    }
}
