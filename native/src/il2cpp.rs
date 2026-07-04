//! Heaven Plan B — native IL2CPP binding layer.
//!
//! Replaces frida-il2cpp-bridge: resolve the `il2cpp_*` C API exported by
//! GameAssembly.dll via GetProcAddress, then walk domain → assemblies → image →
//! class → method/field entirely in-process. No Frida, no Python.
//!
//! FOOTGUN: every Heaven-owned thread that touches managed memory MUST call
//! `thread_attach(domain())` once, or IL2CPP crashes. The game's render thread
//! is already attached; our loader/worker thread is not.

#![allow(dead_code)]

use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use retour::RawDetour;

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

// ── Opaque IL2CPP pointer aliases (we never deref the structs directly, except
//    MethodInfo+0 = methodPointer, handled in `method_pointer`). ─────────────
pub type Domain = *mut c_void;
pub type Thread = *mut c_void;
pub type Assembly = *mut c_void;
pub type Image = *mut c_void;
pub type Class = *mut c_void;
pub type Method = *const c_void; // MethodInfo*
pub type Field = *mut c_void;
pub type Object = *mut c_void;

// ── Exported function signatures (C ABI). ───────────────────────────────────
type FnDomainGet = unsafe extern "C" fn() -> Domain;
type FnThreadAttach = unsafe extern "C" fn(Domain) -> Thread;
type FnThreadDetach = unsafe extern "C" fn(Thread);
type FnDomainGetAssemblies = unsafe extern "C" fn(Domain, *mut usize) -> *const Assembly;
type FnAssemblyGetImage = unsafe extern "C" fn(Assembly) -> Image;
type FnImageGetName = unsafe extern "C" fn(Image) -> *const c_char;
type FnClassFromName = unsafe extern "C" fn(Image, *const c_char, *const c_char) -> Class;
type FnClassGetMethodFromName = unsafe extern "C" fn(Class, *const c_char, i32) -> Method;
type FnClassGetFieldFromName = unsafe extern "C" fn(Class, *const c_char) -> Field;
type FnFieldGetOffset = unsafe extern "C" fn(Field) -> usize;
type FnRuntimeInvoke =
    unsafe extern "C" fn(Method, Object, *mut *mut c_void, *mut Object) -> Object;
type FnObjectNew = unsafe extern "C" fn(Class) -> Object;
type FnObjectGetClass = unsafe extern "C" fn(Object) -> Class;
type FnStringChars = unsafe extern "C" fn(Object) -> *const u16;
type FnStringLength = unsafe extern "C" fn(Object) -> i32;
type FnClassGetName = unsafe extern "C" fn(Class) -> *const c_char;
type FnClassGetStaticFieldData = unsafe extern "C" fn(Class) -> *mut c_void;
type FnClassGetType = unsafe extern "C" fn(Class) -> *mut c_void;
type FnTypeGetObject = unsafe extern "C" fn(*mut c_void) -> Object;
type FnClassGetMethods = unsafe extern "C" fn(Class, *mut *mut c_void) -> Method;
type FnMethodGetName = unsafe extern "C" fn(Method) -> *const c_char;
type FnMethodGetParamCount = unsafe extern "C" fn(Method) -> u32;
type FnMethodGetParam = unsafe extern "C" fn(Method, u32) -> *mut c_void; // Il2CppType*
type FnTypeGetName = unsafe extern "C" fn(*mut c_void) -> *mut c_char;
type FnStringNew = unsafe extern "C" fn(*const c_char) -> Object;
type FnMethodGetFlags = unsafe extern "C" fn(Method, *mut u32) -> u32;
type FnClassGetNestedTypes = unsafe extern "C" fn(Class, *mut *mut c_void) -> Class;
// Allocate a managed array of `len` elements of `element_class`. Returns Il2CppArray* (data @0x20).
type FnArrayNew = unsafe extern "C" fn(Class, usize) -> Object;
// GC write barrier: store `value` into `*field` of managed object `obj` (keeps the ref reachable).
type FnGcWbarrierSetField = unsafe extern "C" fn(Object, *mut *mut c_void, *mut c_void);

/// Resolved bundle of the IL2CPP exports we use. Built once in `init`.
struct Api {
    domain_get: FnDomainGet,
    thread_attach: FnThreadAttach,
    thread_detach: FnThreadDetach,
    domain_get_assemblies: FnDomainGetAssemblies,
    assembly_get_image: FnAssemblyGetImage,
    image_get_name: FnImageGetName,
    class_from_name: FnClassFromName,
    class_get_method_from_name: FnClassGetMethodFromName,
    class_get_field_from_name: FnClassGetFieldFromName,
    field_get_offset: FnFieldGetOffset,
    runtime_invoke: FnRuntimeInvoke,
    object_new: FnObjectNew,
    object_get_class: FnObjectGetClass,
    string_chars: FnStringChars,
    string_length: FnStringLength,
    class_get_name: FnClassGetName,
    class_get_static_field_data: FnClassGetStaticFieldData,
    class_get_type: FnClassGetType,
    type_get_object: FnTypeGetObject,
    class_get_methods: FnClassGetMethods,
    method_get_name: FnMethodGetName,
    method_get_param_count: FnMethodGetParamCount,
    method_get_param: FnMethodGetParam,
    type_get_name: FnTypeGetName,
    string_new: FnStringNew,
    method_get_flags: FnMethodGetFlags,
    class_get_nested_types: FnClassGetNestedTypes,
    array_new: FnArrayNew,
    gc_wbarrier_set_field: FnGcWbarrierSetField,
}
// SAFETY: the resolved code lives for the process lifetime; pointers are read-only.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Api> = OnceLock::new();

/// Get the loaded GameAssembly.dll module handle, or null if not yet loaded.
fn game_module() -> HMODULE {
    // "GameAssembly.dll" as a UTF-16, null-terminated wide string.
    let wide: Vec<u16> = "GameAssembly.dll\0".encode_utf16().collect();
    unsafe { GetModuleHandleW(wide.as_ptr()) }
}

/// True once GameAssembly.dll is present in the process.
pub fn game_loaded() -> bool {
    !game_module().is_null()
}

/// Resolve one exported symbol to a typed fn pointer. Returns None if absent.
unsafe fn resolve<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    // name must be a null-terminated byte string.
    debug_assert_eq!(*name.last().unwrap(), 0, "resolve() name must be NUL-terminated");
    let proc = GetProcAddress(module, name.as_ptr());
    proc.map(|p| std::mem::transmute_copy::<_, T>(&p))
}

macro_rules! load {
    ($m:expr, $sym:literal) => {
        match resolve($m, concat!($sym, "\0").as_bytes()) {
            Some(f) => f,
            None => return Err(concat!("missing export: ", $sym)),
        }
    };
}

/// Resolve the IL2CPP API. Call once GameAssembly.dll is loaded. Idempotent.
pub fn init() -> Result<(), &'static str> {
    if API.get().is_some() {
        return Ok(());
    }
    let m = game_module();
    if m.is_null() {
        return Err("GameAssembly.dll not loaded yet");
    }
    let api = unsafe {
        Api {
            domain_get: load!(m, "il2cpp_domain_get"),
            thread_attach: load!(m, "il2cpp_thread_attach"),
            thread_detach: load!(m, "il2cpp_thread_detach"),
            domain_get_assemblies: load!(m, "il2cpp_domain_get_assemblies"),
            assembly_get_image: load!(m, "il2cpp_assembly_get_image"),
            image_get_name: load!(m, "il2cpp_image_get_name"),
            class_from_name: load!(m, "il2cpp_class_from_name"),
            class_get_method_from_name: load!(m, "il2cpp_class_get_method_from_name"),
            class_get_field_from_name: load!(m, "il2cpp_class_get_field_from_name"),
            field_get_offset: load!(m, "il2cpp_field_get_offset"),
            runtime_invoke: load!(m, "il2cpp_runtime_invoke"),
            object_new: load!(m, "il2cpp_object_new"),
            object_get_class: load!(m, "il2cpp_object_get_class"),
            string_chars: load!(m, "il2cpp_string_chars"),
            string_length: load!(m, "il2cpp_string_length"),
            class_get_name: load!(m, "il2cpp_class_get_name"),
            class_get_static_field_data: load!(m, "il2cpp_class_get_static_field_data"),
            class_get_type: load!(m, "il2cpp_class_get_type"),
            type_get_object: load!(m, "il2cpp_type_get_object"),
            class_get_methods: load!(m, "il2cpp_class_get_methods"),
            method_get_name: load!(m, "il2cpp_method_get_name"),
            method_get_param_count: load!(m, "il2cpp_method_get_param_count"),
            method_get_param: load!(m, "il2cpp_method_get_param"),
            type_get_name: load!(m, "il2cpp_type_get_name"),
            string_new: load!(m, "il2cpp_string_new"),
            method_get_flags: load!(m, "il2cpp_method_get_flags"),
            class_get_nested_types: load!(m, "il2cpp_class_get_nested_types"),
            array_new: load!(m, "il2cpp_array_new"),
            gc_wbarrier_set_field: load!(m, "il2cpp_gc_wbarrier_set_field"),
        }
    };
    let _ = API.set(api);
    Ok(())
}

fn api() -> &'static Api {
    API.get().expect("il2cpp::init() not called / failed")
}

/// True once the IL2CPP API has been resolved (init succeeded). Cheap guard for callers
/// that may run before the runtime is up — they must bail instead of panicking in `api()`.
pub fn ready() -> bool {
    API.get().is_some()
}

// ── High-level helpers ──────────────────────────────────────────────────────

pub fn domain() -> Domain {
    unsafe { (api().domain_get)() }
}

/// Attach the CURRENT thread to the IL2CPP domain. Call once per Heaven thread
/// before touching managed memory.
pub fn attach_current_thread() -> Thread {
    unsafe { (api().thread_attach)(domain()) }
}

/// Detach a previously-attached thread (unregister it from the GC). Do this
/// before the thread exits, so it isn't left dangling for the shutdown GC.
pub fn detach_thread(th: Thread) {
    if !th.is_null() {
        unsafe { (api().thread_detach)(th) }
    }
}

/// Find a class by "Namespace.Name" (or bare "Name") across every loaded
/// assembly image. Returns null if not found.
pub fn class(full_name: &str) -> Class {
    let (ns, name) = match full_name.rfind('.') {
        Some(i) => (&full_name[..i], &full_name[i + 1..]),
        None => ("", full_name),
    };
    let ns_c = to_cstring(ns);
    let name_c = to_cstring(name);
    unsafe {
        let dom = domain();
        let mut count: usize = 0;
        let asms = (api().domain_get_assemblies)(dom, &mut count);
        if asms.is_null() {
            return std::ptr::null_mut();
        }
        for i in 0..count {
            let asm = *asms.add(i);
            if asm.is_null() {
                continue;
            }
            let img = (api().assembly_get_image)(asm);
            if img.is_null() {
                continue;
            }
            let k = (api().class_from_name)(img, ns_c.as_ptr(), name_c.as_ptr());
            if !k.is_null() {
                return k;
            }
        }
        std::ptr::null_mut()
    }
}

/// Look up a NESTED class by its outer class full name + the nested simple name.
/// `class_from_name` can't see nested types, so we enumerate the outer's nested types.
pub fn nested_class(outer_full: &str, nested_name: &str) -> Class {
    let outer = class(outer_full);
    if outer.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let k = (api().class_get_nested_types)(outer, &mut iter);
            if k.is_null() {
                break;
            }
            if class_name(k) == nested_name {
                return k;
            }
        }
    }
    std::ptr::null_mut()
}

/// Every nested type of `klass` as (simple_name, Class). Empty when the class has none or
/// introspection is unavailable. Nested types are invisible to image class enumeration, so
/// the scanner needs this to reach e.g. `WorkDataUtil.RacePresetData` or `…Item.ItemData`.
pub fn nested_types(klass: Class) -> Vec<(String, Class)> {
    let mut out = Vec::new();
    if klass.is_null() {
        return out;
    }
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let k = (api().class_get_nested_types)(klass, &mut iter);
            if k.is_null() {
                break;
            }
            out.push((class_name(k), k));
        }
    }
    out
}

/// Look up a method on a class by name + argument count (-1 = any count).
pub fn method(klass: Class, name: &str, argc: i32) -> Method {
    if klass.is_null() {
        return std::ptr::null();
    }
    let n = to_cstring(name);
    unsafe { (api().class_get_method_from_name)(klass, n.as_ptr(), argc) }
}

/// The compiled native entry point of a method = MethodInfo->methodPointer,
/// which is the first pointer-sized field of the MethodInfo struct. This is the
/// address we detour (retour) and the address we cast to call the original.
pub fn method_pointer(m: Method) -> *const c_void {
    if m.is_null() {
        return std::ptr::null();
    }
    unsafe { *(m as *const *const c_void) }
}

/// Heuristic: is this compiled code already hooked (its prologue is a jmp)? A real IL2CPP
/// method prologue never starts with a jump, so `E9`/`EB` (rel jmp) or `FF 25` (abs jmp) means
/// another overlay detoured it first. Used to avoid double-detouring the same address.
pub unsafe fn is_detoured(code: *const c_void) -> bool {
    if code.is_null() {
        return false;
    }
    let p = code as *const u8;
    let b0 = std::ptr::read_unaligned(p);
    b0 == 0xE9 || b0 == 0xEB || (b0 == 0xFF && std::ptr::read_unaligned(p.add(1)) == 0x25)
}

/// Resolve `klass.name` (argc args), detour its compiled code to `detour`, and stash the
/// trampoline (the original entry point) in `tramp` + keep the detour alive in `keep`.
/// The detour forwards the trailing hidden MethodInfo* it receives, so callers don't need
/// it separately. Returns Err with the method name on any miss.
pub unsafe fn hook_method(
    klass: Class,
    name: &str,
    argc: i32,
    detour: *const (),
    tramp: &AtomicUsize,
    keep: &OnceLock<RawDetour>,
) -> Result<(), String> {
    let m = method(klass, name, argc);
    if m.is_null() {
        return Err(format!("{name}: not found"));
    }
    let target = method_pointer(m);
    if target.is_null() {
        return Err(format!("{name}: null ptr"));
    }
    // If the method's prologue is already a jmp, another overlay has detoured it first.
    // Double-detouring the same address with a different hook engine corrupts the
    // trampolines and freezes the game — so we yield this hook instead of stacking on it.
    if is_detoured(target) {
        // Another mod (e.g. a co-resident loader) detoured this method first — yield to avoid
        // corrupting trampolines. The arbiter records the cede so the build can report it.
        #[cfg(feature = "hachimi")]
        crate::arbiter::record(
            &format!("{}.{}", class_name(klass), name),
            crate::arbiter::Owner::External,
        );
        return Err(format!("{name}: already detoured (skipped)"));
    }
    let d = RawDetour::new(target as *const (), detour).map_err(|e| format!("{name}: {e}"))?;
    d.enable().map_err(|e| format!("{name} enable: {e}"))?;
    tramp.store(d.trampoline() as *const () as usize, Ordering::Relaxed);
    let _ = keep.set(d);
    #[cfg(feature = "hachimi")]
    crate::arbiter::record(
        &format!("{}.{}", class_name(klass), name),
        crate::arbiter::Owner::Heaven,
    );
    Ok(())
}

/// Byte offset of an instance field on a class (for the ObscuredInt reads etc.).
pub fn field_offset(klass: Class, name: &str) -> Option<usize> {
    if klass.is_null() {
        return None;
    }
    let n = to_cstring(name);
    unsafe {
        let f = (api().class_get_field_from_name)(klass, n.as_ptr());
        if f.is_null() {
            None
        } else {
            Some((api().field_get_offset)(f))
        }
    }
}

/// Raw address of a STATIC field's storage (base of the class's static data + the field's
/// offset). Lets us read/write a static field directly (e.g. DOTween.timeScale) with a
/// plain memory store — no runtime call, safe from any thread. Null if not found.
pub fn static_field_addr(klass: Class, name: &str) -> *mut c_void {
    if klass.is_null() {
        return std::ptr::null_mut();
    }
    let n = to_cstring(name);
    unsafe {
        let f = (api().class_get_field_from_name)(klass, n.as_ptr());
        if f.is_null() {
            return std::ptr::null_mut();
        }
        let base = (api().class_get_static_field_data)(klass);
        if base.is_null() {
            return std::ptr::null_mut();
        }
        let off = (api().field_get_offset)(f);
        (base as *mut u8).add(off) as *mut c_void
    }
}

/// The `System.Type` managed object for a class (e.g. to pass `typeof(Component)` as a
/// method argument). Null if the class is null.
pub fn type_object(klass: Class) -> Object {
    if klass.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let t = (api().class_get_type)(klass);
        if t.is_null() {
            return std::ptr::null_mut();
        }
        (api().type_get_object)(t)
    }
}

/// List all methods of a class as "name/paramCount" strings (for RE/diagnostics).
pub fn class_methods(klass: Class) -> Vec<String> {
    let mut out = Vec::new();
    if klass.is_null() {
        return out;
    }
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let m = (api().class_get_methods)(klass, &mut iter);
            if m.is_null() {
                break;
            }
            let np = (api().method_get_name)(m);
            if np.is_null() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(np).to_string_lossy().into_owned();
            let pc = (api().method_get_param_count)(m);
            out.push(format!("{name}/{pc}"));
        }
    }
    out
}

/// Parameter type names of all overloads of `name` on `klass` (e.g. ["String, Boolean", "Int32"]).
/// One string per overload matching the name. For RE'ing a method's exact signature.
pub fn method_param_types(klass: Class, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if klass.is_null() {
        return out;
    }
    let target = name;
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let m = (api().class_get_methods)(klass, &mut iter);
            if m.is_null() {
                break;
            }
            let np = (api().method_get_name)(m);
            if np.is_null() {
                continue;
            }
            let nm = std::ffi::CStr::from_ptr(np).to_string_lossy();
            if nm != target {
                continue;
            }
            let pc = (api().method_get_param_count)(m);
            let mut parts = Vec::new();
            for i in 0..pc {
                let t = (api().method_get_param)(m, i);
                if t.is_null() {
                    parts.push("?".to_string());
                    continue;
                }
                let tn = (api().type_get_name)(t);
                if tn.is_null() {
                    parts.push("?".to_string());
                } else {
                    parts.push(std::ffi::CStr::from_ptr(tn).to_string_lossy().into_owned());
                }
            }
            out.push(parts.join(", "));
        }
    }
    out
}

// ── class enumeration (RE/scan tooling) ──────────────────────────────────────
// `il2cpp_image_get_class_count` / `il2cpp_image_get_class` enumerate every type in an
// image. Resolved lazily (like resolve_icall) so a runtime that lacks the exports never
// breaks init — the scan feature just reports "unavailable".
type FnImageGetClassCount = unsafe extern "C" fn(Image) -> usize;
type FnImageGetClass = unsafe extern "C" fn(Image, usize) -> Class;
type FnClassGetNamespace = unsafe extern "C" fn(Class) -> *const c_char;
static CLASS_ENUM: OnceLock<Option<(FnImageGetClassCount, FnImageGetClass, FnClassGetNamespace)>> =
    OnceLock::new();

fn class_enum_api() -> Option<&'static (FnImageGetClassCount, FnImageGetClass, FnClassGetNamespace)> {
    CLASS_ENUM
        .get_or_init(|| {
            let m = game_module();
            if m.is_null() {
                return None;
            }
            unsafe {
                let count = resolve::<FnImageGetClassCount>(m, b"il2cpp_image_get_class_count\0")?;
                let get = resolve::<FnImageGetClass>(m, b"il2cpp_image_get_class\0")?;
                let ns = resolve::<FnClassGetNamespace>(m, b"il2cpp_class_get_namespace\0")?;
                Some((count, get, ns))
            }
        })
        .as_ref()
}

/// Namespace-qualified name of a class ("Namespace.Name", or bare "Name").
pub fn class_full_name(klass: Class) -> String {
    if klass.is_null() {
        return String::new();
    }
    let name = class_name(klass);
    match class_enum_api() {
        Some((_, _, ns_fn)) => {
            let ns = unsafe { cstr_to_string((ns_fn)(klass)) };
            if ns.is_empty() {
                name
            } else {
                format!("{ns}.{name}")
            }
        }
        None => name,
    }
}

/// Every loaded class whose SIMPLE NAME contains `substr` (case-insensitive), as
/// (full_name, Class) pairs. Empty if the enumeration exports are unavailable.
/// Metadata-only — safe from any attached thread.
pub fn find_classes(substr: &str) -> Vec<(String, Class)> {
    let mut out = Vec::new();
    let Some((count_fn, get_fn, _)) = class_enum_api() else {
        return out;
    };
    let needle = substr.to_lowercase();
    unsafe {
        let dom = domain();
        let mut n_asm: usize = 0;
        let asms = (api().domain_get_assemblies)(dom, &mut n_asm);
        if asms.is_null() {
            return out;
        }
        for i in 0..n_asm {
            let asm = *asms.add(i);
            if asm.is_null() {
                continue;
            }
            let img = (api().assembly_get_image)(asm);
            if img.is_null() {
                continue;
            }
            let n = (count_fn)(img);
            for j in 0..n {
                let k = (get_fn)(img, j);
                if k.is_null() {
                    continue;
                }
                if class_name(k).to_lowercase().contains(&needle) {
                    out.push((class_full_name(k), k));
                }
            }
        }
    }
    out
}

// ── field/parent introspection (RE/scan tooling) ─────────────────────────────
// Lazily resolved like the class enumeration: absent exports just disable the feature.
type FnClassGetFields = unsafe extern "C" fn(Class, *mut *mut c_void) -> Field;
type FnFieldGetName = unsafe extern "C" fn(Field) -> *const c_char;
type FnFieldGetTypeFn = unsafe extern "C" fn(Field) -> *mut c_void;
type FnClassGetParent = unsafe extern "C" fn(Class) -> Class;
static INTROSPECT: OnceLock<Option<(FnClassGetFields, FnFieldGetName, FnFieldGetTypeFn, FnClassGetParent)>> =
    OnceLock::new();

fn introspect_api() -> Option<&'static (FnClassGetFields, FnFieldGetName, FnFieldGetTypeFn, FnClassGetParent)> {
    INTROSPECT
        .get_or_init(|| {
            let m = game_module();
            if m.is_null() {
                return None;
            }
            unsafe {
                let fields = resolve::<FnClassGetFields>(m, b"il2cpp_class_get_fields\0")?;
                let fname = resolve::<FnFieldGetName>(m, b"il2cpp_field_get_name\0")?;
                let ftype = resolve::<FnFieldGetTypeFn>(m, b"il2cpp_field_get_type\0")?;
                let parent = resolve::<FnClassGetParent>(m, b"il2cpp_class_get_parent\0")?;
                Some((fields, fname, ftype, parent))
            }
        })
        .as_ref()
}

/// All instance/static fields of a class as (name, offset, type_name). Empty when the
/// introspection exports are unavailable. For RE/diagnostics.
pub fn class_fields(klass: Class) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    if klass.is_null() {
        return out;
    }
    let Some((fields_fn, fname_fn, ftype_fn, _)) = introspect_api() else {
        return out;
    };
    unsafe {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let f = (fields_fn)(klass, &mut iter);
            if f.is_null() {
                break;
            }
            let name = cstr_to_string((fname_fn)(f));
            let off = (api().field_get_offset)(f);
            let t = (ftype_fn)(f);
            let tn = if t.is_null() {
                "?".to_string()
            } else {
                let p = (api().type_get_name)(t);
                if p.is_null() { "?".to_string() } else { cstr_to_string(p) }
            };
            out.push((name, off, tn));
        }
    }
    out
}

/// Immediate base class (null for System.Object / when introspection is unavailable).
pub fn class_parent(klass: Class) -> Class {
    if klass.is_null() {
        return std::ptr::null_mut();
    }
    match introspect_api() {
        Some((_, _, _, parent_fn)) => unsafe { (parent_fn)(klass) },
        None => std::ptr::null_mut(),
    }
}

// `il2cpp_resolve_icall(const char* signature)` returns the native function backing an
// [InternalCall] method. The freecam hooks these (Transform set_position_Injected,
// Internal_LookAt_Injected). Resolved lazily so a missing export never breaks init.
type FnResolveIcall = unsafe extern "C" fn(*const c_char) -> *const c_void;
static RESOLVE_ICALL: OnceLock<Option<FnResolveIcall>> = OnceLock::new();

/// Resolve an engine internal call by full signature, e.g.
/// `"UnityEngine.Transform::set_position_Injected(UnityEngine.Vector3&)"`. Null if absent.
pub fn resolve_icall(signature: &str) -> *const c_void {
    let f = RESOLVE_ICALL.get_or_init(|| {
        let m = game_module();
        if m.is_null() {
            return None;
        }
        unsafe { resolve::<FnResolveIcall>(m, b"il2cpp_resolve_icall\0") }
    });
    match f {
        Some(func) => {
            let sig = to_cstring(signature);
            unsafe { func(sig.as_ptr()) }
        }
        None => std::ptr::null(),
    }
}

/// Allocate a new managed object of `klass` (e.g. a PointerEventData).
pub fn object_new(klass: Class) -> Object {
    if klass.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { (api().object_new)(klass) }
}

/// Allocate a managed array of `len` elements of `element_class`. Element storage starts at
/// offset 0x20; for reference-type elements each slot is an 8-byte pointer. Returns null on error.
pub fn array_new(element_class: Class, len: usize) -> Object {
    if element_class.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { (api().array_new)(element_class, len) }
}

/// GC write barrier: store reference `value` into the field at `field_addr` (which lives inside the
/// managed object `owner`). Use this whenever writing a managed reference into managed memory from
/// native code, so the GC keeps `value` reachable and doesn't collect it mid-operation.
pub unsafe fn wbarrier_set(owner: Object, field_addr: *mut c_void, value: Object) {
    (api().gc_wbarrier_set_field)(owner, field_addr as *mut *mut c_void, value);
}

/// Invoke a method via the runtime (boxed args). For perf-critical hot paths,
/// prefer casting `method_pointer` to a typed fn and calling directly.
pub unsafe fn runtime_invoke(m: Method, this: Object, args: &mut [*mut c_void]) -> Object {
    let mut exc: Object = std::ptr::null_mut();
    let argp = if args.is_empty() { std::ptr::null_mut() } else { args.as_mut_ptr() };
    (api().runtime_invoke)(m, this, argp, &mut exc)
}

/// True if a method is static (no implicit `this` first arg). METHOD_ATTRIBUTE_STATIC = 0x10.
pub fn method_is_static(m: Method) -> bool {
    if m.is_null() {
        return false;
    }
    unsafe { ((api().method_get_flags)(m, std::ptr::null_mut()) & 0x10) != 0 }
}

/// Allocate a managed System.String from a Rust &str (UTF-8 → IL2CPP string).
/// The GC owns the result; only call from an attached thread. Null on failure.
pub fn new_string(s: &str) -> Object {
    let c = to_cstring(s);
    unsafe { (api().string_new)(c.as_ptr()) }
}

/// Read a managed System.String into a Rust String (UTF-16 → UTF-8).
pub fn read_string(s: Object) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe {
        let len = (api().string_length)(s);
        if len <= 0 {
            return String::new();
        }
        let chars = (api().string_chars)(s);
        if chars.is_null() {
            return String::new();
        }
        let slice = std::slice::from_raw_parts(chars, len as usize);
        String::from_utf16_lossy(slice)
    }
}

/// Runtime class of a managed object (null if obj is null). Use this to resolve methods on NESTED
/// types (`il2cpp_class_from_name` can't find `Outer.Nested` by namespace) — get the class from a
/// live instance instead.
pub fn object_class(obj: Object) -> Class {
    if obj.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { (api().object_get_class)(obj) }
}

/// Runtime class name of a managed object ("" if null). For type checks.
pub fn object_class_name(obj: Object) -> String {
    if obj.is_null() {
        return String::new();
    }
    unsafe {
        let k = (api().object_get_class)(obj);
        class_name(k)
    }
}

/// Class name as a Rust string (for runtime type checks).
pub fn class_name(klass: Class) -> String {
    if klass.is_null() {
        return String::new();
    }
    unsafe {
        let p = (api().class_get_name)(klass);
        cstr_to_string(p)
    }
}

// ── small utils ──────────────────────────────────────────────────────────────
fn to_cstring(s: &str) -> Vec<c_char> {
    let mut v: Vec<c_char> = s.bytes().map(|b| b as c_char).collect();
    v.push(0);
    v
}
unsafe fn cstr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}
