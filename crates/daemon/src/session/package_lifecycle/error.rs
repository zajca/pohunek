//! Maps registry, install and descriptor failures to the stable `package.*`
//! error kinds.

// Rust guideline compliant 2026-10-04

use package::install::InstallError;
use package::registry::RegistryError;
use package::verify::VerifyError;
use protocol::PackageErrorKind;

use crate::agent::host::PackageRejection;

/// The error kind of a registry failure.
///
/// A failure to read the package root says nothing about its content, so it is
/// a registry failure and never "root invalid".
#[must_use]
pub(super) fn registry_kind(error: &RegistryError) -> PackageErrorKind {
    match error {
        RegistryError::Busy => PackageErrorKind::Busy,
        RegistryError::Archive(_) | RegistryError::Install(InstallError::LimitsExceeded) => {
            PackageErrorKind::ArchiveInvalid
        }
        RegistryError::NotInstalled => PackageErrorKind::NotInstalled,
        RegistryError::IdentityConflict => PackageErrorKind::IdentityConflict,
        RegistryError::IdentityInstalled => PackageErrorKind::IdentityInstalled,
        RegistryError::StillReferenced => PackageErrorKind::Referenced,
        RegistryError::RootInvalid(VerifyError::Unreadable { .. }) => {
            PackageErrorKind::RegistryFailed
        }
        RegistryError::Install(
            InstallError::RootConflict | InstallError::ExistingRootInvalid(_),
        )
        | RegistryError::RootInvalid(_) => PackageErrorKind::RootInvalid,
        RegistryError::RootIntact => PackageErrorKind::RootIntact,
        RegistryError::TooManyPackages => PackageErrorKind::LimitReached,
        // Corruption, an unsupported schema, an unsafe directory, a failed
        // commit or removal, a filesystem failure and every variant this
        // build does not know.
        _ => PackageErrorKind::RegistryFailed,
    }
}

/// The error kind of a package that cannot be loaded.
#[must_use]
pub(super) fn rejection_kind(rejection: &PackageRejection) -> PackageErrorKind {
    if rejection.claims_shell() {
        return PackageErrorKind::RuntimeNotClaimable;
    }
    match rejection {
        PackageRejection::NotRegistered => PackageErrorKind::NotInstalled,
        PackageRejection::Root(VerifyError::Unreadable { .. }) => PackageErrorKind::RegistryFailed,
        PackageRejection::Root(_) => PackageErrorKind::RootInvalid,
        PackageRejection::IdentityMismatch
        | PackageRejection::DescriptorMissing
        | PackageRejection::Descriptor(_) => PackageErrorKind::DescriptorInvalid,
        PackageRejection::ReservedRuntimeId => PackageErrorKind::RuntimeNotClaimable,
        PackageRejection::RuntimeIdConflict => PackageErrorKind::RuntimeConflict,
        PackageRejection::Registry(error) => registry_kind(error),
        // A disabled package is not an install failure, and an unknown
        // rejection fails closed.
        _ => PackageErrorKind::RegistryFailed,
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind as IoKind;

    use package::ArchiveError;

    use super::*;
    use crate::agent::host::{DefinitionError, DefinitionInvariant};

    #[test]
    fn registry_errors_map_to_their_stable_kinds() {
        let table = [
            (RegistryError::Busy, PackageErrorKind::Busy),
            (RegistryError::Corrupt, PackageErrorKind::RegistryFailed),
            (
                RegistryError::UnsupportedSchema,
                PackageErrorKind::RegistryFailed,
            ),
            (RegistryError::Unsafe, PackageErrorKind::RegistryFailed),
            (
                RegistryError::Archive(ArchiveError::DigestMismatch),
                PackageErrorKind::ArchiveInvalid,
            ),
            (
                RegistryError::Install(InstallError::LimitsExceeded),
                PackageErrorKind::ArchiveInvalid,
            ),
            (
                RegistryError::Install(InstallError::RootConflict),
                PackageErrorKind::RootInvalid,
            ),
            (
                RegistryError::Install(InstallError::StagingExists),
                PackageErrorKind::RegistryFailed,
            ),
            (RegistryError::NotInstalled, PackageErrorKind::NotInstalled),
            (
                RegistryError::IdentityConflict,
                PackageErrorKind::IdentityConflict,
            ),
            (
                RegistryError::IdentityInstalled,
                PackageErrorKind::IdentityInstalled,
            ),
            (RegistryError::StillReferenced, PackageErrorKind::Referenced),
            (
                RegistryError::RootInvalid(VerifyError::Added),
                PackageErrorKind::RootInvalid,
            ),
            (
                RegistryError::RootInvalid(VerifyError::RootMissing),
                PackageErrorKind::RootInvalid,
            ),
            (
                RegistryError::RootInvalid(VerifyError::Unreadable {
                    kind: IoKind::PermissionDenied,
                }),
                PackageErrorKind::RegistryFailed,
            ),
            (RegistryError::RootIntact, PackageErrorKind::RootIntact),
            (
                RegistryError::TooManyRevokedKeys,
                PackageErrorKind::RegistryFailed,
            ),
            (
                RegistryError::TooManyPackages,
                PackageErrorKind::LimitReached,
            ),
            (
                RegistryError::CommitUncertain,
                PackageErrorKind::RegistryFailed,
            ),
            (
                RegistryError::RemovalIncomplete,
                PackageErrorKind::RegistryFailed,
            ),
            (
                RegistryError::Filesystem {
                    kind: IoKind::NotFound,
                },
                PackageErrorKind::RegistryFailed,
            ),
        ];
        for (error, kind) in table {
            assert_eq!(registry_kind(&error), kind, "{error:?}");
        }
    }

    #[test]
    fn rejections_map_to_their_stable_kinds() {
        let table = [
            (
                PackageRejection::NotRegistered,
                PackageErrorKind::NotInstalled,
            ),
            (
                PackageRejection::IdentityMismatch,
                PackageErrorKind::DescriptorInvalid,
            ),
            (
                PackageRejection::Root(VerifyError::Modified { entry: 0 }),
                PackageErrorKind::RootInvalid,
            ),
            (
                PackageRejection::Root(VerifyError::RootMissing),
                PackageErrorKind::RootInvalid,
            ),
            (
                PackageRejection::Root(VerifyError::Unreadable {
                    kind: IoKind::PermissionDenied,
                }),
                PackageErrorKind::RegistryFailed,
            ),
            (
                PackageRejection::DescriptorMissing,
                PackageErrorKind::DescriptorInvalid,
            ),
            (
                PackageRejection::Descriptor(DefinitionError::TooLarge),
                PackageErrorKind::DescriptorInvalid,
            ),
            (
                PackageRejection::ReservedRuntimeId,
                PackageErrorKind::RuntimeNotClaimable,
            ),
            (
                PackageRejection::RuntimeIdConflict,
                PackageErrorKind::RuntimeConflict,
            ),
            (
                PackageRejection::Registry(RegistryError::Busy),
                PackageErrorKind::Busy,
            ),
            (PackageRejection::Disabled, PackageErrorKind::RegistryFailed),
            (
                PackageRejection::Descriptor(DefinitionError::Invariant(
                    DefinitionInvariant::ShellRequiresHostShell,
                )),
                PackageErrorKind::RuntimeNotClaimable,
            ),
            (
                PackageRejection::Descriptor(DefinitionError::Invariant(
                    DefinitionInvariant::ShellIsPackageless,
                )),
                PackageErrorKind::RuntimeNotClaimable,
            ),
            (
                PackageRejection::Descriptor(DefinitionError::Invariant(
                    DefinitionInvariant::BuiltinRequiresPackage,
                )),
                PackageErrorKind::DescriptorInvalid,
            ),
        ];
        for (rejection, kind) in table {
            assert_eq!(rejection_kind(&rejection), kind, "{rejection:?}");
        }
    }
}
