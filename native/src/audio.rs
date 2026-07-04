//! Heaven — native intro-song playback.
//!
//! Plays the intro track while the video runs (title scene), stops on skip/leave. The OGG
//! is read from `intro_song.ogg` next to the DLL (same drop-in model as `intro_full.bin`),
//! so swapping the intro never needs a rebuild. rodio's OutputStream is `!Send`, so a
//! dedicated thread owns the device + sink and obeys commands posted via an atomic.

#![allow(dead_code)]

use std::io::Cursor;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rodio::Source; // for `.repeat_infinite()` — the song loops with the looping video

/// The wall-clock instant at which the intro song actually began playing (device open + first
/// sample queued). The video player uses THIS as its frame-clock origin so the two stay locked
/// — the video holds frame 0 until audio truly starts, then both advance together. `None` while
/// not playing.
static START_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// When the intro audio actually started (for the video to sync its clock to). `None` if not
/// playing yet.
pub fn playback_start() -> Option<Instant> {
    START_AT.lock().ok().and_then(|g| *g)
}

/// The intro track, read once from `intro_song.ogg` next to the DLL. `None` if absent
/// (the video then plays without our audio).
fn song() -> Option<&'static [u8]> {
    static SONG: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    SONG.get_or_init(|| std::fs::read(crate::paths::local_file("intro_song.ogg")).ok())
        .as_deref()
}

// 0 = no command, 1 = play (from start), 2 = stop.
static CMD: AtomicU8 = AtomicU8::new(0);

pub fn play() {
    CMD.store(1, Ordering::Relaxed);
}
pub fn stop() {
    CMD.store(2, Ordering::Relaxed);
}

fn log(msg: &str) {
    crate::tools::log(msg);
}

/// Spawn the audio worker. The output device (and its WASAPI/COM backend thread) is
/// opened ONLY while the intro song plays and dropped the moment it stops or finishes.
/// A rodio `OutputStream` kept alive for the whole process never gets dropped cleanly,
/// so its backend thread deadlocks during `ExitProcess` → the game hangs on close.
/// Scoping the stream to playback means that by the time the user quits (normally
/// outside the title) no device is open and shutdown is clean.
pub fn spawn() {
    std::thread::spawn(|| {
        // (stream, _handle, sink) dropped together when playback ends.
        let mut active: Option<(rodio::OutputStream, rodio::OutputStreamHandle, rodio::Sink)> =
            None;
        // True while a song is queued but the device hasn't begun emitting sound yet — we resolve
        // the real origin (stamp START_AT) only once sink.get_pos() advances past 0.
        let mut origin_pending = false;
        loop {
            match CMD.swap(0, Ordering::Relaxed) {
                1 => {
                    active = None; // drop any previous stream first
                    origin_pending = false;
                    if let Ok(mut g) = START_AT.lock() { *g = None; }
                    let Some(bytes) = song() else {
                        // No song: mark a start instant NOW so the video plays (silently) on the
                        // wall clock — there's no audio to sync to.
                        if let Ok(mut g) = START_AT.lock() { *g = Some(Instant::now()); }
                        log("[audio] no intro_song.ogg — skipping audio");
                        continue;
                    };
                    match rodio::OutputStream::try_default() {
                        Ok((stream, handle)) => match rodio::Sink::try_new(&handle) {
                            Ok(sink) => match rodio::Decoder::new(Cursor::new(bytes)) {
                                Ok(src) => {
                                    // The intro video loops (worker wraps frames), so loop the
                                    // song too — otherwise the audio falls silent after one pass.
                                    sink.append(src.repeat_infinite());
                                    // Do NOT stamp START_AT here. `append` only QUEUES audio; on
                                    // slower machines the device buffer takes up to a few seconds to
                                    // actually emit sound, so anchoring the video clock to queue-time
                                    // made the video race ahead of the song (A/V desync). We wait for
                                    // the TRUE start below (sink.get_pos()>0) and back-date the origin.
                                    origin_pending = true;
                                    active = Some((stream, handle, sink));
                                    log("[audio] play (awaiting true start)");
                                }
                                Err(e) => {
                                    if let Ok(mut g) = START_AT.lock() { *g = Some(Instant::now()); }
                                    log(&format!("[audio] decode err: {e}"));
                                }
                            },
                            Err(e) => {
                                if let Ok(mut g) = START_AT.lock() { *g = Some(Instant::now()); }
                                log(&format!("[audio] sink err: {e}"));
                            }
                        },
                        Err(e) => {
                            if let Ok(mut g) = START_AT.lock() { *g = Some(Instant::now()); }
                            log(&format!("[audio] no output device: {e}"));
                        }
                    }
                }
                2 => {
                    origin_pending = false;
                    if let Ok(mut g) = START_AT.lock() { *g = None; }
                    if active.take().is_some() {
                        log("[audio] stop");
                    }
                }
                _ => {
                    // Song finished on its own → close the device so it can't linger
                    // and block process shutdown later.
                    if let Some((_, _, sink)) = active.as_ref() {
                        if sink.empty() {
                            active = None;
                            log("[audio] finished");
                        }
                    }
                }
            }
            // Resolve the TRUE playback origin: rodio's get_pos() only advances past 0 once the
            // device actually pulls samples (sound out). Back-date the origin so the video locks to
            // audible audio on any PC instead of to queue time.
            if origin_pending {
                match active.as_ref() {
                    Some((_, _, sink)) => {
                        let pos = sink.get_pos();
                        if pos > Duration::ZERO {
                            let origin = Instant::now().checked_sub(pos).unwrap_or_else(Instant::now);
                            if let Ok(mut g) = START_AT.lock() { *g = Some(origin); }
                            origin_pending = false;
                            log(&format!("[audio] true start (get_pos={}ms)", pos.as_millis()));
                        }
                    }
                    None => origin_pending = false,
                }
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });
}
