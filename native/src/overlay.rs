//! Heaven internal overlay — imgui render loop drawn inside the game's D3D11
//! swapchain via hudhook.
//!
//! Visual design: "Command Rail" (Heaven HUD design direction, amber accent).
//! The panels dock to one screen edge as a rail and can flip left/right; each
//! is a draggable/resizable imgui window styled as dark translucent telemetry
//! glass. Data contract is shared with the external HUD (data.rs); toggles flip
//! the native modules directly (no IPC round-trip).

use hudhook::imgui::{
    self, Condition, Context, FontConfig, FontSource, StyleColor, StyleVar, Ui,
};
use hudhook::{ImguiRenderLoop, TextureLoader};

use std::time::Instant;

// Career/Race data types are only used by the info panels (feature `panels`).
use crate::ipc;

// ── Heaven "Umamusume" palette (RGBA 0..1) — purple/pink theme matched to the mockup ──
const ACCENT: [f32; 4] = [0.769, 0.416, 1.0, 1.0]; // lavender-purple (section titles, checks)
const ACCENT_HI: [f32; 4] = [0.88, 0.64, 1.0, 1.0];
const PINK: [f32; 4] = [1.0, 0.361, 0.796, 1.0]; // toggle/slider pink
const TEXT: [f32; 4] = [0.87, 0.83, 0.93, 1.0];
const DIM: [f32; 4] = [0.56, 0.50, 0.66, 1.0]; // muted lavender-gray
const GOLD: [f32; 4] = [0.843, 0.694, 0.365, 1.0];
const GOOD: [f32; 4] = [0.55, 0.85, 0.66, 1.0];
const WARN: [f32; 4] = [1.0, 0.72, 0.42, 1.0];
const BAD: [f32; 4] = [1.0, 0.42, 0.55, 1.0];
const BLUE: [f32; 4] = [0.55, 0.70, 1.0, 1.0];

const PANEL_BG: [f32; 4] = [0.090, 0.051, 0.169, 0.985]; // page behind the cards
const TITLE_BG: [f32; 4] = [0.10, 0.07, 0.17, 1.0];
const TITLE_BG_ON: [f32; 4] = [0.20, 0.10, 0.26, 1.0];
const BORDER: [f32; 4] = [0.40, 0.30, 0.56, 0.45];
const FRAME_BG: [f32; 4] = [0.20, 0.15, 0.32, 1.0];
const FRAME_HI: [f32; 4] = [0.26, 0.19, 0.40, 1.0];
const BTN_BG: [f32; 4] = [0.23, 0.17, 0.36, 1.0];
const BTN_HI: [f32; 4] = [0.31, 0.23, 0.46, 1.0];
const AMBER_SOFT: [f32; 4] = [0.769, 0.416, 1.0, 0.22]; // accent soft (name kept for reuse)
const AMBER_MED: [f32; 4] = [0.769, 0.416, 1.0, 0.42];

// Menu chrome.
const SIDEBAR_BG: [f32; 4] = [0.043, 0.024, 0.086, 1.0]; // sidebar column
const CARD_BG: [f32; 4] = [0.175, 0.105, 0.32, 0.97]; // section card (lighter, floats on page)
const CARD_BORDER: [f32; 4] = [0.52, 0.42, 0.72, 0.28]; // subtle card outline
const BADGE_BG: [f32; 4] = [0.769, 0.416, 1.0, 0.20]; // rounded square behind a section icon
const SEL_BG: [f32; 4] = [0.56, 0.42, 0.90, 0.42]; // selected sidebar pill (soft lavender)
const TRACK_BG: [f32; 4] = [0.24, 0.19, 0.36, 1.0]; // slider track
const GRAD_L: [f32; 4] = [1.0, 0.36, 0.80, 1.0]; // slider fill left (pink)
const GRAD_R: [f32; 4] = [0.77, 0.42, 1.0, 1.0]; // slider fill right (purple)
const SIDEBAR_W: f32 = 176.0;
const MENU_W: f32 = 720.0;
const MENU_H: f32 = 720.0;

// Bundled fonts (SIL OFL): Inter for body/UI, Orbitron for section titles. Premium-launcher look.
// Per the design kit: body = Inter Medium, section titles = Cinzel SemiBold, numbers = Orbitron Medium.
const INTER_TTF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Inter-Medium.ttf"));
const INTER_SB_TTF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Inter-SemiBold.ttf"));
const CINZEL_TTF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Cinzel-SemiBold.ttf"));
const ORBITRON_TTF: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Orbitron-Medium.ttf"));

// Keys the user can bind to open/close the menu (settings.toggle_key indexes this).
// The FIRST 14 keep their order/index for backward-compat with saved binds; the rest are
// appended (imgui-rs 0.11 exposes no A-Z letter keys, so those can't be bound here).
const MENU_KEYS: [(&str, imgui::Key); 49] = [
    ("Insert", imgui::Key::Insert),
    ("Home", imgui::Key::Home),
    ("End", imgui::Key::End),
    ("Delete", imgui::Key::Delete),
    ("Page Up", imgui::Key::PageUp),
    ("Page Down", imgui::Key::PageDown),
    ("F1", imgui::Key::F1),
    ("F2", imgui::Key::F2),
    ("F3", imgui::Key::F3),
    ("F4", imgui::Key::F4),
    ("F5", imgui::Key::F5),
    ("F6", imgui::Key::F6),
    ("F7", imgui::Key::F7),
    ("F8", imgui::Key::F8),
    ("F9", imgui::Key::F9),
    ("F10", imgui::Key::F10),
    ("F11", imgui::Key::F11),
    ("F12", imgui::Key::F12),
    ("Tab", imgui::Key::Tab),
    ("Space", imgui::Key::Space),
    ("Backspace", imgui::Key::Backspace),
    ("Enter", imgui::Key::Enter),
    ("Up", imgui::Key::UpArrow),
    ("Down", imgui::Key::DownArrow),
    ("Left", imgui::Key::LeftArrow),
    ("Right", imgui::Key::RightArrow),
    ("0", imgui::Key::Alpha0),
    ("1", imgui::Key::Alpha1),
    ("2", imgui::Key::Alpha2),
    ("3", imgui::Key::Alpha3),
    ("4", imgui::Key::Alpha4),
    ("5", imgui::Key::Alpha5),
    ("6", imgui::Key::Alpha6),
    ("7", imgui::Key::Alpha7),
    ("8", imgui::Key::Alpha8),
    ("9", imgui::Key::Alpha9),
    ("Num 0", imgui::Key::Keypad0),
    ("Num 1", imgui::Key::Keypad1),
    ("Num 2", imgui::Key::Keypad2),
    ("Num 3", imgui::Key::Keypad3),
    ("Num 4", imgui::Key::Keypad4),
    ("Num 5", imgui::Key::Keypad5),
    ("Num 6", imgui::Key::Keypad6),
    ("Num 7", imgui::Key::Keypad7),
    ("Num 8", imgui::Key::Keypad8),
    ("Num 9", imgui::Key::Keypad9),
    ("Scroll Lock", imgui::Key::ScrollLock),
    ("Pause", imgui::Key::Pause),
    ("`", imgui::Key::GraveAccent),
];

fn menu_key_idx() -> usize {
    (crate::settings::toggle_key() as usize).min(MENU_KEYS.len() - 1)
}

// True while the user is rebinding the menu key (clicked the bind button, waiting for a press).
static MENU_KEY_BINDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// Set right after a bind: swallows the menu toggle while the just-pressed key is still held, so
// pressing the new key to bind it doesn't ALSO toggle (close) the menu. Cleared on key release.
static SUPPRESS_TOGGLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The "open/close menu key" control as a CLICK-TO-BIND button — shared by BOTH menus (premium
/// + classic). Click it, then press a key and that key becomes the menu hotkey; press Esc to
/// cancel. `premium` selects the styled `btn` vs the plain classic button.
fn menu_key_button(ui: &Ui, premium: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if MENU_KEY_BINDING.load(Relaxed) {
        // Listening: the first known key pressed becomes the bind (Esc cancels).
        if ui.is_key_pressed_no_repeat(imgui::Key::Escape) {
            MENU_KEY_BINDING.store(false, Relaxed);
        } else {
            for (i, (_, k)) in MENU_KEYS.iter().enumerate() {
                if ui.is_key_pressed_no_repeat(*k) {
                    crate::settings::set_toggle_key(i as u32);
                    MENU_KEY_BINDING.store(false, Relaxed);
                    SUPPRESS_TOGGLE.store(true, Relaxed); // don't let this same held press toggle the menu
                    break;
                }
            }
        }
    }
    let label: &str = if MENU_KEY_BINDING.load(Relaxed) {
        "Press a key..."
    } else {
        MENU_KEYS[menu_key_idx()].0
    };
    let clicked = if premium {
        btn(ui, "##menukey", label)
    } else {
        ui.button(format!("{label}##menukeyc"))
    };
    if clicked {
        let b = MENU_KEY_BINDING.load(Relaxed);
        MENU_KEY_BINDING.store(!b, Relaxed);
    }
}

const STATS: [(&str, &str); 5] = [
    ("SPD", "speed"), ("STA", "stamina"), ("POW", "power"),
    ("GUT", "guts"), ("WIZ", "wiz"),
];

// The icon font id (Segoe MDL2). `FontId` wraps a raw `*const Font` so it isn't Send/Sync
// and can't live in the (Send+Sync) overlay struct — but initialize() and draw_menu() both
// run on the render thread, so a thread-local holds it safely.
thread_local! {
    static ICON_FONT: std::cell::Cell<Option<imgui::FontId>> = const { std::cell::Cell::new(None) };
    // Orbitron face for section titles (the "premium launcher" look).
    static TITLE_FONT: std::cell::Cell<Option<imgui::FontId>> = const { std::cell::Cell::new(None) };
    // Inter SemiBold for emphasised values (FPS, speed, ON/OFF).
    static VALUE_FONT: std::cell::Cell<Option<imgui::FontId>> = const { std::cell::Cell::new(None) };
    // Inter SemiBold for sidebar nav labels (slightly heavier than the Medium body font).
    static NAV_FONT: std::cell::Cell<Option<imgui::FontId>> = const { std::cell::Cell::new(None) };
    // Sidebar nav icons (image textures, indexed by `nav_icon_idx`). TextureId is Copy, so the
    // whole array lives in a Cell. Populated in initialize() (banner builds); None elsewhere →
    // falls back to the Segoe MDL2 glyph.
    static NAV_TEX: std::cell::Cell<[Option<imgui::TextureId>; 8]> = const { std::cell::Cell::new([None; 8]) };
    static DIVIDER_TEX: std::cell::Cell<Option<imgui::TextureId>> = const { std::cell::Cell::new(None) };
    static SPARK_TEX: std::cell::Cell<[Option<imgui::TextureId>; 3]> = const { std::cell::Cell::new([None; 3]) };
    static ORB_TEX: std::cell::Cell<Option<imgui::TextureId>> = const { std::cell::Cell::new(None) };
    // Tileable window background + falling sakura petals.
    static BG_TEX: std::cell::Cell<Option<imgui::TextureId>> = const { std::cell::Cell::new(None) };
    static PETAL_TEX: std::cell::Cell<[Option<imgui::TextureId>; 3]> = const { std::cell::Cell::new([None; 3]) };
    // Game icons extracted to <dll dir>\heaven-icons\ (skill icon_id → tex, uma charaId → tex)
    // + skill_id → icon_id map. Loaded once in initialize(). Empty if the folder isn't present.
    static SKILL_TEX: std::cell::RefCell<std::collections::HashMap<i32, imgui::TextureId>> = std::cell::RefCell::new(std::collections::HashMap::new());
    static UMA_TEX: std::cell::RefCell<std::collections::HashMap<i32, imgui::TextureId>> = std::cell::RefCell::new(std::collections::HashMap::new());
    static SKILL_ICON_MAP: std::cell::RefCell<std::collections::HashMap<i32, i32>> = std::cell::RefCell::new(std::collections::HashMap::new());
    // skill_id → localized description (for the hover tooltip).
    static SKILL_DESC: std::cell::RefCell<std::collections::HashMap<i32, String>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

thread_local! {
    // Per-window "was the left mouse down last frame" — so we persist only on the
    // release EDGE, not every idle frame (which re-locked settings + re-diffed 25+
    // fields every frame the window was open). Mirrors the energy-chip dirty pattern.
    static WIN_DRAG: std::cell::RefCell<std::collections::HashMap<String, bool>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Persist a window's geometry (pos+size) once the user finishes dragging (mouse released), so
/// it reopens at that size/position forever. Call inside the window's build closure.
fn persist_window(ui: &Ui, key: &str) {
    let down = ui.is_mouse_down(imgui::MouseButton::Left);
    let was_down = WIN_DRAG.with(|m| {
        let mut m = m.borrow_mut();
        let prev = *m.get(key).unwrap_or(&false);
        if prev != down {
            m.insert(key.to_string(), down);
        }
        prev
    });
    // Only on the release edge (down → up): set_win_rect itself no-ops when the
    // geometry hasn't moved >1px, so a stray click elsewhere costs nothing.
    if was_down && !down {
        let p = ui.window_pos();
        let s = ui.window_size();
        crate::settings::set_win_rect(key, [p[0], p[1], s[0], s[1]]);
    }
}

thread_local! {
    // Last frame delta (set once per frame) so widget helpers can ease without threading it
    // through every signature.
    static FRAME_DT: std::cell::Cell<f32> = const { std::cell::Cell::new(0.016) };
    // Per-widget animation state (keyed by a stable id) — eased toward a target each frame.
    static ANIM: std::cell::RefCell<std::collections::HashMap<String, f32>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Exponential ease toward `target` (frame-rate independent). `speed` ≈ how fast (10–20 feels
/// like 100–180 ms). Returns the eased value to use this frame.
fn anim_step(key: &str, target: f32, speed: f32) -> f32 {
    let dt = FRAME_DT.with(|c| c.get());
    ANIM.with(|m| {
        let mut m = m.borrow_mut();
        let cur = *m.get(key).unwrap_or(&target);
        let k = 1.0 - (-dt * speed).exp();
        let mut nv = cur + (target - cur) * k;
        if (nv - target).abs() < 0.0015 {
            nv = target;
        }
        m.insert(key.to_string(), nv);
        nv
    })
}

/// Force an animation value (e.g. reset a fade to 0 on a tab switch).
fn anim_set(key: &str, v: f32) {
    ANIM.with(|m| {
        m.borrow_mut().insert(key.to_string(), v);
    });
}

/// Linear blend between two RGBA colours.
fn lerp_col(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

/// Truncate `s` to fit `max_w` px in the current font, appending "…" when cut (measures the
/// real glyph width instead of a fixed char count, so wide names don't overflow their column).
#[allow(dead_code)]
fn ellipsize(ui: &Ui, s: &str, max_w: f32) -> String {
    if ui.calc_text_size(s)[0] <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        let mut probe = out.clone();
        probe.push(ch);
        probe.push('\u{2026}');
        if ui.calc_text_size(&probe)[0] > max_w {
            break;
        }
        out.push(ch);
    }
    out.push('\u{2026}');
    out
}

/// Draw a value in the heavier (SemiBold) font.
fn val(ui: &Ui, col: [f32; 4], text: &str) {
    if let Some(f) = VALUE_FONT.with(|c| c.get()) {
        let _t = ui.push_font(f);
        ui.text_colored(col, text);
    } else {
        ui.text_colored(col, text);
    }
}

pub struct HeavenOverlay {
    show: bool,
    toggle_was_down: bool, // edge-detect the menu key so holding it doesn't toggle repeatedly
    tab: usize, // selected sidebar category
    prev_tab: usize, // last frame's tab — detects switches to trigger the content fade-in
    rail_right: bool,
    relayout: bool,
    fps_on: bool,
    fps_val: i32,
    ui_tempo_val: f32,
    energy_pos: [f32; 2],
    energy_dirty: bool,
    last_frame: Option<Instant>,
    fps_display: f32, // true FPS = frames counted per real-time window (no smoothing)
    fps_frames: u32,  // frames counted in the current window
    fps_window: f32,  // wall-clock seconds accumulated in the current window
    anim_t: f32, // accumulated seconds, drives the falling-petal animation
    frame_dt: f32, // last frame delta (seconds) for UI easing
    #[cfg(feature = "banner")]
    banner_tex: Option<imgui::TextureId>,
    #[cfg(feature = "banner")]
    menu_logo_tex: Option<imgui::TextureId>,
    #[cfg(feature = "banner")]
    crest_tex: Option<imgui::TextureId>,
    #[cfg(feature = "banner")]
    sil_tex: Option<imgui::TextureId>,
    #[cfg(feature = "banner")]
    start_btn_tex: Option<imgui::TextureId>,
    #[cfg(feature = "banner")]
    intro_done: bool,
    #[cfg(feature = "banner")]
    was_title: bool,
    #[cfg(feature = "banner")]
    intro_force: bool,
    // Auto-started the intro once this launch (as soon as the D3D device was captured,
    // independent of the IL2CPP boot) — so the video covers the game's splash logos.
    #[cfg(feature = "banner")]
    intro_auto_started: bool,
}

// Umamusume header banner — baked RGBA (sky + ground + circular character + nameplate).
// Local-only art (Cygames IP); the file lives outside the repo and only the private
// `banner` build embeds it.
// The menu header banner art.
#[cfg(all(feature = "banner", not(feature = "oracle")))]
const BANNER_RGBA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/banner.rgba"));
#[cfg(feature = "banner")]
const BANNER_W: f32 = 960.0;
#[cfg(feature = "banner")]
const BANNER_H: f32 = 384.0;
// "START GAME" skip button (baked RGBA, orange→pink gradient + white outline). Local art.
#[cfg(feature = "banner")]
const START_RGBA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/start_game.rgba"));
#[cfg(feature = "banner")]
const START_W: f32 = 681.0;
#[cfg(feature = "banner")]
const START_H: f32 = 136.0;
// Menu sidebar logo (baked RGBA). Local art.
#[cfg(feature = "banner")]
const LOGO_RGBA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/menu_logo.rgba"));
#[cfg(feature = "banner")]
const LOGO_W: f32 = 600.0;
#[cfg(feature = "banner")]
const LOGO_H: f32 = 181.0;
// Sidebar gold crest emblem + translucent character silhouette (baked RGBA). Local art.
#[cfg(feature = "banner")]
const CREST_RGBA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/crest.rgba"));
#[cfg(feature = "banner")]
const CREST_SZ: f32 = 480.0;
#[cfg(feature = "banner")]
const SIL_RGBA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/sil.rgba"));
#[cfg(feature = "banner")]
const SIL_W: f32 = 230.0;
#[cfg(feature = "banner")]
const SIL_H: f32 = 294.0;
// Sidebar navigation icons (baked RGBA, 64²) — premium crystal/gold launcher icons.
// Order matches `nav_icon_idx`: Skip, Performance, Capture, Intro, Camera, Panels, Updates, About.
#[cfg(feature = "banner")]
const NAV_ICON_RGBA: [&[u8]; 8] = [
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/skip.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/perf.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/capture.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/intro.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/camera.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/panels.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/updates.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icons/about.rgba")),
];
#[cfg(feature = "banner")]
const NAV_ICON_SZ: u32 = 64;
// Elegant gold divider drawn under each section title.
#[cfg(feature = "banner")]
const DIVIDER_RGBA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/divider.rgba"));
#[cfg(feature = "banner")]
const DIVIDER_W: u32 = 256;
#[cfg(feature = "banner")]
const DIVIDER_H: u32 = 32;
// Sparkle particles + floating glow orb for the sidebar (baked RGBA, magenta-tinted).
#[cfg(feature = "banner")]
const SPARK_RGBA: [&[u8]; 3] = [
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/spark_01.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/spark_02.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/spark_03.rgba")),
];
#[cfg(feature = "banner")]
const ORB_RGBA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/glow_orb.rgba"));
#[cfg(feature = "banner")]
const PARTICLE_SZ: u32 = 128;
// Seamless tileable window background (dark purple, horseshoe + constellation motifs).
#[cfg(feature = "banner")]
const BG_RGBA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/background.rgba"));
#[cfg(feature = "banner")]
const BG_SZ: u32 = 768;
// Falling sakura petals (baked RGBA, 96²) for the animated sidebar accent.
#[cfg(feature = "banner")]
const PETAL_RGBA: [&[u8]; 3] = [
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/petal_01.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/petal_02.rgba")),
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/petal_03.rgba")),
];
#[cfg(feature = "banner")]
const PETAL_SZ: u32 = 96;

impl HeavenOverlay {
    pub fn new() -> Self {
        let cur = crate::fps::current();
        let (ex, ey) = crate::settings::energy_pos();
        Self {
            show: true,
            toggle_was_down: false,
            tab: 0,
            prev_tab: 0,
            rail_right: crate::settings::rail_right(),
            relayout: true, // snap to the rail on first frame
            fps_on: cur > 0,
            fps_val: if cur > 0 { cur } else { 60 },
            ui_tempo_val: crate::ui_tempo::tempo(),
            energy_pos: [ex, ey],
            energy_dirty: false,
            last_frame: None,
            fps_display: 0.0,
            fps_frames: 0,
            fps_window: 0.0,
            anim_t: 0.0,
            frame_dt: 0.016,
            #[cfg(feature = "banner")]
            banner_tex: None,
            #[cfg(feature = "banner")]
            menu_logo_tex: None,
            #[cfg(feature = "banner")]
            crest_tex: None,
            #[cfg(feature = "banner")]
            sil_tex: None,
            #[cfg(feature = "banner")]
            start_btn_tex: None,
            // Idle until the device is captured (auto-start) or the Replay button.
            #[cfg(feature = "banner")]
            intro_done: false,
            #[cfg(feature = "banner")]
            intro_auto_started: false,
            #[cfg(feature = "banner")]
            was_title: false,
            #[cfg(feature = "banner")]
            intro_force: false,
        }
    }
}

impl ImguiRenderLoop for HeavenOverlay {
    /// One-time setup: load a clean font (the default imgui bitmap font looks
    /// cheap and lacks our glyphs) and disable imgui.ini so the rail layout is
    /// authoritative every session.
    fn initialize<'a>(&'a mut self, ctx: &mut Context, _loader: TextureLoader<'a>) {
        ctx.set_ini_filename(None);
        // Upload the header banner (raw RGBA) once. Non-fatal: if it fails, we just
        // render without a banner.
        #[cfg(feature = "banner")]
        {
            self.banner_tex = _loader(BANNER_RGBA, BANNER_W as u32, BANNER_H as u32).ok();
            self.menu_logo_tex = _loader(LOGO_RGBA, LOGO_W as u32, LOGO_H as u32).ok();
            self.crest_tex = _loader(CREST_RGBA, CREST_SZ as u32, CREST_SZ as u32).ok();
            self.sil_tex = _loader(SIL_RGBA, SIL_W as u32, SIL_H as u32).ok();
            self.start_btn_tex = _loader(START_RGBA, START_W as u32, START_H as u32).ok();
            // Sidebar nav icons + divider + particles (image-based, replace the font glyphs).
            let mut navs = [None; 8];
            for (i, bytes) in NAV_ICON_RGBA.iter().enumerate() {
                navs[i] = _loader(bytes, NAV_ICON_SZ, NAV_ICON_SZ).ok();
            }
            NAV_TEX.with(|c| c.set(navs));
            DIVIDER_TEX.with(|c| c.set(_loader(DIVIDER_RGBA, DIVIDER_W, DIVIDER_H).ok()));
            let mut sparks = [None; 3];
            for (i, bytes) in SPARK_RGBA.iter().enumerate() {
                sparks[i] = _loader(bytes, PARTICLE_SZ, PARTICLE_SZ).ok();
            }
            SPARK_TEX.with(|c| c.set(sparks));
            ORB_TEX.with(|c| c.set(_loader(ORB_RGBA, PARTICLE_SZ, PARTICLE_SZ).ok()));
            BG_TEX.with(|c| c.set(_loader(BG_RGBA, BG_SZ, BG_SZ).ok()));
            let mut petals = [None; 3];
            for (i, bytes) in PETAL_RGBA.iter().enumerate() {
                petals[i] = _loader(bytes, PETAL_SZ, PETAL_SZ).ok();
            }
            PETAL_TEX.with(|c| c.set(petals));
            // The intro video is no longer pre-loaded as N resident textures (VRAM-
            // bound, ~15 s max). It now streams the whole clip through a single dynamic
            // texture via `intro_player` (drawn with our own D3D11 quad). Nothing to
            // load here — the player reads `intro_full.bin` (next to the DLL) lazily.
        }
        // Game icons (skill / uma) extracted to <dll dir>\heaven-icons\ as raw 64x64 RGBA.
        // Loaded once here via the texture loader (only available in initialize). Absent folder
        // → maps stay empty → the HUD just shows text without icons.
        #[cfg(feature = "freecam")]
        {
            let base = crate::paths::local_file("heaven-icons");
            let mut load_dir = |sub: &str| -> std::collections::HashMap<i32, imgui::TextureId> {
                let mut out = std::collections::HashMap::new();
                if let Ok(rd) = std::fs::read_dir(base.join(sub)) {
                    for e in rd.flatten() {
                        let p = e.path();
                        if p.extension().and_then(|s| s.to_str()) != Some("rgba") {
                            continue;
                        }
                        let id = match p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<i32>().ok()) {
                            Some(i) => i,
                            None => continue,
                        };
                        if let Ok(bytes) = std::fs::read(&p) {
                            if bytes.len() == 64 * 64 * 4 {
                                // The loader ties the data to its lifetime, so leak the bytes
                                // (one-time, ~16KB each) to get a 'static slice.
                                let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
                                if let Ok(t) = _loader(leaked, 64, 64) {
                                    out.insert(id, t);
                                }
                            }
                        }
                    }
                }
                out
            };
            SKILL_TEX.with(|m| *m.borrow_mut() = load_dir("skill"));
            UMA_TEX.with(|m| *m.borrow_mut() = load_dir("uma"));
            if let Ok(txt) = std::fs::read_to_string(base.join("skill_icon_map.csv")) {
                let mut map = std::collections::HashMap::new();
                for line in txt.lines() {
                    if let Some((a, b)) = line.split_once(',') {
                        if let (Ok(s), Ok(i)) = (a.trim().parse::<i32>(), b.trim().parse::<i32>()) {
                            map.insert(s, i);
                        }
                    }
                }
                SKILL_ICON_MAP.with(|m| *m.borrow_mut() = map);
            }
            // skill_id → description (tab-separated: id\ttext).
            if let Ok(txt) = std::fs::read_to_string(base.join("skill_desc.tsv")) {
                let mut map = std::collections::HashMap::new();
                for line in txt.lines() {
                    if let Some((a, d)) = line.split_once('\t') {
                        if let Ok(s) = a.trim().parse::<i32>() {
                            map.insert(s, d.to_string());
                        }
                    }
                }
                SKILL_DESC.with(|m| *m.borrow_mut() = map);
            }
        }
        // Body / UI font = Inter Medium (added FIRST → imgui's default for all text).
        ctx.fonts().add_font(&[FontSource::TtfData {
            data: INTER_TTF,
            size_pixels: 17.0,
            config: Some(FontConfig {
                oversample_h: 3,
                oversample_v: 3,
                rasterizer_multiply: 1.05,
                ..FontConfig::default()
            }),
        }]);
        // Segoe MDL2 Assets — UI icon glyphs (Private Use Area) for section / category icons.
        static ICON_RANGE: [u32; 3] = [0xE700, 0xEAFF, 0]; // covers Info (E946) etc.
        if let Ok(bytes) = std::fs::read(r"C:\Windows\Fonts\segmdl2.ttf") {
            let id = ctx.fonts().add_font(&[FontSource::TtfData {
                data: &bytes,
                size_pixels: 16.0,
                config: Some(FontConfig {
                    oversample_h: 2,
                    oversample_v: 2,
                    glyph_ranges: imgui::FontGlyphRanges::from_slice(&ICON_RANGE),
                    ..FontConfig::default()
                }),
            }]);
            ICON_FONT.with(|c| c.set(Some(id)));
        }
        // Cinzel SemiBold for section titles.
        let tid = ctx.fonts().add_font(&[FontSource::TtfData {
            data: CINZEL_TTF,
            size_pixels: 18.0,
            config: Some(FontConfig {
                oversample_h: 3,
                oversample_v: 3,
                rasterizer_multiply: 1.05,
                ..FontConfig::default()
            }),
        }]);
        TITLE_FONT.with(|c| c.set(Some(tid)));
        // Orbitron Medium for emphasised numbers / values.
        let vid = ctx.fonts().add_font(&[FontSource::TtfData {
            data: ORBITRON_TTF,
            size_pixels: 15.0,
            config: Some(FontConfig {
                oversample_h: 3,
                oversample_v: 3,
                rasterizer_multiply: 1.05,
                ..FontConfig::default()
            }),
        }]);
        VALUE_FONT.with(|c| c.set(Some(vid)));
        // Inter SemiBold for sidebar nav labels (same 17 px size as the body, a touch heavier).
        let nid = ctx.fonts().add_font(&[FontSource::TtfData {
            data: INTER_SB_TTF,
            size_pixels: 17.0,
            config: Some(FontConfig {
                oversample_h: 3,
                oversample_v: 3,
                rasterizer_multiply: 1.05,
                ..FontConfig::default()
            }),
        }]);
        NAV_FONT.with(|c| c.set(Some(nid)));
    }

    /// Block window input from reaching the game whenever imgui wants the mouse/keyboard
    /// (i.e. the cursor is over our menu) — otherwise clicks fall through to the game.
    fn should_block_messages(&self, io: &imgui::Io) -> bool {
        io.want_capture_mouse || io.want_capture_keyboard
    }

    fn render(&mut self, ui: &mut Ui) {
        // Keep DOTween's timeScale pinned to our UI tempo (survives any game reset).
        crate::ui_tempo::enforce();

        // Real frame rate. imgui's `io.framerate` is unreliable under hudhook (it gets a
        // fixed delta, so it always reads ~60). hudhook calls render() once per presented
        // frame, so the TRUE FPS is simply how many render() calls happen per real second:
        // we count frames over a short wall-clock window and divide. No EMA, no 1/dt
        // averaging bias — it's the exact count of frames shown on screen per second.
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            let dt = now.duration_since(prev).as_secs_f32();
            if dt > 0.0 {
                self.fps_frames += 1;
                self.fps_window += dt;
                if self.fps_window >= 0.5 {
                    self.fps_display = self.fps_frames as f32 / self.fps_window;
                    self.fps_frames = 0;
                    self.fps_window = 0.0;
                }
            }
            // Advance the petal animation clock (clamp dt so a stall doesn't jump them).
            self.anim_t += dt.min(0.1);
            self.frame_dt = dt.min(0.1);
        }
        self.last_frame = Some(now);

        // Native intro-video player. It auto-starts as soon as the D3D device is captured —
        // which happens on hudhook's first rendered frame, ~1 s into launch, LONG before the
        // IL2CPP runtime finishes booting. That lets the video cover the game's splash logos
        // instead of making you wait through them. Stops on click (skip) or when the title
        // scene gives way to Home (detected once IL2CPP is up). The whole clip streams through
        // one dynamic texture, drawn fullscreen on the background draw list (over the game,
        // behind our control panels).
        #[cfg(feature = "banner")]
        {
            // Only engage the intro path when a custom intro is actually present (intro_full.bin).
            // With no media we never mute the title BGM, draw, or show the START button.
            let has_video = crate::intro_player::has_video();
            // Auto-start once per launch, the moment we can actually draw (device captured).
            if has_video
                && !self.intro_auto_started
                && !self.intro_done
                && crate::intro_player::is_captured()
            {
                self.intro_auto_started = true;
                crate::audio::play();
                crate::intro_player::start();
            }

            let title = crate::startup_probe::is_title();
            // First time we reach the title (IL2CPP now up): save + mute the original BGM.
            if has_video && title && !self.was_title && !self.intro_done {
                crate::bgm::mute();
            }
            // Leaving the title scene (→ Home) ends the intro for good this session.
            if !title && self.was_title {
                self.intro_done = true;
                crate::audio::stop();
                crate::bgm::restore();
                crate::bgm::restore_voice();
                crate::intro_player::stop();
            }
            self.was_title = title;
            // The game's own PlayBgm re-asserts the title BGM volume AFTER any one-shot mute,
            // so re-force it to 0 every frame while at the title (cheap) — wins the race. Only
            // matters once IL2CPP is up; harmless no-op before that.
            if has_video && title && !self.intro_done {
                crate::bgm::force_mute();
                crate::bgm::mute_voice();
            }

            let active = has_video && !self.intro_done && (self.intro_auto_started || self.intro_force);
            if active {
                // The whole clip streams through one dynamic texture, drawn by our own
                // D3D11 quad (over the game, behind the control panels).
                crate::intro_player::enqueue_draw();

                // The ONLY way to skip: a "START GAME" button bottom-right. Clicking
                // anywhere else does nothing (no accidental skips).
                let [dw, dh] = ui.io().display_size;
                let bh = 88.0;
                let bw = bh * (START_W / START_H);
                let margin = 46.0;
                let pad = ui.push_style_var(StyleVar::WindowPadding([0.0, 0.0]));
                let clicked = ui
                    .window("##startgame")
                    .position([dw - bw - margin, dh - bh - margin], Condition::Always)
                    .size([bw, bh], Condition::Always)
                    .no_decoration()
                    .draw_background(false)
                    .movable(false)
                    .save_settings(false)
                    .build(|| {
                        let p0 = ui.cursor_screen_pos();
                        let hit = ui.invisible_button("##sg", [bw, bh]);
                        let hov = ui.is_item_hovered();
                        if let Some(t) = self.start_btn_tex {
                            // Slight lift + full opacity on hover for feedback.
                            let lift = if hov { 3.0 } else { 0.0 };
                            let a = if hov { 1.0 } else { 0.9 };
                            ui.get_window_draw_list()
                                .add_image(t, [p0[0], p0[1] - lift], [p0[0] + bw, p0[1] + bh - lift])
                                .col([1.0, 1.0, 1.0, a])
                                .build();
                        }
                        hit
                    })
                    .unwrap_or(false);
                pad.end();
                if clicked {
                    self.intro_done = true;
                    self.intro_force = false;
                    crate::audio::stop();
                    crate::intro_player::stop();
                }
            }
        }

        // Edge-detect the toggle key: one physical press = one toggle (no key-repeat,
        // which otherwise flips the menu rapidly while the key is held).
        let key_down = ui.is_key_down(MENU_KEYS[menu_key_idx()].1);
        if SUPPRESS_TOGGLE.load(std::sync::atomic::Ordering::Relaxed) {
            // A key was just bound and is still physically held — swallow toggles until it's
            // released, so the press that bound it doesn't also close the menu.
            if !key_down {
                SUPPRESS_TOGGLE.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        } else if key_down && !self.toggle_was_down {
            self.show = !self.show;
            // Opening the menu once dismisses the first-launch hint forever.
            if self.show && !crate::settings::seen_hint() {
                crate::settings::set_seen_hint(true);
            }
        }
        self.toggle_was_down = key_down;

        // Freecam mouse zoom — works even with the panel hidden (drag/keys are polled in
        // freecam's own input thread). Wheel → zoom, when not over the Heaven panel.
        #[cfg(feature = "freecam")]
        if crate::freecam::is_enabled() {
            let io = ui.io();
            // Tell freecam when the cursor is over a Heaven window, so dragging the telemetry
            // box / panels moves the box and does NOT orbit the race camera.
            crate::freecam::set_ui_capture(io.want_capture_mouse);
            if !io.want_capture_mouse && io.mouse_wheel != 0.0 {
                crate::freecam::mouse_zoom(io.mouse_wheel);
            }
        }

        // Freecam live telemetry HUD — drawn BEFORE the menu-visibility return, so it stays
        // on screen during the race whether or not the Heaven menu is open (it's a HUD, not a
        // menu panel). Only while the freecam is following a Uma. panel_style is self-sufficient.
        #[cfg(feature = "freecam")]
        if crate::freecam::race_active() {
            let [dw, _dh] = ui.io().display_size;
            let tx = if self.rail_right { (dw - 300.0 - 14.0).max(0.0) } else { 14.0 };
            draw_freecam_telemetry(ui, tx, 150.0, Condition::FirstUseEver);
        }

        // First-launch hint: until the user opens the menu once, show which key opens it
        // (otherwise a closed menu with an unknown key leaves them stuck).
        if !self.show && !crate::settings::seen_hint() {
            draw_first_launch_hint(ui);
        }
        if !self.show {
            return;
        }

        // Keep the FPS controls in sync with the ACTUAL cap state. new() runs
        // before the async boot thread applies persisted settings, so without
        // this the checkbox/slider would show stale values (e.g. unchecked while
        // the game is already capped from settings.json). Dragging the slider
        // sets current()==fps_val, so this never fights the user.
        let cur = crate::fps::current();
        self.fps_on = cur != 0;
        if cur > 0 {
            self.fps_val = cur;
        }

        // ── push the Heaven telemetry style for every window this frame ──
        let _c0 = ui.push_style_color(StyleColor::WindowBg, PANEL_BG);
        let _c1 = ui.push_style_color(StyleColor::ChildBg, [0.0, 0.0, 0.0, 0.0]);
        let _c2 = ui.push_style_color(StyleColor::Border, BORDER);
        let _c3 = ui.push_style_color(StyleColor::TitleBg, TITLE_BG);
        let _c4 = ui.push_style_color(StyleColor::TitleBgActive, TITLE_BG_ON);
        let _c5 = ui.push_style_color(StyleColor::TitleBgCollapsed, TITLE_BG);
        let _c6 = ui.push_style_color(StyleColor::Text, TEXT);
        let _c7 = ui.push_style_color(StyleColor::TextDisabled, DIM);
        let _c8 = ui.push_style_color(StyleColor::Button, BTN_BG);
        let _c9 = ui.push_style_color(StyleColor::ButtonHovered, BTN_HI);
        let _c10 = ui.push_style_color(StyleColor::ButtonActive, AMBER_MED);
        let _c11 = ui.push_style_color(StyleColor::FrameBg, FRAME_BG);
        let _c12 = ui.push_style_color(StyleColor::FrameBgHovered, FRAME_HI);
        let _c13 = ui.push_style_color(StyleColor::FrameBgActive, FRAME_HI);
        let _c14 = ui.push_style_color(StyleColor::CheckMark, ACCENT);
        let _c15 = ui.push_style_color(StyleColor::SliderGrab, ACCENT);
        let _c16 = ui.push_style_color(StyleColor::SliderGrabActive, ACCENT_HI);
        let _c17 = ui.push_style_color(StyleColor::Header, AMBER_SOFT);
        let _c18 = ui.push_style_color(StyleColor::HeaderHovered, AMBER_MED);
        let _c19 = ui.push_style_color(StyleColor::HeaderActive, AMBER_MED);
        let _c20 = ui.push_style_color(StyleColor::Separator, BORDER);
        let _c21 = ui.push_style_color(StyleColor::PlotHistogram, ACCENT);
        let _c22 = ui.push_style_color(StyleColor::ResizeGrip, [0.4, 0.4, 0.45, 0.25]);
        let _c23 = ui.push_style_color(StyleColor::ResizeGripHovered, AMBER_MED);
        let _c24 = ui.push_style_color(StyleColor::ResizeGripActive, ACCENT);

        let _v0 = ui.push_style_var(StyleVar::WindowRounding(10.0));
        let _v1 = ui.push_style_var(StyleVar::ChildRounding(8.0));
        let _v2 = ui.push_style_var(StyleVar::FrameRounding(6.0));
        let _v3 = ui.push_style_var(StyleVar::GrabRounding(6.0));
        let _v4 = ui.push_style_var(StyleVar::WindowBorderSize(1.0));
        let _v5 = ui.push_style_var(StyleVar::WindowPadding([12.0, 11.0]));
        let _v6 = ui.push_style_var(StyleVar::FramePadding([9.0, 5.0]));
        let _v7 = ui.push_style_var(StyleVar::ItemSpacing([8.0, 7.0]));
        let _v8 = ui.push_style_var(StyleVar::ScrollbarRounding(6.0));
        let _v9 = ui.push_style_var(StyleVar::WindowTitleAlign([0.0, 0.5]));

        // ── rail layout: anchor windows to the chosen edge ──
        let [dw, _dh] = ui.io().display_size;
        let applied = self.relayout;
        let cond = if applied { Condition::Always } else { Condition::FirstUseEver };
        let right = self.rail_right;
        let margin = 14.0;
        let x = |w: f32| if right { (dw - w - margin).max(0.0) } else { margin };

        // The premium sidebar menu, or the classic "Controls" rail if the user picked it.
        if crate::settings::classic_menu() {
            self.draw_controls(ui, x(400.0), cond);
        } else {
            self.draw_menu(ui);
        }

        if applied {
            self.relayout = false;
        }
    }
}

impl HeavenOverlay {
    /// The menu: a centered (or edge-docked) window with a left category sidebar and a
    /// content page on the right. Categories keep the panel from growing as features pile up.
    fn draw_menu(&mut self, ui: &Ui) {
        let fps_now = self.fps_display.round() as i32; // true FPS = frames/sec (see render)
        let [dw, dh] = ui.io().display_size;
        let centered = crate::settings::menu_centered();
        // Restore the user's saved menu size/position if they've moved/resized it before.
        let saved = crate::settings::win_rect("menu");
        let (w, h) = match saved {
            Some(r) => (r[2].clamp(280.0, dw - 28.0), r[3].clamp(200.0, dh - 28.0)),
            None => (MENU_W.min(dw - 28.0), MENU_H.min(dh - 28.0)),
        };
        // Default position from the centered/rail layout (used on first open or a relayout toggle).
        let default_pos = if centered {
            [((dw - w) * 0.5).max(0.0), ((dh - h) * 0.5).max(0.0)]
        } else {
            let m = 14.0;
            let x = if self.rail_right { (dw - w - m).max(0.0) } else { m };
            [x, ((dh - h) * 0.5).max(0.0)]
        };
        // A relayout (centered/rail toggle) forces the default position; otherwise restore the
        // user's saved position if they've moved it (so the menu stays where they put it).
        let pos = if self.relayout {
            default_pos
        } else {
            saved.map(|r| [r[0], r[1]]).unwrap_or(default_pos)
        };
        let cond = if self.relayout { Condition::Always } else { Condition::FirstUseEver };

        // Category list + content come from the single-source menu model, so the premium and
        // classic menus can't drift. `self.tab` indexes `tabs`; the content loop keys off name.
        let menu = crate::menu_model::model();
        #[allow(unused_mut)]
        let mut tabs: Vec<&str> = menu.iter().map(|t| t.name).collect();
        if self.tab >= tabs.len() {
            self.tab = 0;
        }

        // Re-sync the cached slider values from the live state every frame, so the UI shows the
        // PERSISTED settings: they're applied (settings::apply_on_boot) after this overlay is
        // constructed, so the values captured at construction are stale otherwise.
        let cur_fps = crate::fps::current();
        self.fps_on = cur_fps != 0;
        if cur_fps > 0 {
            self.fps_val = cur_fps;
        }
        self.ui_tempo_val = crate::ui_tempo::tempo();

        let relayout = &mut self.relayout;
        let rail_right = &mut self.rail_right;
        let fps_on = &mut self.fps_on;
        let fps_val = &mut self.fps_val;
        let ui_tempo_val = &mut self.ui_tempo_val;
        // A tab switch (from last frame) resets the content fade-in to 0.
        if self.tab != self.prev_tab {
            anim_set("tab_fade", 0.0);
            self.prev_tab = self.tab;
        }
        let tab = &mut self.tab;
        let anim_t = self.anim_t;
        FRAME_DT.with(|c| c.set(self.frame_dt));
        let icon_font = ICON_FONT.with(|c| c.get());
        #[cfg(feature = "banner")]
        let banner_tex = self.banner_tex;
        #[cfg(feature = "banner")]
        let logo_tex = self.menu_logo_tex;
        #[cfg(feature = "banner")]
        let crest_tex = self.crest_tex;
        #[cfg(feature = "banner")]
        let sil_tex = self.sil_tex;
        #[cfg(feature = "banner")]
        let intro_done = &mut self.intro_done;
        #[cfg(feature = "banner")]
        let intro_force = &mut self.intro_force;

        ui.window("Heaven")
            .size([w, h], Condition::FirstUseEver) // initial size; user can drag-resize
            .position(pos, cond)
            .title_bar(false)
            .collapsible(false)
            .resizable(true)
            .build(|| {
                let p0 = ui.window_pos();
                let wsz = ui.window_size();
                // Below this height the silhouette/sparkles are hidden so the nav rows never
                // overlap them; the nav reclaims that reserved bottom space (see bottom_limit).
                let show_decor = wsz[1] >= 520.0;
                let pmax = [p0[0] + wsz[0], p0[1] + wsz[1]];
                // Tileable background texture over the whole window, dimmed with a scrim so the
                // content cards stay readable. Shows through the page margins between cards.
                #[cfg(feature = "banner")]
                if let Some(bg) = BG_TEX.with(|c| c.get()) {
                    let dl = ui.get_window_draw_list();
                    dl.add_image(bg, p0, pmax).col([1.0, 1.0, 1.0, 0.5]).build();
                    dl.add_rect(p0, pmax, [0.043, 0.024, 0.086, 0.62]).filled(true).rounding(10.0).build();
                }
                // Darker sidebar strip on the left (rounded only on the left to match the window).
                ui.get_window_draw_list()
                    .add_rect(p0, [p0[0] + SIDEBAR_W, p0[1] + wsz[1]], SIDEBAR_BG)
                    .filled(true)
                    .rounding(10.0)
                    .round_top_right(false)
                    .round_bot_right(false)
                    .build();
                // Falling sakura petals drifting down the sidebar (subtle animated accent).
                #[cfg(feature = "banner")]
                {
                    let petals = PETAL_TEX.with(|c| c.get());
                    if petals.iter().any(|p| p.is_some()) {
                        let dl = ui.get_window_draw_list();
                        // (x-fraction across sidebar, fall speed px/s, phase, size, tex idx)
                        let defs: [(f32, f32, f32, f32, usize); 7] = [
                            (0.18, 26.0, 0.0, 17.0, 0),
                            (0.42, 19.0, 40.0, 22.0, 1),
                            (0.66, 31.0, 80.0, 15.0, 2),
                            (0.84, 22.0, 120.0, 19.0, 0),
                            (0.30, 34.0, 170.0, 14.0, 1),
                            (0.56, 17.0, 210.0, 21.0, 2),
                            (0.74, 28.0, 260.0, 16.0, 0),
                        ];
                        let span = wsz[1] + 80.0;
                        for (xf, sp, ph, sz, ti) in defs {
                            if let Some(t) = petals[ti] {
                                let yy = p0[1] - 40.0 + ((anim_t * sp + ph) % span);
                                let sway = (anim_t * 0.7 + ph).sin() * 9.0;
                                let cx = p0[0] + 10.0 + xf * (SIDEBAR_W - 30.0) + sway;
                                dl.add_image(t, [cx, yy], [cx + sz, yy + sz])
                                    .col([1.0, 1.0, 1.0, 0.55])
                                    .build();
                            }
                        }
                    }
                }

                ui.columns(2, "##menu", false);
                ui.set_column_width(0, SIDEBAR_W);

                // ── sidebar: crest + wordmark + category list ──
                #[cfg(feature = "banner")]
                {
                    let mut y = 6.0;
                    if let Some(t) = crest_tex {
                        let cs = 78.0;
                        // Soft magenta halo behind the crest (floating glow orb).
                        if let Some(orb) = ORB_TEX.with(|c| c.get()) {
                            let ocx = p0[0] + SIDEBAR_W * 0.5;
                            let ocy = p0[1] + y + cs * 0.5;
                            let os = cs * 0.92;
                            ui.get_window_draw_list()
                                .add_image(orb, [ocx - os, ocy - os], [ocx + os, ocy + os])
                                .col([1.0, 1.0, 1.0, 0.55])
                                .build();
                        }
                        ui.set_cursor_pos([(SIDEBAR_W - cs) * 0.5, y]);
                        imgui::Image::new(t, [cs, cs]).build(ui);
                        y += cs;
                    }
                    if let Some(t) = logo_tex {
                        let lw = (SIDEBAR_W - 36.0) * 0.87;
                        let lh = lw * (LOGO_H / LOGO_W);
                        ui.set_cursor_pos([(SIDEBAR_W - lw) * 0.5, y]);
                        imgui::Image::new(t, [lw, lh]).build(ui);
                        y += lh + 6.0;
                    }
                    ui.set_cursor_pos([10.0, y]);
                }
                {
                    // The selected/hover backgrounds are drawn by hand (animated), so the
                    // selectable itself stays transparent.
                    let _hs = ui.push_style_color(StyleColor::Header, [0.0, 0.0, 0.0, 0.0]);
                    let _hh = ui.push_style_color(StyleColor::HeaderHovered, [0.0, 0.0, 0.0, 0.0]);
                    let _ha = ui.push_style_color(StyleColor::HeaderActive, [0.0, 0.0, 0.0, 0.0]);
                    let _fr = ui.push_style_var(StyleVar::FrameRounding(8.0));
                    let nav_tex = NAV_TEX.with(|c| c.get());
                    let nav_y0 = ui.cursor_pos()[1];
                    // Distribute the items evenly down the available space (above the silhouette)
                    // so the column looks ordered, with larger icons and clear separation —
                    // instead of small icons bunched at the top with empty bar below.
                    let n_items = tabs.len() as f32;
                    let bottom_limit = wsz[1] - if show_decor { 200.0 } else { 36.0 };
                    let nav_avail = (bottom_limit - nav_y0).max(n_items * 46.0);
                    let nav_pitch = (nav_avail / n_items).clamp(46.0, 56.0);
                    let nav_half = nav_pitch * 0.5;
                    let icon_sz = 42.0_f32;
                    // Animated active background + gold→magenta bar that slide toward the
                    // selected row (~160 ms). Drawn BEFORE the rows so content sits on top.
                    let active_y = anim_step("nav_bar_y", nav_y0 + (*tab as f32) * nav_pitch, 12.0);
                    {
                        let dl = ui.get_window_draw_list();
                        dl.add_rect(
                            [p0[0] + 8.0, p0[1] + active_y + 3.0],
                            [p0[0] + SIDEBAR_W - 8.0, p0[1] + active_y + nav_pitch - 3.0],
                            SEL_BG,
                        )
                        .filled(true)
                        .rounding(10.0)
                        .build();
                        dl.add_rect_filled_multicolor(
                            [p0[0] + 4.0, p0[1] + active_y + nav_half - 14.0],
                            [p0[0] + 8.5, p0[1] + active_y + nav_half + 14.0],
                            GOLD,
                            GOLD,
                            [0.77, 0.42, 1.0, 1.0],
                            [0.77, 0.42, 1.0, 1.0],
                        );
                    }
                    for (i, name) in tabs.iter().enumerate() {
                        let cy = ui.cursor_pos()[1];
                        let sel = *tab == i;
                        // Empty selectable = the clickable pill background (taller row so the
                        // larger crystal icons breathe).
                        ui.set_cursor_pos([10.0, cy]);
                        if ui
                            .selectable_config(format!("##nav{i}"))
                            .selected(sel)
                            .size([SIDEBAR_W - 20.0, nav_pitch - 6.0])
                            .build()
                        {
                            *tab = i;
                        }
                        let hov = ui.is_item_hovered();
                        // Eased hover amount (~100 ms ease-out) drives the wash + icon glow.
                        let hv = anim_step(&format!("nav_h{i}"), if hov { 1.0 } else { 0.0 }, 22.0);
                        if hv > 0.001 && !sel {
                            ui.get_window_draw_list()
                                .add_rect(
                                    [p0[0] + 8.0, p0[1] + cy + 3.0],
                                    [p0[0] + SIDEBAR_W - 8.0, p0[1] + cy + nav_pitch - 3.0],
                                    [0.80, 0.44, 1.0, 0.16 * hv],
                                )
                                .filled(true)
                                .rounding(9.0)
                                .build();
                        }
                        // States: NORMAL (glow ×1) · HOVER (+15% glow, +10% brightness) ·
                        // ACTIVE (+25% glow, gold accent). Label eases dim → accent on hover.
                        let nav_dim = [0.70, 0.64, 0.82, 1.0];
                        let col = if sel { TEXT } else { lerp_col(nav_dim, ACCENT_HI, hv) };
                        let icc = [p0[0] + 35.0, p0[1] + cy + nav_half]; // icon centre
                        let nav_t = nav_icon_idx(name).and_then(|ix| nav_tex[ix]);
                        if let Some(t) = nav_t {
                            let dl = ui.get_window_draw_list();
                            // Magenta contrast glow: +15% on hover, +25% on active.
                            let gf = 1.0 + 0.15 * hv + if sel { 0.25 } else { 0.0 };
                            for (r, a) in [
                                (icon_sz * 0.60, 0.07_f32),
                                (icon_sz * 0.44, 0.11),
                                (icon_sz * 0.32, 0.16),
                            ] {
                                dl.add_circle(icc, r, [0.85, 0.42, 1.0, (a * gf).min(0.55)])
                                    .filled(true)
                                    .build();
                            }
                            // Active: a faint gold ring behind the icon (subtle metallic accent).
                            if sel {
                                dl.add_circle(icc, icon_sz * 0.5 - 2.0, [0.843, 0.694, 0.365, 0.22])
                                    .thickness(2.0)
                                    .build();
                            }
                            // Subtle rounded badge plate.
                            let bh = icon_sz * 0.5 + 4.0;
                            let badge = if sel { [0.80, 0.50, 1.0, 0.26] } else { [0.78, 0.49, 1.0, 0.08] };
                            dl.add_rect([icc[0] - bh, icc[1] - bh], [icc[0] + bh, icc[1] + bh], badge)
                                .filled(true)
                                .rounding(11.0)
                                .build();
                            let ip0 = [icc[0] - icon_sz * 0.5, icc[1] - icon_sz * 0.5];
                            let ip1 = [icc[0] + icon_sz * 0.5, icc[1] + icon_sz * 0.5];
                            dl.add_image(t, ip0, ip1).build();
                            // Brightness pass: +10% on hover, a touch more when active.
                            let boost = if sel { 0.25 } else { 0.0 } + 0.12 * hv;
                            if boost > 0.01 {
                                dl.add_image(t, ip0, ip1).col([1.0, 1.0, 1.0, boost.min(0.85)]).build();
                            }
                        } else {
                            let bh = icon_sz * 0.5 - 1.0;
                            let badge = if sel { [0.78, 0.49, 1.0, 0.85] } else { [0.78, 0.49, 1.0, 0.14] };
                            ui.get_window_draw_list()
                                .add_rect([icc[0] - bh, icc[1] - bh], [icc[0] + bh, icc[1] + bh], badge)
                                .filled(true)
                                .rounding(8.0)
                                .build();
                            if let Some(f) = icon_font {
                                ui.set_cursor_pos([26.0, cy + nav_half - 8.0]);
                                let _t = ui.push_font(f);
                                ui.text_colored(if sel { [0.10, 0.07, 0.16, 1.0] } else { ACCENT }, cat_icon(name));
                            }
                        }
                        ui.set_cursor_pos([66.0, cy + nav_half - 9.0]);
                        if let Some(f) = NAV_FONT.with(|c| c.get()) {
                            let _t = ui.push_font(f);
                            ui.text_colored(col, name);
                        } else {
                            ui.text_colored(col, name);
                        }
                        ui.set_cursor_pos([10.0, cy + nav_pitch]);
                    }
                }

                // Translucent character silhouette near the bottom of the sidebar.
                #[cfg(feature = "banner")]
                if let Some(t) = sil_tex.filter(|_| show_decor) {
                    let sw = 150.0;
                    let sh = sw * (SIL_H / SIL_W);
                    let sx = p0[0] + (SIDEBAR_W - sw) * 0.5;
                    let sy = p0[1] + wsz[1] - sh - 14.0;
                    ui.get_window_draw_list()
                        .add_image(t, [sx, sy], [sx + sw, sy + sh])
                        .build();
                }

                // Sparkle particles scattered in the lower sidebar (cosmetic). Uses the baked
                // spark textures when available, falling back to simple drawn 4-point stars.
                if show_decor {
                    let dl = ui.get_window_draw_list();
                    let base_y = p0[1] + wsz[1] - 175.0;
                    let stars: [(f32, f32, f32, [f32; 4]); 8] = [
                        (30.0, 0.0, 5.0, [0.84, 0.56, 0.96, 0.50]),
                        (100.0, 22.0, 3.4, [0.93, 0.52, 0.78, 0.46]),
                        (56.0, 52.0, 4.6, [0.80, 0.56, 1.0, 0.50]),
                        (130.0, 68.0, 3.0, [0.93, 0.52, 0.78, 0.42]),
                        (36.0, 94.0, 4.0, [0.82, 0.56, 0.98, 0.46]),
                        (92.0, 118.0, 5.2, [0.90, 0.53, 0.82, 0.50]),
                        (148.0, 106.0, 2.6, [0.80, 0.56, 1.0, 0.40]),
                        (66.0, 144.0, 3.4, [0.88, 0.52, 0.80, 0.46]),
                    ];
                    let spark_tex = SPARK_TEX.with(|c| c.get());
                    let has_img = spark_tex.iter().any(|s| s.is_some());
                    for (k, (sx, sy, r, c)) in stars.iter().enumerate() {
                        let cx = p0[0] + sx;
                        let cyy = base_y + sy;
                        if has_img {
                            if let Some(t) = spark_tex[k % 3] {
                                let s = r * 3.2;
                                let a = (c[3] + 0.2).min(0.85);
                                dl.add_image(t, [cx - s, cyy - s], [cx + s, cyy + s])
                                    .col([1.0, 1.0, 1.0, a])
                                    .build();
                                continue;
                            }
                        }
                        dl.add_line([cx - r, cyy], [cx + r, cyy], *c).thickness(1.4).build();
                        dl.add_line([cx, cyy - r], [cx, cyy + r], *c).thickness(1.4).build();
                        dl.add_circle([cx, cyy], 1.3, *c).filled(true).build();
                    }
                }

                // Discreet footer pinned to the very bottom of the sidebar.
                {
                    let ft = concat!("v", env!("CARGO_PKG_VERSION"), "   \u{00b7}   Night DC");
                    let fw = ui.calc_text_size(ft)[0];
                    ui.get_window_draw_list().add_text(
                        [p0[0] + (SIDEBAR_W - fw) * 0.5, p0[1] + wsz[1] - 19.0],
                        [0.50, 0.44, 0.62, 0.85],
                        ft,
                    );
                }

                // Thin luminous divider between the sidebar and the content column.
                {
                    let lx = p0[0] + SIDEBAR_W;
                    let midy = p0[1] + wsz[1] * 0.5;
                    let solid = [0.86, 0.55, 1.0, 0.42];
                    let trans = [0.86, 0.55, 1.0, 0.0];
                    let dl = ui.get_window_draw_list();
                    dl.add_rect_filled_multicolor([lx - 1.0, p0[1] + 16.0], [lx + 1.0, midy], trans, trans, solid, solid);
                    dl.add_rect_filled_multicolor([lx - 1.0, midy], [lx + 1.0, p0[1] + wsz[1] - 16.0], solid, solid, trans, trans);
                }

                ui.next_column();

                // ── content column ──
                let content_w = wsz[0] - SIDEBAR_W - 24.0;

                // Header: a glass strip (gradient sheen + gold border) with the wordmark on the
                // left and live FPS / speed / skip metrics on the right (numbers in Orbitron).
                {
                    let sp = ui.cursor_screen_pos();
                    let bh = 34.0;
                    let mx = [sp[0] + content_w, sp[1] + bh];
                    {
                        let dl = ui.get_window_draw_list();
                        dl.add_rect(sp, mx, [0.12, 0.07, 0.20, 0.94]).filled(true).rounding(9.0).build();
                        dl.add_rect_filled_multicolor(
                            [sp[0] + 2.0, sp[1] + 2.0],
                            [mx[0] - 2.0, mx[1] - 2.0],
                            [0.24, 0.15, 0.36, 0.55],
                            [0.16, 0.10, 0.28, 0.12],
                            [0.16, 0.10, 0.28, 0.12],
                            [0.24, 0.15, 0.36, 0.55],
                        );
                        dl.add_rect(sp, mx, [0.84, 0.69, 0.36, 0.42]).rounding(9.0).thickness(1.2).build();
                        dl.add_circle([sp[0] + 17.0, sp[1] + bh * 0.5], 4.0, GOOD).filled(true).build();
                    }
                    // Wordmark (Cinzel) + version.
                    let tf = TITLE_FONT.with(|c| c.get());
                    let hx = sp[0] + 30.0;
                    let hy = sp[1] + bh * 0.5 - 9.0;
                    let hw = if let Some(f) = tf {
                        let _t = ui.push_font(f);
                        ui.get_window_draw_list().add_text([hx, hy], TEXT, "HEAVEN");
                        ui.calc_text_size("HEAVEN")[0]
                    } else {
                        ui.get_window_draw_list().add_text([hx, hy], TEXT, "HEAVEN");
                        ui.calc_text_size("HEAVEN")[0]
                    };
                    ui.get_window_draw_list().add_text(
                        [hx + hw + 8.0, sp[1] + bh * 0.5 - 7.0],
                        DIM,
                        concat!("v", env!("CARGO_PKG_VERSION")),
                    );
                    // Right edge of the wordmark+version — chips that would cross it are dropped
                    // (narrow window / large UI scale) so they never overlap the title.
                    let ver_w = ui.calc_text_size(concat!("v", env!("CARGO_PKG_VERSION")))[0];
                    let word_right = hx + hw + 8.0 + ver_w + 10.0;
                    // Right-aligned metric chips: dark crystal pills with a subtle border, an
                    // Inter label + Orbitron value, and a faint glow when the value is "active".
                    let skip_on = crate::skip::is_event_enabled()
                        || crate::skip::is_train_enabled()
                        || crate::skip::is_race_result_enabled();
                    let vf = VALUE_FONT.with(|c| c.get());
                    // (label, value, active)
                    let chips: [(&str, String, bool); 3] = [
                        ("FPS", format!("{fps_now}"), false),
                        ("SPEED", format!("{:.1}x", *ui_tempo_val), (*ui_tempo_val - 1.0).abs() >= 0.05),
                        ("SKIP", (if skip_on { "ON" } else { "OFF" }).to_string(), skip_on),
                    ];
                    let (cpad, lbl_gap, gap, chip_h) = (9.0_f32, 6.0_f32, 7.0_f32, 22.0_f32);
                    let chip_y = sp[1] + (bh - chip_h) * 0.5;
                    let val_w = |s: &str| -> f32 {
                        if let Some(f) = vf {
                            let _t = ui.push_font(f);
                            return ui.calc_text_size(s)[0];
                        }
                        ui.calc_text_size(s)[0]
                    };
                    let dl = ui.get_window_draw_list();
                    let mut rx = mx[0] - 12.0;
                    for (lbl, vv, act) in chips.iter().rev() {
                        let lw = ui.calc_text_size(lbl)[0];
                        let vw = val_w(vv);
                        let cwid = cpad * 2.0 + lw + lbl_gap + vw;
                        let cx0 = rx - cwid;
                        let cx1 = rx;
                        // Would cross into the wordmark → drop this and the remaining (further-left) chips.
                        if cx0 < word_right {
                            break;
                        }
                        if *act {
                            dl.add_rect(
                                [cx0 - 2.0, chip_y - 2.0],
                                [cx1 + 2.0, chip_y + chip_h + 2.0],
                                [0.45, 0.85, 0.62, 0.12],
                            )
                            .filled(true)
                            .rounding(8.0)
                            .build();
                        }
                        dl.add_rect([cx0, chip_y], [cx1, chip_y + chip_h], [0.10, 0.06, 0.18, 0.62])
                            .filled(true)
                            .rounding(7.0)
                            .build();
                        dl.add_rect(
                            [cx0, chip_y],
                            [cx1, chip_y + chip_h],
                            [0.60, 0.50, 0.85, if *act { 0.42 } else { 0.22 }],
                        )
                        .rounding(7.0)
                        .thickness(1.0)
                        .build();
                        let ly = chip_y + chip_h * 0.5 - 7.0;
                        dl.add_text([cx0 + cpad, ly], DIM, lbl);
                        let vcol = if *act { GOOD } else { TEXT };
                        if let Some(f) = vf {
                            let _t = ui.push_font(f);
                            dl.add_text([cx0 + cpad + lw + lbl_gap, ly], vcol, vv);
                        } else {
                            dl.add_text([cx0 + cpad + lw + lbl_gap, ly], vcol, vv);
                        }
                        rx = cx0 - gap;
                    }
                    drop(dl);
                    ui.set_cursor_screen_pos([sp[0], sp[1] + bh + 10.0]);
                }

                #[cfg(feature = "banner")]
                if let Some(t) = banner_tex {
                    let bsp = ui.cursor_screen_pos();
                    let bh = content_w * (BANNER_H / BANNER_W);
                    imgui::Image::new(t, [content_w, bh]).build(ui);
                    // Gradient fade along the bottom edge so the page/cards appear to emerge
                    // from the banner rather than sitting in a hard-edged box.
                    let fade = 48.0;
                    let top = [0.07, 0.04, 0.13, 0.0];
                    let bot = [0.07, 0.04, 0.13, 0.92];
                    ui.get_window_draw_list().add_rect_filled_multicolor(
                        [bsp[0], bsp[1] + bh - fade],
                        [bsp[0] + content_w, bsp[1] + bh],
                        top,
                        top,
                        bot,
                        bot,
                    );
                    ui.dummy([0.0, 2.0]);
                }

                let _cardbg = ui.push_style_color(StyleColor::ChildBg, [0.0, 0.0, 0.0, 0.0]);
                let _cardpad = ui.push_style_var(StyleVar::WindowPadding([2.0, 6.0]));
                let page_top = ui.cursor_screen_pos();
                ui.child_window("##page")
                    .size([content_w, 0.0])
                    .flags(imgui::WindowFlags::NO_SCROLL_WITH_MOUSE)
                    .build(|| {
                    // Mouse-wheel scroll: inside imgui columns a child doesn't claim the wheel on
                    // hover (it needs a click/focus first), so drive the scroll manually.
                    let wheel = ui.io().mouse_wheel;
                    if wheel != 0.0
                        && ui.is_window_hovered_with_flags(imgui::WindowHoveredFlags::ROOT_AND_CHILD_WINDOWS)
                    {
                        let ny = (ui.scroll_y() - wheel * 52.0).clamp(0.0, ui.scroll_max_y());
                        ui.set_scroll_y(ny);
                    }
                    // Card width = the child's usable width (already excludes any scrollbar).
                    let cw = ui.content_region_avail()[0] - 2.0;
                    // Unified menu: BOTH styles render from crate::menu_model::model() — one
                    // source of truth, so premium and classic can't drift. Premium visuals
                    // (cards, pills, fonts, glass icons) are unchanged; only the control LIST is
                    // shared. Bespoke blocks are Ctrl::Custom, drawn by the hand-written arms.
                    let sel_name = tabs[*tab];
                    for mt in menu.iter().filter(|t| t.name == sel_name) {
                        for sec in &mt.sections {
                            let title_up = sec.title.to_uppercase();
                            let glyph = sec.icon.to_string();
                            card(ui, cw, sec.title, || {
                                use crate::menu_model::{Ctrl, Custom};
                                section(ui, icon_font, &glyph, &title_up);
                                if !sec.blurb.is_empty() {
                                    ui.text_colored(DIM, sec.blurb);
                                }
                                for c in &sec.controls {
                                    match c {
                                        Ctrl::Toggle { id, label, get, set } => {
                                            let g = *get;
                                            let s = *set;
                                            ui.dummy([0.0, 6.0]);
                                            if toggle_row(ui, id, label, g(), cw) {
                                                s(!g());
                                                crate::settings::save_current();
                                            }
                                        }
                                        Ctrl::SliderF32 { id, min, max, get, set, unit, .. } => {
                                            let g = *get;
                                            let s = *set;
                                            ui.dummy([0.0, 8.0]);
                                            let mut v = g();
                                            if pink_slider_f32(ui, id, *min, *max, &mut v, cw - 32.0) {
                                                s(v);
                                                crate::settings::save_current();
                                            }
                                            ui.dummy([0.0, 4.0]);
                                            ui.text_colored(DIM, "Current:");
                                            ui.same_line();
                                            let cc = if (v - 1.0).abs() < 0.05 { DIM } else { GOOD };
                                            val(ui, cc, &format!("{:.1}{}", v, unit));
                                        }
                                        Ctrl::Cycle { id, label, label_of, next } => {
                                            let lo = *label_of;
                                            let nx = *next;
                                            ui.dummy([0.0, 8.0]);
                                            ui.text_colored(DIM, *label);
                                            ui.same_line();
                                            if btn(ui, id, lo()) {
                                                nx();
                                                crate::settings::save_current();
                                            }
                                        }
                                        Ctrl::Button { id, label, action } => {
                                            let a = *action;
                                            ui.dummy([0.0, 6.0]);
                                            if btn(ui, id, label) {
                                                a();
                                            }
                                        }
                                        Ctrl::Note(t) => {
                                            ui.dummy([0.0, 4.0]);
                                            ui.text_colored(DIM, *t);
                                        }
                                        Ctrl::Custom(Custom::Fps) => {
                                            let cur = crate::fps::current();
                                            let capped = cur > 0;
                                            let unlimited = cur < 0;
                                            ui.dummy([0.0, 6.0]);
                                            // Cap and Unlimited are mutually exclusive and reflect the real
                                            // mode; toggling the active one returns to Off (no more "both on").
                                            if toggle_row(ui, "##cap", "Cap FPS", capped, cw) {
                                                *fps_on = !capped;
                                                crate::fps::set_cap(if capped { 0 } else { *fps_val });
                                                crate::settings::save_current();
                                            }
                                            ui.dummy([0.0, 6.0]);
                                            if toggle_row(ui, "##unl", "Unlimited", unlimited, cw) {
                                                *fps_on = false;
                                                crate::fps::set_cap(if unlimited { 0 } else { -1 });
                                                crate::settings::save_current();
                                            }
                                            ui.dummy([0.0, 8.0]);
                                            ui.text_colored(DIM, "FPS limit");
                                            if pink_slider_i32(ui, "##fpscap", 1, 300, fps_val, cw - 32.0) {
                                                *fps_on = true;
                                                crate::fps::set_cap(*fps_val);
                                                crate::settings::save_current();
                                            }
                                            ui.dummy([0.0, 4.0]);
                                            ui.text_colored(DIM, "Current cap:");
                                            ui.same_line();
                                            let (cap_txt, cap_col) = if cur < 0 {
                                                ("Unlimited".to_string(), GOOD)
                                            } else if cur == 0 {
                                                ("off".to_string(), DIM)
                                            } else {
                                                (format!("{cur} FPS"), GOOD)
                                            };
                                            val(ui, cap_col, &cap_txt);
                                            ui.dummy([0.0, 2.0]);
                                            ui.text_colored(DIM, "Real FPS:");
                                            ui.same_line();
                                            val(ui, GOOD, &format!("{fps_now}"));
                                        }
                                        #[cfg(feature = "freecam")]
                                        Ctrl::Custom(Custom::Freecam) => {
                                            let fc = crate::freecam::is_enabled();
                                            ui.dummy([0.0, 6.0]);
                                            if toggle_row(ui, "##fc", "Race freecam", fc, cw) {
                                                crate::settings::set_freecam(!fc);
                                            }
                                            if fc {
                                                ui.dummy([0.0, 4.0]);
                                                if crate::freecam::is_follow() {
                                                    ui.text_colored(ACCENT, format!("follow gate {}/{}", crate::freecam::target_gate(), crate::freecam::max_gate()));
                                                    if btn(ui, "##prevuma", "< prev Uma") {
                                                        crate::freecam::cycle_target(-1);
                                                    }
                                                    ui.same_line();
                                                    if btn(ui, "##nextuma", "next Uma >") {
                                                        crate::freecam::cycle_target(1);
                                                    }
                                                }
                                                ui.text_colored(DIM, "[ ] Uma  \u{00b7}  arrows orbit/zoom  \u{00b7}  I/K height");
                                                ui.dummy([0.0, 4.0]);
                                                draw_preset_manager(ui, cw);
                                                let pose = crate::freecam::captured_pose();
                                                if !pose.is_empty() {
                                                    ui.text_colored(GOOD, pose);
                                                }
                                            }
                                        }
                                        #[cfg(feature = "banner")]
                                        Ctrl::Custom(Custom::Intro) => {
                                            let has_file = crate::paths::local_file("intro_full.bin").exists();
                                            ui.text_colored(DIM, "Status:");
                                            ui.same_line();
                                            let playing = crate::intro_player::is_playing();
                                            if playing {
                                                ui.text_colored(GOOD, "playing");
                                            } else if has_file {
                                                ui.text_colored(GOOD, "custom intro ready");
                                            } else {
                                                ui.text_colored(WARN, "no intro file");
                                            }
                                            ui.dummy([0.0, 8.0]);
                                            if btn(ui, "##replay", "Replay intro") {
                                                *intro_done = false;
                                                *intro_force = true;
                                                crate::audio::play();
                                                crate::intro_player::start();
                                            }
                                            ui.dummy([0.0, 6.0]);
                                            ui.text_colored(DIM, "Drop intro_full.bin + intro_song.ogg next to the DLL,");
                                            ui.text_colored(DIM, "or build them with tools/pack_intro.py.");
                                        }
                                        Ctrl::Custom(Custom::Updates) => {
                                            ui.text_colored(DIM, "Current version");
                                            ui.text_colored(ACCENT, concat!("Heaven MOD  v", env!("CARGO_PKG_VERSION")));
                                            let ust = crate::update::status();
                                            ui.dummy([0.0, 4.0]);
                                            ui.text_colored(DIM, "Status:");
                                            ui.same_line();
                                            if ust.is_empty() {
                                                ui.text_colored(DIM, "press Check for updates");
                                            } else {
                                                ui.text_colored(GOOD, &ust);
                                            }
                                            ui.dummy([0.0, 10.0]);
                                            if btn(ui, "##rel", "Releases") {
                                                open_url(crate::update::RELEASES_URL);
                                            }
                                        }
                                        Ctrl::Custom(Custom::AboutLayout) => {
                                            let cen = crate::settings::menu_centered();
                                            ui.dummy([0.0, 6.0]);
                                            if toggle_row(ui, "##cen", "Centered window", cen, cw) {
                                                crate::settings::set_menu_centered(!cen);
                                                *relayout = true;
                                            }
                                            if !cen {
                                                ui.dummy([0.0, 4.0]);
                                                let flip = if *rail_right { "Dock left" } else { "Dock right" };
                                                if btn(ui, "##dock", flip) {
                                                    *rail_right = !*rail_right;
                                                    crate::settings::set_rail_right(*rail_right);
                                                    *relayout = true;
                                                }
                                            }
                                            ui.dummy([0.0, 8.0]);
                                            ui.text_colored(DIM, "Open / close key");
                                            ui.same_line();
                                            menu_key_button(ui, true);
                                            ui.same_line();
                                            ui.text_colored(DIM, "(click, then press a key)");
                                            ui.dummy([0.0, 8.0]);
                                            let classic = crate::settings::classic_menu();
                                            if toggle_row(ui, "##classic", "Classic menu", classic, cw) {
                                                crate::settings::set_classic_menu(!classic);
                                            }
                                            ui.dummy([0.0, 2.0]);
                                            ui.text_colored(DIM, "Switch to the original basic menu.");
                                        }
                                        Ctrl::Custom(Custom::Credits) => {
                                            ui.dummy([0.0, 6.0]);
                                            if btn_primary(ui, "##kofi", "Support me on Ko-fi") {
                                                open_url("https://ko-fi.com/nighty33");
                                            }
                                            ui.dummy([0.0, 6.0]);
                                            if btn(ui, "##gh", "GitHub") {
                                                open_url("https://github.com/Nighty3333/Heaven-Internal-Public-Version-");
                                            }
                                            ui.same_line();
                                            if btn(ui, "##chl", "Changelog") {
                                                open_url(crate::update::RELEASES_URL);
                                            }
                                            ui.dummy([0.0, 8.0]);
                                            ui.text_colored(ACCENT, concat!("Heaven  v", env!("CARGO_PKG_VERSION")));
                                            ui.text_colored(DIM, "made by Night DC \u{00b7} nighty3333");
                                        }
                                        Ctrl::Custom(Custom::TeamTrials) => {
                                            ui.dummy([0.0, 6.0]);
                                            let tt = crate::settings::tt_capture();
                                            if toggle_row(ui, "##tt", "Capture results", tt, cw) {
                                                crate::settings::set_tt_capture(!tt);
                                            }
                                            if tt {
                                                ui.dummy([0.0, 2.0]);
                                                val(ui, GOOD, &format!("{} saved", crate::htt::saved()));
                                            }
                                        }
                                        #[allow(unreachable_patterns)]
                                        Ctrl::Custom(_) => {}
                                    }
                                }
                            });
                        }
                    }
                });
                // Content fade-in on tab switch: a backdrop-coloured scrim over the page that
                // fades from opaque → clear (~140 ms). Works over the hand-drawn cards too
                // (a global imgui alpha wouldn't touch the draw-list content).
                {
                    let fade = anim_step("tab_fade", 1.0, 16.0);
                    if fade < 0.999 {
                        ui.get_window_draw_list()
                            .add_rect(
                                page_top,
                                [p0[0] + wsz[0] - 8.0, p0[1] + wsz[1] - 8.0],
                                [0.05, 0.03, 0.10, (1.0 - fade) * 0.9],
                            )
                            .filled(true)
                            .build();
                    }
                }
                ui.columns(1, "##end", false);
                persist_window(ui, "menu"); // remember a user-resized menu size forever
            });
    }
}

impl HeavenOverlay {
    /// Classic "Controls" rail — the original basic menu, kept as an alternative to the premium
    /// sidebar. Plain imgui widgets; docks to the chosen screen edge. Toggled from either menu.
    fn draw_controls(&mut self, ui: &Ui, x: f32, cond: Condition) {
        let fps_now = self.fps_display.round() as i32; // real measured FPS (see render)
        // Re-sync the cached slider values from LIVE state every frame, so the classic menu
        // shows the PERSISTED settings (applied by apply_on_boot AFTER this overlay is built,
        // so the construction-time values are stale). Same fix the premium menu has.
        let cur_fps = crate::fps::current();
        self.fps_on = cur_fps != 0;
        if cur_fps > 0 {
            self.fps_val = cur_fps;
        }
        // Restore the user's saved classic-menu geometry (position + size) if they moved/resized
        // it — same persistence the premium menu has (it was missing here, so the classic menu
        // always reopened at its default docked spot). A relayout (edge toggle) still forces the
        // default docked position; otherwise we honor whatever the user set. Size defaults to
        // auto-height (0.0) until the user resizes the window.
        let [dw, dh] = ui.io().display_size;
        let saved_geo = crate::settings::win_rect("controls");
        let win_size = match saved_geo {
            // `.max()` on the upper bound keeps min<=max so clamp can't panic on a tiny display
            // (panic in a render hook = hard crash under panic=abort).
            Some(r) => [
                r[2].clamp(280.0, (dw - 28.0).max(280.0)),
                r[3].clamp(0.0, (dh - 28.0).max(0.0)),
            ],
            None => [400.0, 0.0],
        };
        let win_pos = if self.relayout {
            [x, 14.0]
        } else {
            saved_geo.map(|r| [r[0], r[1]]).unwrap_or([x, 14.0])
        };
        let relayout = &mut self.relayout;
        let rail_right = &mut self.rail_right;
        let fps_on = &mut self.fps_on;
        let fps_val = &mut self.fps_val;
        #[cfg(feature = "banner")]
        let self_banner_tex = self.banner_tex;
        #[cfg(feature = "banner")]
        let intro_done = &mut self.intro_done;
        #[cfg(feature = "banner")]
        let intro_force = &mut self.intro_force;

        ui.window("Heaven \u{00b7} Controls")
            .size(win_size, Condition::FirstUseEver)
            .position(win_pos, cond)
            .title_bar(false)
            .collapsible(false)
            .resizable(true)
            .build(|| {
                #[cfg(feature = "banner")]
                if let Some(tex) = self_banner_tex {
                    let ww = ui.window_size()[0];
                    let h = ww * (BANNER_H / BANNER_W);
                    ui.set_cursor_pos([0.0, 0.0]);
                    imgui::Image::new(tex, [ww, h]).build(ui);
                    ui.set_cursor_pos([12.0, h + 9.0]);
                }

                // Switch back to the premium menu.
                let mut classic = crate::settings::classic_menu();
                if ui.checkbox("Classic menu (uncheck for the new UI)", &mut classic) {
                    crate::settings::set_classic_menu(classic);
                }
                ui.separator();

                // Unified classic menu: same control source as the premium menu
                // (crate::menu_model::model()), grouped into collapsible categories so the list
                // isn't one giant scroll. Bespoke blocks reuse the classic-style code below.
                {
                    use crate::menu_model::{Ctrl, Custom};
                    let menu = crate::menu_model::model();
                    for (ti, t) in menu.iter().enumerate() {
                        let flags = if ti == 0 { imgui::TreeNodeFlags::DEFAULT_OPEN } else { imgui::TreeNodeFlags::empty() };
                        if !ui.collapsing_header(t.name, flags) {
                            continue;
                        }
                        for sec in &t.sections {
                            ui.text_colored(DIM, sec.title);
                            for c in &sec.controls {
                                match c {
                                    Ctrl::Toggle { id, label, get, set } => {
                                        let g = *get;
                                        let s = *set;
                                        let mut b = g();
                                        if ui.checkbox(&format!("{label}##{id}"), &mut b) {
                                            s(b);
                                            crate::settings::save_current();
                                        }
                                        ui.same_line();
                                        ui.text_colored(if b { GOOD } else { DIM }, if b { "ON" } else { "off" });
                                    }
                                    Ctrl::SliderF32 { id, min, max, get, set, unit, .. } => {
                                        let g = *get;
                                        let s = *set;
                                        let mut v = g();
                                        let tcol = if (v - 1.0).abs() < 0.05 { DIM } else { GOOD };
                                        ui.text_colored(tcol, format!("{:.1}{}", v, unit));
                                        ui.set_next_item_width(-1.0);
                                        if ui.slider(&format!("##{id}"), *min, *max, &mut v) {
                                            s(v);
                                            crate::settings::save_current();
                                        }
                                    }
                                    Ctrl::Cycle { id, label, label_of, next } => {
                                        let lo = *label_of;
                                        let nx = *next;
                                        ui.text_colored(DIM, *label);
                                        ui.same_line();
                                        if ui.button(&format!("{}##{}", lo(), id)) {
                                            nx();
                                            crate::settings::save_current();
                                        }
                                    }
                                    Ctrl::Button { id, label, action } => {
                                        let a = *action;
                                        if ui.button(&format!("{label}##{id}")) {
                                            a();
                                        }
                                    }
                                    Ctrl::Note(t) => {
                                        ui.text_colored(DIM, *t);
                                    }
                                    Ctrl::Custom(Custom::Fps) => {
                                        let cur = crate::fps::current();
                                        let mut capped = cur > 0;
                                        if ui.checkbox("Cap FPS##capc", &mut capped) {
                                            *fps_on = capped;
                                            crate::fps::set_cap(if capped { *fps_val } else { 0 });
                                            crate::settings::save_current();
                                        }
                                        ui.same_line();
                                        let (cap_txt, cap_col) = if cur < 0 {
                                            ("Unlimited".to_string(), GOOD)
                                        } else if cur == 0 {
                                            ("off".to_string(), DIM)
                                        } else {
                                            (format!("{cur}"), GOOD)
                                        };
                                        ui.text_colored(cap_col, cap_txt);
                                        ui.same_line();
                                        ui.text_colored(DIM, format!("\u{00b7} {fps_now} fps now"));
                                        let mut unlimited = cur < 0;
                                        if ui.checkbox("Unlimited##unlc", &mut unlimited) {
                                            *fps_on = false;
                                            crate::fps::set_cap(if unlimited { -1 } else { 0 });
                                            crate::settings::save_current();
                                        }
                                        ui.same_line();
                                        ui.text_colored(if unlimited { GOOD } else { DIM }, "no limit");
                                        ui.set_next_item_width(-1.0);
                                        if ui.slider("##fpscapc", 1, 300, fps_val) {
                                            *fps_on = true;
                                            crate::fps::set_cap(*fps_val);
                                            crate::settings::save_current();
                                        }
                                    }
                                    #[cfg(feature = "freecam")]
                                    Ctrl::Custom(Custom::Freecam) => {
                                        let mut fc = crate::freecam::is_enabled();
                                        if ui.checkbox("Race freecam##fcc", &mut fc) {
                                            crate::settings::set_freecam(fc);
                                        }
                                        ui.same_line();
                                        ui.text_colored(if fc { GOOD } else { DIM }, if fc { "ON" } else { "off" });
                                        if fc {
                                            if crate::freecam::is_follow() {
                                                ui.text_colored(ACCENT, format!("follow gate {}/{}", crate::freecam::target_gate(), crate::freecam::max_gate()));
                                                if ui.button("< prev Uma##pvc") {
                                                    crate::freecam::cycle_target(-1);
                                                }
                                                ui.same_line();
                                                if ui.button("next Uma >##nxc") {
                                                    crate::freecam::cycle_target(1);
                                                }
                                            }
                                            ui.text_colored(DIM, "[ ] Uma  arrows orbit/zoom  I/K height");
                                            draw_preset_manager(ui, 240.0);
                                            let pose = crate::freecam::captured_pose();
                                            if !pose.is_empty() {
                                                ui.text_colored(GOOD, pose);
                                            }
                                        }
                                    }
                                    #[cfg(feature = "banner")]
                                    Ctrl::Custom(Custom::Intro) => {
                                        if ui.button("Replay intro##ric") {
                                            *intro_done = false;
                                            *intro_force = true;
                                            crate::audio::play();
                                            crate::intro_player::start();
                                        }
                                        ui.same_line();
                                        let cap = crate::intro_player::is_captured();
                                        ui.text_colored(
                                            if !cap { DIM } else if *intro_done { DIM } else { GOOD },
                                            if !cap { "no device" } else if *intro_done { "idle" } else { "playing" },
                                        );
                                    }
                                    Ctrl::Custom(Custom::Updates) => {
                                        if ui.button("Releases##rlc") {
                                            open_url(crate::update::RELEASES_URL);
                                        }
                                        let ust = crate::update::status();
                                        if !ust.is_empty() {
                                            ui.text_colored(ACCENT, ust);
                                        }
                                    }
                                    Ctrl::Custom(Custom::AboutLayout) => {
                                        let cen = crate::settings::menu_centered();
                                        let mut cenm = cen;
                                        if ui.checkbox("Centered window##cenc", &mut cenm) {
                                            crate::settings::set_menu_centered(cenm);
                                            *relayout = true;
                                        }
                                        if !cen {
                                            let flip = if *rail_right { "<< Dock left" } else { "Dock right >>" };
                                            if ui.button(&format!("{flip}##dockc")) {
                                                *rail_right = !*rail_right;
                                                crate::settings::set_rail_right(*rail_right);
                                                *relayout = true;
                                            }
                                        }
                                        ui.text_colored(DIM, "Open / close key");
                                        ui.same_line();
                                        menu_key_button(ui, false);
                                    }
                                    Ctrl::Custom(Custom::Credits) => {
                                        let _kc = ui.push_style_color(StyleColor::Button, [0.96, 0.33, 0.33, 0.90]);
                                        let _kh = ui.push_style_color(StyleColor::ButtonHovered, [1.0, 0.45, 0.45, 1.0]);
                                        let _ka = ui.push_style_color(StyleColor::ButtonActive, [0.85, 0.25, 0.25, 1.0]);
                                        if ui.button("Support on Ko-fi##kofic") {
                                            open_url("https://ko-fi.com/nighty33");
                                        }
                                    }
                                    Ctrl::Custom(Custom::TeamTrials) => {
                                        let mut tt = crate::settings::tt_capture();
                                        if ui.checkbox("Team Trials##ttc", &mut tt) {
                                            crate::settings::set_tt_capture(tt);
                                        }
                                        ui.same_line();
                                        if tt {
                                            ui.text_colored(GOOD, format!("ON  ({} saved)", crate::htt::saved()));
                                        } else {
                                            ui.text_colored(DIM, "OFF");
                                        }
                                    }
                                    #[allow(unreachable_patterns)]
                                    Ctrl::Custom(_) => {}
                                }
                            }
                            ui.spacing();
                        }
                    }
                }

                ui.separator();
                ui.text_colored(DIM, ipc::status());
                ui.text_colored(ACCENT, concat!("Heaven v", env!("CARGO_PKG_VERSION")));
                ui.text_colored(DIM, "made by Night DC : nighty3333");
                persist_window(ui, "controls"); // remember the classic menu's moved/resized geometry
            });
    }
}

/// Open a URL in the user's default browser without flashing a console window.
/// `explorer.exe <url>` hands the URL to the default protocol handler.
fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer.exe").arg(url).spawn();
}

// ── Custom widgets (drawn by hand to match the Umamusume mockup) ─────────────────

/// Segoe MDL2 icon glyph for a sidebar category.
/// Index into `NAV_TEX` (image sidebar icons) for a section name. None → use the font glyph.
fn nav_icon_idx(name: &str) -> Option<usize> {
    // Reuse the existing baked crystal textures for the redesigned tab names (idx 2/6 freed up).
    Some(match name {
        "Gameplay" => 0,    // was Skip (Play crystal)
        "Performance" => 1, // unchanged
        "Visuals" => 3,     // was Intro (Video crystal)
        "Camera" => 4,      // unchanged
        "Interface" => 5,   // was Panels (ViewAll crystal)
        "About" => 7,       // unchanged
        _ => return None,
    })
}

fn cat_icon(name: &str) -> &'static str {
    match name {
        "Gameplay" => "\u{E768}",    // Play
        "Camera" => "\u{E722}",      // Camera
        "Visuals" => "\u{E790}",     // Brightness/visuals
        "Performance" => "\u{E9D9}", // Speed
        "Interface" => "\u{E8A9}",   // ViewAll
        "About" => "\u{E946}",       // Info
        _ => "\u{E700}",             // GlobalNav
    }
}

/// First-launch hint pill (top-center): "Press <key> to open Heaven". Shown only until the
/// user opens the menu once, so a closed menu with an unknown toggle key isn't a dead end.
fn draw_first_launch_hint(ui: &Ui) {
    let key = MENU_KEYS[menu_key_idx()].0;
    let label = format!("Press  {key}  to open Heaven");
    let [dw, _dh] = ui.io().display_size;
    let tw = ui.calc_text_size(&label)[0];
    let (padx, pady) = (16.0_f32, 8.0_f32);
    let w = tw + padx * 2.0;
    let h = ui.calc_text_size(&label)[1] + pady * 2.0;
    let x = ((dw - w) * 0.5).max(0.0);
    let y = 16.0;
    let dl = ui.get_background_draw_list();
    dl.add_rect([x, y], [x + w, y + h], [0.082, 0.047, 0.157, 0.92])
        .filled(true)
        .rounding(8.0)
        .build();
    dl.add_rect([x, y], [x + w, y + h], [0.843, 0.694, 0.365, 0.5])
        .rounding(8.0)
        .thickness(1.0)
        .build();
    dl.add_text([x + padx, y + pady], TEXT, &label);
}

thread_local! {
    // Per-section measured content height (keyed by section id), so a card can draw its
    // rounded background BEHIND the content. The bg is drawn first using last frame's height
    // (re-measured every frame), which settles in one frame and handles variable content.
    static CARD_H: std::cell::RefCell<std::collections::HashMap<&'static str, f32>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Wrap a section's content in a rounded card. `w` = card width, `key` = stable section id.
fn card<F: FnOnce()>(ui: &Ui, w: f32, key: &'static str, body: F) {
    let start = ui.cursor_screen_pos();
    let cached = CARD_H.with(|m| m.borrow().get(key).copied()).unwrap_or(60.0);
    let end = [start[0] + w, start[1] + cached];
    // Eased hover amount (~120 ms) — subtly lifts the glow + border when the mouse is over.
    let chv = anim_step(
        &format!("card_{key}"),
        if ui.is_mouse_hovering_rect(start, end) { 1.0 } else { 0.0 },
        16.0,
    );
    {
        let dl = ui.get_window_draw_list();
        // Soft magenta glow behind the card — a few expanding low-alpha rects approximate a
        // blurred drop-shadow, so the panel reads as floating above the page. Grows on hover.
        let gboost = 1.0 + 0.6 * chv;
        for k in 0..3 {
            let e = 2.0 + k as f32 * 2.8;
            let a = (0.055 - k as f32 * 0.014) * gboost;
            dl.add_rect([start[0] - e, start[1] - e], [end[0] + e, end[1] + e], [0.78, 0.40, 0.96, a])
                .filled(true)
                .rounding(18.0)
                .build();
        }
        dl.add_rect(start, end, CARD_BG).filled(true).rounding(16.0).build();
        // Top highlight: a faint lighter band along the upper edge (lit-from-above look).
        dl.add_rect([start[0] + 1.0, start[1] + 1.0], [end[0] - 1.0, start[1] + 12.0], [1.0, 1.0, 1.0, 0.05])
            .filled(true)
            .rounding(15.0)
            .round_bot_left(false)
            .round_bot_right(false)
            .build();
        // Border brightens slightly on hover.
        let border = lerp_col(CARD_BORDER, [0.70, 0.55, 0.95, 0.55], chv);
        dl.add_rect(start, end, border).rounding(16.0).thickness(1.0 + 0.3 * chv).build();
    } // release the draw list before the body draws its own widgets
    ui.set_cursor_screen_pos([start[0] + 22.0, start[1] + 18.0]);
    ui.group(body);
    let measured = (ui.item_rect_max()[1] - start[1]) + 18.0;
    CARD_H.with(|m| {
        m.borrow_mut().insert(key, measured);
    });
    ui.set_cursor_screen_pos([start[0], start[1] + cached.max(measured) + 14.0]);
}

/// A section header: an icon in a soft rounded badge, then an accent-coloured title.
fn section(ui: &Ui, icon_font: Option<imgui::FontId>, glyph: &str, title: &str) {
    if let Some(f) = icon_font {
        let p = ui.cursor_screen_pos();
        let b = 26.0; // badge size
        ui.get_window_draw_list()
            .add_rect([p[0], p[1] - 2.0], [p[0] + b, p[1] - 2.0 + b], BADGE_BG)
            .filled(true)
            .rounding(7.0)
            .build();
        ui.set_cursor_screen_pos([p[0] + 5.0, p[1] + 3.0]);
        {
            let _t = ui.push_font(f);
            ui.text_colored(ACCENT, glyph);
        }
        ui.set_cursor_screen_pos([p[0] + b + 9.0, p[1] + 4.0]);
    }
    if let Some(tf) = TITLE_FONT.with(|c| c.get()) {
        let _t = ui.push_font(tf);
        ui.text_colored(ACCENT, title);
    } else {
        ui.text_colored(ACCENT, title);
    }
    // Elegant gold divider line under the title.
    if let Some(d) = DIVIDER_TEX.with(|c| c.get()) {
        ui.new_line();
        let aw = ui.content_region_avail()[0];
        let p = ui.cursor_screen_pos();
        let dh = 12.0;
        ui.get_window_draw_list()
            .add_image(d, [p[0], p[1]], [p[0] + aw, p[1] + dh])
            .build();
        ui.dummy([0.0, dh + 2.0]);
    }
}

/// Heaven pill toggle. ON = pink track with a magenta glow + gold-ringed knob; OFF = a purple
/// "crystal" track. Returns true on the frame it was clicked.
fn pill_toggle(ui: &Ui, id: &str, on: bool) -> bool {
    let (w, h, rad) = (54.0, 26.0, 13.0);
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [w, h]);
    let hov = ui.is_item_hovered();
    // Eased ON amount + hover amount → knob glide (~130 ms ease-out), colour/glow cross-fade.
    let t = anim_step(&format!("tg{id}"), if on { 1.0 } else { 0.0 }, 19.0);
    let hv = anim_step(&format!("tgh{id}"), if hov { 1.0 } else { 0.0 }, 16.0);
    let dl = ui.get_window_draw_list();
    let cy = p[1] + h * 0.5;

    // ── 1. Outer magenta glow (ON), a touch larger on hover ──
    if t > 0.01 {
        let gboost = 1.0 + 0.5 * hv;
        for k in 0..3 {
            let e = 1.5 + k as f32 * 2.8;
            let a = ((0.17 - k as f32 * 0.05) * t * gboost).max(0.0);
            dl.add_rect([p[0] - e, p[1] - e], [p[0] + w + e, p[1] + h + e], [1.0, 0.34, 0.78, a])
                .filled(true)
                .rounding(rad + e)
                .build();
        }
    }

    // ── 2. Glass body: dark-purple crystal (OFF) cross-fading to pink (ON) ──
    let body = lerp_col([0.20, 0.15, 0.34, 1.0], [0.93, 0.37, 0.73, 1.0], t);
    dl.add_rect(p, [p[0] + w, p[1] + h], body).filled(true).rounding(rad).build();
    // Horizontal magenta→pink gradient streak through the middle (stays inside the rounded
    // body, so no square corners) — gives the ON state its premium gradient feel.
    if t > 0.01 {
        let gl = [1.0, 0.30, 0.60, 0.55 * t];
        let gr = [0.83, 0.44, 1.0, 0.55 * t];
        dl.add_rect_filled_multicolor([p[0] + 8.0, cy - 7.0], [p[0] + w - 8.0, cy + 7.0], gl, gr, gr, gl);
    }
    // Crystal sheen along the top edge.
    let sheen = 0.10 + 0.08 * t;
    dl.add_rect_filled_multicolor(
        [p[0] + 4.0, p[1] + 2.0],
        [p[0] + w - 4.0, cy],
        [1.0, 1.0, 1.0, sheen],
        [1.0, 1.0, 1.0, sheen],
        [1.0, 1.0, 1.0, 0.0],
        [1.0, 1.0, 1.0, 0.0],
    );

    // ── 3. Border: subtle violet (OFF) → thin gold (ON), brighter on hover ──
    let border = lerp_col(
        [0.56, 0.49, 0.75, 0.40 + 0.28 * hv],
        [0.843, 0.694, 0.365, 0.88],
        t,
    );
    dl.add_rect(p, [p[0] + w, p[1] + h], border).rounding(rad).thickness(1.4).build();

    // ── 4. Knob: slides L↔R, white/lilac core, gold ring on (lilac on hover off) ──
    let r = 11.0;
    let travel = w - 2.0 * (r + 3.0);
    let kx = p[0] + (r + 3.0) + t * travel;
    // soft drop shadow under the knob
    dl.add_circle([kx, cy + 1.0], r, [0.0, 0.0, 0.0, 0.18]).filled(true).build();
    // knob body (white with a hint of lilac) + inner highlight
    dl.add_circle([kx, cy], r, [1.0, 0.98, 1.0, 1.0]).filled(true).build();
    dl.add_circle([kx - 1.6, cy - 1.6], r * 0.45, [1.0, 1.0, 1.0, 0.9]).filled(true).build();
    // ring
    let ring = lerp_col([0.78, 0.62, 1.0, 0.32 + 0.5 * hv], [0.843, 0.694, 0.365, 1.0], t);
    dl.add_circle([kx, cy], r, ring).thickness(2.0).build();
    clicked
}

/// A label row with a pill toggle right-aligned at `row_w`. Returns true if clicked.
/// The toggle is 54 px wide plus a glow halo, so it sits ~28 px in from the card's right edge.
fn toggle_row(ui: &Ui, id: &str, label: &str, on: bool, row_w: f32) -> bool {
    ui.text(label);
    ui.same_line_with_pos(row_w - 82.0);
    let cp = ui.cursor_screen_pos();
    ui.set_cursor_screen_pos([cp[0], cp[1] - 3.0]);
    pill_toggle(ui, id, on)
}

/// Pink gradient slider drawn at the cursor. Mutates `val` while dragged; returns true on change.
fn pink_slider_f32(ui: &Ui, id: &str, min: f32, max: f32, val: &mut f32, w: f32) -> bool {
    let h = 20.0;
    let p = ui.cursor_screen_pos();
    ui.invisible_button(id, [w, h]);
    let active = ui.is_item_active();
    let mut changed = false;
    if active {
        let mx = ui.io().mouse_pos[0];
        let t = ((mx - p[0]) / w).clamp(0.0, 1.0);
        let nv = min + t * (max - min);
        if (nv - *val).abs() > f32::EPSILON {
            *val = nv;
            changed = true;
        }
    }
    // Eased handle highlight (hover or drag) → small gold/magenta glow.
    let hv = anim_step(&format!("slh{id}"), if ui.is_item_hovered() || active { 1.0 } else { 0.0 }, 16.0);
    let t_real = ((*val - min) / (max - min)).clamp(0.0, 1.0);
    // Smooth the drawn position toward the real value so the fill/knob glide.
    let t = anim_step(&format!("sl{id}"), t_real, 24.0);
    let cy = p[1] + h * 0.5;
    let fx = p[0] + w * t;
    let dl = ui.get_window_draw_list();
    dl.add_rect([p[0], cy - 3.0], [p[0] + w, cy + 3.0], TRACK_BG)
        .filled(true)
        .rounding(3.0)
        .build();
    dl.add_rect_filled_multicolor([p[0], cy - 3.0], [fx, cy + 3.0], GRAD_L, GRAD_R, GRAD_R, GRAD_L);
    // Handle glow (fades in on hover/drag).
    if hv > 0.01 {
        dl.add_circle([fx, cy], 8.0 + 6.0 * hv, [1.0, 0.42, 0.85, 0.22 * hv]).filled(true).build();
        dl.add_circle([fx, cy], 8.0 + 3.0 * hv, [0.90, 0.72, 0.40, 0.25 * hv]).filled(true).build();
    }
    let kr = 8.0 + 1.0 * hv;
    dl.add_circle([fx, cy], kr, [1.0, 1.0, 1.0, 1.0]).filled(true).build();
    dl.add_circle([fx, cy], kr, GOLD).thickness(2.0).build();
    changed
}

/// Rounded button with an accent border + hover, auto-sized to its label. Returns clicked.
fn btn(ui: &Ui, id: &str, label: &str) -> bool {
    let (pad, h) = (15.0, 32.0);
    let ts = ui.calc_text_size(label);
    let w = ts[0] + pad * 2.0;
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [w, h]);
    let hov = ui.is_item_hovered();
    let dl = ui.get_window_draw_list();
    dl.add_rect(p, [p[0] + w, p[1] + h], if hov { BTN_HI } else { BTN_BG })
        .filled(true)
        .rounding(9.0)
        .build();
    dl.add_rect(p, [p[0] + w, p[1] + h], [0.60, 0.46, 0.90, if hov { 0.65 } else { 0.32 }])
        .rounding(9.0)
        .thickness(1.2)
        .build();
    dl.add_text([p[0] + pad, p[1] + (h - ts[1]) * 0.5], TEXT, label);
    clicked
}

/// Primary (filled pink) button, auto-sized. For the Ko-fi support button.
fn btn_primary(ui: &Ui, id: &str, label: &str) -> bool {
    let (pad, h) = (16.0, 34.0);
    let ts = ui.calc_text_size(label);
    let w = ts[0] + pad * 2.0;
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [w, h]);
    let hov = ui.is_item_hovered();
    let fill = if hov { [0.96, 0.50, 0.74, 1.0] } else { PINK };
    let dl = ui.get_window_draw_list();
    dl.add_rect(p, [p[0] + w, p[1] + h], fill).filled(true).rounding(9.0).build();
    dl.add_text([p[0] + pad, p[1] + (h - ts[1]) * 0.5], [1.0, 1.0, 1.0, 1.0], label);
    clicked
}

/// i32 variant of [`pink_slider_f32`].
fn pink_slider_i32(ui: &Ui, id: &str, min: i32, max: i32, val: &mut i32, w: f32) -> bool {
    let mut f = *val as f32;
    let changed = pink_slider_f32(ui, id, min as f32, max as f32, &mut f, w);
    if changed {
        *val = f.round() as i32;
    }
    changed
}



// ── Heaven-styled panel helpers (match the menu: glass, gradient bars, Orbitron) ──

/// Push the glass-window style used by the info panels. Tokens must outlive the window.
#[cfg(any(feature = "panels", feature = "freecam"))]
fn panel_style(ui: &Ui) -> impl Sized + '_ {
    (
        ui.push_style_color(StyleColor::WindowBg, [0.082, 0.047, 0.157, 0.97]),
        ui.push_style_color(StyleColor::Border, CARD_BORDER),
        ui.push_style_var(StyleVar::WindowRounding(14.0)),
        ui.push_style_var(StyleVar::WindowBorderSize(1.5)),
        ui.push_style_var(StyleVar::WindowPadding([14.0, 12.0])),
    )
}

/// A section title in the Cinzel face (accent colour).
#[cfg(any(feature = "panels", feature = "freecam"))]
fn panel_title(ui: &Ui, text: &str) {
    if let Some(tf) = TITLE_FONT.with(|c| c.get()) {
        let _t = ui.push_font(tf);
        ui.text_colored(ACCENT, text);
    } else {
        ui.text_colored(ACCENT, text);
    }
}

/// A rounded "pill" bar (track + filled portion), optionally with centred overlay text.
#[cfg(any(feature = "panels", feature = "freecam"))]
fn pbar(ui: &Ui, frac: f32, w: f32, h: f32, col: [f32; 4], overlay: &str) {
    let p = ui.cursor_screen_pos();
    {
        let dl = ui.get_window_draw_list();
        dl.add_rect(p, [p[0] + w, p[1] + h], TRACK_BG).filled(true).rounding(h * 0.5).build();
        let f = frac.clamp(0.0, 1.0);
        if f > 0.0 {
            let fw = (w * f).max(h);
            dl.add_rect(p, [p[0] + fw, p[1] + h], col).filled(true).rounding(h * 0.5).build();
        }
    }
    if !overlay.is_empty() {
        let ts = ui.calc_text_size(overlay);
        ui.get_window_draw_list()
            .add_text([p[0] + (w - ts[0]) * 0.5, p[1] + (h - ts[1]) * 0.5], [1.0, 1.0, 1.0, 0.95], overlay);
    }
    ui.dummy([w, h]);
}




/// Per-circuit camera preset manager — a custom animated dropdown (hover-lit rows, eased open,
/// gold caret) listing this circuit's presets, with rename of the selected one + Default / Delete
/// / Add. Keys: O cycles presets live, P saves the current pose into the active one. Width `w`.
#[cfg(feature = "freecam")]
fn draw_preset_manager(ui: &Ui, w: f32) {
    use std::cell::{Cell, RefCell};
    thread_local! {
        static OPEN: Cell<bool> = const { Cell::new(false) };
        static RBUF: RefCell<String> = const { RefCell::new(String::new()) };
        static RIDX: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    let names = crate::freecam::preset_names();
    let active = crate::freecam::preset_active().min(names.len().saturating_sub(1));
    let def = crate::freecam::preset_default();
    let track = crate::freecam::preset_track();

    ui.text_colored(DIM, "Camera presets");
    ui.same_line();
    ui.text_colored(DIM, format!("\u{00b7}  O cycle  \u{00b7}  P save"));

    // ── dropdown header (shows the active preset) ──
    let cur = names.get(active).cloned().unwrap_or_else(|| "— no presets —".into());
    let h = 30.0;
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button("##ddhdr", [w, h]);
    let hov = ui.is_item_hovered();
    let hh = anim_step("ddhdrh", if hov { 1.0 } else { 0.0 }, 16.0);
    let open = OPEN.with(|o| o.get());
    {
        let dl = ui.get_window_draw_list();
        dl.add_rect(p, [p[0] + w, p[1] + h], if hov { BTN_HI } else { BTN_BG }).filled(true).rounding(9.0).build();
        dl.add_rect(p, [p[0] + w, p[1] + h], [0.60, 0.46, 0.90, 0.32 + 0.33 * hh]).rounding(9.0).thickness(1.2).build();
        dl.add_text([p[0] + 12.0, p[1] + (h - 14.0) * 0.5], TEXT, &cur);
        // gold caret (up when open, down when closed)
        let (cx, cy) = (p[0] + w - 16.0, p[1] + h * 0.5);
        if open {
            dl.add_triangle([cx - 5.0, cy + 3.0], [cx + 5.0, cy + 3.0], [cx, cy - 4.0], GOLD).filled(true).build();
        } else {
            dl.add_triangle([cx - 5.0, cy - 3.0], [cx + 5.0, cy - 3.0], [cx, cy + 4.0], GOLD).filled(true).build();
        }
    }
    if clicked {
        OPEN.with(|o| o.set(!open));
    }

    // ── open list (rows with hover highlight) ──
    if open && !names.is_empty() {
        for (i, name) in names.iter().enumerate() {
            let rh = 26.0;
            let rp = ui.cursor_screen_pos();
            let rc = ui.invisible_button(format!("##ddr{i}"), [w, rh]);
            let rhov = ui.is_item_hovered();
            let hl = anim_step(&format!("ddrh{i}"), if rhov { 1.0 } else { 0.0 }, 18.0);
            {
                let dl = ui.get_window_draw_list();
                if hl > 0.01 {
                    dl.add_rect(rp, [rp[0] + w, rp[1] + rh], [0.60, 0.46, 0.90, 0.20 * hl]).filled(true).rounding(7.0).build();
                }
                if i == active {
                    dl.add_circle([rp[0] + 11.0, rp[1] + rh * 0.5], 3.0, GOLD).filled(true).build();
                }
                dl.add_text([rp[0] + 24.0, rp[1] + (rh - 14.0) * 0.5], if i == active { GOLD } else { TEXT }, name);
                if i == def {
                    let t = "default";
                    let ts = ui.calc_text_size(t);
                    dl.add_text([rp[0] + w - ts[0] - 12.0, rp[1] + (rh - 14.0) * 0.5], ACCENT, t);
                }
            }
            if rc {
                crate::freecam::preset_apply_idx(i);
                OPEN.with(|o| o.set(false));
            }
        }
    }

    // ── selected-preset management (rename + default/delete) ──
    if !names.is_empty() {
        ui.dummy([0.0, 4.0]);
        // keep the rename buffer synced to the active preset
        if RIDX.with(|r| r.get()) != active {
            RIDX.with(|r| r.set(active));
            RBUF.with(|b| *b.borrow_mut() = names[active].clone());
        }
        RBUF.with(|b| {
            let mut s = b.borrow_mut();
            ui.set_next_item_width(w);
            if ui.input_text("##presetname", &mut s).hint("preset name").build() {
                crate::freecam::preset_rename(active, &s);
            }
        });
        ui.dummy([0.0, 2.0]);
        if def_btn(ui, "##setdef", "Default", active == def) {
            crate::freecam::preset_set_default(active);
        }
        ui.same_line();
        if btn(ui, "##delpreset", "Delete") {
            crate::freecam::preset_delete(active);
        }
    }
    if names.len() < 4 && track != 0 {
        ui.dummy([0.0, 2.0]);
        if btn(ui, "##addpreset", "+ Add current view") {
            let n = format!("Preset {}", names.len() + 1);
            crate::freecam::preset_add(&n);
        }
    }
}

/// A small button that reads as "selected" (gold border/fill) when `on`. Returns clicked.
#[cfg(feature = "freecam")]
fn def_btn(ui: &Ui, id: &str, label: &str, on: bool) -> bool {
    let (pad, h) = (14.0, 30.0);
    let ts = ui.calc_text_size(label);
    let w = ts[0] + pad * 2.0 + if on { 16.0 } else { 0.0 };
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [w, h]);
    let hov = ui.is_item_hovered();
    let dl = ui.get_window_draw_list();
    let bg = if on { [0.30, 0.24, 0.12, 1.0] } else if hov { BTN_HI } else { BTN_BG };
    dl.add_rect(p, [p[0] + w, p[1] + h], bg).filled(true).rounding(9.0).build();
    dl.add_rect(p, [p[0] + w, p[1] + h], if on { GOLD } else { [0.60, 0.46, 0.90, if hov { 0.65 } else { 0.32 }] })
        .rounding(9.0)
        .thickness(if on { 1.6 } else { 1.2 })
        .build();
    let mut tx = p[0] + pad;
    if on {
        dl.add_circle([p[0] + pad + 4.0, p[1] + h * 0.5], 3.0, GOLD).filled(true).build();
        tx += 16.0;
    }
    dl.add_text([tx, p[1] + (h - ts[1]) * 0.5], if on { GOLD } else { TEXT }, label);
    clicked
}

/// Race grade → (label, badge colour). None for grades we don't badge (Maiden/Debut/etc.).
#[cfg(feature = "freecam")]
fn grade_label(g: i32) -> Option<(&'static str, [f32; 4])> {
    match g {
        100 => Some(("G1", [0.92, 0.28, 0.34, 1.0])), // red
        200 => Some(("G2", [0.66, 0.70, 0.82, 1.0])), // silver
        300 => Some(("G3", [0.80, 0.56, 0.32, 1.0])), // bronze
        400 => Some(("OP", [0.40, 0.62, 0.92, 1.0])), // blue (Open)
        _ => None,
    }
}
/// Draw a small rounded grade pill at the cursor (e.g. "G1") with white text.
#[cfg(feature = "freecam")]
fn grade_badge(ui: &Ui, label: &str, col: [f32; 4]) {
    let (pad, h) = (6.0, 18.0);
    let ts = ui.calc_text_size(label);
    let w = ts[0] + pad * 2.0;
    let p = ui.cursor_screen_pos();
    ui.dummy([w, h]);
    let dl = ui.get_window_draw_list();
    dl.add_rect(p, [p[0] + w, p[1] + h], col).filled(true).rounding(5.0).build();
    dl.add_text([p[0] + pad, p[1] + (h - ts[1]) * 0.5], [1.0, 1.0, 1.0, 1.0], label);
}

/// Texture for a skill's icon (skill_id → icon_id → texture), if extracted.
#[cfg(feature = "freecam")]
fn skill_icon_tex(skill_id: i32) -> Option<imgui::TextureId> {
    let icon_id = SKILL_ICON_MAP.with(|m| m.borrow().get(&skill_id).copied())?;
    SKILL_TEX.with(|m| m.borrow().get(&icon_id).copied())
}
/// Texture for an Uma's portrait (charaId → texture), if extracted.
#[cfg(feature = "freecam")]
fn uma_icon_tex(chara_id: i32) -> Option<imgui::TextureId> {
    UMA_TEX.with(|m| m.borrow().get(&chara_id).copied())
}

/// Freecam live telemetry — the followed Uma's stamina/speed/rank + a comparison to the
/// adjacent rival (the one directly ahead, or the one behind if we're leading). Tied to the
/// freecam target, so `[ ]` switches both the camera and this readout. Heaven-themed (drawn,
/// no image assets). Data comes from `freecam::telemetry()` (live HorseRaceInfo reads).
#[cfg(feature = "freecam")]
fn draw_freecam_telemetry(ui: &Ui, x: f32, y: f32, cond: Condition) {
    let tv = match crate::freecam::telemetry() {
        Some(t) => t,
        None => return,
    };
    let f = tv.followed;
    let sta = |hp: f32, max: f32| -> f32 {
        if max > 0.0 { (hp / max).clamp(0.0, 1.0) } else { 0.0 }
    };
    let bar_col = |r: f32| if r > 0.5 { GOOD } else if r > 0.25 { WARN } else { BAD };

    // Fixed columns (bar never overlaps label/value). c_bar wide enough for "vs rival".
    let c_bar = 84.0;
    let bar_w = 104.0;
    let c_val = 196.0;
    let row = |ui: &Ui, label: &str, ratio: f32, speed: f32| {
        ui.text_colored(DIM, label);
        ui.same_line_with_pos(c_bar);
        pbar(ui, ratio, bar_w, 13.0, bar_col(ratio), &format!("{:.0}%", ratio * 100.0));
        ui.same_line_with_pos(c_val);
        val(ui, BLUE, &format!("{:.1} m/s", speed));
    };

    // Auto-resize (grows as skills fire — no scrollbar). Only the POSITION is remembered: the
    // user can move it and it reopens there; the size always fits the content.
    let _ = cond;
    let saved = crate::settings::win_rect("telemetry");
    let (px, py) = saved.map(|r| (r[0], r[1])).unwrap_or((x, y));
    let _style = panel_style(ui);
    ui.window("Heaven \u{00b7} Freecam Telemetry")
        .position([px, py], Condition::FirstUseEver)
        .title_bar(false)
        .always_auto_resize(true)
        .build(|| {
            panel_title(ui, "LIVE TELEMETRY");
            // Race header: "Hanshin 1600m Mile"
            let header = crate::race::race_header();
            if !header.is_empty() {
                ui.same_line();
                ui.text_colored(GOLD, &header);
            }
            if let Some((lbl, col)) = grade_label(crate::race::race_grade()) {
                ui.same_line();
                grade_badge(ui, lbl, col);
            }
            ui.dummy([0.0, 4.0]);

            // ── followed Uma ── portrait + name + rank + SPURT badge
            if let Some(tex) = uma_icon_tex(tv.chara_id) {
                imgui::Image::new(tex, [42.0, 42.0]).build(ui);
                ui.same_line();
                let cp = ui.cursor_screen_pos();
                ui.set_cursor_screen_pos([cp[0], cp[1] + 13.0]); // center the name to the bigger icon
            }
            ui.text_colored(GOLD, &tv.followed_name);
            ui.same_line();
            val(ui, ACCENT, &format!("P{}/{}", f.order.max(1), tv.field_size));
            if f.spurt {
                ui.same_line();
                val(ui, PINK, "SPURT");
            }
            if f.exhausted {
                ui.same_line();
                val(ui, BAD, "EXHAUSTED");
            }
            let fr = sta(f.hp, f.max_hp);
            row(ui, "Stamina", fr, f.speed);
            // progress (distance covered, % of course if known) + skills fired
            let course = crate::race::course_distance();
            let prog = if course > 0 {
                format!("{:.0}/{}m ({:.0}%)", f.distance, course, (f.distance / course as f32 * 100.0).clamp(0.0, 100.0))
            } else {
                format!("{:.0} m", f.distance)
            };
            ui.text_colored(DIM, &prog);

            ui.dummy([0.0, 6.0]);

            // ── rival comparison (no ▲/▼ glyphs — the body font lacks them) ──
            match tv.rival {
                Some(r) => {
                    if tv.rival_ahead {
                        ui.text_colored(WARN, format!("{:.1} m behind P{}", tv.gap, r.order.max(1)));
                    } else {
                        ui.text_colored(GOOD, format!("Leading by {:.1} m", tv.gap));
                    }
                    ui.same_line();
                    ui.text_colored(DIM, &tv.rival_name);
                    let rr = sta(r.hp, r.max_hp);
                    row(ui, "Rival", rr, r.speed);

                    // deltas (us − rival)
                    ui.dummy([0.0, 3.0]);
                    let dspd = f.speed - r.speed;
                    let dsta = (fr - rr) * 100.0;
                    let dcol = |v: f32| if v >= 0.0 { GOOD } else { BAD };
                    ui.text_colored(DIM, "vs rival");
                    ui.same_line_with_pos(c_bar);
                    val(ui, dcol(dspd), &format!("{dspd:+.1} m/s"));
                    ui.same_line_with_pos(c_val);
                    val(ui, dcol(dsta), &format!("{dsta:+.0}% sta"));
                }
                None => ui.text_colored(DIM, "(no rival in range)"),
            }

            // ── skill activation feed (selected Uma only; window grows as they fire) ──
            let feed = crate::freecam::skill_feed();
            if !feed.is_empty() {
                ui.dummy([0.0, 5.0]);
                ui.text_colored(GOLD, &format!("SKILLS ({})", feed.len()));
                for (id, name) in feed.iter() {
                    ui.group(|| {
                        if let Some(tex) = skill_icon_tex(*id) {
                            imgui::Image::new(tex, [18.0, 18.0]).build(ui);
                            ui.same_line();
                            let cp = ui.cursor_screen_pos();
                            ui.set_cursor_screen_pos([cp[0], cp[1] + 2.0]);
                            ui.text_colored(ACCENT, name);
                        } else {
                            // No icon → keep the dotted text line (default font has "·").
                            ui.text_colored(ACCENT, &format!("\u{00b7} {name}"));
                        }
                    });
                    // Hover → show what the skill does (from the extracted descriptions).
                    if ui.is_item_hovered() {
                        if let Some(desc) = SKILL_DESC.with(|m| m.borrow().get(id).cloned()) {
                            ui.tooltip(|| {
                                ui.dummy([280.0, 0.0]); // pin tooltip width so the text wraps
                                ui.text_colored(GOLD, name);
                                ui.text_wrapped(&desc);
                            });
                        }
                    }
                }
            }

            ui.dummy([0.0, 4.0]);
            ui.text_colored(DIM, "[ ] switch Uma  \u{00b7}  arrows/I-K move  \u{00b7}  P save default");
            persist_window(ui, "telemetry");
        });
}

