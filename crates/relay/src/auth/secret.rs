//! Keeps authentication secrets out of diagnostics.

// Rust guideline compliant 2026-09-08

use std::fmt::{Debug, Formatter};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;

/// Holds one raw secret until it leaves process memory.
pub(crate) struct SecretValue(Zeroizing<String>);

impl SecretValue {
    /// Wraps a received secret immediately.
    pub(crate) fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// Borrows the secret only at its protocol or digest boundary.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl Debug for SecretValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretValue")
            .field("redacted", &true)
            .finish()
    }
}

/// Computes versioned keyed digests for durable secret comparisons.
#[derive(Clone)]
pub struct DigestKey {
    key_id: String,
    bytes: Zeroizing<Vec<u8>>,
}

impl DigestKey {
    /// Creates one active digest key from protected runtime configuration.
    #[must_use]
    pub fn new(key_id: String, bytes: Vec<u8>) -> Self {
        Self {
            key_id,
            bytes: Zeroizing::new(bytes),
        }
    }

    /// Returns the configured safe key identifier.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Computes an HMAC-SHA-256 digest without retaining the input.
    pub(crate) fn digest(&self, value: &str) -> [u8; 32] {
        let mut mac = HmacSha256::new_from_slice(&self.bytes)
            .expect("HMAC accepts any key length for SHA-256");
        mac.update(value.as_bytes());
        mac.finalize().into_bytes().into()
    }

    /// Compares one candidate with a stored digest in constant time.
    pub(crate) fn matches(&self, value: &str, stored: &[u8]) -> bool {
        let digest = self.digest(value);
        digest.as_slice().ct_eq(stored).into()
    }
}

impl Debug for DigestKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DigestKey")
            .field("key_id", &self.key_id)
            .field("redacted", &true)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{DigestKey, SecretValue};

    #[test]
    fn diagnostics_redact_secret_material() {
        let secret = "credential-that-must-not-appear";
        let value = SecretValue::new(secret.to_owned());
        let key = DigestKey::new("active".to_owned(), secret.as_bytes().to_vec());

        assert!(!format!("{value:?}").contains(secret));
        assert!(!format!("{key:?}").contains(secret));
    }

    #[test]
    fn keyed_digest_requires_the_exact_secret() {
        let key = DigestKey::new("active".to_owned(), b"test-key".to_vec());
        let digest = key.digest("first");

        assert!(key.matches("first", &digest));
        assert!(!key.matches("second", &digest));
    }
}
