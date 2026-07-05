//! affinity — exact succession affinity on the Legacy Select screen, shown as user-placed badges.
//!
//! VALUE: we hook `Gallop.SingleModeUtils.CalcRelationPoint(trainee, p1, p2)` and read the value the
//! GAME itself computes (with its real trainee chara id), so it matches the in-game ◎/○/△ rank
//! exactly — same source as the standalone LiveAnalyzer. Per-parent "chain" totals (parent + its 2
//! grandparents + win-saddle bonus) come from re-invoking the original via the trampoline with the
//! second parent null — `CalcRelationPoint(trainee, pX, null)` returns exactly that branch.
//!
//! POSITION: the game UI renders to a nested RenderTexture that can't be inverted to screen reliably,
//! so instead of projecting we let the user DRAG the three numbers where they want (edit mode) and
//! persist the spots as screen FRACTIONS — resolution independent by construction. Size is adjustable.

#![allow(static_mut_refs, dead_code)]

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use retour::RawDetour;

use crate::il2cpp;

// ── on/off ──────────────────────────────────────────────────────────────────────
static ENABLED: AtomicBool = AtomicBool::new(true);
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    save();
}

// ── edit (drag) mode ──────────────────────────────────────────────────────────────
static EDIT: AtomicBool = AtomicBool::new(false);
pub fn edit_mode() -> bool {
    EDIT.load(Ordering::Relaxed)
}
pub fn set_edit_mode(on: bool) {
    EDIT.store(on, Ordering::Relaxed);
    if !on {
        save();
    }
}

// ── screen gate: are we on the Legacy Select MAIN view (not the picker, not other steps)? ──────────
// STEP = the live SingleModeStartStepSuccessionSelect instance (set on its Show(), cleared on Hide()).
// Show/Hide fire as the start flow moves between steps, so this is false on Support Formation / Final
// Confirmation. The picker (tap a slot → candidate list) is the step's `_showDetail` bool @0x50 — true
// while that overlay is up; we hide the badges then too. _showDetail is a plain field read (safe off
// any thread). So badges show ONLY on the Legacy Select main screen.
static STEP: AtomicUsize = AtomicUsize::new(0);
const O_SHOWDETAIL: usize = 0x50;

// A candidate's stat sheet (Skills / Inspiration / Career Info) opens as a DialogCharacterDetail — a
// DialogCommon pushed onto DialogManager, NOT the step's inline `_showDetail` — so the badges used to
// leak on top of it. DIALOG_OPEN mirrors `DialogManager.get_IsShowDialog()`, sampled on the main thread
// by `poll()` (calling that il2cpp getter from the render thread would be a GC hazard).
static DIALOG_OPEN: AtomicBool = AtomicBool::new(false);
static ISDLG_FN: AtomicUsize = AtomicUsize::new(0);
static ISDLG_M: AtomicUsize = AtomicUsize::new(0);

/// True while the Legacy Select MAIN view is the visible step and no picker/detail/dialog is on top.
pub fn active() -> bool {
    let step = STEP.load(Ordering::Relaxed);
    if step == 0 {
        return false;
    }
    // Any dialog open (e.g. the candidate stat sheet) → hide the badges.
    if DIALOG_OPEN.load(Ordering::Relaxed) {
        return false;
    }
    // _showDetail (bool) @0x50 — inline picker overlay open → don't show.
    let detail = unsafe { *((step + O_SHOWDETAIL) as *const u8) };
    detail == 0
}

/// Main-thread poll (driven by ui_tempo's single TweenManager.Update detour): refresh DIALOG_OPEN from the game's
/// own `DialogManager.get_IsShowDialog()`. Only sampled while on Legacy Select (STEP set) — cheap and
/// avoids calling the getter on unrelated screens / before DialogManager exists.
pub fn poll() {
    if STEP.load(Ordering::Relaxed) == 0 {
        DIALOG_OPEN.store(false, Ordering::Relaxed);
        return;
    }
    let f = ISDLG_FN.load(Ordering::Relaxed);
    let m = ISDLG_M.load(Ordering::Relaxed);
    if f == 0 || m == 0 {
        return;
    }
    unsafe {
        // static bool get_IsShowDialog(MethodInfo*)
        let g: extern "C" fn(*const core::ffi::c_void) -> bool = std::mem::transmute(f);
        DIALOG_OPEN.store(g(m as *const core::ffi::c_void), Ordering::Relaxed);
    }
}

// ── values (from the CalcRelationPoint hook) ───────────────────────────────────────
static TOTAL: AtomicI32 = AtomicI32::new(-1);
static IND1: AtomicI32 = AtomicI32::new(-1);
static IND2: AtomicI32 = AtomicI32::new(-1);
static VAL_TS: AtomicU64 = AtomicU64::new(0);

/// (total, parent1 branch, parent2 branch). A value is -1 if not applicable (e.g. a parent unset).
/// None if no recent affinity computation (no pairing evaluated yet on this screen).
pub fn values() -> Option<(i32, i32, i32)> {
    let ts = VAL_TS.load(Ordering::Relaxed);
    if ts == 0 {
        return None;
    }
    Some((TOTAL.load(Ordering::Relaxed), IND1.load(Ordering::Relaxed), IND2.load(Ordering::Relaxed)))
}

// ── positions (screen fractions) + size ────────────────────────────────────────────
// index 0 = total, 1 = parent1, 2 = parent2. Stored as f32 bits.
static POS_X: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
static POS_Y: [AtomicU32; 3] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
static SIZE: AtomicU32 = AtomicU32::new(0); // f32 scale, default 1.6

#[inline]
fn bits(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}
#[inline]
fn set_bits(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

/// (fx, fy) screen-fraction position of badge `i` (0=total,1=p1,2=p2).
pub fn pos(i: usize) -> (f32, f32) {
    (bits(&POS_X[i]), bits(&POS_Y[i]))
}
/// Set badge `i` position as screen fractions (clamped to [0,1]).
pub fn set_pos(i: usize, fx: f32, fy: f32) {
    set_bits(&POS_X[i], fx.clamp(0.0, 1.0));
    set_bits(&POS_Y[i], fy.clamp(0.0, 1.0));
}
pub fn size() -> f32 {
    bits(&SIZE)
}
pub fn set_size(s: f32) {
    set_bits(&SIZE, s.clamp(0.8, 4.0));
    save();
}

// ── persistence ─────────────────────────────────────────────────────────────────
fn cfg_path() -> std::path::PathBuf {
    crate::paths::local_file_migrated("trackside_tt_affinity.json", "heaven_tt_affinity.json")
}
fn save() {
    let v = serde_json::json!({
        "enabled": ENABLED.load(Ordering::Relaxed),
        "size": size(),
        "total": [pos(0).0, pos(0).1],
        "p1": [pos(1).0, pos(1).1],
        "p2": [pos(2).0, pos(2).1],
    });
    let _ = std::fs::write(cfg_path(), v.to_string());
}
fn load_cfg() {
    // sensible defaults (tuned on the real Legacy Select layout, screen fractions) so a fresh user
    // gets good placement with no setup — they can still drag to taste.
    set_bits(&SIZE, 1.38);
    set_pos(0, 0.3720, 0.1504); // total — by the "Affinity:" line
    set_pos(1, 0.1636, 0.6384); // parent 1 — under the left legacy slot
    set_pos(2, 0.3098, 0.6375); // parent 2 — under the right legacy slot
    if let Ok(b) = std::fs::read(cfg_path()) {
        if let Ok(j) = serde_json::from_slice::<serde_json::Value>(&b) {
            if let Some(e) = j.get("enabled").and_then(|x| x.as_bool()) {
                ENABLED.store(e, Ordering::Relaxed);
            }
            if let Some(s) = j.get("size").and_then(|x| x.as_f64()) {
                set_bits(&SIZE, s as f32);
            }
            for (k, i) in [("total", 0usize), ("p1", 1), ("p2", 2)] {
                if let Some(a) = j.get(k).and_then(|x| x.as_array()) {
                    if a.len() == 2 {
                        let fx = a[0].as_f64().unwrap_or(0.0) as f32;
                        let fy = a[1].as_f64().unwrap_or(0.0) as f32;
                        set_pos(i, fx, fy);
                    }
                }
            }
        }
    }
}
/// Persist current positions (call when the user finishes dragging).
pub fn persist() {
    save();
}

fn clock() -> &'static Instant {
    crate::tools::clock()
}
fn now_ms() -> u64 {
    crate::tools::now_ms()
}

// ── CalcRelationPoint hook (the exact game value) ──────────────────────────────────
static TRAMP: AtomicUsize = AtomicUsize::new(0);
static CALC_DETOUR: OnceLock<RawDetour> = OnceLock::new();

// static CalcRelationPoint(i32 trainee, TCD* p1, TCD* p2, MethodInfo*) -> i32
type CalcFn = unsafe extern "C" fn(i32, usize, usize, usize) -> i32;

unsafe extern "C" fn calc_hook(trainee: i32, p1: usize, p2: usize, mi: usize) -> i32 {
    let tr = TRAMP.load(Ordering::Relaxed);
    if tr == 0 {
        return 0;
    }
    let f: CalcFn = std::mem::transmute(tr);
    let total = f(trainee, p1, p2, mi); // the value the game uses (exact)
    // per-parent branch (parent + 2 grandparents + win-saddle) via the trampoline → no recursion.
    let ind1 = if p1 != 0 { f(trainee, p1, 0, mi) } else { -1 };
    let ind2 = if p2 != 0 { f(trainee, p2, 0, mi) } else { -1 };
    if (0..=600).contains(&total) {
        TOTAL.store(total, Ordering::Relaxed);
        IND1.store(ind1, Ordering::Relaxed);
        IND2.store(ind2, Ordering::Relaxed);
        VAL_TS.store(now_ms().max(1), Ordering::Relaxed);
    }
    total
}

// ── screen gate hooks (SingleModeStartStepSuccessionSelect Show/Hide) ──────────
static SHOW_ORIG: AtomicUsize = AtomicUsize::new(0);
static HIDE_ORIG: AtomicUsize = AtomicUsize::new(0);
static SHOW_DETOUR: OnceLock<RawDetour> = OnceLock::new();
static HIDE_DETOUR: OnceLock<RawDetour> = OnceLock::new();

// Show() — the Legacy Select main view became the visible step.
type ShowFn = unsafe extern "C" fn(*mut c_void, *const c_void);
unsafe extern "C" fn show_hook(this: *mut c_void, mi: *const c_void) {
    if !this.is_null() {
        STEP.store(this as usize, Ordering::Relaxed);
    }
    let o = SHOW_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: ShowFn = std::mem::transmute(o);
        f(this, mi);
    }
}

// Hide(bool force) — leaving the step (to Support Formation / Confirmation / back). Drop everything.
type HideFn = unsafe extern "C" fn(*mut c_void, bool, *const c_void);
unsafe extern "C" fn hide_hook(this: *mut c_void, force: bool, mi: *const c_void) {
    STEP.store(0, Ordering::Relaxed);
    VAL_TS.store(0, Ordering::Relaxed); // forget values when leaving (re-captured on the next pairing)
    EDIT.store(false, Ordering::Relaxed);
    let o = HIDE_ORIG.load(Ordering::Relaxed);
    if o != 0 {
        let f: HideFn = std::mem::transmute(o);
        f(this, force, mi);
    }
}

fn log(msg: &str) {
    crate::tools::log(&format!("[affinity] {msg}"));
}


/// Install the value hook + screen gate. Run on an IL2CPP-attached thread (boot).
pub fn install() -> String {
    if !il2cpp::ready() {
        let _ = il2cpp::init();
    }
    if !il2cpp::ready() {
        return "il2cpp not ready".into();
    }
    load_cfg();
    let mut notes = String::new();

    // CalcRelationPoint — read the game's exact value.
    let smu = il2cpp::class("Gallop.SingleModeUtils");
    if smu.is_null() {
        return "SingleModeUtils not found".into();
    }
    unsafe {
        let m = il2cpp::method(smu, "CalcRelationPoint", 3);
        let p = il2cpp::method_pointer(m);
        if p.is_null() || il2cpp::is_detoured(p) {
            notes.push_str("calc:skip ");
        } else if let Ok(d) = RawDetour::new(p as *const (), calc_hook as *const ()) {
            if d.enable().is_ok() {
                TRAMP.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = CALC_DETOUR.set(d);
                notes.push_str("calc:ok ");
            } else {
                notes.push_str("calc:enable-fail ");
            }
        } else {
            notes.push_str("calc:new-fail ");
        }

        // Screen gate: the succession-select STEP's Show()/Hide() (precise to the main view only).
        let k = il2cpp::class("Gallop.SingleModeStartStepSuccessionSelect");
        if k.is_null() {
            notes.push_str("step:miss");
            return format!("affinity: {}", notes.trim());
        }
        let m = il2cpp::method(k, "Show", 0);
        let p = il2cpp::method_pointer(m);
        if !p.is_null() && !il2cpp::is_detoured(p) {
            if let Ok(d) = RawDetour::new(p as *const (), show_hook as *const ()) {
                if d.enable().is_ok() {
                    SHOW_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = SHOW_DETOUR.set(d);
                    notes.push_str("show:ok ");
                }
            }
        } else {
            notes.push_str("show:skip ");
        }
        let m = il2cpp::method(k, "Hide", 1);
        let p = il2cpp::method_pointer(m);
        if !p.is_null() && !il2cpp::is_detoured(p) {
            if let Ok(d) = RawDetour::new(p as *const (), hide_hook as *const ()) {
                if d.enable().is_ok() {
                    HIDE_ORIG.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                    let _ = HIDE_DETOUR.set(d);
                    notes.push_str("hide:ok");
                }
            }
        } else {
            notes.push_str("hide:skip");
        }

        // Dialog gate: cache DialogManager.get_IsShowDialog (static bool) so poll() can hide the badges
        // whenever a dialog (the candidate stat sheet) is open on top of Legacy Select.
        let dm = il2cpp::class("Gallop.DialogManager");
        if !dm.is_null() {
            let m = il2cpp::method(dm, "get_IsShowDialog", 0);
            let p = il2cpp::method_pointer(m);
            if !m.is_null() && !p.is_null() {
                ISDLG_FN.store(p as usize, Ordering::Relaxed);
                ISDLG_M.store(m as usize, Ordering::Relaxed);
                notes.push_str(" dlg:ok");
            } else {
                notes.push_str(" dlg:miss");
            }
        } else {
            notes.push_str(" dlg:noclass");
        }
    }
    let _ = log;
    format!("affinity: {}", notes.trim())
}

/// Draw the exact succession-affinity numbers on the Legacy Select screen as three user-placed
/// badges (Total / Parent 1 / Parent 2), each its own borderless imgui window so it can be dragged
/// (edit mode) and font-scaled. Positions persist as screen fractions (resolution independent).
pub(crate) fn draw_badges_panel(ui: &hudhook::imgui::Ui) {
    use hudhook::imgui;
    use hudhook::imgui::{StyleColor, StyleVar};
    use crate::overlay::VALUE_FONT;
    if !crate::affinity::is_enabled() || !crate::affinity::active() {
        return;
    }
    let edit = crate::affinity::edit_mode();
    let (total, ind1, ind2) = crate::affinity::values().unwrap_or((-1, -1, -1));
    let [dw, dh] = ui.io().display_size;
    if dw < 1.0 || dh < 1.0 {
        return;
    }
    let scale = crate::affinity::size();
    let raw = [total, ind1, ind2];
    // dashboard-style pill: dark fill + thick rounded accent border + white Orbitron number.
    let accents = [
        [1.00, 0.60, 0.13, 1.0], // total — orange/gold
        [0.36, 0.90, 0.52, 1.0], // parent 1 — green
        [0.40, 0.68, 1.00, 1.0], // parent 2 — blue
    ];
    let vfont = VALUE_FONT.with(|c| c.get());

    for i in 0..3usize {
        let v = raw[i];
        if v < 0 && !edit {
            continue; // parent unset → nothing to show (still placeable in edit mode)
        }
        let (fx, fy) = crate::affinity::pos(i);
        let pos = [fx * dw, fy * dh];
        let s = if v < 0 { "\u{2014}".to_string() } else { v.to_string() };
        let cond = if edit { imgui::Condition::FirstUseEver } else { imgui::Condition::Always };
        let accent = accents[i];

        let _r = ui.push_style_var(StyleVar::WindowRounding(40.0));
        let _bs = ui.push_style_var(StyleVar::WindowBorderSize(2.6 * scale.max(0.8)));
        let _pd = ui.push_style_var(StyleVar::WindowPadding([15.0 * scale, 5.0 * scale]));
        let _cb = ui.push_style_color(StyleColor::Border, accent);
        let _cw = ui.push_style_color(StyleColor::WindowBg, [0.06, 0.05, 0.045, 0.96]);

        let mut w = ui
            .window(format!("##aff{i}"))
            .no_decoration()
            .always_auto_resize(true)
            .save_settings(false)
            .focus_on_appearing(false)
            .position(pos, cond)
            .movable(edit);
        if !edit {
            w = w.no_inputs();
        }
        w.build(|| {
            let _f = vfont.map(|f| ui.push_font(f));
            ui.set_window_font_scale(scale);
            ui.text_colored([1.0, 1.0, 1.0, 1.0], &s);
            if edit {
                let wp = ui.window_pos();
                crate::affinity::set_pos(i, (wp[0] / dw).clamp(0.0, 1.0), (wp[1] / dh).clamp(0.0, 1.0));
            }
        });
    }

    if edit {
        let dl = ui.get_background_draw_list();
        let msg = "Affinity: drag each number into place \u{2014} turn off Edit in the menu to save";
        let tw = ui.calc_text_size(msg)[0];
        dl.add_rect([dw * 0.5 - tw * 0.5 - 12.0, 8.0], [dw * 0.5 + tw * 0.5 + 12.0, 32.0], [0.0, 0.0, 0.0, 0.72])
            .filled(true)
            .rounding(6.0)
            .build();
        dl.add_text([dw * 0.5 - tw * 0.5, 12.0], [1.0, 0.95, 0.7, 1.0], msg);
    }
}

/// Legacy Select affinity numbers: enable, drag-to-place (edit mode), size. The value is the game's
/// own CalcRelationPoint result, so it matches the in-game rank exactly.
pub(crate) fn draw_tt_panel(ui: &hudhook::imgui::Ui, w: f32) {
    use crate::overlay::{help_icon, status_dot, DIM, GOOD, WARN};
    ui.dummy([0.0, 4.0]);
    if crate::affinity::active() {
        status_dot(ui, GOOD, "Legacy Select open");
    } else {
        status_dot(ui, WARN, "Open Legacy Select");
    }
    ui.same_line();
    help_icon(
        ui,
        "Shows the exact succession affinity on the Legacy Select screen: the pair Total plus each parent's chain (parent + its 2 grandparents, with win-saddle bonus). Turn on Edit and drag each number where you want it — positions and size are saved.",
    );
    ui.dummy([0.0, 8.0]);

    let mut en = crate::affinity::is_enabled();
    if ui.checkbox("Show affinity numbers", &mut en) {
        crate::affinity::set_enabled(en);
    }
    let mut ed = crate::affinity::edit_mode();
    if ui.checkbox("Edit \u{2014} drag numbers to place them", &mut ed) {
        crate::affinity::set_edit_mode(ed);
    }
    if ed {
        ui.text_colored(DIM, "Drag each number on screen. Uncheck Edit to save.");
    }
    let mut sz = crate::affinity::size();
    ui.set_next_item_width(w * 0.8);
    if ui.slider("Size", 0.8, 4.0, &mut sz) {
        crate::affinity::set_size(sz);
    }

    if let Some((t, a, b)) = crate::affinity::values() {
        ui.dummy([0.0, 6.0]);
        ui.text_colored(DIM, "Current:");
        ui.same_line();
        ui.text_colored([1.0, 0.92, 0.55, 1.0], &format!("Total {t}"));
        ui.same_line();
        let p1 = if a < 0 { "\u{2014}".to_string() } else { a.to_string() };
        let p2 = if b < 0 { "\u{2014}".to_string() } else { b.to_string() };
        ui.text_colored([0.78, 1.0, 0.86, 1.0], &format!("\u{00b7} P1 {p1}"));
        ui.same_line();
        ui.text_colored([0.72, 0.90, 1.0, 1.0], &format!("\u{00b7} P2 {p2}"));
    }
}
