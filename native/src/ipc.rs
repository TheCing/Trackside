//! Shared in-process state store (Plan B).
//!
//! In Plan A this was a loopback TCP server receiving GameState JSON from the
//! Python host. In Plan B everything is in-process: the native readers
//! (state.rs, race.rs) write directly here and the overlay reads via `latest()`.
//! No sockets, no JSON, no Python. The module name is kept as `ipc` only to
//! avoid churn in the `mod` graph — it is now a plain RwLock-backed store.

use crate::data::{CareerState, GameState, RaceState};
use once_cell::sync::Lazy;
use std::sync::{Mutex, RwLock};

/// The live game state. The overlay clones it cheaply per frame via `latest()`.
static STATE: Lazy<RwLock<GameState>> = Lazy::new(|| RwLock::new(GameState::default()));
/// One-line engine status shown in the overlay footer.
static STATUS: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new("starting…".into()));

pub fn latest() -> GameState {
    STATE.read().map(|g| g.clone()).unwrap_or_default()
}

/// Native career reader → publishes the latest CareerState.
pub fn set_career(c: CareerState) {
    if let Ok(mut g) = STATE.write() {
        g.career = c;
    }
}

/// Mutate the race state in place (for incremental frame/event updates).
pub fn with_race<F: FnOnce(&mut RaceState)>(f: F) {
    if let Ok(mut g) = STATE.write() {
        f(&mut g.race);
    }
}

pub fn status() -> String {
    STATUS.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn set_status(s: impl Into<String>) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s.into();
    }
}
