//! Heaven — active Unity scene probe.
//!
//! Publishes whether the active scene is the title screen (`is_title`), which the overlay
//! uses to gate the intro player (stop it on Title → Home) and to mute the original title
//! BGM. Reads `UnityEngine.SceneManagement.SceneManager.GetActiveScene().name` directly via
//! the compiled method pointers.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;


use crate::il2cpp;

/// True while the active Unity scene is the title screen ("Title") — where the original
/// intro movie plays. The overlay reads this to auto-start the native intro player.
static IS_TITLE: AtomicBool = AtomicBool::new(false);

pub fn is_title() -> bool {
    IS_TITLE.load(Ordering::Relaxed)
}

fn log(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true)
        .open(crate::paths::log_file("heaven-native.log")) { let _ = writeln!(f, "{msg}"); }
}

pub fn spawn() {
    std::thread::spawn(|| {
        // NOTE: we do NOT hold an IL2CPP attachment across the wait/poll. A thread that
        // stays attached + alive deadlocks the game's shutdown GC (see boot.rs). Metadata
        // lookups below don't need attachment; we attach ONLY for the microsecond each poll
        // spends actually invoking the managed scene methods, then detach before sleeping —
        // so at any moment the user quits, this thread is detached and the GC won't wait on it.

        // Wait for the scene API to resolve.
        let sm = {
            let mut k = il2cpp::class("UnityEngine.SceneManagement.SceneManager");
            let mut waited = 0u64;
            while k.is_null() && waited < 120_000 {
                std::thread::sleep(Duration::from_millis(250));
                waited += 250;
                k = il2cpp::class("UnityEngine.SceneManagement.SceneManager");
            }
            k
        };
        let sc = il2cpp::class("UnityEngine.SceneManagement.Scene");
        if sm.is_null() || sc.is_null() {
            log("[scene] SceneManager/Scene class not found — probe off");
            return;
        }
        let gas = il2cpp::method(sm, "GetActiveScene", 0); // static -> Scene(i32 handle)
        let gname = il2cpp::method(sc, "get_name", 0); // instance(Scene) -> Il2CppString
        let gas_p = il2cpp::method_pointer(gas);
        let gname_p = il2cpp::method_pointer(gname);
        if gas_p.is_null() || gname_p.is_null() {
            log("[scene] GetActiveScene/get_name not resolvable — probe off");
            return;
        }
        // Static 0-arg: fn(MethodInfo*) -> i32 (Scene = 1-int struct, returned in EAX).
        let f_gas: extern "C" fn(*const c_void) -> i32 = unsafe { std::mem::transmute(gas_p) };
        // Instance get_name on a value type: `this` is a POINTER to the Scene (the int
        // handle), then the trailing MethodInfo*. Returns Il2CppString*.
        let f_gname: extern "C" fn(*const i32, *const c_void) -> il2cpp::Object =
            unsafe { std::mem::transmute(gname_p) };

        log("[scene] probe armed");
        let mut last = String::new();
        let mut ticks = 0u64;
        let mut seen_title = false;
        loop {
            // Attach only for the managed calls, detach immediately after.
            let th = il2cpp::attach_current_thread();
            let handle = f_gas(gas as *const c_void);
            let sptr = f_gname(&handle as *const i32, gname as *const c_void);
            let name = if sptr.is_null() { String::new() } else { il2cpp::read_string(sptr) };
            il2cpp::detach_thread(th);
            let is_title = name == "Title";
            IS_TITLE.store(is_title, Ordering::Relaxed);
            if name != last {
                log(&format!("[scene] -> '{name}' (handle {handle})"));
                last = name.clone();
            }
            if is_title {
                seen_title = true;
            }
            // CRITICAL: an IL2CPP-attached thread that stays alive forever deadlocks the
            // game's shutdown GC ("collect from an unknown thread") — the documented reason
            // boot.rs detaches its own thread. The scene probe is only needed for the title
            // intro lifecycle, so once we've entered the title and then left it (Home/OutGame),
            // the intro is over → detach + exit so the thread is gone long before the user
            // quits. Fallback: bail after ~6 min even if the transition is never observed.
            ticks = ticks.wrapping_add(1);
            let left_title = seen_title && !is_title && !name.is_empty();
            if left_title || ticks > 3000 {
                IS_TITLE.store(false, Ordering::Relaxed);
                log(&format!(
                    "[scene] probe done ({}) — thread exiting",
                    if left_title { "left title" } else { "timeout" }
                ));
                return; // already detached each iteration
            }
            std::thread::sleep(Duration::from_millis(120));
        }
    });
}
