//! Per-file manifest of an installed package root.
//!
//! The manifest records the path, size, SHA-256 and executable bit of every
//! file of the archive, plus the digest of the archive it came from. Install
//! writes it next to the extracted tree; verification parses it back and treats
//! it as untrusted input, applying the same path and limit rules as the archive
//! reader.

// Rust guideline compliant 2026-10-04

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

use protocol::PackageDigest;
use serde::{Deserialize, Serialize};

use crate::archive::{validate_path, PathIndex};
use crate::hash::sha256_hex;
use crate::layout::SHA256_HEX_CHARS;
use crate::limits::Limits;
use crate::VerifiedArchive;

/// Schema version of the manifest this crate reads and writes.
pub(crate) const MANIFEST_SCHEMA: u32 = 1;

/// SHA-256 of the exact manifest bytes, as lowercase hexadecimal.
///
/// The registry records it at install time so a later verification is anchored
/// to the registry and not only to a file stored beside the content.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct ManifestDigest(String);

/// The text is not 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManifestDigestError;

impl Display for ManifestDigestError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("manifest digest must be 64 lowercase hexadecimal characters")
    }
}

impl std::error::Error for ManifestDigestError {}

impl ManifestDigest {
    /// Parses a manifest digest.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestDigestError`] unless `text` is 64 lowercase
    /// hexadecimal characters.
    pub fn parse(text: &str) -> Result<Self, ManifestDigestError> {
        if is_sha256_hex(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(ManifestDigestError)
        }
    }

    /// Digest of the exact manifest bytes.
    pub(crate) fn of(bytes: &[u8]) -> Self {
        Self(sha256_hex(bytes))
    }

    /// The lowercase hexadecimal text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for ManifestDigest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ManifestDigest {
    type Error = ManifestDigestError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if is_sha256_hex(&value) {
            Ok(Self(value))
        } else {
            Err(ManifestDigestError)
        }
    }
}

impl From<ManifestDigest> for String {
    fn from(value: ManifestDigest) -> Self {
        value.0
    }
}

fn is_sha256_hex(text: &str) -> bool {
    text.len() == SHA256_HEX_CHARS
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One file of a package as the manifest records it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFile {
    /// Relative path using `/` separators, valid under the archive path rules.
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// SHA-256 of the file contents, lowercase hexadecimal.
    pub sha256: String,
    /// Whether the extracted file carries the executable bit.
    pub executable: bool,
}

/// The manifest document stored as `manifest.json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub(crate) schema: u32,
    pub(crate) archive: PackageDigest,
    pub(crate) files: Vec<ManifestFile>,
}

/// A manifest breaks the schema, path, order or limit rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManifestInvalid;

impl Manifest {
    /// Builds the manifest of a verified archive.
    pub(crate) fn from_archive(archive: &VerifiedArchive) -> Self {
        Self {
            schema: MANIFEST_SCHEMA,
            archive: archive.digest().clone(),
            files: archive
                .entries()
                .iter()
                .map(|entry| ManifestFile {
                    path: entry.path.clone(),
                    size: u64::try_from(entry.contents.len()).unwrap_or(u64::MAX),
                    sha256: sha256_hex(&entry.contents),
                    executable: entry.executable,
                })
                .collect(),
        }
    }

    /// Canonical bytes written to disk.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_cause| {
            unreachable!("a manifest of strings, integers and booleans always serializes")
        })
    }

    /// Parses manifest bytes read from disk and validates them under `limits`.
    pub(crate) fn from_bytes(bytes: &[u8], limits: &Limits) -> Result<Self, ManifestInvalid> {
        let manifest: Self = serde_json::from_slice(bytes).map_err(|_cause| ManifestInvalid)?;
        manifest.validate(limits)?;
        Ok(manifest)
    }

    /// Checks schema, file count, paths, order, sizes and digests.
    pub(crate) fn validate(&self, limits: &Limits) -> Result<(), ManifestInvalid> {
        if self.schema != MANIFEST_SCHEMA || self.files.len() > limits.max_files {
            return Err(ManifestInvalid);
        }
        let mut index = PathIndex::default();
        let mut total = 0_u64;
        for file in &self.files {
            validate_path(file.path.as_bytes(), limits).map_err(|_cause| ManifestInvalid)?;
            index.insert(&file.path).map_err(|_cause| ManifestInvalid)?;
            if file.size > limits.max_file_bytes || !is_sha256_hex(&file.sha256) {
                return Err(ManifestInvalid);
            }
            total = total.checked_add(file.size).ok_or(ManifestInvalid)?;
        }
        if total > limits.max_expanded_bytes
            || !self
                .files
                .windows(2)
                .all(|pair| pair[0].path < pair[1].path)
        {
            return Err(ManifestInvalid);
        }
        Ok(())
    }

    /// The file for `path`, with its position, by binary search.
    pub(crate) fn find(&self, path: &str) -> Option<(usize, &ManifestFile)> {
        self.files
            .binary_search_by(|file| file.path.as_str().cmp(path))
            .ok()
            .map(|index| (index, &self.files[index]))
    }
}

/// Directory structure implied by the manifest paths.
#[derive(Debug, Default)]
pub(crate) struct Tree {
    /// File name to manifest position.
    pub(crate) files: BTreeMap<String, usize>,
    /// Subdirectory name to subtree.
    pub(crate) directories: BTreeMap<String, Tree>,
}

impl Tree {
    /// Builds the tree of an already validated manifest.
    pub(crate) fn of(manifest: &Manifest) -> Self {
        let mut root = Self::default();
        for (position, file) in manifest.files.iter().enumerate() {
            let mut node = &mut root;
            let mut segments = file.path.split('/').peekable();
            while let Some(segment) = segments.next() {
                if segments.peek().is_some() {
                    node = node.directories.entry(segment.to_owned()).or_default();
                } else {
                    node.files.insert(segment.to_owned(), position);
                }
            }
        }
        root
    }

    /// Number of direct children.
    pub(crate) fn child_count(&self) -> usize {
        self.files.len() + self.directories.len()
    }
}
