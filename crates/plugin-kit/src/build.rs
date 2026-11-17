//! `plugin build` / kit `build`: the canonical archive of a package directory.
//!
//! The identity of the archive is read back from its own `runtime.toml`, so
//! the file name never depends on a caller-supplied string, and the file is
//! created exclusively: an existing archive is an error and is left unchanged.
//!
//! Moved from `xtask::plugin::build` unchanged apart from the error type, so
//! the `xtask plugin build` wrapper keeps its exact contract by delegating
//! here.

use std::fs;
use std::path::PathBuf;

use package::{read_archive, Limits, PackageDigest};
use protocol::{PackageId, PackageVersion};

use crate::archive::{archive_package, build_bytes, reject_output_inside};
use crate::error::Error;
use crate::files::{io_error, write_new_document};

/// Suffix of a package archive file name.
pub const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// The inputs of a package build.
#[derive(Debug)]
pub struct BuildArgs {
    /// Package directory to pack.
    pub dir: PathBuf,
    /// Existing directory the archive file is created in.
    pub output_dir: PathBuf,
}

/// What a build wrote.
#[derive(Debug, serde::Serialize)]
pub struct Built {
    /// Package id read back from the archive's own `runtime.toml`.
    pub package_id: PackageId,
    /// Package version read back from the archive's own `runtime.toml`.
    pub version: PackageVersion,
    /// Digest of the canonical archive bytes.
    pub digest: PackageDigest,
    /// Archive file created in the output directory.
    pub path: PathBuf,
}

impl Built {
    /// The single stdout line of a successful build:
    /// `plugin build ok: <package_id> <version> <digest> <path>`.
    #[must_use]
    pub fn line(&self) -> String {
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
pub fn build(args: &BuildArgs) -> Result<Built, Error> {
    let metadata = fs::metadata(&args.output_dir).map_err(io_error(&args.output_dir))?;
    if !metadata.is_dir() {
        return Err(Error::Usage(format!(
            "the output `{}` is not a directory",
            args.output_dir.display()
        )));
    }
    let bytes = build_bytes(&args.dir, &Limits::DEFAULT)?;
    let verified = read_archive(&bytes, &Limits::DEFAULT).map_err(Error::Package)?;
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
