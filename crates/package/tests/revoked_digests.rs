// Rust guideline compliant 2026-10-04

//! Behavioural tests of the persisted revoked package digests.
//!
//! The set lives in `revoked-digests.json`, a sibling of `catalog-state.json`,
//! and is written by [`Registry::record_catalog`]. Catalogs are signed with
//! keys derived from fixed labels, so every run signs the same bytes.

#![cfg(unix)]

mod common;

use std::fmt::Write as _;

use common::{mode_of, PluginFixture};
use ed25519_dalek::{Signer as _, SigningKey};
use package::registry::RegistryError;
use package::revoked_digests::MAX_REVOKED_DIGESTS_TOTAL;
use package::{
    catalog_signing_message, verify_catalog, BinarySet, Catalog, CatalogEnvelope, CatalogSignature,
    KeyId, PackageDigest, Release, RootKey, Sha256Digest, TrustAnchor, VerifiedCatalog,
    CATALOG_SCHEMA_VERSION,
};
use sha2::{Digest as _, Sha256};

const NOW: u64 = 5_000;
const WINDOW_START: u64 = 1_000;
const WINDOW_END: u64 = 9_000;
const EXPIRES_AT: u64 = 8_000;

fn signing_key(label: &str) -> SigningKey {
    let seed: [u8; 32] = Sha256::digest(label.as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn key_id(label: &str) -> KeyId {
    KeyId::derive(&signing_key(label).verifying_key())
}

fn digest(byte: char) -> PackageDigest {
    PackageDigest::parse(&format!("sha256:{}", byte.to_string().repeat(64))).expect("digest")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String");
        text
    })
}

/// A catalog with `sequence` that revokes the digests built from `revoked`.
fn verified(sequence: u64, revoked: &[char]) -> VerifiedCatalog {
    let root = signing_key("root");
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence,
        expires_at: EXPIRES_AT,
        release: Release {
            version: "1.0.0".to_owned(),
            commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            binary_sets: vec![BinarySet {
                target: "x86_64-unknown-linux-gnu".to_owned(),
                digest: Sha256Digest::parse(&format!("sha256:{}", "a".repeat(64))).unwrap(),
            }],
        },
        revoked_key_ids: Vec::new(),
        revoked_digests: revoked.iter().map(|byte| digest(*byte)).collect(),
        entries: Vec::new(),
    };
    let message = catalog_signing_message(&catalog).expect("message");
    let signature = CatalogSignature {
        key_id: key_id("root"),
        signature: hex(&root.sign(&message).to_bytes()),
    };
    let bytes = serde_json::to_vec(&CatalogEnvelope {
        schema_version: CATALOG_SCHEMA_VERSION,
        catalog,
        key_chain: Vec::new(),
        signatures: vec![signature],
    })
    .expect("serialize");
    let anchor = TrustAnchor::new(
        vec![RootKey::new(root.verifying_key().to_bytes(), WINDOW_START, WINDOW_END).unwrap()],
        Vec::new(),
    )
    .expect("anchor");
    verify_catalog(&bytes, &anchor, NOW, None).expect("valid catalog")
}

fn revoked_file(fixture: &PluginFixture) -> std::path::PathBuf {
    fixture.plugins().join("revoked-digests.json")
}

fn state_file(fixture: &PluginFixture) -> std::path::PathBuf {
    fixture.plugins().join("catalog-state.json")
}

#[test]
fn a_revoked_digest_survives_the_catalog_that_carried_it() {
    let fixture = PluginFixture::new();
    fixture.open().record_catalog(&verified(1, &['a'])).unwrap();

    // A later catalog that omits the digest does not resurrect it, across a
    // reopen of the registry.
    fixture.open().record_catalog(&verified(2, &['b'])).unwrap();
    fixture.open().record_catalog(&verified(3, &[])).unwrap();

    let revoked = fixture.open().revoked_digests().unwrap();
    assert!(revoked.contains(&digest('a')));
    assert!(revoked.contains(&digest('b')));
    assert_eq!(revoked.len(), 2);
    assert_eq!(mode_of(&revoked_file(&fixture)), 0o600);
    assert!(!fixture.plugins().join("revoked-digests.json.tmp").exists());
}

#[test]
fn an_older_catalog_changes_no_revocation() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(5, &['a'])).unwrap();
    let before = std::fs::read(revoked_file(&fixture)).unwrap();

    registry.record_catalog(&verified(4, &['f'])).unwrap();

    assert_eq!(std::fs::read(revoked_file(&fixture)).unwrap(), before);
    assert!(!registry.revoked_digests().unwrap().contains(&digest('f')));
}

#[test]
fn an_equal_sequence_catalog_with_new_digests_is_recorded() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(5, &['a'])).unwrap();
    let state_before = std::fs::read(state_file(&fixture)).unwrap();

    registry.record_catalog(&verified(5, &['a', 'b'])).unwrap();

    assert!(registry.revoked_digests().unwrap().contains(&digest('b')));
    assert_eq!(
        std::fs::read(state_file(&fixture)).unwrap(),
        state_before,
        "the catalog state itself had nothing to change"
    );
}

#[test]
fn a_catalog_without_revoked_digests_writes_no_revocation_file() {
    let fixture = PluginFixture::new();
    fixture.open().record_catalog(&verified(1, &[])).unwrap();

    assert!(!revoked_file(&fixture).exists());
    assert!(fixture.open().revoked_digests().unwrap().is_empty());
}

#[test]
fn the_catalog_state_stays_a_schema_one_file_for_a_rollback() {
    let fixture = PluginFixture::new();
    fixture.open().record_catalog(&verified(7, &['a'])).unwrap();

    // The previous release reads exactly these keys at schema 1 and rejects
    // any other key or schema.
    let text = std::fs::read_to_string(state_file(&fixture)).unwrap();
    assert_eq!(
        text, "{\"schema\":1,\"high_water\":7,\"revoked_key_ids\":[]}",
        "the state file keeps the shape the previous release parses"
    );
    assert!(
        !text.contains("revoked_digests"),
        "no digest leaks into the schema 1 file"
    );
    assert_eq!(
        fixture.open().catalog_state().unwrap().high_water(),
        Some(7)
    );
}

#[test]
fn a_damaged_revocation_file_is_reported_and_never_replaced() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(1, &['a', 'b'])).unwrap();
    let good = std::fs::read_to_string(revoked_file(&fixture)).unwrap();
    let low = digest('a').to_string();
    let high = digest('b').to_string();

    let corruptions: Vec<(&str, Vec<u8>)> = vec![
        ("garbage", b"{ not json".to_vec()),
        ("empty", Vec::new()),
        (
            "unknown field",
            good.replace("\"schema\"", "\"surplus\":1,\"schema\"")
                .into_bytes(),
        ),
        (
            "unsorted digests",
            good.replace(
                &format!("\"{low}\",\"{high}\""),
                &format!("\"{high}\",\"{low}\""),
            )
            .into_bytes(),
        ),
        (
            "duplicate digests",
            good.replace(high.as_str(), low.as_str()).into_bytes(),
        ),
        ("oversized", vec![b' '; 3 * 1024 * 1024]),
    ];
    for (label, bytes) in corruptions {
        std::fs::write(revoked_file(&fixture), &bytes).unwrap();
        assert_eq!(
            registry.revoked_digests().unwrap_err(),
            RegistryError::Corrupt,
            "{label}"
        );
        assert_eq!(
            registry.record_catalog(&verified(9, &[])).unwrap_err(),
            RegistryError::Corrupt,
            "{label}"
        );
        assert_eq!(
            std::fs::read(revoked_file(&fixture)).unwrap(),
            bytes,
            "{label}"
        );
    }

    std::fs::write(
        revoked_file(&fixture),
        good.replace("\"schema\":1", "\"schema\":2"),
    )
    .unwrap();
    assert_eq!(
        registry.revoked_digests().unwrap_err(),
        RegistryError::UnsupportedSchema
    );
}

#[test]
fn the_accumulated_set_is_capped_and_a_refused_record_writes_nothing() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(1, &['a'])).unwrap();
    let before = std::fs::read(revoked_file(&fixture)).unwrap();

    // Fill the persisted set to the cap with distinct synthetic digests, then
    // ask for one more.
    let full: Vec<String> = (0..MAX_REVOKED_DIGESTS_TOTAL)
        .map(|index| format!("sha256:{index:064x}"))
        .collect();
    let mut sorted = full.clone();
    sorted.sort();
    let body = serde_json::json!({"schema": 1, "revoked_digests": sorted});
    std::fs::write(revoked_file(&fixture), serde_json::to_vec(&body).unwrap()).unwrap();
    let full_bytes = std::fs::read(revoked_file(&fixture)).unwrap();

    assert_eq!(
        registry.record_catalog(&verified(2, &['b'])).unwrap_err(),
        RegistryError::TooManyRevokedDigests
    );
    assert_eq!(std::fs::read(revoked_file(&fixture)).unwrap(), full_bytes);
    assert_eq!(
        registry.catalog_state().unwrap().high_water(),
        Some(1),
        "the watermark did not move"
    );
    assert_ne!(before, full_bytes);
}

#[test]
fn a_stale_temporary_does_not_block_the_next_revocation_write() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let stale = fixture.plugins().join("revoked-digests.json.tmp");
    std::fs::write(&stale, b"junk").unwrap();
    common::set_mode(&stale, 0o600);

    registry.record_catalog(&verified(2, &['c'])).unwrap();

    assert!(registry.revoked_digests().unwrap().contains(&digest('c')));
}
