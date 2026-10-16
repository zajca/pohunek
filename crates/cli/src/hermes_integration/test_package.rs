//! Test fixture: the operator bundle rendered from the Hermes package directory.
//!
//! Lifecycle tests need a valid package bundle without a daemon or a package
//! registry. The fixture reads `runtime-packages/hermes` at test run time, so
//! the package directory never becomes a compile-time input of the CLI.

// Rust guideline compliant 2026-10-13

use std::fs;
use std::path::{Path, PathBuf};

use package::version_probe::VersionProbePolicy;

use super::assets::{self, Bundle, PACKAGE_ID};
use super::error::Error;

/// Directory of the official Hermes package, relative to the workspace root.
const PACKAGE_DIR: &str = "runtime-packages/hermes";

/// Stand-in package archive digest. The fixture renders the package directory
/// without building an archive, so no real archive digest exists; any
/// well-formed lowercase SHA-256 hex value satisfies the marker contract.
const FIXTURE_DIGEST: &str = "f00df00df00df00df00df00df00df00df00df00df00df00df00df00df00df00d";

/// Returns the package directory in this workspace.
fn package_dir() -> PathBuf {
    pohunek_test_support::workspace_root().join(PACKAGE_DIR)
}

/// Renders the package directory's closed operator sidecar for `policy_path`.
///
/// # Errors
///
/// Returns [`Error::Io`] when a package file cannot be read, and the
/// rendering errors of [`assets::render_source`].
pub(crate) fn bundle(policy_path: &Path) -> Result<Bundle, Error> {
    let root = package_dir();
    let manifest = fs::read(root.join("operator.toml"))?;
    let assets = assets::render_source(policy_path, &manifest, |path| {
        fs::read(root.join(path)).map_err(Error::from)
    })?;
    Ok(Bundle {
        assets,
        manifest_sha256: assets::checksum(&manifest),
        package_id: PACKAGE_ID.to_owned(),
        package_digest: FIXTURE_DIGEST.to_owned(),
        version_probe: version_policy()?,
    })
}

/// The version policy the package directory's descriptor declares.
///
/// # Errors
///
/// Returns [`Error::Io`] when the descriptor cannot be read, and
/// [`Error::InvalidAsset`] when it declares no valid policy.
pub(crate) fn version_policy() -> Result<VersionProbePolicy, Error> {
    assets::version_policy(&fs::read(
        package_dir().join(assets::RUNTIME_DESCRIPTOR_PATH),
    )?)
}
