//! Builder evidence of `cargo xtask package build --evidence-dir <D>`.
//!
//! Next to the archive, the build writes four documents into `D`:
//!
//! - `<archive>.sha256`: the archive digest in `sha256sum -c` format;
//! - `<rt>.manifest.json`: the package identity, the archive digest and every
//!   archive entry (path, size, SHA-256, executable flag);
//! - `<rt>.provenance-input.json`: the subject (archive name and digest), the
//!   builder, the source commit with the Git tree of `runtime-packages/<rt>`,
//!   and the materials (the archive entries and the `upstream` object of the
//!   runtime's compatibility lock);
//! - `<rt>.compat-vector.json`: the package half of a compatibility
//!   attestation, with the attestation's field names, plus the lock's
//!   `upstream` object and supported range and the runtime's matrix targets.
//!
//! The lock's `upstream` object is copied whole, whatever its kind (an npm
//! package, a provisioned runtime): only `upstream.release` is required, so
//! no field of one upstream kind is imposed on another.
//!
//! Every document is deterministic: no timestamps and no absolute paths, keys
//! in ascending order at every level (struct fields are declared sorted, the
//! copied `upstream` object is re-sorted), two-space indented JSON with one
//! trailing newline, the rendering `compat attest` uses. Two clean clones of
//! one commit therefore produce byte-identical evidence.
//!
//! The package directory must be `runtime-packages/<rt>` of a Git work tree
//! whose descriptor declares runtime `<rt>`. `git status` must report no
//! modified, untracked or ignored file in that directory, the lock or the
//! matrix. Because a clean status does not prove equal bytes (skip-worktree
//! and assume-unchanged entries, `core.autocrlf`, `core.fileMode=false`,
//! clean and smudge filters all hide differences), the bytes themselves are
//! compared with the commit `HEAD` names, resolved once with replace refs
//! ignored: the archive entries must be exactly the blobs of the stated tree
//! of that commit (same paths, mode `100755` exactly for executable entries,
//! the Git blob id of the entry bytes), and the lock and matrix bytes must be
//! the blobs that commit stores at their paths.
//!
//! Every input is read and every document rendered before anything is
//! written. The archive and the four documents are then staged as temporary
//! files next to their targets and renamed into place one by one, archive
//! first, only after all five were written. A refusal or a failure while
//! staging writes nothing; a rename that fails midway can leave the new
//! archive next to a mix of new and old evidence files.

// Rust guideline compliant 2026-10-12

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use package::{read_archive, ArchiveEntry, Limits, VerifiedArchive};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::affected::{GIT, REPOSITORY_REDIRECTING_VARS};
use crate::attestation::{
    check_supported, lock_path, parse_lock, parse_matrix, refuse, sha256_prefixed, Fault,
    MATRIX_PATH, MAX_DOCUMENT_BYTES, PACKAGES_DIR,
};
use crate::catalog::{self, descriptor_identity, hex, DESCRIPTOR_PATH};
use crate::runtime_package::{build_bytes, reject_output_inside};
use crate::XtaskError;

/// Schema version of the manifest, provenance input and compatibility vector.
const SCHEMA_VERSION: u32 = 1;

/// The builder command the provenance input names. It is the canonical form,
/// never the invoked argv, whose paths differ between clones and runners.
const BUILDER_COMMAND: &str = "cargo xtask package build";

/// Version of this xtask, which is the workspace version.
const XTASK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Suffix of the manifest projection file.
const MANIFEST_SUFFIX: &str = ".manifest.json";

/// Suffix of the provenance input file.
const PROVENANCE_SUFFIX: &str = ".provenance-input.json";

/// Suffix of the compatibility vector file.
const COMPAT_VECTOR_SUFFIX: &str = ".compat-vector.json";

/// Suffix of the archive checksum file.
const CHECKSUM_SUFFIX: &str = ".sha256";

/// Mode of the archive and every evidence file: public documents.
const OUTPUT_MODE: u32 = 0o644;

/// Name prefix of the staged files; the leading dot keeps a leftover of a
/// killed run out of the `*.tar.zst` and `*.json` globs of the workflows.
const STAGING_PREFIX: &str = ".pohunek-staging-";

/// Length of a SHA-1 object id in hex, Git's default object format.
const SHA1_HEX_LEN: usize = 40;

/// Length of a SHA-256 object id in hex (`git init --object-format=sha256`).
const SHA256_HEX_LEN: usize = 64;

/// Git tree entry mode of a regular file.
const MODE_REGULAR: &str = "100644";

/// Git tree entry mode of an executable file.
const MODE_EXECUTABLE: &str = "100755";

/// Algorithm prefix of a package digest; the checksum line carries the bare hex.
const DIGEST_PREFIX: &str = "sha256:";

/// One archive entry. `sha256` is lowercase hex without a prefix, as in the
/// installed package root's `manifest.json`.
#[derive(Debug, Serialize)]
struct Entry<'a> {
    executable: bool,
    path: &'a str,
    sha256: String,
    size: u64,
}

/// `<rt>.manifest.json`.
#[derive(Debug, Serialize)]
struct Manifest<'a> {
    digest: &'a str,
    entries: &'a [Entry<'a>],
    package_id: &'a str,
    runtime_api: u32,
    runtime_id: &'a str,
    schema: u32,
    version: &'a str,
}

/// `<rt>.provenance-input.json`.
#[derive(Debug, Serialize)]
struct Provenance<'a> {
    builder: Builder<'a>,
    materials: Materials<'a>,
    schema: u32,
    source: Source<'a>,
    subject: Subject<'a>,
}

#[derive(Debug, Serialize)]
struct Builder<'a> {
    command: &'a str,
    xtask_version: &'a str,
}

#[derive(Debug, Serialize)]
struct Materials<'a> {
    entries: &'a [Entry<'a>],
    upstream: &'a Value,
}

/// The lock field the evidence copies; `compat attest` reads the others.
#[derive(Debug, Deserialize)]
struct LockReference {
    upstream: Value,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Source<'a> {
    commit: &'a str,
    path: &'a str,
    tree: &'a str,
}

#[derive(Debug, Serialize)]
struct Subject<'a> {
    digest: &'a str,
    name: &'a str,
}

/// `<rt>.compat-vector.json`: the attestation fields a package build fixes.
#[derive(Debug, Serialize)]
struct CompatVector<'a> {
    package_digest: &'a str,
    package_id: &'a str,
    runtime: &'a str,
    schema: u32,
    suite_version: u32,
    supported: Supported<'a>,
    targets: Vec<&'a str>,
    upstream: &'a Value,
    upstream_lock_digest: &'a str,
    upstream_version: &'a str,
}

#[derive(Debug, Serialize)]
struct Supported<'a> {
    below: &'a str,
    min: &'a str,
}

/// Builds the archive of `dir` into `output` and its evidence into `evidence_dir`.
///
/// Returns the package digest.
///
/// # Errors
///
/// Refuses, before writing anything, an evidence directory inside the package
/// directory, an archive output inside it, a package directory that is not
/// `runtime-packages/<rt>` of a Git work tree with runtime `<rt>`, a modified,
/// untracked or ignored file in the package directory, the lock or the matrix,
/// a missing lock, a lock of another runtime or package, a lock without
/// `upstream.release` or with a fractional number in `upstream`, a runtime
/// without matrix rows, an evidence path
/// that is not a directory, and an evidence file that is a symbolic link or
/// not a regular file.
pub(crate) fn build(dir: &Path, output: &Path, evidence_dir: &Path) -> Result<String, XtaskError> {
    reject_evidence_inside(dir, evidence_dir)?;
    check_output(output)?;
    reject_output_inside(dir, output)?;
    let archive_name = archive_name(output)?;

    let package_dir = fs::canonicalize(dir).map_err(catalog::io_error(dir))?;
    let root = toplevel(&package_dir)?;
    let runtime = package_name(&root, &package_dir)?;
    let source_path = format!("{PACKAGES_DIR}/{runtime}");
    let lock_file = lock_path(&root, &runtime);
    let lock_relative = lock_file
        .strip_prefix(&root)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| XtaskError::InvalidPath(lock_file.clone()))?
        .to_owned();
    require_clean(&root, &[&source_path, &lock_relative, MATRIX_PATH])?;
    let format = object_format(&root)?;
    // Every later lookup names this commit, so a HEAD that moves during the
    // build cannot pair the stated commit with another tree or lock.
    let commit = rev_parse(&root, format, "HEAD^{commit}")?;
    let tree = rev_parse(&root, format, &format!("{commit}:{source_path}"))?;
    require_tree(&root, &tree)?;

    let bytes = build_bytes(&package_dir, &Limits::DEFAULT)?;
    let archive = read_archive(&bytes, &Limits::DEFAULT).map_err(XtaskError::Package)?;
    require_archive_is_tree(&root, format, &tree, &source_path, &archive)?;
    let lock_bytes = read_lock_bytes(&lock_file, &runtime)?;
    require_commit_blob(&root, format, &commit, &lock_relative, &lock_bytes)?;
    let matrix_bytes = catalog::read_input(&root.join(MATRIX_PATH), MAX_DOCUMENT_BYTES)?;
    require_commit_blob(&root, format, &commit, MATRIX_PATH, &matrix_bytes)?;

    let documents = render(&Inputs {
        runtime: &runtime,
        archive: &archive,
        archive_name: &archive_name,
        lock_bytes: &lock_bytes,
        matrix_bytes: &matrix_bytes,
        source: Source {
            commit: &commit,
            path: &source_path,
            tree: &tree,
        },
    })?;

    check_targets(evidence_dir, &documents)?;
    publish(output, &bytes, evidence_dir, &documents)?;
    Ok(archive.digest().as_str().to_owned())
}

/// Refuses an archive output that exists as anything but a regular file, and
/// one whose directory does not exist.
fn check_output(output: &Path) -> Result<(), XtaskError> {
    match fs::symlink_metadata(output) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(XtaskError::OutputIsSymlink(output.to_path_buf()));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(XtaskError::Usage(format!(
                "archive output `{}` exists and is not a regular file",
                output.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(catalog::io_error(output)(error)),
    }
    require_directory(parent_dir(output), output)
}

/// Requires `dir`, the directory `target` is written into, to exist.
fn require_directory(dir: &Path, target: &Path) -> Result<(), XtaskError> {
    match fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(XtaskError::Usage(format!(
            "the directory of `{}` is not a directory",
            target.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(XtaskError::Usage(
            format!("the directory of `{}` does not exist", target.display()),
        )),
        Err(error) => Err(catalog::io_error(dir)(error)),
    }
}

/// The directory `path` names its entry in; `.` for a bare file name.
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Writes the archive to `output` and the documents into `evidence_dir`.
///
/// Creates `evidence_dir` when missing (its parent exists, see
/// [`check_targets`]) and removes it again when publishing fails, which
/// succeeds only while no file was renamed into it.
fn publish(
    output: &Path,
    archive: &[u8],
    evidence_dir: &Path,
    documents: &[(String, Vec<u8>)],
) -> Result<(), XtaskError> {
    let created = match fs::symlink_metadata(evidence_dir) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(evidence_dir).map_err(catalog::io_error(evidence_dir))?;
            true
        }
        Err(error) => return Err(catalog::io_error(evidence_dir)(error)),
    };
    let result = stage_and_rename(output, archive, evidence_dir, documents);
    if result.is_err() && created {
        // The publishing error is the one reported; a directory that cannot be
        // removed is named so that it is not mistaken for evidence.
        if let Err(cleanup) = fs::remove_dir(evidence_dir) {
            eprintln!(
                "package evidence: could not remove the directory `{}`: {cleanup}",
                evidence_dir.display()
            );
        }
    }
    result
}

/// Stages all five files next to their targets, then renames each into place.
///
/// A failure before the renames drops the staged files, which deletes them. A
/// failed rename leaves the files renamed before it in place.
fn stage_and_rename(
    output: &Path,
    archive: &[u8],
    evidence_dir: &Path,
    documents: &[(String, Vec<u8>)],
) -> Result<(), XtaskError> {
    let mut staged = vec![stage(output, archive)?];
    for (name, document) in documents {
        staged.push(stage(&evidence_dir.join(name), document)?);
    }
    for (file, target) in staged {
        file.persist(&target).map_err(|error| XtaskError::Io {
            path: target.clone(),
            source: error.error,
        })?;
    }
    Ok(())
}

/// Writes `bytes` to a new temporary file in the directory of `target`, with
/// the output mode, synced to disk.
fn stage(target: &Path, bytes: &[u8]) -> Result<(NamedTempFile, PathBuf), XtaskError> {
    let dir = parent_dir(target);
    let mut file = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempfile_in(dir)
        .map_err(catalog::io_error(dir))?;
    let path = file.path().to_path_buf();
    file.write_all(bytes).map_err(catalog::io_error(&path))?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(OUTPUT_MODE))
        .map_err(catalog::io_error(&path))?;
    file.as_file()
        .sync_all()
        .map_err(catalog::io_error(&path))?;
    Ok((file, target.to_path_buf()))
}

/// Refuses an evidence path that exists but is not a directory, a missing
/// one whose parent does not exist, and an evidence file that exists as
/// anything but a regular file.
fn check_targets(evidence_dir: &Path, documents: &[(String, Vec<u8>)]) -> Result<(), XtaskError> {
    match fs::symlink_metadata(evidence_dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(XtaskError::Usage(format!(
                "evidence directory `{}` exists and is not a directory",
                evidence_dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return require_directory(parent_dir(evidence_dir), evidence_dir);
        }
        Err(error) => return Err(catalog::io_error(evidence_dir)(error)),
    }
    for (name, _document) in documents {
        let path = evidence_dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(XtaskError::OutputIsSymlink(path));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(XtaskError::UnsupportedFileType(path));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(catalog::io_error(&path)(error)),
        }
    }
    Ok(())
}

/// What the evidence documents are rendered from.
struct Inputs<'a> {
    runtime: &'a str,
    archive: &'a VerifiedArchive,
    archive_name: &'a str,
    lock_bytes: &'a [u8],
    matrix_bytes: &'a [u8],
    source: Source<'a>,
}

/// Renders the four evidence documents as `(file name, bytes)`.
fn render(inputs: &Inputs<'_>) -> Result<Vec<(String, Vec<u8>)>, XtaskError> {
    let runtime = inputs.runtime;
    let digest = inputs.archive.digest().as_str();
    let descriptor = inputs
        .archive
        .entries()
        .iter()
        .find(|entry| entry.path == DESCRIPTOR_PATH)
        .ok_or_else(|| refuse("package", Fault::Missing))?;
    let identity = descriptor_identity(&descriptor.contents)?;
    if identity.runtime_id.as_str() != runtime {
        return Err(refuse("runtime", Fault::Mismatch));
    }

    let lock_bytes = inputs.lock_bytes;
    let lock = parse_lock(lock_bytes)?;
    if lock.runtime != runtime {
        return Err(refuse(format!("lock.{runtime}.runtime"), Fault::Mismatch));
    }
    if lock.package != identity.package_id.as_str() {
        return Err(refuse(format!("lock.{runtime}.package"), Fault::Mismatch));
    }
    check_supported(&lock)?;
    let reference: LockReference = serde_json::from_slice(lock_bytes)
        .map_err(|_cause| refuse(format!("lock.{runtime}.upstream"), Fault::Malformed))?;
    let upstream = sorted(reference.upstream)
        .ok_or_else(|| refuse(format!("lock.{runtime}.upstream"), Fault::Malformed))?;

    let matrix = parse_matrix(inputs.matrix_bytes)?;
    let targets: Vec<&str> = matrix
        .rows
        .iter()
        .filter(|row| row.runtime == runtime)
        .map(|row| row.target.as_str())
        .collect();
    if targets.is_empty() {
        return Err(refuse("runtime", Fault::NotDeclared));
    }

    let entries: Vec<Entry<'_>> = inputs.archive.entries().iter().map(entry).collect();
    let manifest = Manifest {
        digest,
        entries: &entries,
        package_id: identity.package_id.as_str(),
        runtime_api: identity.runtime_api,
        runtime_id: runtime,
        schema: SCHEMA_VERSION,
        version: identity.version.as_str(),
    };
    let provenance = Provenance {
        builder: Builder {
            command: BUILDER_COMMAND,
            xtask_version: XTASK_VERSION,
        },
        materials: Materials {
            entries: &entries,
            upstream: &upstream,
        },
        schema: SCHEMA_VERSION,
        source: inputs.source,
        subject: Subject {
            digest,
            name: inputs.archive_name,
        },
    };
    let lock_digest = sha256_prefixed(lock_bytes);
    let vector = CompatVector {
        package_digest: digest,
        package_id: identity.package_id.as_str(),
        runtime,
        schema: SCHEMA_VERSION,
        suite_version: matrix.suite_version,
        supported: Supported {
            below: &lock.supported.below,
            min: &lock.supported.min,
        },
        targets,
        upstream: &upstream,
        upstream_lock_digest: &lock_digest,
        upstream_version: &lock.upstream.release,
    };
    let hex_digest = digest
        .strip_prefix(DIGEST_PREFIX)
        .ok_or_else(|| refuse("package_digest", Fault::Malformed))?;
    Ok(vec![
        (
            format!("{}{CHECKSUM_SUFFIX}", inputs.archive_name),
            format!("{hex_digest}  {}\n", inputs.archive_name).into_bytes(),
        ),
        (format!("{runtime}{MANIFEST_SUFFIX}"), pretty(&manifest)?),
        (
            format!("{runtime}{PROVENANCE_SUFFIX}"),
            pretty(&provenance)?,
        ),
        (format!("{runtime}{COMPAT_VECTOR_SUFFIX}"), pretty(&vector)?),
    ])
}

fn entry(entry: &ArchiveEntry) -> Entry<'_> {
    Entry {
        executable: entry.executable,
        path: &entry.path,
        sha256: hex(&Sha256::digest(&entry.contents)),
        size: entry.contents.len() as u64,
    }
}

/// `value` with the keys of every object in ascending order.
///
/// Objects are rebuilt in sorted insertion order, so the rendering is sorted
/// whether or not `serde_json` preserves insertion order. Returns `None` for a
/// fractional or exponent number, whose rendering is not canonical.
fn sorted(value: Value) -> Option<Value> {
    Some(match value {
        Value::Object(map) => {
            let entries: BTreeMap<String, Value> = map
                .into_iter()
                .map(|(key, value)| sorted(value).map(|value| (key, value)))
                .collect::<Option<_>>()?;
            Value::Object(entries.into_iter().collect::<Map<String, Value>>())
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(sorted)
                .collect::<Option<Vec<Value>>>()?,
        ),
        Value::Number(number) if number.is_f64() => return None,
        other => other,
    })
}

fn pretty<T: Serialize>(value: &T) -> Result<Vec<u8>, XtaskError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(XtaskError::Json)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Reads the lock bytes, refusing a missing lock as `lock.<runtime>` missing.
fn read_lock_bytes(path: &Path, runtime: &str) -> Result<Vec<u8>, XtaskError> {
    catalog::read_input(path, MAX_DOCUMENT_BYTES).map_err(|error| match error {
        XtaskError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            refuse(format!("lock.{runtime}"), Fault::Missing)
        }
        other => other,
    })
}

/// The archive file name, which the checksum line and the subject carry.
///
/// Limited to `[A-Za-z0-9._-]`, so the `sha256sum` line needs no escaping.
fn archive_name(output: &Path) -> Result<String, XtaskError> {
    let name = file_name(output)?;
    let safe = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
    if !safe {
        return Err(XtaskError::InvalidPath(output.to_path_buf()));
    }
    Ok(name)
}

fn file_name(path: &Path) -> Result<String, XtaskError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .ok_or_else(|| XtaskError::InvalidPath(path.to_path_buf()))
}

/// Fails when `evidence_dir` is a symlink or resolves to a place inside `dir`.
///
/// The directory may not exist yet: its nearest existing ancestor is resolved
/// and the missing components are appended, which must be plain names.
fn reject_evidence_inside(dir: &Path, evidence_dir: &Path) -> Result<(), XtaskError> {
    let root = fs::canonicalize(dir).map_err(catalog::io_error(dir))?;
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut existing = evidence_dir;
    loop {
        match fs::symlink_metadata(existing) {
            Ok(metadata) if metadata.file_type().is_symlink() && missing.is_empty() => {
                return Err(XtaskError::OutputIsSymlink(evidence_dir.to_path_buf()));
            }
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut components = existing.components();
                match components.next_back() {
                    Some(Component::Normal(name)) => missing.push(PathBuf::from(name)),
                    _ => return Err(XtaskError::InvalidPath(evidence_dir.to_path_buf())),
                }
                existing = components.as_path();
                if existing.as_os_str().is_empty() {
                    existing = Path::new(".");
                }
            }
            Err(error) => return Err(catalog::io_error(existing)(error)),
        }
    }
    let mut resolved = fs::canonicalize(existing).map_err(catalog::io_error(existing))?;
    for name in missing.iter().rev() {
        resolved.push(name);
    }
    if resolved.starts_with(&root) {
        return Err(XtaskError::Usage(format!(
            "evidence directory `{}` is inside the package directory",
            evidence_dir.display()
        )));
    }
    Ok(())
}

/// The canonical top level of the Git work tree holding `dir`.
fn toplevel(dir: &Path) -> Result<PathBuf, XtaskError> {
    let output = git(dir, &["rev-parse", "--show-toplevel"])?;
    let text =
        String::from_utf8(output).map_err(|_cause| XtaskError::InvalidPath(dir.to_path_buf()))?;
    let top = PathBuf::from(text.trim_end_matches('\n'));
    fs::canonicalize(&top).map_err(catalog::io_error(&top))
}

/// The runtime name of `package_dir`, which must be `<root>/runtime-packages/<name>`.
fn package_name(root: &Path, package_dir: &Path) -> Result<String, XtaskError> {
    let name = file_name(package_dir)?;
    if package_dir.parent() != Some(root.join(PACKAGES_DIR).as_path()) {
        return Err(XtaskError::Usage(format!(
            "package evidence needs a directory `{PACKAGES_DIR}/<runtime>` of the Git work tree `{}`, not `{}`",
            root.display(),
            package_dir.display()
        )));
    }
    Ok(name)
}

/// Requires `paths` (relative to `root`) to equal `HEAD`: no staged, unstaged,
/// untracked or ignored file below any of them.
fn require_clean(root: &Path, paths: &[&str]) -> Result<(), XtaskError> {
    let mut args = vec![
        "--literal-pathspecs",
        "--no-optional-locks",
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignored=matching",
        "--",
    ];
    args.extend_from_slice(paths);
    let status = git(root, &args)?;
    if !status.is_empty() {
        let listing: Vec<String> = status
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .collect();
        return Err(XtaskError::Usage(format!(
            "package evidence needs a clean tree: {} differ from HEAD ({})",
            paths.join(", "),
            listing.join("; ")
        )));
    }
    Ok(())
}

/// Requires the object `id` to be a tree: `HEAD:<path>` names a blob for a file.
fn require_tree(root: &Path, id: &str) -> Result<(), XtaskError> {
    let kind = git(root, &["cat-file", "-t", id])?;
    if kind != b"tree\n" {
        return Err(refuse("git tree", Fault::Mismatch));
    }
    Ok(())
}

/// Runs git with `args` in `dir` and returns stdout, failing on a non-zero exit.
///
/// Drops the variables that would point Git at another repository, index or
/// object store, the same set `cargo xtask affected` drops, and ignores
/// replace refs, which would otherwise change what `<commit>:<path>` reads.
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, XtaskError> {
    let mut command = Command::new(GIT);
    command
        .current_dir(dir)
        .arg("--no-replace-objects")
        .args(args);
    for var in REPOSITORY_REDIRECTING_VARS {
        command.env_remove(var);
    }
    let output = command.output().map_err(|source| XtaskError::Io {
        path: PathBuf::from(GIT),
        source,
    })?;
    if !output.status.success() {
        return Err(XtaskError::Usage(format!(
            "package evidence: `{GIT} {}` failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// The object format of a repository, which fixes the object id function.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => SHA1_HEX_LEN,
            Self::Sha256 => SHA256_HEX_LEN,
        }
    }

    /// Whether `text` is a full lowercase hex object id of this format.
    fn is_id(self, text: &str) -> bool {
        text.len() == self.hex_len()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    /// The Git blob id of `bytes`: the hash of `blob <len>\0` and the bytes.
    fn blob_id(self, bytes: &[u8]) -> String {
        let header = format!("blob {}\0", bytes.len());
        match self {
            Self::Sha1 => hex(&Sha1::new()
                .chain_update(header)
                .chain_update(bytes)
                .finalize()),
            Self::Sha256 => hex(&Sha256::new()
                .chain_update(header)
                .chain_update(bytes)
                .finalize()),
        }
    }
}

/// The object format of the repository at `root`.
fn object_format(root: &Path) -> Result<ObjectFormat, XtaskError> {
    match git(root, &["rev-parse", "--show-object-format"])?.as_slice() {
        b"sha1\n" => Ok(ObjectFormat::Sha1),
        b"sha256\n" => Ok(ObjectFormat::Sha256),
        _ => Err(refuse("git object format", Fault::Malformed)),
    }
}

/// Resolves `revision` in `root` to a full object id.
fn rev_parse(root: &Path, format: ObjectFormat, revision: &str) -> Result<String, XtaskError> {
    let output = git(root, &["rev-parse", "--verify", revision])?;
    let id = String::from_utf8_lossy(&output).trim_end().to_owned();
    if !format.is_id(&id) {
        return Err(refuse(format!("git `{revision}`"), Fault::Malformed));
    }
    Ok(id)
}

/// Refusal of a package or input file whose bytes differ from `HEAD`.
fn differs(path: &str, what: &str) -> XtaskError {
    XtaskError::Usage(format!(
        "package evidence: `{path}` differs from HEAD ({what})"
    ))
}

/// Requires the archive entries to be exactly the blobs of `tree`.
///
/// Same path set, mode `100755` exactly for executable entries and `100644`
/// otherwise, and each entry's bytes hashing to the blob id `tree` lists.
fn require_archive_is_tree(
    root: &Path,
    format: ObjectFormat,
    tree: &str,
    source_path: &str,
    archive: &VerifiedArchive,
) -> Result<(), XtaskError> {
    let listing = git(root, &["ls-tree", "-r", "-z", "--full-tree", tree])?;
    let mut head: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    for record in listing
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let text = std::str::from_utf8(record)
            .map_err(|_cause| refuse("git tree listing", Fault::Malformed))?;
        let (meta, path) = text
            .split_once('\t')
            .ok_or_else(|| refuse("git tree listing", Fault::Malformed))?;
        let mut fields = meta.split(' ');
        let (Some(mode), Some(kind), Some(id), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(refuse("git tree listing", Fault::Malformed));
        };
        head.insert(
            path.to_owned(),
            (mode.to_owned(), kind.to_owned(), id.to_owned()),
        );
    }
    for entry in archive.entries() {
        let path = format!("{source_path}/{}", entry.path);
        let Some((mode, kind, id)) = head.remove(&entry.path) else {
            return Err(differs(&path, "absent from HEAD"));
        };
        if kind != "blob" {
            return Err(differs(&path, "not a file in HEAD"));
        }
        let expected = if entry.executable {
            MODE_EXECUTABLE
        } else {
            MODE_REGULAR
        };
        if mode != expected {
            return Err(differs(&path, "file mode"));
        }
        if id != format.blob_id(&entry.contents) {
            return Err(differs(&path, "content"));
        }
    }
    if let Some(path) = head.keys().next() {
        return Err(differs(
            &format!("{source_path}/{path}"),
            "absent from the work tree",
        ));
    }
    Ok(())
}

/// Requires `bytes` to be the blob `commit` stores at `path`.
fn require_commit_blob(
    root: &Path,
    format: ObjectFormat,
    commit: &str,
    path: &str,
    bytes: &[u8],
) -> Result<(), XtaskError> {
    let id = rev_parse(root, format, &format!("{commit}:{path}"))?;
    if id != format.blob_id(bytes) {
        return Err(differs(path, "content"));
    }
    Ok(())
}
