//! theme — accent color themes based on the real registered racing silks of famous
//! keiba names (the horses/owners featured in Umamusume). Each theme is a two-tone
//! accent (primary + secondary) laid over the fixed graphite base; the overlay's
//! accent-family colours all resolve through the getters here, so switching a theme
//! recolours the whole UI at once.
//!
//! Mode: either a fixed user-chosen theme, or "randomize on open" (a new silk is
//! rolled each time the menu is opened). Both persist via `settings`.
//!
//! Silk sources: JRA-registered owner colours as documented on the Umamusume wikis
//! (…/Real_Life pages) and owner colour registries. Colours are an artistic match to
//! each silk's dominant hues, tuned for readability on a dark background — not a
//! pixel-exact reproduction.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

/// One silk-derived theme: a display name + two accent hues (alpha always 1.0).
pub struct Theme {
    pub name: &'static str,
    /// Main accent — section titles, toggles ON, borders, nav badge, slider fill start.
    pub primary: [f32; 4],
    /// Secondary accent — slider fill end + small two-tone highlights.
    pub secondary: [f32; 4],
}

const fn c(r: f32, g: f32, b: f32) -> [f32; 4] {
    [r, g, b, 1.0]
}

/// The curated roster. Index 0 is the house teal (the default look); the rest are
/// silk-based. Order here is the cycle order in the picker.
pub static THEMES: &[Theme] = &[
    // House colour — clean teal on graphite.
    Theme { name: "Trackside", primary: c(0.26, 0.76, 0.70), secondary: c(0.44, 0.88, 0.82) },
    // Green, white sash, red single hoops (Symboli Bokujo).
    Theme { name: "Symboli Rudolf", primary: c(0.18, 0.58, 0.36), secondary: c(0.87, 0.31, 0.31) },
    // Black body, royal-blue sleeves, yellow sawtooth + cap (Kaneko Makoto).
    Theme { name: "Deep Impact", primary: c(0.24, 0.41, 0.86), secondary: c(0.95, 0.80, 0.24) },
    // White, blue chevron hoop, pink sleeves (Tokai Teio / Uchimura).
    Theme { name: "Tokai Teio", primary: c(0.34, 0.62, 0.90), secondary: c(0.96, 0.58, 0.74) },
    // White, green hoop, green sleeves (Mejiro Shoji).
    Theme { name: "Mejiro McQueen", primary: c(0.22, 0.64, 0.40), secondary: c(0.86, 0.92, 0.86) },
    // Solid red silks (Gold Ship); silver nods to his ashige grey coat.
    Theme { name: "Gold Ship", primary: c(0.86, 0.27, 0.29), secondary: c(0.75, 0.79, 0.83) },
    // Yellow, sky-blue sash (Vodka / Tanimizu).
    Theme { name: "Vodka", primary: c(0.95, 0.78, 0.24), secondary: c(0.36, 0.70, 0.92) },
    // Blue, white hoop + sleeves (Daiwa Scarlet / Oshiro); scarlet nods to the name.
    Theme { name: "Daiwa Scarlet", primary: c(0.24, 0.42, 0.82), secondary: c(0.90, 0.31, 0.35) },
    // Purple, white sawtooth (Special Week / Usuda).
    Theme { name: "Special Week", primary: c(0.52, 0.36, 0.74), secondary: c(0.90, 0.90, 0.94) },
    // Green, green hoop, yellow sleeves (Silence Suzuka / Nagai).
    Theme { name: "Silence Suzuka", primary: c(0.20, 0.60, 0.34), secondary: c(0.96, 0.84, 0.30) },
    // Blue, yellow diamonds, red sleeves (Oguri Cap / Kondo).
    Theme { name: "Oguri Cap", primary: c(0.22, 0.44, 0.84), secondary: c(0.88, 0.32, 0.30) },
    // Pink, green hoop, yellow-striped sleeves (T.M. Opera O / Takezono).
    Theme { name: "T.M. Opera O", primary: c(0.92, 0.46, 0.64), secondary: c(0.32, 0.64, 0.42) },
    // Black body, brown triple hoops (Kitasan Black / Ono Shoji) — warm brown/gold.
    Theme { name: "Kitasan Black", primary: c(0.68, 0.47, 0.28), secondary: c(0.86, 0.69, 0.37) },
];

pub fn count() -> usize {
    THEMES.len()
}
pub fn name_at(i: usize) -> &'static str {
    THEMES[i.min(THEMES.len() - 1)].name
}

/// Index of the currently active theme (into THEMES). Loaded by an atomic so any
/// thread can read it during a draw without locking.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

pub fn active_index() -> usize {
    ACTIVE.load(Ordering::Relaxed).min(THEMES.len() - 1)
}
pub fn set_active_index(i: usize) {
    ACTIVE.store(i.min(THEMES.len() - 1), Ordering::Relaxed);
}
pub fn active_name() -> &'static str {
    THEMES[active_index()].name
}

fn theme() -> &'static Theme {
    &THEMES[active_index()]
}

fn lighten(mut col: [f32; 4], amt: f32) -> [f32; 4] {
    col[0] = (col[0] + amt).min(1.0);
    col[1] = (col[1] + amt).min(1.0);
    col[2] = (col[2] + amt).min(1.0);
    col
}

// ── accent getters used across the overlay chrome ──────────────────────────────

/// Main accent (section titles, toggle-ON, borders, nav badge).
pub fn accent() -> [f32; 4] {
    theme().primary
}
/// Brighter accent (hover, rings, highlights).
pub fn accent_hi() -> [f32; 4] {
    lighten(theme().primary, 0.16)
}
/// Secondary accent (slider fill end, small two-tone touches).
pub fn secondary() -> [f32; 4] {
    theme().secondary
}
/// Primary at a custom alpha (washes, borders, glows-turned-flat).
pub fn accent_a(a: f32) -> [f32; 4] {
    let p = theme().primary;
    [p[0], p[1], p[2], a]
}
/// Brighter accent at a custom alpha.
pub fn accent_hi_a(a: f32) -> [f32; 4] {
    let p = lighten(theme().primary, 0.16);
    [p[0], p[1], p[2], a]
}
/// Secondary at a custom alpha.
pub fn secondary_a(a: f32) -> [f32; 4] {
    let s = theme().secondary;
    [s[0], s[1], s[2], a]
}
/// Dark ink that reads on top of a bright accent plate (selected nav glyph, primary btn text).
pub fn on_accent() -> [f32; 4] {
    [0.05, 0.09, 0.09, 1.0]
}
/// Slider fill gradient: primary → secondary (a silk two-tone).
pub fn grad_l() -> [f32; 4] {
    theme().primary
}
pub fn grad_r() -> [f32; 4] {
    theme().secondary
}

// ── mode + activation ──────────────────────────────────────────────────────────

/// Cheap time-seeded PRNG (no dep): xorshift over the process clock. Only used to pick
/// a random theme on menu-open, so quality doesn't matter.
fn roll(n: usize) -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        .wrapping_add(0x9E3779B97F4A7C15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x as usize) % n.max(1)
}

/// Apply the persisted preference: fixed theme, or a fresh random roll. Called once at
/// startup and again each time the menu opens (so "randomize on open" re-rolls).
pub fn apply_from_settings() {
    if crate::settings::theme_random() {
        set_active_index(roll(THEMES.len()));
    } else {
        set_active_index(crate::settings::theme_index());
    }
}

/// Called when the menu transitions closed → open.
pub fn on_menu_opened() {
    if crate::settings::theme_random() {
        set_active_index(roll(THEMES.len()));
    }
}
