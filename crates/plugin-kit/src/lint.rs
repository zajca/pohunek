//! Kit `lint`: load the descriptor through the same loader an install runs.
//!
//! The loader builds the canonical archive of the directory in memory and
//! parses it through
//! [`descriptor_from_archive`](pohunek_daemon::agent::host::descriptor_from_archive),
//! the same function install validation runs before anything is extracted or
//! recorded. A package the kit lints clean is a package the daemon accepts,
//! down to the file layout (`lists_under`) and the `requires` coverage the
//! daemon's own checks run. Errors are the typed install rejections, never
//! raw document text.

use std::path::PathBuf;

use package::Limits;
use pohunek_daemon::agent::host::descriptor_from_archive;
use protocol::{BindingProvenance, PackageId, PackageVersion, RuntimeId};

use crate::archive::{archive_package, build_bytes};
use crate::error::Error;

/// What a passing lint reports: the kind the descriptor exports.
///
/// The kind is what install validation saw: `Context` means the descriptor
/// carries a `[context]` export (`descriptor.context_dir().is_some()`),
/// never merely the absence of a runtime. A commands-only descriptor (no
/// runtime, no context) is refused: the plugin kit does not support
/// commands-only packages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageKind {
    /// The descriptor exports a runtime.
    Runtime,
    /// The descriptor exports a `[context]` directory of files and nothing else.
    Context,
}

impl std::fmt::Display for PackageKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime => write!(f, "runtime"),
            Self::Context => write!(f, "context"),
        }
    }
}

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
    /// Kind the descriptor exports.
    pub kind: PackageKind,
    /// Runtime the package provides; absent for a context package.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<RuntimeId>,
}

impl LintReport {
    /// The single stdout line of a passing lint:
    /// `plugin lint ok: <package_id> <version> <runtime_id>` for a runtime
    /// package, `plugin lint ok: <package_id> <version> context` for a
    /// context package.
    #[must_use]
    pub fn line(&self) -> String {
        match &self.runtime_id {
            Some(runtime_id) => format!(
                "plugin lint ok: {} {} {}",
                self.package_id, self.version, runtime_id
            ),
            None => format!(
                "plugin lint ok: {} {} context",
                self.package_id, self.version
            ),
        }
    }
}

/// Lints the package in `args.dir`: the canonical archive of the directory
/// must parse through the install-time descriptor loader, including the file
/// layout and the `requires` coverage the daemon's own checks run.
pub fn lint(args: &LintArgs) -> Result<LintReport, Error> {
    let bytes = build_bytes(&args.dir, &Limits::DEFAULT)?;
    let verified = package::read_archive(&bytes, &Limits::DEFAULT).map_err(Error::Package)?;
    let descriptor =
        descriptor_from_archive(verified.entries(), verified.digest()).map_err(|rejection| {
            Error::Usage(format!(
                "package lint failed: the definition is invalid ({rejection})"
            ))
        })?;
    let (_package_id, _version) = archive_package(&verified, &args.dir)?;
    let identity = descriptor.identity().clone();
    let context_dir = descriptor.context_dir().is_some();
    let runtime_id = match descriptor.into_parts() {
        (Some(definition), _commands) => {
            let BindingProvenance::Package { package, .. } = &definition.binding().provenance
            else {
                return Err(Error::Usage(
                    "package lint failed: the descriptor parsed to a built-in binding, which a package never has".to_owned(),
                ));
            };
            if package.id != identity.id || package.version != identity.version {
                return Err(Error::Usage(
                    "package lint failed: the descriptor identity differs from its definition identity".to_owned(),
                ));
            }
            Some(definition.runtime_id().clone())
        }
        (None, commands) => {
            if !context_dir && !commands.is_empty() {
                return Err(Error::Usage(
                    "package lint failed: the package exports commands only, which the plugin kit does not support".to_owned(),
                ));
            }
            None
        }
    };
    Ok(LintReport {
        package_id: identity.id,
        version: identity.version,
        kind: match &runtime_id {
            Some(_) => PackageKind::Runtime,
            None if context_dir => PackageKind::Context,
            // A descriptor with neither a runtime nor a context export fails
            // above in `descriptor_from_archive`; reaching here without a
            // context dir is unreachable.
            None => {
                return Err(Error::Usage(
                    "package lint failed: the package exports neither a runtime nor a context"
                        .to_owned(),
                ));
            }
        },
        runtime_id,
    })
}
