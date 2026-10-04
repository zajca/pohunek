//! Strict JSON parsing and the canonical byte form that signatures cover.
//!
//! Signing and verification both go through [`canonical_bytes`], so a signature
//! covers exactly one byte sequence per logical document. The accepted JSON
//! subset is deliberately narrow: no duplicate object keys, no trailing data
//! and only non-negative integers as numbers (no floats, no exponents), so
//! there is no second spelling of a number that could be smuggled past a
//! signature.

// Rust guideline compliant 2026-10-04

use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

/// Why a JSON document was rejected. Carries no document content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JsonError {
    /// The bytes are not a single well-formed JSON value.
    Syntax,
    /// An object has the same key twice.
    DuplicateKey,
    /// A number is negative, fractional or in exponent form.
    UnsupportedNumber,
}

/// Marker used to carry the typed failure through serde's error channel.
const DUPLICATE_KEY_MARKER: &str = "duplicate-key";
const UNSUPPORTED_NUMBER_MARKER: &str = "unsupported-number";

/// Parses `bytes` as exactly one JSON value under the strict subset.
pub(crate) fn parse_strict(bytes: &[u8]) -> Result<Value, JsonError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictValue::deserialize(&mut deserializer).map_err(|error| {
        let text = error.to_string();
        if text.contains(DUPLICATE_KEY_MARKER) {
            JsonError::DuplicateKey
        } else if text.contains(UNSUPPORTED_NUMBER_MARKER) {
            JsonError::UnsupportedNumber
        } else {
            JsonError::Syntax
        }
    })?;
    deserializer.end().map_err(|_cause| JsonError::Syntax)?;
    Ok(value.0)
}

/// Serializes `value` canonically: object keys in ascending byte order, no
/// insignificant whitespace, strings escaped by `serde_json`.
pub(crate) fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_scalar(&Value::String(key.clone()), out);
                out.push(b':');
                if let Some(member) = map.get(key) {
                    write_canonical(member, out);
                }
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        scalar => write_scalar(scalar, out),
    }
}

fn write_scalar(value: &Value, out: &mut Vec<u8>) {
    serde_json::to_writer(&mut *out, value)
        .expect("serializing a scalar JSON value into a Vec is infallible");
}

/// `serde_json::Value` with duplicate-key and number-subset enforcement.
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictVisitor)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(v)))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::from(v)))
    }

    fn visit_i64<E: de::Error>(self, _v: i64) -> Result<Self::Value, E> {
        Err(E::custom(UNSUPPORTED_NUMBER_MARKER))
    }

    fn visit_f64<E: de::Error>(self, _v: f64) -> Result<Self::Value, E> {
        Err(E::custom(UNSUPPORTED_NUMBER_MARKER))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(v.to_owned())))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<StrictValue>()? {
            items.push(item.0);
        }
        Ok(StrictValue(Value::Array(items)))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut seen = BTreeSet::new();
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(DUPLICATE_KEY_MARKER));
            }
            let value = map.next_value::<StrictValue>()?;
            object.insert(key, value.0);
        }
        Ok(StrictValue(Value::Object(object)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_form_sorts_keys_and_drops_whitespace() {
        let value = parse_strict(br#"{ "b": 1, "a": [ {"y":true,"x":null} , "s" ] }"#).unwrap();
        assert_eq!(
            canonical_bytes(&value),
            br#"{"a":[{"x":null,"y":true},"s"],"b":1}"#.to_vec()
        );
    }

    #[test]
    fn rejects_duplicate_keys_at_any_depth() {
        assert_eq!(
            parse_strict(br#"{"a":1,"a":2}"#),
            Err(JsonError::DuplicateKey)
        );
        assert_eq!(
            parse_strict(br#"{"a":[{"k":1,"k":1}]}"#),
            Err(JsonError::DuplicateKey)
        );
    }

    #[test]
    fn rejects_trailing_data_and_garbage() {
        assert_eq!(parse_strict(b"{} {}"), Err(JsonError::Syntax));
        assert_eq!(parse_strict(b"{}x"), Err(JsonError::Syntax));
        assert_eq!(parse_strict(b""), Err(JsonError::Syntax));
    }

    #[test]
    fn rejects_non_integer_numbers() {
        for input in [&b"-1"[..], b"1.5", b"1e3", b"1.0"] {
            assert_eq!(parse_strict(input), Err(JsonError::UnsupportedNumber));
        }
    }

    #[test]
    fn escaped_and_literal_strings_canonicalize_identically() {
        let escaped = parse_strict(br#"{"k":"A"}"#).unwrap();
        let literal = parse_strict(br#"{"k":"A"}"#).unwrap();
        assert_eq!(canonical_bytes(&escaped), canonical_bytes(&literal));
    }
}
