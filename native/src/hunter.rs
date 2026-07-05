//! hunter — Team Trials "opponent hunter": auto-refresh the Select Opponent screen until a target
//! trainer appears, then stop + alert. For people farming a specific padder.
//!
//! There is NO friend-challenge in Global TT (the screen only offers 3 random opponents + Reload),
//! so rolling is the only way to reach a specific player. We drive the game's OWN refresh
//! (`TeamStadiumOpponentSelectViewController.SendApi`) instead of firing a raw request — same as the
//! padder feature: it goes through the validated flow, on the main thread, at the game's natural
//! cadence. The loop is self-driving: `OnOpponentInEnd` fires when the 3 new opponents finish
//! loading → we read them, check vs the target, and (if no match) trigger the next refresh there.
//!
//! Match keys: exact `opponent_viewer_id` (read from OpponentData.ServerData.opponent_viewer_id, a
//! plain Int64 — no Obscured decode) AND/OR trainer name (GetTrainerName), case-insensitive substring.
//!
//! Kickoff: press Start in the menu, then click Reload once in-game — that first load fires
//! OnOpponentInEnd and the loop takes over (SendApi can't be called from the render thread).

#![allow(static_mut_refs)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use retour::RawDetour;

use crate::il2cpp;

static HUNTING: AtomicBool = AtomicBool::new(false);
static FOUND: AtomicBool = AtomicBool::new(false);
// On Start, check the already-offered 3 once (on the main thread) BEFORE rolling — so a target
// that's already on screen is found immediately instead of rolled past.
static CHECK_NOW: AtomicBool = AtomicBool::new(false);
// The matched target, for the on-screen alert. FOUND_VID != 0 ⇒ show the banner.
static FOUND_VID: AtomicI64 = AtomicI64::new(0);
fn found_name_buf() -> &'static Mutex<String> {
    static S: OnceLock<Mutex<String>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(String::new()))
}
/// The matched target (viewer_id, name) for the on-screen alert; vid 0 = nothing found.
pub fn found_vid() -> i64 {
    FOUND_VID.load(Ordering::Relaxed)
}
pub fn found_name() -> String {
    found_name_buf().lock().map(|s| s.clone()).unwrap_or_default()
}
static ROLLS: AtomicUsize = AtomicUsize::new(0);
static MAX_ROLLS: AtomicUsize = AtomicUsize::new(40);
static LAST_TICK_MS: AtomicU64 = AtomicU64::new(0);
// When the next auto-roll is allowed (ms on our clock). u64::MAX = nothing scheduled. The per-frame
// pump fires SendApi only once this time arrives → human-like jittered cadence, not machine-gun.
static NEXT_ROLL_MS: AtomicU64 = AtomicU64::new(u64::MAX);
// The live OpponentSelect view controller: set when its opponents load (OnOpponentInEnd), cleared
// when the screen transitions out (PlayOut). Non-zero ⇒ we're on the Select Opponent screen.
static VC: AtomicUsize = AtomicUsize::new(0);

static ONEND_ORIG: AtomicUsize = AtomicUsize::new(0);
static PLAYOUT_ORIG: AtomicUsize = AtomicUsize::new(0);
static DETOUR: OnceLock<RawDetour> = OnceLock::new();
static PLAYOUT_DETOUR: OnceLock<RawDetour> = OnceLock::new();

/// Human-like delay before the next auto-roll: 1.8–4.8 s, with an occasional longer "rest" (~1/8 of
/// the time, +3–7 s) so the cadence isn't a constant drum-beat. Uses a tiny xorshift seeded once.
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
    let mut d = 1800 + (s % 3000); // 1.8–4.8 s
    if (s >> 33) % 8 == 0 {
        d += 3000 + ((s >> 5) % 4000); // ~1/8: an extra 3–7 s pause
    }
    d
}

fn clock() -> &'static Instant {
    crate::tools::clock()
}
fn now_ms() -> u64 {
    crate::tools::now_ms()
}

fn target_vid() -> &'static Mutex<Option<i64>> {
    static S: OnceLock<Mutex<Option<i64>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}
fn target_name() -> &'static Mutex<Option<String>> {
    static S: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}
fn status_buf() -> &'static Mutex<String> {
    static S: OnceLock<Mutex<String>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(String::new()))
}
fn last_three_buf() -> &'static Mutex<Vec<(i64, String)>> {
    static S: OnceLock<Mutex<Vec<(i64, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Vec::new()))
}

fn set_status(s: String) {
    if let Ok(mut g) = status_buf().lock() {
        *g = s;
    }
}

// ── persisted target (survives restarts) ──────────────────────────────────────
fn target_path() -> std::path::PathBuf {
    crate::paths::local_file_migrated("trackside_tt_hunter.json", "heaven_tt_hunter.json")
}

/// Persist the last-entered target name + viewer ID, so they're there after a restart.
pub fn save_target(name: &str, vid: &str) {
    let v = serde_json::json!({ "name": name, "vid": vid });
    let _ = std::fs::write(target_path(), v.to_string());
}

/// Load the saved target (name, vid) — empty strings if none saved.
pub fn saved_target() -> (String, String) {
    if let Ok(b) = std::fs::read(target_path()) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) {
            let n = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let d = v.get("vid").and_then(|x| x.as_str()).unwrap_or("").to_string();
            return (n, d);
        }
    }
    (String::new(), String::new())
}

fn log(msg: &str) {
    crate::tools::log(&format!("[hunter] {msg}"));
}

// ── public API (menu) ─────────────────────────────────────────────────────────

/// Begin hunting. `name` and/or `vid` may be empty; at least one must be given. `vid` parsed as i64.
pub fn start(name: &str, vid: &str) -> Result<(), String> {
    let name = name.trim();
    let vid = vid.trim();
    if name.is_empty() && vid.is_empty() {
        return Err("Enter a name or a viewer ID.".into());
    }
    let vid_parsed = if vid.is_empty() {
        None
    } else {
        Some(vid.parse::<i64>().map_err(|_| "Invalid viewer ID (must be a number).".to_string())?)
    };
    if !screen_open() {
        return Err("Open the Team Trials Select Opponent screen first.".into());
    }
    *target_vid().lock().map_err(|_| "lock")? = vid_parsed;
    *target_name().lock().map_err(|_| "lock")? =
        if name.is_empty() { None } else { Some(name.to_lowercase()) };
    ROLLS.store(0, Ordering::Relaxed);
    FOUND.store(false, Ordering::Relaxed);
    FOUND_VID.store(0, Ordering::Relaxed); // dismiss any previous on-screen alert
    HUNTING.store(true, Ordering::Relaxed);
    // First the frame pump checks the 3 already on screen; if no match it schedules the first roll.
    CHECK_NOW.store(true, Ordering::Relaxed);
    NEXT_ROLL_MS.store(u64::MAX, Ordering::Relaxed);
    set_status("Hunting…".into());
    Ok(())
}

pub fn stop() {
    HUNTING.store(false, Ordering::Relaxed);
    NEXT_ROLL_MS.store(u64::MAX, Ordering::Relaxed);
    set_status("Stopped.".into());
}

pub fn is_hunting() -> bool {
    HUNTING.load(Ordering::Relaxed)
}
pub fn found() -> bool {
    FOUND.load(Ordering::Relaxed)
}
pub fn rolls() -> usize {
    ROLLS.load(Ordering::Relaxed)
}
pub fn max_rolls() -> usize {
    MAX_ROLLS.load(Ordering::Relaxed)
}
pub fn status() -> String {
    status_buf().lock().map(|s| s.clone()).unwrap_or_default()
}
pub fn last_three() -> Vec<(i64, String)> {
    last_three_buf().lock().map(|s| s.clone()).unwrap_or_default()
}
/// True while the Select Opponent screen is open (its controller is captured, not yet torn down).
pub fn screen_open() -> bool {
    VC.load(Ordering::Relaxed) != 0
}

// ── the OnOpponentInEnd hook: read → check → re-roll (game main thread) ────────

type OnEndFn = unsafe extern "C" fn(*mut c_void, i32, *const c_void);
unsafe extern "C" fn onend_hook(this: *mut c_void, index: i32, mi: *const c_void) {
    if !this.is_null() {
        VC.store(this as usize, Ordering::Relaxed);
    }
    let o = ONEND_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: OnEndFn = std::mem::transmute(o);
        f(this, index, mi);
    }
    if !HUNTING.load(Ordering::Relaxed) {
        return;
    }
    // OnOpponentInEnd fires once per opponent card (3×/batch). Debounce so we process a batch once.
    let now = now_ms();
    if now.saturating_sub(LAST_TICK_MS.load(Ordering::Relaxed)) < 700 {
        return;
    }
    LAST_TICK_MS.store(now, Ordering::Relaxed);
    process_batch();
}

/// A fresh batch of 3 opponents finished loading: read them, check vs the target, and either stop
/// (found / cap) or SCHEDULE the next roll after a human-like delay (the per-frame pump fires it).
unsafe fn process_batch() {
    let opps = read_opponents();
    if let Ok(mut g) = last_three_buf().lock() {
        *g = opps.clone();
    }
    if opps.is_empty() {
        return; // nothing loaded yet; wait for the next batch
    }
    // match: exact viewer_id OR name substring (case-insensitive)
    let tvid = *target_vid().lock().unwrap();
    let tname = target_name().lock().unwrap().clone();
    let hit = opps.iter().find(|(vid, name)| {
        (tvid.is_some() && tvid == Some(*vid))
            || tname.as_ref().map(|t| name.to_lowercase().contains(t)).unwrap_or(false)
    });
    if let Some((vid, name)) = hit {
        FOUND.store(true, Ordering::Relaxed);
        HUNTING.store(false, Ordering::Relaxed);
        NEXT_ROLL_MS.store(u64::MAX, Ordering::Relaxed);
        FOUND_VID.store(*vid, Ordering::Relaxed);
        if let Ok(mut g) = found_name_buf().lock() {
            *g = name.clone();
        }
        set_status(format!("FOUND: {name} ({vid}) after {} rolls — pick them!", rolls()));
        log(&format!("found target {name} ({vid}) after {} rolls", rolls()));
        alert(name, *vid);
        return;
    }
    let names: Vec<&str> = opps.iter().map(|(_, n)| n.as_str()).collect();
    if rolls() >= max_rolls() {
        HUNTING.store(false, Ordering::Relaxed);
        NEXT_ROLL_MS.store(u64::MAX, Ordering::Relaxed);
        set_status(format!("Not found after {} rolls (stopped). Pool is random — try again.", rolls()));
        return;
    }
    // schedule the next roll with a human-like delay (fired by frame_pump)
    let delay = next_delay_ms();
    NEXT_ROLL_MS.store(now_ms() + delay, Ordering::Relaxed);
    set_status(format!("Rolling… {}/{} · next in {:.1}s · last: {}", rolls(), max_rolls(), delay as f32 / 1000.0, names.join(", ")));
}

/// Per-frame, main thread (driven by TweenManager.Update): fire the scheduled roll when its time
/// arrives. Keeps the actual SendApi on the main thread while the delay/jitter lives here.
pub fn frame_pump() {
    if !HUNTING.load(Ordering::Relaxed) || !screen_open() {
        return;
    }
    // Initial check of the already-loaded 3 (main thread) before any roll — catches a target that
    // is already on screen. process_batch() either finds it or schedules the first roll.
    if CHECK_NOW.swap(false, Ordering::Relaxed) {
        unsafe { process_batch() };
        return;
    }
    let due = NEXT_ROLL_MS.load(Ordering::Relaxed);
    if due == u64::MAX || now_ms() < due {
        return;
    }
    let vc = VC.load(Ordering::Relaxed) as *mut c_void;
    if vc.is_null() {
        return;
    }
    NEXT_ROLL_MS.store(u64::MAX, Ordering::Relaxed); // consume; OnOpponentInEnd reschedules next
    ROLLS.fetch_add(1, Ordering::Relaxed);
    unsafe { send_api(vc) };
}

/// Read the 3 currently-offered opponents: (viewer_id, trainer_name). Pure-ish reads + 1 managed
/// call (GetTrainerName) per opponent — fine on the main thread (we're inside OnOpponentInEnd).
unsafe fn read_opponents() -> Vec<(i64, String)> {
    let mut out = Vec::new();
    // WorkDataManager.Instance → get_TeamStadiumData → get_OpponentDataList (List<OpponentData>)
    let wts = team_stadium_data();
    if wts.is_null() {
        return out;
    }
    let list = call_getter_obj(wts, "Gallop.WorkTeamStadiumData", "get_OpponentDataList");
    if list.is_null() {
        return out;
    }
    // List<T>: _items @0x10 (T[]), _size @0x18 ; array data @0x20, 8-byte ref elems
    let items = rd_ptr(list, 0x10);
    let size = rd_i32(list, 0x18);
    if items.is_null() || size <= 0 {
        return out;
    }
    // GetTrainerName lives on the NESTED class OpponentData — resolve it from a live instance
    // (class_from_name can't find nested types by namespace). Cached after the first opponent.
    let mut gtn: Option<il2cpp::Method> = None;
    for i in 0..size as usize {
        let opp = rd_ptr(items, 0x20 + i * 8);
        if opp.is_null() {
            continue;
        }
        // OpponentData.ServerData @0x78 (TeamStadiumOpponent) → opponent_viewer_id @0x18 (plain i64)
        let server = rd_ptr(opp, 0x78);
        if server.is_null() {
            continue;
        }
        let vid = *((server as usize + 0x18) as *const i64);
        if gtn.is_none() {
            let od_class = il2cpp::object_class(opp);
            if !od_class.is_null() {
                let m = il2cpp::method(od_class, "GetTrainerName", 1);
                if !m.is_null() {
                    gtn = Some(m);
                }
            }
        }
        let mut name = String::new();
        if let Some(m) = gtn {
            let p = il2cpp::method_pointer(m);
            if !p.is_null() {
                // GetTrainerName(this, long viewerId, MethodInfo) -> String
                let f: extern "C" fn(*mut c_void, i64, *const c_void) -> *mut c_void =
                    std::mem::transmute(p);
                let s = f(opp, vid, m as *const c_void);
                if !s.is_null() {
                    name = il2cpp::read_string(s);
                }
            }
        }
        out.push((vid, name));
    }
    out
}

/// Drive the game's Reload exactly like clicking the button: call the button's own handler
/// `<InitializeView>b__8_0`, which disables the button, BUILDS THE PROPER success callback (re-inits
/// the view → re-enables buttons + shows the new opponents), then calls SendApi. Calling SendApi(null)
/// directly leaves the button stuck grey because the view never re-initialises.
unsafe fn send_api(vc: *mut c_void) {
    if vc.is_null() {
        return;
    }
    let k = il2cpp::class("Gallop.TeamStadiumOpponentSelectViewController");
    if k.is_null() {
        return;
    }
    let m = il2cpp::method(k, "<InitializeView>b__8_0", 0);
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        log("send_api: reload handler b__8_0 not found");
        return;
    }
    let f: extern "C" fn(*mut c_void, *const c_void) = std::mem::transmute(p);
    f(vc, m as *const c_void);
}

/// Copy a &str into a fixed-size UTF-16 buffer, null-terminated and truncated to fit.
fn wide_into(buf: &mut [u16], s: &str) {
    let mut i = 0;
    for u in s.encode_utf16() {
        if i + 1 >= buf.len() {
            break;
        }
        buf[i] = u;
        i += 1;
    }
    buf[i] = 0;
}

fn alert(name: &str, vid: i64) {
    let body = if name.is_empty() {
        format!("viewer {vid} — pick them now")
    } else {
        format!("{name} ({vid}) — pick them now")
    };
    notify("Trackside — Target found!", &body);
}

/// AFK-proof desktop alert, shared by every hunter-style loop (opponent hunter, room finder):
/// (1) a NATIVE WINDOWS NOTIFICATION (Shell_NotifyIcon balloon → shows as a toast bottom-right +
/// in the Action Center, even with the game in the background, like a desktop-app notification).
/// (2) flash the game's taskbar button continuously until it's focused. The toast carries its own
/// notification sound.
pub(crate) fn notify(title: &str, body: &str) {
    let hwnd = crate::performance::display::game_hwnd();
    if hwnd == 0 {
        return;
    }
    unsafe {
        // (1) toast via a transient tray balloon
        use windows_sys::Win32::UI::Shell::{
            Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
            NOTIFYICONDATAW,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_INFORMATION};
        const UID: u32 = 0x4845; // 'HE'
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd as *mut c_void;
        nid.uID = UID;
        nid.uFlags = NIF_ICON | NIF_INFO | NIF_TIP;
        nid.hIcon = LoadIconW(std::ptr::null_mut(), IDI_INFORMATION);
        nid.dwInfoFlags = NIIF_INFO;
        wide_into(&mut nid.szTip, "Trackside");
        wide_into(&mut nid.szInfoTitle, title);
        wide_into(&mut nid.szInfo, body);
        Shell_NotifyIconW(NIM_ADD, &nid);
        Shell_NotifyIconW(NIM_MODIFY, &nid); // ensure the balloon shows even if the icon already existed

        // remove the tray icon a little after the toast (keeps the tray clean)
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let mut d: NOTIFYICONDATAW = std::mem::zeroed();
            d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            d.hWnd = hwnd as *mut c_void;
            d.uID = UID;
            Shell_NotifyIconW(NIM_DELETE, &d);
        });

        // (2) continuous taskbar flash until the window is focused
        use windows_sys::Win32::UI::WindowsAndMessaging::{FlashWindowEx, FLASHWINFO};
        const FLASHW_ALL: u32 = 0x0000_0003;
        const FLASHW_TIMERNOFG: u32 = 0x0000_000C;
        let mut fw = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd: hwnd as *mut c_void,
            dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
            uCount: 0,
            dwTimeout: 0,
        };
        FlashWindowEx(&mut fw);
    }
}

// TT il2cpp field readers + WorkDataManager accessors are shared with padder — see tt_il2cpp.rs.
use crate::tt_il2cpp::{call_obj_getter as call_getter_obj, rd_i32, rd_ptr, team_stadium_data};

// ── install ───────────────────────────────────────────────────────────────────

// PlayOut(ViewId, IViewInfo): the screen's exit transition → we've left, forget the controller.
type PlayOutFn = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, *const c_void);
unsafe extern "C" fn playout_hook(this: *mut c_void, view_id: i32, info: *mut c_void, mi: *const c_void) {
    VC.store(0, Ordering::Relaxed);
    HUNTING.store(false, Ordering::Relaxed); // leaving the screen ends any hunt
    let o = PLAYOUT_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: PlayOutFn = std::mem::transmute(o);
        f(this, view_id, info, mi);
    }
}

// NOTE: hunter's per-frame pumps (frame_pump + padder/reset/affinity + pruner/roomfinder) are driven
// from the SINGLE TweenManager.Update detour in `ui_tempo::update_hook`. hunter no longer installs its
// own detour on that method — two detours on one 5-byte prologue corrupted each other's trampolines
// (intermittent AV "after-tween"). `frame_pump()` is pub and called from ui_tempo each frame.

/// Detour OnOpponentInEnd (the read/check loop point) + PlayOut (leave). The per-frame roll is pumped
/// from ui_tempo's TweenManager.Update hook. Run on an IL2CPP-attached thread (boot).
pub fn install() -> String {
    if !il2cpp::ready() {
        let _ = il2cpp::init();
    }
    if !il2cpp::ready() {
        return "il2cpp not ready".into();
    }
    let mut notes = String::new();
    // 1) OnOpponentInEnd
    let k = il2cpp::class("Gallop.TeamStadiumOpponentSelectViewController");
    if k.is_null() {
        return "OpponentSelect VC not found".into();
    }
    unsafe {
        let m = il2cpp::method(k, "OnOpponentInEnd", 1);
        let p = il2cpp::method_pointer(m);
        if p.is_null() || crate::il2cpp::is_detoured(p) {
            notes.push_str("onend:skip ");
        } else if let Ok(d) = RawDetour::new(p as *const (), onend_hook as *const ()) {
            if d.enable().is_ok() {
                ONEND_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = DETOUR.set(d);
                notes.push_str("onend:ok ");
            } else {
                notes.push_str("onend:enable-fail ");
            }
        } else {
            notes.push_str("onend:new-fail ");
        }
        // 2) PlayOut — clears presence on leave.
        let m = il2cpp::method(k, "PlayOut", 2);
        let p = il2cpp::method_pointer(m);
        if !p.is_null() && !crate::il2cpp::is_detoured(p) {
            if let Ok(d) = RawDetour::new(p as *const (), playout_hook as *const ()) {
                if d.enable().is_ok() {
                    PLAYOUT_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = PLAYOUT_DETOUR.set(d);
                    notes.push_str("playout:ok ");
                } else {
                    notes.push_str("playout:enable-fail ");
                }
            }
        } else {
            notes.push_str("playout:skip ");
        }
        // 3) Per-frame roll pump: driven from ui_tempo's single TweenManager.Update detour (see NOTE
        //    above) — hunter installs NO detour here anymore, avoiding the stacked-trampoline crash.
        notes.push_str("tick:via-uitempo");
    }
    format!("opponent hunter: {}", notes.trim())
}

// ── UI panel (Team Trials → Opponent Hunter), rendered by the overlay ──────────
pub(crate) fn draw_panel(ui: &hudhook::imgui::Ui, w: f32) {
    use crate::overlay::{btn, btn_primary, help_icon, status_dot, ACCENT, DIM, GOOD, TEXT, WARN};
    use std::cell::{Cell, RefCell};
    thread_local! {
        static NAMEBUF: RefCell<String> = RefCell::new(String::new());
        static VIDBUF: RefCell<String> = RefCell::new(String::new());
        static ERR: RefCell<String> = RefCell::new(String::new());
        static LOADED: Cell<bool> = const { Cell::new(false) };
    }
    // Pre-fill the fields from the persisted target on the first draw (survives restarts).
    if !LOADED.with(|l| l.get()) {
        LOADED.with(|l| l.set(true));
        let (sn, sv) = crate::hunter::saved_target();
        NAMEBUF.with(|b| *b.borrow_mut() = sn);
        VIDBUF.with(|b| *b.borrow_mut() = sv);
    }
    ui.dummy([0.0, 4.0]);
    if crate::hunter::screen_open() {
        status_dot(ui, GOOD, "Select Opponent ready");
    } else {
        status_dot(ui, WARN, "Open Select Opponent");
    }
    ui.same_line();
    help_icon(ui, "Auto-refreshes the opponent list until your target shows up, then stops and alerts. Match by trainer name and/or exact viewer ID. The pool is random, so a target may take many rolls (or not appear).");
    ui.dummy([0.0, 8.0]);

    let hunting = crate::hunter::is_hunting();
    if !hunting {
        ui.text_colored(DIM, "Target — name and/or viewer ID:");
        let ch_n = NAMEBUF.with(|b| {
            let mut s = b.borrow_mut();
            ui.set_next_item_width(w * 0.9);
            ui.input_text("##huntname", &mut s).hint("trainer name").build()
        });
        ui.dummy([0.0, 3.0]);
        let ch_v = VIDBUF.with(|b| {
            let mut s = b.borrow_mut();
            ui.set_next_item_width(w * 0.9);
            ui.input_text("##huntvid", &mut s).hint("viewer ID (exact, optional)").build()
        });
        // Persist on any edit so the target survives a game restart.
        if ch_n || ch_v {
            let n = NAMEBUF.with(|b| b.borrow().clone());
            let v = VIDBUF.with(|b| b.borrow().clone());
            crate::hunter::save_target(&n, &v);
        }
        ui.dummy([0.0, 6.0]);
        if btn_primary(ui, "##huntstart", "Start hunt") {
            let name = NAMEBUF.with(|b| b.borrow().clone());
            let vid = VIDBUF.with(|b| b.borrow().clone());
            match crate::hunter::start(&name, &vid) {
                Ok(_) => ERR.with(|e| e.borrow_mut().clear()),
                Err(e) => ERR.with(|x| *x.borrow_mut() = e),
            }
        }
        ERR.with(|e| {
            let s = e.borrow();
            if !s.is_empty() {
                ui.dummy([0.0, 4.0]);
                ui.text_colored(WARN, &*s);
            }
        });
    } else {
        ui.text_colored(ACCENT, "Hunting…");
        ui.same_line();
        if btn(ui, "##huntstop", "Stop") {
            crate::hunter::stop();
        }
    }

    // status + last three
    let st = crate::hunter::status();
    if !st.is_empty() {
        ui.dummy([0.0, 6.0]);
        let col = if crate::hunter::found() { GOOD } else if st.starts_with("Not found") { WARN } else { TEXT };
        ui.text_colored(col, &st);
    }
    let last = crate::hunter::last_three();
    if !last.is_empty() {
        ui.dummy([0.0, 4.0]);
        for (vid, name) in last.iter() {
            ui.text_colored(DIM, &format!("\u{00b7} {}  ({})", if name.is_empty() { "?" } else { name.as_str() }, vid));
        }
    }
}
