//! Stored plugin index under `<plugins>/index/`: the exact bytes the CLI
//! staged, with a daemon-clock sidecar, and the accumulated reserved ids.
//!
//! The daemon stores the signed document byte-exact so later installs and the
//! doctor can re-verify the same bytes the CLI used. The sidecar
//! `stored.json` records `stored_at` per schema file name; the signed
//! document itself is never rewritten. A missing file means "no stored
//! index"; an unreadable or invalid file is never deleted or rewritten by a
//! read.

// Rust guideline compliant 2026-11-12

use std::io::ErrorKind;

use pohunek_platform::filesystem::FsError;
use serde::{Deserialize, Serialize};

use crate::layout::{COLLECT_PREFIX, DIRECTORY_MODE, FILE_MODE};
use crate::plugin_index::{
    VerifiedIndex, INDEX_SCHEMA_VERSION, INDEX_SCHEMA_VERSION_2, MAX_INDEX_BYTES,
};
use crate::registry::{registry_fs, replace_failure, Registry, RegistryError};
use pohunek_platform::filesystem::{EntryKind, StageOutcome};

/// Name of the stored-index directory inside the plugin root.
const INDEX_DIR: &str = "index";
/// Sidecar name inside [`INDEX_DIR`].
const STORED_NAME: &str = "stored.json";
/// Temporary name for the sidecar replace.
const STORED_TEMP_NAME: &str = "stored.json.tmp";
/// Temporary name for a stored document replace.
const DOC_TEMP_NAME: &str = "document.json.tmp";
/// Schema of the sidecar.
const STORED_SCHEMA: u32 = 1;
/// Largest sidecar: 4 KiB.
const MAX_STORED_BYTES: usize = 4 * 1024;
/// How many reserved ids accumulate in the index state.
pub(crate) const MAX_RESERVED_RUNTIME_IDS: usize = 256;
/// Maximum age of a stored index copy before the CLI refreshes it.
pub const STORED_INDEX_MAX_AGE: u64 = 24 * 60 * 60;
/// Sidecar key for the schema 1 document.
pub const SIDECAR_KEY_V1: &str = "official-v1";
/// Sidecar key for the schema 2 document.
pub const SIDECAR_KEY_V2: &str = "official-v2";

/// What `Registry` stored for one schema file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredIndex {
    /// File name: `official-v1.json` or `official-v2.json`.
    pub file_name: &'static str,
    /// Exact stored bytes.
    pub bytes: Vec<u8>,
    /// Daemon-clock second the document was stored at.
    pub stored_at: u64,
}

impl StoredIndex {
    /// Schema version of the file.
    #[must_use]
    pub fn schema_version(&self) -> u64 {
        if self.file_name == "official-v2.json" {
            INDEX_SCHEMA_VERSION_2
        } else {
            INDEX_SCHEMA_VERSION
        }
    }

    /// File name for a schema version.
    #[must_use]
    pub fn file_name_for(schema_version: u64) -> &'static str {
        if schema_version == INDEX_SCHEMA_VERSION_2 {
            "official-v2.json"
        } else {
            "official-v1.json"
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredFile {
    schema: u32,
    #[serde(
        default,
        rename = "official-v1",
        skip_serializing_if = "Option::is_none"
    )]
    official_v1: Option<u64>,
    #[serde(
        default,
        rename = "official-v2",
        skip_serializing_if = "Option::is_none"
    )]
    official_v2: Option<u64>,
}

impl Registry {
    /// Reads one stored document and its `stored_at`.
    ///
    /// A missing document or a missing sidecar entry is the empty case
    /// handled by the caller as "no stored index". Damaged files are never
    /// replaced.
    pub fn stored_index(&self, schema_version: u64) -> Result<Option<StoredIndex>, RegistryError> {
        let file_name = StoredIndex::file_name_for(schema_version);
        let dir = match self.root.open_child(INDEX_DIR, DIRECTORY_MODE) {
            Ok(dir) => dir,
            Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => return Ok(None),
            Err(error) => return Err(registry_fs(&error)),
        };
        let bytes = match dir.read_file(file_name, FILE_MODE, MAX_INDEX_BYTES) {
            Ok(bytes) => bytes,
            Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => return Ok(None),
            Err(FsError::FileTooLarge { .. }) => return Err(RegistryError::Corrupt),
            Err(error) => return Err(registry_fs(&error)),
        };
        let sidecar = Self::read_sidecar(&dir)?;
        let stored_at = match file_name {
            "official-v2.json" => sidecar.official_v2,
            _ => sidecar.official_v1,
        };
        Ok(stored_at.map(|stored_at| StoredIndex {
            file_name,
            bytes,
            stored_at,
        }))
    }

    fn read_sidecar(
        dir: &pohunek_platform::filesystem::TrustedDir,
    ) -> Result<StoredFile, RegistryError> {
        match dir.read_file(STORED_NAME, FILE_MODE, MAX_STORED_BYTES) {
            Ok(bytes) => {
                let file: StoredFile =
                    serde_json::from_slice(&bytes).map_err(|_cause| RegistryError::Corrupt)?;
                if file.schema != STORED_SCHEMA {
                    return Err(RegistryError::UnsupportedSchema);
                }
                Ok(file)
            }
            Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => Ok(StoredFile {
                schema: STORED_SCHEMA,
                official_v1: None,
                official_v2: None,
            }),
            Err(FsError::FileTooLarge { .. }) => Err(RegistryError::Corrupt),
            Err(error) => Err(registry_fs(&error)),
        }
    }

    /// Stores `bytes` (already verified by the caller) as the document for
    /// `schema_version`, setting `stored_at`, under the registry lock.
    ///
    /// The caller must have verified the bytes and checked the rollback
    /// against the persisted marks before calling; this only writes. A
    /// damaged sidecar is replaced by a valid refresh.
    pub fn store_index(
        &self,
        schema_version: u64,
        verified: &VerifiedIndex,
        bytes: &[u8],
        stored_at: u64,
    ) -> Result<StoredIndex, RegistryError> {
        let _lock = self.acquire_lock()?;
        // Rollback already checked by the caller; store_index only writes.
        let _ = &verified;
        let dir = self
            .root
            .open_or_create_child(INDEX_DIR, DIRECTORY_MODE)
            .map_err(|error| registry_fs(&error))?;
        // Clean a stale document temp left by a stopped writer.
        Self::remove_stale_temporary_in(&dir, DOC_TEMP_NAME);
        let file_name = StoredIndex::file_name_for(schema_version);
        dir.replace_file(file_name, DOC_TEMP_NAME, bytes, FILE_MODE)
            .map_err(replace_failure)?;
        // Update the sidecar in the same lock hold. A damaged sidecar is
        // replaced by a valid refresh; a read failure is propagated so the
        // other schema's stored_at is not silently lost.
        let mut sidecar = match Self::read_sidecar(&dir) {
            Ok(file) => file,
            Err(RegistryError::Corrupt | RegistryError::UnsupportedSchema) => StoredFile {
                schema: STORED_SCHEMA,
                official_v1: None,
                official_v2: None,
            },
            Err(error) => return Err(error),
        };
        match file_name {
            "official-v2.json" => sidecar.official_v2 = Some(stored_at),
            _ => sidecar.official_v1 = Some(stored_at),
        }
        let sidecar_bytes =
            serde_json::to_vec(&sidecar).map_err(|_cause| RegistryError::Corrupt)?;
        Self::remove_stale_temporary_in(&dir, STORED_TEMP_NAME);
        dir.replace_file(STORED_NAME, STORED_TEMP_NAME, &sidecar_bytes, FILE_MODE)
            .map_err(replace_failure)?;
        Ok(StoredIndex {
            file_name,
            bytes: bytes.to_vec(),
            stored_at,
        })
    }

    fn remove_stale_temporary_in(dir: &pohunek_platform::filesystem::TrustedDir, name: &str) {
        // Same rule as the other registry files: a writer stopped before
        // the rename leaves an exclusively-created temp that would wedge
        // every later replace; remove it best-effort under the lock.
        let Ok(Some(identity)) = dir.entry_identity(name, EntryKind::RegularFile) else {
            return;
        };
        if let Ok(StageOutcome::Staged(entry)) = dir.stage_random(name, COLLECT_PREFIX, identity) {
            let _ = entry.remove();
        }
    }
}
