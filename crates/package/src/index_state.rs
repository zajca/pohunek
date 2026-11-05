//! Persistent state the plugin index verifier needs from its caller.
//!
//! [`verify_index`](crate::verify_index) rejects a document whose sequence or
//! revocation sequence is below a mark and refuses keys on a revoked list, but
//! it keeps none of them: the caller persists them. [`Registry`] stores them in
//! `<plugins>/index-state.json`, beside the registry record and under the same
//! advisory lock, with the same rules as the catalog state
//! ([`catalog_state`](crate::catalog_state)):
//!
//! - both marks only move up, so a replayed older index or revocation list
//!   cannot re-enable a revoked key or a withdrawn entry;
//! - revoked key ids only accumulate, so a later document that omits a revoked
//!   key does not resurrect it.
//!
//! The file is independent of `catalog-state.json`; the caller adds the
//! catalog's revoked ids to the verifier's `base_revoked` set. A missing file
//! is the empty state. A damaged file is reported and never replaced, so a
//! verifier is not silently reset to accept old documents.

// Rust guideline compliant 2026-10-04

use std::io::ErrorKind;

use pohunek_platform::filesystem::FsError;
use serde::{Deserialize, Serialize};

use crate::catalog::{KeyId, MAX_REVOKED_KEYS};
use crate::layout::FILE_MODE;
use crate::plugin_index::{IndexVerifierState, VerifiedIndex};
use crate::registry::{registry_fs, replace_failure, Registry, RegistryError};

/// Name of the index state file inside the plugin root.
const INDEX_STATE_NAME: &str = "index-state.json";

/// Name of the temporary file the state is written to before it replaces
/// [`INDEX_STATE_NAME`].
const INDEX_STATE_TEMP_NAME: &str = "index-state.json.tmp";

/// Schema version of the state file this crate reads and writes.
const INDEX_STATE_SCHEMA: u32 = 1;

/// Largest accepted state file: 16 KiB.
///
/// The file holds a schema number, two optional marks and at most
/// [`MAX_REVOKED_KEYS`] key ids of 64 hex characters plus quoting and a
/// separator (about 4.4 KiB at the cap). The bound is applied before parsing so
/// a corrupt or hostile file cannot grow the buffer.
const MAX_INDEX_STATE_BYTES: usize = 16 * 1024;

/// On-disk form of [`IndexVerifierState`]; key ids are stored in ascending
/// order.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    schema: u32,
    index_high_water: Option<u64>,
    revocation_high_water: Option<u64>,
    revoked_key_ids: Vec<KeyId>,
}

impl Registry {
    /// Reads the persisted index state without taking the lock.
    ///
    /// The file is replaced atomically, so the result is one whole committed
    /// state. A missing file is the empty state.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Corrupt`], [`RegistryError::UnsupportedSchema`]
    /// or [`RegistryError::Unsafe`] when the file is not usable.
    pub fn index_state(&self) -> Result<IndexVerifierState, RegistryError> {
        let bytes = match self
            .root
            .read_file(INDEX_STATE_NAME, FILE_MODE, MAX_INDEX_STATE_BYTES)
        {
            Ok(bytes) => bytes,
            Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => {
                return Ok(IndexVerifierState::default());
            }
            Err(FsError::FileTooLarge { .. }) => return Err(RegistryError::Corrupt),
            Err(error) => return Err(registry_fs(&error)),
        };
        parse_state(&bytes)
    }

    /// Merges a verified index into the persisted state and returns the state
    /// now in force.
    ///
    /// Under the registry lock both marks become the larger of the stored one
    /// and the document's, and the revoked key ids are united. A document
    /// whose sequence or revocation sequence is below the stored mark changes
    /// nothing, and a merge that changes nothing writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Busy`] while another writer holds the lock,
    /// the read errors of [`Registry::index_state`] (a damaged file is never
    /// replaced) and [`RegistryError::TooManyRevokedKeys`] when the union
    /// would exceed [`MAX_REVOKED_KEYS`] ids; the file is unchanged on every
    /// error except [`RegistryError::CommitUncertain`].
    pub fn record_index(
        &self,
        verified: &VerifiedIndex,
    ) -> Result<IndexVerifierState, RegistryError> {
        let _lock = self.acquire_lock()?;
        self.remove_stale_temporary(INDEX_STATE_TEMP_NAME)?;
        let current = self.index_state()?;
        let below = |stored: Option<u64>, sequence: u64| stored.is_some_and(|mark| sequence < mark);
        if below(current.index_high_water, verified.sequence())
            || below(
                current.revocation_high_water,
                verified.revocation_sequence(),
            )
        {
            return Ok(current);
        }
        let mut merged = current.clone();
        merged.index_high_water = Some(raised(current.index_high_water, verified.sequence()));
        merged.revocation_high_water = Some(raised(
            current.revocation_high_water,
            verified.revocation_sequence(),
        ));
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
            schema: INDEX_STATE_SCHEMA,
            index_high_water: merged.index_high_water,
            revocation_high_water: merged.revocation_high_water,
            revoked_key_ids: merged.revoked_key_ids.iter().cloned().collect(),
        };
        let bytes = serde_json::to_vec(&file).map_err(|_cause| RegistryError::Corrupt)?;
        self.root
            .replace_file(INDEX_STATE_NAME, INDEX_STATE_TEMP_NAME, &bytes, FILE_MODE)
            .map_err(replace_failure)?;
        Ok(merged)
    }
}

/// The larger of a stored mark and a document sequence.
fn raised(stored: Option<u64>, sequence: u64) -> u64 {
    stored.map_or(sequence, |mark| mark.max(sequence))
}

/// Parses and validates state file bytes.
fn parse_state(bytes: &[u8]) -> Result<IndexVerifierState, RegistryError> {
    /// Only the schema field, so an unknown version is told apart from damage.
    #[derive(Deserialize)]
    struct Header {
        schema: u32,
    }
    let header: Header = serde_json::from_slice(bytes).map_err(|_cause| RegistryError::Corrupt)?;
    if header.schema != INDEX_STATE_SCHEMA {
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
    Ok(IndexVerifierState {
        index_high_water: file.index_high_water,
        revocation_high_water: file.revocation_high_water,
        revoked_key_ids: file.revoked_key_ids.into_iter().collect(),
    })
}
