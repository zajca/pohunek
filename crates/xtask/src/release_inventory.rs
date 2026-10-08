//! The release inventory and its verification.
//!
//! `release-inventory.sha256` lists every file of a release bundle except
//! itself in `sha256sum -c` format, sorted by name. It is the exact list the
//! publisher uploads and verifies. [`verify_bundle`] recomputes it against the
//! directory and cross-checks the signed catalog against the bytes of the
//! daemon archives, the package archives and the attestations in the bundle.

// Rust guideline compliant 2026-10-08

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use package::read_archive;
use package::Limits;

use crate::attestation::{binary_set_digest, executable_hashes};
use crate::catalog::{io_error, read_input, verify_bytes, write_output};
use crate::release::{
    refuse, Fault, InputClass, ANCHOR_FILE, ATTESTATIONS_DIR, BUNDLE_DIR, CATALOG_FILE,
    INVENTORY_FILE, PACKAGES_DIR,
};
use crate::release_policy::{
    attestation_name, bundled_attestation_name, bundled_package_name, package_name,
};
use crate::release_tree::{extract_archive, read_manifest, sha256_bytes, sha256_file, verify_tree};
use crate::XtaskError;

/// Largest accepted inventory file: one 100-byte line per file of a bundle
/// of a few dozen files, with ample room.
const MAX_INVENTORY_BYTES: u64 = 1024 * 1024;

/// Separator between digest and name in `sha256sum` output.
const CHECKSUM_SEPARATOR: &str = "  ";

/// Length of a SHA-256 digest in lowercase hex.
const SHA256_HEX_LEN: usize = 64;

/// Name prefix and suffix of the daemon archives in a bundle.
const DAEMON_PREFIX: &str = "pohunek-daemon-";
const DAEMON_SUFFIX: &str = ".tar.gz";

/// A file name a bundle may contain: no directory part, no leading dot.
fn is_bundle_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._+@=,-".contains(&byte))
}

/// Names of the regular files directly in `dir`, ascending.
///
/// A subdirectory, link or other special file is refused.
fn list_files(dir: &Path) -> Result<Vec<String>, XtaskError> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_error(dir))? {
        let entry = entry.map_err(io_error(dir))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry
            .file_type()
            .map_err(io_error(&entry.path()))?
            .is_file()
        {
            return Err(refuse(InputClass::Inventory, Fault::NotRegularFile, name));
        }
        if !is_bundle_name(&name) {
            return Err(refuse(InputClass::Inventory, Fault::UnsafePath, name));
        }
        names.push(name);
    }
    names.sort_unstable();
    Ok(names)
}

/// Writes the inventory of every file in `dir` except the inventory itself.
pub(crate) fn write_inventory(dir: &Path) -> Result<(), XtaskError> {
    let mut text = String::new();
    for name in list_files(dir)? {
        if name == INVENTORY_FILE {
            continue;
        }
        let digest = sha256_file(&dir.join(&name), InputClass::Inventory, &name)?;
        text.push_str(&digest);
        text.push_str(CHECKSUM_SEPARATOR);
        text.push_str(&name);
        text.push('\n');
    }
    write_output(&dir.join(INVENTORY_FILE), text.as_bytes())
}

/// Reads the inventory of `dir` as digest by name.
fn read_inventory(dir: &Path) -> Result<BTreeMap<String, String>, XtaskError> {
    let path = dir.join(INVENTORY_FILE);
    let bytes = read_input(&path, MAX_INVENTORY_BYTES).map_err(|error| match error {
        XtaskError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            refuse(InputClass::Inventory, Fault::Missing, INVENTORY_FILE)
        }
        other => other,
    })?;
    let malformed = || refuse(InputClass::Inventory, Fault::Malformed, INVENTORY_FILE);
    let text = String::from_utf8(bytes).map_err(|_cause| malformed())?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(malformed());
    }
    let mut entries = BTreeMap::new();
    let mut previous: Option<&str> = None;
    for line in text.lines() {
        let (digest, name) = line.split_once(CHECKSUM_SEPARATOR).ok_or_else(malformed)?;
        let digest_ok = digest.len() == SHA256_HEX_LEN
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !digest_ok || !is_bundle_name(name) || name == INVENTORY_FILE {
            return Err(malformed());
        }
        if previous.is_some_and(|last| last >= name) {
            return Err(refuse(
                InputClass::Inventory,
                Fault::NotSorted,
                INVENTORY_FILE,
            ));
        }
        previous = Some(name);
        entries.insert(name.to_owned(), digest.to_owned());
    }
    Ok(entries)
}

/// What a verified bundle held.
#[derive(Debug)]
pub(crate) struct BundleSummary {
    /// Files covered by the inventory.
    pub(crate) files: usize,
    /// Daemon archives whose catalog and runtime files were cross-checked.
    pub(crate) daemon_archives: usize,
}

/// Verifies the release bundle in `dir` at time `now`.
///
/// The inventory must list exactly the directory's regular files, each with
/// its digest. The top-level catalog must verify against the trust anchor
/// inside every daemon archive, and each daemon archive must carry that
/// catalog, the binary set the catalog names for its target, and exactly the
/// package archives and attestations the catalog lists for that target, byte
/// for byte equal to the bundle's top-level files.
pub(crate) fn verify_bundle(dir: &Path, now: u64) -> Result<BundleSummary, XtaskError> {
    let listed = read_inventory(dir)?;
    let present: BTreeSet<String> = list_files(dir)?
        .into_iter()
        .filter(|name| name != INVENTORY_FILE)
        .collect();
    for name in listed.keys() {
        if !present.contains(name) {
            return Err(refuse(InputClass::Inventory, Fault::Missing, name));
        }
    }
    for name in &present {
        if !listed.contains_key(name) {
            return Err(refuse(InputClass::Inventory, Fault::Unexpected, name));
        }
    }
    for (name, digest) in &listed {
        if &sha256_file(&dir.join(name), InputClass::Inventory, name)? != digest {
            return Err(refuse(InputClass::Inventory, Fault::ChecksumMismatch, name));
        }
    }

    let catalog = read_input(
        &dir.join(CATALOG_FILE),
        u64::try_from(package::MAX_CATALOG_BYTES).unwrap_or(u64::MAX),
    )
    .map_err(|error| match error {
        XtaskError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            refuse(InputClass::Catalog, Fault::Missing, CATALOG_FILE)
        }
        other => other,
    })?;
    let daemons: Vec<&String> = present
        .iter()
        .filter(|name| name.starts_with(DAEMON_PREFIX) && name.ends_with(DAEMON_SUFFIX))
        .collect();
    if daemons.is_empty() {
        return Err(refuse(
            InputClass::Archive,
            Fault::Missing,
            "daemon archive",
        ));
    }
    let scratch_parent = dir.parent().filter(|parent| !parent.as_os_str().is_empty());
    let scratch = tempfile::Builder::new()
        .prefix(".pohunek-verify-")
        .tempdir_in(scratch_parent.unwrap_or_else(|| Path::new(".")))
        .map_err(io_error(dir))?;
    for archive in &daemons {
        check_daemon_archive(dir, archive, &catalog, scratch.path(), now)?;
    }
    Ok(BundleSummary {
        files: listed.len(),
        daemon_archives: daemons.len(),
    })
}

/// Cross-checks one daemon archive of the bundle against the bundle's catalog.
fn check_daemon_archive(
    dir: &Path,
    archive: &str,
    catalog: &[u8],
    scratch: &Path,
    now: u64,
) -> Result<(), XtaskError> {
    let stem = archive
        .strip_suffix(DAEMON_SUFFIX)
        .unwrap_or(archive)
        .to_owned();
    let work = scratch.join(&stem);
    fs::create_dir(&work).map_err(io_error(&work))?;
    let top = extract_archive(&dir.join(archive), archive, &stem, &work)?;
    let manifest = read_manifest(&top, archive)?;
    verify_tree(&top, &manifest, archive)?;
    if manifest.component != "daemon"
        || format!("{DAEMON_PREFIX}{}-{}", manifest.version, manifest.target) != stem
    {
        return Err(refuse(InputClass::Manifest, Fault::Mismatch, archive));
    }

    let anchor = read_input(
        &top.join(ANCHOR_FILE),
        u64::try_from(package::MAX_ANCHOR_BYTES).unwrap_or(u64::MAX),
    )?;
    let verified = verify_bytes(catalog, &anchor, None, now)?;
    let bundled = top.join(BUNDLE_DIR).join(CATALOG_FILE);
    let bundled_catalog = read_input(
        &bundled,
        u64::try_from(package::MAX_CATALOG_BYTES).unwrap_or(u64::MAX),
    )
    .map_err(|_cause| refuse(InputClass::Catalog, Fault::Missing, archive))?;
    if bundled_catalog != catalog {
        return Err(refuse(InputClass::Catalog, Fault::Mismatch, archive));
    }

    let release = verified.release();
    if release.version().to_string() != manifest.version {
        return Err(refuse(
            InputClass::Catalog,
            Fault::Mismatch,
            "release version",
        ));
    }
    let executables = executable_hashes(&top)?;
    let binary_set = binary_set_digest(&executables)?;
    if release
        .binary_set_digest(&manifest.target)
        .map(package::Sha256Digest::as_str)
        != Some(binary_set.as_str())
    {
        return Err(refuse(InputClass::Catalog, Fault::Mismatch, "binary set"));
    }

    let version = manifest.version.as_str();
    let mut packages = BTreeMap::new();
    let mut attestations = BTreeMap::new();
    for entry in verified.entries() {
        let runtime = entry.runtime_id().as_str();
        let package_bytes = read_input(
            &dir.join(package_name(runtime, version)),
            max_package_bytes(),
        )
        .map_err(|_cause| refuse(InputClass::PackageArchive, Fault::Missing, runtime))?;
        let archived =
            read_archive(&package_bytes, &Limits::DEFAULT).map_err(XtaskError::Package)?;
        if archived.digest() != entry.digest() {
            return Err(refuse(InputClass::PackageArchive, Fault::Mismatch, runtime));
        }
        for attestation in entry.attestations() {
            let file = attestation_name(runtime, &attestation.platform);
            let bytes = read_input(&dir.join(&file), MAX_ATTESTATION_BYTES)
                .map_err(|_cause| refuse(InputClass::Attestation, Fault::Missing, &file))?;
            if format!("sha256:{}", sha256_bytes(&bytes)) != attestation.digest.as_str() {
                return Err(refuse(InputClass::Attestation, Fault::Mismatch, file));
            }
            if attestation.platform == manifest.target {
                attestations.insert(
                    bundled_attestation_name(runtime, &attestation.platform),
                    bytes,
                );
            }
        }
        if entry.supports_platform(&manifest.target) {
            packages.insert(bundled_package_name(runtime), package_bytes);
        }
    }
    check_runtime_dir(&top, &packages, &attestations, archive)
}

/// Largest package archive read from the bundle.
fn max_package_bytes() -> u64 {
    Limits::DEFAULT.max_compressed_bytes
}

/// Largest attestation read from the bundle; equals the attestation
/// module's document limit.
const MAX_ATTESTATION_BYTES: u64 = 64 * 1024;

/// Requires `runtime/packages` and `runtime/attestations` of the archive tree
/// to hold exactly the expected files with exactly the expected bytes, and
/// `runtime` to hold nothing else than the catalog and those directories.
fn check_runtime_dir(
    top: &Path,
    packages: &BTreeMap<String, Vec<u8>>,
    attestations: &BTreeMap<String, Vec<u8>>,
    archive: &str,
) -> Result<(), XtaskError> {
    let runtime_dir = top.join(BUNDLE_DIR);
    let mismatch = || refuse(InputClass::Archive, Fault::Mismatch, archive);
    for (sub, expected) in [(PACKAGES_DIR, packages), (ATTESTATIONS_DIR, attestations)] {
        if &read_dir_contents(&runtime_dir.join(sub), archive)? != expected {
            return Err(mismatch());
        }
    }
    let mut allowed = BTreeSet::from([CATALOG_FILE.to_owned()]);
    if !packages.is_empty() {
        allowed.insert(PACKAGES_DIR.to_owned());
    }
    if !attestations.is_empty() {
        allowed.insert(ATTESTATIONS_DIR.to_owned());
    }
    if read_dir_names(&runtime_dir)? != allowed {
        return Err(mismatch());
    }
    Ok(())
}

/// Names directly in `dir`; a missing directory has none.
fn read_dir_names(dir: &Path) -> Result<BTreeSet<String>, XtaskError> {
    let mut names = BTreeSet::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(io_error(dir))?;
                names.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(dir)(error)),
    }
    Ok(names)
}

/// Names and bytes of the regular files directly in `dir`; a missing
/// directory has none.
fn read_dir_contents(dir: &Path, archive: &str) -> Result<BTreeMap<String, Vec<u8>>, XtaskError> {
    let mut files = BTreeMap::new();
    for name in read_dir_names(dir)? {
        let bytes = read_input(&dir.join(&name), max_package_bytes())
            .map_err(|_cause| refuse(InputClass::Archive, Fault::Mismatch, archive))?;
        files.insert(name, bytes);
    }
    Ok(files)
}
