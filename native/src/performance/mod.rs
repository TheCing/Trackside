//! Performance / graphics tweak modules.
//!
//! Groups the four single-responsibility knobs that ship in every build:
//! - [`fps`]      — FPS unlock (targetFrameRate/vSyncCount clamp hooks)
//! - [`graphics`] — model quality (ApplyGraphicsQuality, aniso/LOD/shadow, low-spec pass)
//! - [`cyspring`] — cloth physics (CySpringController.Init UpdateMode, low-spec 60fps cap)
//! - [`display`]  — window/resolution QoL (always-on-top, SetResolution, UI-scale)

pub mod fps;
pub mod graphics;
pub mod cyspring;
pub mod display;

/// Low-resources "potato" master mode: fan out to every subsystem that reacts to it.
pub fn set_low_spec(on: bool) {
    graphics::set_low_spec(on);
    cyspring::set_low_spec(on);
    display::set_low_spec(on);
}

/// Apply persisted settings to the whole performance domain at boot.
pub fn apply(s: &crate::settings::Settings) {
    fps::set_cap(s.fps);
    cyspring::set_enabled(s.cyspring_uncap);
    graphics::set_quality_unlocked(s.gfx_quality);
    graphics::set_extras_enabled(s.gfx_extras);
    display::set_block_minimize(s.block_minimize);
    display::set_display_mode(s.display_mode);
    display::set_render_scale(s.render_scale);
    display::set_ui_scale(s.ui_scale);
    display::set_always_on_top(s.always_on_top);
    set_low_spec(s.low_spec);
}
