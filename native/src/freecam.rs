//! Heaven — race free camera (feature `freecam`, private build only).
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

use crate::htt_il2cpp as h;
use crate::il2cpp;

// diagnostic log (shared with the rest of the native engine)
fn flog(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}
static DIAG_MOTION_SEEN: AtomicBool = AtomicBool::new(false);
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

// ── public API (used by overlay.rs / the full build.rs / boot.rs) ─────────────────────
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
            reset_pace();
            load_default_pose();
        }
    } else if !on && was {
        // Mid-race DISABLE: stop following so drive_this()/drive_cam() go false and the game's own
        // race camera takes back over immediately (telemetry keeps running independently).
        FOLLOW.store(false, Ordering::Relaxed);
    }
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
    reset_skill_feed(); // switched Uma → rescan ITS activated skills
    reset_pace();
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
    reset_skill_feed();
    reset_pace();
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
    reset_pace(); // fresh race → clear the previous race's pace trace
    load_default_pose(); // provisional (track id may be 0 here); reloaded once it's known
    RACE_POSE_LOADED.store(false, Ordering::Relaxed); // re-apply the circuit's default once track id resolves
    CAMSET_HASH.store(0, Ordering::Relaxed); // DIAGNOSTIC: re-arm camera-set change dump
    RACE_CAM_OBJ.store(0, Ordering::Relaxed); // re-find RaceCourseCamera this race
    RACE_CAM_TF.store(0, Ordering::Relaxed); // re-cache RaceCourseCamera transform this race
    EFFECT_OBJ.store(0, Ordering::Relaxed); // re-find RaceEnvEffect this race
    if let Ok(mut b) = telem_buf().lock() {
        b.clear(); // fresh telemetry for the new race (gates re-map)
    }
    reset_skill_feed(); // fresh skill feed for the new race
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
// Telemetry heartbeat: updated every frame a horse is actually being stepped (the running race).
// More precise than the camera gate — it stops on the result screen / between races, so the HUD
// hides and the data is wiped when a new race resumes after any gap.
static LAST_TELEM_MS: AtomicU64 = AtomicU64::new(0);
static RACE_EPOCH: AtomicU64 = AtomicU64::new(0);
fn clock() -> &'static std::time::Instant {
    static C: OnceLock<std::time::Instant> = OnceLock::new();
    C.get_or_init(std::time::Instant::now)
}
fn mark_race() {
    LAST_RACE_MS.store(clock().elapsed().as_millis() as u64, Ordering::Relaxed);
}
fn in_race() -> bool {
    (clock().elapsed().as_millis() as u64).saturating_sub(LAST_RACE_MS.load(Ordering::Relaxed)) < 300
}
/// True only while horses are actively being stepped (the running race) — goes stale on the result
/// screen / between races, so the broadcast HUD hides when you leave the race.
fn telem_fresh() -> bool {
    (clock().elapsed().as_millis() as u64).saturating_sub(LAST_TELEM_MS.load(Ordering::Relaxed)) < 500
}
/// Bumped whenever a new race session starts (telemetry resumed after a gap). The HUD uses it to
/// reset per-race visual state (e.g. the timing-tower slide animation) so nothing carries over.
pub fn race_epoch() -> u64 {
    RACE_EPOCH.load(Ordering::Relaxed)
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
static SCAN_CTR: AtomicU32 = AtomicU32::new(0);

// AGGRESSIVE SCAN: every ~15 race frames, dump every enabled camera (name/depth/fov/
// pos/sepUma) + our intended chase pos + whether RaceCourseCamera's ACTUAL pose matches
// it (i.e. is our OOB pin working). Reveals which camera renders the close-up and why.
unsafe fn full_scan() {
    let n = SCAN_CTR.fetch_add(1, Ordering::Relaxed);
    if n % 15 != 0 {
        return;
    }
    let ga = CAM_GET_ALL.load(Ordering::Relaxed);
    let gn = OBJ_GETNAME_ICALL.load(Ordering::Relaxed);
    let gt = COMP_GET_TF.load(Ordering::Relaxed);
    let gp = GET_POS_ICALL.load(Ordering::Relaxed);
    let gd = CAM_GET_DEPTH.load(Ordering::Relaxed);
    let gf = CAM_GET_FOV.load(Ordering::Relaxed);
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
    let cp = current_pos();
    let (ux, uy, uz) = (getf(&TPX), getf(&TPY), getf(&TPZ));
    flog(&format!(
        "[freecam] SCAN n={n} raceTf={:#x} chase=({:.0},{:.0},{:.0}) uma=({:.0},{:.0},{:.0})",
        RACE_CAM_TF.load(Ordering::Relaxed), cp.x, cp.y, cp.z, ux, uy, uz
    ));
    for i in 0..count {
        let cam = *elems.add(i);
        if cam.is_null() {
            continue;
        }
        let name = il2cpp::read_string(f_name(cam));
        // only the cameras that can render the race view (skip UI / render-tex)
        if name == "UICamera" || name == "New Game Object" || name == "RaceEnvEffect" {
            continue;
        }
        let depth = if gd != 0 { let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> f32 = std::mem::transmute(gd); f(cam, std::ptr::null_mut()) } else { 0.0 };
        let fov = if gf != 0 { let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> f32 = std::mem::transmute(gf); f(cam, std::ptr::null_mut()) } else { 0.0 };
        let (mut px, mut py, mut pz, mut sep) = (0.0f32, 0.0f32, 0.0f32, -1.0f32);
        if gt != 0 && gp != 0 {
            let f_tf: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(gt);
            let tf = f_tf(cam, COMP_GET_TF_MI.load(Ordering::Relaxed) as *mut c_void);
            if !tf.is_null() {
                let f_pos: unsafe extern "C" fn(*mut c_void, *mut V3) = std::mem::transmute(gp);
                let mut p = V3 { x: 0.0, y: 0.0, z: 0.0 };
                f_pos(tf, &mut p);
                px = p.x; py = p.y; pz = p.z;
                let (dx, dy, dz) = (px - ux, py - uy, pz - uz);
                sep = (dx * dx + dy * dy + dz * dz).sqrt();
            }
        }
        flog(&format!("[freecam]   '{name}' d={depth:.0} fov={fov:.0} pos=({px:.0},{py:.0},{pz:.0}) sep={sep:.0}"));
    }
}

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

// ── live telemetry (for the freecam HUD) — HorseRaceInfo field offsets ─────────
static HP_OFF: AtomicUsize = AtomicUsize::new(0);
static MAXHP_OFF: AtomicUsize = AtomicUsize::new(0);
static ORDER_OFF: AtomicUsize = AtomicUsize::new(0);
static SPEED_OFF: AtomicUsize = AtomicUsize::new(0);
static DIST_OFF: AtomicUsize = AtomicUsize::new(0);
static HPEMPTY_OFF: AtomicUsize = AtomicUsize::new(0); // <IsHpEmptyOnRace>: bool, exhausted
static PHASE_OFF: AtomicUsize = AtomicUsize::new(0); // _phase: i32 race phase (>=2 = last spurt)
// Live race-state flags (all pure bool/i32/float fields on HorseRaceInfo — cheap direct reads).
static BADSTART_OFF: AtomicUsize = AtomicUsize::new(0); // <IsBadStart>: bool, late start
static COMPFIGHT_OFF: AtomicUsize = AtomicUsize::new(0); // <IsCompeteFight>: bool, head-to-head fight
static COMPTOP_OFF: AtomicUsize = AtomicUsize::new(0); // <IsCompeteTop>: bool, leading battle
static BLOCKFRONT_OFF: AtomicUsize = AtomicUsize::new(0); // <BlockFrontContinueTime>: f32, >0 = boxed in
static PREVORDER_OFF: AtomicUsize = AtomicUsize::new(0); // <PrevOrder>: i32, for the order-trend arrow
static DEFEAT_OFF: AtomicUsize = AtomicUsize::new(0); // _defeat: i32 DefeatType (why she can't win)
// Static-per-race identity (pointer-chase off `this`): HorseRaceInfo._horseData → HorseData,
// HorseData.<Popularity> + HorseData._responseHorseData (RaceHorseData) → .running_style.
static HDATA_OFF: AtomicUsize = AtomicUsize::new(0); // HorseRaceInfo._horseData (ptr)
static POP_OFF: AtomicUsize = AtomicUsize::new(0); // HorseData.<Popularity>: i32 (人気 rank)
static RESP_OFF: AtomicUsize = AtomicUsize::new(0); // HorseData._responseHorseData (ptr)
static RSTYLE_OFF: AtomicUsize = AtomicUsize::new(0); // RaceHorseData.running_style: i32 (1..4)
static TNAME_OFF: AtomicUsize = AtomicUsize::new(0); // RaceHorseData.trainer_name (managed String ptr)
static VIEWER_OFF: AtomicUsize = AtomicUsize::new(0); // RaceHorseData.viewer_id: i64 (0 = NPC)
// Skill activation FEED (followed Uma only). Read SkillManager._usedSkillIdList (per-uma
// list of activated skill ids), detect new entries, resolve names via MasterDataUtil.
// GetSkillName. All reads are pure; GetSkillName is called only when a NEW skill appears
// (rare event, main thread) — never per-frame-per-horse (that froze the horses).
static SKILLMGR_OFF: AtomicUsize = AtomicUsize::new(0); // HorseRaceInfo._skillManager
static USEDLIST_OFF: AtomicUsize = AtomicUsize::new(0); // SkillManager._usedSkillIdList
static GSN_FN: AtomicUsize = AtomicUsize::new(0); // MasterDataUtil.GetSkillName(id)
static GSN_MI: AtomicUsize = AtomicUsize::new(0);
static GSN_STATIC: AtomicBool = AtomicBool::new(true);
// Skill effect values — in-memory master data (public-safe, no the game data):
// WorkTrainingChallengeData.get_MasterManager() → MasterDataManager.<masterSkillData>@0xc8 →
// MasterSkillData.Get(id) → SkillData (AbilityType11@0x6c, FloatAbilityValue11@0x78 ÷1e4, FloatAbilityTime1@0x60 ÷1e4).
static MM_GET: AtomicUsize = AtomicUsize::new(0);
static MM_GET_MI: AtomicUsize = AtomicUsize::new(0);
static MSD_GET: AtomicUsize = AtomicUsize::new(0);
static MSD_GET_MI: AtomicUsize = AtomicUsize::new(0);
static MSD_INST: AtomicUsize = AtomicUsize::new(0); // cached MasterSkillData* (master data is stable)
static FEED_SEEN: AtomicUsize = AtomicUsize::new(0); // ids already added for the followed Uma
static SKILL_FEED: OnceLock<Mutex<Vec<(i32, String)>>> = OnceLock::new();
fn skill_feed_buf() -> &'static Mutex<Vec<(i32, String)>> {
    SKILL_FEED.get_or_init(|| Mutex::new(Vec::new()))
}
/// Activated skills (id, name) for the currently-followed Uma, in activation order. The id
/// lets the overlay look up the skill's icon.
pub fn skill_feed() -> Vec<(i32, String)> {
    skill_feed_buf().lock().map(|f| f.clone()).unwrap_or_default()
}

fn reset_skill_feed() {
    FEED_SEEN.store(0, Ordering::Relaxed);
    if let Ok(mut f) = skill_feed_buf().lock() {
        f.clear();
    }
}

// ── live race-outlook reads for the followed Uma (followed Uma ONLY, once/frame) ──
static SPURT_OUTLOOK: AtomicI32 = AtomicI32::new(0); // LastSpurtCalcResult bitflags (1/2 hold, 4/8 fade)
static AI_OFF: AtomicUsize = AtomicUsize::new(0); // HorseRaceInfo._horseRaceAI (ptr)
static AI_SPURT_GET: AtomicUsize = AtomicUsize::new(0); // HorseRaceAIReplay.get_LastSpurtCalcResult (REAL impl)
// Live race-state getters on the AI (HorseRaceAIBase real cluster, same safe profile as spurt).
static KAKARI_GET: AtomicUsize = AtomicUsize::new(0); // get_IsTemptation (bool)
static TEMPTMODE_GET: AtomicUsize = AtomicUsize::new(0); // get_TemptationMode (enum)
static KEEPMODE_GET: AtomicUsize = AtomicUsize::new(0); // get_PositionKeepMode (enum)
static DOWNHILL_GET: AtomicUsize = AtomicUsize::new(0); // get_IsDownSlopeAccelMode (bool)
// Active-skill countdown via a pure FIELD walk (no GetCurrentActiveSkill stub — that crashed):
// SkillManager._skills (List<SkillBase>) → SkillBase.Details (List<SkillDetail>) → detail fields.
static SKILLS_LIST_OFF: AtomicUsize = AtomicUsize::new(0); // SkillManager._skills
static SB_MASTER_OFF: AtomicUsize = AtomicUsize::new(0); // SkillBase.<SkillMaster> → SkillData
static SB_DETAILS_OFF: AtomicUsize = AtomicUsize::new(0); // SkillBase.<Details> (List<SkillDetail>)
static SD_LEFT_OFF: AtomicUsize = AtomicUsize::new(0); // SkillDetail.<LeftTime>
static SD_CAT_OFF: AtomicUsize = AtomicUsize::new(0); // SkillDetail.<Category>
static SD_DEBUFF_OFF: AtomicUsize = AtomicUsize::new(0); // SkillDetail._isDebuff
static SD_ACT_OFF: AtomicUsize = AtomicUsize::new(0); // SkillDetail.<IsActivated>

/// One currently-active skill effect on the followed Uma (live countdown).
#[derive(Clone, Copy)]
pub struct ActiveSkill {
    pub id: i32,
    pub left: f32,      // seconds of effect remaining
    pub category: i32,  // SkillCategory: 0 Speed, 1 Heal, 2 Accel, -1 none
    pub debuff: bool,
}
static ACTIVE_SKILLS: OnceLock<Mutex<Vec<ActiveSkill>>> = OnceLock::new();
fn active_skills_buf() -> &'static Mutex<Vec<ActiveSkill>> {
    ACTIVE_SKILLS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Live AI-driven race state for the followed Uma (kakari / position-keep / down-slope).
#[derive(Clone, Copy, Default)]
pub struct FollowState {
    pub kakari: bool,        // get_IsTemptation — over-eager, burning stamina
    pub temptation_mode: i32, // TemptationMode (1 Sashi, 2 Senko, 3 Nige, 4 Boost)
    pub keep_mode: i32,      // PositionKeepMode (1 SpeedUp, 2 Overtake, 3 PaseUp, 4 PaseDown)
    pub downhill: bool,      // get_IsDownSlopeAccelMode — free downhill acceleration
}
static FOLLOW_STATE: OnceLock<Mutex<FollowState>> = OnceLock::new();
fn follow_state_buf() -> &'static Mutex<FollowState> {
    FOLLOW_STATE.get_or_init(|| Mutex::new(FollowState::default()))
}
/// Live race state of the followed Uma (kakari, position-keep mode, down-slope accel).
pub fn follow_state() -> FollowState {
    follow_state_buf().lock().map(|s| *s).unwrap_or_default()
}

/// Followed-Uma only: read the AI's live race-state getters (kakari / position-keep / down-slope).
/// All on HorseRaceAIBase's real method cluster (unique RVAs — the safe kind, like spurt).
unsafe fn update_follow_state(hri: *mut c_void) {
    let ai_off = AI_OFF.load(Ordering::Relaxed);
    if ai_off == 0 {
        return;
    }
    let ai = ((hri as usize + ai_off) as *const *mut c_void).read_unaligned();
    if ai.is_null() {
        return;
    }
    let call_b = |p: usize| -> bool {
        if p == 0 {
            return false;
        }
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool = std::mem::transmute(p);
        f(ai, std::ptr::null_mut())
    };
    let call_i = |p: usize| -> i32 {
        if p == 0 {
            return 0;
        }
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 = std::mem::transmute(p);
        f(ai, std::ptr::null_mut())
    };
    let st = FollowState {
        kakari: call_b(KAKARI_GET.load(Ordering::Relaxed)),
        temptation_mode: call_i(TEMPTMODE_GET.load(Ordering::Relaxed)),
        keep_mode: call_i(KEEPMODE_GET.load(Ordering::Relaxed)),
        downhill: call_b(DOWNHILL_GET.load(Ordering::Relaxed)),
    };
    if let Ok(mut b) = follow_state_buf().lock() {
        *b = st;
    }
}
/// Skills whose effect is active RIGHT NOW on the followed Uma (with seconds remaining).
pub fn active_skills() -> Vec<ActiveSkill> {
    active_skills_buf().lock().map(|b| b.clone()).unwrap_or_default()
}
/// Last-spurt sustainability for the followed Uma: `&3 != 0` = will hold the spurt,
/// `&12 != 0` = will run out of stamina before the line. 0 = not yet computed.
pub fn spurt_outlook() -> i32 {
    SPURT_OUTLOOK.load(Ordering::Relaxed)
}
fn reset_outlook() {
    SPURT_OUTLOOK.store(0, Ordering::Relaxed);
    if let Ok(mut b) = active_skills_buf().lock() {
        b.clear();
    }
    if let Ok(mut s) = follow_state_buf().lock() {
        *s = FollowState::default();
    }
}

// IL2CPP List<T>: _items (T[]) @0x10, _size @0x18; ref-type array data starts at +0x20 (8B refs).
#[inline]
unsafe fn list_ptr_at(list: *mut c_void, i: i32) -> *mut c_void {
    let items = ((list as usize + 0x10) as *const *mut c_void).read_unaligned();
    if items.is_null() {
        return std::ptr::null_mut();
    }
    ((items as usize + 0x20 + i as usize * 8) as *const *mut c_void).read_unaligned()
}
#[inline]
unsafe fn list_size(list: *mut c_void) -> i32 {
    ((list as usize + 0x18) as *const i32).read_unaligned()
}

/// Followed-Uma only: the currently-active skill effects + remaining time, by a PURE FIELD WALK
/// (no `GetCurrentActiveSkill` — that's a crashing stub on this build). Walks SkillManager._skills
/// → each SkillBase's Details → any SkillDetail with IsActivated & LeftTime>0.
unsafe fn update_active_skills(hri: *mut c_void) {
    let mgr_off = SKILLMGR_OFF.load(Ordering::Relaxed);
    let list_off = SKILLS_LIST_OFF.load(Ordering::Relaxed);
    let det_off = SB_DETAILS_OFF.load(Ordering::Relaxed);
    let act_off = SD_ACT_OFF.load(Ordering::Relaxed);
    let left_off = SD_LEFT_OFF.load(Ordering::Relaxed);
    if mgr_off == 0 || list_off == 0 || det_off == 0 || act_off == 0 || left_off == 0 {
        return;
    }
    let mut out: Vec<ActiveSkill> = Vec::new();
    let mgr = ((hri as usize + mgr_off) as *const *mut c_void).read_unaligned();
    if !mgr.is_null() {
        let skills = ((mgr as usize + list_off) as *const *mut c_void).read_unaligned();
        if !skills.is_null() {
            let master_off = SB_MASTER_OFF.load(Ordering::Relaxed);
            let cat_off = SD_CAT_OFF.load(Ordering::Relaxed);
            let deb_off = SD_DEBUFF_OFF.load(Ordering::Relaxed);
            let n = list_size(skills).clamp(0, 64);
            for i in 0..n {
                let sb = list_ptr_at(skills, i);
                if sb.is_null() {
                    continue;
                }
                let details = ((sb as usize + det_off) as *const *mut c_void).read_unaligned();
                if details.is_null() {
                    continue;
                }
                // most-time-remaining active detail of this skill (≤2 details per skill)
                let (mut best_left, mut best_cat, mut best_deb, mut any) = (0.0f32, -1i32, false, false);
                let dn = list_size(details).clamp(0, 4);
                for j in 0..dn {
                    let d = list_ptr_at(details, j);
                    if d.is_null() {
                        continue;
                    }
                    let activated = ((d as usize + act_off) as *const u8).read_unaligned() != 0;
                    let left = ((d as usize + left_off) as *const f32).read_unaligned();
                    if activated && left > 0.05 && left < 60.0 {
                        any = true;
                        if left > best_left {
                            best_left = left;
                            best_cat = if cat_off != 0 { ((d as usize + cat_off) as *const i32).read_unaligned() } else { -1 };
                            best_deb = deb_off != 0 && ((d as usize + deb_off) as *const u8).read_unaligned() != 0;
                        }
                    }
                }
                if any {
                    // skill id ← SkillBase.SkillMaster → SkillData.Id @0x10
                    let id = if master_off != 0 {
                        let m = ((sb as usize + master_off) as *const *mut c_void).read_unaligned();
                        if m.is_null() { 0 } else { ((m as usize + 0x10) as *const i32).read_unaligned() }
                    } else {
                        0
                    };
                    out.push(ActiveSkill { id, left: best_left, category: best_cat, debuff: best_deb });
                }
            }
        }
    }
    if let Ok(mut b) = active_skills_buf().lock() {
        *b = out;
    }
}

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

/// Resolve a skill id → localized name via MasterDataUtil.GetSkillName (static getter).
unsafe fn skill_name(id: i32) -> String {
    let p = GSN_FN.load(Ordering::Relaxed);
    if p == 0 || !GSN_STATIC.load(Ordering::Relaxed) {
        return format!("Skill {id}");
    }
    let f: unsafe extern "C" fn(i32, *const c_void) -> *mut c_void = std::mem::transmute(p);
    let obj = f(id, GSN_MI.load(Ordering::Relaxed) as *const c_void);
    let raw = read_managed_str(obj).filter(|s| !s.is_empty()).unwrap_or_else(|| format!("Skill {id}"));
    // Some skill names carry a trailing tier/symbol glyph the bundled Latin font can't render, so
    // imgui draws it as its fallback char ("?"). Strip trailing spaces + any char beyond Latin
    // Extended-A so the name reads clean (e.g. "Competitive Spirit ?" → "Competitive Spirit").
    let cleaned = raw.trim_end_matches(|c: char| c == ' ' || (c as u32) > 0x024F);
    if cleaned.is_empty() { raw } else { cleaned.to_string() }
}

// gate-independent cache: skill_id → short effect string ("+0.35 m/s 3s"). Master data is constant
// for the session, so once resolved it's cached forever (no per-race reset).
static SKILL_EFFECT: OnceLock<Mutex<HashMap<i32, String>>> = OnceLock::new();
fn skill_effect_buf() -> &'static Mutex<HashMap<i32, String>> {
    SKILL_EFFECT.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Cached effect string for a skill id (empty until resolved / if unknown). Read by the overlay.
pub fn skill_effect_of(id: i32) -> String {
    skill_effect_buf().lock().ok().and_then(|m| m.get(&id).cloned()).unwrap_or_default()
}
/// Read a skill's PRIMARY ability (type/value/time) from the game's in-memory master data and format
/// "+value unit · time". ×10000 fixed-point. Game-thread only (managed calls); call once per id.
unsafe fn compute_skill_effect(id: i32) -> String {
    let getmm = MM_GET.load(Ordering::Relaxed);
    let get = MSD_GET.load(Ordering::Relaxed);
    if getmm == 0 || get == 0 {
        return String::new();
    }
    let mut msd = MSD_INST.load(Ordering::Relaxed);
    if msd == 0 {
        let f: unsafe extern "C" fn(*const c_void) -> *mut c_void = std::mem::transmute(getmm);
        let mgr = f(MM_GET_MI.load(Ordering::Relaxed) as *const c_void);
        if mgr.is_null() {
            return String::new();
        }
        msd = ((mgr as usize + 0xc8) as *const usize).read_unaligned(); // <masterSkillData>k__BackingField
        if msd == 0 {
            return String::new();
        }
        MSD_INST.store(msd, Ordering::Relaxed);
    }
    let gf: unsafe extern "C" fn(*mut c_void, i32, *const c_void) -> *mut c_void = std::mem::transmute(get);
    let sd = gf(msd as *mut c_void, id, MSD_GET_MI.load(Ordering::Relaxed) as *const c_void);
    if sd.is_null() {
        return String::new();
    }
    let base = sd as usize;
    let atype = ((base + 0x6c) as *const i32).read_unaligned(); // AbilityType11
    let aval = ((base + 0x78) as *const i32).read_unaligned(); // FloatAbilityValue11 (×1e4)
    let atime = ((base + 0x60) as *const i32).read_unaligned(); // FloatAbilityTime1 (×1e4)
    if atype == 0 || aval == 0 {
        return String::new();
    }
    let v = aval as f32 / 10000.0; // ×1e4 fixed-point
    let t = atime as f32 / 10000.0;
    let dur = if t > 0.4 { format!(" {t:.1}s") } else { String::new() };
    // Ability types confirmed from real skill data: 1-5 = stat boosts (passive), 21/22/27 = target
    // speed (m/s), 31 = acceleration (m/s²), 9 = HP recovery, 10 = start (Focus). Verified in-game.
    match atype {
        1 => format!("+{v:.0} Speed"),
        2 => format!("+{v:.0} Stamina"),
        3 => format!("+{v:.0} Power"),
        4 => format!("+{v:.0} Guts"),
        5 => format!("+{v:.0} Wisdom"),
        21 | 22 | 27 => format!("+{v:.2} m/s{dur}"),
        31 => format!("+{v:.2} m/s2{dur}"),
        9 => format!("+{:.0}% HP", v * 100.0), // recovery as % of max HP
        _ => format!("+{v:.2}{dur}"),           // 10 (start) + any unmapped type: raw value
    }
}

/// For the followed Uma, append any newly-activated skills to the feed (resolving names).
/// Called only for the followed gate; GetSkillName runs only on genuinely new entries.
unsafe fn update_skill_feed(hri: *mut c_void) {
    let mo = SKILLMGR_OFF.load(Ordering::Relaxed);
    let lo = USEDLIST_OFF.load(Ordering::Relaxed);
    if mo == 0 || lo == 0 {
        return;
    }
    let mgr = ((hri as usize + mo) as *const usize).read_unaligned();
    if mgr == 0 {
        return;
    }
    let list = ((mgr + lo) as *const usize).read_unaligned();
    if list == 0 {
        return;
    }
    // List<int>: _items(int[])@0x10, _size@0x18.
    let items = ((list + 0x10) as *const usize).read_unaligned();
    let size = ((list + 0x18) as *const i32).read_unaligned();
    if items == 0 || size <= 0 || size > 64 {
        return;
    }
    let seen = FEED_SEEN.load(Ordering::Relaxed);
    if size as usize <= seen {
        return;
    }
    let base = (items + 0x20) as *const i32;
    if let Ok(mut feed) = skill_feed_buf().lock() {
        for i in seen..size as usize {
            let id = base.add(i).read_unaligned();
            feed.push((id, skill_name(id)));
            // Resolve + cache this skill's effect string once (game thread = safe for the managed call).
            let need = skill_effect_buf().lock().map(|m| !m.contains_key(&id)).unwrap_or(false);
            if need {
                let eff = compute_skill_effect(id);
                if let Ok(mut m) = skill_effect_buf().lock() {
                    m.insert(id, eff);
                }
            }
        }
    }
    FEED_SEEN.store(size as usize, Ordering::Relaxed);
}
// HorseData.<charaName> field offset (string) — for the gate→name map.
static NAME_OFF: AtomicUsize = AtomicUsize::new(0);

/// Per-horse live telemetry, collected each frame in the run-motion hook (raw reads + a
/// couple of cheap instance getters).
#[derive(Clone, Copy, Default)]
pub struct HorseTelem {
    pub gate: i32,
    pub order: i32, // CurOrder (1 = leading)
    pub hp: f32,
    pub max_hp: f32,
    pub speed: f32,    // m/s
    pub distance: f32, // metres covered
    pub spurt: bool,   // in last spurt (final sprint)
    pub exhausted: bool, // HP empty on race (out of stamina)
    pub skills: i32,   // skills activated so far
    pub late_start: bool, // IsBadStart — botched the gate (late start)
    pub fight: bool,    // IsCompeteFight — locked in a head-to-head duel
    pub leading: bool,  // IsCompeteTop — contesting the lead
    pub blocked: bool,  // BlockFrontContinueTime > 0 — boxed in behind another horse
    pub prev_order: i32, // last frame's order — for the position-trend arrow
    pub popularity: i32, // betting favourite rank (人気); 1 = top pick, 0 = unknown
    pub running_style: i32, // 1 Nige, 2 Senko, 3 Sashi, 4 Oikomi (0 = unknown)
    pub defeat: i32,     // DefeatType — why she can't win (0 none, 1 win, else a reason)
    pub wx: f32,         // world position X (for the track-map minimap)
    pub wz: f32,         // world position Z
}

static TELEM: OnceLock<Mutex<HashMap<i32, HorseTelem>>> = OnceLock::new();
fn telem_buf() -> &'static Mutex<HashMap<i32, HorseTelem>> {
    TELEM.get_or_init(|| Mutex::new(HashMap::new()))
}

// gate → Uma display name (from HorseData.charaName, captured in the ctor hook).
static NAMEMAP: OnceLock<Mutex<HashMap<i32, String>>> = OnceLock::new();
fn name_map() -> &'static Mutex<HashMap<i32, String>> {
    NAMEMAP.get_or_init(|| Mutex::new(HashMap::new()))
}

// gate → (trainer_name, viewer_id) from RaceHorseData. Static per race; read once per gate. Empty
// trainer / viewer 0 = an NPC (career races). Real lobbies (Team Trials, Champions, Room Match) carry
// the human trainer name, and umas of the same viewer_id are the same person's team (1-3 umas).
static TRAINERMAP: OnceLock<Mutex<HashMap<i32, (String, i64)>>> = OnceLock::new();
fn trainer_map() -> &'static Mutex<HashMap<i32, (String, i64)>> {
    TRAINERMAP.get_or_init(|| Mutex::new(HashMap::new()))
}

// gate → finish rank (1 = crossed the line first), captured the frame a Uma's distance first reaches
// the course distance. The tower keeps finished Umas in this exact crossing order during the run-out
// (where raw distances diverge by deceleration) so the on-stream order matches the real result.
static FINISHRANK: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
static FINISH_NEXT: AtomicI32 = AtomicI32::new(0);
fn finish_rank() -> &'static Mutex<HashMap<i32, i32>> {
    FINISHRANK.get_or_init(|| Mutex::new(HashMap::new()))
}

// gate → previous tower position (our DISTANCE-based order, not the game's unreliable CurOrder), so
// the position-change flash (green = gained, red = lost) reflects the real order shown. Refreshed on
// a short throttle so the trend stays non-zero long enough for the flash to catch it.
static PREVPOS: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
static LAST_POS_MS: AtomicU64 = AtomicU64::new(0);
fn prev_pos() -> &'static Mutex<HashMap<i32, i32>> {
    PREVPOS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn gate_name(gate: i32) -> String {
    name_map()
        .lock()
        .ok()
        .and_then(|m| m.get(&gate).cloned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("Gate {gate}"))
}

/// Decode a managed System.String (len@0x10, UTF-16 chars@0x14). Bounded to sane lengths.
unsafe fn read_managed_str(obj: *mut c_void) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    let len = ((obj as usize + 0x10) as *const i32).read_unaligned();
    if len <= 0 || len > 64 {
        return Some(String::new());
    }
    let chars = (obj as usize + 0x14) as *const u16;
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(chars, len as usize)))
}

/// Computed view for the HUD: the followed Uma + the adjacent rival (the one ahead, or the
/// one behind if the followed Uma is leading) + the gap between them.
#[derive(Clone)]
pub struct TelemView {
    pub followed: HorseTelem,
    pub followed_name: String,
    pub rival: Option<HorseTelem>,
    pub rival_name: String,
    pub rival_ahead: bool, // true = rival is ahead; false = rival behind (we're leading)
    pub gap: f32,          // metres between followed and rival
    pub field_size: i32,
    pub chara_id: i32,     // followed Uma's character id (for the portrait icon); 0 if unknown
}

/// Telemetry for the currently-followed Uma + its adjacent rival. None until a race is live.
pub fn telemetry() -> Option<TelemView> {
    let (followed, field_size, rival) = {
        let buf = telem_buf().lock().ok()?;
        if buf.is_empty() {
            return None;
        }
        let target = TARGET_GATE.load(Ordering::Relaxed);
        let followed = *buf.get(&target)?;
        let field_size = buf.len() as i32;
        // Rival = the one directly ahead (order-1); if leading (order 1), the one behind.
        let want_order = if followed.order > 1 { followed.order - 1 } else { followed.order + 1 };
        let rival = buf.values().find(|h| h.order == want_order).copied();
        (followed, field_size, rival)
    };
    let ahead = followed.order > 1;
    let gap = rival.map(|r| (r.distance - followed.distance).abs()).unwrap_or(0.0);
    let followed_name = gate_name(followed.gate);
    let rival_name = rival.map(|r| gate_name(r.gate)).unwrap_or_default();
    let chara_id = id_map().lock().ok().and_then(|m| m.get(&followed.gate).copied()).unwrap_or(0);
    Some(TelemView { followed, followed_name, rival, rival_name, rival_ahead: ahead, gap, field_size, chara_id })
}

/// One row of the broadcast timing tower — the whole field, leader-first.
#[derive(Clone)]
pub struct FieldRow {
    pub pos: i32,        // display position (1 = leader)
    pub gate: i32,
    pub name: String,
    pub style: i32,      // 1 Nige .. 4 Oikomi (0 = unknown)
    pub sta: f32,        // 0..1 stamina
    pub interval: f32,   // metres behind the horse directly ahead (0 for the leader)
    pub gap_leader: f32, // metres behind the leader
    pub trend: i32,      // places gained since last frame (prev_order - order)
    pub popularity: i32, // 人気 rank
    pub spurt: bool,
    pub exhausted: bool,
    pub fight: bool,
    pub blocked: bool,
    pub followed: bool,  // this is the camera's current target
    pub win: f32,        // live win probability 0..1 (physics ETA model)
    pub dist: f32,       // metres covered (for the phase/progress header)
    pub speed: f32,      // current m/s (for time-gap intervals)
    pub trainer: String, // human trainer name (lobby races); empty for NPCs
    pub viewer_id: i64,  // trainer id — same id = same person's team (1-3 umas); 0 = NPC
    pub wx: f32,         // world position X (track-map minimap)
    pub wz: f32,         // world position Z
}

/// The full field for the broadcast timing tower, ordered leader-first. Empty until a race is
/// live. Built from the same per-frame telemetry buffer the HUD uses — all pure reads, so this
/// is read-only and has no effect on the race.
pub fn field_rows() -> Vec<FieldRow> {
    let mut hs: Vec<HorseTelem> = match telem_buf().lock() {
        Ok(b) if !b.is_empty() => b.values().copied().collect(),
        _ => return Vec::new(),
    };
    let target = TARGET_GATE.load(Ordering::Relaxed);
    // Leader-first: by race order when valid, falling back to distance covered.
    // Order: Umas that have CROSSED the finish line first, in their exact CROSSING order (frozen rank),
    // then the still-racing Umas by DISTANCE covered (most = ahead). The game's CurOrder field is
    // unreliable (it resets at the line — used to send the winner to the bottom), and raw run-out
    // distance diverges after the line — so we freeze the crossing order for an on-stream-correct result.
    let franks = finish_rank().lock().map(|m| m.clone()).unwrap_or_default();
    hs.sort_by(|a, b| {
        match (franks.get(&a.gate), franks.get(&b.gate)) {
            (Some(x), Some(y)) => x.cmp(y),                  // both finished → crossing order
            (Some(_), None) => std::cmp::Ordering::Less,     // a finished, b racing → a ahead
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b.distance.partial_cmp(&a.distance).unwrap_or(std::cmp::Ordering::Equal),
        }
    });
    // pos is assigned later from this order; the trend (for the green/red flash) is computed below
    // from OUR distance order, NOT the game's CurOrder.
    let leader_dist = hs.first().map(|h| h.distance).unwrap_or(0.0);
    let n = hs.len().max(1) as f32;
    // ── live win-probability model: PHYSICS-grounded ETA-to-finish (HeavenSim-derived) ──
    // For each Uma we project its real time-to-finish using the game's EXACT HP-drain formula
    // (HpDecBase·gap²/SpeedGapParam1Pow, gap = speed − raceBaseSpeed + 12): if its current HP can
    // sustain its pace to the line it finishes at pace (small spurt bonus in the last 3F); if not,
    // it runs at pace until HP=0 then crawls home at the min-speed floor (raceBaseSpeed·MinSpeedRate).
    // Lowest ETA = most likely winner → softmax over −ETA. Early on, ETAs are near-tied so a betting
    // favourite (人気) prior leads; it fades to zero as the race resolves. Uses only live telemetry.
    let course = crate::race::course_distance() as f32;
    let progress = if course > 0.0 { (leader_dist / course).clamp(0.0, 1.0) } else { 0.0 };
    // raceBaseSpeed (REAL): 20 − (courseDistance − 2000)/1000. Min/gassed floor ≈ rbs·0.85 + guts bump.
    let rbs = if course > 0.0 { 20.0 - (course - 2000.0) / 1000.0 } else { 20.0 };
    let gassed = (rbs * 0.85 + 0.3).max(1.0);
    // expected time (seconds) for horse h to cover its remaining distance, from now
    let eta = |h: &HorseTelem| -> f32 {
        let remaining = if course > 0.0 { (course - h.distance).max(0.0) } else { 1.0 };
        if remaining <= 0.0 {
            return 0.0;
        }
        let spd = h.speed.max(rbs * 0.5).max(1.0);
        if h.exhausted || h.hp <= 1.0 {
            return remaining / gassed; // already gassed → crawl to the line
        }
        // real HP-drain rate at this pace (end-phase guts term approximated, no guts stat live)
        let gap = (spd - rbs + 12.0).max(0.0);
        let gmult = if progress >= 0.66 { 1.7 } else { 1.0 }; // tuned vs 700 real races
        let drain = (gap * gap / 144.0) * 20.0 * gmult;
        let t_at_pace = remaining / spd;
        let hp_needed = drain * t_at_pace;
        if h.hp >= hp_needed {
            let v = spd; // can sustain to the line (tuning showed no spurt-speed bonus helps)
            remaining / v
        } else {
            // gases out partway: hold pace until HP=0, then crawl the rest at the floor
            let t_survive = h.hp / drain.max(1e-3);
            let d_survive = (spd * t_survive).min(remaining);
            t_survive + (remaining - d_survive).max(0.0) / gassed
        }
    };
    let etas: Vec<f32> = hs.iter().map(eta).collect();
    let min_eta = etas.iter().cloned().fold(f32::MAX, f32::min);
    // softmax over −T·(ETA − best) + a fading 人気 prior. T≈1.2/s → ~1s ETA edge ≈ 3× odds.
    let logits: Vec<f32> = hs
        .iter()
        .zip(&etas)
        .map(|(h, e)| {
            let pop = if h.popularity > 0 { h.popularity as f32 } else { n * 0.5 };
            // Temperature sharpens as the race resolves (uncertain early → confident late), tuned
            // against 700 real races. Plus the fading 人気 favourite prior.
            let t = 0.8 + 5.0 * progress;
            -t * (e - min_eta) + 0.45 * (1.0 - progress) * -(pop - 1.0)
        })
        .collect();
    let maxl = logits.iter().cloned().fold(f32::MIN, f32::max);
    let exps: Vec<f32> = logits.iter().map(|l| (l - maxl).exp()).collect();
    let sum: f32 = exps.iter().sum::<f32>().max(1e-6);

    let mut prev_dist = leader_dist;
    let mut out = Vec::with_capacity(hs.len());
    for (i, h) in hs.iter().enumerate() {
        let interval = (prev_dist - h.distance).max(0.0);
        prev_dist = h.distance;
        out.push(FieldRow {
            pos: i as i32 + 1,
            gate: h.gate,
            name: gate_name(h.gate),
            style: h.running_style,
            sta: if h.max_hp > 0.0 { (h.hp / h.max_hp).clamp(0.0, 1.0) } else { 0.0 },
            interval,
            gap_leader: (leader_dist - h.distance).max(0.0),
            trend: if h.prev_order > 0 { h.prev_order - h.order } else { 0 },
            popularity: h.popularity,
            spurt: h.spurt,
            exhausted: h.exhausted,
            fight: h.fight,
            blocked: h.blocked,
            followed: h.gate == target,
            win: exps[i] / sum,
            dist: h.distance,
            speed: h.speed,
            trainer: trainer_map().lock().ok().and_then(|m| m.get(&h.gate).map(|(n, _)| n.clone())).unwrap_or_default(),
            viewer_id: trainer_map().lock().ok().and_then(|m| m.get(&h.gate).map(|(_, v)| *v)).unwrap_or(0),
            wx: h.wx,
            wz: h.wz,
        });
    }
    // Override the trend with OUR distance-based position change (the game's CurOrder is unreliable).
    // +ve = gained a place (moved up) → green flash; -ve = lost → red. The prev-position snapshot is
    // throttled so the trend stays non-zero long enough for the flash, and so the several field_rows
    // calls per frame all read the same value.
    {
        let now = clock().elapsed().as_millis() as u64;
        let refresh = now.saturating_sub(LAST_POS_MS.load(Ordering::Relaxed)) > 150;
        if let Ok(mut prev) = prev_pos().lock() {
            for r in out.iter_mut() {
                r.trend = prev.get(&r.gate).map(|p| p - r.pos).unwrap_or(0);
            }
            if refresh {
                for r in &out {
                    prev.insert(r.gate, r.pos);
                }
                LAST_POS_MS.store(now, Ordering::Relaxed);
            }
        }
    }
    out
}

// ── live speed history (followed Uma) — the WHOLE race, sampled by PROGRESS, drawn left→right ──
/// Number of buckets across 0→100 % of the course. The pace graph fills these as the race runs.
pub const PACE_BUCKETS: usize = 140;
static SPEED_TRACE: OnceLock<Mutex<Vec<f32>>> = OnceLock::new();
fn speed_trace_buf() -> &'static Mutex<Vec<f32>> {
    SPEED_TRACE.get_or_init(|| Mutex::new(Vec::new()))
}
/// The followed Uma's whole-race speed history: one sample per progress bucket, index 0 = race start,
/// the last filled index = current progress. Drawn as a graph that builds left→right over the race.
pub fn speed_trace() -> Vec<f32> {
    speed_trace_buf().lock().map(|t| t.clone()).unwrap_or_default()
}
/// Record the followed Uma's speed at its current race progress (0..1). Fills any skipped buckets up
/// to the current one, so the history only ever grows rightward.
fn push_pace(progress: f32, v: f32) {
    let b = ((progress.clamp(0.0, 1.0) * PACE_BUCKETS as f32) as usize).min(PACE_BUCKETS - 1);
    if let Ok(mut t) = speed_trace_buf().lock() {
        while t.len() <= b {
            let fill = *t.last().unwrap_or(&v);
            t.push(fill);
        }
        t[b] = v;
    }
}
/// Clear the pace history + live outlook (on a new race / when switching followed Uma).
fn reset_pace() {
    if let Ok(mut t) = speed_trace_buf().lock() {
        t.clear();
    }
    reset_outlook();
}
// gate → charaId (HorseData.charaId), captured in the ctor hook — for the portrait icon.
static IDMAP: OnceLock<Mutex<HashMap<i32, i32>>> = OnceLock::new();
fn id_map() -> &'static Mutex<HashMap<i32, i32>> {
    IDMAP.get_or_init(|| Mutex::new(HashMap::new()))
}
static CHARAID_OFF: AtomicUsize = AtomicUsize::new(0);
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
/// True only while a race is actually live and we're following a Uma (recency-gated). The
/// telemetry HUD uses this so it shows ONLY during a race, never out in the menus.
pub fn race_active() -> bool {
    // Driven by the telemetry toggle (independent of the freecam). Stays up while the race is running
    // (fresh telemetry) OR while we're still in the race SCENE (the race-camera manager keeps ticking
    // through the static result screen — "you finished Nth") — so the data persists on that screen and
    // only hides once you advance past it (the scene transitions and `in_race` goes stale).
    crate::settings::telemetry() && (telem_fresh() || in_race())
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
    let gate = gate_map()
        .lock()
        .ok()
        .map(|m| m.get(&(this as usize)).copied().unwrap_or(-1))
        .unwrap_or(-1);
    // Live telemetry for EVERY horse (the HUD reads the followed Uma + its rival from this).
    if gate > 0 && HP_OFF.load(Ordering::Relaxed) != 0 {
        let rdf = |o: &AtomicUsize| -> f32 {
            let v = o.load(Ordering::Relaxed);
            if v == 0 { 0.0 } else { ((this as usize + v) as *const f32).read_unaligned() }
        };
        let ord_off = ORDER_OFF.load(Ordering::Relaxed);
        let order = if ord_off == 0 { 0 } else { ((this as usize + ord_off) as *const i32).read_unaligned() };
        // PURE field reads only — NO managed method calls here (calling getters per-horse-per-frame
        // inside the run-motion hook perturbed the horses / stuck them at the gate).
        // Last spurt = the race's final leg (phase >= 2, i.e. from ~2/3 distance). _phase is a
        // pure i32 field; phases: 0 start, 1 middle, 2 final leg, 3 last-spurt stretch, 4 finish.
        let ph_off = PHASE_OFF.load(Ordering::Relaxed);
        let spurt = ph_off != 0 && ((this as usize + ph_off) as *const i32).read_unaligned() >= 2;
        let hpe_off = HPEMPTY_OFF.load(Ordering::Relaxed);
        let exhausted = hpe_off != 0 && ((this as usize + hpe_off) as *const u8).read_unaligned() != 0;
        // Live race-state flags — pure bool/i32/float reads (same safety profile as the above).
        let rdb = |o: &AtomicUsize| -> bool {
            let v = o.load(Ordering::Relaxed);
            v != 0 && ((this as usize + v) as *const u8).read_unaligned() != 0
        };
        let blk_off = BLOCKFRONT_OFF.load(Ordering::Relaxed);
        let blocked = blk_off != 0 && ((this as usize + blk_off) as *const f32).read_unaligned() > 0.0;
        let pvo_off = PREVORDER_OFF.load(Ordering::Relaxed);
        let prev_order = if pvo_off == 0 { 0 } else { ((this as usize + pvo_off) as *const i32).read_unaligned() };
        let def_off = DEFEAT_OFF.load(Ordering::Relaxed);
        let defeat = if def_off == 0 { 0 } else { ((this as usize + def_off) as *const i32).read_unaligned() };
        // World position (X,Z) for the track-map minimap — pure Vector3 field read.
        let (mut wx, mut wz) = (0.0f32, 0.0f32);
        let po = POS_OFF.load(Ordering::Relaxed);
        if po != 0 {
            let p = (this as usize + po) as *const f32;
            wx = p.read_unaligned();
            wz = p.add(2).read_unaligned();
        }
        // Identity (static per race) — pure pointer-chase, no managed calls: popularity off the
        // HorseData, running style off its server-response data. 0 if any link is absent.
        let (mut popularity, mut running_style) = (0i32, 0i32);
        let hd_off = HDATA_OFF.load(Ordering::Relaxed);
        if hd_off != 0 {
            let hd = ((this as usize + hd_off) as *const *mut c_void).read_unaligned();
            if !hd.is_null() {
                let po = POP_OFF.load(Ordering::Relaxed);
                if po != 0 {
                    popularity = ((hd as usize + po) as *const i32).read_unaligned();
                }
                let ro = RESP_OFF.load(Ordering::Relaxed);
                if ro != 0 {
                    let resp = ((hd as usize + ro) as *const *mut c_void).read_unaligned();
                    if !resp.is_null() {
                        let so = RSTYLE_OFF.load(Ordering::Relaxed);
                        if so != 0 {
                            running_style = ((resp as usize + so) as *const i32).read_unaligned();
                        }
                        // Trainer identity is static per race → read the managed string just once per gate.
                        let unknown = trainer_map().lock().map(|m| !m.contains_key(&gate)).unwrap_or(false);
                        if unknown {
                            let to = TNAME_OFF.load(Ordering::Relaxed);
                            let vo = VIEWER_OFF.load(Ordering::Relaxed);
                            let tname = if to != 0 {
                                let sp = ((resp as usize + to) as *const *mut c_void).read_unaligned();
                                read_managed_str(sp).unwrap_or_default()
                            } else {
                                String::new()
                            };
                            let vid = if vo != 0 { ((resp as usize + vo) as *const i64).read_unaligned() } else { 0 };
                            if let Ok(mut m) = trainer_map().lock() {
                                m.insert(gate, (tname, vid));
                            }
                        }
                    }
                }
            }
        }
        let t = HorseTelem {
            gate,
            order,
            hp: rdf(&HP_OFF),
            max_hp: rdf(&MAXHP_OFF),
            speed: rdf(&SPEED_OFF),
            distance: rdf(&DIST_OFF),
            spurt,
            exhausted,
            skills: 0, // per-uma skills come from the activation FEED (skill_feed())
            late_start: rdb(&BADSTART_OFF),
            fight: rdb(&COMPFIGHT_OFF),
            leading: rdb(&COMPTOP_OFF),
            blocked,
            prev_order,
            popularity,
            running_style,
            defeat,
            wx,
            wz,
        };
        // Heartbeat + new-race detection: if telemetry resumed after a real gap (we left the race
        // and a new one started — even with the SAME player gate), wipe last race's data so nothing
        // freezes over. Then mark the heartbeat fresh.
        let now_ms = clock().elapsed().as_millis() as u64;
        if now_ms.saturating_sub(LAST_TELEM_MS.load(Ordering::Relaxed)) > 800 {
            if let Ok(mut b) = telem_buf().lock() {
                b.clear();
            }
            if let Ok(mut m) = trainer_map().lock() {
                m.clear();
            }
            if let Ok(mut m) = finish_rank().lock() {
                m.clear();
            }
            FINISH_NEXT.store(0, Ordering::Relaxed);
            if let Ok(mut m) = prev_pos().lock() {
                m.clear();
            }
            reset_skill_feed();
            reset_pace();
            RACE_EPOCH.fetch_add(1, Ordering::Relaxed);
        }
        LAST_TELEM_MS.store(now_ms, Ordering::Relaxed);
        if let Ok(mut b) = telem_buf().lock() {
            b.insert(gate, t);
        }
        let course = crate::race::course_distance() as f32;
        // Finish-line crossing: the FIRST frame a Uma reaches the course distance, capture its finish
        // rank (the order it crossed) so the tower freezes that order during the run-out.
        if course > 0.0 && t.distance >= course {
            let unranked = finish_rank().lock().map(|m| !m.contains_key(&gate)).unwrap_or(false);
            if unranked {
                let rank = FINISH_NEXT.fetch_add(1, Ordering::Relaxed) + 1;
                if let Ok(mut m) = finish_rank().lock() {
                    m.insert(gate, rank);
                }
            }
        }
        // Pace history: sample the FOLLOWED Uma's speed at its current race progress (works in
        // telemetry-only too, since this runs whenever telemetry is on — not just under freecam).
        if gate == TARGET_GATE.load(Ordering::Relaxed) && course > 0.0 && t.speed > 0.5 {
            push_pace(t.distance / course, t.speed);
        }
    }

    // Camera capture below is freecam-only (telemetry needs no camera control).
    if !fc || gate != TARGET_GATE.load(Ordering::Relaxed) {
        return ret;
    }
    // Apply this circuit's DEFAULT preset once the track id is known (it may be 0 at race start,
    // so loading it in auto_follow_player would miss the user's saved pose). Once per race.
    if !RACE_POSE_LOADED.load(Ordering::Relaxed) && crate::race::track_id() != 0 {
        load_default_pose();
        RACE_POSE_LOADED.store(true, Ordering::Relaxed);
    }
    // Followed Uma only: skill feed + live outlook.
    update_skill_feed(this);
    update_active_skills(this); // pure field walk, no managed call → safe
    update_follow_state(this); // kakari / position-keep / down-slope (AI real getters)
    // Spurt sustainability: call the AI's REAL getter (unique RVA, not the HorseRaceInfo stub),
    // and only once the spurt phase has started (phase>=2) so the calculator is populated. Guarded
    // by a non-null AI pointer.
    {
        let ph_off = PHASE_OFF.load(Ordering::Relaxed);
        let in_spurt = ph_off != 0 && ((this as usize + ph_off) as *const i32).read_unaligned() >= 2;
        let sg = AI_SPURT_GET.load(Ordering::Relaxed);
        let ai_off = AI_OFF.load(Ordering::Relaxed);
        if in_spurt && sg != 0 && ai_off != 0 {
            let ai = ((this as usize + ai_off) as *const *mut c_void).read_unaligned();
            if !ai.is_null() {
                let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32 = std::mem::transmute(sg);
                SPURT_OUTLOOK.store(f(ai, std::ptr::null_mut()), Ordering::Relaxed);
            }
        }
    }
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
            // Capture the Uma's display name (HorseData.charaName) → gate→name map for the HUD.
            let noff = NAME_OFF.load(Ordering::Relaxed);
            if noff != 0 {
                let sp = ((data as usize + noff) as *const *mut c_void).read_unaligned();
                if let Some(name) = read_managed_str(sp) {
                    if !name.is_empty() {
                        if let Ok(mut nm) = name_map().lock() {
                            nm.insert(gate, name);
                        }
                    }
                }
            }
            // Capture charaId (HorseData.charaId) → gate→id map for the portrait icon.
            let coff = CHARAID_OFF.load(Ordering::Relaxed);
            if coff != 0 {
                let cid = ((data as usize + coff) as *const i32).read_unaligned();
                if cid > 0 {
                    if let Ok(mut m) = id_map().lock() {
                        m.insert(gate, cid);
                    }
                }
            }
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
    // Live telemetry offsets (HorseRaceInfo). All on the same class the run-motion hook's
    // `this` derives from, so they read straight off `this`.
    for (name, slot) in [
        ("_hp", &HP_OFF),
        ("_maxHp", &MAXHP_OFF),
        ("<CurOrder>k__BackingField", &ORDER_OFF),
        ("_lastSpeed", &SPEED_OFF),
        ("_distance", &DIST_OFF),
        ("<IsHpEmptyOnRace>k__BackingField", &HPEMPTY_OFF),
        ("_phase", &PHASE_OFF),
        ("<IsBadStart>k__BackingField", &BADSTART_OFF),
        ("<IsCompeteFight>k__BackingField", &COMPFIGHT_OFF),
        ("<IsCompeteTop>k__BackingField", &COMPTOP_OFF),
        ("<BlockFrontContinueTime>k__BackingField", &BLOCKFRONT_OFF),
        ("<PrevOrder>k__BackingField", &PREVORDER_OFF),
        ("_defeat", &DEFEAT_OFF),
    ] {
        if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), name) {
            slot.store(off, Ordering::Relaxed);
        }
    }
    if HP_OFF.load(Ordering::Relaxed) != 0 {
        got.push("telem");
    }
    // Identity chain for the broadcast tower: HorseRaceInfo._horseData → HorseData
    // (.<Popularity>, ._responseHorseData) → RaceHorseData.running_style.
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), "_horseData") {
        HDATA_OFF.store(off, Ordering::Relaxed);
    }
    let hdc = il2cpp::class("Gallop.HorseData");
    if let Some(off) = il2cpp::field_offset(hdc, "<Popularity>k__BackingField") {
        POP_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(hdc, "_responseHorseData") {
        RESP_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.RaceHorseData"), "running_style") {
        RSTYLE_OFF.store(off, Ordering::Relaxed);
    }
    // Trainer identity (lobby races): RaceHorseData.trainer_name + .viewer_id.
    let rhd = il2cpp::class("Gallop.RaceHorseData");
    if let Some(off) = il2cpp::field_offset(rhd, "trainer_name") {
        TNAME_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(rhd, "viewer_id") {
        VIEWER_OFF.store(off, Ordering::Relaxed);
    }
    // Skill activation feed: HorseRaceInfo._skillManager → SkillManager._usedSkillIdList,
    // + MasterDataUtil.GetSkillName(id) for names.
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), "_skillManager") {
        SKILLMGR_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.SkillManager"), "_usedSkillIdList") {
        USEDLIST_OFF.store(off, Ordering::Relaxed);
    }
    // Spurt sustainability: HorseRaceInfo._horseRaceAI + the AI's REAL getter (HorseRaceAIReplay,
    // unique RVA — NOT the HorseRaceInfo interface stub).
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseRaceInfo"), "_horseRaceAI") {
        AI_OFF.store(off, Ordering::Relaxed);
    }
    let aisg = il2cpp::method(il2cpp::class("Gallop.HorseRaceAIReplay"), "get_LastSpurtCalcResult", 0);
    if !aisg.is_null() {
        AI_SPURT_GET.store(il2cpp::method_pointer(aisg) as usize, Ordering::Relaxed);
    }
    // Live race-state getters (kakari / position-keep / down-slope) on HorseRaceAIBase.
    let aib = il2cpp::class("Gallop.HorseRaceAIBase");
    for (name, slot) in [
        ("get_IsTemptation", &KAKARI_GET),
        ("get_TemptationMode", &TEMPTMODE_GET),
        ("get_PositionKeepMode", &KEEPMODE_GET),
        ("get_IsDownSlopeAccelMode", &DOWNHILL_GET),
    ] {
        let m = il2cpp::method(aib, name, 0);
        if !m.is_null() {
            slot.store(il2cpp::method_pointer(m) as usize, Ordering::Relaxed);
        }
    }
    // Active-skill field walk: SkillManager._skills → SkillBase.{SkillMaster,Details} → SkillDetail.
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.SkillManager"), "_skills") {
        SKILLS_LIST_OFF.store(off, Ordering::Relaxed);
    }
    let sbc = il2cpp::class("Gallop.SkillBase");
    if let Some(off) = il2cpp::field_offset(sbc, "<SkillMaster>k__BackingField") {
        SB_MASTER_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(sbc, "<Details>k__BackingField") {
        SB_DETAILS_OFF.store(off, Ordering::Relaxed);
    }
    let sdc = il2cpp::class("Gallop.SkillDetail");
    if let Some(off) = il2cpp::field_offset(sdc, "<LeftTime>k__BackingField") {
        SD_LEFT_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(sdc, "<Category>k__BackingField") {
        SD_CAT_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(sdc, "_isDebuff") {
        SD_DEBUFF_OFF.store(off, Ordering::Relaxed);
    }
    if let Some(off) = il2cpp::field_offset(sdc, "<IsActivated>k__BackingField") {
        SD_ACT_OFF.store(off, Ordering::Relaxed);
    }
    let mdu = il2cpp::class("Gallop.MasterDataUtil");
    let gsn = il2cpp::method(mdu, "GetSkillName", 1);
    if !gsn.is_null() {
        GSN_FN.store(il2cpp::method_pointer(gsn) as usize, Ordering::Relaxed);
        GSN_MI.store(gsn as usize, Ordering::Relaxed);
        let is_static = unsafe {
            match h::METHOD_GET_FLAGS {
                Some(f) => (f(gsn as *mut h::RawMethod, std::ptr::null_mut()) & h::METHOD_ATTRIBUTE_STATIC) != 0,
                None => true,
            }
        };
        GSN_STATIC.store(is_static, Ordering::Relaxed);
    }
    // Skill effect-value lookup (in-memory master data; public-safe — no the game data).
    let wtc = il2cpp::class("Gallop.WorkTrainingChallengeData");
    let mmget = il2cpp::method(wtc, "get_MasterManager", 0);
    if !mmget.is_null() {
        MM_GET.store(il2cpp::method_pointer(mmget) as usize, Ordering::Relaxed);
        MM_GET_MI.store(mmget as usize, Ordering::Relaxed);
    }
    let msd_cls = il2cpp::class("Gallop.MasterSkillData");
    let sget = il2cpp::method(msd_cls, "Get", 1);
    if !sget.is_null() {
        MSD_GET.store(il2cpp::method_pointer(sget) as usize, Ordering::Relaxed);
        MSD_GET_MI.store(sget as usize, Ordering::Relaxed);
    }
    // HorseData.charaName (string) → Uma display name for the HUD.
    if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseData"), "<charaName>k__BackingField") {
        NAME_OFF.store(off, Ordering::Relaxed);
    }
    // HorseData.charaId (int) → portrait icon lookup. Try a couple of likely field names.
    for fname in ["<charaId>k__BackingField", "charaId", "_charaId"] {
        if let Some(off) = il2cpp::field_offset(il2cpp::class("Gallop.HorseData"), fname) {
            CHARAID_OFF.store(off, Ordering::Relaxed);
            break;
        }
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
