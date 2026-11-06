//! Official-package trust: the host trust anchor, catalog verification and the
//! core and platform compatibility of a catalog entry.

// Rust guideline compliant 2026-10-13

use std::collections::BTreeSet;
use std::sync::Arc;

use package::catalog_state::CatalogState;
use package::{
    verify_catalog, verify_index, Authorization, IndexEntryRefusal, IndexVerifierState, KeyId,
    PackageDigest, RootKey, TrustAnchor, TrustAnchorError, VerifiedCatalog, VerifiedIndex,
};
use protocol::{PackageErrorKind, PackageIdentity, RuntimeId};
use semver::Version;
use tracing::warn;

use crate::catalog_anchor::IndexTrust;

/// Core version a catalog entry's compatibility range is checked against.
const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Operating system names of [`std::env::consts::OS`] the catalog platform
/// names cover.
const OS_LINUX: &str = "linux";
const OS_MACOS: &str = "macos";

/// Vendor and system part of a Linux platform name.
const LINUX_PLATFORM_SYSTEM: &str = "unknown-linux";

/// C library part of a Linux platform name.
const LINUX_GLIBC: &str = "gnu";
const LINUX_MUSL: &str = "musl";

/// Vendor and system part of a macOS platform name.
const MACOS_PLATFORM_SYSTEM: &str = "apple-darwin";

/// The root keys and revoked key ids the host trusts independently of any
/// catalog.
///
/// The inputs are kept so a fresh anchor can be built for every verification:
/// the key ids revoked by catalogs recorded earlier join the base revocations
/// each time. Two values are equal when they are the same anchor.
#[derive(Debug, Clone)]
pub struct HostTrustAnchor {
    inputs: Arc<AnchorInputs>,
}

#[derive(Debug)]
struct AnchorInputs {
    roots: Vec<RootKey>,
    revoked: BTreeSet<KeyId>,
    index: IndexTrust,
}

impl HostTrustAnchor {
    /// Builds an anchor from root keys and the key ids revoked so far.
    ///
    /// # Errors
    ///
    /// Returns the [`TrustAnchorError`] of [`TrustAnchor::new`] for no roots,
    /// too many roots or revoked ids, or a repeated root.
    pub fn new(roots: Vec<RootKey>, revoked: Vec<KeyId>) -> Result<Self, TrustAnchorError> {
        TrustAnchor::new(roots.clone(), revoked.clone())?;
        Ok(Self {
            inputs: Arc::new(AnchorInputs {
                roots,
                revoked: revoked.into_iter().collect(),
                index: IndexTrust::Absent,
            }),
        })
    }

    /// The anchor with `index` as its plugin index channel.
    #[must_use]
    pub fn with_index(self, index: IndexTrust) -> Self {
        Self {
            inputs: Arc::new(AnchorInputs {
                roots: self.inputs.roots.clone(),
                revoked: self.inputs.revoked.clone(),
                index,
            }),
        }
    }

    /// What the host trusts to authorize plugin index installs.
    #[must_use]
    pub fn index(&self) -> &IndexTrust {
        &self.inputs.index
    }

    /// The anchor with the base revocations plus `persisted`.
    fn anchor(&self, persisted: &BTreeSet<KeyId>) -> Result<TrustAnchor, TrustAnchorError> {
        let revoked = self
            .inputs
            .revoked
            .union(persisted)
            .cloned()
            .collect::<Vec<_>>();
        TrustAnchor::new(self.inputs.roots.clone(), revoked)
    }
}

impl PartialEq for HostTrustAnchor {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inputs, &other.inputs)
    }
}

impl Eq for HostTrustAnchor {}

/// Verifies a catalog document against the host anchor, the persisted
/// high-water mark and the persisted revoked key ids.
///
/// # Errors
///
/// Returns [`PackageErrorKind::Untrusted`] for a catalog that fails
/// verification and [`PackageErrorKind::RegistryFailed`] when the persisted
/// revocations no longer fit an anchor.
pub(super) fn verify_catalog_document(
    bytes: &[u8],
    anchor: &HostTrustAnchor,
    state: &CatalogState,
    now: u64,
) -> Result<VerifiedCatalog, PackageErrorKind> {
    let anchor = anchor.anchor(state.revoked_key_ids()).map_err(|error| {
        warn!(%error, "the catalog trust anchor cannot absorb the persisted revocations");
        PackageErrorKind::RegistryFailed
    })?;
    verify_catalog(bytes, &anchor, now, state.high_water()).map_err(|error| {
        warn!(%error, "a package catalog was refused");
        PackageErrorKind::Untrusted
    })
}

/// Verifies a plugin index document against the channel's `endorser`, the
/// persisted marks and the key ids revoked by the anchor, by catalogs
/// recorded earlier and by indexes recorded earlier.
///
/// # Errors
///
/// Returns [`PackageErrorKind::Untrusted`] for an index that fails
/// verification.
pub(super) fn verify_index_document(
    bytes: &[u8],
    endorser: &RootKey,
    anchor: &HostTrustAnchor,
    catalog_state: &CatalogState,
    state: &IndexVerifierState,
    now: u64,
) -> Result<VerifiedIndex, PackageErrorKind> {
    let base_revoked: BTreeSet<KeyId> = anchor
        .inputs
        .revoked
        .union(catalog_state.revoked_key_ids())
        .cloned()
        .collect();
    verify_index(bytes, endorser, &base_revoked, state, now).map_err(|error| {
        warn!(%error, "a plugin index was refused");
        PackageErrorKind::Untrusted
    })
}

/// Checks that the verified index lists the raw archive `digest`.
///
/// The archive is decompressed only after this check passes.
///
/// # Errors
///
/// Returns [`PackageErrorKind::Untrusted`] when no entry carries `digest`.
pub(super) fn require_indexed(
    index: &VerifiedIndex,
    digest: &PackageDigest,
) -> Result<(), PackageErrorKind> {
    if index.lists(digest) {
        Ok(())
    } else {
        warn!("a package archive was refused: the plugin index does not list its digest");
        Err(PackageErrorKind::Untrusted)
    }
}

/// Checks that the verified index authorizes the decoded package and that its
/// entry supports this core version and platform.
///
/// # Errors
///
/// Returns [`PackageErrorKind::RuntimeNotClaimable`] for a runtime id an index
/// never authorizes, [`PackageErrorKind::Incompatible`] when the entry does
/// not support `core` or `platform`, and [`PackageErrorKind::Untrusted`] for
/// every other refusal.
pub(super) fn authorize_indexed(
    index: &VerifiedIndex,
    identity: &PackageIdentity,
    runtime_id: &RuntimeId,
    digest: &PackageDigest,
    core: &Version,
    platform: &str,
) -> Result<(), PackageErrorKind> {
    index
        .authorize(
            &identity.id,
            runtime_id,
            &identity.version,
            digest,
            core,
            platform,
        )
        .map_err(|refusal| {
            warn!(%refusal, "a package was refused by the plugin index");
            match refusal {
                IndexEntryRefusal::RuntimeRefused => PackageErrorKind::RuntimeNotClaimable,
                IndexEntryRefusal::CoreRange
                | IndexEntryRefusal::CoreExcluded
                | IndexEntryRefusal::Platform => PackageErrorKind::Incompatible,
                _ => PackageErrorKind::Untrusted,
            }
        })
}

/// Checks that the verified catalog lists the raw archive `digest` and does
/// not revoke it.
///
/// The archive is decompressed only after this check passes. The package and
/// runtime ids come from the descriptor inside the archive, so
/// [`authorize_official`] completes the decision after decoding.
///
/// # Errors
///
/// Returns [`PackageErrorKind::Untrusted`] when no entry carries `digest` or
/// the catalog revokes it.
pub(super) fn require_listed(
    catalog: &VerifiedCatalog,
    digest: &PackageDigest,
) -> Result<(), PackageErrorKind> {
    let listed = !catalog.is_digest_revoked(digest)
        && catalog
            .entries()
            .iter()
            .any(|entry| entry.digest() == digest);
    if listed {
        Ok(())
    } else {
        Err(PackageErrorKind::Untrusted)
    }
}

/// Checks that the verified catalog authorizes the package as official and
/// that its entry supports this core version and platform.
///
/// # Errors
///
/// Returns [`PackageErrorKind::Untrusted`] when no entry binds the package id,
/// runtime id and digest, and [`PackageErrorKind::Incompatible`] when the
/// entry does not support `core` or `platform`.
pub(super) fn authorize_official(
    catalog: &VerifiedCatalog,
    identity: &PackageIdentity,
    runtime_id: &RuntimeId,
    digest: &PackageDigest,
    core: &Version,
    platform: &str,
) -> Result<(), PackageErrorKind> {
    if catalog.authorize(&identity.id, runtime_id, digest) != Authorization::Official {
        return Err(PackageErrorKind::Untrusted);
    }
    let entry = catalog
        .entries()
        .iter()
        .find(|entry| {
            entry.digest() == digest
                && entry.package_id() == &identity.id
                && entry.runtime_id() == runtime_id
        })
        .ok_or(PackageErrorKind::Untrusted)?;
    if entry.version() != &identity.version {
        return Err(PackageErrorKind::Untrusted);
    }
    if entry.supports_core(core) && entry.supports_platform(platform) {
        Ok(())
    } else {
        Err(PackageErrorKind::Incompatible)
    }
}

/// The core version of this daemon.
///
/// # Errors
///
/// Returns [`PackageErrorKind::RegistryFailed`] when the crate version is not
/// valid semver, which a build defect would cause.
pub(super) fn core_version() -> Result<Version, PackageErrorKind> {
    Version::parse(CORE_VERSION).map_err(|error| {
        warn!(%error, "the daemon version is not valid semver");
        PackageErrorKind::RegistryFailed
    })
}

/// The platform name of this host in the catalog's target-triple style, such
/// as `x86_64-unknown-linux-gnu`.
///
/// # Errors
///
/// Returns [`PackageErrorKind::Incompatible`] on a platform the catalog does
/// not name.
pub(super) fn host_platform() -> Result<String, PackageErrorKind> {
    platform_name(
        std::env::consts::ARCH,
        std::env::consts::OS,
        cfg!(target_env = "musl"),
    )
    .ok_or(PackageErrorKind::Incompatible)
}

/// Builds the catalog platform name from the architecture and operating system
/// names of [`std::env::consts`] and whether the C library is musl.
fn platform_name(arch: &str, os: &str, musl: bool) -> Option<String> {
    match os {
        OS_LINUX => {
            let libc = if musl { LINUX_MUSL } else { LINUX_GLIBC };
            Some(format!("{arch}-{LINUX_PLATFORM_SYSTEM}-{libc}"))
        }
        OS_MACOS => Some(format!("{arch}-{MACOS_PLATFORM_SYSTEM}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_names_follow_the_catalog_style() {
        assert_eq!(
            platform_name("x86_64", "linux", false).as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            platform_name("x86_64", "linux", true).as_deref(),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            platform_name("aarch64", "linux", false).as_deref(),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            platform_name("aarch64", "macos", false).as_deref(),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(platform_name("x86_64", "windows", false), None);
    }

    #[test]
    fn an_anchor_without_roots_is_refused() {
        assert_eq!(
            HostTrustAnchor::new(Vec::new(), Vec::new()).expect_err("no roots"),
            TrustAnchorError::NoRoots
        );
    }
}
