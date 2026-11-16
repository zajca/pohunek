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
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::time::{SystemTime, UNIX_EPOCH};

use pohunek_platform::filesystem::FsError;
use serde::{Deserialize, Serialize};

use protocol::RuntimeId;

use crate::catalog::{KeyId, MAX_REVOKED_KEYS};
use crate::index_store::MAX_RESERVED_RUNTIME_IDS;
use crate::layout::FILE_MODE;
use crate::plugin_index::{IndexEntryKind, IndexVerifierState, VerifiedIndex};
use crate::registry::{registry_fs, replace_failure, Registry, RegistryError};

/// Name of the index state file inside the plugin root.
const INDEX_STATE_NAME: &str = "index-state.json";

/// Name of the temporary file the state is written to before it replaces
/// [`INDEX_STATE_NAME`].
const INDEX_STATE_TEMP_NAME: &str = "index-state.json.tmp";

/// How many preserved copies of a damaged state file one second may hold.
const MAX_DAMAGED_COPIES: u32 = 64;

/// Writes schema 2; reads schema 1 and 2.
const INDEX_STATE_SCHEMA: u32 = 2;
const INDEX_STATE_SCHEMA_V1: u32 = 1;

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
    #[serde(default)]
    reserved_runtime_ids: Vec<String>,
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

    /// Explicit repair of a damaged (corrupt or unsupported-schema)
    /// `index-state.json`.
    ///
    /// The new state is the union of `verified` and every document in
    /// `stored`: both marks are the largest sequence among them and the
    /// revoked key ids and reserved runtime ids are united, so a repair never
    /// lowers what a verified stored copy already established. The new file
    /// is committed atomically; the damaged bytes are copied beside it, with the
    /// same mode, as `index-state.json.damaged-<unix-ts>` before the commit, so
    /// no moment exists without a state file.
    ///
    /// A healthy file takes the ordinary [`Registry::record_index`] path, and
    /// an unsafe one (wrong mode or owner) is returned as
    /// [`RegistryError::Unsafe`] untouched.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Busy`] while another writer holds the lock,
    /// [`RegistryError::Unsafe`], the limit errors of
    /// [`Registry::record_index`] and [`RegistryError::Filesystem`] when the
    /// damaged file cannot be preserved; the damaged file stays in place on
    /// every error except [`RegistryError::CommitUncertain`].
    pub fn repair_index_state(
        &self,
        verified: &VerifiedIndex,
        stored: &[VerifiedIndex],
    ) -> Result<IndexVerifierState, RegistryError> {
        match self.index_state() {
            Ok(_) => return self.record_index(verified),
            Err(RegistryError::Corrupt | RegistryError::UnsupportedSchema) => {}
            Err(error) => return Err(error),
        }
        let _lock = self.acquire_lock()?;
        self.remove_stale_temporary(INDEX_STATE_TEMP_NAME)?;
        let documents: Vec<&VerifiedIndex> = stored.iter().chain([verified]).collect();
        let merged = merged_with(&IndexVerifierState::default(), &documents)?;
        let bytes = encode_state(&merged)?;
        self.preserve_damaged_state()?;
        self.root
            .replace_file(INDEX_STATE_NAME, INDEX_STATE_TEMP_NAME, &bytes, FILE_MODE)
            .map_err(replace_failure)?;
        Ok(merged)
    }

    /// Copies the current state file, with its mode, to a fresh
    /// `index-state.json.damaged-<unix-ts>[-<n>]` name that never replaces an
    /// earlier preserved copy.
    fn preserve_damaged_state(&self) -> Result<(), RegistryError> {
        let filesystem = |error: std::io::Error| RegistryError::Filesystem { kind: error.kind() };
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let root = self.root.path();
        let mut source = std::fs::File::open(root.join(INDEX_STATE_NAME)).map_err(filesystem)?;
        let mode = source.metadata().map_err(filesystem)?.permissions().mode() & 0o777;
        for attempt in 0..MAX_DAMAGED_COPIES {
            let name = if attempt == 0 {
                format!("{INDEX_STATE_NAME}.damaged-{seconds}")
            } else {
                format!("{INDEX_STATE_NAME}.damaged-{seconds}-{attempt}")
            };
            let copy = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(root.join(name));
            match copy {
                Ok(mut copy) => {
                    std::io::copy(&mut source, &mut copy).map_err(filesystem)?;
                    return copy.sync_all().map_err(filesystem);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(filesystem(error)),
            }
        }
        Err(RegistryError::Filesystem {
            kind: ErrorKind::AlreadyExists,
        })
    }

    /// The verifier state implied by already verified documents: the larger
    /// marks, the united revoked key ids and the united reserved runtime ids.
    ///
    /// # Errors
    ///
    /// Returns the limit errors of [`Registry::record_index`].
    pub fn state_of_documents(
        documents: &[VerifiedIndex],
    ) -> Result<IndexVerifierState, RegistryError> {
        let refs: Vec<&VerifiedIndex> = documents.iter().collect();
        merged_with(&IndexVerifierState::default(), &refs)
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
        let merged = merged_with(&current, &[verified])?;
        if merged == current {
            return Ok(current);
        }
        let bytes = encode_state(&merged)?;
        self.root
            .replace_file(INDEX_STATE_NAME, INDEX_STATE_TEMP_NAME, &bytes, FILE_MODE)
            .map_err(replace_failure)?;
        Ok(merged)
    }
}

/// `base` raised by every document: marks become the larger value, revoked key
/// ids and reserved runtime ids are united.
fn merged_with(
    base: &IndexVerifierState,
    documents: &[&VerifiedIndex],
) -> Result<IndexVerifierState, RegistryError> {
    let mut merged = base.clone();
    let mut reserved: Vec<RuntimeId> = merged.reserved_runtime_ids.iter().cloned().collect();
    for document in documents {
        merged.index_high_water = Some(raised(merged.index_high_water, document.sequence()));
        merged.revocation_high_water = Some(raised(
            merged.revocation_high_water,
            document.revocation_sequence(),
        ));
        merged
            .revoked_key_ids
            .extend(document.revoked_key_ids().iter().cloned());
        reserved.extend(
            document
                .entries()
                .iter()
                .filter(|entry| entry.kind() == IndexEntryKind::Runtime)
                .filter_map(|entry| entry.runtime_id.clone()),
        );
    }
    if merged.revoked_key_ids.len() > MAX_REVOKED_KEYS {
        return Err(RegistryError::TooManyRevokedKeys);
    }
    reserved.sort();
    reserved.dedup();
    if reserved.len() > MAX_RESERVED_RUNTIME_IDS {
        return Err(RegistryError::TooManyReservedRuntimeIds);
    }
    merged.reserved_runtime_ids = reserved.into_iter().collect();
    Ok(merged)
}

/// Serializes `state` as the state file, within [`MAX_INDEX_STATE_BYTES`].
fn encode_state(state: &IndexVerifierState) -> Result<Vec<u8>, RegistryError> {
    let file = StateFile {
        schema: INDEX_STATE_SCHEMA,
        index_high_water: state.index_high_water,
        revocation_high_water: state.revocation_high_water,
        revoked_key_ids: state.revoked_key_ids.iter().cloned().collect(),
        reserved_runtime_ids: state
            .reserved_runtime_ids
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect(),
    };
    let bytes = serde_json::to_vec(&file).map_err(|_cause| RegistryError::Corrupt)?;
    if bytes.len() > MAX_INDEX_STATE_BYTES {
        return Err(RegistryError::TooManyReservedRuntimeIds);
    }
    Ok(bytes)
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
    if header.schema != INDEX_STATE_SCHEMA && header.schema != INDEX_STATE_SCHEMA_V1 {
        return Err(RegistryError::UnsupportedSchema);
    }
    let file: StateFile = serde_json::from_slice(bytes).map_err(|_cause| RegistryError::Corrupt)?;
    let ordered = file
        .revoked_key_ids
        .windows(2)
        .all(|pair| pair[0] < pair[1]);
    let reserved_ordered = file
        .reserved_runtime_ids
        .windows(2)
        .all(|pair| pair[0] < pair[1]);
    if file.revoked_key_ids.len() > MAX_REVOKED_KEYS
        || !ordered
        || file.reserved_runtime_ids.len() > MAX_RESERVED_RUNTIME_IDS
        || !reserved_ordered
    {
        return Err(RegistryError::Corrupt);
    }
    let reserved: Result<std::collections::BTreeSet<RuntimeId>, _> = file
        .reserved_runtime_ids
        .iter()
        .map(|text| RuntimeId::parse(text))
        .collect();
    let reserved = reserved.map_err(|_cause| RegistryError::Corrupt)?;
    Ok(IndexVerifierState {
        index_high_water: file.index_high_water,
        revocation_high_water: file.revocation_high_water,
        revoked_key_ids: file.revoked_key_ids.into_iter().collect(),
        reserved_runtime_ids: reserved,
    })
}
