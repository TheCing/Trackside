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
//! `dump()` writes a self-contained `heaven-diag.txt` next to the other logs that the user can send.

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
// linked in `banner` builds). Returns null if the module isn't loaded in this process.
extern "system" {
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
}
fn module_loaded(name: &str) -> bool {
    let mut bytes = name.as_bytes().to_vec();
    bytes.push(0); // NUL-terminate for the ANSI Win32 call
    unsafe { !GetModuleHandleA(bytes.as_ptr()).is_null() }
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
    s.push_str("===== HEAVEN DIAGNOSTIC REPORT =====\n");
    s.push_str(&format!("Heaven version : {}\n", env!("CARGO_PKG_VERSION")));
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

    // Runtime / IL2CPP
    s.push_str("\n--- Runtime ---\n");
    s.push_str(&format!("GameAssembly loaded : {}\n", il2cpp::game_loaded()));
    s.push_str(&format!("IL2CPP domain ready : {}\n", !il2cpp::domain().is_null()));

    // Other mods / loaders that can collide with Heaven's hooks.
    s.push_str("\n--- Other loaders / mods detected ---\n");
    let known = [
        ("cri_mana_vpx.dll", "Hachimi (cri_mana_vpx loader)"),
        ("hachimi.dll", "Hachimi"),
        ("version.dll", "version.dll proxy (Heaven or other)"),
        ("UnityPlayer.dll", "UnityPlayer (game / Hachimi proxy)"),
        ("winhttp.dll", "winhttp proxy"),
        ("dxgi.dll", "dxgi proxy"),
        ("dinput8.dll", "dinput8 proxy"),
    ];
    let mut any = false;
    for (dll, desc) in known {
        if module_loaded(dll) {
            s.push_str(&format!("  [present] {dll}  ({desc})\n"));
            any = true;
        }
    }
    if !any {
        s.push_str("  (none of the known proxy/mod DLLs detected)\n");
    }
    s.push_str(
        "NOTE: if another mod is present it may hook the same methods FIRST and Heaven yields —\n      such a hook shows as 'already detoured (skipped)' in the install results below.\n",
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
    let path = crate::paths::log_file("heaven-diag.txt");
    std::fs::write(&path, report()).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

/// Menu Button action (no return channel) — dump the report and log where it landed.
pub fn dump_action() {
    match dump() {
        Ok(p) => log_line(&format!("[diag] report saved: {p}")),
        Err(e) => log_line(&format!("[diag] report save FAILED: {e}")),
    }
}
