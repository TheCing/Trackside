//! Heaven Plan B — native bootstrap.
//!
//! On DLL attach we cannot touch IL2CPP yet (GameAssembly.dll loads after our
//! DllMain), so we spawn a worker thread that:
//!   1) waits for GameAssembly.dll,
//!   2) resolves the IL2CPP C API + attaches the thread to the domain,
//!   3) installs every native module (career reader, SuperSkip, FPS, race),
//!   4) marks the engine ready.
//! From then on the game's own threads drive our hooks and the overlay renders
//! the shared state. No Frida, no Python — this is the full-native runtime.
//!
//! A concise startup report is written to logs/heaven-native.log.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::Duration;

use crate::fps;
use crate::htt;
use crate::il2cpp;
use crate::ipc;
#[cfg(feature = "raceread")]
use crate::race;
use crate::settings;
use crate::skip;

fn log(msg: &str) {
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Spawn the native engine thread. hudhook re-invokes `new_with_engine()` (→ this) EVERY time it
/// rebuilds the render loop on a D3D swapchain reset (window resize / display-mode change), so this
/// can be called dozens of times per process. Boot must run ONCE: the IL2CPP hooks it installs live
/// for the whole process, and re-running it spawns redundant boot / scene-probe / audio threads —
/// the extra IL2CPP-attached probe threads are a GC hazard (a stray one alive during a collection
/// trips "Collecting from unknown thread", e.g. at graduation/career-end). Guard it process-once.
/// The D3D capture + overlay in `new_with_engine` correctly still run each time (new device).
pub fn spawn() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static BOOTED: AtomicBool = AtomicBool::new(false);
    if BOOTED.swap(true, Ordering::SeqCst) {
        return; // already booted this process
    }
    std::thread::spawn(|| {
        log("==== Heaven native engine starting ====");
        log("Heaven MOD — made by Night DC : nighty3333");
        ipc::set_status("waiting for GameAssembly…");

        // 1) Wait for GameAssembly.dll.
        let mut waited: u64 = 0;
        while !il2cpp::game_loaded() {
            std::thread::sleep(Duration::from_millis(250));
            waited += 250;
            if waited > 180_000 {
                log("TIMEOUT: GameAssembly.dll never appeared");
                return;
            }
        }
        log(&format!("step1: GameAssembly loaded ({waited}ms)"));

        // 2) Resolve the IL2CPP exports (GetProcAddress only — no managed calls).
        if let Err(e) = il2cpp::init() {
            log(&format!("step2: il2cpp::init FAILED: {e}"));
            return;
        }
        log("step2: exports resolved");

        // 3) Wait for the IL2CPP domain to exist (domain_get = no alloc, safe).
        ipc::set_status("waiting for IL2CPP runtime…");
        let mut rwait: u64 = 0;
        while il2cpp::domain().is_null() {
            std::thread::sleep(Duration::from_millis(250));
            rwait += 250;
            if rwait > 180_000 {
                log("step3: TIMEOUT domain");
                return;
            }
        }
        log(&format!("step3: domain present ({rwait}ms)"));

        // 3b) Let the runtime/GC fully settle before we touch it. With the proxy
        //     loader we reach this point during early init; attaching into a
        //     freshly-created domain races the GC. A short settle window makes
        //     the proxy path behave like the (working) late-injection path.
        std::thread::sleep(Duration::from_secs(5));
        log("step3b: settle done");

        // 4) Attach this thread, then confirm classes resolve.
        let heaven_thread = il2cpp::attach_current_thread();
        log("step4: thread attached");
        let mut cwait: u64 = 0;
        while il2cpp::class("Gallop.ButtonCommon").is_null() {
            std::thread::sleep(Duration::from_millis(250));
            cwait += 250;
            if cwait > 60_000 {
                log("step4: TIMEOUT classes");
                return;
            }
        }
        log(&format!("step5: classes resolvable — runtime ready ({}ms total)", waited + rwait + cwait));

        // Arm the crash detector before installing our hooks, so a fault in any of them is
        // logged with a breadcrumb to heaven-crash.log.
        crate::crashlog::install();

        // Active-scene probe (gates the intro player on the title screen) + intro-song
        // audio worker + BGM mute API. Private (`banner`) build only; the video player's
        // device capture runs separately and early (from new_with_engine).
        #[cfg(feature = "banner")]
        {
            crate::startup_probe::spawn();
            crate::audio::spawn();
            crate::bgm::init();
        }

        // External mod DLLs in heaven_plugins/ are LOADED much EARLIER by the version.dll
        // proxy (in its DllMain, before the overlay and IL2CPP) so self-contained proxy-style
        // mods install their hooks in time. See proxy/src/lib.rs::load_plugins_early.
        //
        // SDK-style plugins (built against an external mod-host SDK: they export `hachimi_init`
        // and hook through a host-supplied vtable) can't act on a bare LoadLibrary — they need
        // the host to call their init AFTER il2cpp is up. We do exactly that here, now that the
        // runtime is ready, handing them a Heaven-backed compatible vtable. Self-contained mods
        // (no such export) are left untouched (already started by the early loader).
        log(&format!("plugins(sdk): {}", crate::hachimi_compat::init_plugins()));

        // 3) Install modules. Each is independent; one failing never blocks the
        //    others (keeps the proven core alive even if an experimental part
        //    can't resolve on a future game patch).
        let (tr_ok, ev_ok, snotes) = skip::install();
        log(&format!("superskip: training={tr_ok} events={ev_ok} [{}]", snotes.trim_end()));
        crate::diag::record_install("superskip", &format!("training={tr_ok} events={ev_ok} [{}]", snotes.trim_end()));
        match skip::install_race_result() {
            Ok(note) => {
                log(&format!("race-result (off by default): armed [{}]", note.trim_end()));
                crate::diag::record_install("race-result", &format!("armed [{}]", note.trim_end()));
            }
            Err(e) => {
                log(&format!("race-result: not armed ({e})"));
                crate::diag::record_install("race-result", &format!("NOT armed ({e})"));
            }
        }
        {
            let fps_status = fps::install();
            log(&format!("fps control: {fps_status}"));
            crate::diag::record_install("fps control", &fps_status);
        }
        match crate::ui_tempo::install() {
            Ok(detail) => { log(&format!("ui tempo: {detail}")); crate::diag::record_install("ui tempo", detail); }
            Err(e) => { log(&format!("ui tempo: deferred ({e})")); crate::diag::record_install("ui tempo", &format!("deferred ({e})")); }
        }
        crate::crashlog::crumb(4);
        match crate::cyspring::install() {
            Ok(()) => { log("cyspring uncap: OK"); crate::diag::record_install("cyspring uncap", "OK"); }
            Err(e) => { log(&format!("cyspring uncap: deferred ({e})")); crate::diag::record_install("cyspring uncap", &format!("deferred ({e})")); }
        }
        crate::crashlog::crumb(1);
        match crate::graphics::install() {
            Ok(()) => { log("graphics tweaks: OK"); crate::diag::record_install("graphics tweaks", "OK"); }
            Err(e) => { log(&format!("graphics tweaks: deferred ({e})")); crate::diag::record_install("graphics tweaks", &format!("deferred ({e})")); }
        }
        crate::crashlog::crumb(2);
        match crate::display::install() {
            Ok(()) => { log("display tweaks: OK"); crate::diag::record_install("display tweaks", "OK"); }
            Err(e) => { log(&format!("display tweaks: deferred ({e})")); crate::diag::record_install("display tweaks", &format!("deferred ({e})")); }
        }
        crate::crashlog::crumb(3);
        crate::display::install_window();
        crate::crashlog::crumb(0);
        #[cfg(feature = "raceread")]
        {
            let r = race::install();
            log(&format!("race reader: {r}"));
            crate::diag::record_install("race reader", &r);
        }

        #[cfg(feature = "freecam")]
        {
            let r = crate::freecam::install();
            log(&format!("freecam: {r}"));
            crate::diag::record_install("freecam", &r);
        }

        // Response hook (full build): parses the msgpack race response to
        // identify the player's horse → needed by the Top-1 race-result skip gate.

        // Public build: the player-horse identity parse that the full build would otherwise
        // provide (so the race-result skip's "only when you WON" gate works). Only
        // when the full build is absent — with the full build present its hook already does this.
        #[cfg(all(feature = "racenet", not(feature = "oracle")))]
        {
            crate::race_net::install();
            log("race_net: armed (player-id only)");
            crate::diag::record_install("race_net", "armed (player-id only)");
        }

        // Companion-overlay bridge: forward requests (CompressRequest hook) + responses (fed from the
        // DecompressResponse hook in the full build/race_net) to companion overlays over UDP 17229,
        // so tools that used a separate capture plugin work with Heaven directly.
        {
            crate::uma_bridge::install();
            log("uma_bridge: request capture armed");
            crate::diag::record_install("uma_bridge", "armed");
        }

        // HorseTheTrails — native Team Trials capture (hooks TeamStadiumResult).
        // Runs while this boot thread is still IL2CPP-attached (scan needs it).
        {
            let r = htt::install();
            log(&format!("HorseTheTrails: {r}"));
            crate::diag::record_install("HorseTheTrails", &r);
        }

        // Veterans export (Hakuraku-format trained-uma dump). Needs the same attached
        // boot thread (it scans the assembly for the TrainedChara[] method).
        {
            let r = crate::umas::install();
            log(&format!("veterans export: {r}"));
            crate::diag::record_install("veterans export", &r);
        }

        // Team Trials deck-profile capture (hooks TeamStadiumDeckBuilder.Setup/Release to track the
        // live team-edit screen, so the padder can drive its grid). Non-fatal if it misses.
        {
            let r = crate::padder::install();
            log(&format!("tt padder: {r}"));
            crate::diag::record_install("tt padder", &r);
        }
        {
            let r = crate::hunter::install();
            log(&format!("tt hunter: {r}"));
            crate::diag::record_install("tt hunter", &r);
        }
        // Legacy Select affinity numbers (on-screen exact pair total + per-parent values).
        // Read-only; its per-frame tick rides hunter's TweenManager.Update pump.
        {
            let r = crate::affinity::install();
            log(&format!("affinity: {r}"));
            crate::diag::record_install("affinity", &r);
        }

        // Soft reset (GameSystem.SoftwareReset) — reload to title without killing the process.
        // Resolved by name; fired from the overlay via reset::request(), executed on the main-thread
        // TweenManager.Update pump (reset::poll()).
        {
            let r = crate::reset::install();
            log(&format!("soft reset: {r}"));
            crate::diag::record_install("soft reset", &r);
        }

        // Heaven+Hachimi variant: report which hooks Heaven owns vs ceded to a co-resident mod.
        #[cfg(feature = "hachimi")]
        log(&format!("hook arbiter: {}", crate::arbiter::report()));

        // Apply persisted toggle state (SuperSkip / Race-result / FPS / TT).
        settings::apply_on_boot();
        log("settings: applied persisted state");

        // Install is done. Hooks now run on the GAME's (already-attached) threads,
        // so this boot thread no longer needs to be attached. DETACH it cleanly
        // and let it exit — leaving it attached + alive made the shutdown GC
        // "collect from an unknown thread" when the game closes. Detaching
        // unregisters it from the GC so teardown is clean.
        il2cpp::detach_thread(heaven_thread);
        ipc::set_status("Heaven native engine ready");
        log("==== ready (boot thread detached) ====");

        // Auto-check for a new release in the background (spawns its own thread, no IL2CPP).
        // Honours the "don't ask again for this version" skip; the overlay shows the dialog.
        crate::selfupdate::check(false);
    });
}

