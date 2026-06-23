//! HorseTheTrails — IL2CPP class/field introspection bindings.
//!
//! The base `il2cpp.rs` resolves only the handful of exports the core modules
//! need. The Team Trials capture additionally needs field-offset / class /
//! array introspection to read named fields out of the game's response object.
//! Those extra `il2cpp_*` exports are resolved here, from GameAssembly.dll via
//! GetProcAddress, and exposed through the targeted-read helpers at the bottom
//! (`field_offset`, `read_i32`, `array_elem`, `read_string`, ...).

#![allow(non_upper_case_globals, dead_code, static_mut_refs)]

use core::ffi::{c_char, c_void, CStr};
use std::ffi::CString;

use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

pub type RawObject = c_void;
pub type RawArray = c_void;
pub type RawClass = c_void;
pub type RawField = c_void;
pub type RawMethod = c_void;
pub type RawType = c_void;
pub type RawImage = c_void;
pub type RawAssembly = c_void;
pub type RawDomain = c_void;
pub type RawThread = c_void;

pub const FIELD_ATTRIBUTE_STATIC: u32 = 0x0010;
pub const FIELD_ATTRIBUTE_LITERAL: u32 = 0x0040;
pub const METHOD_ATTRIBUTE_STATIC: u32 = 0x0010;

pub type FnClassGetFields = unsafe extern "C" fn(*mut RawClass, *mut *mut c_void) -> *mut RawField;
pub type FnFieldGetName = unsafe extern "C" fn(*mut RawField) -> *const c_char;
pub type FnFieldGetType = unsafe extern "C" fn(*mut RawField) -> *mut RawType;
pub type FnFieldGetOffset = unsafe extern "C" fn(*mut RawField) -> usize;
pub type FnFieldGetFlags = unsafe extern "C" fn(*mut RawField) -> u32;
pub type FnFieldStaticGetValue = unsafe extern "C" fn(*mut RawField, *mut c_void);
pub type FnTypeGetType = unsafe extern "C" fn(*mut RawType) -> i32;
pub type FnArrayLength = unsafe extern "C" fn(*mut RawArray) -> u32;
pub type FnArrayNew = unsafe extern "C" fn(*mut RawClass, usize) -> *mut RawArray;
pub type FnObjectGetClass = unsafe extern "C" fn(*mut RawObject) -> *mut RawClass;
pub type FnClassGetName = unsafe extern "C" fn(*mut RawClass) -> *const c_char;
pub type FnClassGetParent = unsafe extern "C" fn(*mut RawClass) -> *mut RawClass;
pub type FnClassGetMethods = unsafe extern "C" fn(*mut RawClass, *mut *mut c_void) -> *mut RawMethod;
pub type FnMethodGetParamCount = unsafe extern "C" fn(*mut RawMethod) -> u32;
pub type FnMethodGetParam = unsafe extern "C" fn(*mut RawMethod, u32) -> *mut RawType;
pub type FnMethodGetName = unsafe extern "C" fn(*mut RawMethod) -> *const c_char;
pub type FnClassFromType = unsafe extern "C" fn(*mut RawType) -> *mut RawClass;
pub type FnClassIsEnum = unsafe extern "C" fn(*mut RawClass) -> bool;
pub type FnClassIsValueType = unsafe extern "C" fn(*mut RawClass) -> bool;
pub type FnClassValueSize = unsafe extern "C" fn(*mut RawClass, *mut u32) -> i32;
pub type FnClassGetElementClass = unsafe extern "C" fn(*mut RawClass) -> *mut RawClass;
pub type FnImageGetClassCount = unsafe extern "C" fn(*mut RawImage) -> usize;
pub type FnImageGetClass = unsafe extern "C" fn(*mut RawImage, usize) -> *mut RawClass;
pub type FnClassFromName = unsafe extern "C" fn(*const RawImage, *const c_char, *const c_char) -> *mut RawClass;
pub type FnDomainGet = unsafe extern "C" fn() -> *mut RawDomain;
pub type FnDomainGetAssemblies = unsafe extern "C" fn(*mut RawDomain, *mut usize) -> *mut *const RawAssembly;
pub type FnAssemblyGetImage = unsafe extern "C" fn(*const RawAssembly) -> *mut RawImage;
pub type FnImageGetName = unsafe extern "C" fn(*const RawImage) -> *const c_char;
pub type FnThreadCurrent = unsafe extern "C" fn() -> *mut RawThread;
pub type FnThreadAttach = unsafe extern "C" fn(*mut RawDomain) -> *mut RawThread;
pub type FnThreadDetach = unsafe extern "C" fn(*mut RawThread);
pub type FnClassGetMethodFromName = unsafe extern "C" fn(*mut RawClass, *const c_char, i32) -> *mut RawMethod;
pub type FnMethodGetFlags = unsafe extern "C" fn(*mut RawMethod, *mut u32) -> u32;
pub type FnClassGetType = unsafe extern "C" fn(*mut RawClass) -> *mut RawType;
pub type FnTypeGetObject = unsafe extern "C" fn(*mut RawType) -> *mut RawObject;

macro_rules! fnptrs {
    ($($name:ident : $ty:ty),+ $(,)?) => {
        $( pub static mut $name: Option<$ty> = None; )+
    };
}
fnptrs! {
    CLASS_GET_FIELDS: FnClassGetFields,
    FIELD_GET_NAME: FnFieldGetName,
    FIELD_GET_TYPE: FnFieldGetType,
    FIELD_GET_OFFSET: FnFieldGetOffset,
    FIELD_GET_FLAGS: FnFieldGetFlags,
    FIELD_STATIC_GET_VALUE: FnFieldStaticGetValue,
    TYPE_GET_TYPE: FnTypeGetType,
    ARRAY_LENGTH: FnArrayLength,
    ARRAY_NEW: FnArrayNew,
    OBJECT_GET_CLASS: FnObjectGetClass,
    CLASS_GET_NAME: FnClassGetName,
    CLASS_GET_NAMESPACE: FnClassGetName,
    CLASS_GET_PARENT: FnClassGetParent,
    CLASS_GET_METHODS: FnClassGetMethods,
    METHOD_GET_PARAM_COUNT: FnMethodGetParamCount,
    METHOD_GET_PARAM: FnMethodGetParam,
    METHOD_GET_NAME: FnMethodGetName,
    CLASS_FROM_TYPE: FnClassFromType,
    CLASS_IS_ENUM: FnClassIsEnum,
    CLASS_IS_VALUETYPE: FnClassIsValueType,
    CLASS_VALUE_SIZE: FnClassValueSize,
    CLASS_GET_ELEMENT_CLASS: FnClassGetElementClass,
    IMAGE_GET_CLASS_COUNT: FnImageGetClassCount,
    IMAGE_GET_CLASS: FnImageGetClass,
    CLASS_FROM_NAME: FnClassFromName,
    DOMAIN_GET: FnDomainGet,
    DOMAIN_GET_ASSEMBLIES: FnDomainGetAssemblies,
    ASSEMBLY_GET_IMAGE: FnAssemblyGetImage,
    IMAGE_GET_NAME: FnImageGetName,
    THREAD_CURRENT: FnThreadCurrent,
    THREAD_ATTACH: FnThreadAttach,
    THREAD_DETACH: FnThreadDetach,
    CLASS_GET_METHOD_FROM_NAME: FnClassGetMethodFromName,
    METHOD_GET_FLAGS: FnMethodGetFlags,
    CLASS_GET_TYPE: FnClassGetType,
    TYPE_GET_OBJECT: FnTypeGetObject,
}

unsafe fn resolve(module: *mut c_void, name: &str) -> *mut c_void {
    let c = CString::new(name).unwrap();
    match GetProcAddress(module as _, c.as_ptr() as *const u8) {
        Some(p) => p as *mut c_void,
        None => std::ptr::null_mut(),
    }
}

/// Resolve all reflection exports from GameAssembly.dll. Returns false if the
/// essential ones are missing.
pub unsafe fn init() -> bool {
    let h = GetModuleHandleA(b"GameAssembly.dll\0".as_ptr());
    if h.is_null() {
        return false;
    }
    let m = h as *mut c_void;
    macro_rules! load {
        ($g:ident, $sym:literal) => {
            $g = core::mem::transmute(resolve(m, $sym));
        };
    }
    load!(CLASS_GET_FIELDS, "il2cpp_class_get_fields");
    load!(FIELD_GET_NAME, "il2cpp_field_get_name");
    load!(FIELD_GET_TYPE, "il2cpp_field_get_type");
    load!(FIELD_GET_OFFSET, "il2cpp_field_get_offset");
    load!(FIELD_GET_FLAGS, "il2cpp_field_get_flags");
    load!(FIELD_STATIC_GET_VALUE, "il2cpp_field_static_get_value");
    load!(TYPE_GET_TYPE, "il2cpp_type_get_type");
    load!(ARRAY_LENGTH, "il2cpp_array_length");
    load!(ARRAY_NEW, "il2cpp_array_new");
    load!(OBJECT_GET_CLASS, "il2cpp_object_get_class");
    load!(CLASS_GET_NAME, "il2cpp_class_get_name");
    load!(CLASS_GET_NAMESPACE, "il2cpp_class_get_namespace");
    load!(CLASS_GET_PARENT, "il2cpp_class_get_parent");
    load!(CLASS_GET_METHODS, "il2cpp_class_get_methods");
    load!(METHOD_GET_PARAM_COUNT, "il2cpp_method_get_param_count");
    load!(METHOD_GET_PARAM, "il2cpp_method_get_param");
    load!(METHOD_GET_NAME, "il2cpp_method_get_name");
    load!(CLASS_FROM_TYPE, "il2cpp_class_from_type");
    load!(CLASS_IS_ENUM, "il2cpp_class_is_enum");
    load!(CLASS_IS_VALUETYPE, "il2cpp_class_is_valuetype");
    load!(CLASS_VALUE_SIZE, "il2cpp_class_value_size");
    load!(CLASS_GET_ELEMENT_CLASS, "il2cpp_class_get_element_class");
    load!(IMAGE_GET_CLASS_COUNT, "il2cpp_image_get_class_count");
    load!(IMAGE_GET_CLASS, "il2cpp_image_get_class");
    load!(CLASS_FROM_NAME, "il2cpp_class_from_name");
    load!(DOMAIN_GET, "il2cpp_domain_get");
    load!(DOMAIN_GET_ASSEMBLIES, "il2cpp_domain_get_assemblies");
    load!(ASSEMBLY_GET_IMAGE, "il2cpp_assembly_get_image");
    load!(IMAGE_GET_NAME, "il2cpp_image_get_name");
    load!(THREAD_CURRENT, "il2cpp_thread_current");
    load!(THREAD_ATTACH, "il2cpp_thread_attach");
    load!(THREAD_DETACH, "il2cpp_thread_detach");
    load!(CLASS_GET_METHOD_FROM_NAME, "il2cpp_class_get_method_from_name");
    load!(METHOD_GET_FLAGS, "il2cpp_method_get_flags");
    load!(CLASS_GET_TYPE, "il2cpp_class_get_type");
    load!(TYPE_GET_OBJECT, "il2cpp_type_get_object");

    CLASS_GET_FIELDS.is_some()
        && CLASS_GET_METHODS.is_some()
        && IMAGE_GET_CLASS_COUNT.is_some()
        && IMAGE_GET_CLASS.is_some()
        && DOMAIN_GET_ASSEMBLIES.is_some()
        && ASSEMBLY_GET_IMAGE.is_some()
        && OBJECT_GET_CLASS.is_some()
        && CLASS_GET_METHOD_FROM_NAME.is_some()
}

/// The compiled native function pointer of a MethodInfo = *(MethodInfo + 0).
pub unsafe fn method_addr(method: *mut RawMethod) -> usize {
    if method.is_null() {
        0
    } else {
        *(method as *const usize)
    }
}

// ---------------------------------------------------------------------------
// Targeted by-name read helpers (Plan B).
//
// These replace the generic reflection serializer: instead of walking every
// field into JSON, we resolve only the named fields we need and read them.
// A `Val` is a managed value we can read named fields from — either a heap
// object (header present, fields at +offset) or an unboxed value-type struct
// inside a value-type array (no header, fields at +(offset - 0x10)).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Val {
    pub base: *mut u8,
    pub klass: *mut RawClass,
    pub is_struct: bool,
}

pub unsafe fn obj_class(obj: *mut RawObject) -> *mut RawClass {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    match OBJECT_GET_CLASS {
        Some(f) => f(obj),
        None => std::ptr::null_mut(),
    }
}

pub unsafe fn class_name(klass: *mut RawClass) -> String {
    if klass.is_null() {
        return String::new();
    }
    match CLASS_GET_NAME {
        Some(f) => {
            let p = f(klass);
            if p.is_null() {
                String::new()
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
        None => String::new(),
    }
}

/// Wrap a heap object as a `Val`. None if null.
pub unsafe fn val_of(obj: *mut RawObject) -> Option<Val> {
    if obj.is_null() {
        return None;
    }
    let klass = obj_class(obj);
    if klass.is_null() {
        return None;
    }
    Some(Val { base: obj as *mut u8, klass, is_struct: false })
}

/// Resolve a field's il2cpp offset by name, walking the class and its parents.
pub unsafe fn field_offset(klass: *mut RawClass, name: &str) -> Option<usize> {
    let (get_fields, get_fname, get_foff, get_parent) =
        (CLASS_GET_FIELDS?, FIELD_GET_NAME?, FIELD_GET_OFFSET?, CLASS_GET_PARENT?);
    let mut cur = klass;
    while !cur.is_null() {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let field = get_fields(cur, &mut iter);
            if field.is_null() {
                break;
            }
            let np = get_fname(field);
            if !np.is_null() {
                let n = CStr::from_ptr(np).to_string_lossy();
                if n == name {
                    return Some(get_foff(field));
                }
            }
        }
        cur = get_parent(cur);
    }
    None
}

/// Address of a named field within a `Val`, accounting for struct vs object.
unsafe fn field_addr(v: &Val, name: &str) -> Option<*mut u8> {
    let off = field_offset(v.klass, name)?;
    let adj = if v.is_struct { off.checked_sub(0x10)? } else { off };
    Some(v.base.add(adj))
}

pub unsafe fn read_i32(v: &Val, name: &str) -> Option<i32> {
    Some(*(field_addr(v, name)? as *const i32))
}
pub unsafe fn read_i64(v: &Val, name: &str) -> Option<i64> {
    Some(*(field_addr(v, name)? as *const i64))
}
pub unsafe fn read_f32(v: &Val, name: &str) -> Option<f32> {
    Some(*(field_addr(v, name)? as *const f32))
}
pub unsafe fn read_bool(v: &Val, name: &str) -> Option<bool> {
    Some(*(field_addr(v, name)? as *const bool))
}

/// Read a reference-typed field (array / string / nested object) as a `Val`.
pub unsafe fn read_ref(v: &Val, name: &str) -> Option<Val> {
    let p = *(field_addr(v, name)? as *const *mut RawObject);
    val_of(p)
}

/// Raw object pointer of a reference field (for arrays/strings we treat manually).
pub unsafe fn read_ref_ptr(v: &Val, name: &str) -> Option<*mut RawObject> {
    let p = *(field_addr(v, name)? as *const *mut RawObject);
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

/// Decode a managed System.String object to a Rust String.
pub unsafe fn read_string(obj: *mut RawObject) -> Option<String> {
    if obj.is_null() {
        return None;
    }
    let len = *((obj as *mut u8).add(0x10) as *const i32);
    if len <= 0 {
        return Some(String::new());
    }
    let chars = (obj as *mut u8).add(0x14) as *const u16;
    if chars.is_null() {
        return None;
    }
    let s = std::slice::from_raw_parts(chars, len as usize);
    Some(String::from_utf16_lossy(s))
}

pub unsafe fn array_len(arr: *mut RawObject) -> usize {
    if arr.is_null() {
        return 0;
    }
    match ARRAY_LENGTH {
        Some(f) => f(arr) as usize,
        None => 0,
    }
}

/// Element `i` of a managed array as a `Val`. Handles both reference-element
/// arrays (data is a pointer table at obj+32) and value-type-element arrays
/// (inline structs of `class_value_size` stride). None if null/out of range.
pub unsafe fn array_elem(arr: *mut RawObject, i: usize) -> Option<Val> {
    if arr.is_null() || i >= array_len(arr) {
        return None;
    }
    let klass = obj_class(arr);
    if klass.is_null() {
        return None;
    }
    let elem = CLASS_GET_ELEMENT_CLASS?(klass);
    if elem.is_null() {
        return None;
    }
    let data = (arr as *mut u8).add(32);
    if CLASS_IS_VALUETYPE?(elem) {
        let mut align: u32 = 0;
        let stride = CLASS_VALUE_SIZE?(elem, &mut align) as usize;
        if stride == 0 {
            return None;
        }
        Some(Val { base: data.add(i * stride), klass: elem, is_struct: true })
    } else {
        let p = *(data as *mut *mut RawObject).add(i);
        val_of(p)
    }
}
