//! Team Trials auto-player — runs Team Trials races back to back until race points run out.
//!
//! Same shape as the Room Match watcher, minus the machinery that flow needed: there is no
//! paddock, no race scene, no screen controller to detour. The whole loop is nine button presses
//! on five screens, so this module is a replay engine and nothing else.
//!
//! The sequence below is not a theory about handler names — it is the player's own clicks,
//! captured by the dev click recorder on 2026-09-07 and read back out of
//! `trackside-logs/click-recording.txt`. Each step is a button's object-path suffix, matched with
//! `ends_with`, so the canvas root and the `Gallop.GameSystem/…` prefix do not matter.
//!
//!   Team Race          `TeamStadiumView/ContentsRoot/Buttons/utx_btn_home_00_sl`
//!   Opponent (top)     `TeamStadiumOpponentSelectView/ContentsRoot/Opponents/1/Button`
//!   Next               `TeamStadiumDecideView/ContentsRoot/NextButton`
//!   Item (leftmost)    `DialogTeamStadiumItemList/…/ItemIcon/Button`
//!   Race! (-1 RP)      `DialogTeamStadiumItemList/ButtonNext`
//!   See All Results    `TeamStadiumRaceListView/ContentsRoot/AllRaceResultButton`
//!   Skip               `TeamStadiumResultCutinUI/AllRaceSkipButton`
//!   Next               `DialogTeamStadiumAllRaceResult/ButtonCommon`
//!   Skip the tally     `TeamStadiumGrandResultView/ContentsRoot/SkipButton`      (optional)
//!   Race Again         `TeamStadiumGrandResultView/ContentsRoot/RetryButton`
//!
//! The SkipButton was not in the recording - the player never pressed it - but the first real run
//! found it, and it turned out to be the "extra click on a high score" from the original
//! description. It is not high-score-specific: the grand result plays a score tally every lap, and
//! Race Again only goes live once that finishes. On a high-score lap the tally runs longer, which
//! is why it reads as an extra step. Pressing Skip shortens every lap and removes the special case.
//!
//! One press is still unknown: whatever replaces Race Again once race points are gone. Nothing is
//! guessed at - every distinct button on the grand result screen is logged once per run, so the
//! first 0-RP run names it. Until then that case ends the run on a timeout, which is correct
//! behaviour either way.
//!
//! The engine knows where it is by looking, not by counting. Every live button that matches a
//! step is noted with a timestamp (`LIVE_AT`), so at any moment the set of live steps says which
//! screen is up. That is used twice: a run can start from ANY screen of the loop, and a run that
//! has lost its place - a press the game accepted and ignored, a lap that came back somewhere
//! unexpected - notices within a few seconds that the screen it is waiting for is not the one on
//! display, and continues from the one that is. A cursor that only ever advances cannot do either;
//! the first real runs proved it by sitting for 18 s on a screen whose buttons were all in view.
//!
//! Everything here runs on the game main thread: `on_button_update` from the ButtonCommon.Update
//! detour, `pump` from the TweenManager tick. The overlay only flips flags and reads strings.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::tools::clock;

fn log(msg: &str) {
    crate::tools::log(&format!("[ttplay] {msg}"));
}

fn now_ms() -> u64 {
    clock().elapsed().as_millis() as u64
}

// ── the recorded flow ───────────────────────────────────────────────────────────────────────────

/// `(path suffix, optional)`. Optional steps are pressed when they appear but never waited for:
/// the run moves on the moment a later step's button is live. Only the opponent pick is optional,
/// because "Race Again" may or may not return to the opponent list, and a lap that skips it must
/// not stall.
const FLOW: &[(&str, bool)] = &[
    // Team Trials top: Team Race
    ("TeamStadiumView(Clone)/ContentsRoot/Buttons/utx_btn_home_00_sl", false),
    // opponent select: the top of the three
    ("TeamStadiumOpponentSelectView(Clone)/ContentsRoot/Opponents/1/Button", true),
    // team decided: Next
    ("TeamStadiumDecideView(Clone)/ContentsRoot/NextButton", false),
    // item list: the leftmost item. Every icon in the list shares this path, so the press target
    // is chosen by sibling order, not by whichever one happens to tick first - see ITEM_STEP.
    ("DialogTeamStadiumItemListContainer(Clone)/DialogTeamStadiumItemListIconBase(Clone)/ItemIcon/Button", false),
    // item list: Race! (spends 1 RP)
    ("DialogTeamStadiumItemList(Clone)/ButtonNext", false),
    // race list: See All Race Results
    ("TeamStadiumRaceListView(Clone)/ContentsRoot/AllRaceResultButton", false),
    // result cut-in: Skip
    ("TeamStadiumResultCutinUI(Clone)/AllRaceSkipButton", false),
    // all-race result dialog: Next - the race is over once this lands, so this is where a race
    // is counted, not at Race Again (which may never come).
    ("DialogTeamStadiumAllRaceResult(Clone)/ButtonCommon", false),
    // grand result: skip the score tally. Optional - Race Again goes live on its own once the
    // tally ends, this just stops us waiting through it.
    ("TeamStadiumGrandResultView(Clone)/ContentsRoot/SkipButton", true),
    // grand result: Race Again (absent once race points run out - that ends the run)
    ("TeamStadiumGrandResultView(Clone)/ContentsRoot/RetryButton", false),
];

/// What each step is called in the overlay. The object names ("AllRaceResultButton") belong in
/// the log; the panel says what the player would say.
const STEP_LABELS: &[&str] = &[
    "Team Race",
    "top opponent",
    "confirm team",
    "leftmost item",
    "Race!",
    "see all results",
    "skip the cut-in",
    "close results",
    "skip the tally",
    "Race Again",
];

/// Which screen each step lives on. Locating uses the MOST ADVANCED screen with a live button
/// (a view can linger, deactivated late, under the one that replaced it) and then the FIRST step
/// on that screen (the item dialog must pick an item before its Next).
const SCREEN: &[u8] = &[0, 1, 2, 3, 3, 4, 5, 6, 7, 7];
const _: () = assert!(FLOW.len() == SCREEN.len());

const N: usize = FLOW.len();
/// Cursor value while the run has not yet found its screen.
const UNRESOLVED: usize = usize::MAX;

/// Race points cap out at 5, so a full bar is 5 races and there is nothing above it to offer.
pub const MAX_RP: i32 = 5;

/// A label per step, or the status line indexes off the end of one of them.
const _: () = assert!(FLOW.len() == STEP_LABELS.len());

/// Index of the item-list step: the one step whose path matches several buttons at once.
const ITEM_STEP: usize = 3;
/// The race is over once this step lands. Counted here rather than at Race Again so a run that
/// ends on the last of the race points still reports the race it just finished.
const RACE_DONE_STEP: usize = 7;
/// First step on the grand result screen. From here on, that screen's buttons are the ones we
/// watch and the ones we log.
const SKIP_STEP: usize = 8;
/// Index of the last step. Pressing it starts the next lap.
const LAST_STEP: usize = FLOW.len() - 1;
/// Laps restart here, not at 0: the Team Trials top screen is behind us by then.
const LAP_START: usize = 1;

/// Screen the last step lives on. Seeing any button from it while Race Again is missing is what
/// separates "out of race points" from "something unexpected happened".
const GRAND_RESULT_VIEW: &str = "TeamStadiumGrandResultView";

// ── state ───────────────────────────────────────────────────────────────────────────────────────

static RUNNING: AtomicBool = AtomicBool::new(false);
static REQ_START: AtomicBool = AtomicBool::new(false);
static REQ_STOP: AtomicBool = AtomicBool::new(false);
static CURSOR: AtomicUsize = AtomicUsize::new(0);
static LAPS: AtomicI32 = AtomicI32::new(0);
/// Stop after this many races. 0 = until race points run out. The bar holds at most
/// [`MAX_RP`] points, so that is the whole useful range.
static LAP_LIMIT: AtomicI32 = AtomicI32::new(0);
/// When the current step's button was first seen live (0 = not yet).
static SEEN_AT: AtomicU64 = AtomicU64::new(0);
static LAST_PRESS: AtomicU64 = AtomicU64::new(0);
static OPT_PRESSES: AtomicI32 = AtomicI32::new(0);
/// When we started waiting for the current step. Reset on EVERY step entry, relocation included,
/// so the relocation spacing stays honest.
static STEP_SINCE: AtomicU64 = AtomicU64::new(0);
/// When the run last made genuine progress - a step further into the lap than any reached since
/// the lap began. Relocating back does not count, and neither does re-entering a step already
/// reached. Drives the learn log and the stall timeout.
///
/// Both used to hang off `STEP_SINCE`, which every relocation reset. A lap that ping-ponged
/// between the item step and Race! reset it every ~3.9 s, so the learn log (6 s) never fired and
/// the stall timeout (90 s) never fired either: one run pressed the same item ~60 times across
/// four minutes and stopped with no record of what was on the screen.
static PROGRESS_SINCE: AtomicU64 = AtomicU64::new(0);
/// Relocations since the last genuine progress. One is ordinary recovery from a swallowed press.
static RELOCATES: AtomicI32 = AtomicI32::new(0);
/// Furthest step reached since the lap began. A step at or behind it is somewhere the run has
/// already been, so arriving there again is not progress however it happened.
static HIGH_WATER: AtomicUsize = AtomicUsize::new(0);
/// When the grand result screen was first seen this lap (0 = not yet).
static GRAND_NO_RETRY_AT: AtomicU64 = AtomicU64::new(0);
/// Set once Race Again has actually been seen live this lap - the difference between "the tally is
/// still playing" and "there are no race points left".
static RETRY_SEEN: AtomicBool = AtomicBool::new(false);
/// Per step: when a button matching it was last seen live (0 = never this run). The run's eyes.
static LIVE_AT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// The step the screen currently points at, and since when - a location has to hold still for
/// [`LOCATE_STABLE_MS`] before it is acted on, so a screen transition cannot mislead.
static LOC_CAND: AtomicUsize = AtomicUsize::new(UNRESOLVED);
static LOC_SINCE: AtomicU64 = AtomicU64::new(0);
static START_AT: AtomicU64 = AtomicU64::new(0);
/// When the game first refused a press on the current step (0 = it has not), and whether that has
/// been reported. A button that is up but locked means something is over it.
static LOCKED_SINCE: AtomicU64 = AtomicU64::new(0);
static LOCK_REPORTED: AtomicBool = AtomicBool::new(false);
/// Live Team Trials top button seen recently — drives the "ready" dot with no extra hooks.
static TOP_SEEN_AT: AtomicU64 = AtomicU64::new(0);

static STATUS: Mutex<String> = Mutex::new(String::new());
static PATH_CACHE: Mutex<Option<HashMap<usize, String>>> = Mutex::new(None);
/// Item-list candidates this frame: (button, sibling index).
static ITEM_CANDS: Mutex<Option<Vec<(usize, i32)>>> = Mutex::new(None);
/// Paths already written to the log by the stall report, so it says each one once.
static LEARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// A step that never turns up ends the run. Generous: the result cut-in alone runs ~10 s.
const STALL_MS: u64 = 90_000;
/// How long the grand result may sit there without a Race Again before we call it out of race
/// points. Measured from the LATER of "grand result first seen" and our last press, so skipping
/// the tally extends it rather than racing it.
///
/// 6 s was the first guess and it was wrong three times in one session: the score tally holds Race
/// Again back ~3.5 s on an ordinary lap and longer on a high score, so the run stopped saying "out
/// of race points" with points still in the bar. Generous now - a real 0-RP stop is not urgent.
const NO_RETRY_MS: u64 = 25_000;
/// A required button must be live this long before it is pressed. A press on a button that has
/// only just appeared is accepted and thrown away by the game.
const SETTLE_MS: u64 = 700;
const GAP_MS: u64 = 700;
/// Optional steps: longer settle, re-pressed while they stay live, bounded.
const OPT_SETTLE_MS: u64 = 1_500;
const OPT_GAP_MS: u64 = 2_500;
const OPT_MAX_PRESSES: i32 = 6;
/// The item list is sampled for this long before the leftmost one is pressed, so every icon in it
/// has ticked at least once - and so the dialog has finished arriving. 700 ms was on the edge:
/// the same press landed one lap and was swallowed the next, 21 ms apart.
const ITEM_COLLECT_MS: u64 = 900;
/// A button seen this recently counts as live.
const LIVE_WINDOW_MS: u64 = 300;
/// A located screen must hold for this long before the run acts on it.
const LOCATE_STABLE_MS: u64 = 1_000;
/// With the expected step not in view for this long, and another screen of the loop stable in
/// view, the run has lost its place - move to where the screen says it is.
const RELOCATE_MS: u64 = 3_000;
/// Once a step is this overdue, every distinct live button is logged once - the diagnostic that
/// names whatever is in the way. The gap between Race! and the race list is ~4.5 s, so this stays
/// quiet on an ordinary lap.
const LEARN_AFTER_MS: u64 = 6_000;
/// Relocations without progress before the run gives up. The screen and the flow disagreeing once
/// is a swallowed press and recoverable; four times running means pressing again will not fix it,
/// and by then the learn log (6 s) has named everything on the screen.
const MAX_RELOCATES: i32 = 4;
/// A press refused for this long gets reported: the button is up, the game will not take it.
const LOCK_REPORT_MS: u64 = 2_000;
/// Started with no screen of the loop in view: wait this long for one, then give up.
const WAIT_FOR_SCREEN_MS: u64 = 300_000;

/// Required steps settle for [`SETTLE_MS`], except where a lap showed that is too tight.
fn settle_ms(idx: usize) -> u64 {
    match idx {
        // Race! right after the item pick: 716 ms was swallowed, 737 ms landed.
        4 => 1_200,
        _ => SETTLE_MS,
    }
}

fn set_status(s: String) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s;
    }
}

pub fn status() -> String {
    STATUS.lock().ok().map(|g| g.clone()).unwrap_or_default()
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

pub fn laps() -> i32 {
    LAPS.load(Ordering::Relaxed)
}

pub fn lap_limit() -> i32 {
    LAP_LIMIT.load(Ordering::Relaxed)
}

pub fn set_lap_limit(n: i32) {
    LAP_LIMIT.store(n.clamp(0, MAX_RP), Ordering::Relaxed);
}

/// `(step, total)` while running, for the progress pill.
pub fn progress() -> Option<(usize, usize)> {
    if !RUNNING.load(Ordering::Relaxed) {
        return None;
    }
    let cur = CURSOR.load(Ordering::Relaxed);
    if cur == UNRESOLVED {
        return None;
    }
    Some((cur + 1, FLOW.len()))
}

/// True while a Team Trials top-screen button has ticked in the last 2 s.
pub fn on_top_screen() -> bool {
    let t = TOP_SEEN_AT.load(Ordering::Relaxed);
    t != 0 && now_ms().saturating_sub(t) < 2_000
}

pub fn request_start() {
    REQ_START.store(true, Ordering::Relaxed);
}

pub fn request_stop() {
    REQ_STOP.store(true, Ordering::Relaxed);
}

fn begin() {
    let now = now_ms();
    RUNNING.store(true, Ordering::Relaxed);
    // No assumption about where the player is: the first stable screen of the loop sets the cursor.
    CURSOR.store(UNRESOLVED, Ordering::Relaxed);
    LAPS.store(0, Ordering::Relaxed);
    SEEN_AT.store(0, Ordering::Relaxed);
    LAST_PRESS.store(0, Ordering::Relaxed);
    OPT_PRESSES.store(0, Ordering::Relaxed);
    STEP_SINCE.store(now, Ordering::Relaxed);
    PROGRESS_SINCE.store(now, Ordering::Relaxed);
    RELOCATES.store(0, Ordering::Relaxed);
    HIGH_WATER.store(0, Ordering::Relaxed);
    START_AT.store(now, Ordering::Relaxed);
    GRAND_NO_RETRY_AT.store(0, Ordering::Relaxed);
    RETRY_SEEN.store(false, Ordering::Relaxed);
    LOCKED_SINCE.store(0, Ordering::Relaxed);
    LOCK_REPORTED.store(false, Ordering::Relaxed);
    for t in LIVE_AT.iter() {
        t.store(0, Ordering::Relaxed);
    }
    LOC_CAND.store(UNRESOLVED, Ordering::Relaxed);
    LOC_SINCE.store(0, Ordering::Relaxed);
    if let Ok(mut c) = PATH_CACHE.lock() {
        *c = Some(HashMap::new());
    }
    if let Ok(mut c) = ITEM_CANDS.lock() {
        *c = Some(Vec::new());
    }
    if let Ok(mut c) = LEARNED.lock() {
        *c = Some(HashSet::new());
    }
    let limit = LAP_LIMIT.load(Ordering::Relaxed);
    log(&format!(
        "start: {} presses per lap, {}",
        FLOW.len(),
        if limit > 0 { format!("stopping after {limit} race(s)") } else { "running until race points run out".into() }
    ));
    set_status("Looking for a Team Trials screen\u{2026}".into());
}

fn finish(msg: &str) {
    if !RUNNING.swap(false, Ordering::Relaxed) {
        return;
    }
    log(&format!("stopped: {msg} ({} race(s) run)", LAPS.load(Ordering::Relaxed)));
    set_status(format!("{msg} \u{00b7} {} race(s) run", LAPS.load(Ordering::Relaxed)));
    if let Ok(mut c) = PATH_CACHE.lock() {
        *c = None;
    }
    if let Ok(mut c) = ITEM_CANDS.lock() {
        *c = None;
    }
}

/// Genuine progress: restart the no-progress clock and forgive the relocations that came before.
fn note_progress() {
    PROGRESS_SINCE.store(now_ms(), Ordering::Relaxed);
    RELOCATES.store(0, Ordering::Relaxed);
}

/// Move the cursor on. Only a step beyond this lap's high-water mark counts as progress: the item
/// step advances to Race! on every re-press, and treating that as progress is what let a stuck lap
/// run for four minutes with both safety nets held off.
fn step_to(next: usize) {
    if next > HIGH_WATER.load(Ordering::Relaxed) {
        HIGH_WATER.store(next, Ordering::Relaxed);
        note_progress();
    }
    enter_step(next);
}

/// Start of a lap (or the first placement of the run): unambiguous progress, and the high-water
/// mark starts again from here.
fn lap_to(next: usize) {
    HIGH_WATER.store(next, Ordering::Relaxed);
    note_progress();
    enter_step(next);
}

/// Go where the screen says the run is. Never progress - the flow and the screen disagreed, and
/// the count of how often that has happened is what ends a run that cannot get past a step.
fn relocate_to(next: usize) {
    RELOCATES.fetch_add(1, Ordering::Relaxed);
    enter_step(next);
}

fn enter_step(next: usize) {
    CURSOR.store(next, Ordering::Relaxed);
    SEEN_AT.store(0, Ordering::Relaxed);
    OPT_PRESSES.store(0, Ordering::Relaxed);
    STEP_SINCE.store(now_ms(), Ordering::Relaxed);
    LOCKED_SINCE.store(0, Ordering::Relaxed);
    LOCK_REPORTED.store(false, Ordering::Relaxed);
    if next <= SKIP_STEP {
        GRAND_NO_RETRY_AT.store(0, Ordering::Relaxed);
        RETRY_SEEN.store(false, Ordering::Relaxed);
    }
    if let Ok(mut c) = PATH_CACHE.lock() {
        if let Some(m) = c.as_mut() {
            m.clear();
        }
    }
    if let Ok(mut c) = ITEM_CANDS.lock() {
        if let Some(v) = c.as_mut() {
            v.clear();
        }
    }
}

/// Steps that may legitimately be pressed next: the current one, plus - while the current one is
/// optional - the ones after it up to and including the first required step.
fn candidates() -> Vec<(usize, &'static str)> {
    let cur = CURSOR.load(Ordering::Relaxed);
    let mut out = Vec::new();
    if cur == UNRESOLVED {
        return out;
    }
    for (i, (p, opt)) in FLOW.iter().enumerate().skip(cur) {
        out.push((i, *p));
        if !*opt {
            break;
        }
    }
    out
}

fn cached_path(this: *mut c_void) -> String {
    let key = this as usize;
    if let Ok(c) = PATH_CACHE.lock() {
        if let Some(p) = c.as_ref().and_then(|m| m.get(&key)) {
            return p.clone();
        }
    }
    let p = unsafe { bridge::object_path(this) };
    if let Ok(mut c) = PATH_CACHE.lock() {
        if let Some(m) = c.as_mut() {
            if m.len() < 256 {
                m.insert(key, p.clone());
            }
        }
    }
    p
}

fn leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ── where are we? ───────────────────────────────────────────────────────────────────────────────

/// The step the screen points at right now: on the most advanced screen with a live button, its
/// first step. `None` when nothing of the loop is in view (mid-race, or not in Team Trials).
fn screen_step(now: u64) -> Option<usize> {
    let live = |i: usize| {
        let t = LIVE_AT[i].load(Ordering::Relaxed);
        t != 0 && now.saturating_sub(t) <= LIVE_WINDOW_MS
    };
    let top = (0..N).filter(|&i| live(i)).map(|i| SCREEN[i]).max()?;
    (0..N).find(|&i| SCREEN[i] == top && live(i))
}

/// Every tick: follow the screen's step and note how long it has held still.
fn track_location(now: u64) {
    match screen_step(now) {
        None => LOC_CAND.store(UNRESOLVED, Ordering::Relaxed),
        Some(s) => {
            if LOC_CAND.swap(s, Ordering::Relaxed) != s {
                LOC_SINCE.store(now, Ordering::Relaxed);
            }
        }
    }
}

/// The screen's step, once it has held for [`LOCATE_STABLE_MS`].
fn stable_location(now: u64) -> Option<usize> {
    let s = LOC_CAND.load(Ordering::Relaxed);
    (s != UNRESOLVED && now.saturating_sub(LOC_SINCE.load(Ordering::Relaxed)) >= LOCATE_STABLE_MS).then_some(s)
}

/// Any of the steps we would press next seen live just now?
fn expected_in_view(now: u64) -> bool {
    candidates().iter().any(|(i, _)| {
        let t = LIVE_AT[*i].load(Ordering::Relaxed);
        t != 0 && now.saturating_sub(t) <= LIVE_WINDOW_MS
    })
}

// ── per-button tick ─────────────────────────────────────────────────────────────────────────────

/// Called from the ButtonCommon.Update detour for every live button, every frame. Two atomic loads
/// and a name compare when idle.
pub fn on_button_update(this: *mut c_void) {
    if this.is_null() {
        return;
    }
    let name = crate::ui_input::button_name(this);
    if name.is_empty() {
        return;
    }
    // Readiness dot: the Team Trials top screen's own button, no extra hook needed.
    if name == leaf(FLOW[0].0) {
        TOP_SEEN_AT.store(now_ms(), Ordering::Relaxed);
    }
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let now = now_ms();
    let cur = CURSOR.load(Ordering::Relaxed);
    let resolved = cur != UNRESOLVED;
    // Which steps could this button be, by leaf name alone? (Two steps are plain "Button".)
    let by_leaf: Vec<usize> = (0..N).filter(|&i| leaf(FLOW[i].0) == name).collect();
    let on_grand = resolved && cur >= SKIP_STEP;
    let overdue = resolved && now >= PROGRESS_SINCE.load(Ordering::Relaxed) + LEARN_AFTER_MS;
    // The path walk costs a dozen calls; only pay it for a button that could matter.
    if by_leaf.is_empty() && !on_grand && !overdue {
        return;
    }
    let path = cached_path(this);
    if path.is_empty() {
        return;
    }
    // The grand result is the fork in the flow: Race Again, or nothing because the bar is empty.
    // While we are on it, note when it appeared and log every distinct button it shows - that log
    // is what will name the 0-RP exit the first time a run actually ends on one.
    if on_grand && path.contains(GRAND_RESULT_VIEW) {
        if GRAND_NO_RETRY_AT.load(Ordering::Relaxed) == 0 {
            GRAND_NO_RETRY_AT.store(now, Ordering::Relaxed);
        }
        if name == leaf(FLOW[LAST_STEP].0) {
            RETRY_SEEN.store(true, Ordering::Relaxed);
        }
        learn("grand result button", &path);
    }
    // The run's eyes: every step this button satisfies is live right now.
    let mut any = false;
    for &i in &by_leaf {
        if path.ends_with(FLOW[i].0) {
            LIVE_AT[i].store(now, Ordering::Relaxed);
            any = true;
        }
    }
    if !any {
        // Not part of the loop. Overdue, that is exactly what we want named.
        if overdue {
            learn("live button while waiting", &path);
        }
        return;
    }
    if !resolved {
        return; // the pump will place the cursor once the screen holds still
    }
    let cands = candidates();
    let Some((idx, expected)) = cands.iter().find(|(i, p)| by_leaf.contains(i) && path.ends_with(p)).cloned() else {
        return;
    };
    if SEEN_AT.load(Ordering::Relaxed) == 0 {
        SEEN_AT.store(now, Ordering::Relaxed);
        set_status(format!("Step {}/{}: {} is up", idx + 1, FLOW.len(), STEP_LABELS[idx]));
    }
    // The item list is the one step whose path matches several buttons. Collect them all and let
    // the pump press the leftmost; pressing on tick would take whichever ticked first.
    if idx == ITEM_STEP {
        let sib = unsafe { bridge::sibling_index_up(this, 2) }.unwrap_or(i32::MAX);
        if let Ok(mut g) = ITEM_CANDS.lock() {
            let v = g.get_or_insert_with(Vec::new);
            if let Some(e) = v.iter_mut().find(|(p, _)| *p == this as usize) {
                e.1 = sib;
            } else if v.len() < 32 {
                v.push((this as usize, sib));
            }
        }
        return;
    }
    press_if_settled(this, idx, expected, now);
}

/// Shared press gate: settle, throttle, then click. Advances the cursor for required steps.
fn press_if_settled(this: *mut c_void, idx: usize, expected: &'static str, now: u64) {
    let optional = FLOW[idx].1;
    let (settle, gap) = if optional { (OPT_SETTLE_MS, OPT_GAP_MS) } else { (settle_ms(idx), GAP_MS) };
    if now < SEEN_AT.load(Ordering::Relaxed) + settle || now < LAST_PRESS.load(Ordering::Relaxed) + gap {
        return;
    }
    if optional && OPT_PRESSES.load(Ordering::Relaxed) >= OPT_MAX_PRESSES {
        return;
    }
    if !unsafe { crate::ui_input::click_now(this) } {
        // Locked: the button is up but the game will not take a press. Keep trying, and say so
        // once it has gone on long enough to mean something is over the screen.
        let since = LOCKED_SINCE.load(Ordering::Relaxed);
        if since == 0 {
            LOCKED_SINCE.store(now, Ordering::Relaxed);
        } else if now >= since + LOCK_REPORT_MS && !LOCK_REPORTED.swap(true, Ordering::Relaxed) {
            log(&format!(
                "step {}/{}: {} is up but the game refuses the press (locked) - something is probably over it",
                idx + 1,
                FLOW.len(),
                leaf(expected)
            ));
            set_status(format!("Step {}/{}: {} is up but blocked", idx + 1, FLOW.len(), STEP_LABELS[idx]));
        }
        return;
    }
    LOCKED_SINCE.store(0, Ordering::Relaxed);
    LAST_PRESS.store(now, Ordering::Relaxed);
    let total = FLOW.len();
    if optional {
        let k = OPT_PRESSES.fetch_add(1, Ordering::Relaxed) + 1;
        log(&format!("step {}/{total}: pressed {} (optional, press {k})", idx + 1, leaf(expected)));
        set_status(format!("Step {}/{total}: {} ({k})", idx + 1, STEP_LABELS[idx]));
        return; // stay here; a later step's button ends it
    }
    log(&format!("step {}/{total}: pressed {expected}", idx + 1));
    if idx == RACE_DONE_STEP {
        // The race is in the bag here. Counting at Race Again instead would report 0 for a run
        // that raced and then found the bar empty - which is exactly what the first run did.
        let n = LAPS.fetch_add(1, Ordering::Relaxed) + 1;
        let limit = LAP_LIMIT.load(Ordering::Relaxed);
        log(&format!("race {n} finished"));
        step_to(idx + 1);
        if limit > 0 && n >= limit {
            finish("Reached the race limit");
        } else {
            set_status(format!("Race {n} done \u{00b7} starting the next"));
        }
        return;
    }
    if idx == LAST_STEP {
        lap_to(LAP_START);
        set_status("Next race starting\u{2026}".into());
        return;
    }
    set_status(format!("Step {}/{total}: {} done", idx + 1, STEP_LABELS[idx]));
    step_to(idx + 1);
}

// ── main-thread tick ────────────────────────────────────────────────────────────────────────────

/// Called from the TweenManager tick. Handles the start/stop requests, the item-list choice, and
/// the two ways a run ends that no single button press can tell us about.
pub fn pump() {
    if REQ_STOP.swap(false, Ordering::Relaxed) {
        finish("Stopped");
    }
    if REQ_START.swap(false, Ordering::Relaxed) && !RUNNING.load(Ordering::Relaxed) {
        begin();
    }
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    let now = now_ms();
    track_location(now);
    let cur = CURSOR.load(Ordering::Relaxed);

    // Not placed yet: the first screen of the loop that holds still is where we start.
    if cur == UNRESOLVED {
        if let Some(s) = stable_location(now) {
            log(&format!("located: the screen shows step {}/{} ({}) - starting there", s + 1, N, STEP_LABELS[s]));
            lap_to(s);
            set_status(format!("Step {}/{}: {} is up", s + 1, N, STEP_LABELS[s]));
        } else if now >= START_AT.load(Ordering::Relaxed) + WAIT_FOR_SCREEN_MS {
            finish("No Team Trials screen turned up");
        }
        return;
    }

    // Lost the place? The step we want has not been in view for a while, and a screen of the loop
    // has been sitting there the whole time. Go where the screen is. This is what a press the game
    // swallowed looks like from here: the cursor moved on, the screen did not.
    if !expected_in_view(now) && now >= STEP_SINCE.load(Ordering::Relaxed) + RELOCATE_MS {
        if let Some(s) = stable_location(now) {
            if s != cur {
                log(&format!(
                    "lost step {}/{} ({}); the screen shows step {}/{} ({}) - continuing from there",
                    cur + 1,
                    N,
                    STEP_LABELS[cur],
                    s + 1,
                    N,
                    STEP_LABELS[s]
                ));
                relocate_to(s);
                if RELOCATES.load(Ordering::Relaxed) >= MAX_RELOCATES {
                    log(&format!(
                        "stuck at step {}/{} ({}): the screen and the flow have disagreed {} times \
                         running with no progress. Every live button on this screen is listed above.",
                        s + 1,
                        N,
                        STEP_LABELS[s],
                        MAX_RELOCATES
                    ));
                    finish("Stuck - see the log");
                }
                return;
            }
        }
    }

    // Item list: press the leftmost icon once every icon has had a chance to tick.
    if cur == ITEM_STEP {
        // "Use Parfaits" off: race with what the team has. Wait for the icons to tick anyway -
        // that is the proof the dialog has arrived - then move the cursor past them. Race! is in
        // the same dialog, so the ordinary settle-and-press takes it from here.
        if !crate::settings::tt_use_items() {
            let seen = SEEN_AT.load(Ordering::Relaxed);
            if seen != 0 && now >= seen + ITEM_COLLECT_MS {
                log("item list: Use Parfaits is off - racing without one");
                step_to(ITEM_STEP + 1);
            }
            return;
        }
        let seen = SEEN_AT.load(Ordering::Relaxed);
        if seen != 0 && now >= seen + ITEM_COLLECT_MS && now >= LAST_PRESS.load(Ordering::Relaxed) + GAP_MS {
            let mut cands: Vec<(usize, i32)> =
                ITEM_CANDS.lock().ok().and_then(|g| g.clone()).unwrap_or_default();
            if !cands.is_empty() {
                cands.sort_by_key(|(_, sib)| *sib);
                if cands.len() > 1 {
                    log(&format!(
                        "item list: {} items, sibling order {:?} - taking the leftmost",
                        cands.len(),
                        cands.iter().map(|(_, s)| *s).collect::<Vec<_>>()
                    ));
                }
                let target = cands[0].0 as *mut c_void;
                if unsafe { crate::ui_input::click_now(target) } {
                    LAST_PRESS.store(now, Ordering::Relaxed);
                    log(&format!("step {}/{}: pressed the leftmost item", ITEM_STEP + 1, FLOW.len()));
                    set_status(format!("Step {}/{}: {} done", ITEM_STEP + 1, FLOW.len(), STEP_LABELS[ITEM_STEP]));
                    step_to(ITEM_STEP + 1);
                }
            }
        }
    }

    // Out of race points: the grand result has been up a good while, we have stopped pressing
    // things on it, and Race Again has never once gone live. Anything shorter than this catches a
    // score tally still playing and calls a full bar empty.
    if cur >= SKIP_STEP && !RETRY_SEEN.load(Ordering::Relaxed) {
        let t = GRAND_NO_RETRY_AT.load(Ordering::Relaxed);
        let base = t.max(LAST_PRESS.load(Ordering::Relaxed));
        if t != 0 && now >= base + NO_RETRY_MS {
            finish("Out of race points");
            return;
        }
    }

    // A step that never turns up. Log every live button once so the missing press can be encoded,
    // then stop - this run will not press something it does not recognise.
    if now >= PROGRESS_SINCE.load(Ordering::Relaxed) + STALL_MS {
        log(&format!(
            "stalled waiting for step {}/{} ({}). Buttons seen on this screen are logged above.",
            cur + 1,
            FLOW.len(),
            FLOW[cur].0
        ));
        finish("Stalled - see the log");
    }
}

/// Log a button path once per run under `tag`. This is how anything the recording did not
/// contain gets named - the tally skip was found this way - instead of guessed at.
fn learn(tag: &str, path: &str) {
    if let Ok(mut g) = LEARNED.lock() {
        let set = g.get_or_insert_with(HashSet::new);
        if set.len() < 64 && set.insert(path.to_string()) {
            log(&format!("{tag}: {path}"));
        }
    }
}

/// Preview-host design aid: pose the panel mid-run so it can be styled without the game.
pub fn mock_for_preview() {
    if std::env::var_os("TRACKSIDE_TTPLAY_MOCK").is_none() {
        return;
    }
    RUNNING.store(true, Ordering::Relaxed);
    CURSOR.store(5, Ordering::Relaxed);
    LAPS.store(3, Ordering::Relaxed);
    TOP_SEEN_AT.store(now_ms(), Ordering::Relaxed);
    set_status("Step 6/10: see all results done".into());
}

// ── IL2CPP bridge ───────────────────────────────────────────────────────────────────────────────

mod bridge {
    use super::*;
    use crate::il2cpp;
    use crate::pruner::bridge::{invoke0, plain_string};

    fn plausible(p: *mut c_void) -> bool {
        let v = p as usize;
        v >= 0x10000 && v < 0x0000_8000_0000_0000
    }

    /// Full transform path of a component ("Root/Panel/Button"), bounded depth — the identity
    /// record & replay is built on, since button names collide and paths do not.
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

    /// `Transform.GetSiblingIndex()` on the ancestor `levels` above this component's own transform.
    /// Used to tell apart list entries whose paths are identical: in the item list the varying part
    /// is the container's position under Content, two levels above the button.
    ///
    /// Int-returning 0-arg call, read back out of the boxed `System.Int32` — no struct returns, so
    /// no calling-convention guesswork.
    pub unsafe fn sibling_index_up(component: *mut c_void, levels: usize) -> Option<i32> {
        if !plausible(component) {
            return None;
        }
        let mut tf = invoke0(component, il2cpp::object_class(component), "get_transform");
        for _ in 0..levels {
            if !plausible(tf) {
                return None;
            }
            tf = invoke0(tf, il2cpp::object_class(tf), "get_parent");
        }
        if !plausible(tf) {
            return None;
        }
        let k = il2cpp::object_class(tf);
        let m = il2cpp::method(k, "GetSiblingIndex", 0);
        if m.is_null() {
            return None;
        }
        let (r, exc) = il2cpp::runtime_invoke_exc(m, tf, &mut []);
        if !exc.is_null() || r.is_null() {
            return None;
        }
        Some(*((r as usize + 0x10) as *const i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every test drives the same module-level state, so they take turns.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn start_at(step: usize) {
        HIGH_WATER.store(0, Ordering::Relaxed);
        RELOCATES.store(0, Ordering::Relaxed);
        lap_to(step);
    }

    /// The 2026-09-15 log: the item step advanced to Race!, Race! never turned up, the run
    /// relocated back to the item step and pressed it again - ~60 times across four minutes.
    /// Each re-press used to count as progress, which reset the clock the learn log (6 s) and the
    /// stall timeout (90 s) both hang off, so neither ever fired and the run never gave up.
    #[test]
    fn an_item_ping_pong_makes_no_progress_and_ends_the_run() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        start_at(LAP_START);
        for s in LAP_START..=ITEM_STEP {
            step_to(s);
        }
        let clock = PROGRESS_SINCE.load(Ordering::Relaxed);
        assert_eq!(RELOCATES.load(Ordering::Relaxed), 0);

        for i in 1..=MAX_RELOCATES {
            step_to(ITEM_STEP + 1); // the re-press: a step already reached, so not progress
            relocate_to(ITEM_STEP); // Race! never showed; back where the screen says we are
            assert_eq!(RELOCATES.load(Ordering::Relaxed), i, "relocation {i} must be counted");
            assert_eq!(
                PROGRESS_SINCE.load(Ordering::Relaxed),
                clock,
                "the no-progress clock must not restart on a re-press"
            );
        }
        assert!(
            RELOCATES.load(Ordering::Relaxed) >= MAX_RELOCATES,
            "four fruitless relocations must end the run"
        );
    }

    /// One swallowed press is ordinary. Relocating once and then getting past the step clears the
    /// count, so a lap that stumbles does not spend its budget.
    #[test]
    fn one_swallowed_press_recovers() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        start_at(LAP_START);
        for s in LAP_START..=ITEM_STEP {
            step_to(s);
        }
        step_to(ITEM_STEP + 1);
        relocate_to(ITEM_STEP);
        assert_eq!(RELOCATES.load(Ordering::Relaxed), 1);
        step_to(ITEM_STEP + 1); // the re-press, still not progress
        assert_eq!(RELOCATES.load(Ordering::Relaxed), 1);
        step_to(ITEM_STEP + 2); // past the high-water mark: real progress
        assert_eq!(RELOCATES.load(Ordering::Relaxed), 0, "progress forgives the relocation");
    }

    /// The high-water mark is per lap. Without the reset the second lap could never progress,
    /// because every step of it sits behind the first lap's mark.
    #[test]
    fn a_new_lap_starts_the_high_water_mark_again() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        start_at(LAP_START);
        for s in LAP_START..N {
            step_to(s);
        }
        assert_eq!(HIGH_WATER.load(Ordering::Relaxed), N - 1);
        lap_to(LAP_START);
        assert_eq!(HIGH_WATER.load(Ordering::Relaxed), LAP_START);
        relocate_to(LAP_START + 1);
        assert_eq!(RELOCATES.load(Ordering::Relaxed), 1);
        step_to(LAP_START + 2);
        assert_eq!(RELOCATES.load(Ordering::Relaxed), 0, "the new lap must be able to progress");
    }
}
