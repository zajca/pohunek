//! Official-package trust: the host trust anchor, catalog verification and the
//! core and platform compatibility of a catalog entry.

// Rust guideline compliant 2026-10-04

use std::collections::BTreeSet;
use std::sync::Arc;

use package::catalog_state::CatalogState;
use package::{
    verify_catalog, Authorization, KeyId, PackageDigest, RootKey, TrustAnchor, TrustAnchorError,
    VerifiedCatalog,
};
use protocol::{PackageErrorKind, PackageIdentity, RuntimeId};
use semver::Version;
use tracing::warn;

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
            }),
        })
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
    fn this_host_has_a_platform_name_and_a_core_version() {
        assert!(host_platform().expect("a supported host").contains('-'));
        core_version().expect("the crate version is semver");
    }

    #[test]
    fn an_anchor_without_roots_is_refused() {
        assert_eq!(
            HostTrustAnchor::new(Vec::new(), Vec::new()).expect_err("no roots"),
            TrustAnchorError::NoRoots
        );
    }
}
