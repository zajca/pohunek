//! Secret per-session environment carried by `session.new`.
//!
//! The names a caller may fill come from the host profile (`secret_env` key);
//! the values travel on the wire in [`SessionSecretEnv`]. The type redacts its
//! `Debug` form, reports a malformed entry with fixed text that never echoes a
//! name or a value, and is never persisted or listed.

// Rust guideline compliant 2026-06-26

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{Error as _, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    ErrorClass, ProtocolError, MAX_SESSION_SECRET_ENV_ENTRIES, MAX_SESSION_SECRET_ENV_NAME_BYTES,
    MAX_SESSION_SECRET_ENV_VALUE_BYTES,
};

/// Overrides every scalar `visit_*` of a visitor so a value of the wrong shape
/// is refused with the fixed text; serde's default message quotes the value.
macro_rules! refuse_scalars {
    () => {
        fn visit_bool<E: serde::de::Error>(self, _value: bool) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_i64<E: serde::de::Error>(self, _value: i64) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_i128<E: serde::de::Error>(self, _value: i128) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_u64<E: serde::de::Error>(self, _value: u64) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_u128<E: serde::de::Error>(self, _value: u128) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_f64<E: serde::de::Error>(self, _value: f64) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_char<E: serde::de::Error>(self, _value: char) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_bytes<E: serde::de::Error>(self, _value: &[u8]) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_byte_buf<E: serde::de::Error>(self, _value: Vec<u8>) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, _seq: A) -> Result<Self::Value, A::Error> {
            Err(serde::de::Error::custom(SECRET_ENV_INVALID_MESSAGE))
        }
    };
}

/// Error code of a request naming a variable the resolved profile does not declare.
pub const SECRET_ENV_NOT_DECLARED_CODE: &str = "secret_env_not_declared";
/// Error code of a launch of a slot profile that lacks a declared variable.
pub const SECRET_ENV_MISSING_CODE: &str = "secret_env_missing";
/// Error code of a malformed `secret_env` map.
pub const SECRET_ENV_INVALID_CODE: &str = "secret_env_invalid";
/// Error code a client raises when the daemon cannot honor `secret_env`.
pub const SESSION_NEW_SECRET_ENV_UNSUPPORTED_CODE: &str = "session_new_secret_env_unsupported";

/// Fixed text of every deserialization failure of the map.
///
/// serde's own type errors can quote the offending value, so the parser reports
/// only this text; the daemon recognizes it to answer [`SECRET_ENV_INVALID_CODE`].
pub const SECRET_ENV_INVALID_MESSAGE: &str = "secret_env: invalid entry";

/// Why a `secret_env` map is malformed, without naming an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSecretEnvError;

impl fmt::Display for SessionSecretEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(SECRET_ENV_INVALID_MESSAGE)
    }
}

impl std::error::Error for SessionSecretEnvError {}

/// Secret variables for one `session.new`: variable name to value.
///
/// Holds at most [`MAX_SESSION_SECRET_ENV_ENTRIES`] entries whose values are
/// non-empty, free of NUL and at most [`MAX_SESSION_SECRET_ENV_VALUE_BYTES`]
/// bytes. `Debug` shows only the entry count.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SessionSecretEnv(BTreeMap<String, String>);

impl SessionSecretEnv {
    /// Builds the map from `entries`, enforcing the bounds.
    ///
    /// # Errors
    ///
    /// [`SessionSecretEnvError`] for too many entries, an empty or over-long
    /// name, or an empty, over-long or NUL-carrying value.
    pub fn new(entries: BTreeMap<String, String>) -> Result<Self, SessionSecretEnvError> {
        if entries.len() > MAX_SESSION_SECRET_ENV_ENTRIES {
            return Err(SessionSecretEnvError);
        }
        for (name, value) in &entries {
            if !valid_entry(name, value) {
                return Err(SessionSecretEnvError);
            }
        }
        Ok(Self(entries))
    }

    /// Whether no variable is carried.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of variables carried.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The variable names, sorted. Names are not secret but are never logged.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// The `(name, value)` pairs, sorted by name, consuming the map.
    #[must_use]
    pub fn into_entries(self) -> Vec<(String, String)> {
        self.0.into_iter().collect()
    }
}

fn valid_entry(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SESSION_SECRET_ENV_NAME_BYTES
        && !value.is_empty()
        && value.len() <= MAX_SESSION_SECRET_ENV_VALUE_BYTES
        && !value.as_bytes().contains(&0)
}

impl fmt::Debug for SessionSecretEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionSecretEnv({} entries, redacted)", self.0.len())
    }
}

impl Serialize for SessionSecretEnv {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SessionSecretEnv {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;

        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = SessionSecretEnv;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(SECRET_ENV_INVALID_MESSAGE)
            }

            refuse_scalars!();

            fn visit_str<E: serde::de::Error>(self, _value: &str) -> Result<Self::Value, E> {
                Err(E::custom(SECRET_ENV_INVALID_MESSAGE))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = BTreeMap::new();
                // The bound is checked per entry so an oversized map is refused
                // without buffering it.
                while let Some((name, value)) = map.next_entry::<String, EntryValue>()? {
                    if entries.len() >= MAX_SESSION_SECRET_ENV_ENTRIES {
                        return Err(A::Error::custom(SECRET_ENV_INVALID_MESSAGE));
                    }
                    entries.insert(name, value.0);
                }
                SessionSecretEnv::new(entries)
                    .map_err(|_error| A::Error::custom(SECRET_ENV_INVALID_MESSAGE))
            }
        }

        deserializer.deserialize_any(EntriesVisitor)
    }
}

/// A map value that accepts only a string and refuses anything else with the
/// fixed text, so serde never quotes the offending value.
struct EntryValue(String);

impl<'de> Deserialize<'de> for EntryValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StringOnly;

        impl<'de> Visitor<'de> for StringOnly {
            type Value = EntryValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(SECRET_ENV_INVALID_MESSAGE)
            }

            refuse_scalars!();

            fn visit_map<A: MapAccess<'de>>(self, _map: A) -> Result<Self::Value, A::Error> {
                Err(A::Error::custom(SECRET_ENV_INVALID_MESSAGE))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<EntryValue, E> {
                Ok(EntryValue(v.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<EntryValue, E> {
                Ok(EntryValue(v))
            }
        }

        deserializer.deserialize_any(StringOnly)
    }
}

impl SessionSecretEnv {
    /// Whether serde's `skip_serializing_if` should omit the field.
    #[must_use]
    pub fn is_unset(value: &Self) -> bool {
        value.is_empty()
    }
}

impl ProtocolError {
    /// The `runtime/secret_env_not_declared` error. Names no variable.
    #[must_use]
    pub fn secret_env_not_declared() -> Self {
        Self::new(
            ErrorClass::Runtime,
            SECRET_ENV_NOT_DECLARED_CODE,
            "the agent profile does not declare a secret variable the request fills",
            Some("declare the variable name in the profile's secret_env key".to_owned()),
        )
    }

    /// The `runtime/secret_env_missing` error. Names no variable.
    #[must_use]
    pub fn secret_env_missing() -> Self {
        Self::new(
            ErrorClass::Runtime,
            SECRET_ENV_MISSING_CODE,
            "the agent profile declares secret variables this launch does not fill",
            Some(
                "start the session with every variable the profile's secret_env declares"
                    .to_owned(),
            ),
        )
    }

    /// The `runtime/secret_env_invalid` error. Names no variable.
    #[must_use]
    pub fn secret_env_invalid() -> Self {
        Self::new(
            ErrorClass::Runtime,
            SECRET_ENV_INVALID_CODE,
            "secret_env is malformed: at most 16 entries, each a non-empty string value of at most 16 KiB without NUL",
            None,
        )
    }

    /// The `daemon/session_new_secret_env_unsupported` error a client raises
    /// before it sends `session.new` to a daemon that cannot honor `secret_env`.
    #[must_use]
    pub fn session_new_secret_env_unsupported(host: Option<&str>) -> Self {
        let target = host.map_or_else(
            || "the local daemon".to_owned(),
            |route| format!("host '{route}'"),
        );
        Self::new(
            ErrorClass::Daemon,
            SESSION_NEW_SECRET_ENV_UNSUPPORTED_CODE,
            format!("{target} predates secret_env and would drop it silently; nothing was sent"),
            Some("upgrade the daemon, then retry".to_owned()),
        )
    }
}
