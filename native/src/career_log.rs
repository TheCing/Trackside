//! Career Log — persist every training career, turn by turn, for offline analysis.
//!
//! Trackside already sees every decompressed response; this consumer keeps the career-relevant
//! slice of each turn so a finished run can be studied later (training-gain modelling, deck
//! comparisons, "did I hit my stat targets" post-mortems). It replaces having to remember to run a
//! companion tool live — if the overlay is loaded, the career is logged.
//!
//! Output: `<game>/trackside-careers/<epoch>_<card_id>_s<scenario>.jsonl`, ONE JSON OBJECT PER LINE
//! (one line per turn). JSONL specifically so a crash or an alt-F4 mid-career still leaves a valid,
//! parseable file — no closing bracket required.
//!
//! What is kept (and why it is small): only the fields a model needs — the chara block (stats, deck,
//! bonds, motivation, energy), the per-facility `command_info_array` (the game's own projected gains),
//! and `live_data_set` for scenario state. Race replays, story payloads and art references are
//! dropped; on a sample career that is ~19% of the raw size (11.8 MB -> 2.2 MB).
//!
//! **No credentials, ever.** `STRIP` removes session/auth/device fields, so these files are safe to
//! share when reporting a bug — unlike raw packet dumps. (Same policy as `jp_idle`.)
//!
//! Cost: a few hundred KB per career, an off-thread append per turn. Default ON, disable under
//! Gameplay -> Career log.

#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rmpv::Value;
use serde_json::{json, Map as JsonMap, Value as J};

use crate::msgpack::{as_arr, contains, find_key, map_get, to_json};

/// Session/auth/device fields — never written to disk.
const STRIP: &[&str] = &[
    "viewer_id", "device", "device_id", "device_name", "graphics_device_name", "ip_address",
    "platform_os_version", "carrier", "keychain", "locale", "button_info", "dmm_viewer_id",
    "dmm_onetime_token", "steam_id", "steam_session_auth_ticket", "steam_session_ticket",
];

/// chara_info fields worth keeping (everything a training model or a post-mortem needs).
const CHARA_KEEP: &[&str] = &[
    "turn", "scenario_id", "card_id", "single_mode_chara_id",
    "speed", "stamina", "power", "guts", "wiz", "skill_point",
    "max_speed", "max_stamina", "max_power", "max_guts", "max_wiz",
    "vital", "max_vital", "motivation", "fans", "state", "playing_state",
    "support_card_array", "evaluation_info_array", "training_level_info_array",
    "chara_effect_id_array", "skill_array", "skill_tips_array", "race_program_id",
    // Events that fired this turn. Kept so a captured career can be REPLAYED offline against
    // event-driven features (the chain tracker's per-card progress is derived purely from these
    // story_ids). Without it, every change to that logic costs a full in-game career to verify -
    // which is exactly what it cost while the tracker was being built.
    "unchecked_event_array",
];

/// live_data_set fields (Grand Live / scenario state).
const LIVE_KEEP: &[&str] = &[
    "command_info_array", "live_performance_info", "training_bonus_array",
    "next_square_info_array", "master_live_id_array", "next_live_id_array",
    "effected_live_id_array", "live_result_array", "reserve_square_id",
];

/// Keep at most this many career files; oldest are pruned.
const MAX_CAREERS: usize = 300;

static TURNS: AtomicU64 = AtomicU64::new(0);
static CAREERS: AtomicU64 = AtomicU64::new(0);
static LAST_TURN: AtomicI64 = AtomicI64::new(-1);
static LAST_CARD: AtomicI64 = AtomicI64::new(-1);
static CURRENT: Mutex<Option<PathBuf>> = Mutex::new(None);
static LATEST: Mutex<Option<String>> = Mutex::new(None);

fn dir() -> PathBuf {
    let d = crate::paths::dll_dir().join("trackside-careers");
    let _ = std::fs::create_dir_all(&d);
    d
}
fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}
fn int(v: Option<&Value>) -> i64 {
    v.and_then(|x| x.as_i64().or_else(|| x.as_u64().map(|n| n as i64))).unwrap_or(0)
}

/// Recursively drop any STRIP-listed key from a decoded JSON tree (defence in depth: the fields we
/// copy shouldn't contain them, but never write a token by accident).
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

/// The chara_info map that carries the live stat block (packets can hold trimmed copies).
fn live_chara(root: &Value) -> Option<&Value> {
    let mut hits: Vec<&Value> = Vec::new();
    find_key(root, "chara_info", &mut hits);
    hits.into_iter().find(|c| c.is_map() && map_get(c, "speed").is_some())
}

fn subset(v: &Value, keys: &[&str]) -> J {
    let mut m = JsonMap::new();
    for k in keys {
        if let Some(x) = map_get(v, k) {
            let mut jv = to_json(x);
            scrub(&mut jv);
            m.insert((*k).to_string(), jv);
        }
    }
    J::Object(m)
}

/// Drop the oldest files once the folder exceeds MAX_CAREERS.
fn prune() {
    let Ok(rd) = std::fs::read_dir(dir()) else { return };
    let mut files: Vec<_> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .filter_map(|e| e.metadata().ok().and_then(|m| m.modified().ok()).map(|t| (t, e.path())))
        .collect();
    if files.len() <= MAX_CAREERS {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    for (_, p) in files.iter().take(files.len() - MAX_CAREERS) {
        let _ = std::fs::remove_file(p);
    }
}

/// Called on every decompressed RESPONSE carrying a career state. Appends one line per turn.
pub fn note_response(bytes: &[u8]) {
    if !crate::settings::career_log() {
        return;
    }
    if !contains(bytes, b"chara_info") || !contains(bytes, b"turn") {
        return;
    }
    let mut cur = std::io::Cursor::new(bytes);
    let Ok(root) = rmpv::decode::read_value(&mut cur) else { return };
    let Some(ci) = live_chara(&root) else { return };

    let turn = int(map_get(ci, "turn"));
    let card = int(map_get(ci, "card_id"));
    let scenario = int(map_get(ci, "scenario_id"));
    if turn <= 0 || card <= 0 {
        return;
    }

    // A new career = the trainee changed, or the turn counter went backwards.
    let prev_turn = LAST_TURN.load(Ordering::Relaxed);
    let prev_card = LAST_CARD.load(Ordering::Relaxed);
    let new_career = prev_card != card || turn < prev_turn;
    if turn == prev_turn && prev_card == card {
        return; // same turn re-sent (several packets per turn) — keep the first
    }
    LAST_TURN.store(turn, Ordering::Relaxed);
    LAST_CARD.store(card, Ordering::Relaxed);

    let path = {
        let Ok(mut slot) = CURRENT.lock() else { return };
        if new_career || slot.is_none() {
            let p = dir().join(format!("{}_{card}_s{scenario}.jsonl", now_ms()));
            let n = CAREERS.fetch_add(1, Ordering::Relaxed) + 1;
            crate::tools::log(&format!(
                "[career] new career #{n}: card {card}, scenario {scenario} -> {}",
                p.file_name().and_then(|s| s.to_str()).unwrap_or("?")
            ));
            *slot = Some(p);
            prune();
        }
        slot.clone().unwrap()
    };

    // Build the trimmed turn record.
    let mut rec = JsonMap::new();
    rec.insert("chara_info".into(), subset(ci, CHARA_KEEP));
    let mut home_cmds: Vec<&Value> = Vec::new();
    find_key(&root, "command_info_array", &mut home_cmds);
    if let Some(arr) = home_cmds.iter().find(|a| {
        as_arr(a).map(|v| v.iter().any(|c| map_get(c, "params_inc_dec_info_array").is_some())).unwrap_or(false)
    }) {
        let mut jv = to_json(arr);
        scrub(&mut jv);
        rec.insert("command_info_array".into(), jv);
    }
    let mut live: Vec<&Value> = Vec::new();
    find_key(&root, "live_data_set", &mut live);
    if let Some(l) = live.into_iter().find(|l| l.is_map()) {
        rec.insert("live_data_set".into(), subset(l, LIVE_KEEP));
    }
    let line = match serde_json::to_string(&json!(rec)) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Off-thread append so the game's network thread never waits on disk.
    std::thread::spawn(move || {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
        }
    });

    let n = TURNS.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut l) = LATEST.lock() {
        *l = Some(format!("turn {turn} (scenario {scenario}) \u{2014} {n} turns logged"));
    }
}

/// (careers started this session, turns written this session)
pub fn stats() -> (u64, u64) {
    (CAREERS.load(Ordering::Relaxed), TURNS.load(Ordering::Relaxed))
}
pub fn latest() -> Option<String> {
    LATEST.lock().ok().and_then(|l| l.clone())
}
/// How many career files are on disk. CACHED - see below.
///
/// This is called from the overlay panel, which redraws on the RENDER THREAD every frame. The
/// uncached version ran `create_dir_all` plus a full `read_dir` enumeration (up to MAX_CAREERS =
/// 300 entries, each with an extension check) at frame rate, against the game's install directory.
/// That is blocking I/O on the render thread, and it deadlocked the game: the render thread stalled
/// in the filesystem while the main thread waited on it, both frozen with no crash. The hang
/// watchdog caught it as `last UI step: 'ui:careerlog'`.
///
/// The count only changes when a career starts, so a stale value for a second is harmless. Refresh
/// is time-bounded and the scan happens at most once per REFRESH interval no matter how many frames
/// ask.
pub fn files_on_disk() -> usize {
    use std::sync::atomic::AtomicUsize;
    const REFRESH: std::time::Duration = std::time::Duration::from_secs(3);
    static CACHED: AtomicUsize = AtomicUsize::new(0);
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let due = {
        let Ok(mut last) = LAST.lock() else { return CACHED.load(Ordering::Relaxed) };
        let due = last.map(|t| t.elapsed() >= REFRESH).unwrap_or(true);
        if due {
            *last = Some(std::time::Instant::now());
        }
        due
    };
    if due {
        // `dir()` is deliberately NOT used here: it calls create_dir_all, and a syscall per frame
        // was half the original problem. Read-only path, and a missing folder just counts as zero.
        let path = crate::paths::dll_dir().join("trackside-careers");
        let n = std::fs::read_dir(path)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
                    .count()
            })
            .unwrap_or(0);
        CACHED.store(n, Ordering::Relaxed);
    }
    CACHED.load(Ordering::Relaxed)
}
