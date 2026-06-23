//! Heaven Plan B — persistent overlay settings.
//!
//! With no Python host, the DLL persists its own UI/toggle state to a small JSON
//! file. Loaded once at boot (after modules install) and re-saved whenever the
//! user changes a control in the overlay — so your settings stick across sessions.
//!
//! Defaults (first run): Training + Events skip ON, Race-result OFF, FPS Off,
//! rail docked to the right edge.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::{fps, htt, skip};

// True once apply_on_boot has applied the persisted state. Until then, save_current() must
// NOT run: the live fps/ui_tempo modules still hold pre-apply defaults (0 / 1.0), so a menu
// interaction during the ~5-8s boot window would otherwise overwrite the saved file with
// those defaults (the "my FPS/speed don't persist" bug).
static APPLIED: AtomicBool = AtomicBool::new(false);

fn settings_path() -> std::path::PathBuf {
    crate::paths::local_file("heaven-settings.json")
}

fn slog(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub skip_training: bool,
    pub skip_events: bool,
    #[serde(default = "default_true")]
    pub skip_shop: bool,
    #[serde(default = "default_true")]
    pub skip_rival: bool,
    pub race_result: bool,
    pub fps: i32,
    #[serde(default = "default_ui_tempo")]
    pub ui_tempo: f32,
    pub rail_right: bool,
    pub energy_x: f32,
    pub energy_y: f32,
    pub bonds_only: bool,
    pub tt_capture: bool,
    // Persisted UI toggles. Default OFF.
    #[serde(default)]
    pub show_career: bool,
    #[serde(default)]
    pub show_race: bool,
    #[serde(default)]
    pub show_energy: bool,
    // Menu layout: true = centered floating window (default), false = docked to a screen edge.
    #[serde(default = "default_true")]
    pub menu_centered: bool,
    // Index into the overlay's menu-key list (which key toggles the menu). 0 = Insert.
    #[serde(default)]
    pub toggle_key: u32,
    // Whether the first-launch "press <key> to open" hint has been seen/dismissed.
    #[serde(default)]
    pub seen_hint: bool,
    // Uncap the character cloth/hair (spring-bone) physics update rate (cosmetic).
    #[serde(default)]
    pub cyspring_uncap: bool,
    // Force the highest 3D model quality tier (cosmetic).
    #[serde(default)]
    pub gfx_quality: bool,
    // Enhance textures (anisotropic) + LOD + shadow resolution (cosmetic).
    #[serde(default)]
    pub gfx_extras: bool,
    // Display / window QoL.
    #[serde(default)]
    pub always_on_top: bool,
    #[serde(default)]
    pub block_minimize: bool,
    #[serde(default)]
    pub display_mode: i32, // 0 off, 1 borderless, 2 exclusive, 3 windowed
    #[serde(default = "default_one_f32")]
    pub render_scale: f32,
    #[serde(default = "default_one_f32")]
    pub ui_scale: f32,
    // Low-resources "potato" master mode: minimum everything for very weak PCs.
    #[serde(default)]
    pub low_spec: bool,
    // Use the classic "Controls" rail menu instead of the premium sidebar menu.
    #[serde(default)]
    pub classic_menu: bool,
    // Persisted overlay setting; the field is
    // always present so the JSON stays stable across builds.
    #[serde(default)]
    pub oracle: bool,
    // Race freecam enabled.
    #[serde(default)]
    pub freecam: bool,
    // Export each race to JSON on disk (grouped by race type) for the web viewer.
    #[serde(default)]
    pub race_export: bool,
    // Export trained "veteran" umas to heaven_umas/veterans.json (Hakuraku format).
    #[serde(default)]
    pub umas_export: bool,
    // Freecam 3rd-person camera presets PER CIRCUIT (track id → named presets + which is default).
    // Captured/cycled in-race; renamed/managed in the overlay. Persisted forever.
    #[serde(default)]
    pub cam_tracks: std::collections::HashMap<i32, TrackCams>,
    // Per-window saved geometry (key → [x, y, w, h]) so a window the user resizes/moves reopens
    // at that size/position forever.
    #[serde(default)]
    pub win: std::collections::HashMap<String, [f32; 4]>,
}

/// A named 3rd-person chase pose.
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct CamPreset {
    pub name: String,
    pub dist: f32,
    pub yaw: f32,
    pub pitch: f32,
    pub eyeh: f32,
}

/// A circuit's camera presets + which one is the default (index into `presets`).
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct TrackCams {
    pub presets: Vec<CamPreset>,
    pub default_idx: u32,
}

/// Max presets per circuit.
pub const MAX_PRESETS: usize = 4;

fn default_one_f32() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            skip_training: true,
            skip_events: true,
            skip_shop: true,
            skip_rival: true,
            // Track the compile-time default: builds with `races_on` (public) default
            // race-result skip ON; otherwise persisted state would force it OFF on a
            // fresh install. Private has no `races_on`, so this stays false there.
            race_result: cfg!(feature = "races_on"),
            fps: 0,
            ui_tempo: 1.0,
            rail_right: true,
            energy_x: 60.0,
            energy_y: 60.0,
            bonds_only: false,
            tt_capture: false,
            show_career: false,
            show_race: false,
            show_energy: false,
            menu_centered: true,
            toggle_key: 0,
            seen_hint: false,
            cyspring_uncap: false,
            gfx_quality: false,
            gfx_extras: false,
            always_on_top: false,
            block_minimize: false,
            display_mode: 0,
            render_scale: 1.0,
            ui_scale: 1.0,
            low_spec: false,
            classic_menu: false,
            oracle: false,
            freecam: false,
            cam_tracks: std::collections::HashMap::new(),
            win: std::collections::HashMap::new(),
            race_export: false,
            umas_export: false,
        }
    }
}

fn default_ui_tempo() -> f32 { 1.0 }

fn cache() -> &'static Mutex<Settings> {
    static CACHE: OnceLock<Mutex<Settings>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(load_file()))
}

fn load_file() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_file(s: &Settings) {
    if let Ok(j) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(settings_path(), j);
    }
}

/// Apply persisted settings at startup. Call after all modules are installed.
pub fn apply_on_boot() {
    let s = load_file();
    skip::set_train_enabled(s.skip_training);
    skip::set_event_enabled(s.skip_events);
    skip::set_shop_enabled(s.skip_shop);
    skip::set_rival_enabled(s.skip_rival);
    skip::set_race_result_enabled(s.race_result);
    fps::set_cap(s.fps);
    crate::ui_tempo::set_tempo(s.ui_tempo);
    crate::cyspring::set_enabled(s.cyspring_uncap);
    crate::graphics::set_quality_unlocked(s.gfx_quality);
    crate::graphics::set_extras_enabled(s.gfx_extras);
    crate::display::set_block_minimize(s.block_minimize);
    crate::display::set_display_mode(s.display_mode);
    crate::display::set_render_scale(s.render_scale);
    crate::display::set_ui_scale(s.ui_scale);
    crate::display::set_always_on_top(s.always_on_top);
    crate::graphics::set_low_spec(s.low_spec);
    crate::cyspring::set_low_spec(s.low_spec);
    crate::display::set_low_spec(s.low_spec);
    htt::set_enabled(s.tt_capture);
    #[cfg(feature = "freecam")]
    crate::freecam::set_enabled(s.freecam);
    #[cfg(feature = "raceread")]
    crate::race_export::set_enabled(s.race_export);
    crate::umas::set_enabled(s.umas_export);
    if let Ok(mut c) = cache().lock() {
        *c = s;
    }
    APPLIED.store(true, Ordering::Relaxed);
}

/// Team Trials in-process capture toggle (WinHTTP tap).
pub fn tt_capture() -> bool {
    cache().lock().map(|c| c.tt_capture).unwrap_or(false)
}
pub fn set_tt_capture(on: bool) {
    htt::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.tt_capture = on;
        write_file(&c);
    }
}

/// Persist current module state (Training/Events/Races/FPS), preserving the
/// rail side. Call from the overlay whenever one of those controls changes.
pub fn save_current() {
    // Guard against the boot-window clobber: before apply_on_boot has run, the live fps/
    // ui_tempo modules hold defaults, and writing them would wipe the saved values.
    if !APPLIED.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut c) = cache().lock() {
        c.skip_training = skip::is_train_enabled();
        c.skip_events = skip::is_event_enabled();
        c.skip_shop = skip::is_shop_enabled();
        c.skip_rival = skip::is_rival_enabled();
        c.race_result = skip::is_race_result_enabled();
        c.fps = fps::current();
        c.ui_tempo = crate::ui_tempo::tempo();
        write_file(&c);
    }
}

/// Which edge the rail is docked to (true = right, false = left).
pub fn rail_right() -> bool {
    cache().lock().map(|c| c.rail_right).unwrap_or(true)
}

/// Flip / set the rail side and persist it.
pub fn set_rail_right(right: bool) {
    if let Ok(mut c) = cache().lock() {
        c.rail_right = right;
        write_file(&c);
    }
}

/// Menu layout: true = centered floating window, false = docked to an edge.
pub fn menu_centered() -> bool {
    cache().lock().map(|c| c.menu_centered).unwrap_or(true)
}

pub fn set_menu_centered(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.menu_centered = on;
        write_file(&c);
    }
}

/// Index of the key that opens/closes the overlay menu (see overlay's MENU_KEYS).
pub fn toggle_key() -> u32 {
    cache().lock().map(|c| c.toggle_key).unwrap_or(0)
}

pub fn set_toggle_key(idx: u32) {
    if let Ok(mut c) = cache().lock() {
        c.toggle_key = idx;
        write_file(&c);
    }
}

/// Whether the first-launch "press <key> to open the menu" hint has been seen.
pub fn seen_hint() -> bool {
    cache().lock().map(|c| c.seen_hint).unwrap_or(false)
}
pub fn set_seen_hint(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.seen_hint = on;
        write_file(&c);
    }
}

/// Whether the cloth/hair (spring-bone) physics update rate is uncapped.
pub fn cyspring_uncap() -> bool {
    cache().lock().map(|c| c.cyspring_uncap).unwrap_or(false)
}

pub fn set_cyspring_uncap(on: bool) {
    crate::cyspring::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.cyspring_uncap = on;
        write_file(&c);
    }
}

/// Force the highest 3D model quality tier.
pub fn gfx_quality() -> bool {
    cache().lock().map(|c| c.gfx_quality).unwrap_or(false)
}

pub fn set_gfx_quality(on: bool) {
    crate::graphics::set_quality_unlocked(on);
    if let Ok(mut c) = cache().lock() {
        c.gfx_quality = on;
        write_file(&c);
    }
}

/// Enhanced textures (anisotropic) + LOD + shadow resolution.
pub fn gfx_extras() -> bool {
    cache().lock().map(|c| c.gfx_extras).unwrap_or(false)
}

pub fn set_gfx_extras(on: bool) {
    crate::graphics::set_extras_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.gfx_extras = on;
        write_file(&c);
    }
}

// ── Display / window QoL ──
pub fn always_on_top() -> bool {
    cache().lock().map(|c| c.always_on_top).unwrap_or(false)
}
pub fn set_always_on_top(on: bool) {
    crate::display::set_always_on_top(on);
    if let Ok(mut c) = cache().lock() {
        c.always_on_top = on;
        write_file(&c);
    }
}

pub fn block_minimize() -> bool {
    cache().lock().map(|c| c.block_minimize).unwrap_or(false)
}
pub fn set_block_minimize(on: bool) {
    crate::display::set_block_minimize(on);
    if let Ok(mut c) = cache().lock() {
        c.block_minimize = on;
        write_file(&c);
    }
}

pub fn display_mode() -> i32 {
    cache().lock().map(|c| c.display_mode).unwrap_or(0)
}
pub fn set_display_mode(m: i32) {
    crate::display::set_display_mode(m);
    if let Ok(mut c) = cache().lock() {
        c.display_mode = m;
        write_file(&c);
    }
}

pub fn render_scale() -> f32 {
    cache().lock().map(|c| c.render_scale).unwrap_or(1.0)
}
pub fn set_render_scale(s: f32) {
    crate::display::set_render_scale(s);
    if let Ok(mut c) = cache().lock() {
        c.render_scale = s;
        write_file(&c);
    }
}

pub fn ui_scale() -> f32 {
    cache().lock().map(|c| c.ui_scale).unwrap_or(1.0)
}
pub fn set_ui_scale(s: f32) {
    crate::display::set_ui_scale(s);
    if let Ok(mut c) = cache().lock() {
        c.ui_scale = s;
        write_file(&c);
    }
}

/// Low-resources "potato" master mode (overrides the graphics enhancements).
pub fn low_spec() -> bool {
    cache().lock().map(|c| c.low_spec).unwrap_or(false)
}
pub fn set_low_spec(on: bool) {
    crate::graphics::set_low_spec(on);
    crate::cyspring::set_low_spec(on);
    crate::display::set_low_spec(on);
    if let Ok(mut c) = cache().lock() {
        c.low_spec = on;
        write_file(&c);
    }
}

/// Use the classic "Controls" rail menu instead of the premium sidebar menu.
pub fn classic_menu() -> bool {
    cache().lock().map(|c| c.classic_menu).unwrap_or(false)
}
pub fn set_classic_menu(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.classic_menu = on;
        write_file(&c);
    }
}

/// "Bonds only" view mode for the info panel (hide stats/aptitudes).
pub fn bonds_only() -> bool {
    cache().lock().map(|c| c.bonds_only).unwrap_or(false)
}
pub fn set_bonds_only(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.bonds_only = on;
        write_file(&c);
    }
}

/// Whether the info panel window is shown at all.
pub fn show_career() -> bool {
    cache().lock().map(|c| c.show_career).unwrap_or(false)
}
pub fn set_show_career(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.show_career = on;
        write_file(&c);
    }
}

/// Whether the live Race panel is shown (it still self-hides when no race is active).
pub fn show_race() -> bool {
    cache().lock().map(|c| c.show_race).unwrap_or(false)
}
pub fn set_show_race(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.show_race = on;
        write_file(&c);
    }
}

/// Whether the floating info chip is shown.
pub fn show_energy() -> bool {
    cache().lock().map(|c| c.show_energy).unwrap_or(false)
}
pub fn set_show_energy(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.show_energy = on;
        write_file(&c);
    }
}

/// Persisted overlay setting.
pub fn oracle() -> bool {
    cache().lock().map(|c| c.oracle).unwrap_or(false)
}
pub fn set_oracle(on: bool) {
    if let Ok(mut c) = cache().lock() {
        c.oracle = on;
        write_file(&c);
    }
}

/// Race freecam enabled + persisted. Module call is freecam-build only.
pub fn freecam() -> bool {
    cache().lock().map(|c| c.freecam).unwrap_or(false)
}
pub fn set_freecam(on: bool) {
    #[cfg(feature = "freecam")]
    crate::freecam::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.freecam = on;
        write_file(&c);
    }
}

/// Export each race to JSON on disk (grouped by race type) for the web viewer.
pub fn race_export() -> bool {
    cache().lock().map(|c| c.race_export).unwrap_or(false)
}
pub fn set_race_export(on: bool) {
    #[cfg(feature = "raceread")]
    crate::race_export::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.race_export = on;
        write_file(&c);
    }
}

/// Export trained veteran umas to heaven_umas/veterans.json (Hakuraku format).
pub fn umas_export() -> bool {
    cache().lock().map(|c| c.umas_export).unwrap_or(false)
}
pub fn set_umas_export(on: bool) {
    crate::umas::set_enabled(on);
    if let Ok(mut c) = cache().lock() {
        c.umas_export = on;
        write_file(&c);
    }
}

/// A circuit's camera presets (clone). Empty if none saved.
pub fn cam_presets(track: i32) -> Vec<CamPreset> {
    cache().lock().ok().and_then(|c| c.cam_tracks.get(&track).map(|t| t.presets.clone())).unwrap_or_default()
}

/// The circuit's default preset index (clamped to a valid preset, or 0).
pub fn cam_default_idx(track: i32) -> usize {
    cache()
        .lock()
        .ok()
        .and_then(|c| c.cam_tracks.get(&track).map(|t| (t.default_idx as usize).min(t.presets.len().saturating_sub(1))))
        .unwrap_or(0)
}

/// The pose of the circuit's default preset: Some((dist,yaw,pitch,eyeH)) if any preset exists.
pub fn cam_default_pose(track: i32) -> Option<(f32, f32, f32, f32)> {
    cache().lock().ok().and_then(|c| {
        c.cam_tracks.get(&track).and_then(|t| {
            t.presets.get((t.default_idx as usize).min(t.presets.len().saturating_sub(1)))
                .map(|p| (p.dist, p.yaw, p.pitch, p.eyeh))
        })
    })
}

/// Add a new preset to a circuit (capped at MAX_PRESETS). Returns its index, or None if full.
pub fn cam_add_preset(track: i32, name: &str, dist: f32, yaw: f32, pitch: f32, eyeh: f32) -> Option<usize> {
    if let Ok(mut c) = cache().lock() {
        let t = c.cam_tracks.entry(track).or_default();
        if t.presets.len() >= MAX_PRESETS {
            return None;
        }
        t.presets.push(CamPreset { name: name.to_string(), dist, yaw, pitch, eyeh });
        let idx = t.presets.len() - 1;
        write_file(&c);
        return Some(idx);
    }
    None
}

/// Overwrite an existing preset's pose (keeps its name).
pub fn cam_update_preset(track: i32, idx: usize, dist: f32, yaw: f32, pitch: f32, eyeh: f32) {
    if let Ok(mut c) = cache().lock() {
        if let Some(p) = c.cam_tracks.get_mut(&track).and_then(|t| t.presets.get_mut(idx)) {
            p.dist = dist; p.yaw = yaw; p.pitch = pitch; p.eyeh = eyeh;
            write_file(&c);
        }
    }
}

pub fn cam_rename_preset(track: i32, idx: usize, name: &str) {
    if let Ok(mut c) = cache().lock() {
        if let Some(p) = c.cam_tracks.get_mut(&track).and_then(|t| t.presets.get_mut(idx)) {
            p.name = name.to_string();
            write_file(&c);
        }
    }
}

pub fn cam_delete_preset(track: i32, idx: usize) {
    if let Ok(mut c) = cache().lock() {
        if let Some(t) = c.cam_tracks.get_mut(&track) {
            if idx < t.presets.len() {
                t.presets.remove(idx);
                if t.default_idx as usize >= t.presets.len() {
                    t.default_idx = 0;
                }
                write_file(&c);
            }
        }
    }
}

pub fn cam_set_default(track: i32, idx: usize) {
    if let Ok(mut c) = cache().lock() {
        if let Some(t) = c.cam_tracks.get_mut(&track) {
            if idx < t.presets.len() {
                t.default_idx = idx as u32;
                write_file(&c);
            }
        }
    }
}

/// Saved geometry [x,y,w,h] for a window key, if the user has moved/resized it.
pub fn win_rect(key: &str) -> Option<[f32; 4]> {
    cache().lock().ok().and_then(|c| c.win.get(key).copied())
}
/// Persist a window's geometry (only writes when it actually changed, to limit disk writes).
pub fn set_win_rect(key: &str, rect: [f32; 4]) {
    if let Ok(mut c) = cache().lock() {
        let changed = c.win.get(key).map(|r| {
            (r[0] - rect[0]).abs() > 1.0 || (r[1] - rect[1]).abs() > 1.0
                || (r[2] - rect[2]).abs() > 1.0 || (r[3] - rect[3]).abs() > 1.0
        }).unwrap_or(true);
        if changed {
            c.win.insert(key.to_string(), rect);
            write_file(&c);
        }
    }
}

/// Saved screen position of the floating info chip.
pub fn energy_pos() -> (f32, f32) {
    cache().lock().map(|c| (c.energy_x, c.energy_y)).unwrap_or((60.0, 60.0))
}

/// Persist the info chip position (called when the user finishes dragging it).
pub fn set_energy_pos(x: f32, y: f32) {
    if let Ok(mut c) = cache().lock() {
        c.energy_x = x;
        c.energy_y = y;
        write_file(&c);
    }
}
