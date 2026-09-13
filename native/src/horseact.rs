//! horseACT plugin manager — install, update and step aside for ayaliz/horseACT.
//!
//! WHY. Trackside carried its own reimplementation of horseACT's race dump (`race_export`,
//! `race_packet_export`, `htt`, `umas`). Every time horseACT changed its payload and Hakuraku's
//! parser followed, ours had to be re-derived from the parser, and twice the lineage tree came out
//! wrong. Tracking upstream by RUNNING upstream is the only fix that does not recur: horseACT is a
//! Hachimi SDK plugin, and `hachimi_compat` already hosts those - it hands the DLL our vtable
//! (horseACT uses only the seven-slot prefix) and backs its hooks with our own detour engine.
//!
//! WHAT THIS MODULE DOES.
//!   - Knows where the plugin lives (`trackside_plugins/horseACT.dll`), which release tag was
//!     installed (a sidecar file), and whether the host initialised it this session.
//!   - Fetches the latest release from GitHub and downloads the DLL on request. The DLL is never
//!     bundled: horseACT carries no license file, so it is fetched from its author's own releases,
//!     which also means every install is current upstream by construction.
//!   - Updates a LOADED plugin by renaming the running file aside and writing the new one next
//!     to it. Windows lets a loaded module's file be renamed (not deleted); the proxy loads every
//!     `*.dll` in the folder at the next start, and the `.old` is swept then.
//!   - Pre-writes horseACT's own config so its output lands under `trackside-races` instead of
//!     `Documents`, and so Team Trials results are saved (its code default is off).
//!   - Tells the native exporters to stand down while horseACT is active, so nothing is dumped
//!     twice. They stay in the build as a labelled fallback for anyone who does not install it.
//!
//! Everything network- or disk-heavy runs on a worker thread. The overlay only reads status.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Mutex;

const REPO: &str = "ayaliz/horseACT";
const ASSET: &str = "horseACT.dll";
const SIDECAR: &str = "horseACT.version";
const LATEST_URL: &str = "https://api.github.com/repos/ayaliz/horseACT/releases/latest";

pub const PHASE_IDLE: u8 = 0;
pub const PHASE_CHECKING: u8 = 1;
pub const PHASE_DOWNLOADING: u8 = 2;

static PHASE: AtomicU8 = AtomicU8::new(PHASE_IDLE);
static CHECKED: AtomicBool = AtomicBool::new(false);
static PENDING_RESTART: AtomicBool = AtomicBool::new(false);
/// (tag, download url, size) of the latest release, once fetched.
static LATEST: Mutex<Option<(String, String, u64)>> = Mutex::new(None);
static MSG: Mutex<String> = Mutex::new(String::new());
/// Preview host only: pose the card as installed so the settings block can be styled.
static MOCK_INSTALLED: AtomicBool = AtomicBool::new(false);

// ── horseACT's own config, exposed in the overlay ───────────────────────────────────────────────
//
// horseACT reads `hachimi/horseACTConfig.json` ONCE, at init, into OnceLock statics. So every
// edit here is written to that file and applies at the next launch - the block says so. The file
// is read on the boot thread and written on a worker; the overlay only touches the cached copy.
// Unknown keys in the file (a future horseACT option) are preserved on write.

/// The subset of horseACT's config we surface. `field_blacklist` is one comma-separated string
/// here because that is how the overlay edits it; the file keeps it as an array.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub output_path: String,
    pub api_key: String,
    pub server_url: String,
    pub field_blacklist: String,
    pub save_career_races: bool,
    pub save_tt_races: bool,
}

const DEFAULT_BLACKLIST: &[&str] = &[
    "_ownerViewerId", "_viewerId", "owner_viewer_id", "viewer_id",
    "<SimData>k__BackingField", "<SimReader>k__BackingField", "CreateTime", "succession_history_array",
];

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
/// Set once an edit has been written this session: horseACT will not see it until relaunch.
static CONFIG_DIRTY: AtomicBool = AtomicBool::new(false);

/// Trackside's defaults for horseACT - see `ensure_config` for why they differ from its own.
pub fn default_config() -> Config {
    Config {
        output_path: default_output_path(),
        api_key: String::new(),
        server_url: String::new(),
        field_blacklist: DEFAULT_BLACKLIST.join(", "),
        save_career_races: true,
        save_tt_races: true,
    }
}

pub fn default_output_path() -> String {
    crate::paths::dll_dir().join("trackside-races").to_string_lossy().into_owned()
}

fn read_config_value() -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(config_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn config_from_value(v: &serde_json::Value) -> Config {
    let d = default_config();
    let s = |k: &str, dflt: String| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string()).unwrap_or(dflt);
    let b = |k: &str, dflt: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(dflt);
    let bl = v
        .get("fieldBlacklist")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|e| e.as_str()).collect::<Vec<_>>().join(", "))
        .unwrap_or(d.field_blacklist.clone());
    Config {
        output_path: s("outputPath", d.output_path),
        api_key: s("apiKey", d.api_key),
        server_url: s("serverUrl", d.server_url),
        field_blacklist: bl,
        // horseACT's own code defaults: career true, TT FALSE. Absent keys mean its defaults.
        save_career_races: b("saveCareerRaces", true),
        save_tt_races: b("saveTTRaces", false),
    }
}

/// Read the file into the cache. Boot thread or worker only - never the render thread.
pub fn load_config() {
    let c = read_config_value().map(|v| config_from_value(&v)).unwrap_or_else(default_config);
    if let Ok(mut g) = CONFIG.lock() {
        *g = Some(c);
    }
}

/// The cached config (defaults until `load_config` has run).
pub fn config() -> Config {
    CONFIG.lock().ok().and_then(|g| g.clone()).unwrap_or_else(default_config)
}

pub fn config_dirty() -> bool {
    CONFIG_DIRTY.load(Ordering::Relaxed)
}

/// Update the cache and write the file on a worker, preserving keys we do not surface.
/// Values are never logged: the API key is a secret.
pub fn set_config(c: Config) {
    if let Ok(mut g) = CONFIG.lock() {
        if g.as_ref() == Some(&c) {
            return;
        }
        *g = Some(c.clone());
    }
    CONFIG_DIRTY.store(true, Ordering::Relaxed);
    std::thread::spawn(move || {
        static WRITE_LOCK: Mutex<()> = Mutex::new(());
        let _w = WRITE_LOCK.lock();
        let mut v = read_config_value().unwrap_or_else(|| serde_json::json!({}));
        let Some(map) = v.as_object_mut() else { return };
        let blacklist: Vec<serde_json::Value> = c
            .field_blacklist
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| serde_json::Value::String(s.to_string()))
            .collect();
        map.insert("outputPath".into(), serde_json::Value::String(c.output_path.trim().to_string()));
        map.insert("apiKey".into(), serde_json::Value::String(c.api_key.trim().to_string()));
        map.insert("serverUrl".into(), serde_json::Value::String(c.server_url.trim().to_string()));
        map.insert("fieldBlacklist".into(), serde_json::Value::Array(blacklist));
        map.insert("saveCareerRaces".into(), serde_json::Value::Bool(c.save_career_races));
        map.insert("saveTTRaces".into(), serde_json::Value::Bool(c.save_tt_races));
        let path = config_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(&v) {
            Ok(s) => match std::fs::write(&path, s + "\n") {
                Ok(_) => log("config written (applies at the next launch)"),
                Err(e) => log(&format!("config write failed: {e}")),
            },
            Err(e) => log(&format!("config serialise failed: {e}")),
        }
    });
}

fn log(msg: &str) {
    crate::tools::log(&format!("[horseact] {msg}"));
}

fn set_msg(s: impl Into<String>) {
    if let Ok(mut m) = MSG.lock() {
        *m = s.into();
    }
}

// ── where things are ────────────────────────────────────────────────────────────────────────────

pub fn plugin_dir() -> PathBuf {
    crate::paths::local_dir_migrated("trackside_plugins", "heaven_plugins")
}

pub fn dll_path() -> PathBuf {
    plugin_dir().join(ASSET)
}

fn sidecar_path() -> PathBuf {
    plugin_dir().join(SIDECAR)
}

/// horseACT reads `<game>/hachimi/horseACTConfig.json` (it creates the folder itself).
fn config_path() -> PathBuf {
    crate::paths::dll_dir().join("hachimi").join("horseACTConfig.json")
}

/// Where horseACT's files end up with our config: `<outputPath>/Saved races/<type>/`.
pub fn output_dir() -> PathBuf {
    crate::paths::dll_dir().join("trackside-races").join("Saved races")
}

// ── state the overlay reads ─────────────────────────────────────────────────────────────────────

pub fn installed() -> bool {
    MOCK_INSTALLED.load(Ordering::Relaxed) || dll_path().is_file()
}

/// Release tag written by the installer. `None` for a DLL the user dropped in by hand.
pub fn installed_tag() -> Option<String> {
    if MOCK_INSTALLED.load(Ordering::Relaxed) {
        return Some("v1.1.6".into());
    }
    std::fs::read_to_string(sidecar_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Did the plugin host initialise horseACT this session? This is what makes the native
/// exporters stand down - a file on disk is not enough, it has to be running.
pub fn active() -> bool {
    crate::hachimi_compat::plugin_active(ASSET)
}

pub fn phase() -> u8 {
    PHASE.load(Ordering::Relaxed)
}

pub fn checked() -> bool {
    CHECKED.load(Ordering::Relaxed)
}

pub fn pending_restart() -> bool {
    PENDING_RESTART.load(Ordering::Relaxed)
}

pub fn latest_tag() -> Option<String> {
    LATEST.lock().ok().and_then(|g| g.as_ref().map(|(t, _, _)| t.clone()))
}

/// True when a release newer than the installed one is known.
pub fn update_available() -> bool {
    match (installed_tag(), latest_tag()) {
        (Some(cur), Some(latest)) => cur != latest,
        (None, Some(_)) => installed(), // hand-installed, version unknown: offer the update
        _ => false,
    }
}

pub fn message() -> String {
    MSG.lock().map(|m| m.clone()).unwrap_or_default()
}

// ── boot ────────────────────────────────────────────────────────────────────────────────────────

/// Called once at boot, after the plugin host has run. Sweeps `.old` files from a previous update
/// and makes sure a hand-installed DLL still gets our config.
pub fn note_boot() {
    sweep_old();
    if installed() {
        ensure_config();
    }
    load_config();
    match (installed(), active()) {
        (true, true) => log(&format!("active ({})", installed_tag().unwrap_or_else(|| "version unknown".into()))),
        (true, false) => log("installed but not initialised by the host this session"),
        _ => {}
    }
}

/// Remove `*.dll.old` left by an update. Safe now: nothing has them open at this point of boot.
fn sweep_old() {
    let Ok(rd) = std::fs::read_dir(plugin_dir()) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.to_string_lossy().to_ascii_lowercase().ends_with(".dll.old") {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Write horseACT's config if it does not exist yet. Never overwrites: a user who changed it
/// (an API key, a different folder) keeps their choices.
///
/// Two deliberate departures from horseACT's own defaults, both decided 2026-09-13:
///   - `outputPath` is `<game>/trackside-races`, so files stay next to the game with the rest of
///     Trackside's output instead of landing in Documents. horseACT appends `Saved races` itself.
///   - `saveTTRaces` is true. horseACT's README says that is the default; its code says false.
///     We want its Team Trials dump, since ours stands down while it runs.
pub fn ensure_config() {
    let path = config_path();
    if path.exists() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let out = crate::paths::dll_dir().join("trackside-races");
    let cfg = serde_json::json!({
        "outputPath": out.to_string_lossy(),
        "apiKey": "",
        "serverUrl": "",
        "fieldBlacklist": [
            "_ownerViewerId", "_viewerId", "owner_viewer_id", "viewer_id",
            "<SimData>k__BackingField", "<SimReader>k__BackingField",
            "CreateTime", "succession_history_array"
        ],
        "saveCareerRaces": true,
        "saveTTRaces": true
    });
    match serde_json::to_string_pretty(&cfg) {
        Ok(s) => match std::fs::write(&path, s + "\n") {
            Ok(_) => log(&format!("wrote {} (output -> trackside-races)", path.display())),
            Err(e) => log(&format!("could not write {}: {e}", path.display())),
        },
        Err(e) => log(&format!("config serialise failed: {e}")),
    }
}

// ── check + install (worker threads) ────────────────────────────────────────────────────────────

/// Fetch the latest release once per session. Idempotent; the overlay calls it lazily the first
/// time the Plugins tab is drawn, so a launch that never opens it makes no request.
pub fn check_latest() {
    if CHECKED.swap(true, Ordering::SeqCst) {
        return;
    }
    if PHASE.compare_exchange(PHASE_IDLE, PHASE_CHECKING, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        CHECKED.store(false, Ordering::SeqCst);
        return;
    }
    std::thread::spawn(|| {
        match fetch_latest() {
            Ok((tag, url, size)) => {
                log(&format!("latest release {tag} ({size} bytes)"));
                if let Ok(mut g) = LATEST.lock() {
                    *g = Some((tag, url, size));
                }
                set_msg("");
            }
            Err(e) => {
                log(&format!("release check failed: {e}"));
                set_msg(format!("Could not check GitHub: {e}"));
                CHECKED.store(false, Ordering::SeqCst); // let the user retry
            }
        }
        PHASE.store(PHASE_IDLE, Ordering::SeqCst);
    });
}

/// "Check for updates": forget the session's cached answer and ask again.
pub fn recheck() {
    CHECKED.store(false, Ordering::SeqCst);
    check_latest();
}

fn fetch_latest() -> Result<(String, String, u64), String> {
    let body = crate::http::get_string(LATEST_URL)?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("bad JSON: {e}"))?;
    let tag = v.get("tag_name").and_then(|t| t.as_str()).ok_or("no tag_name")?.to_string();
    let assets = v.get("assets").and_then(|a| a.as_array()).ok_or("no assets")?;
    let asset = assets
        .iter()
        .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(ASSET))
        .ok_or_else(|| format!("release {tag} has no {ASSET}"))?;
    let url = asset.get("browser_download_url").and_then(|u| u.as_str()).ok_or("asset has no URL")?.to_string();
    let size = asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
    Ok((tag, url, size))
}

/// Download the latest release into the plugin folder. Takes effect at the next launch: the
/// running copy (if any) is renamed aside, never patched in place.
pub fn install() {
    if PHASE.compare_exchange(PHASE_IDLE, PHASE_DOWNLOADING, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return;
    }
    set_msg("Downloading\u{2026}");
    std::thread::spawn(|| {
        let r = run_install();
        match &r {
            Ok(tag) => {
                PENDING_RESTART.store(true, Ordering::SeqCst);
                set_msg(format!("horseACT {tag} installed \u{00b7} restart the game to activate it"));
            }
            Err(e) => set_msg(format!("Install failed: {e}")),
        }
        PHASE.store(PHASE_IDLE, Ordering::SeqCst);
    });
}

fn run_install() -> Result<String, String> {
    let latest = LATEST.lock().ok().and_then(|g| g.clone());
    let (tag, url, size) = match latest {
        Some(l) => l,
        None => {
            let l = fetch_latest()?;
            if let Ok(mut g) = LATEST.lock() {
                *g = Some(l.clone());
            }
            l
        }
    };
    log(&format!("downloading {ASSET} {tag} from {REPO}"));
    let bytes = crate::http::get(&url)?;
    if size != 0 && bytes.len() as u64 != size {
        return Err(format!("size mismatch: got {} bytes, release says {size}", bytes.len()));
    }
    // The one thing every SDK plugin must have. A wrong or truncated file fails here, not at boot.
    if !bytes.windows(b"hachimi_init".len()).any(|w| w == b"hachimi_init") {
        return Err("downloaded file is not a Hachimi SDK plugin (no hachimi_init export)".into());
    }
    let dir = plugin_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let dll = dll_path();
    if dll.exists() {
        // Loaded this session or not, renaming is always allowed; overwriting a loaded module is not.
        let old = dir.join(format!("{ASSET}.old"));
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&dll, &old).map_err(|e| format!("could not move the current DLL aside: {e}"))?;
    }
    std::fs::write(&dll, &bytes).map_err(|e| format!("write {}: {e}", dll.display()))?;
    let _ = std::fs::write(sidecar_path(), format!("{tag}\n"));
    ensure_config();
    load_config();
    log(&format!("installed {tag} ({} bytes) -> {}", bytes.len(), dll.display()));
    Ok(tag)
}

/// Preview-host design aid: pose the card as "installed, update available".
pub fn mock_for_preview() {
    if std::env::var_os("TRACKSIDE_HORSEACT_MOCK").is_none() {
        return;
    }
    CHECKED.store(true, Ordering::Relaxed);
    MOCK_INSTALLED.store(true, Ordering::Relaxed);
    if let Ok(mut g) = LATEST.lock() {
        *g = Some(("v1.1.7".into(), String::new(), 2_388_992));
    }
    if let Ok(mut g) = CONFIG.lock() {
        *g = Some(default_config());
    }
}
