//! Which package may serve which runtime id.
//!
//! [`decide_claim`] is the single policy the package lifecycle applies when a
//! package is installed, enabled or selected. It is a pure function of who
//! authorized the package and who serves the runtime id right now, so the rules
//! are testable as a table.

// Rust guideline compliant 2026-10-04

use protocol::{PackageId, RuntimeId};

use super::registry::RESERVED_RUNTIME_IDS;

/// How a package was authorized to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    /// The owner trusted the archive locally: an explicit digest or a linked
    /// developer directory.
    Local,
    /// The signed official catalog authorized the archive.
    Official,
}

/// Who serves a runtime id in the live runtime registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServedBy {
    /// A built-in runtime.
    Builtin,
    /// A loaded package with this package id.
    Package(PackageId),
}

/// Why a package may not claim a runtime id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimRefusal {
    /// The host never lets this package serve the id.
    NotClaimable,
    /// Something else already serves the id.
    Conflict,
}

/// Whether the host reserves `runtime_id` as the shell or one of the official
/// aliases.
#[must_use]
pub fn is_reserved(runtime_id: &RuntimeId) -> bool {
    RESERVED_RUNTIME_IDS.contains(&runtime_id.as_str())
}

/// Decides whether the package `package` may claim `runtime_id`.
///
/// - The shell is never claimable.
/// - An official alias (a reserved id other than the shell) is claimable only
///   by an [`Authority::Official`] package. It takes the alias over from the
///   built-in that serves it, and conflicts with a loaded package of another
///   package id.
/// - Any other id is claimable unless a built-in or a loaded package of a
///   different package id serves it; another version of the same package id
///   may take it over.
///
/// # Errors
///
/// Returns the [`ClaimRefusal`] naming the violated rule.
pub fn decide_claim(
    runtime_id: &RuntimeId,
    package: &PackageId,
    authority: Authority,
    served_by: Option<&ServedBy>,
) -> Result<(), ClaimRefusal> {
    if runtime_id.as_str() == RuntimeId::SHELL {
        return Err(ClaimRefusal::NotClaimable);
    }
    if is_reserved(runtime_id) && authority != Authority::Official {
        return Err(ClaimRefusal::NotClaimable);
    }
    match served_by {
        None => Ok(()),
        Some(ServedBy::Builtin) if is_reserved(runtime_id) => Ok(()),
        Some(ServedBy::Builtin) => Err(ClaimRefusal::Conflict),
        Some(ServedBy::Package(other)) if other != package => Err(ClaimRefusal::Conflict),
        Some(ServedBy::Package(_same)) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(id: &str) -> RuntimeId {
        RuntimeId::parse(id).expect("runtime id")
    }

    fn package(id: &str) -> PackageId {
        PackageId::parse(id).expect("package id")
    }

    fn other() -> ServedBy {
        ServedBy::Package(package("acme.runtime.other"))
    }

    fn same() -> ServedBy {
        ServedBy::Package(package("acme.runtime.pi"))
    }

    #[test]
    fn the_claim_table_is_exhaustive() {
        let aliases: Vec<&str> = RESERVED_RUNTIME_IDS
            .into_iter()
            .filter(|id| *id != RuntimeId::SHELL)
            .collect();
        assert_eq!(aliases.len(), RESERVED_RUNTIME_IDS.len() - 1);
        let own = package("acme.runtime.pi");
        let builtin = ServedBy::Builtin;
        let (other, same) = (other(), same());

        // The shell is never claimable, whoever authorized the package.
        for authority in [Authority::Local, Authority::Official] {
            for served in [None, Some(&builtin), Some(&other), Some(&same)] {
                assert_eq!(
                    decide_claim(&runtime(RuntimeId::SHELL), &own, authority, served),
                    Err(ClaimRefusal::NotClaimable),
                    "{authority:?} {served:?}"
                );
            }
        }

        for alias in aliases {
            let id = runtime(alias);
            // A local package never takes an alias.
            for served in [None, Some(&builtin), Some(&other), Some(&same)] {
                assert_eq!(
                    decide_claim(&id, &own, Authority::Local, served),
                    Err(ClaimRefusal::NotClaimable),
                    "{alias} local {served:?}"
                );
            }
            // An official package takes it, including from the built-in.
            assert_eq!(decide_claim(&id, &own, Authority::Official, None), Ok(()));
            assert_eq!(
                decide_claim(&id, &own, Authority::Official, Some(&builtin)),
                Ok(()),
                "{alias}"
            );
            assert_eq!(
                decide_claim(&id, &own, Authority::Official, Some(&other)),
                Err(ClaimRefusal::Conflict)
            );
            assert_eq!(
                decide_claim(&id, &own, Authority::Official, Some(&same)),
                Ok(())
            );
        }

        // An ordinary id is the same for both authorities.
        let id = runtime("pi");
        for authority in [Authority::Local, Authority::Official] {
            assert_eq!(decide_claim(&id, &own, authority, None), Ok(()));
            assert_eq!(decide_claim(&id, &own, authority, Some(&same)), Ok(()));
            assert_eq!(
                decide_claim(&id, &own, authority, Some(&other)),
                Err(ClaimRefusal::Conflict)
            );
            assert_eq!(
                decide_claim(&id, &own, authority, Some(&builtin)),
                Err(ClaimRefusal::Conflict)
            );
        }
    }
}
