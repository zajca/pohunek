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
    catalog_document_bytes, sign_catalog, AnchorFile, Attestation, BinarySet, Catalog,
    CatalogEntry, KeyId, Release, RootKey, Sha256Digest, CATALOG_SCHEMA_VERSION,
};

/// Start of every signing window.
pub(crate) const WINDOW_START: u64 = 1;

/// End of every signing window and of a fresh catalog: 2100-01-01.
pub(crate) const WINDOW_END: u64 = 4_102_444_800;

/// A core range every released version satisfies.
pub(crate) const ANY_CORE: &str = ">=0.0.0";

/// Core version of every fixture catalog's release block.
///
/// Far above any real core version so that an entry range such as
/// `>=999.0.0` admits the release while the daemon under test still judges it
/// incompatible with its own version.
pub(crate) const RELEASE_VERSION: &str = "999.0.0";

/// Source commit of every fixture catalog's release block.
const RELEASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// Runtime API version of every fixture entry.
pub(crate) const RUNTIME_API: u32 = 1;

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

fn fixture_digest(byte: u8) -> Sha256Digest {
    Sha256Digest::parse(&format!("sha256:{}", format!("{byte:02x}").repeat(32)))
        .expect("fixture digest")
}

/// One attestation per platform, as a catalog entry requires.
pub(crate) fn attestations_for(platforms: &[String]) -> Vec<Attestation> {
    platforms
        .iter()
        .map(|platform| Attestation {
            platform: platform.clone(),
            digest: fixture_digest(0x77),
        })
        .collect()
}

/// A release block with a binary set for the host platform and for every
/// platform an entry lists.
fn release_for(entries: &[CatalogEntry]) -> Release {
    let mut targets: Vec<String> = entries
        .iter()
        .flat_map(|entry| entry.platforms.iter().cloned())
        .chain(std::iter::once(host_platform()))
        .collect();
    targets.sort();
    targets.dedup();
    Release {
        version: RELEASE_VERSION.to_owned(),
        commit: RELEASE_COMMIT.to_owned(),
        binary_sets: targets
            .into_iter()
            .map(|target| BinarySet {
                target,
                digest: fixture_digest(0x99),
            })
            .collect(),
    }
}

pub(crate) fn catalog_of(sequence: u64, expires_at: u64, entries: Vec<CatalogEntry>) -> Catalog {
    Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence,
        expires_at,
        release: release_for(&entries),
        revoked_key_ids: Vec::new(),
        revoked_digests: Vec::new(),
        entries,
    }
}

/// The signed catalog document bytes for `catalog`.
pub(crate) fn signed(key: &SigningKey, catalog: Catalog) -> Vec<u8> {
    catalog_document_bytes(&sign_catalog(catalog, key).expect("sign")).expect("document")
}
