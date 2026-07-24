//! Redacting wire types for sensitive protocol values.
//!
//! Serialization remains available because these values must cross the local
//! worker socket. Their `Debug` implementations reveal only type and size
//! metadata, preventing routine structured logs from exposing contents.

// Rust guideline compliant 2026-06-26

use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// Maximum bytes accepted in a one-use token or lease challenge.
///
/// Random URL-safe tokens are normally much shorter. This bound prevents an
/// untrusted control peer from turning a credential field into an allocation
/// amplifier.
const MAX_CREDENTIAL_BYTES: usize = 512;

/// Reports invalid sensitive protocol data.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SecretError {
    /// A credential was empty.
    #[error("{field} must not be empty")]
    EmptyCredential {
        /// Name of the invalid credential field.
        field: &'static str,
    },
    /// A credential exceeded its wire limit.
    #[error("{field} is {actual} bytes; maximum is {maximum}")]
    CredentialTooLong {
        /// Name of the invalid credential field.
        field: &'static str,
        /// Observed byte length.
        actual: usize,
        /// Maximum accepted byte length.
        maximum: usize,
    },
    /// A credential contained a non-URL-safe byte.
    #[error("{field} contains a disallowed byte at index {index}")]
    InvalidCredentialByte {
        /// Name of the invalid credential field.
        field: &'static str,
        /// Byte index of the invalid value.
        index: usize,
    },
    /// An environment name was not suitable for process creation.
    #[error("environment variable name is invalid")]
    InvalidEnvName,
    /// An environment value contained a null byte.
    #[error("environment variable value contains a null byte")]
    InvalidEnvValue,
}

fn validate_credential(value: &str, field: &'static str) -> Result<(), SecretError> {
    if value.is_empty() {
        return Err(SecretError::EmptyCredential { field });
    }
    if value.len() > MAX_CREDENTIAL_BYTES {
        return Err(SecretError::CredentialTooLong {
            field,
            actual: value.len(),
            maximum: MAX_CREDENTIAL_BYTES,
        });
    }
    if let Some((index, _)) = value
        .bytes()
        .enumerate()
        .find(|(_, byte)| !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(SecretError::InvalidCredentialByte { field, index });
    }
    Ok(())
}

macro_rules! define_credential {
    ($name:ident, $field:literal, $summary:literal) => {
        #[doc = $summary]
        #[derive(Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a validated sensitive value.
            ///
            /// # Errors
            ///
            /// Returns [`SecretError`] when `value` is empty, oversized, or
            /// outside the URL-safe credential alphabet.
            pub fn new(value: impl AsRef<str>) -> Result<Self, SecretError> {
                let value = value.as_ref();
                validate_credential(value, $field)?;
                Ok(Self(value.to_owned()))
            }
        }

        impl Debug for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("redacted", &true)
                    .field("bytes", &self.0.len())
                    .finish()
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

define_credential!(
    DataToken,
    "data token",
    "Carries a random one-use data-stream credential."
);
define_credential!(
    LeaseChallenge,
    "lease challenge",
    "Carries the connection-bound controller lease challenge."
);

/// Stores sensitive bytes with redacted diagnostics.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    /// Takes ownership of sensitive bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Borrows the sensitive bytes for their intended operation.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the wrapper and returns the sensitive bytes.
    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        self.0
    }

    /// Returns the byte count without exposing contents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Reports whether the value contains no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Debug for SecretBytes {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretBytes")
            .field("redacted", &true)
            .field("bytes", &self.0.len())
            .finish()
    }
}

/// Stores a validated process environment with redacted diagnostics.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SecretEnv(BTreeMap<String, String>);

impl SecretEnv {
    /// Creates a validated sensitive environment.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when a name is empty, contains `=` or a null
    /// byte, or when a value contains a null byte.
    pub fn new(values: BTreeMap<String, String>) -> Result<Self, SecretError> {
        for (name, value) in &values {
            if name.is_empty() || name.bytes().any(|byte| matches!(byte, b'=' | b'\0')) {
                return Err(SecretError::InvalidEnvName);
            }
            if value.as_bytes().contains(&b'\0') {
                return Err(SecretError::InvalidEnvValue);
            }
        }
        Ok(Self(values))
    }

    /// Borrows entries for child process construction.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Consumes the wrapper and returns environment entries.
    #[must_use]
    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.0
    }

    /// Returns the number of environment entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Reports whether the environment has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Debug for SecretEnv {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretEnv")
            .field("redacted", &true)
            .field("entries", &self.0.len())
            .finish()
    }
}

impl<'de> Deserialize<'de> for SecretEnv {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let values = BTreeMap::<String, String>::deserialize(deserializer)?;
        Self::new(values).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_debug_implementations_redact_values() {
        let secret = "token_value_ThatMustNotAppear";
        let token = DataToken::new(secret).expect("valid token");
        let challenge = LeaseChallenge::new(secret).expect("valid challenge");
        let bytes = SecretBytes::new(secret.as_bytes().to_vec());
        let env = SecretEnv::new(BTreeMap::from([(
            "API_TOKEN".to_owned(),
            secret.to_owned(),
        )]))
        .expect("valid environment");

        for rendered in [
            format!("{token:?}"),
            format!("{challenge:?}"),
            format!("{bytes:?}"),
            format!("{env:?}"),
        ] {
            assert!(
                !rendered.contains(secret),
                "sensitive debug output leaked the seeded value"
            );
            assert!(rendered.contains("redacted"));
        }
    }

    #[test]
    fn environment_deserialization_revalidates_process_rules() {
        let error = serde_json::from_str::<SecretEnv>(r#"{"BAD=NAME":"value"}"#)
            .expect_err("invalid environment name must fail");

        assert!(error.to_string().contains("name is invalid"));
    }
}
