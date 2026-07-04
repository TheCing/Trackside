//! Heaven — load / stall profiler (diagnostic).
//!
//! Quantifies where per-frame time goes so "slow menus / long loads" can be judged
//! REAL vs perceived. Everything is measured in milliseconds under a stable category:
//!   net.decompress — `Gallop.HttpHelper.DecompressResponse` (the game's own decrypt + lz4)
//!   net.parse      — Heaven's msgpack scan of that response (uma_bridge / race / the full build)
//!   heaven.render  — Heaven's per-frame imgui overlay build (are WE the cost?)
//!   heaven.pump    — Heaven's per-frame main-thread pumps (hunter/padder/reset/affinity)
//!   stall          — main-thread frame gaps: ANY freeze the player actually perceives
//!
//! Cheap always-on aggregates (count/avg/max/last/#over). A CSV line is written only when a
//! sample meets its warn threshold, and a full summary snapshot is flushed every ~10 s. Output
//! lands next to the other Heaven logs, in `<dll dir>/heaven-logs/`:
//!   heaven-loadprof.csv          — every notable (over-threshold) sample, with a T+seconds stamp
//!   heaven-loadprof-summary.txt  — rolling aggregate table
//! Not an advantage feature — pure instrumentation, safe in every build.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

// Warn thresholds (ms): a sample at/over this is written to the CSV as notable. Tuned so an idle
// 60 fps frame (~16 ms) is silent and only genuine hitches surface.
const WARN_DECOMPRESS: f64 = 40.0;
const WARN_PARSE: f64 = 15.0;
const WARN_RENDER: f64 = 8.0;
const WARN_PUMP: f64 = 4.0;
const WARN_STALL: f64 = 200.0; // inter-frame gap over this = a hitch worth a line

static ENABLED: AtomicBool = AtomicBool::new(true);
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[derive(Default, Clone)]
struct Agg {
    count: u64,
    sum: f64,
    max: f64,
    last: f64,
    over: u64,
    worst_at_s: f64,
}

fn table() -> &'static Mutex<BTreeMap<&'static str, Agg>> {
    static T: OnceLock<Mutex<BTreeMap<&'static str, Agg>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[inline]
fn secs() -> f64 {
    crate::tools::clock().elapsed().as_secs_f64()
}

/// Record a timed sample under `cat` (a stable &'static label). Updates the aggregate and, if the
/// sample meets/exceeds `warn`, appends a CSV line. `detail` is a free note (size, label…).
pub fn note(cat: &'static str, ms: f64, warn: f64, detail: &str) {
    if !is_enabled() {
        return;
    }
    let at = secs();
    let over = ms >= warn;
    if let Ok(mut m) = table().lock() {
        let e = m.entry(cat).or_default();
        e.count += 1;
        e.sum += ms;
        e.last = ms;
        if ms > e.max {
            e.max = ms;
            e.worst_at_s = at;
        }
        if over {
            e.over += 1;
        }
    }
    if over {
        csv_line(at, cat, ms, detail);
    }
}

fn csv_line(at: f64, cat: &str, ms: f64, detail: &str) {
    let path = crate::paths::log_file("heaven-loadprof.csv");
    let need_header = !path.exists();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        if need_header {
            let _ = writeln!(f, "t_seconds,category,ms,detail");
        }
        let _ = writeln!(f, "{at:.1},{cat},{ms:.1},{detail}");
    }
}

// ── frame stall + Heaven's own render cost ─────────────────────────────────
static LAST_FRAME_US: AtomicU64 = AtomicU64::new(0);
static LAST_DUMP_S: AtomicU64 = AtomicU64::new(0);

/// Call once at the very top of the per-frame overlay render. Detects the gap since the previous
/// frame (a large gap = the MAIN THREAD was blocked = a stall the player feels) and returns a scope
/// guard that records Heaven's own render cost on drop. Also flushes the summary file every ~10 s.
pub fn frame() -> FrameScope {
    if is_enabled() {
        let now_us = crate::tools::clock().elapsed().as_micros() as u64;
        let last = LAST_FRAME_US.swap(now_us, Ordering::Relaxed);
        if last != 0 && now_us > last {
            let gap = (now_us - last) as f64 / 1000.0;
            if gap >= WARN_STALL {
                note("stall", gap, WARN_STALL, "frame-gap");
            }
        }
        // periodic summary flush (~10 s) — CAS so only one frame writes per window.
        let s = now_us / 1_000_000;
        let prev = LAST_DUMP_S.load(Ordering::Relaxed);
        if s >= prev + 10
            && LAST_DUMP_S
                .compare_exchange(prev, s, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            dump_summary();
        }
    }
    FrameScope { t: Instant::now() }
}

pub struct FrameScope {
    t: Instant,
}
impl Drop for FrameScope {
    fn drop(&mut self) {
        if is_enabled() {
            let ms = self.t.elapsed().as_secs_f64() * 1000.0;
            note("heaven.render", ms, WARN_RENDER, "overlay");
        }
    }
}

// ── convenience wrappers for the wired call sites ──────────────────────────
pub fn decompress(len: usize, ms: f64) {
    note("net.decompress", ms, WARN_DECOMPRESS, &format!("{}KB", len / 1024));
}
pub fn parse(ms: f64, detail: &str) {
    note("net.parse", ms, WARN_PARSE, detail);
}
pub fn pump(ms: f64) {
    note("heaven.pump", ms, WARN_PUMP, "tween-pumps");
}

/// A formatted aggregate table of everything measured so far (summary file + a menu panel).
pub fn report() -> String {
    let mut out = format!("Heaven load profiler @ T+{:.0}s\n", secs());
    out.push_str(&format!(
        "{:<15}{:>7}{:>9}{:>9}{:>9}{:>7}   worst\n",
        "category", "n", "avg", "max", "last", "#warn"
    ));
    if let Ok(m) = table().lock() {
        for (cat, e) in m.iter() {
            let avg = if e.count > 0 { e.sum / e.count as f64 } else { 0.0 };
            out.push_str(&format!(
                "{:<15}{:>7}{:>9.1}{:>9.1}{:>9.1}{:>7}   @T+{:.0}s\n",
                cat, e.count, avg, e.max, e.last, e.over, e.worst_at_s
            ));
        }
    }
    out
}

/// Snapshot of the aggregate table for a live menu panel: (category, count, avg, max, over).
pub fn snapshot() -> Vec<(String, u64, f64, f64, u64)> {
    let mut v = Vec::new();
    if let Ok(m) = table().lock() {
        for (cat, e) in m.iter() {
            let avg = if e.count > 0 { e.sum / e.count as f64 } else { 0.0 };
            v.push((cat.to_string(), e.count, avg, e.max, e.over));
        }
    }
    v
}

pub fn dump_summary() {
    let path = crate::paths::log_file("heaven-loadprof-summary.txt");
    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(report().as_bytes());
    }
}

/// Clear all aggregates (menu "reset" button); the CSV history is kept.
pub fn reset() {
    if let Ok(mut m) = table().lock() {
        m.clear();
    }
    LAST_FRAME_US.store(0, Ordering::Relaxed);
}
