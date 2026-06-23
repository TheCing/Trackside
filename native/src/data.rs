//! Mirror of Python `heaven_app.gamestate.GameState.to_dict()`.
//!
//! Field names and nesting MUST match the Python dataclasses (snake_case) so
//! the newline-delimited JSON produced by `sinks.TcpForwarder` deserializes
//! cleanly here. Every field is `#[serde(default)]` so partial/older payloads
//! never break the renderer.

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct CareerState {
    pub present: bool,
    pub n: i64,
    pub card_id: i64,
    pub chara_name: String,
    pub motivation: i64,
    pub motivation_label: String,
    pub grade: i64,
    pub running_style: i64,
    pub running_style_label: String,
    pub hp: i64,
    pub max_hp: i64,
    pub skill_point: i64,
    pub fan_count: i64,
    pub stats: BTreeMap<String, i64>,
    pub caps: BTreeMap<String, i64>,
    pub aptitudes: BTreeMap<String, String>,
    pub bonds: Vec<Bond>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Bond {
    pub slot: i64,
    pub name: String,
    pub limit_break: serde_json::Value, // int or "" — keep flexible
    pub bond: i64,
    pub is_npc: bool,
    pub friend: bool,
    pub golden: bool,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RaceHorse {
    pub idx: i64,
    pub dist: f64,
    pub lane: f64,
    pub speed: f64,
    pub hp: f64,
    pub max_hp: f64,
    pub tempt: i64,
    pub block: i64,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RaceSkill {
    pub time: f64,
    pub horse: i64,
    pub is_player: bool,
    pub skill_id: i64,
    pub name: String,
    pub condition_text: String,
    pub ability: String,
    pub value: String,
    pub duration: String,
    pub wit: bool,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct RaceState {
    pub active: bool,
    pub time: f64,
    pub duration: f64,
    pub horse_count: i64,
    pub player_index: i64,
    pub player_name: String,
    pub horses: Vec<RaceHorse>,
    pub skills: Vec<RaceSkill>,
    pub feed: Vec<serde_json::Value>,
    pub duels: i64,
    pub fights: i64,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct GameState {
    pub career: CareerState,
    pub race: RaceState,
    pub core_ready: bool,
    pub core_modules: Vec<String>,
}
