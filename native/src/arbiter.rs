//! Feature arbiter — visibility + auto-disable over hook coexistence.
//!
//! `il2cpp::hook_method` REFUSES to double-hook a method another mod detoured first
//! ("already detoured (skipped)") → Trackside yields, no crash, but the feature is lost.
//! The arbiter records every hook outcome keyed by `Class.method`, so Heaven can:
//!   - SHOW which features it ceded to a co-resident mod (boot log + overlay), and
//!   - AUTO-DISABLE its own duplicate tweaks (`is_ceded`) so the menu is honest and
//!     two mods never fight over / double-apply the same game state (e.g. UI speed).

use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Heaven,
    External,
    Chained,
    Missing,
}

// (key = "Class.method", owner). Mutex is const-constructible → no lazy init needed.
static RECORDS: Mutex<Vec<(String, Owner)>> = Mutex::new(Vec::new());

/// Record a hook outcome under a unique `Class.method` key (last write wins).
pub fn record(key: &str, owner: Owner) {
    if let Ok(mut v) = RECORDS.lock() {
        if let Some(e) = v.iter_mut().find(|(k, _)| k == key) {
            e.1 = owner;
        } else {
            v.push((key.to_string(), owner));
        }
    }
}

/// Classify a `hook_method` result and record it under `key`.
pub fn note(key: &str, res: &Result<(), String>) {
    let owner = match res {
        Ok(()) => Owner::Heaven,
        Err(e) if e.contains("already detoured") => Owner::External,
        Err(_) => Owner::Missing,
    };
    record(key, owner);
}

/// True if a method matching `suffix` was ceded to another mod (it's hooked by them,
/// not us). `suffix` is matched against the end of the key so callers can pass either
/// the full "Class.method" or just the distinctive part.
pub fn is_ceded(suffix: &str) -> bool {
    RECORDS
        .lock()
        .map(|v| {
            v.iter()
                .any(|(k, o)| *o == Owner::External && k.ends_with(suffix))
        })
        .unwrap_or(false)
}

pub fn is_chained(suffix: &str) -> bool {
    RECORDS
        .lock()
        .map(|v| {
            v.iter()
                .any(|(k, o)| *o == Owner::Chained && k.ends_with(suffix))
        })
        .unwrap_or(false)
}

/// One-line summary for the boot log / overlay.
pub fn report() -> String {
    let v = match RECORDS.lock() {
        Ok(v) => v,
        Err(e) => e.into_inner(),
    };
    let owned = v.iter().filter(|(_, o)| *o == Owner::Heaven).count();
    let ext: Vec<&str> = v
        .iter()
        .filter(|(_, o)| *o == Owner::External)
        .map(|(k, _)| k.as_str())
        .collect();
    if ext.is_empty() {
        format!("{owned} hook(s) owned by Trackside, none ceded")
    } else {
        format!(
            "{owned} owned by Trackside, {} ceded to a co-resident mod: [{}]",
            ext.len(),
            ext.join(", ")
        )
    }
}

// ── cohabitation ordering ───────────────────────────────────────────────────────
//
// The cede logic above only works if the OTHER mod hooked FIRST — `is_detoured` can only see a
// detour that already exists. Hachimi does not cooperate with that assumption:
//
//   09:46:00.51  Hachimi: "GameAssembly finished loading" — starts installing ~250 detours,
//                serially, one every ~250 ms (it stalls the main thread doing it; our own
//                watchdog logged RENDER + main stalled during the storm)
//   09:46:03.94  us: every install done, "10 hook(s) owned by Trackside, none ceded"
//   09:46:12.89  Hachimi: "GraphicSettings: new_hook!: ApplyGraphicsQuality"   ← ours, overwritten
//   09:46:43.66  Hachimi: "Hooking finished"
//
// We finish in a 4-second burst and WIN the race, so nothing looks detoured, we take every hook —
// and then Hachimi writes its own jmp over prologues we had already copied into trampolines. Our
// trampoline then returns into the middle of ITS jmp: crash log showed 0xC0000005, *execute* at
// 0xffff142a (an address in no module), breadcrumb `graphics::on_apply_quality`. Its LAST hook is
// DOTween's TweenManager.Update — our main-thread pump — so the blast radius is everything.
//
// Fix: in the Hachimi build, don't install until Hachimi has finished. Then `is_detoured` sees the
// real picture and the cede logic does what it was written to do. `ui_tempo` still CHAINS on top
// for TweenManager.Update rather than ceding, which is correct and now also safe: hooking last is
// the safe direction — being hooked over is what corrupts a trampoline.

/// Is a Hachimi proxy loaded? Both of its Windows proxy names also exist as legitimate system or
/// game DLLs, so the name alone proves nothing — what identifies Hachimi is a module with that name
/// loaded from the GAME folder (the real winhttp.dll lives in System32, the game's own
/// cri_mana_vpx.dll in UmamusumePrettyDerby_Data\Plugins\x86_64).
///
/// Known gap: the `unityplayer.dll` proxy install is undetectable this way — that name legitimately
/// sits in the game folder. Such an install falls through to the old racy behaviour rather than
/// waiting 120 s on every launch that has no Hachimi at all.
#[cfg(feature = "hachimi")]
fn hachimi_loaded() -> Option<String> {
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};

    fn path_of(name: Option<&str>) -> Option<String> {
        unsafe {
            let handle = match name {
                Some(n) => {
                    let w: Vec<u16> = n.encode_utf16().chain(std::iter::once(0)).collect();
                    let h = GetModuleHandleW(w.as_ptr());
                    if h.is_null() {
                        return None;
                    }
                    h
                }
                None => std::ptr::null_mut(),
            };
            let mut buf = [0u16; MAX_PATH as usize];
            let n = GetModuleFileNameW(handle, buf.as_mut_ptr(), buf.len() as u32);
            if n == 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..n as usize]))
        }
    }
    fn dir_of(p: &str) -> String {
        match p.rfind('\\') {
            Some(i) => p[..i].to_ascii_lowercase(),
            None => p.to_ascii_lowercase(),
        }
    }

    let game_dir = dir_of(&path_of(None)?);
    for name in ["winhttp.dll", "cri_mana_vpx.dll"] {
        if let Some(p) = path_of(Some(name)) {
            if dir_of(&p) == game_dir {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Block until Hachimi has finished installing its detours (or we give up waiting).
///
/// The completion signal is its LAST hook, `DG.Tweening.Core.TweenManager.Update` — verified as the
/// final `new_hook!` line, immediately before "Hooking finished", in two independent runs. That is
/// an ordering assumption about someone else's code, so it is capped and LOUDLY logged either way:
/// if upstream reorders its table we fall back to the timeout, which is no worse than the racy
/// behaviour we have today, and the log says which path was taken.
#[cfg(feature = "hachimi")]
pub fn wait_for_cohabitant() {
    use std::time::{Duration, Instant};

    let Some(via) = hachimi_loaded() else {
        crate::tools::log("cohabitant: no Hachimi proxy loaded — installing immediately");
        return;
    };
    let target = unsafe {
        let k = crate::il2cpp::class("DG.Tweening.Core.TweenManager");
        if k.is_null() {
            crate::tools::log("cohabitant: TweenManager unresolved — cannot sequence, installing now");
            return;
        }
        let m = crate::il2cpp::method(k, "Update", 3);
        if m.is_null() {
            crate::tools::log("cohabitant: TweenManager.Update unresolved — installing now");
            return;
        }
        crate::il2cpp::method_pointer(m)
    };
    if target.is_null() {
        return;
    }

    crate::tools::log(&format!(
        "cohabitant: Hachimi loaded (via {via}) — waiting for it to finish hooking before we install"
    ));
    let start = Instant::now();
    const CAP: Duration = Duration::from_secs(120);
    loop {
        if unsafe { crate::il2cpp::is_detoured(target) } {
            // Its last hook is in. Give the tail of the storm a moment to drain before we start
            // copying prologues out from under it.
            std::thread::sleep(Duration::from_secs(2));
            crate::tools::log(&format!(
                "cohabitant: Hachimi finished hooking after {:.1}s — installing ours on top",
                start.elapsed().as_secs_f32()
            ));
            return;
        }
        if start.elapsed() >= CAP {
            crate::tools::warn(
                "cohabitant: Hachimi never detoured TweenManager.Update within 120s — installing \
                 anyway. If it hooks our methods AFTER this, expect trampoline corruption; check \
                 whether its hook order changed.",
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
