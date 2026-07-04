//! menu_model — the SINGLE source of truth for the overlay menu's controls.
//!
//! Both renderers (premium `overlay::draw_menu` and the classic `overlay::draw_controls`)
//! consume `model()`. Neither defines its own control list any more, so the two menus can
//! no longer drift apart — adding or moving a control is a one-line edit here and it shows
//! up in both styles automatically.
//!
//! This file is LOGIC ONLY. Every premium visual (Cinzel/Inter/Orbitron fonts, glass icons,
//! sakura petals, the silhouette, animated cards + pills, background textures) lives in the
//! renderers and is untouched. The model just says WHAT controls exist and HOW they're wired
//! to their getters/setters; each renderer decides HOW to draw them.
//!
//! Anything too bespoke for a generic widget (the freecam follow/preset panel, the tri-state
//! FPS card, the intro/updates/about blocks) is represented by `Ctrl::Custom(..)` and drawn by
//! the renderer's existing hand-written code — so nothing premium is lost.

#![allow(dead_code)]

/// One control in a section. Getters/setters are plain module fns (no captures).
pub enum Ctrl {
    /// On/off. Renderer flips it: `set(!get())` then persists.
    Toggle {
        id: &'static str,
        label: &'static str,
        get: fn() -> bool,
        set: fn(bool),
    },
    /// Float slider with a unit suffix for the readout (e.g. "x").
    SliderF32 {
        id: &'static str,
        label: &'static str,
        min: f32,
        max: f32,
        get: fn() -> f32,
        set: fn(f32),
        unit: &'static str,
    },
    /// Cycles through a fixed set of states on click (e.g. screen mode).
    Cycle {
        id: &'static str,
        label: &'static str,
        label_of: fn() -> &'static str,
        next: fn(),
    },
    /// A plain action button.
    Button {
        id: &'static str,
        label: &'static str,
        action: fn(),
    },
    /// Static descriptive line under a section header.
    Note(&'static str),
    /// Hand-drawn block the renderer dispatches to its own bespoke code (preserves
    /// every premium custom widget unchanged).
    Custom(Custom),
}

/// Bespoke blocks each renderer draws with its existing code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Custom {
    Fps,             // tri-state cap / unlimited / slider + live readout
    Freecam,         // enable + follow controls + preset manager
    KeyBinds,        // rebindable freecam key bindings
    TeamTrials,      // capture toggle + "N saved" count
    TtPadder,        // Team Trials deck profiles — snapshot + 1-click swap
    TtHunter,        // Team Trials opponent hunter — auto-refresh until a target appears
    Affinity,        // Legacy Select succession-affinity numbers — enable + drag-to-place + size
    Intro,           // intro status + replay button
    Updates,         // version + check/pull/releases
    AboutLayout,     // centered / dock side / toggle-key rebind / classic-menu toggle
    Credits,         // Ko-fi / GitHub / changelog / version / author
}

pub struct Section {
    pub title: &'static str,
    /// MDL2 glyph (drawn with the icon font by the premium renderer; classic ignores it).
    /// Distinct per section — replaces the repeated gear icon. Tweak freely.
    pub icon: char,
    pub blurb: &'static str,
    pub controls: Vec<Ctrl>,
}

pub struct Tab {
    pub name: &'static str,
    pub icon: char,
    pub sections: Vec<Section>,
}

// ── compound actions that need a wrapper (fn pointers can't capture) ──────────
fn cycle_display_mode() {
    crate::settings::set_display_mode((crate::settings::display_mode() + 1) % 4);
}
fn display_mode_label() -> &'static str {
    match crate::settings::display_mode() {
        1 => "Borderless",
        2 => "Exclusive",
        3 => "Windowed",
        _ => "Default",
    }
}

/// Build the whole menu. Order = display order. cfg-gated controls simply aren't pushed
/// in builds that lack the feature, so both renderers stay correct per build.
pub fn model() -> Vec<Tab> {
    let mut tabs: Vec<Tab> = Vec::new();

    // ── 1) GAMEPLAY ──────────────────────────────────────────────────────────
    #[allow(unused_mut)]
    let mut gameplay = vec![
        Section {
            title: "Superskip",
            icon: '\u{E768}',
            blurb: "Skip events, training cut-ins and race results.",
            controls: vec![
                Ctrl::Toggle { id: "ev", label: "Events", get: crate::skip::is_event_enabled, set: crate::skip::set_event_enabled },
                Ctrl::Toggle { id: "tr", label: "Training", get: crate::skip::is_train_enabled, set: crate::skip::set_train_enabled },
                Ctrl::Toggle { id: "rr", label: "Races (won only)", get: crate::skip::is_race_result_enabled, set: crate::skip::set_race_result_enabled },
                Ctrl::Toggle { id: "sh", label: "Shop", get: crate::skip::is_shop_enabled, set: crate::skip::set_shop_enabled },
            ],
        },
        Section {
            title: "Game speed",
            icon: '\u{E916}',
            blurb: "Speed up UI animations and story / event text.",
            controls: vec![Ctrl::SliderF32 {
                id: "speed", label: "Speed", min: 1.0, max: 10.0,
                get: crate::ui_tempo::tempo, set: crate::ui_tempo::set_tempo, unit: "x",
            }],
        },
    ];
    tabs.push(Tab { name: "Gameplay", icon: '\u{E768}', sections: gameplay });

    // ── 1b) TEAM TRIALS ──────────────────────────────────────────────────────
    // Deck profiles: snapshot the current 15-Uma team and swap it back with one click
    // (good team <-> padding team). Up to 5 renameable profiles, persisted next to the DLL.
    tabs.push(Tab {
        name: "Team Trials",
        icon: '\u{E74E}',
        sections: vec![
            Section {
                title: "Deck profiles",
                icon: '\u{E74E}',
                blurb: "Save your team and swap the whole 15 with one click.",
                controls: vec![Ctrl::Custom(Custom::TtPadder)],
            },
            Section {
                title: "Opponent hunter",
                icon: '\u{E721}',
                blurb: "Auto-refresh the opponent list until a trainer you name shows up.",
                controls: vec![Ctrl::Custom(Custom::TtHunter)],
            },
            Section {
                title: "TT Capture",
                icon: '\u{E74E}',
                blurb: "Saved results are read by the Heaven dashboard.",
                controls: vec![Ctrl::Custom(Custom::TeamTrials)],
            },
        ],
    });

    // ── 2) CAMERA ────────────────────────────────────────────────────────────
    #[cfg(feature = "freecam")]
    tabs.push(Tab {
        name: "Race Director",
        icon: '\u{E722}',
        sections: vec![
            // 1) Freecam — the 3rd-person camera (enable + follow + per-circuit presets). Independent
            //    of telemetry: you can run the camera with no HUD, or the HUD with no camera.
            Section {
                title: "Freecam",
                icon: '\u{E722}',
                blurb: "3rd-person race camera with per-circuit presets.",
                controls: vec![Ctrl::Custom(Custom::Freecam)],
            },
            // 2) Key bindings — rebind every freecam control.
            Section {
                title: "Key bindings",
                icon: '\u{E765}',
                blurb: "Rebind the freecam controls to any key.",
                controls: vec![Ctrl::Custom(Custom::KeyBinds)],
            },
            // 3) Telemetry — the whole broadcast HUD (independent of freecam), with its panels.
            Section {
                title: "Telemetry",
                icon: '\u{E9D9}',
                blurb: "Live broadcast HUD — shows all the data during any race, freecam or not.",
                controls: vec![
                    Ctrl::Toggle { id: "tel", label: "Telemetry HUD", get: crate::settings::telemetry, set: crate::settings::set_telemetry },
                    Ctrl::Toggle { id: "ttw", label: "Timing tower", get: crate::settings::tele_tower, set: crate::settings::set_tele_tower },
                    Ctrl::Toggle { id: "twp", label: "Win probability", get: crate::settings::tele_winprob, set: crate::settings::set_tele_winprob },
                    Ctrl::Toggle { id: "tmk", label: "Head marker (needs freecam)", get: crate::settings::tele_marker, set: crate::settings::set_tele_marker },
                    Ctrl::Toggle { id: "tba", label: "Duel callout", get: crate::settings::tele_battle, set: crate::settings::set_tele_battle },
                    Ctrl::Toggle { id: "tgr", label: "Grade badge", get: crate::settings::tele_grade, set: crate::settings::set_tele_grade },
                    Ctrl::Toggle { id: "tpo", label: "Uma portrait", get: crate::settings::tele_portrait, set: crate::settings::set_tele_portrait },
                    Ctrl::Toggle { id: "tri", label: "Rival comparison", get: crate::settings::tele_rival, set: crate::settings::set_tele_rival },
                    Ctrl::Toggle { id: "tsk", label: "Skill feed", get: crate::settings::tele_skills, set: crate::settings::set_tele_skills },
                    Ctrl::Toggle { id: "tpa", label: "Pace trace", get: crate::settings::tele_pace, set: crate::settings::set_tele_pace },
                    Ctrl::SliderF32 { id: "tsc", label: "HUD scale", min: 0.6, max: 2.0, get: crate::settings::tele_scale, set: crate::settings::set_tele_scale, unit: "x" },
                    Ctrl::Button { id: "tpb", label: "Broadcast preset (clean)", action: crate::settings::tele_preset_broadcast },
                    Ctrl::Button { id: "tpf", label: "Full preset", action: crate::settings::tele_preset_full },
                ],
            },
        ],
    });

    // ── 3) VISUALS ───────────────────────────────────────────────────────────
    tabs.push(Tab {
        name: "Visuals",
        icon: '\u{E790}',
        sections: vec![
            Section {
                title: "Graphics",
                icon: '\u{E790}',
                blurb: "Force full 3D model quality, beyond the in-game cap.",
                controls: vec![
                    Ctrl::Toggle { id: "gq", label: "Max 3D quality", get: crate::settings::gfx_quality, set: crate::settings::set_gfx_quality },
                    Ctrl::Toggle { id: "ge", label: "Enhanced textures & shadows", get: crate::settings::gfx_extras, set: crate::settings::set_gfx_extras },
                    Ctrl::Note("Applies on the next scene / character load."),
                ],
            },
            Section {
                title: "Cloth physics",
                icon: '\u{EA86}',
                blurb: "Uncap hair / cloth physics so they stay smooth at high FPS.",
                controls: vec![Ctrl::Toggle { id: "cyspring", label: "Uncap cloth physics", get: crate::settings::cyspring_uncap, set: crate::settings::set_cyspring_uncap }],
            },
        ],
    });

    // ── 4) PERFORMANCE ───────────────────────────────────────────────────────
    tabs.push(Tab {
        name: "Performance",
        icon: '\u{E9D9}',
        sections: vec![
            Section {
                title: "Low resources mode",
                icon: '\u{E950}',
                blurb: "Potato mode: lowest quality, no shadows/AA, lighter physics.",
                controls: vec![
                    Ctrl::Toggle { id: "lowspec", label: "Low resources mode", get: crate::settings::low_spec, set: crate::settings::set_low_spec },
                    Ctrl::Note("Overrides Visuals. Applies on next scene load."),
                ],
            },
            Section {
                title: "Frame rate",
                icon: '\u{E9D9}',
                blurb: "",
                controls: vec![Ctrl::Custom(Custom::Fps)],
            },
        ],
    });

    // ── 5) INTERFACE ─────────────────────────────────────────────────────────
    let mut interface = Vec::new();
    interface.push(Section {
        title: "Window",
        icon: '\u{E737}',
        blurb: "",
        controls: vec![
            Ctrl::Toggle { id: "aot", label: "Always on top", get: crate::settings::always_on_top, set: crate::settings::set_always_on_top },
            Ctrl::Toggle { id: "bm", label: "Block minimize", get: crate::settings::block_minimize, set: crate::settings::set_block_minimize },
            Ctrl::Cycle { id: "dm", label: "Screen mode", label_of: display_mode_label, next: cycle_display_mode },
        ],
    });
    interface.push(Section {
        title: "Layout",
        icon: '\u{E8A1}',
        blurb: "",
        controls: vec![Ctrl::Custom(Custom::AboutLayout)],
    });
    interface.push(Section {
        title: "Affinity numbers",
        icon: '\u{E8C9}',
        blurb: "Show the exact succession affinity on the Legacy Select screen.",
        controls: vec![Ctrl::Custom(Custom::Affinity)],
    });
    #[cfg(feature = "banner")]
    interface.push(Section {
        title: "Intro video",
        icon: '\u{E714}',
        blurb: "",
        controls: vec![Ctrl::Custom(Custom::Intro)],
    });
    tabs.push(Tab { name: "Interface", icon: '\u{E8A9}', sections: interface });

    // ── 5b) PLUGINS ──────────────────────────────────────────────────────────
    // Native, in-process stand-ins for the companion plugins (horseACT / CarrotBlender),
    // in their own tab (kept right above About) so all plugin-related tooling lives in one
    // place. Both builds.
    tabs.push(Tab {
        name: "Plugins",
        icon: '\u{E71D}',
        sections: vec![Section {
            title: "Companion plugins",
            icon: '\u{E7C3}',
            blurb: "Built-in stand-ins for horseACT and CarrotBlender — no external DLLs needed.",
            controls: vec![
                Ctrl::Toggle { id: "rex", label: "Export races (horseACT)", get: crate::settings::race_export, set: crate::settings::set_race_export },
                Ctrl::Toggle { id: "vex", label: "Export veterans (Hakuraku)", get: crate::settings::umas_export, set: crate::settings::set_umas_export },
                Ctrl::Toggle { id: "cbr", label: "Companion feed (CarrotBlender)", get: crate::friendlyplugins::bridge_enabled, set: crate::friendlyplugins::set_bridge_enabled },
            ],
        }],
    });

    // ── 6) ABOUT ─────────────────────────────────────────────────────────────
    #[allow(unused_mut)]
    let mut about = vec![
        Section {
            title: "Updates",
            icon: '\u{E72C}',
            blurb: "",
            controls: vec![Ctrl::Custom(Custom::Updates)],
        },
        Section {
            title: "About",
            icon: '\u{E946}',
            blurb: "",
            controls: vec![Ctrl::Custom(Custom::Credits)],
        },
    ];
    // General diagnostics — for debugging issues that don't reproduce for everyone (e.g. a skip
    // that works for one player but not another). The button writes a self-contained report the
    // affected player can send; the toggle adds verbose runtime logging and drops a report at once.
    about.push(Section {
        title: "Diagnostics",
        icon: '\u{E90F}',
        blurb: "Generate a report to debug problems (yours or another player's).",
        controls: vec![
            Ctrl::Toggle { id: "diag", label: "Verbose diagnostics", get: crate::diag::enabled, set: crate::diag::set_enabled },
            Ctrl::Button { id: "diagdump", label: "Save diagnostic report", action: crate::diag::dump_action },
            Ctrl::Note("Writes heaven-logs/heaven-diag.txt next to the game — send that file."),
        ],
    });
    // Dev-only capture toggles (net capture / geom capture) intentionally NOT here —
    // they live in the the extra tab tab where the rest of the RE tooling is.
    tabs.push(Tab { name: "About", icon: '\u{E946}', sections: about });

    tabs
}
