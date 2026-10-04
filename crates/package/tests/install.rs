// Rust guideline compliant 2026-10-04

//! Behavioural tests of descriptor-relative extraction and root verification.

#![cfg(unix)]

mod common;

use std::os::unix::fs::symlink;

use common::{entry, mode_of, sample_entries, set_mode, verified, Fixture};
use package::install::{collect_staging, install_archive, InstallError, InstallOutcome};
use package::verify::{verify_root, VerifyError};
use package::Limits;
use sha2::{Digest as _, Sha256};

fn install(fixture: &Fixture) -> package::install::Installation {
    install_archive(
        &fixture.packages,
        &verified(&sample_entries()),
        &Limits::DEFAULT,
    )
    .expect("install")
}

fn digest() -> package::PackageDigest {
    verified(&sample_entries()).digest().clone()
}

fn verify(fixture: &Fixture) -> Result<package::verify::VerifiedRoot, VerifyError> {
    verify_root(&fixture.packages, &digest(), &Limits::DEFAULT)
}

#[test]
fn install_extracts_the_tree_owner_private() {
    let fixture = Fixture::new();
    let installation = install(&fixture);

    assert_eq!(installation.outcome, InstallOutcome::Created);
    let files = fixture.files_path(&digest());
    assert_eq!(
        std::fs::read(files.join("runtime.toml")).unwrap(),
        b"schema = 1\n"
    );
    assert_eq!(mode_of(&fixture.root_path(&digest())), 0o700);
    assert_eq!(mode_of(&files), 0o700);
    assert_eq!(mode_of(&files.join("detect")), 0o700);
    assert_eq!(mode_of(&files.join("integration/assets")), 0o700);
    assert_eq!(mode_of(&files.join("runtime.toml")), 0o600);
    assert_eq!(mode_of(&files.join("integration/assets/hook.sh")), 0o700);
    assert_eq!(
        mode_of(&fixture.root_path(&digest()).join("manifest.json")),
        0o600
    );
    assert_eq!(installation.root.files().len(), 4);
    assert_eq!(installation.root.files_path(), files);
}

#[test]
fn install_leaves_no_staging_residue() {
    let fixture = Fixture::new();
    install(&fixture);

    let names: Vec<String> = std::fs::read_dir(fixture.packages_path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 1, "only the published root remains: {names:?}");
    assert!(!names[0].starts_with('.'));
}

#[test]
fn reinstalling_the_same_archive_is_idempotent() {
    let fixture = Fixture::new();
    let first = install(&fixture);
    let second = install(&fixture);

    assert_eq!(second.outcome, InstallOutcome::AlreadyPresent);
    assert_eq!(first.root.manifest_digest(), second.root.manifest_digest());
}

#[test]
fn read_file_returns_manifest_checked_bytes() {
    let fixture = Fixture::new();
    let root = install(&fixture).root;

    assert_eq!(
        root.read_file("integration/assets/hook.sh").unwrap(),
        b"#!/bin/sh\n"
    );
    assert_eq!(root.read_file("nope.toml"), Err(VerifyError::UnknownPath));
    assert_eq!(root.read_file("detect"), Err(VerifyError::UnknownPath));

    std::fs::write(fixture.files_path(&digest()).join("LICENSE"), b"GPL\n").unwrap();
    assert!(matches!(
        root.read_file("LICENSE"),
        Err(VerifyError::Modified { .. })
    ));
}

#[test]
fn verification_accepts_an_untouched_root() {
    let fixture = Fixture::new();
    let installed = install(&fixture);

    let verified = verify(&fixture).expect("untouched root verifies");
    assert_eq!(verified.digest(), &digest());
    assert_eq!(verified.manifest_digest(), installed.root.manifest_digest());
}

#[test]
fn verification_reports_a_missing_root() {
    let fixture = Fixture::new();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::RootMissing);
}

#[test]
fn verification_detects_modified_contents_of_equal_size() {
    let fixture = Fixture::new();
    install(&fixture);
    std::fs::write(fixture.files_path(&digest()).join("LICENSE"), b"GPL\n").unwrap();

    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::Modified { .. })
    ));
}

#[test]
fn verification_detects_a_grown_and_a_truncated_file() {
    let fixture = Fixture::new();
    install(&fixture);
    let license = fixture.files_path(&digest()).join("LICENSE");

    std::fs::write(&license, b"MIT\nextra\n").unwrap();
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::Modified { .. })
    ));
    std::fs::write(&license, b"M").unwrap();
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::Modified { .. })
    ));
}

#[test]
fn verification_detects_a_removed_file_and_directory() {
    let fixture = Fixture::new();
    install(&fixture);
    let files = fixture.files_path(&digest());

    std::fs::remove_file(files.join("LICENSE")).unwrap();
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::Missing { entry: Some(_) })
    ));
    std::fs::write(files.join("LICENSE"), b"MIT\n").unwrap();
    set_mode(&files.join("LICENSE"), 0o600);
    verify(&fixture).expect("restored file verifies");

    std::fs::remove_dir_all(files.join("detect")).unwrap();
    assert!(matches!(verify(&fixture), Err(VerifyError::Missing { .. })));
}

#[test]
fn verification_detects_added_files_and_directories() {
    let fixture = Fixture::new();
    install(&fixture);
    let files = fixture.files_path(&digest());

    std::fs::write(files.join("extra.txt"), b"x").unwrap();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::Added);
    std::fs::remove_file(files.join("extra.txt")).unwrap();

    std::fs::create_dir(files.join("detect").join("nested")).unwrap();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::Added);
    std::fs::remove_dir(files.join("detect").join("nested")).unwrap();

    std::fs::write(fixture.root_path(&digest()).join("stray"), b"x").unwrap();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::Added);
}

#[test]
fn verification_detects_wrong_modes() {
    let fixture = Fixture::new();
    install(&fixture);
    let files = fixture.files_path(&digest());

    set_mode(&files.join("LICENSE"), 0o644);
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongMode { entry: Some(_) })
    ));
    set_mode(&files.join("LICENSE"), 0o600);

    set_mode(&files.join("integration/assets/hook.sh"), 0o600);
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongMode { .. })
    ));
    set_mode(&files.join("integration/assets/hook.sh"), 0o700);

    set_mode(&files.join("LICENSE"), 0o700);
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongMode { .. })
    ));
    set_mode(&files.join("LICENSE"), 0o600);

    set_mode(&files.join("detect"), 0o755);
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongMode { entry: None })
    ));
    set_mode(&files.join("detect"), 0o700);

    set_mode(&fixture.root_path(&digest()), 0o750);
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongMode { .. })
    ));
    set_mode(&fixture.root_path(&digest()), 0o700);
    verify(&fixture).expect("restored modes verify");
}

#[test]
fn verification_detects_a_file_replaced_by_a_symlink() {
    let fixture = Fixture::new();
    install(&fixture);
    let license = fixture.files_path(&digest()).join("LICENSE");
    let target = fixture.base.join("outside.txt");
    std::fs::write(&target, b"MIT\n").unwrap();
    std::fs::remove_file(&license).unwrap();
    symlink(&target, &license).unwrap();

    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongType { entry: Some(_) })
    ));
}

#[test]
fn verification_detects_a_directory_replaced_by_a_symlink() {
    let fixture = Fixture::new();
    install(&fixture);
    let files = fixture.files_path(&digest());
    let outside = fixture.base.join("outside");
    std::fs::rename(files.join("detect"), &outside).unwrap();
    symlink(&outside, files.join("detect")).unwrap();

    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongType { .. })
    ));
    // The retained descriptors never followed the link into `outside`.
    assert_eq!(
        std::fs::read(outside.join("default.toml")).unwrap(),
        b"[detect]\n"
    );
}

#[test]
fn verification_detects_a_root_replaced_by_a_symlink() {
    let fixture = Fixture::new();
    install(&fixture);
    let root = fixture.root_path(&digest());
    let moved = fixture.base.join("moved-root");
    std::fs::rename(&root, &moved).unwrap();
    symlink(&moved, &root).unwrap();

    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongType { entry: None })
    ));
}

#[test]
fn verification_detects_a_hard_linked_file() {
    let fixture = Fixture::new();
    install(&fixture);
    let license = fixture.files_path(&digest()).join("LICENSE");
    std::fs::hard_link(&license, fixture.base.join("alias")).unwrap();

    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::Hardlinked { .. })
    ));
}

#[test]
fn verification_detects_a_file_replaced_by_a_directory_or_fifo() {
    let fixture = Fixture::new();
    install(&fixture);
    let license = fixture.files_path(&digest()).join("LICENSE");

    std::fs::remove_file(&license).unwrap();
    std::fs::create_dir(&license).unwrap();
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongType { .. })
    ));
    std::fs::remove_dir(&license).unwrap();

    nix::unistd::mkfifo(
        &license,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .unwrap();
    assert!(matches!(
        verify(&fixture),
        Err(VerifyError::WrongType { .. })
    ));
}

#[test]
fn verification_rejects_a_missing_or_corrupt_manifest() {
    let fixture = Fixture::new();
    install(&fixture);
    let manifest = fixture.root_path(&digest()).join("manifest.json");
    let original = std::fs::read(&manifest).unwrap();

    std::fs::write(&manifest, b"not json").unwrap();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::ManifestInvalid);

    std::fs::write(&manifest, vec![b' '; 300 * 1024]).unwrap();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::ManifestInvalid);

    std::fs::remove_file(&manifest).unwrap();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::ManifestMissing);

    std::fs::write(&manifest, &original).unwrap();
    set_mode(&manifest, 0o600);
    verify(&fixture).expect("restored manifest verifies");
}

#[test]
fn verification_rejects_a_manifest_that_breaks_the_path_rules() {
    let fixture = Fixture::new();
    install(&fixture);
    let manifest = fixture.root_path(&digest()).join("manifest.json");
    let text = std::fs::read_to_string(&manifest).unwrap();

    for hostile in ["../evil", "/abs", "a//b", "bad name"] {
        std::fs::write(
            &manifest,
            text.replace("\"LICENSE\"", &format!("\"{hostile}\"")),
        )
        .unwrap();
        assert_eq!(
            verify(&fixture).unwrap_err(),
            VerifyError::ManifestInvalid,
            "{hostile}"
        );
    }
    std::fs::write(&manifest, text.replace("\"schema\":1", "\"schema\":2")).unwrap();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::ManifestInvalid);
    std::fs::write(
        &manifest,
        text.replace("\"files\"", "\"extra\":1,\"files\""),
    )
    .unwrap();
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::ManifestInvalid);
}

#[test]
fn verification_rejects_a_manifest_naming_another_archive() {
    let fixture = Fixture::new();
    install(&fixture);
    let manifest = fixture.root_path(&digest()).join("manifest.json");
    let other = verified(&[entry("other.toml", b"x", false)]);
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(
        &manifest,
        text.replace(digest().as_str(), other.digest().as_str()),
    )
    .unwrap();

    assert_eq!(
        verify(&fixture).unwrap_err(),
        VerifyError::ManifestArchiveMismatch
    );
}

#[test]
fn install_refuses_to_replace_a_tampered_existing_root() {
    let fixture = Fixture::new();
    install(&fixture);
    std::fs::write(fixture.files_path(&digest()).join("LICENSE"), b"GPL\n").unwrap();

    let error = install_archive(
        &fixture.packages,
        &verified(&sample_entries()),
        &Limits::DEFAULT,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        InstallError::ExistingRootInvalid(VerifyError::Modified { .. })
    ));
    // The tampered file is still there: nothing was overwritten.
    assert_eq!(
        std::fs::read(fixture.files_path(&digest()).join("LICENSE")).unwrap(),
        b"GPL\n"
    );
}

#[test]
fn install_refuses_an_existing_root_that_holds_different_files() {
    let fixture = Fixture::new();
    install(&fixture);
    // A self-consistent root under the same name whose LICENSE differs.
    let root = fixture.root_path(&digest());
    let license = root.join("files/LICENSE");
    std::fs::write(&license, b"GPL\n").unwrap();
    let manifest = root.join("manifest.json");
    let old = hex_digest(b"MIT\n");
    let new = hex_digest(b"GPL\n");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, text.replace(&old, &new)).unwrap();
    verify(&fixture).expect("the forged root is self-consistent");

    let error = install_archive(
        &fixture.packages,
        &verified(&sample_entries()),
        &Limits::DEFAULT,
    )
    .unwrap_err();
    assert_eq!(error, InstallError::RootConflict);
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut text, byte| {
            write!(text, "{byte:02x}").expect("writing to a String");
            text
        })
}

#[test]
fn install_rejects_an_archive_over_the_store_limits_without_residue() {
    let fixture = Fixture::new();
    let tight = Limits {
        max_files: 2,
        ..Limits::DEFAULT
    };
    let error =
        install_archive(&fixture.packages, &verified(&sample_entries()), &tight).unwrap_err();
    assert_eq!(error, InstallError::LimitsExceeded);

    let tight = Limits {
        max_file_bytes: 4,
        ..Limits::DEFAULT
    };
    let error =
        install_archive(&fixture.packages, &verified(&sample_entries()), &tight).unwrap_err();
    assert_eq!(error, InstallError::LimitsExceeded);

    let tight = Limits {
        max_expanded_bytes: 16,
        ..Limits::DEFAULT
    };
    let error =
        install_archive(&fixture.packages, &verified(&sample_entries()), &tight).unwrap_err();
    assert_eq!(error, InstallError::LimitsExceeded);

    assert_eq!(
        std::fs::read_dir(fixture.packages_path()).unwrap().count(),
        0
    );
}

#[test]
fn an_interrupted_install_is_recovered_by_collecting_its_staging_directory() {
    let fixture = Fixture::new();
    let archive = verified(&sample_entries());
    let hex = archive
        .digest()
        .as_str()
        .strip_prefix("sha256:")
        .unwrap()
        .to_owned();
    // The disk state after a crash between staging and publication: a partly
    // filled staging directory and no root.
    let staging = fixture.packages_path().join(format!(".staging-{hex}"));
    std::fs::create_dir_all(staging.join("files/detect")).unwrap();
    std::fs::write(staging.join("files/runtime.toml"), b"partial").unwrap();
    std::fs::create_dir(fixture.packages_path().join(".collect-0123456789abcdef")).unwrap();
    set_mode(&staging, 0o700);

    let error = install_archive(&fixture.packages, &archive, &Limits::DEFAULT).unwrap_err();
    assert_eq!(error, InstallError::StagingExists);
    assert_eq!(verify(&fixture).unwrap_err(), VerifyError::RootMissing);

    assert_eq!(collect_staging(&fixture.packages).unwrap(), 2);
    assert_eq!(
        std::fs::read_dir(fixture.packages_path()).unwrap().count(),
        0
    );
    let installation = install_archive(&fixture.packages, &archive, &Limits::DEFAULT).unwrap();
    assert_eq!(installation.outcome, InstallOutcome::Created);
}

#[test]
fn collecting_ignores_published_roots_and_unrelated_names() {
    let fixture = Fixture::new();
    install(&fixture);
    std::fs::create_dir(fixture.packages_path().join("unrelated")).unwrap();
    set_mode(&fixture.packages_path().join("unrelated"), 0o700);

    assert_eq!(collect_staging(&fixture.packages).unwrap(), 0);
    verify(&fixture).expect("root untouched");
    assert!(fixture.packages_path().join("unrelated").is_dir());
}

#[test]
fn collecting_refuses_a_staging_name_that_is_not_a_directory() {
    let fixture = Fixture::new();
    std::fs::write(fixture.packages_path().join(".staging-file"), b"x").unwrap();
    assert_eq!(
        collect_staging(&fixture.packages).unwrap_err(),
        InstallError::UnsafeResidue
    );
    std::fs::remove_file(fixture.packages_path().join(".staging-file")).unwrap();

    symlink(&fixture.base, fixture.packages_path().join(".staging-link")).unwrap();
    assert_eq!(
        collect_staging(&fixture.packages).unwrap_err(),
        InstallError::UnsafeResidue
    );
    assert!(
        fixture.base.is_dir(),
        "a staging symlink target is never touched"
    );
}

#[test]
fn errors_never_echo_paths_or_contents() {
    let fixture = Fixture::new();
    let archive = verified(&[entry("secret-name.toml", b"secret-contents", false)]);
    let installed = install_archive(&fixture.packages, &archive, &Limits::DEFAULT).unwrap();
    let file = fixture
        .files_path(archive.digest())
        .join("secret-name.toml");

    std::fs::write(&file, b"SECRET-CONTENTS").unwrap();
    let verify_error =
        verify_root(&fixture.packages, archive.digest(), &Limits::DEFAULT).unwrap_err();
    let install_error = install_archive(&fixture.packages, &archive, &Limits::DEFAULT).unwrap_err();
    std::fs::write(
        fixture.files_path(archive.digest()).join("added-secret"),
        b"x",
    )
    .unwrap();
    let added = verify_root(&fixture.packages, archive.digest(), &Limits::DEFAULT).unwrap_err();
    drop(installed);

    for text in [
        verify_error.to_string(),
        format!("{verify_error:?}"),
        install_error.to_string(),
        format!("{install_error:?}"),
        added.to_string(),
        format!("{added:?}"),
    ] {
        let base = fixture.base.to_string_lossy().into_owned();
        for needle in ["secret", "SECRET", base.as_str()] {
            assert!(!text.contains(needle), "`{text}` echoes `{needle}`");
        }
    }
}

/// Environment marker that makes the test below run its descriptor-limited
/// body instead of spawning itself.
const FD_LIMIT_CHILD: &str = "PACKAGE_TEST_FD_LIMIT_CHILD";

/// Descriptor limit of the child: far below the 512 trees x 20 levels an
/// extraction that kept every directory open would need, and above the
/// handful the test harness and a 21-deep path hold.
const FD_LIMIT: u64 = 128;

/// Directory levels below each tree root.
const TREE_DEPTH: usize = 20;

fn deep_tree_entries() -> Vec<package::ArchiveEntry> {
    (0..Limits::DEFAULT.max_files)
        .map(|tree| {
            let levels = vec!["d"; TREE_DEPTH].join("/");
            entry(
                &format!("t{tree:03}/{levels}/f"),
                format!("{tree}").as_bytes(),
                false,
            )
        })
        .collect()
}

#[test]
fn extraction_holds_descriptors_bounded_by_depth_not_directory_count() {
    use rustix::process::{setrlimit, Resource, Rlimit};

    if std::env::var_os(FD_LIMIT_CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "extraction_holds_descriptors_bounded_by_depth_not_directory_count",
                "--nocapture",
            ])
            .env(FD_LIMIT_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("fd-limit-extraction-ok"),
            "child did not run its body:\n{stdout}"
        );
        return;
    }

    let entries = deep_tree_entries();
    let archive = verified(&entries);
    let fixture = Fixture::new();
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(FD_LIMIT),
            maximum: Some(FD_LIMIT),
        },
    )
    .unwrap();

    let installation = install_archive(&fixture.packages, &archive, &Limits::DEFAULT)
        .expect("extraction fits the descriptor limit");
    assert_eq!(installation.root.files().len(), entries.len());
    verify_root(&fixture.packages, archive.digest(), &Limits::DEFAULT).expect("root verifies");
    println!("fd-limit-extraction-ok");
}
