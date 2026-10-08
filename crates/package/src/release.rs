//! Release block and per-platform attestation records of the signed catalog.
//!
//! The release block names the core release the catalog belongs to: its
//! version, the source commit and the digest of the canonical binary set built
//! for each target. Each catalog entry carries one attestation digest per
//! supported platform. This module only validates and exposes those values;
//! it does not compute the digests, fetch the attested files or decide what a
//! caller does with them.
//!
//! The format is described in `docs/knowledge/concepts/runtime-catalog.md`.

// Rust guideline compliant 2026-10-08

use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};

use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::catalog::{decode_hex, valid_platform, CatalogEntryRejection, CatalogError};

/// Largest accepted number of binary sets in a release block: 16.
///
/// One set per release target; the shipped matrix is a handful of targets, so
/// this leaves headroom while bounding the uniqueness check on unauthenticated
/// input. Lowering it below the real target count makes valid catalogs fail.
pub const MAX_BINARY_SETS: usize = 16;

/// Largest accepted number of attestations in one entry: 16.
///
/// An entry has exactly one attestation per platform, so this equals
/// [`crate::MAX_PLATFORMS_PER_ENTRY`]; raising only one of the two makes the
/// other the effective ceiling.
pub const MAX_ATTESTATIONS_PER_ENTRY: usize = 16;

/// Text that starts every digest string.
const DIGEST_PREFIX: &str = "sha256:";

/// Length of a SHA-256 digest in bytes.
const SHA256_BYTES: usize = 32;

/// Length of a Git commit id in bytes (SHA-1 object name).
const COMMIT_BYTES: usize = 20;

/// A SHA-256 digest in the form `sha256:<64 lowercase hex>`.
///
/// The catalog treats the digest as opaque: what was hashed is defined by the
/// producer of the release (binary set or attestation file).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Parses `sha256:` followed by exactly 64 lowercase hex characters.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Schema`] for any other text.
    pub fn parse(text: &str) -> Result<Self, CatalogError> {
        text.strip_prefix(DIGEST_PREFIX)
            .and_then(decode_hex::<SHA256_BYTES>)
            .map(|_bytes| Self(text.to_owned()))
            .ok_or(CatalogError::Schema)
    }

    /// The `sha256:<64 hex>` form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for Sha256Digest {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(|_cause| serde::de::Error::custom("invalid sha256 digest"))
    }
}

/// The digest of the canonical binary set built for one target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BinarySet {
    /// Target triple name, lowercase `[a-z0-9._-]`.
    pub target: String,
    /// Digest of the CLI, daemon and session worker binaries for `target`.
    pub digest: Sha256Digest,
}

/// The attestation digest of one platform of an entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attestation {
    /// Platform the attestation covers, lowercase `[a-z0-9._-]`.
    pub platform: String,
    /// Digest of the attestation file bytes.
    pub digest: Sha256Digest,
}

/// The core release a catalog belongs to.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// Core version as `X.Y.Z`, without pre-release or build metadata.
    pub version: String,
    /// Source commit: 40 lowercase hex characters.
    pub commit: String,
    /// One binary set per target, at least one.
    pub binary_sets: Vec<BinarySet>,
}

/// Why the release block was rejected.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ReleaseRejection {
    /// The version is not `X.Y.Z` semver without pre-release or build parts.
    #[error("release version is not a plain X.Y.Z version")]
    Version,
    /// The commit is not 40 lowercase hex characters.
    #[error("release commit is not 40 lowercase hex characters")]
    Commit,
    /// The binary set list is empty or names an invalid target.
    #[error("release binary set list is invalid")]
    BinarySets,
    /// Two binary sets name the same target.
    #[error("release lists a target twice")]
    DuplicateTarget,
}

/// A release block whose fields passed validation.
#[derive(Clone, Debug)]
pub struct VerifiedRelease {
    version: Version,
    commit: String,
    binary_sets: Vec<BinarySet>,
}

impl VerifiedRelease {
    /// Core version the catalog was released with.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Source commit, 40 lowercase hex characters.
    #[must_use]
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// Binary sets in catalog order.
    #[must_use]
    pub fn binary_sets(&self) -> &[BinarySet] {
        &self.binary_sets
    }

    /// Digest of the binary set built for `target`.
    #[must_use]
    pub fn binary_set_digest(&self, target: &str) -> Option<&Sha256Digest> {
        self.binary_sets
            .iter()
            .find(|set| set.target == target)
            .map(|set| &set.digest)
    }
}

/// Validates the release block.
pub(crate) fn validate_release(release: &Release) -> Result<VerifiedRelease, ReleaseRejection> {
    let version = Version::parse(&release.version)
        .ok()
        .filter(|parsed| {
            parsed.pre.is_empty()
                && parsed.build.is_empty()
                && parsed.to_string() == release.version
        })
        .ok_or(ReleaseRejection::Version)?;
    if decode_hex::<COMMIT_BYTES>(&release.commit).is_none() {
        return Err(ReleaseRejection::Commit);
    }
    if release.binary_sets.is_empty()
        || release.binary_sets.len() > MAX_BINARY_SETS
        || !release
            .binary_sets
            .iter()
            .all(|set| valid_platform(&set.target))
    {
        return Err(ReleaseRejection::BinarySets);
    }
    let mut targets = BTreeSet::new();
    if !release
        .binary_sets
        .iter()
        .all(|set| targets.insert(set.target.as_str()))
    {
        return Err(ReleaseRejection::DuplicateTarget);
    }
    Ok(VerifiedRelease {
        version,
        commit: release.commit.clone(),
        binary_sets: release.binary_sets.clone(),
    })
}

/// Checks that `attestations` hold exactly one record per entry platform and
/// that every platform has a binary set in the release.
pub(crate) fn check_attestations(
    platforms: &[String],
    attestations: &[Attestation],
    release: &VerifiedRelease,
) -> Result<(), CatalogEntryRejection> {
    let mut attested = BTreeSet::new();
    if attestations.is_empty()
        || attestations.len() > MAX_ATTESTATIONS_PER_ENTRY
        || !attestations
            .iter()
            .all(|record| valid_platform(&record.platform))
        || !attestations
            .iter()
            .all(|record| attested.insert(record.platform.as_str()))
    {
        return Err(CatalogEntryRejection::Attestations);
    }
    if !attested
        .iter()
        .all(|platform| platforms.iter().any(|candidate| candidate == platform))
    {
        return Err(CatalogEntryRejection::AttestationPlatform);
    }
    if !platforms
        .iter()
        .all(|platform| attested.contains(platform.as_str()))
    {
        return Err(CatalogEntryRejection::MissingAttestation);
    }
    if !platforms
        .iter()
        .all(|platform| release.binary_set_digest(platform).is_some())
    {
        return Err(CatalogEntryRejection::NoBinarySet);
    }
    Ok(())
}
