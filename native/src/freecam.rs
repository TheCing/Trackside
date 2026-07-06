//! Heaven — race free camera (feature `freecam`).
//!
//! Clean build: only the proven mechanism. Follows the player's Uma in 3rd-person
//! during a race. Cosmetic (results are server-side).
//!
//! Mechanism (own implementation, proven on Global):
//!   - Hook `RaceCameraManager.AlterLateUpdate` + `RaceViewBase.LateUpdateView` →
//!     raise a bracket flag (`UPDATE_RACE_CAM`) while the race camera updates.
//!   - Inside the bracket (and only once the Uma is captured), override the Unity
//!     transform icalls on the camera: `set_position_Injected` /
//!     `set_localPosition_Injected` / `set_rotation_Injected` /
//!     `Internal_LookAt_Injected` → orbit pose behind/above the Uma, aimed at her.
//!   - Suppress the game's cinematic cuts while freecam owns the view:
//!     `RaceCameraManager.ChangeCameraMode` (skip), `PlayEventCamera` (return false),
//!     `RaceModelController.UpdateCameraDistanceBlendRate` (skip).
//!   - Capture the followed Uma: hook `HorseRaceInfoReplay.get_RunMotionSpeed`
//!     (per horse, per frame) → read `HorseRaceInfo._position` / `_rotationOnLane`.
//!     Gate↔instance map from `HorseRaceInfoReplay..ctor` + `HorseData.get_GateNo`.
//!   - FOV is left NATIVE (overriding it reveals the void below the terrain at some
//!     sections = a blue band). Framing is adjusted with the mouse wheel (distance).
//!
//! KNOWN LIMITATION: on aggressive courses (e.g. Hanshin) the game's camera director
//! still switches the camera mid-race (establishing shots). Taming that fully is WIP
//! (see docs/freecam-status.md).

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use retour::RawDetour;

use crate::il2cpp;

// diagnostic log (shared with the rest of the native engine)
fn flog(msg: &str) {
    crate::tools::log(msg);
}
static DIAG_CAPTURED: AtomicBool = AtomicBool::new(false);

// ── state ────────────────────────────────────────────────────────────────────
static ENABLED: AtomicBool = AtomicBool::new(false);
static UPDATE_RACE_CAM: AtomicBool = AtomicBool::new(false);

// free-fly camera (secondary; follow is the default)
static PX: AtomicU32 = AtomicU32::new(0);
static PY: AtomicU32 = AtomicU32::new(0);
static PZ: AtomicU32 = AtomicU32::new(0);
static YAW: AtomicU32 = AtomicU32::new(0);
static PITCH: AtomicU32 = AtomicU32::new(0);

// follow (3rd-person orbit) mode
static FOLLOW: AtomicBool = AtomicBool::new(false);
static TARGET_GATE: AtomicI32 = AtomicI32::new(1);
static MAX_GATE: AtomicI32 = AtomicI32::new(1);
static EYE_H: AtomicU32 = AtomicU32::new(0); // height offset above _position
static HAVE_TARGET: AtomicBool = AtomicBool::new(false);
static EVER_CAP: AtomicBool = AtomicBool::new(false); // captured the target ≥once this race
// chosen horse position (this frame) + cached forward
static TPX: AtomicU32 = AtomicU32::new(0);
static TPY: AtomicU32 = AtomicU32::new(0);
static TPZ: AtomicU32 = AtomicU32::new(0);
static FX: AtomicU32 = AtomicU32::new(0);
static FY: AtomicU32 = AtomicU32::new(0);
static FZ: AtomicU32 = AtomicU32::new(0);
// orbit controls (relative to the Uma's heading)
static ORBIT_YAW: AtomicU32 = AtomicU32::new(0);
static ORBIT_PITCH: AtomicU32 = AtomicU32::new(0);
static DIST: AtomicU32 = AtomicU32::new(0);
const DEF_POS: (f32, f32, f32) = (-51.72, 7.91, 108.57);
const DEF_EYE_H: f32 = 1.0;
const LOOK_RADIUS: f32 = 5.0;
const PITCH_LIMIT: f32 = 1.55;
// Built-in default chase pose (overridden by the user's saved pose — P key, persisted).
const DEF_DIST: f32 = 61.52;
const DEF_YAW: f32 = 3.125; // orbit yaw offset (≈π → side/front of the Uma)
const DEF_PITCH: f32 = 0.24;
// The game zooms RaceCourseCamera's FOV to ~0-3° for post-skill close-ups; we force a fixed
// moderate FOV on OUR camera only so those stay a steady chase. ~10 = the game's chase FOV.
const FOLLOW_FOV: f32 = 10.0;
// Movement feel.
const ORBIT_STEP: f32 = 0.075; // arrow yaw per tick
const PITCH_STEP: f32 = 0.055; // arrow pitch per tick
const ORBIT_PITCH_LIMIT: f32 = 1.55; // near straight up/down
const DIST_MIN: f32 = 0.1;
const DIST_MAX: f32 = 200.0; // huge zoom-out range
const HEIGHT_STEP: f32 = 0.08; // K height per tick

// ── first-person (experimental) ───────────────────────────────────────────────
// FP = ride the Uma: camera AT her head, looking FORWARD down the track (where the course
// geometry exists), with a clamped look cone so you can't pan into the unrendered "void"
// behind/beside elevated courses. Toggle with V. Reuses the follow target's pos (TP*) + forward (F*).
static FIRST_PERSON: AtomicBool = AtomicBool::new(false);
const FP_FWD: f32 = 1.7; // forward offset from model origin → the head
const FP_EYE_H: f32 = 1.35; // default head/eye height in FP (I/K still adjusts)
const FP_CONE_YAW: f32 = 1.40; // ~80° horizontal look cone (avoid the void)
const FP_CONE_PITCH: f32 = 0.70; // ~40° vertical look cone

#[inline]
fn getf(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}
#[inline]
fn setf(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V3 {
    x: f32,
    y: f32,
    z: f32,
}

// ── public API (used by overlay.rs / boot.rs) ─────────────────────
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub fn set_enabled(on: bool) {
    let was = ENABLED.swap(on, Ordering::Relaxed);
    if on && !was {
        reset();
        // Mid-race ENABLE: engage the chase on the current target right away (don't wait for the
        // next race). Re-arm capture + re-find the race camera so the hand-off is immediate.
        if in_race() && TARGET_GATE.load(Ordering::Relaxed) > 0 {
            EVER_CAP.store(false, Ordering::Relaxed);
            FOLLOW.store(true, Ordering::Relaxed);
            HAVE_TARGET.store(false, Ordering::Relaxed);
            RACE_POSE_LOADED.store(false, Ordering::Relaxed);
            RACE_CAM_OBJ.store(0, Ordering::Relaxed);
            RACE_CAM_TF.store(0, Ordering::Relaxed);
            CAMSET_HASH.store(0, Ordering::Relaxed);
            crate::race_director::reset_pace();
            load_default_pose();
        }
    } else if !on && was {
        // Mid-race DISABLE: stop following so drive_this()/drive_cam() go false and the game's own
        // race camera takes back over immediately (telemetry keeps running independently).
        FOLLOW.store(false, Ordering::Relaxed);
    }
}
/// Apply persisted settings to the free camera at boot.
pub fn apply(s: &crate::settings::Settings) {
    set_enabled(s.freecam);
}
pub fn is_follow() -> bool {
    FOLLOW.load(Ordering::Relaxed)
}
pub fn is_first_person() -> bool {
    FIRST_PERSON.load(Ordering::Relaxed)
}
/// Toggle first-person view. On enter: re-center the look to forward + a sensible head height.
pub fn toggle_first_person() {
    let on = !FIRST_PERSON.fetch_xor(true, Ordering::Relaxed);
    if on {
        setf(&ORBIT_YAW, 0.0);
        setf(&ORBIT_PITCH, 0.0);
        if getf(&EYE_H) < 0.5 {
            setf(&EYE_H, FP_EYE_H);
        }
    }
}
pub fn target_gate() -> i32 {
    TARGET_GATE.load(Ordering::Relaxed)
}
pub fn max_gate() -> i32 {
    MAX_GATE.load(Ordering::Relaxed).max(1)
}
/// Project the followed-Uma's head to imgui screen pixels (top-left origin) using the EXACT
/// freecam pose we render with (eye/look-at/FOV) — so the marker can't drift away from her like
/// `Camera.WorldToScreenPoint` did (that reads the game's animating cinematic camera, on a
/// different thread). Pure math, render-thread safe. None when not following or behind the cam.
pub fn project_head_marker(width: f32, height: f32) -> Option<(f32, f32)> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    if !(FOLLOW.load(Ordering::Relaxed) && EVER_CAP.load(Ordering::Relaxed)) {
        return None;
    }
    let eye = current_pos();
    let tgt = current_lookat();
    let head = V3 { x: getf(&TPX), y: getf(&TPY) + MARK_HEAD_H, z: getf(&TPZ) };
    let cross = |a: V3, b: V3| V3 {
        x: a.y * b.z - a.z * b.y,
        y: a.z * b.x - a.x * b.z,
        z: a.x * b.y - a.y * b.x,
    };
    let norm = |v: V3| -> Option<V3> {
        let n = (v.x * v.x + v.y * v.y + v.z * v.z).sqrt();
        if n < 1e-4 || !n.is_finite() {
            None
        } else {
            Some(V3 { x: v.x / n, y: v.y / n, z: v.z / n })
        }
    };
    // camera basis (world up = +Y); right = up × forward, camUp = forward × right
    let forward = norm(V3 { x: tgt.x - eye.x, y: tgt.y - eye.y, z: tgt.z - eye.z })?;
    let right = norm(cross(V3 { x: 0.0, y: 1.0, z: 0.0 }, forward))?;
    let cam_up = cross(forward, right);
    let d = V3 { x: head.x - eye.x, y: head.y - eye.y, z: head.z - eye.z };
    let depth = d.x * forward.x + d.y * forward.y + d.z * forward.z; // >0 = in front
    if depth <= 0.05 {
        return None;
    }
    let vx = d.x * right.x + d.y * right.y + d.z * right.z;
    let vy = d.x * cam_up.x + d.y * cam_up.y + d.z * cam_up.z;
    let f = 1.0 / (FOLLOW_FOV.to_radians() * 0.5).tan(); // FOLLOW_FOV = vertical FOV we render
    let aspect = width / height;
    let ndc_x = (f / aspect) * (vx / depth);
    let ndc_y = f * (vy / depth);
    if !ndc_x.is_finite() || !ndc_y.is_finite() {
        return None;
    }
    let sx = (ndc_x * 0.5 + 0.5) * width;
    let sy = (0.5 - ndc_y * 0.5) * height; // imgui top-left origin
    Some((sx, sy))
}
/// Follow a different Uma (cycle the target gate by `delta`, wrapping 1..=MAX_GATE).
/// Re-captures the new Uma; keeps the current view mode + framing.
pub fn cycle_target(delta: i32) {
    let mx = MAX_GATE.load(Ordering::Relaxed).max(1);
    let mut g = TARGET_GATE.load(Ordering::Relaxed) + delta;
    if g < 1 {
        g = mx;
    } else if g > mx {
        g = 1;
    }
    TARGET_GATE.store(g, Ordering::Relaxed);
    FOLLOW.store(true, Ordering::Relaxed);
    EVER_CAP.store(false, Ordering::Relaxed); // re-capture the new Uma's pos/forward
    HAVE_TARGET.store(false, Ordering::Relaxed);
    crate::race_director::on_switch_follow(); // switched Uma → rescan ITS activated skills + fresh pace
}

/// Follow a SPECIFIC Uma directly by gate/post position (1..=MAX_GATE). The 1-9 keys use this
/// to jump straight to a horse instead of cycling through them with `[ ]`.
pub fn follow_gate(gate: i32) {
    let mx = MAX_GATE.load(Ordering::Relaxed).max(1);
    if gate < 1 || gate > mx {
        return; // no horse at that gate this race — ignore
    }
    TARGET_GATE.store(gate, Ordering::Relaxed);
    FOLLOW.store(true, Ordering::Relaxed);
    EVER_CAP.store(false, Ordering::Relaxed);
    HAVE_TARGET.store(false, Ordering::Relaxed);
    crate::race_director::on_switch_follow();
}

/// Called when the player's own horse is identified (from the race response). If
/// freecam is on, auto-lock follow onto the player at a good default chase pose.
pub fn auto_follow_player(gate: i32) {
    if gate <= 0 {
        return;
    }
    // Always record the player's gate so the telemetry HUD can default its "followed" panel to it —
    // this works even with the freecam OFF (telemetry is independent now). Only ENGAGE the camera
    // (follow + pose capture) when the freecam is enabled.
    let new_target = TARGET_GATE.load(Ordering::Relaxed) != gate;
    TARGET_GATE.store(gate, Ordering::Relaxed);
    if !ENABLED.load(Ordering::Relaxed) {
        return; // telemetry-only: target known, camera not engaged
    }
    // Already following this same Uma → keep the user's framing (the race response
    // can arrive more than once; re-running reset the camera mid-race).
    if FOLLOW.load(Ordering::Relaxed) && !new_target {
        return;
    }
    EVER_CAP.store(false, Ordering::Relaxed);
    FOLLOW.store(true, Ordering::Relaxed);
    HAVE_TARGET.store(false, Ordering::Relaxed);
    crate::race_director::reset_pace(); // fresh race → clear the previous race's pace trace
    load_default_pose(); // provisional (track id may be 0 here); reloaded once it's known
    RACE_POSE_LOADED.store(false, Ordering::Relaxed); // re-apply the circuit's default once track id resolves
    CAMSET_HASH.store(0, Ordering::Relaxed); // DIAGNOSTIC: re-arm camera-set change dump
    RACE_CAM_OBJ.store(0, Ordering::Relaxed); // re-find RaceCourseCamera this race
    RACE_CAM_TF.store(0, Ordering::Relaxed); // re-cache RaceCourseCamera transform this race
    EFFECT_OBJ.store(0, Ordering::Relaxed); // re-find RaceEnvEffect this race
    crate::race_director::on_new_race(); // fresh telemetry + skill feed for the new race (gates re-map)
    flog(&format!("[freecam] auto-follow player gate {gate}"));
}

/// Mouse drag (left button) → look / orbit. dx,dy are pixel deltas.
pub fn mouse_look(dx: f32, dy: f32) {
    let s = 0.010; // mouse-drag orbit/look sensitivity
    if FOLLOW.load(Ordering::Relaxed) {
        let fp = FIRST_PERSON.load(Ordering::Relaxed);
        let ny = getf(&ORBIT_YAW) + dx * s;
        setf(&ORBIT_YAW, if fp { ny.clamp(-FP_CONE_YAW, FP_CONE_YAW) } else { ny });
        let plim = if fp { FP_CONE_PITCH } else { ORBIT_PITCH_LIMIT };
        let p = (getf(&ORBIT_PITCH) - dy * s).clamp(-plim, plim);
        setf(&ORBIT_PITCH, p);
    } else {
        setf(&YAW, getf(&YAW) + dx * s);
        let p = (getf(&PITCH) - dy * s).clamp(-PITCH_LIMIT, PITCH_LIMIT);
        setf(&PITCH, p);
    }
}

/// Mouse wheel → zoom (follow: orbit distance; free-fly: dolly along forward).
pub fn mouse_zoom(notches: f32) {
    if FOLLOW.load(Ordering::Relaxed) {
        // zoom step scales with distance → fast across the huge range, fine up close.
        let cur = getf(&DIST);
        let d = (cur - notches * (cur * 0.16 + 0.8)).clamp(DIST_MIN, DIST_MAX); // wheel up = closer (fast)
        setf(&DIST, d);
    } else {
        let f = freefly_forward();
        let step = notches * 2.7;
        setf(&PX, getf(&PX) + f.x * step);
        setf(&PY, getf(&PY) + f.y * step);
        setf(&PZ, getf(&PZ) + f.z * step);
    }
}

fn reset() {
    setf(&PX, DEF_POS.0);
    setf(&PY, DEF_POS.1);
    setf(&PZ, DEF_POS.2);
    setf(&YAW, std::f32::consts::PI);
    setf(&PITCH, 0.0);
    load_default_pose();
}

// Built-in per-circuit chase poses [track id, dist, yaw, pitch, eyeH] shipped with the MOD, so
// these courses already frame well out of the box. A pose the user saves with P (in settings)
// takes priority; unlisted courses use the generic default. Users can override any with P.
const BUILTIN_POSES: &[(i32, f32, f32, f32, f32)] = &[
    (10002, 63.57, 3.145, 0.170, 1.0), // Hakodate
    (10005, 49.06, 3.155, 0.120, 1.0), // Nakayama
    (10006, 51.53, 6.285, 0.210, 1.0), // Tokyo
    (10008, 69.12, 3.135, 0.180, 1.0), // Kyoto
    (10009, 49.06, 3.155, 0.080, 1.0), // Hanshin
    (10101, 63.57, 3.165, 0.120, 1.0), // Oi
];

/// Index of the active camera preset for the current circuit (cycled with O).
static ACTIVE_PRESET: AtomicUsize = AtomicUsize::new(0);
/// Whether this race's default pose has been applied (once the track id is known).
static RACE_POSE_LOADED: AtomicBool = AtomicBool::new(false);

fn set_pose(dist: f32, yaw: f32, pitch: f32, eyeh: f32) {
    setf(&DIST, dist);
    setf(&ORBIT_YAW, yaw);
    setf(&ORBIT_PITCH, pitch);
    setf(&EYE_H, eyeh);
}

/// Load the chase pose for the CURRENT circuit at race start. Priority: the circuit's DEFAULT
/// preset (user) → the MOD's built-in pose for that course → the generic default.
fn load_default_pose() {
    let tid = crate::race::track_id();
    ACTIVE_PRESET.store(crate::settings::cam_default_idx(tid), Ordering::Relaxed);
    let src;
    let (dist, yaw, pitch, eyeh) = if let Some(p) = crate::settings::cam_default_pose(tid) {
        src = "preset";
        p
    } else if let Some(p) = BUILTIN_POSES.iter().find(|p| p.0 == tid) {
        src = "builtin";
        (p.1, p.2, p.3, p.4)
    } else {
        src = "generic";
        (DEF_DIST, DEF_YAW, DEF_PITCH, DEF_EYE_H)
    };
    set_pose(dist, yaw, pitch, eyeh);
    flog(&format!("[freecam] load pose track={tid} src={src} idx={} dist={dist:.1}", ACTIVE_PRESET.load(Ordering::Relaxed)));
}

/// Apply a preset (by index) of the current circuit to the live camera.
fn apply_preset(idx: usize) {
    let tid = crate::race::track_id();
    let ps = crate::settings::cam_presets(tid);
    if let Some(p) = ps.get(idx) {
        ACTIVE_PRESET.store(idx, Ordering::Relaxed);
        set_pose(p.dist, p.yaw, p.pitch, p.eyeh);
    }
}

/// O key: cycle to the next preset of the current circuit and apply it live.
fn cycle_preset() {
    let tid = crate::race::track_id();
    let n = crate::settings::cam_presets(tid).len();
    if n == 0 {
        return;
    }
    let next = (ACTIVE_PRESET.load(Ordering::Relaxed) + 1) % n;
    apply_preset(next);
}

fn freefly_forward() -> V3 {
    let yaw = getf(&YAW);
    let pitch = getf(&PITCH);
    let cp = pitch.cos();
    V3 { x: yaw.sin() * cp, y: pitch.sin(), z: yaw.cos() * cp }
}

/// Camera position this frame (follow → orbiting the chosen Uma; else free-fly).
fn current_pos() -> V3 {
    if FOLLOW.load(Ordering::Relaxed) && EVER_CAP.load(Ordering::Relaxed) {
        if FIRST_PERSON.load(Ordering::Relaxed) {
            // at the Uma's head: model origin + forward*FP_FWD, raised to eye height
            return V3 {
                x: getf(&TPX) + getf(&FX) * FP_FWD,
                y: getf(&TPY) + getf(&EYE_H),
                z: getf(&TPZ) + getf(&FZ) * FP_FWD,
            };
        }
        let eh = getf(&EYE_H);
        let fx = getf(&FX);
        let fz = getf(&FZ);
        let heading = fx.atan2(fz);
        let oa = heading + std::f32::consts::PI + getf(&ORBIT_YAW); // yaw=0 → behind her
        let op = getf(&ORBIT_PITCH);
        let dist = getf(&DIST);
        let cp = op.cos();
        V3 {
            x: getf(&TPX) + cp * oa.sin() * dist,
            y: getf(&TPY) + eh + op.sin() * dist,
            z: getf(&TPZ) + cp * oa.cos() * dist,
        }
    } else {
        V3 { x: getf(&PX), y: getf(&PY), z: getf(&PZ) }
    }
}

fn current_lookat() -> V3 {
    if FOLLOW.load(Ordering::Relaxed) && EVER_CAP.load(Ordering::Relaxed) {
        if FIRST_PERSON.load(Ordering::Relaxed) {
            // look forward (Uma heading ± clamped cone), never panning into the void
            let p = current_pos();
            let heading = getf(&FX).atan2(getf(&FZ));
            let yaw = heading + getf(&ORBIT_YAW).clamp(-FP_CONE_YAW, FP_CONE_YAW);
            let pitch = getf(&ORBIT_PITCH).clamp(-FP_CONE_PITCH, FP_CONE_PITCH);
            let cp = pitch.cos();
            return V3 {
                x: p.x + yaw.sin() * cp * LOOK_RADIUS,
                y: p.y + pitch.sin() * LOOK_RADIUS,
                z: p.z + yaw.cos() * cp * LOOK_RADIUS,
            };
        }
        let eh = getf(&EYE_H);
        let fl = 1.5; // aim just ahead of her head → she stays framed, track beyond
        V3 {
            x: getf(&TPX) + getf(&FX) * fl,
            y: getf(&TPY) + eh + getf(&FY) * fl,
            z: getf(&TPZ) + getf(&FZ) * fl,
        }
    } else {
        let p = current_pos();
        let f = freefly_forward();
        V3 { x: p.x + f.x * LOOK_RADIUS, y: p.y + f.y * LOOK_RADIUS, z: p.z + f.z * LOOK_RADIUS }
    }
}

/// Unity-style LookRotation: forward (+ up=Y) → quaternion [x,y,z,w].
fn look_rotation(f: V3) -> [f32; 4] {
    let fl = (f.x * f.x + f.y * f.y + f.z * f.z).sqrt();
    if fl < 1e-5 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    let fwd = V3 { x: f.x / fl, y: f.y / fl, z: f.z / fl };
    // right = normalize(cross(up, forward)), up=(0,1,0)
    let (mut rx, ry, mut rz) = (fwd.z, 0.0f32, -fwd.x);
    let rl = (rx * rx + ry * ry + rz * rz).sqrt();
    if rl < 1e-5 {
        rx = 1.0;
        rz = 0.0;
    } else {
        rx /= rl;
        rz /= rl;
    }
    // up2 = cross(forward, right)
    let ux = fwd.y * rz - fwd.z * ry;
    let uy = fwd.z * rx - fwd.x * rz;
    let uz = fwd.x * ry - fwd.y * rx;
    let (m00, m01, m02) = (rx, ux, fwd.x);
    let (m10, m11, m12) = (ry, uy, fwd.y);
    let (m20, m21, m22) = (rz, uz, fwd.z);
    let tr = m00 + m11 + m22;
    if tr > 0.0 {
        let s = (tr + 1.0).sqrt() * 2.0;
        [(m21 - m12) / s, (m02 - m20) / s, (m10 - m01) / s, 0.25 * s]
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).sqrt() * 2.0;
        [0.25 * s, (m01 + m10) / s, (m02 + m20) / s, (m21 - m12) / s]
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).sqrt() * 2.0;
        [(m01 + m10) / s, 0.25 * s, (m12 + m21) / s, (m02 - m20) / s]
    } else {
        let s = (1.0 + m22 - m00 - m11).sqrt() * 2.0;
        [(m02 + m20) / s, (m12 + m21) / s, 0.25 * s, (m10 - m01) / s]
    }
}

// ── FP fog (mask the void) ────────────────────────────────────────────────────
// Each helper loads a resolved RenderSettings static setter (Method) and calls it as
// f(value, methodInfo) — same convention as graphics.rs.
unsafe fn fog_b(slot: &AtomicUsize, v: bool) {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() { return; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return; }
    let f: extern "C" fn(bool, *const c_void) = std::mem::transmute(p);
    f(v, m as *const c_void);
}
unsafe fn fog_i(slot: &AtomicUsize, v: i32) {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() { return; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return; }
    let f: extern "C" fn(i32, *const c_void) = std::mem::transmute(p);
    f(v, m as *const c_void);
}
unsafe fn fog_f(slot: &AtomicUsize, v: f32) {
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() { return; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return; }
    let f: extern "C" fn(f32, *const c_void) = std::mem::transmute(p);
    f(v, m as *const c_void);
}
unsafe fn fog_color(slot: &AtomicUsize, rgba: &[f32; 4]) {
    // Color (16 bytes) is passed by reference on win64 for a static method.
    let m = slot.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() { return; }
    let p = il2cpp::method_pointer(m);
    if p.is_null() { return; }
    let f: extern "C" fn(*const [f32; 4], *const c_void) = std::mem::transmute(p);
    f(rgba as *const [f32; 4], m as *const c_void);
}
/// Enable/disable linear distance fog (runs on the game thread, il2cpp-attached).
unsafe fn apply_fp_fog(on: bool) {
    if on {
        fog_color(&SET_FOGCOLOR, &FOG_RGBA);
        fog_i(&SET_FOGMODE, FOG_MODE_LINEAR);
        fog_f(&SET_FOGSTART, FOG_START);
        fog_f(&SET_FOGEND, FOG_END);
        fog_b(&SET_FOG, true);
    } else {
        fog_b(&SET_FOG, false);
    }
}

/// Set the near clip plane on our camera (instance method: f(cam, value, methodInfo)).
unsafe fn set_near_clip(cam: *mut c_void, v: f32) {
    let m = SET_NEARCLIP.load(Ordering::Relaxed) as il2cpp::Method;
    if m.is_null() || cam.is_null() {
        return;
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return;
    }
    let f: extern "C" fn(*mut c_void, f32, *const c_void) = std::mem::transmute(p);
    f(cam, v, m as *const c_void);
}

// ── icall / method typedefs ───────────────────────────────────────────────────
type VoidM = unsafe extern "C" fn(*mut c_void, *mut c_void);
type FloatM = unsafe extern "C" fn(*mut c_void, *mut c_void) -> f32;
type CtorM = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void);
type GateM = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type SetPosIcall = unsafe extern "C" fn(*mut c_void, *mut V3);
type SetRotIcall = unsafe extern "C" fn(*mut c_void, *mut [f32; 4]);
type LookAtIcall = unsafe extern "C" fn(*mut c_void, *mut V3, *mut V3);

// "in a race" recency gate (RaceCameraManager only updates during races).
static LAST_RACE_MS: AtomicU64 = AtomicU64::new(0);
fn clock() -> &'static std::time::Instant {
    crate::tools::clock()
}
fn mark_race() {
    LAST_RACE_MS.store(clock().elapsed().as_millis() as u64, Ordering::Relaxed);
}
/// True while the race camera is actively updating (races only). Read by `race_director` for the
/// telemetry HUD's "still in the race scene" fallback.
pub fn in_race() -> bool {
    (clock().elapsed().as_millis() as u64).saturating_sub(LAST_RACE_MS.load(Ordering::Relaxed)) < 300
}

// trampolines (original fn ptr) + kept detours
static TR_LATE: AtomicUsize = AtomicUsize::new(0);
static TR_VIEW: AtomicUsize = AtomicUsize::new(0);
static TR_SETPOS: AtomicUsize = AtomicUsize::new(0);
static TR_SETLOCAL: AtomicUsize = AtomicUsize::new(0);
static TR_SETROT: AtomicUsize = AtomicUsize::new(0);
static TR_LOOKAT: AtomicUsize = AtomicUsize::new(0);
static TR_MOTION: AtomicUsize = AtomicUsize::new(0);
static TR_CTOR: AtomicUsize = AtomicUsize::new(0);
static TR_CMODE: AtomicUsize = AtomicUsize::new(0);
static TR_PEC: AtomicUsize = AtomicUsize::new(0);
static TR_UCDBR: AtomicUsize = AtomicUsize::new(0);
static TR_UFOV: AtomicUsize = AtomicUsize::new(0);
static TR_SETEN: AtomicUsize = AtomicUsize::new(0);
static TR_SETACTIVE: AtomicUsize = AtomicUsize::new(0);

// DIAGNOSTIC (skill-aura hunt): dedup-log effect-ish object names activated in-race, via
// either GameObject.SetActive(true) or Behaviour.set_enabled(true). Reveals the name of
// the speed-line/aura effect so we can target it.
static FX_SEEN: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
fn fx_seen() -> &'static Mutex<std::collections::HashSet<String>> {
    FX_SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
static FX_FRAME: AtomicU32 = AtomicU32::new(0); // race-frame counter for timing correlation
// When true, fx_log records EVERY activated object (deduped). Kept OFF in shipping
// builds — it logged every GameObject name to disk during races (perf + log spam).
// Flip true locally only when hunting a specific prefab.
static FX_LOG_ALL: AtomicBool = AtomicBool::new(false);
fn fx_log(kind: &str, name: &str) {
    if !FX_LOG_ALL.load(Ordering::Relaxed) {
        let l = name.to_lowercase();
        let hit = ["ef", "fx", "skill", "aura", "line", "speed", "particle", "glow", "trail", "effect", "spark", "flash", "wind", "dust"]
            .iter()
            .any(|k| l.contains(k));
        if !hit {
            return;
        }
    }
    let key = format!("{kind}:{name}");
    let newk = fx_seen().lock().map(|mut s| s.insert(key.clone())).unwrap_or(false);
    if newk {
        flog(&format!("[freecam] FX f={} {key}", FX_FRAME.load(Ordering::Relaxed)));
    }
}

static D_LATE: OnceLock<RawDetour> = OnceLock::new();
static D_VIEW: OnceLock<RawDetour> = OnceLock::new();
static D_SETPOS: OnceLock<RawDetour> = OnceLock::new();
static D_SETLOCAL: OnceLock<RawDetour> = OnceLock::new();
static D_SETROT: OnceLock<RawDetour> = OnceLock::new();
static D_LOOKAT: OnceLock<RawDetour> = OnceLock::new();
static D_MOTION: OnceLock<RawDetour> = OnceLock::new();
static D_CTOR: OnceLock<RawDetour> = OnceLock::new();
static D_CMODE: OnceLock<RawDetour> = OnceLock::new();
static D_PEC: OnceLock<RawDetour> = OnceLock::new();
static D_UCDBR: OnceLock<RawDetour> = OnceLock::new();
static D_UFOV: OnceLock<RawDetour> = OnceLock::new();
static D_SETEN: OnceLock<RawDetour> = OnceLock::new();
static D_SETACTIVE: OnceLock<RawDetour> = OnceLock::new();
// RaceRendererVisibilitySwitcher neutralizer (first-person void fix).
static D_VISSW: OnceLock<RawDetour> = OnceLock::new();
static TR_VISSW: AtomicUsize = AtomicUsize::new(0);
static RESET_RENDERER_FN: AtomicUsize = AtomicUsize::new(0); // ResetRenderer code ptr
static RESET_RENDERER_MI: AtomicUsize = AtomicUsize::new(0); // ResetRenderer MethodInfo*
// FP fog (masks the void on elevated courses). UnityEngine.RenderSettings static setters.
static SET_FOG: AtomicUsize = AtomicUsize::new(0);      // set_fog(bool)
static SET_FOGMODE: AtomicUsize = AtomicUsize::new(0);  // set_fogMode(FogMode int)
static SET_FOGSTART: AtomicUsize = AtomicUsize::new(0); // set_fogStartDistance(float)
static SET_FOGEND: AtomicUsize = AtomicUsize::new(0);   // set_fogEndDistance(float)
static SET_FOGCOLOR: AtomicUsize = AtomicUsize::new(0); // set_fogColor(Color)
static FOG_APPLIED: AtomicBool = AtomicBool::new(false);
const FOG_MODE_LINEAR: i32 = 3;
const FOG_START: f32 = 90.0;  // near scenery stays clear up to here
const FOG_END: f32 = 450.0;   // void fades to fog color by here
const FOG_RGBA: [f32; 4] = [0.62, 0.67, 0.74, 1.0]; // light sky-grey
// Near-clip reduction: the game's RaceCourseCamera has a large near plane (fine for the far
// chase). When you zoom in toward first-person, that plane clips the Umas + nearby ground →
// they vanish and the clear color (navy "void") shows. A tiny near clip lets close geometry
// render → no disappearing / no void when close.
static SET_NEARCLIP: AtomicUsize = AtomicUsize::new(0); // Camera.set_nearClipPlane(float)
const NEAR_CLIP: f32 = 0.03;

// ── DIAGNOSTIC (start-dash camera director) — remove when done ─────────────────
static CAM_GET_ALL: AtomicUsize = AtomicUsize::new(0);
static CAM_GET_ALL_MI: AtomicUsize = AtomicUsize::new(0);
static OBJ_GETNAME_ICALL: AtomicUsize = AtomicUsize::new(0);
static COMP_GET_TF: AtomicUsize = AtomicUsize::new(0);
static COMP_GET_TF_MI: AtomicUsize = AtomicUsize::new(0);
static GET_POS_ICALL: AtomicUsize = AtomicUsize::new(0);
static CAM_GET_DEPTH: AtomicUsize = AtomicUsize::new(0);
static BEH_GET_ENABLED: AtomicUsize = AtomicUsize::new(0);
static BEH_SET_ENABLED: AtomicUsize = AtomicUsize::new(0);
static CAM_GET_FOV: AtomicUsize = AtomicUsize::new(0);
/// Height above the model origin for the head marker (clears the Uma's head).
const MARK_HEAD_H: f32 = 2.3;
static RACE_CAM_OBJ: AtomicUsize = AtomicUsize::new(0); // cached RaceCourseCamera object
static EFFECT_OBJ: AtomicUsize = AtomicUsize::new(0); // cached RaceEnvEffect camera (skill-aura suspect)
// TEST: disable RaceEnvEffect while following to see if the skill speed-line "aura"
// that smears the close chase comes from it. Flip false to revert.
static TEST_KILL_ENVEFFECT: AtomicBool = AtomicBool::new(false);
// TEST: suppress skill-aura / speed-line effect objects (pfb_eff_com_skill / eps_line /
// eps_wind) while following, to stop them smearing the close chase. Cut-in untouched.
static TEST_KILL_AURA: AtomicBool = AtomicBool::new(true);
// Keep OUR chase camera (RaceCourseCamera) on screen during the mid-race director
// switches. The game swaps in `MultiCamera0` (overlay, depth 2) and `EventCamera`
// (replaces RaceCourseCamera → the feet/tail close-up). We disable those and re-enable
// RaceCourseCamera so our chase keeps rendering. EXCEPTION: skill cut-ins (CutInCamera
// & friends) are left completely alone (skill cut-ins stay).
unsafe fn tame_cameras() {
    let ga = CAM_GET_ALL.load(Ordering::Relaxed);
    let gn = OBJ_GETNAME_ICALL.load(Ordering::Relaxed);
    let se = BEH_SET_ENABLED.load(Ordering::Relaxed);
    if ga == 0 || gn == 0 || se == 0 {
        return;
    }
    let f_all: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(ga);
    let arr = f_all(CAM_GET_ALL_MI.load(Ordering::Relaxed) as *mut c_void);
    if arr.is_null() {
        return;
    }
    let count = (*((arr as *const u8).add(0x18) as *const usize)).min(16);
    let elems = (arr as *const u8).add(0x20) as *const *mut c_void;
    let f_name: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(gn);
    let f_en: unsafe extern "C" fn(*mut c_void, bool, *mut c_void) = std::mem::transmute(se);

    // Disable the establishing/event cameras (MultiCamera*, EventCamera) and re-assert
    // RaceCourseCamera EVERY frame. We do NOT touch the skill cut-in cameras (CutIn*,
    // EffectCamera) — those sit at a higher depth and keep rendering over our chase, so
    // the skill animation still plays while the establishing close-ups are suppressed.
    for i in 0..count {
        let cam = *elems.add(i);
        if cam.is_null() {
            continue;
        }
        let name = il2cpp::read_string(f_name(cam));
        if name == "RaceCourseCamera" {
            RACE_CAM_OBJ.store(cam as usize, Ordering::Relaxed);
        } else if name.starts_with("MultiCamera") || name == "EventCamera" {
            f_en(cam, false, std::ptr::null_mut());
        }
    }
    // re-assert our chase camera (it may have been disabled to swap in EventCamera, so
    // it won't appear in the enumeration above → use the cached object pointer).
    let rc = RACE_CAM_OBJ.load(Ordering::Relaxed);
    if rc != 0 {
        f_en(rc as *mut c_void, true, std::ptr::null_mut());
        // Its transform is stale (the game was driving EventCamera, not us), so snap it
        // to the chase pose NOW — otherwise it shows a 1-2 frame jump when re-enabled.
        let gt = COMP_GET_TF.load(Ordering::Relaxed);
        let sp = TR_SETPOS.load(Ordering::Relaxed);
        let sr = TR_SETROT.load(Ordering::Relaxed);
        if gt != 0 && sp != 0 && sr != 0 {
            let f_tf: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(gt);
            let tf = f_tf(rc as *mut c_void, COMP_GET_TF_MI.load(Ordering::Relaxed) as *mut c_void);
            if !tf.is_null() {
                let p = current_pos();
                let l = current_lookat();
                let mut pv = p;
                let f_setpos: SetPosIcall = std::mem::transmute(sp);
                f_setpos(tf, &mut pv);
                let fwd = V3 { x: l.x - p.x, y: l.y - p.y, z: l.z - p.z };
                let mut q = look_rotation(fwd);
                let f_setrot: SetRotIcall = std::mem::transmute(sr);
                f_setrot(tf, &mut q);
            }
        }
    }
}
static STARTCAM_FRAMES: AtomicU32 = AtomicU32::new(0);
static CAMSET_HASH: AtomicU64 = AtomicU64::new(0);
static LAST_CAM_MODE: AtomicI64 = AtomicI64::new(-999);
// RaceCameraManager instance + method pointers (for riding the game's own Player Camera
// and killing the spurt radial blur).
static CAM_MGR: AtomicUsize = AtomicUsize::new(0);
static M_DISABLE_BLUR: AtomicUsize = AtomicUsize::new(0);
static M_PLAY_PCAM: AtomicUsize = AtomicUsize::new(0);
static M_STOP_PCAM: AtomicUsize = AtomicUsize::new(0);
static M_IS_PCAM: AtomicUsize = AtomicUsize::new(0);
static M_GET_CUR_CAM: AtomicUsize = AtomicUsize::new(0);
// Log the active-camera set ONLY when it changes (a camera appears/disappears) across
// the WHOLE race, so we capture the mid-race director switches ("se cambia sola"), not
// just the start. Each change dumps name/enabled/depth/pos + distance to the Uma.
unsafe fn log_start_cams() {
    let ga = CAM_GET_ALL.load(Ordering::Relaxed);
    let gn = OBJ_GETNAME_ICALL.load(Ordering::Relaxed);
    let gt = COMP_GET_TF.load(Ordering::Relaxed);
    let gp = GET_POS_ICALL.load(Ordering::Relaxed);
    if ga == 0 || gn == 0 {
        return;
    }
    let f_all: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(ga);
    let arr = f_all(CAM_GET_ALL_MI.load(Ordering::Relaxed) as *mut c_void);
    if arr.is_null() {
        return;
    }
    let count = (*((arr as *const u8).add(0x18) as *const usize)).min(16);
    let elems = (arr as *const u8).add(0x20) as *const *mut c_void;
    let f_name: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(gn);
    let gd = CAM_GET_DEPTH.load(Ordering::Relaxed);
    let ge = BEH_GET_ENABLED.load(Ordering::Relaxed);

    // Build a cheap hash of (name + enabled) to detect set changes.
    let mut hash: u64 = count as u64;
    let mut names: Vec<(String, bool, f32, *mut c_void)> = Vec::with_capacity(count);
    for i in 0..count {
        let cam = *elems.add(i);
        if cam.is_null() {
            continue;
        }
        let name = il2cpp::read_string(f_name(cam));
        let enabled = if ge != 0 { let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool = std::mem::transmute(ge); f(cam, std::ptr::null_mut()) } else { false };
        let depth = if gd != 0 { let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> f32 = std::mem::transmute(gd); f(cam, std::ptr::null_mut()) } else { 0.0 };
        for b in name.bytes() {
            hash = hash.wrapping_mul(31).wrapping_add(b as u64);
        }
        hash = hash.wrapping_mul(2).wrapping_add(enabled as u64);
        names.push((name, enabled, depth, cam));
    }
    if hash == CAMSET_HASH.load(Ordering::Relaxed) {
        return; // no change → don't spam
    }
    CAMSET_HASH.store(hash, Ordering::Relaxed);

    let frame = STARTCAM_FRAMES.fetch_add(1, Ordering::Relaxed);
    let (ux, uy, uz) = (getf(&TPX), getf(&TPY), getf(&TPZ));
    flog(&format!("[freecam] CAMSET change #{frame} everCap={} cams={count}", EVER_CAP.load(Ordering::Relaxed)));
    for (name, enabled, depth, cam) in &names {
        let (mut px, mut py, mut pz, mut sep) = (0.0f32, 0.0f32, 0.0f32, -1.0f32);
        if gt != 0 && gp != 0 {
            let f_tf: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(gt);
            let tf = f_tf(*cam, COMP_GET_TF_MI.load(Ordering::Relaxed) as *mut c_void);
            if !tf.is_null() {
                let f_pos: unsafe extern "C" fn(*mut c_void, *mut V3) = std::mem::transmute(gp);
                let mut p = V3 { x: 0.0, y: 0.0, z: 0.0 };
                f_pos(tf, &mut p);
                px = p.x; py = p.y; pz = p.z;
                let (dx, dy, dz) = (px - ux, py - uy, pz - uz);
                sep = (dx * dx + dy * dy + dz * dz).sqrt();
            }
        }
        flog(&format!("[freecam]   '{name}' en={enabled} depth={depth:.0} pos=({px:.0},{py:.0},{pz:.0}) sepUma={sep:.0}"));
    }
}

// follow plumbing: field offsets + gate map
static POS_OFF: AtomicUsize = AtomicUsize::new(0);
static ROT_OFF: AtomicUsize = AtomicUsize::new(0);


/// P key: save the current pose into the ACTIVE preset of this circuit. If the circuit has no
/// presets yet, create the first one. Persisted per circuit.
fn save_active_preset() {
    let tid = crate::race::track_id();
    if tid == 0 {
        flog("[freecam] save skipped (no track id yet)");
        return;
    }
    let (dist, yaw, pitch, eyeh) = (getf(&DIST), getf(&ORBIT_YAW), getf(&ORBIT_PITCH), getf(&EYE_H));
    let n = crate::settings::cam_presets(tid).len();
    if n == 0 {
        if let Some(idx) = crate::settings::cam_add_preset(tid, "Preset 1", dist, yaw, pitch, eyeh) {
            ACTIVE_PRESET.store(idx, Ordering::Relaxed);
        }
    } else {
        let idx = ACTIVE_PRESET.load(Ordering::Relaxed).min(n - 1);
        crate::settings::cam_update_preset(tid, idx, dist, yaw, pitch, eyeh);
        ACTIVE_PRESET.store(idx, Ordering::Relaxed);
    }
    flog(&format!("[freecam] saved preset {} for track {tid}", ACTIVE_PRESET.load(Ordering::Relaxed)));
}

// ── preset management API (overlay calls these for the current circuit) ─────────
pub fn preset_track() -> i32 {
    crate::race::track_id()
}
/// Names of the current circuit's presets.
pub fn preset_names() -> Vec<String> {
    crate::settings::cam_presets(crate::race::track_id()).into_iter().map(|p| p.name).collect()
}
pub fn preset_active() -> usize {
    ACTIVE_PRESET.load(Ordering::Relaxed)
}
pub fn preset_default() -> usize {
    crate::settings::cam_default_idx(crate::race::track_id())
}
/// Add the current pose as a new named preset (capped). Returns true if added.
pub fn preset_add(name: &str) -> bool {
    let tid = crate::race::track_id();
    if tid == 0 {
        return false;
    }
    if let Some(idx) = crate::settings::cam_add_preset(
        tid, name, getf(&DIST), getf(&ORBIT_YAW), getf(&ORBIT_PITCH), getf(&EYE_H),
    ) {
        ACTIVE_PRESET.store(idx, Ordering::Relaxed);
        true
    } else {
        false
    }
}
pub fn preset_apply_idx(idx: usize) {
    apply_preset(idx);
}
pub fn preset_rename(idx: usize, name: &str) {
    crate::settings::cam_rename_preset(crate::race::track_id(), idx, name);
}
pub fn preset_delete(idx: usize) {
    let tid = crate::race::track_id();
    crate::settings::cam_delete_preset(tid, idx);
    let n = crate::settings::cam_presets(tid).len();
    if n > 0 {
        ACTIVE_PRESET.store(ACTIVE_PRESET.load(Ordering::Relaxed).min(n - 1), Ordering::Relaxed);
    }
}
pub fn preset_set_default(idx: usize) {
    crate::settings::cam_set_default(crate::race::track_id(), idx);
}
/// Overwrite the active preset with the current pose (P key behaviour, exposed for a menu button).
pub fn preset_save_active() {
    save_active_preset();
}

static GATENO_CODE: AtomicUsize = AtomicUsize::new(0);
static GATENO_MI: AtomicUsize = AtomicUsize::new(0);
static GATE_MAP: OnceLock<Mutex<HashMap<usize, i32>>> = OnceLock::new();
fn gate_map() -> &'static Mutex<HashMap<usize, i32>> {
    GATE_MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Override the camera transform this call? Only inside the race-camera bracket AND
/// once the Uma is captured (before capture we leave the game's intro camera alone).
#[inline]
fn drive_cam() -> bool {
    UPDATE_RACE_CAM.load(Ordering::Relaxed)
        && FOLLOW.load(Ordering::Relaxed)
        && EVER_CAP.load(Ordering::Relaxed)
}

// Transform of RaceCourseCamera (our chase camera), cached from the set_enabled hook.
static RACE_CAM_TF: AtomicUsize = AtomicUsize::new(0);

/// Find RaceCourseCamera by name and cache its Camera object + transform, so the FOV
/// lock and pose pin work from the moment we start following (not only after the game
/// first toggles it at a skill). Idempotent; cheap (runs only while not yet cached).
unsafe fn cache_race_camera() {
    if RACE_CAM_OBJ.load(Ordering::Relaxed) != 0 {
        return;
    }
    let ga = CAM_GET_ALL.load(Ordering::Relaxed);
    let gn = OBJ_GETNAME_ICALL.load(Ordering::Relaxed);
    let gt = COMP_GET_TF.load(Ordering::Relaxed);
    if ga == 0 || gn == 0 {
        return;
    }
    let f_all: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(ga);
    let arr = f_all(CAM_GET_ALL_MI.load(Ordering::Relaxed) as *mut c_void);
    if arr.is_null() {
        return;
    }
    let count = (*((arr as *const u8).add(0x18) as *const usize)).min(16);
    let elems = (arr as *const u8).add(0x20) as *const *mut c_void;
    let f_name: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(gn);
    for i in 0..count {
        let cam = *elems.add(i);
        if cam.is_null() {
            continue;
        }
        let nm = il2cpp::read_string(f_name(cam));
        if nm == "RaceEnvEffect" {
            EFFECT_OBJ.store(cam as usize, Ordering::Relaxed);
        } else if nm == "RaceCourseCamera" {
            RACE_CAM_OBJ.store(cam as usize, Ordering::Relaxed);
            if gt != 0 {
                let f_tf: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(gt);
                let tf = f_tf(cam, COMP_GET_TF_MI.load(Ordering::Relaxed) as *mut c_void);
                RACE_CAM_TF.store(tf as usize, Ordering::Relaxed);
            }
            dump_camera_components(cam); // DIAGNOSTIC: list image-effect components (spurt blur)
        }
    }
}

static COMPS_DUMPED: AtomicBool = AtomicBool::new(false);
/// DIAGNOSTIC: log the il2cpp class name of every component on RaceCourseCamera's
/// GameObject (once). The spurt motion-blur is an always-on image effect (no enable
/// toggle in the log) → this reveals its component type so we can disable it.
unsafe fn dump_camera_components(cam: *mut c_void) {
    if COMPS_DUMPED.swap(true, Ordering::Relaxed) || cam.is_null() {
        return;
    }
    // gameObject = Component.get_gameObject()
    let m_go = il2cpp::method(il2cpp::class("UnityEngine.Component"), "get_gameObject", 0);
    if m_go.is_null() {
        flog("[freecam] CAMCOMPS bail: get_gameObject method null");
        return;
    }
    let go = il2cpp::runtime_invoke(m_go, cam, &mut []);
    if go.is_null() {
        flog("[freecam] CAMCOMPS bail: gameObject null");
        return;
    }
    // typeof(Component) → GetComponents(Type) returns Component[]
    let comp_type = il2cpp::type_object(il2cpp::class("UnityEngine.Component"));
    let m_gc = il2cpp::method(il2cpp::class("UnityEngine.GameObject"), "GetComponents", 1);
    if comp_type.is_null() || m_gc.is_null() {
        flog(&format!("[freecam] CAMCOMPS bail: comp_type_null={} GetComponents_null={}", comp_type.is_null(), m_gc.is_null()));
        return;
    }
    // runtime_invoke wants params[i] = pointer to the arg slot even for reference types →
    // pass &comp_type (address of the Type object pointer), not comp_type directly.
    let mut ct = comp_type;
    let mut args = [&mut ct as *mut il2cpp::Object as *mut c_void];
    let arr = il2cpp::runtime_invoke(m_gc, go, &mut args);
    if arr.is_null() {
        flog("[freecam] CAMCOMPS bail: GetComponents returned null (tried &Type)");
        return;
    }
    let count = (*((arr as *const u8).add(0x18) as *const usize)).min(64);
    let elems = (arr as *const u8).add(0x20) as *const *mut c_void;
    let mut names = String::new();
    for i in 0..count {
        let c = *elems.add(i);
        if c.is_null() {
            continue;
        }
        names.push_str(&il2cpp::object_class_name(c));
        names.push(' ');
    }
    flog(&format!("[freecam] CAMCOMPS n={count}: {names}"));

    // TEST: kill the navy "terrain void" by changing how RaceCourseCamera clears the
    // background. SolidColor(2)=navy void; Skybox(1)=draw the sky sphere where there's no
    // geometry (incl. below horizon) → no more blue band. Flip CLEAR_MODE to experiment
    // (1=Skybox 2=SolidColor[orig] 3=Depth 4=Nothing).
    let mode = CLEAR_MODE.load(Ordering::Relaxed) as i32;
    if mode != 2 {
        let m_cf = il2cpp::method(il2cpp::class("UnityEngine.Camera"), "set_clearFlags", 1);
        if !m_cf.is_null() {
            let mut flag = mode;
            let mut a = [&mut flag as *mut i32 as *mut c_void];
            il2cpp::runtime_invoke(m_cf, cam, &mut a);
            flog(&format!("[freecam] clearFlags set to {mode} on RaceCourseCamera"));
        }
    }
}
static CLEAR_MODE: AtomicU32 = AtomicU32::new(2); // 2=leave game default (Depth caused white blowout; void handled by looking ALONG the track in FP)

/// Should we override THIS transform's pose to the chase? Inside the bracket (the race
/// camera the manager drives) OR — crucially — out-of-bracket when it's our cached
/// RaceCourseCamera transform. The game repositions RaceCourseCamera to low close-up
/// poses after skills OUTSIDE the bracket; pinning it here keeps the chase steady.
#[inline]
fn drive_this(this: *mut c_void) -> bool {
    if drive_cam() {
        return true;
    }
    let rt = RACE_CAM_TF.load(Ordering::Relaxed);
    rt != 0 && this as usize == rt && following()
}

/// True once we're actively following the captured Uma. The cinematic-cut suppressors
/// gate on this so the game's race-INTRO cinematic (which runs BEFORE we've captured
/// the Uma, and which the game builds via ChangeCameraMode / PlayEventCamera) plays
/// normally — suppressing those during the intro broke it into a stuck close-up.
#[inline]
fn following() -> bool {
    ENABLED.load(Ordering::Relaxed)
        && FOLLOW.load(Ordering::Relaxed)
        && EVER_CAP.load(Ordering::Relaxed)
        && in_race()
}
/// Sticky variant used ONLY by the cinematic-cut suppressors (ChangeCameraMode / PlayEventCamera).
/// The strict `following()` drops the instant `AlterLateUpdate`/`LateUpdateView` pause for >300ms —
/// which they do during skill cut-ins and camera transitions (2-3s). In that gap the game's
/// `ChangeCameraMode` slipped through and grabbed the camera back ("freecam changes on its own").
/// EVER_CAP still gates it (false during the intro → the intro close-up is untouched), but we widen
/// the race-recency window so brief update stalls no longer surrender the view. The HUD keeps using
/// the strict `following()` so it still hides promptly on the result screen.
#[inline]
fn suppress_cuts() -> bool {
    ENABLED.load(Ordering::Relaxed)
        && FOLLOW.load(Ordering::Relaxed)
        && EVER_CAP.load(Ordering::Relaxed)
        && (clock().elapsed().as_millis() as u64).saturating_sub(LAST_RACE_MS.load(Ordering::Relaxed)) < 5000
}
/// The currently-followed gate (the identity the camera owns). Read by `race_director` so the
/// telemetry HUD keys its "followed" panel to the same Uma the camera is chasing.
#[inline]
pub fn followed_gate() -> i32 {
    TARGET_GATE.load(Ordering::Relaxed)
}
/// Gate (post position) of a HorseRaceInfoReplay instance, or -1 if unmapped. Populated in the
/// ctor hook. Read by `race_director` to attribute per-frame telemetry to a lane.
#[inline]
pub fn gate_of(this: *mut c_void) -> i32 {
    gate_map().lock().ok().map(|m| m.get(&(this as usize)).copied().unwrap_or(-1)).unwrap_or(-1)
}
/// HorseRaceInfo._position field offset (camera's captured field). Read by `race_director` for the
/// track-map world position — same field, so no duplicate offset atomic.
#[inline]
pub fn pos_off() -> usize {
    POS_OFF.load(Ordering::Relaxed)
}

// ── bracket hooks ─────────────────────────────────────────────────────────────
unsafe extern "C" fn on_alter_late_update(this: *mut c_void, mi: *mut c_void) {
    mark_race();
    // FP fog: apply when first-person turns on, remove when it turns off.
    {
        let fp = FIRST_PERSON.load(Ordering::Relaxed);
        if fp != FOG_APPLIED.load(Ordering::Relaxed) {
            apply_fp_fog(fp);
            FOG_APPLIED.store(fp, Ordering::Relaxed);
        }
    }
    // Tiny near clip — FP-only now (FP is shelved; keep 3rd-person depth precision untouched).
    if FIRST_PERSON.load(Ordering::Relaxed) {
        let cam = RACE_CAM_OBJ.load(Ordering::Relaxed);
        if cam != 0 {
            set_near_clip(cam as *mut c_void, NEAR_CLIP);
        }
    }
    UPDATE_RACE_CAM.store(ENABLED.load(Ordering::Relaxed), Ordering::Relaxed);
    let t = TR_LATE.load(Ordering::Relaxed);
    if t != 0 {
        let f: VoidM = std::mem::transmute(t);
        f(this, mi);
    }
    UPDATE_RACE_CAM.store(false, Ordering::Relaxed);
    CAM_MGR.store(this as usize, Ordering::Relaxed);
    if following() {
        FX_FRAME.fetch_add(1, Ordering::Relaxed); // DIAGNOSTIC frame counter for FX log
        // Kill the spurt radial blur (smear).
        let db = M_DISABLE_BLUR.load(Ordering::Relaxed);
        if db != 0 {
            let f: VoidM = std::mem::transmute(db);
            f(this, std::ptr::null_mut());
        }
        // 3rd-person → make sure the game's Player Camera is stopped if it happens to be on.
        let is_p = M_IS_PCAM.load(Ordering::Relaxed);
        let playing = if is_p != 0 {
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool = std::mem::transmute(is_p);
            f(this, std::ptr::null_mut())
        } else {
            false
        };
        if playing {
            let sp = M_STOP_PCAM.load(Ordering::Relaxed);
            if sp != 0 {
                let f: VoidM = std::mem::transmute(sp);
                f(this, std::ptr::null_mut());
            }
        }
    }
    // TEST: kill the RaceEnvEffect overlay while following (skill-aura suspect).
    if TEST_KILL_ENVEFFECT.load(Ordering::Relaxed) && following() {
        let e = EFFECT_OBJ.load(Ordering::Relaxed);
        let se = TR_SETEN.load(Ordering::Relaxed);
        if e != 0 && se != 0 {
            let f: unsafe extern "C" fn(*mut c_void, bool, *mut c_void) = std::mem::transmute(se);
            f(e as *mut c_void, false, std::ptr::null_mut());
        }
    }
}

unsafe extern "C" fn on_view_late_update(this: *mut c_void, mi: *mut c_void) {
    mark_race();
    UPDATE_RACE_CAM.store(ENABLED.load(Ordering::Relaxed), Ordering::Relaxed);
    let t = TR_VIEW.load(Ordering::Relaxed);
    if t != 0 {
        let f: VoidM = std::mem::transmute(t);
        f(this, mi);
    }
    UPDATE_RACE_CAM.store(false, Ordering::Relaxed);
}

// UnityEngine.Camera.get_fieldOfView — the game collapses RaceCourseCamera's FOV to
// ~0-3° for dramatic close-ups after skills. Force OUR camera's FOV to a steady value
// (only RaceCourseCamera, by cached object ptr → other cameras / cut-ins untouched).
unsafe extern "C" fn on_unity_get_fov(this: *mut c_void, mi: *mut c_void) -> f32 {
    if following() {
        let rc = RACE_CAM_OBJ.load(Ordering::Relaxed);
        if rc != 0 && this as usize == rc {
            // one-shot component dump + clear-mode apply (guaranteed call site: this hook
            // runs every frame for RaceCourseCamera while following).
            if !COMPS_DUMPED.load(Ordering::Relaxed) {
                dump_camera_components(this);
            }
            return FOLLOW_FOV;
        }
    }
    let t = TR_UFOV.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> f32 = std::mem::transmute(t);
        return f(this, mi);
    }
    0.0
}

// Behaviour.set_enabled(bool) — intercept the camera-director's enable/disable at the
// SOURCE (timing-independent, unlike racing the enable each frame). While following:
//   • MultiCamera* / EventCamera being ENABLED  → force disabled (the close-ups).
//   • RaceCourseCamera being DISABLED            → force enabled (keep our chase).
// Skill cut-in cameras (CutIn*, EffectCamera) are untouched → the skill still plays.
unsafe extern "C" fn on_set_enabled(this: *mut c_void, value: bool, mi: *mut c_void) {
    let mut v = value;
    if following() && !this.is_null() {
        let gn = OBJ_GETNAME_ICALL.load(Ordering::Relaxed);
        if gn != 0 {
            let f_name: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(gn);
            let name = il2cpp::read_string(f_name(this));
            if value {
                // DIAGNOSTIC: log the COMPONENT CLASS (e.g. MotionBlur/RadialBlur) too, not
                // just the GameObject name — to find the spurt motion-blur image effect.
                let cls = il2cpp::object_class_name(this);
                fx_log("EN", &format!("{name}#{cls}"));
            }
            if name.starts_with("MultiCamera") || name == "EventCamera" {
                v = false;
            } else if name == "RaceCourseCamera" {
                v = true;
                RACE_CAM_OBJ.store(this as usize, Ordering::Relaxed); // for the FOV scope
                dump_camera_components(this); // DIAGNOSTIC: list image-effect components (once)
                // cache its transform so we can pin its pose out-of-bracket
                let gt = COMP_GET_TF.load(Ordering::Relaxed);
                if gt != 0 && RACE_CAM_TF.load(Ordering::Relaxed) == 0 {
                    let f_tf: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(gt);
                    let tf = f_tf(this, COMP_GET_TF_MI.load(Ordering::Relaxed) as *mut c_void);
                    RACE_CAM_TF.store(tf as usize, Ordering::Relaxed);
                }
            }
        }
    }
    let t = TR_SETEN.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, bool, *mut c_void) = std::mem::transmute(t);
        f(this, v, mi);
    }
}

// DIAGNOSTIC: GameObject.SetActive(bool) — log effect-ish objects activated in-race
// (the skill aura is likely activated this way). Pass-through (we never modify it).
unsafe extern "C" fn on_set_active(this: *mut c_void, value: bool, mi: *mut c_void) {
    let mut v = value;
    if value && !this.is_null() && following() {
        let gn = OBJ_GETNAME_ICALL.load(Ordering::Relaxed);
        if gn != 0 {
            let f_name: unsafe extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(gn);
            let name = il2cpp::read_string(f_name(this));
            fx_log("ACT", &name);
            // TEST: suppress the skill-aura / speed-line effects that smear the chase,
            // WITHOUT touching the cut-in (cutin / chr* / skillname / flash).
            if TEST_KILL_AURA.load(Ordering::Relaxed)
                && (name.starts_with("pfb_eff_com_skill")
                    || name.starts_with("pfb_eff_eps_line")
                    || name.starts_with("pfb_eff_eps_wind")
                    // final-spurt speed/concentration lines (smear over all runners)
                    || name.starts_with("M_Line"))
            {
                v = false;
            }
        }
    }
    let t = TR_SETACTIVE.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, bool, *mut c_void) = std::mem::transmute(t);
        f(this, v, mi);
    }
}

// ── transform overrides (bracket-scoped) ──────────────────────────────────────
unsafe extern "C" fn on_set_position(this: *mut c_void, value: *mut V3) {
    if drive_this(this) && !value.is_null() {
        *value = current_pos();
    }
    let t = TR_SETPOS.load(Ordering::Relaxed);
    if t != 0 {
        let f: SetPosIcall = std::mem::transmute(t);
        f(this, value);
    }
}

unsafe extern "C" fn on_set_localposition(this: *mut c_void, value: *mut V3) {
    if drive_this(this) && !value.is_null() {
        *value = current_pos();
    }
    let t = TR_SETLOCAL.load(Ordering::Relaxed);
    if t != 0 {
        let f: SetPosIcall = std::mem::transmute(t);
        f(this, value);
    }
}

unsafe extern "C" fn on_set_rotation(this: *mut c_void, q: *mut [f32; 4]) {
    if drive_this(this) && !q.is_null() {
        let p = current_pos();
        let l = current_lookat();
        let fwd = V3 { x: l.x - p.x, y: l.y - p.y, z: l.z - p.z };
        *q = look_rotation(fwd);
    }
    let t = TR_SETROT.load(Ordering::Relaxed);
    if t != 0 {
        let f: SetRotIcall = std::mem::transmute(t);
        f(this, q);
    }
}

unsafe extern "C" fn on_lookat(this: *mut c_void, world_pos: *mut V3, world_up: *mut V3) {
    if drive_this(this) && !world_pos.is_null() {
        *world_pos = current_lookat();
    }
    let t = TR_LOOKAT.load(Ordering::Relaxed);
    if t != 0 {
        let f: LookAtIcall = std::mem::transmute(t);
        f(this, world_pos, world_up);
    }
}

// ── cinematic-cut suppressors ─────────────────────────────────────────────────
// RaceCameraManager.ChangeCameraMode(mode, arg2) — swallow it so the game can't cut
// to its cinematic / multi camera modes while freecam owns the view.
unsafe extern "C" fn on_change_cam_mode(this: *mut c_void, mode: i64, arg2: i64, mi: *mut c_void) {
    // DIAGNOSTIC: log the camera MODE the game requests as the race progresses (to find the
    // "Dueling"/final-stretch mode). TPX tells us roughly where on the track it happened.
    if LAST_CAM_MODE.swap(mode, Ordering::Relaxed) != mode {
        flog(&format!("[freecam] ChangeCameraMode mode={mode} arg2={arg2} tpx={:.0} suppress={} follow={}", getf(&TPX), suppress_cuts(), following()));
    }
    if suppress_cuts() {
        return;
    }
    let t = TR_CMODE.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, i64, i64, *mut c_void) = std::mem::transmute(t);
        f(this, mode, arg2, mi);
    }
}

// RaceCameraManager.PlayEventCamera(...) — returns BOOL (whether an event camera was
// played). Return false while freecam owns the view so no scripted event camera runs.
unsafe extern "C" fn on_play_event_camera(
    this: *mut c_void,
    a0: *mut c_void,
    a1: *mut c_void,
    a2: *mut c_void,
    a3: *mut c_void,
    a4: *mut c_void,
    mi: *mut c_void,
) -> bool {
    if suppress_cuts() {
        return false;
    }
    let t = TR_PEC.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(
            *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void,
        ) -> bool = std::mem::transmute(t);
        return f(this, a0, a1, a2, a3, a4, mi);
    }
    false
}

// RaceRendererVisibilitySwitcher.UpdateRenderer(pos, targetPos) — the per-object occlusion
// culler that disables scenery renderers lying on the camera→target ray. In first-person that
// ray sweeps the course and blanks geometry (the intermittent "void"). While FP is on, force
// the renderer ON (ResetRenderer) instead of running the cull. 3rd-person keeps stock behavior.
unsafe extern "C" fn on_vis_update(this: *mut c_void, pos: *mut c_void, tgt: *mut c_void, mi: *mut c_void) {
    if FIRST_PERSON.load(Ordering::Relaxed) {
        let rr = RESET_RENDERER_FN.load(Ordering::Relaxed);
        if rr != 0 && !this.is_null() {
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void) = std::mem::transmute(rr);
            f(this, RESET_RENDERER_MI.load(Ordering::Relaxed) as *mut c_void); // -> set_enabled(true)
        }
        return;
    }
    let t = TR_VISSW.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) = std::mem::transmute(t);
        f(this, pos, tgt, mi);
    }
}

// RaceModelController.UpdateCameraDistanceBlendRate(p1,p2,p3) — the distance blend
// that drags the view to cinematic establishing shots. Skip while freecam is on.
unsafe extern "C" fn on_update_camera_distance_blend_rate(
    this: *mut c_void,
    p1: *mut c_void,
    p2: *mut c_void,
    p3: *mut c_void,
    mi: *mut c_void,
) {
    if following() {
        return;
    }
    let t = TR_UCDBR.load(Ordering::Relaxed);
    if t != 0 {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void) =
            std::mem::transmute(t);
        f(this, p1, p2, p3, mi);
    }
}

// ── follow capture ────────────────────────────────────────────────────────────
// HorseRaceInfoReplay.get_RunMotionSpeed(this) — per horse, per frame. Read the
// chosen gate's `_position` and `_rotationOnLane` (forward, EMA-smoothed).
unsafe extern "C" fn on_run_motion(this: *mut c_void, mi: *mut c_void) -> f32 {
    let t = TR_MOTION.load(Ordering::Relaxed);
    let ret = if t != 0 {
        let f: FloatM = std::mem::transmute(t);
        f(this, mi)
    } else {
        0.0
    };
    // Collect telemetry when the freecam is following OR the telemetry HUD is on (independent now).
    // The camera-capture part below additionally requires the freecam to be engaged (`fc`).
    let fc = ENABLED.load(Ordering::Relaxed) && FOLLOW.load(Ordering::Relaxed);
    if !(fc || crate::settings::telemetry()) {
        return ret;
    }
    let gate = gate_of(this);
    let target = TARGET_GATE.load(Ordering::Relaxed);
    // Live telemetry for EVERY horse (Race Director reads the followed Uma + its rival from this).
    let course = crate::race::course_distance() as f32;
    crate::race_director::publish_frame(this, gate, target, course);

    // Camera capture below is freecam-only (telemetry needs no camera control).
    if !fc || gate != target {
        return ret;
    }
    // Apply this circuit's DEFAULT preset once the track id is known (it may be 0 at race start,
    // so loading it in auto_follow_player would miss the user's saved pose). Once per race.
    if !RACE_POSE_LOADED.load(Ordering::Relaxed) && crate::race::track_id() != 0 {
        load_default_pose();
        RACE_POSE_LOADED.store(true, Ordering::Relaxed);
    }
    // Followed Uma only: skill feed + active-skill countdown + AI state + last-spurt outlook.
    crate::race_director::update_followed(this);
    let off = POS_OFF.load(Ordering::Relaxed);
    if off == 0 {
        return ret;
    }
    let p = (this as *const u8).add(off) as *const f32;
    let nx = p.read_unaligned();
    let ny = p.add(1).read_unaligned();
    let nz = p.add(2).read_unaligned();

    // forward from the horse's rotation quaternion (Unity layout [x,y,z,w]; fwd = q*(0,0,1))
    let roff = ROT_OFF.load(Ordering::Relaxed);
    let (mut cfx, mut cfy, mut cfz, mut have_fwd) = (0.0f32, 0.0f32, 1.0f32, false);
    if roff != 0 {
        let q = (this as *const u8).add(roff) as *const f32;
        let qx = q.read_unaligned();
        let qy = q.add(1).read_unaligned();
        let qz = q.add(2).read_unaligned();
        let qw = q.add(3).read_unaligned();
        let fx = 2.0 * (qx * qz + qw * qy);
        let fy = 2.0 * (qy * qz - qw * qx);
        let fz = 1.0 - 2.0 * (qx * qx + qy * qy);
        let n = (fx * fx + fy * fy + fz * fz).sqrt();
        if n > 0.1 && n.is_finite() {
            cfx = fx / n;
            cfy = fy / n;
            cfz = fz / n;
            have_fwd = true;
        }
    }

    if !HAVE_TARGET.load(Ordering::Relaxed) {
        if have_fwd {
            setf(&FX, cfx);
            setf(&FY, cfy);
            setf(&FZ, cfz);
        } else {
            setf(&FX, 0.0);
            setf(&FY, 0.0);
            setf(&FZ, 1.0);
        }
        if !DIAG_CAPTURED.swap(true, Ordering::Relaxed) {
            flog(&format!("[freecam] captured gate {gate} at ({nx:.2}, {ny:.2}, {nz:.2})"));
        }
        HAVE_TARGET.store(true, Ordering::Relaxed);
        EVER_CAP.store(true, Ordering::Relaxed);
        // Cache RaceCourseCamera NOW (not at the first skill) so the FOV lock + pose pin
        // are active from the very start → no framing jump when the first skill fires.
        cache_race_camera();
    } else if have_fwd {
        // EMA-smooth toward the rotation forward (kills wobble / corner swing).
        let a = 0.2;
        let sfx = getf(&FX) * (1.0 - a) + cfx * a;
        let sfy = getf(&FY) * (1.0 - a) + cfy * a;
        let sfz = getf(&FZ) * (1.0 - a) + cfz * a;
        let n = (sfx * sfx + sfy * sfy + sfz * sfz).sqrt();
        if n > 1e-4 {
            setf(&FX, sfx / n);
            setf(&FY, sfy / n);
            setf(&FZ, sfz / n);
        }
    }
    setf(&TPX, nx);
    setf(&TPY, ny);
    setf(&TPZ, nz);
    ret
}

// HorseRaceInfoReplay..ctor(this, data, reader) — map instance → gate.
unsafe extern "C" fn on_hri_ctor(this: *mut c_void, data: *mut c_void, reader: *mut c_void, mi: *mut c_void) {
    let t = TR_CTOR.load(Ordering::Relaxed);
    if t != 0 {
        let f: CtorM = std::mem::transmute(t);
        f(this, data, reader, mi);
    }
    let code = GATENO_CODE.load(Ordering::Relaxed);
    if code != 0 && !data.is_null() {
        let g: GateM = std::mem::transmute(code);
        let gate = g(data, GATENO_MI.load(Ordering::Relaxed) as *mut c_void);
        if gate > 0 {
            if let Ok(mut m) = gate_map().lock() {
                m.insert(this as usize, gate);
            }
            if gate > MAX_GATE.load(Ordering::Relaxed) {
                MAX_GATE.store(gate, Ordering::Relaxed);
            }
            // Race Director captures the static-per-race identity (display name + charaId) for the HUD.
            crate::race_director::on_ctor(gate, data);
        }
    }
}

// ── hook install helper ───────────────────────────────────────────────────────
unsafe fn hook_at(target: *const c_void, detour: *const (), tr: &AtomicUsize, keep: &OnceLock<RawDetour>) -> bool {
    if target.is_null() {
        return false;
    }
    if crate::il2cpp::is_detoured(target) {
        return false;
    }
    match RawDetour::new(target as *const (), detour) {
        Ok(d) => {
            if d.enable().is_ok() {
                tr.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
                let _ = keep.set(d);
                return true;
            }
            false
        }
        Err(_) => false,
    }
}

// ── input ─────────────────────────────────────────────────────────────────────
fn vk_down(vk: i32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

/// True only when the game window is the foreground window. Input polling is global,
/// so without this the camera would react while you're alt-tabbed typing elsewhere.
fn game_focused() -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return false;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        pid == GetCurrentProcessId()
    }
}

// Rebind capture: the menu sets this to an RdKey index; the input thread then binds the next key
// the user presses to that action. -1 = not capturing.
static RD_CAPTURE: AtomicI32 = AtomicI32::new(-1);
/// Arm a rebind: the next key pressed becomes action `idx`'s bind (-1 to cancel). Called from the menu.
pub fn rd_capture_start(idx: i32) {
    RD_CAPTURE.store(idx, Ordering::Relaxed);
}
/// Which action is currently waiting for a key (-1 = none). The menu shows "press a key…" for it.
pub fn rd_capturing() -> i32 {
    RD_CAPTURE.load(Ordering::Relaxed)
}

/// First pressed key among the bindable candidates (mouse buttons + bare modifiers excluded).
fn scan_pressed_vk() -> Option<i32> {
    const CAND: &[i32] = &[
        0x08, 0x09, 0x0D, 0x20, // Backspace Tab Enter Space
        0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, // PgUp PgDn End Home arrows
        0x2D, 0x2E, // Insert Delete
    ];
    for &vk in CAND {
        if vk_down(vk) {
            return Some(vk);
        }
    }
    for vk in 0x30..=0x5A {
        if vk_down(vk) {
            return Some(vk);
        }
    } // 0-9, A-Z
    for vk in 0x60..=0x6F {
        if vk_down(vk) {
            return Some(vk);
        }
    } // numpad
    for vk in 0x70..=0x7B {
        if vk_down(vk) {
            return Some(vk);
        }
    } // F1-F12
    for vk in 0xBA..=0xC0 {
        if vk_down(vk) {
            return Some(vk);
        }
    } // ; = , - . / `
    for vk in 0xDB..=0xDE {
        if vk_down(vk) {
            return Some(vk);
        }
    } // [ \ ] '
    None
}

fn input_tick() {
    if !ENABLED.load(Ordering::Relaxed) || !game_focused() {
        return;
    }
    // Rebind mode: capture the next key for the armed action (Escape cancels). Don't drive the
    // camera while binding so the captured key doesn't also move the view.
    let cap = RD_CAPTURE.load(Ordering::Relaxed);
    if cap >= 0 {
        if vk_down(0x1B) {
            RD_CAPTURE.store(-1, Ordering::Relaxed);
        } else if let Some(vk) = scan_pressed_vk() {
            crate::settings::set_rd_key(cap as usize, vk);
            RD_CAPTURE.store(-1, Ordering::Relaxed);
        }
        return;
    }
    mouse_drag_tick(); // left-drag = orbit/look

    // All key binds are rebindable (settings::rd_key, RdKey order). 1-9 stay fixed = gate numbers.
    let k = |i: usize| crate::settings::rd_key(i);
    if vk_down(k(0)) { setf(&ORBIT_YAW, getf(&ORBIT_YAW) - ORBIT_STEP); } // orbit left
    if vk_down(k(1)) { setf(&ORBIT_YAW, getf(&ORBIT_YAW) + ORBIT_STEP); } // orbit right
    if vk_down(k(2)) { let c = getf(&DIST); setf(&DIST, (c - (c * 0.06 + 0.25)).clamp(DIST_MIN, DIST_MAX)); } // zoom in
    if vk_down(k(3)) { let c = getf(&DIST); setf(&DIST, (c + (c * 0.06 + 0.25)).clamp(DIST_MIN, DIST_MAX)); } // zoom out
    if vk_down(k(4)) { setf(&EYE_H, getf(&EYE_H) + HEIGHT_STEP); } // raise height
    if vk_down(k(5)) { setf(&EYE_H, getf(&EYE_H) - HEIGHT_STEP); } // lower height

    if edge(vk_down(k(6)), &EDGE_LB) {
        cycle_target(-1); // previous Uma
    }
    if edge(vk_down(k(7)), &EDGE_RB) {
        cycle_target(1); // next Uma
    }
    // 1-9 = jump straight to the Uma in that gate (edge-detected; ignored if no horse there).
    for i in 0..9 {
        if edge(vk_down(0x31 + i as i32), &EDGE_NUM[i]) {
            follow_gate(i as i32 + 1);
        }
    }

    if edge(vk_down(k(8)), &EDGE_O) {
        cycle_preset(); // cycle saved presets
    }
    if edge(vk_down(k(9)), &EDGE_P) {
        save_active_preset();
        if let Ok(mut s) = last_pose().lock() {
            *s = format!(
                "saved preset {}: dist={:.1} eyeH={:.2}",
                preset_active() + 1, getf(&DIST), getf(&EYE_H)
            );
        }
    }
    // First-person view is DISABLED (experimental: the world geometry isn't drawn on some elevated /
    // curved courses → a blank "void"). Kept out of release builds; freecam stays 3rd-person only.
    let _ = &EDGE_V;
}

static EDGE_O: AtomicBool = AtomicBool::new(false);
static EDGE_P: AtomicBool = AtomicBool::new(false);
static EDGE_V: AtomicBool = AtomicBool::new(false);
static EDGE_LB: AtomicBool = AtomicBool::new(false);
static EDGE_RB: AtomicBool = AtomicBool::new(false);
static EDGE_NUM: [AtomicBool; 9] = [const { AtomicBool::new(false) }; 9]; // 1-9 direct-follow keys
/// Rising-edge detector: true once per press (so a held key = one action).
/// NOTE: swap UNCONDITIONALLY — a short-circuited `now && !store.swap(now)` skips the swap
/// on release (now=false), leaving `store` stuck true so it would only ever fire ONCE.
fn edge(now: bool, store: &AtomicBool) -> bool {
    let was = store.swap(now, Ordering::Relaxed);
    now && !was
}
static LAST_POSE: OnceLock<Mutex<String>> = OnceLock::new();
fn last_pose() -> &'static Mutex<String> {
    LAST_POSE.get_or_init(|| Mutex::new(String::new()))
}
/// Last chase pose captured with the J key (for the overlay panel). Empty until pressed.
pub fn captured_pose() -> String {
    last_pose().lock().map(|s| s.clone()).unwrap_or_default()
}

// mouse drag (polling — reliable; hudhook doesn't feed the delta outside the panel)
static LAST_MX: AtomicI32 = AtomicI32::new(0);
static LAST_MY: AtomicI32 = AtomicI32::new(0);
static MOUSE_INIT: AtomicBool = AtomicBool::new(false);

// True while imgui wants the mouse (cursor over a Heaven window — telemetry HUD, menu,
// panels…). Set from the render thread each frame; the freecam drag respects it so dragging
// the telemetry box (or any panel) doesn't also orbit the race camera.
static UI_CAPTURE: AtomicBool = AtomicBool::new(false);
pub fn set_ui_capture(on: bool) {
    UI_CAPTURE.store(on, Ordering::Relaxed);
}

fn mouse_drag_tick() {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut pt = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut pt) } == 0 {
        return;
    }
    // Mouse is over a Heaven window → let imgui have the drag (move the box), don't orbit.
    // Reset the baseline so leaving the window doesn't cause a camera jump.
    if UI_CAPTURE.load(Ordering::Relaxed) {
        MOUSE_INIT.store(false, Ordering::Relaxed);
        LAST_MX.store(pt.x, Ordering::Relaxed);
        LAST_MY.store(pt.y, Ordering::Relaxed);
        return;
    }
    if vk_down(0x01) {
        // left button held → drag = look/orbit (the click still reaches the game)
        if MOUSE_INIT.load(Ordering::Relaxed) {
            let dx = (pt.x - LAST_MX.load(Ordering::Relaxed)) as f32;
            let dy = (pt.y - LAST_MY.load(Ordering::Relaxed)) as f32;
            if dx != 0.0 || dy != 0.0 {
                mouse_look(dx, dy);
            }
        }
        MOUSE_INIT.store(true, Ordering::Relaxed);
    } else {
        MOUSE_INIT.store(false, Ordering::Relaxed);
    }
    LAST_MX.store(pt.x, Ordering::Relaxed);
    LAST_MY.store(pt.y, Ordering::Relaxed);
}

static INPUT_STARTED: AtomicBool = AtomicBool::new(false);
fn start_input_thread() {
    if INPUT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| loop {
        input_tick();
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
}

/// Install the race freecam hooks. Returns a short note of what resolved.
pub fn install() -> String {
    reset();
    let mut got: Vec<&str> = Vec::new();

    let mgr = il2cpp::class("Gallop.RaceCameraManager");
    {
        // Bind the radial-blur kill (spurt smear) used while following.
        M_DISABLE_BLUR.store(il2cpp::method_pointer(il2cpp::method(mgr, "DisableRadialBlur", 0)) as usize, Ordering::Relaxed);
    }
    unsafe {
        if hook_at(il2cpp::method_pointer(il2cpp::method(mgr, "AlterLateUpdate", 0)), on_alter_late_update as *const (), &TR_LATE, &D_LATE) {
            got.push("late");
        }
        if hook_at(il2cpp::method_pointer(il2cpp::method(mgr, "ChangeCameraMode", 2)), on_change_cam_mode as *const (), &TR_CMODE, &D_CMODE) {
            got.push("cmode");
        }
        if hook_at(il2cpp::method_pointer(il2cpp::method(mgr, "PlayEventCamera", 5)), on_play_event_camera as *const (), &TR_PEC, &D_PEC) {
            got.push("pec");
        }
    }

    let view_base = il2cpp::class("Gallop.RaceViewBase");
    unsafe {
        if hook_at(il2cpp::method_pointer(il2cpp::method(view_base, "LateUpdateView", 0)), on_view_late_update as *const (), &TR_VIEW, &D_VIEW) {
            got.push("view");
        }
    }

    let model_ctrl = il2cpp::class("Gallop.RaceModelController");
    unsafe {
        if hook_at(il2cpp::method_pointer(il2cpp::method(model_ctrl, "UpdateCameraDistanceBlendRate", 3)), on_update_camera_distance_blend_rate as *const (), &TR_UCDBR, &D_UCDBR) {
            got.push("ucdbr");
        }
    }

    // First-person void fix: neutralize the per-object occlusion culler.
    let vissw = il2cpp::class("Gallop.RaceRendererVisibilitySwitcher");
    unsafe {
        let m_reset = il2cpp::method(vissw, "ResetRenderer", 0);
        RESET_RENDERER_FN.store(il2cpp::method_pointer(m_reset) as usize, Ordering::Relaxed);
        RESET_RENDERER_MI.store(m_reset as usize, Ordering::Relaxed);
        if hook_at(il2cpp::method_pointer(il2cpp::method(vissw, "UpdateRenderer", 2)), on_vis_update as *const (), &TR_VISSW, &D_VISSW) {
            got.push("vissw");
        }
    }

    // FP fog: resolve UnityEngine.RenderSettings static setters (called on FP toggle).
    let rs = il2cpp::class("UnityEngine.RenderSettings");
    SET_FOG.store(il2cpp::method(rs, "set_fog", 1) as usize, Ordering::Relaxed);
    SET_FOGMODE.store(il2cpp::method(rs, "set_fogMode", 1) as usize, Ordering::Relaxed);
    SET_FOGSTART.store(il2cpp::method(rs, "set_fogStartDistance", 1) as usize, Ordering::Relaxed);
    SET_FOGEND.store(il2cpp::method(rs, "set_fogEndDistance", 1) as usize, Ordering::Relaxed);
    SET_FOGCOLOR.store(il2cpp::method(rs, "set_fogColor", 1) as usize, Ordering::Relaxed);

    // transform icalls
    unsafe {
        let setpos = il2cpp::resolve_icall("UnityEngine.Transform::set_position_Injected(UnityEngine.Vector3&)");
        if hook_at(setpos, on_set_position as *const (), &TR_SETPOS, &D_SETPOS) {
            got.push("pos");
        }
        let setlocal = il2cpp::resolve_icall("UnityEngine.Transform::set_localPosition_Injected(UnityEngine.Vector3&)");
        if hook_at(setlocal, on_set_localposition as *const (), &TR_SETLOCAL, &D_SETLOCAL) {
            got.push("lpos");
        }
        let setrot = il2cpp::resolve_icall("UnityEngine.Transform::set_rotation_Injected(UnityEngine.Quaternion&)");
        if hook_at(setrot, on_set_rotation as *const (), &TR_SETROT, &D_SETROT) {
            got.push("rot");
        }
        let lookat = il2cpp::resolve_icall(
            "UnityEngine.Transform::Internal_LookAt_Injected(UnityEngine.Vector3&,UnityEngine.Vector3&)",
        );
        if hook_at(lookat, on_lookat as *const (), &TR_LOOKAT, &D_LOOKAT) {
            got.push("lookat");
        }
    }

    // follow plumbing
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), "_position") {
        POS_OFF.store(off, Ordering::Relaxed);
        got.push("posfield");
    }
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), "_rotationOnLane") {
        ROT_OFF.store(off, Ordering::Relaxed);
        got.push("rotfield");
    }
    // Live race-telemetry field offsets + method pointers (HorseRaceInfo / HorseData /
    // RaceHorseData / SkillManager / AI) are owned by the Race Director module.
    if crate::race_director::install_offsets() {
        got.push("telem");
    }
    let hd = il2cpp::class("Gallop.HorseData");
    let gateno = il2cpp::method(hd, "get_GateNo", 0);
    if !gateno.is_null() {
        GATENO_CODE.store(il2cpp::method_pointer(gateno) as usize, Ordering::Relaxed);
        GATENO_MI.store(gateno as usize, Ordering::Relaxed);
    }
    let replay = il2cpp::class("Gallop.HorseRaceInfoReplay");
    unsafe {
        if hook_at(il2cpp::method_pointer(il2cpp::method(replay, "get_RunMotionSpeed", 0)), on_run_motion as *const (), &TR_MOTION, &D_MOTION) {
            got.push("motion");
        }
        if hook_at(il2cpp::method_pointer(il2cpp::method(replay, ".ctor", 2)), on_hri_ctor as *const (), &TR_CTOR, &D_CTOR) {
            got.push("gate");
        }
    }

    // Camera-director taming: intercept Behaviour.set_enabled at the source.
    unsafe {
        let beh_cls = il2cpp::class("UnityEngine.Behaviour");
        if hook_at(il2cpp::method_pointer(il2cpp::method(beh_cls, "set_enabled", 1)), on_set_enabled as *const (), &TR_SETEN, &D_SETEN) {
            got.push("seten");
        }
        // Force RaceCourseCamera's FOV (kills the post-skill close-up zoom).
        let cam_cls = il2cpp::class("UnityEngine.Camera");
        if hook_at(il2cpp::method_pointer(il2cpp::method(cam_cls, "get_fieldOfView", 0)), on_unity_get_fov as *const (), &TR_UFOV, &D_UFOV) {
            got.push("fov");
        }
        // DIAGNOSTIC: GameObject.SetActive — for the skill-aura hunt.
        let go_cls = il2cpp::class("UnityEngine.GameObject");
        if hook_at(il2cpp::method_pointer(il2cpp::method(go_cls, "SetActive", 1)), on_set_active as *const (), &TR_SETACTIVE, &D_SETACTIVE) {
            got.push("setactive");
        }
    }

    // DIAGNOSTIC bindings (start-dash camera director)
    {
        let cam_cls = il2cpp::class("UnityEngine.Camera");
        let m_all = il2cpp::method(cam_cls, "get_allCameras", 0);
        if !m_all.is_null() {
            CAM_GET_ALL.store(il2cpp::method_pointer(m_all) as usize, Ordering::Relaxed);
            CAM_GET_ALL_MI.store(m_all as usize, Ordering::Relaxed);
        }
        let m_depth = il2cpp::method(cam_cls, "get_depth", 0);
        if !m_depth.is_null() {
            CAM_GET_DEPTH.store(il2cpp::method_pointer(m_depth) as usize, Ordering::Relaxed);
        }
        // FP near-clip setter (store the Method; called as f(cam, value, methodInfo)).
        let m_nc = il2cpp::method(cam_cls, "set_nearClipPlane", 1);
        if !m_nc.is_null() {
            SET_NEARCLIP.store(m_nc as usize, Ordering::Relaxed);
        }
        let m_fov = il2cpp::method(cam_cls, "get_fieldOfView", 0);
        if !m_fov.is_null() {
            CAM_GET_FOV.store(il2cpp::method_pointer(m_fov) as usize, Ordering::Relaxed);
        }
        let comp_cls = il2cpp::class("UnityEngine.Component");
        let m_tf = il2cpp::method(comp_cls, "get_transform", 0);
        if !m_tf.is_null() {
            COMP_GET_TF.store(il2cpp::method_pointer(m_tf) as usize, Ordering::Relaxed);
            COMP_GET_TF_MI.store(m_tf as usize, Ordering::Relaxed);
        }
        let beh_cls = il2cpp::class("UnityEngine.Behaviour");
        let m_en = il2cpp::method(beh_cls, "get_enabled", 0);
        if !m_en.is_null() {
            BEH_GET_ENABLED.store(il2cpp::method_pointer(m_en) as usize, Ordering::Relaxed);
        }
        let m_seten = il2cpp::method(beh_cls, "set_enabled", 1);
        if !m_seten.is_null() {
            BEH_SET_ENABLED.store(il2cpp::method_pointer(m_seten) as usize, Ordering::Relaxed);
        }
        OBJ_GETNAME_ICALL.store(il2cpp::resolve_icall("UnityEngine.Object::GetName(UnityEngine.Object)") as usize, Ordering::Relaxed);
        GET_POS_ICALL.store(il2cpp::resolve_icall("UnityEngine.Transform::get_position_Injected(UnityEngine.Vector3&)") as usize, Ordering::Relaxed);
    }

    start_input_thread();
    format!("hooks=[{}] build={BUILD_TAG}", got.join(","))
}

/// Bump this every build so the boot log unambiguously identifies the loaded DLL.
const BUILD_TAG: &str = "2026-06-10m-fp-close";

// ── UI panels & helpers moved out of overlay (Race Director keybinds + preset manager) ──

/// Human-readable name for a Win32 VK code (for the key-bind UI).
fn vk_name(vk: i32) -> String {
    match vk {
        0 => "—".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "PgUp".into(),
        0x22 => "PgDn".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x30..=0x39 => ((b'0' + (vk - 0x30) as u8) as char).to_string(),
        0x41..=0x5A => ((b'A' + (vk - 0x41) as u8) as char).to_string(),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        0x70..=0x7B => format!("F{}", vk - 0x70 + 1),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        _ => format!("0x{vk:02X}"),
    }
}

/// A small button that reads as "selected" (gold border/fill) when `on`. Returns clicked.
fn def_btn(ui: &hudhook::imgui::Ui, id: &str, label: &str, on: bool) -> bool {
    use crate::overlay::{BTN_BG, BTN_HI, GOLD, TEXT};
    let (pad, h) = (14.0, 30.0);
    let ts = ui.calc_text_size(label);
    let w = ts[0] + pad * 2.0 + if on { 16.0 } else { 0.0 };
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button(id, [w, h]);
    let hov = ui.is_item_hovered();
    let dl = ui.get_window_draw_list();
    let bg = if on { [0.30, 0.24, 0.12, 1.0] } else if hov { BTN_HI } else { BTN_BG };
    dl.add_rect(p, [p[0] + w, p[1] + h], bg).filled(true).rounding(9.0).build();
    dl.add_rect(p, [p[0] + w, p[1] + h], if on { GOLD } else { [0.60, 0.46, 0.90, if hov { 0.65 } else { 0.32 }] })
        .rounding(9.0)
        .thickness(if on { 1.6 } else { 1.2 })
        .build();
    let mut tx = p[0] + pad;
    if on {
        dl.add_circle([p[0] + pad + 4.0, p[1] + h * 0.5], 3.0, GOLD).filled(true).build();
        tx += 16.0;
    }
    dl.add_text([tx, p[1] + (h - ts[1]) * 0.5], if on { GOLD } else { TEXT }, label);
    clicked
}

/// Race Director key-bind editor: one row per action with its current key; click a key then press
/// the new one (Esc cancels). 1-9 = gate numbers, fixed. Used in both the premium + classic menus.
pub(crate) fn draw_keybinds_panel(ui: &hudhook::imgui::Ui, w: f32) {
    use crate::overlay::{btn, BAD, DIM, GOLD};
    ui.dummy([0.0, 6.0]);
    ui.text_colored(GOLD, "Key bindings");
    ui.text_colored(DIM, "click a key, then press the new one (Esc cancels)");
    ui.dummy([0.0, 2.0]);
    let cap = crate::freecam::rd_capturing();
    // Conflict detection: a VK bound to more than one action is flagged red.
    let vks: Vec<i32> = (0..11).map(crate::settings::rd_key).collect();
    let conflict = |i: usize| vks[i] != 0 && vks.iter().filter(|&&v| v == vks[i]).count() > 1;
    const BINDS: &[(usize, &str)] = &[
        (0, "Orbit left"),
        (1, "Orbit right"),
        (2, "Zoom in"),
        (3, "Zoom out"),
        (4, "Raise height"),
        (5, "Lower height"),
        (6, "Previous Uma"),
        (7, "Next Uma"),
        (8, "Cycle preset"),
        (9, "Save preset"),
    ];
    for &(idx, label) in BINDS {
        let row_y = ui.cursor_screen_pos()[1];
        ui.set_cursor_screen_pos([ui.cursor_screen_pos()[0], row_y + 8.0]); // align label to button mid
        let dup = conflict(idx);
        ui.text_colored(if dup { BAD } else { [0.86, 0.86, 0.91, 1.0] }, label);
        if dup {
            ui.same_line();
            ui.text_colored(BAD, "(dup)");
        }
        ui.same_line_with_pos((w - 92.0).max(108.0));
        ui.set_cursor_screen_pos([ui.cursor_screen_pos()[0], row_y]);
        let keytxt = if cap == idx as i32 {
            "press a key…".to_string()
        } else {
            vk_name(crate::settings::rd_key(idx))
        };
        if btn(ui, &format!("##rdk{idx}"), &keytxt) {
            // toggle: clicking the armed one again cancels
            crate::freecam::rd_capture_start(if cap == idx as i32 { -1 } else { idx as i32 });
        }
        ui.dummy([0.0, 3.0]);
    }
}

/// Per-circuit camera preset manager — a custom animated dropdown listing this circuit's presets,
/// with rename of the selected one + Default / Delete / Add. Keys: O cycles presets, P saves. Width `w`.
pub(crate) fn draw_preset_panel(ui: &hudhook::imgui::Ui, w: f32) {
    use crate::overlay::{accent, anim_step, btn, BTN_BG, BTN_HI, DIM, GOLD, TEXT};
    use std::cell::{Cell, RefCell};
    thread_local! {
        static OPEN: Cell<bool> = const { Cell::new(false) };
        static RBUF: RefCell<String> = const { RefCell::new(String::new()) };
        static RIDX: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    let names = crate::freecam::preset_names();
    let active = crate::freecam::preset_active().min(names.len().saturating_sub(1));
    let def = crate::freecam::preset_default();
    let track = crate::freecam::preset_track();

    ui.text_colored(DIM, "Camera presets");
    ui.same_line();
    ui.text_colored(DIM, format!("\u{00b7}  O cycle  \u{00b7}  P save"));

    // ── dropdown header (shows the active preset) ──
    let cur = names.get(active).cloned().unwrap_or_else(|| "— no presets —".into());
    let h = 30.0;
    let p = ui.cursor_screen_pos();
    let clicked = ui.invisible_button("##ddhdr", [w, h]);
    let hov = ui.is_item_hovered();
    let hh = anim_step("ddhdrh", if hov { 1.0 } else { 0.0 }, 16.0);
    let open = OPEN.with(|o| o.get());
    {
        let dl = ui.get_window_draw_list();
        dl.add_rect(p, [p[0] + w, p[1] + h], if hov { BTN_HI } else { BTN_BG }).filled(true).rounding(9.0).build();
        dl.add_rect(p, [p[0] + w, p[1] + h], [0.60, 0.46, 0.90, 0.32 + 0.33 * hh]).rounding(9.0).thickness(1.2).build();
        dl.add_text([p[0] + 12.0, p[1] + (h - 14.0) * 0.5], TEXT, &cur);
        // gold caret (up when open, down when closed)
        let (cx, cy) = (p[0] + w - 16.0, p[1] + h * 0.5);
        if open {
            dl.add_triangle([cx - 5.0, cy + 3.0], [cx + 5.0, cy + 3.0], [cx, cy - 4.0], GOLD).filled(true).build();
        } else {
            dl.add_triangle([cx - 5.0, cy - 3.0], [cx + 5.0, cy - 3.0], [cx, cy + 4.0], GOLD).filled(true).build();
        }
    }
    if clicked {
        OPEN.with(|o| o.set(!open));
    }

    // ── open list (rows with hover highlight) ──
    if open && !names.is_empty() {
        for (i, name) in names.iter().enumerate() {
            let rh = 26.0;
            let rp = ui.cursor_screen_pos();
            let rc = ui.invisible_button(format!("##ddr{i}"), [w, rh]);
            let rhov = ui.is_item_hovered();
            let hl = anim_step(&format!("ddrh{i}"), if rhov { 1.0 } else { 0.0 }, 18.0);
            {
                let dl = ui.get_window_draw_list();
                if hl > 0.01 {
                    dl.add_rect(rp, [rp[0] + w, rp[1] + rh], [0.60, 0.46, 0.90, 0.20 * hl]).filled(true).rounding(7.0).build();
                }
                if i == active {
                    dl.add_circle([rp[0] + 11.0, rp[1] + rh * 0.5], 3.0, GOLD).filled(true).build();
                }
                dl.add_text([rp[0] + 24.0, rp[1] + (rh - 14.0) * 0.5], if i == active { GOLD } else { TEXT }, name);
                if i == def {
                    let t = "default";
                    let ts = ui.calc_text_size(t);
                    dl.add_text([rp[0] + w - ts[0] - 12.0, rp[1] + (rh - 14.0) * 0.5], accent(), t);
                }
            }
            if rc {
                crate::freecam::preset_apply_idx(i);
                OPEN.with(|o| o.set(false));
            }
        }
    }

    // ── selected-preset management (rename + default/delete) ──
    if !names.is_empty() {
        ui.dummy([0.0, 4.0]);
        // keep the rename buffer synced to the active preset
        if RIDX.with(|r| r.get()) != active {
            RIDX.with(|r| r.set(active));
            RBUF.with(|b| *b.borrow_mut() = names[active].clone());
        }
        RBUF.with(|b| {
            let mut s = b.borrow_mut();
            ui.set_next_item_width(w);
            if ui.input_text("##presetname", &mut s).hint("preset name").build() {
                crate::freecam::preset_rename(active, &s);
            }
        });
        ui.dummy([0.0, 2.0]);
        if def_btn(ui, "##setdef", "Default", active == def) {
            crate::freecam::preset_set_default(active);
        }
        ui.same_line();
        if btn(ui, "##delpreset", "Delete") {
            crate::freecam::preset_delete(active);
        }
    }
    if names.len() < 4 && track != 0 {
        ui.dummy([0.0, 2.0]);
        if btn(ui, "##addpreset", "+ Add current view") {
            let n = format!("Preset {}", names.len() + 1);
            crate::freecam::preset_add(&n);
        }
    }
}
