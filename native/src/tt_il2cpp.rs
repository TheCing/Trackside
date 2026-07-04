//! Heaven — shared IL2CPP helpers for the Team Trials features (opponent `hunter` +
//! deck `padder`). Trivial field readers plus the `WorkDataManager → TeamStadiumData`
//! accessors that both modules used to each carry a private copy of.

use core::ffi::c_void;

use crate::il2cpp;

/// Read a pointer field at `base + off`.
#[inline]
pub unsafe fn rd_ptr(base: *mut c_void, off: usize) -> *mut c_void {
    if base.is_null() {
        return std::ptr::null_mut();
    }
    *((base as usize + off) as *const *mut c_void)
}

/// Read an i32 field at `base + off`.
#[inline]
pub unsafe fn rd_i32(base: *mut c_void, off: usize) -> i32 {
    if base.is_null() {
        return 0;
    }
    *((base as usize + off) as *const i32)
}

/// Decode a CodeStage ObscuredInt at `base + off` (currentCryptoKey@0, hiddenValue@4):
/// plain = hiddenValue ^ currentCryptoKey.
#[inline]
pub unsafe fn rd_obscured_i32(base: *mut c_void, off: usize) -> i32 {
    if base.is_null() {
        return 0;
    }
    let key = *((base as usize + off) as *const i32);
    let hidden = *((base as usize + off + 4) as *const i32);
    hidden ^ key
}

/// Call a 0-arg instance getter returning an object pointer. Null-safe on every step.
pub unsafe fn call_obj_getter(this: *mut c_void, klass: &str, method: &str) -> *mut c_void {
    if this.is_null() {
        return std::ptr::null_mut();
    }
    let k = il2cpp::class(klass);
    if k.is_null() {
        return std::ptr::null_mut();
    }
    let m = il2cpp::method(k, method, 0);
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let p = il2cpp::method_pointer(m);
    if p.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(*mut c_void, *const c_void) -> *mut c_void = std::mem::transmute(p);
    f(this, m as *const c_void)
}

/// `WorkDataManager.Instance` (static get_Instance). Null if not available.
pub unsafe fn work_data_manager() -> *mut c_void {
    let wdm_class = il2cpp::class("Gallop.WorkDataManager");
    if wdm_class.is_null() {
        return std::ptr::null_mut();
    }
    let gi = il2cpp::method(wdm_class, "get_Instance", 0);
    let gip = il2cpp::method_pointer(gi);
    if gip.is_null() {
        return std::ptr::null_mut();
    }
    let f: extern "C" fn(*const c_void) -> *mut c_void = std::mem::transmute(gip);
    f(gi as *const c_void)
}

/// `WorkDataManager.Instance → get_TeamStadiumData()`. Null if Team Trials not loaded.
pub unsafe fn team_stadium_data() -> *mut c_void {
    let wdm = work_data_manager();
    if wdm.is_null() {
        return std::ptr::null_mut();
    }
    call_obj_getter(wdm, "Gallop.WorkDataManager", "get_TeamStadiumData")
}
