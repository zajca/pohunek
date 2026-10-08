//! Compatibility attestations for release assembly.
//!
//! `compat attest` turns a consumer report (written by the out-of-process
//! consumer suite after a successful run) into an attestation document over
//! `(commit, target, binary set, package, upstream lock, suite version,
//! platform, upstream version)`. `compat verify` recomputes every field of such
//! a document from the bytes of the artifacts and refuses on any difference.
//! `compat matrix-check` keeps `compat/matrix.json` equal to the official
//! packages times the declared targets.
//!
//! Every digest is recomputed from file bytes; a digest quoted by a report or
//! an attestation is only ever compared with the recomputed value, never used.
//! `platform` is the target triple: the daemon reports its host platform in
//! the same triple style, so the two are equal today.

// Rust guideline compliant 2026-10-08

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use cli::service::layout::BINARIES;
use nix::fcntl::OFlag;
use package::{read_archive, Limits};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::XtaskError;

/// Schema version of the attestation, the consumer report and the matrix.
const SCHEMA_VERSION: u32 = 1;

/// Domain separator that starts the binary-set digest input; the trailing
/// newline and the version suffix keep it distinct from any future layout.
const BINARY_SET_DOMAIN: &str = "pohunek-binary-set-v1\n";

/// The executables of one release, whose bytes the binary-set digest covers.
///
/// Equal to the installer's `BINARIES` list (CLI, daemon, session worker); a
/// directory missing one of them is refused.
const BINARY_SET: [&str; 3] = BINARIES;

/// Targets every official package is attested on; `compat/matrix.json` holds
/// one row per official package and target.
const DECLARED_TARGETS: [&str; 2] = ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"];

/// Largest accepted JSON input (report, lock, matrix, attestation): all are
/// a few hundred bytes, so this only bounds a hostile file.
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024;

/// Largest accepted executable. The release binaries are tens of MiB; the cap
/// only bounds a hostile or runaway file.
const MAX_BINARY_BYTES: u64 = 1024 * 1024 * 1024;

/// Read buffer for hashing an executable.
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// Mode of the written attestation: a public document.
const OUTPUT_MODE: u32 = 0o644;

/// Archive member that holds the package descriptor.
const DESCRIPTOR_PATH: &str = "runtime.toml";

/// Length of a full lowercase hex git commit id.
const COMMIT_HEX_LEN: usize = 40;

/// Matrix location relative to the repository root.
const MATRIX_PATH: &str = "compat/matrix.json";

/// Directory of the official runtime packages relative to the repository root.
const PACKAGES_DIR: &str = "runtime-packages";

/// Lock location of a runtime relative to the repository root.
const LOCK_FILE: &str = "compatibility-lock.json";

/// Lock location of `runtime` under the repository `root`.
pub(crate) fn lock_path(root: &Path, runtime: &str) -> PathBuf {
    root.join("compat").join(runtime).join(LOCK_FILE)
}

/// Why a field was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// The recomputed value differs from the stated one.
    Mismatch,
    /// The value or its file could not be parsed or has the wrong shape.
    Malformed,
    /// The value is not declared in the compatibility matrix.
    NotDeclared,
    /// A required value or file is absent.
    Missing,
    /// The path is not a regular file (a symbolic link is refused).
    NotRegularFile,
    /// The version lies outside the lock's supported range.
    OutsideSupportedRange,
    /// The value appears more than once.
    Duplicate,
    /// The entries are not in canonical ascending order.
    Unsorted,
    /// The document bytes are not the canonical rendering.
    NotCanonical,
    /// The output file is one of the inputs (same path or same inode).
    OverlapsInput,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mismatch => "does not match the recomputed value",
            Self::Malformed => "is malformed",
            Self::NotDeclared => "is not declared in the compatibility matrix",
            Self::Missing => "is missing",
            Self::NotRegularFile => "is not a regular file",
            Self::OutsideSupportedRange => "is outside the lock's supported range",
            Self::Duplicate => "is duplicated",
            Self::Unsorted => "is not in canonical order",
            Self::NotCanonical => "is not in canonical form",
            Self::OverlapsInput => "is the same file as an input",
        })
    }
}

/// A refused attestation input, naming the field and never echoing content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttestationError {
    /// The attestation field (or input) that was refused.
    pub field: String,
    /// Why it was refused.
    pub fault: Fault,
}

impl fmt::Display for AttestationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "field `{}` {}", self.field, self.fault)
    }
}

impl std::error::Error for AttestationError {}

fn refuse(field: impl Into<String>, fault: Fault) -> XtaskError {
    XtaskError::Attestation(AttestationError {
        field: field.into(),
        fault,
    })
}

/// The attestation document. Fields are declared in ascending key order so
/// the serialized object has sorted keys.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attestation {
    binary_set_digest: String,
    commit: String,
    pub(crate) package_digest: String,
    package_id: String,
    platform: String,
    pub(crate) runtime: String,
    schema: u32,
    suite_version: u32,
    pub(crate) target: String,
    upstream_lock_digest: String,
    upstream_version: String,
}

/// What the consumer suite writes after a successful run.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsumerReport {
    schema: u32,
    runtime: String,
    suite_version: u32,
    package_digest: String,
    upstream_version: String,
    executables: BTreeMap<String, String>,
}

/// `compat/matrix.json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Matrix {
    schema: u32,
    suite_version: u32,
    rows: Vec<Row>,
}

#[derive(Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(deny_unknown_fields)]
pub(crate) struct Row {
    pub(crate) runtime: String,
    pub(crate) target: String,
}

/// The fields of `compat/<runtime>/compatibility-lock.json` the attestation
/// reads; other fields belong to the runtime's own checks.
#[derive(Debug, Deserialize)]
struct Lock {
    schema: u32,
    package: String,
    runtime: String,
    upstream: LockUpstream,
    supported: LockRange,
}

#[derive(Debug, Deserialize)]
struct LockUpstream {
    release: String,
}

#[derive(Debug, Deserialize)]
struct LockRange {
    min: String,
    below: String,
}

/// Arguments that select the bytes an attestation is recomputed from.
#[derive(Debug, clap::Args)]
pub(crate) struct RecomputeArgs {
    /// Directory holding the `pohunek`, `pohunekd` and `pohunek-sessiond`
    /// executables that will be bundled.
    #[arg(long, value_name = "DIR")]
    pub(crate) bin_dir: PathBuf,
    /// Runtime package archive (`package build` output).
    #[arg(long, value_name = "FILE")]
    pub(crate) package: PathBuf,
    /// The runtime's `compat/<runtime>/compatibility-lock.json`.
    #[arg(long, value_name = "FILE")]
    pub(crate) lock: PathBuf,
    /// The checked-in `compat/matrix.json`.
    #[arg(long, value_name = "FILE")]
    pub(crate) matrix: PathBuf,
    /// Full 40-character lowercase hex commit the artifacts were built from.
    #[arg(long, value_name = "SHA")]
    pub(crate) commit: String,
    /// Core target triple the executables were built for.
    #[arg(long, value_name = "TRIPLE")]
    pub(crate) target: String,
}

/// `cargo xtask compat` actions.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum CompatAction {
    /// Write the attestation of one successful consumer run.
    ///
    /// Everything is recomputed from the bytes given: the report is accepted
    /// only when its runtime, package digest, upstream version and executable
    /// hashes equal the recomputed values and `(runtime, target)` is a row of
    /// the matrix.
    Attest {
        /// Consumer report written by the out-of-process consumer suite.
        #[arg(long, value_name = "FILE")]
        report: PathBuf,
        #[command(flatten)]
        inputs: RecomputeArgs,
        /// Attestation file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Recompute an attestation from artifact bytes and compare every field.
    Verify {
        /// Attestation file to check.
        #[arg(long, value_name = "FILE")]
        attestation: PathBuf,
        #[command(flatten)]
        inputs: RecomputeArgs,
    },
    /// Check `compat/matrix.json` against the official packages and targets.
    ///
    /// Offline: reads only the repository tree.
    MatrixCheck {
        /// Repository root; defaults to the checkout this xtask belongs to.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
    },
    /// Stage and verify pinned upstream runtime releases.
    #[command(flatten)]
    Upstream(crate::upstream_stage::UpstreamAction),
}

/// Runs one `compat` action; `default_root` is this checkout's root.
pub(crate) fn run(action: CompatAction, default_root: &Path) -> Result<(), XtaskError> {
    match action {
        CompatAction::Attest {
            report,
            inputs,
            output,
        } => {
            let doc = attest(&report, &inputs, &output)?;
            println!(
                "compat attest ok: {} on {} at suite {}",
                doc.runtime, doc.target, doc.suite_version
            );
        }
        CompatAction::Verify {
            attestation,
            inputs,
        } => {
            let doc = verify_attestation(&attestation, &inputs)?;
            println!(
                "compat verify ok: {} on {} at suite {}",
                doc.runtime, doc.target, doc.suite_version
            );
        }
        CompatAction::MatrixCheck { root } => {
            let root = root.unwrap_or_else(|| default_root.to_path_buf());
            let rows = matrix_check(&root)?;
            println!("compat matrix-check ok: {rows} rows");
        }
        CompatAction::Upstream(action) => crate::upstream_stage::run(action, default_root)?,
    }
    Ok(())
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> XtaskError {
    let path = path.to_path_buf();
    move |source| XtaskError::Io { path, source }
}

fn hex_digest(hasher: Sha256) -> String {
    format!("{:x}", hasher.finalize())
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex_digest(hasher))
}

/// Opens `path` as a regular file without following a final symbolic link.
fn open_regular(path: &Path) -> Result<File, std::io::Error> {
    OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
        .open(path)
}

fn is_symlink_refusal(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32)
}

/// Reads the regular file at `path`, at most `max_bytes`, refusing a final
/// symbolic link.
fn read_input(path: &Path, max_bytes: u64) -> Result<Vec<u8>, XtaskError> {
    let file = open_regular(path).map_err(io_error(path))?;
    let metadata = file.metadata().map_err(io_error(path))?;
    if !metadata.is_file() {
        return Err(XtaskError::UnsupportedFileType(path.to_path_buf()));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error(path))?;
    if u64::try_from(bytes.len()).map_or(true, |read| read > max_bytes) {
        return Err(XtaskError::Usage(format!(
            "`{}` is larger than the {max_bytes}-byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

/// Writes `bytes` to `output` with no-follow semantics.
fn write_output(output: &Path, bytes: &[u8]) -> Result<(), XtaskError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(OUTPUT_MODE)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits())
        .open(output)
        .map_err(|error| {
            if is_symlink_refusal(&error) {
                XtaskError::OutputIsSymlink(output.to_path_buf())
            } else {
                XtaskError::Io {
                    path: output.to_path_buf(),
                    source: error,
                }
            }
        })?;
    file.write_all(bytes).map_err(io_error(output))
}

/// Parses `bytes` as JSON of type `T`, naming `field` on failure.
fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8], field: &str) -> Result<T, XtaskError> {
    serde_json::from_slice(bytes).map_err(|_cause| refuse(field, Fault::Malformed))
}

/// SHA-256 (lowercase hex) of the executable `name` in `dir`.
///
/// The file is opened without following symbolic links and must be regular.
fn hash_executable(dir: &Path, name: &str) -> Result<String, XtaskError> {
    let field = format!("executables.{name}");
    let path = dir.join(name);
    let mut file = open_regular(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            refuse(&field, Fault::Missing)
        } else if is_symlink_refusal(&error) {
            refuse(&field, Fault::NotRegularFile)
        } else {
            XtaskError::Io {
                path: path.clone(),
                source: error,
            }
        }
    })?;
    if !file.metadata().map_err(io_error(&path))?.is_file() {
        return Err(refuse(field, Fault::NotRegularFile));
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let read = file.read(&mut buffer).map_err(io_error(&path))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_BINARY_BYTES {
            return Err(XtaskError::Usage(format!(
                "`{}` is larger than the {MAX_BINARY_BYTES}-byte limit",
                path.display()
            )));
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(hasher))
}

/// SHA-256 of every executable of the binary set found in `dir`, by name.
///
/// A missing executable, a symbolic link or a non-regular file is refused.
pub(crate) fn executable_hashes(dir: &Path) -> Result<BTreeMap<String, String>, XtaskError> {
    BINARY_SET
        .iter()
        .map(|name| Ok(((*name).to_owned(), hash_executable(dir, name)?)))
        .collect()
}

/// The canonical binary-set digest `sha256:<hex>` over per-executable hashes.
///
/// The hashed input is [`BINARY_SET_DOMAIN`], then for each executable name
/// in ascending byte order `<name>\0<64 hex sha256 of the file>\n`. The map
/// must hold exactly the names of [`BINARY_SET`].
pub(crate) fn binary_set_digest(by_name: &BTreeMap<String, String>) -> Result<String, XtaskError> {
    let mut names: Vec<&str> = BINARY_SET.to_vec();
    names.sort_unstable();
    if by_name.len() != names.len() || !names.iter().all(|name| by_name.contains_key(*name)) {
        return Err(refuse("executables", Fault::Mismatch));
    }
    let mut hasher = Sha256::new();
    hasher.update(BINARY_SET_DOMAIN.as_bytes());
    for name in names {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(by_name[name].as_bytes());
        hasher.update(b"\n");
    }
    Ok(format!("sha256:{}", hex_digest(hasher)))
}

/// Reads and validates a matrix file.
fn read_matrix(path: &Path) -> Result<Matrix, XtaskError> {
    let matrix: Matrix = parse_json(&read_input(path, MAX_DOCUMENT_BYTES)?, "matrix")?;
    if matrix.schema != SCHEMA_VERSION {
        return Err(refuse("matrix.schema", Fault::Mismatch));
    }
    if matrix.suite_version == 0 {
        return Err(refuse("matrix.suite_version", Fault::Malformed));
    }
    if matrix
        .rows
        .iter()
        .any(|row| row.runtime.is_empty() || row.target.is_empty())
    {
        return Err(refuse("matrix.rows", Fault::Malformed));
    }
    for pair in matrix.rows.windows(2) {
        match pair[0].cmp(&pair[1]) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => return Err(refuse("matrix.rows", Fault::Duplicate)),
            std::cmp::Ordering::Greater => return Err(refuse("matrix.rows", Fault::Unsorted)),
        }
    }
    Ok(matrix)
}

/// Reads a lock; returns it with the digest of its exact bytes.
fn read_lock(path: &Path) -> Result<(Lock, String), XtaskError> {
    let bytes = read_input(path, MAX_DOCUMENT_BYTES)?;
    let lock: Lock = parse_json(&bytes, "upstream_lock")?;
    if lock.schema != SCHEMA_VERSION {
        return Err(refuse("upstream_lock", Fault::Malformed));
    }
    Ok((lock, sha256_prefixed(&bytes)))
}

/// Package digest, package id and runtime id of an archive, read from its bytes.
fn read_package(path: &Path) -> Result<(String, String, String), XtaskError> {
    let limits = Limits::DEFAULT;
    let bytes = read_input(path, limits.max_compressed_bytes)?;
    let verified = read_archive(&bytes, &limits).map_err(XtaskError::Package)?;
    let descriptor = verified
        .entries()
        .iter()
        .find(|entry| entry.path == DESCRIPTOR_PATH)
        .ok_or_else(|| refuse("package", Fault::Missing))?;
    let malformed = |_cause| refuse("package", Fault::Malformed);
    let text = std::str::from_utf8(&descriptor.contents).map_err(malformed)?;
    let table: toml::Table = text
        .parse()
        .map_err(|_cause| refuse("package", Fault::Malformed))?;
    let id = table
        .get("id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| refuse("package_id", Fault::Malformed))?;
    let runtime = table
        .get("runtime")
        .and_then(toml::Value::as_table)
        .and_then(|runtime| runtime.get("id"))
        .and_then(toml::Value::as_str)
        .ok_or_else(|| refuse("runtime", Fault::Malformed))?;
    Ok((
        verified.digest().as_str().to_owned(),
        id.to_owned(),
        runtime.to_owned(),
    ))
}

pub(crate) fn is_commit(text: &str) -> bool {
    text.len() == COMMIT_HEX_LEN
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// An attestation recomputed from artifact bytes, with the executable hashes
/// it was derived from.
struct Recomputed {
    doc: Attestation,
    executables: BTreeMap<String, String>,
}

/// Recomputes every attestation field from the bytes named by `inputs`.
///
/// Refuses when the commit is not 40 lowercase hex, the package, lock and
/// matrix disagree, `(runtime, target)` is not a matrix row, the lock's
/// pinned release is outside its supported range, or an executable is
/// missing or not a regular file.
fn recompute(inputs: &RecomputeArgs) -> Result<Recomputed, XtaskError> {
    if !is_commit(&inputs.commit) {
        return Err(refuse("commit", Fault::Malformed));
    }
    let matrix = read_matrix(&inputs.matrix)?;
    let (lock, upstream_lock_digest) = read_lock(&inputs.lock)?;
    let (package_digest, package_id, package_runtime) = read_package(&inputs.package)?;
    if package_runtime != lock.runtime {
        return Err(refuse("runtime", Fault::Mismatch));
    }
    if package_id != lock.package {
        return Err(refuse("package_id", Fault::Mismatch));
    }
    if !matrix.rows.iter().any(|row| row.runtime == lock.runtime) {
        return Err(refuse("runtime", Fault::NotDeclared));
    }
    if !matrix
        .rows
        .iter()
        .any(|row| row.runtime == lock.runtime && row.target == inputs.target)
    {
        return Err(refuse("target", Fault::NotDeclared));
    }
    check_supported(&lock)?;
    let executables = executable_hashes(&inputs.bin_dir)?;
    let doc = Attestation {
        binary_set_digest: binary_set_digest(&executables)?,
        commit: inputs.commit.clone(),
        package_digest,
        package_id,
        platform: inputs.target.clone(),
        runtime: lock.runtime,
        schema: SCHEMA_VERSION,
        suite_version: matrix.suite_version,
        target: inputs.target.clone(),
        upstream_lock_digest,
        upstream_version: lock.upstream.release,
    };
    Ok(Recomputed { doc, executables })
}

/// Requires the lock's pinned release to lie in `[min, below)`.
fn check_supported(lock: &Lock) -> Result<(), XtaskError> {
    let parse = |text: &str| {
        Version::parse(text).map_err(|_cause| refuse("upstream_lock", Fault::Malformed))
    };
    let release = parse(&lock.upstream.release)?;
    let min = parse(&lock.supported.min)?;
    let below = parse(&lock.supported.below)?;
    if release < min || release >= below {
        return Err(refuse("upstream_version", Fault::OutsideSupportedRange));
    }
    Ok(())
}

fn canonical_bytes(doc: &Attestation) -> Result<Vec<u8>, XtaskError> {
    let mut bytes = serde_json::to_vec_pretty(doc).map_err(XtaskError::Json)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes the attestation for a successful consumer run and returns it.
fn attest(
    report_path: &Path,
    inputs: &RecomputeArgs,
    output: &Path,
) -> Result<Attestation, XtaskError> {
    let expected = recompute(inputs)?;
    let report: ConsumerReport =
        parse_json(&read_input(report_path, MAX_DOCUMENT_BYTES)?, "report")?;
    if report.schema != SCHEMA_VERSION {
        return Err(refuse("report.schema", Fault::Mismatch));
    }
    if report.runtime != expected.doc.runtime {
        return Err(refuse("runtime", Fault::Mismatch));
    }
    if report.suite_version != expected.doc.suite_version {
        return Err(refuse("suite_version", Fault::Mismatch));
    }
    if report.package_digest != expected.doc.package_digest {
        return Err(refuse("package_digest", Fault::Mismatch));
    }
    if report.upstream_version != expected.doc.upstream_version {
        return Err(refuse("upstream_version", Fault::Mismatch));
    }
    if report.executables.len() != expected.executables.len() {
        return Err(refuse("executables", Fault::Mismatch));
    }
    for (name, hash) in &expected.executables {
        if report.executables.get(name) != Some(hash) {
            return Err(refuse(format!("executables.{name}"), Fault::Mismatch));
        }
    }
    let mut input_files = vec![
        ("report".to_owned(), report_path.to_path_buf()),
        ("package".to_owned(), inputs.package.clone()),
        ("lock".to_owned(), inputs.lock.clone()),
        ("matrix".to_owned(), inputs.matrix.clone()),
    ];
    input_files.extend(
        BINARY_SET
            .iter()
            .map(|name| (format!("executables.{name}"), inputs.bin_dir.join(name))),
    );
    reject_output_overlap(output, &input_files)?;
    write_output(output, &canonical_bytes(&expected.doc)?)?;
    Ok(expected.doc)
}

/// Refuses an `output` that is one of the named input files.
///
/// An existing output is compared by device and inode, which also catches a
/// hard link; an output that does not exist yet is compared by its
/// canonicalized parent and file name. Runs before anything is written, since
/// truncating an input would destroy it while the command reports success.
fn reject_output_overlap(
    output: &Path,
    input_files: &[(String, PathBuf)],
) -> Result<(), XtaskError> {
    use std::os::unix::fs::MetadataExt as _;

    let existing = match std::fs::metadata(output) {
        Ok(metadata) => Some((metadata.dev(), metadata.ino())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error(output)(error)),
    };
    let future = if existing.is_none() {
        let parent = match output.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let name = output
            .file_name()
            .ok_or_else(|| XtaskError::InvalidPath(output.to_path_buf()))?;
        Some(
            std::fs::canonicalize(parent)
                .map_err(io_error(parent))?
                .join(name),
        )
    } else {
        None
    };
    for (field, path) in input_files {
        let same = match (existing, &future) {
            (Some((dev, ino)), _) => {
                let metadata = std::fs::metadata(path).map_err(io_error(path))?;
                metadata.dev() == dev && metadata.ino() == ino
            }
            (None, Some(future)) => std::fs::canonicalize(path).map_err(io_error(path))? == *future,
            (None, None) => false,
        };
        if same {
            return Err(refuse(format!("output.{field}"), Fault::OverlapsInput));
        }
    }
    Ok(())
}

/// Recomputes the attestation from `inputs` and requires the document at
/// `attestation` to equal it field for field and to be canonically rendered.
///
/// The document is never a source of any value: this is the check the
/// release assembler runs on the bytes it downloaded.
pub(crate) fn verify_attestation(
    attestation: &Path,
    inputs: &RecomputeArgs,
) -> Result<Attestation, XtaskError> {
    let bytes = read_input(attestation, MAX_DOCUMENT_BYTES)?;
    let stated: Attestation = parse_json(&bytes, "document")?;
    let expected = recompute(inputs)?.doc;
    if stated.schema != expected.schema {
        return Err(refuse("schema", Fault::Mismatch));
    }
    let text_fields = [
        ("runtime", &stated.runtime, &expected.runtime),
        ("package_id", &stated.package_id, &expected.package_id),
        ("commit", &stated.commit, &expected.commit),
        ("target", &stated.target, &expected.target),
        ("platform", &stated.platform, &expected.platform),
        (
            "binary_set_digest",
            &stated.binary_set_digest,
            &expected.binary_set_digest,
        ),
        (
            "package_digest",
            &stated.package_digest,
            &expected.package_digest,
        ),
        (
            "upstream_lock_digest",
            &stated.upstream_lock_digest,
            &expected.upstream_lock_digest,
        ),
        (
            "upstream_version",
            &stated.upstream_version,
            &expected.upstream_version,
        ),
    ];
    for (field, stated_value, expected_value) in text_fields {
        if stated_value != expected_value {
            return Err(refuse(field, Fault::Mismatch));
        }
    }
    if stated.suite_version != expected.suite_version {
        return Err(refuse("suite_version", Fault::Mismatch));
    }
    if canonical_bytes(&expected)? != bytes {
        return Err(refuse("document", Fault::NotCanonical));
    }
    Ok(expected)
}

/// A matrix that passed [`checked_matrix`], with the official runtimes it covers.
pub(crate) struct CheckedMatrix {
    /// Official runtime ids (the package directory names), ascending.
    pub(crate) runtimes: Vec<String>,
    /// The matrix rows, ascending.
    pub(crate) rows: Vec<Row>,
}

/// Requires `compat/matrix.json` under `root` to hold exactly one row per
/// official package directory and declared target, and every package to have
/// a lock for its runtime. Returns the row count.
fn matrix_check(root: &Path) -> Result<usize, XtaskError> {
    Ok(checked_matrix(root, &root.join(MATRIX_PATH))?.rows.len())
}

/// Requires the matrix file `matrix_path` to hold exactly one row per official
/// package directory under `root` and declared target, and every package to
/// have a lock for its runtime.
pub(crate) fn checked_matrix(root: &Path, matrix_path: &Path) -> Result<CheckedMatrix, XtaskError> {
    let matrix = read_matrix(matrix_path)?;
    let packages = root.join(PACKAGES_DIR);
    let mut runtimes = Vec::new();
    for entry in std::fs::read_dir(&packages).map_err(io_error(&packages))? {
        let entry = entry.map_err(io_error(&packages))?;
        let file_type = entry.file_type().map_err(io_error(&packages))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(XtaskError::InvalidPath(entry.path()));
        };
        if file_type.is_symlink() {
            return Err(refuse(format!("packages.{name}"), Fault::NotRegularFile));
        }
        if file_type.is_dir() {
            runtimes.push(name);
        }
    }
    runtimes.sort_unstable();
    for runtime in &runtimes {
        let lock_path = lock_path(root, runtime);
        let (lock, _digest) = match read_lock(&lock_path) {
            Ok(read) => read,
            Err(XtaskError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(refuse(format!("lock.{runtime}"), Fault::Missing));
            }
            Err(error) => return Err(error),
        };
        if &lock.runtime != runtime {
            return Err(refuse(format!("lock.{runtime}"), Fault::Mismatch));
        }
    }
    for runtime in &runtimes {
        for target in DECLARED_TARGETS {
            if !matrix
                .rows
                .iter()
                .any(|row| &row.runtime == runtime && row.target == target)
            {
                return Err(refuse(format!("rows.{runtime}/{target}"), Fault::Missing));
            }
        }
    }
    for row in &matrix.rows {
        if !runtimes.contains(&row.runtime) || !DECLARED_TARGETS.contains(&row.target.as_str()) {
            return Err(refuse(
                format!("rows.{}/{}", row.runtime, row.target),
                Fault::NotDeclared,
            ));
        }
    }
    Ok(CheckedMatrix {
        runtimes,
        rows: matrix.rows,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt as _};

    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const GNU: &str = "x86_64-unknown-linux-gnu";
    const MUSL: &str = "x86_64-unknown-linux-musl";

    fn sha(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex_digest(hasher)
    }

    fn lock_json(release: &str, below: &str) -> String {
        format!(
            "{{\n  \"schema\": 1,\n  \"package\": \"pohunek.runtime.codex\",\n  \"runtime\": \"codex\",\n  \"upstream\": {{\"release\": \"{release}\"}},\n  \"supported\": {{\"min\": \"0.160.0\", \"below\": \"{below}\"}}\n}}\n"
        )
    }

    fn matrix_json(rows: &[(&str, &str)]) -> String {
        let rows: Vec<String> = rows
            .iter()
            .map(|(runtime, target)| {
                format!("    {{\"runtime\": \"{runtime}\", \"target\": \"{target}\"}}")
            })
            .collect();
        format!(
            "{{\n  \"schema\": 1,\n  \"suite_version\": 3,\n  \"rows\": [\n{}\n  ]\n}}\n",
            rows.join(",\n")
        )
    }

    /// One consistent set of inputs in a temp dir.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self {
                dir: pohunek_test_support::tempdir().expect("tempdir"),
            };
            fs::create_dir(fixture.path("bin")).expect("mkdir");
            for name in BINARY_SET {
                fs::write(fixture.path("bin").join(name), Self::binary_bytes(name)).expect("write");
            }
            fixture.write_package("package.tar.zst", "pohunek.runtime.codex", "codex");
            fs::write(fixture.path("lock.json"), lock_json("0.160.0", "0.161.0")).expect("write");
            fs::write(
                fixture.path("matrix.json"),
                matrix_json(&[("codex", GNU), ("codex", MUSL)]),
            )
            .expect("write");
            fixture.write_report(&fixture.report("0.160.0"));
            fixture
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn binary_bytes(name: &str) -> Vec<u8> {
            format!("image of {name}").into_bytes()
        }

        fn write_package(&self, file: &str, id: &str, runtime: &str) {
            let source = self.path(&format!("src-{file}"));
            fs::create_dir_all(&source).expect("mkdir");
            fs::write(
                source.join("runtime.toml"),
                format!("id = \"{id}\"\nversion = \"1.0.0\"\n[runtime]\nid = \"{runtime}\"\n"),
            )
            .expect("write");
            crate::run([
                "package".into(),
                "build".into(),
                source.display().to_string(),
                "-o".into(),
                self.path(file).display().to_string(),
            ])
            .expect("package build");
        }

        fn package_digest(&self, file: &str) -> String {
            format!("sha256:{}", sha(&fs::read(self.path(file)).expect("read")))
        }

        fn report(&self, upstream_version: &str) -> serde_json::Value {
            let executables: serde_json::Map<String, serde_json::Value> = BINARY_SET
                .iter()
                .map(|name| {
                    (
                        (*name).to_owned(),
                        serde_json::Value::String(sha(&Self::binary_bytes(name))),
                    )
                })
                .collect();
            serde_json::json!({
                "schema": 1,
                "runtime": "codex",
                "suite_version": 3,
                "package_digest": self.package_digest("package.tar.zst"),
                "upstream_version": upstream_version,
                "executables": executables,
            })
        }

        fn write_report(&self, report: &serde_json::Value) {
            fs::write(self.path("report.json"), report.to_string()).expect("write");
        }

        fn inputs(&self, commit: &str, target: &str) -> Vec<String> {
            let path = |name: &str| self.path(name).display().to_string();
            vec![
                "--bin-dir".into(),
                path("bin"),
                "--package".into(),
                path("package.tar.zst"),
                "--lock".into(),
                path("lock.json"),
                "--matrix".into(),
                path("matrix.json"),
                "--commit".into(),
                commit.into(),
                "--target".into(),
                target.into(),
            ]
        }

        fn attest(&self, commit: &str, target: &str) -> Result<(), XtaskError> {
            let mut args = vec![
                "compat".to_owned(),
                "attest".into(),
                "--report".into(),
                self.path("report.json").display().to_string(),
                "--output".into(),
                self.path("attestation.json").display().to_string(),
            ];
            args.extend(self.inputs(commit, target));
            crate::run(args)
        }

        fn verify(&self) -> Result<(), XtaskError> {
            let mut args = vec![
                "compat".to_owned(),
                "verify".into(),
                "--attestation".into(),
                self.path("attestation.json").display().to_string(),
            ];
            args.extend(self.inputs(COMMIT, GNU));
            crate::run(args)
        }
    }

    fn expect_refusal(result: Result<(), XtaskError>, field: &str, fault: Fault) {
        match result {
            Err(XtaskError::Attestation(error)) => {
                assert_eq!(
                    (error.field.as_str(), error.fault),
                    (field, fault),
                    "wrong refusal: {error}"
                );
            }
            other => panic!("expected refusal of `{field}`, got {other:?}"),
        }
    }

    #[test]
    fn binary_set_digest_has_the_documented_layout() {
        // Expected value computed outside this code base with
        // `printf 'pohunek-binary-set-v1\npohunek\0<h1>\npohunek-sessiond\0<h2>\npohunekd\0<h3>\n' | sha256sum`
        // where hN is the digit N repeated 64 times.
        let hashes: BTreeMap<String, String> = [
            ("pohunek", "1".repeat(64)),
            ("pohunekd", "3".repeat(64)),
            ("pohunek-sessiond", "2".repeat(64)),
        ]
        .into_iter()
        .map(|(name, hash)| (name.to_owned(), hash))
        .collect();
        assert_eq!(
            binary_set_digest(&hashes).expect("digest"),
            "sha256:f5d9817388e28e2b90ade9853f0df626e74606853d493f2a85cd27fcb2beb3a5"
        );
    }

    #[test]
    fn a_good_report_yields_a_verifiable_deterministic_attestation() {
        let fixture = Fixture::new();
        fixture.attest(COMMIT, GNU).expect("attest");
        fixture.verify().expect("verify");
        let first = fs::read(fixture.path("attestation.json")).expect("read");
        assert_eq!(first.last(), Some(&b'\n'));
        assert_eq!(
            fs::metadata(fixture.path("attestation.json"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            OUTPUT_MODE
        );
        let text = String::from_utf8(first.clone()).expect("utf8");
        let keys: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("  \""))
            .filter_map(|line| line.split('"').next())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
        assert_eq!(keys.len(), 11);
        assert!(text.contains(&format!("\"commit\": \"{COMMIT}\"")));
        assert!(text.contains(&format!("\"platform\": \"{GNU}\"")));
        assert!(text.contains("\"suite_version\": 3"));
        assert!(text.contains(&format!(
            "\"upstream_lock_digest\": \"sha256:{}\"",
            sha(lock_json("0.160.0", "0.161.0").as_bytes())
        )));
        assert!(text.contains(&format!(
            "\"package_digest\": \"{}\"",
            fixture.package_digest("package.tar.zst")
        )));

        fixture.attest(COMMIT, GNU).expect("attest again");
        assert_eq!(
            fs::read(fixture.path("attestation.json")).expect("read"),
            first
        );
    }

    #[test]
    fn a_flipped_byte_in_a_binary_is_refused() {
        let fixture = Fixture::new();
        fs::write(fixture.path("bin/pohunekd"), b"image of pohunekE").expect("write");
        expect_refusal(
            fixture.attest(COMMIT, GNU),
            "executables.pohunekd",
            Fault::Mismatch,
        );
        assert!(!fixture.path("attestation.json").exists());
    }

    #[test]
    fn a_report_listing_another_binarys_hash_is_refused() {
        let fixture = Fixture::new();
        let mut report = fixture.report("0.160.0");
        report["executables"]["pohunek"] =
            serde_json::Value::String(sha(&Fixture::binary_bytes("pohunekd")));
        fixture.write_report(&report);
        expect_refusal(
            fixture.attest(COMMIT, GNU),
            "executables.pohunek",
            Fault::Mismatch,
        );
    }

    #[test]
    fn a_report_for_another_package_archive_is_refused() {
        let fixture = Fixture::new();
        fixture.write_package("other.tar.zst", "pohunek.runtime.codex", "codex");
        fs::write(fixture.path("src-other.tar.zst/extra.toml"), b"x = 1\n").expect("write");
        crate::run([
            "package".into(),
            "build".into(),
            fixture.path("src-other.tar.zst").display().to_string(),
            "-o".into(),
            fixture.path("other.tar.zst").display().to_string(),
        ])
        .expect("package build");
        let mut report = fixture.report("0.160.0");
        report["package_digest"] =
            serde_json::Value::String(fixture.package_digest("other.tar.zst"));
        fixture.write_report(&report);
        expect_refusal(
            fixture.attest(COMMIT, GNU),
            "package_digest",
            Fault::Mismatch,
        );
    }

    #[test]
    fn a_runtime_or_target_outside_the_matrix_is_refused() {
        let fixture = Fixture::new();
        fs::write(fixture.path("matrix.json"), matrix_json(&[("pi", GNU)])).expect("write");
        expect_refusal(fixture.attest(COMMIT, GNU), "runtime", Fault::NotDeclared);

        fs::write(fixture.path("matrix.json"), matrix_json(&[("codex", GNU)])).expect("write");
        expect_refusal(fixture.attest(COMMIT, MUSL), "target", Fault::NotDeclared);
        expect_refusal(
            fixture.attest(COMMIT, "aarch64-apple-darwin"),
            "target",
            Fault::NotDeclared,
        );
    }

    #[test]
    fn an_upstream_version_other_than_the_pinned_one_is_refused() {
        let fixture = Fixture::new();
        fixture.write_report(&fixture.report("0.160.1"));
        expect_refusal(
            fixture.attest(COMMIT, GNU),
            "upstream_version",
            Fault::Mismatch,
        );
    }

    #[test]
    fn a_pinned_release_outside_the_supported_range_is_refused() {
        let fixture = Fixture::new();
        fs::write(fixture.path("lock.json"), lock_json("0.161.0", "0.161.0")).expect("write");
        fixture.write_report(&fixture.report("0.161.0"));
        expect_refusal(
            fixture.attest(COMMIT, GNU),
            "upstream_version",
            Fault::OutsideSupportedRange,
        );
    }

    #[test]
    fn a_commit_that_is_not_40_lowercase_hex_is_refused() {
        let fixture = Fixture::new();
        for commit in [
            "0123456",
            &COMMIT.to_uppercase(),
            &format!("{COMMIT}0"),
            "main",
        ] {
            expect_refusal(fixture.attest(commit, GNU), "commit", Fault::Malformed);
        }
    }

    #[test]
    fn a_report_from_an_older_suite_is_refused_after_a_matrix_bump() {
        let fixture = Fixture::new();
        fixture.attest(COMMIT, GNU).expect("attest");
        fs::write(
            fixture.path("matrix.json"),
            matrix_json(&[("codex", GNU), ("codex", MUSL)])
                .replace("\"suite_version\": 3", "\"suite_version\": 4"),
        )
        .expect("write");
        expect_refusal(
            fixture.attest(COMMIT, GNU),
            "suite_version",
            Fault::Mismatch,
        );
    }

    #[test]
    fn an_output_that_is_an_input_is_refused_and_leaves_it_intact() {
        let fixture = Fixture::new();
        for (field, name) in [
            ("report", "report.json"),
            ("package", "package.tar.zst"),
            ("lock", "lock.json"),
            ("matrix", "matrix.json"),
            ("executables.pohunekd", "bin/pohunekd"),
        ] {
            let before = fs::read(fixture.path(name)).expect("read");
            let link = fixture.path(&format!("link-{}", name.replace('/', "-")));
            fs::hard_link(fixture.path(name), &link).expect("hard link");
            for output in [fixture.path(name), link] {
                let mut args = vec![
                    "compat".to_owned(),
                    "attest".into(),
                    "--report".into(),
                    fixture.path("report.json").display().to_string(),
                    "--output".into(),
                    output.display().to_string(),
                ];
                args.extend(fixture.inputs(COMMIT, GNU));
                expect_refusal(
                    crate::run(args),
                    &format!("output.{field}"),
                    Fault::OverlapsInput,
                );
            }
            assert_eq!(fs::read(fixture.path(name)).expect("read"), before);
        }
    }

    #[test]
    fn a_runtime_that_differs_between_report_lock_and_package_is_refused() {
        let fixture = Fixture::new();
        let mut report = fixture.report("0.160.0");
        report["runtime"] = serde_json::Value::String("pi".to_owned());
        fixture.write_report(&report);
        expect_refusal(fixture.attest(COMMIT, GNU), "runtime", Fault::Mismatch);

        let fixture = Fixture::new();
        fixture.write_package("package.tar.zst", "pohunek.runtime.pi", "pi");
        expect_refusal(fixture.attest(COMMIT, GNU), "runtime", Fault::Mismatch);
    }

    #[test]
    fn a_lock_changed_after_attesting_fails_verification() {
        let fixture = Fixture::new();
        fixture.attest(COMMIT, GNU).expect("attest");
        fs::write(fixture.path("lock.json"), lock_json("0.160.0", "0.162.0")).expect("write");
        expect_refusal(fixture.verify(), "upstream_lock_digest", Fault::Mismatch);
    }

    #[test]
    fn verification_names_each_recomputed_field_that_differs() {
        let fixture = Fixture::new();
        fixture.attest(COMMIT, GNU).expect("attest");

        let mut doc: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.path("attestation.json")).expect("read"))
                .expect("json");
        doc["binary_set_digest"] = serde_json::Value::String(format!("sha256:{}", "0".repeat(64)));
        fs::write(fixture.path("attestation.json"), doc.to_string()).expect("write");
        expect_refusal(fixture.verify(), "binary_set_digest", Fault::Mismatch);

        fixture.attest(COMMIT, GNU).expect("attest");
        fs::write(fixture.path("bin/pohunek"), b"swapped").expect("write");
        expect_refusal(fixture.verify(), "binary_set_digest", Fault::Mismatch);
    }

    #[test]
    fn a_reformatted_attestation_is_not_canonical() {
        let fixture = Fixture::new();
        fixture.attest(COMMIT, GNU).expect("attest");
        let doc: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.path("attestation.json")).expect("read"))
                .expect("json");
        fs::write(fixture.path("attestation.json"), doc.to_string()).expect("write");
        expect_refusal(fixture.verify(), "document", Fault::NotCanonical);
    }

    #[test]
    fn a_missing_or_symlinked_binary_is_refused() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.path("bin/pohunek-sessiond")).expect("remove");
        expect_refusal(
            fixture.attest(COMMIT, GNU),
            "executables.pohunek-sessiond",
            Fault::Missing,
        );

        fs::write(
            fixture.path("real"),
            Fixture::binary_bytes("pohunek-sessiond"),
        )
        .expect("write");
        symlink(fixture.path("real"), fixture.path("bin/pohunek-sessiond")).expect("symlink");
        expect_refusal(
            fixture.attest(COMMIT, GNU),
            "executables.pohunek-sessiond",
            Fault::NotRegularFile,
        );
    }

    fn matrix_root(dirs: &[&str], rows: &[(&str, &str)]) -> tempfile::TempDir {
        let root = pohunek_test_support::tempdir().expect("tempdir");
        for dir in dirs {
            fs::create_dir_all(root.path().join("runtime-packages").join(dir)).expect("mkdir");
            fs::create_dir_all(root.path().join("compat").join(dir)).expect("mkdir");
            fs::write(
                root.path().join("compat").join(dir).join(LOCK_FILE),
                lock_json("0.160.0", "0.161.0").replace("codex", dir),
            )
            .expect("write");
        }
        fs::write(root.path().join("runtime-packages/README.md"), b"docs\n").expect("write");
        fs::write(root.path().join(MATRIX_PATH), matrix_json(rows)).expect("write");
        root
    }

    fn run_matrix_check(root: &Path) -> Result<(), XtaskError> {
        crate::run([
            "compat".to_owned(),
            "matrix-check".into(),
            "--root".into(),
            root.display().to_string(),
        ])
    }

    #[test]
    fn matrix_check_accepts_exactly_packages_times_targets() {
        let root = matrix_root(
            &["codex", "pi"],
            &[("codex", GNU), ("codex", MUSL), ("pi", GNU), ("pi", MUSL)],
        );
        run_matrix_check(root.path()).expect("consistent matrix");
    }

    #[test]
    fn matrix_check_refuses_a_package_without_rows_and_a_row_without_a_package() {
        let root = matrix_root(&["codex", "pi"], &[("codex", GNU), ("codex", MUSL)]);
        expect_refusal(
            run_matrix_check(root.path()),
            &format!("rows.pi/{GNU}"),
            Fault::Missing,
        );

        let root = matrix_root(
            &["codex"],
            &[("codex", GNU), ("codex", MUSL), ("ghost", GNU)],
        );
        expect_refusal(
            run_matrix_check(root.path()),
            &format!("rows.ghost/{GNU}"),
            Fault::NotDeclared,
        );

        let root = matrix_root(
            &["codex"],
            &[("codex", GNU), ("codex", MUSL), ("codex", "z-target")],
        );
        expect_refusal(
            run_matrix_check(root.path()),
            "rows.codex/z-target",
            Fault::NotDeclared,
        );
    }

    #[test]
    fn matrix_check_refuses_duplicate_and_unsorted_rows_and_a_missing_lock() {
        let root = matrix_root(
            &["codex"],
            &[("codex", GNU), ("codex", GNU), ("codex", MUSL)],
        );
        expect_refusal(
            run_matrix_check(root.path()),
            "matrix.rows",
            Fault::Duplicate,
        );

        let root = matrix_root(&["codex"], &[("codex", MUSL), ("codex", GNU)]);
        expect_refusal(
            run_matrix_check(root.path()),
            "matrix.rows",
            Fault::Unsorted,
        );

        let root = matrix_root(&["codex"], &[("codex", GNU), ("codex", MUSL)]);
        fs::remove_file(root.path().join("compat/codex").join(LOCK_FILE)).expect("remove");
        expect_refusal(run_matrix_check(root.path()), "lock.codex", Fault::Missing);
    }

    #[test]
    fn the_checked_in_matrix_matches_the_repository() {
        run_matrix_check(&pohunek_test_support::workspace_root())
            .expect("compat/matrix.json is current");
    }
}
