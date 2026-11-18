//! `plugin build`: the canonical archive of a package directory.
//!
//! A thin wrapper over the kit crate: the logic lives in
//! `plugin_kit::build` so the kit `build` command and `xtask plugin build`
//! share it, and this module only converts the argument, result and error
//! types to the `xtask` contract.

// Rust guideline compliant 2026-11-06

use std::path::PathBuf;

use protocol::{PackageId, PackageVersion};

use crate::XtaskError;

/// Suffix of a package archive file name.
const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// The inputs of `plugin build`.
pub(crate) struct BuildArgs {
    pub(crate) dir: PathBuf,
    pub(crate) output_dir: PathBuf,
}

/// What `plugin build` wrote.
pub(crate) struct Built {
    pub(crate) package_id: PackageId,
    pub(crate) version: PackageVersion,
    pub(crate) digest: package::PackageDigest,
    pub(crate) path: PathBuf,
}

impl Built {
    /// The single stdout line of a successful build:
    /// `plugin build ok: <package_id> <version> <digest> <path>`.
    pub(crate) fn line(&self) -> String {
        format!(
            "plugin build ok: {} {} {} {}",
            self.package_id,
            self.version,
            self.digest.as_str(),
            self.path.display()
        )
    }
}

/// Builds the archive of `args.dir` into `args.output_dir`.
///
/// The output directory must exist and lie outside the package directory, and
/// the archive file must not exist yet. A refused input writes nothing.
pub(crate) fn build(args: &BuildArgs) -> Result<Built, XtaskError> {
    let built = plugin_kit::build(&plugin_kit::BuildArgs {
        dir: args.dir.clone(),
        output_dir: args.output_dir.clone(),
    })?;
    debug_assert_eq!(plugin_kit::ARCHIVE_SUFFIX, ARCHIVE_SUFFIX);
    Ok(Built {
        package_id: built.package_id,
        version: built.version,
        digest: built.digest,
        path: built.path,
    })
}
