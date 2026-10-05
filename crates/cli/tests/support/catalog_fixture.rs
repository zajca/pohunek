//! Throwaway catalog trust for tests that install an official package.
//!
//! The signing key is generated from random bytes inside the test process and
//! never leaves it; the anchor and catalog built here exist only for one test.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-06

use ed25519_dalek::SigningKey;
use package::{
    catalog_document_bytes, sign_catalog, AnchorFile, Catalog, CatalogEntry, KeyId, RootKey,
    CATALOG_SCHEMA_VERSION,
};

/// Start of every signing window.
pub(crate) const WINDOW_START: u64 = 1;

/// End of every signing window and of a fresh catalog: 2100-01-01.
pub(crate) const WINDOW_END: u64 = 4_102_444_800;

/// A core range every released version satisfies.
pub(crate) const ANY_CORE: &str = ">=0.0.0";

/// A fresh test keypair; the seed is random and exists only in this process.
pub(crate) fn test_key() -> SigningKey {
    let mut seed = [0_u8; 32];
    getrandom::getrandom(&mut seed).expect("operating system randomness");
    SigningKey::from_bytes(&seed)
}

pub(crate) fn key_id(key: &SigningKey) -> KeyId {
    KeyId::derive(&key.verifying_key())
}

pub(crate) fn root_of(key: &SigningKey, not_before: u64, not_after: u64) -> RootKey {
    RootKey::new(key.verifying_key().to_bytes(), not_before, not_after).expect("root key")
}

/// The anchor file bytes trusting `key` for the whole signing window.
pub(crate) fn anchor_for(key: &SigningKey) -> Vec<u8> {
    anchor_with(key, WINDOW_START, WINDOW_END, Vec::new())
}

pub(crate) fn anchor_with(
    key: &SigningKey,
    not_before: u64,
    not_after: u64,
    revoked: Vec<KeyId>,
) -> Vec<u8> {
    AnchorFile::new(vec![root_of(key, not_before, not_after)], revoked)
        .expect("anchor")
        .to_bytes()
        .expect("anchor bytes")
}

/// The platform name the daemon matches catalog entries against.
pub(crate) fn host_platform() -> String {
    let arch = std::env::consts::ARCH;
    if cfg!(target_os = "macos") {
        format!("{arch}-apple-darwin")
    } else if cfg!(target_env = "musl") {
        format!("{arch}-unknown-linux-musl")
    } else {
        format!("{arch}-unknown-linux-gnu")
    }
}

pub(crate) fn catalog_of(sequence: u64, expires_at: u64, entries: Vec<CatalogEntry>) -> Catalog {
    Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence,
        expires_at,
        revoked_key_ids: Vec::new(),
        revoked_digests: Vec::new(),
        entries,
    }
}

/// The signed catalog document bytes for `catalog`.
pub(crate) fn signed(key: &SigningKey, catalog: Catalog) -> Vec<u8> {
    catalog_document_bytes(&sign_catalog(catalog, key).expect("sign")).expect("document")
}
