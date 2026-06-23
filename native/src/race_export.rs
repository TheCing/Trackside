//! race_export — dump each race to a JSON the user can upload to a web race
//! visualizer (the `RaceInfo` object incl. its `<SimDataBase64>k__BackingField`).
//!
//! Mechanism: `race::compute_header` already detects a NEW race (the `RaceInfo`
//! pointer changing) and hands us the live `RaceInfo` object — so we don't add a
//! hot hook. On a new race we walk the object graph by IL2CPP reflection into a
//! `serde_json::Value` (using the field/type/array exports already resolved in
//! `htt_il2cpp`), then write it to disk grouped by `<RaceType>k__BackingField`.
//!
//! The serializer mirrors the on-disk schema the visualizer expects: every
//! instance field is emitted under its raw managed name (`<X>k__BackingField`),
//! enums resolve to their member name, `Obscured*` numeric values are decrypted,
//! and arrays/strings/nested objects recurse. The heavy serialize + file write
//! runs on a worker thread; only the managed-memory walk touches the game thread
//! (which is IL2CPP-attached when `compute_header` runs).

#![allow(static_mut_refs)]

use core::ffi::{c_void, CStr};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Number, Value};

use crate::htt_il2cpp as h;

const MAX_DEPTH: usize = 60;
const MAX_ARRAY: u32 = 8192;

// The race viewer (hakuraku) shows an "outdated, go download the other tool" notice
// unless the file declares this version key. We emit it so the viewer treats our
// output as current and never points the user elsewhere. Bump to match the viewer's
// expected current release.
const VIEWER_VERSION: &str = "1.1.2";

/// Walk an arbitrary managed object to a JSON string (for one-shot RE/census of
/// unknown object layouts, e.g. the career acquired-skill list). Safe to call from
/// an attached thread; returns "<err>" on failure.
pub fn dump_object_json(addr: usize) -> String {
    if addr == 0 {
        return "null".into();
    }
    std::panic::catch_unwind(move || unsafe {
        let mut visited: HashSet<usize> = HashSet::new();
        let val = convert_object(addr as *mut c_void, 0, &mut visited);
        serde_json::to_string(&val).unwrap_or_default()
    })
    .unwrap_or_else(|_| "<err>".into())
}

/// Stamp the viewer version key onto the root object.
fn stamp_version(v: &mut Value) {
    if let Value::Object(map) = v {
        map.insert("horseACT_version".to_string(), Value::String(VIEWER_VERSION.to_string()));
    }
}

// Runtime mirror of the settings toggle. The RaceInfo getter we hook fires very
// often, so the hot path checks this atomic instead of locking the settings cache.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Mirror the persisted toggle into the fast path. Called by settings.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

// Cached `<SimDataBase64>k__BackingField` offset on RaceInfo. usize::MAX = unknown.
static SIM_OFFSET: AtomicUsize = AtomicUsize::new(usize::MAX);
// Dedup: last race we dumped (RaceInfo ptr + its SimData ptr). A race is "new" when
// either changes (the game reuses a RaceInfo address but swaps the SimData on a re-run).
static LAST_RI: AtomicUsize = AtomicUsize::new(0);
static LAST_SIM: AtomicUsize = AtomicUsize::new(0);
// Last RaceInfo we logged a diagnostic line for (so the log shows one line per race).
static LAST_DIAG: AtomicUsize = AtomicUsize::new(0);

fn elog(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::paths::log_file("heaven-native.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

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

unsafe fn convert_object(obj: *mut c_void, depth: usize, visited: &mut HashSet<usize>) -> Value {
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

// ── new-race entry point (called from race::compute_header) ───────────────────

/// Called every time `compute_header` sees the live `RaceInfo`. Cheap pointer
/// compares; only walks + saves when a genuinely new race is detected and the
/// export toggle is on. Safe to call from the (attached) game thread.
pub fn maybe_dump(ri: *mut c_void) {
    if ri.is_null() || !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let addr = ri as usize;
    let _ = std::panic::catch_unwind(move || unsafe { dump_inner(addr as *mut c_void) });
}

unsafe fn dump_inner(ri: *mut c_void) {
    let klass = match h::OBJECT_GET_CLASS {
        Some(f) => f(ri),
        None => return,
    };
    if klass.is_null() {
        return;
    }

    // Resolve + cache the SimDataBase64 field offset once.
    let mut sim_off = SIM_OFFSET.load(Ordering::Relaxed);
    if sim_off == usize::MAX {
        sim_off = h::field_offset(klass, "<SimDataBase64>k__BackingField").unwrap_or(0);
        SIM_OFFSET.store(sim_off, Ordering::Relaxed);
    }
    let sim_ptr = if sim_off != 0 {
        *((ri as usize + sim_off) as *const usize)
    } else {
        0
    };
    // One line per distinct RaceInfo so the log shows the getter firing + whether the
    // SimData blob has populated yet (diagnostic; cheap, ~1 line per race).
    if (ri as usize) != LAST_DIAG.load(Ordering::Relaxed) {
        LAST_DIAG.store(ri as usize, Ordering::Relaxed);
        elog(&format!("[race-export] rt hook: ri={ri:p} sim_off={sim_off:#x} sim_ptr={sim_ptr:#x}"));
    }
    // If we know the SimData slot and it's still empty, the race isn't ready —
    // don't dump (and don't mark it seen, so we retry on the next call).
    if sim_off != 0 && sim_ptr == 0 {
        return;
    }

    let last_ri = LAST_RI.load(Ordering::Relaxed);
    let last_sim = LAST_SIM.load(Ordering::Relaxed);
    let is_new = (ri as usize) != last_ri || (sim_off != 0 && sim_ptr != last_sim);
    if !is_new {
        return;
    }
    LAST_RI.store(ri as usize, Ordering::Relaxed);
    LAST_SIM.store(sim_ptr, Ordering::Relaxed);

    let mut visited: HashSet<usize> = HashSet::new();
    let val = convert_object(ri, 0, &mut visited);
    let base = crate::paths::dll_dir().join("heaven-races");
    // Hand off the (pure-Rust) serialize + disk write to a worker thread so the
    // game thread isn't blocked on I/O.
    std::thread::spawn(move || save(val, base));
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' })
        .collect::<String>()
        .trim()
        .to_string()
}

fn folder_for(race_type: &str) -> String {
    match race_type {
        "Single" => "Career".into(),
        "RoomMatch" => "Room match".into(),
        "Champions" => "Champions meeting".into(),
        "Practice" => "Practice room".into(),
        "Stadium" | "TeamStadium" | "Daily" => "Team trials".into(),
        "" => "Other".into(),
        // Any unknown race type self-groups into a folder named after it, so new
        // categories are captured (and surfaced) without a code change.
        other => sanitize(other),
    }
}

/// Find the winner (FinishOrder == 0) across the known horse arrays → (name, raw time).
fn winner_of(v: &Value) -> Option<(String, f64)> {
    for field in ["<RaceHorse>k__BackingField", "<PlayerTeamMemberArray>k__BackingField"] {
        if let Some(arr) = v.get(field).and_then(|x| x.as_array()) {
            if let Some(w) = arr
                .iter()
                .find(|h| h.get("FinishOrder").and_then(|x| x.as_i64()) == Some(0))
            {
                let name = w
                    .get("<charaName>k__BackingField")
                    .and_then(|x| x.as_str())
                    .unwrap_or("race")
                    .to_string();
                let time = w.get("FinishTimeRaw").and_then(|x| x.as_f64()).unwrap_or(0.0);
                return Some((name, time));
            }
        }
    }
    None
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn write_json(value: Value, dir: PathBuf, filename: String) {
    if let Err(e) = std::fs::create_dir_all(&dir) {
        elog(&format!("[race-export] mkdir failed: {e}"));
        return;
    }
    let path = dir.join(filename);
    match serde_json::to_string_pretty(&value) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(_) => elog(&format!("[race-export] saved {}", path.display())),
            Err(e) => elog(&format!("[race-export] write failed: {e}")),
        },
        Err(e) => elog(&format!("[race-export] serialize failed: {e}")),
    }
}

fn save(mut value: Value, base: PathBuf) {
    // The visualizer needs the replay blob; skip empty races.
    match value.get("<SimDataBase64>k__BackingField") {
        Some(Value::String(s)) if !s.is_empty() => {}
        _ => {
            elog("[race-export] skipped: SimDataBase64 missing/empty");
            return;
        }
    }
    stamp_version(&mut value);
    let race_type = value
        .get("<RaceType>k__BackingField")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let dir = base.join(folder_for(&race_type));
    let stamp = now_ms();
    let filename = match winner_of(&value) {
        Some((name, time)) => format!("{}-{:.4}s-{}.json", sanitize(&name), time, stamp),
        None => format!("race-{stamp}.json"),
    };
    write_json(value, dir, filename);
}

/// Team Trials export. The Team Trials result never goes through `RaceInfo`
/// (the races are auto-resolved, so `get_RaceTrackId` never fires) — but Heaven
/// already hooks `TeamStadiumResult..ctor` for the dashboard capture, so we reuse
/// that hook's response object here: walk the whole result payload to JSON and
/// drop it under "Team trials". No SimData gate — the TT payload carries its own
/// per-race replay blobs (`race_scenario`).
pub fn dump_team_trials(response: *mut c_void) {
    if response.is_null() || !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let addr = response as usize;
    let _ = std::panic::catch_unwind(move || unsafe {
        let mut visited: HashSet<usize> = HashSet::new();
        let mut val = convert_object(addr as *mut c_void, 0, &mut visited);
        // Sanity: only dump if it actually looks like a Team Trials result.
        let looks_tt = val.get("race_result_array").is_some()
            || val
                .get("data")
                .and_then(|d| d.get("race_result_array"))
                .is_some();
        if !looks_tt {
            elog("[race-export] TT: no race_result_array; skipped");
            return;
        }
        stamp_version(&mut val);
        let dir = crate::paths::dll_dir().join("heaven-races").join("Team trials");
        let stamp = now_ms();
        std::thread::spawn(move || write_json(val, dir, format!("TT-{stamp}.json")));
    });
}
