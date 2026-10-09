//! Directory traversal, canonical archive building, and archive reading through public APIs.

// Rust guideline compliant 2026-10-09

use std::fs;
use std::io;
use std::os::unix::fs::{symlink, PermissionsExt as _};

use package::directory::{build_directory_archive, DirectoryError};
use package::{build_archive, read_archive, ArchiveEntry, ArchiveError, EntryRejection, Limits};

fn package_dir() -> tempfile::TempDir {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("detect")).expect("mkdir");
    fs::write(dir.path().join("runtime.toml"), b"schema = 1\n").expect("write");
    fs::write(dir.path().join("detect/default.toml"), b"[detect]\n").expect("write");
    dir
}

#[test]
fn the_archive_equals_the_one_built_from_the_same_files() {
    let dir = package_dir();
    let bytes = build_directory_archive(dir.path(), &Limits::DEFAULT).expect("build");
    let expected = build_archive(
        &[
            ArchiveEntry {
                path: "detect/default.toml".to_owned(),
                contents: b"[detect]\n".to_vec(),
                executable: false,
            },
            ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: b"schema = 1\n".to_vec(),
                executable: false,
            },
        ],
        &Limits::DEFAULT,
    )
    .expect("expected archive");
    assert_eq!(bytes, expected);
}

#[test]
fn executable_bit_is_carried_into_the_archive() {
    let dir = package_dir();
    let script = dir.path().join("hook.sh");
    fs::write(&script, b"#!/bin/sh\n").expect("write");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("chmod");
    let bytes = build_directory_archive(dir.path(), &Limits::DEFAULT).expect("build");
    let archive = read_archive(&bytes, &Limits::DEFAULT).expect("read");
    let hook = archive
        .entries()
        .iter()
        .find(|entry| entry.path == "hook.sh")
        .expect("hook");
    assert!(hook.executable);
}

#[test]
fn symlinks_are_rejected() {
    let dir = package_dir();
    symlink("runtime.toml", dir.path().join("link")).expect("symlink");
    assert_eq!(
        build_directory_archive(dir.path(), &Limits::DEFAULT),
        Err(DirectoryError::UnsupportedFileType)
    );
}

#[test]
fn non_utf8_names_are_rejected() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    let dir = package_dir();
    fs::write(dir.path().join(OsStr::from_bytes(b"bad\xff")), b"x").expect("write");
    assert_eq!(
        build_directory_archive(dir.path(), &Limits::DEFAULT),
        Err(DirectoryError::InvalidPath)
    );
}

#[test]
fn a_missing_directory_is_an_io_error() {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    assert_eq!(
        build_directory_archive(&dir.path().join("absent"), &Limits::DEFAULT),
        Err(DirectoryError::Io {
            kind: io::ErrorKind::NotFound
        })
    );
}

#[test]
fn invalid_package_paths_are_reported_as_archive_errors() {
    let dir = package_dir();
    fs::write(dir.path().join("with space"), b"x").expect("write");
    assert!(matches!(
        build_directory_archive(dir.path(), &Limits::DEFAULT),
        Err(DirectoryError::Archive(_))
    ));
}

#[test]
fn oversized_file_is_rejected_without_buffering_more_than_the_limit() {
    let dir = package_dir();
    let big = fs::File::create(dir.path().join("big")).expect("create");
    big.set_len(Limits::DEFAULT.max_file_bytes + 1)
        .expect("sparse");
    assert!(matches!(
        build_directory_archive(dir.path(), &Limits::DEFAULT),
        Err(DirectoryError::Archive(ArchiveError::Entry {
            reason: EntryRejection::FileTooLarge,
            ..
        }))
    ));
}

#[test]
fn aggregate_size_is_limited_before_files_are_read() {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    for name in ["a", "b", "c"] {
        let file = fs::File::create(dir.path().join(name)).expect("create");
        file.set_len(4096).expect("sparse");
    }
    // Three files cost 3 x (512 + 4096) bytes of tar stream.
    let limits = Limits {
        max_expanded_bytes: 3 * (512 + 4096) - 1,
        ..Limits::DEFAULT
    };
    assert!(matches!(
        build_directory_archive(dir.path(), &limits),
        Err(DirectoryError::Archive(
            ArchiveError::ExpandedTooLarge { .. }
        ))
    ));
}

#[test]
fn file_count_and_path_length_are_limited_during_traversal() {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    for name in ["a", "b", "c"] {
        fs::write(dir.path().join(name), b"x").expect("write");
    }
    let few = Limits {
        max_files: 2,
        ..Limits::DEFAULT
    };
    assert_eq!(
        build_directory_archive(dir.path(), &few),
        Err(DirectoryError::Archive(ArchiveError::TooManyFiles {
            limit: 2
        }))
    );
    let short = Limits {
        max_path_bytes: 0,
        ..Limits::DEFAULT
    };
    assert!(matches!(
        build_directory_archive(dir.path(), &short),
        Err(DirectoryError::Archive(ArchiveError::Entry {
            reason: EntryRejection::PathTooLong,
            ..
        }))
    ));
}

#[test]
fn a_fifo_in_the_tree_is_an_unsupported_file_type() {
    let dir = package_dir();
    nix::unistd::mkfifo(
        &dir.path().join("pipe"),
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("mkfifo");
    assert_eq!(
        build_directory_archive(dir.path(), &Limits::DEFAULT),
        Err(DirectoryError::UnsupportedFileType)
    );
}

#[test]
fn a_symlinked_root_is_refused() {
    let dir = package_dir();
    let holder = pohunek_test_support::tempdir().expect("tempdir");
    let link = holder.path().join("link");
    symlink(dir.path(), &link).expect("symlink");
    assert_eq!(
        build_directory_archive(&link, &Limits::DEFAULT),
        Err(DirectoryError::Changed)
    );
}

#[test]
fn a_directory_with_more_subdirectories_than_the_budget_is_rejected() {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    for index in 0..5 {
        fs::create_dir(dir.path().join(format!("d{index}"))).expect("mkdir");
    }
    let limits = Limits {
        max_files: 3,
        ..Limits::DEFAULT
    };
    assert_eq!(
        build_directory_archive(dir.path(), &limits),
        Err(DirectoryError::Archive(ArchiveError::TooManyFiles {
            limit: 3
        }))
    );
}

#[test]
fn a_chain_of_empty_directories_deeper_than_the_path_limit_is_rejected() {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    let limits = Limits {
        max_path_bytes: 8,
        ..Limits::DEFAULT
    };
    fs::create_dir_all(dir.path().join("aaaa/bbbb/cccc")).expect("mkdir");
    assert!(matches!(
        build_directory_archive(dir.path(), &limits),
        Err(DirectoryError::Archive(ArchiveError::Entry {
            reason: EntryRejection::PathTooLong,
            ..
        }))
    ));
}

#[test]
fn a_multilevel_tree_of_empty_directories_shares_one_directory_budget() {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    // Two directories per level are within a budget of four on their own;
    // the tree as a whole holds six.
    for top in ["a", "b"] {
        for inner in ["x", "y"] {
            fs::create_dir_all(dir.path().join(top).join(inner)).expect("mkdir");
        }
    }
    let limits = Limits {
        max_files: 4,
        ..Limits::DEFAULT
    };
    assert_eq!(
        build_directory_archive(dir.path(), &limits),
        Err(DirectoryError::Archive(ArchiveError::TooManyFiles {
            limit: 4
        }))
    );
    let roomy = Limits {
        max_files: 6,
        ..Limits::DEFAULT
    };
    build_directory_archive(dir.path(), &roomy).expect("six directories fit a budget of six");
}

#[test]
fn a_full_file_budget_still_admits_a_subdirectory() {
    let dir = pohunek_test_support::tempdir().expect("tempdir");
    for index in 0..3 {
        fs::write(dir.path().join(format!("f{index}")), b"x").expect("write");
    }
    fs::create_dir(dir.path().join("empty")).expect("mkdir");
    let limits = Limits {
        max_files: 3,
        ..Limits::DEFAULT
    };
    build_directory_archive(dir.path(), &limits).expect("3 files and an empty directory fit");
}
