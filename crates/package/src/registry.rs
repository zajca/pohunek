//! Owner-private, content-addressed package registry.
//!
//! The registry owns one plugin root directory:
//!
//! ```text
//! <root>/registry.json     versioned record of installed packages, atomically replaced
//! <root>/registry.lock     advisory lock serializing every mutation
//! <root>/packages/<hex>/   verified package roots, named by archive digest
//! ```
//!
//! Every mutation is a transaction under the exclusive lock: recover residue
//! of interrupted work, read the record, change it, and atomically replace it
//! with a bumped generation. Install publishes the package root before it
//! records it, and uninstall un-records before it deletes, so the record never
//! names a root that was not published first. A root that later goes missing
//! or fails verification is reported as typed incompatible state by
//! [`Registry::retained_roots`] and is never substituted by another package.
//!
//! The registry does not read the clock, parse package manifests or know which
//! digests live sessions reference: the caller supplies the install time, the
//! package identity and the set of retained digests.

// Rust guideline compliant 2026-10-04

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::ErrorKind;
use std::path::Path;

use pohunek_platform::filesystem::{
    AdvisoryLock, AtomicReplaceError, EntryKind, FsError, StageOutcome, TrustedDir,
};
use protocol::{PackageDigest, PackageId, PackageIdentity};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::install::{collect_staging, install_archive, remove_directory, InstallError};
use crate::layout::{digest_hex, COLLECT_PREFIX, DIRECTORY_MODE, FILE_MODE};
use crate::manifest::ManifestDigest;
use crate::verify::{verify_root, VerifiedRoot, VerifyError};
use crate::{read_archive_with_digest, ArchiveError, Limits};

/// Name of the registry record inside the plugin root.
const REGISTRY_NAME: &str = "registry.json";

/// Name of the temporary file the record is written to before it replaces
/// [`REGISTRY_NAME`].
const REGISTRY_TEMP_NAME: &str = "registry.json.tmp";

/// Name of the advisory lock file that serializes registry mutations.
const LOCK_NAME: &str = "registry.lock";

/// Name of the directory that holds the package roots.
const PACKAGES_DIR: &str = "packages";

/// Schema version of the registry record this crate reads and writes.
const REGISTRY_SCHEMA: u32 = 1;

/// Largest accepted registry record: 1 MiB.
///
/// [`MAX_PACKAGES`] records of at most about 500 bytes each, plus the selected
/// map, stay well below it. The bound is applied before parsing so a corrupt
/// or hostile file cannot grow the buffer.
const MAX_REGISTRY_BYTES: usize = 1024 * 1024;

/// Largest number of installed packages: 1024.
///
/// A developer machine holds a handful of runtimes and a few versions of each.
/// The ceiling keeps the record under the 1 MiB record limit and bounds the
/// work of a retention check.
pub const MAX_PACKAGES: usize = 1024;

/// Where an installed package came from, which decides how it is trusted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PackageSource {
    /// Authorized through the signed official catalog.
    Official,
    /// Installed from an archive the owner pinned by digest: local third-party
    /// trust only.
    ExplicitDigest,
    /// Copied from a developer directory with `link`.
    Link,
}

/// One installed package.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageRecord {
    digest: PackageDigest,
    identity: PackageIdentity,
    source: PackageSource,
    manifest: ManifestDigest,
    installed_at_unix_seconds: u64,
    enabled: bool,
}

impl PackageRecord {
    /// Digest of the archive, the content address of the package root.
    #[must_use]
    pub fn digest(&self) -> &PackageDigest {
        &self.digest
    }

    /// Package id and version.
    #[must_use]
    pub fn identity(&self) -> &PackageIdentity {
        &self.identity
    }

    /// How the package was authorized.
    #[must_use]
    pub fn source(&self) -> PackageSource {
        self.source
    }

    /// Digest of the manifest recorded at install time.
    #[must_use]
    pub fn manifest(&self) -> &ManifestDigest {
        &self.manifest
    }

    /// Install time, seconds since the Unix epoch, as the caller supplied it.
    #[must_use]
    pub fn installed_at_unix_seconds(&self) -> u64 {
        self.installed_at_unix_seconds
    }

    /// Whether fresh launches may use the package.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// The registry record: installed packages and the selected digest per id.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryState {
    schema: u32,
    generation: u64,
    packages: Vec<PackageRecord>,
    selected: BTreeMap<PackageId, PackageDigest>,
}

impl RegistryState {
    fn empty() -> Self {
        Self {
            schema: REGISTRY_SCHEMA,
            generation: 0,
            packages: Vec::new(),
            selected: BTreeMap::new(),
        }
    }

    /// Number of committed changes; it grows by one with every mutation that
    /// changes the record and never repeats.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Installed packages in ascending digest order.
    #[must_use]
    pub fn packages(&self) -> &[PackageRecord] {
        &self.packages
    }

    /// The installed package with `digest`.
    #[must_use]
    pub fn package(&self, digest: &PackageDigest) -> Option<&PackageRecord> {
        self.position(digest).map(|index| &self.packages[index])
    }

    /// The digest selected for bare requests of package `id`.
    #[must_use]
    pub fn selected(&self, id: &PackageId) -> Option<&PackageDigest> {
        self.selected.get(id)
    }

    fn position(&self, digest: &PackageDigest) -> Option<usize> {
        self.packages
            .binary_search_by(|record| record.digest.cmp(digest))
            .ok()
    }

    /// Checks the invariants every reader and writer relies on.
    fn validate(&self) -> bool {
        if self.schema != REGISTRY_SCHEMA || self.packages.len() > MAX_PACKAGES {
            return false;
        }
        if !self
            .packages
            .windows(2)
            .all(|pair| pair[0].digest < pair[1].digest)
        {
            return false;
        }
        let mut identities = HashSet::new();
        if !self
            .packages
            .iter()
            .all(|record| identities.insert(&record.identity))
        {
            return false;
        }
        self.selected.iter().all(|(id, digest)| {
            self.package(digest)
                .is_some_and(|record| record.identity.id == *id)
        })
    }
}

/// A set of package digests that something still references.
///
/// The caller builds it from live workers, live sessions, lost sessions and
/// durable resume bindings; the registry refuses to retire any digest in it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetainedDigests(BTreeSet<PackageDigest>);

impl RetainedDigests {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a referenced digest.
    pub fn insert(&mut self, digest: PackageDigest) {
        self.0.insert(digest);
    }

    /// Whether `digest` is referenced.
    #[must_use]
    pub fn contains(&self, digest: &PackageDigest) -> bool {
        self.0.contains(digest)
    }

    /// The referenced digests in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = &PackageDigest> {
        self.0.iter()
    }
}

impl FromIterator<PackageDigest> for RetainedDigests {
    fn from_iter<T: IntoIterator<Item = PackageDigest>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Extend<PackageDigest> for RetainedDigests {
    fn extend<T: IntoIterator<Item = PackageDigest>>(&mut self, iter: T) {
        self.0.extend(iter);
    }
}

/// Why a retained root cannot be used.
///
/// The state is read-only and typed: the runtime that pinned the digest is
/// incompatible, and no other package or the shell stands in for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IncompatibleReason {
    /// The registry has no record of the digest.
    NotRegistered,
    /// The recorded package root is missing from disk.
    RootMissing,
    /// The package root fails verification.
    RootInvalid(VerifyError),
}

/// Whether a retained digest still has a usable root.
#[derive(Debug)]
#[non_exhaustive]
pub enum RetainedState {
    /// The root exists and verified against its recorded manifest.
    Ready(VerifiedRoot),
    /// The root cannot be used.
    Incompatible(IncompatibleReason),
}

/// One retained digest and the state of its root.
#[derive(Debug)]
pub struct RetainedRoot {
    /// The referenced digest.
    pub digest: PackageDigest,
    /// Whether its root is usable.
    pub state: RetainedState,
}

/// Why a registry operation failed.
///
/// Like the archive and verification errors, no variant carries a path or
/// content.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RegistryError {
    /// Another process or thread holds the registry lock; retry later.
    #[error("the registry is locked by another writer")]
    Busy,
    /// The registry record is not valid; it is never replaced or repaired
    /// silently.
    #[error("the registry record is corrupt")]
    Corrupt,
    /// The registry record has a schema version this build does not know.
    #[error("the registry record has an unsupported schema version")]
    UnsupportedSchema,
    /// A registry file or directory has the wrong type, mode or owner.
    #[error("a registry file or directory is not owner-private")]
    Unsafe,
    /// The archive is not a valid package archive or has another digest.
    #[error("archive rejected: {0}")]
    Archive(#[source] ArchiveError),
    /// Extraction failed.
    #[error("install failed: {0}")]
    Install(#[source] InstallError),
    /// The digest is not installed.
    #[error("the package is not installed")]
    NotInstalled,
    /// The digest is installed under another package identity.
    #[error("the digest is installed under another package identity")]
    IdentityConflict,
    /// The package identity is installed from another archive.
    #[error("the package identity is installed from another archive")]
    IdentityInstalled,
    /// A live or resumable binding still references the digest.
    #[error("the package digest is still referenced")]
    StillReferenced,
    /// The recorded package root fails verification and is left untouched.
    #[error("the package root is invalid: {0}")]
    RootInvalid(#[source] VerifyError),
    /// The registry holds [`MAX_PACKAGES`] packages.
    #[error("the registry holds the maximum number of packages")]
    TooManyPackages,
    /// The record was replaced but directory durability is uncertain; read
    /// the state again before relying on it.
    #[error("the registry record was written but durability is uncertain")]
    CommitUncertain,
    /// The package was unregistered but its root could not be fully deleted;
    /// the next mutation collects the residue.
    #[error("the package was removed from the registry but its root remains")]
    RemovalIncomplete,
    /// The filesystem failed.
    #[error("filesystem failure in the registry: {kind}")]
    Filesystem {
        /// Kind of the operating-system failure.
        kind: ErrorKind,
    },
}

/// Everything an install needs besides the archive bytes.
#[derive(Clone, Debug)]
pub struct InstallRequest<'a> {
    /// The archive bytes.
    pub archive: &'a [u8],
    /// The digest the archive must have.
    pub expected: &'a PackageDigest,
    /// Package id and version, from the package's runtime manifest.
    pub identity: PackageIdentity,
    /// How the package was authorized.
    pub source: PackageSource,
    /// Whether the package starts enabled.
    pub enabled: bool,
    /// Whether the package becomes the selected one for its id.
    pub select: bool,
    /// Install time, seconds since the Unix epoch.
    pub installed_at_unix_seconds: u64,
}

/// What an install did to the registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InstallStatus {
    /// The package was extracted (or an unrecorded verified root was adopted)
    /// and recorded.
    Installed,
    /// The package was already recorded with a verified root; nothing changed.
    AlreadyInstalled,
    /// The package was recorded but its root was missing; the root was
    /// extracted again and the record is unchanged.
    RootRestored,
}

/// The result of a successful install.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallReport {
    /// What the call did.
    pub status: InstallStatus,
    /// The package record.
    pub record: PackageRecord,
    /// Registry generation after the call.
    pub generation: u64,
}

/// The package registry of one plugin root.
#[derive(Debug)]
pub struct Registry {
    root: TrustedDir,
    packages: TrustedDir,
    limits: Limits,
}

/// An exclusive hold on the registry with its current record loaded.
struct Transaction<'a> {
    registry: &'a Registry,
    state: RegistryState,
    _lock: AdvisoryLock,
}

impl Registry {
    /// Opens the registry in the owner-private plugin root `root`, creating
    /// its `packages` directory when absent.
    ///
    /// `root` must be the retained descriptor of an exact-`0700` directory
    /// owned by the current user, such as `<state>/plugins`.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Unsafe`] or [`RegistryError::Filesystem`] when
    /// the `packages` directory cannot be opened safely.
    pub fn open(root: TrustedDir, limits: Limits) -> Result<Self, RegistryError> {
        let packages = root
            .open_or_create_child(PACKAGES_DIR, DIRECTORY_MODE)
            .map_err(|error| registry_fs(&error))?;
        Ok(Self {
            root,
            packages,
            limits,
        })
    }

    /// Opens or creates the plugin root at the absolute path `path`, then
    /// opens the registry in it.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`TrustedDir::open_or_create_absolute`] mapped to
    /// [`RegistryError`], and those of [`Registry::open`].
    pub fn open_at(path: &Path, limits: Limits) -> Result<Self, RegistryError> {
        let root = TrustedDir::open_or_create_absolute(path, DIRECTORY_MODE)
            .map_err(|error| registry_fs(&error))?;
        Self::open(root, limits)
    }

    /// Reads the current record without taking the lock.
    ///
    /// The record is replaced atomically, so the result is one whole
    /// committed state. A missing record is the empty state.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Corrupt`], [`RegistryError::UnsupportedSchema`]
    /// or [`RegistryError::Unsafe`] when the record is not usable.
    pub fn state(&self) -> Result<RegistryState, RegistryError> {
        let bytes = match self
            .root
            .read_file(REGISTRY_NAME, FILE_MODE, MAX_REGISTRY_BYTES)
        {
            Ok(bytes) => bytes,
            Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => {
                return Ok(RegistryState::empty());
            }
            Err(FsError::FileTooLarge { .. }) => return Err(RegistryError::Corrupt),
            Err(error) => return Err(registry_fs(&error)),
        };
        parse_state(&bytes)
    }

    /// Installs an archive: verifies it, extracts and verifies its root, and
    /// records it.
    ///
    /// Idempotent for the same digest and identity. A digest recorded under
    /// another identity, or an identity recorded from another digest, is
    /// refused. A recorded package whose root is missing is extracted again;
    /// one whose root is present but invalid is refused and left as found.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`]; on every error the record is unchanged.
    pub fn install(&self, request: &InstallRequest<'_>) -> Result<InstallReport, RegistryError> {
        let archive = read_archive_with_digest(request.archive, request.expected, &self.limits)
            .map_err(RegistryError::Archive)?;
        let mut transaction = self.begin()?;
        let digest = archive.digest().clone();

        if let Some(existing) = transaction.state.package(&digest).cloned() {
            if existing.identity != request.identity {
                return Err(RegistryError::IdentityConflict);
            }
            let status = match self.verify_record(&existing) {
                Ok(_root) => InstallStatus::AlreadyInstalled,
                Err(VerifyError::RootMissing) => {
                    let installation = install_archive(&self.packages, &archive, &self.limits)
                        .map_err(RegistryError::Install)?;
                    if installation.root.manifest_digest() != &existing.manifest {
                        return Err(RegistryError::RootInvalid(VerifyError::ManifestChanged));
                    }
                    InstallStatus::RootRestored
                }
                Err(error) => return Err(RegistryError::RootInvalid(error)),
            };
            return Ok(InstallReport {
                status,
                record: existing,
                generation: transaction.state.generation,
            });
        }

        if transaction
            .state
            .packages
            .iter()
            .any(|record| record.identity == request.identity)
        {
            return Err(RegistryError::IdentityInstalled);
        }
        if transaction.state.packages.len() >= MAX_PACKAGES {
            return Err(RegistryError::TooManyPackages);
        }
        let installation = install_archive(&self.packages, &archive, &self.limits)
            .map_err(RegistryError::Install)?;
        let record = PackageRecord {
            digest,
            identity: request.identity.clone(),
            source: request.source,
            manifest: installation.root.manifest_digest().clone(),
            installed_at_unix_seconds: request.installed_at_unix_seconds,
            enabled: request.enabled,
        };
        let at = transaction
            .state
            .packages
            .partition_point(|existing| existing.digest < record.digest);
        transaction.state.packages.insert(at, record.clone());
        if request.select {
            transaction
                .state
                .selected
                .insert(record.identity.id.clone(), record.digest.clone());
        }
        transaction.commit()?;
        Ok(InstallReport {
            status: InstallStatus::Installed,
            record,
            generation: transaction.state.generation,
        })
    }

    /// Enables or disables an installed package and returns the generation.
    ///
    /// Enabling requires a root that verifies; disabling never reads the
    /// root. Setting the state a package already has changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::NotInstalled`] for an unknown digest and
    /// [`RegistryError::RootInvalid`] when enabling a package whose root fails
    /// verification.
    pub fn set_enabled(&self, digest: &PackageDigest, enabled: bool) -> Result<u64, RegistryError> {
        let mut transaction = self.begin()?;
        let index = transaction
            .state
            .position(digest)
            .ok_or(RegistryError::NotInstalled)?;
        if transaction.state.packages[index].enabled == enabled {
            return Ok(transaction.state.generation);
        }
        if enabled {
            self.verify_record(&transaction.state.packages[index])
                .map_err(RegistryError::RootInvalid)?;
        }
        transaction.state.packages[index].enabled = enabled;
        transaction.commit()?;
        Ok(transaction.state.generation)
    }

    /// Selects `digest` for bare requests of its package id and returns the
    /// generation.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::NotInstalled`] for an unknown digest and
    /// [`RegistryError::RootInvalid`] when its root fails verification.
    pub fn select(&self, digest: &PackageDigest) -> Result<u64, RegistryError> {
        let mut transaction = self.begin()?;
        let record = transaction
            .state
            .package(digest)
            .cloned()
            .ok_or(RegistryError::NotInstalled)?;
        if transaction.state.selected.get(&record.identity.id) == Some(digest) {
            return Ok(transaction.state.generation);
        }
        self.verify_record(&record)
            .map_err(RegistryError::RootInvalid)?;
        transaction
            .state
            .selected
            .insert(record.identity.id, record.digest);
        transaction.commit()?;
        Ok(transaction.state.generation)
    }

    /// Uninstalls a package unless something still references it.
    ///
    /// The record is replaced first and the root deleted afterwards, so the
    /// record never names a root that was already removed. A root that is
    /// present but fails verification is refused; a missing root is simply
    /// unrecorded. Returns the generation.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::StillReferenced`] when `retained` contains the
    /// digest, [`RegistryError::NotInstalled`] for an unknown digest and
    /// [`RegistryError::RootInvalid`] for a modified root.
    pub fn uninstall(
        &self,
        digest: &PackageDigest,
        retained: &RetainedDigests,
    ) -> Result<u64, RegistryError> {
        let mut transaction = self.begin()?;
        let index = transaction
            .state
            .position(digest)
            .ok_or(RegistryError::NotInstalled)?;
        if retained.contains(digest) {
            return Err(RegistryError::StillReferenced);
        }
        let root_present = match self.verify_record(&transaction.state.packages[index]) {
            Ok(_root) => true,
            Err(VerifyError::RootMissing) => false,
            Err(error) => return Err(RegistryError::RootInvalid(error)),
        };
        transaction.state.packages.remove(index);
        transaction
            .state
            .selected
            .retain(|_id, selected| selected != digest);
        transaction.commit()?;
        if root_present {
            remove_directory(&self.packages, digest_hex(digest))
                .map_err(|_cause| RegistryError::RemovalIncomplete)?;
        }
        Ok(transaction.state.generation)
    }

    /// Verifies one installed package against its recorded manifest.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::NotInstalled`] for an unknown digest and
    /// [`RegistryError::RootInvalid`] when the root is missing or fails
    /// verification.
    pub fn verify(&self, digest: &PackageDigest) -> Result<VerifiedRoot, RegistryError> {
        let state = self.state()?;
        let record = state.package(digest).ok_or(RegistryError::NotInstalled)?;
        self.verify_record(record)
            .map_err(RegistryError::RootInvalid)
    }

    /// Verifies the root of every digest in `referenced`.
    ///
    /// A digest whose root is not registered, missing or invalid is returned
    /// as [`RetainedState::Incompatible`]; the caller marks the affected
    /// runtime read-only and never substitutes another package. Results are in
    /// ascending digest order.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] only when the registry record itself cannot
    /// be read.
    pub fn retained_roots(
        &self,
        referenced: &RetainedDigests,
    ) -> Result<Vec<RetainedRoot>, RegistryError> {
        let state = self.state()?;
        Ok(referenced
            .iter()
            .map(|digest| {
                let retained = match state.package(digest) {
                    None => RetainedState::Incompatible(IncompatibleReason::NotRegistered),
                    Some(record) => match self.verify_record(record) {
                        Ok(root) => RetainedState::Ready(root),
                        Err(VerifyError::RootMissing) => {
                            RetainedState::Incompatible(IncompatibleReason::RootMissing)
                        }
                        Err(error) => {
                            RetainedState::Incompatible(IncompatibleReason::RootInvalid(error))
                        }
                    },
                };
                RetainedRoot {
                    digest: digest.clone(),
                    state: retained,
                }
            })
            .collect())
    }

    /// Package roots on disk that the registry does not record.
    ///
    /// An interrupted install or uninstall leaves one. Installing the same
    /// archive again adopts it after verification; nothing deletes it
    /// automatically, because a lost or deleted record must never cascade into
    /// deleting content.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError`] when the record or the directory cannot be
    /// read.
    pub fn unregistered_roots(&self) -> Result<Vec<PackageDigest>, RegistryError> {
        let state = self.state()?;
        let names = self
            .packages
            .entry_names()
            .map_err(|error| registry_fs(&error))?;
        let mut roots: Vec<PackageDigest> = names
            .iter()
            .filter_map(|name| name.to_str())
            .filter_map(|name| PackageDigest::parse(&format!("sha256:{name}")).ok())
            .filter(|digest| state.package(digest).is_none())
            .collect();
        roots.sort();
        Ok(roots)
    }

    /// Verifies a recorded package's root against the recorded manifest.
    fn verify_record(&self, record: &PackageRecord) -> Result<VerifiedRoot, VerifyError> {
        let root = verify_root(&self.packages, &record.digest, &self.limits)?;
        if root.manifest_digest() == &record.manifest {
            Ok(root)
        } else {
            Err(VerifyError::ManifestChanged)
        }
    }

    /// Takes the exclusive lock, recovers interrupted work and loads the
    /// record.
    fn begin(&self) -> Result<Transaction<'_>, RegistryError> {
        let lock = self
            .root
            .acquire_lock(LOCK_NAME, FILE_MODE)
            .map_err(|error| match error {
                FsError::LockContended { .. } => RegistryError::Busy,
                other => registry_fs(&other),
            })?;
        collect_staging(&self.packages).map_err(RegistryError::Install)?;
        self.remove_stale_temporary()?;
        let state = self.state()?;
        Ok(Transaction {
            registry: self,
            state,
            _lock: lock,
        })
    }

    /// Removes a record temporary left by a writer that stopped before the
    /// rename; a leftover would make the next exclusive create fail.
    fn remove_stale_temporary(&self) -> Result<(), RegistryError> {
        let Some(identity) = self
            .root
            .entry_identity(REGISTRY_TEMP_NAME, EntryKind::RegularFile)
            .map_err(|error| registry_fs(&error))?
        else {
            return Ok(());
        };
        match self
            .root
            .stage_random(REGISTRY_TEMP_NAME, COLLECT_PREFIX, identity)
            .map_err(|error| registry_fs(&error))?
        {
            StageOutcome::Staged(entry) => {
                entry.remove().map_err(|error| registry_fs(&error))?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

impl Transaction<'_> {
    /// Bumps the generation and atomically replaces the record.
    fn commit(&mut self) -> Result<(), RegistryError> {
        self.state.generation = self
            .state
            .generation
            .checked_add(1)
            .ok_or(RegistryError::Corrupt)?;
        let bytes = serde_json::to_vec(&self.state).map_err(|_cause| RegistryError::Corrupt)?;
        if bytes.len() > MAX_REGISTRY_BYTES {
            return Err(RegistryError::TooManyPackages);
        }
        match self
            .registry
            .root
            .replace_file(REGISTRY_NAME, REGISTRY_TEMP_NAME, &bytes, FILE_MODE)
        {
            Ok(()) => Ok(()),
            Err(AtomicReplaceError::BeforeCommit(error)) => Err(registry_fs(&error)),
            Err(AtomicReplaceError::CommittedDurabilityUncertain(_)) => {
                Err(RegistryError::CommitUncertain)
            }
            Err(_) => Err(RegistryError::Filesystem {
                kind: ErrorKind::Other,
            }),
        }
    }
}

/// Parses and validates registry bytes.
fn parse_state(bytes: &[u8]) -> Result<RegistryState, RegistryError> {
    /// Only the schema field, so an unknown version is told apart from damage.
    #[derive(Deserialize)]
    struct Header {
        schema: u32,
    }
    let header: Header = serde_json::from_slice(bytes).map_err(|_cause| RegistryError::Corrupt)?;
    if header.schema != REGISTRY_SCHEMA {
        return Err(RegistryError::UnsupportedSchema);
    }
    let state: RegistryState =
        serde_json::from_slice(bytes).map_err(|_cause| RegistryError::Corrupt)?;
    if state.validate() {
        Ok(state)
    } else {
        Err(RegistryError::Corrupt)
    }
}

/// Classifies a filesystem failure without carrying its path or text.
fn registry_fs(error: &FsError) -> RegistryError {
    match error {
        FsError::UnsafeType { .. }
        | FsError::UnsafeMode { .. }
        | FsError::UnsafeOwner { .. }
        | FsError::UnsafeAcl { .. }
        | FsError::UnsafeLinkCount { .. } => RegistryError::Unsafe,
        other => RegistryError::Filesystem {
            kind: other.io_kind().unwrap_or(ErrorKind::Other),
        },
    }
}
