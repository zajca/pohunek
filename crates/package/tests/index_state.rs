// Rust guideline compliant 2026-10-05

//! Behavioural tests of the persisted plugin index verifier state and of the
//! index channel file.
//!
//! Documents are signed with keys derived from fixed labels and `now` is
//! explicit, so no test needs a random number generator or the clock.
//! Expected marks and revoked ids come from the slice specification, not from
//! the code under test.

#![cfg(unix)]

mod common;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{mode_of, set_mode, PluginFixture};
use ed25519_dalek::{Signer as _, SigningKey};
use package::registry::RegistryError;
use package::{
    index_key_record_signing_message, index_signing_message, parse_index_channel,
    revocation_signing_message, verify_index, AnchorFile, CatalogSignature, IndexBody,
    IndexChannel, IndexChannelError, IndexEntry, IndexError, IndexKeyRecord, IndexVerifierState,
    KeyId, PackageDigest, RevocationBody, RootKey, SignedRevocations, VerifiedIndex,
    INDEX_SCHEMA_VERSION, MAX_INDEX_CHANNEL_BYTES, MAX_REVOKED_KEYS,
};
use protocol::{PackageId, PackageVersion, RuntimeId};
use sha2::{Digest as _, Sha256};

const NOW: u64 = 1_800_000_000;
const DAY: u64 = 24 * 60 * 60;
const ROOT_START: u64 = NOW - 1_000_000;
const ROOT_END: u64 = NOW + 100_000_000;
const KEY_START: u64 = NOW - 1_000;
const KEY_END: u64 = NOW + 10_000_000;
/// Index sequences in the scenarios are offsets from this second, so every
/// document is unexpired and not ahead of the clock.
const BASE: u64 = NOW - 3_600;

fn signing_key(label: &str) -> SigningKey {
    let seed: [u8; 32] = Sha256::digest(label.as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn key_id(label: &str) -> KeyId {
    KeyId::derive(&signing_key(label).verifying_key())
}

fn root(label: &str) -> RootKey {
    RootKey::new(
        signing_key(label).verifying_key().to_bytes(),
        ROOT_START,
        ROOT_END,
    )
    .expect("valid root")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String");
        text
    })
}

fn endorsed_record(subject: &str) -> IndexKeyRecord {
    let endorser = signing_key("endorser-root");
    let subject_key = signing_key(subject);
    let mut record = IndexKeyRecord {
        key_id: KeyId::derive(&subject_key.verifying_key()),
        public_key: hex(subject_key.verifying_key().as_bytes()),
        scope: "official".to_owned(),
        not_before: KEY_START,
        not_after: KEY_END,
        endorsed_by: key_id("endorser-root"),
        signature: String::new(),
    };
    record.signature = hex(&endorser
        .sign(&index_key_record_signing_message(&record))
        .to_bytes());
    record
}

/// Index bytes signed by the index key named `signer`, with the given
/// sequences and embedded revocations.
fn index_bytes(signer: &str, sequence: u64, revocation_sequence: u64, revoked: &[&str]) -> Vec<u8> {
    let endorser = signing_key("endorser-root");
    let body = IndexBody {
        schema_version: INDEX_SCHEMA_VERSION,
        scope: "official".to_owned(),
        sequence,
        expires_at: sequence + 7 * DAY,
        entries: vec![IndexEntry {
            package_id: PackageId::parse("pohunek.runtime.muse").expect("package id"),
            runtime_id: RuntimeId::parse("muse").expect("runtime id"),
            version: PackageVersion::parse("0.2.0").expect("version"),
            digest: PackageDigest::parse(&format!("sha256:{:064x}", 1)).expect("digest"),
            url: "https://example.test/muse.tar.zst".to_owned(),
            platforms: vec!["x86_64-unknown-linux-gnu".to_owned()],
            core: ">=0.40.0".to_owned(),
        }],
    };
    let signer_key = signing_key(signer);
    let signature = CatalogSignature {
        key_id: KeyId::derive(&signer_key.verifying_key()),
        signature: hex(&signer_key
            .sign(&index_signing_message(&body).expect("message"))
            .to_bytes()),
    };
    let revocation_body = RevocationBody {
        schema_version: INDEX_SCHEMA_VERSION,
        sequence: revocation_sequence,
        revoked_key_ids: revoked.iter().map(|label| key_id(label)).collect(),
    };
    let revocations = SignedRevocations {
        signature: CatalogSignature {
            key_id: key_id("endorser-root"),
            signature: hex(&endorser
                .sign(&revocation_signing_message(&revocation_body).expect("message"))
                .to_bytes()),
        },
        revocations: revocation_body,
    };
    serde_json::to_vec(&serde_json::json!({
        "schema_version": INDEX_SCHEMA_VERSION,
        "index": body,
        "key_records": [endorsed_record("index-key"), endorsed_record("index-key-2")],
        "signatures": [signature],
        "revocations": revocations,
    }))
    .expect("serialize")
}

fn verify_with(bytes: &[u8], state: &IndexVerifierState) -> Result<VerifiedIndex, IndexError> {
    verify_index(bytes, &root("endorser-root"), &BTreeSet::new(), state, NOW)
}

/// A verified index signed by K with `sequence` offsets from [`BASE`].
fn verified(offset: u64, revocation_sequence: u64, revoked: &[&str]) -> VerifiedIndex {
    let bytes = index_bytes("index-key", BASE + offset, revocation_sequence, revoked);
    verify_with(&bytes, &IndexVerifierState::default()).expect("valid index")
}

fn state_file(fixture: &PluginFixture) -> PathBuf {
    fixture.plugins().join("index-state.json")
}

fn ids(labels: &[&str]) -> BTreeSet<KeyId> {
    labels.iter().map(|label| key_id(label)).collect()
}

// --- W2-2.T1: marks only move up ---------------------------------------------

#[test]
fn a_recorded_index_round_trips_across_reopen() {
    let fixture = PluginFixture::new();
    let recorded = fixture
        .open()
        .record_index(&verified(10, 1, &["old-a", "old-b"]))
        .unwrap();
    assert_eq!(recorded.index_high_water, Some(BASE + 10));
    assert_eq!(recorded.revocation_high_water, Some(1));
    assert_eq!(recorded.revoked_key_ids, ids(&["old-a", "old-b"]));

    assert_eq!(fixture.open().index_state().unwrap(), recorded);
    assert_eq!(mode_of(&state_file(&fixture)), 0o600);
    assert!(
        !fixture.plugins().join("index-state.json.tmp").exists(),
        "no temporary is left behind"
    );
}

#[test]
fn the_index_mark_only_moves_up_and_revoked_ids_unite() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_index(&verified(10, 1, &["a"])).unwrap();
    let after_ten = std::fs::read(state_file(&fixture)).unwrap();

    let after_seven = registry.record_index(&verified(7, 1, &["late"])).unwrap();
    assert_eq!(after_seven.index_high_water, Some(BASE + 10));
    assert_eq!(after_seven.revoked_key_ids, ids(&["a"]));
    assert_eq!(std::fs::read(state_file(&fixture)).unwrap(), after_ten);

    let after_twelve = registry.record_index(&verified(12, 1, &["b"])).unwrap();
    assert_eq!(after_twelve.index_high_water, Some(BASE + 12));
    assert_eq!(
        registry.index_state().unwrap().index_high_water,
        Some(BASE + 12)
    );
    assert_eq!(after_twelve.revoked_key_ids, ids(&["a", "b"]));
    let stored = std::fs::read_to_string(state_file(&fixture)).unwrap();
    let (first, second) = (key_id("a").to_string(), key_id("b").to_string());
    let (low, high) = if first < second {
        (first, second)
    } else {
        (second, first)
    };
    assert!(
        stored.contains(&format!("\"{low}\",\"{high}\"")),
        "revoked ids are stored sorted"
    );
}

#[test]
fn the_revocation_mark_only_moves_up() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_index(&verified(10, 5, &[])).unwrap();
    let before = std::fs::read(state_file(&fixture)).unwrap();

    // A newer index carrying an older revocation list, and the reverse, merge
    // nothing: neither mark moves and no id is taken.
    let older_revocation = registry.record_index(&verified(12, 4, &["x"])).unwrap();
    assert_eq!(older_revocation.index_high_water, Some(BASE + 10));
    assert_eq!(older_revocation.revocation_high_water, Some(5));
    assert!(older_revocation.revoked_key_ids.is_empty());
    let older_index = registry.record_index(&verified(7, 9, &["y"])).unwrap();
    assert_eq!(older_index.index_high_water, Some(BASE + 10));
    assert_eq!(older_index.revocation_high_water, Some(5));
    assert!(older_index.revoked_key_ids.is_empty());
    assert_eq!(std::fs::read(state_file(&fixture)).unwrap(), before);

    let raised = registry.record_index(&verified(12, 6, &[])).unwrap();
    assert_eq!(raised.index_high_water, Some(BASE + 12));
    assert_eq!(raised.revocation_high_water, Some(6));
}

#[test]
fn an_equal_sequence_is_merged() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_index(&verified(10, 1, &["a"])).unwrap();
    let state = registry.record_index(&verified(10, 1, &["b"])).unwrap();
    assert_eq!(state.index_high_water, Some(BASE + 10));
    assert_eq!(state.revoked_key_ids, ids(&["a", "b"]));
}

#[test]
fn a_merge_that_changes_nothing_writes_nothing() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_index(&verified(10, 1, &["a"])).unwrap();
    let before = std::fs::metadata(state_file(&fixture)).unwrap();
    let bytes = std::fs::read(state_file(&fixture)).unwrap();

    registry.record_index(&verified(10, 1, &["a"])).unwrap();
    // A rewrite renames a new file into place, which changes the inode.
    assert_eq!(
        std::fs::metadata(state_file(&fixture)).unwrap().ino(),
        before.ino()
    );
    assert_eq!(std::fs::read(state_file(&fixture)).unwrap(), bytes);
}

#[test]
fn the_revoked_key_cap_is_enforced_without_writing() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let labels: Vec<String> = (0..MAX_REVOKED_KEYS).map(|n| format!("k{n}")).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    registry.record_index(&verified(1, 1, &refs)).unwrap();
    let before = std::fs::read(state_file(&fixture)).unwrap();

    assert_eq!(
        registry
            .record_index(&verified(2, 2, &["one-too-many"]))
            .unwrap_err(),
        RegistryError::TooManyRevokedKeys
    );
    assert_eq!(std::fs::read(state_file(&fixture)).unwrap(), before);
    assert_eq!(
        registry.index_state().unwrap().index_high_water,
        Some(BASE + 1)
    );
}

// --- W2-2.T2: a damaged file is never replaced --------------------------------

#[test]
fn a_corrupt_state_file_is_reported_and_never_replaced() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_index(&verified(3, 1, &["a", "b"])).unwrap();
    let good = std::fs::read_to_string(state_file(&fixture)).unwrap();
    let (first, second) = (key_id("a").to_string(), key_id("b").to_string());
    let (low, high) = if first < second {
        (&first, &second)
    } else {
        (&second, &first)
    };
    let too_many: Vec<String> = (0..=MAX_REVOKED_KEYS)
        .map(|n| format!("\"{n:064x}\""))
        .collect();

    let corruptions: Vec<(&str, Vec<u8>)> = vec![
        ("truncated", b"{\"schema\":1,".to_vec()),
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
        (
            "over the id cap",
            format!(
                "{{\"schema\":1,\"index_high_water\":1,\"revocation_high_water\":1,\"revoked_key_ids\":[{}]}}",
                too_many.join(",")
            )
            .into_bytes(),
        ),
        ("oversized", vec![b' '; 64 * 1024]),
    ];
    for (label, bytes) in corruptions {
        std::fs::write(state_file(&fixture), &bytes).unwrap();
        assert_eq!(
            registry.index_state().unwrap_err(),
            RegistryError::Corrupt,
            "{label}"
        );
        assert_eq!(
            registry.record_index(&verified(9, 9, &[])).unwrap_err(),
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
    registry.record_index(&verified(3, 1, &[])).unwrap();
    let text = std::fs::read_to_string(state_file(&fixture)).unwrap();
    let changed = text.replace("\"schema\":1", "\"schema\":2");
    std::fs::write(state_file(&fixture), &changed).unwrap();

    assert_eq!(
        registry.index_state().unwrap_err(),
        RegistryError::UnsupportedSchema
    );
    assert_eq!(
        registry.record_index(&verified(9, 9, &[])).unwrap_err(),
        RegistryError::UnsupportedSchema
    );
    assert_eq!(
        std::fs::read_to_string(state_file(&fixture)).unwrap(),
        changed
    );
}

#[test]
fn the_state_file_must_be_an_owner_private_regular_file() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_index(&verified(3, 1, &[])).unwrap();

    set_mode(&state_file(&fixture), 0o644);
    assert_eq!(registry.index_state().unwrap_err(), RegistryError::Unsafe);
    assert_eq!(
        registry.record_index(&verified(9, 9, &[])).unwrap_err(),
        RegistryError::Unsafe
    );
    assert_eq!(registry.index_state().unwrap_err(), RegistryError::Unsafe);
    set_mode(&state_file(&fixture), 0o600);
    registry.index_state().unwrap();

    let moved = fixture.base.join("state");
    std::fs::rename(state_file(&fixture), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, state_file(&fixture)).unwrap();
    assert_eq!(registry.index_state().unwrap_err(), RegistryError::Unsafe);
    assert_eq!(
        registry.record_index(&verified(9, 9, &[])).unwrap_err(),
        RegistryError::Unsafe
    );
}

#[test]
fn the_index_state_is_independent_of_the_catalog_state() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.record_index(&verified(3, 1, &["a"])).unwrap();

    assert!(!fixture.plugins().join("catalog-state.json").exists());
    assert_eq!(registry.catalog_state().unwrap().high_water(), None);
    std::fs::write(fixture.plugins().join("catalog-state.json"), b"{ not json").unwrap();
    set_mode(&fixture.plugins().join("catalog-state.json"), 0o600);
    assert_eq!(
        registry.index_state().unwrap().index_high_water,
        Some(BASE + 3)
    );
}

// --- crash points (design 3.7a) -----------------------------------------------

#[test]
fn a_verified_index_that_was_never_recorded_leaves_no_trace() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let bytes = index_bytes("index-key", BASE + 10, 1, &["a"]);
    verify_with(&bytes, &registry.index_state().unwrap()).unwrap();

    assert!(!state_file(&fixture).exists());
    // The attempt after the crash verifies the same document again.
    verify_with(&bytes, &registry.index_state().unwrap()).unwrap();
}

#[test]
fn a_stale_temporary_does_not_block_the_next_write() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let temporary = fixture.plugins().join("index-state.json.tmp");
    std::fs::write(&temporary, b"junk").unwrap();
    set_mode(&temporary, 0o600);

    registry.record_index(&verified(2, 1, &[])).unwrap();
    assert_eq!(
        registry.index_state().unwrap().index_high_water,
        Some(BASE + 2)
    );
    assert!(!temporary.exists());
}

#[test]
fn a_recorded_document_is_accepted_again_and_records_nothing_more() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let bytes = index_bytes("index-key", BASE + 10, 1, &["a"]);
    let first = verify_with(&bytes, &registry.index_state().unwrap()).unwrap();
    registry.record_index(&first).unwrap();
    let before = std::fs::metadata(state_file(&fixture)).unwrap().ino();

    // Crash after the state rename, before the package install: the retry
    // verifies the same document against the raised marks.
    let again = verify_with(&bytes, &registry.index_state().unwrap()).unwrap();
    registry.record_index(&again).unwrap();
    assert_eq!(
        std::fs::metadata(state_file(&fixture)).unwrap().ino(),
        before
    );
}

// --- the persisted state feeds the verifier (design 3.4, W2-2.D3) --------------

#[test]
fn the_persisted_state_makes_the_verifier_refuse_replays_and_revoked_keys() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let accepted = index_bytes("index-key", BASE + 10, 5, &["index-key-2"]);
    let verified_first = verify_with(&accepted, &registry.index_state().unwrap()).unwrap();
    registry.record_index(&verified_first).unwrap();
    let state = registry.index_state().unwrap();

    let older = index_bytes("index-key", BASE + 7, 5, &[]);
    assert_eq!(
        verify_with(&older, &state).unwrap_err(),
        IndexError::StaleSequence {
            sequence: BASE + 7,
            high_water: BASE + 10
        }
    );
    let older_revocation = index_bytes("index-key", BASE + 12, 4, &[]);
    assert_eq!(
        verify_with(&older_revocation, &state).unwrap_err(),
        IndexError::StaleRevocation {
            sequence: 4,
            high_water: 5
        }
    );
    // The earlier document revoked K2; a later document that omits the
    // revocation still cannot be signed by it.
    let revoked_signer = index_bytes("index-key-2", BASE + 12, 5, &[]);
    assert_eq!(
        verify_with(&revoked_signer, &state).unwrap_err(),
        IndexError::SignerRevoked
    );
    verify_with(&accepted, &state).expect("the recorded document verifies again");
    verify_with(&index_bytes("index-key", BASE + 12, 5, &[]), &state)
        .expect("a newer document verifies");
}

// --- concurrency (design 3.7b) --------------------------------------------------

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
    let labels: Vec<String> = (1..=WRITERS).map(|n| format!("writer-{n}")).collect();
    let documents: Vec<VerifiedIndex> = (1..=WRITERS)
        .zip(&labels)
        .map(|(n, label)| verified(n, n, &[label.as_str()]))
        .collect();
    let recorded = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for document in &documents {
            let (fixture, recorded) = (&fixture, &recorded);
            scope.spawn(move || {
                let registry = fixture.open();
                until_unlocked(|| registry.record_index(document));
                recorded.fetch_add(1, Ordering::Relaxed);
            });
        }
    });

    assert_eq!(recorded.load(Ordering::Relaxed), 8);
    let state = fixture.open().index_state().unwrap();
    assert_eq!(state.index_high_water, Some(BASE + WRITERS));
    assert_eq!(state.revocation_high_water, Some(WRITERS));
    // A writer that ran after a higher sequence was stored is ignored, so
    // only the highest document's revocations are guaranteed.
    assert!(state.revoked_key_ids.contains(&key_id("writer-8")));
    assert!(state
        .revoked_key_ids
        .iter()
        .all(|id| labels.iter().any(|label| &key_id(label) == id)));
}

#[test]
fn concurrent_writers_with_one_sequence_lose_no_revoked_id() {
    const WRITERS: u64 = 8;
    let fixture = PluginFixture::new();
    fixture.open();
    let labels: Vec<String> = (1..=WRITERS).map(|n| format!("same-{n}")).collect();
    let documents: Vec<VerifiedIndex> = labels
        .iter()
        .map(|label| verified(5, 1, &[label.as_str()]))
        .collect();

    std::thread::scope(|scope| {
        for document in &documents {
            let fixture = &fixture;
            scope.spawn(move || {
                let registry = fixture.open();
                until_unlocked(|| registry.record_index(document));
            });
        }
    });

    let state = fixture.open().index_state().unwrap();
    assert_eq!(state.index_high_water, Some(BASE + 5));
    assert_eq!(
        state.revoked_key_ids,
        labels.iter().map(|label| key_id(label)).collect()
    );
}

// --- W2-2.T3: the channel file ---------------------------------------------------

fn anchor(labels: &[&str]) -> AnchorFile {
    AnchorFile::new(labels.iter().map(|label| root(label)).collect(), Vec::new())
        .expect("valid anchor")
}

fn channel_text(label: &str) -> String {
    format!(
        "{{\"schema_version\":1,\"index_endorser_key_id\":\"{}\"}}",
        key_id(label)
    )
}

fn channel_error(text: &str) -> IndexChannelError {
    parse_index_channel(text.as_bytes()).expect_err("must be rejected")
}

#[test]
fn the_channel_names_the_anchor_root_with_its_id() {
    let channel = parse_index_channel(channel_text("endorser-root").as_bytes()).unwrap();
    // The endorser is neither the first nor the last root.
    let anchor = anchor(&["ci-root", "endorser-root", "other-root"]);

    let endorser = channel.endorser(&anchor).unwrap();
    assert_eq!(endorser.id(), &key_id("endorser-root"));
    assert_eq!(endorser.public_key(), root("endorser-root").public_key());
    assert_eq!(endorser.not_before(), ROOT_START);
    assert_eq!(endorser.not_after(), ROOT_END);
}

#[test]
fn an_endorser_outside_the_anchor_is_refused() {
    let channel = parse_index_channel(channel_text("stranger").as_bytes()).unwrap();
    assert_eq!(
        channel
            .endorser(&anchor(&["ci-root", "endorser-root"]))
            .unwrap_err(),
        IndexChannelError::EndorserNotInAnchor
    );
}

#[test]
fn a_channel_that_does_not_match_the_schema_is_refused() {
    let id = key_id("endorser-root");
    let cases = [
        (
            "extra member",
            format!("{{\"schema_version\":1,\"index_endorser_key_id\":\"{id}\",\"extra\":true}}"),
        ),
        ("missing id", "{\"schema_version\":1}".to_owned()),
        (
            "missing version",
            format!("{{\"index_endorser_key_id\":\"{id}\"}}"),
        ),
        (
            "short id",
            "{\"schema_version\":1,\"index_endorser_key_id\":\"abcd\"}".to_owned(),
        ),
        (
            "uppercase id",
            format!(
                "{{\"schema_version\":1,\"index_endorser_key_id\":\"{}\"}}",
                id.to_string().to_uppercase()
            ),
        ),
        (
            "id is not a string",
            "{\"schema_version\":1,\"index_endorser_key_id\":7}".to_owned(),
        ),
        (
            "version is not a number",
            format!("{{\"schema_version\":\"1\",\"index_endorser_key_id\":\"{id}\"}}"),
        ),
        ("not an object", "[]".to_owned()),
    ];
    for (label, text) in cases {
        assert_eq!(channel_error(&text), IndexChannelError::Schema, "{label}");
    }
}

#[test]
fn an_unsupported_channel_schema_version_is_refused() {
    for version in ["0", "2", "18446744073709551615"] {
        let text = format!(
            "{{\"schema_version\":{version},\"index_endorser_key_id\":\"{}\"}}",
            key_id("endorser-root")
        );
        assert_eq!(
            channel_error(&text),
            IndexChannelError::UnsupportedSchemaVersion,
            "version {version}"
        );
    }
}

#[test]
fn a_malformed_channel_is_refused_before_it_is_interpreted() {
    let id = key_id("endorser-root");
    assert_eq!(
        parse_index_channel(&vec![b' '; MAX_INDEX_CHANNEL_BYTES + 1]).unwrap_err(),
        IndexChannelError::TooLarge
    );
    let mut padded = channel_text("endorser-root").into_bytes();
    padded.resize(MAX_INDEX_CHANNEL_BYTES, b' ');
    parse_index_channel(&padded).expect("a document of exactly the limit parses");
    assert_eq!(channel_error("{ not json"), IndexChannelError::Syntax);
    assert_eq!(channel_error(""), IndexChannelError::Syntax);
    assert_eq!(
        channel_error(&format!("{}trailing", channel_text("endorser-root"))),
        IndexChannelError::Syntax
    );
    assert_eq!(
        channel_error(&format!(
            "{{\"schema_version\":1,\"schema_version\":1,\"index_endorser_key_id\":\"{id}\"}}"
        )),
        IndexChannelError::DuplicateKey
    );
    for number in ["1.0", "-1", "1e0"] {
        assert_eq!(
            channel_error(&format!(
                "{{\"schema_version\":{number},\"index_endorser_key_id\":\"{id}\"}}"
            )),
            IndexChannelError::UnsupportedNumber,
            "{number}"
        );
    }
}

#[test]
fn a_channel_serializes_to_indented_json_that_parses_back() {
    let channel = IndexChannel::new(key_id("endorser-root"));
    let bytes = channel.to_bytes().unwrap();
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        format!(
            "{{\n  \"schema_version\": 1,\n  \"index_endorser_key_id\": \"{}\"\n}}\n",
            key_id("endorser-root")
        )
    );
    assert_eq!(parse_index_channel(&bytes).unwrap(), channel);
    assert_eq!(channel.index_endorser_key_id(), &key_id("endorser-root"));
}
