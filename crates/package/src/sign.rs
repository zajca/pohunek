//! Catalog signing for release tooling.
//!
//! Signing takes a key the caller has already obtained; this module reads no
//! key material from anywhere and embeds none. The signature covers
//! [`catalog_signing_message`], the same bytes [`crate::verify_catalog`]
//! recomputes, so a catalog signed here verifies against an anchor holding the
//! signer's public key.

// Rust guideline compliant 2026-10-05

use ed25519_dalek::{Signer as _, SigningKey};

use crate::catalog::encode_hex;
use crate::{
    catalog_signing_message, Catalog, CatalogEnvelope, CatalogError, CatalogSignature, KeyId,
    CATALOG_SCHEMA_VERSION,
};

/// Signs `catalog` with `key` and returns the complete document with no key
/// chain and one signature.
///
/// Ed25519 signatures are deterministic, so the same catalog and key always
/// give the same document.
///
/// # Errors
///
/// Returns [`CatalogError::Schema`] if the catalog cannot be serialized.
pub fn sign_catalog(catalog: Catalog, key: &SigningKey) -> Result<CatalogEnvelope, CatalogError> {
    let message = catalog_signing_message(&catalog)?;
    let signature = CatalogSignature {
        key_id: KeyId::derive(&key.verifying_key()),
        signature: encode_hex(&key.sign(&message).to_bytes()),
    };
    Ok(CatalogEnvelope {
        schema_version: CATALOG_SCHEMA_VERSION,
        catalog,
        key_chain: Vec::new(),
        signatures: vec![signature],
    })
}

/// The deterministic bytes of a catalog document: indented JSON with one
/// trailing newline.
///
/// # Errors
///
/// Returns [`CatalogError::Schema`] if the document cannot be serialized.
pub fn catalog_document_bytes(envelope: &CatalogEnvelope) -> Result<Vec<u8>, CatalogError> {
    let mut bytes = serde_json::to_vec_pretty(envelope).map_err(|_cause| CatalogError::Schema)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{verify_catalog, AnchorFile, BinarySet, Release, RootKey, Sha256Digest};

    const WINDOW_START: u64 = 1;
    const WINDOW_END: u64 = 4_102_444_800;
    const NOW: u64 = 1_000;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn anchor_of(key: &SigningKey) -> crate::TrustAnchor {
        let root =
            RootKey::new(key.verifying_key().to_bytes(), WINDOW_START, WINDOW_END).expect("root");
        AnchorFile::new(vec![root], Vec::new())
            .expect("anchor")
            .trust_anchor()
            .expect("verifier input")
    }

    fn empty_catalog() -> Catalog {
        Catalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            sequence: 3,
            expires_at: WINDOW_END,
            release: Release {
                version: "1.2.3".to_owned(),
                commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                binary_sets: vec![BinarySet {
                    target: "x86_64-unknown-linux-gnu".to_owned(),
                    digest: Sha256Digest::parse(&format!("sha256:{}", "a".repeat(64)))
                        .expect("digest"),
                }],
            },
            revoked_key_ids: Vec::new(),
            revoked_digests: Vec::new(),
            entries: Vec::new(),
        }
    }

    #[test]
    fn a_signed_catalog_verifies_against_the_signers_anchor() {
        let signer = key(1);
        let envelope = sign_catalog(empty_catalog(), &signer).expect("sign");
        let bytes = catalog_document_bytes(&envelope).expect("bytes");
        let verified = verify_catalog(&bytes, &anchor_of(&signer), NOW, None).expect("verify");
        assert_eq!(verified.sequence(), 3);
        assert_eq!(verified.signer(), &KeyId::derive(&signer.verifying_key()));
    }
}
