//! Release bundle assembly.
//!
//! `release assemble` turns the artifacts the producer jobs of one release
//! produced into the bundle the publisher uploads: the final daemon archives
//! (the producer tree plus the signed runtime catalog and the packages and
//! attestations of that target), the unchanged CLI, relay and SDK artifacts,
//! the catalog, the package archives and attestations, and the inventory that
//! lists them all.
//!
//! The assembler trusts no JSON and no tag name. It requires the input
//! directory to hold exactly the files the checked-in policy and the
//! compatibility matrix imply, verifies every checksum file, unpacks each
//! daemon archive with the safe extractor, recomputes the binary-set digest
//! from the unpacked bytes and runs `compat verify` for every matrix row on
//! those recomputed inputs. Only then does it build the catalog, sign it,
//! verify it against the trust anchor the bundle ships and repack the daemon
//! archives with the existing `packaging/` scripts. The bundle is written to a
//! sibling directory and renamed into place, so a refused release leaves no
//! output behind.
//!
//! The catalog `sequence` is derived from the version and `expires_at` from
//! the commit time and the policy, so the same inputs always produce the same
//! bytes; no wall clock is read.

// Rust guideline compliant 2026-10-08

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;

use crate::attestation::{
    binary_set_digest, checked_matrix, executable_hashes, is_commit, lock_path, verify_attestation,
    RecomputeArgs, Row,
};
use crate::catalog::{build_to, io_error, read_input, sign_to, verify, write_output};
use crate::release_inventory::{verify_bundle, write_inventory};
use crate::release_policy::{
    attestation_name, bundled_attestation_name, bundled_package_name, checksum_name, load_policy,
    package_name, sdk_name, ArchiveSlot, Component, Expected, Policy,
};
use crate::release_tree::{
    extract_archive, read_manifest, sha256_bytes, sha256_file, verify_tree, Manifest, MANIFEST_FILE,
};
use crate::XtaskError;

/// Name of the inventory file in a bundle.
pub(crate) const INVENTORY_FILE: &str = "release-inventory.sha256";

/// Name of the signed catalog, at the top of the bundle and in each daemon
/// archive under [`BUNDLE_DIR`].
pub(crate) const CATALOG_FILE: &str = "runtime-catalog.json";

/// Name of the trust anchor at the top of a daemon archive.
pub(crate) const ANCHOR_FILE: &str = "runtime-catalog-anchor.json";

/// Directory of a daemon archive that carries the catalog, packages and
/// attestations.
pub(crate) const BUNDLE_DIR: &str = "runtime";

/// Subdirectory of [`BUNDLE_DIR`] that holds the package archives.
pub(crate) const PACKAGES_DIR: &str = "packages";

/// Subdirectory of [`BUNDLE_DIR`] that holds the attestations.
pub(crate) const ATTESTATIONS_DIR: &str = "attestations";

/// Bits of each version component in the catalog sequence.
///
/// The sequence is `major << 2*BITS | minor << BITS | patch`, which orders
/// like the versions as long as every component fits; a component of `2^BITS`
/// or more is refused instead of wrapping. Changing the width renumbers every
/// future sequence relative to published ones.
const SEQUENCE_COMPONENT_BITS: u32 = 20;

/// Seconds in a day, for the catalog validity window.
const SECONDS_PER_DAY: u64 = 86_400;

/// Mode of a published file.
const PUBLIC_FILE_MODE: u32 = 0o644;

/// Mode of a published directory.
const PUBLIC_DIR_MODE: u32 = 0o755;

/// Prefix of the sibling directory the bundle is staged in.
const STAGING_PREFIX: &str = ".pohunek-assemble-";

/// The packaging scripts the repacking delegates to, relative to the
/// repository root.
const WRITE_MANIFEST_SCRIPT: &str = "packaging/write-manifest";
const ARCHIVE_SCRIPT: &str = "packaging/archive";

/// Signing state a release daemon archive may record; a development build
/// is never a release input.
const DEVELOPMENT_SIGNING: &str = "unsigned-development";

/// Kind of file or value a release refusal concerns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputClass {
    /// The release policy file.
    Policy,
    /// The compatibility matrix.
    Matrix,
    /// The trust anchor file.
    Anchor,
    /// The release version.
    Version,
    /// The release commit.
    Commit,
    /// A release archive (CLI, daemon or relay).
    Archive,
    /// A `.sha256` file.
    Checksum,
    /// An SDK tarball.
    SdkPackage,
    /// A runtime package archive.
    PackageArchive,
    /// A compatibility attestation.
    Attestation,
    /// The signed catalog.
    Catalog,
    /// The inventory file or an entry of it.
    Inventory,
    /// An archive `MANIFEST` or one of its members.
    Manifest,
    /// The output directory.
    Output,
    /// A file that matches no expected input.
    Unexpected,
    /// A packaging script that failed.
    Tool,
}

impl fmt::Display for InputClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Policy => "release policy",
            Self::Matrix => "compatibility matrix",
            Self::Anchor => "trust anchor",
            Self::Version => "release version",
            Self::Commit => "release commit",
            Self::Archive => "release archive",
            Self::Checksum => "checksum file",
            Self::SdkPackage => "SDK tarball",
            Self::PackageArchive => "package archive",
            Self::Attestation => "attestation",
            Self::Catalog => "catalog",
            Self::Inventory => "inventory",
            Self::Manifest => "archive manifest",
            Self::Output => "output directory",
            Self::Unexpected => "input file",
            Self::Tool => "packaging script",
        })
    }
}

/// Why a release input was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// A required file is absent.
    Missing,
    /// A second file fills a slot that one file already fills.
    Duplicate,
    /// The file is not part of the expected set.
    Unexpected,
    /// The path is not a regular file.
    NotRegularFile,
    /// A recomputed SHA-256 differs from the stated one.
    ChecksumMismatch,
    /// Two values that must agree differ.
    Mismatch,
    /// The content does not have the required shape.
    Malformed,
    /// The archive cannot be decoded.
    Corrupt,
    /// A member path leaves the archive tree or is not a plain name.
    UnsafePath,
    /// A member is neither a regular file nor a directory.
    UnsupportedMember,
    /// A size limit was exceeded.
    TooLarge,
    /// The archive holds more members than allowed.
    TooManyMembers,
    /// A value lies outside the supported range.
    OutOfRange,
    /// The output path is already taken.
    AlreadyExists,
    /// Entries are not in ascending order.
    NotSorted,
    /// A packaging script exited unsuccessfully.
    ToolFailed,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "is missing",
            Self::Duplicate => "repeats a slot another file already fills",
            Self::Unexpected => "is not expected",
            Self::NotRegularFile => "is not a regular file",
            Self::ChecksumMismatch => "does not match its SHA-256",
            Self::Mismatch => "does not match the expected value",
            Self::Malformed => "is malformed",
            Self::Corrupt => "cannot be decoded",
            Self::UnsafePath => "has a member path outside the archive tree",
            Self::UnsupportedMember => "has a member that is not a regular file or directory",
            Self::TooLarge => "exceeds a size limit",
            Self::TooManyMembers => "has too many members",
            Self::OutOfRange => "is outside the supported range",
            Self::AlreadyExists => "already exists",
            Self::NotSorted => "is not in ascending order",
            Self::ToolFailed => "failed",
        })
    }
}

/// A refused release input. Names the input class and the file name or
/// field, never file content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseError {
    /// What kind of input was refused.
    pub class: InputClass,
    /// Why it was refused.
    pub fault: Fault,
    /// File name, member path or field of the refused input.
    pub name: String,
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:?} {}", self.class, self.name, self.fault)
    }
}

impl std::error::Error for ReleaseError {}

/// Builds the refusal of `class` named `name`.
pub(crate) fn refuse(class: InputClass, fault: Fault, name: impl Into<String>) -> XtaskError {
    XtaskError::Release(ReleaseError {
        class,
        fault,
        name: name.into(),
    })
}

/// `cargo xtask release` actions.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum ReleaseAction {
    /// Assemble the release bundle from the producer artifacts.
    ///
    /// Refuses unless the input directory holds exactly the files the policy
    /// and the compatibility matrix imply, every checksum and every
    /// attestation recomputes from the bytes, and the signed catalog verifies
    /// against `--anchor`. Nothing is written to `--output` on a refusal.
    Assemble(AssembleArgs),
    /// Verify a release bundle directory against its inventory.
    ///
    /// Recomputes `release-inventory.sha256` against the directory and
    /// re-verifies the catalog against the trust anchor inside every daemon
    /// archive.
    VerifyInventory {
        /// Bundle directory.
        #[arg(long, value_name = "DIR")]
        dir: PathBuf,
        /// Unix second to verify the catalog at; defaults to the current time.
        #[arg(long, value_name = "SECONDS")]
        now: Option<u64>,
    },
}

/// Arguments of `release assemble`.
#[derive(Debug, clap::Args)]
pub(crate) struct AssembleArgs {
    /// Release version `X.Y.Z`.
    #[arg(long, value_name = "X.Y.Z")]
    version: String,
    /// Full 40-character lowercase hex commit the release was built from.
    #[arg(long, value_name = "SHA")]
    commit: String,
    /// Commit time as Unix seconds: the archives' timestamp, the catalog's
    /// reference time and the base of its expiry.
    #[arg(long, value_name = "SECONDS")]
    commit_time: u64,
    /// The checked-in `packaging/release-policy.json`.
    #[arg(long, value_name = "FILE")]
    policy: PathBuf,
    /// The checked-in `compat/matrix.json`.
    #[arg(long, value_name = "FILE")]
    matrix: PathBuf,
    /// Flat directory of the downloaded producer artifacts.
    #[arg(long, value_name = "DIR")]
    inputs: PathBuf,
    /// The trust anchor file the daemon archives ship.
    #[arg(long, value_name = "FILE")]
    anchor: PathBuf,
    /// Signing key file (owner-private, 64 hex characters).
    #[arg(long, value_name = "FILE")]
    key_file: PathBuf,
    /// Key id the key file must have (64 hex characters).
    #[arg(long, value_name = "HEX")]
    key_id: String,
    /// Bundle directory to create; it must not exist.
    #[arg(long, value_name = "DIR")]
    output: PathBuf,
}

/// Runs one `release` action; `root` is this checkout's root.
pub(crate) fn run(action: ReleaseAction, root: &Path) -> Result<(), XtaskError> {
    match action {
        ReleaseAction::Assemble(args) => {
            let summary = assemble(&args, root)?;
            println!(
                "release assemble ok: {} files, sequence {}",
                summary.files, summary.sequence
            );
        }
        ReleaseAction::VerifyInventory { dir, now } => {
            let now = match now {
                Some(now) => now,
                None => crate::catalog::unix_now()?,
            };
            let summary = verify_bundle(&dir, now)?;
            println!(
                "release verify-inventory ok: {} files, {} daemon archives",
                summary.files, summary.daemon_archives
            );
        }
    }
    Ok(())
}

/// What a successful assembly produced.
#[derive(Debug)]
pub(crate) struct AssembleSummary {
    pub(crate) files: usize,
    pub(crate) sequence: u64,
}

/// Parses `X.Y.Z` into its components, each below `2^SEQUENCE_COMPONENT_BITS`.
fn parse_version(text: &str) -> Result<[u64; 3], XtaskError> {
    let malformed = || refuse(InputClass::Version, Fault::Malformed, "version");
    let mut parts = text.split('.');
    let mut components = [0_u64; 3];
    for slot in &mut components {
        let part = parts.next().ok_or_else(malformed)?;
        let plain = !part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'));
        if !plain {
            return Err(malformed());
        }
        *slot = part.parse().map_err(|_cause| malformed())?;
        if *slot >> SEQUENCE_COMPONENT_BITS != 0 {
            return Err(refuse(InputClass::Version, Fault::OutOfRange, "version"));
        }
    }
    if parts.next().is_some() {
        return Err(malformed());
    }
    Ok(components)
}

/// The catalog sequence of a version: it orders like the version.
fn sequence_of(version: [u64; 3]) -> u64 {
    (version[0] << (2 * SEQUENCE_COMPONENT_BITS))
        | (version[1] << SEQUENCE_COMPONENT_BITS)
        | version[2]
}

/// The extracted daemon archive of one target.
struct DaemonTree {
    target: String,
    top: PathBuf,
    manifest: Manifest,
    binary_set: String,
}

/// An attestation that passed verification.
struct Attested {
    runtime: String,
    target: String,
    file: String,
    digest: String,
}

/// Everything the catalog is built from.
struct CatalogPlan<'a> {
    args: &'a AssembleArgs,
    trees: &'a BTreeMap<String, DaemonTree>,
    attested: &'a [Attested],
    runtimes: &'a [String],
    sequence: u64,
    expires_at: u64,
    work: &'a Path,
}

/// Everything the daemon archives are repacked from.
struct Repack<'a> {
    root: &'a Path,
    args: &'a AssembleArgs,
    catalog: &'a [u8],
    attested: &'a [Attested],
    bundle: &'a Path,
}

fn assemble(args: &AssembleArgs, root: &Path) -> Result<AssembleSummary, XtaskError> {
    let version = parse_version(&args.version)?;
    let sequence = sequence_of(version);
    if !is_commit(&args.commit) {
        return Err(refuse(InputClass::Commit, Fault::Malformed, "commit"));
    }
    let policy = load_policy(&args.policy)?;
    let expires_at = policy
        .catalog_validity_days
        .checked_mul(SECONDS_PER_DAY)
        .and_then(|window| args.commit_time.checked_add(window))
        .ok_or_else(|| {
            refuse(
                InputClass::Policy,
                Fault::OutOfRange,
                "catalog_validity_days",
            )
        })?;
    let matrix = checked_matrix(root, &args.matrix)?;
    let daemon_targets = policy.daemon_targets();
    if let Some(row) = matrix
        .rows
        .iter()
        .find(|row| !daemon_targets.contains(&row.target.as_str()))
    {
        return Err(refuse(InputClass::Matrix, Fault::Unexpected, &row.target));
    }
    let anchor = read_input(
        &args.anchor,
        u64::try_from(package::MAX_ANCHOR_BYTES).unwrap_or(u64::MAX),
    )?;
    check_anchor(&anchor)?;

    let output = &args.output;
    ensure_output_free(output)?;
    let parent = match output.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };

    // The input set equals what the policy and the matrix imply.
    Expected::new(&policy, &args.version, &matrix.runtimes, &matrix.rows)
        .check_directory(&args.inputs)?;
    verify_checksums(&policy, &args.version, &args.inputs)?;

    let staging = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(parent)
        .map_err(io_error(parent))?;
    let work = staging.path().join("work");
    let bundle = staging.path().join("bundle");
    for dir in [&work, &bundle] {
        fs::create_dir(dir).map_err(io_error(dir))?;
    }

    // Each daemon archive is unpacked and its binary-set digest recomputed.
    let mut trees = BTreeMap::new();
    for slot in policy
        .archives
        .iter()
        .filter(|slot| slot.component == Component::Daemon)
    {
        let tree = unpack_daemon(args, slot, &anchor, &work)?;
        trees.insert(slot.target.clone(), tree);
    }

    // Every matrix row's attestation is recomputed from those bytes.
    let attested = verify_rows(args, root, &matrix.rows, &trees)?;

    // The catalog is built, signed and verified against the shipped anchor.
    let signed = build_catalog(&CatalogPlan {
        args,
        trees: &trees,
        attested: &attested,
        runtimes: &matrix.runtimes,
        sequence,
        expires_at,
        work: &work,
    })?;
    let catalog = read_input(
        &signed,
        u64::try_from(package::MAX_CATALOG_BYTES).unwrap_or(u64::MAX),
    )?;
    verify(&signed, &args.anchor, None, args.commit_time)?;

    publish_files(args, &policy, &matrix.runtimes, &attested, &bundle)?;
    write_public(&bundle.join(CATALOG_FILE), &catalog)?;
    let repack = Repack {
        root,
        args,
        catalog: &catalog,
        attested: &attested,
        bundle: &bundle,
    };
    for tree in trees.values() {
        repack_daemon(&repack, tree)?;
    }
    write_inventory(&bundle)?;
    let summary = verify_bundle(&bundle, args.commit_time)?;

    fs::set_permissions(&bundle, fs::Permissions::from_mode(PUBLIC_DIR_MODE))
        .map_err(io_error(&bundle))?;
    ensure_output_free(output)?;
    if output.exists() {
        fs::remove_dir(output).map_err(io_error(output))?;
    }
    fs::rename(&bundle, output).map_err(io_error(output))?;
    Ok(AssembleSummary {
        files: summary.files,
        sequence,
    })
}

/// Requires the anchor bytes to be a valid trust anchor.
fn check_anchor(bytes: &[u8]) -> Result<(), XtaskError> {
    package::parse_anchor(bytes)
        .map_err(XtaskError::Anchor)?
        .trust_anchor()
        .map_err(|error| XtaskError::Anchor(package::AnchorFileError::Anchor(error)))?;
    Ok(())
}

/// Requires `output` to be absent or an empty directory.
fn ensure_output_free(output: &Path) -> Result<(), XtaskError> {
    match fs::symlink_metadata(output) {
        Ok(metadata) => {
            let empty_dir = metadata.is_dir()
                && fs::read_dir(output)
                    .map_err(io_error(output))?
                    .next()
                    .is_none();
            if empty_dir {
                Ok(())
            } else {
                Err(refuse(
                    InputClass::Output,
                    Fault::AlreadyExists,
                    output.display().to_string(),
                ))
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(output)(error)),
    }
}

/// Largest accepted `.sha256` file: one digest, two spaces, a file name.
const CHECKSUM_FILE_BYTES: u64 = 1024;

/// Checks the `.sha256` file of every archive and SDK tarball.
fn verify_checksums(policy: &Policy, version: &str, inputs: &Path) -> Result<(), XtaskError> {
    let mut artifacts: Vec<(String, InputClass)> = policy
        .archives
        .iter()
        .map(|slot| (slot.archive_name(version), InputClass::Archive))
        .collect();
    artifacts.extend(
        policy
            .sdk_packages
            .iter()
            .map(|name| (sdk_name(name, version), InputClass::SdkPackage)),
    );
    for (artifact, class) in artifacts {
        let checksum = checksum_name(&artifact);
        let bytes = read_input(&inputs.join(&checksum), CHECKSUM_FILE_BYTES)?;
        let stated = parse_checksum(&bytes, &artifact)
            .ok_or_else(|| refuse(InputClass::Checksum, Fault::Malformed, &checksum))?;
        if sha256_file(&inputs.join(&artifact), class, &artifact)? != stated {
            return Err(refuse(
                InputClass::Checksum,
                Fault::ChecksumMismatch,
                checksum,
            ));
        }
    }
    Ok(())
}

/// The digest of a `sha256sum`-format line `<64 hex>  <artifact>\n`, or
/// `None` when the file is anything else.
fn parse_checksum(bytes: &[u8], artifact: &str) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let line = text.strip_suffix('\n')?;
    let (digest, name) = line.split_once("  ")?;
    let digest_ok = digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    (digest_ok && name == artifact).then(|| digest.to_owned())
}

/// Unpacks and checks the daemon archive of `slot`.
fn unpack_daemon(
    args: &AssembleArgs,
    slot: &ArchiveSlot,
    anchor: &[u8],
    work: &Path,
) -> Result<DaemonTree, XtaskError> {
    let archive = slot.archive_name(&args.version);
    let dest = work.join(&slot.target);
    fs::create_dir(&dest).map_err(io_error(&dest))?;
    let top = extract_archive(
        &args.inputs.join(&archive),
        &archive,
        &slot.stem(&args.version),
        &dest,
    )?;
    let manifest = read_manifest(&top, &archive)?;
    verify_tree(&top, &manifest, &archive)?;
    if manifest.component != "daemon"
        || manifest.version != args.version
        || manifest.target != slot.target
        || manifest.signing == DEVELOPMENT_SIGNING
    {
        return Err(refuse(InputClass::Manifest, Fault::Mismatch, archive));
    }
    let shipped = read_input(
        &top.join(ANCHOR_FILE),
        u64::try_from(package::MAX_ANCHOR_BYTES).unwrap_or(u64::MAX),
    )
    .map_err(|_cause| refuse(InputClass::Anchor, Fault::Missing, &archive))?;
    if shipped != anchor {
        return Err(refuse(InputClass::Anchor, Fault::Mismatch, archive));
    }
    if fs::symlink_metadata(top.join(BUNDLE_DIR)).is_ok() {
        return Err(refuse(InputClass::Archive, Fault::Unexpected, BUNDLE_DIR));
    }
    let binary_set = binary_set_digest(&executable_hashes(&top)?)?;
    Ok(DaemonTree {
        target: slot.target.clone(),
        top,
        manifest,
        binary_set,
    })
}

/// Largest attestation file the assembler reads; equals the attestation
/// module's document limit.
const ATTESTATION_BYTES: u64 = 64 * 1024;

/// Runs `compat verify` on the recomputed inputs of every matrix row.
fn verify_rows(
    args: &AssembleArgs,
    root: &Path,
    rows: &[Row],
    trees: &BTreeMap<String, DaemonTree>,
) -> Result<Vec<Attested>, XtaskError> {
    let mut attested = Vec::with_capacity(rows.len());
    let mut package_digests: BTreeMap<&str, String> = BTreeMap::new();
    for row in rows {
        let tree = trees
            .get(&row.target)
            .ok_or_else(|| refuse(InputClass::Matrix, Fault::Unexpected, &row.target))?;
        let file = attestation_name(&row.runtime, &row.target);
        let path = args.inputs.join(&file);
        let document = verify_attestation(
            &path,
            &RecomputeArgs {
                bin_dir: tree.top.clone(),
                package: args.inputs.join(package_name(&row.runtime, &args.version)),
                lock: lock_path(root, &row.runtime),
                matrix: args.matrix.clone(),
                commit: args.commit.clone(),
                target: row.target.clone(),
            },
        )?;
        let previous =
            package_digests.insert(row.runtime.as_str(), document.package_digest.clone());
        if previous.is_some_and(|digest| digest != document.package_digest) {
            return Err(refuse(InputClass::Attestation, Fault::Mismatch, &file));
        }
        let bytes = read_input(&path, ATTESTATION_BYTES)?;
        attested.push(Attested {
            runtime: document.runtime,
            target: document.target,
            file,
            digest: format!("sha256:{}", sha256_bytes(&bytes)),
        });
    }
    Ok(attested)
}

/// Schema version of the spec `catalog build` reads.
const CATALOG_SPEC_SCHEMA: u64 = 2;

/// Writes the spec for `catalog build`, builds and signs the catalog and
/// returns the signed file's path.
fn build_catalog(plan: &CatalogPlan<'_>) -> Result<PathBuf, XtaskError> {
    let args = plan.args;
    let binary_sets: Vec<_> = plan
        .trees
        .values()
        .map(|tree| json!({ "target": tree.target, "digest": tree.binary_set }))
        .collect();
    let mut packages = Vec::new();
    for runtime in plan.runtimes {
        let rows: Vec<&Attested> = plan
            .attested
            .iter()
            .filter(|row| &row.runtime == runtime)
            .collect();
        let platforms: Vec<&str> = rows.iter().map(|row| row.target.as_str()).collect();
        let attestations: Vec<_> = rows
            .iter()
            .map(|row| json!({ "platform": row.target, "digest": row.digest }))
            .collect();
        packages.push(json!({
            "archive": args.inputs.join(package_name(runtime, &args.version)),
            "platforms": platforms,
            "attestations": attestations,
            "core": format!("={}", args.version),
        }));
    }
    let spec = json!({
        "schema_version": CATALOG_SPEC_SCHEMA,
        "sequence": plan.sequence,
        "expires_at": plan.expires_at,
        "release": {
            "version": args.version,
            "commit": args.commit,
            "binary_sets": binary_sets,
        },
        "revoked_key_ids": [],
        "revoked_digests": [],
        "packages": packages,
    });
    let spec_path = plan.work.join("catalog-spec.json");
    write_output(
        &spec_path,
        &serde_json::to_vec(&spec).map_err(XtaskError::Json)?,
    )?;
    let unsigned = plan.work.join("catalog-unsigned.json");
    build_to(&spec_path, &unsigned)?;
    let signed = plan.work.join(CATALOG_FILE);
    sign_to(
        &unsigned,
        &args.key_file,
        &args.key_id,
        &signed,
        args.commit_time,
    )?;
    Ok(signed)
}

/// Writes `bytes` to `path` with the public file mode.
fn write_public(path: &Path, bytes: &[u8]) -> Result<(), XtaskError> {
    write_output(path, bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(PUBLIC_FILE_MODE)).map_err(io_error(path))
}

/// Copies the file `from` to `to` with the public file mode.
fn copy_public(from: &Path, to: &Path) -> Result<(), XtaskError> {
    fs::copy(from, to).map_err(io_error(to))?;
    fs::set_permissions(to, fs::Permissions::from_mode(PUBLIC_FILE_MODE)).map_err(io_error(to))
}

/// Copies the unchanged artifacts into the bundle: CLI and relay archives
/// and SDK tarballs with their checksums, package archives and attestations.
fn publish_files(
    args: &AssembleArgs,
    policy: &Policy,
    runtimes: &[String],
    attested: &[Attested],
    bundle: &Path,
) -> Result<(), XtaskError> {
    let copy = |name: &str| copy_public(&args.inputs.join(name), &bundle.join(name));
    for slot in policy
        .archives
        .iter()
        .filter(|slot| slot.component != Component::Daemon)
    {
        let archive = slot.archive_name(&args.version);
        copy(&archive)?;
        copy(&checksum_name(&archive))?;
    }
    for name in &policy.sdk_packages {
        let tarball = sdk_name(name, &args.version);
        copy(&tarball)?;
        copy(&checksum_name(&tarball))?;
    }
    for runtime in runtimes {
        copy(&package_name(runtime, &args.version))?;
    }
    for row in attested {
        copy(&row.file)?;
    }
    Ok(())
}

/// Adds the catalog, packages and attestations of the target to the unpacked
/// daemon tree, rewrites its manifest and archives it into the bundle.
fn repack_daemon(repack: &Repack<'_>, tree: &DaemonTree) -> Result<(), XtaskError> {
    let args = repack.args;
    let runtime_dir = tree.top.join(BUNDLE_DIR);
    let rows: Vec<&Attested> = repack
        .attested
        .iter()
        .filter(|row| row.target == tree.target)
        .collect();
    let mut dirs = vec![runtime_dir.clone()];
    if !rows.is_empty() {
        dirs.push(runtime_dir.join(PACKAGES_DIR));
        dirs.push(runtime_dir.join(ATTESTATIONS_DIR));
    }
    for dir in &dirs {
        fs::DirBuilder::new()
            .mode(PUBLIC_DIR_MODE)
            .create(dir)
            .map_err(io_error(dir))?;
    }
    write_public(&runtime_dir.join(CATALOG_FILE), repack.catalog)?;
    for row in &rows {
        copy_public(
            &args.inputs.join(package_name(&row.runtime, &args.version)),
            &runtime_dir
                .join(PACKAGES_DIR)
                .join(bundled_package_name(&row.runtime)),
        )?;
        copy_public(
            &args.inputs.join(&row.file),
            &runtime_dir
                .join(ATTESTATIONS_DIR)
                .join(bundled_attestation_name(&row.runtime, &row.target)),
        )?;
    }

    let manifest_path = tree.top.join(MANIFEST_FILE);
    fs::remove_file(&manifest_path).map_err(io_error(&manifest_path))?;
    let mut manifest_args = vec![
        tree.top.as_os_str().to_owned(),
        "daemon".into(),
        args.version.clone().into(),
        tree.target.clone().into(),
        tree.manifest.signing.clone().into(),
    ];
    manifest_args.extend(tree.manifest.minimum_macos.clone().map(Into::into));
    run_script(
        repack.root,
        WRITE_MANIFEST_SCRIPT,
        &manifest_args,
        args.commit_time,
    )?;
    let parent = tree.top.parent().unwrap_or(&tree.top);
    let stem = tree
        .top
        .file_name()
        .map(std::ffi::OsStr::to_owned)
        .unwrap_or_default();
    run_script(
        repack.root,
        ARCHIVE_SCRIPT,
        &[
            parent.as_os_str().to_owned(),
            stem,
            repack.bundle.as_os_str().to_owned(),
        ],
        args.commit_time,
    )
}

/// Runs the `sh` script `script` of the checkout with the release time as
/// the archive timestamp. Its own diagnostics go to stderr.
fn run_script(
    root: &Path,
    script: &str,
    arguments: &[std::ffi::OsString],
    commit_time: u64,
) -> Result<(), XtaskError> {
    let status = Command::new("sh")
        .arg(root.join(script))
        .args(arguments)
        .env("SOURCE_DATE_EPOCH", commit_time.to_string())
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map_err(io_error(&root.join(script)))?;
    if status.success() {
        Ok(())
    } else {
        Err(refuse(InputClass::Tool, Fault::ToolFailed, script))
    }
}
