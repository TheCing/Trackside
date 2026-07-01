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

#![allow(dead_code)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_void, CStr, CString, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use retour::RawDetour;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE};

// Opaque pointer aliases — every concrete il2cpp/host pointer is a machine
// pointer, so c_void pointers are layout-identical to the SDK's concrete types.
type Class = *mut c_void;
type Method = *const c_void;
type Field = *mut c_void;
type Object = *mut c_void;
type Image = *const c_void;
type ThreadPtr = *mut c_void;
type ArrayPtr = *mut c_void;
type StringPtr = *mut c_void;
type TypeEnum = i32;
type ArraySize = usize;

const SDK_VERSION: i32 = 3;

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum InitResult {
    Error = 0,
    Ok = 1,
}

type HachimiInitFn = unsafe extern "C" fn(vtable: *const Vtable, version: i32) -> InitResult;

// ════════════════════════════════════════════════════════════════════════════
// il2cpp C API resolved from GameAssembly.dll. Resolved once, lazily.
// ════════════════════════════════════════════════════════════════════════════
struct Api {
    class_from_name: unsafe extern "C" fn(Image, *const c_char, *const c_char) -> Class,
    class_get_method_from_name: unsafe extern "C" fn(Class, *const c_char, i32) -> Method,
    class_get_methods: unsafe extern "C" fn(Class, *mut *mut c_void) -> Method,
    class_get_field_from_name: unsafe extern "C" fn(Class, *const c_char) -> Field,
    class_get_nested_types: unsafe extern "C" fn(Class, *mut *mut c_void) -> Class,
    class_get_name: unsafe extern "C" fn(Class) -> *const c_char,
    field_get_value: unsafe extern "C" fn(Object, Field, *mut c_void),
    field_set_value: unsafe extern "C" fn(Object, Field, *mut c_void),
    field_static_get_value: unsafe extern "C" fn(Field, *mut c_void),
    field_static_set_value: unsafe extern "C" fn(Field, *mut c_void),
    object_new: unsafe extern "C" fn(Class) -> Object,
    object_unbox: unsafe extern "C" fn(Object) -> *mut c_void,
    runtime_object_init: unsafe extern "C" fn(Object),
    array_new: unsafe extern "C" fn(Class, usize) -> ArrayPtr,
    string_new: unsafe extern "C" fn(*const c_char) -> StringPtr,
    string_chars: unsafe extern "C" fn(StringPtr) -> *mut u16,
    string_length: unsafe extern "C" fn(StringPtr) -> i32,
    resolve_icall: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    domain_get: unsafe extern "C" fn() -> *mut c_void,
    domain_get_assemblies: unsafe extern "C" fn(*mut c_void, *mut usize) -> *const *mut c_void,
    assembly_get_image: unsafe extern "C" fn(*mut c_void) -> Image,
    image_get_name: unsafe extern "C" fn(Image) -> *const c_char,
    thread_current: unsafe extern "C" fn() -> ThreadPtr,
    thread_get_all_attached_threads: unsafe extern "C" fn(*mut usize) -> *mut ThreadPtr,
}
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn game_module() -> HMODULE {
    let w: Vec<u16> = "GameAssembly.dll\0".encode_utf16().collect();
    unsafe { GetModuleHandleW(w.as_ptr()) }
}

unsafe fn sym<T>(m: HMODULE, name: &[u8]) -> Option<T> {
    GetProcAddress(m, name.as_ptr()).map(|p| std::mem::transmute_copy::<_, T>(&p))
}

macro_rules! need {
    ($m:expr, $s:literal) => {
        match unsafe { sym($m, concat!($s, "\0").as_bytes()) } {
            Some(f) => f,
            None => return None,
        }
    };
}

fn build_api() -> Option<Api> {
    let m = game_module();
    if m.is_null() {
        return None;
    }
    Some(Api {
        class_from_name: need!(m, "il2cpp_class_from_name"),
        class_get_method_from_name: need!(m, "il2cpp_class_get_method_from_name"),
        class_get_methods: need!(m, "il2cpp_class_get_methods"),
        class_get_field_from_name: need!(m, "il2cpp_class_get_field_from_name"),
        class_get_nested_types: need!(m, "il2cpp_class_get_nested_types"),
        class_get_name: need!(m, "il2cpp_class_get_name"),
        field_get_value: need!(m, "il2cpp_field_get_value"),
        field_set_value: need!(m, "il2cpp_field_set_value"),
        field_static_get_value: need!(m, "il2cpp_field_static_get_value"),
        field_static_set_value: need!(m, "il2cpp_field_static_set_value"),
        object_new: need!(m, "il2cpp_object_new"),
        object_unbox: need!(m, "il2cpp_object_unbox"),
        runtime_object_init: need!(m, "il2cpp_runtime_object_init"),
        array_new: need!(m, "il2cpp_array_new"),
        string_new: need!(m, "il2cpp_string_new"),
        string_chars: need!(m, "il2cpp_string_chars"),
        string_length: need!(m, "il2cpp_string_length"),
        resolve_icall: need!(m, "il2cpp_resolve_icall"),
        domain_get: need!(m, "il2cpp_domain_get"),
        domain_get_assemblies: need!(m, "il2cpp_domain_get_assemblies"),
        assembly_get_image: need!(m, "il2cpp_assembly_get_image"),
        image_get_name: need!(m, "il2cpp_image_get_name"),
        thread_current: need!(m, "il2cpp_thread_current"),
        thread_get_all_attached_threads: need!(m, "il2cpp_thread_get_all_attached_threads"),
    })
}

fn api() -> Option<&'static Api> {
    API.get_or_init(build_api).as_ref()
}

// ── hook registry ───────────────────────────────────────────────────────────
struct HookEntry {
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

// ════════════════════════════════════════════════════════════════════════════
// The vtable (SDK v3). FIELD ORDER + SIGNATURES MUST MATCH THE SDK EXACTLY.
// Callback/opaque params are typed as raw pointers (same ABI as Option<fn>).
// ════════════════════════════════════════════════════════════════════════════
#[repr(C)]
pub struct Vtable {
    hachimi_instance: unsafe extern "C" fn() -> *const c_void,
    hachimi_get_interceptor: unsafe extern "C" fn(this: *const c_void) -> *const c_void,

    interceptor_hook: unsafe extern "C" fn(*const c_void, *mut c_void, *mut c_void) -> *mut c_void,
    interceptor_hook_vtable: unsafe extern "C" fn(*const c_void, *mut *mut c_void, usize, *mut c_void) -> *mut c_void,
    interceptor_get_trampoline_addr: unsafe extern "C" fn(*const c_void, *mut c_void) -> *mut c_void,
    interceptor_unhook: unsafe extern "C" fn(*const c_void, *mut c_void) -> *mut c_void,

    il2cpp_resolve_symbol: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    il2cpp_get_assembly_image: unsafe extern "C" fn(*const c_char) -> Image,
    il2cpp_get_class: unsafe extern "C" fn(Image, *const c_char, *const c_char) -> Class,
    il2cpp_get_method: unsafe extern "C" fn(Class, *const c_char, i32) -> Method,
    il2cpp_get_method_overload: unsafe extern "C" fn(Class, *const c_char, *const TypeEnum, usize) -> Method,
    il2cpp_get_method_addr: unsafe extern "C" fn(Class, *const c_char, i32) -> *mut c_void,
    il2cpp_get_method_overload_addr: unsafe extern "C" fn(Class, *const c_char, *const TypeEnum, usize) -> *mut c_void,
    il2cpp_get_method_cached: unsafe extern "C" fn(Class, *const c_char, i32) -> Method,
    il2cpp_get_method_addr_cached: unsafe extern "C" fn(Class, *const c_char, i32) -> *mut c_void,
    il2cpp_find_nested_class: unsafe extern "C" fn(Class, *const c_char) -> Class,
    il2cpp_resolve_icall: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    il2cpp_class_get_methods: unsafe extern "C" fn(Class, *mut *mut c_void) -> Method,
    il2cpp_get_field_from_name: unsafe extern "C" fn(Class, *const c_char) -> Field,
    il2cpp_get_field_value: unsafe extern "C" fn(Object, Field, *mut c_void),
    il2cpp_set_field_value: unsafe extern "C" fn(Object, Field, *const c_void),
    il2cpp_get_static_field_value: unsafe extern "C" fn(Field, *mut c_void),
    il2cpp_set_static_field_value: unsafe extern "C" fn(Field, *const c_void),
    il2cpp_object_new: unsafe extern "C" fn(Class) -> Object,
    il2cpp_unbox: unsafe extern "C" fn(Object) -> *mut c_void,
    il2cpp_get_main_thread: unsafe extern "C" fn() -> ThreadPtr,
    il2cpp_get_attached_threads: unsafe extern "C" fn(*mut usize) -> *mut ThreadPtr,
    il2cpp_schedule_on_thread: unsafe extern "C" fn(ThreadPtr, *mut c_void),
    il2cpp_create_array: unsafe extern "C" fn(Class, ArraySize) -> ArrayPtr,
    il2cpp_get_singleton_like_instance: unsafe extern "C" fn(Class) -> Object,

    log: unsafe extern "C" fn(i32, *const c_char, *const c_char),

    gui_register_menu_item: unsafe extern "C" fn(*const c_char, *mut c_void, *mut c_void) -> bool,
    gui_register_menu_section: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool,
    gui_show_notification: unsafe extern "C" fn(*const c_char) -> bool,
    gui_ui_heading: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    gui_ui_label: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    gui_ui_small: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    gui_ui_separator: unsafe extern "C" fn(*mut c_void) -> bool,
    gui_ui_button: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    gui_ui_small_button: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    gui_ui_checkbox: unsafe extern "C" fn(*mut c_void, *const c_char, *mut bool) -> bool,
    gui_ui_text_edit_singleline: unsafe extern "C" fn(*mut c_void, *mut c_char, usize) -> bool,
    gui_ui_horizontal: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> bool,
    gui_ui_grid: unsafe extern "C" fn(*mut c_void, *const c_char, usize, f32, f32, *mut c_void, *mut c_void) -> bool,
    gui_ui_end_row: unsafe extern "C" fn(*mut c_void) -> bool,
    gui_ui_colored_label: unsafe extern "C" fn(*mut c_void, u8, u8, u8, u8, *const c_char) -> bool,
    gui_register_menu_item_icon: unsafe extern "C" fn(*const c_char, *const c_char, *const u8, usize) -> bool,
    gui_register_menu_section_with_icon: unsafe extern "C" fn(*const c_char, *const c_char, *const u8, usize, *mut c_void, *mut c_void) -> bool,
    gui_new_window_id: unsafe extern "C" fn() -> i32,
    gui_show_window: unsafe extern "C" fn(i32, *const c_char, *mut c_void, *mut c_void, *mut c_void) -> bool,
    gui_close_window: unsafe extern "C" fn(i32),

    android_dex_load: unsafe extern "C" fn(*const u8, usize, *const c_char) -> u64,
    android_dex_unload: unsafe extern "C" fn(u64) -> bool,
    android_dex_call_static_noargs: unsafe extern "C" fn(u64, *const c_char, *const c_char) -> bool,
    android_dex_call_static_string: unsafe extern "C" fn(u64, *const c_char, *const c_char, *const c_char) -> bool,

    il2cpp_runtime_object_init: unsafe extern "C" fn(Object),
    il2cpp_string_new: unsafe extern "C" fn(*const c_char) -> StringPtr,
    il2cpp_string_chars: unsafe extern "C" fn(StringPtr) -> *mut u16,
    il2cpp_string_length: unsafe extern "C" fn(StringPtr) -> i32,
    gui_ui_combo_menu: unsafe extern "C" fn(*mut c_void, *const c_char, *mut i32, *const *const c_char, usize, *mut c_char, usize) -> bool,
    hachimi_register_on_game_initialized: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool,
    hachimi_register_present_callback: unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool,
    gui_get_menu_width: unsafe extern "C" fn() -> f32,
    gui_set_menu_width: unsafe extern "C" fn(f32),
    hachimi_get_base_dir: unsafe extern "C" fn() -> *const c_char,
    hachimi_get_data_path: unsafe extern "C" fn() -> *const c_char,
}

// ABI guard: the SDK v3 vtable is exactly 66 fn-pointer slots. If this fails, a
// field was added/removed/misordered and the layout no longer matches the SDK.
const _: () = assert!(std::mem::size_of::<Vtable>() == 66 * std::mem::size_of::<usize>());

const VTABLE: Vtable = Vtable {
    hachimi_instance: vt_hachimi_instance,
    hachimi_get_interceptor: vt_hachimi_get_interceptor,
    interceptor_hook: vt_interceptor_hook,
    interceptor_hook_vtable: vt_interceptor_hook_vtable,
    interceptor_get_trampoline_addr: vt_interceptor_get_trampoline_addr,
    interceptor_unhook: vt_interceptor_unhook,
    il2cpp_resolve_symbol: vt_resolve_symbol,
    il2cpp_get_assembly_image: vt_get_assembly_image,
    il2cpp_get_class: vt_get_class,
    il2cpp_get_method: vt_get_method,
    il2cpp_get_method_overload: vt_get_method_overload,
    il2cpp_get_method_addr: vt_get_method_addr,
    il2cpp_get_method_overload_addr: vt_get_method_overload_addr,
    il2cpp_get_method_cached: vt_get_method,
    il2cpp_get_method_addr_cached: vt_get_method_addr,
    il2cpp_find_nested_class: vt_find_nested_class,
    il2cpp_resolve_icall: vt_resolve_icall,
    il2cpp_class_get_methods: vt_class_get_methods,
    il2cpp_get_field_from_name: vt_get_field_from_name,
    il2cpp_get_field_value: vt_get_field_value,
    il2cpp_set_field_value: vt_set_field_value,
    il2cpp_get_static_field_value: vt_get_static_field_value,
    il2cpp_set_static_field_value: vt_set_static_field_value,
    il2cpp_object_new: vt_object_new,
    il2cpp_unbox: vt_unbox,
    il2cpp_get_main_thread: vt_get_main_thread,
    il2cpp_get_attached_threads: vt_get_attached_threads,
    il2cpp_schedule_on_thread: vt_schedule_on_thread,
    il2cpp_create_array: vt_create_array,
    il2cpp_get_singleton_like_instance: vt_get_singleton_like_instance,
    log: vt_log,
    gui_register_menu_item: vt_gui_register_menu_item,
    gui_register_menu_section: vt_gui_register_menu_section,
    gui_show_notification: vt_gui_show_notification,
    gui_ui_heading: vt_gui_ui_text2,
    gui_ui_label: vt_gui_ui_text2,
    gui_ui_small: vt_gui_ui_text2,
    gui_ui_separator: vt_gui_ui_ui1,
    gui_ui_button: vt_gui_ui_text2,
    gui_ui_small_button: vt_gui_ui_text2,
    gui_ui_checkbox: vt_gui_ui_checkbox,
    gui_ui_text_edit_singleline: vt_gui_ui_text_edit,
    gui_ui_horizontal: vt_gui_ui_callback2,
    gui_ui_grid: vt_gui_ui_grid,
    gui_ui_end_row: vt_gui_ui_ui1,
    gui_ui_colored_label: vt_gui_ui_colored_label,
    gui_register_menu_item_icon: vt_gui_register_menu_item_icon,
    gui_register_menu_section_with_icon: vt_gui_register_menu_section_with_icon,
    gui_new_window_id: vt_gui_new_window_id,
    gui_show_window: vt_gui_show_window,
    gui_close_window: vt_gui_close_window,
    android_dex_load: vt_android_dex_load,
    android_dex_unload: vt_android_bool_u64,
    android_dex_call_static_noargs: vt_android_dex_call_noargs,
    android_dex_call_static_string: vt_android_dex_call_string,
    il2cpp_runtime_object_init: vt_runtime_object_init,
    il2cpp_string_new: vt_string_new,
    il2cpp_string_chars: vt_string_chars,
    il2cpp_string_length: vt_string_length,
    gui_ui_combo_menu: vt_gui_ui_combo_menu,
    hachimi_register_on_game_initialized: vt_register_on_game_initialized,
    hachimi_register_present_callback: vt_register_present_callback,
    gui_get_menu_width: vt_gui_get_menu_width,
    gui_set_menu_width: vt_gui_set_menu_width,
    hachimi_get_base_dir: vt_get_base_dir,
    hachimi_get_data_path: vt_get_data_path,
};

static HOST_TOKEN: u8 = 0;
static INTERCEPTOR_TOKEN: u8 = 0;

// ── core: instance / interceptor handles (opaque, plugin only passes them back) ─
unsafe extern "C" fn vt_hachimi_instance() -> *const c_void {
    &HOST_TOKEN as *const u8 as *const c_void
}
unsafe extern "C" fn vt_hachimi_get_interceptor(_this: *const c_void) -> *const c_void {
    &INTERCEPTOR_TOKEN as *const u8 as *const c_void
}

// ── core: hook interceptor (retour) ─────────────────────────────────────────
unsafe extern "C" fn vt_interceptor_hook(_this: *const c_void, orig: *mut c_void, hook: *mut c_void) -> *mut c_void {
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
unsafe extern "C" fn vt_interceptor_hook_vtable(_this: *const c_void, vtable: *mut *mut c_void, index: usize, hook: *mut c_void) -> *mut c_void {
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
unsafe extern "C" fn vt_interceptor_get_trampoline_addr(_this: *const c_void, hook: *mut c_void) -> *mut c_void {
    if let Ok(g) = hooks().lock() {
        if let Some(e) = g.iter().find(|e| e.hook_addr == hook as usize) {
            return e.tramp as *mut c_void;
        }
    }
    std::ptr::null_mut()
}
unsafe extern "C" fn vt_interceptor_unhook(_this: *const c_void, hook: *mut c_void) -> *mut c_void {
    if let Ok(mut g) = hooks().lock() {
        if let Some(pos) = g.iter().position(|e| e.hook_addr == hook as usize) {
            let e = g.remove(pos);
            let _ = e.detour.disable();
            return e.orig_addr as *mut c_void;
        }
    }
    std::ptr::null_mut()
}

// ── core: il2cpp bridge ─────────────────────────────────────────────────────
unsafe extern "C" fn vt_resolve_symbol(name: *const c_char) -> *mut c_void {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let m = game_module();
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = CStr::from_ptr(name).to_bytes_with_nul();
    GetProcAddress(m, bytes.as_ptr()).map(|p| p as *mut c_void).unwrap_or(std::ptr::null_mut())
}
unsafe extern "C" fn vt_resolve_icall(name: *const c_char) -> *mut c_void {
    match api() {
        Some(a) => (a.resolve_icall)(name),
        None => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn vt_get_assembly_image(name: *const c_char) -> Image {
    let Some(a) = api() else { return std::ptr::null() };
    if name.is_null() {
        return std::ptr::null();
    }
    let want = CStr::from_ptr(name);
    let domain = (a.domain_get)();
    if domain.is_null() {
        return std::ptr::null();
    }
    let mut count = 0usize;
    let asms = (a.domain_get_assemblies)(domain, &mut count);
    if asms.is_null() {
        return std::ptr::null();
    }
    for i in 0..count {
        let asm = *asms.add(i);
        if asm.is_null() {
            continue;
        }
        let img = (a.assembly_get_image)(asm);
        if !img.is_null() {
            let n = (a.image_get_name)(img);
            if !n.is_null() && CStr::from_ptr(n) == want {
                return img;
            }
        }
    }
    std::ptr::null()
}
unsafe extern "C" fn vt_get_class(image: Image, ns: *const c_char, name: *const c_char) -> Class {
    match api() {
        Some(a) => (a.class_from_name)(image, ns, name),
        None => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn vt_get_method(class: Class, name: *const c_char, argc: i32) -> Method {
    let Some(a) = api() else { return std::ptr::null() };
    if class.is_null() {
        return std::ptr::null();
    }
    (a.class_get_method_from_name)(class, name, argc)
}
unsafe extern "C" fn vt_get_method_addr(class: Class, name: *const c_char, argc: i32) -> *mut c_void {
    let m = vt_get_method(class, name, argc);
    if m.is_null() {
        return std::ptr::null_mut();
    }
    *(m as *const *mut c_void) // MethodInfo->methodPointer @ +0
}
// Overload resolution by exact param types is not bridged; best-effort by name +
// arg count (distinct-type same-arity overloads are rare). Documented limitation.
unsafe extern "C" fn vt_get_method_overload(class: Class, name: *const c_char, _params: *const TypeEnum, count: usize) -> Method {
    vt_get_method(class, name, count as i32)
}
unsafe extern "C" fn vt_get_method_overload_addr(class: Class, name: *const c_char, _params: *const TypeEnum, count: usize) -> *mut c_void {
    vt_get_method_addr(class, name, count as i32)
}
unsafe extern "C" fn vt_class_get_methods(class: Class, iter: *mut *mut c_void) -> Method {
    match api() {
        Some(a) => (a.class_get_methods)(class, iter),
        None => std::ptr::null(),
    }
}
unsafe extern "C" fn vt_find_nested_class(class: Class, name: *const c_char) -> Class {
    let Some(a) = api() else { return std::ptr::null_mut() };
    if class.is_null() || name.is_null() {
        return std::ptr::null_mut();
    }
    let want = CStr::from_ptr(name);
    let mut iter: *mut c_void = std::ptr::null_mut();
    loop {
        let nested = (a.class_get_nested_types)(class, &mut iter);
        if nested.is_null() {
            return std::ptr::null_mut();
        }
        let n = (a.class_get_name)(nested);
        if !n.is_null() && CStr::from_ptr(n) == want {
            return nested;
        }
    }
}
unsafe extern "C" fn vt_get_field_from_name(class: Class, name: *const c_char) -> Field {
    let Some(a) = api() else { return std::ptr::null_mut() };
    if class.is_null() {
        return std::ptr::null_mut();
    }
    (a.class_get_field_from_name)(class, name)
}
unsafe extern "C" fn vt_get_field_value(obj: Object, field: Field, out: *mut c_void) {
    if let Some(a) = api() {
        (a.field_get_value)(obj, field, out);
    }
}
unsafe extern "C" fn vt_set_field_value(obj: Object, field: Field, val: *const c_void) {
    if let Some(a) = api() {
        (a.field_set_value)(obj, field, val as *mut c_void);
    }
}
unsafe extern "C" fn vt_get_static_field_value(field: Field, out: *mut c_void) {
    if let Some(a) = api() {
        (a.field_static_get_value)(field, out);
    }
}
unsafe extern "C" fn vt_set_static_field_value(field: Field, val: *const c_void) {
    if let Some(a) = api() {
        (a.field_static_set_value)(field, val as *mut c_void);
    }
}
unsafe extern "C" fn vt_object_new(class: Class) -> Object {
    match api() {
        Some(a) => (a.object_new)(class),
        None => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn vt_unbox(obj: Object) -> *mut c_void {
    match api() {
        Some(a) => (a.object_unbox)(obj),
        None => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn vt_runtime_object_init(obj: Object) {
    if let Some(a) = api() {
        (a.runtime_object_init)(obj);
    }
}
unsafe extern "C" fn vt_get_main_thread() -> ThreadPtr {
    match api() {
        Some(a) => (a.thread_current)(),
        None => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn vt_get_attached_threads(out_size: *mut usize) -> *mut ThreadPtr {
    match api() {
        Some(a) => (a.thread_get_all_attached_threads)(out_size),
        None => {
            if !out_size.is_null() {
                *out_size = 0;
            }
            std::ptr::null_mut()
        }
    }
}
unsafe extern "C" fn vt_schedule_on_thread(_thread: ThreadPtr, _cb: *mut c_void) {
    plog("plugin called schedule_on_thread (unsupported, ignored)");
}
unsafe extern "C" fn vt_create_array(elem: Class, len: ArraySize) -> ArrayPtr {
    match api() {
        Some(a) => (a.array_new)(elem, len),
        None => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn vt_get_singleton_like_instance(_class: Class) -> Object {
    std::ptr::null_mut()
}
unsafe extern "C" fn vt_string_new(text: *const c_char) -> StringPtr {
    match api() {
        Some(a) => (a.string_new)(text),
        None => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn vt_string_chars(s: StringPtr) -> *mut u16 {
    match api() {
        Some(a) => (a.string_chars)(s),
        None => std::ptr::null_mut(),
    }
}
unsafe extern "C" fn vt_string_length(s: StringPtr) -> i32 {
    match api() {
        Some(a) => (a.string_length)(s),
        None => 0,
    }
}

unsafe extern "C" fn vt_log(level: i32, target: *const c_char, message: *const c_char) {
    let t = if target.is_null() { String::new() } else { CStr::from_ptr(target).to_string_lossy().into_owned() };
    let m = if message.is_null() { String::new() } else { CStr::from_ptr(message).to_string_lossy().into_owned() };
    let lvl = match level { 1 => "ERROR", 2 => "WARN", 3 => "INFO", 4 => "DEBUG", 5 => "TRACE", _ => "INFO" };
    plog(&format!("[{lvl}] {t}: {m}"));
}

// ── host services: game-initialized callback (fire immediately — by the time we
//    init plugins the runtime is already up) ──────────────────────────────────
type GameInitFn = unsafe extern "C" fn(userdata: *mut c_void);
unsafe extern "C" fn vt_register_on_game_initialized(cb: *mut c_void, userdata: *mut c_void) -> bool {
    if cb.is_null() {
        return false;
    }
    let f: GameInitFn = std::mem::transmute(cb);
    f(userdata);
    true
}
// Present callback (per-frame draw) is not wired into Heaven's render loop yet;
// a packet-capture plugin doesn't need it to capture. Accept it so the plugin
// proceeds; if a plugin's drawing is essential we can route Heaven's Present here.
unsafe extern "C" fn vt_register_present_callback(_cb: *mut c_void, _userdata: *mut c_void) -> bool {
    plog("plugin registered a present callback (not driven; capture still works)");
    true
}

// ── host services: data paths (a plugin may dump/read files here) ────────────
fn data_dir_cstring() -> &'static CString {
    static P: OnceLock<CString> = OnceLock::new();
    P.get_or_init(|| {
        let dir = crate::paths::dll_dir().join("hachimi");
        let _ = std::fs::create_dir_all(&dir);
        CString::new(dir.to_string_lossy().as_bytes().to_vec()).unwrap_or_default()
    })
}
fn base_dir_cstring() -> &'static CString {
    static P: OnceLock<CString> = OnceLock::new();
    P.get_or_init(|| {
        let dir = crate::paths::dll_dir();
        CString::new(dir.to_string_lossy().as_bytes().to_vec()).unwrap_or_default()
    })
}
unsafe extern "C" fn vt_get_base_dir() -> *const c_char {
    base_dir_cstring().as_ptr()
}
unsafe extern "C" fn vt_get_data_path() -> *const c_char {
    data_dir_cstring().as_ptr()
}

// ── stubs: GUI + Android host services (kept IN ORDER; not used by capture
//    plugins). Returning a sane default (false/0) is safe; the slot is correct. ─
unsafe extern "C" fn vt_gui_register_menu_item(_a: *const c_char, _b: *mut c_void, _c: *mut c_void) -> bool { false }
unsafe extern "C" fn vt_gui_register_menu_section(_a: *mut c_void, _b: *mut c_void) -> bool { false }
unsafe extern "C" fn vt_gui_show_notification(_a: *const c_char) -> bool { false }
unsafe extern "C" fn vt_gui_ui_text2(_ui: *mut c_void, _t: *const c_char) -> bool { false }
unsafe extern "C" fn vt_gui_ui_ui1(_ui: *mut c_void) -> bool { false }
unsafe extern "C" fn vt_gui_ui_checkbox(_ui: *mut c_void, _t: *const c_char, _v: *mut bool) -> bool { false }
unsafe extern "C" fn vt_gui_ui_text_edit(_ui: *mut c_void, _b: *mut c_char, _l: usize) -> bool { false }
unsafe extern "C" fn vt_gui_ui_callback2(_ui: *mut c_void, _cb: *mut c_void, _u: *mut c_void) -> bool { false }
unsafe extern "C" fn vt_gui_ui_grid(_ui: *mut c_void, _id: *const c_char, _c: usize, _x: f32, _y: f32, _cb: *mut c_void, _u: *mut c_void) -> bool { false }
unsafe extern "C" fn vt_gui_ui_colored_label(_ui: *mut c_void, _r: u8, _g: u8, _b: u8, _a: u8, _t: *const c_char) -> bool { false }
unsafe extern "C" fn vt_gui_register_menu_item_icon(_a: *const c_char, _b: *const c_char, _c: *const u8, _d: usize) -> bool { false }
unsafe extern "C" fn vt_gui_register_menu_section_with_icon(_a: *const c_char, _b: *const c_char, _c: *const u8, _d: usize, _e: *mut c_void, _f: *mut c_void) -> bool { false }
unsafe extern "C" fn vt_gui_new_window_id() -> i32 { 0 }
unsafe extern "C" fn vt_gui_show_window(_id: i32, _t: *const c_char, _c: *mut c_void, _b: *mut c_void, _u: *mut c_void) -> bool { false }
unsafe extern "C" fn vt_gui_close_window(_id: i32) {}
unsafe extern "C" fn vt_gui_ui_combo_menu(_ui: *mut c_void, _id: *const c_char, _s: *mut i32, _i: *const *const c_char, _n: usize, _st: *mut c_char, _sl: usize) -> bool { false }
unsafe extern "C" fn vt_gui_get_menu_width() -> f32 { 0.0 }
unsafe extern "C" fn vt_gui_set_menu_width(_w: f32) {}
unsafe extern "C" fn vt_android_dex_load(_p: *const u8, _l: usize, _c: *const c_char) -> u64 { 0 }
unsafe extern "C" fn vt_android_bool_u64(_h: u64) -> bool { false }
unsafe extern "C" fn vt_android_dex_call_noargs(_h: u64, _m: *const c_char, _s: *const c_char) -> bool { false }
unsafe extern "C" fn vt_android_dex_call_string(_h: u64, _m: *const c_char, _s: *const c_char, _a: *const c_char) -> bool { false }

// ── plugin log ──────────────────────────────────────────────────────────────
fn plog(msg: &str) {
    use std::io::Write;
    let path = crate::paths::log_file("heaven-plugins.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{msg}");
    }
}

// ════════════════════════════════════════════════════════════════════════════
// entry point — called from boot once il2cpp is ready.
// ════════════════════════════════════════════════════════════════════════════
fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// For every `*.dll` in `heaven_plugins/` exporting `hachimi_init`, hand it our
/// compatible vtable so it installs its hooks. DLLs without that export are
/// self-contained mods already started by the early loader — left alone.
pub fn init_plugins() -> String {
    let dir: PathBuf = crate::paths::dll_dir().join("heaven_plugins");
    if !dir.is_dir() {
        return "no heaven_plugins/ (skipped)".into();
    }
    if api().is_none() {
        return "il2cpp api unavailable (skipped)".into();
    }
    let mut dlls: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("dll")).unwrap_or(false))
            .collect(),
        Err(e) => return format!("read_dir failed: {e}"),
    };
    dlls.sort();
    if dlls.is_empty() {
        return "heaven_plugins/ empty".into();
    }

    let mut inited = 0u32;
    let mut notes = String::new();
    for path in &dlls {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let w = wide(path.as_os_str());
        let mut handle = unsafe { GetModuleHandleW(w.as_ptr()) };
        if handle.is_null() {
            handle = unsafe { LoadLibraryW(w.as_ptr()) };
        }
        if handle.is_null() {
            notes.push_str(&format!(" [{name}: load FAIL]"));
            continue;
        }
        let init: Option<HachimiInitFn> = unsafe { sym(handle, b"hachimi_init\0") };
        let Some(init) = init else {
            continue; // self-contained mod, not an SDK plugin
        };
        plog(&format!("calling hachimi_init: {name} (host v{SDK_VERSION})"));
        let res = unsafe { init(&VTABLE, SDK_VERSION) };
        if res == InitResult::Ok {
            inited += 1;
            plog(&format!("init OK: {name}"));
            notes.push_str(&format!(" [{name}: OK]"));
        } else {
            plog(&format!("init ERROR: {name}"));
            notes.push_str(&format!(" [{name}: ERROR]"));
        }
    }
    if inited == 0 && notes.is_empty() {
        "no SDK plugins (none export hachimi_init)".into()
    } else {
        format!("{inited} initialised{notes}")
    }
}
