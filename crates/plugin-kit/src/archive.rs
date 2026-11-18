//! Package archive identities: the descriptor reads the strict archive
//! reader validates the way `plugin install` validates them. The kit reads
//! the descriptor name, the package id/version, the runtime identity, and
//! builds the canonical archive in two passes with an output guard.

use std::fs;
use std::path::Path;

use package::directory::{build_directory_archive, DirectoryError};
use package::{read_archive, Limits, VerifiedArchive};
use protocol::{PackageId, PackageVersion, RuntimeId};

use crate::error::Error;
use crate::files::io_error;

/// Name of the package descriptor inside a package directory and archive.
pub const DESCRIPTOR_PATH: &str = "runtime.toml";

/// Identity a runtime package descriptor declares.
#[derive(Debug)]
pub struct Identity {
    /// Package identity.
    pub package_id: PackageId,
    /// Package version.
    pub version: PackageVersion,
    /// Runtime the package provides.
    pub runtime_id: RuntimeId,
    /// Runtime API the descriptor needs.
    pub runtime_api: u32,
}

/// Reads the package id and version from the `runtime.toml` bytes `descriptor`.
///
/// Every package kind declares them, including a context package that serves
/// no runtime.
pub fn descriptor_package(
    descriptor: &[u8],
) -> Result<(PackageId, PackageVersion, toml::Table), Error> {
    let invalid =
        |reason: &str| Error::Usage(format!("package descriptor `{DESCRIPTOR_PATH}` {reason}"));
    let text = std::str::from_utf8(descriptor).map_err(|_cause| invalid("is not UTF-8"))?;
    let table: toml::Table = text
        .parse()
        .map_err(|_cause| invalid("is not valid TOML"))?;
    let string = |value: Option<&toml::Value>, what: &str| {
        value
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| invalid(&format!("has no string `{what}`")))
    };
    let package_id = PackageId::parse(&string(table.get("id"), "id")?)
        .map_err(|_cause| invalid("has an invalid package `id`"))?;
    let version = PackageVersion::parse(&string(table.get("version"), "version")?)
        .map_err(|_cause| invalid("has an invalid `version`"))?;
    Ok((package_id, version, table))
}

/// Reads the identity and runtime API from the `runtime.toml` bytes `descriptor`.
pub fn descriptor_identity(descriptor: &[u8]) -> Result<Identity, Error> {
    let invalid =
        |reason: &str| Error::Usage(format!("package descriptor `{DESCRIPTOR_PATH}` {reason}"));
    let (package_id, version, table) = descriptor_package(descriptor)?;
    let runtime_api = table
        .get("runtime_api")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value >= 1)
        .ok_or_else(|| invalid("has no positive integer `runtime_api`"))?;
    let runtime = table
        .get("runtime")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| invalid("has no `[runtime]` table"))?;
    let runtime_id = RuntimeId::parse(
        runtime
            .get("id")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid("has no string `runtime.id`"))?,
    )
    .map_err(|_cause| invalid("has an invalid `runtime.id`"))?;
    Ok(Identity {
        package_id,
        version,
        runtime_id,
        runtime_api,
    })
}

/// The package id and version the `runtime.toml` of the verified archive read
/// from `archive` declares.
pub fn archive_package(
    verified: &VerifiedArchive,
    archive: &Path,
) -> Result<(PackageId, PackageVersion), Error> {
    let descriptor = verified
        .entries()
        .iter()
        .find(|entry| entry.path == DESCRIPTOR_PATH)
        .ok_or_else(|| {
            Error::Usage(format!(
                "package archive `{}` has no `{DESCRIPTOR_PATH}`",
                archive.display()
            ))
        })?;
    descriptor_package(&descriptor.contents).map(|(id, version, _table)| (id, version))
}

/// The identity the `runtime.toml` of the verified archive read from
/// `archive` declares.
pub fn archive_identity(verified: &VerifiedArchive, archive: &Path) -> Result<Identity, Error> {
    let descriptor = verified
        .entries()
        .iter()
        .find(|entry| entry.path == DESCRIPTOR_PATH)
        .ok_or_else(|| {
            Error::Usage(format!(
                "package archive `{}` has no `{DESCRIPTOR_PATH}`",
                archive.display()
            ))
        })?;
    descriptor_identity(&descriptor.contents)
}

/// Builds the canonical archive bytes of `dir`, mapping directory failures to
/// [`Error`] values that name `dir`.
pub fn build_bytes(dir: &Path, limits: &Limits) -> Result<Vec<u8>, Error> {
    build_directory_archive(dir, limits).map_err(|error| match error {
        DirectoryError::UnsupportedFileType => Error::UnsupportedFileType(dir.to_path_buf()),
        DirectoryError::InvalidPath => Error::InvalidPath(dir.to_path_buf()),
        DirectoryError::Io { kind } => Error::Io {
            path: dir.to_path_buf(),
            source: kind.into(),
        },
        DirectoryError::Archive(error) => Error::Package(error),
        // `Changed` and any future variant: the tree is not a stable input.
        other => Error::Usage(format!("package directory is not readable: {other}")),
    })
}

/// Fails when `output` is a symlink or resolves to a place inside `dir`.
///
/// A symlink is rejected outright, dangling or not: the write would follow it
/// to a destination this check cannot vouch for.
pub fn reject_output_inside(dir: &Path, output: &Path) -> Result<(), Error> {
    let root = fs::canonicalize(dir).map_err(io_error(dir))?;
    let resolved = match fs::symlink_metadata(output) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(Error::OutputIsSymlink(output.to_path_buf()));
        }
        Ok(_) => fs::canonicalize(output).map_err(io_error(output))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = match output.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            let name = output
                .file_name()
                .ok_or_else(|| Error::InvalidPath(output.to_path_buf()))?;
            fs::canonicalize(parent)
                .map_err(io_error(parent))?
                .join(name)
        }
        Err(error) => return Err(io_error(output)(error)),
    };
    if resolved.starts_with(&root) {
        return Err(Error::OutputInsideInput(output.to_path_buf()));
    }
    Ok(())
}

/// Builds the archive of `dir` twice and requires identical bytes.
///
/// Returns the package digest.
pub fn verify_bytes(dir: &Path) -> Result<String, Error> {
    let first = build_bytes(dir, &Limits::DEFAULT)?;
    let second = build_bytes(dir, &Limits::DEFAULT)?;
    if first != second {
        return Err(Error::Usage(
            "package verify failed: two builds of the same directory differ".to_string(),
        ));
    }
    Ok(read_archive(&first, &Limits::DEFAULT)
        .map_err(Error::Package)?
        .digest()
        .to_string())
}
