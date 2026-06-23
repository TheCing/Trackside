//! HorseTheTrails — native in-process Team Trials capture (own-design extraction).
//!
//! Hooks the game's `TeamStadiumResult..ctor(...)` and, on each Team Trials
//! result, reads only the fields we need (by name) out of the managed response
//! object and writes them in **our own compact per-trial format** to
//! `data/htt/native/<trial_id>.json`. Heaven's `htt_import.py` parses the raw
//! `race_scenario` blobs (gzip+base64) and builds `team_trials_history.jsonl`.
//!
//! No generic reflection serializer — just targeted reads via Heaven's
//! GetProcAddress-resolved IL2CPP bindings, written straight into our schema.
//!
//! Hook point: `TeamStadiumResult..ctor`, verified directly from the game image.

#![allow(static_mut_refs)]

use core::ffi::CStr;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use retour::RawDetour;
use serde_json::{json, Map, Value};

use crate::htt_il2cpp::{self as h, RawImage, RawMethod, RawObject, Val};

static ENABLED: AtomicBool = AtomicBool::new(false);
static SAVED: AtomicUsize = AtomicUsize::new(0);
static ORIG: AtomicUsize = AtomicUsize::new(0);
static DETOUR: OnceLock<RawDetour> = OnceLock::new();

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}
/// Number of Team Trials results captured this session. Currently not surfaced in the
/// unified menu (the TT toggle is a plain on/off) — kept for the count display / future use.
#[allow(dead_code)]
pub fn saved() -> usize {
    SAVED.load(Ordering::Relaxed)
}

fn hlog(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("horsethetrails.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

type CtorFn = unsafe extern "C" fn(*mut RawObject, *mut RawObject, *const RawMethod);

unsafe extern "C" fn tt_ctor_hook(this: *mut RawObject, response: *mut RawObject, method: *const RawMethod) {
    let orig = ORIG.load(Ordering::Relaxed);
    if orig != 0 {
        let f: CtorFn = std::mem::transmute(orig);
        f(this, response, method);
    }
    // A Team Trials result is being built → tell the race-result skip to stand down
    // (it's a career-only feature and would otherwise jam the TT result UI). Set this
    // even when capture is OFF — the guard must hold regardless of the toggle.
    crate::skip::set_in_team_trials(true);
    // Hakuraku-format Team Trials export (independent toggle): walk the full result
    // payload to JSON under heaven-races/Team trials. No-op unless its toggle is on.
    #[cfg(feature = "raceread")]
    crate::race_export::dump_team_trials(response);
    if !ENABLED.load(Ordering::Relaxed) || response.is_null() {
        return;
    }
    // Never let a read fault take down the game (panic = abort).
    if let Some(val) = extract(response) {
        save(val);
    }
}

// ---------------------------------------------------------------------------
// Targeted extraction → our compact per-trial schema.
// ---------------------------------------------------------------------------

/// Locate the value that actually holds `race_result_array` — either the
/// response object directly, or its `data` member if it's a CommonResponse.
unsafe fn payload_of(response: *mut RawObject) -> Option<Val> {
    let root = h::val_of(response)?;
    if h::field_offset(root.klass, "race_result_array").is_some() {
        return Some(root);
    }
    if let Some(data) = h::read_ref(&root, "data") {
        if h::field_offset(data.klass, "race_result_array").is_some() {
            return Some(data);
        }
    }
    None
}

/// Find the race_start_params entry whose `round` matches, else fall back to index.
unsafe fn params_for_round(rsp: *mut RawObject, round: i32, idx: usize) -> Option<Val> {
    if rsp.is_null() {
        return None;
    }
    let n = h::array_len(rsp);
    for i in 0..n {
        if let Some(p) = h::array_elem(rsp, i) {
            if h::read_i32(&p, "round") == Some(round) {
                return Some(p);
            }
        }
    }
    h::array_elem(rsp, idx)
}

/// Within a race_horse_data_array, find the entry for a given trained_chara_id.
unsafe fn horse_for(horse_arr: Option<*mut RawObject>, tcid: i32) -> Option<Val> {
    let arr = horse_arr?;
    let n = h::array_len(arr);
    for i in 0..n {
        if let Some(hd) = h::array_elem(arr, i) {
            if h::read_i32(&hd, "trained_chara_id") == Some(tcid) {
                return Some(hd);
            }
        }
    }
    None
}

unsafe fn extract(response: *mut RawObject) -> Option<Value> {
    let payload = payload_of(response)?;

    let support_bonus = h::read_i32(&payload, "support_card_bonus").unwrap_or(0);
    let rr = h::read_ref_ptr(&payload, "race_result_array")?;
    let rsp = h::read_ref_ptr(&payload, "race_start_params_array");

    let mut trial_seed: Option<i32> = None;
    let mut races: Vec<Value> = Vec::new();

    let n_races = h::array_len(rr);
    for r in 0..n_races {
        let race = match h::array_elem(rr, r) {
            Some(v) => v,
            None => continue,
        };
        let round = h::read_i32(&race, "round").unwrap_or(r as i32 + 1);
        let distance_type = h::read_i32(&race, "distance_type").unwrap_or(0);
        let team_total = h::read_i32(&race, "team_total_score").unwrap_or(0);
        let scenario = h::read_ref_ptr(&race, "race_scenario")
            .and_then(|s| h::read_string(s))
            .unwrap_or_default();

        let params = params_for_round(rsp.unwrap_or(std::ptr::null_mut()), round, r);
        if trial_seed.is_none() {
            if let Some(p) = &params {
                // random_seed is unique per trial RUN (and stable when you re-view
                // the same result), so it makes a collision-free trial id. The old
                // race_instance_id was the *course* id, which repeats across trials
                // that share a course → file overwrites + bad dedup.
                trial_seed = h::read_i32(p, "random_seed").filter(|&s| s != 0)
                    .or_else(|| h::read_i32(p, "race_instance_id"));
            }
        }
        let horse_arr = params.as_ref().and_then(|p| h::read_ref_ptr(p, "race_horse_data_array"));

        // Track & Condition (stadium) fields for this round, read from its start
        // params. Safe by-name reads → absent fields just come back null. These
        // let Heaven re-populate stadium observations from the native capture
        // (previously only the mitmproxy team_stadium/start path produced them).
        let (race_instance_id, weather, ground_condition, season, round_seed) =
            if let Some(p) = &params {
                (h::read_i32(p, "race_instance_id"),
                 h::read_i32(p, "weather"),
                 h::read_i32(p, "ground_condition"),
                 h::read_i32(p, "season"),
                 h::read_i32(p, "random_seed"))
            } else {
                (None, None, None, None, None)
            };

        let cra = match h::read_ref_ptr(&race, "chara_result_array") {
            Some(a) => a,
            None => continue,
        };
        let mut charas: Vec<Value> = Vec::new();
        let n_h = h::array_len(cra);
        for hi in 0..n_h {
            let cr = match h::array_elem(cra, hi) {
                Some(v) => v,
                None => continue,
            };
            // Your team only.
            if h::read_i32(&cr, "team_id") != Some(1) {
                continue;
            }
            let tcid = h::read_i32(&cr, "trained_chara_id").unwrap_or(0);

            // display_score = sum of chara_result_array[i].score_array[].score
            let mut display_score: i64 = 0;
            if let Some(sa) = h::read_ref_ptr(&cr, "score_array") {
                for si in 0..h::array_len(sa) {
                    if let Some(se) = h::array_elem(sa, si) {
                        display_score += h::read_i32(&se, "score").unwrap_or(0) as i64;
                    }
                }
            }

            let mut o = Map::new();
            // horse_idx = position within the FULL chara_result_array — this is the
            // index the race_scenario events key their per-horse data on.
            o.insert("horse_idx".into(), json!(hi));
            o.insert("trained_chara_id".into(), json!(tcid));
            o.insert("finish_order".into(), json!(h::read_i32(&cr, "finish_order")));
            o.insert("finish_time".into(), json!(h::read_i32(&cr, "finish_time")));
            o.insert("display_score".into(), json!(display_score));

            if let Some(hd) = horse_for(horse_arr, tcid) {
                o.insert("card_id".into(), json!(h::read_i32(&hd, "card_id")));
                o.insert("chara_id".into(), json!(h::read_i32(&hd, "chara_id")));
                o.insert("speed".into(), json!(h::read_i32(&hd, "speed")));
                o.insert("stamina".into(), json!(h::read_i32(&hd, "stamina")));
                o.insert("power".into(), json!(h::read_i32(&hd, "pow")));
                o.insert("guts".into(), json!(h::read_i32(&hd, "guts")));
                o.insert("wiz".into(), json!(h::read_i32(&hd, "wiz")));
                o.insert("running_style".into(), json!(h::read_i32(&hd, "running_style")));
                // frame_order = starting gate (for stadium / Track & Condition).
                o.insert("frame_order".into(), json!(h::read_i32(&hd, "frame_order")));

                let mut owned: Vec<i32> = Vec::new();
                if let Some(sk) = h::read_ref_ptr(&hd, "skill_array") {
                    for si in 0..h::array_len(sk) {
                        if let Some(se) = h::array_elem(sk, si) {
                            if let Some(id) = h::read_i32(&se, "skill_id") {
                                owned.push(id);
                            }
                        }
                    }
                }
                o.insert("owned_skills".into(), json!(owned));
            }
            charas.push(Value::Object(o));
        }

        races.push(json!({
            "race_idx": r,
            "round": round,
            "distance_type": distance_type,
            "team_total_score": team_total,
            "race_scenario": scenario,
            "race_instance_id": race_instance_id,
            "weather": weather,
            "ground_condition": ground_condition,
            "season": season,
            "random_seed": round_seed,
            "charas": charas,
        }));
    }

    let captured_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let trial_id = match trial_seed {
        Some(s) if s != 0 => format!("tt_{}", s as u32),
        _ => format!("tt_ms_{captured_ms}"),
    };

    Some(json!({
        "trial_id": trial_id,
        "captured_ms": captured_ms,
        "support_card_bonus": support_bonus,
        "races": races,
    }))
}

fn save(val: Value) {
    let trial_id = val.get("trial_id").and_then(|v| v.as_str()).unwrap_or("tt_unknown").to_string();
    // Write into the Heaven dashboard's own data folder (portable across PCs).
    let dir = crate::paths::tt_capture_dir();
    let _ = std::fs::create_dir_all(&dir);
    // Stable filename by trial_id → re-viewing the same result overwrites, no dupes.
    let path = dir.join(format!("{trial_id}.json"));
    if let Ok(s) = serde_json::to_string(&val) {
        if let Ok(mut f) = std::fs::File::create(&path) {
            if f.write_all(s.as_bytes()).is_ok() {
                SAVED.fetch_add(1, Ordering::Relaxed);
                hlog(&format!("[htt] saved {}", path.display()));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Hook installation — resolve the ctor directly from the game image.
// ---------------------------------------------------------------------------

pub(crate) unsafe fn find_game_image() -> *mut RawImage {
    for n in [
        "umamusume",
        "Assembly-CSharp",
        "Gallop",
    ] {
        let img = find_image_by_name(n);
        if !img.is_null() {
            return img;
        }
    }
    std::ptr::null_mut()
}

unsafe fn find_image_by_name(name: &str) -> *mut RawImage {
    let domain = h::DOMAIN_GET.unwrap()();
    if domain.is_null() {
        return std::ptr::null_mut();
    }
    let mut count = 0usize;
    let asms = h::DOMAIN_GET_ASSEMBLIES.unwrap()(domain, &mut count);
    if asms.is_null() {
        return std::ptr::null_mut();
    }
    for i in 0..count {
        let a = *asms.add(i);
        if a.is_null() {
            continue;
        }
        let img = h::ASSEMBLY_GET_IMAGE.unwrap()(a);
        if img.is_null() {
            continue;
        }
        let np = h::IMAGE_GET_NAME.unwrap()(img);
        if np.is_null() {
            continue;
        }
        let nm = CStr::from_ptr(np).to_string_lossy();
        if nm.eq_ignore_ascii_case(name) || nm.trim_end_matches(".dll").eq_ignore_ascii_case(name) {
            return img;
        }
    }
    std::ptr::null_mut()
}

/// Find a class in the image whose (namespace-less) name contains `needle`.
unsafe fn find_class(image: *mut RawImage, needle: &str) -> *mut crate::htt_il2cpp::RawClass {
    let count = h::IMAGE_GET_CLASS_COUNT.unwrap()(image);
    let mut fuzzy: *mut crate::htt_il2cpp::RawClass = std::ptr::null_mut();
    for i in 0..count {
        let klass = h::IMAGE_GET_CLASS.unwrap()(image, i);
        if klass.is_null() {
            continue;
        }
        let name = h::class_name(klass);
        if name == needle {
            return klass; // exact match wins
        }
        if fuzzy.is_null() && name.contains(needle) {
            fuzzy = klass;
        }
    }
    fuzzy
}

/// Resolve the TeamStadiumResult ctor, install the detour.
/// Must run on an IL2CPP-attached thread (boot thread, before detach).
pub fn install() -> String {
    unsafe {
        if !h::init() {
            return "il2cpp reflection init failed".into();
        }
        let image = find_game_image();
        if image.is_null() {
            return "game image not found".into();
        }
        let klass = find_class(image, "TeamStadiumResult");
        if klass.is_null() {
            return "result class not found".into();
        }
        let class_name = h::class_name(klass);

        let get_method = match h::CLASS_GET_METHOD_FROM_NAME {
            Some(f) => f,
            None => return "class_get_method_from_name unavailable".into(),
        };
        // 1-arg ctor (the CommonResponse/payload). Fall back to 2 args if needed.
        let mut ctor = {
            let c = std::ffi::CString::new(".ctor").unwrap();
            let m = get_method(klass, c.as_ptr(), 1);
            if m.is_null() {
                get_method(klass, c.as_ptr(), 2)
            } else {
                m
            }
        };
        if ctor.is_null() {
            // Last resort: any .ctor with 0..=3 params.
            let c = std::ffi::CString::new(".ctor").unwrap();
            for argc in 0..=3 {
                let m = get_method(klass, c.as_ptr(), argc);
                if !m.is_null() {
                    ctor = m;
                    break;
                }
            }
        }
        if ctor.is_null() {
            return format!("no .ctor found on {class_name}");
        }

        let fnptr = h::method_addr(ctor);
        if fnptr == 0 {
            return "method pointer null".into();
        }
        if crate::il2cpp::is_detoured(fnptr as *const std::ffi::c_void) {
            return "already detoured (skipped)".into();
        }
        match RawDetour::new(fnptr as *const (), tt_ctor_hook as *const ()) {
            Ok(d) => {
                if d.enable().is_err() {
                    return "detour enable failed".into();
                }
                ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = DETOUR.set(d);
                format!("hooked {class_name}..ctor (targeted extraction)")
            }
            Err(e) => format!("detour failed: {e}"),
        }
    }
}
