//! Throwaway release trust: a catalog signed with a random key that exists only
//! while [`sign`] runs, and the anchor that trusts that key.
//!
//! Test-only tooling uses it so a daemon installs packages that no release
//! catalog lists. The release assembler never reads these files as inputs: it
//! refuses a daemon archive that carries a `runtime/` tree or an anchor other
//! than the one it is given, and the catalog is only verified against the
//! committed production anchor.

// Rust guideline compliant 2026-10-14

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use ed25519_dalek::SigningKey;
use package::{
    catalog_document_bytes, sign_catalog, AnchorFile, Attestation, BinarySet, Catalog,
    CatalogEntry, PackageDigest, Release, RootKey, Sha256Digest, CATALOG_SCHEMA_VERSION,
};
use zeroize::Zeroize as _;

use crate::catalog::{unix_now, verify_bytes, Identity};
use crate::release;
use crate::XtaskError;

/// Mode of the anchor and catalog files, which hold public data only.
pub(crate) const PUBLIC_FILE_MODE: u32 = 0o644;

/// Seconds a throwaway catalog stays valid. A day covers a gate run and a
/// release smoke started from the same job, and leaves no long-lived document.
const CATALOG_LIFETIME_SECONDS: u64 = 24 * 60 * 60;

/// Core range of the throwaway entries. The tooling authorizes packages for
/// the binaries it was handed; core compatibility is not what it checks.
pub(crate) const ANY_CORE: &str = ">=0.0.0";

/// Source commit of the throwaway release block. A placeholder: 40 hex digits
/// are the only property the daemon checks.
const PLACEHOLDER_COMMIT: &str = "0000000000000000000000000000000000000000";

/// Binary-set and attestation digest of the throwaway catalog. A placeholder:
/// the daemon checks the shape, and no release attests these archives.
const PLACEHOLDER_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Bytes of operating-system randomness an Ed25519 seed takes.
const SEED_BYTES: usize = 32;

/// The core release a throwaway catalog declares: its `X.Y.Z` version and the
/// anti-rollback sequence that release's catalog carries.
#[derive(Debug)]
pub(crate) struct CoreRelease {
    pub(crate) version: String,
    pub(crate) sequence: u64,
}

/// The core release of the build version `version`, parsed once. Build
/// metadata (`+upgrade.<commit>` on a stamped head) is cut here, and both the
/// version text and the sequence come from the same parsed components.
pub(crate) fn core_release(version: &str) -> Result<CoreRelease, XtaskError> {
    let core = version
        .split_once('+')
        .map_or(version, |(core, _metadata)| core);
    let parts = release::parse_version(core)?;
    Ok(CoreRelease {
        version: format!("{}.{}.{}", parts[0], parts[1], parts[2]),
        sequence: release::sequence_of(parts),
    })
}

/// An anchor and the catalog it verifies, both as the bytes written to disk.
#[derive(Debug)]
pub(crate) struct Trust {
    pub(crate) anchor: Vec<u8>,
    pub(crate) catalog: Vec<u8>,
}

/// The catalog entry of one package archive, for this machine's platforms.
pub(crate) fn package_entry(
    identity: Identity,
    digest: PackageDigest,
) -> Result<CatalogEntry, XtaskError> {
    let platforms = machine_platforms();
    let placeholder = placeholder_digest()?;
    Ok(CatalogEntry {
        package_id: identity.package_id,
        runtime_id: identity.runtime_id,
        version: identity.version,
        digest,
        runtime_api: identity.runtime_api,
        attestations: platforms
            .iter()
            .map(|platform| Attestation {
                platform: platform.clone(),
                digest: placeholder.clone(),
            })
            .collect(),
        platforms,
        core: ANY_CORE.to_owned(),
    })
}

/// Signs one catalog listing `entries` for `release` with a fresh random key
/// and returns it with the anchor trusting that key.
///
/// The release's sequence is the anti-rollback counter: a host that recorded a
/// higher one refuses the catalog. The document is verified against the anchor
/// before it is returned, so a catalog the daemon would refuse is never used.
pub(crate) fn sign(entries: &[CatalogEntry], release: &CoreRelease) -> Result<Trust, XtaskError> {
    let mut signed = sign_each(&[entries], release)?;
    Ok(Trust {
        anchor: signed.anchor,
        catalog: signed.catalogs.remove(0),
    })
}

/// An anchor and the catalogs it verifies, both as the bytes written to disk.
#[derive(Debug)]
pub(crate) struct SignedSet {
    pub(crate) anchor: Vec<u8>,
    pub(crate) catalogs: Vec<Vec<u8>>,
}

/// Signs one catalog per entry list of `catalogs`, all for `release` and with
/// one fresh random key, and returns them in order with the anchor trusting
/// that key. See [`sign`].
pub(crate) fn sign_each(
    catalogs: &[&[CatalogEntry]],
    release: &CoreRelease,
) -> Result<SignedSet, XtaskError> {
    let now = unix_now()?;
    let expires_at = now.saturating_add(CATALOG_LIFETIME_SECONDS);
    let key = random_key()?;
    let root = RootKey::new(key.verifying_key().to_bytes(), 0, expires_at)
        .map_err(|error| XtaskError::Anchor(package::AnchorFileError::Anchor(error)))?;
    let anchor = AnchorFile::new(vec![root], Vec::new())
        .map_err(|error| XtaskError::Anchor(package::AnchorFileError::Anchor(error)))?
        .to_bytes()
        .map_err(XtaskError::Anchor)?;
    let placeholder = placeholder_digest()?;
    let mut documents = Vec::with_capacity(catalogs.len());
    for entries in catalogs {
        let catalog = Catalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            sequence: release.sequence,
            expires_at,
            release: Release {
                version: release.version.clone(),
                commit: PLACEHOLDER_COMMIT.to_owned(),
                binary_sets: machine_platforms()
                    .into_iter()
                    .map(|target| BinarySet {
                        target,
                        digest: placeholder.clone(),
                    })
                    .collect(),
            },
            revoked_key_ids: Vec::new(),
            revoked_digests: Vec::new(),
            entries: entries.to_vec(),
        };
        let envelope = sign_catalog(catalog, &key).map_err(XtaskError::Catalog)?;
        let document = catalog_document_bytes(&envelope).map_err(XtaskError::Catalog)?;
        verify_bytes(&document, &anchor, None, now)?;
        documents.push(document);
    }
    Ok(SignedSet {
        anchor,
        catalogs: documents,
    })
}

/// Writes public bytes to a new file with [`PUBLIC_FILE_MODE`].
pub(crate) fn write_public(path: &Path, bytes: &[u8]) -> Result<(), XtaskError> {
    fs::write(path, bytes)
        .and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(PUBLIC_FILE_MODE)))
        .map_err(|error| {
            XtaskError::Usage(format!(
                "cannot write the gate file `{}`: {error}",
                path.display()
            ))
        })
}

/// A fresh Ed25519 key from operating-system randomness; the seed buffer is
/// cleared once the key holds it.
fn random_key() -> Result<SigningKey, XtaskError> {
    let mut seed = [0_u8; SEED_BYTES];
    getrandom::getrandom(&mut seed).map_err(|error| {
        XtaskError::Usage(format!(
            "operating-system randomness is unavailable: {error}"
        ))
    })?;
    let key = SigningKey::from_bytes(&seed);
    seed.zeroize();
    Ok(key)
}

fn placeholder_digest() -> Result<Sha256Digest, XtaskError> {
    Sha256Digest::parse(PLACEHOLDER_DIGEST)
        .map_err(|_error| XtaskError::Usage("the placeholder catalog digest is invalid".into()))
}

/// The platform names a release daemon of this machine can report. The C
/// library of a Linux release binary (glibc or musl) is a property of the
/// binary under test, not of xtask, so both are listed.
pub(crate) fn machine_platforms() -> Vec<String> {
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
