//! Event title lookup — `story_id` → human title, for the SuperSkip breadcrumb that
//! names each training event as it appears in the log. Pure localization data bundled
//! from master.mdb (text_data category 181); regenerate via the data-refresh tooling
//! after a game update.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

const EVENT_TITLES_JSON: &str = include_str!("../../data/event_titles.json");

fn titles() -> &'static HashMap<String, String> {
    static T: OnceLock<HashMap<String, String>> = OnceLock::new();
    T.get_or_init(|| serde_json::from_str(EVENT_TITLES_JSON).unwrap_or_default())
}

/// Event title for a story_id, or "" when unresolved. Titles carry the game's line-wrap
/// hints (literal `\n` and/or real newlines); the log line is one line, so flatten to spaces.
pub fn event_title(story_id: i64) -> String {
    let t = titles().get(&story_id.to_string()).cloned().unwrap_or_default();
    let t = t.replace("\\n", " ").replace('\n', " ");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}
