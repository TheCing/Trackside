//! il2cpp C API resolved from GameAssembly.dll. Resolved once, lazily.
//!
//! This is the compat layer's OWN export table (distinct from `il2cpp.rs` /
//! `htt_il2cpp.rs`) — it only pulls the entry points the host vtable needs.

use std::ffi::c_char;
use std::ffi::c_void;
use std::sync::OnceLock;

use super::{
    need, ArrayPtr, Class, Field, Image, Method, Object, StringPtr, ThreadPtr,
};

pub(crate) struct Api {
    pub(crate) class_from_name: unsafe extern "C" fn(Image, *const c_char, *const c_char) -> Class,
    pub(crate) class_get_method_from_name: unsafe extern "C" fn(Class, *const c_char, i32) -> Method,
    pub(crate) class_get_methods: unsafe extern "C" fn(Class, *mut *mut c_void) -> Method,
    pub(crate) class_get_field_from_name: unsafe extern "C" fn(Class, *const c_char) -> Field,
    pub(crate) class_get_nested_types: unsafe extern "C" fn(Class, *mut *mut c_void) -> Class,
    pub(crate) class_get_name: unsafe extern "C" fn(Class) -> *const c_char,
    pub(crate) field_get_value: unsafe extern "C" fn(Object, Field, *mut c_void),
    pub(crate) field_set_value: unsafe extern "C" fn(Object, Field, *mut c_void),
    pub(crate) field_static_get_value: unsafe extern "C" fn(Field, *mut c_void),
    pub(crate) field_static_set_value: unsafe extern "C" fn(Field, *mut c_void),
    pub(crate) object_new: unsafe extern "C" fn(Class) -> Object,
    pub(crate) object_unbox: unsafe extern "C" fn(Object) -> *mut c_void,
    pub(crate) runtime_object_init: unsafe extern "C" fn(Object),
    pub(crate) array_new: unsafe extern "C" fn(Class, usize) -> ArrayPtr,
    pub(crate) string_new: unsafe extern "C" fn(*const c_char) -> StringPtr,
    pub(crate) string_chars: unsafe extern "C" fn(StringPtr) -> *mut u16,
    pub(crate) string_length: unsafe extern "C" fn(StringPtr) -> i32,
    pub(crate) resolve_icall: unsafe extern "C" fn(*const c_char) -> *mut c_void,
    pub(crate) domain_get: unsafe extern "C" fn() -> *mut c_void,
    pub(crate) domain_get_assemblies:
        unsafe extern "C" fn(*mut c_void, *mut usize) -> *const *mut c_void,
    pub(crate) assembly_get_image: unsafe extern "C" fn(*mut c_void) -> Image,
    pub(crate) image_get_name: unsafe extern "C" fn(Image) -> *const c_char,
    pub(crate) thread_current: unsafe extern "C" fn() -> ThreadPtr,
    pub(crate) thread_get_all_attached_threads: unsafe extern "C" fn(*mut usize) -> *mut ThreadPtr,
}
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn build_api() -> Option<Api> {
    let m = super::game_module();
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

pub(crate) fn api() -> Option<&'static Api> {
    API.get_or_init(build_api).as_ref()
}
