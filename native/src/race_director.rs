//! Race Director — live race-telemetry data provider.
//!
//! This owns everything that READS live race state each frame for the broadcast HUD:
//! the HorseRaceInfo field offsets (HP / order / speed / distance / phase / flags), the
//! per-horse telemetry buffer + name / trainer / finish / prev-position maps, the skill
//! activation feed, the active-skill countdown, the followed-Uma AI state, the last-spurt
//! outlook, the pace trace and the win-probability model.
//!
//! It is NOT the camera. The only thing it shares with the free camera is the identity of
//! the FOLLOWED Uma — which the camera owns. `freecam` exposes `followed_gate()` /
//! `gate_of()` / `in_race()`; the race-camera hooks (`on_run_motion` / `on_hri_ctor`, which
//! also drive the camera) publish telemetry into here via the `publish_*` / `on_ctor` /
//! `update_followed` entry points.
#![allow(dead_code)]

use crate::htt_il2cpp as h;
use crate::il2cpp;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

// ── shared clock (global) ─────────────────────────────────────────────────────
fn clock() -> &'static std::time::Instant {
    crate::tools::clock()
}

// ── race heartbeat (telemetry side) ───────────────────────────────────────────
static LAST_TELEM_MS: AtomicU64 = AtomicU64::new(0);
static RACE_EPOCH: AtomicU64 = AtomicU64::new(0);
fn telem_fresh() -> bool {
    (clock().elapsed().as_millis() as u64).saturating_sub(LAST_TELEM_MS.load(Ordering::Relaxed)) < 500
}
/// Bumps once per race start. The tower uses it to reset per-race UI animation state.
pub fn race_epoch() -> u64 {
    RACE_EPOCH.load(Ordering::Relaxed)
}
/// Telemetry HUD is active: the toggle is on AND either telemetry is fresh or we're still
/// in the race scene (the manager keeps ticking through the static result screen).
pub fn race_active() -> bool {
    crate::settings::telemetry() && (telem_fresh() || crate::freecam::in_race())
}

// ── live telemetry — HorseRaceInfo field offsets ──────────────────────────────
static HP_OFF: AtomicUsize = AtomicUsize::new(0);
static MAXHP_OFF: AtomicUsize = AtomicUsize::new(0);
static ORDER_OFF: AtomicUsize = AtomicUsize::new(0);
static SPEED_OFF: AtomicUsize = AtomicUsize::new(0);
static DIST_OFF: AtomicUsize = AtomicUsize::new(0);
static HPEMPTY_OFF: AtomicUsize = AtomicUsize::new(0); // <IsHpEmptyOnRace>: bool, exhausted
static PHASE_OFF: AtomicUsize = AtomicUsize::new(0); // _phase: i32 race phase (>=2 = last spurt)
// Live race-state flags (all pure bool/i32/float fields on HorseRaceInfo — cheap direct reads).
static BADSTART_OFF: AtomicUsize = AtomicUsize::new(0); // <IsBadStart>: bool, late start
static COMPFIGHT_OFF: AtomicUsize = AtomicUsize::new(0); // <IsCompeteFight>: bool, head-to-head fight
static COMPTOP_OFF: AtomicUsize = AtomicUsize::new(0); // <IsCompeteTop>: bool, leading battle
static BLOCKFRONT_OFF: AtomicUsize = AtomicUsize::new(0); // <BlockFrontContinueTime>: f32, >0 = boxed in
static PREVORDER_OFF: AtomicUsize = AtomicUsize::new(0); // <PrevOrder>: i32, for the order-trend arrow
static DEFEAT_OFF: AtomicUsize = AtomicUsize::new(0); // _defeat: i32 DefeatType (why she can't win)
// Static-per-race identity (pointer-chase off `this`): HorseRaceInfo._horseData → HorseData,
// HorseData.<Popularity> + HorseData._responseHorseData (RaceHorseData) → .running_style.
static HDATA_OFF: AtomicUsize = AtomicUsize::new(0); // HorseRaceInfo._horseData (ptr)
static POP_OFF: AtomicUsize = AtomicUsize::new(0); // HorseData.<Popularity>: i32 (人気 rank)
static RESP_OFF: AtomicUsize = AtomicUsize::new(0); // HorseData._responseHorseData (ptr)
static RSTYLE_OFF: AtomicUsize = AtomicUsize::new(0); // RaceHorseData.running_style: i32 (1..4)
static TNAME_OFF: AtomicUsize = AtomicUsize::new(0); // RaceHorseData.trainer_name (managed String ptr)
static VIEWER_OFF: AtomicUsize = AtomicUsize::new(0); // RaceHorseData.viewer_id: i64 (0 = NPC)
// Skill activation FEED (followed Uma only). Read SkillManager._usedSkillIdList (per-uma
// list of activated skill ids), detect new entries, resolve names via MasterDataUtil.
// GetSkillName. All reads are pure; GetSkillName is called only when a NEW skill appears
// (rare event, main thread) — never per-frame-per-horse (that froze the horses).
static SKILLMGR_OFF: AtomicUsize = AtomicUsize::new(0); // HorseRaceInfo._skillManager
static USEDLIST_OFF: AtomicUsize = AtomicUsize::new(0); // SkillManager._usedSkillIdList
static GSN_FN: AtomicUsize = AtomicUsize::new(0); // MasterDataUtil.GetSkillName(id)
static GSN_MI: AtomicUsize = AtomicUsize::new(0);
static GSN_STATIC: AtomicBool = AtomicBool::new(true);
// Skill effect values — in-memory master data (public-safe, no the game data):
// WorkTrainingChallengeData.get_MasterManager() → MasterDataManager.<masterSkillData>@0xc8 →
// MasterSkillData.Get(id) → SkillData (AbilityType11@0x6c, FloatAbilityValue11@0x78 ÷1e4, FloatAbilityTime1@0x60 ÷1e4).
static MM_GET: AtomicUsize = AtomicUsize::new(0);
static MM_GET_MI: AtomicUsize = AtomicUsize::new(0);
static MSD_GET: AtomicUsize = AtomicUsize::new(0);
static MSD_GET_MI: AtomicUsize = AtomicUsize::new(0);
static MSD_INST: AtomicUsize = AtomicUsize::new(0); // cached MasterSkillData* (master data is stable)
static FEED_SEEN: AtomicUsize = AtomicUsize::new(0); // ids already added for the followed Uma
static SKILL_FEED: OnceLock<Mutex<Vec<(i32, String)>>> = OnceLock::new();
fn skill_feed_buf() -> &'static Mutex<Vec<(i32, String)>> {
    SKILL_FEED.get_or_init(|| Mutex::new(Vec::new()))
}
/// Activated skills (id, name) for the currently-followed Uma, in activation order. The id
/// lets the overlay look up the skill's icon.
pub fn skill_feed() -> Vec<(i32, String)> {
    skill_feed_buf().lock().map(|f| f.clone()).unwrap_or_default()
}

fn reset_skill_feed() {
    FEED_SEEN.store(0, Ordering::Relaxed);
    if let Ok(mut f) = skill_feed_buf().lock() {
        f.clear();
    }
}

// ── live race-outlook reads for the followed Uma (followed Uma ONLY, once/frame) ──
static SPURT_OUTLOOK: AtomicI32 = AtomicI32::new(0); // LastSpurtCalcResult bitflags (1/2 hold, 4/8 fade)
static AI_OFF: AtomicUsize = AtomicUsize::new(0); // HorseRaceInfo._horseRaceAI (ptr)
static AI_SPURT_GET: AtomicUsize = AtomicUsize::new(0); // HorseRaceAIReplay.get_LastSpurtCalcResult (REAL impl)
// Live race-state getters on the AI (HorseRaceAIBase real cluster, same safe profile as spurt).
static KAKARI_GET: AtomicUsize = AtomicUsize::new(0); // get_IsTemptation (bool)
static TEMPTMODE_GET: AtomicUsize = AtomicUsize::new(0); // get_TemptationMode (enum)
static KEEPMODE_GET: AtomicUsize = AtomicUsize::new(0); // get_PositionKeepMode (enum)
static DOWNHILL_GET: AtomicUsize = AtomicUsize::new(0); // get_IsDownSlopeAccelMode (bool)
// Active-skill countdown via a pure FIELD walk (no GetCurrentActiveSkill stub — that crashed):
// SkillManager._skills (List<SkillBase>) → SkillBase.Details (List<SkillDetail>) → detail fields.
static SKILLS_LIST_OFF: AtomicUsize = AtomicUsize::new(0); // SkillManager._skills
static SB_MASTER_OFF: AtomicUsize = AtomicUsize::new(0); // SkillBase.<SkillMaster> → SkillData
static SB_DETAILS_OFF: AtomicUsize = AtomicUsize::new(0); // SkillBase.<Details> (List<SkillDetail>)
static SD_LEFT_OFF: AtomicUsize = AtomicUsize::new(0); // SkillDetail.<LeftTime>
static SD_CAT_OFF: AtomicUsize = AtomicUsize::new(0); // SkillDetail.<Category>
static SD_DEBUFF_OFF: AtomicUsize = AtomicUsize::new(0); // SkillDetail._isDebuff
static SD_ACT_OFF: AtomicUsize = AtomicUsize::new(0); // SkillDetail.<IsActivated>

/// One currently-active skill effect on the followed Uma (live countdown).
#[derive(Clone, Copy)]
pub struct ActiveSkill {
    pub id: i32,
    pub left: f32,      // seconds of effect remaining
    pub category: i32,  // SkillCategory: 0 Speed, 1 Heal, 2 Accel, -1 none
    pub debuff: bool,
}
static ACTIVE_SKILLS: OnceLock<Mutex<Vec<ActiveSkill>>> = OnceLock::new();
fn active_skills_buf() -> &'static Mutex<Vec<ActiveSkill>> {
    ACTIVE_SKILLS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Live AI-driven race state for the followed Uma (kakari / position-keep / down-slope).
#[derive(Clone, Copy, Default)]
pub struct FollowState {
    pub kakari: bool,        // get_IsTemptation — over-eager, burning stamina
    pub temptation_mode: i32, // TemptationMode (1 Sashi, 2 Senko, 3 Nige, 4 Boost)
    pub keep_mode: i32,      // PositionKeepMode (1 SpeedUp, 2 Overtake, 3 PaseUp, 4 PaseDown)
    pub downhill: bool,      // get_IsDownSlopeAccelMode — free downhill acceleration
}
static FOLLOW_STATE: OnceLock<Mutex<FollowState>> = OnceLock::new();
fn follow_state_buf() -> &'static Mutex<FollowState> {
    FOLLOW_STATE.get_or_init(|| Mutex::new(FollowState::default()))
}
/// Live race state of the followed Uma (kakari, position-keep mode, down-slope accel).
pub fn follow_state() -> FollowState {
    follow_state_buf().lock().map(|s| *s).unwrap_or_default()
}

/// Followed-Uma only: read the AI's live race-state getters (kakari / position-keep / down-slope).
/// All on HorseRaceAIBase's real method cluster (unique RVAs — the safe kind, like spurt).
unsafe fn update_follow_state(hri: *mut c_void) {
    let ai_off = AI_OFF.load(Ordering::Relaxed);
    if ai_off == 0 {
        return;
    }
    let ai = ((hri as usize + ai_off) as *const *mut c_void).read_unaligned();
    if ai.is_null() {
        return;
    }
    let call_b = |p: usize| -> bool {
        if p == 0 {
            return false;
        }
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool = std::mem::transmute(p);
        f(ai, std::ptr::null_mut())
    };
    let call_i = |p: usize| -> i32 {
        if p == 0 {
            return 0;
        }
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 = std::mem::transmute(p);
        f(ai, std::ptr::null_mut())
    };
    let st = FollowState {
        kakari: call_b(KAKARI_GET.load(Ordering::Relaxed)),
        temptation_mode: call_i(TEMPTMODE_GET.load(Ordering::Relaxed)),
        keep_mode: call_i(KEEPMODE_GET.load(Ordering::Relaxed)),
        downhill: call_b(DOWNHILL_GET.load(Ordering::Relaxed)),
    };
    if let Ok(mut b) = follow_state_buf().lock() {
        *b = st;
    }
}
/// Skills whose effect is active RIGHT NOW on the followed Uma (with seconds remaining).
pub fn active_skills() -> Vec<ActiveSkill> {
    active_skills_buf().lock().map(|b| b.clone()).unwrap_or_default()
}
/// Last-spurt sustainability for the followed Uma: `&3 != 0` = will hold the spurt,
/// `&12 != 0` = will run out of stamina before the line. 0 = not yet computed.
pub fn spurt_outlook() -> i32 {
    SPURT_OUTLOOK.load(Ordering::Relaxed)
}
fn reset_outlook() {
    SPURT_OUTLOOK.store(0, Ordering::Relaxed);
    if let Ok(mut b) = active_skills_buf().lock() {
        b.clear();
    }
    if let Ok(mut s) = follow_state_buf().lock() {
        *s = FollowState::default();
    }
}

// IL2CPP List<T>: _items (T[]) @0x10, _size @0x18; ref-type array data starts at +0x20 (8B refs).
#[inline]
unsafe fn list_ptr_at(list: *mut c_void, i: i32) -> *mut c_void {
    let items = ((list as usize + 0x10) as *const *mut c_void).read_unaligned();
    if items.is_null() {
        return std::ptr::null_mut();
    }
    ((items as usize + 0x20 + i as usize * 8) as *const *mut c_void).read_unaligned()
}
#[inline]
unsafe fn list_size(list: *mut c_void) -> i32 {
    ((list as usize + 0x18) as *const i32).read_unaligned()
}

/// Followed-Uma only: the currently-active skill effects + remaining time, by a PURE FIELD WALK
/// (no `GetCurrentActiveSkill` — that's a crashing stub on this build). Walks SkillManager._skills
/// → each SkillBase's Details → any SkillDetail with IsActivated & LeftTime>0.
unsafe fn update_active_skills(hri: *mut c_void) {
    let mgr_off = SKILLMGR_OFF.load(Ordering::Relaxed);
    let list_off = SKILLS_LIST_OFF.load(Ordering::Relaxed);
    let det_off = SB_DETAILS_OFF.load(Ordering::Relaxed);
    let act_off = SD_ACT_OFF.load(Ordering::Relaxed);
    let left_off = SD_LEFT_OFF.load(Ordering::Relaxed);
    if mgr_off == 0 || list_off == 0 || det_off == 0 || act_off == 0 || left_off == 0 {
        return;
    }
    let mut out: Vec<ActiveSkill> = Vec::new();
    let mgr = ((hri as usize + mgr_off) as *const *mut c_void).read_unaligned();
    if !mgr.is_null() {
        let skills = ((mgr as usize + list_off) as *const *mut c_void).read_unaligned();
        if !skills.is_null() {
            let master_off = SB_MASTER_OFF.load(Ordering::Relaxed);
            let cat_off = SD_CAT_OFF.load(Ordering::Relaxed);
            let deb_off = SD_DEBUFF_OFF.load(Ordering::Relaxed);
            let n = list_size(skills).clamp(0, 64);
            for i in 0..n {
                let sb = list_ptr_at(skills, i);
                if sb.is_null() {
                    continue;
                }
                let details = ((sb as usize + det_off) as *const *mut c_void).read_unaligned();
                if details.is_null() {
                    continue;
                }
                // most-time-remaining active detail of this skill (≤2 details per skill)
                let (mut best_left, mut best_cat, mut best_deb, mut any) = (0.0f32, -1i32, false, false);
                let dn = list_size(details).clamp(0, 4);
                for j in 0..dn {
                    let d = list_ptr_at(details, j);
                    if d.is_null() {
                        continue;
                    }
                    let activated = ((d as usize + act_off) as *const u8).read_unaligned() != 0;
                    let left = ((d as usize + left_off) as *const f32).read_unaligned();
                    if activated && left > 0.05 && left < 60.0 {
                        any = true;
                        if left > best_left {
                            best_left = left;
                            best_cat = if cat_off != 0 { ((d as usize + cat_off) as *const i32).read_unaligned() } else { -1 };
                            best_deb = deb_off != 0 && ((d as usize + deb_off) as *const u8).read_unaligned() != 0;
                        }
                    }
                }
                if any {
                    // skill id ← SkillBase.SkillMaster → SkillData.Id @0x10
                    let id = if master_off != 0 {
                        let m = ((sb as usize + master_off) as *const *mut c_void).read_unaligned();
                        if m.is_null() { 0 } else { ((m as usize + 0x10) as *const i32).read_unaligned() }
                    } else {
                        0
                    };
                    out.push(ActiveSkill { id, left: best_left, category: best_cat, debuff: best_deb });
                }
            }
        }
    }
    if let Ok(mut b) = active_skills_buf().lock() {
        *b = out;
    }
}

/// Resolve a skill id → localized name via MasterDataUtil.GetSkillName (static getter).
unsafe fn skill_name(id: i32) -> String {
    let p = GSN_FN.load(Ordering::Relaxed);
    if p == 0 || !GSN_STATIC.load(Ordering::Relaxed) {
        return format!("Skill {id}");
    }
    let f: unsafe extern "C" fn(i32, *const c_void) -> *mut c_void = std::mem::transmute(p);
    let obj = f(id, GSN_MI.load(Ordering::Relaxed) as *const c_void);
    let raw = read_managed_str(obj).filter(|s| !s.is_empty()).unwrap_or_else(|| format!("Skill {id}"));
    // Some skill names carry a trailing tier/symbol glyph the bundled Latin font can't render, so
    // imgui draws it as its fallback char ("?"). Strip trailing spaces + any char beyond Latin
    // Extended-A so the name reads clean (e.g. "Competitive Spirit ?" → "Competitive Spirit").
    let cleaned = raw.trim_end_matches(|c: char| c == ' ' || (c as u32) > 0x024F);
    if cleaned.is_empty() { raw } else { cleaned.to_string() }
}

// gate-independent cache: skill_id → short effect string ("+0.35 m/s 3s"). Master data is constant
// for the session, so once resolved it's cached forever (no per-race reset).
static SKILL_EFFECT: OnceLock<Mutex<HashMap<i32, String>>> = OnceLock::new();
fn skill_effect_buf() -> &'static Mutex<HashMap<i32, String>> {
    SKILL_EFFECT.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Cached effect string for a skill id (empty until resolved / if unknown). Read by the overlay.
pub fn skill_effect_of(id: i32) -> String {
    skill_effect_buf().lock().ok().and_then(|m| m.get(&id).cloned()).unwrap_or_default()
}
/// Read a skill's PRIMARY ability (type/value/time) from the game's in-memory master data and format
/// "+value unit · time". ×10000 fixed-point. Game-thread only (managed calls); call once per id.
unsafe fn compute_skill_effect(id: i32) -> String {
    let getmm = MM_GET.load(Ordering::Relaxed);
    let get = MSD_GET.load(Ordering::Relaxed);
    if getmm == 0 || get == 0 {
        return String::new();
    }
    let mut msd = MSD_INST.load(Ordering::Relaxed);
    if msd == 0 {
        let f: unsafe extern "C" fn(*const c_void) -> *mut c_void = std::mem::transmute(getmm);
        let mgr = f(MM_GET_MI.load(Ordering::Relaxed) as *const c_void);
        if mgr.is_null() {
            return String::new();
        }
        msd = ((mgr as usize + 0xc8) as *const usize).read_unaligned(); // <masterSkillData>k__BackingField
        if msd == 0 {
            return String::new();
        }
        MSD_INST.store(msd, Ordering::Relaxed);
    }
    let gf: unsafe extern "C" fn(*mut c_void, i32, *const c_void) -> *mut c_void = std::mem::transmute(get);
    let sd = gf(msd as *mut c_void, id, MSD_GET_MI.load(Ordering::Relaxed) as *const c_void);
    if sd.is_null() {
        return String::new();
    }
    let base = sd as usize;
    let atype = ((base + 0x6c) as *const i32).read_unaligned(); // AbilityType11
    let aval = ((base + 0x78) as *const i32).read_unaligned(); // FloatAbilityValue11 (×1e4)
    let atime = ((base + 0x60) as *const i32).read_unaligned(); // FloatAbilityTime1 (×1e4)
    if atype == 0 || aval == 0 {
        return String::new();
    }
    let v = aval as f32 / 10000.0; // ×1e4 fixed-point
    let t = atime as f32 / 10000.0;
    let dur = if t > 0.4 { format!(" {t:.1}s") } else { String::new() };
    // Ability types confirmed from real skill data: 1-5 = stat boosts (passive), 21/22/27 = target
    // speed (m/s), 31 = acceleration (m/s²), 9 = HP recovery, 10 = start (Focus). Verified in-game.
    match atype {
        1 => format!("+{v:.0} Speed"),
        2 => format!("+{v:.0} Stamina"),
        3 => format!("+{v:.0} Power"),
        4 => format!("+{v:.0} Guts"),
        5 => format!("+{v:.0} Wisdom"),
        21 | 22 | 27 => format!("+{v:.2} m/s{dur}"),
        31 => format!("+{v:.2} m/s2{dur}"),
        9 => format!("+{:.0}% HP", v * 100.0), // recovery as % of max HP
        _ => format!("+{v:.2}{dur}"),           // 10 (start) + any unmapped type: raw value
    }
}

/// For the followed Uma, append any newly-activated skills to the feed (resolving names).
/// Called only for the followed gate; GetSkillName runs only on genuinely new entries.
unsafe fn update_skill_feed(hri: *mut c_void) {
    let mo = SKILLMGR_OFF.load(Ordering::Relaxed);
    let lo = USEDLIST_OFF.load(Ordering::Relaxed);
    if mo == 0 || lo == 0 {
        return;
    }
    let mgr = ((hri as usize + mo) as *const usize).read_unaligned();
    if mgr == 0 {
        return;
    }
    let list = ((mgr + lo) as *const usize).read_unaligned();
    if list == 0 {
        return;
    }
    // List<int>: _items(int[])@0x10, _size@0x18.
    let items = ((list + 0x10) as *const usize).read_unaligned();
    let size = ((list + 0x18) as *const i32).read_unaligned();
    if items == 0 || size <= 0 || size > 64 {
        return;
    }
    let seen = FEED_SEEN.load(Ordering::Relaxed);
    if size as usize <= seen {
        return;
    }
    let base = (items + 0x20) as *const i32;
    if let Ok(mut feed) = skill_feed_buf().lock() {
        for i in seen..size as usize {
            let id = base.add(i).read_unaligned();
            feed.push((id, skill_name(id)));
            // Resolve + cache this skill's effect string once (game thread = safe for the managed call).
            let need = skill_effect_buf().lock().map(|m| !m.contains_key(&id)).unwrap_or(false);
            if need {
                let eff = compute_skill_effect(id);
                if let Ok(mut m) = skill_effect_buf().lock() {
                    m.insert(id, eff);
                }
            }
        }
    }
    FEED_SEEN.store(size as usize, Ordering::Relaxed);
}
// HorseData.<charaName> field offset (string) — for the gate→name map.
static NAME_OFF: AtomicUsize = AtomicUsize::new(0);
// HorseData.charaId field offset — for the portrait icon (gate→id map).
static CHARAID_OFF: AtomicUsize = AtomicUsize::new(0);

/// Per-horse live telemetry, collected each frame in the run-motion hook (raw reads + a
/// couple of cheap instance getters).
#[derive(Clone, Copy, Default)]
pub struct HorseTelem {
    pub gate: i32,
    pub order: i32, // CurOrder (1 = leading)
    pub hp: f32,
    pub max_hp: f32,
    pub speed: f32,    // m/s
    pub distance: f32, // metres covered
    pub spurt: bool,   // in last spurt (final sprint)
    pub exhausted: bool, // HP empty on race (out of stamina)
    pub skills: i32,   // skills activated so far
    pub late_start: bool, // IsBadStart — botched the gate (late start)
    pub fight: bool,    // IsCompeteFight — locked in a head-to-head duel
    pub leading: bool,  // IsCompeteTop — contesting the lead
    pub blocked: bool,  // BlockFrontContinueTime > 0 — boxed in behind another horse
    pub prev_order: i32, // last frame's order — for the position-trend arrow
    pub popularity: i32, // betting favourite rank (人気); 1 = top pick, 0 = unknown
    pub running_style: i32, // 1 Nige, 2 Senko, 3 Sashi, 4 Oikomi (0 = unknown)
    pub defeat: i32,     // DefeatType — why she can't win (0 none, 1 win, else a reason)
    pub wx: f32,         // world position X (for the track-map minimap)
    pub wz: f32,         // world position Z
}

static TELEM: OnceLock<Mutex<HashMap<i32, HorseTelem>>> = OnceLock::new();
fn telem_buf() -> &'static Mutex<HashMap<i32, HorseTelem>> {
    TELEM.get_or_init(|| Mutex::new(HashMap::new()))
}

// gate → Uma display name (from HorseData.charaName, captured in the ctor hook).
static NAMEMAP: OnceLock<Mutex<HashMap<i32, String>>> = OnceLock::new();
fn name_map() -> &'static Mutex<HashMap<i32, String>> {
    NAMEMAP.get_or_init(|| Mutex::new(HashMap::new()))
}

// gate → (trainer_name, viewer_id) from RaceHorseData. Static per race; read once per gate. Empty
// trainer / viewer 0 = an NPC (career races). Real lobbies (Team Trials, Champions, Room Match) carry
// the human trainer name, and umas of the same viewer_id are the same person's team (1-3 umas).
static TRAINERMAP: OnceLock<Mutex<HashMap<i32, (String, i64)>>> = OnceLock::new();
fn trainer_map() -> &'static Mutex<HashMap<i32, (String, i64)>> {
    TRAINERMAP.get_or_init(|| Mutex::new(HashMap::new()))
}

// gate → finish rank (1 = crossed the line first), captured the frame a Uma's distance first reaches
// the course distance. The tower keeps finished Umas in this exact crossing order during the run-out
// (where raw distances diverge by deceleration) so the on-stream order matches the real result.
static FINISHRANK: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
static FINISH_NEXT: AtomicI32 = AtomicI32::new(0);
fn finish_rank() -> &'static Mutex<HashMap<i32, i32>> {
    FINISHRANK.get_or_init(|| Mutex::new(HashMap::new()))
}

// gate → previous tower position (our DISTANCE-based order, not the game's unreliable CurOrder), so
// the position-change flash (green = gained, red = lost) reflects the real order shown. Refreshed on
// a short throttle so the trend stays non-zero long enough for the flash to catch it.
static PREVPOS: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
static LAST_POS_MS: AtomicU64 = AtomicU64::new(0);
fn prev_pos() -> &'static Mutex<HashMap<i32, i32>> {
    PREVPOS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn gate_name(gate: i32) -> String {
    name_map()
        .lock()
        .ok()
        .and_then(|m| m.get(&gate).cloned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Gate {gate}"))
}

/// Decode a managed System.String (len@0x10, UTF-16 chars@0x14). Bounded to sane lengths.
unsafe fn read_managed_str(obj: *mut c_void) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    let len = ((obj as usize + 0x10) as *const i32).read_unaligned();
    if len <= 0 || len > 64 {
        return Some(String::new());
    }
    let chars = (obj as usize + 0x14) as *const u16;
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(chars, len as usize)))
}

/// Computed view for the HUD: the followed Uma + the adjacent rival (the one ahead, or the
/// one behind if the followed Uma is leading) + the gap between them.
#[derive(Clone)]
pub struct TelemView {
    pub followed: HorseTelem,
    pub followed_name: String,
    pub rival: Option<HorseTelem>,
    pub rival_name: String,
    pub rival_ahead: bool, // true = rival is ahead; false = rival behind (we're leading)
    pub gap: f32,          // metres between followed and rival
    pub field_size: i32,
    pub chara_id: i32,     // followed Uma's character id (for the portrait icon); 0 if unknown
}

/// Telemetry for the currently-followed Uma + its adjacent rival. None until a race is live.
pub fn telemetry() -> Option<TelemView> {
    let (followed, field_size, rival) = {
        let buf = telem_buf().lock().ok()?;
        if buf.is_empty() {
            return None;
        }
        let target = crate::freecam::followed_gate();
        let followed = *buf.get(&target)?;
        let field_size = buf.len() as i32;
        // Rival = the one directly ahead (order-1); if leading (order 1), the one behind.
        let want_order = if followed.order > 1 { followed.order - 1 } else { followed.order + 1 };
        let rival = buf.values().find(|h| h.order == want_order).copied();
        (followed, field_size, rival)
    };
    let ahead = followed.order > 1;
    let gap = rival.map(|r| (r.distance - followed.distance).abs()).unwrap_or(0.0);
    let followed_name = gate_name(followed.gate);
    let rival_name = rival.map(|r| gate_name(r.gate)).unwrap_or_default();
    let chara_id = id_map().lock().ok().and_then(|m| m.get(&followed.gate).copied()).unwrap_or(0);
    Some(TelemView { followed, followed_name, rival, rival_name, rival_ahead: ahead, gap, field_size, chara_id })
}

/// One row of the broadcast timing tower — the whole field, leader-first.
#[derive(Clone)]
pub struct FieldRow {
    pub pos: i32,        // display position (1 = leader)
    pub gate: i32,
    pub name: String,
    pub style: i32,      // 1 Nige .. 4 Oikomi (0 = unknown)
    pub sta: f32,        // 0..1 stamina
    pub interval: f32,   // metres behind the horse directly ahead (0 for the leader)
    pub gap_leader: f32, // metres behind the leader
    pub trend: i32,      // places gained since last frame (prev_order - order)
    pub popularity: i32, // 人気 rank
    pub spurt: bool,
    pub exhausted: bool,
    pub fight: bool,
    pub blocked: bool,
    pub followed: bool,  // this is the camera's current target
    pub win: f32,        // live win probability 0..1 (physics ETA model)
    pub dist: f32,       // metres covered (for the phase/progress header)
    pub speed: f32,      // current m/s (for time-gap intervals)
    pub trainer: String, // human trainer name (lobby races); empty for NPCs
    pub viewer_id: i64,  // trainer id — same id = same person's team (1-3 umas); 0 = NPC
    pub wx: f32,         // world position X (track-map minimap)
    pub wz: f32,         // world position Z
}

/// The full field for the broadcast timing tower, ordered leader-first. Empty until a race is
/// live. Built from the same per-frame telemetry buffer the HUD uses — all pure reads, so this
/// is read-only and has no effect on the race.
pub fn field_rows() -> Vec<FieldRow> {
    let mut hs: Vec<HorseTelem> = match telem_buf().lock() {
        Ok(b) if !b.is_empty() => b.values().copied().collect(),
        _ => return Vec::new(),
    };
    let target = crate::freecam::followed_gate();
    // Leader-first: by race order when valid, falling back to distance covered.
    // Order: Umas that have CROSSED the finish line first, in their exact CROSSING order (frozen rank),
    // then the still-racing Umas by DISTANCE covered (most = ahead). The game's CurOrder field is
    // unreliable (it resets at the line — used to send the winner to the bottom), and raw run-out
    // distance diverges after the line — so we freeze the crossing order for an on-stream-correct result.
    let franks = finish_rank().lock().map(|m| m.clone()).unwrap_or_default();
    hs.sort_by(|a, b| {
        match (franks.get(&a.gate), franks.get(&b.gate)) {
            (Some(x), Some(y)) => x.cmp(y),                  // both finished → crossing order
            (Some(_), None) => std::cmp::Ordering::Less,     // a finished, b racing → a ahead
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b.distance.partial_cmp(&a.distance).unwrap_or(std::cmp::Ordering::Equal),
        }
    });
    // pos is assigned later from this order; the trend (for the green/red flash) is computed below
    // from OUR distance order, NOT the game's CurOrder.
    let leader_dist = hs.first().map(|h| h.distance).unwrap_or(0.0);
    let n = hs.len().max(1) as f32;
    // ── live win-probability model: PHYSICS-grounded ETA-to-finish (HeavenSim-derived) ──
    // For each Uma we project its real time-to-finish using the game's EXACT HP-drain formula
    // (HpDecBase·gap²/SpeedGapParam1Pow, gap = speed − raceBaseSpeed + 12): if its current HP can
    // sustain its pace to the line it finishes at pace (small spurt bonus in the last 3F); if not,
    // it runs at pace until HP=0 then crawls home at the min-speed floor (raceBaseSpeed·MinSpeedRate).
    // Lowest ETA = most likely winner → softmax over −ETA. Early on, ETAs are near-tied so a betting
    // favourite (人気) prior leads; it fades to zero as the race resolves. Uses only live telemetry.
    let course = crate::race::course_distance() as f32;
    let progress = if course > 0.0 { (leader_dist / course).clamp(0.0, 1.0) } else { 0.0 };
    // raceBaseSpeed (REAL): 20 − (courseDistance − 2000)/1000. Min/gassed floor ≈ rbs·0.85 + guts bump.
    let rbs = if course > 0.0 { 20.0 - (course - 2000.0) / 1000.0 } else { 20.0 };
    let gassed = (rbs * 0.85 + 0.3).max(1.0);
    // expected time (seconds) for horse h to cover its remaining distance, from now
    let eta = |h: &HorseTelem| -> f32 {
        let remaining = if course > 0.0 { (course - h.distance).max(0.0) } else { 1.0 };
        if remaining <= 0.0 {
            return 0.0;
        }
        let spd = h.speed.max(rbs * 0.5).max(1.0);
        if h.exhausted || h.hp <= 1.0 {
            return remaining / gassed; // already gassed → crawl to the line
        }
        // real HP-drain rate at this pace (end-phase guts term approximated, no guts stat live)
        let gap = (spd - rbs + 12.0).max(0.0);
        let gmult = if progress >= 0.66 { 1.7 } else { 1.0 }; // tuned vs 700 real races
        let drain = (gap * gap / 144.0) * 20.0 * gmult;
        let t_at_pace = remaining / spd;
        let hp_needed = drain * t_at_pace;
        if h.hp >= hp_needed {
            let v = spd; // can sustain to the line (tuning showed no spurt-speed bonus helps)
            remaining / v
        } else {
            // gases out partway: hold pace until HP=0, then crawl the rest at the floor
            let t_survive = h.hp / drain.max(1e-3);
            let d_survive = (spd * t_survive).min(remaining);
            t_survive + (remaining - d_survive).max(0.0) / gassed
        }
    };
    let etas: Vec<f32> = hs.iter().map(eta).collect();
    let min_eta = etas.iter().cloned().fold(f32::MAX, f32::min);
    // softmax over −T·(ETA − best) + a fading 人気 prior. T≈1.2/s → ~1s ETA edge ≈ 3× odds.
    let logits: Vec<f32> = hs
        .iter()
        .zip(&etas)
        .map(|(h, e)| {
            let pop = if h.popularity > 0 { h.popularity as f32 } else { n * 0.5 };
            // Temperature sharpens as the race resolves (uncertain early → confident late), tuned
            // against 700 real races. Plus the fading 人気 favourite prior.
            let t = 0.8 + 5.0 * progress;
            -t * (e - min_eta) + 0.45 * (1.0 - progress) * -(pop - 1.0)
        })
        .collect();
    let maxl = logits.iter().cloned().fold(f32::MIN, f32::max);
    let exps: Vec<f32> = logits.iter().map(|l| (l - maxl).exp()).collect();
    let sum: f32 = exps.iter().sum::<f32>().max(1e-6);

    let mut prev_dist = leader_dist;
    let mut out = Vec::with_capacity(hs.len());
    for (i, h) in hs.iter().enumerate() {
        let interval = (prev_dist - h.distance).max(0.0);
        prev_dist = h.distance;
        out.push(FieldRow {
            pos: i as i32 + 1,
            gate: h.gate,
            name: gate_name(h.gate),
            style: h.running_style,
            sta: if h.max_hp > 0.0 { (h.hp / h.max_hp).clamp(0.0, 1.0) } else { 0.0 },
            interval,
            gap_leader: (leader_dist - h.distance).max(0.0),
            trend: if h.prev_order > 0 { h.prev_order - h.order } else { 0 },
            popularity: h.popularity,
            spurt: h.spurt,
            exhausted: h.exhausted,
            fight: h.fight,
            blocked: h.blocked,
            followed: h.gate == target,
            win: exps[i] / sum,
            dist: h.distance,
            speed: h.speed,
            trainer: trainer_map().lock().ok().and_then(|m| m.get(&h.gate).map(|(n, _)| n.clone())).unwrap_or_default(),
            viewer_id: trainer_map().lock().ok().and_then(|m| m.get(&h.gate).map(|(_, v)| *v)).unwrap_or(0),
            wx: h.wx,
            wz: h.wz,
        });
    }
    // Override the trend with OUR distance-based position change (the game's CurOrder is unreliable).
    // +ve = gained a place (moved up) → green flash; -ve = lost → red. The prev-position snapshot is
    // throttled so the trend stays non-zero long enough for the flash, and so the several field_rows
    // calls per frame all read the same value.
    {
        let now = clock().elapsed().as_millis() as u64;
        let refresh = now.saturating_sub(LAST_POS_MS.load(Ordering::Relaxed)) > 150;
        if let Ok(mut prev) = prev_pos().lock() {
            for r in out.iter_mut() {
                r.trend = prev.get(&r.gate).map(|p| p - r.pos).unwrap_or(0);
            }
            if refresh {
                for r in &out {
                    prev.insert(r.gate, r.pos);
                }
                LAST_POS_MS.store(now, Ordering::Relaxed);
            }
        }
    }
    out
}

// ── live speed history (followed Uma) — the WHOLE race, sampled by PROGRESS, drawn left→right ──
/// Number of buckets across 0→100 % of the course. The pace graph fills these as the race runs.
pub const PACE_BUCKETS: usize = 140;
static SPEED_TRACE: OnceLock<Mutex<Vec<f32>>> = OnceLock::new();
fn speed_trace_buf() -> &'static Mutex<Vec<f32>> {
    SPEED_TRACE.get_or_init(|| Mutex::new(Vec::new()))
}
/// The followed Uma's whole-race speed history: one sample per progress bucket, index 0 = race start,
/// the last filled index = current progress. Drawn as a graph that builds left→right over the race.
pub fn speed_trace() -> Vec<f32> {
    speed_trace_buf().lock().map(|t| t.clone()).unwrap_or_default()
}
/// Record the followed Uma's speed at its current race progress (0..1). Fills any skipped buckets up
/// to the current one, so the history only ever grows rightward.
fn push_pace(progress: f32, v: f32) {
    let b = ((progress.clamp(0.0, 1.0) * PACE_BUCKETS as f32) as usize).min(PACE_BUCKETS - 1);
    if let Ok(mut t) = speed_trace_buf().lock() {
        while t.len() <= b {
            let fill = *t.last().unwrap_or(&v);
            t.push(fill);
        }
        t[b] = v;
    }
}
/// Clear the pace history + live outlook (on a new race / when switching followed Uma).
pub fn reset_pace() {
    if let Ok(mut t) = speed_trace_buf().lock() {
        t.clear();
    }
    reset_outlook();
}
/// Clear the pace history + skill feed (freecam calls this when switching the followed Uma).
pub fn on_switch_follow() {
    reset_skill_feed();
    reset_pace();
}
/// Fresh-race wipe (freecam calls this on a new-race capture): clear telemetry + skill feed.
pub fn on_new_race() {
    if let Ok(mut b) = telem_buf().lock() {
        b.clear();
    }
    reset_skill_feed();
}
// gate → charaId (HorseData.charaId), captured in the ctor hook — for the portrait icon.
static IDMAP: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
fn id_map() -> &'static Mutex<HashMap<i32, i32>> {
    IDMAP.get_or_init(|| Mutex::new(HashMap::new()))
}

// ── publish entry points (called from the freecam race-camera hooks) ──────────
/// Per-frame telemetry for one horse — called from `on_run_motion` for EVERY horse when telemetry
/// (or the freecam) is active. `this` = HorseRaceInfoReplay, `gate` its lane, `target` the followed
/// gate, `course` the course distance (m). Pure reads; publishes into the telemetry buffer.
pub unsafe fn publish_frame(this: *mut c_void, gate: i32, target: i32, course: f32) {
    if !(gate > 0 && HP_OFF.load(Ordering::Relaxed) != 0) {
        return;
    }
    let rdf = |o: &AtomicUsize| -> f32 {
        let v = o.load(Ordering::Relaxed);
        if v == 0 { 0.0 } else { ((this as usize + v) as *const f32).read_unaligned() }
    };
    let ord_off = ORDER_OFF.load(Ordering::Relaxed);
    let order = if ord_off == 0 { 0 } else { ((this as usize + ord_off) as *const i32).read_unaligned() };
    // PURE field reads only — NO managed method calls here (calling getters per-horse-per-frame
    // inside the run-motion hook perturbed the horses / stuck them at the gate).
    // Last spurt = the race's final leg (phase >= 2, i.e. from ~2/3 distance). _phase is a
    // pure i32 field; phases: 0 start, 1 middle, 2 final leg, 3 last-spurt stretch, 4 finish.
    let ph_off = PHASE_OFF.load(Ordering::Relaxed);
    let spurt = ph_off != 0 && ((this as usize + ph_off) as *const i32).read_unaligned() >= 2;
    let hpe_off = HPEMPTY_OFF.load(Ordering::Relaxed);
    let exhausted = hpe_off != 0 && ((this as usize + hpe_off) as *const u8).read_unaligned() != 0;
    // Live race-state flags — pure bool/i32/float reads (same safety profile as the above).
    let rdb = |o: &AtomicUsize| -> bool {
        let v = o.load(Ordering::Relaxed);
        v != 0 && ((this as usize + v) as *const u8).read_unaligned() != 0
    };
    let blk_off = BLOCKFRONT_OFF.load(Ordering::Relaxed);
    let blocked = blk_off != 0 && ((this as usize + blk_off) as *const f32).read_unaligned() > 0.0;
    let pvo_off = PREVORDER_OFF.load(Ordering::Relaxed);
    let prev_order = if pvo_off == 0 { 0 } else { ((this as usize + pvo_off) as *const i32).read_unaligned() };
    let def_off = DEFEAT_OFF.load(Ordering::Relaxed);
    let defeat = if def_off == 0 { 0 } else { ((this as usize + def_off) as *const i32).read_unaligned() };
    // World position (X,Z) for the track-map minimap — pure Vector3 field read.
    let (mut wx, mut wz) = (0.0f32, 0.0f32);
    let po = crate::freecam::pos_off();
    if po != 0 {
        let p = (this as usize + po) as *const f32;
        wx = p.read_unaligned();
        wz = p.add(2).read_unaligned();
    }
    // Identity (static per race) — pure pointer-chase, no managed calls: popularity off the
    // HorseData, running style off its server-response data. 0 if any link is absent.
    let (mut popularity, mut running_style) = (0i32, 0i32);
    let hd_off = HDATA_OFF.load(Ordering::Relaxed);
    if hd_off != 0 {
        let hd = ((this as usize + hd_off) as *const *mut c_void).read_unaligned();
        if !hd.is_null() {
            let po = POP_OFF.load(Ordering::Relaxed);
            if po != 0 {
                popularity = ((hd as usize + po) as *const i32).read_unaligned();
            }
            let ro = RESP_OFF.load(Ordering::Relaxed);
            if ro != 0 {
                let resp = ((hd as usize + ro) as *const *mut c_void).read_unaligned();
                if !resp.is_null() {
                    let so = RSTYLE_OFF.load(Ordering::Relaxed);
                    if so != 0 {
                        running_style = ((resp as usize + so) as *const i32).read_unaligned();
                    }
                    // Trainer identity is static per race → read the managed string just once per gate.
                    let unknown = trainer_map().lock().map(|m| !m.contains_key(&gate)).unwrap_or(false);
                    if unknown {
                        let to = TNAME_OFF.load(Ordering::Relaxed);
                        let vo = VIEWER_OFF.load(Ordering::Relaxed);
                        let tname = if to != 0 {
                            let sp = ((resp as usize + to) as *const *mut c_void).read_unaligned();
                            read_managed_str(sp).unwrap_or_default()
                        } else {
                            String::new()
                        };
                        let vid = if vo != 0 { ((resp as usize + vo) as *const i64).read_unaligned() } else { 0 };
                        if let Ok(mut m) = trainer_map().lock() {
                            m.insert(gate, (tname, vid));
                        }
                    }
                }
            }
        }
    }
    let t = HorseTelem {
        gate,
        order,
        hp: rdf(&HP_OFF),
        max_hp: rdf(&MAXHP_OFF),
        speed: rdf(&SPEED_OFF),
        distance: rdf(&DIST_OFF),
        spurt,
        exhausted,
        skills: 0, // per-uma skills come from the activation FEED (skill_feed())
        late_start: rdb(&BADSTART_OFF),
        fight: rdb(&COMPFIGHT_OFF),
        leading: rdb(&COMPTOP_OFF),
        blocked,
        prev_order,
        popularity,
        running_style,
        defeat,
        wx,
        wz,
    };
    // Heartbeat + new-race detection: if telemetry resumed after a real gap (we left the race
    // and a new one started — even with the SAME player gate), wipe last race's data so nothing
    // freezes over. Then mark the heartbeat fresh.
    let now_ms = clock().elapsed().as_millis() as u64;
    if now_ms.saturating_sub(LAST_TELEM_MS.load(Ordering::Relaxed)) > 800 {
        if let Ok(mut b) = telem_buf().lock() {
            b.clear();
        }
        if let Ok(mut m) = trainer_map().lock() {
            m.clear();
        }
        if let Ok(mut m) = finish_rank().lock() {
            m.clear();
        }
        FINISH_NEXT.store(0, Ordering::Relaxed);
        if let Ok(mut m) = prev_pos().lock() {
            m.clear();
        }
        reset_skill_feed();
        reset_pace();
        RACE_EPOCH.fetch_add(1, Ordering::Relaxed);
    }
    LAST_TELEM_MS.store(now_ms, Ordering::Relaxed);
    if let Ok(mut b) = telem_buf().lock() {
        b.insert(gate, t);
    }
    // Finish-line crossing: the FIRST frame a Uma reaches the course distance, capture its finish
    // rank (the order it crossed) so the tower freezes that order during the run-out.
    if course > 0.0 && t.distance >= course {
        let unranked = finish_rank().lock().map(|m| !m.contains_key(&gate)).unwrap_or(false);
        if unranked {
            let rank = FINISH_NEXT.fetch_add(1, Ordering::Relaxed) + 1;
            if let Ok(mut m) = finish_rank().lock() {
                m.insert(gate, rank);
            }
        }
    }
    // Pace history: sample the FOLLOWED Uma's speed at its current race progress (works in
    // telemetry-only too, since this runs whenever telemetry is on — not just under freecam).
    if gate == target && course > 0.0 && t.speed > 0.5 {
        push_pace(t.distance / course, t.speed);
    }
}

/// Followed-Uma only: skill feed + active-skill countdown + AI state + last-spurt outlook.
/// Called from `on_run_motion` for the followed gate. `this` = HorseRaceInfoReplay.
pub unsafe fn update_followed(this: *mut c_void) {
    update_skill_feed(this);
    update_active_skills(this); // pure field walk, no managed call → safe
    update_follow_state(this); // kakari / position-keep / down-slope (AI real getters)
    // Spurt sustainability: call the AI's REAL getter (unique RVA, not the HorseRaceInfo stub),
    // and only once the spurt phase has started (phase>=2) so the calculator is populated. Guarded
    // by a non-null AI pointer.
    let ph_off = PHASE_OFF.load(Ordering::Relaxed);
    let in_spurt = ph_off != 0 && ((this as usize + ph_off) as *const i32).read_unaligned() >= 2;
    let sg = AI_SPURT_GET.load(Ordering::Relaxed);
    let ai_off = AI_OFF.load(Ordering::Relaxed);
    if in_spurt && sg != 0 && ai_off != 0 {
        let ai = ((this as usize + ai_off) as *const *mut c_void).read_unaligned();
        if !ai.is_null() {
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 = std::mem::transmute(sg);
            SPURT_OUTLOOK.store(f(ai, std::ptr::null_mut()), Ordering::Relaxed);
        }
    }
}

/// Constructor-time capture (called from `on_hri_ctor`): map gate → display name + charaId.
/// `data` = HorseData. Reads two static-per-race fields once per gate.
pub unsafe fn on_ctor(gate: i32, data: *mut c_void) {
    // Capture the Uma's display name (HorseData.charaName) → gate→name map for the HUD.
    let noff = NAME_OFF.load(Ordering::Relaxed);
    if noff != 0 {
        let sp = ((data as usize + noff) as *const *mut c_void).read_unaligned();
        if let Some(name) = read_managed_str(sp) {
            if !name.is_empty() {
                if let Ok(mut nm) = name_map().lock() {
                    nm.insert(gate, name);
                }
            }
        }
    }
    // Capture charaId (HorseData.charaId) → gate→id map for the portrait icon.
    let coff = CHARAID_OFF.load(Ordering::Relaxed);
    if coff != 0 {
        let cid = ((data as usize + coff) as *const i32).read_unaligned();
        if cid > 0 {
            if let Ok(mut m) = id_map().lock() {
                m.insert(gate, cid);
            }
        }
    }
}

// ── install: resolve the telemetry field offsets + method pointers ────────────
/// Resolve every HorseRaceInfo / HorseData / RaceHorseData / SkillManager / AI field offset and
/// method pointer the telemetry provider reads. Called once from `freecam::install()`. Returns
/// `true` if the core telemetry offset (_hp) resolved (for the boot-log "telem" tag).
pub fn install_offsets() -> bool {
    // Live telemetry offsets (HorseRaceInfo). All on the same class the run-motion hook's
    // `this` derives from, so they read straight off `this`.
    for (name, slot) in [
        ("_hp", &HP_OFF),
        ("_maxHp", &MAXHP_OFF),
        ("<CurOrder>k__BackingField", &ORDER_OFF),
        ("_lastSpeed", &SPEED_OFF),
        ("_distance", &DIST_OFF),
        ("<IsHpEmptyOnRace>k__BackingField", &HPEMPTY_OFF),
        ("_phase", &PHASE_OFF),
        ("<IsBadStart>k__BackingField", &BADSTART_OFF),
        ("<IsCompeteFight>k__BackingField", &COMPFIGHT_OFF),
        ("<IsCompeteTop>k__BackingField", &COMPTOP_OFF),
        ("<BlockFrontContinueTime>k__BackingField", &BLOCKFRONT_OFF),
        ("<PrevOrder>k__BackingField", &PREVORDER_OFF),
        ("_defeat", &DEFEAT_OFF),
    ] {
        if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), name) {
            slot.store(off, Ordering::Relaxed);
        }
    }
    // Identity chain for the broadcast tower: HorseRaceInfo._horseData → HorseData
    // (.<Popularity>, ._responseHorseData) → RaceHorseData.running_style.
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), "_horseData") {
        HDATA_OFF.store(off, Ordering::Relaxed);
    }
    let hdc = il2cpp::class("Gallop.HorseData");
    if let Some(off) = il2cpp::field_offset(hdc, "<Popularity>k__BackingField") {
        POP_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(hdc, "_responseHorseData") {
        RESP_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.RaceHorseData"), "running_style") {
        RSTYLE_OFF.store(off, Ordering::Relaxed);
    }
    // Trainer identity (lobby races): RaceHorseData.trainer_name + .viewer_id.
    let rhd = il2cpp::class("Gallop.RaceHorseData");
    if let Some(off) = il2cpp::field_offset(rhd, "trainer_name") {
        TNAME_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(rhd, "viewer_id") {
        VIEWER_OFF.store(off, Ordering::Relaxed);
    }
    // Skill activation feed: HorseRaceInfo._skillManager → SkillManager._usedSkillIdList,
    // + MasterDataUtil.GetSkillName(id) for names.
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), "_skillManager") {
        SKILLMGR_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.SkillManager"), "_usedSkillIdList") {
        USEDLIST_OFF.store(off, Ordering::Relaxed);
    }
    // Spurt sustainability: HorseRaceInfo._horseRaceAI + the AI's REAL getter (HorseRaceAIReplay,
    // unique RVA — NOT the HorseRaceInfo interface stub).
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), "_horseRaceAI") {
        AI_OFF.store(off, Ordering::Relaxed);
    }
    let aisg = il2cpp::method(il2cpp::class("Gallop.HorseRaceAIReplay"), "get_LastSpurtCalcResult", 0);
    if !aisg.is_null() {
        AI_SPURT_GET.store(il2cpp::method_pointer(aisg) as usize, Ordering::Relaxed);
    }
    // Live race-state getters (kakari / position-keep / down-slope) on HorseRaceAIBase.
    let aib = il2cpp::class("Gallop.HorseRaceAIBase");
    for (name, slot) in [
        ("get_IsTemptation", &KAKARI_GET),
        ("get_TemptationMode", &TEMPTMODE_GET),
        ("get_PositionKeepMode", &KEEPMODE_GET),
        ("get_IsDownSlopeAccelMode", &DOWNHILL_GET),
    ] {
        let m = il2cpp::method(aib, name, 0);
        if !m.is_null() {
            slot.store(il2cpp::method_pointer(m) as usize, Ordering::Relaxed);
        }
    }
    // Active-skill field walk: SkillManager._skills → SkillBase.{SkillMaster,Details} → SkillDetail.
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.SkillManager"), "_skills") {
        SKILLS_LIST_OFF.store(off, Ordering::Relaxed);
    }
    let sbc = il2cpp::class("Gallop.SkillBase");
    if let Some(off) = il2cpp::field_offset(sbc, "<SkillMaster>k__BackingField") {
        SB_MASTER_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(sbc, "<Details>k__BackingField") {
        SB_DETAILS_OFF.store(off, Ordering::Relaxed);
    }
    let sdc = il2cpp::class("Gallop.SkillDetail");
    if let Some(off) = il2cpp::field_offset(sdc, "<LeftTime>k__BackingField") {
        SD_LEFT_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(sdc, "<Category>k__BackingField") {
        SD_CAT_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(sdc, "_isDebuff") {
        SD_DEBUFF_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(sdc, "<IsActivated>k__BackingField") {
        SD_ACT_OFF.store(off, Ordering::Relaxed);
    }
    let mdu = il2cpp::class("Gallop.MasterDataUtil");
    let gsn = il2cpp::method(mdu, "GetSkillName", 1);
    if !gsn.is_null() {
        GSN_FN.store(il2cpp::method_pointer(gsn) as usize, Ordering::Relaxed);
        GSN_MI.store(gsn as usize, Ordering::Relaxed);
        let is_static = unsafe {
            match h::METHOD_GET_FLAGS {
                Some(f) => (f(gsn as *mut h::RawMethod, std::ptr::null_mut()) & h::METHOD_ATTRIBUTE_STATIC) != 0,
                None => true,
            }
        };
        GSN_STATIC.store(is_static, Ordering::Relaxed);
    }
    // Skill effect-value lookup (in-memory master data; public-safe — no the game data).
    let wtc = il2cpp::class("Gallop.WorkTrainingChallengeData");
    let mmget = il2cpp::method(wtc, "get_MasterManager", 0);
    if !mmget.is_null() {
        MM_GET.store(il2cpp::method_pointer(mmget) as usize, Ordering::Relaxed);
        MM_GET_MI.store(mmget as usize, Ordering::Relaxed);
    }
    let msd_cls = il2cpp::class("Gallop.MasterSkillData");
    let sget = il2cpp::method(msd_cls, "Get", 1);
    if !sget.is_null() {
        MSD_GET.store(il2cpp::method_pointer(sget) as usize, Ordering::Relaxed);
        MSD_GET_MI.store(sget as usize, Ordering::Relaxed);
    }
    // HorseData.charaName (string) → Uma display name for the HUD.
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseData"), "<charaName>k__BackingField") {
        NAME_OFF.store(off, Ordering::Relaxed);
    }
    // HorseData.charaId (int) → portrait icon lookup. Try a couple of likely field names.
    for fname in ["<charaId>k__BackingField", "charaId", "_charaId"] {
        if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseData"), fname) {
            CHARAID_OFF.store(off, Ordering::Relaxed);
            break;
        }
    }
    HP_OFF.load(Ordering::Relaxed) != 0
}
