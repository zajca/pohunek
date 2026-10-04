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
use package::{Limits, PackageDigest};
use protocol::{BindingProvenance, PackageIdentity, ProtocolError, RuntimeId};
use thiserror::Error;

use super::definition::{
    DefinitionError, DefinitionOrigin, RuntimeDefinition, MAX_DEFINITION_BYTES,
};
use super::registry::RESERVED_RUNTIME_IDS;
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

    /// Verifies the root of `digest` and builds its definition.
    fn load_definition(
        &self,
        digest: &PackageDigest,
        identity: &PackageIdentity,
    ) -> Result<RuntimeDefinition, PackageRejection> {
        let root = self.ready_root(digest)?;
        let definition = definition_from_root(&root, digest)?;
        match &definition.binding().provenance {
            BindingProvenance::Package { package, .. } if package == identity => {}
            _ => return Err(PackageRejection::IdentityMismatch),
        }
        if RESERVED_RUNTIME_IDS.contains(&definition.runtime_id().as_str()) {
            return Err(PackageRejection::ReservedRuntimeId);
        }
        Ok(definition)
    }
}

/// Parses the descriptor of a verified root.
fn definition_from_root(
    root: &VerifiedRoot,
    digest: &PackageDigest,
) -> Result<RuntimeDefinition, PackageRejection> {
    let size = root
        .files()
        .iter()
        .find(|file| file.path == RUNTIME_DESCRIPTOR_PATH)
        .map(|file| file.size)
        .ok_or(PackageRejection::DescriptorMissing)?;
    if size > u64::try_from(MAX_DEFINITION_BYTES).unwrap_or(u64::MAX) {
        return Err(PackageRejection::Descriptor(DefinitionError::TooLarge));
    }
    let bytes = root
        .read_file(RUNTIME_DESCRIPTOR_PATH)
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
        |name| detection_manifest(root, name),
    )
    .map_err(PackageRejection::Descriptor)
}

/// Resolves the `detect_manifest` a package descriptor names: a package file,
/// read through the verified root, parsed as a detection manifest.
fn detection_manifest(root: &VerifiedRoot, name: &str) -> Result<Arc<Manifest>, DefinitionError> {
    let bytes = root.read_file(name).map_err(|error| match error {
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
