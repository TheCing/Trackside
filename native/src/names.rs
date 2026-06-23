//! Heaven Plan B — B6: in-process name enrichment.
//!
//! In Plan A the Python host (DataDB) mapped ids → names. Plan B has no Python,
//! so we embed the curated lookup tables straight into the DLL with include_str!
//! and parse them once. Paths are relative to this source file
//! (native/src/names.rs → ../../data/).

#![allow(dead_code)]

use once_cell::sync::Lazy;
use std::collections::HashMap;

use serde_json::Value;

const CHARA_JSON: &str = include_str!("../../data/chara_list.json");
const SUPPORT_JSON: &str = include_str!("../../data/support_list.json");
const SKILL_JSON: &str = include_str!("../../data/skill_data.json");

// chara_list.json:  { "100101": "Special Week", ... }
static CHARA: Lazy<HashMap<String, String>> =
    Lazy::new(|| serde_json::from_str(CHARA_JSON).unwrap_or_default());
// support_list.json: { "10001": { "name": "...", "rarity": "R", "type": "Guts" } }
static SUPPORT: Lazy<HashMap<String, Value>> =
    Lazy::new(|| serde_json::from_str(SUPPORT_JSON).unwrap_or_default());
// skill_data.json: { "10071": { "name": "...", ... } }
static SKILL: Lazy<HashMap<String, Value>> =
    Lazy::new(|| serde_json::from_str(SKILL_JSON).unwrap_or_default());

fn name_field(map: &HashMap<String, Value>, id: i64) -> String {
    map.get(&id.to_string())
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Character display name for a card/chara id ("" if unknown).
pub fn chara_name(id: i64) -> String {
    CHARA.get(&id.to_string()).cloned().unwrap_or_default()
}

/// Support card display name ("" if unknown).
pub fn support_name(id: i64) -> String {
    name_field(&SUPPORT, id)
}

/// Skill display name ("" if unknown).
pub fn skill_name(id: i64) -> String {
    name_field(&SKILL, id)
}
