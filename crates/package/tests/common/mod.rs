// Rust guideline compliant 2026-10-04

//! Fixtures shared by the install and registry integration tests.
//!
//! Tests tamper with installed roots through ordinary path-based `std::fs`
//! calls; the code under test never does.

#![allow(dead_code, reason = "each test binary uses a subset of the helpers")]

use std::path::{Path, PathBuf};

use package::{build_archive, read_archive, ArchiveEntry, Limits, VerifiedArchive};
use pohunek_platform::filesystem::TrustedDir;

/// Exact mode of the store directories.
pub(crate) const PRIVATE_DIRECTORY: u32 = 0o700;

pub(crate) fn entry(path: &str, contents: &[u8], executable: bool) -> ArchiveEntry {
    ArchiveEntry {
        path: path.to_owned(),
        contents: contents.to_vec(),
        executable,
    }
}

/// Four files in two directories, one executable.
pub(crate) fn sample_entries() -> Vec<ArchiveEntry> {
    vec![
        entry("runtime.toml", b"schema = 1\n", false),
        entry("detect/default.toml", b"[detect]\n", false),
        entry("integration/assets/hook.sh", b"#!/bin/sh\n", true),
        entry("LICENSE", b"MIT\n", false),
    ]
}

pub(crate) fn archive_bytes(entries: &[ArchiveEntry]) -> Vec<u8> {
    build_archive(entries, &Limits::DEFAULT).expect("sample entries build")
}

pub(crate) fn verified(entries: &[ArchiveEntry]) -> VerifiedArchive {
    read_archive(&archive_bytes(entries), &Limits::DEFAULT).expect("built archive reads")
}

/// A temporary owner-private directory with an opened `packages` child.
pub(crate) struct Fixture {
    _dir: tempfile::TempDir,
    pub(crate) base: PathBuf,
    pub(crate) packages: TrustedDir,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let dir = pohunek_test_support::tempdir().expect("private fixture directory");
        let base = dir.path().to_path_buf();
        let packages =
            TrustedDir::open_or_create_absolute(base.join("packages"), PRIVATE_DIRECTORY)
                .expect("open packages directory");
        Self {
            _dir: dir,
            base,
            packages,
        }
    }

    pub(crate) fn packages_path(&self) -> PathBuf {
        self.base.join("packages")
    }

    /// Path of the extracted tree of the root named by `digest`.
    pub(crate) fn files_path(&self, digest: &package::PackageDigest) -> PathBuf {
        self.root_path(digest).join("files")
    }

    pub(crate) fn root_path(&self, digest: &package::PackageDigest) -> PathBuf {
        let hex = digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("digest prefix");
        self.packages_path().join(hex)
    }
}

pub(crate) fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::symlink_metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777
}

pub(crate) fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A temporary directory whose `plugins` child is the registry root.
pub(crate) struct PluginFixture {
    _dir: tempfile::TempDir,
    pub(crate) base: PathBuf,
}

impl PluginFixture {
    pub(crate) fn new() -> Self {
        let dir = pohunek_test_support::tempdir().expect("private fixture directory");
        let base = dir.path().to_path_buf();
        Self { _dir: dir, base }
    }

    pub(crate) fn plugins(&self) -> PathBuf {
        self.base.join("plugins")
    }

    pub(crate) fn packages(&self) -> PathBuf {
        self.plugins().join("packages")
    }

    pub(crate) fn registry_file(&self) -> PathBuf {
        self.plugins().join("registry.json")
    }

    pub(crate) fn root(&self, digest: &package::PackageDigest) -> PathBuf {
        let hex = digest
            .as_str()
            .strip_prefix("sha256:")
            .expect("digest prefix");
        self.packages().join(hex)
    }

    pub(crate) fn open(&self) -> package::registry::Registry {
        self.open_with(Limits::DEFAULT)
    }

    pub(crate) fn open_with(&self, limits: Limits) -> package::registry::Registry {
        package::registry::Registry::open_at(&self.plugins(), limits).expect("open registry")
    }
}
