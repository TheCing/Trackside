//! Hook registry (retour-based) + the interceptor/instance vtable callbacks.
//!
//! The plugin asks the host to install/remove hooks through these entry points;
//! Heaven's own retour engine backs them and remembers each hook so the plugin
//! can look up its trampoline or unhook later.

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};

use retour::RawDetour;
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE};

use super::{HOST_TOKEN, INTERCEPTOR_TOKEN};

pub(crate) struct HookEntry {
    hook_addr: usize,
    orig_addr: usize,
    tramp: usize,
    detour: RawDetour,
}
unsafe impl Send for HookEntry {}

static HOOKS: OnceLock<Mutex<Vec<HookEntry>>> = OnceLock::new();
fn hooks() -> &'static Mutex<Vec<HookEntry>> {
    HOOKS.get_or_init(|| Mutex::new(Vec::new()))
}

// ── core: instance / interceptor handles (opaque, plugin only passes them back) ─
pub(crate) unsafe extern "C" fn vt_hachimi_instance() -> *const c_void {
    &HOST_TOKEN as *const u8 as *const c_void
}
pub(crate) unsafe extern "C" fn vt_hachimi_get_interceptor(_this: *const c_void) -> *const c_void {
    &INTERCEPTOR_TOKEN as *const u8 as *const c_void
}

// ── core: hook interceptor (retour) ─────────────────────────────────────────
pub(crate) unsafe extern "C" fn vt_interceptor_hook(_this: *const c_void, orig: *mut c_void, hook: *mut c_void) -> *mut c_void {
    if orig.is_null() || hook.is_null() {
        return std::ptr::null_mut();
    }
    if let Ok(g) = hooks().lock() {
        if let Some(e) = g.iter().find(|e| e.hook_addr == hook as usize) {
            return e.tramp as *mut c_void;
        }
    }
    match RawDetour::new(orig as *const (), hook as *const ()) {
        Ok(d) => {
            if d.enable().is_err() {
                return std::ptr::null_mut();
            }
            let tramp = d.trampoline() as *const () as usize;
            if let Ok(mut g) = hooks().lock() {
                g.push(HookEntry { hook_addr: hook as usize, orig_addr: orig as usize, tramp, detour: d });
            }
            tramp as *mut c_void
        }
        Err(_) => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_interceptor_hook_vtable(_this: *const c_void, vtable: *mut *mut c_void, index: usize, hook: *mut c_void) -> *mut c_void {
    if vtable.is_null() || hook.is_null() {
        return std::ptr::null_mut();
    }
    let slot = vtable.add(index);
    let orig = *slot;
    let mut old = 0u32;
    if VirtualProtect(slot as *mut c_void, std::mem::size_of::<*mut c_void>(), PAGE_EXECUTE_READWRITE, &mut old) == 0 {
        return std::ptr::null_mut();
    }
    *slot = hook;
    let mut tmp = 0u32;
    VirtualProtect(slot as *mut c_void, std::mem::size_of::<*mut c_void>(), old, &mut tmp);
    orig
}
pub(crate) unsafe extern "C" fn vt_interceptor_get_trampoline_addr(_this: *const c_void, hook: *mut c_void) -> *mut c_void {
    if let Ok(g) = hooks().lock() {
        if let Some(e) = g.iter().find(|e| e.hook_addr == hook as usize) {
            return e.tramp as *mut c_void;
        }
    }
    std::ptr::null_mut()
}
pub(crate) unsafe extern "C" fn vt_interceptor_unhook(_this: *const c_void, hook: *mut c_void) -> *mut c_void {
    if let Ok(mut g) = hooks().lock() {
        if let Some(pos) = g.iter().position(|e| e.hook_addr == hook as usize) {
            let e = g.remove(pos);
            let _ = e.detour.disable();
            return e.orig_addr as *mut c_void;
        }
    }
    std::ptr::null_mut()
}
