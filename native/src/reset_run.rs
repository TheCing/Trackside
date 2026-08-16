//! Reset Run — give up the current career and go straight into the next one, in one click.
//!
//! UI-DRIVEN, not request-driven - a deliberate choice, matching how `roomfinder` handles joining a
//! room: drive the game's own buttons rather than hand-building a request. `single_mode` does expose
//! `finish`/`start` verbs (docs-internal/game-methods.md), but firing `finish` blind sends a
//! DESTRUCTIVE server call with a payload we guessed, against a career that cannot be recovered.
//! Room Finder already refuses to auto-send room entry for exactly this reason. Driving the game's
//! own flow means the game validates every step and career state cannot be corrupted.
//!
//! THREADING, the rule that keeps biting: the overlay button runs on the RENDER thread and only sets
//! a flag via [`request`]. Every managed call happens in [`poll`], driven on the game's MAIN thread
//! by the single `TweenManager.Update` detour (`ui_tempo`), same as `reset::poll`. Never IL2CPP from
//! the render thread — see [`crate::reset`] and the render-thread rule in the internal docs.
//!
//! ## What the discovery scan found (2026-08-03, 204 candidate classes)
//!
//! The game calls it RETIRE, not "give up" - which is why the first needle list nearly missed it.
//! The entire flow lives on ONE class, and the compiler-generated names spell out the sequence:
//!
//! ```text
//! Gallop.DialogSingleModeTopMenu:
//!     IsDisableRetireButton/0                    - precondition: is retiring even allowed now
//!     OnClickRetireButton/0                      - entry point (0-arg instance method)
//!     <OnClickRetireButton>b__23_0/0             - the closure the confirm dialog's Yes invokes
//!     <OnClickRetireButton>g__ReturnToHome|23_1/0 - the post-retire transition
//! ```
//!
//! `Gallop.DialogDecideRetire` also exists but exposes NO methods of its own - it is a generic
//! confirm driven by callbacks, which is why `b__23_0` is where the actual work happens. NOTE: it is
//! also NOT what the visible "Give Up" dialog instantiates - see "Confirming" below.
//!
//! `g__ReturnToHome` answers the second half of the feature: after retiring, the game returns to
//! Home by itself, so "then loads up the next" needs no separate drive step.
//!
//! ## Chosen approach: drive the VISIBLE flow
//!
//! Invoke `OnClickRetireButton`, let the game raise `DialogDecideRetire`, then click its Yes.
//! NOT `b__23_0` directly, for two reasons:
//!   * it skips the game's own confirmation on an irreversible action;
//!   * `b__23_0` is a COMPILER-GENERATED name. A game patch renumbers those silently, and we would
//!     be invoking whatever inherited the number - on a method that ends someone's career.
//! `OnClickRetireButton` and `IsDisableRetireButton` are real declared names and stable.
//!
//! ## Getting the live instance: FindObjectsOfType, not a setup hook
//!
//! `OnClickRetireButton` is an INSTANCE method, so it needs a live `DialogSingleModeTopMenu`, which
//! exists only while that menu is open. The codebase's usual answer is to hook the dialog's own
//! setup and stash `this` (`skill_buyer`, `roomfinder`) - but that requires knowing the setup
//! method's NAME, and the discovery scan filtered method names to retire/click/decide, so it never
//! told us one. Guessing between Setup/Awake/Initialize/OnOpen would be a coin flip that fails
//! silently.
//!
//! Instead: `UnityEngine.Object.FindObjectsOfType(typeof(DialogSingleModeTopMenu))`, the same call
//! `glasses` already uses. It needs no name, and it returns empty unless the menu is open - which is
//! exactly the precondition we wanted anyway, so the "is the menu open?" check and the "get the
//! instance" step collapse into one.
//!
//! ## What this drives, and what it deliberately does NOT
//!
//! It opens the game's own retire confirmation. It does NOT press that dialog's Yes. Pressing Yes
//! means invoking `<OnClickRetireButton>b__23_0`, and that name is COMPILER-GENERATED: a game patch
//! renumbers those silently, and we would be blind-invoking whatever inherited `23_0` on a call that
//! permanently ends a career. Wrong-method-with-the-right-name is not a risk worth taking for one
//! saved click. The safe route to full automation is driving that dialog's button at the UI level.
//!
//! ## The two corrections that made it one click
//!
//! 1. The confirm is `Gallop.DialogSingleModeDeleteConfirm`, NOT `DialogDecideRetire`. The latter
//!    exists as a type but is never instantiated, so waiting for a live one reported "no
//!    confirmation appeared" while the Give Up dialog was plainly on screen. Probing which dialog
//!    classes actually had live instances after a click named the real one.
//! 2. `DialogSingleModeTopMenu` has a STATIC `Open`, so the menu no longer has to be open already.
//!    The original discovery scan filtered method names to retire/click/decide, which is why it
//!    stayed hidden - the fix was to dump the class unfiltered.
//!
//! ## Driving it
//!
//! `poll` walks OPEN_MENU -> WAIT_MENU -> CLICK_RETIRE -> WAIT_CONFIRM -> CLICK_GIVE_UP, one step
//! per tick. Each wait is bounded ([`WAIT_TICKS`]); if the menu is already open the first stage is
//! skipped.
//!
//! Two guards keep this from becoming a blind invoke on an irreversible action:
//!   * [`choose_open`] only accepts a STATIC overload whose parameters are all reference types, so
//!     passing null is legal. A bool/int/enum parameter would be garbage read from null, so those
//!     overloads are refused rather than guessed at.
//!   * [`choose_give_up`] matches the confirm button against REAL declared names only
//!     ([`GIVE_UP_NAMES`]), never a compiler-generated closure like `<OnClickRetireButton>b__23_0`,
//!     whose number a patch renumbers silently.
//!
//! Every failure degrades to the previous behaviour - the dialog is left up and the player presses
//! Give Up - rather than pressing something we cannot name.
//!
//! Post-retire needs no drive step: `g__ReturnToHome` means the game returns to Home by itself.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::OnceLock;

use crate::il2cpp;

/// Set by the overlay (render thread), consumed by `poll` (main thread).
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// One-shot screen probe, for building the "start the next career" flow.
///
/// Every step of the retire work was blocked until a probe named the real class - DialogDecideRetire
/// was wrong, the confirm had no handlers, the opener was filtered out of the first scan. Rather
/// than guess five more screens one round trip at a time, this dumps whatever screen is currently
/// live on demand. Press it on each screen; the log then has the whole flow.
static PROBE: AtomicBool = AtomicBool::new(false);

/// Where the driver is. One step per `poll` tick - never a blocking burst.
mod stage {
    pub const IDLE: u8 = 0;
    /// Menu not open yet: invoke the opener and wait for it.
    pub const OPEN_MENU: u8 = 1;
    pub const WAIT_MENU: u8 = 2;
    /// Gate, then click Retire.
    pub const CLICK_RETIRE: u8 = 3;
    pub const WAIT_CONFIRM: u8 = 4;
    /// Press the confirm dialog's Give Up.
    pub const CLICK_GIVE_UP: u8 = 5;
    /// Retired; waiting for the player to press Career on the main menu (its class is still
    /// unknown - the one probe that caught the home screen had only the hamburger menu open).
    pub const WAIT_WIZARD: u8 = 6;
    /// Wizard open: click the ACTIVE step's Next until the deck screen.
    pub const ADVANCE: u8 = 7;
    /// Next clicked; wait for `_currentStepUI` to actually change before clicking again.
    pub const ADVANCE_WAIT: u8 = 8;
    /// Deck screen: waiting for the player to open the Friends slot (opener also unknown -
    /// PartsSupportCardDeck never matched a probe needle; fixed now, data pending).
    pub const WAIT_FRIEND_DIALOG: u8 = 9;
    /// Friend dialog open: find the remembered card and click it.
    pub const PICK_FRIEND: u8 = 10;
    /// Card picked; wait for the dialog to close, then Next once more.
    pub const AFTER_PICK: u8 = 11;
    /// Wait for the start-confirmation dialog, then STOP - never past it.
    pub const WAIT_START_CONFIRM: u8 = 12;
}
static STAGE: AtomicU8 = AtomicU8::new(stage::IDLE);
/// The step object Next was clicked on, so ADVANCE_WAIT can tell a real swap from no-op.
static ADV_FROM: AtomicUsize = AtomicUsize::new(0);
static ADV_COUNT: AtomicU32 = AtomicU32::new(0);
/// One-shot guards for the two auto-opened hops: each opener is clicked at most twice per flow
/// (one retry for a loading hiccup), never every tick - a click-per-tick loop against a slow
/// screen transition would queue dozens of navigations.
static WIZ_CLICKS: AtomicU32 = AtomicU32::new(0);
static FRIEND_CLICKS: AtomicU32 = AtomicU32::new(0);
/// Legacy restore is one-shot per flow: applying it again after the player has adjusted the picks
/// would silently overwrite their choice.
static LEGACY_APPLIED: AtomicU32 = AtomicU32::new(0);

/// `poll` runs about once per frame, so these are a couple of seconds each - long enough for a
/// dialog's open animation, short enough not to sit there.
const WAIT_TICKS: u32 = 180;
/// Waits that depend on the PLAYER doing something (press Career, open the Friends slot) get a
/// minute, not three seconds.
const USER_WAIT_TICKS: u32 = 3600;
static WAITED: AtomicU32 = AtomicU32::new(0);

/// Real, declared names the confirm dialog's positive button could plausibly use.
///
/// Deliberately EXCLUDES compiler-generated names (`<...>b__23_0`): a patch renumbers those
/// silently and we would be blind-invoking whatever inherited the number, on a call that ends a
/// career for good. If none match, the driver stops and logs rather than guessing.
const GIVE_UP_NAMES: &[&str] = &[
    "OnClickDecideButton",
    "OnClickSubmitButton",
    "OnClickYesButton",
    "OnClickOkButton",
    "OnClickDeleteButton",
    "OnClickGiveUpButton",
    "OnClickRetireButton",
    "OnClickPositiveButton",
    "OnClickRightButton",
];

struct Api {
    menu: il2cpp::Class,
    click: il2cpp::Method,
    gate: il2cpp::Method,
    find_objects: il2cpp::Method,
    ty_menu: il2cpp::Object,
    /// The confirm the game ACTUALLY shows - `DialogSingleModeDeleteConfirm`, not
    /// `DialogDecideRetire`, which never gets instantiated. Established by probing live objects.
    confirm: il2cpp::Class,
    ty_confirm: il2cpp::Object,
    /// The confirm's positive button, matched from [`GIVE_UP_NAMES`]. Null means not found; the
    /// driver then leaves the dialog up rather than pressing something it cannot name.
    give_up: il2cpp::Method,
    /// Static `DialogSingleModeTopMenu.Open` overload we can satisfy. Null means we open nothing.
    open: il2cpp::Method,
    open_argc: usize,

    // ── next-career wizard (offsets resolved by NAME at boot, never hardcoded) ──
    /// `SingleModeStartView` - live iff the wizard is open.
    ty_wizard: il2cpp::Object,
    /// Any live `SingleModeStartStepBase` reaches the controller through this field.
    ty_step_base: il2cpp::Object,
    off_step_vc: usize,
    /// On the controller: the ACTIVE step object. The whole reason the wizard is drivable - every
    /// step is live at once, so only the controller knows which `OnClickNextButton` is real.
    off_vc_current_ui: usize,
    /// The start confirmation. Its presence is the STOP signal; we never click into it.
    ty_start_confirm: il2cpp::Object,
    /// Friend-card dialog + items.
    ty_friend_dialog: il2cpp::Object,
    ty_friend_item: il2cpp::Object,
    off_item_info: usize,
    off_fci_viewer: usize,
    off_fci_card: usize,
    off_fci_name: usize,
    /// `HomeTrainingButton` - the home screen's career entry. Its `SetOnGotoSingleModeStartView`
    /// setter names its destination outright, which is as close to documentation as IL2CPP gets.
    ty_home_button: il2cpp::Object,
    /// `PartsSupportCardDeckListItem` - the deck card whose `OnClickFriendButton` opens the
    /// friends dialog.
    ty_deck_item: il2cpp::Object,
    /// `SingleModeStartStepSuccessionSelect.ApplyTempTrainedCharaData/0` - the game's OWN restore
    /// of the last-used legacies (its inner closure is literally `GetSavedTrainedCharaData`).
    /// Preferred over the visible Auto-Select, which picks what the GAME thinks is best rather
    /// than what was chosen last - and Auto-Select routes through `OpenAutoSelectConfirm`, i.e.
    /// another dialog to drive.
    apply_temp_legacy: il2cpp::Method,
    validate_succession: il2cpp::Method,
    update_next_succession: il2cpp::Method,
}
unsafe impl Send for Api {}
unsafe impl Sync for Api {}
static API: OnceLock<Option<Api>> = OnceLock::new();

fn log(msg: &str) {
    crate::tools::log(&format!("[reset-run] {msg}"));
}

/// Last outcome as a code, not a string: the overlay reads this every frame from the render thread,
/// and that path must never take a lock.
pub mod status {
    pub const NONE: u8 = 0;
    pub const NOT_OPEN: u8 = 1;
    pub const BLOCKED: u8 = 2;
    pub const CONFIRM_UP: u8 = 3;
    pub const THREW: u8 = 4;
    pub const UNAVAILABLE: u8 = 6;
    pub const DONE: u8 = 7;
    pub const NO_MENU: u8 = 8;
    pub const PRESS_CAREER: u8 = 9;
    pub const ADVANCING: u8 = 10;
    pub const OPEN_FRIENDS: u8 = 11;
    pub const NO_SAVED_CARD: u8 = 12;
    pub const CARD_NOT_VISIBLE: u8 = 13;
    pub const AT_CONFIRM: u8 = 14;
    pub const STEP_STUCK: u8 = 15;
    pub const NEEDS_CHOICE: u8 = 16;
}
static STATUS: AtomicU8 = AtomicU8::new(status::NONE);

pub fn last_status() -> &'static str {
    match STATUS.load(Ordering::Relaxed) {
        status::NOT_OPEN => "Open the career menu first, then press Reset Run",
        status::BLOCKED => "The game says retiring is not available right now",
        status::CONFIRM_UP => "Confirmation is up - press Give Up",
        status::THREW => "The retire call failed - see trackside-logs",
        status::UNAVAILABLE => "Retire methods unavailable - see trackside-logs",
        status::DONE => "Career given up",
        status::NO_MENU => "Could not open the career menu - open it and press again",
        status::PRESS_CAREER => "Press Career on the main menu to continue",
        status::ADVANCING => "Advancing through career setup...",
        status::OPEN_FRIENDS => "Deck screen - open the Friends slot (card picked automatically)",
        status::NO_SAVED_CARD => "No card remembered yet - pick one to save it for next time",
        status::CARD_NOT_VISIBLE => "Remembered card not on screen - scroll to it or pick manually",
        status::AT_CONFIRM => "Paused at confirmation - review and press Start",
        status::STEP_STUCK => "Setup step did not advance - continue manually",
        status::NEEDS_CHOICE => "This screen needs a choice - pick, then Next (resumes at the deck)",
        _ => "",
    }
}

pub fn clear_status() {
    STATUS.store(status::NONE, Ordering::Relaxed);
}

// ── "same card as last chosen" memory ───────────────────────────────────────
// Captured PASSIVELY: a hook on the friend-card item's OnClickFrame reads `_selectInfo` whenever a
// card is picked by hand, so the memory needs no UI. Identity is (ViewerId, SupportCardId) - the
// probe proved selection passes a FriendCardInfo OBJECT, so matching on identity is exact and list
// order never matters.
static LAST_VIEWER: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static LAST_CARD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static FRAME_TRAMP: AtomicUsize = AtomicUsize::new(0);
static FRAME_DETOUR: OnceLock<retour::RawDetour> = OnceLock::new();

fn friend_file() -> std::path::PathBuf {
    crate::paths::local_file_migrated("trackside_last_friend.json", "trackside_last_friend.json")
}

fn save_friend(viewer: i64, card: i32, name: &str) {
    let j = serde_json::json!({"viewer_id": viewer, "support_card_id": card, "user_name": name});
    let Ok(txt) = serde_json::to_vec_pretty(&j) else { return };
    // Off-thread, same as every other disk write reachable from a hook (render-thread rule).
    std::thread::spawn(move || {
        let _ = std::fs::write(friend_file(), txt);
    });
}

fn load_friend() {
    let Ok(bytes) = std::fs::read(friend_file()) else { return };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return };
    LAST_VIEWER.store(v["viewer_id"].as_i64().unwrap_or(0), Ordering::Relaxed);
    LAST_CARD.store(v["support_card_id"].as_i64().unwrap_or(0) as i32, Ordering::Relaxed);
}

/// OnClickFrame detour: remember the picked card, then let the game do its thing. Main thread
/// (it is a click handler), so reading managed fields here is safe; the file write is off-thread.
unsafe extern "C" fn on_friend_frame(this: *mut c_void, mi: *const c_void) {
    if let Some(Some(api)) = API.get() {
        if api.off_item_info != 0 {
            let info = *((this as usize + api.off_item_info) as *const *mut c_void);
            if !info.is_null() {
                let viewer = *((info as usize + api.off_fci_viewer) as *const i64);
                let card = *((info as usize + api.off_fci_card) as *const i32);
                let name_obj = *((info as usize + api.off_fci_name) as *const il2cpp::Object);
                let name =
                    if name_obj.is_null() { String::new() } else { il2cpp::read_string(name_obj) };
                if card != 0 {
                    LAST_VIEWER.store(viewer, Ordering::Relaxed);
                    LAST_CARD.store(card, Ordering::Relaxed);
                    log(&format!("friend card remembered: {card} ({name})"));
                    save_friend(viewer, card, &name);
                }
            }
        }
    }
    let t = FRAME_TRAMP.load(Ordering::Relaxed);
    if t != 0 {
        let orig: unsafe extern "C" fn(*mut c_void, *const c_void) = std::mem::transmute(t);
        orig(this, mi);
    }
}

/// Resolve a 0-arg method on the object's OWN class, walking up parents. `OnClickNextButton`
/// exists on both the base and several derived steps; invoking the base slot on a derived object
/// would run the wrong override.
fn method_on_instance(obj: *mut c_void, name: &str) -> il2cpp::Method {
    let mut k = il2cpp::object_class(obj as il2cpp::Object);
    while !k.is_null() {
        let m = il2cpp::method(k, name, 0);
        if !m.is_null() {
            return m;
        }
        k = il2cpp::class_parent(k);
    }
    std::ptr::null_mut() as il2cpp::Method
}

/// Every live object of `ty` (bounded). The friend list recycles items through a LoopScroll, so
/// only VISIBLE cards exist as live objects - callers must treat "not found" as "not visible",
/// not "not in the list".
unsafe fn all_of_type(api: &Api, ty: il2cpp::Object) -> Vec<*mut c_void> {
    let mut out = Vec::new();
    if ty.is_null() {
        return out;
    }
    let mut args: [*mut c_void; 1] = [ty as *mut c_void];
    let (arr, exc) = il2cpp::runtime_invoke_exc(api.find_objects, std::ptr::null_mut(), &mut args);
    if !exc.is_null() || arr.is_null() {
        return out;
    }
    let arr = arr as *mut c_void;
    let n = crate::htt_il2cpp::array_len(arr as *mut _);
    for i in 0..n.min(64) {
        let p = *((arr as *const u8).add(0x20 + i * 8) as *const *mut c_void);
        if !p.is_null() {
            out.push(p);
        }
    }
    out
}

/// The wizard controller, reached through any live step's back-pointer (the controller is a plain
/// class - FindObjectsOfType cannot see it, which is why the live probes never listed it).
unsafe fn wizard_controller(api: &Api) -> *mut c_void {
    if api.off_step_vc == 0 {
        return std::ptr::null_mut();
    }
    let step = first_of_type(api, api.ty_step_base);
    if step.is_null() {
        return std::ptr::null_mut();
    }
    *((step as usize + api.off_step_vc) as *const *mut c_void)
}

fn finish(code: u8, msg: &str) {
    log(msg);
    STATUS.store(code, Ordering::Relaxed);
    STAGE.store(stage::IDLE, Ordering::Relaxed);
}

/// First live object of `ty`, or null. Main thread only. 0x20 is the IL2CPP array data offset on
/// x64, same as `glasses`.
unsafe fn first_of_type(api: &Api, ty: il2cpp::Object) -> *mut c_void {
    if ty.is_null() {
        return std::ptr::null_mut();
    }
    let mut args: [*mut c_void; 1] = [ty as *mut c_void];
    let (arr, exc) = il2cpp::runtime_invoke_exc(api.find_objects, std::ptr::null_mut(), &mut args);
    if !exc.is_null() {
        log(&format!("FindObjectsOfType threw {}", il2cpp::object_class_name(exc)));
        return std::ptr::null_mut();
    }
    let arr = arr as *mut c_void;
    if arr.is_null() {
        return std::ptr::null_mut();
    }
    let n = crate::htt_il2cpp::array_len(arr as *mut _);
    if n == 0 || n > 4096 {
        return std::ptr::null_mut();
    }
    *((arr as *const u8).add(0x20) as *const *mut c_void)
}

/// Invoke a 0-arg instance method returning bool. IL2CPP boxes value-type returns; payload at 0x10.
unsafe fn invoke_bool(m: il2cpp::Method, this: *mut c_void) -> Option<bool> {
    let (ret, exc) = il2cpp::runtime_invoke_exc(m, this as il2cpp::Object, &mut []);
    if !exc.is_null() {
        log(&format!("gate threw {}", il2cpp::object_class_name(exc)));
        return None;
    }
    let ret = ret as *mut c_void;
    if ret.is_null() {
        return None;
    }
    Some(*((ret as *const u8).add(0x10)) != 0)
}

/// True if every parameter is a REFERENCE type, i.e. null is a legal argument.
///
/// This is what makes calling `Open` without knowing its semantics safe: a value-type parameter
/// (bool/int/enum) read from a null pointer would be garbage, so those overloads are refused
/// outright rather than fed a guess.
fn all_params_nullable(m: il2cpp::Method) -> bool {
    let ps = il2cpp::method_params_of(m);
    !ps.is_empty()
        && ps.iter().all(|t| {
            t.contains('.') && !t.starts_with("System.Int") && !t.starts_with("System.Boolean")
        })
}

/// Pick a static `Open` overload we can actually satisfy, preferring the fewest arguments.
fn choose_open(menu: il2cpp::Class) -> (il2cpp::Method, usize) {
    for argc in 0..=2usize {
        let m = il2cpp::method(menu, "Open", argc as i32);
        if m.is_null() {
            continue;
        }
        let params = il2cpp::method_params_of(m);
        let stat = il2cpp::method_is_static(m);
        log(&format!("Open/{argc}: static={stat} params={params:?}"));
        if !stat {
            continue; // an instance Open needs the very instance we are trying to create
        }
        if argc == 0 || all_params_nullable(m) {
            return (m, argc);
        }
    }
    (std::ptr::null_mut() as il2cpp::Method, 0)
}

/// Prefix of the closure the confirm's Give Up button invokes.
///
/// The confirm dialog itself has NO handlers - its whole surface is
/// `GetFormType Setup Open .ctor`, i.e. it is opened with a callback and the button just invokes
/// that delegate. So the only thing to press is the delegate, which lives on the MENU as a
/// compiler-generated closure of `OnClickRetireButton`.
///
/// Matching the PREFIX rather than the full `<OnClickRetireButton>b__23_0` is what makes this safe.
/// The suffix is a compiler counter a patch renumbers silently, but the prefix is the real declared
/// method name: any `<OnClickRetireButton>b__*` is by construction a closure of retire and nothing
/// else. `g__ReturnToHome|23_1` is excluded by the `b__` - that is the local function, not the
/// callback. If the match is ever ambiguous we refuse and leave the dialog to the player.
const GIVE_UP_CLOSURE_PREFIX: &str = "<OnClickRetireButton>b__";

/// Find what the Give Up button actually invokes. Returns a method to call on the MENU instance.
fn choose_give_up(menu: il2cpp::Class, confirm: il2cpp::Class) -> il2cpp::Method {
    if !confirm.is_null() {
        let names = il2cpp::class_methods(confirm);
        log(&format!("{} methods: {}", il2cpp::class_full_name(confirm), names.join(" ")));
        // Kept for the day the dialog grows a real named handler - then prefer it.
        for want in GIVE_UP_NAMES {
            let m = il2cpp::method(confirm, want, 0);
            if !m.is_null() {
                log(&format!("give-up button -> {}.{want}", il2cpp::class_full_name(confirm)));
                return m;
            }
        }
    }
    let hits: Vec<String> = il2cpp::class_methods(menu)
        .into_iter()
        .filter(|n| n.starts_with(GIVE_UP_CLOSURE_PREFIX) && n.ends_with("/0"))
        .collect();
    if hits.len() != 1 {
        log(&format!("give-up closure ambiguous ({hits:?}) - leaving the dialog to the player"));
        return std::ptr::null_mut() as il2cpp::Method;
    }
    let name = hits[0].trim_end_matches("/0");
    let m = il2cpp::method(menu, name, 0);
    if m.is_null() {
        log(&format!("give-up closure {name} would not resolve"));
    } else {
        log(&format!("give-up button -> menu.{name}"));
    }
    m
}

/// Resolve everything once. Called at boot; safe to call again.
pub fn install() -> String {
    let mut status = String::new();
    API.get_or_init(|| {
        let menu = il2cpp::class("Gallop.DialogSingleModeTopMenu");
        if menu.is_null() {
            status = "reset run: DialogSingleModeTopMenu not found".into();
            return None;
        }
        // The dialog the game really shows for "Give Up". DialogDecideRetire was the original
        // assumption and never has a live instance - probing live objects named this one.
        let confirm = il2cpp::class("Gallop.DialogSingleModeDeleteConfirm");
        let k_obj = il2cpp::class("UnityEngine.Object");
        // FindObjectsOfType has same-argc GENERIC overloads - match on parameter type, not argc.
        let find_objects = if k_obj.is_null() {
            std::ptr::null_mut() as il2cpp::Method
        } else {
            il2cpp::method_with_param_types(k_obj, "FindObjectsOfType", &["System.Type"])
        };
        let click = il2cpp::method(menu, "OnClickRetireButton", 0);
        let gate = il2cpp::method(menu, "IsDisableRetireButton", 0);
        let (open, open_argc) = choose_open(menu);
        let give_up = choose_give_up(menu, confirm);

        // Wizard metadata, all by name. A game patch that renames any of these downgrades the
        // feature (boot line shows wizard:MISSING) instead of corrupting a run.
        let k_wizard = il2cpp::class("Gallop.SingleModeStartView");
        let k_step_base = il2cpp::class("Gallop.SingleModeStartStepBase");
        let k_vc = il2cpp::class("Gallop.SingleModeStartViewController");
        let k_start_confirm = il2cpp::class("Gallop.DialogSingleModeStartConfirmEntrySelectMode");
        let k_friend_dialog = il2cpp::class("Gallop.DialogSingleModeSelectFriendCard");
        let k_friend_item = il2cpp::class("Gallop.PartsSingleModeSelectFriendCardItem");
        let k_home_button = il2cpp::class("Gallop.HomeTrainingButton");
        let k_deck_item = il2cpp::class("Gallop.PartsSupportCardDeckListItem");
        let k_succ = il2cpp::class("Gallop.SingleModeStartStepSuccessionSelect");
        let off_step_vc = if k_step_base.is_null() {
            0
        } else {
            il2cpp::field_offset(k_step_base, "<ViewController>k__BackingField").unwrap_or(0)
        };
        let off_vc_current_ui = if k_vc.is_null() {
            0
        } else {
            il2cpp::field_offset(k_vc, "_currentStepUI").unwrap_or(0)
        };
        let off_item_info = if k_friend_item.is_null() {
            0
        } else {
            il2cpp::field_offset(k_friend_item, "_selectInfo").unwrap_or(0)
        };
        // FriendCardInfo is nested two deep: ViewController -> EntryInfo -> FriendCardInfo.
        let (mut off_fci_viewer, mut off_fci_card, mut off_fci_name) = (0usize, 0usize, 0usize);
        if !k_vc.is_null() {
            let k_entry = il2cpp::nested_in(k_vc, "EntryInfo");
            if !k_entry.is_null() {
                let k_fci = il2cpp::nested_in(k_entry, "FriendCardInfo");
                if !k_fci.is_null() {
                    off_fci_viewer = il2cpp::field_offset(k_fci, "ViewerId").unwrap_or(0);
                    off_fci_card = il2cpp::field_offset(k_fci, "SupportCardId").unwrap_or(0);
                    off_fci_name = il2cpp::field_offset(k_fci, "UserName").unwrap_or(0);
                }
            }
        }
        // The last-chosen memory hook. Best-effort: without it the memory never updates, but
        // nothing else degrades.
        if !k_friend_item.is_null() && off_item_info != 0 && off_fci_card != 0 {
            match unsafe {
                il2cpp::hook_method(
                    k_friend_item,
                    "OnClickFrame",
                    0,
                    on_friend_frame as *const (),
                    &FRAME_TRAMP,
                    &FRAME_DETOUR,
                )
            } {
                Ok(()) => {}
                Err(e) => log(&format!("OnClickFrame hook FAILED: {e} - card memory disabled")),
            }
        }
        load_friend();
        let wizard_ok = !k_wizard.is_null()
            && !k_step_base.is_null()
            && off_step_vc != 0
            && off_vc_current_ui != 0
            && !k_start_confirm.is_null();
        let card_ok = off_item_info != 0 && off_fci_card != 0 && LAST_CARD.load(Ordering::Relaxed) != 0;
        status = format!(
            "reset run: click:{} gate:{} confirm:{} find:{} open:{} giveup:{} wizard:{} card:{}",
            if click.is_null() { "MISSING" } else { "ok" },
            if gate.is_null() { "MISSING" } else { "ok" },
            if confirm.is_null() { "MISSING" } else { "ok" },
            if find_objects.is_null() { "MISSING" } else { "ok" },
            if open.is_null() { "MISSING" } else { "ok" },
            if give_up.is_null() { "MISSING" } else { "ok" },
            if wizard_ok { "ok" } else { "MISSING" },
            if card_ok { "remembered" } else { "none" },
        );
        status.push_str(&format!(
            " home:{} deck:{} legacy:{}",
            if k_home_button.is_null() { "MISSING" } else { "ok" },
            if k_deck_item.is_null() { "MISSING" } else { "ok" },
            if k_succ.is_null() || il2cpp::method(k_succ, "ApplyTempTrainedCharaData", 0).is_null() {
                "MISSING"
            } else {
                "ok"
            },
        ));
        if click.is_null() || find_objects.is_null() {
            return None;
        }
        Some(Api {
            menu,
            click,
            gate,
            find_objects,
            ty_menu: il2cpp::type_object(menu),
            confirm,
            ty_confirm: if confirm.is_null() {
                std::ptr::null_mut() as il2cpp::Object
            } else {
                il2cpp::type_object(confirm)
            },
            give_up,
            open,
            open_argc,
            ty_wizard: if k_wizard.is_null() {
                std::ptr::null_mut() as il2cpp::Object
            } else {
                il2cpp::type_object(k_wizard)
            },
            ty_step_base: if k_step_base.is_null() {
                std::ptr::null_mut() as il2cpp::Object
            } else {
                il2cpp::type_object(k_step_base)
            },
            off_step_vc,
            off_vc_current_ui,
            ty_start_confirm: if k_start_confirm.is_null() {
                std::ptr::null_mut() as il2cpp::Object
            } else {
                il2cpp::type_object(k_start_confirm)
            },
            ty_friend_dialog: if k_friend_dialog.is_null() {
                std::ptr::null_mut() as il2cpp::Object
            } else {
                il2cpp::type_object(k_friend_dialog)
            },
            ty_friend_item: if k_friend_item.is_null() {
                std::ptr::null_mut() as il2cpp::Object
            } else {
                il2cpp::type_object(k_friend_item)
            },
            off_item_info,
            off_fci_viewer,
            off_fci_card,
            off_fci_name,
            ty_home_button: if k_home_button.is_null() {
                std::ptr::null_mut() as il2cpp::Object
            } else {
                il2cpp::type_object(k_home_button)
            },
            ty_deck_item: if k_deck_item.is_null() {
                std::ptr::null_mut() as il2cpp::Object
            } else {
                il2cpp::type_object(k_deck_item)
            },
            apply_temp_legacy: if k_succ.is_null() {
                std::ptr::null_mut() as il2cpp::Method
            } else {
                il2cpp::method(k_succ, "ApplyTempTrainedCharaData", 0)
            },
            validate_succession: if k_succ.is_null() {
                std::ptr::null_mut() as il2cpp::Method
            } else {
                il2cpp::method(k_succ, "ValidateSuccessionSelect", 0)
            },
            update_next_succession: if k_succ.is_null() {
                std::ptr::null_mut() as il2cpp::Method
            } else {
                il2cpp::method(k_succ, "UpdateNextButton", 0)
            },
        })
    });
    if status.is_empty() {
        "reset run: ready (cached)".into()
    } else {
        status
    }
}

/// Overlay entry point. Render-thread safe: sets a flag and returns.
pub fn request() {
    REQUESTED.store(true, Ordering::Relaxed);
    STATUS.store(status::NONE, Ordering::Relaxed);
    ADV_COUNT.store(0, Ordering::Relaxed);
    WIZ_CLICKS.store(0, Ordering::Relaxed);
    FRIEND_CLICKS.store(0, Ordering::Relaxed);
    LEGACY_APPLIED.store(0, Ordering::Relaxed);
}

/// Overlay entry point for the screen probe. Render-thread safe.
pub fn probe_screen() {
    PROBE.store(true, Ordering::Relaxed);
}

#[cfg(feature = "devtools")]
/// Dump every live UI class and the methods worth clicking. Main thread only, on demand only -
/// FindObjectsOfType walks every live object, so this must never run per-frame.
fn probe_live_screen(api: &Api) {
    let mut seen: Vec<String> = Vec::new();
    for needle in ["Dialog", "ViewController", "SingleMode", "SupportCardDeck", "Home", "Succession"] {
        for (full, k) in il2cpp::find_classes(needle) {
            if seen.contains(&full) {
                continue;
            }
            let ty = il2cpp::type_object(k);
            if ty.is_null() || unsafe { first_of_type(api, ty).is_null() } {
                continue;
            }
            seen.push(full.clone());
            let clicks: Vec<String> = il2cpp::class_methods(k)
                .into_iter()
                .filter(|n| {
                    let l = n.to_ascii_lowercase();
                    l.contains("click") || l.contains("select") || l.contains("decide")
                        || l.contains("start") || l.contains("next") || l.contains("push")
                        || l.contains("auto") || l.contains("validate")
                })
                .collect();
            log(&format!("LIVE {full}: {}", clicks.join(" ")));
            // Fields too, on their own line. Methods alone were not enough: every wizard step is
            // live at once, so the ACTIVE one can only be identified from a state field (expected on
            // SingleModeStartView, which stays live and declares no methods of its own). Field data
            // is also what decides whether "same card as last time" can key on a card id rather than
            // a list index, which would silently pick the wrong card if the list ever reorders.
            let fields: Vec<String> = il2cpp::class_fields(k)
                .into_iter()
                .map(|(n, off, ty)| format!("{n}@{off:#x}:{ty}"))
                .collect();
            if !fields.is_empty() {
                log(&format!("FIELDS {full}: {}", fields.join(" ")));
            }
        }
    }
    // Static shapes, no instance needed. The live sweep established that the wizard's whole state
    // rides in SingleModeStartViewController.EntryInfo and that friend-card selection passes a
    // FriendCardInfo OBJECT (not an index). What is still unknown is the field layout inside those
    // types and the Step enum's values - plain metadata, so dump it directly.
    // The two screens that need a SELECTION before Next is legal. Their handles are not in the
    // filtered live sweep (the game's own Auto-Select is the obvious lever and its method name is
    // unknown), so dump them whole - methods AND fields, unfiltered.
    for name in [
        "Gallop.SingleModeStartStepSuccessionSelect",
        "Gallop.SingleModeStartStepCardSelect",
        "Gallop.PartsSingleModeStartSuccessionSlot",
    ] {
        let k = il2cpp::class(name);
        if k.is_null() {
            continue;
        }
        log(&format!("ALL {name} methods: {}", il2cpp::class_methods(k).join(" ")));
        let f: Vec<String> = il2cpp::class_fields(k)
            .into_iter()
            .map(|(n, off, ty)| format!("{n}@{off:#x}:{ty}"))
            .collect();
        log(&format!("ALL {name} fields: {}", f.join(" ")));
    }
    for name in ["Gallop.SingleModeStartViewController"] {
        let k = il2cpp::class(name);
        if k.is_null() {
            continue;
        }
        let f: Vec<String> = il2cpp::class_fields(k)
            .into_iter()
            .map(|(n, off, ty)| format!("{n}@{off:#x}:{ty}"))
            .collect();
        log(&format!("SHAPE {name}: {}", f.join(" ")));
        for nested in ["EntryInfo"] {
            let nk = il2cpp::nested_in(k, nested);
            if nk.is_null() {
                continue;
            }
            let f: Vec<String> = il2cpp::class_fields(nk)
                .into_iter()
                .map(|(n, off, ty)| format!("{n}@{off:#x}:{ty}"))
                .collect();
            log(&format!("SHAPE {name}.{nested}: {}", f.join(" ")));
            let fk = il2cpp::nested_in(nk, "FriendCardInfo");
            if !fk.is_null() {
                let f: Vec<String> = il2cpp::class_fields(fk)
                    .into_iter()
                    .map(|(n, off, ty)| format!("{n}@{off:#x}:{ty}"))
                    .collect();
                log(&format!("SHAPE {name}.{nested}.FriendCardInfo: {}", f.join(" ")));
            }
        }
    }
    let step_enum = il2cpp::nested_class("Gallop.SingleModeStartView", "Step");
    if !step_enum.is_null() {
        let c: Vec<String> = il2cpp::enum_constants(step_enum)
            .into_iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect();
        log(&format!("SHAPE SingleModeStartView.Step: {}", c.join(" ")));
    }
    log(&format!("screen probe done - {} live classes", seen.len()));
}

/// True while a reset is pending, so the button can show progress.
pub fn is_pending() -> bool {
    REQUESTED.load(Ordering::Relaxed) || STAGE.load(Ordering::Relaxed) != stage::IDLE
}

fn go(next: u8) {
    WAITED.store(0, Ordering::Relaxed);
    STAGE.store(next, Ordering::Relaxed);
}

/// True once the wait budget is spent.
fn waited_out() -> bool {
    WAITED.fetch_add(1, Ordering::Relaxed) >= WAIT_TICKS
}

/// Main-thread step, driven from the `TweenManager.Update` detour.
pub fn poll() {
    if REQUESTED.swap(false, Ordering::Relaxed) {
        go(stage::OPEN_MENU);
    }
    let probe = PROBE.swap(false, Ordering::Relaxed);
    let st = STAGE.load(Ordering::Relaxed);
    if st == stage::IDLE && !probe {
        return;
    }
    if !il2cpp::ready() {
        finish(status::UNAVAILABLE, "IL2CPP not ready - ignored");
        return;
    }
    let Some(api) = API.get_or_init(|| None).as_ref() else {
        finish(status::UNAVAILABLE, "retire methods unavailable - see the boot line");
        return;
    };

    #[cfg(feature = "devtools")]
    if probe {
        crate::crashlog::step("reset-run:probe");
        probe_live_screen(api);
        if st == stage::IDLE {
            return;
        }
    }

    crate::crashlog::step("reset-run:poll");
    unsafe {
        match st {
            stage::OPEN_MENU => {
                if !first_of_type(api, api.ty_menu).is_null() {
                    go(stage::CLICK_RETIRE); // already open, nothing to do
                    return;
                }
                if api.open.is_null() {
                    finish(status::NOT_OPEN, "no usable Open overload - open the career menu first");
                    return;
                }
                let mut args: [*mut c_void; 2] = [std::ptr::null_mut(); 2];
                let (_, exc) = il2cpp::runtime_invoke_exc(
                    api.open,
                    std::ptr::null_mut(),
                    &mut args[..api.open_argc],
                );
                if !exc.is_null() {
                    finish(
                        status::NO_MENU,
                        &format!("Open threw {}", il2cpp::object_class_name(exc)),
                    );
                    return;
                }
                log("career menu opening");
                go(stage::WAIT_MENU);
            }

            stage::WAIT_MENU => {
                if !first_of_type(api, api.ty_menu).is_null() {
                    go(stage::CLICK_RETIRE);
                } else if waited_out() {
                    finish(status::NO_MENU, "career menu did not appear");
                }
            }

            stage::CLICK_RETIRE => {
                let menu = first_of_type(api, api.ty_menu);
                if menu.is_null() {
                    finish(status::NOT_OPEN, "career menu vanished before the retire click");
                    return;
                }
                // Ask the game whether retiring is allowed rather than assuming. A missing gate is
                // not fatal: the game re-validates on its own confirm anyway.
                if !api.gate.is_null() {
                    if let Some(true) = invoke_bool(api.gate, menu) {
                        finish(status::BLOCKED, "the game says retiring is not available right now");
                        return;
                    }
                }
                let (_, exc) = il2cpp::runtime_invoke_exc(api.click, menu as il2cpp::Object, &mut []);
                if !exc.is_null() {
                    finish(
                        status::THREW,
                        &format!("OnClickRetireButton threw {}", il2cpp::object_class_name(exc)),
                    );
                    return;
                }
                log("retire clicked");
                go(stage::WAIT_CONFIRM);
            }

            stage::WAIT_CONFIRM => {
                // A clean return from the click already means the dialog is up; this wait exists
                // only to get the INSTANCE so its button can be pressed. A timeout here is therefore
                // not a failure - it just means the player finishes the last click, as before.
                if !first_of_type(api, api.ty_confirm).is_null() {
                    go(stage::CLICK_GIVE_UP);
                } else if waited_out() {
                    finish(status::CONFIRM_UP, "confirm instance not found - press Give Up yourself");
                }
            }

            stage::CLICK_GIVE_UP => {
                if api.give_up.is_null() {
                    finish(status::CONFIRM_UP, "no named give-up button - press Give Up yourself");
                    return;
                }
                // Invoked on the MENU: the callback is the menu's closure, not a dialog method.
                // We still waited for the confirm to exist first, so the game is in exactly the
                // state it would be in had the player pressed the button themselves.
                let menu = first_of_type(api, api.ty_menu);
                if menu.is_null() {
                    finish(status::CONFIRM_UP, "menu vanished - press Give Up yourself");
                    return;
                }
                let (_, exc) =
                    il2cpp::runtime_invoke_exc(api.give_up, menu as il2cpp::Object, &mut []);
                if !exc.is_null() {
                    finish(
                        status::CONFIRM_UP,
                        &format!(
                            "give-up click threw {} - press Give Up yourself",
                            il2cpp::object_class_name(exc)
                        ),
                    );
                    return;
                }
                log("gave up - continuing to career setup");
                STATUS.store(status::PRESS_CAREER, Ordering::Relaxed);
                go(stage::WAIT_WIZARD);
            }

            stage::WAIT_WIZARD => {
                if !first_of_type(api, api.ty_wizard).is_null()
                    && !wizard_controller(api).is_null()
                {
                    STATUS.store(status::ADVANCING, Ordering::Relaxed);
                    go(stage::ADVANCE);
                    return;
                }
                // Home probe named the career entry: HomeTrainingButton.OnClick. Click it when it
                // shows up (home takes a few seconds to load after a retire); at most twice, then
                // fall back to asking the player. The wizard-open check above stays authoritative -
                // probes showed wizard objects do NOT linger at home, so it cannot false-positive.
                let ticks = WAITED.fetch_add(1, Ordering::Relaxed);
                if ticks % 60 == 0 && WIZ_CLICKS.load(Ordering::Relaxed) < 2 {
                    let btn = first_of_type(api, api.ty_home_button);
                    if !btn.is_null() {
                        let m = method_on_instance(btn, "OnClick");
                        if !m.is_null() {
                            let (_, exc) =
                                il2cpp::runtime_invoke_exc(m, btn as il2cpp::Object, &mut []);
                            if exc.is_null() {
                                WIZ_CLICKS.fetch_add(1, Ordering::Relaxed);
                                log("career button clicked");
                            } else {
                                log(&format!(
                                    "career button threw {}",
                                    il2cpp::object_class_name(exc)
                                ));
                            }
                        }
                    }
                }
                if ticks >= USER_WAIT_TICKS {
                    finish(
                        status::PRESS_CAREER,
                        "career setup did not open - press Career yourself",
                    );
                }
            }

            stage::ADVANCE => {
                let vc = wizard_controller(api);
                if vc.is_null() {
                    finish(status::STEP_STUCK, "wizard controller vanished");
                    return;
                }
                let cur = *((vc as usize + api.off_vc_current_ui) as *const *mut c_void);
                if cur.is_null() {
                    if waited_out() {
                        finish(status::STEP_STUCK, "no active step");
                    }
                    return;
                }
                let cls = il2cpp::object_class_name(cur as il2cpp::Object);
                if cls.contains("EquipSelect") {
                    // Deck screen. The Friends-slot opener is the other unknown, so this hop is
                    // manual too; the friend DIALOG onward is automated again.
                    STATUS.store(status::OPEN_FRIENDS, Ordering::Relaxed);
                    go(stage::WAIT_FRIEND_DIALOG);
                    return;
                }
                // Legacy screen: restore the last-used legacies through the game's own path
                // before pressing Next, otherwise Next throws on the empty slots. Done ONCE per
                // flow - re-applying after the player has adjusted the picks would overwrite them.
                if cls.contains("SuccessionSelect")
                    && !api.apply_temp_legacy.is_null()
                    && LEGACY_APPLIED.fetch_add(1, Ordering::Relaxed) == 0
                {
                    let (_, exc) = il2cpp::runtime_invoke_exc(
                        api.apply_temp_legacy,
                        cur as il2cpp::Object,
                        &mut [],
                    );
                    if exc.is_null() {
                        // Let the step re-derive validity and the Next button's state from the
                        // restored picks, exactly as the game does after a manual pick.
                        for m in [api.validate_succession, api.update_next_succession] {
                            if !m.is_null() {
                                let _ = il2cpp::runtime_invoke_exc(m, cur as il2cpp::Object, &mut []);
                            }
                        }
                        log("legacies restored from the game's saved selection");
                        // Give the restore a tick to land before Next; the click retries next pass.
                        ADV_FROM.store(cur as usize, Ordering::Relaxed);
                        go(stage::ADVANCE);
                        return;
                    }
                    log(&format!(
                        "legacy restore threw {} - leaving the choice to the player",
                        il2cpp::object_class_name(exc)
                    ));
                }
                let next = method_on_instance(cur, "OnClickNextButton");
                if next.is_null() {
                    finish(status::STEP_STUCK, &format!("{cls} has no OnClickNextButton"));
                    return;
                }
                let (_, exc) = il2cpp::runtime_invoke_exc(next, cur as il2cpp::Object, &mut []);
                if !exc.is_null() {
                    // Uma select and legacy select REQUIRE a selection before Next is legal - it
                    // throws ArgumentNullException on an empty slot. That is the screen telling us
                    // it wants input, not a fault: park in the wait stage so the player chooses and
                    // presses Next, and the driver picks the flow back up at the deck. Previously
                    // this killed the whole flow at the first such screen.
                    log(&format!("{cls} needs a choice ({})", il2cpp::object_class_name(exc)));
                    STATUS.store(status::NEEDS_CHOICE, Ordering::Relaxed);
                    go(stage::ADVANCE_WAIT);
                    return;
                }
                log(&format!("next: {cls}"));
                ADV_FROM.store(cur as usize, Ordering::Relaxed);
                go(stage::ADVANCE_WAIT);
            }

            stage::ADVANCE_WAIT => {
                // Self-verifying: the click only counts once the controller swaps the active step.
                // A dialog in the way (TP recovery, scenario notice) keeps the step unchanged, and
                // that must NOT trigger another blind Next into whatever is on screen.
                let vc = wizard_controller(api);
                if vc.is_null() {
                    finish(status::STEP_STUCK, "wizard controller vanished mid-advance");
                    return;
                }
                let cur = *((vc as usize + api.off_vc_current_ui) as *const *mut c_void);
                if !cur.is_null() && cur as usize != ADV_FROM.load(Ordering::Relaxed) {
                    let n = ADV_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    if n >= 8 {
                        finish(status::STEP_STUCK, "too many step advances - stopping");
                        return;
                    }
                    STATUS.store(status::ADVANCING, Ordering::Relaxed);
                    go(stage::ADVANCE);
                    return;
                }
                // A screen waiting on the PLAYER gets the long budget; an ordinary advance keeps
                // the short one, so a genuinely stuck step still reports quickly.
                let budget = if STATUS.load(Ordering::Relaxed) == status::NEEDS_CHOICE {
                    USER_WAIT_TICKS
                } else {
                    WAIT_TICKS
                };
                if WAITED.fetch_add(1, Ordering::Relaxed) >= budget {
                    finish(status::STEP_STUCK, "step did not advance (a dialog may be up)");
                }
            }

            stage::WAIT_FRIEND_DIALOG => {
                if !first_of_type(api, api.ty_friend_dialog).is_null() {
                    go(stage::PICK_FRIEND);
                    return;
                }
                // Deck probe named the opener: the deck item's OnClickFriendButton. Only clicked
                // when EXACTLY ONE deck item is live - the scroll instantiates just the current
                // deck (probe measured 1), and if a future patch keeps neighbours alive too,
                // clicking "the first one" could act on the WRONG DECK. Ambiguity degrades to the
                // manual hop instead.
                let ticks = WAITED.fetch_add(1, Ordering::Relaxed);
                if ticks % 60 == 0 && FRIEND_CLICKS.load(Ordering::Relaxed) < 2 {
                    let items = all_of_type(api, api.ty_deck_item);
                    if items.len() == 1 {
                        let m = method_on_instance(items[0], "OnClickFriendButton");
                        if !m.is_null() {
                            let (_, exc) =
                                il2cpp::runtime_invoke_exc(m, items[0] as il2cpp::Object, &mut []);
                            if exc.is_null() {
                                FRIEND_CLICKS.fetch_add(1, Ordering::Relaxed);
                                log("friends slot clicked");
                            } else {
                                log(&format!(
                                    "friends slot threw {}",
                                    il2cpp::object_class_name(exc)
                                ));
                            }
                        }
                    } else if !items.is_empty() {
                        log(&format!(
                            "{} deck items live - ambiguous, leaving the slot to the player",
                            items.len()
                        ));
                    }
                }
                if ticks >= USER_WAIT_TICKS {
                    finish(status::OPEN_FRIENDS, "friends slot not opened - finish setup manually");
                }
            }

            stage::PICK_FRIEND => {
                let want_card = LAST_CARD.load(Ordering::Relaxed);
                let want_viewer = LAST_VIEWER.load(Ordering::Relaxed);
                if want_card == 0 {
                    finish(status::NO_SAVED_CARD, "no remembered friend card");
                    return;
                }
                let mut clicked = false;
                for item in all_of_type(api, api.ty_friend_item) {
                    let info = *((item as usize + api.off_item_info) as *const *mut c_void);
                    if info.is_null() {
                        continue;
                    }
                    let viewer = *((info as usize + api.off_fci_viewer) as *const i64);
                    let card = *((info as usize + api.off_fci_card) as *const i32);
                    if card == want_card && viewer == want_viewer {
                        let m = method_on_instance(item, "OnClickFrame");
                        if m.is_null() {
                            break;
                        }
                        let (_, exc) =
                            il2cpp::runtime_invoke_exc(m, item as il2cpp::Object, &mut []);
                        if exc.is_null() {
                            clicked = true;
                            log(&format!("picked remembered friend card {card}"));
                        }
                        break;
                    }
                }
                if clicked {
                    go(stage::AFTER_PICK);
                } else {
                    // LoopScroll recycles items - only visible cards are live. Not-found means
                    // not-on-screen, and auto-scrolling an unknown widget is a guess too far.
                    finish(status::CARD_NOT_VISIBLE, "remembered card not among visible items");
                }
            }

            stage::AFTER_PICK => {
                // The pick closes the dialog game-side; then one more Next reaches confirmation.
                if !first_of_type(api, api.ty_friend_dialog).is_null() {
                    if waited_out() {
                        finish(status::CARD_NOT_VISIBLE, "friend dialog did not close after pick");
                    }
                    return;
                }
                let vc = wizard_controller(api);
                if vc.is_null() {
                    finish(status::STEP_STUCK, "wizard controller vanished after pick");
                    return;
                }
                let cur = *((vc as usize + api.off_vc_current_ui) as *const *mut c_void);
                if cur.is_null() {
                    finish(status::STEP_STUCK, "no active step after pick");
                    return;
                }
                let next = method_on_instance(cur, "OnClickNextButton");
                if next.is_null() {
                    finish(status::STEP_STUCK, "deck step has no OnClickNextButton");
                    return;
                }
                let (_, exc) = il2cpp::runtime_invoke_exc(next, cur as il2cpp::Object, &mut []);
                if !exc.is_null() {
                    finish(
                        status::STEP_STUCK,
                        &format!("deck Next threw {}", il2cpp::object_class_name(exc)),
                    );
                    return;
                }
                go(stage::WAIT_START_CONFIRM);
            }

            stage::WAIT_START_CONFIRM => {
                // THE STOP. The confirmation exposes StartSingleMode one call away; this driver
                // must never be the thing that presses it. Its appearance ends the flow.
                if !first_of_type(api, api.ty_start_confirm).is_null() {
                    finish(status::AT_CONFIRM, "paused at start confirmation");
                } else if waited_out() {
                    finish(status::STEP_STUCK, "start confirmation did not appear");
                }
            }

            _ => STAGE.store(stage::IDLE, Ordering::Relaxed),
        }
    }
}
