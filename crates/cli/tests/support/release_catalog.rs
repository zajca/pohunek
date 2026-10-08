//! What the consumer suite knows about the package archive it installs, and
//! the throwaway trust a row-mode run generates for it.
//!
//! A row-mode run has no official catalog yet: the attestation it will feed
//! does not exist. The harness therefore signs a catalog with a key that is
//! generated here, lives only in this process, and is trusted only through
//! the anchor placed beside the copied daemon. The release block and the
//! attestation digests of that catalog are placeholders; the daemon checks
//! their shape, not their content.

// Rust guideline compliant 2026-10-08

use std::path::Path;

use ed25519_dalek::SigningKey;
use package::{
    read_archive, Attestation, BinarySet, Catalog, CatalogEntry, Limits, PackageDigest, Release,
    Sha256Digest, CATALOG_SCHEMA_VERSION,
};
use pohunek_daemon::agent::host::{
    definition_from_archive, LaunchProgram, VersionProbePolicy, SUPPORTED_RUNTIME_API,
};
use protocol::{BindingProvenance, PackageId, PackageVersion, RuntimeId};

use crate::catalog_fixture::{anchor_for, signed, test_key, WINDOW_END};

/// Source commit of a row-mode release block. A placeholder: 40 hex digits
/// are the only property the daemon checks.
const PLACEHOLDER_COMMIT: &str = "0000000000000000000000000000000000000000";

/// Digest of a row-mode binary set and attestation. A placeholder: the real
/// values are computed by the release tooling after this suite has run.
const PLACEHOLDER_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Publication counter of the row-mode catalog; the host starts empty, so any
/// positive value is accepted.
const SEQUENCE: u64 = 1;

/// The package archive as the suite reads it.
#[derive(Debug)]
pub(crate) struct PackageFacts {
    pub(crate) digest: PackageDigest,
    pub(crate) package_id: PackageId,
    pub(crate) package_version: PackageVersion,
    pub(crate) runtime_id: RuntimeId,
    /// Executable the runtime launches, resolved on `PATH`.
    pub(crate) program: String,
    pub(crate) probe: VersionProbePolicy,
}

/// Reads the archive at `path`.
///
/// # Errors
///
/// Fails with a message when the file is not a package archive, when its
/// descriptor is not an accepted one, or when it launches no fixed program or
/// declares no version probe (the suite cannot attest a version it cannot
/// read).
pub(crate) fn read_package(path: &Path) -> Result<PackageFacts, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let archive = read_archive(&bytes, &Limits::DEFAULT)
        .map_err(|error| format!("{} is not a package archive: {error}", path.display()))?;
    let digest = archive.digest().clone();
    let definition = definition_from_archive(archive.entries(), &digest)
        .map_err(|error| format!("the package descriptor is not accepted: {error}"))?;
    let BindingProvenance::Package { package, .. } = &definition.binding().provenance else {
        return Err("the descriptor does not bind to a package".to_owned());
    };
    let LaunchProgram::Fixed(program) = definition.program() else {
        return Err("the runtime does not launch a fixed program".to_owned());
    };
    let probe = definition
        .version_probe_policy()
        .cloned()
        .ok_or("the runtime declares no version probe".to_owned())?;
    Ok(PackageFacts {
        digest,
        package_id: package.id.clone(),
        package_version: package.version.clone(),
        runtime_id: definition.runtime_id().clone(),
        program: program.clone(),
        probe,
    })
}

/// A throwaway signing root: the key exists only in this process and is
/// trusted only through [`ThrowawayRoot::anchor`].
pub(crate) struct ThrowawayRoot {
    key: SigningKey,
    /// The anchor file bytes that trust the key.
    pub(crate) anchor: Vec<u8>,
}

impl ThrowawayRoot {
    pub(crate) fn new() -> Self {
        let key = test_key();
        let anchor = anchor_for(&key);
        Self { key, anchor }
    }

    /// A signed catalog that authorizes exactly `facts.digest` for release
    /// `core_version` on every platform the daemon can report for this
    /// machine.
    pub(crate) fn catalog(&self, facts: &PackageFacts, core_version: &str) -> Vec<u8> {
        let platforms = machine_platforms();
        let digest = Sha256Digest::parse(PLACEHOLDER_DIGEST).expect("placeholder digest");
        let entry = CatalogEntry {
            package_id: facts.package_id.clone(),
            runtime_id: facts.runtime_id.clone(),
            version: facts.package_version.clone(),
            digest: facts.digest.clone(),
            // The daemon accepts a descriptor only with this runtime API version.
            runtime_api: SUPPORTED_RUNTIME_API,
            platforms: platforms.clone(),
            attestations: platforms
                .iter()
                .map(|platform| Attestation {
                    platform: platform.clone(),
                    digest: digest.clone(),
                })
                .collect(),
            core: format!("={core_version}"),
        };
        let catalog = Catalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            sequence: SEQUENCE,
            expires_at: WINDOW_END,
            release: Release {
                version: core_version.to_owned(),
                commit: PLACEHOLDER_COMMIT.to_owned(),
                binary_sets: platforms
                    .iter()
                    .map(|target| BinarySet {
                        target: target.clone(),
                        digest: digest.clone(),
                    })
                    .collect(),
            },
            revoked_key_ids: Vec::new(),
            revoked_digests: Vec::new(),
            entries: vec![entry],
        };
        signed(&self.key, catalog)
    }
}

/// The platform names a release daemon of this machine can report: the C
/// library of a Linux release binary (glibc or musl) is a property of the
/// binary under test, not of this test executable.
fn machine_platforms() -> Vec<String> {
    let arch = std::env::consts::ARCH;
    if cfg!(target_os = "macos") {
        vec![format!("{arch}-apple-darwin")]
    } else {
        vec![
            format!("{arch}-unknown-linux-gnu"),
            format!("{arch}-unknown-linux-musl"),
        ]
    }
}
