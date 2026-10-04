//! Catalog trust of the package lifecycle: official installs through a signed
//! catalog and every way a catalog or an entry is refused.
//!
//! Keys derive from fixed labels. The signing windows and the catalog expiry
//! span the real clock, because the daemon reads it directly.

// Rust guideline compliant 2026-10-04

use std::fmt::Write as _;

use ed25519_dalek::{Signer as _, SigningKey};
use package::registry::Registry;
use package::{
    catalog_signing_message, Catalog, CatalogEntry, CatalogEnvelope, CatalogSignature, KeyId,
    Limits, RootKey, CATALOG_SCHEMA_VERSION,
};
use protocol::{
    PackageErrorKind, PackageId, PackageInstallParams, PackageOrigin, PackageTrust, PackageVersion,
    RuntimeId,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::test_fixture::{explicit, Fixture, Package, INERT_PROGRAM, PACKAGE, RUNTIME};
use super::trust::host_platform;
use super::HostTrustAnchor;
use crate::agent::host::RESERVED_RUNTIME_IDS;

/// Start of every signing window: the first second after the epoch.
const WINDOW_START: u64 = 1;

/// End of every signing window and of a fresh catalog: 2100-01-01.
const WINDOW_END: u64 = 4_102_444_800;

/// A core range every released version satisfies.
const ANY_CORE: &str = ">=0.0.0";

fn signing_key(label: &str) -> SigningKey {
    let seed: [u8; 32] = Sha256::digest(label.as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

fn key_id(key: &SigningKey) -> KeyId {
    KeyId::derive(&key.verifying_key())
}

fn root(key: &SigningKey) -> RootKey {
    RootKey::new(key.verifying_key().to_bytes(), WINDOW_START, WINDOW_END).expect("valid root")
}

fn anchor(keys: &[&SigningKey], revoked: Vec<KeyId>) -> HostTrustAnchor {
    HostTrustAnchor::new(keys.iter().map(|key| root(key)).collect(), revoked).expect("anchor")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        write!(text, "{byte:02x}").expect("writing to a String");
        text
    })
}

/// A catalog entry that binds `package` to `package_id`, `runtime` and
/// `version`.
fn entry(package: &Package, package_id: &str, runtime: &str, version: &str) -> CatalogEntry {
    CatalogEntry {
        package_id: PackageId::parse(package_id).expect("package id"),
        runtime_id: RuntimeId::parse(runtime).expect("runtime id"),
        version: PackageVersion::parse(version).expect("version"),
        digest: package.digest.clone(),
        platforms: vec![host_platform().expect("a supported platform")],
        core: ANY_CORE.to_owned(),
    }
}

fn catalog(sequence: u64, entries: Vec<CatalogEntry>) -> Catalog {
    Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        sequence,
        expires_at: WINDOW_END,
        revoked_key_ids: Vec::new(),
        revoked_digests: Vec::new(),
        entries,
    }
}

fn signed_by(key: &SigningKey, catalog: Catalog) -> Vec<u8> {
    let message = catalog_signing_message(&catalog).expect("signing message");
    let signature = CatalogSignature {
        key_id: key_id(key),
        signature: hex(&key.sign(&message).to_bytes()),
    };
    serde_json::to_vec(&CatalogEnvelope {
        schema_version: CATALOG_SCHEMA_VERSION,
        catalog,
        key_chain: Vec::new(),
        signatures: vec![signature],
    })
    .expect("serialize")
}

fn catalog_install(
    fixture: &Fixture,
    package: &Package,
    document: &[u8],
    dry_run: bool,
) -> PackageInstallParams {
    PackageInstallParams {
        archive_path: fixture.write_archive(package),
        trust: PackageTrust::Catalog {
            catalog_path: fixture.write("catalog.json", document),
        },
        enable: true,
        select: true,
        dry_run,
    }
}

fn state(fixture: &Fixture) -> package::catalog_state::CatalogState {
    Registry::open_at(&fixture.plugins, Limits::DEFAULT)
        .expect("registry")
        .catalog_state()
        .expect("catalog state")
}

fn default_catalog(package: &Package, sequence: u64) -> Catalog {
    catalog(sequence, vec![entry(package, PACKAGE, RUNTIME, "1.0.0")])
}

#[tokio::test]
async fn a_host_without_a_trust_anchor_fails_closed() {
    let fixture = Fixture::new("catalog-no-anchor");
    let package = Package::pi();
    let key = signing_key("root");
    let document = signed_by(&key, default_catalog(&package, 1));

    let refused = fixture
        .registry
        .package_install(catalog_install(&fixture, &package, &document, false))
        .await
        .expect_err("no anchor");

    assert_eq!(refused, PackageErrorKind::TrustUnavailable);
    assert!(!fixture.root(&package.digest).exists());
}

#[tokio::test]
async fn a_signed_catalog_installs_an_official_package_and_persists_its_sequence() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-official", anchor(&[&key], Vec::new()));
    let package = Package::pi();
    let document = signed_by(&key, default_catalog(&package, 5));

    let installed = fixture
        .registry
        .package_install(catalog_install(&fixture, &package, &document, false))
        .await
        .expect("official install");

    assert_eq!(installed.package.origin, PackageOrigin::Official);
    assert!(installed.package.enabled && installed.package.selected);
    assert_eq!(fixture.serving(RUNTIME), Some(package.digest));
    assert_eq!(state(&fixture).high_water(), Some(5));
}

#[tokio::test]
async fn a_catalog_dry_run_records_nothing() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-dry-run", anchor(&[&key], Vec::new()));
    let package = Package::pi();
    let document = signed_by(&key, default_catalog(&package, 5));

    let preview = fixture
        .registry
        .package_install(catalog_install(&fixture, &package, &document, true))
        .await
        .expect("preview");

    assert_eq!(preview.package.origin, PackageOrigin::Official);
    assert_eq!(state(&fixture).high_water(), None);
    assert!(fixture
        .registry
        .package_list()
        .await
        .expect("list")
        .packages
        .is_empty());
}

#[tokio::test]
async fn an_explicit_digest_never_yields_an_official_package() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-explicit", anchor(&[&key], Vec::new()));
    let package = Package::pi();
    let path = fixture.write_archive(&package);

    let installed = fixture
        .registry
        .package_install(explicit(&path, &package, true, true))
        .await
        .expect("explicit install");

    assert_eq!(installed.package.origin, PackageOrigin::ExplicitDigest);
}

#[tokio::test]
async fn catalogs_that_do_not_verify_install_nothing() {
    let key = signing_key("root");
    let outsider = signing_key("outsider");
    let fixture = Fixture::with_anchor(
        "catalog-refused",
        anchor(&[&key], vec![key_id(&signing_key("revoked"))]),
    );
    let package = Package::pi();
    let good = signed_by(&key, default_catalog(&package, 5));

    let mut tampered: Value = serde_json::from_slice(&good).expect("json");
    tampered["catalog"]["sequence"] = Value::from(99);
    let other = Package::build("acme.runtime.other", "1.0.0", "other", INERT_PROGRAM);
    let mut expired = default_catalog(&package, 5);
    expired.expires_at = 1_000;
    let unknown_digest = catalog(5, vec![entry(&other, PACKAGE, RUNTIME, "1.0.0")]);
    let wrong_version = catalog(5, vec![entry(&package, PACKAGE, RUNTIME, "9.9.9")]);
    let wrong_runtime = catalog(5, vec![entry(&package, PACKAGE, "elsewhere", "1.0.0")]);
    let wrong_package = catalog(
        5,
        vec![entry(&package, "acme.runtime.other", RUNTIME, "1.0.0")],
    );

    let cases = [
        ("tampered", serde_json::to_vec(&tampered).expect("json")),
        (
            "unknown signer",
            signed_by(&outsider, default_catalog(&package, 5)),
        ),
        ("expired", signed_by(&key, expired)),
        ("digest not in the catalog", signed_by(&key, unknown_digest)),
        ("version differs", signed_by(&key, wrong_version)),
        ("runtime differs", signed_by(&key, wrong_runtime)),
        ("package id differs", signed_by(&key, wrong_package)),
        ("garbage", b"not a catalog".to_vec()),
    ];
    let before = fixture.snapshot();
    for (name, document) in cases {
        let refused = fixture
            .registry
            .package_install(catalog_install(&fixture, &package, &document, false))
            .await
            .expect_err(name);
        assert_eq!(refused, PackageErrorKind::Untrusted, "{name}");
        assert_eq!(fixture.snapshot(), before, "{name}");
    }
    assert_eq!(state(&fixture).high_water(), None);
}

#[tokio::test]
async fn a_signer_the_anchor_revokes_is_refused() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor(
        "catalog-revoked-anchor",
        anchor(&[&key], vec![key_id(&key)]),
    );
    let package = Package::pi();
    let document = signed_by(&key, default_catalog(&package, 5));

    let refused = fixture
        .registry
        .package_install(catalog_install(&fixture, &package, &document, false))
        .await
        .expect_err("revoked signer");

    assert_eq!(refused, PackageErrorKind::Untrusted);
}

#[tokio::test]
async fn an_older_catalog_is_refused_after_a_newer_one_was_used() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-stale", anchor(&[&key], Vec::new()));
    let first = Package::pi();
    let second = Package::build("acme.runtime.two", "1.0.0", "two", INERT_PROGRAM);
    fixture
        .registry
        .package_install(catalog_install(
            &fixture,
            &first,
            &signed_by(&key, default_catalog(&first, 10)),
            false,
        ))
        .await
        .expect("sequence 10");
    let stale = signed_by(
        &key,
        catalog(9, vec![entry(&second, "acme.runtime.two", "two", "1.0.0")]),
    );

    let refused = fixture
        .registry
        .package_install(catalog_install(&fixture, &second, &stale, false))
        .await
        .expect_err("stale sequence");

    assert_eq!(refused, PackageErrorKind::Untrusted);
    assert_eq!(state(&fixture).high_water(), Some(10));
    assert!(!fixture.root(&second.digest).exists());
}

#[tokio::test]
async fn a_key_an_earlier_catalog_revoked_stays_revoked() {
    let first_key = signing_key("first");
    let second_key = signing_key("second");
    let fixture = Fixture::with_anchor(
        "catalog-persisted-revocation",
        anchor(&[&first_key, &second_key], Vec::new()),
    );
    let one = Package::pi();
    let two = Package::build("acme.runtime.two", "1.0.0", "two", INERT_PROGRAM);
    let mut revoking = default_catalog(&one, 1);
    revoking.revoked_key_ids = vec![key_id(&second_key)];
    fixture
        .registry
        .package_install(catalog_install(
            &fixture,
            &one,
            &signed_by(&first_key, revoking),
            false,
        ))
        .await
        .expect("the first catalog installs");
    assert!(state(&fixture)
        .revoked_key_ids()
        .contains(&key_id(&second_key)));

    let later = |key: &SigningKey| {
        signed_by(
            key,
            catalog(2, vec![entry(&two, "acme.runtime.two", "two", "1.0.0")]),
        )
    };
    let refused = fixture
        .registry
        .package_install(catalog_install(&fixture, &two, &later(&second_key), false))
        .await
        .expect_err("the revoked key no longer signs");
    assert_eq!(refused, PackageErrorKind::Untrusted);

    fixture
        .registry
        .package_install(catalog_install(&fixture, &two, &later(&first_key), false))
        .await
        .expect("the surviving key still signs");
}

#[tokio::test]
async fn an_entry_for_another_core_or_platform_is_incompatible() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-incompatible", anchor(&[&key], Vec::new()));
    let package = Package::pi();
    let mut wrong_core = entry(&package, PACKAGE, RUNTIME, "1.0.0");
    wrong_core.core = ">=999.0.0".to_owned();
    let mut wrong_platform = entry(&package, PACKAGE, RUNTIME, "1.0.0");
    wrong_platform.platforms = vec!["plan9-mips".to_owned()];

    for (name, entry) in [("core", wrong_core), ("platform", wrong_platform)] {
        let document = signed_by(&key, catalog(5, vec![entry]));
        let refused = fixture
            .registry
            .package_install(catalog_install(&fixture, &package, &document, false))
            .await
            .expect_err(name);
        assert_eq!(refused, PackageErrorKind::Incompatible, "{name}");
    }
    assert!(!fixture.root(&package.digest).exists());
}

#[tokio::test]
async fn an_official_alias_conflicts_while_a_built_in_serves_it() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-alias", anchor(&[&key], Vec::new()));
    for alias in RESERVED_RUNTIME_IDS
        .into_iter()
        .filter(|id| *id != RuntimeId::SHELL)
    {
        let package = Package::build("acme.runtime.alias", "1.0.0", alias, INERT_PROGRAM);
        let document = signed_by(
            &key,
            catalog(
                5,
                vec![entry(&package, "acme.runtime.alias", alias, "1.0.0")],
            ),
        );

        let refused = fixture
            .registry
            .package_install(catalog_install(&fixture, &package, &document, false))
            .await
            .expect_err(alias);

        assert_eq!(refused, PackageErrorKind::RuntimeConflict, "{alias}");
        assert!(!fixture.root(&package.digest).exists());
    }
}

#[tokio::test]
async fn unreadable_catalog_and_archive_paths_are_source_errors() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-paths", anchor(&[&key], Vec::new()));
    let package = Package::pi();
    let archive = fixture.write_archive(&package);

    for catalog_path in [
        "relative.json".to_owned(),
        format!("{}/missing.json", fixture.dir.display()),
    ] {
        let refused = fixture
            .registry
            .package_install(PackageInstallParams {
                archive_path: archive.clone(),
                trust: PackageTrust::Catalog { catalog_path },
                enable: true,
                select: true,
                dry_run: false,
            })
            .await
            .expect_err("unreadable catalog");
        assert_eq!(refused, PackageErrorKind::SourceUnreadable);
    }
}

/// `count` distinct well-formed key ids derived from `label`.
fn revoked_ids(label: &str, count: usize) -> Vec<KeyId> {
    (0..count)
        .map(|index| {
            let digest = Sha256::digest(format!("{label}-{index}").as_bytes());
            KeyId::parse(&hex(&digest)).expect("a 64-digit key id")
        })
        .collect()
}

#[tokio::test]
async fn a_failed_trust_state_write_installs_enables_and_reloads_nothing() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-record-failure", anchor(&[&key], Vec::new()));
    let one = Package::pi();
    let two = Package::build("acme.runtime.two", "1.0.0", "two", INERT_PROGRAM);
    let mut first = default_catalog(&one, 1);
    first.revoked_key_ids = revoked_ids("first", 40);
    fixture
        .registry
        .package_install(catalog_install(
            &fixture,
            &one,
            &signed_by(&key, first),
            false,
        ))
        .await
        .expect("the first catalog installs");
    let before = fixture.snapshot();
    let state_before = state(&fixture);
    // The union with the first catalog's revocations exceeds the persisted
    // bound, so the trust state cannot be written.
    let mut second = catalog(2, vec![entry(&two, "acme.runtime.two", "two", "1.0.0")]);
    second.revoked_key_ids = revoked_ids("second", 40);

    let refused = fixture
        .registry
        .package_install(catalog_install(
            &fixture,
            &two,
            &signed_by(&key, second),
            false,
        ))
        .await
        .expect_err("the trust state cannot be recorded");

    assert_eq!(refused, PackageErrorKind::RegistryFailed);
    assert_eq!(fixture.snapshot(), before, "nothing was installed");
    assert_eq!(
        state(&fixture),
        state_before,
        "the trust state is unchanged"
    );
    assert_eq!(
        fixture.serving("two"),
        None,
        "nothing was enabled or reloaded"
    );
}

#[tokio::test]
async fn trust_state_is_persisted_before_a_package_that_then_fails_to_install() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("catalog-record-first", anchor(&[&key], Vec::new()));
    let package = Package::pi();
    fixture
        .registry
        .package_install(catalog_install(
            &fixture,
            &package,
            &signed_by(&key, default_catalog(&package, 5)),
            false,
        ))
        .await
        .expect("sequence 5 installs");
    // The same identity from another archive: the install is refused after the
    // catalog was verified and recorded.
    let rival = Package::build(PACKAGE, "1.0.0", RUNTIME, "/bin/true");
    let refused = fixture
        .registry
        .package_install(catalog_install(
            &fixture,
            &rival,
            &signed_by(
                &key,
                catalog(7, vec![entry(&rival, PACKAGE, RUNTIME, "1.0.0")]),
            ),
            false,
        ))
        .await
        .expect_err("the identity is installed from another archive");

    assert_eq!(refused, PackageErrorKind::IdentityInstalled);
    assert_eq!(state(&fixture).high_water(), Some(7), "recorded first");
    assert!(!fixture.root(&rival.digest).exists());
}
