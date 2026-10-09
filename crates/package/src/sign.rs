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
