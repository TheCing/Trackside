//! Vtable function implementations: the il2cpp bridge, logging, host services
//! (game-initialized / present callbacks, data paths) and the GUI/Android stubs.
//!
//! Every function here is a vtable slot backing. Signatures/order are fixed by the
//! SDK; see `vtable.rs` for how they wire into the const `VTABLE`.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::OnceLock;

use windows_sys::Win32::System::LibraryLoader::GetProcAddress;

use super::il2cpp_api::api;
use super::{plog, ArrayPtr, ArraySize, Class, Field, Image, Method, Object, StringPtr, ThreadPtr, TypeEnum};

// ── core: il2cpp bridge ─────────────────────────────────────────────────────
pub(crate) unsafe extern "C" fn vt_resolve_symbol(name: *const c_char) -> *mut c_void {
    if name.is_null() {
        return std::ptr::null_mut();
    }
    let m = super::game_module();
    if m.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = CStr::from_ptr(name).to_bytes_with_nul();
    GetProcAddress(m, bytes.as_ptr()).map(|p| p as *mut c_void).unwrap_or(std::ptr::null_mut())
}
pub(crate) unsafe extern "C" fn vt_resolve_icall(name: *const c_char) -> *mut c_void {
    match api() {
        Some(a) => (a.resolve_icall)(name),
        None => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_get_assembly_image(name: *const c_char) -> Image {
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
pub(crate) unsafe extern "C" fn vt_get_class(image: Image, ns: *const c_char, name: *const c_char) -> Class {
    match api() {
        Some(a) => (a.class_from_name)(image, ns, name),
        None => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_get_method(class: Class, name: *const c_char, argc: i32) -> Method {
    let Some(a) = api() else { return std::ptr::null() };
    if class.is_null() {
        return std::ptr::null();
    }
    (a.class_get_method_from_name)(class, name, argc)
}
pub(crate) unsafe extern "C" fn vt_get_method_addr(class: Class, name: *const c_char, argc: i32) -> *mut c_void {
    let m = vt_get_method(class, name, argc);
    if m.is_null() {
        return std::ptr::null_mut();
    }
    *(m as *const *mut c_void) // MethodInfo->methodPointer @ +0
}
// Overload resolution by exact param types is not bridged; best-effort by name +
// arg count (distinct-type same-arity overloads are rare). Documented limitation.
pub(crate) unsafe extern "C" fn vt_get_method_overload(class: Class, name: *const c_char, _params: *const TypeEnum, count: usize) -> Method {
    vt_get_method(class, name, count as i32)
}
pub(crate) unsafe extern "C" fn vt_get_method_overload_addr(class: Class, name: *const c_char, _params: *const TypeEnum, count: usize) -> *mut c_void {
    vt_get_method_addr(class, name, count as i32)
}
pub(crate) unsafe extern "C" fn vt_class_get_methods(class: Class, iter: *mut *mut c_void) -> Method {
    match api() {
        Some(a) => (a.class_get_methods)(class, iter),
        None => std::ptr::null(),
    }
}
pub(crate) unsafe extern "C" fn vt_find_nested_class(class: Class, name: *const c_char) -> Class {
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
pub(crate) unsafe extern "C" fn vt_get_field_from_name(class: Class, name: *const c_char) -> Field {
    let Some(a) = api() else { return std::ptr::null_mut() };
    if class.is_null() {
        return std::ptr::null_mut();
    }
    (a.class_get_field_from_name)(class, name)
}
pub(crate) unsafe extern "C" fn vt_get_field_value(obj: Object, field: Field, out: *mut c_void) {
    if let Some(a) = api() {
        (a.field_get_value)(obj, field, out);
    }
}
pub(crate) unsafe extern "C" fn vt_set_field_value(obj: Object, field: Field, val: *const c_void) {
    if let Some(a) = api() {
        (a.field_set_value)(obj, field, val as *mut c_void);
    }
}
pub(crate) unsafe extern "C" fn vt_get_static_field_value(field: Field, out: *mut c_void) {
    if let Some(a) = api() {
        (a.field_static_get_value)(field, out);
    }
}
pub(crate) unsafe extern "C" fn vt_set_static_field_value(field: Field, val: *const c_void) {
    if let Some(a) = api() {
        (a.field_static_set_value)(field, val as *mut c_void);
    }
}
pub(crate) unsafe extern "C" fn vt_object_new(class: Class) -> Object {
    match api() {
        Some(a) => (a.object_new)(class),
        None => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_unbox(obj: Object) -> *mut c_void {
    match api() {
        Some(a) => (a.object_unbox)(obj),
        None => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_runtime_object_init(obj: Object) {
    if let Some(a) = api() {
        (a.runtime_object_init)(obj);
    }
}
pub(crate) unsafe extern "C" fn vt_get_main_thread() -> ThreadPtr {
    match api() {
        Some(a) => (a.thread_current)(),
        None => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_get_attached_threads(out_size: *mut usize) -> *mut ThreadPtr {
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
pub(crate) unsafe extern "C" fn vt_schedule_on_thread(_thread: ThreadPtr, _cb: *mut c_void) {
    plog("plugin called schedule_on_thread (unsupported, ignored)");
}
pub(crate) unsafe extern "C" fn vt_create_array(elem: Class, len: ArraySize) -> ArrayPtr {
    match api() {
        Some(a) => (a.array_new)(elem, len),
        None => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_get_singleton_like_instance(_class: Class) -> Object {
    std::ptr::null_mut()
}
pub(crate) unsafe extern "C" fn vt_string_new(text: *const c_char) -> StringPtr {
    match api() {
        Some(a) => (a.string_new)(text),
        None => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_string_chars(s: StringPtr) -> *mut u16 {
    match api() {
        Some(a) => (a.string_chars)(s),
        None => std::ptr::null_mut(),
    }
}
pub(crate) unsafe extern "C" fn vt_string_length(s: StringPtr) -> i32 {
    match api() {
        Some(a) => (a.string_length)(s),
        None => 0,
    }
}

pub(crate) unsafe extern "C" fn vt_log(level: i32, target: *const c_char, message: *const c_char) {
    let t = if target.is_null() { String::new() } else { CStr::from_ptr(target).to_string_lossy().into_owned() };
    let m = if message.is_null() { String::new() } else { CStr::from_ptr(message).to_string_lossy().into_owned() };
    let lvl = match level { 1 => "ERROR", 2 => "WARN", 3 => "INFO", 4 => "DEBUG", 5 => "TRACE", _ => "INFO" };
    plog(&format!("[{lvl}] {t}: {m}"));
}

// ── host services: game-initialized callback (fire immediately — by the time we
//    init plugins the runtime is already up) ──────────────────────────────────
type GameInitFn = unsafe extern "C" fn(userdata: *mut c_void);
pub(crate) unsafe extern "C" fn vt_register_on_game_initialized(cb: *mut c_void, userdata: *mut c_void) -> bool {
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
pub(crate) unsafe extern "C" fn vt_register_present_callback(_cb: *mut c_void, _userdata: *mut c_void) -> bool {
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
pub(crate) unsafe extern "C" fn vt_get_base_dir() -> *const c_char {
    base_dir_cstring().as_ptr()
}
pub(crate) unsafe extern "C" fn vt_get_data_path() -> *const c_char {
    data_dir_cstring().as_ptr()
}

// ── stubs: GUI + Android host services (kept IN ORDER; not used by capture
//    plugins). Returning a sane default (false/0) is safe; the slot is correct. ─
pub(crate) unsafe extern "C" fn vt_gui_register_menu_item(_a: *const c_char, _b: *mut c_void, _c: *mut c_void) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_register_menu_section(_a: *mut c_void, _b: *mut c_void) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_show_notification(_a: *const c_char) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_ui_text2(_ui: *mut c_void, _t: *const c_char) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_ui_ui1(_ui: *mut c_void) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_ui_checkbox(_ui: *mut c_void, _t: *const c_char, _v: *mut bool) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_ui_text_edit(_ui: *mut c_void, _b: *mut c_char, _l: usize) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_ui_callback2(_ui: *mut c_void, _cb: *mut c_void, _u: *mut c_void) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_ui_grid(_ui: *mut c_void, _id: *const c_char, _c: usize, _x: f32, _y: f32, _cb: *mut c_void, _u: *mut c_void) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_ui_colored_label(_ui: *mut c_void, _r: u8, _g: u8, _b: u8, _a: u8, _t: *const c_char) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_register_menu_item_icon(_a: *const c_char, _b: *const c_char, _c: *const u8, _d: usize) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_register_menu_section_with_icon(_a: *const c_char, _b: *const c_char, _c: *const u8, _d: usize, _e: *mut c_void, _f: *mut c_void) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_new_window_id() -> i32 { 0 }
pub(crate) unsafe extern "C" fn vt_gui_show_window(_id: i32, _t: *const c_char, _c: *mut c_void, _b: *mut c_void, _u: *mut c_void) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_close_window(_id: i32) {}
pub(crate) unsafe extern "C" fn vt_gui_ui_combo_menu(_ui: *mut c_void, _id: *const c_char, _s: *mut i32, _i: *const *const c_char, _n: usize, _st: *mut c_char, _sl: usize) -> bool { false }
pub(crate) unsafe extern "C" fn vt_gui_get_menu_width() -> f32 { 0.0 }
pub(crate) unsafe extern "C" fn vt_gui_set_menu_width(_w: f32) {}
pub(crate) unsafe extern "C" fn vt_android_dex_load(_p: *const u8, _l: usize, _c: *const c_char) -> u64 { 0 }
pub(crate) unsafe extern "C" fn vt_android_bool_u64(_h: u64) -> bool { false }
pub(crate) unsafe extern "C" fn vt_android_dex_call_noargs(_h: u64, _m: *const c_char, _s: *const c_char) -> bool { false }
pub(crate) unsafe extern "C" fn vt_android_dex_call_string(_h: u64, _m: *const c_char, _s: *const c_char, _a: *const c_char) -> bool { false }
