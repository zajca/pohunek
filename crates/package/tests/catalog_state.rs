// Rust guideline compliant 2026-10-04

//! Behavioural tests of the persisted catalog verifier state.
//!
//! Catalogs are signed with keys derived from fixed labels, so every run signs
//! the same bytes and no test needs a random number generator or the clock.

#![cfg(unix)]

mod common;

use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{mode_of, PluginFixture};
use ed25519_dalek::{Signer as _, SigningKey};
use package::registry::RegistryError;
use package::{
    catalog_signing_message, verify_catalog, Catalog, CatalogEnvelope, CatalogSignature, KeyId,
    RootKey, TrustAnchor, VerifiedCatalog, CATALOG_SCHEMA_VERSION, MAX_REVOKED_KEYS,
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String");
        text
    })
}

/// A catalog with `sequence` that revokes the keys derived from `revoked`.
fn verified(sequence: u64, revoked: &[&str]) -> VerifiedCatalog {
    let root = signing_key("root");
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence,
        expires_at: EXPIRES_AT,
        revoked_key_ids: revoked.iter().map(|label| key_id(label)).collect(),
        revoked_digests: Vec::new(),
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

fn state_file(fixture: &PluginFixture) -> std::path::PathBuf {
    fixture.plugins().join("catalog-state.json")
}

#[test]
fn a_fresh_registry_has_the_empty_catalog_state() {
    let fixture = PluginFixture::new();
    let state = fixture.open().catalog_state().unwrap();
    assert_eq!(state.high_water(), None);
    assert!(state.revoked_key_ids().is_empty());
    assert!(!state_file(&fixture).exists());
}

#[test]
fn a_recorded_catalog_round_trips_across_reopen() {
    let fixture = PluginFixture::new();
    let recorded = fixture
        .open()
        .record_catalog(&verified(7, &["old-a", "old-b"]))
        .unwrap();
    assert_eq!(recorded.high_water(), Some(7));
    assert_eq!(recorded.revoked_key_ids().len(), 2);

    let reread = fixture.open().catalog_state().unwrap();
    assert_eq!(reread, recorded);
    assert!(reread.revoked_key_ids().contains(&key_id("old-a")));
    assert_eq!(mode_of(&state_file(&fixture)), 0o600);
    assert!(
        !fixture.plugins().join("catalog-state.json.tmp").exists(),
        "no temporary is left behind"
    );
}

#[test]
fn the_high_water_mark_only_moves_up() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(7, &[])).unwrap();
    let before = std::fs::read(state_file(&fixture)).unwrap();

    let lower = registry.record_catalog(&verified(6, &["late"])).unwrap();
    assert_eq!(lower.high_water(), Some(7));
    assert!(
        lower.revoked_key_ids().is_empty(),
        "a lower sequence is ignored"
    );
    assert_eq!(std::fs::read(state_file(&fixture)).unwrap(), before);

    let equal = registry.record_catalog(&verified(7, &["same"])).unwrap();
    assert_eq!(equal.high_water(), Some(7));
    assert!(equal.revoked_key_ids().contains(&key_id("same")));

    let higher = registry.record_catalog(&verified(9, &[])).unwrap();
    assert_eq!(higher.high_water(), Some(9));
    assert_eq!(registry.catalog_state().unwrap(), higher);
}

#[test]
fn revoked_key_ids_accumulate_across_catalogs() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(1, &["first"])).unwrap();
    registry.record_catalog(&verified(2, &["second"])).unwrap();
    let state = registry.record_catalog(&verified(3, &[])).unwrap();

    let expected = [key_id("first"), key_id("second")];
    assert_eq!(state.revoked_key_ids().len(), 2);
    assert!(expected
        .iter()
        .all(|id| state.revoked_key_ids().contains(id)));
}

#[test]
fn a_merge_that_changes_nothing_writes_nothing() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(4, &["a"])).unwrap();
    let marker = state_file(&fixture);
    std::fs::remove_file(&marker).unwrap();
    // The stored state is gone, so only a real change recreates the file.
    let state = registry.record_catalog(&verified(4, &["a"])).unwrap();
    assert_eq!(state.high_water(), Some(4));
    assert!(marker.exists());

    let before = std::fs::read(&marker).unwrap();
    registry.record_catalog(&verified(4, &["a"])).unwrap();
    assert_eq!(std::fs::read(&marker).unwrap(), before);
}

#[test]
fn the_revoked_key_cap_is_enforced_without_writing() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let labels: Vec<String> = (0..MAX_REVOKED_KEYS).map(|n| format!("k{n}")).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    registry.record_catalog(&verified(1, &refs)).unwrap();
    let before = std::fs::read(state_file(&fixture)).unwrap();

    assert_eq!(
        registry
            .record_catalog(&verified(2, &["one-too-many"]))
            .unwrap_err(),
        RegistryError::TooManyRevokedKeys
    );
    assert_eq!(std::fs::read(state_file(&fixture)).unwrap(), before);
    assert_eq!(registry.catalog_state().unwrap().high_water(), Some(1));
}

#[test]
fn a_corrupt_state_file_is_reported_and_never_replaced() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(3, &["a", "b"])).unwrap();
    let good = std::fs::read_to_string(state_file(&fixture)).unwrap();
    let a = key_id("a").to_string();
    let b = key_id("b").to_string();
    let (low, high) = if a < b { (&a, &b) } else { (&b, &a) };

    let corruptions: Vec<(&str, Vec<u8>)> = vec![
        ("garbage", b"{ not json".to_vec()),
        ("empty", Vec::new()),
        (
            "unknown field",
            good.replace("\"schema\"", "\"surplus\":1,\"schema\"")
                .into_bytes(),
        ),
        (
            "bad key id",
            good.replace(low.as_str(), "not-a-key-id").into_bytes(),
        ),
        (
            "unsorted ids",
            good.replace(
                &format!("\"{low}\",\"{high}\""),
                &format!("\"{high}\",\"{low}\""),
            )
            .into_bytes(),
        ),
        (
            "duplicate ids",
            good.replace(high.as_str(), low.as_str()).into_bytes(),
        ),
        ("oversized", vec![b' '; 64 * 1024]),
    ];
    for (label, bytes) in corruptions {
        std::fs::write(state_file(&fixture), &bytes).unwrap();
        assert_eq!(
            registry.catalog_state().unwrap_err(),
            RegistryError::Corrupt,
            "{label}"
        );
        assert_eq!(
            registry.record_catalog(&verified(9, &[])).unwrap_err(),
            RegistryError::Corrupt,
            "{label}"
        );
        assert_eq!(
            std::fs::read(state_file(&fixture)).unwrap(),
            bytes,
            "{label}"
        );
    }
}

#[test]
fn an_unknown_schema_version_is_reported() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(3, &[])).unwrap();
    let text = std::fs::read_to_string(state_file(&fixture)).unwrap();
    std::fs::write(
        state_file(&fixture),
        text.replace("\"schema\":1", "\"schema\":2"),
    )
    .unwrap();

    assert_eq!(
        registry.catalog_state().unwrap_err(),
        RegistryError::UnsupportedSchema
    );
    assert_eq!(
        registry.record_catalog(&verified(9, &[])).unwrap_err(),
        RegistryError::UnsupportedSchema
    );
}

#[test]
fn the_state_file_must_be_an_owner_private_regular_file() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_catalog(&verified(3, &[])).unwrap();

    common::set_mode(&state_file(&fixture), 0o644);
    assert_eq!(registry.catalog_state().unwrap_err(), RegistryError::Unsafe);
    common::set_mode(&state_file(&fixture), 0o600);
    registry.catalog_state().unwrap();

    let moved = fixture.base.join("state");
    std::fs::rename(state_file(&fixture), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, state_file(&fixture)).unwrap();
    assert_eq!(registry.catalog_state().unwrap_err(), RegistryError::Unsafe);
}

#[test]
fn a_stale_temporary_does_not_block_the_next_write() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    std::fs::write(fixture.plugins().join("catalog-state.json.tmp"), b"junk").unwrap();
    common::set_mode(&fixture.plugins().join("catalog-state.json.tmp"), 0o600);

    registry.record_catalog(&verified(2, &[])).unwrap();
    assert_eq!(registry.catalog_state().unwrap().high_water(), Some(2));
}

/// Retries `operation` while another writer holds the lock.
fn until_unlocked<T>(mut operation: impl FnMut() -> Result<T, RegistryError>) -> T {
    for _ in 0..10_000_000 {
        match operation() {
            Err(RegistryError::Busy) => std::thread::yield_now(),
            other => return other.unwrap(),
        }
    }
    panic!("the registry lock was never released");
}

#[test]
fn concurrent_writers_lose_no_update() {
    const WRITERS: u64 = 8;
    let fixture = PluginFixture::new();
    fixture.open();
    let catalogs: Vec<VerifiedCatalog> = (1..=WRITERS)
        .map(|n| verified(n, &[format!("writer-{n}").as_str()]))
        .collect();
    let recorded = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for catalog in &catalogs {
            let (fixture, recorded) = (&fixture, &recorded);
            scope.spawn(move || {
                let registry = fixture.open();
                until_unlocked(|| registry.record_catalog(catalog));
                recorded.fetch_add(1, Ordering::Relaxed);
            });
        }
    });

    assert_eq!(recorded.load(Ordering::Relaxed), 8);
    let state = fixture.open().catalog_state().unwrap();
    assert_eq!(state.high_water(), Some(WRITERS));
    // A writer that ran after a higher sequence was stored is ignored, so
    // only the highest catalog's revocations are guaranteed; every stored id
    // belongs to one of the catalogs.
    assert!(state.revoked_key_ids().contains(&key_id("writer-8")));
    assert!(state
        .revoked_key_ids()
        .iter()
        .all(|id| (1..=WRITERS).any(|n| *id == key_id(&format!("writer-{n}")))));
}
