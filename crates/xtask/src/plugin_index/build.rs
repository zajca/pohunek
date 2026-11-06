//! `index build`: the unsigned plugin index of an entries tree.
//!
//! The tree is `<dir>/official/<package_id>/<version>.toml` and nothing else.
//! Every entry file is parsed with `deny_unknown_fields`; its `[source]` table
//! is admission metadata that is validated here and never copied into the
//! signed document, so no pull-request-controlled string reaches a shell or an
//! API path unchecked.

// Rust guideline compliant 2026-11-06

use std::fs;
use std::os::unix::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};

use package::{
    check_index_body, parse_index_envelope, IndexBody, IndexEntry, IndexEnvelope, IndexKeyRecord,
    PackageDigest, SignedRevocations, INDEX_SCHEMA_VERSION, MAX_INDEX_BYTES, MAX_INDEX_KEY_RECORDS,
    OFFICIAL_SCOPE,
};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde::Deserialize;

use super::{document_bytes, write_new_document, MAX_REVOCATIONS_FILE_BYTES};
use crate::catalog::{io_error, read_input};
use crate::XtaskError;

/// Directory of the entries tree that holds the official entries.
const OFFICIAL_DIR: &str = "official";

/// Placeholder file the index repository keeps in an empty `official/`; it must
/// be empty.
const KEEP_FILE: &str = ".gitkeep";

/// Extension of an entry file.
const ENTRY_EXTENSION: &str = "toml";

/// Largest accepted entry file. An entry is under 1 KiB; 16 KiB leaves room
/// for comments without admitting an arbitrary file into the parser.
const MAX_ENTRY_FILE_BYTES: u64 = 16 * 1024;

/// Largest accepted `--key-record` file. A record is about 700 bytes.
const MAX_KEY_RECORD_FILE_BYTES: u64 = 16 * 1024;

/// Length of a git commit id in lowercase hex.
const COMMIT_HEX_LEN: usize = 40;

/// One entry file: the signed fields and the admission metadata.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryFile {
    package_id: PackageId,
    runtime_id: RuntimeId,
    version: PackageVersion,
    digest: PackageDigest,
    url: String,
    platforms: Vec<String>,
    core: String,
    source: Source,
}

/// Where an entry's package was built from; checked at admission, not signed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    repo: String,
    repo_id: u64,
    tag: String,
    commit: String,
    built_with: String,
}

/// The inputs of `index build`.
pub(crate) struct BuildArgs {
    pub(crate) entries: PathBuf,
    pub(crate) revocations: PathBuf,
    pub(crate) key_records: Vec<PathBuf>,
    pub(crate) sequence: u64,
    pub(crate) expires_at: u64,
    pub(crate) output: PathBuf,
}

fn refuse(reason: String) -> XtaskError {
    XtaskError::Usage(reason)
}

/// Whether `text` is one or more characters from `allowed`.
fn all_of(text: &str, allowed: impl Fn(char) -> bool) -> bool {
    !text.is_empty() && text.chars().all(allowed)
}

impl Source {
    /// Checks the fields against the entry they belong to.
    ///
    /// `repo` is `owner/name` over the characters GitHub allows, `repo_id` is
    /// a GitHub repository id (never 0), `tag` is `v` plus the entry version,
    /// `commit` is 40 lowercase hex digits and `built_with` is a release tag
    /// `vX.Y.Z`. A commit sha in `built_with` is refused: it could name a
    /// commit of a fork network instead of a release of this repository.
    fn validate(&self, version: &PackageVersion) -> Result<(), XtaskError> {
        let repo_ok = self.repo.split_once('/').is_some_and(|(owner, name)| {
            all_of(owner, |c| c.is_ascii_alphanumeric() || c == '-')
                // An owner never starts with `-`, which a command line would read as an option.
                && !owner.starts_with('-')
                && all_of(name, |c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                // `.` and `..` match the character class but name no
                // repository and would change an API path.
                && name != "."
                && name != ".."
        });
        if !repo_ok {
            return Err(refuse(
                "[source] repo must be `owner/name` of letters, digits, `-`, `.` and `_`".into(),
            ));
        }
        if self.repo_id == 0 {
            return Err(refuse("[source] repo_id must be greater than 0".into()));
        }
        if self.tag != format!("v{}", version.as_str()) {
            return Err(refuse(
                "[source] tag must be `v` followed by the entry version".into(),
            ));
        }
        if self.commit.len() != COMMIT_HEX_LEN
            || !self
                .commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(refuse(
                "[source] commit must be 40 lowercase hex characters".into(),
            ));
        }
        let built_with_ok = self.built_with.strip_prefix('v').is_some_and(|numbers| {
            let parts: Vec<&str> = numbers.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|part| all_of(part, |c| c.is_ascii_digit()))
        });
        if !built_with_ok {
            return Err(refuse(
                "[source] built_with must be a release tag `vX.Y.Z`".into(),
            ));
        }
        Ok(())
    }

    /// The `key=value` lines `--print-source` prints.
    fn lines(&self) -> String {
        format!(
            "repo={}\nrepo_id={}\ntag={}\ncommit={}\nbuilt_with={}\n",
            self.repo, self.repo_id, self.tag, self.commit, self.built_with
        )
    }
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
fn read_tree(root: &Path) -> Result<Vec<EntryFile>, XtaskError> {
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
            let entry = read_entry(&path)?;
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

/// Parses and validates one entry file.
fn read_entry(path: &Path) -> Result<EntryFile, XtaskError> {
    let bytes = read_input(path, MAX_ENTRY_FILE_BYTES)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_cause| refuse(format!("`{}` is not UTF-8", path.display())))?;
    let entry: EntryFile = toml::from_str(text).map_err(|error| {
        refuse(format!(
            "`{}` is not a valid entry file: {}",
            path.display(),
            error.message()
        ))
    })?;
    entry
        .source
        .validate(&entry.version)
        .map_err(|error| refuse(format!("`{}`: {error}", path.display())))?;
    Ok(entry)
}

/// The signed entry of an entry file; `[source]` stays behind.
fn index_entry(entry: EntryFile) -> IndexEntry {
    IndexEntry {
        package_id: entry.package_id,
        runtime_id: entry.runtime_id,
        version: entry.version,
        digest: entry.digest,
        url: entry.url,
        platforms: entry.platforms,
        core: entry.core,
    }
}

/// The signed entry of the single entry file `path`, checked like `index
/// build` checks an entry of a tree.
pub(crate) fn entry_of(path: &Path) -> Result<IndexEntry, XtaskError> {
    read_entry(path).map(index_entry)
}

/// The signed entries of the tree at `root`, in `(package_id, version)` order,
/// each checked like `index build` checks it.
pub(super) fn entries_of(root: &Path) -> Result<Vec<IndexEntry>, XtaskError> {
    Ok(read_tree(root)?.into_iter().map(index_entry).collect())
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
    let entries = entries_of(&args.entries)?;

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
        schema_version: INDEX_SCHEMA_VERSION,
        index: IndexBody {
            schema_version: INDEX_SCHEMA_VERSION,
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
