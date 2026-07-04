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
