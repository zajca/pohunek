//! `index build`: the unsigned plugin index of an entries tree.
//!
//! The tree is `<dir>/official/<package_id>/<version>.toml` and nothing else.
//! Every entry file is parsed with `deny_unknown_fields`; its `[source]` table
//! is admission metadata that is validated here and never copied into the
//! signed document, so no pull-request-controlled string reaches a shell or an
//! API path unchecked. The single-file reader lives in the kit crate
//! ([`plugin_kit::entry`]); this module keeps only the tree walk.

// Rust guideline compliant 2026-11-06

use std::fs;
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};

use package::{
    check_index_body, parse_index_envelope, IndexBody, IndexEntry, IndexEntryKind, IndexEnvelope,
    IndexKeyRecord, SignedRevocations, INDEX_SCHEMA_VERSION, INDEX_SCHEMA_VERSION_2,
    MAX_INDEX_BYTES, MAX_INDEX_KEY_RECORDS, OFFICIAL_SCOPE,
};
use plugin_kit::entry::EntryFile;

use super::{document_bytes, write_new_document, MAX_REVOCATIONS_FILE_BYTES};
use crate::catalog::{io_error, read_input};
use crate::XtaskError;

/// Directory of the entries tree that holds the official entries.
const OFFICIAL_DIR: &str = "official";

/// Placeholder file the index repository keeps in an empty `official/`; it must
/// be empty.
const KEEP_FILE: &str = ".gitkeep";

/// Extension of an entry file, as the kit reader requires it.
const ENTRY_EXTENSION: &str = "toml";

/// Largest accepted `--key-record` file. A record is about 700 bytes.
const MAX_KEY_RECORD_FILE_BYTES: u64 = 16 * 1024;

/// The inputs of `index build`.
pub(crate) struct BuildArgs {
    pub(crate) entries: PathBuf,
    pub(crate) revocations: PathBuf,
    pub(crate) key_records: Vec<PathBuf>,
    pub(crate) sequence: u64,
    pub(crate) expires_at: u64,
    pub(crate) output: PathBuf,
    /// Schema of the document: 1 lists runtime entries only, 2 also lists
    /// context entries.
    pub(crate) schema_version: u64,
}

fn refuse(reason: String) -> XtaskError {
    XtaskError::Usage(reason)
}

/// The shared entry reader error as an `XtaskError`: the kit and `xtask`
/// print the same refusal text.
fn kit_error(error: plugin_kit::Error) -> XtaskError {
    XtaskError::from(error)
}

/// The names of the directory `path`, sorted, each with its type.
fn read_dir_sorted(path: &Path) -> Result<Vec<(String, fs::FileType)>, XtaskError> {
    let mut found = Vec::new();
    for item in fs::read_dir(path).map_err(io_error(path))? {
        let item = item.map_err(io_error(path))?;
        let file_type = item.file_type().map_err(io_error(path))?;
        let name = item.file_name().into_string().map_err(|_name| {
            refuse(format!(
                "`{}` holds a name that is not UTF-8",
                path.display()
            ))
        })?;
        found.push((name, file_type));
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(found)
}

/// Describes a file type for a refusal message.
fn kind_of(file_type: fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        "a symbolic link"
    } else if file_type.is_dir() {
        "a directory"
    } else if file_type.is_file() {
        "a file"
    } else if file_type.is_fifo() || file_type.is_socket() {
        "a special file"
    } else {
        "an unsupported file type"
    }
}

/// Reads the entries tree: exactly `official/<package_id>/<version>.toml`.
///
/// Any other file, directory or depth, any symbolic link, a directory name
/// that differs from the entry's `package_id` and a file stem that differs
/// from its `version` is an error. The result is in `(package_id, version)`
/// order.
pub(super) fn read_tree(root: &Path) -> Result<Vec<EntryFile>, XtaskError> {
    let mut entries = Vec::new();
    let top = read_dir_sorted(root)?;
    for (name, file_type) in &top {
        if name != OFFICIAL_DIR || !file_type.is_dir() {
            return Err(refuse(format!(
                "`{}` holds `{name}`, which is {}; only the `{OFFICIAL_DIR}` directory is allowed",
                root.display(),
                kind_of(*file_type)
            )));
        }
    }
    if top.is_empty() {
        return Err(refuse(format!(
            "`{}` holds no `{OFFICIAL_DIR}` directory",
            root.display()
        )));
    }
    let official = root.join(OFFICIAL_DIR);
    for (package_dir, package_type) in read_dir_sorted(&official)? {
        let package_path = official.join(&package_dir);
        if package_dir == KEEP_FILE && package_type.is_file() {
            let keep = fs::metadata(&package_path).map_err(io_error(&package_path))?;
            if keep.len() != 0 {
                return Err(refuse(format!(
                    "`{}` must be empty",
                    package_path.display()
                )));
            }
            continue;
        }
        if !package_type.is_dir() {
            return Err(refuse(format!(
                "`{}` is {}; `{OFFICIAL_DIR}` holds only package directories",
                package_path.display(),
                kind_of(package_type)
            )));
        }
        for (file_name, file_type) in read_dir_sorted(&package_path)? {
            let path = package_path.join(&file_name);
            let stem = file_name
                .strip_suffix(&format!(".{ENTRY_EXTENSION}"))
                .filter(|_stem| file_type.is_file())
                .ok_or_else(|| {
                    refuse(format!(
                        "`{}` is {}; a package directory holds only `<version>.{ENTRY_EXTENSION}` files",
                        path.display(),
                        kind_of(file_type)
                    ))
                })?;
            // The single-file reader is the kit's: one reader parses and
            // validates an entry file for every caller.
            let entry = plugin_kit::entry::read_entry_file(&path).map_err(kit_error)?;
            if entry.package_id.as_str() != package_dir {
                return Err(refuse(format!(
                    "`{}` declares package_id `{}`, but its directory is `{package_dir}`",
                    path.display(),
                    entry.package_id
                )));
            }
            if entry.version.as_str() != stem {
                return Err(refuse(format!(
                    "`{}` declares version `{}`, but its file stem is `{stem}`",
                    path.display(),
                    entry.version
                )));
            }
            entries.push(entry);
        }
    }
    entries.sort_by(|left, right| {
        (&left.package_id, &left.version).cmp(&(&right.package_id, &right.version))
    });
    Ok(entries)
}

/// The signed entries of the tree at `root`, in `(package_id, version)` order,
/// each checked like `index build` checks it, as entries of a document of
/// `schema_version`. A schema 1 document lists the runtime entries only: it
/// cannot express a context entry, so the tree's context entries stay out.
/// The signed form comes from the kit's reader.
pub(super) fn entries_of(root: &Path, schema_version: u64) -> Result<Vec<IndexEntry>, XtaskError> {
    read_tree(root)?
        .into_iter()
        .filter(|entry| {
            schema_version != INDEX_SCHEMA_VERSION || entry.kind() == IndexEntryKind::Runtime
        })
        .map(|entry| entry.signed_entry(schema_version).map_err(kit_error))
        .collect()
}

/// Builds the unsigned index document and writes it to `args.output`.
///
/// The envelope carries `signatures: []`. It is checked with the package
/// crate's key-free parser and body rules (the daemon's own) before the
/// output file is created, so a refusal writes nothing. Returns the number of
/// entries.
pub(crate) fn build(args: &BuildArgs, now: u64) -> Result<usize, XtaskError> {
    if args.key_records.is_empty() || args.key_records.len() > MAX_INDEX_KEY_RECORDS {
        return Err(refuse(format!(
            "--key-record must be given 1 to {MAX_INDEX_KEY_RECORDS} times"
        )));
    }
    if !package::SUPPORTED_INDEX_SCHEMA_VERSIONS.contains(&args.schema_version) {
        return Err(refuse(format!(
            "--schema-version must be {INDEX_SCHEMA_VERSION} or {INDEX_SCHEMA_VERSION_2}"
        )));
    }
    let entries = entries_of(&args.entries, args.schema_version)?;

    let mut key_records = Vec::with_capacity(args.key_records.len());
    for path in &args.key_records {
        let bytes = read_input(path, MAX_KEY_RECORD_FILE_BYTES)?;
        let record: IndexKeyRecord = serde_json::from_slice(&bytes)
            .map_err(|_cause| refuse(format!("`{}` is not an index key record", path.display())))?;
        key_records.push(record);
    }
    let revocation_bytes = read_input(&args.revocations, MAX_REVOCATIONS_FILE_BYTES)?;
    let revocations: SignedRevocations =
        serde_json::from_slice(&revocation_bytes).map_err(|_cause| {
            refuse(format!(
                "`{}` is not a revocation document",
                args.revocations.display()
            ))
        })?;

    let envelope = IndexEnvelope {
        schema_version: args.schema_version,
        index: IndexBody {
            schema_version: args.schema_version,
            scope: OFFICIAL_SCOPE.to_owned(),
            sequence: args.sequence,
            expires_at: args.expires_at,
            entries,
        },
        key_records,
        signatures: Vec::new(),
        revocations,
    };
    check_index_body(&envelope.index, now).map_err(XtaskError::Index)?;
    let bytes = document_bytes(&envelope)?;
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(XtaskError::Index(package::IndexError::TooLarge));
    }
    parse_index_envelope(&bytes).map_err(XtaskError::Index)?;
    write_new_document(&args.output, &bytes)?;
    Ok(envelope.index.entries.len())
}

/// The validated `[source]` of the tree's only entry, as `key=value` lines.
///
/// A tree with zero or several entries is refused: admission checks one entry
/// per call.
pub(crate) fn print_source(entries: &Path) -> Result<String, XtaskError> {
    let mut tree = read_tree(entries)?;
    match (tree.pop(), tree.is_empty()) {
        (Some(entry), true) => Ok(entry.source.lines()),
        _ => Err(refuse(
            "--print-source needs a tree that holds exactly one entry".into(),
        )),
    }
}
