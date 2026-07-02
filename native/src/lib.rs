//! Heaven MOD — internal overlay DLL entry point.
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
// Private build only — gated with the `banner` feature, like the video player itself.
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
mod cyspring;
mod data;
mod diag;
mod plugins;
mod hachimi_compat;
mod display;
#[cfg(feature = "freecam")]
mod freecam;
mod fps;
mod graphics;
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
mod overlay;
mod padder;
mod paths;
// Live race reader (Race panel + race-result win-gate). Built in both the full
// private build and the public build (the race-result skip needs finish placement).
#[cfg(feature = "raceread")]
mod race;
// Per-race JSON export (RaceInfo → disk, grouped by race type) for the web viewer.
#[cfg(feature = "raceread")]
mod race_export;
mod reset;
mod umas;
// Player-horse identity from the network response (msgpack). Public build only —
// the private build gets the same data from `the full build`'s response hook.
#[cfg(feature = "racenet")]
mod race_net;
mod selfupdate;
mod settings;
mod skip;
#[cfg(feature = "banner")]
mod startup_probe;
// Live career reader (extra info panels). Private builds only.
mod ui_tempo;
mod uma_bridge;
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
