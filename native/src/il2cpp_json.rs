//! Heaven — generic IL2CPP managed-object → `serde_json::Value` reflection walker.
//!
//! A general-purpose "dump any managed object to JSON" engine: it walks fields (this class
//! + base classes), value-types, arrays (with primitive fast paths), enums (member-name
//! resolution) and the numeric `Obscured*` value-types (XOR decrypt). It knows nothing about
//! races — `race_export` (race/Team-Trials dumps) and `umas` (veteran/census dumps) call
//! `convert_object` on top of it. Extracted from race_export so the serializer lives in one
//! clearly-named place instead of inside a feature file.

use core::ffi::{c_void, CStr};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Number, Value};

use crate::htt_il2cpp as h;

const MAX_DEPTH: usize = 60;
const MAX_ARRAY: u32 = 8192;

fn num_f64(v: f64) -> Value {
    Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null)
}

// ── enum member-name cache (keyed by class pointer) ──────────────────────────
fn enum_cache() -> &'static Mutex<HashMap<usize, Vec<(String, i64)>>> {
    static C: OnceLock<Mutex<HashMap<usize, Vec<(String, i64)>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

unsafe fn class_name(klass: *mut c_void) -> String {
    if klass.is_null() {
        return String::new();
    }
    match h::CLASS_GET_NAME {
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

unsafe fn resolve_enum(addr: *mut u8, klass: *mut c_void) -> Value {
    let cur = *(addr as *const i32) as i64;
    let key = klass as usize;
    if let Ok(mut cache) = enum_cache().lock() {
        let entry = cache.entry(key).or_insert_with(|| {
            let mut out = Vec::new();
            let (get_fields, get_name, get_flags, sget) =
                match (h::CLASS_GET_FIELDS, h::FIELD_GET_NAME, h::FIELD_GET_FLAGS, h::FIELD_STATIC_GET_VALUE) {
                    (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
                    _ => return out,
                };
            let mut iter: *mut c_void = std::ptr::null_mut();
            loop {
                let field = get_fields(klass, &mut iter);
                if field.is_null() {
                    break;
                }
                let flags = get_flags(field);
                if (flags & h::FIELD_ATTRIBUTE_STATIC) != 0 && (flags & h::FIELD_ATTRIBUTE_LITERAL) != 0 {
                    let mut buf: i32 = 0;
                    sget(field, &mut buf as *mut i32 as *mut c_void);
                    let np = get_name(field);
                    if !np.is_null() {
                        out.push((CStr::from_ptr(np).to_string_lossy().into_owned(), buf as i64));
                    }
                }
            }
            out
        });
        for (name, val) in entry.iter() {
            if *val == cur {
                return Value::String(name.clone());
            }
        }
    }
    Value::Number(Number::from(cur))
}

/// Decrypt the 5 numeric `Obscured*` value types (XOR of hidden ^ key). Other
/// Obscured kinds (e.g. ObscuredString) fall through to a normal struct walk.
unsafe fn decrypt_obscured(base: *mut u8, klass: *mut c_void, name: &str) -> Option<Value> {
    let (get_fields, get_name, get_flags, get_off) =
        (h::CLASS_GET_FIELDS?, h::FIELD_GET_NAME?, h::FIELD_GET_FLAGS?, h::FIELD_GET_OFFSET?);
    let mut hidden_off: Option<usize> = None;
    let mut key_off: Option<usize> = None;
    let mut iter: *mut c_void = std::ptr::null_mut();
    loop {
        let field = get_fields(klass, &mut iter);
        if field.is_null() {
            break;
        }
        if (get_flags(field) & h::FIELD_ATTRIBUTE_STATIC) != 0 {
            continue;
        }
        let off = get_off(field);
        if off < 0x10 {
            continue;
        }
        let np = get_name(field);
        if np.is_null() {
            continue;
        }
        match CStr::from_ptr(np).to_string_lossy().as_ref() {
            "hiddenValue" => hidden_off = Some(off - 0x10),
            "currentCryptoKey" => key_off = Some(off - 0x10),
            _ => {}
        }
    }
    let (h_off, k_off) = (hidden_off?, key_off?);
    match name {
        "ObscuredInt" => {
            let hv = *(base.add(h_off) as *const i32);
            let kv = *(base.add(k_off) as *const i32);
            Some(Value::Number(Number::from(hv ^ kv)))
        }
        "ObscuredLong" => {
            let hv = *(base.add(h_off) as *const i64);
            let kv = *(base.add(k_off) as *const i64);
            Some(Value::Number(Number::from(hv ^ kv)))
        }
        "ObscuredBool" => {
            let hv = *(base.add(h_off) as *const i32);
            let kv = *(base.add(k_off) as *const i32);
            Some(Value::Bool((hv ^ kv) != 0))
        }
        "ObscuredFloat" => {
            let hv = *(base.add(h_off) as *const u32);
            let kv = *(base.add(k_off) as *const u32);
            Some(num_f64(f32::from_bits(hv ^ kv) as f64))
        }
        "ObscuredDouble" => {
            let hv = *(base.add(h_off) as *const u64);
            let kv = *(base.add(k_off) as *const u64);
            Some(num_f64(f64::from_bits(hv ^ kv)))
        }
        _ => None,
    }
}

unsafe fn read_value(
    addr: *mut u8,
    te: i32,
    ftype: *mut c_void,
    depth: usize,
    visited: &mut HashSet<usize>,
) -> Value {
    match te {
        0x02 => Value::Bool(*(addr as *const u8) != 0),                 // bool
        0x03 => Value::Number(Number::from(*(addr as *const u16))),     // char
        0x04 => Value::Number(Number::from(*(addr as *const i8) as i64)),
        0x05 => Value::Number(Number::from(*(addr as *const u8) as i64)),
        0x06 => Value::Number(Number::from(*(addr as *const i16) as i64)),
        0x07 => Value::Number(Number::from(*(addr as *const u16) as i64)),
        0x08 => Value::Number(Number::from(*(addr as *const i32) as i64)),
        0x09 => Value::Number(Number::from(*(addr as *const u32) as i64)),
        0x0A | 0x18 => Value::Number(Number::from(*(addr as *const i64))),
        0x0B | 0x19 => Value::Number(Number::from(*(addr as *const u64))),
        0x0C => num_f64(*(addr as *const f32) as f64),                  // r4
        0x0D => num_f64(*(addr as *const f64)),                        // r8
        // string / class / object / arrays / generic-inst → reference, recurse
        0x0E | 0x12 | 0x14 | 0x15 | 0x1C | 0x1D => {
            let p = *(addr as *const *mut c_void);
            convert_object(p, depth + 1, visited)
        }
        0x11 => read_valuetype(addr, ftype, depth, visited),           // value type
        _ => Value::Null,
    }
}

unsafe fn read_valuetype(
    addr: *mut u8,
    ftype: *mut c_void,
    depth: usize,
    visited: &mut HashSet<usize>,
) -> Value {
    if ftype.is_null() {
        return Value::Null;
    }
    let klass = match h::CLASS_FROM_TYPE {
        Some(f) => f(ftype),
        None => return Value::Null,
    };
    if klass.is_null() {
        return Value::Null;
    }
    let name = class_name(klass);
    if name.starts_with("Obscured") {
        if let Some(v) = decrypt_obscured(addr, klass, &name) {
            return v;
        }
    }
    if let Some(is_enum) = h::CLASS_IS_ENUM {
        if is_enum(klass) {
            return resolve_enum(addr, klass);
        }
    }
    convert_struct(addr, klass, depth + 1, visited)
}

/// Walk an inline value-type struct (fields at base + offset - 0x10).
unsafe fn convert_struct(
    base: *mut u8,
    klass: *mut c_void,
    depth: usize,
    visited: &mut HashSet<usize>,
) -> Value {
    if depth > MAX_DEPTH {
        return Value::String("<max depth>".into());
    }
    let (get_fields, get_name, get_flags, get_off, get_type, type_te) = match (
        h::CLASS_GET_FIELDS,
        h::FIELD_GET_NAME,
        h::FIELD_GET_FLAGS,
        h::FIELD_GET_OFFSET,
        h::FIELD_GET_TYPE,
        h::TYPE_GET_TYPE,
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f)) => (a, b, c, d, e, f),
        _ => return Value::Null,
    };
    let mut map = Map::new();
    let mut iter: *mut c_void = std::ptr::null_mut();
    loop {
        let field = get_fields(klass, &mut iter);
        if field.is_null() {
            break;
        }
        if (get_flags(field) & h::FIELD_ATTRIBUTE_STATIC) != 0 {
            continue;
        }
        let np = get_name(field);
        if np.is_null() {
            continue;
        }
        let name = CStr::from_ptr(np).to_string_lossy().into_owned();
        let off = get_off(field);
        if off < 0x10 {
            map.insert(name, Value::Null);
            continue;
        }
        let ftype = get_type(field);
        let te = type_te(ftype);
        let val = read_value(base.add(off - 0x10), te, ftype, depth, visited);
        map.insert(name, val);
    }
    Value::Object(map)
}

/// Walk an arbitrary managed object to a JSON `Value`. The public entry point:
/// callers pass `depth = 0` and a fresh `visited` set.
pub unsafe fn convert_object(obj: *mut c_void, depth: usize, visited: &mut HashSet<usize>) -> Value {
    if obj.is_null() {
        return Value::Null;
    }
    let key = obj as usize;
    if visited.contains(&key) {
        return Value::String("<cycle>".into());
    }
    if depth > MAX_DEPTH {
        return Value::String("<max depth>".into());
    }
    let klass = match h::OBJECT_GET_CLASS {
        Some(f) => f(obj),
        None => return Value::Null,
    };
    if klass.is_null() {
        return Value::Null;
    }
    visited.insert(key);
    let cname = class_name(klass);

    // Arrays.
    if cname.ends_with("[]") {
        let res = convert_array(obj, klass, &cname, depth, visited);
        visited.remove(&key);
        return res;
    }

    // System.String.
    if cname == "String" {
        visited.remove(&key);
        return Value::String(h::read_string(obj).unwrap_or_default());
    }

    // Plain object: walk this class + base classes.
    let (get_fields, get_name, get_flags, get_off, get_type, type_te, get_parent) = match (
        h::CLASS_GET_FIELDS,
        h::FIELD_GET_NAME,
        h::FIELD_GET_FLAGS,
        h::FIELD_GET_OFFSET,
        h::FIELD_GET_TYPE,
        h::TYPE_GET_TYPE,
        h::CLASS_GET_PARENT,
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f), Some(g)) => (a, b, c, d, e, f, g),
        _ => {
            visited.remove(&key);
            return Value::Null;
        }
    };
    let mut map = Map::new();
    let mut cur = klass;
    while !cur.is_null() {
        let mut iter: *mut c_void = std::ptr::null_mut();
        loop {
            let field = get_fields(cur, &mut iter);
            if field.is_null() {
                break;
            }
            if (get_flags(field) & h::FIELD_ATTRIBUTE_STATIC) != 0 {
                continue;
            }
            let np = get_name(field);
            if np.is_null() {
                continue;
            }
            let name = CStr::from_ptr(np).to_string_lossy().into_owned();
            if map.contains_key(&name) {
                continue;
            }
            let off = get_off(field);
            let ftype = get_type(field);
            let te = type_te(ftype);
            // Heap object: field data at obj + offset (offset already includes the header).
            let val = read_value((obj as *mut u8).add(off), te, ftype, depth, visited);
            map.insert(name, val);
        }
        cur = get_parent(cur);
        if !cur.is_null() {
            let pn = class_name(cur);
            if pn == "Object" || pn == "ValueType" {
                break;
            }
        }
    }
    visited.remove(&key);
    Value::Object(map)
}

unsafe fn convert_array(
    obj: *mut c_void,
    klass: *mut c_void,
    cname: &str,
    depth: usize,
    visited: &mut HashSet<usize>,
) -> Value {
    let len = match h::ARRAY_LENGTH {
        Some(f) => f(obj),
        None => return Value::Null,
    };
    if len > MAX_ARRAY {
        return Value::String(format!("<array len={len} truncated>"));
    }
    let data = (obj as *mut u8).add(32);
    // Primitive fast paths.
    match cname {
        "Int32[]" | "System.Int32[]" => {
            let s = std::slice::from_raw_parts(data as *const i32, len as usize);
            return Value::Array(s.iter().map(|v| Value::Number(Number::from(*v))).collect());
        }
        "UInt32[]" | "System.UInt32[]" => {
            let s = std::slice::from_raw_parts(data as *const u32, len as usize);
            return Value::Array(s.iter().map(|v| Value::Number(Number::from(*v))).collect());
        }
        "Int64[]" | "System.Int64[]" => {
            let s = std::slice::from_raw_parts(data as *const i64, len as usize);
            return Value::Array(s.iter().map(|v| Value::Number(Number::from(*v))).collect());
        }
        "Single[]" | "System.Single[]" => {
            let s = std::slice::from_raw_parts(data as *const f32, len as usize);
            return Value::Array(s.iter().map(|v| num_f64(*v as f64)).collect());
        }
        "Byte[]" | "System.Byte[]" => {
            let s = std::slice::from_raw_parts(data as *const u8, len as usize);
            return Value::Array(s.iter().map(|v| Value::Number(Number::from(*v))).collect());
        }
        "Boolean[]" | "System.Boolean[]" => {
            let s = std::slice::from_raw_parts(data as *const u8, len as usize);
            return Value::Array(s.iter().map(|v| Value::Bool(*v != 0)).collect());
        }
        _ => {}
    }
    let elem = match h::CLASS_GET_ELEMENT_CLASS {
        Some(f) => f(klass),
        None => return Value::Array(Vec::new()),
    };
    if elem.is_null() {
        return Value::Array(Vec::new());
    }
    let is_vt = h::CLASS_IS_VALUETYPE.map(|f| f(elem)).unwrap_or(false);
    let mut out = Vec::with_capacity(len as usize);
    if !is_vt {
        let table = data as *const *mut c_void;
        for i in 0..len as usize {
            out.push(convert_object(*table.add(i), depth + 1, visited));
        }
    } else {
        let mut align: u32 = 0;
        let stride = match h::CLASS_VALUE_SIZE {
            Some(f) => f(elem, &mut align) as usize,
            None => 0,
        };
        if stride == 0 {
            return Value::String("<struct array: unknown stride>".into());
        }
        let is_enum = h::CLASS_IS_ENUM.map(|f| f(elem)).unwrap_or(false);
        let ename = class_name(elem);
        for i in 0..len as usize {
            let ep = data.add(i * stride);
            let v = if is_enum {
                resolve_enum(ep, elem)
            } else if ename.starts_with("Obscured") {
                decrypt_obscured(ep, elem, &ename)
                    .unwrap_or_else(|| convert_struct(ep, elem, depth + 1, visited))
            } else {
                convert_struct(ep, elem, depth + 1, visited)
            };
            out.push(v);
        }
    }
    Value::Array(out)
}
