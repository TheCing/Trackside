//! Heaven — msgpack (rmpv) navigation helpers, shared by the response-hook consumers
//! (the response hook's player-id parse). Generic tree walking
//! only; no feature logic lives here.

#![allow(dead_code)]

use rmpv::Value;

/// Value stored under `key` in a msgpack map, or None.
pub fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Map(m) = v {
        m.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, val)| val)
    } else {
        None
    }
}

/// The array backing a `Value::Array`, or None.
pub fn as_arr(v: &Value) -> Option<&Vec<Value>> {
    if let Value::Array(a) = v {
        Some(a)
    } else {
        None
    }
}

/// Collect every value stored under `key` anywhere in the tree (depth-first).
pub fn find_key<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Map(m) => {
            for (k, val) in m {
                if k.as_str() == Some(key) {
                    out.push(val);
                }
                find_key(val, key, out);
            }
        }
        Value::Array(a) => {
            for val in a {
                find_key(val, key, out);
            }
        }
        _ => {}
    }
}

/// True if `needle` occurs as a contiguous byte subslice of `hay`.
pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// rmpv → serde_json, verbatim (map keys stringified). For packet subtrees exported as JSON
/// files (the UmaExtractor-format veterans dump). Binaries/exts become null (none in our data).
pub fn to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::String(s) => J::String(s.as_str().unwrap_or_default().to_string()),
        Value::F32(f) => serde_json::Number::from_f64(*f as f64).map(J::Number).unwrap_or(J::Null),
        Value::F64(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
        Value::Integer(_) => v
            .as_i64()
            .map(|x| J::Number(x.into()))
            .or_else(|| v.as_u64().map(|x| J::Number(x.into())))
            .unwrap_or(J::Null),
        Value::Array(a) => J::Array(a.iter().map(to_json).collect()),
        Value::Map(m) => J::Object(
            m.iter()
                .map(|(k, val)| (k.as_str().map(|s| s.to_string()).unwrap_or_else(|| format!("{k}")), to_json(val)))
                .collect(),
        ),
        _ => J::Null,
    }
}
