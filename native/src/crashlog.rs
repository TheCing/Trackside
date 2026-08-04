//! Heaven — crash detector. Installs a last-chance unhandled-exception filter that, when the
//! game crashes, writes `trackside-crash.log` with the exception code, the faulting address, WHICH
//! module that address is in (ours = `trackside.dll`, the game = `GameAssembly.dll`, …) and
//! the last "breadcrumb" — the hook that was executing. That pinpoints which feature crashed.
//!
//! The breadcrumb is a single cheap atomic the risky hooks stamp on entry (no I/O on the hot
//! path), so the only cost during normal play is one relaxed store per hooked call.

#![allow(dead_code)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

// Minimal SEH structs (manual FFI — avoids depending on windows-sys's Diagnostics module layout).
#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut c_void,
    num_params: u32,
    info: [usize; 15], // repr(C) pads `num_params` so this lands at offset 32, matching Win32
}
#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut c_void,
}
type TopFilter = Option<unsafe extern "system" fn(*const ExceptionPointers) -> i32>;
#[link(name = "kernel32")]
extern "system" {
    fn SetUnhandledExceptionFilter(filter: TopFilter) -> TopFilter;
}

static BREADCRUMB: AtomicU32 = AtomicU32::new(0);

/// Stamp the current execution point (called on entry to risky hooks). Cheap relaxed store.
#[inline]
pub fn crumb(code: u32) {
    BREADCRUMB.store(code, Ordering::Relaxed);
}

// ── granular string breadcrumb ──────────────────────────────────────────────────
// A `&'static str` (which lives forever) stored as (ptr,len) so hot hooks can stamp a precise step
// with zero allocation. Instrumented hooks set it on entry to each risky step and to "idle:<hook>" on
// exit — so the crash log shows the EXACT step (and whether we crashed INSIDE a hook or after it).
static CRUMB_PTR: AtomicUsize = AtomicUsize::new(0);
static CRUMB_LEN: AtomicUsize = AtomicUsize::new(0);

#[inline]
pub fn step(s: &'static str) {
    // Whoever bumps the step counter IS the main thread by definition - record it so the hang
    // watchdog can suspend and sample the right thread rather than guessing.
    MAIN_TID.store(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }, Ordering::Relaxed);
    CRUMB_PTR.store(s.as_ptr() as usize, Ordering::Relaxed);
    CRUMB_LEN.store(s.len(), Ordering::Relaxed);
    STEP_SEQ.fetch_add(1, Ordering::Relaxed);
}

/// Monotonic count of step() calls — the hang watchdog's heartbeat. The tween pump stamps steps
/// every frame, so a stalled counter means the game's main thread is stuck.
static STEP_SEQ: AtomicUsize = AtomicUsize::new(0);

pub fn step_seq() -> usize {
    STEP_SEQ.load(Ordering::Relaxed)
}

/// The most recent step string (best-effort; for the hang watchdog's report).
pub fn last_step() -> String {
    let (p, l) = (CRUMB_PTR.load(Ordering::Relaxed), CRUMB_LEN.load(Ordering::Relaxed));
    if p == 0 || l == 0 || l > 128 {
        return "?".into();
    }
    unsafe {
        std::str::from_utf8(std::slice::from_raw_parts(p as *const u8, l))
            .unwrap_or("?")
            .to_string()
    }
}

/// Render-thread heartbeat, bumped once per overlay frame.
///
/// The main-thread watchdog stayed silent through a hardlock, which was itself the clue: the
/// game's logic thread was fine and the RENDER thread was stuck (the overlay draws there). A
/// freeze with a live main thread is invisible to a main-thread-only watchdog, so both are
/// tracked and reported separately.
static FRAME_SEQ: AtomicUsize = AtomicUsize::new(0);

static UI_PTR: AtomicUsize = AtomicUsize::new(0);
static UI_LEN: AtomicUsize = AtomicUsize::new(0);

/// Breadcrumb for the RENDER thread (overlay drawing). Separate from the main-thread crumb so a
/// frozen screen can be located precisely, every frame, forever — the previous panel trace only
/// covered 3 frames and so went dark exactly when the hang happened.
#[inline]
pub fn ui_step(s: &'static str) {
    UI_PTR.store(s.as_ptr() as usize, Ordering::Relaxed);
    UI_LEN.store(s.len(), Ordering::Relaxed);
}

pub fn last_ui_step() -> String {
    let (p, l) = (UI_PTR.load(Ordering::Relaxed), UI_LEN.load(Ordering::Relaxed));
    if p == 0 || l == 0 || l > 128 {
        return "?".into();
    }
    unsafe {
        std::str::from_utf8(std::slice::from_raw_parts(p as *const u8, l))
            .unwrap_or("?")
            .to_string()
    }
}

#[inline]
pub fn frame_tick() {
    // Same for the overlay/render thread.
    RENDER_TID.store(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }, Ordering::Relaxed);
    FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
}

/// Watchdog for HANGS — the gap in our diagnostics. Crashes dump the breadcrumb, but a frozen
/// main thread dumps nothing: three hardlocks in a row gave zero data. This thread watches the
/// step counter (bumped every frame by the tween pump); if it stops advancing for 8 s while the
/// process lives, it logs the LAST step reached — turning "hardlock, no data" into a named
/// location, exactly what the per-call step markers did for access violations.

// ── stack capture at stall time ──────────────────────────────────────────────
// The breadcrumbs name a LOCATION but not a CAUSE, and across four hangs they named four different
// panels - because they record whatever was drawing, not what is blocked. This walks the actual
// stalled threads instead: suspend, RtlVirtualUnwind the real call chain, resolve each frame to
// module+offset. When BOTH threads are down it sweeps every thread in the process, because the
// main/render pair kept showing two VICTIMS queued on someone else's wait.

/// OS thread ids, recorded by the same markers the watchdog already watches.
pub(crate) static MAIN_TID: AtomicU32 = AtomicU32::new(0);
pub(crate) static RENDER_TID: AtomicU32 = AtomicU32::new(0);

/// Resolve an address to `module+0xoffset`, or a bare hex address if it is not in a known module.
unsafe fn describe_addr(addr: usize) -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    let mut h = std::ptr::null_mut();
    let ok = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        addr as *const u16,
        &mut h,
    );
    if ok == 0 || h.is_null() {
        return format!("{addr:#x}");
    }
    let mut buf = [0u16; 260];
    let n = GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) as usize;
    let full = String::from_utf16_lossy(&buf[..n]);
    let name = full.rsplit(std::path::MAIN_SEPARATOR).next().unwrap_or("?").to_string();
    format!("{name}+{:#x}", addr - h as usize)
}

// x64 unwind, manual FFI in this file's style. Exported by kernel32 (forwarded to ntdll).
extern "system" {
    fn RtlLookupFunctionEntry(
        control_pc: u64,
        image_base: *mut u64,
        history: *mut c_void,
    ) -> *mut c_void;
    fn RtlVirtualUnwind(
        handler_type: u32,
        image_base: u64,
        control_pc: u64,
        function_entry: *mut c_void,
        context: *mut c_void,
        handler_data: *mut *mut c_void,
        establisher_frame: *mut u64,
        context_pointers: *mut c_void,
    ) -> *mut c_void;
}

const MAX_FRAMES: usize = 24;

/// Capture `tid`'s call stack: suspend, grab the context, RESUME, then unwind the copy.
///
/// Two lessons are load-bearing here:
///   * v1 SCANNED the stack for module-shaped values. Scans return stale frames from dead calls -
///     phantom frames once sent a whole hang investigation at an innocent DLL. RtlVirtualUnwind
///     walks the real chain.
///   * NOTHING may allocate or take a lock while the target is suspended - see the resume comment
///     inside. Raw addresses go into a caller-provided array; strings happen after this returns.
unsafe fn walk_thread(tid: u32, frames: &mut [usize; MAX_FRAMES]) -> usize {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        GetThreadContext, CONTEXT, CONTEXT_FULL_AMD64,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION,
        THREAD_SUSPEND_RESUME,
    };
    if tid == 0 {
        return 0;
    }
    let h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, 0, tid);
    if h.is_null() {
        return 0;
    }
    if SuspendThread(h) == u32::MAX {
        CloseHandle(h);
        return 0;
    }
    // CONTEXT must be 16-byte aligned on x64 or GetThreadContext fails silently.
    #[repr(align(16))]
    struct Aligned(CONTEXT);
    let mut a = Aligned(std::mem::zeroed());
    a.0.ContextFlags = CONTEXT_FULL_AMD64;
    let got = GetThreadContext(h, &mut a.0) != 0;

    // RESUME BEFORE UNWINDING. RtlLookupFunctionEntry takes ntdll's dynamic function-table lock,
    // which IL2CPP also takes when it registers JIT unwind data. Unwinding while threads are
    // suspended can therefore block on a lock held BY a thread we just froze - the watchdog would
    // deadlock with its victims still suspended, converting a transient stall into the permanent
    // hang it exists to diagnose. That window is far wider under Wine, where field reports come
    // from. Everything below works on a COPY of the context, so the thread runs free.
    ResumeThread(h);
    CloseHandle(h);
    if !got {
        return 0;
    }

    let ctx = &mut a.0;
    let mut n = 0usize;
    while n < MAX_FRAMES && ctx.Rip > 0x10000 && ctx.Rsp > 0x10000 {
        frames[n] = ctx.Rip as usize;
        n += 1;
        let mut image_base: u64 = 0;
        let fentry = RtlLookupFunctionEntry(ctx.Rip, &mut image_base, std::ptr::null_mut());
        if fentry.is_null() {
            // Leaf function: the return address is at Rsp. The thread is running again, so this
            // read can race - fault-guard it and stop cleanly rather than crash the watchdog.
            match read_usize_guarded(ctx.Rsp as usize) {
                Some(v) => {
                    ctx.Rip = v as u64;
                    ctx.Rsp += 8;
                }
                None => break,
            }
        } else {
            let mut handler_data: *mut c_void = std::ptr::null_mut();
            let mut establisher: u64 = 0;
            RtlVirtualUnwind(
                0,
                image_base,
                ctx.Rip,
                fentry,
                ctx as *mut CONTEXT as *mut c_void,
                &mut handler_data,
                &mut establisher,
                std::ptr::null_mut(),
            );
        }
    }
    n
}

/// Read a usize from a possibly-unmapped address without faulting the process.
///
/// Needed because unwinding now happens with the target thread RUNNING (see walk_thread): its
/// stack can be reused under us, so a leaf-frame read may land on freed or guard memory. A
/// diagnostic must never be the thing that crashes the game.
unsafe fn read_usize_guarded(addr: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        VirtualQuery, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS,
    };
    if addr < 0x10000 || addr % 8 != 0 {
        return None;
    }
    let mut mbi: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
    let n = VirtualQuery(
        addr as *const c_void,
        &mut mbi,
        std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
    );
    if n == 0 || mbi.State != 0x1000 {
        return None; // not MEM_COMMIT
    }
    if mbi.Protect & (PAGE_GUARD | PAGE_NOACCESS) != 0 || mbi.Protect == 0 {
        return None;
    }
    Some(std::ptr::read_unaligned(addr as *const usize))
}


unsafe fn dump_thread(label: &str, tid: u32) {
    let mut frames = [0usize; MAX_FRAMES];
    let n = walk_thread(tid, &mut frames);
    if n == 0 {
        crate::tools::log(&format!("[watchdog] {label} stack (tid {tid}): <capture failed>"));
        return;
    }
    let body: Vec<String> = frames[..n].iter().map(|&a| describe_addr(a)).collect();
    crate::tools::log(&format!("[watchdog] {label} stack (tid {tid}): {}", body.join(" <- ")));
}

/// Dump EVERY thread in the process. The main/render pair kept showing two VICTIMS - render queued
/// on a d3d11 internal wait, main queued behind render - while whichever thread actually holds the
/// resource was never in the picture. The culprit can only hide if we do not look at it.
unsafe fn dump_all_threads() {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
    // ToolHelp is behind a windows-sys feature we don't enable; manual FFI, per this file's style.
    #[repr(C)]
    struct ThreadEntry32 {
        dw_size: u32,
        cnt_usage: u32,
        th32_thread_id: u32,
        th32_owner_process_id: u32,
        tp_base_pri: i32,
        tp_delta_pri: i32,
        dw_flags: u32,
    }
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> *mut c_void;
        fn Thread32First(snap: *mut c_void, entry: *mut ThreadEntry32) -> i32;
        fn Thread32Next(snap: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    }
    const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
    const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
    if snap == INVALID_HANDLE_VALUE {
        crate::tools::log("[watchdog] all-threads: snapshot failed");
        return;
    }
    let (pid, me) = (GetCurrentProcessId(), GetCurrentThreadId());
    let mut te: ThreadEntry32 = std::mem::zeroed();
    te.dw_size = std::mem::size_of::<ThreadEntry32>() as u32;
    let mut shown = 0usize;
    let mut ok = Thread32First(snap, &mut te);
    while ok != 0 && shown < 64 {
        if te.th32_owner_process_id == pid && te.th32_thread_id != me {
            let mut frames = [0usize; MAX_FRAMES];
            let n = walk_thread(te.th32_thread_id, &mut frames);
            if n > 0 {
                let body: Vec<String> = frames[..n].iter().map(|&a| describe_addr(a)).collect();
                let line = body.join(" <- ");
                // Flag the interesting ones so the culprit jumps out of ~40 lines of parked
                // threadpool waits: anything touching the D3D runtime or our own code.
                let mark = if line.contains("d3d11") || line.contains("dxgi") || line.contains("trackside") {
                    " ***"
                } else {
                    ""
                };
                crate::tools::log(&format!("[watchdog] thread {}{mark}: {line}", te.th32_thread_id));
                shown += 1;
            }
        }
        ok = Thread32Next(snap, &mut te);
    }
    CloseHandle(snap);
    crate::tools::log(&format!("[watchdog] all-threads dump complete ({shown} threads)"));
}

pub fn spawn_hang_watchdog() {
    std::thread::spawn(|| {
        let (mut last_main, mut last_frame) = (0usize, 0usize);
        let (mut rep_main, mut rep_frame) = (false, false);
        let mut rep_all = false;
        loop {
            // 1 s poll (~2 s to report). The first version polled every 8 s and reported nothing
            // in practice: someone watching a frozen screen kills the game well inside that
            // window. A hang watchdog has to beat the user's patience, not a network timeout.
            std::thread::sleep(std::time::Duration::from_millis(2500));
            let now_main = step_seq();
            let now_frame = FRAME_SEQ.load(Ordering::Relaxed);

            if now_main == last_main && now_main != 0 {
                if !rep_main {
                    rep_main = true;
                    crate::tools::log(&format!(
                        "[watchdog] MAIN THREAD STALLED — no step for ~5s; last step: '{}' (seq {})",
                        last_step(),
                        now_main
                    ));
                    unsafe { dump_thread("MAIN", MAIN_TID.load(Ordering::Relaxed)) };
                }
            } else {
                rep_main = false;
            }

            if now_frame == last_frame && now_frame != 0 {
                if !rep_frame {
                    rep_frame = true;
                    crate::tools::log(&format!(
                        "[watchdog] RENDER THREAD STALLED — no overlay frame for ~5s (frame {});                          last UI step: '{}'; main thread {}",
                        now_frame,
                        last_ui_step(),
                        if now_main == last_main { "ALSO stalled" } else { "still running" }
                    ));
                    unsafe { dump_thread("RENDER", RENDER_TID.load(Ordering::Relaxed)) };
                }
            } else {
                rep_frame = false;
            }

            // Both down at once means the two dumps above are showing VICTIMS - the holder of
            // whatever they are queued on is some third thread. Sweep them all, once per episode.
            if rep_main && rep_frame && !rep_all {
                rep_all = true;
                unsafe { dump_all_threads() };
            } else if !rep_main && !rep_frame {
                rep_all = false;
            }

            last_main = now_main;
            last_frame = now_frame;
        }
    });
}

fn crumb_name(c: u32) -> &'static str {
    match c {
        0 => "none (crashed outside our hooks)",
        1 => "boot: graphics::install",
        2 => "boot: display::install",
        3 => "boot: display::install_window",
        4 => "boot: cyspring::install",
        11 => "display::on_get_width (Gallop.Screen.get_Width)",
        12 => "display::on_get_height (Gallop.Screen.get_Height)",
        13 => "display::on_set_resolution (UnityEngine.Screen.SetResolution)",
        14 => "display::on_resize_ui (UIManager.ChangeResizeUIForPC)",
        15 => "display::apply_ui_scale (CanvasScaler array)",
        16 => "display::recreate_RT (CreateRenderTextureFromScreen)",
        21 => "graphics::on_apply_quality (ApplyGraphicsQuality)",
        31 => "cyspring::on_init (CySpringController.Init)",
        _ => "?",
    }
}

fn write_crash(msg: &str) {
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("trackside-crash.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// Resolve which loaded module an address belongs to → (file name, offset from module base).
unsafe fn module_for(addr: usize) -> (String, usize) {
    let mut hmod: HMODULE = std::ptr::null_mut();
    let ok = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        addr as *const u16,
        &mut hmod,
    );
    if ok == 0 || hmod.is_null() {
        return ("<unknown>".into(), addr);
    }
    let mut buf = [0u16; 260];
    let n = GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32) as usize;
    let path = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
    let name = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string();
    (name, addr.wrapping_sub(hmod as usize))
}

unsafe extern "system" fn handler(info: *const ExceptionPointers) -> i32 {
    const CONTINUE_SEARCH: i32 = 0; // let Windows / WER crash normally after we log
    if info.is_null() {
        return CONTINUE_SEARCH;
    }
    let rec = (*info).record;
    if rec.is_null() {
        return CONTINUE_SEARCH;
    }
    let code = (*rec).code;
    let addr = (*rec).address as usize;
    let (module, off) = module_for(addr);
    let bc = BREADCRUMB.load(Ordering::Relaxed);
    // granular string step (if any hook stamped one)
    let cp = CRUMB_PTR.load(Ordering::Relaxed);
    let cl = CRUMB_LEN.load(Ordering::Relaxed);
    let step_str: String = if cp != 0 && cl > 0 && cl < 256 {
        std::str::from_utf8(std::slice::from_raw_parts(cp as *const u8, cl))
            .unwrap_or("<bad>")
            .to_string()
    } else {
        "(none)".into()
    };

    let mut extra = String::new();
    // 0xC0000005 = access violation → record read/write/execute + the bad data address.
    if code == 0xC000_0005 && (*rec).num_params >= 2 {
        let kind = match (*rec).info[0] {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "?",
        };
        let at = (*rec).info[1];
        extra = format!("\n  access violation: {kind} at 0x{at:016x}");
    }

    write_crash(&format!(
        "\n=== CRASH ===\n  code   : 0x{code:08x}\n  at     : 0x{addr:016x}  ({module} + 0x{off:x})\n  hook   : [{bc}] {}\n  step   : {step_str}{extra}\n=============",
        crumb_name(bc)
    ));
    CONTINUE_SEARCH
}

/// Arm the crash detector. Re-armed a few times because the game's own crash handler
/// (Unity) installs later and would otherwise replace ours.
pub fn install() {
    unsafe {
        SetUnhandledExceptionFilter(Some(handler));
    }
    // Rust panics NEVER reach the SEH filter under panic=abort (the runtime __fastfails),
    // which is why panic crashes used to leave an empty log. A panic hook still runs first —
    // capture the message + location + last breadcrumb, THEN let the abort proceed.
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".into());
        let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".into()
        };
        let step = {
            let cp = CRUMB_PTR.load(Ordering::Relaxed);
            let cl = CRUMB_LEN.load(Ordering::Relaxed);
            if cp != 0 && cl > 0 && cl < 256 {
                unsafe {
                    std::str::from_utf8(std::slice::from_raw_parts(cp as *const u8, cl))
                        .unwrap_or("<bad>")
                        .to_string()
                }
            } else {
                "(none)".into()
            }
        };
        write_crash(&format!("RUST PANIC at {loc}\n  message: {msg}\n  last step: {step}"));
    }));
    write_crash("--- trackside crash detector armed ---");
    std::thread::spawn(|| {
        for delay in [2u64, 6, 12] {
            std::thread::sleep(std::time::Duration::from_secs(delay));
            unsafe {
                SetUnhandledExceptionFilter(Some(handler));
            }
        }
    });
}
