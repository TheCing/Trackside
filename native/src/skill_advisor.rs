//! End-of-career skill buy optimizer — port of UmaLauncher's skill_recommender.py /
//! daftuyda/UmaTools knapsack DP with group mutex, hint discounts, and aptitude multipliers.
//!
//! `chara_info` is captured from API responses in `response_hook`; the user opens the
//! Gameplay tab and presses Recommend (manual — no auto-popup on the skill screen).

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use once_cell::sync::Lazy;
use serde::Deserialize;

use crate::overlay::{accent, btn_primary_enabled, help_icon, status_dot, text_wrapped_colored, DIM, GOOD, TEXT, WARN};

const RANK_RANGES_JSON: &str = include_str!("../../data/rank_ranges.json");
const CARD_CHARA_JSON: &str = include_str!("../../data/card_chara.json");
const CHAINS_JSON: &str = include_str!("../../data/skill_chains.json");
const CARD_INHERENT_JSON: &str = include_str!("../../data/card_inherent.json");
const SKILL_ROLES_JSON: &str = include_str!("../../data/skill_roles.json");
const SKILL_DATA_JSON: &str = include_str!("../../data/skill_data.json");
const CM_PRESETS_JSON: &str = include_str!("../../data/cm_presets.json");
const COURSE_SPECS_JSON: &str = include_str!("../../data/course_specs.json");
const SKILL_CONSTRAINTS_JSON: &str = include_str!("../../data/skill_race_constraints.json");

const HINT_DISCOUNTS: [f64; 6] = [0.0, 0.10, 0.20, 0.30, 0.35, 0.40];
const MAX_HINT_LEVEL: i32 = 5;
/// Fast Learner (切れ者, condition id 7): extra 10% off every purchase, stacking
/// additively with the hint discount — matches UmaLauncher's FAST_LEARNER_DISCOUNT.
const FAST_LEARNER_DISCOUNT: f64 = 0.10;
const UNIQUE_OWN_RARITIES: [i32; 2] = [4, 5];

static CHARA: OnceLock<Mutex<Option<CharaInfo>>> = OnceLock::new();
static LAST_RESULT: OnceLock<Mutex<Option<RecommendResult>>> = OnceLock::new();
static RECOMMEND_BUSY: AtomicBool = AtomicBool::new(false);
static PRESET_LABELS: Lazy<Vec<(i32, String)>> = Lazy::new(build_preset_labels);

/// DP table size cap — real SP budgets are well below this; prevents multi‑second
/// stalls / OOM if packet data is corrupt or modded.
const MAX_DP_BUDGET: i32 = 20_000;

fn chara_slot() -> &'static Mutex<Option<CharaInfo>> {
    CHARA.get_or_init(|| Mutex::new(None))
}
fn result_slot() -> &'static Mutex<Option<RecommendResult>> {
    LAST_RESULT.get_or_init(|| Mutex::new(None))
}

#[derive(Clone, Debug)]
pub struct OwnedSkill {
    pub skill_id: i32,
    pub level: i32,
}

#[derive(Clone, Debug)]
pub struct SkillTip {
    pub group_id: i32,
    pub rarity: i32,
    pub level: i32,
}

#[derive(Clone, Debug)]
pub struct CharaInfo {
    pub skill_point: i32,
    pub card_id: i32,
    pub talent_level: i32,
    pub speed: i32,
    pub stamina: i32,
    pub power: i32,
    pub guts: i32,
    pub wiz: i32,
    pub proper_ground_turf: i32,
    pub proper_ground_dirt: i32,
    pub proper_distance_short: i32,
    pub proper_distance_mile: i32,
    pub proper_distance_middle: i32,
    pub proper_distance_long: i32,
    pub proper_running_style_nige: i32,
    pub proper_running_style_senko: i32,
    pub proper_running_style_sashi: i32,
    pub proper_running_style_oikomi: i32,
    pub skill_array: Vec<OwnedSkill>,
    pub skill_tips_array: Vec<SkillTip>,
    pub has_fast_learner: bool,
}

#[derive(Clone, Debug)]
pub struct ChainStep {
    pub skill_id: i32,
    pub name: String,
    pub cost: i32,
    pub hint_level: i32,
}

#[derive(Clone, Debug)]
pub struct PoolItem {
    pub skill_id: i32,
    pub group_id: i32,
    pub name: String,
    pub cost: i32,
    pub grade: i32,
    pub hint_level: i32,
    pub rarity: i32,
    pub role: String,
    pub multiplier: f64,
    pub chain: Vec<ChainStep>,
}

#[derive(Clone, Debug, Default)]
pub struct RatingBreakdown {
    pub stats: i32,
    pub skills: i32,
    pub unique: i32,
    pub total: i32,
}

#[derive(Clone, Debug)]
pub struct RecommendResult {
    pub selected: Vec<PoolItem>,
    pub skipped: Vec<PoolItem>,
    pub budget: i32,
    pub spent: i32,
    pub rating_gain: i32,
    /// Candidate count after filters — lets the UI show when filters are hiding skills.
    pub pool_size: usize,
    pub current: RatingBreakdown,
    pub projected: RatingBreakdown,
}

#[derive(Deserialize)]
struct ChainTier {
    id: i32,
    group_rate: i32,
    rarity: i32,
    cost: i32,
    grade: i32,
}

#[derive(Deserialize)]
struct InherentEntry {
    skill_id: i32,
    need_rank: i32,
}

#[derive(Deserialize)]
struct CmPreset {
    id: i32,
    name: Option<String>,
    date: Option<String>,
    #[serde(rename = "courseId")]
    course_id: i32,
    season: i32,
    ground: i32,
    weather: i32,
}

#[derive(Deserialize)]
struct CourseSpec {
    track_id: i32,
    distance: i32,
    distance_type: i32,
    surface: i32,
    turn: i32,
}

#[derive(Deserialize)]
struct ConstraintRule {
    eq: Option<Vec<i32>>,
    neq: Option<Vec<i32>>,
    lt: Option<i32>,
    le: Option<i32>,
    gt: Option<i32>,
    ge: Option<i32>,
}

static CHAINS: Lazy<HashMap<String, Vec<ChainTier>>> =
    Lazy::new(|| serde_json::from_str(CHAINS_JSON).unwrap_or_default());
static CARD_INHERENT: Lazy<HashMap<String, Vec<InherentEntry>>> =
    Lazy::new(|| serde_json::from_str(CARD_INHERENT_JSON).unwrap_or_default());
static SKILL_ROLES: Lazy<HashMap<String, String>> =
    Lazy::new(|| serde_json::from_str(SKILL_ROLES_JSON).unwrap_or_default());
static SKILL_META: Lazy<HashMap<String, serde_json::Value>> =
    Lazy::new(|| serde_json::from_str(SKILL_DATA_JSON).unwrap_or_default());
static CM_PRESETS: Lazy<Vec<CmPreset>> =
    Lazy::new(|| serde_json::from_str(CM_PRESETS_JSON).unwrap_or_default());
static COURSE_SPECS: Lazy<HashMap<String, CourseSpec>> =
    Lazy::new(|| serde_json::from_str(COURSE_SPECS_JSON).unwrap_or_default());
static SKILL_CONSTRAINTS: Lazy<HashMap<String, HashMap<String, ConstraintRule>>> =
    Lazy::new(|| serde_json::from_str(SKILL_CONSTRAINTS_JSON).unwrap_or_default());
static STAT_SCORES: Lazy<Vec<i32>> = Lazy::new(build_stat_scores);

fn bucket_mult(bucket: &str) -> f64 {
    match bucket {
        "good" => 1.10,
        "average" => 0.90,
        "bad" => 0.80,
        "terrible" => 0.70,
        _ => 1.0,
    }
}

fn role_group(role: &str) -> &str {
    match role {
        "turf" | "dirt" => "surface",
        "sprint" | "mile" | "medium" | "long" => "distance",
        "front" | "pace" | "late" | "end" => "style",
        other => other,
    }
}

fn aptitude_bucket(val: i32) -> &'static str {
    match val {
        8 | 7 => "good",
        6 | 5 => "average",
        4 | 3 | 2 => "bad",
        1 => "terrible",
        _ => "terrible",
    }
}

fn build_aptitude_buckets(info: &CharaInfo) -> HashMap<&'static str, &'static str> {
    let mut out = HashMap::new();
    out.insert("turf", aptitude_bucket(info.proper_ground_turf));
    out.insert("dirt", aptitude_bucket(info.proper_ground_dirt));
    out.insert("sprint", aptitude_bucket(info.proper_distance_short));
    out.insert("mile", aptitude_bucket(info.proper_distance_mile));
    out.insert("medium", aptitude_bucket(info.proper_distance_middle));
    out.insert("long", aptitude_bucket(info.proper_distance_long));
    out.insert("front", aptitude_bucket(info.proper_running_style_nige));
    out.insert("pace", aptitude_bucket(info.proper_running_style_senko));
    out.insert("late", aptitude_bucket(info.proper_running_style_sashi));
    out.insert("end", aptitude_bucket(info.proper_running_style_oikomi));
    out
}

fn aptitude_multiplier(role_str: &str, buckets: &HashMap<&'static str, &'static str>) -> f64 {
    if role_str.is_empty() {
        return 1.0;
    }
    let raw: Vec<&str> = role_str
        .split('/')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if raw.is_empty() {
        return 1.0;
    }
    if raw.len() == 1 {
        let role = raw[0];
        return buckets.get(role).map(|b| bucket_mult(b)).unwrap_or(1.0);
    }
    let mut group_max: HashMap<&str, f64> = HashMap::new();
    for role in raw {
        if let Some(bucket) = buckets.get(role) {
            let mult = bucket_mult(bucket);
            let grp = role_group(role);
            group_max
                .entry(grp)
                .and_modify(|m| *m = mult.max(*m))
                .or_insert(mult);
        }
    }
    if group_max.is_empty() {
        return 1.0;
    }
    group_max.values().product()
}

fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn skill_name(id: i32) -> String {
    SKILL_META
        .get(&id.to_string())
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string()
}

fn skill_meta(id: i32) -> Option<(i32, i32, i32)> {
    let v = SKILL_META.get(&id.to_string())?;
    let grade = v.get("grade_value")?.as_i64()? as i32;
    let group_id = v.get("group_id")?.as_i64()? as i32;
    let rarity = v.get("rarity")?.as_i64()? as i32;
    Some((grade, group_id, rarity))
}

/// Chain group id for a skill (for the Apply driver's chain-tier click mapping).
pub fn group_of(id: i32) -> Option<i32> {
    skill_meta(id).map(|(_, gid, _)| gid)
}

/// The skill ids acquired by pressing + `clicks` times on a learn-screen item showing
/// `any_tier_id`: the first N unowned purchasable tiers of its chain group, in order.
/// (One learn-screen item represents the whole ○→◎ chain; each + buys the next tier.)
pub fn tier_ids_for_clicks(any_tier_id: i32, clicks: i32) -> Vec<i32> {
    if clicks <= 0 {
        return Vec::new();
    }
    let owned: HashSet<i32> = chara_slot()
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|c| c.skill_array.iter().map(|x| x.skill_id).collect()))
        .unwrap_or_default();
    let Some(gid) = group_of(any_tier_id) else {
        return vec![any_tier_id];
    };
    let Some(members) = CHAINS.get(&gid.to_string()) else {
        return vec![any_tier_id];
    };
    let max_owned_gr = members
        .iter()
        .filter(|m| owned.contains(&m.id) && m.group_rate > 0)
        .map(|m| m.group_rate)
        .max()
        .unwrap_or(0);
    let out: Vec<i32> = members
        .iter()
        .filter(|m| m.group_rate > max_owned_gr && !UNIQUE_OWN_RARITIES.contains(&m.rarity))
        .take(clicks as usize)
        .map(|m| m.id)
        .collect();
    if out.is_empty() { vec![any_tier_id] } else { out }
}

/// The game's skill-list sort key (master.mdb `disp_order`): ascending matches the order the
/// player sees on the end-of-career learn screen. Unknown skills sort last.
pub fn skill_disp_order(id: i32) -> i32 {
    SKILL_META
        .get(&id.to_string())
        .and_then(|v| v.get("disp_order"))
        .and_then(|n| n.as_i64())
        .map(|n| n as i32)
        .unwrap_or(i32::MAX)
}

fn effective_cost(base_cost: i32, hint_level: i32, fast_learner: bool) -> i32 {
    let lvl = hint_level.clamp(0, MAX_HINT_LEVEL) as usize;
    let mut discount = HINT_DISCOUNTS.get(lvl).copied().unwrap_or(0.0);
    if fast_learner {
        discount += FAST_LEARNER_DISCOUNT;
    }
    let multiplier = (1.0 - discount).max(0.0);
    // int(x + 1e-9) like the reference — a bare floor() loses 1 SP whenever the product
    // lands a hair under an exact integer in float.
    ((base_cost as f64) * multiplier + 1e-9).floor().max(0.0) as i32
}

fn inherent_skills(card_id: i32, talent_level: i32) -> HashSet<i32> {
    let mut out = HashSet::new();
    if let Some(entries) = CARD_INHERENT.get(&card_id.to_string()) {
        for e in entries {
            if e.need_rank <= talent_level {
                out.insert(e.skill_id);
            }
        }
    }
    out
}

fn build_stat_scores() -> Vec<i32> {
    const R1: [i32; 25] = [
        5, 8, 10, 13, 16, 18, 21, 24, 26, 28, 29, 30, 31, 33, 34, 35, 39, 41, 42, 43, 52, 55, 66,
        68, 68,
    ];
    const R2: [i32; 81] = [
        79, 80, 81, 83, 84, 85, 86, 88, 89, 90, 92, 93, 94, 96, 97, 98, 100, 101, 102, 103, 105,
        106, 107, 109, 110, 111, 113, 114, 115, 117, 118, 119, 121, 122, 123, 124, 126, 127, 128,
        130, 131, 132, 134, 135, 136, 138, 139, 140, 141, 143, 144, 145, 147, 148, 149, 151, 152,
        153, 155, 156, 157, 159, 160, 161, 162, 164, 165, 166, 168, 169, 170, 172, 173, 174, 176,
        177, 178, 179, 181, 182, 182,
    ];
    const MAX: usize = 2500;
    let mut sc = vec![0i32; MAX + 1];
    let mut raw = 0i32;
    let mut idx = 0usize;
    for c in 1..=1200 {
        if c <= 49 {
            idx = 0;
        } else if c <= 99 {
            idx = 1;
        } else if c % 50 == 0 {
            idx += 1;
        }
        raw += R1[idx.min(R1.len() - 1)];
        sc[c] = ((raw as f64) / 10.0).round() as i32;
    }
    raw = 38413;
    idx = 0;
    for c in 1201..=2000 {
        if c <= 1209 {
            idx = 0;
        } else if c <= 1219 {
            idx = 1;
        } else if c % 10 == 0 {
            idx += 1;
        }
        raw += R2[idx.min(R2.len() - 1)];
        sc[c] = ((raw as f64) / 10.0).round() as i32;
    }
    raw = 142796;
    idx = 0;
    let mut rate = 183i32;
    for c in 2001..=MAX as i32 {
        if idx >= 25 {
            rate += 1;
            idx = 0;
        }
        raw += rate;
        idx += 1;
        sc[c as usize] = ((raw as f64) / 10.0).round() as i32;
    }
    sc
}

fn clamp_stat(v: i32) -> usize {
    v.clamp(0, 2500) as usize
}

fn compute_rating_breakdown(info: &CharaInfo, extra_skill_score: i32) -> RatingBreakdown {
    let sc = &*STAT_SCORES;
    let stats_total = sc[clamp_stat(info.speed)]
        + sc[clamp_stat(info.stamina)]
        + sc[clamp_stat(info.power)]
        + sc[clamp_stat(info.guts)]
        + sc[clamp_stat(info.wiz)];

    let buckets = build_aptitude_buckets(info);
    let mut owned_skill_score = 0i32;
    let mut unique_level = 0i32;
    for s in &info.skill_array {
        let Some((base_grade, _gid, rarity)) = skill_meta(s.skill_id) else {
            continue;
        };
        if UNIQUE_OWN_RARITIES.contains(&rarity) {
            unique_level = unique_level.max(s.level);
            continue;
        }
        if base_grade <= 0 {
            continue;
        }
        // skill_roles.json is keyed by skill_id (condition-derived by refresh_skill_data.py).
        let role = SKILL_ROLES.get(&s.skill_id.to_string()).cloned().unwrap_or_default();
        let mult = aptitude_multiplier(&role, &buckets);
        owned_skill_score += ((base_grade as f64) * mult).round() as i32;
    }

    let star = if info.talent_level == 0 {
        5
    } else {
        info.talent_level
    };
    let mult_per_level = if star == 1 || star == 2 { 120 } else { 170 };
    let unique_bonus = unique_level * mult_per_level;
    let skills_total = owned_skill_score + extra_skill_score;
    RatingBreakdown {
        stats: stats_total,
        skills: skills_total,
        unique: unique_bonus,
        total: stats_total + skills_total + unique_bonus,
    }
}

fn build_candidate_pool(info: &CharaInfo, offered_ids: &HashSet<i32>) -> (Vec<PoolItem>, i32) {
    let budget = info.skill_point;
    let owned_ids: HashSet<i32> = info.skill_array.iter().map(|s| s.skill_id).collect();
    let mut hint_by_group_rarity: HashMap<(i32, i32), i32> = HashMap::new();
    for tip in &info.skill_tips_array {
        let key = (tip.group_id, tip.rarity);
        hint_by_group_rarity
            .entry(key)
            .and_modify(|lv| *lv = (*lv).max(tip.level))
            .or_insert(tip.level);
    }

    let inherent_ids = inherent_skills(info.card_id, info.talent_level);
    let buckets = build_aptitude_buckets(info);

    // Groups of every skill the game is actually offering (live capture). This is what makes
    // inherited (green) skills, unhinted whites/golds, and other run-specific offers visible —
    // the static reconstruction below can't see them. Additive: never removes a group.
    let mut offered_groups: HashSet<i32> = HashSet::new();
    for sid in offered_ids {
        if let Some((_g, gid, _r)) = skill_meta(*sid) {
            offered_groups.insert(gid);
        }
    }

    let mut reachable_groups: HashSet<i32> = hint_by_group_rarity.keys().map(|(g, _)| *g).collect();
    reachable_groups.extend(offered_groups.iter().copied());
    for sid in &inherent_ids {
        for (gid_str, members) in CHAINS.iter() {
            if members.iter().any(|m| m.id == *sid) {
                if let Ok(gid) = gid_str.parse::<i32>() {
                    reachable_groups.insert(gid);
                }
            }
        }
    }

    // A tier is purchasable if the game literally lists it, OR reconstruction says so
    // (hint at its rarity / innate to the card). The live list is authoritative and additive.
    let tier_offered = |sid: i32, gid: i32, rar: i32| {
        offered_ids.contains(&sid)
            || inherent_ids.contains(&sid)
            || hint_by_group_rarity.contains_key(&(gid, rar))
    };

    let mut pool = Vec::new();
    for gid in reachable_groups {
        let key = gid.to_string();
        let Some(members) = CHAINS.get(&key) else {
            continue;
        };
        let max_owned_gr = members
            .iter()
            .filter(|m| owned_ids.contains(&m.id) && m.group_rate > 0)
            .map(|m| m.group_rate)
            .max()
            .unwrap_or(0);

        let mut cumulative_cost = 0;
        let mut cumulative_chain: Vec<ChainStep> = Vec::new();
        for m in members {
            if m.group_rate <= 0 {
                continue;
            }
            if UNIQUE_OWN_RARITIES.contains(&m.rarity) {
                continue;
            }
            if m.group_rate <= max_owned_gr {
                continue;
            }
            if !tier_offered(m.id, gid, m.rarity) {
                // Non-offered tier. For a group the game is actively listing (e.g. an
                // inherited green offered only at its upper tier), skip just this tier and
                // keep walking to the offered one; otherwise the chain ends here.
                if offered_groups.contains(&gid) {
                    continue;
                }
                break;
            }
            if m.grade <= 0 {
                continue;
            }
            let hint_lv = hint_by_group_rarity.get(&(gid, m.rarity)).copied().unwrap_or(0);
            let tier_cost = effective_cost(m.cost, hint_lv, info.has_fast_learner);
            cumulative_cost += tier_cost;
            let nm = skill_name(m.id);
            cumulative_chain.push(ChainStep {
                skill_id: m.id,
                name: nm.clone(),
                cost: tier_cost,
                hint_level: hint_lv,
            });

            let role = SKILL_ROLES.get(&m.id.to_string()).cloned().unwrap_or_default();
            let mult = aptitude_multiplier(&role, &buckets);
            let grade = ((m.grade as f64) * mult).round() as i32;
            pool.push(PoolItem {
                skill_id: m.id,
                group_id: gid,
                name: nm,
                cost: cumulative_cost,
                grade,
                hint_level: hint_lv,
                rarity: m.rarity,
                role,
                multiplier: mult,
                chain: cumulative_chain.clone(),
            });
        }
    }
    (pool, budget)
}

fn skill_matches_filter(role: &str, only_distance: &str, only_style: &str) -> bool {
    if only_distance.is_empty() && only_style.is_empty() {
        return true;
    }
    if role.is_empty() {
        return true;
    }
    let parts: Vec<&str> = role.split('/').map(|p| p.trim()).filter(|p| !p.is_empty()).collect();
    let distances: Vec<&str> = parts.iter().copied().filter(|p| role_group(p) == "distance").collect();
    let styles: Vec<&str> = parts.iter().copied().filter(|p| role_group(p) == "style").collect();
    if !only_distance.is_empty() && !distances.is_empty() && !distances.contains(&only_distance) {
        return false;
    }
    if !only_style.is_empty() && !styles.is_empty() && !styles.contains(&only_style) {
        return false;
    }
    true
}

fn race_spec_from_preset(preset_id: i32) -> Option<HashMap<String, i32>> {
    let preset = CM_PRESETS.iter().find(|p| p.id == preset_id)?;
    let course = COURSE_SPECS.get(&preset.course_id.to_string())?;
    let mut spec = HashMap::new();
    spec.insert("track_id".into(), course.track_id);
    spec.insert("distance".into(), course.distance);
    spec.insert("distance_type".into(), course.distance_type);
    spec.insert("ground_type".into(), course.surface);
    spec.insert("rotation".into(), course.turn);
    spec.insert("ground_condition".into(), preset.ground);
    spec.insert("weather".into(), preset.weather);
    spec.insert("season".into(), preset.season);
    Some(spec)
}

fn skill_passes_race(skill_id: i32, race_spec: &HashMap<String, i32>) -> bool {
    let Some(rules) = SKILL_CONSTRAINTS.get(&skill_id.to_string()) else {
        return true;
    };
    for (field, rule) in rules {
        let Some(race_val) = race_spec.get(field) else {
            continue;
        };
        if let Some(eq) = &rule.eq {
            if !eq.contains(race_val) {
                return false;
            }
        }
        if let Some(neq) = &rule.neq {
            if neq.contains(race_val) {
                return false;
            }
        }
        if let Some(lt) = rule.lt {
            if !(*race_val < lt) {
                return false;
            }
        }
        if let Some(le) = rule.le {
            if !(*race_val <= le) {
                return false;
            }
        }
        if let Some(gt) = rule.gt {
            if !(*race_val > gt) {
                return false;
            }
        }
        if let Some(ge) = rule.ge {
            if !(*race_val >= ge) {
                return false;
            }
        }
    }
    true
}

fn solve_knapsack(items: &[PoolItem], budget: i32) -> (Vec<PoolItem>, i32, i32) {
    if budget <= 0 || items.is_empty() {
        return (Vec::new(), 0, 0);
    }
    let budget = budget.min(MAX_DP_BUDGET);
    let b = budget as usize;
    // BTreeMap: fixed group iteration order so equal-value solutions tie-break the same
    // way every run (HashMap order is randomized per process).
    let mut groups: BTreeMap<i32, Vec<&PoolItem>> = BTreeMap::new();
    for it in items {
        groups.entry(it.group_id).or_default().push(it);
    }
    let group_list: Vec<Vec<&PoolItem>> = groups.into_values().collect();
    let mut dp = vec![0i32; b + 1];
    let mut choice: Vec<Vec<i32>> = vec![vec![-1; b + 1]; group_list.len()];

    for (g_idx, group_items) in group_list.iter().enumerate() {
        let old_dp = dp.clone();
        for c in 0..=b {
            let mut best_grade = old_dp[c];
            let mut best_choice = -1;
            for (i, it) in group_items.iter().enumerate() {
                let cost = it.cost as usize;
                if cost <= c {
                    let cand = old_dp[c - cost] + it.grade;
                    if cand > best_grade {
                        best_grade = cand;
                        best_choice = i as i32;
                    }
                }
            }
            dp[c] = best_grade;
            choice[g_idx][c] = best_choice;
        }
    }

    let mut selected = Vec::new();
    let mut c = b;
    for g_idx in (0..group_list.len()).rev() {
        let pick = choice[g_idx][c];
        if pick >= 0 {
            let it = group_list[g_idx][pick as usize];
            c = c.saturating_sub(it.cost as usize);
            selected.push((*it).clone());
        }
    }
    selected.reverse();
    let spent: i32 = selected.iter().map(|it| it.cost).sum();
    let grade: i32 = selected.iter().map(|it| it.grade).sum();
    (selected, spent, grade)
}

pub fn set_chara_info(info: CharaInfo) {
    if let Ok(mut slot) = chara_slot().lock() {
        *slot = Some(info);
    }
    if let Ok(mut r) = result_slot().lock() {
        *r = None;
    }
}

pub fn has_chara() -> bool {
    chara_slot().lock().map(|s| s.is_some()).unwrap_or(false)
}

pub fn chara_skill_points() -> i32 {
    chara_slot()
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|c| c.skill_point))
        .unwrap_or(0)
}

pub fn has_fast_learner() -> bool {
    chara_slot()
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|c| c.has_fast_learner))
        .unwrap_or(false)
}

pub fn recommend_now() -> Option<RecommendResult> {
    let info = chara_slot().lock().ok()?.clone()?;
    let only_distance = crate::settings::skill_filter_distance();
    let only_style = crate::settings::skill_filter_style();
    let preset_id = crate::settings::skill_filter_preset();
    let result = recommend(&info, &only_distance, &only_style, preset_id);
    if let Ok(mut r) = result_slot().lock() {
        *r = Some(result.clone());
    }
    Some(result)
}

pub fn is_recommend_busy() -> bool {
    RECOMMEND_BUSY.load(Ordering::Relaxed)
}

/// A recompute requested while a run was in flight. The worker re-kicks itself when it
/// finishes, so a click/filter change during "Computing…" is never silently swallowed.
static RERUN_QUEUED: AtomicBool = AtomicBool::new(false);

/// Run the optimizer on a worker thread so the render loop (and game) stay responsive.
pub fn request_recommend() {
    if RECOMMEND_BUSY.swap(true, Ordering::SeqCst) {
        RERUN_QUEUED.store(true, Ordering::SeqCst);
        return;
    }
    let info = match chara_slot().lock().ok().and_then(|s| s.clone()) {
        Some(i) => i,
        None => {
            RECOMMEND_BUSY.store(false, Ordering::SeqCst);
            return;
        }
    };
    let only_distance = crate::settings::skill_filter_distance();
    let only_style = crate::settings::skill_filter_style();
    let preset_id = crate::settings::skill_filter_preset();
    std::thread::spawn(move || {
        let result = recommend(&info, &only_distance, &only_style, preset_id);
        if let Ok(mut r) = result_slot().lock() {
            *r = Some(result);
        }
        RECOMMEND_BUSY.store(false, Ordering::SeqCst);
        if RERUN_QUEUED.swap(false, Ordering::SeqCst) {
            request_recommend();
        }
    });
}

/// One-click entry: open the optimizer window and (re)compute if career data is captured.
pub fn open_optimizer() {
    set_window_open(true);
    if has_chara() {
        request_recommend();
    }
}

/// The shared Distance / Style / Race-preset combo row (panel + optimizer window).
/// Any change persists the filter AND immediately queues a recompute — filters always
/// take effect without a separate button press. Returns true if something changed.
pub fn draw_filter_row(ui: &hudhook::imgui::Ui, w: f32) -> bool {
    const DISTANCES: [(&str, &str); 5] = [
        ("", "Any distance"),
        ("sprint", "Sprint"),
        ("mile", "Mile"),
        ("medium", "Medium"),
        ("long", "Long"),
    ];
    const STYLES: [(&str, &str); 5] = [
        ("", "Any style"),
        ("front", "Front (Nige)"),
        ("pace", "Pace (Senko)"),
        ("late", "Late (Sashi)"),
        ("end", "End (Oikomi)"),
    ];
    let mut changed = false;

    let cur_d = crate::settings::skill_filter_distance();
    let mut dist_idx = DISTANCES.iter().position(|(k, _)| *k == cur_d).unwrap_or(0);
    let dist_labels: Vec<&str> = DISTANCES.iter().map(|(_, l)| *l).collect();
    ui.set_next_item_width(w * 0.30);
    if ui.combo_simple_string("##skdist", &mut dist_idx, &dist_labels) {
        if let Some((key, _)) = DISTANCES.get(dist_idx) {
            crate::settings::set_skill_filter_distance(key);
            changed = true;
        }
    }
    ui.same_line();
    let cur_s = crate::settings::skill_filter_style();
    let mut style_idx = STYLES.iter().position(|(k, _)| *k == cur_s).unwrap_or(0);
    let style_labels: Vec<&str> = STYLES.iter().map(|(_, l)| *l).collect();
    ui.set_next_item_width(w * 0.30);
    if ui.combo_simple_string("##skstyle", &mut style_idx, &style_labels) {
        if let Some((key, _)) = STYLES.get(style_idx) {
            crate::settings::set_skill_filter_style(key);
            changed = true;
        }
    }
    ui.same_line();
    let presets = &*PRESET_LABELS;
    let cur_p = crate::settings::skill_filter_preset();
    let mut preset_idx = presets
        .iter()
        .position(|(id, _)| *id == cur_p)
        .unwrap_or(0)
        .min(presets.len().saturating_sub(1));
    let preset_labels_str: Vec<&str> = presets.iter().map(|(_, l)| l.as_str()).collect();
    ui.set_next_item_width((w * 0.34).max(80.0));
    if ui.combo_simple_string("##skpreset", &mut preset_idx, &preset_labels_str) {
        if let Some((id, _)) = presets.get(preset_idx) {
            crate::settings::set_skill_filter_preset(*id);
            changed = true;
        }
    }
    if changed {
        crate::tools::log(&format!(
            "[advisor] filters -> dist='{}' style='{}' preset={} (recompute)",
            crate::settings::skill_filter_distance(),
            crate::settings::skill_filter_style(),
            crate::settings::skill_filter_preset()
        ));
        request_recommend();
    }
    changed
}

pub fn last_result() -> Option<RecommendResult> {
    result_slot().lock().ok().and_then(|r| r.clone())
}

/// Last calibration: (our model's total, the game's official rank_score). Shown in the
/// panel so rating-model drift is visible instead of anecdotal.
static CALIBRATION: OnceLock<Mutex<Option<(i32, i32)>>> = OnceLock::new();

pub fn last_calibration() -> Option<(i32, i32)> {
    CALIBRATION.get_or_init(|| Mutex::new(None)).lock().ok().and_then(|c| *c)
}

/// Career complete: the game just told us the OFFICIAL rating for a fully-known chara.
/// Compute our model on the identical data and write a per-component attribution log —
/// this turns "the calc is off" into "the skills component is off by X".
pub fn calibrate_against(actual: i32, info: &CharaInfo) {
    let b = compute_rating_breakdown(info, 0);
    if let Ok(mut c) = CALIBRATION.get_or_init(|| Mutex::new(None)).lock() {
        *c = Some((b.total, actual));
    }
    let mut out = format!(
        "==== RATING CALIBRATION ====\ngame rank_score = {actual}\nour model       = {} (stats {} + skills {} + unique {})\ndelta           = {} ({}%)\n\nstats: spd {} sta {} pow {} guts {} wiz {}\nper-skill contributions:\n",
        b.total,
        b.stats,
        b.skills,
        b.unique,
        actual - b.total,
        (actual - b.total) * 1000 / actual.max(1) / 10,
        info.speed,
        info.stamina,
        info.power,
        info.guts,
        info.wiz
    );
    let buckets = build_aptitude_buckets(info);
    for s in &info.skill_array {
        let Some((grade, _gid, rarity)) = skill_meta(s.skill_id) else {
            out.push_str(&format!("  {} lv{}  (NOT IN DATA — contributes 0 in our model!)\n", s.skill_id, s.level));
            continue;
        };
        if UNIQUE_OWN_RARITIES.contains(&rarity) {
            out.push_str(&format!(
                "  {} lv{} [unique r{rarity}] -> bonus {}/lv\n",
                skill_name(s.skill_id),
                s.level,
                if info.talent_level == 1 || info.talent_level == 2 { 120 } else { 170 }
            ));
            continue;
        }
        let role = SKILL_ROLES.get(&s.skill_id.to_string()).cloned().unwrap_or_default();
        let mult = aptitude_multiplier(&role, &buckets);
        out.push_str(&format!(
            "  {} lv{} grade {} x {:.2} = {}\n",
            skill_name(s.skill_id),
            s.level,
            grade,
            mult,
            ((grade as f64) * mult).round() as i32
        ));
    }
    crate::tools::log_to("trackside-rating-calib.txt", &out);
}

/// Total rating for the captured chara PLUS a set of pending (marked-for-purchase) skills —
/// used by the live header while the player hand-picks on the game's skill screen. Skills
/// already owned contribute nothing extra. Baseline when `pending` is empty.
pub fn rating_with_pending(pending: &[i32]) -> i32 {
    let Some(info) = chara_slot().lock().ok().and_then(|s| s.clone()) else {
        return 0;
    };
    let owned: HashSet<i32> = info.skill_array.iter().map(|s| s.skill_id).collect();
    let buckets = build_aptitude_buckets(&info);
    let mut extra = 0i32;
    for sid in pending {
        if owned.contains(sid) {
            continue;
        }
        let Some((grade, _gid, rarity)) = skill_meta(*sid) else { continue };
        if grade <= 0 || UNIQUE_OWN_RARITIES.contains(&rarity) {
            continue;
        }
        let role = SKILL_ROLES.get(&sid.to_string()).cloned().unwrap_or_default();
        extra += ((grade as f64) * aptitude_multiplier(&role, &buckets)).round() as i32;
    }
    compute_rating_breakdown(&info, extra).total
}

/// Drop the cached recommendation (the live offered list changed under it). The panel shows
/// the Recommend button again rather than a stale buy list.
pub fn invalidate_result() {
    if let Ok(mut r) = result_slot().lock() {
        *r = None;
    }
}

/// Unique skills the game is showing on the learn screen (rarity 4/5 — displayed by the
/// optimizer as non-purchasable rows so the list mirrors the shop 1:1).
pub fn offered_uniques() -> Vec<(i32, String)> {
    let mut out: Vec<(i32, String)> = crate::skill_buyer::offered_skill_ids()
        .into_iter()
        .filter(|sid| {
            skill_meta(*sid).map(|(_, _, rar)| UNIQUE_OWN_RARITIES.contains(&rar)).unwrap_or(false)
        })
        .map(|sid| (sid, skill_name(sid)))
        .collect();
    out.sort_by_key(|(sid, _)| (skill_disp_order(*sid), *sid));
    out
}

fn recommend(info: &CharaInfo, only_distance: &str, only_style: &str, preset_id: i32) -> RecommendResult {
    // Ground truth: the exact skills the game is offering right now (empty if we're not on
    // the learn screen — then it's pure static reconstruction).
    let offered_ids: HashSet<i32> = crate::skill_buyer::offered_skill_ids().into_iter().collect();
    let (mut pool, budget) = build_candidate_pool(info, &offered_ids);
    if !only_distance.is_empty() || !only_style.is_empty() {
        pool.retain(|it| skill_matches_filter(&it.role, only_distance, only_style));
    }
    if preset_id >= 0 {
        if let Some(spec) = race_spec_from_preset(preset_id) {
            pool.retain(|it| skill_passes_race(it.skill_id, &spec));
        }
    }
    let current = compute_rating_breakdown(info, 0);
    let pool_size = pool.len();
    if pool.is_empty() {
        return RecommendResult {
            selected: Vec::new(),
            skipped: Vec::new(),
            budget,
            spent: 0,
            rating_gain: 0,
            pool_size,
            current: current.clone(),
            projected: current,
        };
    }
    let (mut selected, spent, rating_gain) = solve_knapsack(&pool, budget);
    // Present buys in the game's own learn-screen order so the list matches what the
    // player sees when they scroll the shop.
    selected.sort_by_key(|it| (skill_disp_order(it.skill_id), it.skill_id));
    // A pick's `chain` covers every tier bought on the way up (itself included) — none of
    // those belong in "not bought".
    let picked: HashSet<i32> = selected
        .iter()
        .flat_map(|it| it.chain.iter().map(|c| c.skill_id).chain(std::iter::once(it.skill_id)))
        .collect();
    let mut skipped: Vec<PoolItem> = pool.into_iter().filter(|it| !picked.contains(&it.skill_id)).collect();
    skipped.sort_by(|a, b| {
        let va = a.grade as f64 / (a.cost.max(1) as f64);
        let vb = b.grade as f64 / (b.cost.max(1) as f64);
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    let projected = compute_rating_breakdown(info, rating_gain);
    // Verbose: the whole decision in one line — filters, candidate pool, what got bought,
    // SP used, and the rating swing. Answers "why did it pick these / miss that".
    if crate::tools::debug_enabled() {
        crate::tools::debug(&format!(
            "[advisor] recommend: budget={budget}SP offered={} pool={pool_size} filt(dist='{only_distance}',style='{only_style}',preset={preset_id}) -> buy {} spend {spent}SP, rating {} -> {} (+{rating_gain})",
            offered_ids.len(),
            selected.len(),
            current.total,
            projected.total,
        ));
    }
    RecommendResult {
        selected,
        skipped,
        budget,
        spent,
        rating_gain,
        pool_size,
        current,
        projected,
    }
}

fn build_preset_labels() -> Vec<(i32, String)> {
    let mut out = vec![(-1, "None (no race filter)".to_string())];
    for p in CM_PRESETS.iter() {
        let name = p.name.as_deref().unwrap_or("CM");
        let date = p.date.as_deref().unwrap_or("?");
        out.push((p.id, format!("{name} — {date}")));
    }
    out
}

pub fn preset_labels() -> Vec<(i32, String)> {
    PRESET_LABELS.clone()
}

// ── Rating rank ladder ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RankRange {
    id: i32,
    min: i32,
    max: i32,
}

static RANK_RANGES: Lazy<Vec<RankRange>> =
    Lazy::new(|| serde_json::from_str(RANK_RANGES_JSON).unwrap_or_default());
static CARD_CHARA: Lazy<HashMap<String, i32>> =
    Lazy::new(|| serde_json::from_str(CARD_CHARA_JSON).unwrap_or_default());

/// master.mdb `single_mode_rank` id for a rating (1-based; 0 if the table is empty).
pub fn rank_id_for(rating: i32) -> i32 {
    for r in RANK_RANGES.iter() {
        if rating >= r.min && rating <= r.max {
            return r.id;
        }
    }
    RANK_RANGES.last().map(|r| r.id).unwrap_or(0)
}

/// Display label for a rank id. Ids 1–18 are the letter ranks; the remaining 80 ids are
/// exactly eight U-tiers × (base + 1..9): UG..UG9, UF..UF9, …, US..US9.
/// NOTE: the U-tier ladder is inferred from the table shape (98 = 18 + 8×10) — verify the
/// first in-game career-finish screen against this and adjust if the game disagrees.
pub fn rank_label(rank_id: i32) -> String {
    const LETTERS: [&str; 18] = [
        "G", "G+", "F", "F+", "E", "E+", "D", "D+", "C", "C+", "B", "B+", "A", "A+", "S", "S+",
        "SS", "SS+",
    ];
    if rank_id >= 1 && rank_id <= 18 {
        return LETTERS[(rank_id - 1) as usize].to_string();
    }
    const U_TIERS: [&str; 8] = ["UG", "UF", "UE", "UD", "UC", "UB", "UA", "US"];
    let u = rank_id - 19;
    if u >= 0 && (u as usize) < U_TIERS.len() * 10 {
        let tier = U_TIERS[(u / 10) as usize];
        let step = u % 10;
        return if step == 0 { tier.to_string() } else { format!("{tier}{step}") };
    }
    "?".to_string()
}

/// (min, max) rating bounds of the rank a given rating currently sits in.
pub fn rank_bounds(rating: i32) -> (i32, i32) {
    for r in RANK_RANGES.iter() {
        if rating >= r.min && rating <= r.max {
            return (r.min, r.max);
        }
    }
    RANK_RANGES.last().map(|r| (r.min, r.max)).unwrap_or((0, 1))
}

/// The next rank up from `rating`: (rank_id, threshold rating). None if already top rank.
pub fn next_rank(rating: i32) -> Option<(i32, i32)> {
    let cur = rank_id_for(rating);
    RANK_RANGES.iter().find(|r| r.id == cur + 1).map(|r| (r.id, r.min))
}

/// Character display name for the captured career (via card_chara → chara_name); "" if unknown.
pub fn chara_display_name() -> String {
    let cid = chara_id();
    if cid == 0 {
        return String::new();
    }
    crate::names::chara_name(cid as i64)
}

/// chara_id for the captured career's card (portrait lookup); 0 if unknown.
pub fn chara_id() -> i32 {
    let card = chara_slot()
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|c| c.card_id))
        .unwrap_or(0);
    CARD_CHARA.get(&card.to_string()).copied().unwrap_or(0)
}

// ── Optimizer window state ───────────────────────────────────────────────────

static WINDOW_OPEN: AtomicBool = AtomicBool::new(false);

pub fn window_open() -> bool {
    WINDOW_OPEN.load(Ordering::Relaxed)
}
pub fn set_window_open(on: bool) {
    WINDOW_OPEN.store(on, Ordering::Relaxed);
}

/// Preview-host design aid: when TRACKSIDE_SKOPT_MOCK is set, fabricate a plausible
/// result from bundled skill data and open the window — so the optimizer can be styled
/// in Preview-Trackside.ps1 without a captured career. Inert unless the env var is set.
pub fn mock_for_preview() {
    if std::env::var_os("TRACKSIDE_SKOPT_MOCK").is_none() {
        return;
    }
    let mut metas: Vec<(i32, i32, i32, String)> = SKILL_META
        .iter()
        .filter_map(|(k, v)| {
            let id = k.parse::<i32>().ok()?;
            let rarity = v.get("rarity")?.as_i64()? as i32;
            let grade = v.get("grade_value")?.as_i64()? as i32;
            let name = v.get("name")?.as_str()?.to_string();
            (grade > 0).then_some((id, rarity, grade, name))
        })
        .collect();
    metas.sort_by_key(|(id, ..)| *id);
    let mk = |(id, rarity, grade, name): &(i32, i32, i32, String), cost: i32, chain_from: Option<&(i32, i32, i32, String)>| {
        let mut chain = Vec::new();
        if let Some(pre) = chain_from {
            chain.push(ChainStep { skill_id: pre.0, name: pre.3.clone(), cost: cost / 2, hint_level: 2 });
        }
        chain.push(ChainStep { skill_id: *id, name: name.clone(), cost, hint_level: 0 });
        PoolItem {
            skill_id: *id,
            group_id: *id / 10,
            name: name.clone(),
            cost,
            grade: *grade,
            hint_level: if chain.len() > 1 { 2 } else { 0 },
            rarity: *rarity,
            role: "mile/pace".into(),
            multiplier: 1.1,
            chain,
        }
    };
    let golds: Vec<&(i32, i32, i32, String)> = metas.iter().filter(|m| m.1 >= 2).take(4).collect();
    let whites: Vec<&(i32, i32, i32, String)> = metas.iter().filter(|m| m.1 == 1).take(9).collect();
    let mut selected = Vec::new();
    for (i, g) in golds.iter().enumerate() {
        let pre = whites.get(6 + i % 3).copied();
        selected.push(mk(g, 260 + 40 * i as i32, pre));
    }
    for w in whites.iter().take(6) {
        selected.push(mk(w, 108, None));
    }
    let spent: i32 = selected.iter().map(|it| it.cost).sum();
    let current = RatingBreakdown { stats: 14_800, skills: 5_200, unique: 468, total: 20_468 };
    let projected = RatingBreakdown { stats: 14_800, skills: 11_726, unique: 468, total: 26_994 };
    let res = RecommendResult {
        rating_gain: projected.total - current.total,
        pool_size: selected.len() + 14,
        selected,
        skipped: Vec::new(),
        budget: spent + 2,
        spent,
        current,
        projected,
    };
    if let Ok(mut r) = result_slot().lock() {
        *r = Some(res);
    }
    set_window_open(true);
}

/// Comma-grouped number for the hero card (26994 → "26,994").
pub fn fmt_thousands(n: i32) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

pub(crate) fn draw_panel(ui: &hudhook::imgui::Ui, w: f32) {
    ui.dummy([0.0, 4.0]);
    if has_chara() {
        let fl = if has_fast_learner() { " · Fast Learner" } else { "" };
        status_dot(ui, GOOD, &format!("Career data captured — {} SP{}", chara_skill_points(), fl));
    } else {
        status_dot(ui, WARN, "Open end-of-career skill screen first");
    }
    // Live-capture indicator: shows at a glance whether recommendations come from the
    // game's real offered list (v2 build, on-screen) or static reconstruction only.
    let live_n = crate::skill_buyer::offered_skill_ids().len();
    if live_n > 0 {
        status_dot(ui, GOOD, &format!("Live shop list captured — {live_n} skills"));
    } else {
        status_dot(ui, DIM, "Live shop list: not captured (static reconstruction)");
    }
    // Rating-model calibration vs the last career's official rank_score: exact match =
    // trust the projections; drift = the calib log names the off component.
    if let Some((ours, actual)) = last_calibration() {
        let d = actual - ours;
        if d == 0 {
            status_dot(ui, GOOD, &format!("Rating model exact vs last career ({actual})"));
        } else {
            status_dot(
                ui,
                WARN,
                &format!("Rating model {d:+} vs last career (game {actual}, ours {ours}) — see trackside-rating-calib.txt"),
            );
        }
    }
    ui.same_line();
    help_icon(
        ui,
        "Captures your horse from the game's API when you're on the skill-buy screen. \
         Set distance/style/race filters, then Recommend to maximize rating under your SP budget. \
         Skills with no role tag are always kept.",
    );
    ui.dummy([0.0, 8.0]);

    // Shared filter row — changing anything recomputes immediately (no button press).
    ui.text_colored(DIM, "Filters:");
    draw_filter_row(ui, w);

    ui.dummy([0.0, 8.0]);
    // One click: opens the window AND runs the optimizer. Re-clicking recomputes.
    if btn_primary_enabled(ui, "##skopt_open", "Optimizer", has_chara()) {
        open_optimizer();
    }
    if is_recommend_busy() {
        ui.same_line_with_spacing(0.0, 8.0);
        ui.text_colored(accent(), "Computing…");
    }

    // Clone out so the worker never contends with a frame mid-draw.
    let res = result_slot().lock().ok().and_then(|guard| guard.clone());
    if let Some(res) = res {
        draw_results(ui, &res);
    }
}

fn draw_results(ui: &hudhook::imgui::Ui, res: &RecommendResult) {
    ui.dummy([0.0, 8.0]);
    let delta = res.projected.total - res.current.total;
    text_wrapped_colored(
        ui,
        TEXT,
        &format!(
            "Rating {} → {} (+{})  |  SP {} / spent {} / left {}",
            res.current.total,
            res.projected.total,
            delta,
            res.budget,
            res.spent,
            res.budget - res.spent
        ),
    );
    text_wrapped_colored(
        ui,
        DIM,
        &format!(
            "stats {} + skills {} → {} + unique {}",
            res.current.stats,
            res.current.skills,
            res.projected.skills,
            res.current.unique
        ),
    );
    ui.dummy([0.0, 6.0]);
    if res.selected.is_empty() && res.skipped.is_empty() {
        text_wrapped_colored(ui, WARN, "No purchasable skills found for these filters.");
    } else {
        if !res.selected.is_empty() {
            ui.text_colored(accent(), "Buy:");
            for it in res.selected.iter() {
                let hint = if it.hint_level > 0 {
                    format!(" (hint Lv{})", it.hint_level)
                } else {
                    String::new()
                };
                let role = if it.role.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", it.role)
                };
                ui.text_colored(
                    GOOD,
                    &format!(
                        "\u{00b7} {}{}{} — {} SP  +{}",
                        it.name, hint, role, it.cost, it.grade
                    ),
                );
                if it.chain.len() > 1 {
                    let via: Vec<String> = it.chain[..it.chain.len() - 1]
                        .iter()
                        .map(|c| format!("{} ({})", c.name, c.cost))
                        .collect();
                    ui.text_colored(DIM, &format!("  via {}", via.join(" → ")));
                }
            }
        }
        if !res.skipped.is_empty() {
            ui.dummy([0.0, 4.0]);
            ui.text_colored(DIM, "Not bought (top 10 by rating/SP):");
            let bought_groups: HashSet<i32> = res.selected.iter().map(|it| it.group_id).collect();
            for it in res.skipped.iter().take(10) {
                // Same group as a buy = excluded by the chain mutex, not by budget.
                let mutex = if bought_groups.contains(&it.group_id) {
                    "  (other tier of a buy)"
                } else {
                    ""
                };
                ui.text_colored(
                    TEXT,
                    &format!("\u{00b7} {} — {} SP  +{}{}", it.name, it.cost, it.grade, mutex),
                );
            }
        }
    }
}
