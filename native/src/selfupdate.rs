//! In-game self-updater. Checks the GitHub releases API for versions newer than this
//! build, shows the COMBINED changelog of every missed version (newest-first), downloads
//! the new `trackside.dll` to a staging file, and lets the version.dll proxy swap it
//! in on the next launch — no external installer, no forced game exit.
//!
//! A loaded DLL can't replace itself, so we stage `trackside.dll.new` next to the
//! current one; the proxy (which runs first, before the overlay is loaded) applies it.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;

use serde_json::Value;

use crate::http;

/// This build's version (from Cargo.toml). Releases are tagged `v<this>`.
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// True for locally-built DEV builds — `Build-Trackside.ps1` sets `TRACKSIDE_DEV=1` before the
/// cargo build, so this bakes in only for dev iterations. A dev DLL's hash never matches the
/// published release asset's, which otherwise fires a spurious "hotfix available" popup every
/// session; dev builds skip the same-tag hotfix check. Release builds (built without the env
/// var, by the release tool) leave it false, so genuine re-uploaded-DLL hotfixes still surface.
const IS_DEV_BUILD: bool = option_env!("TRACKSIDE_DEV").is_some();

const REPO: &str = "TheCing/Trackside";

/// The loose DLL asset the release must carry for one-click updates (uploaded alongside the zips
/// by the release tool). Per-variant so a Heaven+Hachimi install pulls the H+H DLL, not the plain
/// one. If the asset is absent, the prompt still shows but Download reports it.
#[cfg(feature = "hachimi")]
const DLL_ASSET: &str = "trackside_hh.dll";
#[cfg(not(feature = "hachimi"))]
const DLL_ASSET: &str = "trackside.dll";

/// A pending update the overlay draws as the "update available" dialog.
#[derive(Clone, Default)]
pub struct Pending {
    pub target: String,      // newest tag, e.g. "v3.6.2"
    pub count: usize,        // how many versions ahead of us
    pub changelog: String,   // combined, slimmed notes, newest-first (or the diff for a hotfix)
    pub dll_url: String,     // download URL of the DLL on the target release
    pub same_version: bool,  // true = a hotfix under our SAME tag (DLL changed, number didn't)
    pub is_direct: bool,     // true = set by an alternate (non-GitHub) update source
}


static PENDING: OnceLock<Mutex<Option<Pending>>> = OnceLock::new();
fn pending_slot() -> &'static Mutex<Option<Pending>> {
    PENDING.get_or_init(|| Mutex::new(None))
}
/// The current pending update, if the dialog should be shown.
pub fn pending() -> Option<Pending> {
    pending_slot().lock().ok().and_then(|g| g.clone())
}
fn clear_pending() {
    if let Ok(mut g) = pending_slot().lock() {
        *g = None;
    }
}

static STATUS: OnceLock<Mutex<String>> = OnceLock::new();
fn status_slot() -> &'static Mutex<String> {
    STATUS.get_or_init(|| Mutex::new(String::new()))
}
pub fn status() -> String {
    status_slot().lock().map(|s| s.clone()).unwrap_or_default()
}
fn set_status(s: impl Into<String>) {
    if let Ok(mut g) = status_slot().lock() {
        *g = s.into();
    }
}

static BUSY: AtomicBool = AtomicBool::new(false);
pub fn is_busy() -> bool {
    BUSY.load(Ordering::Relaxed)
}

/// Parse "3.6.0" / "v3.6.0" → (major, minor, patch); missing parts default to 0.
///
/// Any semver suffix is stripped before comparing, so the PRIVATE build's `1.0.6+p` compares
/// EQUAL to the public `1.0.6` rather than parsing to 0. That matters: a trailing-garbage parse
/// used to yield `(1,0,0)`, which made every public release look newer than the private build and
/// would have offered private users the Oracle-less public DLL.
fn parse_ver(s: &str) -> (u32, u32, u32) {
    let s = s.trim().trim_start_matches('v');
    // Drop build metadata ("+p") and any pre-release tail ("-rc1") — neither affects precedence here.
    let s = s.split(['+', '-']).next().unwrap_or("");
    // Take leading digits per component so a stray suffix degrades to the number, never to 0.
    let mut it = s.split('.').map(|p| {
        let digits: String = p.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u32>().unwrap_or(0)
    });
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}
fn is_newer(tag: &str, base: &str) -> bool {
    parse_ver(tag) > parse_ver(base)
}

/// Replace common typographic Unicode with ASCII (the overlay font only covers basic Latin, so
/// `—`/`→`/curly quotes/`…` render as '?'), strip inline markdown (`**bold**`, `` `code` ``), and
/// drop any other non-ASCII. Keeps release notes readable in imgui.
fn clean(s: &str) -> String {
    let s = s.replace("**", "").replace("__", "").replace('`', "");
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{2014}' | '\u{2013}' => out.push('-'),               // em / en dash
            '\u{2192}' | '\u{00bb}' | '\u{2794}' => out.push_str("->"), // arrows
            '\u{2018}' | '\u{2019}' | '\u{02bc}' => out.push('\''), // curly single quotes
            '\u{201c}' | '\u{201d}' => out.push('"'),               // curly double quotes
            '\u{2026}' => out.push_str("..."),                      // ellipsis
            '\u{2022}' | '\u{2219}' | '\u{00b7}' | '\u{25cf}' => out.push('-'), // bullets / middot
            '\u{00a0}' => out.push(' '),                            // nbsp
            c if c.is_ascii() => out.push(c),
            _ => {} // any other non-ASCII would render as '?', drop it
        }
    }
    out
}

/// Slim a release body (markdown) down to headers + bullets so it renders cleanly in imgui:
/// keep `## `/`### ` headings and `- `/`* ` bullets, drop the rest. All text is `clean`ed.
fn slim_notes(body: &str) -> String {
    let mut out = String::new();
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(h) = t.strip_prefix("## ").or_else(|| t.strip_prefix("### ")) {
            out.push_str(&clean(h));
            out.push('\n');
        } else if let Some(b) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            out.push_str("  - ");
            out.push_str(&clean(b));
            out.push('\n');
        }
    }
    out
}

/// FNV-1a 64-bit — a fast, dependency-free content hash to tell whether the release DLL differs
/// from ours. NOT cryptographic (we only need change detection); `hv release` computes the SAME
/// hash in Python and publishes it as the `<dll>.hash` asset.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Hash of our own on-disk DLL (`trackside.dll` in the game folder), hex. None if unreadable.
fn local_dll_hash() -> Option<String> {
    let path = crate::paths::dll_dir().join("trackside.dll");
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:016x}", fnv1a(&bytes)))
}

/// Lines present in `new` but not in `seen` (trimmed compare) — the "what changed" for a hotfix.
/// Empty `seen` (or no shared lines) → return the whole thing.
fn changelog_diff(new: &str, seen: &str) -> String {
    if seen.trim().is_empty() {
        return new.to_string();
    }
    let seen_lines: std::collections::HashSet<&str> = seen.lines().map(|l| l.trim()).collect();
    let mut out = String::new();
    for line in new.lines() {
        let t = line.trim();
        if !t.is_empty() && !seen_lines.contains(t) {
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.trim().is_empty() {
        new.to_string()
    } else {
        out
    }
}

/// Check GitHub for newer releases. `force` = ignore the "don't ask again" skip (used by the
/// manual "Check for updates" button; the auto-check on boot passes false). Background thread.
pub fn check(force: bool) {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        run_check(force);
        BUSY.store(false, Ordering::SeqCst);
    });
}

fn run_check(force: bool) {
    // Dev/private builds (TRACKSIDE_DEV=1) NEVER self-update. The update channel is the PUBLIC repo,
    // so any offer — a newer version OR a same-tag hotfix — would replace this DLL with the public
    // build, silently dropping the private Event Oracle build. (The old code only skipped the
    // same-tag hotfix, so a newer public release could still clobber the private install.) Private
    // users receive updates by re-distribution, not the in-game updater.
    if IS_DEV_BUILD {
        clear_pending();
        return set_status("Self-update disabled (dev/private build)");
    }
    set_status("Checking...");
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
    let body = match http::get_string(&url) {
        Ok(b) => b,
        Err(e) => return set_status(format!("Check failed: {e}")),
    };
    let json: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return set_status("Check failed: bad response"),
    };
    let Some(arr) = json.as_array() else {
        return set_status("Check failed: no releases");
    };

    // Releases arrive newest-first. Keep every published (non-draft, non-prerelease) one
    // strictly newer than this build.
    let newer: Vec<&Value> = arr
        .iter()
        .filter(|r| !r.get("draft").and_then(|v| v.as_bool()).unwrap_or(false))
        .filter(|r| !r.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false))
        .filter(|r| is_newer(r.get("tag_name").and_then(|v| v.as_str()).unwrap_or(""), CURRENT))
        .collect();

    if newer.is_empty() {
        // No newer VERSION — but maybe a same-tag HOTFIX (a fixed DLL re-uploaded without a bump).
        return check_same_tag_hotfix(arr);
    }

    let latest = newer[0];
    let target = latest.get("tag_name").and_then(|v| v.as_str()).unwrap_or("").to_string();

    // Respect the per-version skip unless this was a manual check.
    if !force && crate::settings::update_skip() == target {
        return set_status(format!("Update {target} available (silenced)"));
    }

    // The loose DLL asset on the newest release (its DLL already contains the in-between fixes).
    let mut dll_url = String::new();
    if let Some(assets) = latest.get("assets").and_then(|v| v.as_array()) {
        for a in assets {
            if a.get("name").and_then(|v| v.as_str()) == Some(DLL_ASSET) {
                dll_url =
                    a.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                break;
            }
        }
    }

    // Combined changelog, newest-first.
    let mut changelog = String::new();
    for rel in &newer {
        let tag = rel.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
        let notes = slim_notes(rel.get("body").and_then(|v| v.as_str()).unwrap_or(""));
        changelog.push_str(tag);
        changelog.push('\n');
        changelog.push_str(&notes);
        changelog.push('\n');
    }

    if let Ok(mut g) = pending_slot().lock() {
        *g = Some(Pending {
            target: target.clone(),
            count: newer.len(),
            changelog,
            dll_url,
            same_version: false,
            is_direct: false,
        });
    }
    set_status(format!("Update {target} available"));
}

/// Same-version hotfix: find the release tagged as our CURRENT version and compare its DLL hash (the
/// `<dll>.hash` asset) with ours. If they differ, a fixed DLL was re-uploaded under the same number →
/// offer it, showing only the changelog lines that changed since we last saw them.
fn check_same_tag_hotfix(arr: &[Value]) {
    // Dev builds always differ from the published DLL by hash — don't nag about a "hotfix".
    if IS_DEV_BUILD {
        clear_pending();
        return set_status("Up to date (dev build)");
    }
    let cur = format!("v{CURRENT}");
    let Some(rel) = arr.iter().find(|r| {
        let t = r.get("tag_name").and_then(|v| v.as_str()).unwrap_or("");
        t == cur || t.trim_start_matches('v') == CURRENT
    }) else {
        clear_pending();
        return set_status("Up to date");
    };

    let notes = slim_notes(rel.get("body").and_then(|v| v.as_str()).unwrap_or(""));

    // Locate the DLL + its .hash asset on this release.
    let hash_name = format!("{DLL_ASSET}.hash");
    let (mut dll_url, mut hash_url) = (String::new(), String::new());
    if let Some(assets) = rel.get("assets").and_then(|v| v.as_array()) {
        for a in assets {
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let url = a.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or("");
            if name == DLL_ASSET {
                dll_url = url.to_string();
            } else if name == hash_name {
                hash_url = url.to_string();
            }
        }
    }

    let remote_hash =
        if hash_url.is_empty() { None } else { http::get_string(&hash_url).ok().map(|s| s.trim().to_string()) };
    let local_hash = local_dll_hash();

    if let (Some(rh), Some(lh)) = (remote_hash, local_hash) {
        if rh != lh && !dll_url.is_empty() {
            // Hotfix: the release DLL differs from ours under the same tag.
            let diff = changelog_diff(&notes, &crate::settings::update_seen_changelog());
            if let Ok(mut g) = pending_slot().lock() {
                *g = Some(Pending {
                    target: cur.clone(),
                    count: 0,
                    changelog: diff,
                    dll_url,
                    same_version: true,
                    is_direct: false,
                });
            }
            return set_status(format!("Hotfix for {cur} available"));
        }
    }

    // Up to date — remember what we've seen so a future hotfix can diff against it.
    crate::settings::set_update_seen_changelog(&notes);
    clear_pending();
    set_status("Up to date");
}

/// Download the new DLL to a staging file next to the current one. The proxy swaps it in on
/// the next launch. Background thread.
pub fn download() {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        run_download();
        BUSY.store(false, Ordering::SeqCst);
    });
}

fn run_download() {
    let Some(p) = pending() else { return };
    stage_and_restart(&p.dll_url, &p.target);
}

/// Download a specific version's DLL to the staging file and restart so the proxy applies it. Shared
/// by the normal update (`download`) and the version switch / downgrade (`switch_to`).
fn stage_and_restart(dll_url: &str, tag: &str) {
    if dll_url.is_empty() {
        return set_status("No downloadable DLL in the release");
    }
    set_status("Downloading...");
    let bytes = match http::get(dll_url) {
        Ok(b) => b,
        Err(e) => return set_status(format!("Download failed: {e}")),
    };
    // Sanity: a PE/DLL starts with "MZ". Guards against a truncated / HTML error page.
    if bytes.len() < 2 || &bytes[..2] != b"MZ" {
        return set_status("Download failed: not a valid DLL");
    }
    let staging = crate::paths::dll_dir().join("trackside.dll.new");
    match std::fs::write(&staging, &bytes) {
        Ok(_) => {
            // Auto-apply: show the notice briefly, then close + relaunch the game. The proxy swaps
            // the staged DLL in on the fresh launch (works the same for a newer OR older version).
            set_status(format!("Downloaded {tag} - restarting the game to apply..."));
            std::thread::sleep(std::time::Duration::from_secs(3));
            restart_game();
        }
        Err(e) => set_status(format!("Save failed: {e}")),
    }
}

// ── version switch / downgrade ────────────────────────────────────────────────────
// Every release that carries THIS variant's loose DLL (DLL_ASSET) can be switched to — that's v3.5.9+
// (older releases only shipped the zip). The list grows on its own as we publish more releases.
static VERSIONS: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new(); // (tag, dll_url), newest-first
fn versions_slot() -> &'static Mutex<Vec<(String, String)>> {
    VERSIONS.get_or_init(|| Mutex::new(Vec::new()))
}
/// Cached switchable versions (tag, dll_url), newest-first. Empty until `list_versions()` has run.
pub fn versions() -> Vec<(String, String)> {
    versions_slot().lock().map(|v| v.clone()).unwrap_or_default()
}

/// Populate `versions()` with every release that has our variant's loose DLL. Background thread.
pub fn list_versions() {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        run_list_versions();
        BUSY.store(false, Ordering::SeqCst);
    });
}

fn run_list_versions() {
    set_status("Loading versions...");
    let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=30");
    let body = match http::get_string(&url) {
        Ok(b) => b,
        Err(e) => return set_status(format!("Versions failed: {e}")),
    };
    let json: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return set_status("Versions failed: bad response"),
    };
    let Some(arr) = json.as_array() else {
        return set_status("Versions failed: no releases");
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for r in arr {
        if r.get("draft").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if r.get("prerelease").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let tag = r.get("tag_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if tag.is_empty() {
            continue;
        }
        // Only versions that carry OUR variant's loose DLL are switchable (skips pre-3.5.9 zips).
        let mut dll_url = String::new();
        if let Some(assets) = r.get("assets").and_then(|v| v.as_array()) {
            for a in assets {
                if a.get("name").and_then(|v| v.as_str()) == Some(DLL_ASSET) {
                    dll_url = a
                        .get("browser_download_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    break;
                }
            }
        }
        if !dll_url.is_empty() {
            out.push((tag, dll_url));
        }
    }
    let n = out.len();
    if let Ok(mut g) = versions_slot().lock() {
        *g = out; // GitHub returns newest-first; keep that order
    }
    set_status(format!("{n} version(s) available"));
}

/// Download + switch to a chosen version (newer or older). Powers the version dropdown. Background.
pub fn switch_to(dll_url: String, tag: String) {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        stage_and_restart(&dll_url, &tag);
        BUSY.store(false, Ordering::SeqCst);
    });
}

/// True once the staged download is on disk (the dialog switches to a "restart to apply" state).
pub fn staged() -> bool {
    status().starts_with("Downloaded")
}

/// Close the game and relaunch it so the proxy applies the staged DLL on the fresh launch — no
/// external installer. A DETACHED PowerShell helper gives the game a few seconds to close on its
/// own (from the WM_CLOSE we post below) and then FORCE-KILLS it by PID if it's still alive, before
/// relaunching the exe. The force-kill is what makes this reliable: a clean WM_CLOSE shutdown can
/// hang forever when a second mod is co-resident (two mods' threads racing the IL2CPP shutdown GC),
/// or the window may ignore/miss WM_CLOSE — in either case the old code's `Wait-Process` blocked
/// forever and the game never relaunched. Killing by PID needs no window and can't hang.
pub fn restart_game() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let pid = std::process::id();
    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return set_status("Restart failed: game path unknown (restart manually)"),
    };
    let exe = exe_path.to_string_lossy().replace('\'', "''");
    let dir = exe_path.parent().map(|d| d.to_string_lossy().replace('\'', "''")).unwrap_or_default();
    // Wait up to ~5s for a clean exit; if the process is still alive, force-kill it by PID (a hung
    // shutdown with a co-resident mod, or an ignored WM_CLOSE). Then relaunch from the game folder.
    let script = format!(
        "$p={pid}; \
         for($i=0;$i -lt 50 -and (Get-Process -Id $p -ErrorAction SilentlyContinue);$i++){{Start-Sleep -Milliseconds 100}}; \
         if(Get-Process -Id $p -ErrorAction SilentlyContinue){{Stop-Process -Id $p -Force -ErrorAction SilentlyContinue}}; \
         Start-Sleep -Milliseconds 1200; \
         Start-Process -FilePath '{exe}' -WorkingDirectory '{dir}'"
    );
    let spawned = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-NonInteractive", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn();
    if spawned.is_err() {
        return set_status("Restart failed to spawn helper (restart manually)");
    }
    set_status("Restarting the game...");
    // Best-effort graceful close (clean WM_CLOSE). If it doesn't take, the helper force-kills by PID.
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};
        let title: Vec<u16> = "Umamusume".encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if !hwnd.is_null() {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
}

/// "Not now" without the remember tick — close; the prompt returns next launch.
pub fn dismiss() {
    clear_pending();
}

/// "Not now" WITH the remember tick — silence this exact version. A newer release re-opens
/// the prompt (its tag won't match the stored skip).
pub fn skip() {
    if let Some(p) = pending() {
        crate::settings::set_update_skip(&p.target);
    }
    clear_pending();
}

/// The self-update prompt — drawn independently of the menu whenever the updater has a pending
/// update. Combined changelog + Download / Not now. The "don't ask again" tick only takes effect
/// on Not now (silences just that version; a newer release re-opens the prompt).
pub(crate) fn draw_dialog(ui: &hudhook::imgui::Ui) {
    use hudhook::imgui::{Condition, StyleColor, StyleVar};
    use crate::overlay::open_url;
    // The GitHub self-update, or an alternate in-app update source when one is present.
    let p = match crate::selfupdate::pending() {
        Some(p) => p,
        None => {
            { return }
        }
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    static REMEMBER: AtomicBool = AtomicBool::new(false);

    // Muted, opaque, easy-on-the-eyes palette (darker + more solid than the menu; no bright white).
    // Teal-on-graphite, matching the menu rebrand.
    const DLG_BG: [f32; 4] = [0.045, 0.058, 0.060, 0.99]; // solid dark graphite, near-opaque
    const DLG_CHILD: [f32; 4] = [0.030, 0.040, 0.042, 1.0]; // changelog block, a touch darker
    const DLG_BORDER: [f32; 4] = [0.30, 0.60, 0.58, 0.26];
    const DLG_TITLE: [f32; 4] = [0.44, 0.86, 0.80, 1.0]; // teal (the dialog's headline accent)
    const DLG_SUB: [f32; 4] = [0.44, 0.52, 0.52, 1.0]; // muted cool grey
    const DLG_BODY: [f32; 4] = [0.72, 0.80, 0.79, 1.0]; // changelog bullets — readable, still soft
    const DLG_VER: [f32; 4] = [0.36, 0.82, 0.76, 1.0]; // version tag headers
    const DLG_SECTION: [f32; 4] = [0.46, 0.62, 0.61, 1.0]; // section headers (New / Fixes)
    const DLG_BTN: [f32; 4] = [0.11, 0.16, 0.16, 1.0];
    const DLG_BTN_HI: [f32; 4] = [0.16, 0.23, 0.23, 1.0];
    const DLG_BTN_ACT: [f32; 4] = [0.20, 0.30, 0.30, 1.0];

    // Keep the style tokens alive for the whole window scope (tuple = dropped together at fn end).
    let _sc = (
        ui.push_style_color(StyleColor::WindowBg, DLG_BG),
        ui.push_style_color(StyleColor::ChildBg, DLG_CHILD),
        ui.push_style_color(StyleColor::Border, DLG_BORDER),
        ui.push_style_color(StyleColor::Button, DLG_BTN),
        ui.push_style_color(StyleColor::ButtonHovered, DLG_BTN_HI),
        ui.push_style_color(StyleColor::ButtonActive, DLG_BTN_ACT),
        ui.push_style_color(StyleColor::Text, DLG_BODY),
        ui.push_style_color(StyleColor::CheckMark, DLG_TITLE),
        ui.push_style_color(StyleColor::FrameBg, DLG_CHILD),
        ui.push_style_color(StyleColor::FrameBgHovered, DLG_BTN),
    );
    let _sv = (
        ui.push_style_var(StyleVar::WindowRounding(11.0)),
        ui.push_style_var(StyleVar::ChildRounding(8.0)),
        ui.push_style_var(StyleVar::FrameRounding(7.0)),
        ui.push_style_var(StyleVar::WindowPadding([16.0, 14.0])),
    );

    let [dw, dh] = ui.io().display_size;
    let d = crate::overlay::dpi(ui); // high-DPI/4K baseline
    let (w, h) = (420.0_f32 * d, 440.0_f32 * d);
    ui.window("##hv_update")
        // FirstUseEver = centre the first time, then remember where the user drags/resizes it.
        .position([(dw - w) * 0.5, (dh - h) * 0.5], Condition::FirstUseEver)
        .size([w, h], Condition::FirstUseEver)
        .size_constraints([340.0, 220.0], [1000.0, 1000.0]) // min / max while resizing
        .title_bar(false) // title lives in the body (drag from the header / empty areas)
        .collapsible(false)
        .resizable(true)
        .movable(true)
        .build(|| {
            ui.set_window_font_scale(ui.window_size()[0] / 420.0); // = dpi at default, grows on resize
            let title = if p.same_version {
                format!("Update for {}", p.target)
            } else {
                format!("{} available", p.target)
            };
            ui.text_colored(DLG_TITLE, title);
            let sub = if p.same_version {
                "What changed since your version".to_string()
            } else if p.count > 1 {
                format!("{} new versions", p.count)
            } else {
                "New version".to_string()
            };
            ui.text_colored(DLG_SUB, sub);
            ui.dummy([0.0, 6.0]);

            // Negative height = fill down to ~92px from the bottom (leaving room for the footer), so
            // the changelog grows/shrinks when the user resizes the window.
            ui.child_window("##hv_changelog").size([0.0, -92.0]).build(|| {
                // Colour by line type so a long combined changelog stays scannable: version tags
                // (lavender + spacing), section headers (New / Fixes), bullets (readable body).
                for line in p.changelog.lines() {
                    let t = line.trim_start();
                    if t.is_empty() {
                        continue;
                    }
                    let is_tag =
                        t.starts_with('v') && t.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit());
                    if is_tag {
                        ui.dummy([0.0, 5.0]);
                        let _c = ui.push_style_color(StyleColor::Text, DLG_VER);
                        ui.text(t);
                    } else if let Some(bullet) = t.strip_prefix('-') {
                        let _c = ui.push_style_color(StyleColor::Text, DLG_BODY);
                        ui.text_wrapped(format!("   -{bullet}"));
                    } else {
                        let _c = ui.push_style_color(StyleColor::Text, DLG_SECTION);
                        ui.text(t);
                    }
                }
            });
            ui.dummy([0.0, 8.0]);

            let staged = crate::selfupdate::staged();
            let busy = crate::selfupdate::is_busy();

            if staged {
                // Download done; the game auto-restarts shortly. Offer an immediate restart too.
                if ui.button("Restart now") {
                    crate::selfupdate::restart_game();
                }
            } else {
                let mut rem = REMEMBER.load(Ordering::Relaxed);
                if ui.checkbox("Don't ask again for this version", &mut rem) {
                    REMEMBER.store(rem, Ordering::Relaxed);
                }
                ui.dummy([0.0, 4.0]);
                if busy {
                    ui.text_colored(DLG_SUB, "Working...");
                } else {
                    if ui.button("Download") {
                        let handled = false;
                        if !handled {
                            crate::selfupdate::download();
                        }
                    }
                    ui.same_line();
                    if ui.button("Not now") {
                        let lic = false;
                        if lic {
                        } else if REMEMBER.load(Ordering::Relaxed) {
                            crate::selfupdate::skip();
                        } else {
                            crate::selfupdate::dismiss();
                        }
                        REMEMBER.store(false, Ordering::Relaxed);
                    }
                    if !p.is_direct {
                        ui.same_line();
                        if ui.button("View on GitHub") {
                            open_url(crate::update::RELEASES_URL);
                        }
                    }
                }
            }

            // Only the GitHub path has a meaningful status line to show.
            if !p.is_direct {
                let st = crate::selfupdate::status();
                if !st.is_empty() {
                    ui.dummy([0.0, 4.0]);
                    ui.text_colored(DLG_SUB, st);
                }
            }
        });
}
