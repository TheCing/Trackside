//! Heaven — diagnostics.
//!
//! A general "what's going on" snapshot for cross-machine debugging: when a feature works for one
//! person but not another, we need to see WHICH hooks actually installed, whether another mod stole
//! them first, what the game/runtime looks like, and the current toggle/gate state — without asking
//! the user to read raw logs.
//!
//! The install registry is ALWAYS populated at boot (cheap), so a full report is available any time
//! via the menu's "Save diagnostic report" button — even with the verbose toggle off. The toggle
//! just adds extra runtime logging for the harder cases and drops a fresh report when flipped on.
//!
//! `dump()` writes a self-contained `trackside-diag.txt` next to the other logs that the user can send.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::il2cpp;

static ENABLED: AtomicBool = AtomicBool::new(false);
// (module, status) install results, recorded by boot::spawn in install order.
static INSTALL: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Menu toggle target. Flipping it ON immediately writes a report (the common "turn it on and send
/// me the file" flow) so the user doesn't also have to find the button.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    log_line(&format!("[diag] verbose diagnostics {}", if on { "ON" } else { "off" }));
    if on {
        dump_action();
    }
}

/// Record one module install result. Called from boot for EVERY module, always (independent of the
/// toggle), so the report has the full picture even if the user never flips diagnostics on.
pub fn record_install(module: &str, status: &str) {
    if let Ok(mut v) = INSTALL.lock() {
        v.push((module.to_string(), status.to_string()));
    }
}

fn log_line(msg: &str) {
    crate::tools::log(msg);
}

// Minimal kernel32 import so module detection works in EVERY build (the `windows` crate is only
// linked in `banner` builds).
extern "system" {
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn GetModuleFileNameA(module: *mut c_void, filename: *mut u8, size: u32) -> u32;
}

/// Full on-disk path of a loaded module (by base name), or None if it isn't loaded.
fn module_path(name: &str) -> Option<String> {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0); // NUL-terminate for the ANSI Win32 call
    unsafe {
        let h = GetModuleHandleA(bytes.as_ptr());
        if h.is_null() {
            return None;
        }
        let mut buf = [0u8; 512];
        let n = GetModuleFileNameA(h, buf.as_mut_ptr(), buf.len() as u32);
        if n == 0 {
            return None;
        }
        Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }
}

/// True when `path`'s directory is the game root (where the proxy loaders live). This is what
/// separates a real injector/proxy from the genuine same-named DLL: proxy-hijack DLLs sit in the
/// game root, whereas the genuine `dxgi.dll`/`winhttp.dll` load from System32 and the genuine
/// `cri_mana_vpx.dll` loads from `UmamusumePrettyDerby_Data\Plugins\x86_64`. Case-insensitive.
fn in_game_root(path: &str) -> bool {
    let root = crate::paths::dll_dir();
    std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_lowercase() == root.to_string_lossy().to_lowercase())
        .unwrap_or(false)
}

fn build_kind() -> &'static str {
    "public"
}

fn yn(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

/// Build the full diagnostic report as text.
pub fn report() -> String {
    let mut s = String::new();
    s.push_str("===== TRACKSIDE DIAGNOSTIC REPORT =====\n");
    s.push_str(&format!("Trackside version : {}\n", env!("CARGO_PKG_VERSION")));
    s.push_str(&format!("Build          : {}\n", build_kind()));
    // Compile-time feature fingerprint of the DLL. `#[cfg]` pushes (not an array of `cfg!()` bools)
    // so feature-name literals for absent features are NOT compiled in — the public leak guard
    // keeps private-only feature names out of the public DLL.
    let mut feats: Vec<&str> = Vec::new();
    #[cfg(feature = "raceread")]
    feats.push("raceread");
    #[cfg(feature = "freecam")]
    feats.push("freecam");
    #[cfg(feature = "banner")]
    feats.push("banner");
    #[cfg(feature = "racenet")]
    feats.push("racenet");
    #[cfg(feature = "races_on")]
    feats.push("races_on");
    s.push_str(&format!("Features       : {}\n", feats.join(", ")));

    // Runtime / IL2CPP. IMPORTANT: `report()` is invoked from the MENU, i.e. the render thread —
    // so it must use only render-SAFE probes. `game_loaded()` is GetModuleHandle and `ready()`
    // is an OnceLock check; both are inert. The old `il2cpp::domain()` here was a LIVE
    // `il2cpp_domain_get()` runtime call: calling it off the game main thread registered the
    // render thread with the GC, and the next collection then blocked forever waiting for that
    // thread to reach a managed safepoint (it never does — it's in D3D/imgui) → whole-process
    // freeze a few seconds after toggling Verbose logging on. Never call live IL2CPP from here.
    s.push_str("\n--- Runtime ---\n");
    s.push_str(&format!("GameAssembly loaded : {}\n", il2cpp::game_loaded()));
    s.push_str(&format!("IL2CPP API ready    : {}\n", il2cpp::ready()));

    // Other loaders / proxies. Only DLLs loaded FROM THE GAME ROOT count — that's the proxy-
    // hijack slot. A same-named DLL loaded from System32 (dxgi/winhttp) or the Unity plugins
    // folder (cri_mana_vpx is the genuine CRI video codec) is NOT a mod and is skipped, so this
    // section stops crying wolf about every system DLL the game happens to load.
    s.push_str("\n--- Loaders / proxies in the game folder ---\n");
    let suspects = [
        ("version.dll", "version.dll proxy"),
        ("winhttp.dll", "winhttp proxy"),
        ("dxgi.dll", "dxgi proxy (ReShade / other injector)"),
        ("dinput8.dll", "dinput8 proxy"),
        ("cri_mana_vpx.dll", "Hachimi (active cri_mana_vpx proxy in game root)"),
        ("hachimi.dll", "Hachimi"),
    ];
    let mut any = false;
    for (dll, desc) in suspects {
        match module_path(dll) {
            Some(p) if in_game_root(&p) => {
                // version.dll in the root is OUR proxy — expected, not a conflict.
                let note = if dll.eq_ignore_ascii_case("version.dll") {
                    "Trackside proxy — expected"
                } else {
                    desc
                };
                s.push_str(&format!("  [game-root] {dll}  ({note})\n"));
                any = true;
            }
            _ => {}
        }
    }
    if !any {
        s.push_str("  (no proxy/injector DLLs loaded from the game folder)\n");
    }
    s.push_str(
        "NOTE: another IN-ROOT proxy may hook the same methods FIRST and Trackside yields — that\n      shows as 'already detoured (skipped)' in the install results below.\n",
    );

    // Hook install results — the core of the report.
    s.push_str("\n--- Hook install results ---\n");
    match INSTALL.lock() {
        Ok(v) if !v.is_empty() => {
            for (m, st) in v.iter() {
                s.push_str(&format!("  {m:<22}: {st}\n"));
            }
        }
        _ => s.push_str(
            "  (none recorded yet — boot may not have finished; relaunch and reach the title screen)\n",
        ),
    }

    // Current toggle states (what the user actually has enabled).
    s.push_str("\n--- Current toggles ---\n");
    s.push_str(&format!("Superskip events    : {}\n", yn(crate::skip::is_event_enabled())));
    s.push_str(&format!("Superskip training  : {}\n", yn(crate::skip::is_train_enabled())));
    s.push_str(&format!("Superskip shop      : {}\n", yn(crate::skip::is_shop_enabled())));
    s.push_str(&format!("Race-result skip    : {}\n", yn(crate::skip::is_race_result_enabled())));
    s.push_str(&format!("UI tempo            : {:.1}x\n", crate::ui_tempo::tempo()));
    s.push_str(&format!("FPS cap             : {}\n", crate::performance::fps::current()));
    s.push_str(&format!("Max 3D quality      : {}\n", yn(crate::settings::gfx_quality())));
    s.push_str(&format!("Cloth uncap         : {}\n", yn(crate::settings::cyspring_uncap())));

    // Skip subsystem live state — directly answers "why isn't a skip working".
    s.push_str("\n--- Skip subsystem ---\n");
    s.push_str(&format!("  {}\n", crate::skip::diag()));

    s.push_str(&format!("\nDiagnostics verbose mode: {}\n", yn(enabled())));
    s.push_str("===== END =====\n");
    s
}

/// Write the report next to the logs. Returns the path on success.
pub fn dump() -> Result<String, String> {
    let path = crate::paths::log_file("trackside-diag.txt");
    std::fs::write(&path, report()).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

/// Menu Button action (no return channel) — dump the report and log where it landed.
pub fn dump_action() {
    match dump() {
        Ok(p) => log_line(&format!("[diag] report saved: {p}")),
        Err(e) => crate::tools::error(&format!("[diag] report save FAILED: {e}")),
    }
}
