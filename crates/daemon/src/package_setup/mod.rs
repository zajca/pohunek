//! `package.setup_resolve`: resolves one setup entry of an installed package to
//! its verified program.
//!
//! The daemon never runs the program. The caller is the `pohunek` CLI on the
//! owner's terminal, which starts the program with the owner's arguments. The
//! daemon's part is the trust decision: the entry must be declared by the
//! selected, enabled version whose root is verified against its digest and not
//! revoked, so the program path it returns is the one the package shipped.

// Rust guideline compliant 2026-11-23

use package::registry::PackageRecord;
use protocol::{CommandErrorKind, CommandName, PackageId, SetupResolveResult};
use tracing::info;

use crate::agent::host::PackageStore;

/// Resolves the setup entry `setup` of the selected version of `package`.
///
/// The order is installed, selected, declared, enabled, then the root and its
/// revocation status. Every error is a fixed kind; none carries a path.
///
/// # Errors
///
/// The [`CommandErrorKind`] of the first failed check.
pub(crate) fn resolve(
    store: &PackageStore,
    package: &PackageId,
    setup: &CommandName,
) -> Result<SetupResolveResult, CommandErrorKind> {
    let outcome = resolve_entry(store, package, setup);
    match &outcome {
        Ok(resolved) => info!(
            package = %package,
            version = %resolved.package.version,
            digest = %resolved.digest,
            setup = %setup,
            outcome = "ok",
            "package setup resolved"
        ),
        Err(kind) => info!(
            package = %package,
            setup = %setup,
            outcome = kind.code(),
            "package setup refused"
        ),
    }
    outcome
}

fn resolve_entry(
    store: &PackageStore,
    package: &PackageId,
    setup: &CommandName,
) -> Result<SetupResolveResult, CommandErrorKind> {
    let state = store
        .registry_state()
        .map_err(|_error| CommandErrorKind::Unavailable)?;
    if !state
        .packages()
        .iter()
        .any(|record| &record.identity().id == package)
    {
        return Err(CommandErrorKind::PackageNotInstalled);
    }
    let digest = state
        .selected(package)
        .ok_or(CommandErrorKind::NotSelected)?
        .clone();
    let record = state
        .package(&digest)
        .ok_or(CommandErrorKind::Unavailable)?;
    let identity = record.identity().clone();
    // Reading the descriptor verifies the root against the digest, so an
    // undeclared entry of a tampered root is refused as unavailable.
    let descriptor = store
        .read_descriptor(&digest, &identity)
        .map_err(|_rejection| CommandErrorKind::Unavailable)?;
    let entry = descriptor
        .setup()
        .iter()
        .find(|entry| &entry.name == setup)
        .ok_or(CommandErrorKind::SetupNotDeclared)?;
    if !record.enabled() {
        return Err(CommandErrorKind::PackageDisabled);
    }
    ensure_not_revoked(store, record)?;
    let files = store
        .verified_files_path(&digest)
        .map_err(|_rejection| CommandErrorKind::Unavailable)?;
    let program = files.join(&entry.program);
    Ok(SetupResolveResult {
        package: identity,
        setup: entry.name.clone(),
        digest,
        root: path_text(&files)?,
        program: path_text(&program)?,
        args: entry.args.clone(),
    })
}

/// A catalog-authorized package whose digest a recorded catalog revoked runs
/// nothing, and neither does one whose revocation set cannot be read.
fn ensure_not_revoked(
    store: &PackageStore,
    record: &PackageRecord,
) -> Result<(), CommandErrorKind> {
    store
        .ensure_not_revoked(record)
        .map_err(|_rejection| CommandErrorKind::Unavailable)
}

/// The UTF-8 text of `path`; a path that is not UTF-8 cannot be written to the
/// wire as a path, so it is unavailable.
fn path_text(path: &std::path::Path) -> Result<String, CommandErrorKind> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(CommandErrorKind::Unavailable)
}
