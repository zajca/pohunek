//! Revoked package digests in the package service: the flag on an installed
//! package, the refusal to select, enable or pin one, and the doctor finding.

// Rust guideline compliant 2026-11-10

use package::registry::{PackageRecord, PackageSource, RegistryState};
use package::revoked_digests::RevokedDigests;
use package::PackageDigest;
use protocol::PackageErrorKind;
use tracing::warn;

use super::error::registry_kind;
use super::Context;

/// The digests recorded catalogs revoked.
///
/// # Errors
///
/// Returns the kind of a revocation file that cannot be read.
pub(super) fn revoked_set(context: &Context) -> Result<RevokedDigests, PackageErrorKind> {
    context
        .store
        .registry()
        .revoked_digests()
        .map_err(|error| registry_kind(&error))
}

/// Whether `record` is a catalog-authorized package whose digest `revoked`
/// holds.
///
/// Explicit-digest and linked packages are the owner's own trust decision and
/// are never revoked by a catalog.
pub(super) fn is_revoked(revoked: &RevokedDigests, record: &PackageRecord) -> bool {
    record.source() == PackageSource::Official && revoked.contains(record.digest())
}

/// Refuses to enable, select or pin the installed package `digest` when a
/// recorded catalog revoked it.
///
/// # Errors
///
/// Returns [`PackageErrorKind::Revoked`] for a revoked digest and the registry
/// kind when the revocation file cannot be read.
pub(super) fn require_not_revoked(
    context: &Context,
    state: &RegistryState,
    digest: &PackageDigest,
) -> Result<(), PackageErrorKind> {
    let Some(record) = state.package(digest) else {
        return Ok(());
    };
    let revoked = revoked_set(context)?;
    if is_revoked(&revoked, record) {
        warn!(%digest, "a package was refused: a recorded catalog revoked its digest");
        return Err(PackageErrorKind::Revoked);
    }
    Ok(())
}
