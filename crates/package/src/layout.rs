//! Names, modes and bounds of the on-disk package root layout.
//!
//! One package root is `packages/<hex>/` holding the extracted tree below
//! `files/` and the per-file manifest beside it as `manifest.json`. The
//! manifest lives outside `files/` because every ASCII path is a valid archive
//! path, so no name inside the tree could be reserved for it.

// Rust guideline compliant 2026-10-04

use protocol::PackageDigest;

/// Name of the per-file manifest inside a package root.
pub(crate) const MANIFEST_NAME: &str = "manifest.json";

/// Name of the directory that holds the extracted archive tree.
pub(crate) const FILES_DIR: &str = "files";

/// Name prefix of a staging directory that is still being filled.
///
/// The prefix starts with `.`, which a content-addressed root name (lowercase
/// hex) never does, so a staging directory cannot be mistaken for a root.
pub(crate) const STAGING_PREFIX: &str = ".staging-";

/// Name prefix the platform quarantine uses while a residue is deleted.
pub(crate) const COLLECT_PREFIX: &str = ".collect-";

/// Mode of every directory the package store creates.
pub(crate) const DIRECTORY_MODE: u32 = 0o700;

/// Mode of a non-executable extracted file and of the manifest.
pub(crate) const FILE_MODE: u32 = 0o600;

/// Mode of an extracted file whose canonical archive mode is executable.
pub(crate) const EXECUTABLE_MODE: u32 = 0o700;

/// Largest accepted manifest: 256 KiB.
///
/// A manifest lists at most [`crate::MAX_FILES`] entries of roughly 250 bytes
/// (a path of at most [`crate::MAX_PATH_BYTES`] bytes, a 64-character digest
/// and the size and mode fields), about 125 KiB at the file limit. The ceiling
/// leaves room for formatting changes while keeping the buffer read before
/// parsing small.
pub(crate) const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Length of the lowercase hexadecimal SHA-256 digest text.
pub(crate) const SHA256_HEX_CHARS: usize = 64;

/// Prefix of the digest text that precedes the hexadecimal characters.
const DIGEST_PREFIX: &str = "sha256:";

/// Returns the lowercase hexadecimal part of `digest`, the name of its root.
pub(crate) fn digest_hex(digest: &PackageDigest) -> &str {
    digest
        .as_str()
        .strip_prefix(DIGEST_PREFIX)
        .unwrap_or_else(|| unreachable!("a validated package digest starts with `sha256:`"))
}
