//! Room Match watcher — the mirror image of the room finder.
//!
//! The finder signs you up for races; this runs the ones that are ready. From the Room Match top
//! screen it opens Sign-Ups, takes the first signed-up race whose start time has passed, walks it
//! through Race Details -> To Waiting Room -> Race! -> paddock -> race -> result -> back to top,
//! and repeats until nothing is ready or the player stops it.
//!
//! Every step is the game's own button or the handler behind it, resolved by name; nothing here
//! sends a request the client would not send itself. Screen changes are observed by detouring the
//! four screen controllers' `PlayInView`, because a `ViewController` is a plain C# object, not a
//! `UnityEngine.Object`, so `FindObjectsOfType` cannot see it - the screen probe never listed one.
//! Dialogs and views ARE MonoBehaviours and are found on demand, once, by type.
//!
//! Sources: live screen probe 2026-09-01 (four screens) + `heaven-roommatch-scan.txt` (2026-07-02).
//!
//!   Sign-Ups dialog  `DialogRoomMatchRegistSaveList.OnClickRegistRaceListItem(int roomId)`
//!   Race Details     `DialogRoomMatchRaceDetail.ChangeRoomMatchLobbyScene()`   ("To Waiting Room")
//!   Waiting room     `RoomMatchLobbyViewController.CanStartRace()` gate, then the view's RaceButton
//!   Paddock          `PaddockViewControllerBase.OnClickRaceStart()` / `OnClickRaceSkip()`
//!   Result           `DialogRoomMatchSaveRoomConfirm` (dismiss), `RoomMatchRaceResultViewController.OnClickOsBackKey()`
//!   Readiness        `WorkRoomMatchData.RoomData.get_StartUnixTime()` <= now  ("Ready to race!")
//!
//! Everything runs on the game main thread via `pump()` from the TweenManager tick. The overlay
//! only flips flags and reads status strings.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::RawDetour;

use crate::il2cpp;
use crate::tools::clock;

fn log(msg: &str) {
    crate::tools::log(&format!("[roomwatch] {msg}"));
}

fn now_ms() -> u64 {
    clock().elapsed().as_millis() as u64
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── which screen is up (from the controllers' PlayInView) ───────────────────────────────────────
const SCR_NONE: u8 = 0;
const SCR_TOP: u8 = 1;
const SCR_LOBBY: u8 = 2;
const SCR_PADDOCK: u8 = 3;
const SCR_RESULT: u8 = 4;
static SCREEN: AtomicU8 = AtomicU8::new(SCR_NONE);
static SCREEN_VC: AtomicUsize = AtomicUsize::new(0);
static SCREEN_AT: AtomicU64 = AtomicU64::new(0);

fn screen_name(s: u8) -> &'static str {
    match s {
        SCR_TOP => "top",
        SCR_LOBBY => "waiting room",
        SCR_PADDOCK => "paddock",
        SCR_RESULT => "result",
        _ => "none",
    }
}

fn enter_screen(which: u8, vc: *mut c_void) {
    if !vc.is_null() {
        SCREEN.store(which, Ordering::Relaxed);
        SCREEN_VC.store(vc as usize, Ordering::Relaxed);
        SCREEN_AT.store(now_ms(), Ordering::Relaxed);
        if is_running() {
            log(&format!("screen: {}", screen_name(which)));
        }
    }
}

fn leave_screen(which: u8) {
    if SCREEN.load(Ordering::Relaxed) == which {
        SCREEN.store(SCR_NONE, Ordering::Relaxed);
        SCREEN_VC.store(0, Ordering::Relaxed);
    }
}

type VcFn = unsafe extern "C" fn(*mut c_void, *const c_void);

macro_rules! screen_hook {
    ($fn_name:ident, $orig:ident, $det:ident, enter $which:expr) => {
        static $orig: AtomicUsize = AtomicUsize::new(0);
        static $det: OnceLock<RawDetour> = OnceLock::new();
        unsafe extern "C" fn $fn_name(this: *mut c_void, mi: *const c_void) {
            enter_screen($which, this);
            let o = $orig.load(Ordering::Relaxed);
            if o != 0 {
                let f: VcFn = std::mem::transmute(o);
                f(this, mi);
            }
        }
    };
    ($fn_name:ident, $orig:ident, $det:ident, leave $which:expr) => {
        static $orig: AtomicUsize = AtomicUsize::new(0);
        static $det: OnceLock<RawDetour> = OnceLock::new();
        unsafe extern "C" fn $fn_name(this: *mut c_void, mi: *const c_void) {
            leave_screen($which);
            let o = $orig.load(Ordering::Relaxed);
            if o != 0 {
                let f: VcFn = std::mem::transmute(o);
                f(this, mi);
            }
        }
    };
}

screen_hook!(top_in, TOP_IN_ORIG, TOP_IN_DET, enter SCR_TOP);
screen_hook!(top_out, TOP_OUT_ORIG, TOP_OUT_DET, leave SCR_TOP);
screen_hook!(lobby_in, LOBBY_IN_ORIG, LOBBY_IN_DET, enter SCR_LOBBY);
screen_hook!(lobby_out, LOBBY_OUT_ORIG, LOBBY_OUT_DET, leave SCR_LOBBY);
screen_hook!(paddock_in, PADDOCK_IN_ORIG, PADDOCK_IN_DET, enter SCR_PADDOCK);
screen_hook!(result_in, RESULT_IN_ORIG, RESULT_IN_DET, enter SCR_RESULT);

/// The paddock controller the GAME is driving when it enters the paddock step proper, captured
/// from its own `PaddockViewControllerBase.StartPaddock()` call. The pointer taken at `PlayInView`
/// is dead once the runner overview advances (its class name read back empty three seconds later),
/// so the only trustworthy handle is the one the game passes here, at the moment it is live.
static PADDOCK_STEP_VC: AtomicUsize = AtomicUsize::new(0);
static PADDOCK_STEP_AT: AtomicU64 = AtomicU64::new(0);
static START_PADDOCK_ORIG: AtomicUsize = AtomicUsize::new(0);
static START_PADDOCK_DET: OnceLock<RawDetour> = OnceLock::new();
unsafe extern "C" fn start_paddock_hook(this: *mut c_void, mi: *const c_void) {
    if !this.is_null() {
        PADDOCK_STEP_VC.store(this as usize, Ordering::Relaxed);
        PADDOCK_STEP_AT.store(now_ms(), Ordering::Relaxed);
        if is_running() {
            let same = this as usize == SCREEN_VC.load(Ordering::Relaxed);
            log(&format!("paddock step: StartPaddock on {this:p} (same object as PlayInView: {same})"));
        }
    }
    let o = START_PADDOCK_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: VcFn = std::mem::transmute(o);
        f(this, mi);
    }
}

const TOP_VC: &str = "Gallop.RoomMatchTopViewController";
const LOBBY_VC: &str = "Gallop.RoomMatchLobbyViewController";
const PADDOCK_VC: &str = "Gallop.RoomMatchPaddockViewController";
const RESULT_VC: &str = "Gallop.RoomMatchRaceResultViewController";

unsafe fn hook(k: il2cpp::Class, method: &str, target: *const (), orig: &AtomicUsize, det: &OnceLock<RawDetour>) -> &'static str {
    let m = il2cpp::method(k, method, 0);
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return "miss";
    }
    if il2cpp::is_detoured(p) {
        return "skip";
    }
    match RawDetour::new(p as *const (), target) {
        Ok(d) => {
            if d.enable().is_ok() {
                orig.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = det.set(d);
                "ok"
            } else {
                "enable-fail"
            }
        }
        Err(_) => "new-fail",
    }
}

/// Boot-time install. Only the screen detours; everything else resolves on demand.
pub fn install() -> String {
    if !il2cpp::ready() {
        return "room watcher: runtime not ready".into();
    }
    let mut notes: Vec<String> = Vec::new();
    unsafe {
        let pairs: [(&str, &str, *const (), &AtomicUsize, &OnceLock<RawDetour>, &str); 6] = [
            (TOP_VC, "PlayInView", top_in as *const (), &TOP_IN_ORIG, &TOP_IN_DET, "top-in"),
            (TOP_VC, "PlayOutView", top_out as *const (), &TOP_OUT_ORIG, &TOP_OUT_DET, "top-out"),
            (LOBBY_VC, "PlayInView", lobby_in as *const (), &LOBBY_IN_ORIG, &LOBBY_IN_DET, "lobby-in"),
            (LOBBY_VC, "PlayOutView", lobby_out as *const (), &LOBBY_OUT_ORIG, &LOBBY_OUT_DET, "lobby-out"),
            (PADDOCK_VC, "PlayInView", paddock_in as *const (), &PADDOCK_IN_ORIG, &PADDOCK_IN_DET, "paddock-in"),
            (RESULT_VC, "PlayInView", result_in as *const (), &RESULT_IN_ORIG, &RESULT_IN_DET, "result-in"),
        ];
        for (cls, m, target, orig, det, label) in pairs {
            let k = il2cpp::class(cls);
            let r = if k.is_null() { "class-miss" } else { hook(k, m, target, orig, det) };
            notes.push(format!("{label}:{r}"));
        }
        let k = il2cpp::class("Gallop.PaddockViewControllerBase");
        let r = if k.is_null() {
            "class-miss"
        } else {
            hook(k, "StartPaddock", start_paddock_hook as *const (), &START_PADDOCK_ORIG, &START_PADDOCK_DET)
        };
        notes.push(format!("paddock-step:{r}"));
    }
    format!("room watcher: {}", notes.join(" "))
}

// ── state machine ───────────────────────────────────────────────────────────────────────────────
const S_IDLE: u8 = 0;
const S_OPEN_LIST: u8 = 1;
const S_PICK: u8 = 2;
const S_DETAIL: u8 = 3;
const S_LOBBY: u8 = 4;
const S_PADDOCK: u8 = 5;
const S_RACE: u8 = 6;
const S_RESULT_SAVE: u8 = 7;
const S_RESULT_BACK: u8 = 8;
const S_RESULT_CONFIRM: u8 = 9;
const S_RETURN: u8 = 10;
/// Replaying a recorded click sequence from the waiting room until the top screen returns.
const S_REPLAY: u8 = 11;

static STATE: AtomicU8 = AtomicU8::new(S_IDLE);
static NEXT: AtomicU64 = AtomicU64::new(0);
static DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);
static TRIES: AtomicI32 = AtomicI32::new(0);
static CURRENT_ROOM: AtomicI64 = AtomicI64::new(0);
static DONE_COUNT: AtomicI32 = AtomicI32::new(0);
static SKIP_RACE: AtomicBool = AtomicBool::new(false);
static REQ_START: AtomicBool = AtomicBool::new(false);
static REQ_STOP: AtomicBool = AtomicBool::new(false);
/// When Race! was pressed on the paddock, and how many follow-up confirmations were accepted.
static RACE_PRESSED_AT: AtomicU64 = AtomicU64::new(0);
static CONFIRMS: AtomicI32 = AtomicI32::new(0);
/// The end-of-race save prompt has been seen this race (so its absence means "dismissed").
static SAVE_PROMPT_SEEN: AtomicBool = AtomicBool::new(false);
/// Runner-overview step advance: which controller candidate was tried, and when the next may be.
static STEP_TRIES: AtomicI32 = AtomicI32::new(0);
static STEP_NEXT: AtomicU64 = AtomicU64::new(0);
/// First frame the paddock's Race! button existed (settle before pressing), and press count.
static HOLDER_SEEN_AT: AtomicU64 = AtomicU64::new(0);
static RACE_PRESSES: AtomicI32 = AtomicI32::new(0);
static DIAG_NEXT: AtomicU64 = AtomicU64::new(0);
/// Live ButtonCommons named "Button" seen by the tick while on the paddock: (ptr, last-seen ms).
static GENERIC_BUTTONS: Mutex<Option<Vec<(usize, u64)>>> = Mutex::new(None);
static CARDS_PRESSED_AT: AtomicU64 = AtomicU64::new(0);
static CARDS_PRESSES: AtomicI32 = AtomicI32::new(0);
static DONE: Mutex<Option<HashSet<i64>>> = Mutex::new(None);
static STATUS: Mutex<String> = Mutex::new(String::new());

const RETRY_MS: u64 = 500;
const RACE_TIMEOUT_MS: u64 = 12 * 60 * 1000;

fn set_status(s: String) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s;
    }
}
fn say(s: String) {
    log(&s);
    set_status(s);
}
fn goto(state: u8, settle_ms: u64, timeout_ms: u64) {
    STATE.store(state, Ordering::Relaxed);
    NEXT.store(now_ms() + settle_ms, Ordering::Relaxed);
    DEADLINE.store(now_ms() + timeout_ms, Ordering::Relaxed);
    TRIES.store(0, Ordering::Relaxed);
}
fn expired() -> bool {
    now_ms() > DEADLINE.load(Ordering::Relaxed)
}
fn retry() {
    NEXT.store(now_ms() + RETRY_MS, Ordering::Relaxed);
    TRIES.fetch_add(1, Ordering::Relaxed);
}
fn fail(msg: &str) {
    say(format!("Stopped: {msg}"));
    STATE.store(S_IDLE, Ordering::Relaxed);
}
fn mark_done(room: i64) {
    if let Ok(mut g) = DONE.lock() {
        g.get_or_insert_with(HashSet::new).insert(room);
    }
}
fn is_done(room: i64) -> bool {
    DONE.lock().ok().and_then(|g| g.as_ref().map(|s| s.contains(&room))).unwrap_or(false)
}

// ── overlay API (render-thread safe: flags + string reads only) ─────────────────────────────────
pub fn is_running() -> bool {
    STATE.load(Ordering::Relaxed) != S_IDLE
}
pub fn done_count() -> i32 {
    DONE_COUNT.load(Ordering::Relaxed)
}
pub fn status() -> String {
    STATUS.lock().map(|g| g.clone()).unwrap_or_default()
}
pub fn skip_race() -> bool {
    SKIP_RACE.load(Ordering::Relaxed)
}
pub fn set_skip_race(on: bool) {
    SKIP_RACE.store(on, Ordering::Relaxed);
}
pub fn on_top_screen() -> bool {
    SCREEN.load(Ordering::Relaxed) == SCR_TOP
}
pub fn request_start() {
    REQ_START.store(true, Ordering::Relaxed);
}
pub fn request_stop() {
    REQ_STOP.store(true, Ordering::Relaxed);
}

// ── record & replay ─────────────────────────────────────────────────────────────────────────────
//
// Every guess about "which handler does this button call" is replaced by the game's own answer:
// `ButtonCommon.OnPointerClick` fires for every button the player presses, and the button's
// object PATH (transform ancestry) identifies it even when its name is just "Button". Recording
// writes one line per press; replay presses the same paths in the same order, each one when it is
// live on screen and has been for a moment. No handler names, no step theories.
//
// File: <game>/trackside-logs/click-recording.txt, lines "<ms>\t<screen>\t<path>".

/// The Room Match race flow from the waiting room to the top screen, as RECORDED from a real
/// play-through on 2026-09-02 (see docs-internal/game-methods.md for the full table). Each entry is
/// a button's object-path suffix (everything after the canvas root; matched with `ends_with`, so
/// the two dialog canvases and the `Gallop.GameSystem/…` prefix do not matter) and whether the
/// step is optional. Optional steps are pressed if they show up but never waited for: the replay
/// moves past them the moment a later step's button is live.
///
/// Why paths and not handlers: button names collide ("Button"), controller pointers die between
/// steps, and the same handler does different things per paddock step. The path is the one stable
/// identity, and this sequence is the player's own, not a theory.
const FLOW: &[(&str, bool)] = &[
    // waiting room: Race!
    ("RoomMatchHubView(Clone)/RoomMatchLobbyView(Clone)/ContentsRoot/LabelAndButtons/TweenRoot/RaceButton", false),
    // runner overview: Next
    ("RoomMatchPaddockView(Clone)/ContentsRoot/MotivationDisp(Clone)/RaceEntryTablePanelMotivation(Clone)/BottomRoot/Button", false),
    // 3D paddock: Race!
    ("RoomMatchPaddockView(Clone)/ContentsRoot/PaddockContentsRoot/ContentsRoot/RaceStartButtonRoot/RaceStartButtonCenter", false),
    // race scene, cards screen: Race!
    ("RaceMainView(Clone)/ContentsRoot/RaceEntryTablePanelLandscape(Clone)/BottomRoot/Button", false),
    // in-race skip (only when "Skip race playback" is on). Optional steps are pressed while the
    // button stays live - every 3 s, after a 2.5 s settle - because a press that lands while the
    // race is still loading is accepted and ignored (measured: two presses at +0.7 s and +1.4 s
    // after the cards Race! did nothing). The step ends when a later step's button appears.
    ("RaceMainView(Clone)/RaceUILandscape(Clone)/ContentsRoot/Bottom/RightBottomButtonRoot/Btn_skip", true),
    // race result panel: Next
    ("RaceMainView(Clone)/RaceResultList(Clone)/ContentsRoot/CustomButtonRoot/ButtonM00", false),
    // Room Match result: follow panel Next
    ("RoomMatchRaceResultView(Clone)/ContentsRoot/RaceResultFriendFollowPanel(Clone)/BottomRoot/Button", false),
    // save prompt (single-button dialog): dismiss - the game then returns to the top on its own
    ("NoImageEffectGameCanvas/DialogCanvas/DialogCommon(Clone)/Mask/CircleMask/ContentsRoot/Small/Base/Footer/Buttons/ButtonCenter", false),
];

/// Dev-build override: a fresh recording on disk replaces FLOW, so the sequence can be re-captured
/// after a game update without a code change first.
const RECORDING_FILE: &str = "click-recording.txt";
static RECORDING: AtomicBool = AtomicBool::new(false);
static REC_START: AtomicU64 = AtomicU64::new(0);
static REC_COUNT: AtomicI32 = AtomicI32::new(0);
static REPLAY_STEPS: Mutex<Option<Vec<(String, bool)>>> = Mutex::new(None);
static REPLAY_CURSOR: AtomicUsize = AtomicUsize::new(0);
static REPLAY_SEEN_AT: AtomicU64 = AtomicU64::new(0);
static REPLAY_LAST_PRESS: AtomicU64 = AtomicU64::new(0);
static REPLAY_PATH_CACHE: Mutex<Option<std::collections::HashMap<usize, String>>> = Mutex::new(None);
/// Presses made on the current optional step (bounded; the step is never waited for).
static REPLAY_OPT_PRESSES: AtomicI32 = AtomicI32::new(0);

pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}
pub fn recorded_count() -> i32 {
    REC_COUNT.load(Ordering::Relaxed)
}
/// Number of steps in the recording on disk (0 = none).
pub fn recording_len() -> usize {
    load_recording().len()
}

/// (screen, path) per recorded press.
fn load_recording_full() -> Vec<(String, String)> {
    let path = crate::paths::log_file(RECORDING_FILE);
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let mut it = l.split('\t');
            let _ms = it.next()?;
            let screen = it.next()?.trim().to_string();
            let path = it.next()?.trim().to_string();
            if path.is_empty() { None } else { Some((screen, path)) }
        })
        .collect()
}

fn load_recording() -> Vec<String> {
    load_recording_full().into_iter().map(|(_, p)| p).collect()
}

/// Overlay toggle. Starting truncates the file; the first press is t=0.
pub fn set_recording(on: bool) {
    if on {
        let path = crate::paths::log_file(RECORDING_FILE);
        let _ = std::fs::write(&path, "# Trackside click recording: <ms>\t<screen>\t<button path>\n");
        REC_START.store(now_ms(), Ordering::Relaxed);
        REC_COUNT.store(0, Ordering::Relaxed);
        RECORDING.store(true, Ordering::Relaxed);
        log("recording clicks - press through the whole flow, then stop recording");
    } else if RECORDING.swap(false, Ordering::Relaxed) {
        log(&format!("recording stopped - {} clicks saved", REC_COUNT.load(Ordering::Relaxed)));
    }
}

/// Called from the ButtonCommon.OnPointerClick hook (game main thread) for every press.
pub fn record_click(this: *mut c_void) {
    if !RECORDING.load(Ordering::Relaxed) || this.is_null() {
        return;
    }
    let path = unsafe { bridge::object_path(this) };
    let ms = now_ms().saturating_sub(REC_START.load(Ordering::Relaxed));
    let screen = screen_name(SCREEN.load(Ordering::Relaxed));
    let line = format!("{ms}\t{screen}\t{path}\n");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(crate::paths::log_file(RECORDING_FILE)) {
        let _ = f.write_all(line.as_bytes());
    }
    let n = REC_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    log(&format!("[rec #{n}] +{ms}ms {screen}: {path}"));
}

fn replay_begin() -> usize {
    let skip = SKIP_RACE.load(Ordering::Relaxed);
    let mut steps: Vec<(String, bool)> =
        FLOW.iter().filter(|(_, opt)| skip || !*opt).map(|(p, opt)| (p.to_string(), *opt)).collect();
    let mut start = 0usize;
    let mut source = "built-in flow";
    #[cfg(feature = "devtools")]
    {
        let full = load_recording_full();
        if !full.is_empty() {
            // The watcher hands over IN the waiting room; start at the first press recorded there.
            start = full.iter().position(|(scr, _)| scr == "waiting room").unwrap_or(0);
            steps = full.into_iter().map(|(_, p)| (p, false)).collect();
            source = "recording on disk (dev override)";
        }
    }
    let n = steps.len();
    log(&format!("replay: {n} presses from the {source}{}", if skip { ", race skip on" } else { "" }));
    if let Ok(mut g) = REPLAY_STEPS.lock() {
        *g = Some(steps);
    }
    REPLAY_CURSOR.store(start, Ordering::Relaxed);
    REPLAY_OPT_PRESSES.store(0, Ordering::Relaxed);
    REPLAY_SEEN_AT.store(0, Ordering::Relaxed);
    REPLAY_LAST_PRESS.store(0, Ordering::Relaxed);
    if let Ok(mut c) = REPLAY_PATH_CACHE.lock() {
        *c = Some(std::collections::HashMap::new());
    }
    n
}

fn replay_expected() -> Option<(usize, usize, String)> {
    let cur = REPLAY_CURSOR.load(Ordering::Relaxed);
    let g = REPLAY_STEPS.lock().ok()?;
    let steps = g.as_ref()?;
    steps.get(cur).map(|(p, _)| (cur, steps.len(), p.clone()))
}

/// Steps that may legitimately be pressed next: the current one, and - while the current one is
/// optional - the ones after it up to and including the first required step.
fn replay_candidates() -> Vec<(usize, String)> {
    let cur = REPLAY_CURSOR.load(Ordering::Relaxed);
    let Ok(g) = REPLAY_STEPS.lock() else { return Vec::new() };
    let Some(steps) = g.as_ref() else { return Vec::new() };
    let mut out = Vec::new();
    for (i, (p, opt)) in steps.iter().enumerate().skip(cur) {
        out.push((i, p.clone()));
        if !*opt {
            break;
        }
    }
    out
}

/// Button tick during replay: press the expected path once it has been live for a moment.
fn replay_on_button(this: *mut c_void) {
    let cands = replay_candidates();
    if cands.is_empty() {
        return;
    }
    let total = REPLAY_STEPS.lock().ok().and_then(|g| g.as_ref().map(|v| v.len())).unwrap_or(0);
    let name = crate::ui_input::button_name(this);
    let Some((cur, expected)) = cands
        .iter()
        .find(|(_, p)| p.rsplit('/').next().map(|l| l == name).unwrap_or(false))
        .cloned()
    else {
        return;
    };
    let leaf = expected.rsplit('/').next().unwrap_or(&expected);
    let key = this as usize;
    let path = {
        let cached = REPLAY_PATH_CACHE.lock().ok().and_then(|c| c.as_ref().and_then(|m| m.get(&key).cloned()));
        match cached {
            Some(p) => p,
            None => {
                let p = unsafe { bridge::object_path(this) };
                if let Ok(mut c) = REPLAY_PATH_CACHE.lock() {
                    if let Some(m) = c.as_mut() {
                        if m.len() < 256 {
                            m.insert(key, p.clone());
                        }
                    }
                }
                p
            }
        }
    };
    if !path.ends_with(expected.as_str()) {
        return;
    }
    let optional = REPLAY_STEPS
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|v| v.get(cur).map(|(_, o)| *o)))
        .unwrap_or(false);
    let now = now_ms();
    let seen = REPLAY_SEEN_AT.load(Ordering::Relaxed);
    if seen == 0 {
        REPLAY_SEEN_AT.store(now, Ordering::Relaxed);
        set_status(format!("Replay {}/{total}: {leaf} is up", cur + 1));
        return;
    }
    // Settle: a press on a button that has only just appeared is accepted and ignored. Required
    // steps: 700 ms live, 700 ms since the last press. Optional (in-race) steps: 2.5 s live, then
    // one press every 3 s while the button stays live, at most 8.
    let (settle, gap) = if optional { (2500, 3000) } else { (700, 700) };
    if now < seen + settle || now < REPLAY_LAST_PRESS.load(Ordering::Relaxed) + gap {
        return;
    }
    if optional && REPLAY_OPT_PRESSES.load(Ordering::Relaxed) >= 8 {
        return;
    }
    if unsafe { crate::ui_input::click_now(this) } {
        REPLAY_LAST_PRESS.store(now, Ordering::Relaxed);
        if optional {
            let k = REPLAY_OPT_PRESSES.fetch_add(1, Ordering::Relaxed) + 1;
            log(&format!("replay {}/{total}: pressed {leaf} (optional, press {k})", cur + 1));
            set_status(format!("Replay {}/{total}: {leaf} ({k})", cur + 1));
            return; // stay on this step; a later step's button ends it
        }
        REPLAY_SEEN_AT.store(0, Ordering::Relaxed);
        REPLAY_OPT_PRESSES.store(0, Ordering::Relaxed);
        REPLAY_CURSOR.store(cur + 1, Ordering::Relaxed);
        if let Ok(mut c) = REPLAY_PATH_CACHE.lock() {
            if let Some(m) = c.as_mut() {
                m.clear();
            }
        }
        log(&format!("replay {}/{total}: pressed {expected}", cur + 1));
        set_status(format!("Replay {}/{total}: pressed {leaf}", cur + 1));
    }
}

/// Names of the race-scene result panel's advance button. Logged-and-learned: any other name seen
/// while waiting is written once so the list can be corrected from one run.
const RESULT_NEXT_NAMES: &[&str] = &["NextButton", "ButtonNext", "Next", "RaceResultNextButton"];
static SEEN_NAMES: Mutex<Option<HashSet<String>>> = Mutex::new(None);
static LAST_NEXT_PRESS: AtomicU64 = AtomicU64::new(0);

/// Called from ButtonCommon.Update (every live button, every frame). While the watcher is waiting
/// for the race to finish, press the race-scene result panel's Next so the flow reaches the Room
/// Match result screen. Nothing else is ever pressed from here.
pub fn on_button_update(this: *mut c_void) {
    let st = STATE.load(Ordering::Relaxed);
    if this.is_null() {
        return;
    }
    if st == S_REPLAY {
        replay_on_button(this);
        return;
    }
    if st == S_PADDOCK {
        // The cards screen ("Race!" under vertical runner cards) is invisible to every other
        // instrument: no PaddockContentsHolder, no StartPaddock, FindObjectsOfType empty. Its one
        // button is named just "Button". Record each live one with a timestamp; the pump presses
        // only when there is exactly ONE candidate on screen.
        if crate::ui_input::button_name(this) == "Button" {
            if let Ok(mut g) = GENERIC_BUTTONS.lock() {
                let v = g.get_or_insert_with(Vec::new);
                let now = now_ms();
                if let Some(e) = v.iter_mut().find(|(p, _)| *p == this as usize) {
                    e.1 = now;
                } else if v.len() < 16 {
                    v.push((this as usize, now));
                }
            }
        }
        return;
    }
    if st != S_RACE && st != S_RESULT_SAVE {
        return;
    }
    // In the race wait, not before the paddock confirmations are done with - the race has to be
    // running first. On the result screen there is a second Next (the "Select a Room Match to
    // Follow" panel) BEFORE the save prompt; same press, no gate.
    if st == S_RACE && now_ms() < RACE_PRESSED_AT.load(Ordering::Relaxed) + 25_000 {
        return;
    }
    let name = crate::ui_input::button_name(this);
    if name.is_empty() {
        return;
    }
    if RESULT_NEXT_NAMES.iter().any(|n| *n == name) {
        if now_ms() < LAST_NEXT_PRESS.load(Ordering::Relaxed) + 1500 {
            return;
        }
        if unsafe { crate::ui_input::click_now(this) } {
            LAST_NEXT_PRESS.store(now_ms(), Ordering::Relaxed);
            log(&format!("race result panel: pressed \"{name}\""));
            set_status("Race finished - leaving the result panel…".into());
        }
        return;
    }
    // Learn: record each distinct button name once so a differently named Next shows up in the log.
    if let Ok(mut g) = SEEN_NAMES.lock() {
        let set = g.get_or_insert_with(HashSet::new);
        if set.len() < 64 && set.insert(name.clone()) {
            log(&format!("race result panel: button seen \"{name}\""));
        }
    }
}

/// Main-thread tick (TweenManager.Update). No-op unless running or requested.
pub fn pump() {
    if REQ_STOP.swap(false, Ordering::Relaxed) && is_running() {
        fail("stopped by you");
        return;
    }
    if REQ_START.swap(false, Ordering::Relaxed) {
        if !il2cpp::ready() {
            say("Game not ready yet.".into());
            return;
        }
        if let Ok(mut g) = DONE.lock() {
            *g = Some(HashSet::new());
        }
        if let Ok(mut g) = SEEN_NAMES.lock() {
            *g = Some(HashSet::new());
        }
        DONE_COUNT.store(0, Ordering::Relaxed);
        say("Opening Sign-Ups…".into());
        goto(S_OPEN_LIST, 0, 15_000);
    }
    let st = STATE.load(Ordering::Relaxed);
    if st == S_IDLE || now_ms() < NEXT.load(Ordering::Relaxed) {
        return;
    }
    crate::crashlog::step("roomwatch:pump");
    unsafe { step(st) };
}

unsafe fn step(st: u8) {
    match st {
        S_OPEN_LIST => {
            // Already open (the player may have opened it themselves) -> go read it.
            if !bridge::first_instance("Gallop.DialogRoomMatchRegistSaveList").is_null() {
                goto(S_PICK, 700, 10_000);
                return;
            }
            if expired() {
                fail("couldn't open Sign-Ups - open Room Match first, then Start");
                return;
            }
            // Press the top screen's own Sign-Ups button, no more than once every 3 s.
            if TRIES.load(Ordering::Relaxed) % 6 == 0 {
                match bridge::press_top_button("_registSaveButton") {
                    Ok(()) => log("pressed Sign-Ups"),
                    Err(e) => set_status(format!("Waiting for the Room Match top screen ({e})")),
                }
            }
            retry();
        }
        S_PICK => {
            let dlg = bridge::first_instance("Gallop.DialogRoomMatchRegistSaveList");
            if dlg.is_null() {
                if expired() {
                    fail("Sign-Ups dialog went away");
                } else {
                    retry();
                }
                return;
            }
            let entries = bridge::my_entries();
            if entries.is_empty() && !expired() {
                // The list is fetched when the dialog opens; give it a moment.
                set_status("Reading your sign-ups…".into());
                retry();
                return;
            }
            let now = now_unix();
            let mut ready: Option<i64> = None;
            for e in &entries {
                let left = e.start_unix - now;
                let state = if is_done(e.room_id) {
                    "done this session"
                } else if e.start_unix > 0 && left <= 0 {
                    "READY"
                } else {
                    "not yet"
                };
                log(&format!(
                    "entry room {} host \"{}\": starts in {left}s, canWatch={} simDone={} allowDisplay={} -> {state}",
                    e.room_id, e.host, e.can_watch, e.sim_done, e.allow_display
                ));
                if ready.is_none() && state == "READY" {
                    ready = Some(e.room_id);
                }
            }
            let Some(room) = ready else {
                let n = DONE_COUNT.load(Ordering::Relaxed);
                say(format!("Nothing ready to race ({n} watched this run)."));
                STATE.store(S_IDLE, Ordering::Relaxed);
                return;
            };
            CURRENT_ROOM.store(room, Ordering::Relaxed);
            match bridge::click_regist_item(dlg, room as i32) {
                Ok(()) => {
                    say(format!("Room {room}: opening Race Details…"));
                    goto(S_DETAIL, 600, 10_000);
                }
                Err(e) => fail(&format!("couldn't open the race ({e})")),
            }
        }
        S_DETAIL => {
            let d = bridge::first_instance("Gallop.DialogRoomMatchRaceDetail");
            if d.is_null() {
                if expired() {
                    fail("Race Details never opened");
                } else {
                    retry();
                }
                return;
            }
            match bridge::invoke0_checked(d, "ChangeRoomMatchLobbyScene") {
                Ok(()) => {
                    say("To Waiting Room…".into());
                    goto(S_LOBBY, 1500, 45_000);
                }
                Err(e) => fail(&format!("To Waiting Room failed ({e})")),
            }
        }
        S_LOBBY => {
            if SCREEN.load(Ordering::Relaxed) != SCR_LOBBY {
                if expired() {
                    fail("waiting room never appeared");
                } else {
                    retry();
                }
                return;
            }
            let vc = SCREEN_VC.load(Ordering::Relaxed) as *mut c_void;
            let can = bridge::invoke_bool(vc, "CanStartRace").unwrap_or(false);
            if !can {
                if expired() {
                    fail("race never became startable (still counting down?)");
                } else {
                    set_status("In the waiting room - waiting for Ready to race!…".into());
                    retry();
                }
                return;
            }
            // A recording, when one exists, always wins over the hand-built path below.
            {
                let n = replay_begin();
                if n > 0 {
                    say(format!("Replaying your recording ({n} clicks) from the waiting room…"));
                    goto(S_REPLAY, 0, RACE_TIMEOUT_MS);
                    return;
                }
            }
            match bridge::click_lobby_race_button() {
                Ok(()) => {
                    say("Race! pressed - loading the paddock…".into());
                    STEP_TRIES.store(0, Ordering::Relaxed);
                    STEP_NEXT.store(0, Ordering::Relaxed);
                    HOLDER_SEEN_AT.store(0, Ordering::Relaxed);
                    RACE_PRESSES.store(0, Ordering::Relaxed);
                    PADDOCK_STEP_VC.store(0, Ordering::Relaxed);
                    CARDS_PRESSES.store(0, Ordering::Relaxed);
                    CARDS_PRESSED_AT.store(0, Ordering::Relaxed);
                    if let Ok(mut g) = GENERIC_BUTTONS.lock() {
                        *g = Some(Vec::new());
                    }
                    goto(S_PADDOCK, 2500, 60_000);
                }
                Err(e) => {
                    if expired() {
                        fail(&format!("Race! button ({e})"));
                    } else {
                        retry();
                    }
                }
            }
        }
        S_PADDOCK => {
            if SCREEN.load(Ordering::Relaxed) != SCR_PADDOCK {
                if expired() {
                    fail("paddock never appeared");
                } else {
                    retry();
                }
                return;
            }
            // PlayInView fires before the paddock is interactive; let it settle, then press.
            if now_ms() < SCREEN_AT.load(Ordering::Relaxed) + 2500 {
                retry();
                return;
            }
            // Press the paddock's actual Race! button. Invoking the controller's
            // `OnClickRaceStart()` returned cleanly and started nothing - the button's own click
            // path is what the game wires, so use that (`PaddockContentsHolder`, centre slot for
            // the single-button layout, right slot otherwise). Skip mode has no separate button
            // on this layout and keeps the controller handler.
            // The controller's first step is a runner overview; the paddock (and its Race!
            // button) only exists past it. Its buttons are named just "Button", so they cannot be
            // pressed by name - advance the step through the controller instead.
            //
            // MEASURED across four runs: `OnClickRaceStart()` on the overview step is the Next -
            // it is the same handler the paddock's Race! button uses, and the step value decides
            // what it does. The run that reached the paddock this way then started the race from
            // a Race! press (race header logged). `StartPaddock()` also reaches a paddock, but one
            // that refuses Race! forever (no race header in three runs) - it skips the step
            // sequence the rival-skip module already found must not be forced. `OnTapStart` is
            // inert on the overview.
            let step_vc = PADDOCK_STEP_VC.load(Ordering::Relaxed) as *mut c_void;
            let in_step = !step_vc.is_null()
                && PADDOCK_STEP_AT.load(Ordering::Relaxed) >= SCREEN_AT.load(Ordering::Relaxed);
            if in_step {
                // The game entered the paddock step and handed us its controller. Let the paddock
                // settle, then fire the SAME handler its Race! button fires, on that pointer.
                if now_ms() < PADDOCK_STEP_AT.load(Ordering::Relaxed) + 3000 {
                    set_status("Paddock - settling…".into());
                    retry();
                    return;
                }
                let n = RACE_PRESSES.load(Ordering::Relaxed);
                if n >= 3 {
                    fail("Race! handler fired 3 times with no race - stopping");
                    return;
                }
                let step = bridge::field_i32(step_vc, "_paddockStepValue");
                let started = bridge::field_u8(step_vc, "_isRaceStart");
                let method = if SKIP_RACE.load(Ordering::Relaxed) { "OnClickRaceSkip" } else { "OnClickRaceStart" };
                match bridge::invoke0_checked(step_vc, method) {
                    Ok(()) => {
                        log(&format!(
                            "paddock: {method} on the step controller (step={step:?} isRaceStart={started:?}, attempt {})",
                            n + 1
                        ));
                        say("Race! - confirming…".into());
                        RACE_PRESSED_AT.store(now_ms(), Ordering::Relaxed);
                        CONFIRMS.store(0, Ordering::Relaxed);
                        RACE_PRESSES.store(n + 1, Ordering::Relaxed);
                        DIAG_NEXT.store(0, Ordering::Relaxed);
                        goto(S_RACE, 800, RACE_TIMEOUT_MS);
                    }
                    Err(e) => fail(&format!("{method} on the paddock ({e})")),
                }
                return;
            }
            if bridge::first_instance("Gallop.PaddockContentsHolder").is_null() {
                let n = STEP_TRIES.load(Ordering::Relaxed);
                // Cards screen: press its one generic button - only when exactly one is live.
                if n >= 1
                    && now_ms() >= STEP_NEXT.load(Ordering::Relaxed)
                    && now_ms() >= CARDS_PRESSED_AT.load(Ordering::Relaxed) + 5000
                    && CARDS_PRESSES.load(Ordering::Relaxed) < 3
                {
                    let live: Vec<usize> = GENERIC_BUTTONS
                        .lock()
                        .ok()
                        .and_then(|g| g.as_ref().map(|v| {
                            let now = now_ms();
                            v.iter().filter(|(_, t)| now.saturating_sub(*t) < 400).map(|(p, _)| *p).collect()
                        }))
                        .unwrap_or_default();
                    match live.len() {
                        1 => {
                            let b = live[0] as *mut c_void;
                            let parent = bridge::parent_name(b);
                            if crate::ui_input::click_now(b) {
                                let k = CARDS_PRESSES.fetch_add(1, Ordering::Relaxed) + 1;
                                CARDS_PRESSED_AT.store(now_ms(), Ordering::Relaxed);
                                log(&format!("cards screen: pressed the single \"Button\" (parent \"{parent}\", press {k})"));
                                // MEASURED: this press starts the race (race header 22 s later). There
                                // is no further paddock on this flow - the cards screen IS the Room
                                // Match paddock step - so hand off to the race wait NOW; staying here
                                // let the paddock deadline stop the run mid-race.
                                say("Race! (cards) pressed - race loading…".into());
                                RACE_PRESSED_AT.store(now_ms(), Ordering::Relaxed);
                                CONFIRMS.store(0, Ordering::Relaxed);
                                RACE_PRESSES.store(99, Ordering::Relaxed); // no holder to re-press on this flow
                                DIAG_NEXT.store(0, Ordering::Relaxed);
                                goto(S_RACE, 800, RACE_TIMEOUT_MS);
                                return;
                            }
                        }
                        0 => {}
                        k => {
                            if now_ms() >= DIAG_NEXT.load(Ordering::Relaxed) {
                                DIAG_NEXT.store(now_ms() + 3000, Ordering::Relaxed);
                                let names: Vec<String> = live.iter().map(|p| bridge::parent_name(*p as *mut c_void)).collect();
                                log(&format!("cards screen: {k} generic buttons live, not pressing (parents: {})", names.join(", ")));
                            }
                        }
                    }
                }
                let vc = SCREEN_VC.load(Ordering::Relaxed) as *mut c_void;
                // One call only: the PlayInView pointer is dead once this advances.
                let cand: &[&str] = &["OnClickRaceStart"];
                if (n as usize) < cand.len() && now_ms() >= STEP_NEXT.load(Ordering::Relaxed) {
                    let m = cand[n as usize];
                    match bridge::invoke0_checked(vc, m) {
                        Ok(()) => log(&format!("paddock overview: called {m}")),
                        Err(e) => log(&format!("paddock overview: {m} - {e}")),
                    }
                    STEP_TRIES.store(n + 1, Ordering::Relaxed);
                    STEP_NEXT.store(now_ms() + 2500, Ordering::Relaxed);
                }
                if expired() {
                    fail("paddock never appeared past the runner overview - press Next by hand");
                } else {
                    set_status("Paddock - leaving the runner overview…".into());
                    retry();
                }
                return;
            }
            // The paddock proper has just appeared (the overview step is behind us). A press in
            // its first second was accepted by the button and ignored by the paddock; give it a
            // moment, remembered from the first frame the Race! button existed.
            if HOLDER_SEEN_AT.load(Ordering::Relaxed) == 0 {
                HOLDER_SEEN_AT.store(now_ms(), Ordering::Relaxed);
            }
            if now_ms() < HOLDER_SEEN_AT.load(Ordering::Relaxed) + 2000 {
                set_status("Paddock - settling…".into());
                retry();
                return;
            }
            {
                // Controller is alive here (StartPaddock just ran on it). Record its step state
                // so a refused press can be explained instead of guessed at.
                let vc = SCREEN_VC.load(Ordering::Relaxed) as *mut c_void;
                let step = bridge::field_i32(vc, "_paddockStepValue");
                let started = bridge::field_u8(vc, "_isRaceStart");
                log(&format!("paddock: before press - step={step:?} isRaceStart={started:?}"));
            }
            let skip = SKIP_RACE.load(Ordering::Relaxed);
            let r = if skip {
                let vc = SCREEN_VC.load(Ordering::Relaxed) as *mut c_void;
                bridge::invoke0_checked(vc, "OnClickRaceSkip")
            } else {
                bridge::click_paddock_race_button()
            };
            match r {
                Ok(()) => {
                    say(format!("{} - confirming…", if skip { "Skipping the race" } else { "Race! pressed" }));
                    RACE_PRESSED_AT.store(now_ms(), Ordering::Relaxed);
                    CONFIRMS.store(0, Ordering::Relaxed);
                    RACE_PRESSES.store(1, Ordering::Relaxed);
                    DIAG_NEXT.store(0, Ordering::Relaxed);
                    goto(S_RACE, 800, RACE_TIMEOUT_MS);
                }
                Err(e) => {
                    if expired() {
                        fail(&format!("paddock Race! ({e})"));
                    } else {
                        set_status(format!("Paddock - waiting for the Race! button ({e})…"));
                        retry();
                    }
                }
            }
        }
        S_RACE => {
            if SCREEN.load(Ordering::Relaxed) == SCR_RESULT {
                say("Result screen - through the follow panel to the save prompt…".into());
                SAVE_PROMPT_SEEN.store(false, Ordering::Relaxed);
                goto(S_RESULT_SAVE, 1500, 30_000);
                return;
            }
            if expired() {
                fail("race didn't reach the result screen in time");
                return;
            }
            // Race! is followed by one or more confirmation dialogs (the paddock's
            // `CloseDialogAndRaceStart` path). Accept each one as it appears - but only while
            // still on the paddock and only in the window right after the press, so an in-race
            // dialog later on is never touched. Confirm is the right slot; centre covers a
            // single-button layout.
            let on_paddock = SCREEN.load(Ordering::Relaxed) == SCR_PADDOCK;
            let recent = now_ms() < RACE_PRESSED_AT.load(Ordering::Relaxed) + 25_000;
            let confirmed = CONFIRMS.load(Ordering::Relaxed);
            if on_paddock && recent && confirmed < 5 {
                let top = bridge::forefront_dialog();
                if !top.is_null() {
                    match bridge::press_dialog_slot(top, &["_rightButton", "_centerButton"], "race-confirm") {
                        Ok(_) => {
                            let n = CONFIRMS.fetch_add(1, Ordering::Relaxed) + 1;
                            set_status(format!("Race! confirmed ({n}) - race running…"));
                            NEXT.store(now_ms() + 900, Ordering::Relaxed);
                        }
                        Err(_) => NEXT.store(now_ms() + 500, Ordering::Relaxed),
                    }
                    return;
                }
            }
            if on_paddock && now_ms() >= DIAG_NEXT.load(Ordering::Relaxed) {
                DIAG_NEXT.store(now_ms() + 3000, Ordering::Relaxed);
                let holder = !bridge::first_instance("Gallop.PaddockContentsHolder").is_null();
                let view = !bridge::first_instance("Gallop.RoomMatchPaddockView").is_null();
                let top = bridge::forefront_dialog();
                let dlg = if top.is_null() { "none".to_string() } else { il2cpp::object_class_name(top) };
                log(&format!(
                    "paddock: {}s after press - holder={holder} paddockView={view} forefrontDialog={dlg}",
                    (now_ms().saturating_sub(RACE_PRESSED_AT.load(Ordering::Relaxed))) / 1000
                ));
            }
            // No dialog, still "on the paddock": did the press take? NEVER touch the paddock
            // controller here - the game frees it as the race scene loads, it has no PlayOutView
            // to clear our pointer, and reading its `_isRaceStart` after the press crashed the
            // game (0xc0000005 in il2cpp_object_get_class). The safe truth is whether the
            // paddock's Race! button still EXISTS: FindObjectsOfType never returns a destroyed
            // object, so "holder gone" means the paddock is being torn down for the race, and
            // "holder still here 4 s after the press" means the press was ignored - press again.
            if on_paddock && confirmed == 0 {
                let presses = RACE_PRESSES.load(Ordering::Relaxed);
                if now_ms() >= RACE_PRESSED_AT.load(Ordering::Relaxed) + 4000 && presses < 6 {
                    if bridge::first_instance("Gallop.PaddockContentsHolder").is_null() {
                        if presses > 0 {
                            set_status("Race starting…".into());
                        }
                        RACE_PRESSES.store(99, Ordering::Relaxed); // settled: stop checking
                    } else {
                        log(&format!("paddock: Race! button still up 4s after press #{presses} - pressing again"));
                        if bridge::click_paddock_race_button().is_ok() {
                            RACE_PRESSED_AT.store(now_ms(), Ordering::Relaxed);
                            RACE_PRESSES.fetch_add(1, Ordering::Relaxed);
                            set_status(format!("Race! pressed again ({}) - confirming…", presses + 1));
                        }
                    }
                } else if on_paddock
                    && confirmed == 0
                    && (1..3).contains(&presses)
                    && now_ms() >= RACE_PRESSED_AT.load(Ordering::Relaxed) + 6000
                    && PADDOCK_STEP_VC.load(Ordering::Relaxed) != 0
                {
                    // Handler path (no holder to check): if nothing followed in 6 s, go back to
                    // the paddock state, which re-fires the handler on the step controller.
                    log(&format!("paddock: nothing followed handler attempt {presses} in 6s - retrying"));
                    goto(S_PADDOCK, 0, 60_000);
                }
            }
            NEXT.store(now_ms() + 700, Ordering::Relaxed);
        }
        S_RESULT_SAVE => {
            let d = bridge::first_instance("Gallop.DialogRoomMatchSaveRoomConfirm");
            if d.is_null() {
                if SAVE_PROMPT_SEEN.load(Ordering::Relaxed) {
                    // Dismissed -> leave the result screen.
                    goto(S_RESULT_BACK, 400, 8_000);
                } else if expired() {
                    // Never appeared (the follow panel's Next is pressed from the button tick);
                    // try to leave anyway rather than sit here.
                    log("save prompt never appeared - leaving the result screen");
                    goto(S_RESULT_BACK, 0, 8_000);
                } else {
                    set_status("Result screen - waiting for the save prompt…".into());
                    retry();
                }
                return;
            }
            SAVE_PROMPT_SEEN.store(true, Ordering::Relaxed);
            if expired() {
                fail("couldn't dismiss the save-race prompt - close it by hand");
                return;
            }
            // Its own DialogCommon; left slot is the dismiss. Right would SAVE the race.
            let dc = bridge::field_ptr(d, "_dialog");
            match bridge::press_dialog_slot(dc, &["_leftButton", "_centerButton"], "save-prompt") {
                Ok(_) => NEXT.store(now_ms() + 900, Ordering::Relaxed),
                Err(_) => retry(),
            }
        }
        S_RESULT_BACK => {
            if SCREEN.load(Ordering::Relaxed) != SCR_RESULT {
                goto(S_RETURN, 500, 30_000);
                return;
            }
            let vc = SCREEN_VC.load(Ordering::Relaxed) as *mut c_void;
            match bridge::invoke0_checked(vc, "OnClickOsBackKey") {
                Ok(()) => {
                    log("result: back");
                    goto(S_RESULT_CONFIRM, 800, 6_000);
                }
                Err(e) => fail(&format!("leaving the result screen failed ({e})")),
            }
        }
        S_RESULT_CONFIRM => {
            // Back usually asks "return to Room Match?" - accept it. If nothing asked, the scene is
            // already changing.
            if SCREEN.load(Ordering::Relaxed) != SCR_RESULT {
                goto(S_RETURN, 500, 30_000);
                return;
            }
            let top = bridge::forefront_dialog();
            if !top.is_null() {
                match bridge::press_dialog_slot(top, &["_rightButton", "_centerButton"], "leave-result") {
                    Ok(_) => goto(S_RETURN, 800, 30_000),
                    Err(_) => retry(),
                }
                return;
            }
            if expired() {
                goto(S_RETURN, 0, 30_000);
            } else {
                retry();
            }
        }
        S_REPLAY => {
            if SCREEN.load(Ordering::Relaxed) == SCR_TOP && REPLAY_CURSOR.load(Ordering::Relaxed) > 0 {
                let room = CURRENT_ROOM.load(Ordering::Relaxed);
                mark_done(room);
                let n = DONE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                say(format!("Room {room} done via replay ({n} this run) - checking for more…"));
                goto(S_OPEN_LIST, 1500, 15_000);
                return;
            }
            // Every recorded press done but not back at the top: the recording may have ended
            // early. Fall back to the hand-built exit if we are on the result screen.
            if replay_expected().is_none()
                && REPLAY_CURSOR.load(Ordering::Relaxed) > 0
                && now_ms() > REPLAY_LAST_PRESS.load(Ordering::Relaxed) + 20_000
            {
                if SCREEN.load(Ordering::Relaxed) == SCR_RESULT {
                    log("replay: all presses done, still on the result screen - using the built-in exit");
                    goto(S_RESULT_BACK, 0, 8_000);
                } else {
                    fail("replay finished but the top screen never returned - record through to the top");
                }
                return;
            }
            if expired() {
                let (cur, total) = replay_expected().map(|(c, t, _)| (c, t)).unwrap_or((0, 0));
                fail(&format!("replay stalled at step {}/{total}", cur + 1));
                return;
            }
            NEXT.store(now_ms() + 500, Ordering::Relaxed);
        }
        S_RETURN => {
            if SCREEN.load(Ordering::Relaxed) == SCR_TOP {
                let room = CURRENT_ROOM.load(Ordering::Relaxed);
                mark_done(room);
                let n = DONE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                say(format!("Room {room} done ({n} this run) - checking for more…"));
                goto(S_OPEN_LIST, 1500, 15_000);
            } else if expired() {
                fail("didn't return to the Room Match top screen");
            } else {
                retry();
            }
        }
        _ => STATE.store(S_IDLE, Ordering::Relaxed),
    }
}

// ── IL2CPP bridge ───────────────────────────────────────────────────────────────────────────────
mod bridge {
    use super::*;
    use crate::pruner::bridge::{invoke0, plain_string, rd_ptr, unbox_i64, work_data_manager};

    fn plausible(p: *mut c_void) -> bool {
        let v = p as usize;
        v >= 0x10000 && v < 0x0000_8000_0000_0000
    }

    /// First live instance of a MonoBehaviour class, or null. One FindObjectsOfType call - fine
    /// on demand, never per frame without a state gate.
    pub unsafe fn first_instance(class: &str) -> *mut c_void {
        let k = il2cpp::class(class);
        let k_obj = il2cpp::class("UnityEngine.Object");
        if k.is_null() || k_obj.is_null() {
            return std::ptr::null_mut();
        }
        let find = il2cpp::method_with_param_types(k_obj, "FindObjectsOfType", &["System.Type"]);
        let ty = il2cpp::type_object(k);
        if find.is_null() || ty.is_null() {
            return std::ptr::null_mut();
        }
        let mut args: [*mut c_void; 1] = [ty as *mut c_void];
        let (arr, exc) = il2cpp::runtime_invoke_exc(find, std::ptr::null_mut(), &mut args);
        if !exc.is_null() || arr.is_null() {
            return std::ptr::null_mut();
        }
        let arr = arr as *mut c_void;
        if crate::htt_il2cpp::array_len(arr as *mut _) == 0 {
            return std::ptr::null_mut();
        }
        let o = rd_ptr(arr, 0x20);
        if plausible(o) { o } else { std::ptr::null_mut() }
    }

    pub unsafe fn field_ptr(obj: *mut c_void, name: &str) -> *mut c_void {
        if !plausible(obj) {
            return std::ptr::null_mut();
        }
        let k = il2cpp::object_class(obj);
        match il2cpp::field_offset(k, name) {
            Some(off) => {
                let p = rd_ptr(obj, off);
                if plausible(p) { p } else { std::ptr::null_mut() }
            }
            None => std::ptr::null_mut(),
        }
    }

    /// 0-arg instance call, resolved on the object's own class (parents included), managed
    /// exceptions reported instead of swallowed.
    pub unsafe fn invoke0_checked(obj: *mut c_void, name: &str) -> Result<(), String> {
        if !plausible(obj) {
            return Err("no object".into());
        }
        let k = il2cpp::object_class(obj);
        let m = il2cpp::method(k, name, 0);
        if m.is_null() {
            return Err(format!("{name} not found on {}", il2cpp::class_name(k)));
        }
        let (_, exc) = il2cpp::runtime_invoke_exc(m, obj, &mut []);
        if !exc.is_null() {
            return Err(format!("{name} threw {}", il2cpp::object_class_name(exc)));
        }
        Ok(())
    }

    /// Raw field reads by name, for a controller that is PROVABLY alive at the call site.
    pub unsafe fn field_i32(obj: *mut c_void, name: &str) -> Option<i32> {
        if !plausible(obj) {
            return None;
        }
        let off = il2cpp::field_offset(il2cpp::object_class(obj), name)?;
        Some(*((obj as usize + off) as *const i32))
    }
    pub unsafe fn field_u8(obj: *mut c_void, name: &str) -> Option<u8> {
        if !plausible(obj) {
            return None;
        }
        let off = il2cpp::field_offset(il2cpp::object_class(obj), name)?;
        Some(*((obj as usize + off) as *const u8))
    }

    /// Full transform path of a component ("Root/Panel/Button"), bounded depth. This is the
    /// identity used by record & replay: names alone collide ("Button"), paths do not.
    pub unsafe fn object_path(component: *mut c_void) -> String {
        if !plausible(component) {
            return String::new();
        }
        let mut tf = invoke0(component, il2cpp::object_class(component), "get_transform");
        let mut parts: Vec<String> = Vec::new();
        for _ in 0..12 {
            if !plausible(tf) {
                break;
            }
            let k = il2cpp::object_class(tf);
            parts.push(plain_string(invoke0(tf, k, "get_name")));
            tf = invoke0(tf, k, "get_parent");
        }
        parts.reverse();
        parts.join("/")
    }

    /// `<parent GameObject name>` of a component, for telling identically named buttons apart.
    pub unsafe fn parent_name(component: *mut c_void) -> String {
        if !plausible(component) {
            return "?".into();
        }
        let tf = invoke0(component, il2cpp::object_class(component), "get_transform");
        if !plausible(tf) {
            return "?".into();
        }
        let parent = invoke0(tf, il2cpp::object_class(tf), "get_parent");
        if !plausible(parent) {
            return "(root)".into();
        }
        plain_string(invoke0(parent, il2cpp::object_class(parent), "get_name"))
    }

    pub unsafe fn invoke_bool(obj: *mut c_void, name: &str) -> Option<bool> {
        if !plausible(obj) {
            return None;
        }
        let k = il2cpp::object_class(obj);
        let m = il2cpp::method(k, name, 0);
        if m.is_null() {
            return None;
        }
        let (r, exc) = il2cpp::runtime_invoke_exc(m, obj, &mut []);
        if !exc.is_null() || r.is_null() {
            return None;
        }
        // Boxed System.Boolean: one byte after the object header.
        Some(*((r as usize + 0x10) as *const u8) != 0)
    }

    /// Press a ButtonCommon field on the Room Match top view (`Gallop.RoomMatchTop`).
    pub unsafe fn press_top_button(field: &str) -> Result<(), String> {
        let top = first_instance("Gallop.RoomMatchTop");
        if top.is_null() {
            return Err("top view not on screen".into());
        }
        let b = field_ptr(top, field);
        if b.is_null() {
            return Err(format!("{field} missing"));
        }
        if crate::ui_input::click_now(b) { Ok(()) } else { Err("button locked".into()) }
    }

    /// The Sign-Ups dialog's own row handler. The int is the room id: the list is built with an
    /// `Action<int, ulong>` (roomId, registerId) and the regist-tab lambda forwards the first.
    pub unsafe fn click_regist_item(dlg: *mut c_void, room_id: i32) -> Result<(), String> {
        let k = il2cpp::object_class(dlg);
        let m = il2cpp::method(k, "OnClickRegistRaceListItem", 1);
        if m.is_null() {
            return Err("OnClickRegistRaceListItem not found".into());
        }
        let mut v = room_id;
        let mut args: [*mut c_void; 1] = [&mut v as *mut i32 as *mut c_void];
        let (_, exc) = il2cpp::runtime_invoke_exc(m, dlg, &mut args);
        if !exc.is_null() {
            return Err(format!("threw {}", il2cpp::object_class_name(exc)));
        }
        Ok(())
    }

    /// The waiting room's Race! button, through the view's own accessor so lock state is honoured.
    pub unsafe fn click_lobby_race_button() -> Result<(), String> {
        let view = first_instance("Gallop.RoomMatchLobbyView");
        if view.is_null() {
            return Err("lobby view not found".into());
        }
        let b = invoke0(view, il2cpp::object_class(view), "get_RaceButton");
        if !plausible(b) {
            return Err("RaceButton missing".into());
        }
        if crate::ui_input::click_now(b) { Ok(()) } else { Err("locked".into()) }
    }

    /// The paddock's Race! button via `PaddockContentsHolder`'s own accessors. Centre is the
    /// single-button layout the Room Match paddock shows; right exists on layouts with a skip.
    pub unsafe fn click_paddock_race_button() -> Result<(), String> {
        let holder = first_instance("Gallop.PaddockContentsHolder");
        if holder.is_null() {
            return Err("paddock contents not found".into());
        }
        let k = il2cpp::object_class(holder);
        let mut names: Vec<String> = Vec::new();
        for getter in ["get_RaceStartButtonCenter", "get_RaceStartButtonRight"] {
            let b = invoke0(holder, k, getter);
            if !plausible(b) {
                continue;
            }
            names.push(format!("{getter}=\"{}\"", crate::ui_input::button_name(b)));
            if crate::ui_input::click_now(b) {
                log(&format!("paddock: pressed {getter} ({})", names.join(" ")));
                return Ok(());
            }
        }
        if names.is_empty() { Err("no race button on the holder".into()) } else { Err(format!("locked: {}", names.join(" "))) }
    }

    pub unsafe fn forefront_dialog() -> *mut c_void {
        let dm = il2cpp::class("Gallop.DialogManager");
        let m = il2cpp::method(dm, "GetForeFrontDialog", 0);
        if m.is_null() {
            return std::ptr::null_mut();
        }
        let top = il2cpp::runtime_invoke(m, std::ptr::null_mut(), &mut []);
        if plausible(top) { top } else { std::ptr::null_mut() }
    }

    /// Press the first present slot of a DialogCommon's current DialogObject, logging every
    /// slot's name first (they are unnamed "ButtonLeft"/"ButtonRight", so order IS the choice).
    pub unsafe fn press_dialog_slot(dc: *mut c_void, order: &[&str], what: &str) -> Result<&'static str, String> {
        if !plausible(dc) {
            return Err("no dialog".into());
        }
        let dobj = field_ptr(dc, "_currentDialogObj");
        if dobj.is_null() {
            return Err("dialog has no content".into());
        }
        let mut found: Vec<(&str, *mut c_void)> = Vec::new();
        for slot in ["_leftButton", "_centerButton", "_rightButton"] {
            let b = field_ptr(dobj, slot);
            if !b.is_null() {
                found.push((slot, b));
            }
        }
        if found.is_empty() {
            return Err("dialog has no buttons".into());
        }
        log(&format!(
            "{what}: buttons {}",
            found.iter().map(|(s, b)| format!("{s}=\"{}\"", crate::ui_input::button_name(*b))).collect::<Vec<_>>().join(" ")
        ));
        for want in order {
            if let Some((slot, b)) = found.iter().find(|(s, _)| s == want) {
                if crate::ui_input::click_now(*b) {
                    log(&format!("{what}: pressed {slot}"));
                    return Ok(slot);
                }
                return Err("locked".into());
            }
        }
        Err("wanted slot absent".into())
    }

    pub struct Entry {
        pub room_id: i64,
        pub host: String,
        pub start_unix: i64,
        pub can_watch: bool,
        pub sim_done: bool,
        pub allow_display: bool,
    }

    /// `WorkRoomMatchData.get_MyEntryRoomList()` - the races the player is signed up for.
    pub unsafe fn my_entries() -> Vec<Entry> {
        let mut out = Vec::new();
        let wdm = work_data_manager();
        if wdm.is_null() {
            return out;
        }
        let blob = invoke0(wdm, il2cpp::class("Gallop.WorkDataManager"), "get_RoomMatchData");
        if !plausible(blob) {
            return out;
        }
        let list = invoke0(blob, il2cpp::object_class(blob), "get_MyEntryRoomList");
        if !plausible(list) {
            return out;
        }
        let items = rd_ptr(list, 0x10);
        let n = *((list as usize + 0x18) as *const i32);
        if !plausible(items) || n <= 0 || n > 64 {
            return out;
        }
        for i in 0..n as usize {
            let e = rd_ptr(items, 0x20 + i * 8);
            if !plausible(e) {
                continue;
            }
            let k = il2cpp::object_class(e);
            let room_id = unbox_i64(invoke0(e, k, "get_RoomId")).unwrap_or(0);
            if room_id == 0 {
                continue;
            }
            let hu = invoke0(e, k, "get_HostUser");
            let host = if plausible(hu) { plain_string(invoke0(hu, il2cpp::object_class(hu), "get_Name")) } else { String::new() };
            let b = |name: &str| -> bool {
                let r = invoke0(e, k, name);
                plausible(r) && *((r as usize + 0x10) as *const u8) != 0
            };
            out.push(Entry {
                room_id,
                host,
                start_unix: unbox_i64(invoke0(e, k, "get_StartUnixTime")).unwrap_or(0),
                can_watch: b("get_CanWatch"),
                sim_done: b("get_IsRaceSimulateDone"),
                allow_display: b("get_AllowDisplay"),
            });
        }
        out
    }
}
