//! Plugin index trust of the package lifecycle: installs authorized by a
//! signed index and every way an index or an entry is refused.
//!
//! Keys derive from fixed labels. Sequences, expiries and key windows derive
//! from the real clock, because the daemon reads it directly and the verifier
//! bounds a sequence by `now`.

// Rust guideline compliant 2026-11-05

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer as _, SigningKey};
use package::registry::Registry;
use package::{
    index_key_record_signing_message, index_signing_message, revocation_signing_message,
    CatalogSignature, IndexBody, IndexEntry, IndexEnvelope, IndexKeyRecord, KeyId, Limits,
    RevocationBody, RootKey, SignedRevocations, INDEX_SCHEMA_VERSION,
};
use pohunek_test_support::catalog::host_platform;
use protocol::{
    PackageErrorKind, PackageId, PackageInstallParams, PackageOrigin, PackageTrust, PackageVersion,
    RuntimeId,
};

use super::catalog_tests::{anchor, catalog, catalog_install, entry, signed_by, signing_key};
use super::test_fixture::{Fixture, Package, HOST_STATE_COMMAND, INERT_PROGRAM, PACKAGE, RUNTIME};
use crate::catalog_anchor::{CatalogTrust, IndexChannelFault, IndexTrust};
use crate::packages::HostTrustAnchor;

/// One day in seconds.
const DAY: u64 = 24 * 60 * 60;

/// How long a fixture index lives; below the verifier's lifetime cap.
const INDEX_LIFETIME: u64 = 7 * DAY;

/// How far before `now` a fixture key record opens.
const KEY_BACK_DATE: u64 = 1_000;

/// How long a fixture key record stays valid; below the verifier's cap.
const KEY_WINDOW: u64 = 100 * DAY;

/// Start of the endorser root's window: the first second after the epoch.
const ROOT_START: u64 = 1;

/// End of the endorser root's window: 2100-01-01.
const ROOT_END: u64 = 4_102_444_800;

/// A core range every released version satisfies.
const ANY_CORE: &str = ">=0.0.0";

/// Package id and runtime id of the default indexed fixture package.
const INDEXED_PACKAGE: &str = "pohunek.runtime.idxfix";
const INDEXED_RUNTIME: &str = "idxfix";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_secs()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String");
        text
    })
}

fn key_id(key: &SigningKey) -> KeyId {
    KeyId::derive(&key.verifying_key())
}

/// The endorser root R, the index key K it endorses and an outsider.
struct Keys {
    root: SigningKey,
    key: SigningKey,
    outsider: SigningKey,
}

impl Keys {
    fn new() -> Self {
        Self {
            root: signing_key("index-root"),
            key: signing_key("index-key"),
            outsider: signing_key("index-outsider"),
        }
    }

    fn root_key(&self) -> RootKey {
        RootKey::new(self.root.verifying_key().to_bytes(), ROOT_START, ROOT_END)
            .expect("valid root")
    }

    /// A host anchor holding R whose channel names R.
    fn trusted_anchor(&self, revoked: Vec<KeyId>) -> HostTrustAnchor {
        anchor(&[&self.root], revoked).with_index(IndexTrust::Loaded(Box::new(self.root_key())))
    }

    /// A fixture whose host trusts R for catalogs and plugin indexes.
    fn fixture(&self, tag: &str) -> Fixture {
        Fixture::with_anchor(tag, self.trusted_anchor(Vec::new()))
    }

    /// The key record of `subject`, endorsed by R.
    fn record(&self, subject: &SigningKey) -> IndexKeyRecord {
        let start = now() - KEY_BACK_DATE;
        let mut record = IndexKeyRecord {
            key_id: key_id(subject),
            public_key: hex(subject.verifying_key().as_bytes()),
            scope: "official".to_owned(),
            not_before: start,
            not_after: start + KEY_WINDOW,
            endorsed_by: key_id(&self.root),
            signature: String::new(),
        };
        record.signature = hex(&self
            .root
            .sign(&index_key_record_signing_message(&record))
            .to_bytes());
        record
    }

    fn revocations(&self, sequence: u64, revoked: Vec<KeyId>) -> SignedRevocations {
        let body = RevocationBody {
            schema_version: INDEX_SCHEMA_VERSION,
            sequence,
            revoked_key_ids: revoked,
        };
        let message = revocation_signing_message(&body).expect("revocation message");
        SignedRevocations {
            signature: CatalogSignature {
                key_id: key_id(&self.root),
                signature: hex(&self.root.sign(&message).to_bytes()),
            },
            revocations: body,
        }
    }

    /// An index document signed by `signer` that lists `entries` at
    /// `sequence`.
    fn document(
        &self,
        signer: &SigningKey,
        sequence: u64,
        entries: Vec<IndexEntry>,
        revocations: SignedRevocations,
    ) -> Vec<u8> {
        let body = IndexBody {
            schema_version: INDEX_SCHEMA_VERSION,
            scope: "official".to_owned(),
            sequence,
            expires_at: sequence + INDEX_LIFETIME,
            entries,
        };
        let message = index_signing_message(&body).expect("index message");
        let signature = CatalogSignature {
            key_id: key_id(signer),
            signature: hex(&signer.sign(&message).to_bytes()),
        };
        serde_json::to_vec(&IndexEnvelope {
            schema_version: INDEX_SCHEMA_VERSION,
            index: body,
            key_records: vec![self.record(&self.key)],
            signatures: vec![signature],
            revocations,
        })
        .expect("serialize the index")
    }

    /// A document signed by K at `sequence` with an empty revocation list at
    /// sequence 1.
    fn signed(&self, sequence: u64, entries: Vec<IndexEntry>) -> Vec<u8> {
        self.document(
            &self.key,
            sequence,
            entries,
            self.revocations(1, Vec::new()),
        )
    }
}

/// An index entry binding `package` to the given identity, supported on this
/// host and every core version.
fn index_entry(package: &Package, package_id: &str, runtime: &str, version: &str) -> IndexEntry {
    IndexEntry {
        package_id: PackageId::parse(package_id).expect("package id"),
        runtime_id: RuntimeId::parse(runtime).expect("runtime id"),
        version: PackageVersion::parse(version).expect("version"),
        digest: package.digest.clone(),
        url: "https://example.test/package.tar.zst".to_owned(),
        platforms: vec![host_platform()],
        core: ANY_CORE.to_owned(),
    }
}

fn idxfix(version: &str) -> Package {
    Package::build(INDEXED_PACKAGE, version, INDEXED_RUNTIME, INERT_PROGRAM)
}

fn idxfix_entry(package: &Package, version: &str) -> IndexEntry {
    index_entry(package, INDEXED_PACKAGE, INDEXED_RUNTIME, version)
}

fn index_install(
    fixture: &Fixture,
    package: &Package,
    document: &[u8],
    dry_run: bool,
) -> PackageInstallParams {
    PackageInstallParams {
        archive_path: fixture.write_archive(package),
        trust: PackageTrust::Index {
            index_path: fixture.write("index.json", document),
        },
        enable: true,
        select: true,
        dry_run,
        accept_commands: false,
    }
}

fn index_state(fixture: &Fixture) -> package::IndexVerifierState {
    Registry::open_at(&fixture.plugins, Limits::DEFAULT)
        .expect("registry")
        .index_state()
        .expect("index state")
}

async fn install_refused(
    fixture: &Fixture,
    package: &Package,
    document: &[u8],
) -> PackageErrorKind {
    fixture
        .registry
        .packages()
        .package_install(index_install(fixture, package, document, false))
        .await
        .expect_err("the install is refused")
}

/// Asserts that a refused install left neither a package nor index marks.
fn assert_nothing_recorded(fixture: &Fixture, package: &Package) {
    assert!(!fixture.root(&package.digest).exists());
    assert_eq!(index_state(fixture), package::IndexVerifierState::default());
    assert!(!fixture.plugins.join("index-state.json").exists());
}

#[tokio::test]
async fn a_signed_index_installs_an_official_package_and_persists_its_marks() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-official");
    let sequence = now();
    let first = idxfix("1.0.0");
    let second = idxfix("1.1.0");

    let installed = fixture
        .registry
        .packages()
        .package_install(index_install(
            &fixture,
            &first,
            &keys.signed(sequence, vec![idxfix_entry(&first, "1.0.0")]),
            false,
        ))
        .await
        .expect("indexed install");

    assert_eq!(installed.package.origin, PackageOrigin::Official);
    assert!(installed.package.enabled && installed.package.selected);
    assert_eq!(fixture.serving(INDEXED_RUNTIME), Some(first.digest.clone()));
    assert_eq!(index_state(&fixture).index_high_water, Some(sequence));

    let before = fixture.snapshot();
    let stale = keys.signed(sequence - 1, vec![idxfix_entry(&second, "1.1.0")]);
    assert_eq!(
        install_refused(&fixture, &second, &stale).await,
        PackageErrorKind::Untrusted
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "a refused install changes nothing"
    );
    assert_eq!(index_state(&fixture).index_high_water, Some(sequence));

    // An equal sequence re-verifies, so a second version listed by the same
    // document installs.
    let same = keys.signed(sequence, vec![idxfix_entry(&second, "1.1.0")]);
    fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &second, &same, false))
        .await
        .expect("an equal sequence is accepted");
}

#[tokio::test]
async fn an_index_dry_run_records_nothing() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-dry-run");
    let sequence = now();
    let package = idxfix("1.0.0");
    let document = keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]);

    let preview = fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &package, &document, true))
        .await
        .expect("preview");

    assert_eq!(preview.package.origin, PackageOrigin::Official);
    assert_nothing_recorded(&fixture, &package);
    assert!(fixture
        .registry
        .packages()
        .package_list()
        .await
        .expect("list")
        .packages
        .is_empty());
}

#[tokio::test]
async fn an_index_never_authorizes_a_release_runtime_a_command_package_or_unlisted_bytes() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-refused-packages");
    let sequence = now();

    for runtime in ["codex", "pi", "shell", "hermes", "claude"] {
        let package_id = format!("pohunek.runtime.{runtime}");
        let package = Package::build(&package_id, "1.0.0", runtime, INERT_PROGRAM);
        let document = keys.signed(
            sequence,
            vec![index_entry(&package, &package_id, runtime, "1.0.0")],
        );
        assert_eq!(
            install_refused(&fixture, &package, &document).await,
            PackageErrorKind::RuntimeNotClaimable,
            "{runtime}"
        );
        assert_nothing_recorded(&fixture, &package);
    }

    let with_commands = Package::build_with_commands(
        INDEXED_PACKAGE,
        "1.0.0",
        INDEXED_RUNTIME,
        INERT_PROGRAM,
        HOST_STATE_COMMAND,
    );
    let document = keys.signed(sequence, vec![idxfix_entry(&with_commands, "1.0.0")]);
    let mut params = index_install(&fixture, &with_commands, &document, false);
    params.accept_commands = true;
    assert_eq!(
        fixture
            .registry
            .packages()
            .package_install(params)
            .await
            .expect_err("commands are refused"),
        PackageErrorKind::Untrusted
    );
    assert_nothing_recorded(&fixture, &with_commands);

    let commands_only = Package::commands_only(INDEXED_PACKAGE, "1.0.0", HOST_STATE_COMMAND);
    let document = keys.signed(sequence, vec![idxfix_entry(&commands_only, "1.0.0")]);
    let mut params = index_install(&fixture, &commands_only, &document, false);
    params.accept_commands = true;
    assert_eq!(
        fixture
            .registry
            .packages()
            .package_install(params)
            .await
            .expect_err("a commands-only package is refused"),
        PackageErrorKind::Untrusted
    );
    assert_nothing_recorded(&fixture, &commands_only);

    // Bytes that are not an archive and whose digest the index does not list
    // are refused as untrusted: the listing check runs before decompression,
    // which would answer `ArchiveInvalid`.
    let listed = idxfix("1.0.0");
    let document = keys.signed(sequence, vec![idxfix_entry(&listed, "1.0.0")]);
    let junk = Package {
        bytes: (0..64_u8).map(|byte| byte.wrapping_mul(37)).collect(),
        digest: listed.digest.clone(),
    };
    let archive_path = fixture.write("junk.tar.zst", &junk.bytes);
    let params = PackageInstallParams {
        archive_path,
        trust: PackageTrust::Index {
            index_path: fixture.write("index.json", &document),
        },
        enable: true,
        select: true,
        dry_run: false,
        accept_commands: false,
    };
    assert_eq!(
        fixture
            .registry
            .packages()
            .package_install(params)
            .await
            .expect_err("unlisted bytes are refused"),
        PackageErrorKind::Untrusted
    );
    assert_nothing_recorded(&fixture, &listed);
}

#[tokio::test]
async fn an_entry_that_does_not_bind_the_package_is_refused() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-refused-bindings");
    let sequence = now();
    let package = idxfix("1.0.0");
    let host = host_platform();

    let wrong_version = idxfix_entry(&package, "9.9.9");
    let wrong_runtime = index_entry(&package, INDEXED_PACKAGE, "elsewhere", "1.0.0");
    let wrong_package = index_entry(&package, "pohunek.runtime.other", INDEXED_RUNTIME, "1.0.0");
    let outside_namespace = Package::build(
        "acme.runtime.idxfix",
        "1.0.0",
        INDEXED_RUNTIME,
        INERT_PROGRAM,
    );
    let unbound = Package::build(
        "pohunek.tool.idxfix",
        "1.0.0",
        INDEXED_RUNTIME,
        INERT_PROGRAM,
    );
    for (candidate, entry) in [
        (&package, wrong_version),
        (&package, wrong_runtime),
        (&package, wrong_package),
        (
            &outside_namespace,
            index_entry(
                &outside_namespace,
                "acme.runtime.idxfix",
                INDEXED_RUNTIME,
                "1.0.0",
            ),
        ),
        (
            &unbound,
            index_entry(&unbound, "pohunek.tool.idxfix", INDEXED_RUNTIME, "1.0.0"),
        ),
    ] {
        let document = keys.signed(sequence, vec![entry]);
        assert_eq!(
            install_refused(&fixture, candidate, &document).await,
            PackageErrorKind::Untrusted
        );
        assert_nothing_recorded(&fixture, candidate);
    }

    let mut excluded_core = idxfix_entry(&package, "1.0.0");
    excluded_core.core = ">=999.0.0".to_owned();
    let mut unlisted_platform = idxfix_entry(&package, "1.0.0");
    unlisted_platform.platforms = vec![format!("not-{host}")];
    let mut unusable_core = idxfix_entry(&package, "1.0.0");
    unusable_core.core = "not a range".to_owned();
    for entry in [excluded_core, unlisted_platform, unusable_core] {
        let document = keys.signed(sequence, vec![entry]);
        assert_eq!(
            install_refused(&fixture, &package, &document).await,
            PackageErrorKind::Incompatible
        );
        assert_nothing_recorded(&fixture, &package);
    }
}

#[tokio::test]
async fn documents_that_do_not_verify_install_nothing() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-refused-documents");
    let sequence = now();
    let package = idxfix("1.0.0");
    let entries = || vec![idxfix_entry(&package, "1.0.0")];

    let tampered = {
        let mut value: serde_json::Value =
            serde_json::from_slice(&keys.signed(sequence, entries())).expect("json");
        value["index"]["sequence"] = serde_json::Value::from(sequence + 1);
        serde_json::to_vec(&value).expect("serialize")
    };
    let expired = {
        let body_sequence = now() - 30 * DAY;
        keys.signed(body_sequence, entries())
    };
    // A catalog root is not an index key: its signature counts only when a
    // key record names it.
    let signed_by_root = keys.document(
        &keys.root,
        sequence,
        entries(),
        keys.revocations(1, Vec::new()),
    );
    let signed_by_outsider = keys.document(
        &keys.outsider,
        sequence,
        entries(),
        keys.revocations(1, Vec::new()),
    );
    let future = keys.signed(now() + 10 * DAY, entries());
    let revokes_signer = keys.document(
        &keys.key,
        sequence,
        entries(),
        keys.revocations(1, vec![key_id(&keys.key)]),
    );
    for (name, document) in [
        ("tampered", tampered),
        ("expired", expired),
        ("signed by the root", signed_by_root),
        ("signed by an outsider", signed_by_outsider),
        ("sequence in the future", future),
        ("revokes its own signer", revokes_signer),
        ("not json", b"not an index".to_vec()),
        (
            "a catalog document",
            signed_by(
                &keys.root,
                catalog(
                    5,
                    vec![entry(&package, INDEXED_PACKAGE, INDEXED_RUNTIME, "1.0.0")],
                ),
            ),
        ),
    ] {
        assert_eq!(
            install_refused(&fixture, &package, &document).await,
            PackageErrorKind::Untrusted,
            "{name}"
        );
        assert_nothing_recorded(&fixture, &package);
    }
}

#[tokio::test]
async fn an_unreadable_index_file_is_a_source_error() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-unreadable");
    let package = idxfix("1.0.0");
    let missing = PackageInstallParams {
        archive_path: fixture.write_archive(&package),
        trust: PackageTrust::Index {
            index_path: fixture
                .dir
                .join("absent.json")
                .to_string_lossy()
                .into_owned(),
        },
        enable: true,
        select: true,
        dry_run: false,
        accept_commands: false,
    };
    assert_eq!(
        fixture
            .registry
            .packages()
            .package_install(missing)
            .await
            .expect_err("a missing index"),
        PackageErrorKind::SourceUnreadable
    );

    let oversized = vec![b' '; package::MAX_INDEX_BYTES + 1];
    let params = index_install(&fixture, &package, &oversized, false);
    assert_eq!(
        fixture
            .registry
            .packages()
            .package_install(params)
            .await
            .expect_err("an oversized index"),
        PackageErrorKind::SourceUnreadable
    );
    assert_nothing_recorded(&fixture, &package);
}

#[tokio::test]
async fn revoked_keys_and_replayed_revocation_lists_are_refused() {
    let keys = Keys::new();
    let package = idxfix("1.0.0");
    let entries = || vec![idxfix_entry(&package, "1.0.0")];
    let sequence = now();

    // Revoked by the anchor.
    let by_anchor = Fixture::with_anchor(
        "index-revoked-anchor",
        keys.trusted_anchor(vec![key_id(&keys.key)]),
    );
    assert_eq!(
        install_refused(&by_anchor, &package, &keys.signed(sequence, entries())).await,
        PackageErrorKind::Untrusted
    );
    assert_nothing_recorded(&by_anchor, &package);

    // Revoked by a recorded catalog.
    let by_catalog = keys.fixture("index-revoked-catalog");
    let catalog_package = Package::pi();
    let mut revoking = catalog(7, vec![entry(&catalog_package, PACKAGE, RUNTIME, "1.0.0")]);
    revoking.revoked_key_ids = vec![key_id(&keys.key)];
    let document = signed_by(&keys.root, revoking);
    by_catalog
        .registry
        .packages()
        .package_install(catalog_install(
            &by_catalog,
            &catalog_package,
            &document,
            false,
        ))
        .await
        .expect("the catalog install records its revocations");
    assert_eq!(
        install_refused(&by_catalog, &package, &keys.signed(sequence, entries())).await,
        PackageErrorKind::Untrusted
    );

    // An index records the key ids its revocation list revokes, and a later
    // document with an older revocation list is refused.
    let by_index = keys.fixture("index-revoked-index");
    let other = idxfix("2.0.0");
    let second_key = signing_key("index-key-2");
    let mut rotated = keys.document(
        &keys.key,
        sequence,
        vec![idxfix_entry(&other, "2.0.0")],
        keys.revocations(5, vec![key_id(&second_key)]),
    );
    by_index
        .registry
        .packages()
        .package_install(index_install(&by_index, &other, &rotated, false))
        .await
        .expect("the index records its revocation list");
    assert_eq!(index_state(&by_index).revocation_high_water, Some(5));
    assert!(index_state(&by_index)
        .revoked_key_ids
        .contains(&key_id(&second_key)));
    rotated = keys.document(
        &keys.key,
        sequence + 1,
        entries(),
        keys.revocations(4, Vec::new()),
    );
    assert_eq!(
        install_refused(&by_index, &package, &rotated).await,
        PackageErrorKind::Untrusted
    );
    assert!(!by_index.root(&package.digest).exists());
}

#[tokio::test]
async fn marks_persisted_before_a_crash_gate_the_retry_and_damaged_state_is_kept() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-crash-points");
    let sequence = now();
    let package = idxfix("1.0.0");
    let document = keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]);

    // State as a crash between `record_index` and the registry install leaves
    // it: marks raised, the package absent, and a stale temporary file.
    let registry = Registry::open_at(&fixture.plugins, Limits::DEFAULT).expect("registry");
    let verified = package::verify_index(
        &document,
        &keys.root_key(),
        &std::collections::BTreeSet::new(),
        &package::IndexVerifierState::default(),
        now(),
    )
    .expect("the fixture index verifies");
    registry.record_index(&verified).expect("record the marks");
    let temporary = fixture.plugins.join("index-state.json.tmp");
    std::fs::write(&temporary, b"partial").expect("write a stale temporary");

    assert_eq!(
        install_refused(
            &fixture,
            &package,
            &keys.signed(sequence - 1, vec![idxfix_entry(&package, "1.0.0")])
        )
        .await,
        PackageErrorKind::Untrusted,
        "a document below the persisted mark is refused"
    );
    fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &package, &document, false))
        .await
        .expect("the same document installs after the crash");
    assert_eq!(index_state(&fixture).index_high_water, Some(sequence));

    // A damaged state file is reported and never replaced.
    let damaged = keys.fixture("index-damaged-state");
    std::fs::write(damaged.plugins.join("index-state.json"), b"{ damaged")
        .expect("write a damaged state");
    let before = std::fs::read(damaged.plugins.join("index-state.json")).expect("read");
    assert_eq!(
        install_refused(&damaged, &package, &document).await,
        PackageErrorKind::RegistryFailed
    );
    assert_eq!(
        std::fs::read(damaged.plugins.join("index-state.json")).expect("read"),
        before
    );
    assert!(!damaged.root(&package.digest).exists());
}

#[tokio::test]
async fn a_failed_registry_install_leaves_the_marks_raised_and_the_package_absent() {
    use std::os::unix::fs::PermissionsExt as _;

    let keys = Keys::new();
    let fixture = keys.fixture("index-install-fails");
    let sequence = now();
    let package = idxfix("1.0.0");
    let document = keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]);

    // A package store that cannot be written fails the registry install after
    // the index state was recorded.
    let store = fixture.plugins.join("packages");
    let writable = std::fs::metadata(&store).expect("store").permissions();
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o500))
        .expect("make the package store read-only");
    let failed = fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &package, &document, false))
        .await;
    std::fs::set_permissions(&store, writable).expect("restore the package store");

    failed.expect_err("the registry install fails");
    assert_eq!(index_state(&fixture).index_high_water, Some(sequence));
    assert!(!fixture.root(&package.digest).exists());

    fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &package, &document, false))
        .await
        .expect("the same document installs on the next attempt");
    assert_eq!(fixture.serving(INDEXED_RUNTIME), Some(package.digest));
}

#[tokio::test]
async fn a_host_without_a_channel_refuses_index_installs_and_keeps_catalog_installs() {
    let keys = Keys::new();
    let package = idxfix("1.0.0");
    let document = keys.signed(now(), vec![idxfix_entry(&package, "1.0.0")]);
    let catalog_package = Package::pi();
    let catalog_document = signed_by(
        &keys.root,
        catalog(5, vec![entry(&catalog_package, PACKAGE, RUNTIME, "1.0.0")]),
    );

    for (tag, trust, expected, supported) in [
        (
            "index-no-channel",
            CatalogTrust::Loaded(anchor(&[&keys.root], Vec::new())),
            PackageErrorKind::TrustUnavailable,
            false,
        ),
        (
            "index-invalid-channel",
            CatalogTrust::Loaded(
                anchor(&[&keys.root], Vec::new())
                    .with_index(IndexTrust::Invalid(IndexChannelFault::Unsafe)),
            ),
            PackageErrorKind::TrustAnchorInvalid,
            false,
        ),
        (
            "index-no-anchor",
            CatalogTrust::Absent,
            PackageErrorKind::TrustUnavailable,
            false,
        ),
        (
            "index-invalid-anchor",
            CatalogTrust::Invalid(crate::catalog_anchor::AnchorFault::UnsafeFile),
            PackageErrorKind::TrustAnchorInvalid,
            false,
        ),
        (
            "index-with-channel",
            CatalogTrust::Loaded(keys.trusted_anchor(Vec::new())),
            PackageErrorKind::Untrusted,
            true,
        ),
    ] {
        let fixture = Fixture::with_trust(tag, trust);
        assert_eq!(
            fixture
                .registry
                .package_authority()
                .index_install_supported(),
            supported,
            "{tag}"
        );
        if !supported {
            assert_eq!(
                install_refused(&fixture, &package, &document).await,
                expected,
                "{tag}"
            );
            assert_nothing_recorded(&fixture, &package);
        }
        let catalog_install_result = fixture
            .registry
            .packages()
            .package_install(catalog_install(
                &fixture,
                &catalog_package,
                &catalog_document,
                false,
            ))
            .await;
        match tag {
            "index-no-anchor" | "index-invalid-anchor" => {
                assert!(catalog_install_result.is_err(), "{tag}");
            }
            _ => {
                let installed = catalog_install_result.expect("catalog installs are unaffected");
                assert_eq!(installed.package.origin, PackageOrigin::Official, "{tag}");
            }
        }
    }
}
