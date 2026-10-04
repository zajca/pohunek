//! Persistent state the catalog verifier needs from its caller.
//!
//! [`verify_catalog`](crate::verify_catalog) rejects a catalog whose sequence
//! is below a high-water mark and refuses keys on a revoked list, but it keeps
//! neither: the caller persists them. [`Registry`] stores both in
//! `<plugins>/catalog-state.json`, beside the registry record and under the
//! same advisory lock:
//!
//! - the high-water mark only moves up, so a replayed older catalog cannot
//!   re-enable a revoked package;
//! - revoked key ids only accumulate, so a later catalog that omits a revoked
//!   key does not resurrect it.
//!
//! A missing file is the empty state. A damaged file is reported and never
//! replaced, so a verifier is not silently reset to accept old catalogs.

// Rust guideline compliant 2026-10-04

use std::collections::BTreeSet;
use std::io::ErrorKind;

use pohunek_platform::filesystem::FsError;
use serde::{Deserialize, Serialize};

use crate::catalog::{KeyId, VerifiedCatalog, MAX_REVOKED_KEYS};
use crate::layout::FILE_MODE;
use crate::registry::{registry_fs, replace_failure, Registry, RegistryError};

/// Name of the catalog state file inside the plugin root.
const CATALOG_STATE_NAME: &str = "catalog-state.json";

/// Name of the temporary file the state is written to before it replaces
/// [`CATALOG_STATE_NAME`].
const CATALOG_STATE_TEMP_NAME: &str = "catalog-state.json.tmp";

/// Schema version of the state file this crate reads and writes.
const CATALOG_STATE_SCHEMA: u32 = 1;

/// Largest accepted state file: 16 KiB.
///
/// The file holds a schema number, an optional sequence and at most
/// [`MAX_REVOKED_KEYS`] key ids of 64 hex characters plus quoting and a
/// separator (about 4.3 KiB at the cap). The bound is applied before parsing so
/// a corrupt or hostile file cannot grow the buffer.
const MAX_CATALOG_STATE_BYTES: usize = 16 * 1024;

/// The persisted catalog verifier inputs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogState {
    high_water: Option<u64>,
    revoked_key_ids: BTreeSet<KeyId>,
}

impl CatalogState {
    /// Highest catalog sequence recorded so far; pass it as the
    /// `high_water_mark` of [`verify_catalog`](crate::verify_catalog).
    /// `None` before any catalog was recorded.
    #[must_use]
    pub fn high_water(&self) -> Option<u64> {
        self.high_water
    }

    /// Every key id any recorded catalog revoked; add them to the revoked list
    /// of the trust anchor.
    #[must_use]
    pub fn revoked_key_ids(&self) -> &BTreeSet<KeyId> {
        &self.revoked_key_ids
    }
}

/// On-disk form of [`CatalogState`]; key ids are stored in ascending order.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    schema: u32,
    high_water: Option<u64>,
    revoked_key_ids: Vec<KeyId>,
}

impl Registry {
    /// Reads the persisted catalog state without taking the lock.
    ///
    /// The file is replaced atomically, so the result is one whole committed
    /// state. A missing file is the empty state.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Corrupt`], [`RegistryError::UnsupportedSchema`]
    /// or [`RegistryError::Unsafe`] when the file is not usable.
    pub fn catalog_state(&self) -> Result<CatalogState, RegistryError> {
        let bytes =
            match self
                .root
                .read_file(CATALOG_STATE_NAME, FILE_MODE, MAX_CATALOG_STATE_BYTES)
            {
                Ok(bytes) => bytes,
                Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => {
                    return Ok(CatalogState::default());
                }
                Err(FsError::FileTooLarge { .. }) => return Err(RegistryError::Corrupt),
                Err(error) => return Err(registry_fs(&error)),
            };
        parse_state(&bytes)
    }

    /// Merges a verified catalog into the persisted state and returns the
    /// state now in force.
    ///
    /// Under the registry lock the high-water mark becomes the larger of the
    /// stored one and the catalog sequence and the revoked key ids are united.
    /// A catalog with a sequence below the stored mark changes nothing, and a
    /// merge that changes nothing writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Busy`] while another writer holds the lock,
    /// the read errors of [`Registry::catalog_state`] (a damaged file is never
    /// replaced) and [`RegistryError::TooManyRevokedKeys`] when the union
    /// would exceed [`MAX_REVOKED_KEYS`] ids; the file is unchanged on every
    /// error except [`RegistryError::CommitUncertain`].
    pub fn record_catalog(
        &self,
        verified: &VerifiedCatalog,
    ) -> Result<CatalogState, RegistryError> {
        let _lock = self.acquire_lock()?;
        self.remove_stale_temporary(CATALOG_STATE_TEMP_NAME)?;
        let current = self.catalog_state()?;
        if current
            .high_water
            .is_some_and(|stored| verified.sequence() < stored)
        {
            return Ok(current);
        }
        let mut merged = current.clone();
        merged.high_water = Some(current.high_water.map_or(verified.sequence(), |stored| {
            stored.max(verified.sequence())
        }));
        merged
            .revoked_key_ids
            .extend(verified.revoked_key_ids().iter().cloned());
        if merged.revoked_key_ids.len() > MAX_REVOKED_KEYS {
            return Err(RegistryError::TooManyRevokedKeys);
        }
        if merged == current {
            return Ok(current);
        }
        let file = StateFile {
            schema: CATALOG_STATE_SCHEMA,
            high_water: merged.high_water,
            revoked_key_ids: merged.revoked_key_ids.iter().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&file).map_err(|_cause| RegistryError::Corrupt)?;
        self.root
            .replace_file(
                CATALOG_STATE_NAME,
                CATALOG_STATE_TEMP_NAME,
                &bytes,
                FILE_MODE,
            )
            .map_err(replace_failure)?;
        Ok(merged)
    }
}

/// Parses and validates state file bytes.
fn parse_state(bytes: &[u8]) -> Result<CatalogState, RegistryError> {
    /// Only the schema field, so an unknown version is told apart from damage.
    #[derive(Deserialize)]
    struct Header {
        schema: u32,
    }
    let header: Header = serde_json::from_slice(bytes).map_err(|_cause| RegistryError::Corrupt)?;
    if header.schema != CATALOG_STATE_SCHEMA {
        return Err(RegistryError::UnsupportedSchema);
    }
    let file: StateFile = serde_json::from_slice(bytes).map_err(|_cause| RegistryError::Corrupt)?;
    let ordered = file
        .revoked_key_ids
        .windows(2)
        .all(|pair| pair[0] < pair[1]);
    if file.revoked_key_ids.len() > MAX_REVOKED_KEYS || !ordered {
        return Err(RegistryError::Corrupt);
    }
    Ok(CatalogState {
        high_water: file.high_water,
        revoked_key_ids: file.revoked_key_ids.into_iter().collect(),
    })
}
