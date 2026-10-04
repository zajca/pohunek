//! Maps a package that cannot serve its runtime to the typed wire fault.

// Rust guideline compliant 2026-10-04

use package::verify::VerifyError;
use protocol::PackageFault;

use crate::agent::host::PackageRejection;

/// The fault a rejection reports, or `None` when it is not a fault of the
/// package (an unknown digest, a package the owner disabled).
///
/// Verification failures a build does not know are reported as unreadable: a
/// package is never reported healthy because its failure was not recognised.
#[must_use]
pub(super) fn fault_of(rejection: &PackageRejection) -> Option<PackageFault> {
    if rejection.claims_shell() {
        return Some(PackageFault::RuntimeNotClaimable);
    }
    match rejection {
        PackageRejection::NotRegistered | PackageRejection::Disabled => None,
        PackageRejection::IdentityMismatch => Some(PackageFault::IdentityMismatch),
        PackageRejection::Root(error) => Some(root_fault(*error)),
        PackageRejection::DescriptorMissing => Some(PackageFault::DescriptorMissing),
        PackageRejection::Descriptor(_) => Some(PackageFault::DescriptorInvalid),
        PackageRejection::ReservedRuntimeId => Some(PackageFault::RuntimeNotClaimable),
        PackageRejection::RuntimeIdConflict => Some(PackageFault::RuntimeConflict),
        _ => Some(PackageFault::RootUnreadable),
    }
}

fn root_fault(error: VerifyError) -> PackageFault {
    match error {
        VerifyError::RootMissing => PackageFault::RootMissing,
        VerifyError::ManifestMissing
        | VerifyError::ManifestInvalid
        | VerifyError::ManifestArchiveMismatch
        | VerifyError::ManifestChanged
        | VerifyError::Missing { .. }
        | VerifyError::Added
        | VerifyError::Modified { .. } => PackageFault::RootModified,
        VerifyError::WrongMode { .. }
        | VerifyError::WrongType { .. }
        | VerifyError::WrongOwner { .. }
        | VerifyError::Hardlinked { .. } => PackageFault::RootUnsafe,
        _ => PackageFault::RootUnreadable,
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use package::registry::RegistryError;

    use super::*;
    use crate::agent::host::{DefinitionError, DefinitionInvariant};

    #[test]
    fn every_rejection_maps_to_one_fault() {
        let table = [
            (PackageRejection::NotRegistered, None),
            (PackageRejection::Disabled, None),
            (
                PackageRejection::IdentityMismatch,
                Some(PackageFault::IdentityMismatch),
            ),
            (
                PackageRejection::DescriptorMissing,
                Some(PackageFault::DescriptorMissing),
            ),
            (
                PackageRejection::Descriptor(DefinitionError::UnknownManifest),
                Some(PackageFault::DescriptorInvalid),
            ),
            (
                PackageRejection::ReservedRuntimeId,
                Some(PackageFault::RuntimeNotClaimable),
            ),
            (
                PackageRejection::RuntimeIdConflict,
                Some(PackageFault::RuntimeConflict),
            ),
            (
                PackageRejection::Descriptor(DefinitionError::Invariant(
                    DefinitionInvariant::ShellRequiresHostShell,
                )),
                Some(PackageFault::RuntimeNotClaimable),
            ),
            (
                PackageRejection::Registry(RegistryError::Corrupt),
                Some(PackageFault::RootUnreadable),
            ),
        ];
        for (rejection, fault) in table {
            assert_eq!(fault_of(&rejection), fault, "{rejection:?}");
        }
    }

    #[test]
    fn every_verification_failure_maps_to_one_fault() {
        let table = [
            (VerifyError::RootMissing, PackageFault::RootMissing),
            (VerifyError::ManifestMissing, PackageFault::RootModified),
            (VerifyError::ManifestInvalid, PackageFault::RootModified),
            (
                VerifyError::ManifestArchiveMismatch,
                PackageFault::RootModified,
            ),
            (VerifyError::ManifestChanged, PackageFault::RootModified),
            (
                VerifyError::Missing { entry: Some(0) },
                PackageFault::RootModified,
            ),
            (VerifyError::Added, PackageFault::RootModified),
            (
                VerifyError::Modified { entry: 1 },
                PackageFault::RootModified,
            ),
            (
                VerifyError::WrongMode { entry: None },
                PackageFault::RootUnsafe,
            ),
            (
                VerifyError::WrongType { entry: Some(2) },
                PackageFault::RootUnsafe,
            ),
            (
                VerifyError::WrongOwner { entry: None },
                PackageFault::RootUnsafe,
            ),
            (
                VerifyError::Hardlinked { entry: Some(0) },
                PackageFault::RootUnsafe,
            ),
            (VerifyError::UnknownPath, PackageFault::RootUnreadable),
            (
                VerifyError::Unreadable {
                    kind: ErrorKind::PermissionDenied,
                },
                PackageFault::RootUnreadable,
            ),
        ];
        for (error, fault) in table {
            assert_eq!(
                fault_of(&PackageRejection::Root(error)),
                Some(fault),
                "{error:?}"
            );
        }
    }
}
