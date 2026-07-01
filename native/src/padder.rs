//! padder — Team Trials "deck profiles": snapshot the current 15-Uma team and swap it back
//! with one click. Built for competitive TT play (good team <-> padding team), with up to a
//! handful of renameable profiles persisted next to the DLL.
//!
//! Design (see _research/team-trials, local-only):
//!  - A profile pins each of the 15 slots by `trained_chara_id` (TrainedCharaData._id) — a STABLE
//!    per-graduated-uma id. So a profile survives inventory reordering and pins the exact instance
//!    even when the player owns duplicate characters. (Inventory POSITION is never stored.)
//!  - Snapshot = read the live deck (WorkDataManager -> WorkTeamStadiumData -> TeamStadiumDeckInfo
//!    -> member list) and store {distance_type, member_id, trained_chara_id, running_style} x15.
//!  - Apply = send a TeamStadiumTeamEditRequest with the stored array (same request the game's own
//!    "save deck" button sends — so no new ban surface). Validated on apply: every stored id must
//!    still exist in the roster, or we refuse to send a corrupt deck.
//!
//! The IL2CPP boundary (live read + send) lives in `il2cpp_bridge` below and is resolved by NAME
//! at runtime (robust across game updates). Everything above it is plain Rust + JSON and unit-safe.

#![allow(static_mut_refs)]
// `count`/`overwrite` are public API kept for the UI (re-snapshot / badge) — wired incrementally.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// Install the deck-builder capture hooks (call from boot, on the IL2CPP-attached thread).
pub fn install() -> String {
    il2cpp_bridge::install()
}

/// True while the TT team-edit screen is open (we can drive its grid).
pub fn edit_screen_open() -> bool {
    il2cpp_bridge::edit_screen_open()
}

/// One of the 15 team slots, pinned by the stable trained-uma id.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Slot {
    /// TT distance category (TeamStadiumTeamData.distance_type / MemberInfo.RaceNumber).
    pub distance_type: i32,
    /// Slot index within the category (TeamStadiumTeamData.member_id / MemberInfo.MemberId).
    pub member_id: i32,
    /// STABLE trained-uma id (TrainedCharaData._id). The whole point of profile robustness.
    pub trained_chara_id: i32,
    /// Running style (RaceDefine.RunningStyle: 1 Nige / 2 Senkou / 3 Sashi / 4 Oikomi).
    pub running_style: i32,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub slots: Vec<Slot>,
    /// The team evaluation point captured at snapshot time (sent back verbatim; the server
    /// recomputes anyway, but the real client sends it so we match the wire shape).
    #[serde(default)]
    pub eval_point: i32,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct Store {
    profiles: Vec<Profile>,
}

/// Max profiles the UI offers (user asked for ~5).
pub const MAX_PROFILES: usize = 5;

fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(load_from_disk()))
}

/// A profile queued by the UI (render thread) to be SENT from the game main thread by `pump()`.
/// RequestBase.Send touches Unity main-thread-only objects, so it must not run on the render thread.
fn pending() -> &'static Mutex<Option<Profile>> {
    static P: OnceLock<Mutex<Option<Profile>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// Result/progress of the deferred apply (written by `pump`, shown by the UI).
fn pump_status_buf() -> &'static Mutex<String> {
    static S: OnceLock<Mutex<String>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(String::new()))
}

/// The latest deferred-apply status line, for the UI to display.
pub fn pump_status() -> String {
    pump_status_buf().lock().map(|s| s.clone()).unwrap_or_default()
}

/// Run on the GAME MAIN THREAD every frame (called from ui_tempo's TweenManager.Update hook).
/// If an apply is queued, perform the actual send here — the only safe place for RequestBase.Send.
pub fn pump() {
    // Re-entrancy guard: apply_to_builder calls OnDeckChange → the view refresh may call back into
    // HasError (which also pumps). Don't recurse.
    static IN_PUMP: AtomicBool = AtomicBool::new(false);
    if IN_PUMP.swap(true, Ordering::Relaxed) {
        return;
    }
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            IN_PUMP.store(false, Ordering::Relaxed);
        }
    }
    let _g = Guard;

    // Latch the live builder from the view controller every frame (this replaces the crashy per-frame
    // UpdateView hook). Cheap: one pointer read when the team screen is open, a no-op otherwise.
    il2cpp_bridge::capture_from_vc();

    let profile = match pending().lock() {
        Ok(mut g) => g.take(),
        Err(_) => return,
    };
    let Some(profile) = profile else { return };
    crate::crashlog::step("padder:pump:apply-to-builder");
    let msg = match il2cpp_bridge::apply_to_builder(&profile) {
        Ok(_) => format!("Loaded \"{}\" — press Confirm in-game.", profile.name),
        Err(e) => format!("Apply failed: {e}"),
    };
    if let Ok(mut s) = pump_status_buf().lock() {
        *s = msg;
    }
}

fn json_path() -> std::path::PathBuf {
    crate::paths::dll_dir().join("heaven_tt_profiles.json")
}

fn load_from_disk() -> Store {
    match std::fs::read(json_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Store::default(),
    }
}

fn save_to_disk(s: &Store) {
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        let _ = std::fs::write(json_path(), json);
    }
}

// ── public API consumed by the overlay UI ─────────────────────────────────────

/// (name, slot_count) for every saved profile, in order — for the list UI.
pub fn list() -> Vec<(String, usize)> {
    store()
        .lock()
        .map(|s| s.profiles.iter().map(|p| (p.name.clone(), p.slots.len())).collect())
        .unwrap_or_default()
}

pub fn count() -> usize {
    store().lock().map(|s| s.profiles.len()).unwrap_or(0)
}

/// Snapshot the current live team into a new profile. Returns Ok(name) or an error string.
pub fn save_current(name: &str) -> Result<String, String> {
    let slots = il2cpp_bridge::snapshot_current_deck()?;
    if slots.is_empty() {
        return Err("Could not read the current team (are you in Team Trials?).".into());
    }
    let eval = il2cpp_bridge::current_eval_point().unwrap_or(0);
    let mut s = store().lock().map_err(|_| "lock")?;
    if s.profiles.len() >= MAX_PROFILES {
        return Err(format!("Max {MAX_PROFILES} profiles. Delete one first.").into());
    }
    let name = uniquify(&s, name);
    s.profiles.push(Profile { name: name.clone(), slots, eval_point: eval });
    save_to_disk(&s);
    Ok(name)
}

/// Overwrite an existing profile with the current live team (re-snapshot).
pub fn overwrite(idx: usize) -> Result<(), String> {
    let slots = il2cpp_bridge::snapshot_current_deck()?;
    if slots.is_empty() {
        return Err("Could not read the current team.".into());
    }
    let eval = il2cpp_bridge::current_eval_point().unwrap_or(0);
    let mut s = store().lock().map_err(|_| "lock")?;
    let p = s.profiles.get_mut(idx).ok_or("profile doesn't exist")?;
    p.slots = slots;
    p.eval_point = eval;
    save_to_disk(&s);
    Ok(())
}

pub fn rename(idx: usize, new_name: &str) -> Result<(), String> {
    let mut s = store().lock().map_err(|_| "lock")?;
    let clean = new_name.trim();
    if clean.is_empty() {
        return Err("empty name".into());
    }
    // keep names unique (ignore the slot we're renaming)
    if s.profiles.iter().enumerate().any(|(i, p)| i != idx && p.name == clean) {
        return Err("a profile with that name already exists".into());
    }
    if let Some(p) = s.profiles.get_mut(idx) {
        p.name = clean.to_string();
        save_to_disk(&s);
        Ok(())
    } else {
        Err("profile doesn't exist".into())
    }
}

pub fn delete(idx: usize) -> Result<(), String> {
    let mut s = store().lock().map_err(|_| "lock")?;
    if idx >= s.profiles.len() {
        return Err("profile doesn't exist".into());
    }
    s.profiles.remove(idx);
    save_to_disk(&s);
    Ok(())
}

/// Validate a profile against the live roster WITHOUT sending: returns the list of
/// trained_chara_ids that no longer exist (empty = safe to apply).
pub fn missing_ids(idx: usize) -> Result<Vec<i32>, String> {
    let ids: Vec<i32> = {
        let s = store().lock().map_err(|_| "lock")?;
        let p = s.profiles.get(idx).ok_or("profile doesn't exist")?;
        p.slots.iter().map(|sl| sl.trained_chara_id).collect()
    };
    il2cpp_bridge::filter_missing(&ids)
}

/// Apply a profile (the actual 1-click swap). Validates first; refuses on any missing id.
pub fn apply(idx: usize) -> Result<(), String> {
    let missing = missing_ids(idx)?;
    if !missing.is_empty() {
        return Err(format!(
            "{} Uma(s) in the profile no longer exist (deleted/released). Not applied.",
            missing.len()
        ));
    }
    if !edit_screen_open() {
        return Err("Open the Team Trials team-edit screen first.".into());
    }
    let profile = {
        let s = store().lock().map_err(|_| "lock")?;
        s.profiles.get(idx).cloned().ok_or("profile doesn't exist")?
    };
    // Queue for the main-thread pump — driving the editor's builder must happen on the game main
    // thread (the menu/apply click runs on the render thread).
    *pending().lock().map_err(|_| "lock")? = Some(profile);
    if let Ok(mut s) = pump_status_buf().lock() {
        *s = "Applying…".into();
    }
    Ok(())
}

fn uniquify(s: &Store, name: &str) -> String {
    let base = {
        let t = name.trim();
        if t.is_empty() { "Perfil".to_string() } else { t.to_string() }
    };
    if !s.profiles.iter().any(|p| p.name == base) {
        return base;
    }
    for n in 2..999 {
        let cand = format!("{base} {n}");
        if !s.profiles.iter().any(|p| p.name == cand) {
            return cand;
        }
    }
    base
}

// ──────────────────────────────────────────────────────────────────────────────
// IL2CPP boundary — resolved by name at runtime. The read path (snapshot/roster/eval)
// is a pure field walk; the send path (apply) dispatches the game's own team-edit request.
// All offsets below come from the static dump (_il2cpp_dump/dump.cs) and are VALIDATED LIVE.
// ──────────────────────────────────────────────────────────────────────────────
mod il2cpp_bridge {
    use super::{Profile, Slot};
    use crate::il2cpp;
    use core::ffi::c_void;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::OnceLock;

    use retour::RawDetour;

    fn log(msg: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(crate::paths::log_file("heaven-native.log"))
        {
            let _ = writeln!(f, "[padder] {msg}");
        }
    }

    // ── live deck-builder capture (via the edit-screen view controller) ───────
    // The TT team-edit screen edits a TeamStadiumDeckBuilder's OWN DeckInfo copy (the grid binds to
    // it), so to make the grid update + Confirm light up we must drive THAT instance. Live Frida
    // tracing showed the builder is reached via `TeamStadiumDeckViewController._deckBuilder` @0x38,
    // and the controller's `UpdateView()` fires ~every frame while the screen is open — so it's the
    // ideal hook for BOTH capturing the builder AND running the main-thread pump (no dependency on
    // ui_tempo/Hachimi). Cleared on `EndView`. 0 = no edit screen open.
    static BUILDER: AtomicUsize = AtomicUsize::new(0);
    static VC: AtomicUsize = AtomicUsize::new(0); // live TeamStadiumDeckViewController (0 = screen not shown)
    static INITVIEW_ORIG: AtomicUsize = AtomicUsize::new(0);
    static PLAYIN_ORIG: AtomicUsize = AtomicUsize::new(0);
    static PLAYOUT_ORIG: AtomicUsize = AtomicUsize::new(0);
    static ENDVIEW_ORIG: AtomicUsize = AtomicUsize::new(0);
    static DETOURS: OnceLock<Vec<RawDetour>> = OnceLock::new();

    /// VC field offset: TeamStadiumDeckViewController._deckBuilder (Frida-confirmed @56).
    const VC_DECKBUILDER_OFF: usize = 0x38;

    /// Latch the live builder. Validates it has a DeckInfo so we never store a half-built instance.
    unsafe fn capture(builder: *mut c_void) {
        if builder.is_null() {
            return;
        }
        let di = *((builder as usize + 0x30) as *const *mut c_void);
        if di.is_null() {
            return;
        }
        BUILDER.store(builder as usize, Ordering::Relaxed);
    }

    /// Read the live builder from the stored view controller and latch it. Called every frame from
    /// `super::pump`. This REPLACES the old per-frame `UpdateView` hook: the 2026-07-01 update made
    /// `UpdateView` a 48-byte tail-call thunk whose prologue RawDetour's trampoline can't relocate, so
    /// calling the original through it crashed (access violation entering the team screen). We instead
    /// latch the view controller in `InitializeView` (a real 816-byte method, safe to detour) and read
    /// `_deckBuilder@0x38` off it here each frame.
    pub fn capture_from_vc() {
        let vc = VC.load(Ordering::Relaxed);
        if vc == 0 {
            BUILDER.store(0, Ordering::Relaxed);
            return;
        }
        unsafe {
            let builder = *((vc + VC_DECKBUILDER_OFF) as *const *mut c_void);
            capture(builder);
        }
    }

    // InitializeView(): the edit screen opened → latch the view controller (`this`). It returns
    // IEnumerator (a coroutine the game StartCoroutine()s), so we MUST return the original's value.
    // The builder is read later (per-frame in capture_from_vc), once the coroutine has set _deckBuilder.
    type InitViewFn = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;
    unsafe extern "C" fn initview_hook(this: *mut c_void, mi: *const c_void) -> *mut c_void {
        if !this.is_null() {
            VC.store(this as usize, Ordering::Relaxed);
        }
        let o = INITVIEW_ORIG.load(Ordering::Relaxed);
        if o != 0 {
            let f: InitViewFn = std::mem::transmute(o);
            return f(this, mi);
        }
        std::ptr::null_mut()
    }

    // PlayInView(): the screen (re)enters. Unlike InitializeView (once, on create) this ALSO fires when
    // you return from a sub-screen (uma detail / slot picker), so it re-latches the VC — fixing "open
    // the team edit screen" after backing out of a uma. IEnumerator → return the original's value.
    unsafe extern "C" fn playin_hook(this: *mut c_void, mi: *const c_void) -> *mut c_void {
        if !this.is_null() {
            VC.store(this as usize, Ordering::Relaxed);
        }
        let o = PLAYIN_ORIG.load(Ordering::Relaxed);
        if o != 0 {
            let f: InitViewFn = std::mem::transmute(o);
            return f(this, mi);
        }
        std::ptr::null_mut()
    }

    // PlayOutView(): the screen leaves (e.g. opening a uma detail) → drop the VC/builder so we never
    // drive a paused screen. PlayInView re-latches on return. IEnumerator → return the original's value.
    unsafe extern "C" fn playout_hook(this: *mut c_void, mi: *const c_void) -> *mut c_void {
        VC.store(0, Ordering::Relaxed);
        BUILDER.store(0, Ordering::Relaxed);
        let o = PLAYOUT_ORIG.load(Ordering::Relaxed);
        if o != 0 {
            let f: InitViewFn = std::mem::transmute(o);
            return f(this, mi);
        }
        std::ptr::null_mut()
    }
    // EndView(): screen closed → forget the builder. IMPORTANT: EndView returns IEnumerator — it's a
    // coroutine the game StartCoroutine()s. The hook MUST return the original's IEnumerator. Declaring
    // it `void` (like UpdateView) left garbage in RAX; once the game started using the returned
    // coroutine (2026-07-01 update), that garbage pointer got StartCoroutine()'d → access violation
    // the moment you enter the Team Trials team-change screen. Return the real IEnumerator.
    type EndViewFn = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;
    unsafe extern "C" fn endview_hook(this: *mut c_void, mi: *const c_void) -> *mut c_void {
        crate::crashlog::step("padder:endview:enter");
        BUILDER.store(0, Ordering::Relaxed);
        VC.store(0, Ordering::Relaxed);
        let o = ENDVIEW_ORIG.load(Ordering::Relaxed);
        if o != 0 {
            let f: EndViewFn = std::mem::transmute(o);
            crate::crashlog::step("padder:endview:orig");
            let r = f(this, mi);
            crate::crashlog::step("idle:after-padder-endview");
            return r;
        }
        crate::crashlog::step("idle:padder-endview-null");
        std::ptr::null_mut()
    }

    /// Detour TeamStadiumDeckViewController.UpdateView (capture+pump) + EndView (clear). Hooking the
    /// VIEW CONTROLLER (not the builder) is what works: UpdateView is non-inlined and per-frame; the
    /// builder's own small methods are inlined / fire only on edits. Run on an IL2CPP-attached thread.
    pub fn install() -> String {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return "il2cpp not ready".into();
        }
        let k = il2cpp::class("Gallop.TeamStadiumDeckViewController");
        if k.is_null() {
            return "TeamStadiumDeckViewController class not found".into();
        }
        // 2026-07-01 FIX: the old per-frame UpdateView hook crashed (UpdateView became a 48-byte
        // tail-call thunk RawDetour can't trampoline). Latch the view controller in InitializeView
        // (real 816-byte method, safe) + read the builder per-frame in capture_from_vc from the pump.
        // Both InitializeView and EndView return IEnumerator → their hooks return the original's value.
        let targets: [(&str, i32, *const (), &AtomicUsize); 4] = [
            ("InitializeView", 0, initview_hook as *const (), &INITVIEW_ORIG),
            ("PlayInView", 0, playin_hook as *const (), &PLAYIN_ORIG),
            ("PlayOutView", 0, playout_hook as *const (), &PLAYOUT_ORIG),
            ("EndView", 0, endview_hook as *const (), &ENDVIEW_ORIG),
        ];
        let mut detours = Vec::new();
        let mut ok = 0;
        unsafe {
            for (name, argc, hook, orig) in targets {
                let m = il2cpp::method(k, name, argc);
                let p = il2cpp::method_pointer(m);
                if p.is_null() || crate::il2cpp::is_detoured(p) {
                    continue;
                }
                if let Ok(d) = RawDetour::new(p as *const (), hook) {
                    if d.enable().is_ok() {
                        orig.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                        detours.push(d);
                        ok += 1;
                    }
                }
            }
        }
        let _ = DETOURS.set(detours);
        format!("deck-view capture: {ok}/4 hooks")
    }

    /// True if the TT team-edit screen is open (we have a live builder to drive).
    pub fn edit_screen_open() -> bool {
        BUILDER.load(Ordering::Relaxed) != 0
    }

    /// Call a 0-arg instance getter that returns an object pointer.
    unsafe fn call_obj_getter(this: *mut c_void, klass_name: &str, method: &str) -> *mut c_void {
        if this.is_null() {
            return std::ptr::null_mut();
        }
        let k = il2cpp::class(klass_name);
        if k.is_null() {
            return std::ptr::null_mut();
        }
        let m = il2cpp::method(k, method, 0);
        if m.is_null() {
            return std::ptr::null_mut();
        }
        let p = il2cpp::method_pointer(m);
        if p.is_null() {
            return std::ptr::null_mut();
        }
        let f: extern "C" fn(*mut c_void, *const c_void) -> *mut c_void = std::mem::transmute(p);
        f(this, m as *const c_void)
    }

    /// WorkDataManager.Instance -> WorkTeamStadiumData -> TeamStadiumDeckInfo. Null if TT not loaded.
    unsafe fn deck_info() -> *mut c_void {
        // WorkDataManager : Singleton<WorkDataManager> → static get_Instance()
        let wdm_class = il2cpp::class("Gallop.WorkDataManager");
        if wdm_class.is_null() {
            log("WorkDataManager class missing");
            return std::ptr::null_mut();
        }
        let gi = il2cpp::method(wdm_class, "get_Instance", 0);
        if gi.is_null() {
            log("WorkDataManager.get_Instance missing");
            return std::ptr::null_mut();
        }
        let gip = il2cpp::method_pointer(gi);
        if gip.is_null() {
            return std::ptr::null_mut();
        }
        let f: extern "C" fn(*const c_void) -> *mut c_void = std::mem::transmute(gip);
        let wdm = f(gi as *const c_void);
        if wdm.is_null() {
            return std::ptr::null_mut();
        }
        let wts = call_obj_getter(wdm, "Gallop.WorkDataManager", "get_TeamStadiumData");
        if wts.is_null() {
            return std::ptr::null_mut();
        }
        call_obj_getter(wts, "Gallop.WorkTeamStadiumData", "get_TeamStadiumDeckInfo")
    }

    #[inline]
    unsafe fn rd_ptr(base: *mut c_void, off: usize) -> *mut c_void {
        if base.is_null() { return std::ptr::null_mut(); }
        *((base as usize + off) as *const *mut c_void)
    }
    #[inline]
    unsafe fn rd_i32(base: *mut c_void, off: usize) -> i32 {
        if base.is_null() { return 0; }
        *((base as usize + off) as *const i32)
    }

    /// Decode a CodeStage ObscuredInt at `base+off` (struct: currentCryptoKey@0, hiddenValue@4).
    /// plain = hiddenValue ^ currentCryptoKey. (Layout confirmed against race_export's decoder.)
    #[inline]
    unsafe fn rd_obscured_i32(base: *mut c_void, off: usize) -> i32 {
        if base.is_null() { return 0; }
        let key = *((base as usize + off) as *const i32);
        let hidden = *((base as usize + off + 4) as *const i32);
        hidden ^ key
    }

    /// Snapshot the 15 live slots. Pure field walk — no managed allocation, safe off-thread.
    pub fn snapshot_current_deck() -> Result<Vec<Slot>, String> {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return Err("IL2CPP runtime not ready".into());
        }
        unsafe {
            let di = deck_info();
            if di.is_null() {
                return Err("deck de Team Trials no cargado".into());
            }
            // TeamStadiumDeckInfo._memberList (List<MemberInfo>) @0x10
            let list = rd_ptr(di, 0x10);
            if list.is_null() {
                return Err("member list null".into());
            }
            // List<T>: _items @0x10 (T[]), _size @0x18
            let items = rd_ptr(list, 0x10);
            let size = rd_i32(list, 0x18);
            if items.is_null() || size <= 0 {
                return Err("member list empty".into());
            }
            let mut out = Vec::with_capacity(size as usize);
            for i in 0..size as usize {
                // System.Array data starts at 0x20; ref elements are 8-byte pointers.
                let mi = rd_ptr(items, 0x20 + i * 8);
                if mi.is_null() {
                    continue;
                }
                // MemberInfo: RaceNumber@0x10, MemberId@0x14, _trainedCharaData@0x20, _runningStyle@0x28
                let race_number = rd_i32(mi, 0x10);
                let member_id = rd_i32(mi, 0x14);
                let tcd = rd_ptr(mi, 0x20);
                let running_style = rd_i32(mi, 0x28); // RaceDefine.RunningStyle (plain enum int)
                // empty slot → no trained chara; skip (we only store filled slots)
                if tcd.is_null() {
                    continue;
                }
                // TrainedCharaData._id (ObscuredInt) @0x10
                let trained_chara_id = rd_obscured_i32(tcd, 0x10);
                if trained_chara_id == 0 {
                    continue;
                }
                out.push(Slot {
                    distance_type: race_number,
                    member_id,
                    trained_chara_id,
                    running_style,
                });
            }
            log(&format!("snapshot: {} slots", out.len()));
            Ok(out)
        }
    }

    /// Current team evaluation point (for the wire shape). Best-effort; 0 if unavailable.
    pub fn current_eval_point() -> Option<i32> {
        unsafe {
            let di = deck_info();
            if di.is_null() {
                return None;
            }
            // TeamStadiumDeckInfo.GetTeamRankPoint() — 0-arg instance, returns int.
            let k = il2cpp::class("Gallop.TeamStadiumDeckInfo");
            if k.is_null() {
                return None;
            }
            let m = il2cpp::method(k, "GetTeamRankPoint", 0);
            if m.is_null() {
                return None;
            }
            let p = il2cpp::method_pointer(m);
            if p.is_null() {
                return None;
            }
            let f: extern "C" fn(*mut c_void, *const c_void) -> i32 = std::mem::transmute(p);
            Some(f(di, m as *const c_void))
        }
    }

    /// WorkDataManager.get_TrainedCharaData() — the trained-uma roster work-data. Null if not loaded.
    unsafe fn trained_chara_data() -> *mut c_void {
        let wdm_class = il2cpp::class("Gallop.WorkDataManager");
        if wdm_class.is_null() {
            return std::ptr::null_mut();
        }
        let gi = il2cpp::method(wdm_class, "get_Instance", 0);
        if gi.is_null() {
            return std::ptr::null_mut();
        }
        let gip = il2cpp::method_pointer(gi);
        if gip.is_null() {
            return std::ptr::null_mut();
        }
        let f: extern "C" fn(*const c_void) -> *mut c_void = std::mem::transmute(gip);
        let wdm = f(gi as *const c_void);
        call_obj_getter(wdm, "Gallop.WorkDataManager", "get_TrainedCharaData")
    }

    /// Of `ids`, return the ones the player no longer owns (so apply can refuse a corrupt deck).
    /// Uses WorkTrainedCharaData.Get(id, all=true) — null return = the uma is gone.
    pub fn filter_missing(ids: &[i32]) -> Result<Vec<i32>, String> {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return Err("IL2CPP runtime not ready".into());
        }
        unsafe {
            let tcd = trained_chara_data();
            if tcd.is_null() {
                return Err("Uma roster not loaded".into());
            }
            // WorkTrainedCharaData.Get(int trainedCharaId, bool all) -> TrainedCharaData (null if absent)
            let k = il2cpp::class("Gallop.WorkTrainedCharaData");
            if k.is_null() {
                return Err("WorkTrainedCharaData class missing".into());
            }
            let m = il2cpp::method(k, "Get", 2);
            if m.is_null() {
                return Err("WorkTrainedCharaData.Get missing".into());
            }
            let p = il2cpp::method_pointer(m);
            if p.is_null() {
                return Err("Get pointer null".into());
            }
            let get: extern "C" fn(*mut c_void, i32, bool, *const c_void) -> *mut c_void =
                std::mem::transmute(p);
            let mut missing = Vec::new();
            for &id in ids {
                let found = get(tcd, id, true, m as *const c_void);
                if found.is_null() {
                    missing.push(id);
                }
            }
            Ok(missing)
        }
    }

    /// Resolve a method and its pointer in one go: returns (fn_ptr, MethodInfo*) or None.
    unsafe fn call_method_ptr(klass: *mut c_void, name: &str, argc: i32) -> Option<(*const c_void, *const c_void)> {
        let m = il2cpp::method(klass, name, argc);
        if m.is_null() {
            return None;
        }
        let p = il2cpp::method_pointer(m);
        if p.is_null() {
            return None;
        }
        Some((p, m as *const c_void))
    }

    /// Drive the live team-edit screen's builder so the grid updates and the game's Confirm button
    /// lights up; the user presses the game's own Confirm to send. Goes through the game's
    /// own validated flow — visible + verifiable + safest vs anti-cheat. Must run on the MAIN THREAD.
    pub fn apply_to_builder(profile: &Profile) -> Result<(), String> {
        if !il2cpp::ready() {
            let _ = il2cpp::init();
        }
        if !il2cpp::ready() {
            return Err("IL2CPP runtime not ready".into());
        }
        let builder = BUILDER.load(Ordering::Relaxed) as *mut c_void;
        if builder.is_null() {
            return Err("open the Team Trials team-edit screen first".into());
        }
        unsafe {
            // TeamStadiumDeckBuilder.DeckInfo @0x30 — the deck the grid is bound to.
            let deck_info = *((builder as usize + 0x30) as *const *mut c_void);
            if deck_info.is_null() {
                return Err("editor deck is empty".into());
            }
            let tcd = trained_chara_data();
            if tcd.is_null() {
                return Err("Uma roster not loaded".into());
            }
            let di_class = il2cpp::class("Gallop.TeamStadiumDeckInfo");
            let wtcd_class = il2cpp::class("Gallop.WorkTrainedCharaData");
            let b_class = il2cpp::class("Gallop.TeamStadiumDeckBuilder");
            if di_class.is_null() || wtcd_class.is_null() || b_class.is_null() {
                return Err("TT classes not resolved".into());
            }
            let gm = call_method_ptr(di_class, "GetMember", 2);
            let gc = call_method_ptr(wtcd_class, "Get", 2);
            let odc = call_method_ptr(b_class, "OnDeckChange", 1);
            let (gm, gc, odc) = match (gm, gc, odc) {
                (Some(a), Some(b), Some(c)) => (a, b, c),
                _ => return Err("editor methods not resolved".into()),
            };
            let get_member: extern "C" fn(*mut c_void, i32, i32, *const c_void) -> *mut c_void =
                std::mem::transmute(gm.0);
            let get_chara: extern "C" fn(*mut c_void, i32, bool, *const c_void) -> *mut c_void =
                std::mem::transmute(gc.0);
            let on_change: extern "C" fn(*mut c_void, *mut c_void, *const c_void) =
                std::mem::transmute(odc.0);

            let mut last_member: *mut c_void = std::ptr::null_mut();
            let mut set = 0;
            for slot in &profile.slots {
                let member = get_member(deck_info, slot.distance_type, slot.member_id, gm.1);
                if member.is_null() {
                    continue;
                }
                let chara = get_chara(tcd, slot.trained_chara_id, true, gc.1);
                if chara.is_null() {
                    continue;
                }
                // MemberInfo._trainedCharaData @0x20 (ref → barrier) ; _runningStyle @0x28 (enum int)
                il2cpp::wbarrier_set(member, (member as usize + 0x20) as *mut c_void, chara);
                *((member as usize + 0x28) as *mut i32) = slot.running_style;
                last_member = member;
                set += 1;
            }
            log(&format!("apply_to_builder: set {set}/{} members", profile.slots.len()));
            if set == 0 {
                return Err("could not set any Uma (offsets?)".into());
            }
            // Refresh the grid + recompute IsDeckChanged → the game's Confirm button lights up.
            // The user presses the game's own Confirm to send (the safest, fully-normal flow).
            on_change(builder, last_member, odc.1);
            Ok(())
        }
    }
}
