//! Kit `lint`: load the descriptor and the detection manifest through the
//! same parser an install runs.
//!
//! The loader builds the canonical archive of the directory in memory and
//! parses it through
//! [`definition_from_archive`](pohunek_daemon::agent::host::definition_from_archive),
//! the same function install validation runs before anything is extracted or
//! recorded. A package the kit lints clean is a package the daemon accepts,
//! down to the manifest the daemon's own resolver reads from the package.
//! Errors are the typed install rejections, never raw document text.

use std::path::PathBuf;

use package::Limits;
use pohunek_daemon::agent::host::definition_from_archive;
use protocol::{BindingProvenance, PackageId, PackageVersion, RuntimeId};

use crate::archive::{archive_package, build_bytes};
use crate::error::Error;

/// The inputs of a lint.
#[derive(Debug)]
pub struct LintArgs {
    /// Package directory holding `runtime.toml`.
    pub dir: PathBuf,
}

/// What a passing lint reports.
#[derive(Debug, serde::Serialize)]
pub struct LintReport {
    /// Package id the descriptor declares.
    pub package_id: PackageId,
    /// Package version the descriptor declares.
    pub version: PackageVersion,
    /// Runtime the package provides.
    pub runtime_id: RuntimeId,
}

impl LintReport {
    /// The single stdout line of a passing lint:
    /// `plugin lint ok: <package_id> <version> <runtime_id>`.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "plugin lint ok: {} {} {}",
            self.package_id, self.version, self.runtime_id
        )
    }
}

/// Lints the package in `args.dir`: the canonical archive of the directory
/// must parse through the install-time definition loader, including the
/// detection manifest the daemon's own resolver reads from the package.
pub fn lint(args: &LintArgs) -> Result<LintReport, Error> {
    let bytes = build_bytes(&args.dir, &Limits::DEFAULT)?;
    let verified = package::read_archive(&bytes, &Limits::DEFAULT).map_err(Error::Package)?;
    let definition =
        definition_from_archive(verified.entries(), verified.digest()).map_err(|rejection| {
            Error::Usage(format!(
                "package lint failed: the definition is invalid ({rejection})"
            ))
        })?;
    let (_package_id, _version) = archive_package(&verified, &args.dir)?;
    let BindingProvenance::Package { package, .. } = &definition.binding().provenance else {
        return Err(Error::Usage(
            "package lint failed: the descriptor parsed to a built-in binding, which a package never has".to_owned(),
        ));
    };
    Ok(LintReport {
        package_id: package.id.clone(),
        version: package.version.clone(),
        runtime_id: definition.runtime_id().clone(),
    })
}
