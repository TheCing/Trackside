//! Mod-host SDK compatibility layer (protocol v3).
//!
//! Some companion plugins are built against an external mod-host SDK rather than
//! being self-contained proxy DLLs. Such a plugin exports `hachimi_init(vtable,
//! version)` and does ALL its work through the host-supplied vtable (a hook
//! interceptor + an il2cpp bridge + a few host services). A plain `LoadLibrary`
//! is not enough: nobody calls its init, so it loads but never hooks anything.
//!
//! This module lets Heaven act as that host. We expose a vtable with the EXACT
//! same C layout and version the SDK expects (v3), backed by Heaven's own retour
//! hook engine and the il2cpp C API, then call each plugin's `hachimi_init`. From
//! the plugin's point of view it is talking to a compatible host, so it installs
//! its hooks and runs — with Heaven only, no external loader required.
//!
//! ABI WARNING: the `Vtable` field order, every signature, and `SDK_VERSION` must
//! match the SDK byte-for-byte. A single mismatched/missing field shifts every
//! later slot and the plugin calls the wrong pointer -> crash. Mirrored from the
//! upstream SDK v3 plugin_api. Unused host services are stubbed but kept IN ORDER.
//!
//! Concern split (behaviour-identical to the former single file):
//!   il2cpp_api  — the `Api` export table resolved from GameAssembly.dll
//!   vtable      — the SDK v3 `Vtable` ABI struct + 66-slot const + size assert
//!   interceptor — retour hook registry + interceptor/instance callbacks
//!   services    — il2cpp bridge, logging, host services, GUI/Android stubs
//!   init        — `init_plugins()` (the `heaven_plugins/` scan + `hachimi_init`)

#![allow(dead_code)]
#![allow(non_snake_case)]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

mod il2cpp_api;
mod init;
mod interceptor;
mod services;
mod vtable;

// Public API the app calls.
pub use init::init_plugins;

// ── shared opaque pointer aliases ────────────────────────────────────────────
// Every concrete il2cpp/host pointer is a machine pointer, so c_void pointers are
// layout-identical to the SDK's concrete types.
pub(crate) type Class = *mut c_void;
pub(crate) type Method = *const c_void;
pub(crate) type Field = *mut c_void;
pub(crate) type Object = *mut c_void;
pub(crate) type Image = *const c_void;
pub(crate) type ThreadPtr = *mut c_void;
pub(crate) type ArrayPtr = *mut c_void;
pub(crate) type StringPtr = *mut c_void;
pub(crate) type TypeEnum = i32;
pub(crate) type ArraySize = usize;

pub(crate) const SDK_VERSION: i32 = 3;

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitResult {
    Error = 0,
    Ok = 1,
}

pub(crate) type HachimiInitFn =
    unsafe extern "C" fn(vtable: *const vtable::Vtable, version: i32) -> InitResult;

// ── shared module helpers ────────────────────────────────────────────────────
pub(crate) fn game_module() -> HMODULE {
    let w: Vec<u16> = "GameAssembly.dll\0".encode_utf16().collect();
    unsafe { GetModuleHandleW(w.as_ptr()) }
}

pub(crate) unsafe fn sym<T>(m: HMODULE, name: &[u8]) -> Option<T> {
    GetProcAddress(m, name.as_ptr()).map(|p| std::mem::transmute_copy::<_, T>(&p))
}

macro_rules! need {
    ($m:expr, $s:literal) => {
        match unsafe { $crate::hachimi_compat::sym($m, concat!($s, "\0").as_bytes()) } {
            Some(f) => f,
            None => return None,
        }
    };
}
pub(crate) use need;

// ── plugin log ───────────────────────────────────────────────────────────────
pub(crate) fn plog(msg: &str) {
    crate::tools::log_to("heaven-plugins.log", msg);
}

// Opaque host/interceptor tokens (a plugin only passes them back to us).
pub(crate) static HOST_TOKEN: u8 = 0;
pub(crate) static INTERCEPTOR_TOKEN: u8 = 0;
