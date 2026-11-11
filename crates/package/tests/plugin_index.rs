// Rust guideline compliant 2026-06-26

//! Behavioural tests of the signed plugin index verifier.
//!
//! Keys derive from fixed labels and `now` is explicit, so every run signs the
//! same bytes. Expected results come from the slice specification, not from
//! the verifier: every rejection is asserted as its exact variant.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use ed25519_dalek::{Signer as _, SigningKey};
use package::{
    index_key_record_signing_message, index_signing_message, parse_index_envelope,
    revocation_signing_message, sign_catalog, verify_catalog, verify_index, BinarySet, Catalog,
    CatalogError, CatalogSignature, IndexBody, IndexEntry, IndexEntryKind, IndexEntryRefusal,
    IndexEntryRejection, IndexEnvelope, IndexError, IndexKeyRecord, IndexLimit, IndexVerifierState,
    KeyId, PackageDigest, Release, RevocationBody, RootKey, Sha256Digest, SignedRevocations,
    TrustAnchor, CATALOG_SCHEMA_VERSION, INDEX_SCHEMA_VERSION, INDEX_SCHEMA_VERSION_2,
    MAX_ENTRY_URL_BYTES, MAX_INDEX_BYTES, MAX_INDEX_ENTRIES, MAX_INDEX_KEY_RECORDS,
    MAX_INDEX_KEY_WINDOW_SECONDS, MAX_INDEX_LIFETIME_SECONDS, MAX_INDEX_SEQUENCE_SKEW_SECONDS,
    MAX_INDEX_SIGNATURES, MAX_REVOKED_KEYS,
};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const NOW: u64 = 1_800_000_000;
const DAY: u64 = 24 * 60 * 60;
const PLATFORM: &str = "x86_64-unknown-linux-gnu";
const ROOT_START: u64 = NOW - 1_000_000;
const ROOT_END: u64 = NOW + 100_000_000;
const KEY_START: u64 = NOW - 1_000;
const KEY_END: u64 = NOW + 10_000_000;
const SEQUENCE: u64 = NOW - 3_600;
/// Domain prefixes spelled out here so a change to the constants shows up as a
/// failing test.
const INDEX_DOMAIN: &[u8] = b"pohunek-plugin-index-v1\n";
const REVOCATION_DOMAIN: &[u8] = b"pohunek-plugin-index-revocations-v1\n";
const KEY_RECORD_DOMAIN: &[u8] = b"pohunek-plugin-index-key-record-v1\n";
const CATALOG_DOMAIN: &[u8] = b"pohunek-runtime-catalog-v2\n";
const CATALOG_KEY_RECORD_DOMAIN: &[u8] = b"pohunek-catalog-key-record-v1\n";

fn signing_key(label: &str) -> SigningKey {
    let seed: [u8; 32] = Sha256::digest(label.as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn key_id(key: &SigningKey) -> KeyId {
    KeyId::derive(&key.verifying_key())
}

fn root(key: &SigningKey) -> RootKey {
    RootKey::new(key.verifying_key().to_bytes(), ROOT_START, ROOT_END).expect("valid root")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String");
        text
    })
}

fn digest(index: u64) -> PackageDigest {
    PackageDigest::parse(&format!("sha256:{index:064x}")).expect("digest")
}

fn entry(package: &str, runtime: &str, version: &str, digest_index: u64) -> IndexEntry {
    IndexEntry {
        package_id: PackageId::parse(package).expect("package id"),
        kind: None,
        runtime_id: Some(RuntimeId::parse(runtime).expect("runtime id")),
        version: PackageVersion::parse(version).expect("version"),
        digest: digest(digest_index),
        url: format!("https://example.test/{package}-{version}.tar.zst"),
        platforms: vec![PLATFORM.to_owned()],
        core: ">=0.40.0".to_owned(),
    }
}

fn muse() -> IndexEntry {
    entry("pohunek.runtime.muse", "muse", "0.2.0", 1)
}

fn record(
    endorser: &SigningKey,
    subject: &SigningKey,
    scope: &str,
    not_before: u64,
    not_after: u64,
) -> IndexKeyRecord {
    let mut record = IndexKeyRecord {
        key_id: key_id(subject),
        public_key: hex(subject.verifying_key().as_bytes()),
        scope: scope.to_owned(),
        not_before,
        not_after,
        endorsed_by: key_id(endorser),
        signature: String::new(),
    };
    record.signature = hex(&endorser
        .sign(&index_key_record_signing_message(&record))
        .to_bytes());
    record
}

fn sign_body(key: &SigningKey, body: &IndexBody) -> CatalogSignature {
    let message = index_signing_message(body).expect("message");
    CatalogSignature {
        key_id: key_id(key),
        signature: hex(&key.sign(&message).to_bytes()),
    }
}

fn revocations(endorser: &SigningKey, sequence: u64, revoked: Vec<KeyId>) -> SignedRevocations {
    let body = RevocationBody {
        schema_version: INDEX_SCHEMA_VERSION,
        sequence,
        revoked_key_ids: revoked,
    };
    let message = revocation_signing_message(&body).expect("message");
    SignedRevocations {
        signature: CatalogSignature {
            key_id: key_id(endorser),
            signature: hex(&endorser.sign(&message).to_bytes()),
        },
        revocations: body,
    }
}

/// Keys and the typed document every scenario starts from: endorser root R,
/// index key K endorsed by R, a CI-like root C, and a second index key K2.
struct Fixture {
    r: SigningKey,
    k: SigningKey,
    c: SigningKey,
    k2: SigningKey,
    body: IndexBody,
    records: Vec<IndexKeyRecord>,
    signatures: Vec<CatalogSignature>,
    revocations: SignedRevocations,
    /// The schema version the envelope declares.
    envelope_schema: u64,
}

impl Fixture {
    fn new() -> Self {
        let r = signing_key("endorser-root");
        let k = signing_key("index-key");
        let body = IndexBody {
            schema_version: INDEX_SCHEMA_VERSION,
            scope: "official".to_owned(),
            sequence: SEQUENCE,
            expires_at: SEQUENCE + 7 * DAY,
            entries: vec![muse()],
        };
        Self {
            records: vec![record(&r, &k, "official", KEY_START, KEY_END)],
            signatures: vec![sign_body(&k, &body)],
            revocations: revocations(&r, 1, Vec::new()),
            envelope_schema: INDEX_SCHEMA_VERSION,
            c: signing_key("ci-root"),
            k2: signing_key("index-key-2"),
            body,
            r,
            k,
        }
    }

    /// Replaces the body and re-signs it with K.
    fn with_body(mut self, edit: impl FnOnce(&mut IndexBody)) -> Self {
        edit(&mut self.body);
        self.signatures = vec![sign_body(&self.k, &self.body)];
        self
    }

    fn with_records(mut self, records: Vec<IndexKeyRecord>) -> Self {
        self.records = records;
        self
    }

    fn with_signatures(mut self, signatures: Vec<CatalogSignature>) -> Self {
        self.signatures = signatures;
        self
    }

    fn with_revocations(mut self, revocations: SignedRevocations) -> Self {
        self.revocations = revocations;
        self
    }

    /// Turns the document into a schema 2 one listing `entries`, re-signed.
    fn schema_2(mut self, entries: Vec<IndexEntry>) -> Self {
        self.envelope_schema = INDEX_SCHEMA_VERSION_2;
        self.with_body(|body| {
            body.schema_version = INDEX_SCHEMA_VERSION_2;
            body.entries = entries;
        })
    }

    fn envelope(&self) -> IndexEnvelope {
        IndexEnvelope {
            schema_version: self.envelope_schema,
            index: self.body.clone(),
            key_records: self.records.clone(),
            signatures: self.signatures.clone(),
            revocations: self.revocations.clone(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.envelope()).expect("serialize")
    }

    fn endorser(&self) -> RootKey {
        root(&self.r)
    }
}

fn verify(bytes: &[u8], endorser: &RootKey) -> Result<package::VerifiedIndex, IndexError> {
    verify_with(
        bytes,
        endorser,
        &BTreeSet::new(),
        &IndexVerifierState::default(),
    )
}

fn verify_with(
    bytes: &[u8],
    endorser: &RootKey,
    base_revoked: &BTreeSet<KeyId>,
    state: &IndexVerifierState,
) -> Result<package::VerifiedIndex, IndexError> {
    verify_index(bytes, endorser, base_revoked, state, NOW)
}

fn verify_fixture(fixture: &Fixture) -> Result<package::VerifiedIndex, IndexError> {
    verify(&fixture.bytes(), &fixture.endorser())
}

fn error_of(fixture: &Fixture) -> IndexError {
    verify_fixture(fixture).expect_err("must be rejected")
}

fn mutate(bytes: &[u8], edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(bytes).expect("json");
    edit(&mut value);
    serde_json::to_vec(&value).expect("serialize")
}

/// Re-prefixes a signing message with another domain.
fn swap_domain(message: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let rest = message.strip_prefix(from).expect("domain prefix");
    [to, rest].concat()
}

fn numbered_key_id(index: u64) -> KeyId {
    KeyId::parse(&format!("{index:064x}")).expect("key id")
}

// --- W2-1.T1: trust shape ---------------------------------------------------

#[test]
fn index_signed_by_an_endorsed_key_verifies() {
    let fixture = Fixture::new();
    let verified = verify_fixture(&fixture).expect("valid index");

    assert_eq!(verified.signer(), &key_id(&fixture.k));
    assert_eq!(verified.sequence(), SEQUENCE);
    assert_eq!(verified.expires_at(), SEQUENCE + 7 * DAY);
    assert_eq!(verified.revocation_sequence(), 1);
    assert_eq!(verified.entries(), &[muse()]);
    assert!(verified.lists(&digest(1)));
    assert!(!verified.lists(&digest(2)));
}

#[test]
fn index_signed_by_the_endorser_root_directly_is_an_unknown_signer() {
    let fixture = Fixture::new();
    let fixture = {
        let signature = sign_body(&fixture.r, &fixture.body);
        fixture.with_signatures(vec![signature])
    };

    assert_eq!(error_of(&fixture), IndexError::UnknownSigner);
}

#[test]
fn index_signed_by_an_unlisted_key_is_an_unknown_signer() {
    let fixture = Fixture::new();
    let fixture = {
        let signature = sign_body(&fixture.k2, &fixture.body);
        fixture.with_signatures(vec![signature])
    };

    assert_eq!(error_of(&fixture), IndexError::UnknownSigner);
}

#[test]
fn key_endorsed_by_another_root_is_an_unknown_endorser() {
    let fixture = Fixture::new();
    let c_record = record(&fixture.c, &fixture.k2, "official", KEY_START, KEY_END);
    let signature = sign_body(&fixture.k2, &fixture.body);
    let fixture = fixture
        .with_records(vec![c_record])
        .with_signatures(vec![signature]);

    assert_eq!(error_of(&fixture), IndexError::UnknownEndorser);
}

#[test]
fn key_endorsed_by_another_record_is_an_unknown_endorser() {
    let fixture = Fixture::new();
    let chained = record(&fixture.k, &fixture.k2, "official", KEY_START, KEY_END);
    let signature = sign_body(&fixture.k2, &fixture.body);
    let mut records = fixture.records.clone();
    records.push(chained);
    let fixture = fixture
        .with_records(records)
        .with_signatures(vec![signature]);

    assert_eq!(error_of(&fixture), IndexError::UnknownEndorser);
}

#[test]
fn key_record_with_another_scope_is_a_scope_mismatch() {
    let fixture = Fixture::new();
    let verified_scope = record(&fixture.r, &fixture.k, "verified", KEY_START, KEY_END);
    let fixture = fixture.with_records(vec![verified_scope]);

    assert_eq!(error_of(&fixture), IndexError::ScopeMismatch);
}

#[test]
fn endorsement_signed_by_the_wrong_key_does_not_verify() {
    let fixture = Fixture::new();
    // The record names R as its endorser but is signed by C.
    let mut forged = record(&fixture.c, &fixture.k, "official", KEY_START, KEY_END);
    forged.endorsed_by = key_id(&fixture.r);
    let fixture = fixture.with_records(vec![forged]);

    assert_eq!(error_of(&fixture), IndexError::BadEndorsement);
}

#[test]
fn endorsement_does_not_cover_a_field_changed_after_signing() {
    let fixture = Fixture::new();
    let mut widened = fixture.records[0].clone();
    widened.not_after += 1;
    let fixture = fixture.with_records(vec![widened]);

    assert_eq!(error_of(&fixture), IndexError::BadEndorsement);
}

#[test]
fn endorsement_under_the_catalog_record_domain_does_not_verify() {
    let fixture = Fixture::new();
    let mut forged = fixture.records[0].clone();
    let message = swap_domain(
        &index_key_record_signing_message(&forged),
        KEY_RECORD_DOMAIN,
        CATALOG_KEY_RECORD_DOMAIN,
    );
    forged.signature = hex(&fixture.r.sign(&message).to_bytes());
    let fixture = fixture.with_records(vec![forged]);

    assert_eq!(error_of(&fixture), IndexError::BadEndorsement);
}

// --- Key record structure ---------------------------------------------------

#[test]
fn key_record_with_malformed_key_material_is_rejected() {
    let fixture = Fixture::new();
    let mut not_hex = fixture.records[0].clone();
    not_hex.public_key = "zz".to_owned();
    let mut weak = fixture.records[0].clone();
    // The identity point is a valid encoding but a weak key.
    weak.public_key = format!("01{}", "00".repeat(31));

    for bad in [not_hex, weak] {
        let fixture = Fixture::new().with_records(vec![bad]);
        assert_eq!(error_of(&fixture), IndexError::MalformedKeyMaterial);
    }
}

#[test]
fn key_record_with_a_foreign_key_id_is_rejected() {
    let fixture = Fixture::new();
    let mut mismatched = fixture.records[0].clone();
    mismatched.key_id = key_id(&fixture.k2);
    let fixture = fixture.with_records(vec![mismatched]);

    assert_eq!(error_of(&fixture), IndexError::KeyIdMismatch);
}

#[test]
fn repeated_key_records_are_rejected() {
    let fixture = Fixture::new();
    let twice = vec![fixture.records[0].clone(), fixture.records[0].clone()];
    let fixture = fixture.with_records(twice);

    assert_eq!(error_of(&fixture), IndexError::DuplicateKeyId);
}

#[test]
fn key_record_for_the_endorser_itself_is_a_duplicate_key_id() {
    let fixture = Fixture::new();
    let self_record = record(&fixture.r, &fixture.r, "official", KEY_START, KEY_END);
    let fixture = fixture.with_records(vec![self_record]);

    assert_eq!(error_of(&fixture), IndexError::DuplicateKeyId);
}

#[test]
fn key_record_window_must_be_non_empty_and_at_most_366_days() {
    let fixture = Fixture::new();
    let empty = record(&fixture.r, &fixture.k, "official", KEY_START, KEY_START);
    assert_eq!(
        error_of(&Fixture::new().with_records(vec![empty])),
        IndexError::EmptyKeyWindow
    );

    let too_long = record(
        &fixture.r,
        &fixture.k,
        "official",
        KEY_START,
        KEY_START + MAX_INDEX_KEY_WINDOW_SECONDS + 1,
    );
    assert_eq!(
        error_of(&Fixture::new().with_records(vec![too_long])),
        IndexError::KeyWindowTooLong
    );

    let longest = record(
        &fixture.r,
        &fixture.k,
        "official",
        KEY_START,
        KEY_START + MAX_INDEX_KEY_WINDOW_SECONDS,
    );
    verify_fixture(&Fixture::new().with_records(vec![longest])).expect("366 days is allowed");
}

// --- Signatures -------------------------------------------------------------

#[test]
fn document_without_signatures_is_rejected() {
    let fixture = Fixture::new().with_signatures(Vec::new());

    assert_eq!(error_of(&fixture), IndexError::NoSignatures);
}

#[test]
fn repeated_signature_key_ids_are_rejected() {
    let fixture = Fixture::new();
    let twice = vec![fixture.signatures[0].clone(), fixture.signatures[0].clone()];
    let fixture = fixture.with_signatures(twice);

    assert_eq!(error_of(&fixture), IndexError::DuplicateSignature);
}

#[test]
fn signature_over_a_changed_body_does_not_verify() {
    let bytes = Fixture::new().bytes();
    let tampered = mutate(&bytes, |value| {
        value["index"]["entries"][0]["url"] = Value::from("https://evil.test/a.tar.zst");
    });

    assert_eq!(
        verify(&tampered, &root(&signing_key("endorser-root"))).unwrap_err(),
        IndexError::BadSignature
    );
}

#[test]
fn signature_under_another_domain_does_not_verify() {
    let fixture = Fixture::new();
    for other in [CATALOG_DOMAIN, REVOCATION_DOMAIN] {
        let message = swap_domain(
            &index_signing_message(&fixture.body).unwrap(),
            INDEX_DOMAIN,
            other,
        );
        let forged = CatalogSignature {
            key_id: key_id(&fixture.k),
            signature: hex(&fixture.k.sign(&message).to_bytes()),
        };
        let fixture = Fixture::new().with_signatures(vec![forged]);
        assert_eq!(error_of(&fixture), IndexError::BadSignature);
    }
}

#[test]
fn malformed_signature_hex_is_rejected() {
    let fixture = Fixture::new();
    let bad = CatalogSignature {
        key_id: key_id(&fixture.k),
        signature: "abcd".to_owned(),
    };
    let fixture = fixture.with_signatures(vec![bad]);

    assert_eq!(error_of(&fixture), IndexError::MalformedKeyMaterial);
}

#[test]
fn signing_key_outside_its_window_is_rejected() {
    let fixture = Fixture::new();
    let future = record(&fixture.r, &fixture.k, "official", NOW + 1, KEY_END);
    assert_eq!(
        error_of(&Fixture::new().with_records(vec![future])),
        IndexError::SignerNotYetValid
    );

    let ended = record(&fixture.r, &fixture.k, "official", KEY_START, NOW);
    assert_eq!(
        error_of(&Fixture::new().with_records(vec![ended])),
        IndexError::SignerExpired
    );
}

#[test]
fn first_valid_signature_is_the_signer() {
    let fixture = Fixture::new();
    let k2_record = record(&fixture.r, &fixture.k2, "official", KEY_START, KEY_END);
    let garbage = CatalogSignature {
        key_id: key_id(&fixture.k2),
        signature: "00".repeat(64),
    };
    let good = fixture.signatures[0].clone();
    let mut records = fixture.records.clone();
    records.push(k2_record);
    let fixture = fixture
        .with_records(records)
        .with_signatures(vec![garbage, good]);

    let verified = verify_fixture(&fixture).expect("second signature passes");
    assert_eq!(verified.signer(), &key_id(&fixture.k));
}

#[test]
fn when_no_signature_passes_the_first_failure_is_returned() {
    let fixture = Fixture::new();
    let ended = record(&fixture.r, &fixture.k2, "official", KEY_START, NOW);
    let first = CatalogSignature {
        key_id: key_id(&fixture.k),
        signature: "00".repeat(64),
    };
    let second = sign_body(&fixture.k2, &fixture.body);
    let mut records = fixture.records.clone();
    records.push(ended);
    let fixture = fixture
        .with_records(records)
        .with_signatures(vec![first, second]);

    assert_eq!(error_of(&fixture), IndexError::BadSignature);
}

// --- W2-1.T2: revocation ----------------------------------------------------

#[test]
fn key_listed_in_the_document_revocations_is_revoked() {
    let fixture = Fixture::new();
    let list = revocations(&fixture.r, 1, vec![key_id(&fixture.k)]);
    let fixture = fixture.with_revocations(list);

    assert_eq!(error_of(&fixture), IndexError::SignerRevoked);
}

#[test]
fn key_listed_in_the_caller_revocations_is_revoked() {
    let fixture = Fixture::new();
    let revoked: BTreeSet<KeyId> = [key_id(&fixture.k)].into();

    let from_base = verify_with(
        &fixture.bytes(),
        &fixture.endorser(),
        &revoked,
        &IndexVerifierState::default(),
    );
    assert_eq!(from_base.unwrap_err(), IndexError::SignerRevoked);

    let state = IndexVerifierState {
        revoked_key_ids: revoked,
        ..IndexVerifierState::default()
    };
    let from_state = verify_with(
        &fixture.bytes(),
        &fixture.endorser(),
        &BTreeSet::new(),
        &state,
    );
    assert_eq!(from_state.unwrap_err(), IndexError::SignerRevoked);
}

#[test]
fn revoked_endorser_rejects_the_document() {
    let fixture = Fixture::new();
    let own_id = key_id(&fixture.r);
    let endorser = fixture.endorser();

    let from_base = verify_with(
        &fixture.bytes(),
        &endorser,
        &[own_id.clone()].into(),
        &IndexVerifierState::default(),
    );
    assert_eq!(from_base.unwrap_err(), IndexError::SignerRevoked);

    let state = IndexVerifierState {
        revoked_key_ids: [own_id.clone()].into(),
        ..IndexVerifierState::default()
    };
    let from_state = verify_with(&fixture.bytes(), &endorser, &BTreeSet::new(), &state);
    assert_eq!(from_state.unwrap_err(), IndexError::SignerRevoked);

    let listed =
        fixture.with_revocations(revocations(&signing_key("endorser-root"), 1, vec![own_id]));
    assert_eq!(error_of(&listed), IndexError::SignerRevoked);
}

#[test]
fn revocation_list_must_be_signed_by_the_endorser() {
    let fixture = Fixture::new();

    // Another key id on the signature.
    let mut other_id = revocations(&fixture.r, 1, Vec::new());
    other_id.signature.key_id = key_id(&fixture.k);
    assert_eq!(
        error_of(&Fixture::new().with_revocations(other_id)),
        IndexError::BadRevocation
    );

    // The endorser's id with another key's signature.
    let mut wrong_key = revocations(&fixture.k, 1, Vec::new());
    wrong_key.signature.key_id = key_id(&fixture.r);
    assert_eq!(
        error_of(&Fixture::new().with_revocations(wrong_key)),
        IndexError::BadRevocation
    );

    // A body changed after signing.
    let mut tampered = revocations(&fixture.r, 1, Vec::new());
    tampered.revocations.sequence = 2;
    assert_eq!(
        error_of(&Fixture::new().with_revocations(tampered)),
        IndexError::BadRevocation
    );

    // Malformed signature hex.
    let mut malformed = revocations(&fixture.r, 1, Vec::new());
    malformed.signature.signature = "abcd".to_owned();
    assert_eq!(
        error_of(&Fixture::new().with_revocations(malformed)),
        IndexError::BadRevocation
    );
}

#[test]
fn revocation_signature_under_another_domain_does_not_verify() {
    let fixture = Fixture::new();
    let body = fixture.revocations.revocations.clone();
    for other in [INDEX_DOMAIN, CATALOG_DOMAIN] {
        let message = swap_domain(
            &revocation_signing_message(&body).unwrap(),
            REVOCATION_DOMAIN,
            other,
        );
        let forged = SignedRevocations {
            revocations: body.clone(),
            signature: CatalogSignature {
                key_id: key_id(&fixture.r),
                signature: hex(&fixture.r.sign(&message).to_bytes()),
            },
        };
        assert_eq!(
            error_of(&Fixture::new().with_revocations(forged)),
            IndexError::BadRevocation
        );
    }
}

#[test]
fn revocation_list_below_the_high_water_mark_is_stale() {
    let fixture =
        Fixture::new().with_revocations(revocations(&signing_key("endorser-root"), 4, Vec::new()));
    let state = IndexVerifierState {
        revocation_high_water: Some(5),
        ..IndexVerifierState::default()
    };

    let outcome = verify_with(
        &fixture.bytes(),
        &fixture.endorser(),
        &BTreeSet::new(),
        &state,
    );
    assert_eq!(
        outcome.unwrap_err(),
        IndexError::StaleRevocation {
            sequence: 4,
            high_water: 5
        }
    );

    let equal =
        Fixture::new().with_revocations(revocations(&signing_key("endorser-root"), 5, Vec::new()));
    let verified = verify_with(&equal.bytes(), &equal.endorser(), &BTreeSet::new(), &state)
        .expect("an equal revocation sequence is accepted");
    assert_eq!(verified.revocation_sequence(), 5);
}

#[test]
fn verified_index_reports_the_revoked_key_ids_of_its_document() {
    let fixture = Fixture::new();
    let revoked = numbered_key_id(7);
    let list = revocations(&fixture.r, 3, vec![revoked.clone()]);
    let verified = verify_fixture(&fixture.with_revocations(list)).expect("valid");

    assert_eq!(verified.revoked_key_ids(), &[revoked].into());
}

#[test]
fn endorser_outside_its_window_is_rejected() {
    let fixture = Fixture::new();
    let public = fixture.r.verifying_key().to_bytes();
    let not_yet = RootKey::new(public, NOW + 1, ROOT_END).unwrap();
    let ended = RootKey::new(public, ROOT_START, NOW).unwrap();

    assert_eq!(
        verify(&fixture.bytes(), &not_yet).unwrap_err(),
        IndexError::EndorserNotYetValid
    );
    assert_eq!(
        verify(&fixture.bytes(), &ended).unwrap_err(),
        IndexError::EndorserExpired
    );
}

// --- Body: scope, sequence, lifetime ----------------------------------------

#[test]
fn index_body_with_another_scope_is_a_scope_mismatch() {
    let fixture = Fixture::new().with_body(|body| body.scope = "verified".to_owned());

    assert_eq!(error_of(&fixture), IndexError::ScopeMismatch);
}

#[test]
fn index_sequence_below_the_high_water_mark_is_stale() {
    let fixture = Fixture::new();
    let state = IndexVerifierState {
        index_high_water: Some(SEQUENCE + 1),
        ..IndexVerifierState::default()
    };
    let outcome = verify_with(
        &fixture.bytes(),
        &fixture.endorser(),
        &BTreeSet::new(),
        &state,
    );
    assert_eq!(
        outcome.unwrap_err(),
        IndexError::StaleSequence {
            sequence: SEQUENCE,
            high_water: SEQUENCE + 1
        }
    );

    let state = IndexVerifierState {
        index_high_water: Some(SEQUENCE),
        ..IndexVerifierState::default()
    };
    verify_with(
        &fixture.bytes(),
        &fixture.endorser(),
        &BTreeSet::new(),
        &state,
    )
    .expect("an equal sequence is accepted");
}

#[test]
fn index_sequence_may_lead_the_clock_by_at_most_one_day() {
    let sequence = NOW + MAX_INDEX_SEQUENCE_SKEW_SECONDS;
    let at_limit = Fixture::new().with_body(|body| {
        body.sequence = sequence;
        body.expires_at = sequence + DAY;
    });
    verify_fixture(&at_limit).expect("86_400 seconds ahead is allowed");

    let past_limit = Fixture::new().with_body(|body| {
        body.sequence = sequence + 1;
        body.expires_at = sequence + 1 + DAY;
    });
    assert_eq!(error_of(&past_limit), IndexError::SequenceInFuture);

    let extreme = Fixture::new().with_body(|body| {
        body.sequence = u64::MAX - 1;
        body.expires_at = u64::MAX;
    });
    assert_eq!(error_of(&extreme), IndexError::SequenceInFuture);
}

#[test]
fn index_lifetime_is_bounded_and_positive() {
    let longest = Fixture::new().with_body(|body| {
        body.expires_at = body.sequence + MAX_INDEX_LIFETIME_SECONDS;
    });
    verify_fixture(&longest).expect("14 days is allowed");

    let too_long = Fixture::new().with_body(|body| {
        body.expires_at = body.sequence + MAX_INDEX_LIFETIME_SECONDS + 1;
    });
    assert_eq!(error_of(&too_long), IndexError::LifetimeTooLong);

    let empty = Fixture::new().with_body(|body| body.expires_at = body.sequence);
    assert_eq!(error_of(&empty), IndexError::LifetimeTooLong);

    let before = Fixture::new().with_body(|body| body.expires_at = body.sequence - 1);
    assert_eq!(error_of(&before), IndexError::LifetimeTooLong);
}

#[test]
fn index_expires_at_its_expiry_second() {
    let at_expiry = Fixture::new().with_body(|body| body.expires_at = NOW);
    assert_eq!(error_of(&at_expiry), IndexError::Expired);

    let one_left = Fixture::new().with_body(|body| body.expires_at = NOW + 1);
    verify_fixture(&one_left).expect("one second left");
}

// --- Parsing, schema and limits --------------------------------------------

#[test]
fn oversized_and_unparsable_documents_are_rejected() {
    let endorser = Fixture::new().endorser();

    assert_eq!(
        verify(&vec![b' '; MAX_INDEX_BYTES + 1], &endorser).unwrap_err(),
        IndexError::TooLarge
    );
    assert_eq!(verify(b"{", &endorser).unwrap_err(), IndexError::Syntax);
    assert_eq!(
        verify(br#"{"a":1,"a":2}"#, &endorser).unwrap_err(),
        IndexError::DuplicateKey
    );
    assert_eq!(
        verify(br#"{"a":1.5}"#, &endorser).unwrap_err(),
        IndexError::UnsupportedNumber
    );
}

#[test]
fn unknown_and_missing_members_are_a_schema_error() {
    let fixture = Fixture::new();
    let endorser = fixture.endorser();
    let bytes = fixture.bytes();

    let extra = mutate(&bytes, |value| {
        value["index"]["extra"] = Value::from(1);
    });
    let missing = mutate(&bytes, |value| {
        value.as_object_mut().unwrap().remove("revocations");
    });
    let bad_key_id = mutate(&bytes, |value| {
        value["signatures"][0]["key_id"] = Value::from("not-hex");
    });

    for bad in [extra, missing, bad_key_id] {
        assert_eq!(verify(&bad, &endorser).unwrap_err(), IndexError::Schema);
    }
}

#[test]
fn another_schema_version_is_unsupported_in_each_member() {
    let fixture = Fixture::new();
    let endorser = fixture.endorser();
    let bytes = fixture.bytes();

    for pointer in [
        "/schema_version",
        "/index/schema_version",
        "/revocations/revocations/schema_version",
    ] {
        let other = mutate(&bytes, |value| {
            *value.pointer_mut(pointer).unwrap() = Value::from(3);
        });
        assert_eq!(
            verify(&other, &endorser).unwrap_err(),
            IndexError::UnsupportedSchemaVersion,
            "{pointer}"
        );
        assert_eq!(
            parse_index_envelope(&other).unwrap_err(),
            IndexError::UnsupportedSchemaVersion,
            "{pointer}"
        );
    }
}

#[test]
fn collection_limits_are_enforced() {
    let fixture = Fixture::new();
    let endorser = fixture.endorser();

    let entries = (0..=MAX_INDEX_ENTRIES as u64)
        .map(|n| entry("pohunek.runtime.muse", "muse", "0.2.0", n))
        .collect::<Vec<_>>();
    let too_many_entries = Fixture::new().with_body(|body| body.entries = entries);
    let too_many_records =
        Fixture::new().with_records(vec![fixture.records[0].clone(); MAX_INDEX_KEY_RECORDS + 1]);
    let too_many_signatures = Fixture::new().with_signatures(vec![
        fixture.signatures[0].clone();
        MAX_INDEX_SIGNATURES + 1
    ]);
    let revoked = (0..=MAX_REVOKED_KEYS as u64).map(numbered_key_id).collect();
    let too_many_revoked = Fixture::new().with_revocations(revocations(&fixture.r, 1, revoked));

    for (bad, limit) in [
        (too_many_entries, IndexLimit::Entries),
        (too_many_records, IndexLimit::KeyRecords),
        (too_many_signatures, IndexLimit::Signatures),
        (too_many_revoked, IndexLimit::RevokedKeys),
    ] {
        assert_eq!(
            verify(&bad.bytes(), &endorser).unwrap_err(),
            IndexError::Limit(limit)
        );
    }
}

#[test]
fn parse_index_envelope_checks_no_signature() {
    let fixture = Fixture::new().with_signatures(Vec::new());
    let envelope = parse_index_envelope(&fixture.bytes()).expect("parses without signatures");

    assert_eq!(envelope.index.entries, vec![muse()]);
    assert_eq!(
        verify_fixture(&fixture).unwrap_err(),
        IndexError::NoSignatures
    );
    assert_eq!(
        parse_index_envelope(&vec![b' '; MAX_INDEX_BYTES + 1]).unwrap_err(),
        IndexError::TooLarge
    );
}

// --- Entry structure --------------------------------------------------------

fn entry_error(entries: Vec<IndexEntry>) -> IndexError {
    error_of(&Fixture::new().with_body(|body| body.entries = entries))
}

fn rejected(index: usize, reason: IndexEntryRejection) -> IndexError {
    IndexError::Entry { index, reason }
}

#[test]
fn entry_platform_lists_are_checked() {
    let with = |platforms: Vec<String>| {
        let mut bad = entry("pohunek.runtime.other", "other", "1.0.0", 2);
        bad.platforms = platforms;
        entry_error(vec![muse(), bad])
    };
    let names = |count: usize| (0..count).map(|n| format!("p{n}")).collect::<Vec<_>>();

    assert_eq!(
        with(Vec::new()),
        rejected(1, IndexEntryRejection::Platforms)
    );
    assert_eq!(with(names(17)), rejected(1, IndexEntryRejection::Platforms));
    assert_eq!(
        with(vec!["X86".to_owned()]),
        rejected(1, IndexEntryRejection::Platforms)
    );
    assert_eq!(
        with(vec!["a".to_owned(), "a".to_owned()]),
        rejected(1, IndexEntryRejection::Platforms)
    );
    let sixteen = Fixture::new().with_body(|body| {
        body.entries[0].platforms = names(16);
    });
    verify_fixture(&sixteen).expect("16 platforms are allowed");
}

#[test]
fn entry_urls_are_printable_ascii_without_whitespace() {
    let with = |url: String| {
        let mut bad = muse();
        bad.url = url;
        entry_error(vec![bad])
    };

    assert_eq!(with(String::new()), rejected(0, IndexEntryRejection::Url));
    assert_eq!(
        with("x".repeat(MAX_ENTRY_URL_BYTES + 1)),
        rejected(0, IndexEntryRejection::Url)
    );
    assert_eq!(
        with("https://a.test/a b".to_owned()),
        rejected(0, IndexEntryRejection::Url)
    );
    assert_eq!(
        with("https://a.test/a\n".to_owned()),
        rejected(0, IndexEntryRejection::Url)
    );
    assert_eq!(
        with("https://a.test/\u{e9}".to_owned()),
        rejected(0, IndexEntryRejection::Url)
    );
    let longest = Fixture::new().with_body(|body| {
        body.entries[0].url = "x".repeat(MAX_ENTRY_URL_BYTES);
    });
    verify_fixture(&longest).expect("512 bytes are allowed");
}

#[test]
fn entry_core_is_only_length_checked_when_the_index_is_verified() {
    let with = |core: String| {
        let mut bad = muse();
        bad.core = core;
        entry_error(vec![bad])
    };
    assert_eq!(with(String::new()), rejected(0, IndexEntryRejection::Core));
    assert_eq!(
        with("x".repeat(129)),
        rejected(0, IndexEntryRejection::Core)
    );

    let unusable = Fixture::new().with_body(|body| {
        body.entries
            .push(entry("pohunek.runtime.other", "other", "1.0.0", 2));
        body.entries[1].core = "not-a-range".to_owned();
    });
    verify_fixture(&unusable).expect("an unparsable core never voids the document");
}

#[test]
fn duplicate_and_conflicting_entries_are_rejected() {
    let mut same_version = muse();
    same_version.digest = digest(2);
    assert_eq!(
        entry_error(vec![muse(), same_version]),
        rejected(1, IndexEntryRejection::DuplicateVersion)
    );

    let mut same_digest = entry("pohunek.runtime.other", "other", "1.0.0", 1);
    same_digest.digest = digest(1);
    assert_eq!(
        entry_error(vec![muse(), same_digest]),
        rejected(1, IndexEntryRejection::DuplicateDigest)
    );

    let two_packages_one_runtime = entry("pohunek.runtime.other", "muse", "1.0.0", 2);
    assert_eq!(
        entry_error(vec![muse(), two_packages_one_runtime]),
        rejected(1, IndexEntryRejection::IdentityConflict)
    );

    let two_runtimes_one_package = entry("pohunek.runtime.muse", "other", "0.3.0", 2);
    assert_eq!(
        entry_error(vec![muse(), two_runtimes_one_package]),
        rejected(1, IndexEntryRejection::IdentityConflict)
    );
}

// --- W2-1.T3: cross-domain and per-entry authorization ----------------------

fn catalog_release() -> Release {
    Release {
        version: "0.40.0".to_owned(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        binary_sets: vec![BinarySet {
            target: PLATFORM.to_owned(),
            digest: Sha256Digest::parse(&format!("sha256:{}", "9".repeat(64))).unwrap(),
        }],
    }
}

#[test]
fn an_index_document_is_refused_as_a_catalog_by_its_schema_version() {
    let fixture = Fixture::new();
    let anchor = TrustAnchor::new(vec![root(&fixture.r)], Vec::new()).unwrap();
    let k_signature = fixture.signatures[0].clone();

    // A catalog-shaped document whose `catalog` member holds the index body.
    // The schema version guard refuses it before any signature is checked;
    // the signature domains are covered by the tests above and below.
    let disguised = serde_json::json!({
        "schema_version": CATALOG_SCHEMA_VERSION,
        "catalog": fixture.body,
        "key_chain": [],
        "signatures": [k_signature],
    });
    let bytes = serde_json::to_vec(&disguised).unwrap();
    assert_eq!(
        verify_catalog(&bytes, &anchor, NOW, None).unwrap_err(),
        CatalogError::UnsupportedSchemaVersion
    );

    // The whole index envelope is not a catalog either.
    assert_eq!(
        verify_catalog(&fixture.bytes(), &anchor, NOW, None).unwrap_err(),
        CatalogError::UnsupportedSchemaVersion
    );
}

#[test]
fn an_index_key_record_is_not_a_catalog_key_record() {
    let fixture = Fixture::new();
    let anchor = TrustAnchor::new(vec![root(&fixture.r)], Vec::new()).unwrap();
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence: 1,
        expires_at: NOW + DAY,
        release: catalog_release(),
        revoked_key_ids: Vec::new(),
        revoked_digests: Vec::new(),
        entries: Vec::new(),
    };
    let signed = sign_catalog(catalog, &fixture.r).unwrap();
    let mut value = serde_json::to_value(&signed).unwrap();
    value["key_chain"] = serde_json::json!([fixture.records[0]]);
    let bytes = serde_json::to_vec(&value).unwrap();

    assert_eq!(
        verify_catalog(&bytes, &anchor, NOW, None).unwrap_err(),
        CatalogError::Schema
    );
}

#[test]
fn a_catalog_is_not_an_index() {
    let fixture = Fixture::new();
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence: 1,
        expires_at: NOW + DAY,
        release: catalog_release(),
        revoked_key_ids: Vec::new(),
        revoked_digests: Vec::new(),
        entries: Vec::new(),
    };
    let signed = sign_catalog(catalog, &fixture.r).unwrap();
    let bytes = serde_json::to_vec(&signed).unwrap();

    // The catalog's schema version is also an index schema version; its shape
    // and its signature domain are not an index's.
    assert_eq!(
        verify(&bytes, &fixture.endorser()).unwrap_err(),
        IndexError::Schema
    );
}

fn authorize(
    verified: &package::VerifiedIndex,
    entry: &IndexEntry,
    core: &str,
) -> Result<(), IndexEntryRefusal> {
    verified.authorize(
        &entry.package_id,
        entry.runtime_id.as_ref().expect("a runtime entry"),
        &entry.version,
        &entry.digest,
        &semver::Version::parse(core).unwrap(),
        PLATFORM,
    )
}

#[test]
fn authorization_is_per_entry_and_core_range_is_parsed_at_install_time() {
    let old = {
        let mut old = entry("pohunek.runtime.old", "old", "1.0.0", 2);
        old.core = "<0.1.0".to_owned();
        old
    };
    let unparsable = {
        let mut unparsable = entry("pohunek.runtime.odd", "odd", "1.0.0", 3);
        unparsable.core = "not-a-range".to_owned();
        unparsable
    };
    let open = {
        let mut open = entry("pohunek.runtime.any", "any", "1.0.0", 4);
        open.core = "*".to_owned();
        open
    };
    let fixture = Fixture::new().with_body(|body| {
        body.entries = vec![muse(), old.clone(), unparsable.clone(), open.clone()];
    });
    let verified = verify_fixture(&fixture).expect("the index verifies as a whole");

    assert_eq!(authorize(&verified, &muse(), "0.40.0"), Ok(()));
    assert_eq!(
        authorize(&verified, &old, "0.40.0"),
        Err(IndexEntryRefusal::CoreExcluded)
    );
    assert_eq!(
        authorize(&verified, &unparsable, "0.40.0"),
        Err(IndexEntryRefusal::CoreRange)
    );
    assert_eq!(
        authorize(&verified, &open, "0.40.0"),
        Err(IndexEntryRefusal::CoreRange)
    );
}

#[test]
fn pre_release_cores_do_not_match_a_plain_range() {
    let verified = verify_fixture(&Fixture::new()).unwrap();

    assert_eq!(
        authorize(&verified, &muse(), "0.41.0-rc.1"),
        Err(IndexEntryRefusal::CoreExcluded)
    );
    assert_eq!(authorize(&verified, &muse(), "0.41.0"), Ok(()));
    assert_eq!(
        authorize(&verified, &muse(), "0.39.9"),
        Err(IndexEntryRefusal::CoreExcluded)
    );
}

#[test]
fn authorization_checks_digest_identity_namespace_and_platform_in_order() {
    let verified = verify_fixture(&Fixture::new()).unwrap();
    let listed = muse();
    let core = semver::Version::new(0, 40, 0);
    let call = |package: &str, runtime: &str, version: &str, digest_index: u64, platform: &str| {
        verified.authorize(
            &PackageId::parse(package).unwrap(),
            &RuntimeId::parse(runtime).unwrap(),
            &PackageVersion::parse(version).unwrap(),
            &digest(digest_index),
            &core,
            platform,
        )
    };

    assert_eq!(
        call("pohunek.runtime.muse", "muse", "0.2.0", 1, PLATFORM),
        Ok(())
    );
    assert_eq!(
        call("pohunek.runtime.muse", "muse", "0.2.0", 9, PLATFORM),
        Err(IndexEntryRefusal::NotListed)
    );
    for (package, runtime, version) in [
        ("pohunek.runtime.other", "muse", "0.2.0"),
        ("pohunek.runtime.muse", "other", "0.2.0"),
        ("pohunek.runtime.muse", "muse", "0.2.1"),
    ] {
        assert_eq!(
            call(package, runtime, version, 1, PLATFORM),
            Err(IndexEntryRefusal::IdentityMismatch),
            "{package} {runtime} {version}"
        );
    }
    assert_eq!(
        call(
            "pohunek.runtime.muse",
            "muse",
            "0.2.0",
            1,
            "aarch64-apple-darwin"
        ),
        Err(IndexEntryRefusal::Platform)
    );
    assert_eq!(listed.platforms, vec![PLATFORM.to_owned()]);
}

#[test]
fn entries_outside_the_namespace_or_binding_are_refused() {
    let outside = entry("acme.runtime.zed", "zed", "1.0.0", 2);
    let unbound_runtime = entry("pohunek.runtime.foo", "bar", "1.0.0", 3);
    let unbound_package = entry("pohunek.thing", "thing", "1.0.0", 4);
    let fixture = Fixture::new().with_body(|body| {
        body.entries = vec![
            muse(),
            outside.clone(),
            unbound_runtime.clone(),
            unbound_package.clone(),
        ];
    });
    let verified = verify_fixture(&fixture).expect("verifies");

    assert_eq!(
        authorize(&verified, &outside, "0.40.0"),
        Err(IndexEntryRefusal::Namespace)
    );
    assert_eq!(
        authorize(&verified, &unbound_runtime, "0.40.0"),
        Err(IndexEntryRefusal::PackageIdBinding)
    );
    assert_eq!(
        authorize(&verified, &unbound_package, "0.40.0"),
        Err(IndexEntryRefusal::PackageIdBinding)
    );
}

#[test]
fn refused_runtime_ids_are_never_authorized() {
    let refused: Vec<IndexEntry> = ["shell", "codex", "claude", "hermes", "pi"]
        .iter()
        .enumerate()
        .map(|(n, runtime)| {
            entry(
                &format!("pohunek.runtime.{runtime}"),
                runtime,
                "9.0.0",
                10 + n as u64,
            )
        })
        .collect();
    let fixture = Fixture::new().with_body(|body| {
        body.entries = refused.clone();
    });
    let verified = verify_fixture(&fixture).expect("a refused entry never voids the document");

    for refused_entry in &refused {
        assert_eq!(
            authorize(&verified, refused_entry, "0.40.0"),
            Err(IndexEntryRefusal::RuntimeRefused),
            "{}",
            refused_entry.runtime_id.as_ref().expect("a runtime entry")
        );
    }
}

#[test]
fn refused_runtime_is_reported_before_the_package_binding() {
    // Release `pi` under a foreign package id: the runtime refusal wins.
    let foreign = entry("pohunek.somethingelse", "pi", "9.0.0", 2);
    // A release package id under another runtime id: the binding refuses it.
    let impostor = entry("pohunek.runtime.pi", "pix", "9.0.0", 3);
    let fixture = Fixture::new().with_body(|body| {
        body.entries = vec![muse(), foreign.clone(), impostor.clone()];
    });
    let verified = verify_fixture(&fixture).expect("verifies");

    assert_eq!(
        authorize(&verified, &foreign, "0.40.0"),
        Err(IndexEntryRefusal::RuntimeRefused)
    );
    assert_eq!(
        authorize(&verified, &impostor, "0.40.0"),
        Err(IndexEntryRefusal::PackageIdBinding)
    );
    assert_eq!(authorize(&verified, &muse(), "0.40.0"), Ok(()));
}

#[test]
fn key_record_message_is_the_domain_plus_the_canonical_record() {
    let fixture = Fixture::new();
    let record = &fixture.records[0];
    let expected = format!(
        "pohunek-plugin-index-key-record-v1\n{{\"endorsed_by\":\"{}\",\"key_id\":\"{}\",\"not_after\":{},\"not_before\":{},\"public_key\":\"{}\",\"scope\":\"official\"}}",
        record.endorsed_by, record.key_id, record.not_after, record.not_before, record.public_key
    );

    assert_eq!(
        index_key_record_signing_message(record),
        expected.as_bytes()
    );
}

#[test]
fn endorsement_covers_scope_and_window_start() {
    let fixture = Fixture::new();

    // R endorsed the key for scope `verified`; relabeling it keeps the signature.
    let mut relabeled = record(&fixture.r, &fixture.k, "verified", KEY_START, KEY_END);
    relabeled.scope = "official".to_owned();
    assert_eq!(
        error_of(&Fixture::new().with_records(vec![relabeled])),
        IndexError::BadEndorsement
    );

    let mut earlier = fixture.records[0].clone();
    earlier.not_before -= 1;
    assert_eq!(
        error_of(&Fixture::new().with_records(vec![earlier])),
        IndexError::BadEndorsement
    );
}

#[test]
fn a_catalog_signature_under_the_index_domain_does_not_verify_as_a_catalog() {
    let fixture = Fixture::new();
    let anchor = TrustAnchor::new(vec![root(&fixture.r)], Vec::new()).unwrap();
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence: 1,
        expires_at: NOW + DAY,
        release: catalog_release(),
        revoked_key_ids: Vec::new(),
        revoked_digests: Vec::new(),
        entries: Vec::new(),
    };
    let genuine = sign_catalog(catalog.clone(), &fixture.r).unwrap();
    verify_catalog(&serde_json::to_vec(&genuine).unwrap(), &anchor, NOW, None)
        .expect("the control catalog verifies");

    let message = swap_domain(
        &package::catalog_signing_message(&catalog).unwrap(),
        CATALOG_DOMAIN,
        INDEX_DOMAIN,
    );
    let mut forged = genuine;
    forged.signatures[0].signature = hex(&fixture.r.sign(&message).to_bytes());
    assert_eq!(
        verify_catalog(&serde_json::to_vec(&forged).unwrap(), &anchor, NOW, None).unwrap_err(),
        CatalogError::BadSignature
    );
}

// --- schema 2: the `context` entry kind -------------------------------------

fn context_entry(package: &str, version: &str, digest_index: u64) -> IndexEntry {
    IndexEntry {
        package_id: PackageId::parse(package).expect("package id"),
        kind: Some(IndexEntryKind::Context),
        runtime_id: None,
        version: PackageVersion::parse(version).expect("version"),
        digest: digest(digest_index),
        url: format!("https://example.test/{package}-{version}.tar.zst"),
        platforms: vec![PLATFORM.to_owned()],
        core: ">=0.42.0".to_owned(),
    }
}

fn kinded(mut entry: IndexEntry) -> IndexEntry {
    entry.kind = Some(IndexEntryKind::Runtime);
    entry
}

fn assistant() -> IndexEntry {
    context_entry("pohunek.context.assistant", "1.0.0", 7)
}

fn authorize_context(
    verified: &package::VerifiedIndex,
    entry: &IndexEntry,
    core: &str,
) -> Result<(), IndexEntryRefusal> {
    verified.authorize_context(
        &entry.package_id,
        &entry.version,
        &entry.digest,
        &semver::Version::parse(core).unwrap(),
        PLATFORM,
    )
}

#[test]
fn a_schema_2_index_lists_runtime_and_context_entries() {
    let fixture = Fixture::new().schema_2(vec![kinded(muse()), assistant()]);
    let verified = verify_fixture(&fixture).expect("a schema 2 index verifies");

    assert_eq!(verified.entries().len(), 2);
    assert_eq!(authorize_context(&verified, &assistant(), "0.42.0"), Ok(()));
    assert_eq!(
        authorize_context(&verified, &assistant(), "0.41.0"),
        Err(IndexEntryRefusal::CoreExcluded)
    );
    assert_eq!(authorize(&verified, &muse(), "0.42.0"), Ok(()));
}

#[test]
fn no_entry_kind_authorizes_the_other() {
    let fixture = Fixture::new().schema_2(vec![kinded(muse()), assistant()]);
    let verified = verify_fixture(&fixture).expect("a schema 2 index verifies");

    // The runtime entry's digest under the context rule, and the reverse.
    let mut as_context = assistant();
    as_context.digest = muse().digest;
    assert_eq!(
        authorize_context(&verified, &as_context, "0.42.0"),
        Err(IndexEntryRefusal::KindMismatch)
    );
    let mut as_runtime = muse();
    as_runtime.digest = assistant().digest;
    assert_eq!(
        authorize(&verified, &as_runtime, "0.42.0"),
        Err(IndexEntryRefusal::KindMismatch)
    );
}

#[test]
fn entry_kind_and_runtime_id_must_fit_the_schema() {
    // A schema 1 entry has no kind.
    let mut kinded_v1 = muse();
    kinded_v1.kind = Some(IndexEntryKind::Runtime);
    let fixture = Fixture::new().with_body(|body| body.entries = vec![kinded_v1]);
    assert_eq!(
        error_of(&fixture),
        IndexError::Entry {
            index: 0,
            reason: IndexEntryRejection::Kind
        }
    );

    // A schema 2 entry has one.
    let fixture = Fixture::new().schema_2(vec![muse()]);
    assert_eq!(
        error_of(&fixture),
        IndexError::Entry {
            index: 0,
            reason: IndexEntryRejection::Kind
        }
    );

    // A context entry has no runtime id, a runtime entry has one.
    let mut context_with_runtime = assistant();
    context_with_runtime.runtime_id = Some(RuntimeId::parse("assistant").expect("runtime id"));
    let mut runtime_without = kinded(muse());
    runtime_without.runtime_id = None;
    for bad in [context_with_runtime, runtime_without] {
        let fixture = Fixture::new().schema_2(vec![bad]);
        assert_eq!(
            error_of(&fixture),
            IndexError::Entry {
                index: 0,
                reason: IndexEntryRejection::Kind
            }
        );
    }
}

#[test]
fn the_package_id_prefix_belongs_to_the_entry_kind() {
    let context_under_runtime_prefix = context_entry("pohunek.runtime.assistant", "1.0.0", 7);
    let context_without_tail = context_entry("pohunek.context.", "1.0.0", 8);
    let runtime_under_context_prefix = kinded(entry("pohunek.context.muse", "muse", "0.2.0", 9));
    for bad in [
        context_under_runtime_prefix,
        context_without_tail,
        runtime_under_context_prefix,
    ] {
        let fixture = Fixture::new().schema_2(vec![bad]);
        assert_eq!(
            error_of(&fixture),
            IndexError::Entry {
                index: 0,
                reason: IndexEntryRejection::PackageIdKind
            }
        );
    }
}

#[test]
fn a_schema_1_signature_never_verifies_a_schema_2_body() {
    let fixture = Fixture::new();
    let v1_signature = fixture.signatures.clone();
    let fixture = fixture
        .schema_2(vec![kinded(muse())])
        .with_signatures(v1_signature);

    assert_eq!(error_of(&fixture), IndexError::BadSignature);
}

#[test]
fn a_schema_3_document_is_unsupported() {
    let bytes = mutate(&Fixture::new().bytes(), |value| {
        value["schema_version"] = Value::from(3);
    });
    assert_eq!(
        verify(&bytes, &Fixture::new().endorser()).expect_err("schema 3"),
        IndexError::UnsupportedSchemaVersion
    );
}
