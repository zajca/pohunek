// Rust guideline compliant 2026-10-04

//! Behavioural tests of the signed runtime catalog verifier.
//!
//! Keys derive from fixed labels, so every run signs the same bytes and no
//! test needs a random number generator or the host clock.

use std::fmt::Write as _;

use ed25519_dalek::{Signer as _, SigningKey};
use package::{
    catalog_signing_message, key_record_signing_message, verify_catalog, Authorization, Catalog,
    CatalogEntry, CatalogEntryRejection, CatalogEnvelope, CatalogError, CatalogLimit,
    CatalogSignature, KeyId, LocalAuthorization, LocalTrust, LocalTrustError, PackageDigest,
    RootKey, SignedKeyRecord, TrustAnchor, TrustAnchorError, CATALOG_SCHEMA_VERSION,
    MAX_CATALOG_BYTES, MAX_CATALOG_ENTRIES,
};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const NOW: u64 = 5_000;
const WINDOW_START: u64 = 1_000;
const WINDOW_END: u64 = 9_000;
const EXPIRES_AT: u64 = 8_000;
const SEQUENCE: u64 = 7;

fn signing_key(label: &str) -> SigningKey {
    let seed: [u8; 32] = Sha256::digest(label.as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn key_id(key: &SigningKey) -> KeyId {
    KeyId::derive(&key.verifying_key())
}

fn root(key: &SigningKey, not_before: u64, not_after: u64) -> RootKey {
    RootKey::new(key.verifying_key().to_bytes(), not_before, not_after).expect("valid root")
}

fn anchor_for(key: &SigningKey) -> TrustAnchor {
    TrustAnchor::new(vec![root(key, WINDOW_START, WINDOW_END)], Vec::new()).expect("anchor")
}

fn digest(byte: char) -> PackageDigest {
    PackageDigest::parse(&format!("sha256:{}", byte.to_string().repeat(64))).expect("digest")
}

fn entry(package: &str, runtime: &str, version: &str, digest_char: char) -> CatalogEntry {
    CatalogEntry {
        package_id: PackageId::parse(package).expect("package id"),
        runtime_id: RuntimeId::parse(runtime).expect("runtime id"),
        version: PackageVersion::parse(version).expect("version"),
        digest: digest(digest_char),
        platforms: vec!["x86_64-unknown-linux-gnu".to_owned()],
        core: ">=0.33.0, <0.40.0".to_owned(),
    }
}

fn catalog(sequence: u64) -> Catalog {
    Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence,
        expires_at: EXPIRES_AT,
        revoked_key_ids: Vec::new(),
        revoked_digests: Vec::new(),
        entries: vec![
            entry("pohunek.runtime.codex", "codex", "1.0.0", 'a'),
            entry("pohunek.runtime.claude", "claude", "1.0.0", 'b'),
        ],
    }
}

fn sign(key: &SigningKey, catalog: &Catalog) -> CatalogSignature {
    let message = catalog_signing_message(catalog).expect("message");
    CatalogSignature {
        key_id: key_id(key),
        signature: hex(&key.sign(&message).to_bytes()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String");
        text
    })
}

fn record(
    endorser: &SigningKey,
    subject: &SigningKey,
    not_before: u64,
    not_after: u64,
) -> SignedKeyRecord {
    let mut record = SignedKeyRecord {
        key_id: key_id(subject),
        public_key: hex(subject.verifying_key().as_bytes()),
        not_before,
        not_after,
        endorsed_by: key_id(endorser),
        signature: String::new(),
    };
    record.signature = hex(&endorser
        .sign(&key_record_signing_message(&record))
        .to_bytes());
    record
}

fn document(catalog: Catalog, chain: Vec<SignedKeyRecord>, sigs: Vec<CatalogSignature>) -> Vec<u8> {
    serde_json::to_vec(&CatalogEnvelope {
        schema_version: CATALOG_SCHEMA_VERSION,
        catalog,
        key_chain: chain,
        signatures: sigs,
    })
    .expect("serialize")
}

fn signed_by(key: &SigningKey, catalog: Catalog) -> Vec<u8> {
    let signature = sign(key, &catalog);
    document(catalog, Vec::new(), vec![signature])
}

fn verify(bytes: &[u8], anchor: &TrustAnchor) -> Result<package::VerifiedCatalog, CatalogError> {
    verify_catalog(bytes, anchor, NOW, None)
}

fn mutate(bytes: &[u8], edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(bytes).expect("json");
    edit(&mut value);
    serde_json::to_vec(&value).expect("serialize")
}

#[test]
fn valid_catalog_verifies_and_authorizes_official_packages() {
    let root_key = signing_key("root");
    let bytes = signed_by(&root_key, catalog(SEQUENCE));
    let verified = verify(&bytes, &anchor_for(&root_key)).expect("valid catalog");

    assert_eq!(verified.sequence(), SEQUENCE);
    assert_eq!(verified.expires_at(), EXPIRES_AT);
    assert_eq!(verified.signer(), &key_id(&root_key));
    assert_eq!(verified.entries().len(), 2);
    let codex = &verified.entries()[0];
    assert!(codex.supports_platform("x86_64-unknown-linux-gnu"));
    assert!(!codex.supports_platform("aarch64-apple-darwin"));
    assert!(codex.supports_core(&semver::Version::new(0, 33, 1)));
    assert!(!codex.supports_core(&semver::Version::new(0, 40, 0)));

    let package = PackageId::parse("pohunek.runtime.codex").unwrap();
    assert_eq!(
        verified.authorize(&package, &RuntimeId::codex(), &digest('a')),
        Authorization::Official
    );
}

#[test]
fn authorization_requires_package_runtime_and_digest_to_match() {
    let root_key = signing_key("root");
    let verified = verify(
        &signed_by(&root_key, catalog(SEQUENCE)),
        &anchor_for(&root_key),
    )
    .unwrap();
    let codex_package = PackageId::parse("pohunek.runtime.codex").unwrap();
    let other_package = PackageId::parse("evil.codex").unwrap();

    // Wrong package id claiming an official alias with an official digest.
    assert_eq!(
        verified.authorize(&other_package, &RuntimeId::codex(), &digest('a')),
        Authorization::NotAuthorized
    );
    // Right package, wrong runtime alias.
    assert_eq!(
        verified.authorize(&codex_package, &RuntimeId::claude(), &digest('a')),
        Authorization::NotAuthorized
    );
    // Right package, unknown digest.
    assert_eq!(
        verified.authorize(&codex_package, &RuntimeId::codex(), &digest('c')),
        Authorization::NotAuthorized
    );
}

#[test]
fn shell_is_never_authorized_even_with_a_matching_digest() {
    let root_key = signing_key("root");
    let verified = verify(
        &signed_by(&root_key, catalog(SEQUENCE)),
        &anchor_for(&root_key),
    )
    .unwrap();
    let package = PackageId::parse("pohunek.runtime.codex").unwrap();
    assert_eq!(
        verified.authorize(&package, &RuntimeId::shell(), &digest('a')),
        Authorization::NotAuthorized
    );
}

#[test]
fn catalog_entry_claiming_shell_is_rejected() {
    let root_key = signing_key("root");
    let mut body = catalog(SEQUENCE);
    body.entries
        .push(entry("evil.shell", "shell", "1.0.0", 'c'));
    let error = verify(&signed_by(&root_key, body), &anchor_for(&root_key)).unwrap_err();
    assert_eq!(
        error,
        CatalogError::Entry {
            index: 2,
            reason: CatalogEntryRejection::ReservedShell
        }
    );
}

#[test]
fn tampered_content_fails_signature() {
    let root_key = signing_key("root");
    let bytes = signed_by(&root_key, catalog(SEQUENCE));
    let tampered = mutate(&bytes, |value| {
        value["catalog"]["entries"][0]["digest"] = Value::String(digest('d').to_string());
    });
    assert_eq!(
        verify(&tampered, &anchor_for(&root_key)).unwrap_err(),
        CatalogError::BadSignature
    );
}

#[test]
fn signature_from_unknown_key_is_rejected() {
    let root_key = signing_key("root");
    let attacker = signing_key("attacker");
    let bytes = signed_by(&attacker, catalog(SEQUENCE));
    assert_eq!(
        verify(&bytes, &anchor_for(&root_key)).unwrap_err(),
        CatalogError::UnknownSigner
    );
}

#[test]
fn signature_by_another_key_under_a_trusted_key_id_is_rejected() {
    let root_key = signing_key("root");
    let attacker = signing_key("attacker");
    let body = catalog(SEQUENCE);
    let mut forged = sign(&attacker, &body);
    forged.key_id = key_id(&root_key);
    let bytes = document(body, Vec::new(), vec![forged]);
    assert_eq!(
        verify(&bytes, &anchor_for(&root_key)).unwrap_err(),
        CatalogError::BadSignature
    );
}

#[test]
fn signing_key_outside_its_window_is_rejected() {
    let root_key = signing_key("root");
    let bytes = signed_by(&root_key, catalog(SEQUENCE));
    let anchor = anchor_for(&root_key);

    assert_eq!(
        verify_catalog(&bytes, &anchor, WINDOW_START - 1, None).unwrap_err(),
        CatalogError::SignerNotYetValid
    );
    // The window is half-open: `not_after` itself is already expired, and the
    // catalog's own expiry (EXPIRES_AT) is before it, so use a catalog that
    // outlives the key.
    let mut long_lived = catalog(SEQUENCE);
    long_lived.expires_at = WINDOW_END + 100;
    let bytes = signed_by(&root_key, long_lived);
    assert_eq!(
        verify_catalog(&bytes, &anchor, WINDOW_END, None).unwrap_err(),
        CatalogError::SignerExpired
    );
    verify_catalog(&bytes, &anchor, WINDOW_END - 1, None).expect("verifies");
}

#[test]
fn key_revoked_by_the_anchor_is_rejected() {
    let root_key = signing_key("root");
    let bytes = signed_by(&root_key, catalog(SEQUENCE));
    let anchor = TrustAnchor::new(
        vec![root(&root_key, WINDOW_START, WINDOW_END)],
        vec![key_id(&root_key)],
    )
    .unwrap();
    assert_eq!(
        verify(&bytes, &anchor).unwrap_err(),
        CatalogError::SignerRevoked
    );
}

#[test]
fn catalog_that_revokes_its_own_signer_is_rejected() {
    let root_key = signing_key("root");
    let mut body = catalog(SEQUENCE);
    body.revoked_key_ids.push(key_id(&root_key));
    assert_eq!(
        verify(&signed_by(&root_key, body), &anchor_for(&root_key)).unwrap_err(),
        CatalogError::SignerRevoked
    );
}

#[test]
fn rotation_chain_accepts_a_catalog_signed_by_the_successor() {
    let root_key = signing_key("root");
    let first = signing_key("signer-1");
    let second = signing_key("signer-2");
    let chain = vec![
        record(&root_key, &first, 2_000, 8_500),
        record(&first, &second, 3_000, 8_400),
    ];
    let body = catalog(SEQUENCE);
    let signature = sign(&second, &body);
    let bytes = document(body, chain, vec![signature]);
    let verified = verify(&bytes, &anchor_for(&root_key)).expect("chained signer");
    assert_eq!(verified.signer(), &key_id(&second));
}

#[test]
fn rotation_chain_rejects_a_revoked_intermediate_key() {
    let root_key = signing_key("root");
    let first = signing_key("signer-1");
    let second = signing_key("signer-2");
    let chain = vec![
        record(&root_key, &first, 2_000, 8_500),
        record(&first, &second, 3_000, 8_400),
    ];
    let mut body = catalog(SEQUENCE);
    body.revoked_key_ids.push(key_id(&first));
    let signature = sign(&second, &body);
    let bytes = document(body, chain, vec![signature]);
    assert_eq!(
        verify(&bytes, &anchor_for(&root_key)).unwrap_err(),
        CatalogError::SignerRevoked
    );
}

#[test]
fn rotation_chain_requires_every_ancestor_to_be_valid_now() {
    let root_key = signing_key("root");
    let first = signing_key("signer-1");
    let chain = vec![record(&root_key, &first, 2_000, 8_500)];
    let body = catalog(SEQUENCE);
    let signature = sign(&first, &body);
    let bytes = document(body, chain, vec![signature]);
    // The signer is valid at NOW but its root has expired.
    let anchor = TrustAnchor::new(vec![root(&root_key, WINDOW_START, 4_000)], Vec::new()).unwrap();
    assert_eq!(
        verify(&bytes, &anchor).unwrap_err(),
        CatalogError::SignerExpired
    );
}

#[test]
fn rotation_chain_rejects_broken_records() {
    let root_key = signing_key("root");
    let first = signing_key("signer-1");
    let second = signing_key("signer-2");
    let stranger = signing_key("stranger");
    let anchor = anchor_for(&root_key);
    let body = catalog(SEQUENCE);
    let signature = sign(&first, &body);

    // Record listed before its endorser.
    let out_of_order = vec![
        record(&first, &second, 3_000, 8_000),
        record(&root_key, &first, 2_000, 8_500),
    ];
    assert_eq!(
        verify(
            &document(body.clone(), out_of_order, vec![signature.clone()]),
            &anchor
        )
        .unwrap_err(),
        CatalogError::UnknownEndorser
    );

    // Endorsement forged by a key that is not the named endorser.
    let mut forged = record(&root_key, &first, 2_000, 8_500);
    forged.signature = record(&stranger, &first, 2_000, 8_500).signature;
    assert_eq!(
        verify(
            &document(body.clone(), vec![forged], vec![signature.clone()]),
            &anchor
        )
        .unwrap_err(),
        CatalogError::BadEndorsement
    );

    // Window altered after endorsement.
    let mut widened = record(&root_key, &first, 2_000, 8_500);
    widened.not_after = 8_999;
    assert_eq!(
        verify(
            &document(body.clone(), vec![widened], vec![signature.clone()]),
            &anchor
        )
        .unwrap_err(),
        CatalogError::BadEndorsement
    );

    // Id that does not belong to the key.
    let mut wrong_id = record(&root_key, &first, 2_000, 8_500);
    wrong_id.key_id = key_id(&second);
    assert_eq!(
        verify(
            &document(body.clone(), vec![wrong_id], vec![signature.clone()]),
            &anchor
        )
        .unwrap_err(),
        CatalogError::KeyIdMismatch
    );

    // A record that re-declares the root.
    let redeclared = record(&root_key, &root_key, 2_000, 8_500);
    assert_eq!(
        verify(
            &document(body.clone(), vec![redeclared], vec![signature.clone()]),
            &anchor
        )
        .unwrap_err(),
        CatalogError::DuplicateKeyId
    );

    // Empty window.
    let empty = record(&root_key, &first, 3_000, 3_000);
    assert_eq!(
        verify(&document(body, vec![empty], vec![signature]), &anchor).unwrap_err(),
        CatalogError::EmptyKeyWindow
    );
}

#[test]
fn stale_sequence_is_rejected_and_equal_sequence_is_accepted() {
    let root_key = signing_key("root");
    let bytes = signed_by(&root_key, catalog(SEQUENCE));
    let anchor = anchor_for(&root_key);

    assert_eq!(
        verify_catalog(&bytes, &anchor, NOW, Some(SEQUENCE + 1)).unwrap_err(),
        CatalogError::StaleSequence {
            sequence: SEQUENCE,
            high_water: SEQUENCE + 1
        }
    );
    verify_catalog(&bytes, &anchor, NOW, Some(SEQUENCE)).expect("verifies");
    verify_catalog(&bytes, &anchor, NOW, Some(SEQUENCE - 1)).expect("verifies");
}

#[test]
fn expired_catalog_is_rejected() {
    let root_key = signing_key("root");
    let bytes = signed_by(&root_key, catalog(SEQUENCE));
    assert_eq!(
        verify_catalog(&bytes, &anchor_for(&root_key), EXPIRES_AT, None).unwrap_err(),
        CatalogError::Expired
    );
}

#[test]
fn missing_and_repeated_signatures_are_rejected() {
    let root_key = signing_key("root");
    let anchor = anchor_for(&root_key);
    let body = catalog(SEQUENCE);
    assert_eq!(
        verify(&document(body.clone(), Vec::new(), Vec::new()), &anchor).unwrap_err(),
        CatalogError::NoSignatures
    );
    let signature = sign(&root_key, &body);
    assert_eq!(
        verify(
            &document(body, Vec::new(), vec![signature.clone(), signature]),
            &anchor
        )
        .unwrap_err(),
        CatalogError::DuplicateSignature
    );
}

#[test]
fn a_valid_signature_alongside_a_bad_one_is_accepted() {
    let root_key = signing_key("root");
    let attacker = signing_key("attacker");
    let body = catalog(SEQUENCE);
    let good = sign(&root_key, &body);
    let junk = sign(&attacker, &body);
    let bytes = document(body, Vec::new(), vec![junk, good]);
    verify(&bytes, &anchor_for(&root_key)).expect("verifies");
}

#[test]
fn duplicate_json_keys_are_rejected() {
    let root_key = signing_key("root");
    let text = String::from_utf8(signed_by(&root_key, catalog(SEQUENCE))).unwrap();
    let anchor = anchor_for(&root_key);

    let in_catalog = text.replacen("\"catalog\":{", "\"catalog\":{\"sequence\":1,", 1);
    assert_eq!(
        verify(in_catalog.as_bytes(), &anchor).unwrap_err(),
        CatalogError::DuplicateKey
    );
    let top_level = text.replacen('{', "{\"catalog\":null,", 1);
    assert_eq!(
        verify(top_level.as_bytes(), &anchor).unwrap_err(),
        CatalogError::DuplicateKey
    );
}

#[test]
fn trailing_data_and_non_integer_numbers_are_rejected() {
    let root_key = signing_key("root");
    let text = String::from_utf8(signed_by(&root_key, catalog(SEQUENCE))).unwrap();
    let anchor = anchor_for(&root_key);

    assert_eq!(
        verify(format!("{text} {{}}").as_bytes(), &anchor).unwrap_err(),
        CatalogError::Syntax
    );
    assert_eq!(
        verify(format!("{text}x").as_bytes(), &anchor).unwrap_err(),
        CatalogError::Syntax
    );
    let float = text.replacen("\"sequence\":7", "\"sequence\":7.0", 1);
    assert_eq!(
        verify(float.as_bytes(), &anchor).unwrap_err(),
        CatalogError::UnsupportedNumber
    );
    let exponent = text.replacen("\"sequence\":7", "\"sequence\":7e0", 1);
    assert_eq!(
        verify(exponent.as_bytes(), &anchor).unwrap_err(),
        CatalogError::UnsupportedNumber
    );
}

#[test]
fn whitespace_and_key_order_do_not_change_the_signed_bytes() {
    let root_key = signing_key("root");
    let anchor = anchor_for(&root_key);
    let bytes = signed_by(&root_key, catalog(SEQUENCE));
    let value: Value = serde_json::from_slice(&bytes).unwrap();

    let pretty = serde_json::to_vec_pretty(&value).unwrap();
    verify(&pretty, &anchor).expect("verifies");
    // `serde_json::Value` orders object keys differently from the struct field
    // order the signer serialized.
    let reordered = serde_json::to_vec(&value).unwrap();
    assert_ne!(reordered, bytes);
    verify(&reordered, &anchor).expect("verifies");
}

#[test]
fn oversized_input_is_rejected_before_parsing() {
    let root_key = signing_key("root");
    let oversized = vec![b' '; MAX_CATALOG_BYTES + 1];
    assert_eq!(
        verify(&oversized, &anchor_for(&root_key)).unwrap_err(),
        CatalogError::TooLarge
    );
}

#[test]
fn collection_limits_are_enforced() {
    let root_key = signing_key("root");
    let mut body = catalog(SEQUENCE);
    let template = body.entries[0].clone();
    body.entries = (0..=MAX_CATALOG_ENTRIES)
        .map(|_| template.clone())
        .collect();
    assert_eq!(
        verify(&signed_by(&root_key, body), &anchor_for(&root_key)).unwrap_err(),
        CatalogError::Limit(CatalogLimit::Entries)
    );
}

#[test]
fn unknown_fields_and_bad_schema_versions_are_rejected_without_echo() {
    let root_key = signing_key("root");
    let anchor = anchor_for(&root_key);
    let bytes = signed_by(&root_key, catalog(SEQUENCE));

    let extra_in_catalog = mutate(&bytes, |value| {
        value["catalog"]["evil_field_name"] = Value::Bool(true);
    });
    let error = verify(&extra_in_catalog, &anchor).unwrap_err();
    assert_eq!(error, CatalogError::Schema);
    assert!(!error.to_string().contains("evil_field_name"));

    let extra_in_entry = mutate(&bytes, |value| {
        value["catalog"]["entries"][0]["extra"] = Value::Bool(true);
    });
    assert_eq!(
        verify(&extra_in_entry, &anchor).unwrap_err(),
        CatalogError::Schema
    );

    let extra_top = mutate(&bytes, |value| value["extra"] = Value::Bool(true));
    assert_eq!(
        verify(&extra_top, &anchor).unwrap_err(),
        CatalogError::Schema
    );

    let bad_version = mutate(&bytes, |value| value["schema_version"] = Value::from(2));
    assert_eq!(
        verify(&bad_version, &anchor).unwrap_err(),
        CatalogError::UnsupportedSchemaVersion
    );

    let bad_digest = mutate(&bytes, |value| {
        value["catalog"]["entries"][0]["digest"] = Value::String("sha256:XYZ".to_owned());
    });
    assert_eq!(
        verify(&bad_digest, &anchor).unwrap_err(),
        CatalogError::Schema
    );
}

#[test]
fn entry_rules_are_enforced() {
    let root_key = signing_key("root");
    let anchor = anchor_for(&root_key);
    let check = |edit: &dyn Fn(&mut Catalog), index: usize, reason: CatalogEntryRejection| {
        let mut body = catalog(SEQUENCE);
        edit(&mut body);
        assert_eq!(
            verify(&signed_by(&root_key, body), &anchor).unwrap_err(),
            CatalogError::Entry { index, reason }
        );
    };

    check(
        &|body| body.entries[0].platforms.clear(),
        0,
        CatalogEntryRejection::Platforms,
    );
    check(
        &|body| body.entries[0].platforms = vec!["a".into(), "a".into()],
        0,
        CatalogEntryRejection::Platforms,
    );
    check(
        &|body| body.entries[0].platforms = vec!["Linux x64".into()],
        0,
        CatalogEntryRejection::Platforms,
    );
    for bad in ["", "*", "not a range", ">=1.0, <"] {
        check(
            &|body| body.entries[1].core = bad.to_owned(),
            1,
            CatalogEntryRejection::CoreRange,
        );
    }
    check(
        &|body| {
            body.entries
                .push(entry("pohunek.runtime.codex", "codex", "1.0.0", 'c'));
        },
        2,
        CatalogEntryRejection::DuplicateVersion,
    );
    check(
        &|body| {
            body.entries
                .push(entry("pohunek.runtime.codex", "codex", "1.1.0", 'a'));
        },
        2,
        CatalogEntryRejection::DuplicateDigest,
    );
    check(
        &|body| {
            body.entries
                .push(entry("evil.codex", "codex", "1.0.0", 'c'));
        },
        2,
        CatalogEntryRejection::IdentityConflict,
    );
    check(
        &|body| {
            body.entries
                .push(entry("pohunek.runtime.codex", "claude", "2.0.0", 'c'));
        },
        2,
        CatalogEntryRejection::IdentityConflict,
    );
    check(
        &|body| body.revoked_digests.push(digest('a')),
        0,
        CatalogEntryRejection::RevokedDigest,
    );
}

#[test]
fn several_versions_of_one_package_are_accepted() {
    let root_key = signing_key("root");
    let mut body = catalog(SEQUENCE);
    body.entries
        .push(entry("pohunek.runtime.codex", "codex", "1.1.0", 'c'));
    let verified = verify(&signed_by(&root_key, body), &anchor_for(&root_key)).unwrap();
    assert_eq!(verified.entries().len(), 3);
}

#[test]
fn revoked_digest_is_not_authorized_and_revoked_keys_are_exposed() {
    let root_key = signing_key("root");
    let retired = signing_key("retired");
    let mut body = catalog(SEQUENCE);
    body.revoked_digests.push(digest('f'));
    body.revoked_key_ids.push(key_id(&retired));
    let verified = verify(&signed_by(&root_key, body), &anchor_for(&root_key)).unwrap();

    assert!(verified.is_digest_revoked(&digest('f')));
    assert!(!verified.is_digest_revoked(&digest('a')));
    assert!(verified.revoked_key_ids().contains(&key_id(&retired)));
    let package = PackageId::parse("pohunek.runtime.codex").unwrap();
    assert_eq!(
        verified.authorize(&package, &RuntimeId::codex(), &digest('f')),
        Authorization::NotAuthorized
    );
}

#[test]
fn canonical_signing_message_is_pinned_and_deterministic() {
    let body = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence: 3,
        expires_at: 9,
        revoked_key_ids: Vec::new(),
        revoked_digests: Vec::new(),
        entries: vec![entry("pohunek.runtime.codex", "codex", "1.0.0", 'a')],
    };
    let expected = format!(
        "pohunek-runtime-catalog-v1\n{{\"entries\":[{{\"core\":\">=0.33.0, <0.40.0\",\
         \"digest\":\"sha256:{}\",\"package_id\":\"pohunek.runtime.codex\",\
         \"platforms\":[\"x86_64-unknown-linux-gnu\"],\"runtime_id\":\"codex\",\
         \"version\":\"1.0.0\"}}],\"expires_at\":9,\"revoked_digests\":[],\
         \"revoked_key_ids\":[],\"schema_version\":1,\"sequence\":3}}",
        "a".repeat(64)
    );
    let first = catalog_signing_message(&body).unwrap();
    assert_eq!(first, expected.as_bytes());
    assert_eq!(first, catalog_signing_message(&body.clone()).unwrap());
}

#[test]
fn key_id_is_the_sha256_of_the_public_key() {
    let key = signing_key("root");
    let expected = hex(&Sha256::digest(key.verifying_key().as_bytes()));
    assert_eq!(key_id(&key).as_str(), expected);
    assert_eq!(KeyId::parse(&expected).unwrap(), key_id(&key));
    KeyId::parse(&expected.to_uppercase()).unwrap_err();
    KeyId::parse("abc").unwrap_err();
}

#[test]
fn trust_anchor_validates_its_inputs() {
    let key = signing_key("root");
    assert_eq!(
        RootKey::new(key.verifying_key().to_bytes(), 5, 5).unwrap_err(),
        TrustAnchorError::EmptyWindow
    );
    let mut identity_point = [0_u8; 32];
    identity_point[0] = 1;
    assert_eq!(
        RootKey::new(identity_point, 0, 10).unwrap_err(),
        TrustAnchorError::MalformedKey
    );
    assert_eq!(
        TrustAnchor::new(Vec::new(), Vec::new()).unwrap_err(),
        TrustAnchorError::NoRoots
    );
    assert_eq!(
        TrustAnchor::new(vec![root(&key, 0, 10), root(&key, 0, 10)], Vec::new()).unwrap_err(),
        TrustAnchorError::DuplicateRoot
    );
}

#[test]
fn local_trust_never_authorizes_reserved_runtimes() {
    let trust = LocalTrust::ExplicitDigest(digest('a'));
    assert_eq!(
        trust.authorize(&RuntimeId::parse("acme.tool").unwrap(), &digest('a')),
        Ok(LocalAuthorization::Local)
    );
    assert_eq!(
        trust.authorize(&RuntimeId::parse("acme.tool").unwrap(), &digest('b')),
        Err(LocalTrustError::DigestMismatch)
    );
    for runtime in [
        RuntimeId::shell(),
        RuntimeId::codex(),
        RuntimeId::claude(),
        RuntimeId::hermes(),
    ] {
        assert_eq!(
            trust.authorize(&runtime, &digest('a')),
            Err(LocalTrustError::ReservedRuntime)
        );
    }
}

#[test]
fn garbage_input_never_panics_or_echoes() {
    let root_key = signing_key("root");
    let anchor = anchor_for(&root_key);
    for input in [
        &b""[..],
        b"null",
        b"[]",
        b"{}",
        b"\xff\xfe",
        b"{\"catalog\":",
    ] {
        let error = verify(input, &anchor).unwrap_err();
        assert!(!error.to_string().is_empty());
    }
}
