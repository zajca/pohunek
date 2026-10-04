// Rust guideline compliant 2026-10-04

//! Behavioural and fault-injection tests of the package registry store.
//!
//! Interrupted operations are reproduced by building the exact disk state a
//! crash leaves behind, then running the next operation on it.

#![cfg(unix)]

mod common;

use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{archive_bytes, entry, mode_of, sample_entries, set_mode, PluginFixture};
use package::install::{install_archive, InstallError};
use package::registry::{
    IncompatibleReason, InstallRequest, InstallStatus, PackageSource, Registry, RegistryError,
    RetainedDigests, RetainedState,
};
use package::verify::VerifyError;
use package::{read_archive, ArchiveEntry, Limits, PackageDigest};
use pohunek_platform::filesystem::TrustedDir;
use protocol::{PackageId, PackageIdentity, PackageVersion};

struct Sample {
    bytes: Vec<u8>,
    digest: PackageDigest,
    identity: PackageIdentity,
}

fn identity(id: &str, version: &str) -> PackageIdentity {
    PackageIdentity {
        id: PackageId::parse(id).unwrap(),
        version: PackageVersion::parse(version).unwrap(),
    }
}

fn sample_of(entries: &[ArchiveEntry], id: &str, version: &str) -> Sample {
    let bytes = archive_bytes(entries);
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .unwrap()
        .digest()
        .clone();
    Sample {
        bytes,
        digest,
        identity: identity(id, version),
    }
}

fn sample() -> Sample {
    sample_of(&sample_entries(), "acme.runtime", "1.0.0")
}

fn other() -> Sample {
    sample_of(
        &[entry("runtime.toml", b"other\n", false)],
        "acme.other",
        "2.0.0",
    )
}

fn request(sample: &Sample) -> InstallRequest<'_> {
    InstallRequest {
        archive: &sample.bytes,
        expected: &sample.digest,
        identity: sample.identity.clone(),
        source: PackageSource::ExplicitDigest,
        enabled: true,
        select: false,
        installed_at_unix_seconds: 1_700_000_000,
    }
}

fn names(directory: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn no_references() -> RetainedDigests {
    RetainedDigests::new()
}

#[test]
fn a_fresh_registry_is_empty_and_owner_private() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();

    let state = registry.state().unwrap();
    assert_eq!(state.generation(), 0);
    assert!(state.packages().is_empty());
    assert_eq!(mode_of(&fixture.plugins()), 0o700);
    assert_eq!(mode_of(&fixture.packages()), 0o700);
}

#[test]
fn install_records_the_package_and_persists_it_across_reopen() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let report = fixture
        .open()
        .install(&InstallRequest {
            select: true,
            ..request(&sample)
        })
        .unwrap();

    assert_eq!(report.status, InstallStatus::Installed);
    assert_eq!(report.generation, 1);
    assert_eq!(report.record.digest(), &sample.digest);
    assert_eq!(report.record.identity(), &sample.identity);
    assert_eq!(report.record.source(), PackageSource::ExplicitDigest);
    assert_eq!(report.record.installed_at_unix_seconds(), 1_700_000_000);
    assert!(report.record.enabled());

    let state = fixture.open().state().unwrap();
    assert_eq!(state.generation(), 1);
    assert_eq!(state.package(&sample.digest), Some(&report.record));
    assert_eq!(state.selected(&sample.identity.id), Some(&sample.digest));
    assert_eq!(mode_of(&fixture.registry_file()), 0o600);
    assert_eq!(mode_of(&fixture.plugins().join("registry.lock")), 0o600);
    assert_eq!(mode_of(&fixture.root(&sample.digest)), 0o700);
    fixture
        .open()
        .verify(&sample.digest)
        .expect("installed root verifies");
}

#[test]
fn install_is_idempotent_for_the_same_digest() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();
    let again = registry.install(&request(&sample)).unwrap();

    assert_eq!(again.status, InstallStatus::AlreadyInstalled);
    assert_eq!(again.generation, 1);
    assert_eq!(registry.state().unwrap().generation(), 1);
}

#[test]
fn install_refuses_a_digest_under_another_identity() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();

    let renamed = InstallRequest {
        identity: identity("acme.renamed", "1.0.0"),
        ..request(&sample)
    };
    assert_eq!(
        registry.install(&renamed).unwrap_err(),
        RegistryError::IdentityConflict
    );
    assert_eq!(registry.state().unwrap().generation(), 1);
}

#[test]
fn install_refuses_an_identity_installed_from_another_archive() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let first = sample();
    registry.install(&request(&first)).unwrap();

    let second = sample_of(
        &[entry("runtime.toml", b"changed\n", false)],
        "acme.runtime",
        "1.0.0",
    );
    assert_eq!(
        registry.install(&request(&second)).unwrap_err(),
        RegistryError::IdentityInstalled
    );
    assert_eq!(registry.state().unwrap().packages().len(), 1);
    assert_eq!(
        names(&fixture.packages()).len(),
        1,
        "the second root was never extracted"
    );
}

#[test]
fn install_rejects_a_digest_mismatch_and_garbage_without_changes() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    let wrong = other().digest;

    let mismatch = InstallRequest {
        expected: &wrong,
        ..request(&sample)
    };
    assert!(matches!(
        registry.install(&mismatch).unwrap_err(),
        RegistryError::Archive(package::ArchiveError::DigestMismatch)
    ));

    let garbage = b"not an archive".to_vec();
    let garbage_digest = sha_digest(&garbage);
    let hostile = InstallRequest {
        archive: &garbage,
        expected: &garbage_digest,
        ..request(&sample)
    };
    assert!(matches!(
        registry.install(&hostile).unwrap_err(),
        RegistryError::Archive(_)
    ));

    assert_eq!(registry.state().unwrap().generation(), 0);
    assert!(names(&fixture.packages()).is_empty());
}

fn sha_digest(bytes: &[u8]) -> PackageDigest {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let mut text = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        write!(text, "{byte:02x}").unwrap();
    }
    PackageDigest::parse(&text).unwrap()
}

#[test]
fn install_rejects_decompression_bombs_and_oversized_archives() {
    use ruzstd::encoding::{compress_to_vec, CompressionLevel};

    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();

    // 80 MiB of zeros compress to a few KiB: far beyond the expansion ratio.
    let bomb = compress_to_vec(
        vec![0_u8; 80 * 1024 * 1024].as_slice(),
        CompressionLevel::Fastest,
    );
    let bomb_digest = sha_digest(&bomb);
    let bomb_request = InstallRequest {
        archive: &bomb,
        expected: &bomb_digest,
        ..request(&sample)
    };
    assert!(matches!(
        registry.install(&bomb_request).unwrap_err(),
        RegistryError::Archive(_)
    ));

    let tight = fixture.open_with(Limits {
        max_compressed_bytes: 16,
        ..Limits::DEFAULT
    });
    assert_eq!(
        tight.install(&request(&sample)).unwrap_err(),
        RegistryError::Archive(package::ArchiveError::CompressedTooLarge { limit: 16 })
    );

    assert_eq!(registry.state().unwrap().generation(), 0);
    assert!(names(&fixture.packages()).is_empty());
}

#[test]
fn enable_disable_and_select_commit_one_generation_each() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();

    assert_eq!(registry.set_enabled(&sample.digest, false).unwrap(), 2);
    assert!(!registry
        .state()
        .unwrap()
        .package(&sample.digest)
        .unwrap()
        .enabled());
    assert_eq!(
        registry.set_enabled(&sample.digest, false).unwrap(),
        2,
        "no change, no commit"
    );
    assert_eq!(registry.set_enabled(&sample.digest, true).unwrap(), 3);
    assert_eq!(registry.select(&sample.digest).unwrap(), 4);
    assert_eq!(registry.select(&sample.digest).unwrap(), 4);
    assert_eq!(
        registry.state().unwrap().selected(&sample.identity.id),
        Some(&sample.digest)
    );
    assert_eq!(
        registry.set_enabled(&other().digest, true).unwrap_err(),
        RegistryError::NotInstalled
    );
    assert_eq!(
        registry.select(&other().digest).unwrap_err(),
        RegistryError::NotInstalled
    );
}

#[test]
fn enabling_and_selecting_refuse_a_root_that_fails_verification() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry
        .install(&InstallRequest {
            enabled: false,
            ..request(&sample)
        })
        .unwrap();
    std::fs::write(fixture.root(&sample.digest).join("files/LICENSE"), b"GPL\n").unwrap();

    assert!(matches!(
        registry.set_enabled(&sample.digest, true).unwrap_err(),
        RegistryError::RootInvalid(VerifyError::Modified { .. })
    ));
    assert!(matches!(
        registry.select(&sample.digest).unwrap_err(),
        RegistryError::RootInvalid(VerifyError::Modified { .. })
    ));
    assert_eq!(registry.state().unwrap().generation(), 1);
    // Disabling never reads the root.
    registry.set_enabled(&sample.digest, false).unwrap();
}

#[test]
fn uninstall_refuses_a_referenced_digest_and_removes_an_unreferenced_one() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry
        .install(&InstallRequest {
            select: true,
            ..request(&sample)
        })
        .unwrap();

    let referenced: RetainedDigests = [sample.digest.clone()].into_iter().collect();
    assert_eq!(
        registry.uninstall(&sample.digest, &referenced).unwrap_err(),
        RegistryError::StillReferenced
    );
    assert_eq!(registry.state().unwrap().generation(), 1);
    registry.verify(&sample.digest).expect("root untouched");

    let generation = registry
        .uninstall(&sample.digest, &no_references())
        .unwrap();
    assert_eq!(generation, 2);
    let state = registry.state().unwrap();
    assert!(state.packages().is_empty());
    assert_eq!(state.selected(&sample.identity.id), None);
    assert!(names(&fixture.packages()).is_empty());
    assert_eq!(
        registry
            .uninstall(&sample.digest, &no_references())
            .unwrap_err(),
        RegistryError::NotInstalled
    );
}

#[test]
fn uninstall_only_honors_the_digest_it_names() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let (first, second) = (sample(), other());
    registry.install(&request(&first)).unwrap();
    registry.install(&request(&second)).unwrap();

    let referenced: RetainedDigests = [second.digest.clone()].into_iter().collect();
    registry.uninstall(&first.digest, &referenced).unwrap();
    registry
        .verify(&second.digest)
        .expect("the referenced package stays");
}

#[test]
fn uninstall_refuses_a_modified_root_and_unrecords_a_missing_one() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();
    std::fs::write(fixture.root(&sample.digest).join("files/LICENSE"), b"GPL\n").unwrap();

    assert!(matches!(
        registry
            .uninstall(&sample.digest, &no_references())
            .unwrap_err(),
        RegistryError::RootInvalid(VerifyError::Modified { .. })
    ));
    assert!(
        fixture.root(&sample.digest).exists(),
        "a modified root is never deleted"
    );
    assert_eq!(registry.state().unwrap().packages().len(), 1);

    std::fs::remove_dir_all(fixture.root(&sample.digest)).unwrap();
    registry
        .uninstall(&sample.digest, &no_references())
        .unwrap();
    assert!(registry.state().unwrap().packages().is_empty());
}

/// Installs `sample` selected, tampers with it through `tamper` and returns
/// the registry state before the removal.
fn tampered(
    fixture: &PluginFixture,
    sample: &Sample,
    tamper: impl FnOnce(&std::path::Path),
) -> package::registry::RegistryState {
    let registry = fixture.open();
    registry
        .install(&InstallRequest {
            select: true,
            ..request(sample)
        })
        .unwrap();
    tamper(&fixture.root(&sample.digest).join("files"));
    registry.state().unwrap()
}

fn assert_removed_as_modified(fixture: &PluginFixture, sample: &Sample, before_generation: u64) {
    let registry = fixture.open();
    let generation = registry
        .remove_modified(&sample.digest, &no_references())
        .unwrap();
    assert_eq!(generation, before_generation + 1, "one commit");
    let state = registry.state().unwrap();
    assert_eq!(state.generation(), generation);
    assert!(state.package(&sample.digest).is_none());
    assert_eq!(state.selected(&sample.identity.id), None);
    assert!(!fixture.root(&sample.digest).exists());
    assert!(names(&fixture.packages())
        .iter()
        .all(|name| !name.starts_with('.')));
}

#[test]
fn remove_modified_removes_a_root_with_changed_content() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let before = tampered(&fixture, &sample, |files| {
        std::fs::write(files.join("LICENSE"), b"GPL\n").unwrap();
    });
    assert_removed_as_modified(&fixture, &sample, before.generation());
}

#[test]
fn remove_modified_removes_a_root_with_an_added_file() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let before = tampered(&fixture, &sample, |files| {
        std::fs::write(files.join("detect/extra.toml"), b"x").unwrap();
        std::fs::create_dir(files.join("surplus")).unwrap();
        std::fs::write(files.join("surplus/inner"), b"x").unwrap();
    });
    assert_removed_as_modified(&fixture, &sample, before.generation());
}

#[test]
fn remove_modified_removes_a_root_with_a_changed_mode() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let before = tampered(&fixture, &sample, |files| {
        set_mode(&files.join("LICENSE"), 0o644);
    });
    assert_removed_as_modified(&fixture, &sample, before.generation());
}

#[test]
fn remove_modified_removes_a_root_with_a_missing_file() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let before = tampered(&fixture, &sample, |files| {
        std::fs::remove_file(files.join("LICENSE")).unwrap();
    });
    assert_removed_as_modified(&fixture, &sample, before.generation());
}

#[test]
fn remove_modified_removes_a_root_with_a_changed_manifest() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let registry = fixture.open();
    registry.install(&request(&sample)).unwrap();
    let manifest = fixture.root(&sample.digest).join("manifest.json");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, format!("{text} ")).unwrap();
    assert!(matches!(
        registry.verify(&sample.digest).unwrap_err(),
        RegistryError::RootInvalid(_)
    ));

    registry
        .remove_modified(&sample.digest, &no_references())
        .unwrap();
    assert!(!fixture.root(&sample.digest).exists());
}

#[test]
fn remove_modified_refuses_a_root_that_verifies_or_is_missing() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();
    let before = registry.state().unwrap();

    assert_eq!(
        registry
            .remove_modified(&sample.digest, &no_references())
            .unwrap_err(),
        RegistryError::RootIntact
    );
    assert_eq!(registry.state().unwrap(), before);
    registry.verify(&sample.digest).unwrap();

    std::fs::remove_dir_all(fixture.root(&sample.digest)).unwrap();
    assert_eq!(
        registry
            .remove_modified(&sample.digest, &no_references())
            .unwrap_err(),
        RegistryError::RootIntact
    );
    assert_eq!(registry.state().unwrap(), before);
}

#[test]
fn remove_modified_refuses_a_retained_root_and_leaves_it_untouched() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let before = tampered(&fixture, &sample, |files| {
        std::fs::write(files.join("LICENSE"), b"GPL\n").unwrap();
    });
    let registry = fixture.open();
    let retained: RetainedDigests = [sample.digest.clone()].into_iter().collect();

    assert_eq!(
        registry
            .remove_modified(&sample.digest, &retained)
            .unwrap_err(),
        RegistryError::StillReferenced
    );
    assert_eq!(registry.state().unwrap(), before);
    assert_eq!(
        std::fs::read(fixture.root(&sample.digest).join("files/LICENSE")).unwrap(),
        b"GPL\n"
    );
}

#[test]
fn remove_modified_refuses_an_unknown_digest() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    assert_eq!(
        registry
            .remove_modified(&sample().digest, &no_references())
            .unwrap_err(),
        RegistryError::NotInstalled
    );
    assert_eq!(registry.state().unwrap().generation(), 0);
}

#[test]
fn remove_modified_only_honors_the_digest_it_names() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let (sample, other) = (sample(), other());
    registry.install(&request(&sample)).unwrap();
    registry.install(&request(&other)).unwrap();
    std::fs::write(fixture.root(&sample.digest).join("files/LICENSE"), b"GPL\n").unwrap();

    registry
        .remove_modified(&sample.digest, &no_references())
        .unwrap();
    let state = registry.state().unwrap();
    assert!(state.package(&other.digest).is_some());
    registry.verify(&other.digest).unwrap();
}

#[test]
fn retained_roots_report_ready_and_typed_incompatible_states() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let (ready, missing, modified, forged) = (
        sample(),
        other(),
        sample_of(&[entry("a.toml", b"a", false)], "acme.a", "1.0.0"),
        sample_of(&[entry("b.toml", b"b", false)], "acme.b", "1.0.0"),
    );
    for sample in [&ready, &missing, &modified, &forged] {
        registry.install(&request(sample)).unwrap();
    }
    let unregistered = sample_of(&[entry("c.toml", b"c", false)], "acme.c", "1.0.0").digest;

    std::fs::remove_dir_all(fixture.root(&missing.digest)).unwrap();
    std::fs::write(fixture.root(&modified.digest).join("files/a.toml"), b"X").unwrap();
    // A self-consistent root whose manifest the registry did not record.
    let manifest = fixture.root(&forged.digest).join("manifest.json");
    std::fs::write(fixture.root(&forged.digest).join("files/b.toml"), b"B").unwrap();
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, text.replace(&sha_hex(b"b"), &sha_hex(b"B"))).unwrap();

    let referenced: RetainedDigests = [
        ready.digest.clone(),
        missing.digest.clone(),
        modified.digest.clone(),
        forged.digest.clone(),
        unregistered.clone(),
    ]
    .into_iter()
    .collect();
    let roots = registry.retained_roots(&referenced).unwrap();
    assert_eq!(roots.len(), 5);
    for root in roots {
        match (&root.digest, root.state) {
            (digest, RetainedState::Ready(verified)) => {
                assert_eq!(digest, &ready.digest);
                assert_eq!(verified.digest(), digest);
            }
            (digest, RetainedState::Incompatible(reason)) if digest == &missing.digest => {
                assert_eq!(reason, IncompatibleReason::RootMissing);
            }
            (digest, RetainedState::Incompatible(reason)) if digest == &modified.digest => {
                assert!(matches!(
                    reason,
                    IncompatibleReason::RootInvalid(VerifyError::Modified { .. })
                ));
            }
            (digest, RetainedState::Incompatible(reason)) if digest == &forged.digest => {
                assert_eq!(
                    reason,
                    IncompatibleReason::RootInvalid(VerifyError::ManifestChanged)
                );
            }
            (digest, RetainedState::Incompatible(reason)) => {
                assert_eq!(digest, &unregistered);
                assert_eq!(reason, IncompatibleReason::NotRegistered);
            }
            (_, other) => panic!("unexpected retained state {other:?}"),
        }
    }
}

fn sha_hex(bytes: &[u8]) -> String {
    sha_digest(bytes)
        .as_str()
        .strip_prefix("sha256:")
        .unwrap()
        .to_owned()
}

#[test]
fn a_recorded_package_with_a_missing_root_is_restored_by_reinstalling() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();
    std::fs::remove_dir_all(fixture.root(&sample.digest)).unwrap();
    assert!(matches!(
        registry.verify(&sample.digest).unwrap_err(),
        RegistryError::RootInvalid(VerifyError::RootMissing)
    ));

    let restored = registry.install(&request(&sample)).unwrap();
    assert_eq!(restored.status, InstallStatus::RootRestored);
    assert_eq!(restored.generation, 1, "the record did not change");
    registry
        .verify(&sample.digest)
        .expect("restored root verifies");
}

#[test]
fn a_recorded_package_with_an_invalid_root_is_never_replaced_by_install() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();
    let license = fixture.root(&sample.digest).join("files/LICENSE");
    std::fs::write(&license, b"GPL\n").unwrap();

    assert!(matches!(
        registry.install(&request(&sample)).unwrap_err(),
        RegistryError::RootInvalid(VerifyError::Modified { .. })
    ));
    assert_eq!(std::fs::read(&license).unwrap(), b"GPL\n");
}

#[test]
fn a_crash_between_staging_and_publication_converges() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    let hex = sha_hex_of_digest(&sample.digest);
    // Partly filled staging directory, no root, no record.
    let staging = fixture.packages().join(format!(".staging-{hex}"));
    std::fs::create_dir_all(staging.join("files/detect")).unwrap();
    std::fs::write(staging.join("files/runtime.toml"), b"partial").unwrap();
    set_mode(&staging, 0o700);
    std::fs::create_dir(fixture.packages().join(".collect-00112233")).unwrap();
    set_mode(&fixture.packages().join(".collect-00112233"), 0o700);

    let report = registry.install(&request(&sample)).unwrap();
    assert_eq!(report.status, InstallStatus::Installed);
    assert_eq!(names(&fixture.packages()), vec![hex]);
    registry.verify(&sample.digest).unwrap();
}

fn sha_hex_of_digest(digest: &PackageDigest) -> String {
    digest.as_str().strip_prefix("sha256:").unwrap().to_owned()
}

#[test]
fn a_crash_between_publication_and_the_record_converges() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    // The root was published, the record write never happened.
    let packages = TrustedDir::open_absolute(fixture.packages(), 0o700).unwrap();
    install_archive(
        &packages,
        &read_archive(&sample.bytes, &Limits::DEFAULT).unwrap(),
        &Limits::DEFAULT,
    )
    .unwrap();

    assert_eq!(registry.state().unwrap().generation(), 0);
    assert_eq!(
        registry.unregistered_roots().unwrap(),
        vec![sample.digest.clone()]
    );
    assert!(matches!(
        registry.verify(&sample.digest).unwrap_err(),
        RegistryError::NotInstalled
    ));

    let report = registry.install(&request(&sample)).unwrap();
    assert_eq!(report.status, InstallStatus::Installed);
    assert!(registry.unregistered_roots().unwrap().is_empty());
    registry.verify(&sample.digest).unwrap();
}

#[test]
fn a_crash_between_unrecording_and_deleting_leaves_an_adoptable_root() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();
    // Registry replaced without the package; the root deletion never ran.
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.registry_file()).unwrap()).unwrap();
    value["generation"] = 2.into();
    value["packages"] = serde_json::json!([]);
    std::fs::write(fixture.registry_file(), serde_json::to_vec(&value).unwrap()).unwrap();

    assert!(registry.state().unwrap().packages().is_empty());
    assert_eq!(
        registry.unregistered_roots().unwrap(),
        vec![sample.digest.clone()]
    );
    assert!(
        fixture.root(&sample.digest).exists(),
        "reading and listing never delete content"
    );
    // A mutation of something else does not delete it either.
    let second = other();
    registry.install(&request(&second)).unwrap();
    assert_eq!(
        registry.unregistered_roots().unwrap(),
        vec![sample.digest.clone()]
    );

    registry.install(&request(&sample)).unwrap();
    assert!(registry.unregistered_roots().unwrap().is_empty());
}

#[test]
fn a_stale_record_temporary_does_not_block_the_next_write() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.state().unwrap();
    std::fs::write(fixture.plugins().join("registry.json.tmp"), b"half-written").unwrap();
    set_mode(&fixture.plugins().join("registry.json.tmp"), 0o600);

    registry.install(&request(&sample)).unwrap();
    assert!(!fixture.plugins().join("registry.json.tmp").exists());
    assert_eq!(registry.state().unwrap().generation(), 1);
}

#[test]
fn a_corrupt_record_is_reported_and_never_overwritten() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();
    let good = std::fs::read(fixture.registry_file()).unwrap();
    let digest = sample.digest.as_str();

    let corruptions: Vec<(&str, Vec<u8>)> = vec![
        ("garbage", b"{ not json".to_vec()),
        ("empty", Vec::new()),
        (
            "unknown field",
            String::from_utf8(good.clone())
                .unwrap()
                .replace("\"generation\"", "\"surplus\":1,\"generation\"")
                .into_bytes(),
        ),
        ("duplicate package", duplicate_package(&good)),
        (
            "selected without package",
            String::from_utf8(good.clone())
                .unwrap()
                .replace(
                    "\"selected\":{}",
                    &format!("\"selected\":{{\"acme.ghost\":\"{digest}\"}}"),
                )
                .into_bytes(),
        ),
        ("oversized", vec![b' '; 2 * 1024 * 1024]),
    ];
    for (label, bytes) in corruptions {
        std::fs::write(fixture.registry_file(), &bytes).unwrap();
        assert_eq!(
            registry.state().unwrap_err(),
            RegistryError::Corrupt,
            "{label}"
        );
        assert_eq!(
            registry.install(&request(&other())).unwrap_err(),
            RegistryError::Corrupt,
            "{label}"
        );
        assert_eq!(
            registry
                .uninstall(&sample.digest, &no_references())
                .unwrap_err(),
            RegistryError::Corrupt
        );
        assert_eq!(
            std::fs::read(fixture.registry_file()).unwrap(),
            bytes,
            "{label}"
        );
    }
    assert_eq!(
        names(&fixture.packages()).len(),
        1,
        "no root was created from a corrupt record"
    );
}

fn duplicate_package(good: &[u8]) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(good).unwrap();
    let first = value["packages"][0].clone();
    value["packages"].as_array_mut().unwrap().push(first);
    serde_json::to_vec(&value).unwrap()
}

#[test]
fn an_unsupported_schema_version_is_reported() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.install(&request(&sample())).unwrap();
    let text = std::fs::read_to_string(fixture.registry_file()).unwrap();
    std::fs::write(
        fixture.registry_file(),
        text.replace("\"schema\":1", "\"schema\":2"),
    )
    .unwrap();

    assert_eq!(
        registry.state().unwrap_err(),
        RegistryError::UnsupportedSchema
    );
}

#[test]
fn registry_files_and_directories_must_be_owner_private_regular_entries() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    registry.install(&request(&sample())).unwrap();

    set_mode(&fixture.registry_file(), 0o644);
    assert_eq!(registry.state().unwrap_err(), RegistryError::Unsafe);
    set_mode(&fixture.registry_file(), 0o600);
    registry.state().unwrap();

    let moved = fixture.base.join("record");
    std::fs::rename(fixture.registry_file(), &moved).unwrap();
    symlink(&moved, fixture.registry_file()).unwrap();
    assert_eq!(registry.state().unwrap_err(), RegistryError::Unsafe);
    std::fs::remove_file(fixture.registry_file()).unwrap();
    std::fs::rename(&moved, fixture.registry_file()).unwrap();

    set_mode(&fixture.plugins(), 0o755);
    assert_eq!(
        Registry::open_at(&fixture.plugins(), Limits::DEFAULT).unwrap_err(),
        RegistryError::Unsafe
    );
    set_mode(&fixture.plugins(), 0o700);

    set_mode(&fixture.packages(), 0o755);
    assert_eq!(
        Registry::open_at(&fixture.plugins(), Limits::DEFAULT).unwrap_err(),
        RegistryError::Unsafe
    );
    set_mode(&fixture.packages(), 0o700);
    Registry::open_at(&fixture.plugins(), Limits::DEFAULT).expect("restored modes open");
}

#[test]
fn a_held_lock_makes_mutations_fail_with_busy_but_reads_proceed() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry.install(&request(&sample)).unwrap();

    let root = TrustedDir::open_absolute(fixture.plugins(), 0o700).unwrap();
    let held = root.acquire_lock("registry.lock", 0o600).unwrap();
    assert_eq!(
        registry.install(&request(&other())).unwrap_err(),
        RegistryError::Busy
    );
    assert_eq!(
        registry.set_enabled(&sample.digest, false).unwrap_err(),
        RegistryError::Busy
    );
    assert_eq!(
        registry.select(&sample.digest).unwrap_err(),
        RegistryError::Busy
    );
    assert_eq!(
        registry
            .uninstall(&sample.digest, &no_references())
            .unwrap_err(),
        RegistryError::Busy
    );
    assert_eq!(registry.state().unwrap().generation(), 1);
    drop(held);
    registry.set_enabled(&sample.digest, false).unwrap();
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
    const WRITERS: usize = 8;
    let fixture = PluginFixture::new();
    fixture.open();
    let samples: Vec<Sample> = (0..WRITERS)
        .map(|index| {
            sample_of(
                &[entry(
                    "runtime.toml",
                    format!("writer {index}\n").as_bytes(),
                    false,
                )],
                &format!("acme.writer{index}"),
                "1.0.0",
            )
        })
        .collect();
    let contended = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for sample in &samples {
            let (fixture, contended) = (&fixture, &contended);
            scope.spawn(move || {
                let registry = fixture.open();
                let report = until_unlocked(|| {
                    registry.install(&request(sample)).inspect_err(|error| {
                        if *error == RegistryError::Busy {
                            contended.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                });
                assert_eq!(report.status, InstallStatus::Installed);
                until_unlocked(|| registry.set_enabled(&sample.digest, false));
            });
        }
    });

    let registry = fixture.open();
    let state = registry.state().unwrap();
    assert_eq!(state.packages().len(), WRITERS);
    assert_eq!(state.generation(), u64::try_from(2 * WRITERS).unwrap());
    for sample in &samples {
        let record = state.package(&sample.digest).expect("no install was lost");
        assert!(!record.enabled(), "no enable/disable update was lost");
        registry.verify(&sample.digest).unwrap();
    }
    assert!(names(&fixture.packages())
        .iter()
        .all(|name| !name.starts_with('.')));
}

#[test]
fn concurrent_installs_of_one_archive_install_it_once() {
    const WRITERS: usize = 8;
    let fixture = PluginFixture::new();
    fixture.open();
    let sample = sample();
    let installed = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..WRITERS {
            let (fixture, sample, installed) = (&fixture, &sample, &installed);
            scope.spawn(move || {
                let registry = fixture.open();
                let report = until_unlocked(|| registry.install(&request(sample)));
                if report.status == InstallStatus::Installed {
                    installed.fetch_add(1, Ordering::Relaxed);
                } else {
                    assert_eq!(report.status, InstallStatus::AlreadyInstalled);
                }
            });
        }
    });

    assert_eq!(installed.load(Ordering::Relaxed), 1);
    let state = fixture.open().state().unwrap();
    assert_eq!(state.generation(), 1);
    assert_eq!(state.packages().len(), 1);
}

#[test]
fn install_surfaces_a_staging_residue_that_is_not_a_directory() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    std::fs::write(fixture.packages().join(".staging-evil"), b"x").unwrap();

    assert_eq!(
        registry.install(&request(&sample())).unwrap_err(),
        RegistryError::Install(InstallError::UnsafeResidue)
    );
    assert_eq!(registry.state().unwrap().generation(), 0);
}

#[test]
fn the_package_cap_is_enforced() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    // The cap check precedes extraction; fill the record directly.
    let first = sample();
    registry.install(&request(&first)).unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.registry_file()).unwrap()).unwrap();
    let template = value["packages"][0].clone();
    let packages = value["packages"].as_array_mut().unwrap();
    packages.clear();
    for index in 0..package::registry::MAX_PACKAGES {
        let mut record = template.clone();
        record["digest"] = format!("sha256:{index:064x}").into();
        record["identity"]["id"] = format!("filler.p{index}").into();
        packages.push(record);
    }
    std::fs::write(fixture.registry_file(), serde_json::to_vec(&value).unwrap()).unwrap();

    assert_eq!(
        registry.install(&request(&other())).unwrap_err(),
        RegistryError::TooManyPackages
    );
}

#[test]
fn a_no_op_enable_or_select_still_verifies_the_root() {
    let fixture = PluginFixture::new();
    let registry = fixture.open();
    let sample = sample();
    registry
        .install(&InstallRequest {
            select: true,
            ..request(&sample)
        })
        .unwrap();
    // Already enabled and selected: valid no-ops keep the generation.
    assert_eq!(registry.set_enabled(&sample.digest, true).unwrap(), 1);
    assert_eq!(registry.select(&sample.digest).unwrap(), 1);

    let license = fixture.root(&sample.digest).join("files/LICENSE");
    std::fs::write(&license, b"GPL\n").unwrap();
    assert!(matches!(
        registry.set_enabled(&sample.digest, true).unwrap_err(),
        RegistryError::RootInvalid(VerifyError::Modified { .. })
    ));
    assert!(matches!(
        registry.select(&sample.digest).unwrap_err(),
        RegistryError::RootInvalid(VerifyError::Modified { .. })
    ));

    std::fs::remove_dir_all(fixture.root(&sample.digest)).unwrap();
    assert!(matches!(
        registry.set_enabled(&sample.digest, true).unwrap_err(),
        RegistryError::RootInvalid(VerifyError::RootMissing)
    ));
    assert!(matches!(
        registry.select(&sample.digest).unwrap_err(),
        RegistryError::RootInvalid(VerifyError::RootMissing)
    ));
    assert_eq!(registry.state().unwrap().generation(), 1);
}

#[test]
fn remove_modified_of_a_hard_linked_root_never_wedges_the_registry() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let other = other();
    let registry = fixture.open();
    registry.install(&request(&sample)).unwrap();
    let files = fixture.root(&sample.digest).join("files");
    let alias = fixture.base.join("alias");
    std::fs::hard_link(files.join("LICENSE"), &alias).unwrap();

    let outcome = registry.remove_modified(&sample.digest, &no_references());
    assert!(
        matches!(outcome, Ok(_) | Err(RegistryError::RemovalIncomplete)),
        "unexpected outcome {outcome:?}"
    );
    assert!(registry.state().unwrap().package(&sample.digest).is_none());

    // A later mutation still runs: residue the removal could not delete does
    // not fail every transaction.
    registry.install(&request(&other)).unwrap();
    registry.uninstall(&other.digest, &no_references()).unwrap();
}

#[test]
fn remove_modified_of_an_unreadable_directory_never_wedges_the_registry() {
    let fixture = PluginFixture::new();
    let sample = sample();
    let other = other();
    let registry = fixture.open();
    registry.install(&request(&sample)).unwrap();
    let directory = fixture.root(&sample.digest).join("files/detect");
    set_mode(&directory, 0o000);

    let outcome = registry.remove_modified(&sample.digest, &no_references());
    if directory.exists() {
        set_mode(&directory, 0o700);
    }
    assert!(
        matches!(outcome, Ok(_) | Err(RegistryError::RemovalIncomplete)),
        "unexpected outcome {outcome:?}"
    );
    registry.install(&request(&other)).unwrap();
}
