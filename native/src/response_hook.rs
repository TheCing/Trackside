//! Heaven — the single `Gallop.HttpHelper::DecompressResponse` hook.
//!
//! One detour reads every decrypted + lz4-decompressed msgpack API response and fans it out:
//!   - to the companion-overlay bridge (`uma_bridge`), for ALL responses;
//!   - the player-horse identity (the one with `viewer_id != 0`) → `race::set_net_player`
//!     (+ freecam auto-follow), so the race-result Top-1 skip knows if you WON;
//!   - remaining race retries (`available_continue_num`) → `race::set_continues_available`;
//!   - (full build only) extra career payloads handled by additional consumers.
//!
//! Read-only: it calls the original, reads the decompressed result, and returns it UNCHANGED. If a
//! co-resident mod already detoured DecompressResponse (e.g. a spark collector) we CHAIN on top —
//! both hooks are read-only, so the response passes through both. This replaces the former duplicate
//! response hooks that lived in the full build.rs and the response hook.rs.

#![allow(dead_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;
use rmpv::Value;

use crate::htt_il2cpp as h;
use crate::msgpack::{as_arr, contains, find_key, map_get};

fn log(msg: &str) {
    crate::tools::log(msg);
}

static INSTALLED: AtomicBool = AtomicBool::new(false);
static ORIG: AtomicUsize = AtomicUsize::new(0);
static DETOUR: OnceLock<RawDetour> = OnceLock::new();

type DecompStaticFn = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;
type DecompInstFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> *mut c_void;

/// Size ceiling for the "capture" markers (breeding rentals + player_state). Real
/// Team-Trials and career-start responses are a few hundred KB; the master-data /
/// resource responses the game pulls at first launch are many MB. See `gate_captures`.
const CAPTURE_SIZE_CAP: usize = 4 * 1024 * 1024;

/// Decide whether the two capture parsers should run for this response — pure so the size
/// gate (the fix for the first-launch "Download Error") is unit-tested below.
///
/// `parse_rentals` / `parse_player_state` clone and fully msgpack-decode the WHOLE response.
/// Their markers are short, generic byte strings ("rp_info", "total_score_info", …) that
/// occur incidentally inside the large master-data blobs downloaded at first launch. Before
/// this gate, matching one defeated the cheap early-return and made us decode a multi-MB blob
/// on the game's network thread, stalling the response until its download step timed out.
/// A large response is never one of ours, so above the cap we report neither — restoring the
/// old build's cheap early-return for those responses. Fails safe: a real payload somehow
/// over the cap is only *missed*, never a stall.
fn gate_captures(slice: &[u8], len: usize, pstate_on: bool) -> (bool, bool) {
    if len >= CAPTURE_SIZE_CAP {
        return (false, false);
    }
    let rentals = contains(slice, b"succession_trained_chara_data");
    let pstate = pstate_on
        && crate::player_state::ENDPOINTS
            .iter()
            .any(|(_, key)| contains(slice, key.as_bytes()));
    (rentals, pstate)
}

unsafe fn on_response(ret: *mut c_void) {
    if ret.is_null() {
        return;
    }
    let len = h::array_len(ret as *mut h::RawObject);
    if len == 0 || len > 50 * 1024 * 1024 {
        return;
    }
    let data = (ret as *mut u8).add(0x20);
    let slice = std::slice::from_raw_parts(data, len);
    // Feed the plain response to the companion-overlay bridge (all responses, before our filter).
    crate::uma_bridge::send_response(slice);

    let has_race = contains(slice, b"race_horse_data");
    let has_cont = contains(slice, b"available_continue_num");
    let has_chara = contains(slice, b"chara_info") && !contains(slice, b"limited_shop_info");
    let has_event = contains(slice, b"choice_array") || contains(slice, b"choice_reward_array");
    // Career complete: the freshly-registered trained chara carries the game's OFFICIAL
    // rank_score — the calibration reference for the advisor's rating model.
    let has_trained = contains(slice, b"add_trained_chara_array");
    // Veteran roster (UmaExtractor-format data.json export). NOTE: this byte-scan also matches
    // "add_trained_chara_array" — parse_veterans uses exact key matching, so that's harmless.
    let has_vets = contains(slice, b"trained_chara_array");
    // Career start (pre_single_mode/index): friends' BORROWABLE parents — the rental half of
    // the dashboard's Breed Optimizer. Team Trials player state ("Your status"): four responses
    // identified by a key only each carries. Both go through the size-gated helper below.
    let (has_rentals, has_pstate) = gate_captures(slice, len, crate::player_state::enabled());

    if !has_race && !has_cont && !has_chara && !has_event && !has_trained && !has_vets && !has_rentals && !has_pstate {
        return;
    }
    // Verbose: which packet types this response carried + its size. The single most useful
    // trace for "did Trackside even see my data" questions.
    if crate::tools::debug_enabled() {
        let mut kinds: Vec<&str> = Vec::new();
        if has_race { kinds.push("race"); }
        if has_cont { kinds.push("continues"); }
        if has_chara { kinds.push("chara_info"); }
        if has_event { kinds.push("event"); }
        if has_trained { kinds.push("trained_chara"); }
        if has_vets { kinds.push("veterans"); }
        if has_rentals { kinds.push("rentals"); }
        if has_pstate { kinds.push("player_state"); }
        crate::tools::debug(&format!("[response] {} bytes -> [{}]", len, kinds.join(", ")));
    }
    let bytes = slice.to_vec();
    if has_race {
        parse_race(&bytes);
    }
    if has_cont {
        parse_continues(&bytes);
    }
    if has_chara {
        parse_chara(&bytes);
    }
    if has_trained {
        parse_trained(&bytes);
    }
    if has_event {
        parse_event(&bytes);
    }
    if has_vets {
        parse_veterans(&bytes);
    }
    if has_rentals {
        parse_rentals(&bytes);
    }
    if has_pstate {
        parse_player_state(&bytes);
    }
}

/// Training-event breadcrumb: name every event as it appears in `unchecked_event_array`, so a
/// crash-truncated log shows which event SuperSkip's SkipStory fired on, and arm the
/// confirm-flow crash guard for the acupuncturist-type events.
fn parse_event(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let val = match rmpv::decode::read_value(&mut cur) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut hits: Vec<&Value> = Vec::new();
    find_key(&val, "unchecked_event_array", &mut hits);
    for arr in hits {
        let Some(list) = as_arr(arr) else { continue };
        for ev in list {
            if !ev.is_map() {
                continue;
            }
            // ALWAYS-ON breadcrumb: name every event as it appears, so a crash-truncated log shows
            // which event SuperSkip's SkipStory fired on right before a hang/crash. This is the
            // passive capture for the "Just an Acupuncturist" choice-of-reward crash (a rare event
            // that can't be reproduced on demand) — correlate this line with the next [event]
            // SkipStory() line and the point the log ends.
            let sid = map_get(ev, "story_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let eid = map_get(ev, "event_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let n_choices = map_get(ev, "event_contents_info")
                .and_then(|c| map_get(c, "choice_array"))
                .and_then(as_arr)
                .map(|a| a.len())
                .unwrap_or(0);
            let title = crate::event_titles::event_title(sid);
            crate::tools::log(&format!(
                "[event] appeared: story={sid} event={eid} choices={n_choices} title=\"{title}\""
            ));
            // Arm the confirm-flow crash guard (suppresses event-skip for the acupuncturist-type
            // event that crashes on "go back"). No-op for ordinary events.
            crate::skip::event::note_event_appeared(sid);
        }
    }
}

/// Veteran roster capture (UmaExtractor-format export): find the EXACT `trained_chara_array`
/// key (the byte-scan gate also matches add_trained_chara_array — find_key does not), take the
/// largest array in the packet, and hand it to umas as pretty-printed verbatim JSON.
fn parse_veterans(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let val = match rmpv::decode::read_value(&mut cur) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut hits: Vec<&Value> = Vec::new();
    find_key(&val, "trained_chara_array", &mut hits);
    let Some(arr) = hits.iter().filter_map(|v| as_arr(v)).max_by_key(|a| a.len()) else { return };
    if arr.is_empty() {
        return;
    }
    let json_arr: Vec<serde_json::Value> = arr.iter().map(crate::msgpack::to_json).collect();
    // Same entries feed the Breed Optimizer's "mine" half — the dashboard's own
    // data.json import hands these in as `trained_chara`, so the shape already fits.
    crate::breeding_trace::set_mine(json_arr.clone());
    if let Ok(json) = serde_json::to_string_pretty(&json_arr) {
        crate::umas::set_veterans_snapshot(json, arr.len());
    }
}

/// Friends' BORROWABLE parents from the career-start response. `succession_trained_chara_data`
/// holds `succession_trained_chara_array` (the parents) + `summary_user_info_array` (their
/// owners' names); the dashboard reads both, so we pass the block through verbatim.
fn parse_rentals(bytes: &[u8]) {
    // ALWAYS-ON breadcrumb: this packet is rare (career start only) and we can only
    // listen for it — unlike upstream, which called pre_single_mode/index outright. So
    // name it whenever the marker hits, to tell "the screen was never opened" apart
    // from "the screen was opened but we failed to read it".
    let mut cur = std::io::Cursor::new(bytes);
    let Ok(val) = rmpv::decode::read_value(&mut cur) else {
        crate::tools::log("[breeding] career-start packet seen but msgpack decode failed");
        return;
    };
    let mut hits: Vec<&Value> = Vec::new();
    find_key(&val, "succession_trained_chara_data", &mut hits);
    if hits.is_empty() {
        crate::tools::log(
            "[breeding] career-start packet seen but no succession_trained_chara_data key \
             (byte-marker matched something else — dump with Verbose on)",
        );
        return;
    }
    // Pick the richest block — the packet can carry trimmed copies nested elsewhere.
    let Some(blk) = hits
        .into_iter()
        .filter(|v| v.is_map())
        .max_by_key(|v| {
            map_get(v, "succession_trained_chara_array")
                .and_then(as_arr)
                .map(|a| a.len())
                .unwrap_or(0)
        })
    else {
        crate::tools::log("[breeding] succession_trained_chara_data present but not a map");
        return;
    };
    let n = map_get(blk, "succession_trained_chara_array")
        .and_then(as_arr)
        .map(|a| a.len())
        .unwrap_or(0);
    if n == 0 {
        // Real case, not a bug: the career-start flow has more than one step, and the
        // early ones carry the block with an empty parent list.
        crate::tools::log("[breeding] career-start packet seen, borrowable list empty (0 parents)");
        return;
    }
    crate::breeding_trace::set_rentals(crate::msgpack::to_json(blk));
}

/// Team Trials player state. The endpoint name isn't in the body, so each of the four
/// responses is identified by a key only it carries (see `player_state::ENDPOINTS`). We hand
/// over the whole `data` map — the dashboard owns the field list.
fn parse_player_state(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let Ok(val) = rmpv::decode::read_value(&mut cur) else { return };
    // The response is {data: {...}, data_headers: {...}} — the extractors' paths are relative
    // to `data`. Fall back to the root if this response isn't wrapped.
    let data = map_get(&val, "data").filter(|v| v.is_map()).unwrap_or(&val);
    for (endpoint, key) in crate::player_state::ENDPOINTS {
        // Exact key match at the data level — the byte-scan that got us here is only a prefilter.
        if map_get(data, key).is_some() {
            crate::player_state::record(endpoint, crate::msgpack::to_json(data));
        }
    }
}

/// Career-complete: pull the new trained chara (official rank_score + final stats + skills)
/// and hand it to the advisor's rating-model calibration.
fn parse_trained(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let val = match rmpv::decode::read_value(&mut cur) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut hits: Vec<&Value> = Vec::new();
    find_key(&val, "add_trained_chara_array", &mut hits);
    for arr in hits {
        let Some(list) = as_arr(arr) else { continue };
        for tc in list {
            if !tc.is_map() {
                continue;
            }
            let rank_score = i32_field(tc, "rank_score");
            if rank_score <= 0 {
                continue;
            }
            let mut skill_array = Vec::new();
            if let Some(sk) = map_get(tc, "skill_array").and_then(as_arr) {
                for s in sk {
                    let sid = i32_field(s, "skill_id");
                    if sid != 0 {
                        skill_array.push(crate::skill_advisor::OwnedSkill {
                            skill_id: sid,
                            level: i32_field(s, "level"),
                        });
                    }
                }
            }
            let info = crate::skill_advisor::CharaInfo {
                skill_point: 0,
                card_id: i32_field(tc, "card_id"),
                talent_level: i32_field(tc, "talent_level").max(1),
                speed: i32_field(tc, "speed"),
                stamina: i32_field(tc, "stamina"),
                power: i32_field(tc, "power"),
                guts: i32_field(tc, "guts"),
                wiz: i32_field(tc, "wiz"),
                proper_ground_turf: i32_field(tc, "proper_ground_turf"),
                proper_ground_dirt: i32_field(tc, "proper_ground_dirt"),
                proper_distance_short: i32_field(tc, "proper_distance_short"),
                proper_distance_mile: i32_field(tc, "proper_distance_mile"),
                proper_distance_middle: i32_field(tc, "proper_distance_middle"),
                proper_distance_long: i32_field(tc, "proper_distance_long"),
                proper_running_style_nige: i32_field(tc, "proper_running_style_nige"),
                proper_running_style_senko: i32_field(tc, "proper_running_style_senko"),
                proper_running_style_sashi: i32_field(tc, "proper_running_style_sashi"),
                proper_running_style_oikomi: i32_field(tc, "proper_running_style_oikomi"),
                skill_array,
                skill_tips_array: Vec::new(),
                has_fast_learner: false,
            };
            log(&format!("[response] trained chara: rank_score={rank_score} (calibrating rating model)"));
            crate::skill_advisor::calibrate_against(rank_score, &info);
            return;
        }
    }
}

/// Find the player's horse in `race_horse_data` (the one with `viewer_id != 0`; NPCs are all 0)
/// and publish its array index + `frame_order` for the race module.
fn parse_race(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let val = match rmpv::decode::read_value(&mut cur) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut arrs: Vec<&Value> = Vec::new();
    find_key(&val, "race_horse_data", &mut arrs);
    for a in arrs {
        if let Some(list) = as_arr(a) {
            for (i, hh) in list.iter().enumerate() {
                let vid = map_get(hh, "viewer_id").and_then(|x| x.as_i64()).unwrap_or(0);
                if vid != 0 {
                    let fo = map_get(hh, "frame_order").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                    log(&format!(
                        "[response] race player: arrIdx={i} frame_order={fo} viewer={vid} horses={}",
                        list.len()
                    ));
                    crate::race::set_net_player(i as i32, fo, list.len() as i32);
                    // Auto-frame the player's Uma at race start (freecam build only).
                    #[cfg(feature = "freecam")]
                    crate::freecam::auto_follow_player(fo);
                    return;
                }
            }
        }
    }
}

/// Read `available_continue_num` (remaining race retries) and publish it so the race-result skip
/// can auto-advance once no retries remain.
fn parse_continues(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let val = match rmpv::decode::read_value(&mut cur) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut hits: Vec<&Value> = Vec::new();
    find_key(&val, "available_continue_num", &mut hits);
    if let Some(n) = hits.first().and_then(|v| v.as_i64()) {
        crate::race::set_continues_available(n as i32);
    }
}

fn i32_field(v: &Value, key: &str) -> i32 {
    map_get(v, key).and_then(|x| x.as_i64()).unwrap_or(0) as i32
}

/// Capture end-of-career `chara_info` for the skill buy optimizer (Gameplay tab).
fn parse_chara(bytes: &[u8]) {
    let mut cur = std::io::Cursor::new(bytes);
    let val = match rmpv::decode::read_value(&mut cur) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut hits: Vec<&Value> = Vec::new();
    find_key(&val, "chara_info", &mut hits);
    for ci in hits {
        if !ci.is_map() {
            continue;
        }
        let mut skill_array = Vec::new();
        if let Some(arr) = map_get(ci, "skill_array").and_then(as_arr) {
            for s in arr {
                let sid = i32_field(s, "skill_id");
                if sid == 0 {
                    continue;
                }
                skill_array.push(crate::skill_advisor::OwnedSkill {
                    skill_id: sid,
                    level: i32_field(s, "level"),
                });
            }
        }
        let mut skill_tips_array = Vec::new();
        if let Some(arr) = map_get(ci, "skill_tips_array").and_then(as_arr) {
            for t in arr {
                skill_tips_array.push(crate::skill_advisor::SkillTip {
                    group_id: i32_field(t, "group_id"),
                    rarity: i32_field(t, "rarity"),
                    level: i32_field(t, "level").max(1),
                });
            }
        }
        // Fast Learner (切れ者) is condition id 7 in chara_effect_id_array — an extra 10%
        // off every skill purchase, on top of hint discounts.
        let has_fast_learner = map_get(ci, "chara_effect_id_array")
            .and_then(as_arr)
            .map(|arr| arr.iter().any(|v| v.as_i64() == Some(7)))
            .unwrap_or(false);
        let info = crate::skill_advisor::CharaInfo {
            skill_point: i32_field(ci, "skill_point"),
            card_id: i32_field(ci, "card_id"),
            talent_level: i32_field(ci, "talent_level").max(1),
            speed: i32_field(ci, "speed"),
            stamina: i32_field(ci, "stamina"),
            power: i32_field(ci, "power"),
            guts: i32_field(ci, "guts"),
            wiz: i32_field(ci, "wiz"),
            proper_ground_turf: i32_field(ci, "proper_ground_turf"),
            proper_ground_dirt: i32_field(ci, "proper_ground_dirt"),
            proper_distance_short: i32_field(ci, "proper_distance_short"),
            proper_distance_mile: i32_field(ci, "proper_distance_mile"),
            proper_distance_middle: i32_field(ci, "proper_distance_middle"),
            proper_distance_long: i32_field(ci, "proper_distance_long"),
            proper_running_style_nige: i32_field(ci, "proper_running_style_nige"),
            proper_running_style_senko: i32_field(ci, "proper_running_style_senko"),
            proper_running_style_sashi: i32_field(ci, "proper_running_style_sashi"),
            proper_running_style_oikomi: i32_field(ci, "proper_running_style_oikomi"),
            skill_array,
            skill_tips_array,
            has_fast_learner,
        };
        log(&format!(
            "[response] chara_info: sp={} card={} skills={} hints={} fast_learner={}",
            info.skill_point,
            info.card_id,
            info.skill_array.len(),
            info.skill_tips_array.len(),
            info.has_fast_learner
        ));
        crate::skill_advisor::set_chara_info(info);
        return;
    }
}

unsafe extern "C" fn hook_static(arg0: *mut c_void, m: *const c_void) -> *mut c_void {
    let t0 = std::time::Instant::now();
    let ret = {
        let t = ORIG.load(Ordering::Relaxed);
        if t != 0 {
            let f: DecompStaticFn = std::mem::transmute(t);
            f(arg0, m)
        } else {
            std::ptr::null_mut()
        }
    };
    profile(ret, t0);
    ret
}

unsafe extern "C" fn hook_inst(this: *mut c_void, arg0: *mut c_void, m: *const c_void) -> *mut c_void {
    let t0 = std::time::Instant::now();
    let ret = {
        let t = ORIG.load(Ordering::Relaxed);
        if t != 0 {
            let f: DecompInstFn = std::mem::transmute(t);
            f(this, arg0, m)
        } else {
            std::ptr::null_mut()
        }
    };
    profile(ret, t0);
    ret
}

/// Time the game's decompress (`t0`→now) and Heaven's own scan of the result, then fan out. The
/// diagnostic split lets us tell whether a slow response is the game's decrypt/lz4 or our parsing.
unsafe fn profile(ret: *mut c_void, t0: std::time::Instant) {
    let decomp_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let len = if ret.is_null() { 0 } else { h::array_len(ret as *mut h::RawObject) };
    crate::loadprof::decompress(len, decomp_ms);
    let p0 = std::time::Instant::now();
    on_response(ret);
    crate::loadprof::parse(p0.elapsed().as_secs_f64() * 1000.0, &format!("{}KB", len / 1024));
}

/// Install the DecompressResponse hook. Idempotent. Called once at boot.
pub fn install() {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        if !h::init() {
            log("[response] il2cpp init failed");
            return;
        }
        let image = h::find_game_image();
        if image.is_null() {
            log("[response] game image not found");
            return;
        }
        let ns = std::ffi::CString::new("Gallop").unwrap();
        let cn = std::ffi::CString::new("HttpHelper").unwrap();
        let klass = match h::CLASS_FROM_NAME {
            Some(f) => f(image, ns.as_ptr(), cn.as_ptr()),
            None => std::ptr::null_mut(),
        };
        if klass.is_null() {
            log("[response] Gallop.HttpHelper not found");
            return;
        }
        let mname = std::ffi::CString::new("DecompressResponse").unwrap();
        let method = match h::CLASS_GET_METHOD_FROM_NAME {
            Some(f) => f(klass, mname.as_ptr(), 1),
            None => std::ptr::null_mut(),
        };
        if method.is_null() {
            log("[response] DecompressResponse(1) not found");
            return;
        }
        let is_static = match h::METHOD_GET_FLAGS {
            Some(f) => (f(method, std::ptr::null_mut()) & h::METHOD_ATTRIBUTE_STATIC) != 0,
            None => true,
        };
        let fnptr = h::method_addr(method);
        if fnptr == 0 {
            log("[response] method pointer null");
            return;
        }
        // If another mod (e.g. a spark collector) detoured DecompressResponse first, CHAIN on top
        // instead of yielding. Both hooks are read-only — each calls the original, reads the
        // decompressed result, and returns it UNCHANGED — so they coexist: the response passes
        // through both. retour relocates the existing jmp prologue into our trampoline.
        let chained = crate::il2cpp::is_detoured(fnptr as *const c_void);
        let det = if is_static { hook_static as *const () } else { hook_inst as *const () };
        match RawDetour::new(fnptr as *const (), det) {
            Ok(d) => {
                if d.enable().is_err() {
                    log("[response] detour enable failed");
                    return;
                }
                ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = DETOUR.set(d);
                if chained {
                    log("[response] already detoured (another mod) — chaining on top");
                }
                log(&format!("[response] hooked Gallop.HttpHelper::DecompressResponse (static={is_static})"));
            }
            Err(e) => log(&format!("[response] detour failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gate_captures;

    /// Place `needle` inside a buffer of `size` bytes (all else zero).
    fn buf_with(size: usize, needle: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; size];
        let at = size / 2;
        v[at..at + needle.len()].copy_from_slice(needle);
        v
    }

    // THE REGRESSION: a first-launch download blob that merely CONTAINS a marker byte-string
    // must not trigger capture. This is what stalled the network thread and timed out the
    // game's download ("Download Error").
    #[test]
    fn large_response_with_pstate_key_is_ignored() {
        let b = buf_with(5 * 1024 * 1024, b"rp_info");
        assert_eq!(gate_captures(&b, b.len(), true), (false, false));
    }

    #[test]
    fn large_response_with_rentals_key_is_ignored() {
        let b = buf_with(8 * 1024 * 1024, b"succession_trained_chara_data");
        assert_eq!(gate_captures(&b, b.len(), true), (false, false));
    }

    // Real payloads (a few hundred KB) still capture.
    #[test]
    fn small_response_with_pstate_key_is_captured() {
        let b = buf_with(300 * 1024, b"total_score_info");
        assert_eq!(gate_captures(&b, b.len(), true), (false, true));
    }

    #[test]
    fn small_response_with_rentals_key_is_captured() {
        let b = buf_with(500 * 1024, b"succession_trained_chara_data");
        assert_eq!(gate_captures(&b, b.len(), true), (true, false));
    }

    // The player_state toggle still gates its half; rentals is independent of it.
    #[test]
    fn pstate_respects_enabled_flag() {
        let b = buf_with(1000, b"rp_info");
        assert_eq!(gate_captures(&b, b.len(), false), (false, false));
    }

    // A response with no marker at all — cheap ignore, both false.
    #[test]
    fn unrelated_response_matches_nothing() {
        let b = buf_with(2 * 1024 * 1024, b"race_horse_data");
        assert_eq!(gate_captures(&b, b.len(), true), (false, false));
    }

    // Exactly at the cap is treated as "large" (>= cap → ignored).
    #[test]
    fn boundary_at_cap_is_ignored() {
        let b = buf_with(super::CAPTURE_SIZE_CAP, b"rp_info");
        assert_eq!(gate_captures(&b, b.len(), true), (false, false));
    }
}
