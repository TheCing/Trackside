//! Trackside — internal overlay DLL entry point (a fork of Heaven by Night DC).
//!
//! Loaded into UmamusumePrettyDerby.exe (by the Frida core's `Module.load`).
//! On attach we start the loopback IPC server and install the D3D11 + imgui
//! overlay via hudhook. From then on the game's own render thread calls our
//! `HeavenOverlay::render`, drawing the HUD inside the swapchain — true
//! in-game rendering, no external window.
//!
//! If the game ever ships on D3D12 / Vulkan, swap `ImguiDx11Hooks` below for
//! `ImguiDx12Hooks` / the Vulkan hook (see build.md).

// Intro player support (native song playback, original-BGM mute, title-scene probe).
// gated with the `banner` feature, like the video player itself.
mod affinity;
#[cfg(feature = "banner")]
mod audio;
#[cfg(feature = "banner")]
mod bgm;
#[cfg(feature = "hachimi")]
mod arbiter;
mod boot;
mod clipboard;
mod crashlog;
mod data;
mod diag;
mod loadprof;
mod hachimi_compat;
#[cfg(feature = "freecam")]
mod followers;
mod freecam;
#[cfg(feature = "freecam")]
mod race_director;
mod performance;
mod hooks;
mod http;
mod il2cpp;
#[cfg(feature = "banner")]
mod intro_player;
mod htt;
mod htt_il2cpp;
mod hunter;
mod ipc;
mod menu_model;
mod names;
// Shared msgpack (rmpv) tree-walk helpers for the response-hook consumers.
// Only compiled when an rmpv-pulling feature is on.
mod msgpack;
mod overlay;
mod padder;
mod paths;
mod pruner;
mod roomfinder;
// Live race reader (Race panel + race-result win-gate). Built in both the full
// both builds (the race-result skip needs finish placement).
#[cfg(feature = "raceread")]
mod race;
// Generic IL2CPP managed-object → JSON reflection walker (used by race_export + umas).
#[cfg(feature = "raceread")]
mod il2cpp_json;
// Per-race JSON export (RaceInfo → disk, grouped by race type) for the web viewer.
#[cfg(feature = "raceread")]
mod race_export;
mod reset;
mod umas;
// The single Gallop.HttpHelper::DecompressResponse hook: player-id (race-result gate) + race
// retries + companion-bridge fan-out + full-build extras.
mod response_hook;
mod selfupdate;
mod settings;
mod skip;
// Shared cross-cutting utilities (logging, process clock). See tools/mod.rs.
mod tools;
// Generic UI click/dialog engine used by the SuperSkip legs (result + shop).
mod ui_input;
// Shared IL2CPP helpers for the Team Trials features (hunter + padder).
mod tt_il2cpp;
#[cfg(feature = "banner")]
mod startup_probe;
mod ui_tempo;
mod uma_bridge;
// Native, in-process stand-ins for the companion plugins (horseACT export, CarrotBlender feed).
mod friendlyplugins;
mod update;

use hudhook::hooks::dx11::ImguiDx11Hooks;

use overlay::HeavenOverlay;

hudhook::hudhook!(ImguiDx11Hooks, HeavenOverlay::new_with_engine());

impl HeavenOverlay {
    /// Construct the render loop and start the native engine. The engine thread
    /// waits for GameAssembly, resolves IL2CPP, installs every native module
    /// (career reader, SuperSkip, FPS, race), and publishes into the shared
    /// state the overlay renders. No Frida, no Python, no TCP.
    pub fn new_with_engine() -> Self {
        boot::spawn();
        // Start the video player's D3D11 device capture early (independent of the IL2CPP
        // boot) so the intro can draw over the splash logos within ~1 s of launch.
        #[cfg(feature = "banner")]
        intro_player::spawn_capture();
        HeavenOverlay::new()
    }
}
