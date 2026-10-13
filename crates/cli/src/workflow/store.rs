//! The workflow plugin store.
//!
//! Installed plugins live in a [`Registry`] rooted at
//! `<state>/pohunek/workflow-plugins`, never below the runtime package root,
//! so the daemon cannot load one. Every plugin also owns a state directory and
//! a config directory named by its id, below exact-`0700` parents; they
//! outlive an uninstall unless the owner purges them.
//!
//! Reading commands open the registry without creating anything, so a refused
//! install or a `list` leaves no directory behind. Every changing command
//! holds the store lock `store.lock` in the store root for its whole run: the
//! one-version check, directory creation, the registry change and the purge
//! are one critical section. It is a separate file from the registry's own
//! lock, which each registry call takes and releases.
//!
//! The store keeps one version per plugin id: lookups by id resolve the
//! selected digest, and an install of another archive of an installed id is
//! refused under the lock.

// Rust guideline compliant 2026-10-13

use std::fs::File;
use std::io::{ErrorKind, Read as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use package::registry::{
    files_path, InstallReport, InstallRequest, InstallStatus, PackageRecord, PackageSource,
    Registry, RegistryError, RetainedDigests,
};
use package::verify::{VerifiedRoot, VerifyError};
use package::{read_archive_with_digest, ArchiveError, Limits, PackageDigest};
use pohunek_platform::filesystem::{
    EntryKind, FileLock, FsError, LockKind, RemoveOutcome, StageOutcome, TrustedDir,
};
use protocol::PackageId;
use serde::Serialize;

use super::manifest::{self, Manifest, MANIFEST_PATH, MAX_MANIFEST_BYTES};
use super::Error;
use crate::paths::WorkflowDirs;

/// Mode of every store, state and config directory: owner-only.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Name prefix a purged or rolled-back directory is moved to before its tree
/// is removed; the staged name is `.purge-<id>+<random hex>`.
///
/// Plugin ids never start with `.`, so such a name never collides with a
/// plugin directory, and they never contain [`PURGE_ID_END`], so the owning id
/// reads back unambiguously. A purge retries every leftover in the parent
/// before it removes the plugin's directory.
const PURGE_PREFIX: &str = ".purge-";

/// Separator after the plugin id in a staged purge name; outside the package
/// id alphabet `[a-z0-9._-]`.
const PURGE_ID_END: char = '+';

/// Name of the store lock in the store root, beside the registry's own files.
const LOCK_NAME: &str = "store.lock";

/// Mode of the store lock file: owner-only, like the registry's files.
const LOCK_FILE_MODE: u32 = 0o600;

/// Name of the unit lifecycle lock in the store root.
///
/// Enable, disable and uninstall hold it across the registry change and the
/// service manager operations, which the store lock cannot cover because
/// each registry change takes and releases that one itself.
const UNITS_LOCK_NAME: &str = "units.lock";

/// Warning when the registry record is gone but the installed files remain.
///
/// The registry leaves undeletable residue under a `.collect-*` name in
/// `packages/` and retries it, best effort, on every later store change.
const REMOVAL_INCOMPLETE: &str = "the plugin record was removed, but some of its installed \
    files could not be deleted and remain under a `.collect-*` name in";

/// Warning when the registry replaced its record without confirming durability.
const UNINSTALL_UNCERTAIN: &str = "the plugin record was removed, but the store could not \
    confirm the change is durable and left the installed files in place; check \
    `pohunek workflow list`";

/// Warning when an install recorded the plugin without confirming durability.
const INSTALL_UNCERTAIN: &str = "the plugin was recorded, but the store could not confirm the \
    change is durable; check `pohunek workflow list`";

/// A verified archive and its validated manifest, ready for consent.
#[derive(Debug)]
pub(crate) struct Candidate {
    bytes: Vec<u8>,
    /// Digest of the archive bytes, as the owner supplied it.
    pub(crate) digest: PackageDigest,
    /// The validated manifest.
    pub(crate) manifest: Manifest,
}

/// Reads the archive at `path`, checks it has `expected` as its digest, and
/// validates its manifest. Nothing is written.
///
/// # Errors
///
/// Returns [`Error::ArchiveInvalid`], [`Error::DigestMismatch`] or
/// [`Error::DescriptorInvalid`].
pub(crate) fn read_candidate(path: &Path, expected: &PackageDigest) -> Result<Candidate, Error> {
    let limit = Limits::DEFAULT.max_compressed_bytes;
    let invalid = |reason: &str| Error::ArchiveInvalid {
        path: path.display().to_string(),
        reason: reason.to_owned(),
    };
    let file = File::open(path).map_err(|_unreadable| invalid("cannot open the file"))?;
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(invalid("not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_unreadable| invalid("cannot read the file"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(invalid("larger than the package archive limit"));
    }
    let archive = match read_archive_with_digest(&bytes, expected, &Limits::DEFAULT) {
        Ok(archive) => archive,
        Err(ArchiveError::DigestMismatch) => return Err(Error::DigestMismatch),
        Err(error) => return Err(invalid(&error.to_string())),
    };
    let manifest = manifest::from_archive(&archive)?;
    Ok(Candidate {
        bytes,
        digest: expected.clone(),
        manifest,
    })
}

/// The workflow plugin store of this user.
#[derive(Debug)]
pub(crate) struct Store {
    dirs: WorkflowDirs,
}

/// How the recorded copy of an archive looks before consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// No plugin of the id is installed.
    No,
    /// The archive is installed and its files verify.
    Intact,
    /// The archive is installed but its files are missing.
    FilesMissing,
    /// The archive is installed but its files fail verification.
    FilesModified,
}

/// What happened to one purged directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Purged {
    /// The directory was removed.
    Removed,
    /// There was no directory to remove.
    Absent,
}

/// The result of an uninstall.
#[derive(Debug)]
pub(crate) struct Removal {
    /// The removed record; `None` when only directories were purged.
    pub(crate) record: Option<PackageRecord>,
    /// A committed removal whose cleanup did not finish, and purge leftovers
    /// of earlier purges that could not be removed.
    pub(crate) warnings: Vec<String>,
    /// State and config directory outcomes, when purged.
    pub(crate) purged: Option<(Purged, Purged)>,
}

impl Store {
    /// The store at `dirs`; nothing is opened yet.
    pub(crate) fn new(dirs: WorkflowDirs) -> Self {
        Self { dirs }
    }

    /// The extracted tree `digest` has, or will have once installed.
    pub(crate) fn files_dir(&self, digest: &PackageDigest) -> PathBuf {
        files_path(&self.dirs.root, digest)
    }

    /// The state directory of plugin `id`.
    pub(crate) fn state_dir(&self, id: &PackageId) -> PathBuf {
        // A package id is one path component: no `/`, no leading `.`, no `..`.
        self.dirs.state.join(id.as_str())
    }

    /// The config directory of plugin `id`.
    pub(crate) fn config_dir(&self, id: &PackageId) -> PathBuf {
        self.dirs.config.join(id.as_str())
    }

    /// Installed plugins in ascending digest order; empty when the store was
    /// never created.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] when the registry cannot be read safely.
    pub(crate) fn list(&self) -> Result<Vec<PackageRecord>, Error> {
        match self.existing()? {
            None => Ok(Vec::new()),
            Some(registry) => Ok(registry.state().map_err(store)?.packages().to_vec()),
        }
    }

    /// The installed record of `id`, if any.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Store`] when the registry cannot be read safely.
    pub(crate) fn installed(&self, id: &PackageId) -> Result<Option<PackageRecord>, Error> {
        let Some(registry) = self.existing()? else {
            return Ok(None);
        };
        let state = registry.state().map_err(store)?;
        Ok(state
            .selected(id)
            .and_then(|digest| state.package(digest))
            .cloned())
    }

    /// How `candidate` is recorded, for the consent review. Takes no lock and
    /// writes nothing; [`Store::install`] checks again under the lock.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PluginInstalled`] when another archive of the id is
    /// installed, and [`Error::Store`] when the store cannot be read.
    pub(crate) fn recorded(&self, candidate: &Candidate) -> Result<Recorded, Error> {
        let id = &candidate.manifest.identity.id;
        let Some(record) = self.installed(id)? else {
            return Ok(Recorded::No);
        };
        if record.digest() != &candidate.digest {
            return Err(Error::PluginInstalled { id: id.to_string() });
        }
        let registry = self.existing()?.ok_or_else(|| unknown(id))?;
        match registry.verify(record.digest()) {
            Ok(_root) => Ok(Recorded::Intact),
            Err(RegistryError::RootInvalid(VerifyError::RootMissing)) => Ok(Recorded::FilesMissing),
            Err(RegistryError::RootInvalid(_cause)) => Ok(Recorded::FilesModified),
            Err(error) => Err(by_id(error, id)),
        }
    }

    /// Installs `candidate` after the owner consented.
    ///
    /// Under the store lock: the one-version check runs again, both
    /// directory parents must be exact-`0700`, and the plugin's state and
    /// config directories are created before the registry records it. A
    /// failure removes the plugin directories this call created. Installing
    /// the recorded archive again changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] while another workflow command runs,
    /// [`Error::PluginInstalled`] when another archive of the id is
    /// installed, [`Error::RootInvalid`] when the recorded root of the same
    /// archive fails verification, [`Error::InstallUncertain`] when the record
    /// replacement committed but cannot be read back (directories kept), and
    /// [`Error::Store`] otherwise.
    pub(crate) fn install(
        &self,
        candidate: &Candidate,
        enabled: bool,
    ) -> Result<(InstallReport, Option<&'static str>), Error> {
        let identity = &candidate.manifest.identity;
        let _lock = self.lock_creating()?;
        if self
            .installed(&identity.id)?
            .is_some_and(|record| record.digest() != &candidate.digest)
        {
            return Err(Error::PluginInstalled {
                id: identity.id.to_string(),
            });
        }
        let installed_at_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_before_epoch| {
                Error::Store("the host clock is before the Unix epoch".to_owned())
            })?
            .as_secs();
        let dirs = PluginDirs::create(&self.dirs, &identity.id)?;
        let result = Registry::open_at(&self.dirs.root, Limits::DEFAULT)
            .and_then(|registry| {
                registry.install(&InstallRequest {
                    archive: &candidate.bytes,
                    expected: &candidate.digest,
                    identity: identity.clone(),
                    source: PackageSource::ExplicitDigest,
                    enabled,
                    select: true,
                    installed_at_unix_seconds,
                })
            })
            .map(|report| (report, None));
        match result {
            Ok(installed) => Ok(installed),
            // The record replacement committed and only its durability is
            // unknown, so the plugin directories stay whatever the read-back
            // finds.
            Err(RegistryError::CommitUncertain) => self.recorded_after_commit(candidate),
            Err(error) => {
                dirs.roll_back(&identity.id);
                Err(by_id(error, &identity.id))
            }
        }
    }

    /// The report of an install whose record replacement committed without
    /// confirmed durability, read back from the registry.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InstallUncertain`] when the read-back fails or finds
    /// no record; the plugin directories are kept either way.
    fn recorded_after_commit(
        &self,
        candidate: &Candidate,
    ) -> Result<(InstallReport, Option<&'static str>), Error> {
        let uncertain = || Error::InstallUncertain {
            id: candidate.manifest.identity.id.to_string(),
        };
        let registry = self.existing().ok().flatten().ok_or_else(uncertain)?;
        let state = registry.state().map_err(|_unreadable| uncertain())?;
        let record = state
            .package(&candidate.digest)
            .cloned()
            .ok_or_else(uncertain)?;
        Ok((
            InstallReport {
                status: InstallStatus::Installed,
                record,
                generation: state.generation(),
            },
            Some(INSTALL_UNCERTAIN),
        ))
    }

    /// Enables or disables plugin `id` and returns its updated record.
    ///
    /// Enabling re-verifies the installed files first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PluginUnknown`], [`Error::Busy`],
    /// [`Error::RootInvalid`] or [`Error::Store`].
    pub(crate) fn set_enabled(
        &self,
        id: &PackageId,
        enabled: bool,
    ) -> Result<PackageRecord, Error> {
        let _lock = self.lock_existing()?.ok_or_else(|| unknown(id))?;
        let (registry, record) = self.require(id)?;
        registry
            .set_enabled(record.digest(), enabled)
            .map_err(|error| by_id(error, id))?;
        self.installed(id)?.ok_or_else(|| unknown(id))
    }

    /// Verifies plugin `id` and reads its manifest from the installed files.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PluginUnknown`], [`Error::RootInvalid`],
    /// [`Error::DescriptorInvalid`] or [`Error::Store`].
    pub(crate) fn inspect(
        &self,
        id: &PackageId,
    ) -> Result<(PackageRecord, VerifiedRoot, Manifest), Error> {
        let (registry, record) = self.require(id)?;
        let root = registry
            .verify(record.digest())
            .map_err(|error| by_id(error, id))?;
        let files = root.files();
        let find = |path: &str| {
            files
                .binary_search_by(|file| file.path.as_str().cmp(path))
                .ok()
                .map(|index| &files[index])
        };
        if find(MANIFEST_PATH).is_some_and(|file| {
            usize::try_from(file.size).map_or(true, |size| size > MAX_MANIFEST_BYTES)
        }) {
            return Err(manifest::Invalid::TooLarge.into());
        }
        let bytes = root.read_file(MANIFEST_PATH).map_err(|cause| match cause {
            VerifyError::UnknownPath => manifest::Invalid::Missing.into(),
            cause => Error::RootInvalid {
                id: id.to_string(),
                cause,
            },
        })?;
        let manifest = manifest::parse(&bytes, |path| find(path).map(|file| file.executable))?;
        Ok((record, root, manifest))
    }

    /// Applies the refusals of [`Store::uninstall`] that depend on the
    /// installed files, without changing anything: with `remove_modified`
    /// the files must fail verification for a reason other than being
    /// missing or unreadable, and without it they must verify or be missing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PluginUnknown`], [`Error::RootIntact`],
    /// [`Error::RootInvalid`] or [`Error::Store`], as [`Store::uninstall`]
    /// would.
    pub(crate) fn check_removable(
        &self,
        id: &PackageId,
        remove_modified: bool,
    ) -> Result<(), Error> {
        let (registry, record) = self.require(id)?;
        match (registry.verify(record.digest()), remove_modified) {
            (Ok(_) | Err(RegistryError::RootInvalid(VerifyError::RootMissing)), true) => {
                Err(Error::RootIntact { id: id.to_string() })
            }
            (Err(RegistryError::RootInvalid(VerifyError::Unreadable { kind })), true)
                if kind != ErrorKind::PermissionDenied =>
            {
                Err(Error::RootInvalid {
                    id: id.to_string(),
                    cause: VerifyError::Unreadable { kind },
                })
            }
            (Err(RegistryError::RootInvalid(_)), true)
            | (Ok(_) | Err(RegistryError::RootInvalid(VerifyError::RootMissing)), false) => Ok(()),
            (Err(error), _) => Err(by_id(error, id)),
        }
    }

    /// Uninstalls plugin `id` and, with `purge`, removes its state and
    /// config directories.
    ///
    /// `remove_modified` removes installed files that fail verification,
    /// which a plain uninstall leaves as found. With `purge`, an id without
    /// a record still has its leftover directories removed, and both
    /// directories are always attempted. A removal the registry committed but
    /// could not finish cleaning up, and a leftover of an earlier purge that
    /// still cannot be removed, are reported in [`Removal::warnings`]; neither
    /// decides the result.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PluginUnknown`], [`Error::Busy`],
    /// [`Error::RootInvalid`] (files left as found), [`Error::RootIntact`],
    /// [`Error::PurgeIncomplete`] naming where remaining data is, or
    /// [`Error::Store`].
    pub(crate) fn uninstall(
        &self,
        id: &PackageId,
        purge: bool,
        remove_modified: bool,
    ) -> Result<Removal, Error> {
        let lock = if purge {
            Some(self.lock_creating()?)
        } else {
            self.lock_existing()?
        };
        let Some(_lock) = lock else {
            return Err(unknown(id));
        };
        let record = self.installed(id)?;
        let mut warnings = Vec::new();
        match &record {
            None if !purge || remove_modified => return Err(unknown(id)),
            None => {}
            Some(record) => {
                let registry = self.existing()?.ok_or_else(|| unknown(id))?;
                let retained = RetainedDigests::new();
                let result = if remove_modified {
                    registry.remove_modified(record.digest(), &retained)
                } else {
                    registry.uninstall(record.digest(), &retained)
                };
                match result {
                    Ok(_generation) => {}
                    Err(RegistryError::RemovalIncomplete) => {
                        // `<root>/packages/<hex>/files` names the package roots'
                        // directory without repeating the registry layout.
                        let files = files_path(&self.dirs.root, record.digest());
                        let packages = files.parent().and_then(Path::parent).unwrap_or(&files);
                        warnings.push(format!(
                            "{REMOVAL_INCOMPLETE} {}; later store changes retry removing them",
                            packages.display()
                        ));
                    }
                    Err(RegistryError::CommitUncertain) => {
                        warnings.push(UNINSTALL_UNCERTAIN.to_owned());
                    }
                    Err(RegistryError::RootIntact) => {
                        return Err(Error::RootIntact { id: id.to_string() });
                    }
                    Err(error) => return Err(by_id(error, id)),
                }
            }
        }
        let purged = if purge {
            let state = purge_dir(&self.dirs.state, id, &mut warnings);
            let config = purge_dir(&self.dirs.config, id, &mut warnings);
            match (state, config) {
                (Ok(state), Ok(config)) => Some((state, config)),
                (state, config) => {
                    let describe = |name: &str, outcome: Result<Purged, String>| match outcome {
                        Ok(Purged::Removed) => format!("{name} directory removed"),
                        Ok(Purged::Absent) => format!("no {name} directory"),
                        Err(failure) => format!("{name} directory: {failure}"),
                    };
                    let mut detail = format!(
                        "{}; {}",
                        describe("state", state),
                        describe("config", config)
                    );
                    for warning in &warnings {
                        detail = format!("{detail}; separately, {warning}");
                    }
                    return Err(Error::PurgeIncomplete {
                        id: id.to_string(),
                        uninstalled: record.is_some(),
                        detail,
                    });
                }
            }
        } else {
            None
        };
        Ok(Removal {
            record,
            warnings,
            purged,
        })
    }

    fn existing(&self) -> Result<Option<Registry>, Error> {
        Registry::open_existing_at(&self.dirs.root, Limits::DEFAULT).map_err(store)
    }

    fn require(&self, id: &PackageId) -> Result<(Registry, PackageRecord), Error> {
        let registry = self.existing()?.ok_or_else(|| unknown(id))?;
        let record = self.installed(id)?.ok_or_else(|| unknown(id))?;
        Ok((registry, record))
    }

    /// Takes the unit lifecycle lock of an existing store; `None` when there
    /// is no store.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] while another enable, disable or uninstall
    /// holds it, and [`Error::Store`] when the store is unsafe.
    pub(crate) fn lock_units(&self) -> Result<Option<FileLock>, Error> {
        match TrustedDir::open_absolute(&self.dirs.root, PRIVATE_DIR_MODE) {
            Ok(root) => root
                .lock_file(UNITS_LOCK_NAME, LOCK_FILE_MODE, LockKind::Exclusive)
                .map(Some)
                .map_err(|error| match error {
                    FsError::LockContended { .. } => Error::Busy,
                    other => fs(&other),
                }),
            Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => Ok(None),
            Err(error) => Err(fs(&error)),
        }
    }

    /// Takes the unit lifecycle lock, creating the store root when it is
    /// missing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] while another enable, disable, uninstall or
    /// enabling install holds it, and [`Error::Store`] when the store is
    /// unsafe.
    pub(crate) fn lock_units_creating(&self) -> Result<FileLock, Error> {
        TrustedDir::open_or_create_absolute(&self.dirs.root, PRIVATE_DIR_MODE)
            .map_err(|error| fs(&error))?
            .lock_file(UNITS_LOCK_NAME, LOCK_FILE_MODE, LockKind::Exclusive)
            .map_err(|error| match error {
                FsError::LockContended { .. } => Error::Busy,
                other => fs(&other),
            })
    }

    /// The knobs file path.
    pub(crate) fn knobs_path(&self) -> &Path {
        &self.dirs.knobs
    }

    /// Takes the store lock, creating the store root when it is missing.
    fn lock_creating(&self) -> Result<FileLock, Error> {
        let root = TrustedDir::open_or_create_absolute(&self.dirs.root, PRIVATE_DIR_MODE)
            .map_err(|error| fs(&error))?;
        lock(&root)
    }

    /// Takes the store lock of an existing store; `None` when there is none.
    fn lock_existing(&self) -> Result<Option<FileLock>, Error> {
        match TrustedDir::open_absolute(&self.dirs.root, PRIVATE_DIR_MODE) {
            Ok(root) => lock(&root).map(Some),
            Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => Ok(None),
            Err(error) => Err(fs(&error)),
        }
    }
}

fn lock(root: &TrustedDir) -> Result<FileLock, Error> {
    root.lock_file(LOCK_NAME, LOCK_FILE_MODE, LockKind::Exclusive)
        .map_err(|error| match error {
            FsError::LockContended { .. } => Error::Busy,
            other => fs(&other),
        })
}

/// The plugin's state and config directories and whether this call created
/// each one.
struct PluginDirs {
    parents: [TrustedDir; 2],
    created: [bool; 2],
}

impl PluginDirs {
    /// Opens both exact-`0700` parents, creating them when missing, before it
    /// creates either plugin directory; a failure removes what this call
    /// created.
    fn create(dirs: &WorkflowDirs, id: &PackageId) -> Result<Self, Error> {
        let open = |path: &Path| {
            TrustedDir::open_or_create_absolute(path, PRIVATE_DIR_MODE).map_err(|error| fs(&error))
        };
        let mut created = Self {
            parents: [open(&dirs.state)?, open(&dirs.config)?],
            created: [false; 2],
        };
        for index in 0..created.parents.len() {
            let parent = &created.parents[index];
            let step = parent
                .entry_identity(id.as_str(), EntryKind::Directory)
                .and_then(|existing| {
                    parent
                        .open_or_create_child(id.as_str(), PRIVATE_DIR_MODE)
                        .map(|_dir| existing.is_none())
                });
            match step {
                Ok(new) => created.created[index] = new,
                Err(error) => {
                    created.roll_back(id);
                    return Err(fs(&error));
                }
            }
        }
        Ok(created)
    }

    /// Removes the plugin directories this call created; best effort, since
    /// the command already fails with the error that caused the rollback.
    fn roll_back(&self, id: &PackageId) {
        for (parent, created) in self.parents.iter().zip(self.created) {
            if created {
                let _ = remove_child(parent, id.as_str(), &purge_prefix(Some(id)));
            }
        }
    }
}

/// Removes the directory `id` below `parent`. `Err` describes the failure and
/// where data remains.
///
/// Leftovers of earlier purges in `parent` are retried first. A leftover of
/// `id` itself is this plugin's data, so one that still cannot be removed
/// fails the purge. A leftover of another id is retried best effort: if it
/// remains, it is added to `warnings` with its location and never decides
/// this plugin's outcome.
fn purge_dir(parent: &Path, id: &PackageId, warnings: &mut Vec<String>) -> Result<Purged, String> {
    let parent = match TrustedDir::open_absolute(parent, PRIVATE_DIR_MODE) {
        Ok(parent) => parent,
        Err(error) if error.io_kind() == Some(ErrorKind::NotFound) => return Ok(Purged::Absent),
        Err(error) => {
            return Err(format!(
                "{} cannot be opened safely: {}",
                parent.display(),
                fs_detail(&error)
            ));
        }
    };
    let mut own_failures = Vec::new();
    let mut own_removed = false;
    match parent.entry_names() {
        Ok(names) => {
            for name in names.iter().filter_map(|name| name.to_str()) {
                let Some(staged) = name.strip_prefix(PURGE_PREFIX) else {
                    continue;
                };
                let owner = staged
                    .rsplit_once(PURGE_ID_END)
                    .and_then(|(owner, _random)| PackageId::parse(owner).ok());
                let mine = owner.as_ref() == Some(id);
                // A leftover keeps its owner, or stays without one, when it is
                // staged again.
                match (
                    remove_child(&parent, name, &purge_prefix(owner.as_ref())),
                    mine,
                ) {
                    (Ok(_removed), true) => own_removed = true,
                    (Ok(_removed), false) => {}
                    (Err(failure), true) => own_failures.push(failure),
                    (Err(failure), false) => warnings.push(format!(
                        "a leftover of an earlier purge of another plugin remains: {failure}"
                    )),
                }
            }
        }
        Err(error) => own_failures.push(format!(
            "leftovers of earlier purges in {} were not checked: {}",
            parent.path().display(),
            fs_detail(&error)
        )),
    }
    let outcome = remove_child(&parent, id.as_str(), &purge_prefix(Some(id)));
    match outcome {
        Err(failure) => {
            own_failures.push(failure);
            Err(own_failures.join("; "))
        }
        Ok(_outcome) if !own_failures.is_empty() => Err(own_failures.join("; ")),
        Ok(Purged::Absent) if own_removed => Ok(Purged::Removed),
        Ok(outcome) => Ok(outcome),
    }
}

/// The staging prefix of a purge of `owner`: `.purge-<id>+`, or `.purge-`
/// for a leftover whose owner cannot be read.
fn purge_prefix(owner: Option<&PackageId>) -> String {
    match owner {
        Some(owner) => format!("{PURGE_PREFIX}{owner}{PURGE_ID_END}"),
        None => PURGE_PREFIX.to_owned(),
    }
}

/// Moves the directory `name` below `parent` to a name starting with
/// `prefix` and removes its tree. `Err` names where the data remains.
fn remove_child(parent: &TrustedDir, name: &str, prefix: &str) -> Result<Purged, String> {
    let original = parent.path().join(name);
    let identity = match parent.entry_identity(name, EntryKind::Directory) {
        Ok(Some(identity)) => identity,
        Ok(None) => return Ok(Purged::Absent),
        Err(error) => {
            return Err(format!(
                "{} was left in place: {}",
                original.display(),
                fs_detail(&error)
            ));
        }
    };
    let left_in_place =
        |detail: &str| format!("{} was left in place: {detail}", original.display());
    match parent.stage_random(name, prefix, identity) {
        Ok(StageOutcome::Staged(entry)) => {
            let staged = entry.path();
            match entry.remove_tree() {
                Ok(RemoveOutcome::Removed | RemoveOutcome::Missing) => Ok(Purged::Removed),
                Ok(_changed) => Err(format!(
                    "{} changed while it was removed; its data remains there",
                    staged.display()
                )),
                Err(error) => Err(format!(
                    "{}; its remaining data is at {}",
                    fs_detail(&error),
                    staged.display()
                )),
            }
        }
        Ok(StageOutcome::Missing) => Ok(Purged::Absent),
        Ok(_changed) => Err(left_in_place("it changed while it was purged")),
        Err(error) => Err(left_in_place(&fs_detail(&error))),
    }
}

fn unknown(id: &PackageId) -> Error {
    Error::PluginUnknown { id: id.to_string() }
}

/// Maps a registry failure about plugin `id`.
fn by_id(error: RegistryError, id: &PackageId) -> Error {
    match error {
        RegistryError::NotInstalled => unknown(id),
        RegistryError::IdentityConflict | RegistryError::IdentityInstalled => {
            Error::PluginInstalled { id: id.to_string() }
        }
        RegistryError::RootInvalid(cause) => Error::RootInvalid {
            id: id.to_string(),
            cause,
        },
        other => store(other),
    }
}

/// Registry errors carry no path or content, so their text is safe to show.
fn store(error: RegistryError) -> Error {
    match error {
        RegistryError::Busy => Error::Busy,
        other => Error::Store(other.to_string()),
    }
}

/// A store, state or config directory failure, without file content.
fn fs(error: &FsError) -> Error {
    Error::Store(fs_detail(error))
}

/// Describes a filesystem failure by its cause only.
fn fs_detail(error: &FsError) -> String {
    match error {
        FsError::UnsafeMode { .. } | FsError::UnsafeOwner { .. } | FsError::UnsafeAcl { .. } => {
            "a directory or file is not owned by you with owner-only permissions".to_owned()
        }
        FsError::UnsafeType { .. } => {
            "an entry is a symbolic link or special file where a plain directory or file belongs"
                .to_owned()
        }
        FsError::UnsafeLinkCount { .. } => "a file has more than one hard link".to_owned(),
        other => format!(
            "filesystem failure: {}",
            other.io_kind().unwrap_or(ErrorKind::Other)
        ),
    }
}
