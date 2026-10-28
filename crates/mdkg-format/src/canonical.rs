//! Canonical JSON (RFC 8785, JSON Canonicalization Scheme) for the value
//! subset mdkg uses: null, booleans, integers, strings, arrays and objects.
//!
//! Records and event envelopes are hashed in this canonical form.
//! Floats are rejected rather than half-supported, and so
//! are integers outside the IEEE 754 safe range, which JSON consumers in
//! JavaScript cannot represent exactly.

use serde_json::Value;

/// Largest integer every JSON consumer can represent exactly (2^53 - 1).
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalError {
    Float(String),
    UnsafeInteger(String),
}

impl std::fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CanonicalError::Float(n) => write!(f, "canonical JSON rejects non-integer number {n}"),
            CanonicalError::UnsafeInteger(n) => {
                write!(
                    f,
                    "canonical JSON rejects integer {n} outside the safe range"
                )
            }
        }
    }
}

impl std::error::Error for CanonicalError {}

/// Serializes `value` in canonical form.
pub fn to_canonical_string(value: &Value) -> Result<String, CanonicalError> {
    let mut out = String::new();
    write_value(value, &mut out)?;
    Ok(out)
}

fn write_value(value: &Value, out: &mut String) -> Result<(), CanonicalError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            let text = n.to_string();
            let integer = n
                .as_i64()
                .or_else(|| n.as_u64().and_then(|u| i64::try_from(u).ok()));
            match integer {
                Some(i) if (-MAX_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&i) => {
                    out.push_str(&i.to_string());
                }
                Some(_) => return Err(CanonicalError::UnsafeInteger(text)),
                None if n.is_f64() => return Err(CanonicalError::Float(text)),
                None => return Err(CanonicalError::UnsafeInteger(text)),
            }
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // RFC 8785 orders keys by their UTF-16 code units.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(&map[key.as_str()], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// ECMAScript `JSON.stringify` string escaping, which RFC 8785 adopts.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_drops_whitespace() {
        let v = json!({"b": [1, {"z": null, "a": true}], "a": "x"});
        assert_eq!(
            to_canonical_string(&v).unwrap(),
            r#"{"a":"x","b":[1,{"a":true,"z":null}]}"#
        );
    }

    #[test]
    fn sorts_keys_by_utf16_code_units() {
        // U+FB33 sorts after U+1F600 in UTF-8 byte order but before it in UTF-16.
        let v = json!({"\u{1F600}": 1, "\u{FB33}": 2, "a": 0});
        assert_eq!(
            to_canonical_string(&v).unwrap(),
            "{\"a\":0,\"\u{1F600}\":1,\"\u{FB33}\":2}"
        );
    }

    #[test]
    fn escapes_like_json_stringify() {
        let v = json!("q\"\\\u{1}\u{7f}é\n");
        assert_eq!(
            to_canonical_string(&v).unwrap(),
            "\"q\\\"\\\\\\u0001\u{7f}é\\n\""
        );
    }

    #[test]
    fn rejects_floats_and_unsafe_integers() {
        assert!(matches!(
            to_canonical_string(&json!(1.5)),
            Err(CanonicalError::Float(_))
        ));
        assert!(to_canonical_string(&json!(MAX_SAFE_INTEGER)).is_ok());
        assert!(matches!(
            to_canonical_string(&json!(MAX_SAFE_INTEGER + 1)),
            Err(CanonicalError::UnsafeInteger(_))
        ));
        assert!(matches!(
            to_canonical_string(&json!(u64::MAX)),
            Err(CanonicalError::UnsafeInteger(_))
        ));
    }
}
