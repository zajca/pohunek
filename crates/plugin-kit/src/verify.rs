//! Kit `verify`: the archive matches the directory, twice built, and the digest.
//!
//! `verify --dir --archive --sha256 <digest>` rebuilds the canonical archive
//! of the package directory twice from independent directory walks, requires
//! byte equality, requires the given archive bytes to equal the rebuilt
//! bytes, and requires their SHA-256 to equal the given digest.

use std::path::PathBuf;

use package::{read_archive, Limits, MAX_COMPRESSED_BYTES};
use sha2::{Digest as _, Sha256};

use crate::archive::{archive_package, build_bytes};
use crate::error::{refuse, Error};
use crate::files::{hex, read_input};

/// Largest accepted archive: the package crate's own reader cap.
const MAX_ARCHIVE_BYTES: u64 = MAX_COMPRESSED_BYTES;

/// The inputs of a verify.
#[derive(Debug)]
pub struct VerifyArgs {
    /// Package directory the archive must rebuild from.
    pub dir: PathBuf,
    /// Archive file to check.
    pub archive: PathBuf,
    /// Expected `sha256:<64 hex>` digest of the archive bytes.
    pub sha256: String,
}

/// What a passing verify reports.
#[derive(Debug, serde::Serialize)]
pub struct VerifyReport {
    /// Package id read back from the archive's own `runtime.toml`.
    pub package_id: protocol::PackageId,
    /// Package version read back from the archive's own `runtime.toml`.
    pub version: protocol::PackageVersion,
    /// Digest of the verified archive bytes.
    pub digest: String,
}

impl VerifyReport {
    /// The single stdout line of a passing verify:
    /// `plugin verify ok: <package_id> <version> <digest>`.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "plugin verify ok: {} {} {}",
            self.package_id, self.version, self.digest
        )
    }
}

/// Verifies the archive against the directory and the digest.
pub fn verify(args: &VerifyArgs) -> Result<VerifyReport, Error> {
    let first = build_bytes(&args.dir, &Limits::DEFAULT)?;
    let second = build_bytes(&args.dir, &Limits::DEFAULT)?;
    if first != second {
        return Err(refuse(
            "package verify failed: two builds of the same directory differ",
        ));
    }
    let archive = read_input(&args.archive, MAX_ARCHIVE_BYTES)?;
    if archive != first {
        return Err(refuse(
            "the archive differs from a fresh build of the package directory",
        ));
    }
    let actual = format!("sha256:{}", hex(&Sha256::digest(&archive)));
    if actual != args.sha256 {
        return Err(refuse("the archive digest differs from `--sha256`"));
    }
    let verified = read_archive(&archive, &Limits::DEFAULT).map_err(Error::Package)?;
    let (package_id, version) = archive_package(&verified, &args.archive)?;
    Ok(VerifyReport {
        package_id,
        version,
        digest: actual,
    })
}
