//! Client-side tolerant decoding of enums a newer daemon may extend.
//!
//! Plain `serde_json::from_*` is strict everywhere: an enum value the decoding
//! build does not know is an error. That is what the daemon wants for client
//! params, the session store, the notification store and governance state.
//! The client decodes daemon responses and events through [`from_value`],
//! [`from_str`] and [`from_slice`] instead. Inside one of those calls an enum
//! that opted in (see [`tolerant_enum!`](crate::tolerant_enum)) maps an unknown
//! string to its hidden `Unknown` variant; every other decode error is still an
//! error.
//!
//! The mode is a thread-local flag that is set only for the duration of one
//! synchronous decode call and restored on exit, including on unwind. It is
//! never a guard object a caller could hold across an `.await`.

use std::cell::Cell;

use serde::de::DeserializeOwned;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Restores the previous mode when the decode call ends or unwinds.
struct Scope {
    previous: bool,
}

impl Scope {
    fn enter() -> Self {
        Self {
            previous: ACTIVE.replace(true),
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.set(self.previous);
    }
}

/// Whether the current decode call runs in tolerant mode.
#[doc(hidden)]
#[must_use]
pub fn is_active() -> bool {
    ACTIVE.get()
}

/// Decodes `value` with unknown values of tolerant enums mapped to `Unknown`.
///
/// # Errors
///
/// Returns the `serde_json` error when `value` does not match `T` for any
/// reason other than an unknown value of a tolerant enum.
pub fn from_value<T: DeserializeOwned>(value: serde_json::Value) -> serde_json::Result<T> {
    let _scope = Scope::enter();
    serde_json::from_value(value)
}

/// Decodes one JSON document like [`from_value`].
///
/// # Errors
///
/// Returns the `serde_json` error when `text` is not valid JSON or does not
/// match `T` for any reason other than an unknown value of a tolerant enum.
pub fn from_str<T: DeserializeOwned>(text: &str) -> serde_json::Result<T> {
    let _scope = Scope::enter();
    serde_json::from_str(text)
}

/// Decodes one JSON document like [`from_value`].
///
/// # Errors
///
/// Returns the `serde_json` error when `bytes` is not valid JSON or does not
/// match `T` for any reason other than an unknown value of a tolerant enum.
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> serde_json::Result<T> {
    let _scope = Scope::enter();
    serde_json::from_slice(bytes)
}

/// Decodes a tolerant enum: strictly outside tolerant mode, and with an
/// unknown string mapped to `unknown` inside it.
#[doc(hidden)]
pub fn deserialize_enum<'de, D, T>(
    deserializer: D,
    strict: fn(serde_json::Value) -> serde_json::Result<T>,
    direct: fn(D) -> Result<T, D::Error>,
    unknown: T,
) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    use serde::Deserialize as _;

    if !is_active() {
        return direct(deserializer);
    }
    let value = serde_json::Value::deserialize(deserializer)?;
    let is_string = value.is_string();
    match strict(value) {
        Ok(decoded) => Ok(decoded),
        Err(_error) if is_string => Ok(unknown),
        Err(error) => Err(D::Error::custom(error)),
    }
}

/// Implements `Serialize` and `Deserialize` for an enum that derives both with
/// `#[serde(remote = "Self")]`, a `#[serde(rename = "unknown")]` variant
/// `Unknown` and `#[serde(skip_deserializing)]` on that variant.
///
/// The derive stays the only definition of the wire names. `Unknown` serializes
/// as `"unknown"` but plain decoding rejects it, so only the tolerant entry
/// points in this module can produce one.
#[macro_export]
macro_rules! tolerant_enum {
    ($name:ident) => {
        impl ::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> ::core::result::Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                $name::serialize(self, serializer)
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::core::result::Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                $crate::tolerant::deserialize_enum(
                    deserializer,
                    |value| $name::deserialize(value),
                    |deserializer| $name::deserialize(deserializer),
                    $name::Unknown,
                )
            }
        }
    };
}
