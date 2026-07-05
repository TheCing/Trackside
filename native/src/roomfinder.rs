//! roomfinder — Room Match "room hunter": auto-refresh the guest room list until a room
//! matching your filters (track / distance / surface / conditions / open slots) shows up,
//! then stop + alert — optionally auto-opening the room's detail dialog so joining is one tap.
//!
//! Same frame as the TT opponent hunter and the follower pruner:
//!   - the UI panel only sets filters and flips request flags (never touches IL2CPP);
//!   - the loop is self-driving like the hunter's: we detour the room-list screen's
//!     `CreateRoomListUI` (fires when a fresh list finishes loading) to know when to read,
//!     and drive the game's OWN reload button handler (`OnClickRoomUpdateButton`) for the
//!     next refresh — validated flow, main thread, with the game's own cooldown respected;
//!   - `pump()` (riding hunter's TweenManager.Update tick) fires the scheduled refresh and
//!     processes fresh lists at a human pace (2–5 s jitter + occasional longer rests),
//!     capped at MAX_CHECKS.
//!
//! IL2CPP names in `bridge` were CONFIRMED from a live scan (trackside-roommatch-scan.txt,
//! 2026-07-02) — see the notes on each constant. NOT auto-sent: a real room ENTRY
//! (RoomMatchEntryRoomRequest) requires an entry_chara_array (which trained Uma races), so a
//! blind fire-and-forget join could be rejected or corrupt entry state. "Auto-open" instead
//! selects the found room and drives the game's own Join Race transition to the runner-entry
//! screen — the user picks runners and Confirms, which sends the validated entry request.

#![allow(static_mut_refs)]
#![allow(dead_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use retour::RawDetour;
use serde::{Deserialize, Serialize};

/// Stop hunting after this many list checks (a check ≈ one refresh cycle).
pub const MAX_CHECKS: usize = 60;
/// If a triggered refresh produces no fresh list within this window (cooldown, network),
/// re-arm and try again rather than hanging forever.
const REFRESH_TIMEOUT_MS: u64 = 15_000;

/// One room as read from the live list. 0 / -1 / empty = the game didn't expose that field
/// (or we can't decode it yet) — unknown values never PASS an active filter.
#[derive(Clone, Serialize, Deserialize)]
pub struct Room {
    pub room_id: i64,
    pub host: String,   // host trainer name (falls back to the room name)
    pub track_id: i32,  // racecourse id (10001 Sapporo … 10101 Ooi), 0 unknown
    pub distance: i32,  // metres, 0 unknown
    pub surface: i32,   // 1 turf, 2 dirt, 0 unknown
    pub season: i32,    // 1 spring … 4 winter, 0 unknown
    pub weather: i32,   // 1 sunny, 2 cloudy, 3 rainy, 4 snowy, 0 unknown
    pub members: i32,   // current entries, -1 unknown
    pub capacity: i32,  // max entries, -1 unknown
    pub remain: i32,    // open slots straight from RoomData.GetRemainEntryNum(), -1 unknown
    /// Career-rank entry gate ("SS or below" etc.): 1 restricted, 0 none, -1 unknown.
    pub rank_restricted: i32,
    /// Uma bans ("Restrictions: Yes"): 1 some Umas banned, 0 none, -1 unknown.
    pub uma_restricted: i32,
}

impl Room {
    /// Distance category from metres: 1 short ≤1400, 2 mile ≤1800, 3 medium ≤2400, 4 long.
    pub fn dist_cat(&self) -> i32 {
        match self.distance {
            0 => 0,
            d if d <= 1400 => 1,
            d if d <= 1800 => 2,
            d if d <= 2400 => 3,
            _ => 4,
        }
    }
    /// Open slots: the game's own remain count when readable, else capacity - members.
    pub fn open_slots(&self) -> Option<i32> {
        if self.remain >= 0 {
            return Some(self.remain);
        }
        if self.members < 0 || self.capacity < 0 {
            None
        } else {
            Some((self.capacity - self.members).max(0))
        }
    }
}

/// Persisted filter set. 0 = "any" for every enum-ish field.
#[derive(Clone, Serialize, Deserialize)]
pub struct Filters {
    pub track_id: i32,
    pub dist_cat: i32,
    pub surface: i32,
    pub season: i32,
    pub weather: i32,
    /// Require at least this many open slots (0 = don't care).
    pub min_open: i32,
    /// Only rooms with NO career-rank entry restriction. serde(default) so filter files
    /// saved before this field existed still load.
    #[serde(default)]
    pub no_rank_restrict: bool,
    /// Only rooms with NO Uma bans.
    #[serde(default)]
    pub no_uma_restrict: bool,
    /// Auto-open the found room's runner-entry screen (the game's own join path) on match.
    pub auto_join: bool,
    /// Saved "My Runners" team (1–5) to auto-load into the entry when a room is found; 0 = none
    /// (open the entry screen and let the user pick). serde(default) for older filter files.
    #[serde(default)]
    pub preset_slot: i32,
    /// After auto-loading the team, auto-press Confirm to send the entry immediately (beats
    /// other players racing to fill the room). Requires preset_slot > 0.
    #[serde(default)]
    pub auto_confirm: bool,
}
impl Default for Filters {
    fn default() -> Self {
        Filters {
            track_id: 0,
            dist_cat: 0,
            surface: 0,
            season: 0,
            weather: 0,
            min_open: 1,
            no_rank_restrict: false,
            no_uma_restrict: false,
            auto_join: false,
            preset_slot: 0,
            auto_confirm: false,
        }
    }
}

impl Filters {
    /// True when `r` passes every active filter. Unknown room values (0/-1) fail an ACTIVE
    /// filter — never match on data we couldn't read (acting on a guess would be worse
    /// than a missed room).
    pub fn matches(&self, r: &Room) -> bool {
        if self.track_id != 0 && r.track_id != self.track_id {
            return false;
        }
        if self.dist_cat != 0 && r.dist_cat() != self.dist_cat {
            return false;
        }
        if self.surface != 0 && r.surface != self.surface {
            return false;
        }
        if self.season != 0 && r.season != self.season {
            return false;
        }
        if self.weather != 0 && r.weather != self.weather {
            return false;
        }
        if self.min_open > 0 {
            match r.open_slots() {
                Some(n) if n >= self.min_open => {}
                _ => return false,
            }
        }
        // "Require none" gates: unknown (-1) fails the ACTIVE filter, same fail-safe rule.
        if self.no_rank_restrict && r.rank_restricted != 0 {
            return false;
        }
        if self.no_uma_restrict && r.uma_restricted != 0 {
            return false;
        }
        true
    }
    /// Human summary of the active filters, for the status line.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.track_id != 0 {
            parts.push(track_name(self.track_id).to_string());
        }
        if self.dist_cat != 0 {
            parts.push(dist_cat_name(self.dist_cat).to_string());
        }
        if self.surface != 0 {
            parts.push(surface_name(self.surface).to_string());
        }
        if self.season != 0 {
            parts.push(season_name(self.season).to_string());
        }
        if self.weather != 0 {
            parts.push(weather_name(self.weather).to_string());
        }
        if self.min_open > 0 {
            parts.push(format!("{}+ open", self.min_open));
        }
        if self.no_rank_restrict {
            parts.push("no rank req".into());
        }
        if self.no_uma_restrict {
            parts.push("no bans".into());
        }
        if self.preset_slot > 0 {
            parts.push(format!("auto-load Team {}", self.preset_slot));
        }
        if parts.is_empty() { "any room".into() } else { parts.join(" · ") }
    }
}

// ── display names (shared with the panel's combo boxes) ───────────────────────

pub const TRACK_IDS: &[i32] = &[10001, 10002, 10003, 10004, 10005, 10006, 10007, 10008, 10009, 10010, 10101];
pub fn track_name(id: i32) -> &'static str {
    match id {
        10001 => "Sapporo",
        10002 => "Hakodate",
        10003 => "Niigata",
        10004 => "Fukushima",
        10005 => "Nakayama",
        10006 => "Tokyo",
        10007 => "Chukyo",
        10008 => "Kyoto",
        10009 => "Hanshin",
        10010 => "Kokura",
        10101 => "Ooi",
        _ => "Any track",
    }
}
pub fn dist_cat_name(c: i32) -> &'static str {
    match c {
        1 => "Short",
        2 => "Mile",
        3 => "Medium",
        4 => "Long",
        _ => "Any distance",
    }
}
pub fn surface_name(s: i32) -> &'static str {
    match s {
        1 => "Turf",
        2 => "Dirt",
        _ => "Any surface",
    }
}
pub fn season_name(s: i32) -> &'static str {
    match s {
        1 => "Spring",
        2 => "Summer",
        3 => "Autumn",
        4 => "Winter",
        _ => "Any season",
    }
}
pub fn weather_name(w: i32) -> &'static str {
    match w {
        1 => "Sunny",
        2 => "Cloudy",
        3 => "Rainy",
        4 => "Snowy",
        _ => "Any weather",
    }
}

// ── state ─────────────────────────────────────────────────────────────────────

static HUNTING: AtomicBool = AtomicBool::new(false);
static REQ_SCAN: AtomicBool = AtomicBool::new(false);
// UI/start asked to warm the saved-team presets into work data; consumed on the main thread.
static REQ_PREFETCH: AtomicBool = AtomicBool::new(false);
// On Start, check the list already on screen once before triggering any refresh.
static CHECK_NOW: AtomicBool = AtomicBool::new(false);
// Set by the CreateRoomListUI detour: a fresh list just finished loading — read it.
static LIST_READY: AtomicBool = AtomicBool::new(false);
static CHECKS: AtomicUsize = AtomicUsize::new(0);
// When the next refresh may fire (ms on our clock); u64::MAX = none scheduled.
static NEXT_MS: AtomicU64 = AtomicU64::new(u64::MAX);
// The matched room, for the panel highlight + alert; 0 = nothing found.
static FOUND_ROOM: AtomicI64 = AtomicI64::new(0);
// The live RoomMatchGuestEntryViewController: set when its list loads / view plays in,
// cleared when the screen transitions out. Non-zero ⇒ we're on the guest room-list screen.
static VC: AtomicUsize = AtomicUsize::new(0);
// The live RoomMatchCharacterEntryViewController ("Please select your runners" screen):
// captured on PlayInView, cleared on PlayOutView. Non-zero ⇒ the runner-entry screen is up,
// so the "Load a saved team" control can act.
static ENTRY_VC: AtomicUsize = AtomicUsize::new(0);
// Team slot (1–5) queued by the UI to load into the entry; 0 = nothing pending. Consumed on
// the game main thread by pump().
static REQ_LOAD: AtomicI32 = AtomicI32::new(0);

// Auto-join pipeline (found room → open → load preset → confirm). Driven entirely on the game
// main thread by pump(). States:
//   IDLE          nothing pending
//   AWAIT_ENTRY   room opened; waiting for the runner-entry screen to appear
//   LOADING       entry screen up; retrying the team load until the UI is built AND the saved
//                 presets have arrived from the server (both are async), or the window closes
//   CONFIRM       team staged; waiting a short beat, then pressing Confirm
//   CONFIRM_DIALOG Confirm pressed; the "Confirm Registration" dialog is up — wait for its OK
//                 action to be captured, then fire it to actually join the room
const AJ_IDLE: i32 = 0;
const AJ_AWAIT_ENTRY: i32 = 1;
const AJ_LOADING: i32 = 2;
const AJ_CONFIRM: i32 = 3;
const AJ_CONFIRM_DIALOG: i32 = 4;
static AJ_STATE: AtomicI32 = AtomicI32::new(AJ_IDLE);
static AJ_SLOT: AtomicI32 = AtomicI32::new(0);
static AJ_WANT_CONFIRM: AtomicBool = AtomicBool::new(false);
// Overall give-up time for the current phase (ms on our clock); u64::MAX = none.
static AJ_DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);
// Earliest time for the next action (load retry / confirm), ms on our clock; u64::MAX = none.
static AJ_NEXT: AtomicU64 = AtomicU64::new(u64::MAX);
// The runner-entry screen must open within this window after auto-open, else give up.
const AJ_ENTRY_TIMEOUT_MS: u64 = 12_000;
// Once the entry screen is up, keep retrying the load for this long: the entry UI is built by a
// play-in coroutine and the saved-team presets arrive over the network, so neither is ready
// instantly. Staging before the UI's per-slot buttons exist crashes the game (UpdateEntryList
// dereferences unbuilt buttons), so we WAIT for readiness rather than guess a fixed delay.
const AJ_LOAD_WINDOW_MS: u64 = 9_000;
// Gap between load attempts while waiting for readiness.
const AJ_RETRY_MS: u64 = 600;
// Let the staged team settle on screen before auto-pressing Confirm.
const AJ_CONFIRM_SETTLE_MS: u64 = 500;
// After pressing Confirm, wait this long for the "Confirm Registration" dialog's OK action to
// be captured before giving up (letting the user tap OK themselves).
const AJ_DIALOG_TIMEOUT_MS: u64 = 5_000;

/// The "Confirm Registration" dialog's OK (right-button) `System.Action`, captured from the
/// dialog's Initialize/PushDialog. Firing it does exactly what tapping OK does — sends the entry
/// and joins the room. 0 = not captured / consumed.
static CONFIRM_ACTION: AtomicUsize = AtomicUsize::new(0);

/// Return the auto-join pipeline to idle (clears both timers and any captured confirm action).
fn reset_auto_join() {
    AJ_STATE.store(AJ_IDLE, Ordering::Relaxed);
    AJ_DEADLINE.store(u64::MAX, Ordering::Relaxed);
    AJ_NEXT.store(u64::MAX, Ordering::Relaxed);
    CONFIRM_ACTION.store(0, Ordering::Relaxed);
}

static CREATE_ORIG: AtomicUsize = AtomicUsize::new(0);
static PLAYOUT_ORIG: AtomicUsize = AtomicUsize::new(0);
static ENTRY_IN_ORIG: AtomicUsize = AtomicUsize::new(0);
static ENTRY_OUT_ORIG: AtomicUsize = AtomicUsize::new(0);
static CONFIRM_INIT_ORIG: AtomicUsize = AtomicUsize::new(0);
static CONFIRM_PUSH_ORIG: AtomicUsize = AtomicUsize::new(0);
static CREATE_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static PLAYOUT_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static ENTRY_IN_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static ENTRY_OUT_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static CONFIRM_INIT_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static CONFIRM_PUSH_DETOUR: OnceLock<RawDetour> = OnceLock::new();

fn store() -> &'static Mutex<Filters> {
    static S: OnceLock<Mutex<Filters>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(load_from_disk()))
}
fn rooms_buf() -> &'static Mutex<Vec<Room>> {
    static S: OnceLock<Mutex<Vec<Room>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}
fn status_buf() -> &'static Mutex<String> {
    static S: OnceLock<Mutex<String>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(String::new()))
}
fn set_status(s: String) {
    if let Ok(mut g) = status_buf().lock() {
        *g = s;
    }
}

fn clock() -> &'static Instant {
    static C: OnceLock<Instant> = OnceLock::new();
    C.get_or_init(Instant::now)
}
fn now_ms() -> u64 {
    clock().elapsed().as_millis() as u64
}

/// Human-like delay between refreshes: 2.0–5.0 s, with an occasional longer rest (~1/8,
/// +3–7 s). Same xorshift scheme as the hunter's roll cadence. The game's own reload
/// cooldown (`_reloadButtonCoolTimer`) still applies on top.
fn next_delay_ms() -> u64 {
    static SEED: AtomicU64 = AtomicU64::new(0);
    let mut s = SEED.load(Ordering::Relaxed);
    if s == 0 {
        s = (clock().elapsed().as_nanos() as u64) | 1;
    }
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    SEED.store(s, Ordering::Relaxed);
    let mut d = 2000 + (s % 3000); // 2.0–5.0 s
    if (s >> 33) % 8 == 0 {
        d += 3000 + ((s >> 5) % 4000); // ~1/8: an extra 3–7 s pause
    }
    d
}

// ── persistence ───────────────────────────────────────────────────────────────

fn json_path() -> std::path::PathBuf {
    crate::paths::local_file_migrated("trackside_room_finder.json", "heaven_room_finder.json")
}
fn load_from_disk() -> Filters {
    match std::fs::read(json_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Filters::default(),
    }
}
fn save_to_disk(f: &Filters) {
    if let Ok(json) = serde_json::to_vec_pretty(f) {
        let _ = std::fs::write(json_path(), json);
    }
}

fn log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) =
        std::fs::OpenOptions::new().create(true).append(true).open(crate::paths::log_file("trackside.log"))
    {
        let _ = writeln!(f, "[roomfinder] {msg}");
    }
}

// ── public API consumed by the overlay UI ─────────────────────────────────────

pub fn is_hunting() -> bool {
    HUNTING.load(Ordering::Relaxed)
}
pub fn checks() -> usize {
    CHECKS.load(Ordering::Relaxed)
}
pub fn status() -> String {
    status_buf().lock().map(|s| s.clone()).unwrap_or_default()
}
/// Rooms seen on the last read (for the panel's live list).
pub fn last_rooms() -> Vec<Room> {
    rooms_buf().lock().map(|r| r.clone()).unwrap_or_default()
}
/// Room id of the last match (0 = none). Non-zero drives the panel's "found" highlight.
pub fn found_room() -> i64 {
    FOUND_ROOM.load(Ordering::Relaxed)
}
/// True while the Room Match guest room-list screen is open (its controller is captured).
pub fn screen_open() -> bool {
    VC.load(Ordering::Relaxed) != 0
}
/// True while the runner-entry screen ("select your runners") is open — the "Load a saved
/// team" control only makes sense there.
pub fn entry_screen_open() -> bool {
    ENTRY_VC.load(Ordering::Relaxed) != 0
}

/// Queue loading saved team `slot` (1–5) into the current entry. Applied on the next pump
/// tick (game main thread). UI-thread safe. No-op if the entry screen isn't open.
pub fn request_load_team(slot: i32) {
    if !(1..=5).contains(&slot) {
        return;
    }
    if !entry_screen_open() {
        set_status("Open the runner-entry screen first (Join a room, then \u{201c}select your runners\u{201d}).".into());
        return;
    }
    REQ_LOAD.store(slot, Ordering::Relaxed);
    set_status(format!("Loading Team {slot}\u{2026}"));
}

pub fn filters() -> Filters {
    store().lock().map(|f| f.clone()).unwrap_or_default()
}
pub fn set_filters(f: Filters) {
    if let Ok(mut g) = store().lock() {
        *g = f;
        save_to_disk(&g);
    }
}

/// Begin hunting. The list already on screen is checked first (on the next pump tick);
/// refreshes only start if it doesn't match.
pub fn start() -> Result<(), String> {
    let f = filters();
    if f.track_id == 0
        && f.dist_cat == 0
        && f.surface == 0
        && f.season == 0
        && f.weather == 0
        && f.min_open == 0
        && !f.no_rank_restrict
        && !f.no_uma_restrict
    {
        return Err("Set at least one filter first (otherwise every room matches).".into());
    }
    if !screen_open() {
        return Err("Open the Room Match room list (Join Room) screen first.".into());
    }
    CHECKS.store(0, Ordering::Relaxed);
    FOUND_ROOM.store(0, Ordering::Relaxed);
    LIST_READY.store(false, Ordering::Relaxed);
    HUNTING.store(true, Ordering::Relaxed);
    CHECK_NOW.store(true, Ordering::Relaxed);
    NEXT_MS.store(u64::MAX, Ordering::Relaxed);
    // If we'll auto-load a team, warm the saved-team presets now so they're in work data by the
    // time a room is found (the game only fetches them when My Runners is opened).
    if f.preset_slot > 0 {
        REQ_PREFETCH.store(true, Ordering::Relaxed);
    }
    set_status(format!("Hunting: {} …", f.summary()));
    log(&format!("start: {}", f.summary()));
    Ok(())
}

pub fn stop() {
    HUNTING.store(false, Ordering::Relaxed);
    NEXT_MS.store(u64::MAX, Ordering::Relaxed);
    set_status("Stopped.".into());
}

/// Ask pump() to dump the RoomMatch class scan (RE aid). UI-thread safe.
pub fn request_scan() {
    REQ_SCAN.store(true, Ordering::Relaxed);
    set_status("Scanning game classes…".into());
}

// ── hooks (game main thread) ──────────────────────────────────────────────────

// CreateRoomListUI(List<RoomData>): the guest room-list screen finished (re)building its list —
// the exact "fresh data is in work data" moment. Also our VC capture point.
type CreateListFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void);
unsafe extern "C" fn create_list_hook(this: *mut c_void, list: *mut c_void, mi: *const c_void) {
    if !this.is_null() {
        VC.store(this as usize, Ordering::Relaxed);
        // We're on the guest room-list screen now — the runner-entry screen isn't current.
        // Cross-clearing keeps exactly one screen "live" so the entry loader UI never sticks
        // after backing out of a room we failed to join.
        ENTRY_VC.store(0, Ordering::Relaxed);
    }
    let o = CREATE_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: CreateListFn = std::mem::transmute(o);
        f(this, list, mi);
    }
    if HUNTING.load(Ordering::Relaxed) {
        LIST_READY.store(true, Ordering::Relaxed);
    }
}

// PlayOutView(): the screen's exit transition → we've left, forget the controller.
type PlayOutFn = unsafe extern "C" fn(*mut c_void, *const c_void);
unsafe extern "C" fn playout_hook(this: *mut c_void, mi: *const c_void) {
    VC.store(0, Ordering::Relaxed);
    if HUNTING.swap(false, Ordering::Relaxed) {
        set_status("Stopped (left the room list screen).".into());
    }
    let o = PLAYOUT_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: PlayOutFn = std::mem::transmute(o);
        f(this, mi);
    }
}

// RoomMatchCharacterEntryViewController.PlayInView(): the runner-entry screen is up — capture
// its controller so the preset loader can target it. Signature: (this, MethodInfo*).
type EntryInFn = unsafe extern "C" fn(*mut c_void, *const c_void);
unsafe extern "C" fn entry_in_hook(this: *mut c_void, mi: *const c_void) {
    if !this.is_null() {
        ENTRY_VC.store(this as usize, Ordering::Relaxed);
        // On the entry screen now — the guest room list isn't current (auto-open already
        // stopped the hunt on its way here).
        VC.store(0, Ordering::Relaxed);
    }
    let o = ENTRY_IN_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: EntryInFn = std::mem::transmute(o);
        f(this, mi);
    }
}

// RoomMatchCharacterEntryViewController.PlayOutView(): left the entry screen — forget it and
// drop any pending load.
type EntryOutFn = unsafe extern "C" fn(*mut c_void, *const c_void);
unsafe extern "C" fn entry_out_hook(this: *mut c_void, mi: *const c_void) {
    ENTRY_VC.store(0, Ordering::Relaxed);
    REQ_LOAD.store(0, Ordering::Relaxed);
    reset_auto_join();
    let o = ENTRY_OUT_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: EntryOutFn = std::mem::transmute(o);
        f(this, mi);
    }
}

// DialogRoomMatchConfirmCharaEntry.Initialize / .PushDialog(ExhibitionRaceEntryCharaInfo[], Action):
// the "Confirm Registration" dialog is being built — capture its OK (`onRight`) action so the
// auto-join pipeline can fire it. Same 2-arg shape for both: (this, charaArray, onRight, MethodInfo*).
type ConfirmDlgFn = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const c_void);
unsafe extern "C" fn confirm_init_hook(this: *mut c_void, chara: *mut c_void, action: *mut c_void, mi: *const c_void) {
    if !action.is_null() {
        CONFIRM_ACTION.store(action as usize, Ordering::Relaxed);
    }
    let o = CONFIRM_INIT_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: ConfirmDlgFn = std::mem::transmute(o);
        f(this, chara, action, mi);
    }
}
unsafe extern "C" fn confirm_push_hook(this: *mut c_void, chara: *mut c_void, action: *mut c_void, mi: *const c_void) {
    if !action.is_null() {
        CONFIRM_ACTION.store(action as usize, Ordering::Relaxed);
    }
    let o = CONFIRM_PUSH_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: ConfirmDlgFn = std::mem::transmute(o);
        f(this, chara, action, mi);
    }
}

// ── main-thread pump (rides hunter's TweenManager.Update hook) ────────────────

/// Run on the GAME MAIN THREAD every frame. Processes fresh lists (initial check or after a
/// refresh), fires the scheduled refresh when its time arrives, and runs queued scans.
pub fn pump() {
    if REQ_SCAN.swap(false, Ordering::Relaxed) {
        set_status(bridge::scan_dump());
    }
    if REQ_PREFETCH.swap(false, Ordering::Relaxed) {
        unsafe { bridge::prefetch_presets() };
    }
    // Load-a-saved-team is independent of hunting; handle it before the hunting gate.
    let slot = REQ_LOAD.swap(0, Ordering::Relaxed);
    if slot != 0 {
        let vc = ENTRY_VC.load(Ordering::Relaxed) as *mut c_void;
        if !unsafe { bridge::entry_ready(vc) } {
            // Staging before the entry UI has built its slots crashes the game — make the user
            // wait until the screen is actually up rather than poking unbuilt buttons.
            set_status("Entry screen still loading — try again in a moment.".into());
        } else {
            match unsafe { bridge::load_preset(vc, slot) } {
                Ok(n) => set_status(format!("Loaded Team {slot} ({n} runner{}).", if n == 1 { "" } else { "s" })),
                Err(e) => set_status(format!("Team {slot}: {e}")),
            }
        }
    }
    pump_auto_join();
    if !HUNTING.load(Ordering::Relaxed) {
        return;
    }
    // Fresh list (or the Start-time check of what's already on screen): read + match.
    if CHECK_NOW.swap(false, Ordering::Relaxed) || LIST_READY.swap(false, Ordering::Relaxed) {
        process_list();
        return;
    }
    let due = NEXT_MS.load(Ordering::Relaxed);
    if due == u64::MAX || now_ms() < due {
        return;
    }
    let vc = VC.load(Ordering::Relaxed) as *mut c_void;
    if vc.is_null() {
        return;
    }
    // Fire the game's own reload. CreateRoomListUI signals when the new list lands; if it
    // never does (cooldown swallowed the click, network hiccup), re-arm after a timeout.
    NEXT_MS.store(now_ms() + REFRESH_TIMEOUT_MS, Ordering::Relaxed);
    unsafe { bridge::click_refresh(vc) };
}

/// Drive the auto-join pipeline once a matching room has been opened: wait for the runner-entry
/// screen, let it settle, then load the preselected team and (optionally) press Confirm — all on
/// the game main thread. Runs regardless of HUNTING (the hunt already stopped when we found the
/// room).
fn pump_auto_join() {
    match AJ_STATE.load(Ordering::Relaxed) {
        AJ_AWAIT_ENTRY => {
            if ENTRY_VC.load(Ordering::Relaxed) != 0 {
                // Entry screen is up. Open the load window and try to stage right away; the
                // LOADING state will keep retrying until the UI is built and presets arrive.
                AJ_DEADLINE.store(now_ms() + AJ_LOAD_WINDOW_MS, Ordering::Relaxed);
                AJ_NEXT.store(now_ms(), Ordering::Relaxed);
                AJ_STATE.store(AJ_LOADING, Ordering::Relaxed);
            } else if now_ms() > AJ_DEADLINE.load(Ordering::Relaxed) {
                reset_auto_join();
                set_status("Auto-join: the entry screen didn't open — load your team manually.".into());
            }
        }
        AJ_LOADING => {
            if now_ms() < AJ_NEXT.load(Ordering::Relaxed) {
                return;
            }
            let vc = ENTRY_VC.load(Ordering::Relaxed) as *mut c_void;
            if vc.is_null() {
                reset_auto_join();
                return;
            }
            let slot = AJ_SLOT.load(Ordering::Relaxed);
            let expired = now_ms() > AJ_DEADLINE.load(Ordering::Relaxed);
            // Never stage until the entry UI has built its per-slot buttons — poking
            // UpdateEntryList before then dereferences objects the play-in coroutine hasn't
            // created yet (hard 0xC0000005 crash that takes the whole overlay down).
            if !unsafe { bridge::entry_ready(vc) } {
                if expired {
                    reset_auto_join();
                    set_status("Auto-join: entry screen wasn't ready in time — load your team manually.".into());
                } else {
                    AJ_NEXT.store(now_ms() + AJ_RETRY_MS, Ordering::Relaxed);
                }
                return;
            }
            match unsafe { bridge::load_preset(vc, slot) } {
                // Staged with runners — either move to confirm or hand off to the user.
                Ok(n) if n > 0 => {
                    if AJ_WANT_CONFIRM.load(Ordering::Relaxed) {
                        AJ_NEXT.store(now_ms() + AJ_CONFIRM_SETTLE_MS, Ordering::Relaxed);
                        AJ_STATE.store(AJ_CONFIRM, Ordering::Relaxed);
                        set_status(format!("Auto-join: Team {slot} loaded ({n}) — confirming…"));
                    } else {
                        reset_auto_join();
                        set_status(format!("Auto-join: Team {slot} loaded ({n} runners) — press Confirm."));
                    }
                }
                // Genuinely empty slot — no point retrying.
                Ok(_) => {
                    reset_auto_join();
                    set_status(format!("Auto-join: Team {slot} looks empty — save runners to it in-game."));
                }
                // Usually "presets still fetching" — retry until the window closes.
                Err(e) => {
                    if expired {
                        reset_auto_join();
                        set_status(format!("Auto-join: Team {slot} not loaded ({e})."));
                    } else {
                        AJ_NEXT.store(now_ms() + AJ_RETRY_MS, Ordering::Relaxed);
                    }
                }
            }
        }
        AJ_CONFIRM => {
            if now_ms() < AJ_NEXT.load(Ordering::Relaxed) {
                return;
            }
            let vc = ENTRY_VC.load(Ordering::Relaxed) as *mut c_void;
            let slot = AJ_SLOT.load(Ordering::Relaxed);
            if vc.is_null() {
                reset_auto_join();
                return;
            }
            // Ignore any stale capture, then press Confirm. OnClickDecideButton validates the
            // entry and pops the "Confirm Registration" dialog — its OK action lands in
            // CONFIRM_ACTION via the dialog hooks, which the next state fires.
            CONFIRM_ACTION.store(0, Ordering::Relaxed);
            match unsafe { bridge::confirm_entry(vc) } {
                Ok(_) => {
                    AJ_DEADLINE.store(now_ms() + AJ_DIALOG_TIMEOUT_MS, Ordering::Relaxed);
                    AJ_NEXT.store(u64::MAX, Ordering::Relaxed);
                    AJ_STATE.store(AJ_CONFIRM_DIALOG, Ordering::Relaxed);
                    set_status(format!("Auto-join: Team {slot} confirmed — accepting registration…"));
                }
                Err(e) => {
                    reset_auto_join();
                    set_status(format!("Auto-join: loaded Team {slot}; confirm failed ({e}) — press Confirm."));
                }
            }
        }
        AJ_CONFIRM_DIALOG => {
            let action = CONFIRM_ACTION.load(Ordering::Relaxed);
            if action != 0 {
                let slot = AJ_SLOT.load(Ordering::Relaxed);
                // reset clears CONFIRM_ACTION too — grab the pointer first.
                reset_auto_join();
                match unsafe { bridge::fire_confirm_ok(action as *mut c_void) } {
                    Ok(_) => set_status(format!("Auto-join: Team {slot} — registered, joining the room!")),
                    Err(e) => set_status(format!("Auto-join: registration confirm failed ({e}) — tap OK.")),
                }
            } else if now_ms() > AJ_DEADLINE.load(Ordering::Relaxed) {
                reset_auto_join();
                set_status("Auto-join: registration dialog didn't appear — tap OK to finish.".into());
            }
        }
        _ => {}
    }
}

/// A list is ready in work data: read → match → stop+alert / schedule the next refresh.
fn process_list() {
    let n = CHECKS.fetch_add(1, Ordering::Relaxed) + 1;
    let rooms = match bridge::read_rooms() {
        Ok(r) => r,
        Err(e) => {
            HUNTING.store(false, Ordering::Relaxed);
            set_status(format!("Stopped: {e}"));
            log(&format!("read failed: {e}"));
            return;
        }
    };
    if let Ok(mut g) = rooms_buf().lock() {
        *g = rooms.clone();
    }

    let f = filters();
    if let Some(hit) = rooms.iter().find(|r| f.matches(r)) {
        HUNTING.store(false, Ordering::Relaxed);
        NEXT_MS.store(u64::MAX, Ordering::Relaxed);
        FOUND_ROOM.store(hit.room_id, Ordering::Relaxed);
        let desc = describe(hit);
        log(&format!("found room {} after {n} checks: {desc}", hit.room_id));
        if f.auto_join {
            let vc = VC.load(Ordering::Relaxed) as *mut c_void;
            match unsafe { bridge::open_room(vc, hit.room_id) } {
                Ok(_) => {
                    // Preselected team? Kick off the full pipeline: the entry screen will open,
                    // pump_auto_join() then loads the team and (optionally) presses Confirm.
                    if f.preset_slot > 0 {
                        AJ_SLOT.store(f.preset_slot, Ordering::Relaxed);
                        AJ_WANT_CONFIRM.store(f.auto_confirm, Ordering::Relaxed);
                        AJ_NEXT.store(u64::MAX, Ordering::Relaxed);
                        AJ_DEADLINE.store(now_ms() + AJ_ENTRY_TIMEOUT_MS, Ordering::Relaxed);
                        AJ_STATE.store(AJ_AWAIT_ENTRY, Ordering::Relaxed);
                        let verb = if f.auto_confirm { "auto-joining" } else { "loading Team" };
                        set_status(format!("FOUND: {desc} — {verb} {}\u{2026}", f.preset_slot));
                        crate::hunter::notify("Trackside — Room found (auto-joining)!", &desc);
                    } else {
                        set_status(format!("FOUND: {desc} — entry opened, pick your runners!"));
                        crate::hunter::notify("Trackside — Room found (entry open)!", &desc);
                    }
                }
                Err(e) => {
                    set_status(format!("FOUND: {desc} — couldn't auto-open ({e}), pick it in the list"));
                    crate::hunter::notify("Trackside — Room found!", &desc);
                }
            }
        } else {
            set_status(format!("FOUND: {desc} (after {n} checks) — join it!"));
            crate::hunter::notify("Trackside — Room found!", &desc);
        }
        return;
    }

    if n >= MAX_CHECKS {
        HUNTING.store(false, Ordering::Relaxed);
        NEXT_MS.store(u64::MAX, Ordering::Relaxed);
        set_status(format!("Not found after {n} checks (stopped). Try again or loosen the filters."));
        return;
    }
    let delay = next_delay_ms();
    NEXT_MS.store(now_ms() + delay, Ordering::Relaxed);
    set_status(format!(
        "Checking… {n}/{MAX_CHECKS} · {} rooms seen · next refresh in {:.1}s",
        rooms.len(),
        delay as f32 / 1000.0
    ));
}

fn describe(r: &Room) -> String {
    let mut s = String::new();
    if !r.host.is_empty() {
        s.push_str(&format!("{}'s room", r.host));
    } else {
        s.push_str(&format!("room {}", r.room_id));
    }
    if r.track_id != 0 {
        s.push_str(&format!(" · {}", track_name(r.track_id)));
    }
    if r.distance > 0 {
        s.push_str(&format!(" {}m", r.distance));
    }
    if r.surface != 0 {
        s.push_str(&format!(" {}", surface_name(r.surface)));
    }
    if let Some(n) = r.open_slots() {
        s.push_str(&format!(" · {n} open"));
    }
    if r.rank_restricted == 0 && r.uma_restricted == 0 {
        s.push_str(" · unrestricted");
    }
    s
}

/// Boot-time install: hooks on the guest room-list screen + diagnostics. Never fatal.
pub fn install() -> String {
    bridge::install()
}

// ── IL2CPP boundary ───────────────────────────────────────────────────────────

mod bridge {
    //! Runtime-resolved access to the Room Match guest room list.
    //!
    //! Names CONFIRMED from a live scan (trackside-roommatch-scan.txt, 2026-07-02):
    //!  - WorkDataManager.get_RoomMatchData() -> Gallop.WorkRoomMatchData
    //!  - WorkRoomMatchData.get_GuestEntryRoomList() -> List<WorkRoomMatchData.RoomData>
    //!  - RoomData (nested; parent ExhibitionRaceDataBase): get_RoomId (plain Int32),
    //!    get_HostUser -> UserData, get_CurrentEntryNum (ObscuredInt),
    //!    GetRemainEntryNum() -> open slots, GetMasterRaceCourseSet() -> master course row
    //!    (race_track_id / distance / ground), get_RoomName (ObscuredString),
    //!    get_RankRestriction/get_RankRestrictionType (ObscuredInt, 0/0 = no rank gate),
    //!    IsRestrictChara() -> Boolean (true = some Umas banned).
    //!  - RoomMatchGuestEntryViewController: OnClickRoomUpdateButton() = the human reload
    //!    path (cooldown included), CreateRoomListUI(List<RoomData>) = fresh-list moment,
    //!    PlayOutView() = screen exit, _selectedRoomId @0x30 + ChangeEntryScene() = the
    //!    Join Race transition to the runner-entry screen (OpenSelectedRoomDetail = the
    //!    Details dialog, kept only as a fallback).
    //!  - Season/weather getters live on the ExhibitionRaceDataBase parent (not in the scan's
    //!    needle set) — tried as candidates; a wrong name degrades to "unknown", never a crash.

    use core::ffi::c_void;

    use retour::RawDetour;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::Room;
    use crate::il2cpp;
    // Shared IL2CPP-decode plumbing (safe runtime_invoke wrapper, Obscured decode, raw reads)
    // lives in the pruner's bridge — same WorkDataManager patterns, no point duplicating it.
    use crate::pruner::bridge::{invoke0, plain_string, rd_i32, rd_ptr, unbox_i64, work_data_manager};

    const VC_CLASS: &str = "Gallop.RoomMatchGuestEntryViewController";
    /// Runner-entry screen controller ("Please select your runners"). Confirmed in the scan:
    /// PlayInView/PlayOutView (capture), set_TempEntryCharaArray + UpdateEntryList (apply).
    const ENTRY_VC_CLASS: &str = "Gallop.RoomMatchCharacterEntryViewController";
    /// Static builder that turns the saved-preset deck dict into the same carousel ItemData
    /// list the "My Runners" dialog shows. `Gallop.RoomMatchUtil.CreateDeckItemDataList()`,
    /// 0 args (confirmed — the 2-arg `ExhibitionRaceUtil` overload is a different method).
    const UTIL_CLASS: &str = "Gallop.RoomMatchUtil";
    /// Nested ItemData carrying a saved team: get_PresetId / get_CharaList / get_HasChara.
    const ITEMDATA_OUTER: &str = "Gallop.PartsExhibitionRaceDeckCarouselItem";
    const ITEMDATA_NAME: &str = "ItemData";
    /// The "Confirm Registration" dialog shown after Confirm; its `onRight` action performs the
    /// actual room join (confirmed in the scan: `_onRight` @0x50, Initialize/PushDialog take it).
    const CONFIRM_DIALOG_CLASS: &str = "Gallop.DialogRoomMatchConfirmCharaEntry";

    /// WorkDataManager getter for the room-match blob (confirmed).
    const WDM_GETTER: &str = "get_RoomMatchData";
    /// The guest room list accessor on WorkRoomMatchData (confirmed; property getter).
    const LIST_GETTERS: &[&str] = &["get_GuestEntryRoomList", "get_MyEntryRoomList"];

    // Per-entry getters. Confirmed on RoomData unless noted; candidates (parent class,
    // unconfirmed) marked (?). il2cpp method resolution walks the parent chain, so
    // ExhibitionRaceDataBase getters resolve from the RoomData instance class.
    const G_SEASON: &[&str] = &["get_Season", "get_SeasonType"]; // (?)
    const G_WEATHER: &[&str] = &["get_Weather", "get_WeatherType"]; // (?)
    const G_HOST_NAME: &[&str] = &["get_Name", "get_TrainerName"]; // on UserData (?)
    const G_CAPACITY: &[&str] = &["get_EntryNum", "get_MaxEntryNum"]; // (?)
    // Master course row (Gallop.MasterRaceCourseSet.RaceCourseSet): getters first, then raw
    // fields (master rows are plain — no Obscured — so typed raw reads are safe).
    const COURSE_TRACK: (&[&str], &[&str]) = (&["get_RaceTrackId"], &["RaceTrackId", "race_track_id"]);
    const COURSE_DIST: (&[&str], &[&str]) = (&["get_Distance"], &["Distance", "distance"]);
    const COURSE_GROUND: (&[&str], &[&str]) = (&["get_Ground", "get_GroundType"], &["Ground", "ground"]);

    fn log(msg: &str) {
        super::log(msg);
    }

    /// Detour CreateRoomListUI (fresh-list moment + VC capture) and PlayOutView (screen exit).
    /// Run on an IL2CPP-attached thread (boot).
    pub fn install() -> String {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return "il2cpp not ready".into();
        }
        let k = il2cpp::class(VC_CLASS);
        if k.is_null() {
            return "GuestEntry VC not found".into();
        }
        let mut notes = String::new();
        unsafe {
            let m = il2cpp::method(k, "CreateRoomListUI", 1);
            let p = il2cpp::method_pointer(m);
            if p.is_null() || il2cpp::is_detoured(p) {
                notes.push_str("list:skip ");
            } else if let Ok(d) = RawDetour::new(p as *const (), super::create_list_hook as *const ()) {
                if d.enable().is_ok() {
                    super::CREATE_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = super::CREATE_DETOUR.set(d);
                    notes.push_str("list:ok ");
                } else {
                    notes.push_str("list:enable-fail ");
                }
            } else {
                notes.push_str("list:new-fail ");
            }
            let m = il2cpp::method(k, "PlayOutView", 0);
            let p = il2cpp::method_pointer(m);
            if p.is_null() || il2cpp::is_detoured(p) {
                notes.push_str("playout:skip ");
            } else if let Ok(d) = RawDetour::new(p as *const (), super::playout_hook as *const ()) {
                if d.enable().is_ok() {
                    super::PLAYOUT_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = super::PLAYOUT_DETOUR.set(d);
                    notes.push_str("playout:ok ");
                } else {
                    notes.push_str("playout:enable-fail ");
                }
            } else {
                notes.push_str("playout:new-fail ");
            }
        }
        // Runner-entry screen capture (for the saved-team loader). Best-effort: a miss here
        // only disables the loader, never the finder.
        notes.push_str(&install_entry_hooks());
        notes.push(' ');
        // "Confirm Registration" dialog capture (for auto-confirm's final OK). Best-effort too.
        notes.push_str(&install_confirm_hooks());
        format!("room finder: {}", notes.trim())
    }

    /// Detour the runner-entry controller's PlayIn/PlayOut so we can target it for preset loads.
    fn install_entry_hooks() -> String {
        let k = il2cpp::class(ENTRY_VC_CLASS);
        if k.is_null() {
            return "entry:vc-not-found".into();
        }
        let mut notes = String::new();
        unsafe {
            let m = il2cpp::method(k, "PlayInView", 0);
            let p = il2cpp::method_pointer(m);
            if p.is_null() || il2cpp::is_detoured(p) {
                notes.push_str("entry-in:skip ");
            } else if let Ok(d) = RawDetour::new(p as *const (), super::entry_in_hook as *const ()) {
                if d.enable().is_ok() {
                    super::ENTRY_IN_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = super::ENTRY_IN_DETOUR.set(d);
                    notes.push_str("entry-in:ok ");
                } else {
                    notes.push_str("entry-in:enable-fail ");
                }
            } else {
                notes.push_str("entry-in:new-fail ");
            }
            let m = il2cpp::method(k, "PlayOutView", 0);
            let p = il2cpp::method_pointer(m);
            if p.is_null() || il2cpp::is_detoured(p) {
                notes.push_str("entry-out:skip");
            } else if let Ok(d) = RawDetour::new(p as *const (), super::entry_out_hook as *const ()) {
                if d.enable().is_ok() {
                    super::ENTRY_OUT_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = super::ENTRY_OUT_DETOUR.set(d);
                    notes.push_str("entry-out:ok");
                } else {
                    notes.push_str("entry-out:enable-fail");
                }
            } else {
                notes.push_str("entry-out:new-fail");
            }
        }
        notes
    }

    /// Detour the "Confirm Registration" dialog's Initialize + PushDialog to capture its OK
    /// (`onRight`) action. Both carry the same (ExhibitionRaceEntryCharaInfo[], System.Action)
    /// shape; whichever the game calls, we grab the action. Best-effort: a miss here only means
    /// auto-confirm stops at the dialog for a manual OK tap.
    fn install_confirm_hooks() -> String {
        let k = il2cpp::class(CONFIRM_DIALOG_CLASS);
        if k.is_null() {
            return "confirm:dlg-not-found".into();
        }
        let mut notes = String::new();
        unsafe {
            let m = il2cpp::method(k, "Initialize", 2);
            let p = il2cpp::method_pointer(m);
            if p.is_null() || il2cpp::is_detoured(p) {
                notes.push_str("confirm-init:skip ");
            } else if let Ok(d) = RawDetour::new(p as *const (), super::confirm_init_hook as *const ()) {
                if d.enable().is_ok() {
                    super::CONFIRM_INIT_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = super::CONFIRM_INIT_DETOUR.set(d);
                    notes.push_str("confirm-init:ok ");
                } else {
                    notes.push_str("confirm-init:enable-fail ");
                }
            } else {
                notes.push_str("confirm-init:new-fail ");
            }
            let m = il2cpp::method(k, "PushDialog", 2);
            let p = il2cpp::method_pointer(m);
            if p.is_null() || il2cpp::is_detoured(p) {
                notes.push_str("confirm-push:skip");
            } else if let Ok(d) = RawDetour::new(p as *const (), super::confirm_push_hook as *const ()) {
                if d.enable().is_ok() {
                    super::CONFIRM_PUSH_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = super::CONFIRM_PUSH_DETOUR.set(d);
                    notes.push_str("confirm-push:ok");
                } else {
                    notes.push_str("confirm-push:enable-fail");
                }
            } else {
                notes.push_str("confirm-push:new-fail");
            }
        }
        notes
    }

    /// First getter that returns a decodable integer, else first raw field with a plain
    /// integer type (width from metadata — never a blind 8-byte read).
    unsafe fn read_num(e: *mut c_void, k: il2cpp::Class, getters: &[&str], fields: &[&str]) -> Option<i64> {
        for g in getters {
            if let Some(v) = unbox_i64(invoke0(e, k, g)) {
                return Some(v);
            }
        }
        for f in fields {
            if let Some(off) = il2cpp::field_offset(k, f) {
                let ty = il2cpp::class_fields(k)
                    .into_iter()
                    .find(|(n, _, _)| n == f)
                    .map(|(_, _, t)| t)
                    .unwrap_or_default();
                return match ty.as_str() {
                    "System.Int64" | "System.UInt64" => Some(*((e as usize + off) as *const i64)),
                    "System.Int32" | "System.UInt32" => Some(*((e as usize + off) as *const i32) as i64),
                    "System.Int16" | "System.UInt16" => Some(*((e as usize + off) as *const i16) as i64),
                    "System.Byte" | "System.SByte" => Some(*((e as usize + off) as *const i8) as i64),
                    _ => None, // Obscured or reference type: raw bytes would be garbage
                };
            }
        }
        None
    }

    /// Read the live guest room list from work data. MAIN THREAD ONLY (managed calls).
    pub fn read_rooms() -> Result<Vec<Room>, String> {
        if !il2cpp::ready() {
            return Err("IL2CPP runtime not ready".into());
        }
        unsafe {
            let wdm = work_data_manager();
            if wdm.is_null() {
                return Err("WorkDataManager not loaded".into());
            }
            let wdm_class = il2cpp::class("Gallop.WorkDataManager");
            let blob = invoke0(wdm, wdm_class, WDM_GETTER);
            if blob.is_null() {
                return Err("room-match work-data not found (enter the Room Match screens first)".into());
            }
            let blob_class = il2cpp::object_class(blob);
            let mut list = std::ptr::null_mut();
            for g in LIST_GETTERS {
                list = invoke0(blob, blob_class, g);
                if !list.is_null() {
                    break;
                }
            }
            if list.is_null() {
                return Err("guest room list not readable (open the room list screen, refresh once)".into());
            }
            // List<T>: _items @0x10 (T[]), _size @0x18 ; array data @0x20, 8-byte refs
            let items = rd_ptr(list, 0x10);
            let size = rd_i32(list, 0x18);
            if items.is_null() || size < 0 {
                return Err("room list layout unexpected (run Scan and send the log)".into());
            }
            let mut out = Vec::with_capacity(size as usize);
            for i in 0..size as usize {
                let e = rd_ptr(items, 0x20 + i * 8);
                if e.is_null() {
                    continue;
                }
                if let Some(r) = read_entry(e) {
                    out.push(r);
                }
            }
            Ok(out)
        }
    }

    /// Decode one WorkRoomMatchData.RoomData via its getters (Obscured-safe through
    /// invoke0 + unbox_i64/plain_string; inherited getters resolve through the parent chain).
    unsafe fn read_entry(e: *mut c_void) -> Option<Room> {
        let k = il2cpp::object_class(e);
        if k.is_null() {
            return None;
        }
        let room_id = unbox_i64(invoke0(e, k, "get_RoomId"))?;
        if room_id == 0 {
            return None;
        }
        // Host trainer name (UserData), falling back to the room's name.
        let mut host = String::new();
        let hu = invoke0(e, k, "get_HostUser");
        if !hu.is_null() {
            let hk = il2cpp::object_class(hu);
            for g in G_HOST_NAME {
                host = plain_string(invoke0(hu, hk, g));
                if !host.is_empty() {
                    break;
                }
            }
        }
        if host.is_empty() {
            host = plain_string(invoke0(e, k, "get_RoomName"));
        }
        // Course row: track / distance / surface.
        let (mut track_id, mut distance, mut surface) = (0i32, 0i32, 0i32);
        let course = invoke0(e, k, "GetMasterRaceCourseSet");
        if !course.is_null() {
            let ck = il2cpp::object_class(course);
            track_id = read_num(course, ck, COURSE_TRACK.0, COURSE_TRACK.1).unwrap_or(0) as i32;
            distance = read_num(course, ck, COURSE_DIST.0, COURSE_DIST.1).unwrap_or(0) as i32;
            surface = read_num(course, ck, COURSE_GROUND.0, COURSE_GROUND.1).unwrap_or(0) as i32;
        }
        // Career-rank gate: get_RankRestriction / get_RankRestrictionType (ObscuredInts,
        // confirmed fields @0xAC/@0xC0). Both 0 = "Career Rank: None" in the room settings;
        // either non-zero = gated. Both undecodable = unknown (-1).
        let rank_restricted = {
            let r = unbox_i64(invoke0(e, k, "get_RankRestriction"));
            let t = unbox_i64(invoke0(e, k, "get_RankRestrictionType"));
            match (r, t) {
                (None, None) => -1,
                (r, t) => (r.unwrap_or(0) != 0 || t.unwrap_or(0) != 0) as i32,
            }
        };
        // Uma bans: the game's own IsRestrictChara() -> Boolean (confirmed method).
        let uma_restricted = unbox_i64(invoke0(e, k, "IsRestrictChara")).map(|b| (b != 0) as i32).unwrap_or(-1);
        Some(Room {
            room_id,
            host,
            track_id,
            distance,
            surface,
            season: read_num(e, k, G_SEASON, &[]).unwrap_or(0) as i32,
            weather: read_num(e, k, G_WEATHER, &[]).unwrap_or(0) as i32,
            members: unbox_i64(invoke0(e, k, "get_CurrentEntryNum")).unwrap_or(-1) as i32,
            capacity: read_num(e, k, G_CAPACITY, &[]).unwrap_or(-1) as i32,
            remain: unbox_i64(invoke0(e, k, "GetRemainEntryNum")).unwrap_or(-1) as i32,
            rank_restricted,
            uma_restricted,
        })
    }

    /// Drive the game's own reload exactly like tapping the button: OnClickRoomUpdateButton
    /// builds the proper success callback (rebuilds the list UI → our CreateRoomListUI hook
    /// fires) and respects the reload cooldown. MAIN THREAD ONLY.
    pub unsafe fn click_refresh(vc: *mut c_void) {
        if vc.is_null() {
            return;
        }
        let k = il2cpp::class(VC_CLASS);
        if k.is_null() {
            return;
        }
        let m = il2cpp::method(k, "OnClickRoomUpdateButton", 0);
        if m.is_null() {
            log("click_refresh: OnClickRoomUpdateButton not found");
            return;
        }
        il2cpp::runtime_invoke(m, vc, &mut []);
    }

    /// Select the found room and jump straight to the game's runner-entry screen
    /// (RoomMatchCharacterEntryViewController — "Please select your runners"), exactly like
    /// tapping the room's Join Race button: set `_selectedRoomId`, then `ChangeEntryScene()`
    /// builds the entry ViewInfo from the selected RoomData and transitions. The actual join
    /// request (SendRoomMatchEntryRoomAPI) only fires when the user Confirms with their picks.
    /// Falls back to the detail dialog if the entry transition isn't resolvable. MAIN THREAD ONLY.
    pub unsafe fn open_room(vc: *mut c_void, room_id: i64) -> Result<(), String> {
        if vc.is_null() {
            return Err("room list screen not captured".into());
        }
        let k = il2cpp::class(VC_CLASS);
        if k.is_null() {
            return Err("GuestEntry VC class not found".into());
        }
        // _selectedRoomId : System.Int32 @0x30 (confirmed) — resolve by name, offset as backstop.
        let off = il2cpp::field_offset(k, "_selectedRoomId").unwrap_or(0x30);
        *((vc as usize + off) as *mut i32) = room_id as i32;
        let m = il2cpp::method(k, "ChangeEntryScene", 0);
        if !m.is_null() {
            il2cpp::runtime_invoke(m, vc, &mut []);
            return Ok(());
        }
        log("open_room: ChangeEntryScene not found, falling back to detail dialog");
        let m = il2cpp::method(k, "OpenSelectedRoomDetail", 0);
        if m.is_null() {
            return Err("neither ChangeEntryScene nor OpenSelectedRoomDetail found".into());
        }
        il2cpp::runtime_invoke(m, vc, &mut []);
        Ok(())
    }

    /// True once the runner-entry screen has built its per-slot character buttons. The screen's
    /// play-in is a coroutine, so `_charaEntryButtonList` (@0x38) is null/empty for a beat after
    /// PlayInView fires; staging (`set_TempEntryCharaArray` + `UpdateEntryList`) before the
    /// buttons exist dereferences unbuilt objects and hard-crashes the game. Gate every stage on
    /// this. MAIN THREAD ONLY.
    pub unsafe fn entry_ready(entry_vc: *mut c_void) -> bool {
        if entry_vc.is_null() || !il2cpp::ready() {
            return false;
        }
        let ek = il2cpp::class(ENTRY_VC_CLASS);
        if ek.is_null() {
            return false;
        }
        let off = il2cpp::field_offset(ek, "_charaEntryButtonList").unwrap_or(0x38);
        let list = rd_ptr(entry_vc, off);
        if list.is_null() {
            return false;
        }
        rd_i32(list, 0x18) > 0
    }

    /// Load saved team `slot` (1–5) into the runner-entry screen, mirroring the "My Runners"
    /// dialog's "Load List" button: build the same saved-team carousel data the dialog uses
    /// (`RoomMatchUtil.CreateDeckItemDataList`), pick the slot's ItemData, and stage its runner
    /// list on the entry controller via the exact fields the game's own load path writes
    /// (`set_TempEntryCharaArray` + `UpdateEntryList`) — no dialog, no delegates. Returns the
    /// runner count. MAIN THREAD ONLY.
    pub unsafe fn load_preset(entry_vc: *mut c_void, slot: i32) -> Result<i32, String> {
        if entry_vc.is_null() {
            return Err("runner-entry screen not open".into());
        }
        if !il2cpp::ready() {
            return Err("IL2CPP runtime not ready".into());
        }
        // Build the saved-team list exactly like the dialog (static, 0 args) → List<ItemData>.
        let util = il2cpp::class(UTIL_CLASS);
        let make = il2cpp::method(util, "CreateDeckItemDataList", 0);
        if make.is_null() {
            return Err("CreateDeckItemDataList not found (run Scan)".into());
        }
        let list = il2cpp::runtime_invoke(make, std::ptr::null_mut(), &mut []);
        // List<T>: _items @0x10 (T[]), _size @0x18 ; array data @0x20, 8-byte refs.
        let items = if list.is_null() { std::ptr::null_mut() } else { rd_ptr(list, 0x10) };
        let size = if list.is_null() { 0 } else { rd_i32(list, 0x18) };
        if list.is_null() || items.is_null() || size <= 0 {
            // The game only fetches presets when My Runners is opened; kick off that fetch so a
            // retry (or the auto-join pipeline's next pass) succeeds.
            prefetch_presets();
            return Err("fetching your saved teams\u{2026} try again in a second".into());
        }
        let idata = il2cpp::nested_class(ITEMDATA_OUTER, ITEMDATA_NAME);
        if idata.is_null() {
            return Err("ItemData class not found (run Scan)".into());
        }
        // Prefer the item whose own PresetId matches the slot; fall back to list position.
        let mut chosen: *mut c_void = std::ptr::null_mut();
        let mut fallback: *mut c_void = std::ptr::null_mut();
        for i in 0..size as usize {
            let it = rd_ptr(items, 0x20 + i * 8);
            if it.is_null() {
                continue;
            }
            let pid = unbox_i64(invoke0(it, idata, "get_PresetId")).unwrap_or(0) as i32;
            if pid == slot {
                chosen = it;
                break;
            }
            if i + 1 == slot as usize {
                fallback = it;
            }
        }
        let item = if !chosen.is_null() { chosen } else { fallback };
        if item.is_null() {
            return Err(format!("no Team {slot} slot found"));
        }
        // Empty slot? get_HasChara distinguishes a saved team from a "Please save…" placeholder.
        if unbox_i64(invoke0(item, idata, "get_HasChara")).unwrap_or(0) == 0 {
            return Err("that team slot is empty — save runners to it in-game first".into());
        }
        let chara_list = invoke0(item, idata, "get_CharaList");
        if chara_list.is_null() {
            return Err("team has no runner list".into());
        }
        // List<ExhibitionRaceEntryCharaInfo>.ToArray() → the array set_TempEntryCharaArray wants.
        let lclass = il2cpp::object_class(chara_list);
        let to_array = il2cpp::method(lclass, "ToArray", 0);
        if to_array.is_null() {
            return Err("List.ToArray unavailable".into());
        }
        let arr = il2cpp::runtime_invoke(to_array, chara_list, &mut []);
        if arr.is_null() {
            return Err("couldn't build the runner array".into());
        }
        let count = rd_i32(arr, 0x18); // IL2CPP array: max_length @0x18, data @0x20.
        let ek = il2cpp::class(ENTRY_VC_CLASS);
        let set_m = il2cpp::method(ek, "set_TempEntryCharaArray", 1);
        if set_m.is_null() {
            return Err("set_TempEntryCharaArray not found (run Scan)".into());
        }
        let mut args: [*mut c_void; 1] = [arr];
        il2cpp::runtime_invoke(set_m, entry_vc, &mut args);
        // Refresh the on-screen entry slots from the staged array.
        let upd = il2cpp::method(ek, "UpdateEntryList", 0);
        if !upd.is_null() {
            il2cpp::runtime_invoke(upd, entry_vc, &mut []);
        }
        log(&format!("loaded team {slot}: {count} runners"));
        Ok(count.max(0))
    }

    /// Press the entry screen's Confirm button (`OnClickDecideButton`) — the same validated path
    /// as tapping Confirm, which sends `SendRoomMatchEntryRoomAPI`. MAIN THREAD ONLY.
    pub unsafe fn confirm_entry(entry_vc: *mut c_void) -> Result<(), String> {
        if entry_vc.is_null() {
            return Err("entry screen not open".into());
        }
        let ek = il2cpp::class(ENTRY_VC_CLASS);
        let m = il2cpp::method(ek, "OnClickDecideButton", 0);
        if m.is_null() {
            return Err("Confirm handler not found".into());
        }
        il2cpp::runtime_invoke(m, entry_vc, &mut []);
        Ok(())
    }

    /// Fire the "Confirm Registration" dialog's captured OK action (`System.Action.Invoke`) —
    /// the exact continuation tapping OK runs, which sends the entry and joins the room. MAIN
    /// THREAD ONLY.
    pub unsafe fn fire_confirm_ok(action: *mut c_void) -> Result<(), String> {
        if action.is_null() {
            return Err("no confirm action".into());
        }
        let ac = il2cpp::class("System.Action");
        if ac.is_null() {
            return Err("System.Action class missing".into());
        }
        let m = il2cpp::method(ac, "Invoke", 0);
        if m.is_null() {
            return Err("Action.Invoke missing".into());
        }
        il2cpp::runtime_invoke(m, action, &mut []);
        Ok(())
    }

    /// Fire `RoomMatchRaceGetPresetArrayRequest` so the server populates `WorkRoomMatchData`'s
    /// deck dict (the source `CreateDeckItemDataList` reads). Fire-and-forget: the game's own
    /// response handler applies the result. Same inherited `RequestBase.Send(7)` path the pruner
    /// uses (null callbacks + all UI flags false = silent). MAIN THREAD ONLY.
    pub unsafe fn prefetch_presets() {
        if !il2cpp::ready() {
            return;
        }
        // The load retry loop calls this every ~600 ms until presets arrive; don't spam the
        // server — one request every few seconds is plenty for the response to land.
        static LAST_PREFETCH_MS: AtomicU64 = AtomicU64::new(0);
        let now = super::now_ms();
        let last = LAST_PREFETCH_MS.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < 3000 {
            return;
        }
        LAST_PREFETCH_MS.store(now, Ordering::Relaxed);
        let k = il2cpp::class("Gallop.RoomMatchRaceGetPresetArrayRequest");
        if k.is_null() {
            log("prefetch: GetPresetArray request class not found");
            return;
        }
        let req = il2cpp::object_new(k);
        if req.is_null() {
            log("prefetch: alloc failed");
            return;
        }
        let m = il2cpp::method(k, "Send", 7);
        if m.is_null() {
            log("prefetch: Send(7) not found");
            return;
        }
        let mut flags: [u8; 5] = [0; 5];
        let mut args: [*mut c_void; 7] = [
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (&mut flags[0]) as *mut u8 as *mut c_void,
            (&mut flags[1]) as *mut u8 as *mut c_void,
            (&mut flags[2]) as *mut u8 as *mut c_void,
            (&mut flags[3]) as *mut u8 as *mut c_void,
            (&mut flags[4]) as *mut u8 as *mut c_void,
        ];
        il2cpp::runtime_invoke(m, req, &mut args);
        log("prefetch: GetPresetArray sent");
    }

    /// Substrings (case-insensitive, matched on the SIMPLE class name) used to gather the
    /// Room Match surface. "preset"/"deck"/"carousel"/"entrycharainfo"/"racepreset" pull in
    /// the "My Runners" preset dialog + saved-team carousel; "workdatautil" carries the
    /// RacePresetData store. Image enumeration only sees TOP-LEVEL classes, so scan_dump also
    /// walks each hit's nested types (that's where RacePresetData / ItemData live).
    const SCAN_NEEDLES: &[&str] = &[
        "roommatch",
        "room",
        "exhibition",
        "preset",
        "deck",
        "carousel",
        "entrycharainfo",
        "racepreset",
        "workdatautil",
    ];

    /// Dump every loaded class matching SCAN_NEEDLES — plus their nested types and parent
    /// chains — with methods and fields, to `trackside-logs/trackside-roommatch-scan.txt`.
    /// MAIN THREAD ONLY.
    pub fn scan_dump() -> String {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return "Scan failed: IL2CPP runtime not ready".into();
        }
        let mut hits = Vec::new();
        for needle in SCAN_NEEDLES {
            hits.extend(il2cpp::find_classes(needle));
        }
        // Pull in nested types of every top-level hit (RacePresetData, ItemData, …), named
        // "Outer.Nested" so the parent-chain grep and lookups stay readable.
        let mut nested: Vec<(String, il2cpp::Class)> = Vec::new();
        for (full, k) in &hits {
            for (nname, nk) in il2cpp::nested_types(*k) {
                nested.push((format!("{full}.{nname}"), nk));
            }
        }
        hits.extend(nested);
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        hits.dedup_by(|a, b| a.0 == b.0);
        if hits.is_empty() {
            return "Scan found nothing (class enumeration unavailable in this runtime?)".into();
        }
        let mut out = String::new();
        out.push_str("Trackside room-finder class scan (methods, fields, parent chains)\n\n");
        for (full, k) in &hits {
            crate::pruner::bridge::dump_class(&mut out, full, *k);
        }
        let dumped: std::collections::HashSet<String> = hits.iter().map(|(n, _)| n.clone()).collect();
        let mut parents: Vec<(String, il2cpp::Class)> = Vec::new();
        for (full, k) in &hits {
            if !full.starts_with("Gallop.") {
                continue;
            }
            let mut p = il2cpp::class_parent(*k);
            while !p.is_null() {
                let n = il2cpp::class_full_name(p);
                if n == "System.Object" || n.is_empty() {
                    break;
                }
                if !dumped.contains(&n) && !parents.iter().any(|(pn, _)| pn == &n) {
                    parents.push((n.clone(), p));
                }
                p = il2cpp::class_parent(p);
            }
        }
        if !parents.is_empty() {
            out.push_str("──── parent classes (inherited entry points) ────\n\n");
            parents.sort_by(|a, b| a.0.cmp(&b.0));
            for (full, k) in &parents {
                crate::pruner::bridge::dump_class(&mut out, full, *k);
            }
        }
        let path = crate::paths::log_file("trackside-roommatch-scan.txt");
        let n_par = parents.len();
        match std::fs::write(&path, out) {
            Ok(_) => format!("Scan: {} classes + {n_par} parents -> {}", hits.len(), path.display()),
            Err(e) => format!("Scan write failed: {e}"),
        }
    }
}
