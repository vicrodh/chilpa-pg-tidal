//! Reading provider JSON the way the cards need it: missing keys read as null,
//! and "present" means a value that is not null, false, zero or empty.

use serde_json::{Map, Value};

pub static NULL: Value = Value::Null;

/// The value at `key`, or null when this is not an object or has no such key.
pub fn at<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.as_object().and_then(|object| object.get(key)).unwrap_or(&NULL)
}

pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// The first present value among `keys`, or null.
pub fn first<'a>(value: &'a Value, keys: &[&str]) -> &'a Value {
    keys.iter().map(|key| at(value, key)).find(|found| truthy(found)).unwrap_or(&NULL)
}

/// A scalar as text: strings as they are, numbers in their JSON form,
/// anything else empty.
pub fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.into(),
        _ => String::new(),
    }
}

pub fn str_at<'a>(value: &'a Value, key: &str) -> &'a str {
    at(value, key).as_str().unwrap_or("")
}

/// The object at `key`, or an empty object.
pub fn obj_at(value: &Value, key: &str) -> Value {
    match at(value, key) {
        Value::Object(fields) => Value::Object(fields.clone()),
        _ => Value::Object(Map::new()),
    }
}

/// The array at `key`, or an empty slice.
pub fn list_at<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    at(value, key).as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// An integer field the way the provider sends counts: a number, or the default.
pub fn int_at(value: &Value, key: &str, default: i64) -> i64 {
    match at(value, key) {
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|n| n as i64)).unwrap_or(default),
        _ => default,
    }
}

/// A request parameter read as an integer, accepting numbers and digit strings.
pub fn param_int(params: &Value, key: &str, default: i64) -> crate::state::Result<i64> {
    match at(params, key) {
        Value::Null => Ok(default),
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|n| n as i64))
            .ok_or_else(|| crate::state::Failure::invalid("Invalid number.")),
        Value::String(text) => text.trim().parse().map_err(|_| crate::state::Failure::invalid("Invalid number.")),
        _ => Err(crate::state::Failure::invalid("Invalid number.")),
    }
}

pub fn set(value: &mut Value, key: &str, field: Value) {
    if let Value::Object(fields) = value {
        fields.insert(key.to_string(), field);
    }
}

/// Serializes the way the provider-independent identities have always been
/// hashed: compact separators with a space, and ASCII-only escapes.
pub fn dumps_spaced(value: &Value) -> String {
    let mut out = String::new();
    write_spaced(value, &mut out);
    out
}

fn write_spaced(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_ascii_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_spaced(item, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_ascii_string(key, out);
                out.push_str(": ");
                write_spaced(item, out);
            }
            out.push('}');
        }
    }
}

fn write_ascii_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 || (ch as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

/// The hexadecimal SHA-256 of `text`.
pub fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Percent-encodes every byte outside the unreserved set, slashes included.
pub fn quote_all(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Case-insensitive comparison key, close to Unicode case folding.
pub fn fold(text: &str) -> String {
    text.to_lowercase().replace('ß', "ss")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identities_serialize_with_spaced_separators_and_ascii_escapes() {
        let value = json!([["track", "7"], ["video", "é"]]);
        assert_eq!(dumps_spaced(&value), r#"[["track", "7"], ["video", "é"]]"#);
    }

    #[test]
    fn quoting_keeps_only_unreserved_bytes() {
        assert_eq!(quote_all("pages/a b"), "pages%2Fa%20b");
        assert_eq!(unquote("pages%2Fa%20b"), "pages/a b");
    }
}
