//! Race summary — a post-race window with the finish order, every runner's stats, and the skills
//! that fired, built from the game's own race simulation. The stats-and-skills slice of what
//! Hakuraku shows for an uploaded race, in-game, the moment the result panel comes up.
//!
//! WHERE THE DATA COMES FROM. When a race's simulation lands on the live `RaceInfo` (the 3D path
//! and the skipped path both do this), `race_export` walks the whole managed object graph into a
//! `serde_json::Value` by reflection. That graph already contains everything this needs:
//!
//!   `<RaceHorse>k__BackingField[]`          name, trainer, finish order/time, popularity, and the
//!                                           response packet's stats/style under `_responseHorseData`
//!   `<SimData>k__BackingField`
//!     `_horseResultDataArray[]`             start delay, last-spurt distance per runner
//!     `_frameDataList._items[]`             per-frame `Time` + `HorseDataArray[i].Distance`
//!     `_simEvDataList._items[]`             events; `type == "Skill"` with
//!                                           `param[0]` = runner, `param[1]` = skill id,
//!                                           `param[4]` = bitmask of runners it targeted
//!   `<RaceCourseSet>`, `<RaceTrack>`, `<GroundCondition>`, `<Weather>`, `<Season>`, `<RaceType>`
//!
//! Field names verified 2026-09-13 against a saved export; the event semantics match Hakuraku's
//! reader (`RaceDataUtils.filterCharaSkills` / `filterCharaTargetedSkills`), which is MIT.
//!
//! WHEN IT SHOWS. The simulation - result included - is attached at race START on the 3D path.
//! Showing it then would spoil the race, so a built summary waits as PENDING and is revealed when
//! the race-result panel's own buttons go live (the per-button tick sees them, on both paths).
//!
//! Everything here is plain data: the parse runs on the exporter's worker thread, the overlay
//! reads a clone of a small struct. No IL2CPP is touched.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value;

fn log(msg: &str) {
    crate::tools::log(&format!("[race-summary] {msg}"));
}

// ── the summary ─────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct SkillHit {
    pub skill_id: i32,
    pub name: String,
    pub time: f32,
    /// Runner's distance along the course when it fired (interpolated from the frame list).
    pub distance: f32,
    /// Other runners this skill targeted (a debuff), by horse index. Empty for self-only skills.
    pub targets: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Runner {
    pub horse_index: usize,
    pub name: String,
    pub trainer: String,
    pub chara_id: i32,
    pub card_id: i32,
    /// 1-based place.
    pub place: i32,
    pub finish_time: f32,
    /// Seconds behind the runner one place ahead (0 for the winner).
    pub gap_prev: f32,
    pub popularity: i32,
    /// 1 front, 2 pace, 3 late, 4 end.
    pub style: i32,
    pub speed: i32,
    pub stamina: i32,
    pub power: i32,
    pub guts: i32,
    pub wit: i32,
    pub motivation: i32,
    pub start_delay: f32,
    pub last_spurt_distance: f32,
    pub is_player: bool,
    pub skills: Vec<SkillHit>,
    /// Debuffs this runner was hit by: (caster horse index, skill).
    pub hit_by: Vec<(usize, SkillHit)>,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub race_type: String,
    pub track: String,
    pub distance_m: i32,
    pub ground: String,
    pub condition: String,
    pub weather: String,
    pub season: String,
    pub runners: Vec<Runner>, // in finish order
    pub player_index: Option<usize>,
    pub built_at_ms: u64,
}

static CURRENT: Mutex<Option<Summary>> = Mutex::new(None);
static PENDING: AtomicBool = AtomicBool::new(false);
static WINDOW_OPEN: AtomicBool = AtomicBool::new(false);
static REVEALED_AT: AtomicU64 = AtomicU64::new(0);
static LAST_LOG_PATH: Mutex<Option<usize>> = Mutex::new(None);

fn now_ms() -> u64 {
    crate::tools::clock().elapsed().as_millis() as u64
}

pub fn enabled() -> bool {
    crate::settings::race_summary()
}

pub fn window_open() -> bool {
    WINDOW_OPEN.load(Ordering::Relaxed) && CURRENT.lock().map(|g| g.is_some()).unwrap_or(false)
}

pub fn set_window_open(open: bool) {
    WINDOW_OPEN.store(open, Ordering::Relaxed);
}

/// The summary the overlay draws (a clone; the struct is a few kilobytes).
pub fn current() -> Option<Summary> {
    CURRENT.lock().ok().and_then(|g| g.clone())
}

pub fn pending() -> bool {
    PENDING.load(Ordering::Relaxed)
}

// ── reveal ──────────────────────────────────────────────────────────────────────────────────────

/// Leaf names of the race-result panel's advance button, across race modes. Cheap prefilter so
/// the path walk below runs for a handful of buttons, not every one on screen.
const RESULT_BUTTONS: &[&str] = &["ButtonM00", "NextButton", "ButtonNext", "Next", "RaceResultNextButton"];

/// Called from the ButtonCommon.Update detour for every live button. One atomic load when
/// nothing is pending. When the race-result list is on screen, reveal the pending summary.
pub fn on_button_update(this: *mut std::ffi::c_void) {
    if !PENDING.load(Ordering::Relaxed) || this.is_null() {
        return;
    }
    let name = crate::ui_input::button_name(this);
    if !RESULT_BUTTONS.iter().any(|n| *n == name) {
        return;
    }
    // Each distinct button's path is computed once: the same pointer ticks every frame.
    let key = this as usize;
    if LAST_LOG_PATH.lock().map(|g| *g == Some(key)).unwrap_or(false) {
        return;
    }
    if let Ok(mut g) = LAST_LOG_PATH.lock() {
        *g = Some(key);
    }
    let path = unsafe { crate::roomwatch::bridge::object_path(this) };
    if path.contains("RaceResultList") {
        reveal();
    }
}

fn reveal() {
    if !PENDING.swap(false, Ordering::Relaxed) {
        return;
    }
    REVEALED_AT.store(now_ms(), Ordering::Relaxed);
    WINDOW_OPEN.store(true, Ordering::Relaxed);
    log("result panel is up - showing the summary");
}

/// A new race started (sim imported): anything still pending from the last one is stale.
pub fn on_new_race() {
    PENDING.store(false, Ordering::Relaxed);
    if let Ok(mut g) = LAST_LOG_PATH.lock() {
        *g = None;
    }
}

// ── build ───────────────────────────────────────────────────────────────────────────────────────

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn i(v: &Value, k: &str) -> i32 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0) as i32
}
fn f(v: &Value, k: &str) -> f32 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32
}

/// Build a summary from the walked `RaceInfo` graph and park it as pending. Worker thread.
pub fn ingest(ri: &Value) {
    if !enabled() {
        return;
    }
    let Some(summary) = build(ri) else {
        log("could not build a summary from this RaceInfo (no runners)");
        return;
    };
    let n = summary.runners.len();
    let skills: usize = summary.runners.iter().map(|r| r.skills.len()).sum();
    if let Ok(mut g) = CURRENT.lock() {
        *g = Some(summary);
    }
    WINDOW_OPEN.store(false, Ordering::Relaxed);
    PENDING.store(true, Ordering::Relaxed);
    if let Ok(mut g) = LAST_LOG_PATH.lock() {
        *g = None;
    }
    log(&format!("built: {n} runners, {skills} skill activations - pending the result panel"));
}

fn build(ri: &Value) -> Option<Summary> {
    let horses = ri.get("<RaceHorse>k__BackingField")?.as_array()?;
    if horses.is_empty() {
        return None;
    }
    let sim = ri.get("<SimData>k__BackingField");
    let results = sim.and_then(|s| s.get("_horseResultDataArray")).and_then(|a| a.as_array());
    let frames: Vec<&Value> = sim
        .and_then(|s| s.get("_frameDataList"))
        .and_then(|l| l.get("_items"))
        .and_then(|a| a.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let events: Vec<&Value> = sim
        .and_then(|s| s.get("_simEvDataList"))
        .and_then(|l| l.get("_items"))
        .and_then(|a| a.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let player = ri.get("_playerHorseIndex").and_then(|x| x.as_i64()).filter(|&p| p >= 0).map(|p| p as usize);

    // Runners, keyed by horse index so events can be attached.
    let mut runners: Vec<Runner> = Vec::with_capacity(horses.len());
    for h in horses {
        let r = h.get("_responseHorseData").cloned().unwrap_or(Value::Null);
        let idx = i(h, "horseIndex").max(0) as usize;
        let res = results.and_then(|a| a.get(idx));
        runners.push(Runner {
            horse_index: idx,
            name: s(h, "<charaName>k__BackingField"),
            trainer: s(h, "<TrainerName>k__BackingField"),
            chara_id: i(h, "charaId"),
            card_id: i(&r, "card_id"),
            place: i(h, "FinishOrder") + 1,
            finish_time: f(h, "FinishTimeRaw"),
            gap_prev: f(h, "FinishDiffTimeFromPrev"),
            popularity: i(&r, "popularity"),
            style: i(&r, "running_style"),
            speed: i(&r, "speed"),
            stamina: i(&r, "stamina"),
            power: i(&r, "pow"),
            guts: i(&r, "guts"),
            wit: i(&r, "wiz"),
            motivation: i(&r, "motivation"),
            start_delay: res.map(|x| f(x, "StartDelayTime")).unwrap_or(0.0),
            last_spurt_distance: res.map(|x| f(x, "LastSpurtStartDistance")).unwrap_or(0.0),
            is_player: Some(idx) == player,
            skills: Vec::new(),
            hit_by: Vec::new(),
        });
    }

    // Distance of runner `idx` at time `t`, linearly interpolated across the sampled frames.
    let distance_at = |idx: usize, t: f32| -> f32 {
        if frames.is_empty() {
            return 0.0;
        }
        let dist = |fr: &Value| fr.get("HorseDataArray").and_then(|a| a.get(idx)).map(|h| f(h, "Distance")).unwrap_or(0.0);
        let mut prev = frames[0];
        for fr in &frames {
            let ft = f(fr, "Time");
            if ft >= t {
                let pt = f(prev, "Time");
                let (d0, d1) = (dist(prev), dist(fr));
                if ft <= pt {
                    return d1;
                }
                let k = ((t - pt) / (ft - pt)).clamp(0.0, 1.0);
                return d0 + (d1 - d0) * k;
            }
            prev = fr;
        }
        dist(frames[frames.len() - 1])
    };

    // Skill events: param[0] runner, param[1] skill id, param[4] target mask.
    let mut hits: Vec<(usize, SkillHit)> = Vec::new();
    for ev in &events {
        if s(ev, "type") != "Skill" {
            continue;
        }
        let Some(p) = ev.get("param").and_then(|x| x.as_array()) else { continue };
        let caster = p.first().and_then(|x| x.as_i64()).unwrap_or(-1);
        let skill_id = p.get(1).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
        if caster < 0 || skill_id <= 0 {
            continue;
        }
        let caster = caster as usize;
        let mask = p.get(4).and_then(|x| x.as_i64()).unwrap_or(0);
        let t = f(ev, "frameTime");
        let targets: Vec<usize> = (0..runners.len()).filter(|&j| j != caster && (mask >> j) & 1 == 1).collect();
        let name = crate::skill_advisor::skill_name(skill_id);
        hits.push((
            caster,
            SkillHit { skill_id, name: if name.is_empty() { format!("skill {skill_id}") } else { name }, time: t, distance: distance_at(caster, t), targets },
        ));
    }
    for (caster, hit) in hits {
        for &tgt in &hit.targets {
            if let Some(r) = runners.iter_mut().find(|r| r.horse_index == tgt) {
                r.hit_by.push((caster, hit.clone()));
            }
        }
        if let Some(r) = runners.iter_mut().find(|r| r.horse_index == caster) {
            r.skills.push(hit);
        }
    }
    for r in &mut runners {
        r.skills.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
    }
    runners.sort_by_key(|r| r.place);

    let course = ri.get("<RaceCourseSet>k__BackingField").cloned().unwrap_or(Value::Null);
    let track_id = ri.get("<RaceTrack>k__BackingField").map(|t| i(t, "Id")).unwrap_or(0);
    Some(Summary {
        race_type: s(ri, "<RaceType>k__BackingField"),
        track: crate::race::track_name(track_id),
        distance_m: i(&course, "Distance"),
        ground: match i(&course, "Ground") {
            1 => "Turf".into(),
            2 => "Dirt".into(),
            _ => String::new(),
        },
        condition: s(ri, "<GroundCondition>k__BackingField"),
        weather: s(ri, "<Weather>k__BackingField"),
        season: s(ri, "<Season>k__BackingField"),
        runners,
        player_index: player,
        built_at_ms: now_ms(),
    })
}

/// Short label for a running style code.
pub fn style_label(style: i32) -> &'static str {
    match style {
        1 => "Front",
        2 => "Pace",
        3 => "Late",
        4 => "End",
        _ => "",
    }
}

/// "1:46.17" from seconds.
pub fn fmt_time(t: f32) -> String {
    if t <= 0.0 {
        return "-".into();
    }
    let m = (t / 60.0).floor() as i32;
    let sec = t - m as f32 * 60.0;
    format!("{m}:{sec:05.2}")
}

/// Preview-host design aid: a posed race so the window can be styled without the game.
pub fn mock_for_preview() {
    if std::env::var_os("TRACKSIDE_RSUM_MOCK").is_none() {
        return;
    }
    let mk = |idx: usize, name: &str, place: i32, t: f32, gap: f32, pop: i32, style: i32, st: [i32; 5], skills: &[(i32, &str, f32, f32)], player: bool| Runner {
        horse_index: idx,
        name: name.into(),
        trainer: "Trainer".into(),
        chara_id: 1000 + idx as i32,
        card_id: 0,
        place,
        finish_time: t,
        gap_prev: gap,
        popularity: pop,
        style,
        speed: st[0],
        stamina: st[1],
        power: st[2],
        guts: st[3],
        wit: st[4],
        motivation: 4,
        start_delay: 0.04,
        last_spurt_distance: 1468.0,
        is_player: player,
        skills: skills
            .iter()
            .map(|&(id, n, tm, d)| SkillHit { skill_id: id, name: n.into(), time: tm, distance: d, targets: Vec::new() })
            .collect(),
        hit_by: Vec::new(),
    };
    let runners = vec![
        mk(7, "Oguri Cap", 1, 105.39, 0.0, 1, 2, [1610, 1055, 1188, 640, 1183], &[(201591, "Concentration", 0.0, 0.0), (200152, "Corner Recovery", 21.4, 480.0), (100061, "Triumphant Pulse", 84.1, 1690.0)], true),
        mk(4, "Symboli Rudolf", 2, 105.46, 0.07, 3, 2, [1502, 1120, 1077, 700, 1050], &[(200142, "Straightaway Recovery", 32.0, 700.0)], false),
        mk(5, "Special Week", 3, 105.74, 0.28, 2, 3, [1571, 963, 1190, 585, 1022], &[(202051, "Escape Artist", 0.0, 0.0), (200062, "Focus", 55.5, 1210.0)], false),
        mk(1, "Gold Ship", 4, 106.12, 0.38, 6, 4, [1538, 642, 1210, 580, 839], &[], false),
        mk(0, "Eishin Flash", 5, 106.17, 0.05, 1, 3, [1569, 874, 1118, 738, 874], &[(200192, "Lane Change", 44.0, 960.0)], false),
        mk(2, "Grass Wonder", 6, 107.68, 1.51, 9, 4, [1245, 728, 853, 515, 1285], &[], false),
    ];
    if let Ok(mut g) = CURRENT.lock() {
        *g = Some(Summary {
            race_type: "RoomMatch".into(),
            track: "Kyoto".into(),
            distance_m: 2200,
            ground: "Turf".into(),
            condition: "Good".into(),
            weather: "Sunny".into(),
            season: "Fall".into(),
            runners,
            player_index: Some(7),
            built_at_ms: 0,
        });
    }
    WINDOW_OPEN.store(true, Ordering::Relaxed);
}
