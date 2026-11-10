//! Package digests that recorded catalogs revoked.
//!
//! A catalog's `revoked_digests` list outlives the catalog that carried it
//! only if the caller persists it. [`Registry`] stores the accumulated set in
//! `<plugins>/revoked-digests.json`, beside the registry record and under the
//! same advisory lock, so an installed package whose digest a later catalog
//! revoked can be refused without any network access.
//!
//! The set is a file of its own, not a field of `catalog-state.json`: that
//! file keeps its schema, so a release without this module still reads the
//! registry after a rollback (and does not enforce the set).
//!
//! - digests only accumulate, so a later catalog that omits a revoked digest
//!   does not resurrect it;
//! - a missing file is the empty set; a damaged file is reported and never
//!   replaced, so a revocation is not silently forgotten.

// Rust guideline compliant 2026-11-10

use std::collections::BTreeSet;
use std::io::ErrorKind;

use pohunek_platform::filesystem::FsError;
use serde::{Deserialize, Serialize};

use crate::catalog::{VerifiedCatalog, MAX_REVOKED_DIGESTS};
use crate::layout::FILE_MODE;
use crate::registry::{registry_fs, replace_failure, Registry, RegistryError};
use crate::PackageDigest;

/// Name of the revoked digest file inside the plugin root.
const REVOKED_DIGESTS_NAME: &str = "revoked-digests.json";

/// Name of the temporary file the set is written to before it replaces
/// [`REVOKED_DIGESTS_NAME`].
const REVOKED_DIGESTS_TEMP_NAME: &str = "revoked-digests.json.tmp";

/// Schema version of the file this crate reads and writes.
const REVOKED_DIGESTS_SCHEMA: u32 = 1;

/// Largest accepted number of persisted revoked digests: 16 catalogs' worth.
///
/// Each catalog carries up to [`MAX_REVOKED_DIGESTS`] digests and the set only
/// accumulates, so the bound keeps the file small while leaving room for
/// years of withdrawn packages.
pub const MAX_REVOKED_DIGESTS_TOTAL: usize = 16 * MAX_REVOKED_DIGESTS;

/// Largest accepted file: 2 MiB.
///
/// A digest is about 80 bytes with quoting and a separator, so the digest cap
/// needs about 1.3 MiB. The bound is applied before parsing so a corrupt or
/// hostile file cannot grow the buffer.
const MAX_REVOKED_DIGESTS_BYTES: usize = 2 * 1024 * 1024;

/// Every package digest any recorded catalog revoked.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RevokedDigests {
    digests: BTreeSet<PackageDigest>,
}

impl RevokedDigests {
    /// Whether a recorded catalog revoked `digest`.
    #[must_use]
    pub fn contains(&self, digest: &PackageDigest) -> bool {
        self.digests.contains(digest)
    }

    /// The revoked digests in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = &PackageDigest> {
        self.digests.iter()
    }

    /// How many digests are revoked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.digests.len()
    }

    /// Whether no digest is revoked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.digests.is_empty()
    }

    /// The set united with the digests `verified` revokes, or `None` when the
    /// union adds nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::TooManyRevokedDigests`] when the union would
    /// exceed [`MAX_REVOKED_DIGESTS_TOTAL`].
    pub(crate) fn united_with(
        &self,
        verified: &VerifiedCatalog,
    ) -> Result<Option<Self>, RegistryError> {
        let mut merged = self.clone();
        merged
            .digests
            .extend(verified.revoked_digests().iter().cloned());
        if merged.digests.len() > MAX_REVOKED_DIGESTS_TOTAL {
            return Err(RegistryError::TooManyRevokedDigests);
        }
        Ok((merged != *self).then_some(merged))
    }
}

/// On-disk form of [`RevokedDigests`]; digests are stored in ascending order.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RevokedFile {
    schema: u32,
    revoked_digests: Vec<PackageDigest>,
}

impl Registry {
    /// Reads the persisted revoked digests without taking the lock.
    ///
    /// The file is replaced atomically, so the result is one whole committed
    /// set. A missing file is the empty set.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Corrupt`], [`RegistryError::UnsupportedSchema`]
    /// or [`RegistryError::Unsafe`] when the file is not usable.
    pub fn revoked_digests(&self) -> Result<RevokedDigests, RegistryError> {
        let bytes =
            match self
                .root
                .read_file(REVOKED_DIGESTS_NAME, FILE_MODE, MAX_REVOKED_DIGESTS_BYTES)
            {
                Ok(bytes) => bytes,
                Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => {
                    return Ok(RevokedDigests::default());
                }
                Err(FsError::FileTooLarge { .. }) => return Err(RegistryError::Corrupt),
                Err(error) => return Err(registry_fs(&error)),
            };
        parse_revoked(&bytes)
    }

    /// Replaces the persisted set with `set`; the caller holds the lock.
    pub(crate) fn write_revoked_digests(&self, set: &RevokedDigests) -> Result<(), RegistryError> {
        let file = RevokedFile {
            schema: REVOKED_DIGESTS_SCHEMA,
            revoked_digests: set.digests.iter().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&file).map_err(|_cause| RegistryError::Corrupt)?;
        self.root
            .replace_file(
                REVOKED_DIGESTS_NAME,
                REVOKED_DIGESTS_TEMP_NAME,
                &bytes,
                FILE_MODE,
            )
            .map_err(replace_failure)
    }

    /// Removes a temporary file an interrupted write left behind; the caller
    /// holds the lock.
    pub(crate) fn remove_stale_revoked_digests_temporary(&self) -> Result<(), RegistryError> {
        self.remove_stale_temporary(REVOKED_DIGESTS_TEMP_NAME)
    }
}

/// Parses and validates the file bytes.
fn parse_revoked(bytes: &[u8]) -> Result<RevokedDigests, RegistryError> {
    /// Only the schema field, so an unknown version is told apart from damage.
    #[derive(Deserialize)]
    struct Header {
        schema: u32,
    }
    let header: Header = serde_json::from_slice(bytes).map_err(|_cause| RegistryError::Corrupt)?;
    if header.schema != REVOKED_DIGESTS_SCHEMA {
        return Err(RegistryError::UnsupportedSchema);
    }
    let file: RevokedFile =
        serde_json::from_slice(bytes).map_err(|_cause| RegistryError::Corrupt)?;
    let ordered = file
        .revoked_digests
        .windows(2)
        .all(|pair| pair[0] < pair[1]);
    if file.revoked_digests.len() > MAX_REVOKED_DIGESTS_TOTAL || !ordered {
        return Err(RegistryError::Corrupt);
    }
    Ok(RevokedDigests {
        digests: file.revoked_digests.into_iter().collect(),
    })
}
