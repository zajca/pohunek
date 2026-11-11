//! `plugin build`: the canonical archive of a package directory.
//!
//! The identity of the archive is read back from its own `runtime.toml`, so
//! the file name never depends on a caller-supplied string, and the file is
//! created exclusively: an existing archive is an error and is left unchanged.

// Rust guideline compliant 2026-11-06

use std::fs;
use std::path::PathBuf;

use package::{read_archive, Limits, PackageDigest};
use protocol::{PackageId, PackageVersion};

use crate::catalog::{archive_package, io_error};
use crate::plugin_index::write_new_document;
use crate::runtime_package::{build_bytes, reject_output_inside};
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
    pub(crate) digest: PackageDigest,
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
    let metadata = fs::metadata(&args.output_dir).map_err(io_error(&args.output_dir))?;
    if !metadata.is_dir() {
        return Err(XtaskError::Usage(format!(
            "the output `{}` is not a directory",
            args.output_dir.display()
        )));
    }
    let bytes = build_bytes(&args.dir, &Limits::DEFAULT)?;
    let verified = read_archive(&bytes, &Limits::DEFAULT).map_err(XtaskError::Package)?;
    let (package_id, version) = archive_package(&verified, &args.dir)?;
    let path = args
        .output_dir
        .join(format!("{package_id}-{version}{ARCHIVE_SUFFIX}"));
    reject_output_inside(&args.dir, &path)?;
    write_new_document(&path, &bytes)?;
    Ok(Built {
        package_id,
        version,
        digest: verified.digest().clone(),
        path,
    })
}
