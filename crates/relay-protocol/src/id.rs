use std::fmt::{Debug, Display, Formatter};

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

const PAYLOAD_BYTES: usize = 32;
const PAYLOAD_CHARS: usize = 43;

/// Reports an invalid opaque relay identifier.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdError {
    #[error("identifier must begin with `{expected}`")]
    Prefix { expected: &'static str },
    #[error("identifier payload must be canonical base64url for {PAYLOAD_BYTES} bytes")]
    Payload,
}

fn parse(value: &str, prefix: &'static str) -> Result<String, IdError> {
    let payload = value
        .strip_prefix(prefix)
        .ok_or(IdError::Prefix { expected: prefix })?;
    if payload.len() != PAYLOAD_CHARS {
        return Err(IdError::Payload);
    }
    let decoded = BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_error| IdError::Payload)?;
    if decoded.len() != PAYLOAD_BYTES || BASE64_URL_SAFE_NO_PAD.encode(decoded) != payload {
        return Err(IdError::Payload);
    }
    Ok(value.to_owned())
}

macro_rules! opaque_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
        #[cfg_attr(feature = "ts", ts(export))]
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        pub struct $name(String);
        impl $name {
            pub fn parse(value: &str) -> Result<Self, IdError> {
                parse(value, $prefix).map(Self)
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl TryFrom<&str> for $name {
            type Error = IdError;
            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }
        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl Debug for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(RelayId, "relay_");

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
        #[cfg_attr(feature = "ts", ts(export))]
        #[cfg_attr(feature = "ts", ts(type = "string"))]
        pub struct $name(uuid::Uuid);
        impl $name {
            /// Creates this relay-local identifier from its canonical `UUID`.
            #[must_use]
            pub const fn from_uuid(value: uuid::Uuid) -> Self {
                Self(value)
            }
            /// Parses a canonical hyphenated `UUID` identifier.
            pub fn parse(value: &str) -> Result<Self, IdError> {
                let parsed = uuid::Uuid::parse_str(value).map_err(|_error| IdError::Payload)?;
                if parsed.hyphenated().to_string() != value {
                    return Err(IdError::Payload);
                }
                Ok(Self(parsed))
            }
            /// Returns the durable `PostgreSQL` `UUID` coordinate.
            #[must_use]
            pub const fn as_uuid(self) -> uuid::Uuid {
                self.0
            }
        }
        impl Debug for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }
        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                Display::fmt(&self.0.hyphenated(), f)
            }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

uuid_id!(PrincipalId);
uuid_id!(TeamId);
uuid_id!(CredentialId);

#[cfg(test)]
mod tests {
    use super::RelayId;

    #[test]
    fn relay_id_requires_the_canonical_opaque_shape() {
        RelayId::parse("relay_not-canonical").expect_err("short relay ID");
        RelayId::parse(&format!("relay_{}", "!".repeat(43))).expect_err("invalid alphabet");
    }
}
