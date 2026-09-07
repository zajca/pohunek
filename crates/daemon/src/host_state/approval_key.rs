//! Private Ed25519 approval-key storage codec.
//!
//! The binary record keeps a host binding, public key, and signing seed outside
//! JSON governance state. The public wrapper never exposes the seed.

// Rust guideline compliant 2026-09-03

use std::fmt::{Debug, Formatter};

use base64::prelude::{Engine as _, BASE64_URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use protocol::{ApprovalKeyReference, HostApprovalSignature, HostId};
use zeroize::{Zeroize as _, Zeroizing};

use super::repository::HostStateRepositoryError;

/// Exact approval-key magic, including the required terminating NUL byte.
const APPROVAL_KEY_MAGIC: [u8; 8] = *b"PHAPKEY\0";
/// First and only supported binary approval-key format.
const APPROVAL_KEY_FORMAT_VERSION: u8 = 1;
/// RFC 8032 Ed25519 approval-key algorithm tag.
const ED25519_ALGORITHM: u8 = 1;
/// Exact byte length of one version-one approval-key record.
pub(crate) const APPROVAL_KEY_RECORD_BYTES: usize = 106;
const MAGIC_RANGE: std::ops::Range<usize> = 0..8;
const FORMAT_VERSION_INDEX: usize = 8;
const ALGORITHM_INDEX: usize = 9;
const HOST_ID_RANGE: std::ops::Range<usize> = 10..42;
const VERIFYING_KEY_RANGE: std::ops::Range<usize> = 42..74;
const SIGNING_SEED_RANGE: std::ops::Range<usize> = 74..106;
const ED25519_SIGNATURE_BYTES: usize = 64;

/// Owner-private approval signer bound to one stable host identity.
pub(crate) struct ApprovalKey {
    signing_key: SigningKey,
    reference: ApprovalKeyReference,
}

impl Debug for ApprovalKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApprovalKey([REDACTED])")
    }
}

impl ApprovalKey {
    pub(crate) fn generate() -> Result<Self, HostStateRepositoryError> {
        let mut seed = Zeroizing::new([0_u8; 32]);
        fill_entropy(&mut *seed, "approval key")?;
        Self::from_seed(&seed)
    }

    fn from_seed(seed: &[u8; 32]) -> Result<Self, HostStateRepositoryError> {
        let signing_key = SigningKey::from_bytes(seed);
        let verifying_key = signing_key.verifying_key();
        validate_verifying_key(verifying_key.to_bytes())?;
        Ok(Self {
            signing_key,
            reference: ApprovalKeyReference::from_ed25519_verifying_key_bytes(
                verifying_key.to_bytes(),
            ),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_test_seed(seed: &[u8; 32]) -> Result<Self, HostStateRepositoryError> {
        Self::from_seed(seed)
    }

    pub(crate) fn reference(&self) -> &ApprovalKeyReference {
        &self.reference
    }

    pub(crate) fn sign(
        &self,
        payload: &[u8],
    ) -> Result<HostApprovalSignature, HostStateRepositoryError> {
        let encoded = BASE64_URL_SAFE_NO_PAD.encode(self.signing_key.sign(payload).to_bytes());
        HostApprovalSignature::parse(&format!("sig_{encoded}")).map_err(|_error| {
            HostStateRepositoryError::InvalidRecord {
                record: "approval.key",
            }
        })
    }

    pub(crate) fn verify_strict(&self, payload: &[u8], signature: &HostApprovalSignature) -> bool {
        let Some(encoded) = signature.as_str().strip_prefix("sig_") else {
            return false;
        };
        let Ok(bytes) = BASE64_URL_SAFE_NO_PAD.decode(encoded) else {
            return false;
        };
        let Ok(bytes) = <[u8; ED25519_SIGNATURE_BYTES]>::try_from(bytes.as_slice()) else {
            return false;
        };
        self.signing_key
            .verifying_key()
            .verify_strict(payload, &Signature::from_bytes(&bytes))
            .is_ok()
    }

    /// Encodes this private record into a zeroizing persistence buffer.
    ///
    /// The buffer deliberately contains the signing seed until the
    /// descriptor-relative repository writer consumes it.
    fn encoded(&self, host_id: &HostId) -> Result<Zeroizing<Vec<u8>>, HostStateRepositoryError> {
        let host_bytes = host_id_bytes(host_id)?;
        let verifying_key = self.signing_key.verifying_key().to_bytes();
        let seed = Zeroizing::new(self.signing_key.to_bytes());
        let mut bytes = Zeroizing::new(Vec::with_capacity(APPROVAL_KEY_RECORD_BYTES));
        bytes.extend_from_slice(&APPROVAL_KEY_MAGIC);
        bytes.push(APPROVAL_KEY_FORMAT_VERSION);
        bytes.push(ED25519_ALGORITHM);
        bytes.extend_from_slice(&host_bytes);
        bytes.extend_from_slice(&verifying_key);
        bytes.extend_from_slice(&*seed);
        Ok(bytes)
    }
}

/// Parsed approval-key record bound to its stable host identity.
#[derive(Debug)]
pub(crate) struct ApprovalKeyRecord {
    pub(crate) host_id: HostId,
    pub(crate) key: ApprovalKey,
}

impl ApprovalKeyRecord {
    pub(crate) fn generate(host_id: &HostId) -> Result<Self, HostStateRepositoryError> {
        Ok(Self {
            host_id: host_id.clone(),
            key: ApprovalKey::generate()?,
        })
    }

    pub(crate) fn encode(&self) -> Result<Zeroizing<Vec<u8>>, HostStateRepositoryError> {
        self.key.encoded(&self.host_id)
    }

    /// Decodes a private record owned by a zeroizing read buffer.
    pub(crate) fn decode(bytes: &Zeroizing<Vec<u8>>) -> Result<Self, HostStateRepositoryError> {
        if bytes.len() != APPROVAL_KEY_RECORD_BYTES
            || bytes[MAGIC_RANGE] != APPROVAL_KEY_MAGIC
            || bytes[FORMAT_VERSION_INDEX] != APPROVAL_KEY_FORMAT_VERSION
            || bytes[ALGORITHM_INDEX] != ED25519_ALGORITHM
        {
            return Err(HostStateRepositoryError::InvalidRecord {
                record: "approval.key",
            });
        }
        let host_id = host_id_from_bytes(bytes[HOST_ID_RANGE].try_into().map_err(|_error| {
            HostStateRepositoryError::InvalidRecord {
                record: "approval.key",
            }
        })?)?;
        let verifying_bytes: [u8; 32] =
            bytes[VERIFYING_KEY_RANGE].try_into().map_err(|_error| {
                HostStateRepositoryError::InvalidRecord {
                    record: "approval.key",
                }
            })?;
        validate_verifying_key(verifying_bytes)?;
        let seed = Zeroizing::new(bytes[SIGNING_SEED_RANGE].try_into().map_err(|_error| {
            HostStateRepositoryError::InvalidRecord {
                record: "approval.key",
            }
        })?);
        let key = ApprovalKey::from_seed(&seed)?;
        if key.reference().ed25519_verifying_key_bytes() != verifying_bytes {
            return Err(HostStateRepositoryError::InvalidRecord {
                record: "approval.key",
            });
        }
        Ok(Self { host_id, key })
    }
}

pub(crate) fn generate_host_id() -> Result<HostId, HostStateRepositoryError> {
    let mut bytes = [0_u8; 32];
    fill_entropy(&mut bytes, "host identity")?;
    let result = host_id_from_bytes(bytes);
    bytes.zeroize();
    result
}

fn validate_verifying_key(bytes: [u8; 32]) -> Result<VerifyingKey, HostStateRepositoryError> {
    let key = VerifyingKey::from_bytes(&bytes).map_err(|_error| {
        HostStateRepositoryError::InvalidRecord {
            record: "approval.key",
        }
    })?;
    if key.is_weak() {
        return Err(HostStateRepositoryError::InvalidRecord {
            record: "approval.key",
        });
    }
    Ok(key)
}

fn host_id_bytes(host_id: &HostId) -> Result<[u8; 32], HostStateRepositoryError> {
    let Some(payload) = host_id.as_str().strip_prefix("host_") else {
        return Err(HostStateRepositoryError::InvalidRecord {
            record: "identity.json",
        });
    };
    BASE64_URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(HostStateRepositoryError::InvalidRecord {
            record: "identity.json",
        })
}

fn host_id_from_bytes(bytes: [u8; 32]) -> Result<HostId, HostStateRepositoryError> {
    HostId::parse(&format!("host_{}", BASE64_URL_SAFE_NO_PAD.encode(bytes))).map_err(|_error| {
        HostStateRepositoryError::InvalidRecord {
            record: "identity.json",
        }
    })
}

fn fill_entropy(bytes: &mut [u8], stage: &'static str) -> Result<(), HostStateRepositoryError> {
    #[cfg(test)]
    if ENTROPY_FAILURE.with(std::cell::Cell::get) {
        ENTROPY_FAILURE.with(|failure| failure.set(false));
        return Err(HostStateRepositoryError::Entropy { stage });
    }
    getrandom::getrandom(bytes).map_err(|_error| HostStateRepositoryError::Entropy { stage })
}

#[cfg(test)]
thread_local! {
    static ENTROPY_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_entropy() {
    ENTROPY_FAILURE.with(|failure| failure.set(true));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_key_and_encoded_record_have_zeroizing_ownership_boundaries() {
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}

        assert_zeroize_on_drop::<SigningKey>();

        let host_id = host_id_from_bytes([1; 32]).expect("host identity");
        let record = ApprovalKeyRecord::generate(&host_id).expect("approval key");
        let encoded: Zeroizing<Vec<u8>> = record.encode().expect("encode approval key");

        assert_eq!(encoded.len(), APPROVAL_KEY_RECORD_BYTES);
        let decoded = ApprovalKeyRecord::decode(&encoded).expect("decode approval key");
        assert_eq!(decoded.host_id, host_id);
        assert_eq!(decoded.key.reference(), record.key.reference());
    }

    #[test]
    fn approval_key_decoder_requires_a_zeroizing_record_owner() {
        fn decode_secret_record(
            decode: fn(&Zeroizing<Vec<u8>>) -> Result<ApprovalKeyRecord, HostStateRepositoryError>,
            bytes: &Zeroizing<Vec<u8>>,
        ) -> Result<ApprovalKeyRecord, HostStateRepositoryError> {
            decode(bytes)
        }

        let bytes = Zeroizing::new(Vec::new());
        decode_secret_record(ApprovalKeyRecord::decode, &bytes)
            .expect_err("empty private record is rejected through the zeroizing decoder boundary");
    }

    #[test]
    fn debug_output_redacts_the_approval_signing_seed() {
        let seed = [0x5a; 32];
        let seed_text = BASE64_URL_SAFE_NO_PAD.encode(seed);
        let host_id = host_id_from_bytes([1; 32]).expect("host identity");
        let key = ApprovalKey::from_test_seed(&seed).expect("approval key");
        let record = ApprovalKeyRecord {
            host_id,
            key: ApprovalKey::from_test_seed(&seed).expect("approval key"),
        };

        let key_debug = format!("{key:?}");
        let record_debug = format!("{record:?}");
        assert_eq!(key_debug, "ApprovalKey([REDACTED])");
        assert!(record_debug.contains("ApprovalKey([REDACTED])"));
        assert!(!key_debug.contains(&seed_text));
        assert!(!record_debug.contains(&seed_text));
    }

    #[test]
    fn rejects_weak_and_invalid_external_verifying_key_candidates() {
        validate_verifying_key([0; 32]).expect_err("all-zero Ed25519 key is weak");
        let has_invalid_candidate = (0_u8..=u8::MAX)
            .map(|byte| [byte; 32])
            .any(|candidate| validate_verifying_key(candidate).is_err());
        assert!(
            has_invalid_candidate,
            "Ed25519 rejects at least one malformed encoding"
        );
    }

    #[test]
    fn exact_binary_record_rejects_every_header_and_key_binding_tamper() {
        let host_id = host_id_from_bytes([1; 32]).expect("host identity");
        let record = ApprovalKeyRecord::generate(&host_id).expect("approval key");
        let encoded = record.encode().expect("encode approval key");
        assert_eq!(encoded.len(), APPROVAL_KEY_RECORD_BYTES);

        let mut wrong_magic = encoded.clone();
        wrong_magic[0] ^= 1;
        let mut wrong_version = encoded.clone();
        wrong_version[FORMAT_VERSION_INDEX] += 1;
        let mut wrong_algorithm = encoded.clone();
        wrong_algorithm[ALGORITHM_INDEX] += 1;
        let mut trailing = encoded.clone();
        trailing.push(0);
        let mut weak_verifying_key = encoded.clone();
        weak_verifying_key[VERIFYING_KEY_RANGE].fill(0);
        let malformed_candidate = (0_u8..=u8::MAX)
            .map(|byte| [byte; 32])
            .find(|candidate| VerifyingKey::from_bytes(candidate).is_err())
            .expect("Ed25519 exposes one malformed candidate encoding");
        let mut malformed_verifying_key = encoded.clone();
        malformed_verifying_key[VERIFYING_KEY_RANGE].copy_from_slice(&malformed_candidate);
        let mut seed_public_mismatch = encoded.clone();
        seed_public_mismatch[SIGNING_SEED_RANGE.start] ^= 1;

        for (case, bytes) in [
            ("wrong magic", wrong_magic),
            ("wrong version", wrong_version),
            ("wrong algorithm", wrong_algorithm),
            ("trailing byte", trailing),
            ("weak verifying key", weak_verifying_key),
            ("malformed verifying key", malformed_verifying_key),
            ("seed/public mismatch", seed_public_mismatch),
        ] {
            assert!(
                matches!(
                    ApprovalKeyRecord::decode(&bytes),
                    Err(HostStateRepositoryError::InvalidRecord {
                        record: "approval.key"
                    })
                ),
                "{case} must fail closed"
            );
        }
    }
}
