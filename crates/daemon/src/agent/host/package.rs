//! Runtime definitions served by installed runtime packages.
//!
//! A package root holds a `runtime.toml` descriptor at its top level and the
//! detection manifest that descriptor names as a package-relative file. Every
//! byte the daemon reads from a root goes through
//! [`package::verify::VerifiedRoot`], which re-verifies the whole tree against
//! its recorded manifest first, so a definition is never built from content
//! the registry did not authenticate.
//!
//! [`PackageSource`] loads the packages fresh launches may use: the selected
//! digest of each package id, when enabled. [`PackageStore::load_pinned`]
//! loads exactly the digest a session was launched with, whatever is selected
//! or enabled now, because a pin must keep resolving to the content it froze.
//! Neither ever substitutes another package or a built-in.

// Rust guideline compliant 2026-10-04

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use package::registry::{IncompatibleReason, RegistryError, RetainedDigests, RetainedState};
use package::verify::{VerifiedRoot, VerifyError};
use package::{ArchiveEntry, Limits, PackageDigest};
use protocol::{BindingProvenance, PackageId, PackageIdentity, ProtocolError, RuntimeId};
use thiserror::Error;

use super::claim::is_reserved;
use super::definition::{
    DefinitionError, DefinitionInvariant, DefinitionOrigin, HandlerId, RuntimeDefinition,
    MAX_DEFINITION_BYTES,
};
use crate::detect::Manifest;

/// Package-relative path of the runtime descriptor inside every runtime
/// package.
pub const RUNTIME_DESCRIPTOR_PATH: &str = "runtime.toml";

/// Why an installed package cannot serve a runtime definition.
///
/// No variant carries a path or file content; definition errors name fields,
/// never values, so the type is safe to log and to show.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PackageRejection {
    /// The registry has no record of the digest.
    #[error("the package is not installed")]
    NotRegistered,
    /// The registry record names another package identity than expected.
    #[error("the package identity does not match its record")]
    IdentityMismatch,
    /// The package is installed but disabled for fresh launches.
    #[error("the package is disabled")]
    Disabled,
    /// The package root is missing or failed verification.
    #[error("the package root cannot be used: {0}")]
    Root(VerifyError),
    /// The package has no `runtime.toml`.
    #[error("the package has no runtime descriptor")]
    DescriptorMissing,
    /// The descriptor is not a valid runtime definition.
    #[error("the runtime descriptor is invalid: {0}")]
    Descriptor(DefinitionError),
    /// The package claims a runtime id only built-in runtimes may use.
    #[error("the package claims a reserved runtime id")]
    ReservedRuntimeId,
    /// Another loaded package claims the same runtime id.
    #[error("another package claims the same runtime id")]
    RuntimeIdConflict,
    /// The registry could not be read or verified.
    #[error("the package registry failed: {0}")]
    Registry(RegistryError),
}

impl PackageRejection {
    /// Whether the descriptor claims the shell runtime id.
    ///
    /// The shell is a built-in that launches the host login shell, so a
    /// package descriptor that names it fails the shell invariants before any
    /// id check runs.
    #[must_use]
    pub fn claims_shell(&self) -> bool {
        matches!(
            self,
            Self::Descriptor(DefinitionError::Invariant(
                DefinitionInvariant::ShellRequiresHostShell
                    | DefinitionInvariant::ShellIsPackageless
            ))
        )
    }

    /// The stable protocol error a refused runtime answers with.
    ///
    /// A digest the registry does not know is the same condition as an
    /// uninstalled runtime and resumes again once it is installed. Every other
    /// cause means installed content that cannot be trusted, which is the
    /// read-only incompatible state; the typed cause stays in the daemon log.
    #[must_use]
    pub fn to_protocol(&self, runtime_id: &RuntimeId) -> ProtocolError {
        match self {
            Self::NotRegistered | Self::Disabled => {
                ProtocolError::runtime_not_installed(runtime_id)
            }
            _ => ProtocolError::runtime_incompatible(runtime_id),
        }
    }
}

/// An installed package a load left out, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedPackage {
    /// Archive digest of the package.
    pub digest: PackageDigest,
    /// Identity the registry recorded for it.
    pub identity: PackageIdentity,
    /// Why it serves no runtime.
    pub reason: PackageRejection,
}

/// The outcome of loading every package fresh launches may use.
#[derive(Debug, Clone, Default)]
pub struct PackageLoad {
    /// Definitions of the packages that verified and parsed.
    pub definitions: Vec<RuntimeDefinition>,
    /// Packages that were selected and enabled but cannot serve a runtime.
    pub rejected: Vec<RejectedPackage>,
}

/// The daemon's handle over the runtime package registry.
#[derive(Debug, Clone)]
pub struct PackageStore {
    registry: Arc<package::registry::Registry>,
}

impl PackageStore {
    /// Opens or creates the owner-private plugin root at the absolute path
    /// `plugins_dir`.
    ///
    /// # Errors
    ///
    /// Returns the registry error when the directory or its `packages`
    /// subdirectory is unsafe (not exact `0700`, not owned by the daemon user,
    /// or reached through a link) or cannot be created.
    pub fn open(plugins_dir: &Path) -> Result<Self, RegistryError> {
        package::registry::Registry::open_at(plugins_dir, Limits::DEFAULT).map(Self::new)
    }

    /// Opens the plugin root at `plugins_dir` only if it exists, creating
    /// nothing; `None` is a host that never installed a package.
    ///
    /// # Errors
    ///
    /// Returns the registry error when an existing directory is unsafe.
    pub fn open_existing(plugins_dir: &Path) -> Result<Option<Self>, RegistryError> {
        package::registry::Registry::open_existing_at(plugins_dir, Limits::DEFAULT)
            .map(|registry| registry.map(Self::new))
    }

    /// Wraps an opened registry.
    #[must_use]
    pub fn new(registry: package::registry::Registry) -> Self {
        Self {
            registry: Arc::new(registry),
        }
    }

    /// The underlying registry.
    #[must_use]
    pub fn registry(&self) -> &package::registry::Registry {
        &self.registry
    }

    /// Verifies the root of `digest` and returns it only when it is ready.
    fn ready_root(&self, digest: &PackageDigest) -> Result<VerifiedRoot, PackageRejection> {
        let referenced = RetainedDigests::from_iter([digest.clone()]);
        let roots = self
            .registry
            .retained_roots(&referenced)
            .map_err(PackageRejection::Registry)?;
        let Some(root) = roots.into_iter().next() else {
            return Err(PackageRejection::NotRegistered);
        };
        match root.state {
            RetainedState::Ready(root) => Ok(root),
            RetainedState::Incompatible(IncompatibleReason::NotRegistered) => {
                Err(PackageRejection::NotRegistered)
            }
            RetainedState::Incompatible(IncompatibleReason::RootMissing) => {
                Err(PackageRejection::Root(VerifyError::RootMissing))
            }
            RetainedState::Incompatible(IncompatibleReason::RootInvalid(error)) => {
                Err(PackageRejection::Root(error))
            }
            // A state this build does not know is unusable, never ready.
            _ => Err(PackageRejection::Root(VerifyError::Unreadable {
                kind: ErrorKind::Other,
            })),
        }
    }

    /// Verifies that the package of `digest` may serve a fresh launch: it is
    /// recorded under `identity`, enabled, and its root verifies.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageRejection`] naming the first failed condition.
    pub fn verify_launchable(
        &self,
        digest: &PackageDigest,
        identity: &PackageIdentity,
    ) -> Result<(), PackageRejection> {
        let record = self.record(digest, identity)?;
        if !record.enabled() {
            return Err(PackageRejection::Disabled);
        }
        self.ready_root(digest).map(|_root| ())
    }

    /// Whether the registry records `digest` under `identity`, enabled or not.
    ///
    /// Reads the registry record only; it does not verify the package root.
    #[must_use]
    pub fn is_recorded(&self, digest: &PackageDigest, identity: &PackageIdentity) -> bool {
        self.record(digest, identity).is_ok()
    }

    /// The record of `digest`, checked against the expected identity.
    fn record(
        &self,
        digest: &PackageDigest,
        identity: &PackageIdentity,
    ) -> Result<package::registry::PackageRecord, PackageRejection> {
        let state = self.registry.state().map_err(PackageRejection::Registry)?;
        let record = state
            .package(digest)
            .ok_or(PackageRejection::NotRegistered)?;
        if record.identity() == identity {
            Ok(record.clone())
        } else {
            Err(PackageRejection::IdentityMismatch)
        }
    }

    /// Loads the definition of exactly the package `digest`, recorded under
    /// `identity`, whether or not it is enabled or selected.
    ///
    /// The definition must name `runtime_id`. A session pinned to a package
    /// resolves through this method, so a newer selected version, a disabled
    /// package or a built-in with the same runtime id never stands in.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageRejection`] when the package is not installed,
    /// its root fails verification, or the descriptor does not describe
    /// `runtime_id` under `identity`.
    pub fn load_pinned(
        &self,
        runtime_id: &RuntimeId,
        digest: &PackageDigest,
        identity: &PackageIdentity,
    ) -> Result<RuntimeDefinition, PackageRejection> {
        self.record(digest, identity)?;
        let definition = self.load_definition(digest, identity)?;
        if definition.runtime_id() == runtime_id {
            Ok(definition)
        } else {
            Err(PackageRejection::IdentityMismatch)
        }
    }

    /// Loads the definition of exactly the package `digest` for a host
    /// profile that binds it by package id and digest.
    ///
    /// The record found by `digest` must belong to the package `package`;
    /// the version is whatever the record holds, because a profile pins the
    /// archive digest and not a version label. The rest is
    /// [`Self::load_pinned`]: enablement and selection are not consulted.
    ///
    /// # Errors
    ///
    /// Returns [`PackageRejection::NotRegistered`] for an unknown digest,
    /// [`PackageRejection::IdentityMismatch`] when the digest belongs to
    /// another package or runtime, and the other rejections of
    /// [`Self::load_pinned`].
    pub fn load_bound(
        &self,
        runtime_id: &RuntimeId,
        digest: &PackageDigest,
        package: &PackageId,
    ) -> Result<RuntimeDefinition, PackageRejection> {
        let state = self.registry.state().map_err(PackageRejection::Registry)?;
        let record = state
            .package(digest)
            .ok_or(PackageRejection::NotRegistered)?;
        if record.identity().id != *package {
            return Err(PackageRejection::IdentityMismatch);
        }
        let identity = record.identity().clone();
        self.load_pinned(runtime_id, digest, &identity)
    }

    /// Verifies the root of `digest` and builds its definition, whatever
    /// runtime id it claims.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageRejection`] when the package is not installed, its
    /// root fails verification, the descriptor is missing or invalid, or it
    /// names another package identity than `identity`.
    pub fn read_definition(
        &self,
        digest: &PackageDigest,
        identity: &PackageIdentity,
    ) -> Result<RuntimeDefinition, PackageRejection> {
        let root = self.ready_root(digest)?;
        let definition = definition_from_files(&RootFiles(&root), digest)?;
        match &definition.binding().provenance {
            BindingProvenance::Package { package, .. } if package == identity => Ok(definition),
            _ => Err(PackageRejection::IdentityMismatch),
        }
    }

    /// The integration handler a recorded package's descriptor names, read
    /// from a verified root without requiring the current descriptor shape.
    ///
    /// A descriptor written before `[integration] hook_schema` existed names
    /// only the handler and no longer parses as a definition, yet a worker
    /// launched under it can still be live. Only the package identity and the
    /// handler id are read; nothing else of the descriptor is trusted.
    ///
    /// # Errors
    ///
    /// Returns the [`PackageRejection`] when the package is not installed
    /// under `identity`, its root fails verification, or the descriptor is
    /// missing, oversized, malformed, or names another package identity.
    pub fn read_legacy_integration_handler(
        &self,
        digest: &PackageDigest,
        identity: &PackageIdentity,
    ) -> Result<Option<HandlerId>, PackageRejection> {
        self.record(digest, identity)?;
        let root = self.ready_root(digest)?;
        let files = RootFiles(&root);
        let size = files
            .size(RUNTIME_DESCRIPTOR_PATH)
            .ok_or(PackageRejection::DescriptorMissing)?;
        if size > u64::try_from(MAX_DEFINITION_BYTES).unwrap_or(u64::MAX) {
            return Err(PackageRejection::Descriptor(DefinitionError::TooLarge));
        }
        let bytes = files
            .read(RUNTIME_DESCRIPTOR_PATH)
            .map_err(PackageRejection::Root)?;
        let malformed = || PackageRejection::Descriptor(DefinitionError::Malformed { line: None });
        let text = std::str::from_utf8(&bytes).map_err(|_error| malformed())?;
        let raw: LegacyDescriptor = toml::from_str(text).map_err(|_error| malformed())?;
        let declared = PackageIdentity {
            id: PackageId::parse(&raw.id).map_err(|_error| malformed())?,
            version: protocol::PackageVersion::parse(&raw.version).map_err(|_error| malformed())?,
        };
        if declared != *identity {
            return Err(PackageRejection::IdentityMismatch);
        }
        raw.integration
            .map(|integration| HandlerId::parse(&integration.handler, "integration.handler"))
            .transpose()
            .map_err(PackageRejection::Descriptor)
    }

    /// [`Self::read_definition`] for a package a fresh launch may use: a
    /// reserved runtime id is refused outright.
    fn load_definition(
        &self,
        digest: &PackageDigest,
        identity: &PackageIdentity,
    ) -> Result<RuntimeDefinition, PackageRejection> {
        let definition = self.read_definition(digest, identity)?;
        if is_reserved(definition.runtime_id()) {
            return Err(PackageRejection::ReservedRuntimeId);
        }
        Ok(definition)
    }
}

/// Builds the definition a package archive declares, from the archive entries
/// alone.
///
/// Installation validates a package with this before anything is extracted or
/// recorded, so the daemon never records a package whose descriptor it would
/// reject. It runs the same parser as [`PackageStore::read_definition`]. The
/// returned definition's binding carries the package identity the descriptor
/// declares.
///
/// # Errors
///
/// Returns [`PackageRejection::DescriptorMissing`] without a `runtime.toml`,
/// [`PackageRejection::ReservedRuntimeId`] for a descriptor that claims the
/// shell, and [`PackageRejection::Descriptor`] for every other invalid
/// descriptor.
pub fn definition_from_archive(
    entries: &[ArchiveEntry],
    digest: &PackageDigest,
) -> Result<RuntimeDefinition, PackageRejection> {
    definition_from_files(&ArchiveFiles(entries), digest)
}

/// Read access to the files of a package, either a verified root on disk or
/// the entries of a verified archive in memory.
trait PackageFiles {
    /// Size of the file at `path`, `None` when the package has no such file.
    fn size(&self, path: &str) -> Option<u64>;

    /// Contents of the file at `path`.
    fn read(&self, path: &str) -> Result<Vec<u8>, VerifyError>;
}

/// The files of a verified root; every read re-verifies the tree.
struct RootFiles<'a>(&'a VerifiedRoot);

impl PackageFiles for RootFiles<'_> {
    fn size(&self, path: &str) -> Option<u64> {
        self.0
            .files()
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.size)
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, VerifyError> {
        self.0.read_file(path)
    }
}

/// The entries of an archive the package crate already verified.
struct ArchiveFiles<'a>(&'a [ArchiveEntry]);

impl PackageFiles for ArchiveFiles<'_> {
    fn size(&self, path: &str) -> Option<u64> {
        self.0
            .iter()
            .find(|entry| entry.path == path)
            .and_then(|entry| u64::try_from(entry.contents.len()).ok())
    }

    fn read(&self, path: &str) -> Result<Vec<u8>, VerifyError> {
        self.0
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.contents.clone())
            .ok_or(VerifyError::UnknownPath)
    }
}

/// The fields of a runtime descriptor the legacy handler lookup reads; every
/// other key is ignored.
#[derive(serde::Deserialize)]
struct LegacyDescriptor {
    id: String,
    version: String,
    integration: Option<LegacyIntegration>,
}

#[derive(serde::Deserialize)]
struct LegacyIntegration {
    handler: String,
}

/// Parses the descriptor of a package.
fn definition_from_files(
    files: &impl PackageFiles,
    digest: &PackageDigest,
) -> Result<RuntimeDefinition, PackageRejection> {
    let size = files
        .size(RUNTIME_DESCRIPTOR_PATH)
        .ok_or(PackageRejection::DescriptorMissing)?;
    if size > u64::try_from(MAX_DEFINITION_BYTES).unwrap_or(u64::MAX) {
        return Err(PackageRejection::Descriptor(DefinitionError::TooLarge));
    }
    let bytes = files
        .read(RUNTIME_DESCRIPTOR_PATH)
        .map_err(PackageRejection::Root)?;
    let text = std::str::from_utf8(&bytes).map_err(|_error| {
        PackageRejection::Descriptor(DefinitionError::Malformed { line: None })
    })?;
    RuntimeDefinition::from_toml(
        text,
        |package| DefinitionOrigin::Package {
            package,
            digest: digest.clone(),
        },
        |name| detection_manifest(files, name),
    )
    .map_err(PackageRejection::Descriptor)
}

/// Resolves the `detect_manifest` a package descriptor names: a package file,
/// parsed as a detection manifest.
fn detection_manifest(
    files: &impl PackageFiles,
    name: &str,
) -> Result<Arc<Manifest>, DefinitionError> {
    let bytes = files.read(name).map_err(|error| match error {
        VerifyError::UnknownPath => DefinitionError::UnknownManifest,
        _ => DefinitionError::Field {
            field: "detect_manifest",
            reason: "could not be read from the verified package",
        },
    })?;
    let invalid = || DefinitionError::Field {
        field: "detect_manifest",
        reason: "is not a valid detection manifest",
    };
    let text = std::str::from_utf8(&bytes).map_err(|_error| invalid())?;
    Manifest::parse_str(text)
        .map(Arc::new)
        .map_err(|_error| invalid())
}

/// Supplies the runtimes of installed, enabled and selected packages.
#[derive(Debug, Clone)]
pub struct PackageSource {
    store: PackageStore,
}

impl PackageSource {
    /// Creates a source over `store`.
    #[must_use]
    pub fn new(store: PackageStore) -> Self {
        Self { store }
    }

    /// The store the source reads.
    #[must_use]
    pub fn store(&self) -> &PackageStore {
        &self.store
    }

    /// Loads the definition of every selected, enabled package.
    ///
    /// Each package root is re-verified before its descriptor is read. A
    /// package that fails is reported in [`PackageLoad::rejected`] and left
    /// out; it never fails the load of another package. A runtime id two
    /// packages claim is refused for both, so the outcome does not depend on
    /// install order, and a reserved id is refused outright.
    ///
    /// # Errors
    ///
    /// Returns the registry error when the registry record itself cannot be
    /// read; no package is loaded then.
    pub fn load_report(&self) -> Result<PackageLoad, RegistryError> {
        let state = self.store.registry.state()?;
        let mut loaded: Vec<(PackageDigest, PackageIdentity, RuntimeDefinition)> = Vec::new();
        let mut rejected = Vec::new();
        for record in state.packages() {
            let selected = state.selected(&record.identity().id) == Some(record.digest());
            if !record.enabled() || !selected {
                continue;
            }
            match self
                .store
                .load_definition(record.digest(), record.identity())
            {
                Ok(definition) => {
                    loaded.push((
                        record.digest().clone(),
                        record.identity().clone(),
                        definition,
                    ));
                }
                Err(reason) => rejected.push(RejectedPackage {
                    digest: record.digest().clone(),
                    identity: record.identity().clone(),
                    reason,
                }),
            }
        }
        let mut claims: BTreeMap<RuntimeId, usize> = BTreeMap::new();
        for (_digest, _identity, definition) in &loaded {
            *claims.entry(definition.runtime_id().clone()).or_default() += 1;
        }
        let mut definitions = Vec::with_capacity(loaded.len());
        for (digest, identity, definition) in loaded {
            if claims.get(definition.runtime_id()).copied().unwrap_or(0) > 1 {
                rejected.push(RejectedPackage {
                    digest,
                    identity,
                    reason: PackageRejection::RuntimeIdConflict,
                });
            } else {
                definitions.push(definition);
            }
        }
        Ok(PackageLoad {
            definitions,
            rejected,
        })
    }
}
