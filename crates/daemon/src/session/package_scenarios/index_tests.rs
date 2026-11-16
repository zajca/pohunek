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
    ArchiveEntry, CatalogSignature, IndexBody, IndexEntry, IndexEntryKind, IndexEnvelope,
    IndexKeyRecord, KeyId, Limits, RevocationBody, RootKey, SignedRevocations,
    INDEX_SCHEMA_VERSION, INDEX_SCHEMA_VERSION_2,
};
use pohunek_test_support::catalog::host_platform;
use protocol::{
    PackageErrorKind, PackageId, PackageInstallParams, PackageOrigin, PackageTrust, PackageVersion,
    RuntimeId,
};

use super::catalog_tests::{anchor, catalog, catalog_install, entry, signed_by, signing_key};
use std::os::unix::fs::PermissionsExt as _;

use super::test_fixture::{Fixture, Package, HOST_STATE_COMMAND, INERT_PROGRAM, PACKAGE, RUNTIME};
use crate::agent::host::{ContextError, PackageRejection};
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
        Self::record_by(&self.root, subject)
    }

    /// The key record of `subject`, endorsed by `endorser`.
    fn record_by(endorser: &SigningKey, subject: &SigningKey) -> IndexKeyRecord {
        let start = now() - KEY_BACK_DATE;
        let mut record = IndexKeyRecord {
            key_id: key_id(subject),
            public_key: hex(subject.verifying_key().as_bytes()),
            scope: "official".to_owned(),
            not_before: start,
            not_after: start + KEY_WINDOW,
            endorsed_by: key_id(endorser),
            signature: String::new(),
        };
        record.signature = hex(&endorser
            .sign(&index_key_record_signing_message(&record))
            .to_bytes());
        record
    }

    fn revocations(&self, sequence: u64, revoked: Vec<KeyId>) -> SignedRevocations {
        Self::revocations_by(&self.root, sequence, revoked)
    }

    /// A revocation list signed by `endorser`.
    fn revocations_by(
        endorser: &SigningKey,
        sequence: u64,
        revoked: Vec<KeyId>,
    ) -> SignedRevocations {
        let body = RevocationBody {
            schema_version: INDEX_SCHEMA_VERSION,
            sequence,
            revoked_key_ids: revoked,
        };
        let message = revocation_signing_message(&body).expect("revocation message");
        SignedRevocations {
            signature: CatalogSignature {
                key_id: key_id(endorser),
                signature: hex(&endorser.sign(&message).to_bytes()),
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
        Self::document_with_records(
            signer,
            vec![self.record(&self.key)],
            sequence,
            entries,
            revocations,
        )
    }

    /// An index document signed by `signer` that carries `key_records`.
    fn document_with_records(
        signer: &SigningKey,
        key_records: Vec<IndexKeyRecord>,
        sequence: u64,
        entries: Vec<IndexEntry>,
        revocations: SignedRevocations,
    ) -> Vec<u8> {
        Self::document_of_schema(
            INDEX_SCHEMA_VERSION,
            signer,
            key_records,
            sequence,
            entries,
            revocations,
        )
    }

    /// An index document of `schema_version` signed by `signer`.
    fn document_of_schema(
        schema_version: u64,
        signer: &SigningKey,
        key_records: Vec<IndexKeyRecord>,
        sequence: u64,
        entries: Vec<IndexEntry>,
        revocations: SignedRevocations,
    ) -> Vec<u8> {
        let body = IndexBody {
            schema_version,
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
            schema_version,
            index: body,
            key_records,
            signatures: vec![signature],
            revocations,
        })
        .expect("serialize the index")
    }

    /// A schema 2 document signed by K at `sequence` with an empty revocation
    /// list at sequence 1.
    fn signed_v2(&self, sequence: u64, entries: Vec<IndexEntry>) -> Vec<u8> {
        Self::document_of_schema(
            INDEX_SCHEMA_VERSION_2,
            &self.key,
            vec![self.record(&self.key)],
            sequence,
            entries,
            self.revocations(1, Vec::new()),
        )
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
        kind: None,
        runtime_id: Some(RuntimeId::parse(runtime).expect("runtime id")),
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
        PackageErrorKind::IndexRollback
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
        PackageErrorKind::IndexRollback
    );
    assert!(!by_index.root(&package.digest).exists());

    // A document signed by the key an earlier index revoked is refused even
    // though its key record verifies and its own list revokes nothing; the
    // same document installs on a host that never saw the revocation.
    let signed_by_revoked = Keys::document_with_records(
        &second_key,
        vec![keys.record(&second_key)],
        sequence + 2,
        entries(),
        keys.revocations(6, Vec::new()),
    );
    assert_eq!(
        install_refused(&by_index, &package, &signed_by_revoked).await,
        PackageErrorKind::Untrusted
    );
    assert!(!by_index.root(&package.digest).exists());
    let control = keys.fixture("index-revoked-control");
    control
        .registry
        .packages()
        .package_install(index_install(&control, &package, &signed_by_revoked, false))
        .await
        .expect("the same document installs where its signer was never revoked");
}

/// An index entry whose digest an earlier recorded catalog revoked is refused
/// as revoked, and the refusal records neither the package nor index marks
/// (case table row 6 of #2099).
#[tokio::test]
async fn an_index_entry_for_a_digest_a_recorded_catalog_revoked_is_refused() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-relisted-revoked");
    let package = idxfix("1.0.0");
    let catalog_package = Package::pi();
    let mut revoking = catalog(7, vec![entry(&catalog_package, PACKAGE, RUNTIME, "1.0.0")]);
    revoking.revoked_digests = vec![package.digest.clone()];
    let document = signed_by(&keys.root, revoking);
    fixture
        .registry
        .packages()
        .package_install(catalog_install(
            &fixture,
            &catalog_package,
            &document,
            false,
        ))
        .await
        .expect("the catalog install records its revoked digests");

    let index = keys.signed(now(), vec![idxfix_entry(&package, "1.0.0")]);
    assert_eq!(
        install_refused(&fixture, &package, &index).await,
        PackageErrorKind::Revoked
    );
    assert_nothing_recorded(&fixture, &package);
}

/// A context entry for a digest a recorded catalog revoked is refused like a
/// runtime entry: revocation is checked on the context candidate too.
#[tokio::test]
async fn a_context_entry_for_a_revoked_digest_is_refused() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-context-revoked");
    let package = context_package();
    let context_entry = context_entry(&package);

    let allowed = keys.signed_v2(now(), vec![context_entry.clone()]);
    fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &package, &allowed, true))
        .await
        .expect("an unrevoked context entry passes its dry run");

    let catalog_package = Package::pi();
    let mut revoking = catalog(7, vec![entry(&catalog_package, PACKAGE, RUNTIME, "1.0.0")]);
    revoking.revoked_digests = vec![package.digest.clone()];
    let document = signed_by(&keys.root, revoking);
    fixture
        .registry
        .packages()
        .package_install(catalog_install(
            &fixture,
            &catalog_package,
            &document,
            false,
        ))
        .await
        .expect("the catalog install records its revoked digests");

    let index = keys.signed_v2(now(), vec![context_entry]);
    assert_eq!(
        install_refused(&fixture, &package, &index).await,
        PackageErrorKind::Revoked
    );
    assert_nothing_recorded(&fixture, &package);
}

/// Package id of the context fixture package.
const CONTEXT_PACKAGE: &str = "pohunek.context.idxctx";

/// A context package archive: a descriptor that exports one directory.
fn context_package() -> Package {
    Package::from_entries(&[
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: format!(
                "schema = 1\nid = \"{CONTEXT_PACKAGE}\"\nversion = \"1.0.0\"\nruntime_api = 1\nrequires = [\"package-context-v1\"]\n\n[context]\ndir = \"bundle\"\n"
            )
            .into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "bundle/index.md".to_owned(),
            contents: b"# Index\n".to_vec(),
            executable: false,
        },
    ])
}

/// The `context` index entry that lists `package`.
fn context_entry(package: &Package) -> IndexEntry {
    IndexEntry {
        package_id: PackageId::parse(CONTEXT_PACKAGE).expect("package id"),
        kind: Some(IndexEntryKind::Context),
        runtime_id: None,
        version: PackageVersion::parse("1.0.0").expect("version"),
        digest: package.digest.clone(),
        url: "https://example.test/package.tar.zst".to_owned(),
        platforms: vec![host_platform()],
        core: ANY_CORE.to_owned(),
    }
}

/// An installed context package that a recorded catalog later revokes is not
/// served: the lookup by selection and the lookup by digest both refuse it.
#[tokio::test]
async fn a_revoked_context_package_is_not_served() {
    let keys = Keys::new();
    let fixture = keys.fixture("index-context-served-revoked");
    let package = context_package();
    let id = PackageId::parse(CONTEXT_PACKAGE).expect("package id");
    let index = keys.signed_v2(now(), vec![context_entry(&package)]);
    fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &package, &index, false))
        .await
        .expect("the index installs the context package");
    fixture
        .registry
        .packages()
        .context_dir(id.clone())
        .await
        .expect("an unrevoked context package is served");

    let catalog_package = Package::pi();
    let mut revoking = catalog(7, vec![entry(&catalog_package, PACKAGE, RUNTIME, "1.0.0")]);
    revoking.revoked_digests = vec![package.digest.clone()];
    fixture
        .registry
        .packages()
        .package_install(catalog_install(
            &fixture,
            &catalog_package,
            &signed_by(&keys.root, revoking),
            false,
        ))
        .await
        .expect("the catalog install records its revoked digests");

    let selected = fixture.registry.packages().context_dir(id.clone()).await;
    assert!(
        matches!(
            selected,
            Err(ContextError::Rejected(PackageRejection::Revoked))
        ),
        "{selected:?}"
    );
    let pinned = fixture
        .registry
        .packages()
        .context_dir_at(id, package.digest.clone())
        .await;
    assert!(
        matches!(
            pinned,
            Err(ContextError::Rejected(PackageRejection::Revoked))
        ),
        "{pinned:?}"
    );

    let refusal =
        crate::assistant::context_error(&ContextError::Rejected(PackageRejection::Revoked));
    let remedy = refusal.recover.expect("the refusal carries a remedy");
    assert!(remedy.contains("pohunek setup"), "{remedy}");
    assert!(!remedy.contains("--sha256"), "{remedy}");
}

#[tokio::test]
async fn only_the_channel_designated_root_endorses_index_keys() {
    // Both roots of the anchor sign catalogs. The channel designates one of
    // them to endorse index keys: a document endorsed by the other, the CI
    // root, is refused, and the designated root is the anchor's first root in
    // one round and its second in the other.
    let first = signing_key("index-root-a");
    let second = signing_key("index-root-b");
    for (round, (designated, ci_root)) in [(&first, &second), (&second, &first)]
        .into_iter()
        .enumerate()
    {
        let keys = Keys {
            root: designated.clone(),
            key: signing_key("index-key"),
            outsider: signing_key("index-outsider"),
        };
        let fixture = Fixture::with_anchor(
            &format!("index-two-roots-{round}"),
            anchor(&[&first, &second], Vec::new())
                .with_index(IndexTrust::Loaded(Box::new(keys.root_key()))),
        );
        let package = idxfix("1.0.0");
        let sequence = now();
        let entries = vec![idxfix_entry(&package, "1.0.0")];

        let endorsed_by_ci_root = Keys::document_with_records(
            &keys.key,
            vec![Keys::record_by(ci_root, &keys.key)],
            sequence,
            entries.clone(),
            keys.revocations(1, Vec::new()),
        );
        assert_eq!(
            install_refused(&fixture, &package, &endorsed_by_ci_root).await,
            PackageErrorKind::Untrusted
        );
        assert_nothing_recorded(&fixture, &package);

        fixture
            .registry
            .packages()
            .package_install(index_install(
                &fixture,
                &package,
                &keys.signed(sequence, entries),
                false,
            ))
            .await
            .expect("the designated root's endorsement installs");
    }
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
        PackageErrorKind::IndexRollback,
        "a document below the persisted mark is refused"
    );
    fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &package, &document, false))
        .await
        .expect("the same document installs after the crash");
    assert_eq!(index_state(&fixture).index_high_water, Some(sequence));
    assert!(
        !temporary.exists(),
        "the stale temporary state file is gone after the retry"
    );

    // A damaged state file is reported and never replaced.
    let damaged = keys.fixture("index-damaged-state");
    std::fs::write(damaged.plugins.join("index-state.json"), b"{ damaged")
        .expect("write a damaged state");
    std::fs::set_permissions(
        damaged.plugins.join("index-state.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("owner-private");
    let before = std::fs::read(damaged.plugins.join("index-state.json")).expect("read");
    assert_eq!(
        install_refused(&damaged, &package, &document).await,
        PackageErrorKind::IndexStateUnreadable
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

#[tokio::test]
async fn scenario_1_apply_stores_and_registers_reserved() {
    let keys = Keys::new();
    let fixture = keys.fixture("scenario-1");
    let sequence = now();
    let package = idxfix("1.0.0");
    let path = fixture.write(
        "index.json",
        &keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]),
    );
    let result = fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: path,
            repair_state: false,
        })
        .await
        .expect("apply");
    assert_eq!(result.sequence, sequence);
    assert_eq!(result.entries, 1);
    assert_eq!(index_state(&fixture).index_high_water, Some(sequence));
}

#[tokio::test]
async fn scenario_2_rollback_leaves_stored_bytes_identical() {
    let keys = Keys::new();
    let fixture = keys.fixture("scenario-2");
    let sequence = now();
    let package = idxfix("1.0.0");
    let doc = keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]);
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write("a.json", &doc),
            repair_state: false,
        })
        .await
        .expect("apply");
    let registry = Registry::open_at(&fixture.plugins, Limits::DEFAULT).expect("registry");
    let before = registry
        .stored_index(1)
        .expect("stored")
        .expect("present")
        .bytes;
    let low = keys.signed(sequence - 5, vec![idxfix_entry(&package, "1.0.0")]);
    let err = fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write("b.json", &low),
            repair_state: false,
        })
        .await
        .expect_err("rollback");
    assert_eq!(err, PackageErrorKind::IndexRollback);
    let after = Registry::open_at(&fixture.plugins, Limits::DEFAULT)
        .expect("registry")
        .stored_index(1)
        .expect("stored")
        .expect("present")
        .bytes;
    assert_eq!(before, after);
}

#[tokio::test]
async fn scenario_5_index_origin_provenance_not_catalog() {
    let keys = Keys::new();
    let fixture = keys.fixture("scenario-5");
    let sequence = now();
    let v1 = idxfix("1.0.0");
    let doc = keys.signed(sequence, vec![idxfix_entry(&v1, "1.0.0")]);
    fixture
        .registry
        .packages()
        .package_install(index_install(&fixture, &v1, &doc, false))
        .await
        .expect("install");
    let registry = Registry::open_at(&fixture.plugins, Limits::DEFAULT).expect("registry");
    let state = registry.state().expect("state");
    let rec = state
        .packages()
        .iter()
        .find(|r| r.digest() == &v1.digest)
        .expect("record");
    assert_eq!(
        rec.provenance(),
        package::registry::PackageProvenance::Index
    );
}

#[tokio::test]
async fn scenario_6_guard_blocks_renamed_ids() {
    let keys = Keys::new();
    let fixture = keys.fixture("scenario-6");
    let sequence = now();
    let listed = Package::build("pohunek.runtime.aurora", "1.0.0", "aurora", INERT_PROGRAM);
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write(
                "i.json",
                &keys.signed(
                    sequence,
                    vec![index_entry(
                        &listed,
                        "pohunek.runtime.aurora",
                        "aurora",
                        "1.0.0",
                    )],
                ),
            ),
            repair_state: false,
        })
        .await
        .expect("apply");
    for id in ["community.aurora", "pohunek.runtime.foo"] {
        let pkg = Package::build(id, "1.0.0", "aurora", INERT_PROGRAM);
        let err = fixture
            .registry
            .packages()
            .package_install(PackageInstallParams {
                archive_path: fixture.write_archive(&pkg),
                trust: PackageTrust::ExplicitDigest {
                    digest: pkg.digest.clone(),
                },
                enable: true,
                select: true,
                dry_run: false,
                accept_commands: false,
            })
            .await
            .expect_err("reserved runtime must be refused");
        assert_eq!(err, PackageErrorKind::RuntimeNotClaimable, "{id}");
    }
}

#[tokio::test]
async fn scenario_7_damaged_sidecar_is_replaced_by_refresh() {
    let keys = Keys::new();
    let fixture = keys.fixture("scenario-7");
    let seq = now();
    let pkg = idxfix("1.0.0");
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write(
                "a.json",
                &keys.signed(seq, vec![idxfix_entry(&pkg, "1.0.0")]),
            ),
            repair_state: false,
        })
        .await
        .expect("apply");
    std::fs::write(
        fixture.plugins.join("index").join("stored.json"),
        b"corrupt",
    )
    .expect("corrupt");
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write(
                "b.json",
                &keys.signed(seq, vec![idxfix_entry(&pkg, "1.0.0")]),
            ),
            repair_state: false,
        })
        .await
        .expect("repair");
    let stored = Registry::open_at(&fixture.plugins, Limits::DEFAULT)
        .expect("registry")
        .stored_index(1)
        .expect("stored")
        .expect("present");
    assert!(stored.stored_at > 0);
}

#[tokio::test]
async fn scenario_7_expired_document_refused_at_apply() {
    let keys = Keys::new();
    let fixture = keys.fixture("scenario-7-expired");
    let past = now().saturating_sub(30 * DAY);
    let pkg = idxfix("1.0.0");
    let expired = keys.signed(past, vec![idxfix_entry(&pkg, "1.0.0")]);
    let err = fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write("old.json", &expired),
            repair_state: false,
        })
        .await
        .expect_err("expired");
    assert!(matches!(err, PackageErrorKind::IndexExpired), "{err:?}");
}

#[tokio::test]
async fn scenario_8_catalog_install_with_stored_index_present() {
    let keys = Keys::new();
    let fixture = keys.fixture("scenario-8");
    let seq = now();
    let pkg = idxfix("1.0.0");
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write(
                "i.json",
                &keys.signed(seq, vec![idxfix_entry(&pkg, "1.0.0")]),
            ),
            repair_state: false,
        })
        .await
        .expect("apply");
    let catalog_package = Package::pi();
    let catalog_document = signed_by(
        &keys.root,
        catalog(5, vec![entry(&catalog_package, PACKAGE, RUNTIME, "1.0.0")]),
    );
    let installed = fixture
        .registry
        .packages()
        .package_install(catalog_install(
            &fixture,
            &catalog_package,
            &catalog_document,
            false,
        ))
        .await
        .expect("catalog");
    assert_eq!(installed.package.origin, PackageOrigin::Official);
}

#[tokio::test]
async fn plugin_index_apply_stores_and_accumulates_reserved_ids() {
    let keys = Keys::new();
    let fixture = keys.fixture("apply-stores");
    let sequence = now();
    let first = idxfix("1.0.0");
    let document = keys.signed(sequence, vec![idxfix_entry(&first, "1.0.0")]);
    let path = fixture.write("index.json", &document);
    let result = fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: path,
            repair_state: false,
        })
        .await
        .expect("apply");
    assert_eq!(result.sequence, sequence);
    assert_eq!(result.schema_version, 1);
    assert_eq!(result.entries, 1);
    // Rollback refused and file unchanged
    let low = keys.signed(sequence - 10, vec![idxfix_entry(&first, "1.0.0")]);
    let path2 = fixture.write("index2.json", &low);
    let err = fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: path2,
            repair_state: false,
        })
        .await
        .expect_err("rollback");
    assert_eq!(err, PackageErrorKind::IndexRollback);
}

#[tokio::test]
async fn namespace_guard_data_driven_both_sides() {
    let keys = Keys::new();
    let fixture = keys.fixture("namespace-guard");
    let local = Package::build("pohunek.runtime.aurora", "1.0.0", "aurora", INERT_PROGRAM);
    let digest = local.digest.clone();
    // Before any stored index: --sha256 works
    let installed = fixture
        .registry
        .packages()
        .package_install(PackageInstallParams {
            archive_path: fixture.write_archive(&local),
            trust: PackageTrust::ExplicitDigest {
                digest: digest.clone(),
            },
            enable: true,
            select: true,
            dry_run: false,
            accept_commands: false,
        })
        .await
        .expect("before admission");
    assert_eq!(installed.package.origin, PackageOrigin::ExplicitDigest);
    // After stored index listing aurora, local reinstall is refused
    let sequence = now();
    let doc = keys.signed(
        sequence,
        vec![index_entry(
            &local,
            "pohunek.runtime.aurora",
            "aurora",
            "1.0.0",
        )],
    );
    let path = fixture.write("index.json", &doc);
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: path,
            repair_state: false,
        })
        .await
        .expect("apply");
    let err = fixture
        .registry
        .packages()
        .package_install(PackageInstallParams {
            archive_path: fixture.write_archive(&local),
            trust: PackageTrust::ExplicitDigest {
                digest: digest.clone(),
            },
            enable: true,
            select: true,
            dry_run: false,
            accept_commands: false,
        })
        .await
        .expect_err("after admission");
    assert_eq!(err, PackageErrorKind::RuntimeNotClaimable);
}

/// M4: a damaged `index-state.json` refuses a local install fail-closed
/// with the specific `package_index_state_unreadable` code, never
/// `package_registry_failed` and never silently through.
#[tokio::test]
async fn scenario_m4_damaged_index_state_is_package_index_state_unreadable() {
    let keys = Keys::new();
    let fixture = keys.fixture("m4-damaged-state");
    // Install a local package so the registry exists.
    let local = Package::build("community.aurora", "1.0.0", "aurora", INERT_PROGRAM);
    let digest = local.digest.clone();
    fixture
        .registry
        .packages()
        .package_install(PackageInstallParams {
            archive_path: fixture.write_archive(&local),
            trust: PackageTrust::ExplicitDigest {
                digest: digest.clone(),
            },
            enable: true,
            select: true,
            dry_run: false,
            accept_commands: false,
        })
        .await
        .expect("local install");
    // Corrupt the index state file after the first successful install.
    let state_path = fixture.plugins.join("index-state.json");
    std::fs::write(&state_path, b"{not json").expect("corrupt state");
    std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o600))
        .expect("owner-private");
    let second = Package::build("community.aurora", "1.1.0", "aurora", INERT_PROGRAM);
    let err = fixture
        .registry
        .packages()
        .package_install(PackageInstallParams {
            archive_path: fixture.write_archive(&second),
            trust: PackageTrust::ExplicitDigest {
                digest: second.digest.clone(),
            },
            enable: true,
            select: true,
            dry_run: false,
            accept_commands: false,
        })
        .await
        .expect_err("fail-closed");
    assert_eq!(
        err,
        PackageErrorKind::IndexStateUnreadable,
        "daemon refuses with the specific code"
    );
}

/// Doctor: a damaged `index-state.json` fails `plugin_index` with
/// `package_index_state_unreadable` and names `--repair-state`.
#[tokio::test]
async fn scenario_m5_doctor_damaged_state_is_index_state_unreadable() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-doctor-state-unreadable");
    // Seed a stored copy via apply.
    let pkg = idxfix("1.0.0");
    fixture
        .registry
        .packages()
        .package_install(index_install(
            &fixture,
            &pkg,
            &keys.signed(now(), vec![idxfix_entry(&pkg, "1.0.0")]),
            false,
        ))
        .await
        .expect("index install");
    let state_path = fixture.plugins.join("index-state.json");
    std::fs::write(&state_path, b"{not json").expect("corrupt state");
    std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    let checks = doctor_plugin_index(&fixture).await;
    let detail = checks
        .iter()
        .find(|c| c.name == "plugin_index")
        .map(|c| c.detail.clone())
        .unwrap_or_default();
    assert!(
        detail.starts_with("package_index_state_unreadable: "),
        "{detail}"
    );
    assert!(detail.contains("--repair-state"), "{detail}");
}

/// Installs `package` under the owner's exact-digest trust.
async fn install_local(fixture: &Fixture, package: &Package) -> Result<(), PackageErrorKind> {
    fixture
        .registry
        .packages()
        .package_install(PackageInstallParams {
            archive_path: fixture.write_archive(package),
            trust: PackageTrust::ExplicitDigest {
                digest: package.digest.clone(),
            },
            enable: true,
            select: true,
            dry_run: false,
            accept_commands: false,
        })
        .await
        .map(|_report| ())
}

/// `plugin.index.apply` of `document`, with or without `repair_state`.
async fn apply_document(
    fixture: &Fixture,
    document: &[u8],
    repair_state: bool,
) -> Result<protocol::PluginIndexApplyResult, PackageErrorKind> {
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write("apply.json", document),
            repair_state,
        })
        .await
}

/// Replaces `index-state.json` with `bytes` at `mode`.
fn plant_state(fixture: &Fixture, bytes: &[u8], mode: u32) {
    let path = fixture.plugins.join("index-state.json");
    std::fs::write(&path, bytes).expect("plant the state file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("state mode");
}

/// The preserved copies of a damaged state file: name and bytes.
fn damaged_copies(fixture: &Fixture) -> Vec<(String, Vec<u8>)> {
    let mut copies: Vec<_> = std::fs::read_dir(&fixture.plugins)
        .expect("read the plugin root")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            name.starts_with("index-state.json.damaged-").then(|| {
                let bytes = std::fs::read(entry.path()).expect("read a preserved copy");
                (name, bytes)
            })
        })
        .collect();
    copies.sort();
    copies
}

/// A document listing one runtime entry for `runtime`.
fn runtime_document(keys: &Keys, sequence: u64, runtime: &str) -> Vec<u8> {
    let listed = Package::build(
        &format!("pohunek.runtime.{runtime}"),
        "1.0.0",
        runtime,
        INERT_PROGRAM,
    );
    keys.signed(
        sequence,
        vec![index_entry(
            &listed,
            &format!("pohunek.runtime.{runtime}"),
            runtime,
            "1.0.0",
        )],
    )
}

/// Recovery end to end: a damaged `index-state.json` refuses local
/// installs and a plain refresh with `package_index_state_unreadable` and is
/// left untouched; `repair_state` replaces it, keeps the damaged bytes beside
/// it with the same mode, raises the marks to the fetched sequence, and the
/// next local install succeeds.
#[tokio::test]
async fn scenario_n2_repair_state_replaces_damaged_file() {
    let keys = Keys::new();
    let fixture = keys.fixture("n2-repair-state");
    apply_document(
        &fixture,
        &runtime_document(&keys, now() - 100, "aurora"),
        false,
    )
    .await
    .expect("first apply stores a copy");
    let damaged = b"{not json".to_vec();
    plant_state(&fixture, &damaged, 0o600);

    let fetched_sequence = now();
    let document = runtime_document(&keys, fetched_sequence, "aurora");
    let plain = apply_document(&fixture, &document, false)
        .await
        .expect_err("a plain refresh is refused");
    assert_eq!(plain, PackageErrorKind::IndexStateUnreadable);
    assert_eq!(
        std::fs::read(fixture.plugins.join("index-state.json")).expect("state"),
        damaged,
        "a refused refresh leaves the damaged file in place"
    );
    assert!(damaged_copies(&fixture).is_empty());
    let local = Package::build("community.dev", "1.0.0", "devrt", INERT_PROGRAM);
    assert_eq!(
        install_local(&fixture, &local)
            .await
            .expect_err("fail-closed"),
        PackageErrorKind::IndexStateUnreadable
    );

    let result = apply_document(&fixture, &document, true)
        .await
        .expect("repair apply");
    assert_eq!(result.sequence, fetched_sequence);
    let copies = damaged_copies(&fixture);
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(copies[0].1, damaged, "the damaged bytes are preserved");
    let mode = |name: &str| {
        std::fs::metadata(fixture.plugins.join(name))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(
        mode(&copies[0].0),
        0o600,
        "the preserved copy keeps its mode"
    );
    assert_eq!(mode("index-state.json"), 0o600);
    assert_eq!(
        index_state(&fixture).index_high_water,
        Some(fetched_sequence)
    );
    install_local(&fixture, &local)
        .await
        .expect("a local install succeeds after the repair");
}

/// A repair never accepts a document below the stored verified copy: the
/// damaged file, the stored copy and the marks stay as they were.
#[tokio::test]
async fn scenario_n2_repair_refuses_a_document_below_the_stored_copy() {
    let keys = Keys::new();
    let fixture = keys.fixture("n2-repair-rollback");
    let high = now();
    apply_document(&fixture, &runtime_document(&keys, high, "aurora"), false)
        .await
        .expect("apply stores the high copy");
    let stored_before = Registry::open_at(&fixture.plugins, Limits::DEFAULT)
        .expect("registry")
        .stored_index(INDEX_SCHEMA_VERSION)
        .expect("stored")
        .expect("present")
        .bytes;
    let damaged = b"{not json".to_vec();
    plant_state(&fixture, &damaged, 0o600);

    let error = apply_document(
        &fixture,
        &runtime_document(&keys, high - 50, "aurora"),
        true,
    )
    .await
    .expect_err("a lagging mirror is refused");
    assert_eq!(error, PackageErrorKind::IndexRollback);
    assert_eq!(
        std::fs::read(fixture.plugins.join("index-state.json")).expect("state"),
        damaged
    );
    assert!(damaged_copies(&fixture).is_empty());
    let stored_after = Registry::open_at(&fixture.plugins, Limits::DEFAULT)
        .expect("registry")
        .stored_index(INDEX_SCHEMA_VERSION)
        .expect("stored")
        .expect("present")
        .bytes;
    assert_eq!(
        stored_before, stored_after,
        "the newer stored copy survives"
    );
}

/// A repair keeps what the stored verified copy established: its reserved
/// runtime ids stay reserved and the marks are the larger sequence.
#[tokio::test]
async fn scenario_n2_repair_keeps_the_stored_copys_reservations_and_marks() {
    let keys = Keys::new();
    let fixture = keys.fixture("n2-repair-union");
    let stored_sequence = now() - 10;
    apply_document(
        &fixture,
        &runtime_document(&keys, stored_sequence, "aurora"),
        false,
    )
    .await
    .expect("apply");
    plant_state(&fixture, b"{not json", 0o600);
    let fetched_sequence = now();
    apply_document(
        &fixture,
        &runtime_document(&keys, fetched_sequence, "borealis"),
        true,
    )
    .await
    .expect("repair");
    let state = index_state(&fixture);
    assert_eq!(state.index_high_water, Some(fetched_sequence));
    let reserved: Vec<_> = state
        .reserved_runtime_ids
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect();
    assert_eq!(reserved, ["aurora", "borealis"]);
    let local = Package::build("community.aurora", "1.0.0", "aurora", INERT_PROGRAM);
    assert_eq!(
        install_local(&fixture, &local).await.expect_err("reserved"),
        PackageErrorKind::RuntimeNotClaimable
    );
}

/// A state file from a newer schema (after a downgrade) is repairable like a
/// corrupt one, and refused without the flag.
#[tokio::test]
async fn scenario_n2_downgrade_schema_state_is_repairable() {
    let keys = Keys::new();
    let fixture = keys.fixture("n2-downgrade");
    let document = runtime_document(&keys, now(), "aurora");
    apply_document(&fixture, &document, false)
        .await
        .expect("apply");
    plant_state(&fixture, br#"{"schema":99}"#, 0o600);
    assert_eq!(
        apply_document(&fixture, &document, false)
            .await
            .expect_err("refused"),
        PackageErrorKind::IndexStateUnreadable
    );
    apply_document(&fixture, &document, true)
        .await
        .expect("repair");
    assert_eq!(damaged_copies(&fixture).len(), 1);
    assert!(index_state(&fixture).index_high_water.is_some());
}

/// A state file with the wrong mode is not repaired: apply and installs refuse
/// with `package_index_state_unreadable`, never `package_registry_failed`, and
/// the file is untouched.
#[tokio::test]
async fn scenario_n2_unsafe_state_is_refused_and_never_repaired() {
    let keys = Keys::new();
    let fixture = keys.fixture("n2-unsafe");
    let document = runtime_document(&keys, now(), "aurora");
    apply_document(&fixture, &document, false)
        .await
        .expect("apply");
    let path = fixture.plugins.join("index-state.json");
    let before = std::fs::read(&path).expect("state");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).expect("widen");
    for repair in [false, true] {
        assert_eq!(
            apply_document(&fixture, &document, repair)
                .await
                .expect_err("refused"),
            PackageErrorKind::IndexStateUnreadable,
            "repair_state={repair}"
        );
    }
    let local = Package::build("community.dev", "1.0.0", "devrt", INERT_PROGRAM);
    assert_eq!(
        install_local(&fixture, &local).await.expect_err("refused"),
        PackageErrorKind::IndexStateUnreadable,
        "a local install on an unsafe state file is not a registry failure"
    );
    assert_eq!(std::fs::read(&path).expect("state"), before);
    assert_eq!(
        std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o666
    );
    assert!(damaged_copies(&fixture).is_empty());
}

/// A repair that cannot preserve the damaged bytes leaves the damaged state
/// file in place, so the host never falls back to the empty verifier state.
#[tokio::test]
async fn scenario_n2_a_failed_repair_never_leaves_the_host_without_a_state_file() {
    let keys = Keys::new();
    let fixture = keys.fixture("n2-failed-repair");
    let document = runtime_document(&keys, now(), "aurora");
    apply_document(&fixture, &document, false)
        .await
        .expect("apply");
    let damaged = b"{not json".to_vec();
    plant_state(&fixture, &damaged, 0o600);
    // Every preserved-copy name of the seconds around now is taken, so the
    // repair fails at the preserve step, after the new state was encoded.
    let start = now();
    for second in start - 1..=start + 3 {
        for attempt in 0..64 {
            let name = if attempt == 0 {
                format!("index-state.json.damaged-{second}")
            } else {
                format!("index-state.json.damaged-{second}-{attempt}")
            };
            std::fs::write(fixture.plugins.join(name), b"earlier").expect("taken name");
        }
    }
    apply_document(&fixture, &document, true)
        .await
        .expect_err("the repair cannot commit");
    assert_eq!(
        std::fs::read(fixture.plugins.join("index-state.json")).expect("a state file remains"),
        damaged
    );
    let local = Package::build("community.dev", "1.0.0", "devrt", INERT_PROGRAM);
    assert_eq!(
        install_local(&fixture, &local)
            .await
            .expect_err("still fail-closed"),
        PackageErrorKind::IndexStateUnreadable
    );
}

/// A preserved copy never replaces an earlier one, whatever second it is
/// taken in.
#[tokio::test]
async fn scenario_n2_repair_never_replaces_an_earlier_preserved_copy() {
    let keys = Keys::new();
    let fixture = keys.fixture("n2-two-copies");
    let document = runtime_document(&keys, now(), "aurora");
    apply_document(&fixture, &document, false)
        .await
        .expect("apply");
    let start = now();
    for second in start - 2..=start + 5 {
        std::fs::write(
            fixture
                .plugins
                .join(format!("index-state.json.damaged-{second}")),
            b"earlier",
        )
        .expect("earlier copy");
    }
    plant_state(&fixture, b"{second damage", 0o600);
    apply_document(&fixture, &document, true)
        .await
        .expect("repair");
    let copies = damaged_copies(&fixture);
    assert_eq!(copies.len(), 9, "{copies:?}");
    assert_eq!(
        copies
            .iter()
            .filter(|(_, bytes)| bytes == b"earlier")
            .count(),
        8
    );
    assert_eq!(
        copies
            .iter()
            .filter(|(_, bytes)| bytes == b"{second damage")
            .count(),
        1
    );
}

/// M1: an index install stores the document alongside the marks, so a
/// stored install from a fresh apply never sees a stale-sequence refusal.
#[tokio::test]
async fn scenario_m1_staged_install_stores_document() {
    let keys = Keys::new();
    let fixture = keys.fixture("m1-install-stores");
    let sequence = now();
    let pkg = idxfix("1.0.0");
    let document = keys.signed(sequence, vec![idxfix_entry(&pkg, "1.0.0")]);
    let index_path = fixture.write("index.json", &document);
    let archive_path = fixture.write_archive(&pkg);
    // Install by the staged index path (the CLI fallback shape).
    let installed = fixture
        .registry
        .packages()
        .package_install(PackageInstallParams {
            archive_path,
            trust: PackageTrust::Index { index_path },
            enable: true,
            select: true,
            dry_run: false,
            accept_commands: false,
        })
        .await
        .expect("indexed install");
    assert_eq!(installed.package.origin, PackageOrigin::Official);
    // The daemon stored the official copy, not just raised marks.
    let stored = fixture.plugins.join("index").join("official-v1.json");
    assert!(stored.exists(), "stored official-v1.json must exist");
    assert_eq!(
        std::fs::read(&stored).expect("read stored"),
        document,
        "stored bytes equal the staged bytes"
    );
}

/// A stored copy that cannot be written never fails a verified index install:
/// the package installs and the marks are raised.
#[tokio::test]
async fn scenario_a_failed_store_never_fails_an_index_install() {
    let keys = Keys::new();
    let fixture = keys.fixture("store-failure");
    // A regular file where the stored-index directory belongs.
    std::fs::write(fixture.plugins.join("index"), b"not a directory").expect("block the store");
    let package = idxfix("1.0.0");
    let sequence = now();
    fixture
        .registry
        .packages()
        .package_install(index_install(
            &fixture,
            &package,
            &keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]),
            false,
        ))
        .await
        .expect("the install does not depend on the stored copy");
    assert_eq!(index_state(&fixture).index_high_water, Some(sequence));
    assert_eq!(
        fixture.serving(INDEXED_RUNTIME),
        Some(package.digest.clone())
    );
}

/// An expired document is refused at apply and nothing is stored.
#[tokio::test]
async fn scenario_m5_expired_stored_document_refused() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-expired-stored");
    let past = now().saturating_sub(30 * DAY);
    let pkg = idxfix("1.0.0");
    let expired = keys.signed(past, vec![idxfix_entry(&pkg, "1.0.0")]);
    let err = fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write("expired.json", &expired),
            repair_state: false,
        })
        .await
        .expect_err("expired apply");
    assert_eq!(err, PackageErrorKind::IndexExpired);
    // Nothing was recorded: the store still reports no stored index.
    let registry = Registry::open_at(&fixture.plugins, Limits::DEFAULT).expect("registry");
    assert!(registry.stored_index(1).expect("stored").is_none());
}

/// A schema 1 registry (checked-in bytes written before the provenance
/// field existed) reads with provenance derived from the source, keeps all
/// three records and the selection, and its next write is schema 2.
#[tokio::test]
async fn scenario_m6_schema1_upgrades_by_source() {
    use package::registry::{PackageProvenance, PackageSource};
    let fixture = Fixture::new("m6-schema1");
    let root = fixture.dir.join("m6-registry");
    std::fs::create_dir_all(root.join("packages")).expect("root");
    for directory in [root.clone(), root.join("packages")] {
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .expect("owner-private dir");
    }
    let registry_file = root.join("registry.json");
    std::fs::write(
        &registry_file,
        include_bytes!("fixtures/registry-schema1.json"),
    )
    .expect("write registry");
    std::fs::set_permissions(&registry_file, std::fs::Permissions::from_mode(0o600))
        .expect("owner-private");
    let registry = Registry::open_at(&root, Limits::DEFAULT).expect("registry");
    let state = registry.state().expect("state");
    let provenances: Vec<_> = state
        .packages()
        .iter()
        .map(|record| (record.source(), record.provenance()))
        .collect();
    assert_eq!(
        provenances,
        [
            (PackageSource::Official, PackageProvenance::Catalog),
            (PackageSource::ExplicitDigest, PackageProvenance::Local),
            (PackageSource::Link, PackageProvenance::Local),
        ]
    );
    let pi = state.packages()[0].digest().clone();
    assert_eq!(
        state.selected(&PackageId::parse("pohunek.runtime.pi").expect("id")),
        Some(&pi),
        "the selection survives the upgrade"
    );
    let schema = |path: &std::path::Path| -> u64 {
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).expect("read"))
            .expect("json")["schema"]
            .as_u64()
            .expect("schema")
    };
    assert_eq!(schema(&registry_file), 1, "no write has happened yet");
    registry.set_enabled(&pi, false).expect("set_enabled");
    assert_eq!(schema(&registry_file), 2, "the next write is schema 2");
    assert_eq!(registry.state().expect("state").packages().len(), 3);
}

async fn doctor_plugin_index(fixture: &Fixture) -> Vec<protocol::DoctorCheck> {
    let paths = crate::Paths {
        runtime_dir: fixture.dir.join("runtime"),
        socket: fixture.dir.join("runtime").join("daemon.sock"),
        lock: fixture.dir.join("runtime").join("daemon.lock"),
        log_dir: fixture.dir.join("logs"),
        state_dir: fixture.dir.clone(),
        data_dir: fixture.dir.join("data"),
        cache_dir: fixture.dir.join("cache"),
        config_home: fixture.dir.join("config"),
        config_dir: fixture.dir.join("config").join("pohunek"),
    };
    let governance = crate::governance::HostGovernanceService::open_test();
    let trust = fixture.registry.package_authority().catalog_trust().clone();
    let report = crate::doctor::report(
        &paths,
        &governance,
        None,
        &trust,
        &crate::agent::ProfileRegistry::default(),
        pohunek_worker_protocol::BaseEnv::default(),
        hostcheck::OverlayChecks::NONE,
        false,
        crate::session::RejectedLinesState::Counted {
            total: 0,
            may_pin: 0,
        },
        &[],
        &crate::session::rights::RootFindings::default(),
        &crate::capabilities::ProbeClock::default(),
        hostcheck::AGENT_RUNTIME_PROBE_BUDGET,
    )
    .await
    .expect("doctor report");
    report
        .checks
        .into_iter()
        .filter(|check| check.name == "plugin_index")
        .collect()
}

/// A CLI refresh (`plugin.index.apply`) leaves doctor `current`.
#[tokio::test]
async fn scenario_m5_doctor_current_after_apply_and_catalog_runtime_stays_ok() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-doctor-current");
    let sequence = now();
    let package = idxfix("1.0.0");
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write(
                "index.json",
                &keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]),
            ),
            repair_state: false,
        })
        .await
        .expect("apply");
    // Bundled catalog runtime (codex/claude/hermes shape) installed through
    // the catalog stays `catalog` provenance and is never treated as an
    // index-origin package by doctor (H2 regression).
    let catalog_package = Package::pi();
    fixture
        .registry
        .packages()
        .package_install(catalog_install(
            &fixture,
            &catalog_package,
            &signed_by(
                &keys.root,
                catalog(5, vec![entry(&catalog_package, PACKAGE, RUNTIME, "1.0.0")]),
            ),
            false,
        ))
        .await
        .expect("catalog install");
    let checks = doctor_plugin_index(&fixture).await;
    assert_eq!(checks.len(), 1);
    assert_eq!(
        (checks[0].status, checks[0].detail.as_str()),
        (protocol::DoctorStatus::Ok, "current")
    );
}

/// A stored copy older than 24 h that has not expired warns with
/// `stale`; the document stays usable.
#[tokio::test]
async fn scenario_m5_doctor_stale_stored_copy_warns() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-doctor-stale");
    let package = idxfix("1.0.0");
    apply_document(
        &fixture,
        &keys.signed(now(), vec![idxfix_entry(&package, "1.0.0")]),
        false,
    )
    .await
    .expect("apply");
    // The daemon clock is not injectable; the sidecar's `stored_at` is aged.
    let index_dir = fixture.plugins.join("index");
    let past = now().saturating_sub(2 * package::index_store::STORED_INDEX_MAX_AGE);
    std::fs::write(
        index_dir.join("stored.json"),
        serde_json::to_vec(&serde_json::json!({"schema": 1, "official-v1": past})).expect("json"),
    )
    .expect("write sidecar");
    std::fs::set_permissions(
        index_dir.join("stored.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .expect("mode");
    let checks = doctor_plugin_index(&fixture).await;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, protocol::DoctorStatus::Warn);
    assert!(
        checks[0].detail.starts_with("stale"),
        "{}",
        checks[0].detail
    );
}

/// Delisting an installed index-origin digest fails doctor with
/// `package_revoked` while the installed package still launches.
#[tokio::test]
async fn scenario_m5_doctor_package_revoked_after_delisting() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-doctor-revoked");
    let sequence = now();
    let package = idxfix("1.0.0");
    fixture
        .registry
        .packages()
        .package_install(index_install(
            &fixture,
            &package,
            &keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]),
            false,
        ))
        .await
        .expect("index install");
    // Publish N+1 without this digest under its id.
    fixture
        .registry
        .packages()
        .package_index_apply(protocol::PluginIndexApplyParams {
            index_path: fixture.write("next.json", &keys.signed(sequence + 1, vec![])),
            repair_state: false,
        })
        .await
        .expect("apply delisting");
    let checks = doctor_plugin_index(&fixture).await;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, protocol::DoctorStatus::Fail);
    assert!(
        checks[0].detail.starts_with("package_revoked:"),
        "{}",
        checks[0].detail
    );
    assert_eq!(
        fixture.serving(INDEXED_RUNTIME),
        Some(package.digest.clone()),
        "a delisted package keeps serving: doctor is a warning, not a launch gate"
    );
}

/// A local package installed before the official index reserved its
/// runtime id keeps serving after the reservation, and doctor warns.
#[tokio::test]
async fn scenario_m5_doctor_reserved_runtime_warns() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-doctor-reserved");
    let local = Package::build("community.aurora", "1.0.0", "aurora", INERT_PROGRAM);
    install_local(&fixture, &local)
        .await
        .expect("local install before the reservation");
    apply_document(&fixture, &runtime_document(&keys, now(), "aurora"), false)
        .await
        .expect("apply");
    assert_eq!(
        fixture.serving("aurora"),
        Some(local.digest.clone()),
        "the installed local package still launches after the reservation"
    );
    let checks = doctor_plugin_index(&fixture).await;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, protocol::DoctorStatus::Warn);
    assert!(
        checks[0]
            .detail
            .starts_with("runtime_id_reserved_by_official:"),
        "{}",
        checks[0].detail
    );
}

/// Scenario 6, link path: a linked directory that declares a runtime id the
/// official index reserved is refused like an exact-digest install.
#[tokio::test]
async fn scenario_6_a_linked_directory_cannot_claim_a_reserved_runtime() {
    let keys = Keys::new();
    let fixture = keys.fixture("scenario-6-link");
    apply_document(&fixture, &runtime_document(&keys, now(), RUNTIME), false)
        .await
        .expect("apply");
    let directory = fixture.dir.join("developer");
    std::fs::create_dir(&directory).expect("developer directory");
    let source = crate::agent::host::fixture::pi_shaped_document(
        std::path::Path::new(INERT_PROGRAM),
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .replace(
        "detect_manifest = \"any\"",
        "detect_manifest = \"detect.toml\"",
    );
    std::fs::write(directory.join("runtime.toml"), &source).expect("descriptor");
    std::fs::write(
        directory.join("detect.toml"),
        "[[rules]]\nid = \"idle_prompt\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n",
    )
    .expect("manifest");
    let error = fixture
        .registry
        .packages()
        .package_link(protocol::PackageLinkParams {
            directory: directory.to_str().expect("utf-8").to_owned(),
            dry_run: false,
            accept_commands: false,
        })
        .await
        .expect_err("the reserved runtime id is refused");
    assert_eq!(error, PackageErrorKind::RuntimeNotClaimable);
}

/// Scenario 7: a damaged stored document fails doctor without stopping the
/// daemon, and a refresh repairs it even with a stale temporary file beside it.
#[tokio::test]
async fn scenario_m5_doctor_damaged_stored_document_and_stale_temp() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-doctor-damaged");
    let package = idxfix("1.0.0");
    let document = keys.signed(now(), vec![idxfix_entry(&package, "1.0.0")]);
    apply_document(&fixture, &document, false)
        .await
        .expect("apply");
    let index_dir = fixture.plugins.join("index");
    for (name, bytes) in [
        ("official-v1.json", &b"corrupt"[..]),
        ("document.json.tmp", b"stale"),
    ] {
        std::fs::write(index_dir.join(name), bytes).expect("plant");
        std::fs::set_permissions(index_dir.join(name), std::fs::Permissions::from_mode(0o600))
            .expect("mode");
    }
    let checks = doctor_plugin_index(&fixture).await;
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].status, protocol::DoctorStatus::Fail);
    assert!(
        checks[0].detail.starts_with("package_index_invalid:"),
        "{}",
        checks[0].detail
    );

    apply_document(&fixture, &document, false)
        .await
        .expect("a refresh replaces the damaged document despite the stale temporary");
    let checks = doctor_plugin_index(&fixture).await;
    assert_eq!(
        (checks[0].status, checks[0].detail.as_str()),
        (protocol::DoctorStatus::Ok, "current")
    );
}

/// An index-origin package without any stored copy (a host that installed by
/// id before the stored index existed) warns with `not_refreshed`, never fails.
#[tokio::test]
async fn scenario_m5_doctor_not_refreshed_for_an_index_package_without_a_stored_copy() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-doctor-not-refreshed");
    let package = idxfix("1.0.0");
    fixture
        .registry
        .packages()
        .package_install(index_install(
            &fixture,
            &package,
            &keys.signed(now(), vec![idxfix_entry(&package, "1.0.0")]),
            false,
        ))
        .await
        .expect("index install");
    std::fs::remove_dir_all(fixture.plugins.join("index")).expect("drop the stored copy");
    let checks = doctor_plugin_index(&fixture).await;
    assert_eq!(checks[0].status, protocol::DoctorStatus::Warn);
    assert!(
        checks[0].detail.starts_with("not_refreshed:"),
        "{}",
        checks[0].detail
    );
}

/// A stored copy past its `expires_at` warns with `package_index_expired`; the
/// installed package keeps serving.
#[tokio::test]
async fn scenario_m5_doctor_reports_an_expired_stored_copy() {
    let keys = Keys::new();
    let fixture = keys.fixture("m5-doctor-index-expired");
    let package = idxfix("1.0.0");
    install_local(
        &fixture,
        &Package::build("community.dev", "1.0.0", "devrt", INERT_PROGRAM),
    )
    .await
    .expect("local install creates the registry");
    // An expired document cannot be applied, so the stored files are planted.
    let sequence = now() - 8 * DAY;
    let expired = keys.signed(sequence, vec![idxfix_entry(&package, "1.0.0")]);
    let index_dir = fixture.plugins.join("index");
    std::fs::create_dir_all(&index_dir).expect("index dir");
    std::fs::set_permissions(&index_dir, std::fs::Permissions::from_mode(0o700)).expect("mode");
    std::fs::write(index_dir.join("official-v1.json"), &expired).expect("document");
    std::fs::write(
        index_dir.join("stored.json"),
        serde_json::to_vec(&serde_json::json!({"schema": 1, "official-v1": now()})).expect("json"),
    )
    .expect("sidecar");
    for name in ["official-v1.json", "stored.json"] {
        std::fs::set_permissions(index_dir.join(name), std::fs::Permissions::from_mode(0o600))
            .expect("mode");
    }
    let checks = doctor_plugin_index(&fixture).await;
    assert_eq!(checks[0].status, protocol::DoctorStatus::Warn);
    assert!(
        checks[0].detail.starts_with("package_index_expired:"),
        "{}",
        checks[0].detail
    );
    assert!(
        fixture.serving("devrt").is_some(),
        "installed packages keep serving"
    );
}
