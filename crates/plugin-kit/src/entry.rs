//! One entry file: the signed fields and the admission metadata.
//!
//! `[source]` is admission metadata that is validated here and never copied
//! into the signed document, so no pull-request-controlled string reaches a
//! shell or an API path unchecked. The same single-file reader also lives in
//! `xtask::plugin_index::build` with the index-host lane (the tree reader
//! `read_tree` stays there); the kit only reads the single entry file a
//! `prove` invocation names.

use std::path::Path;

use package::{IndexEntry, IndexEntryKind, PackageDigest};
use protocol::{PackageId, PackageVersion, RuntimeId};
use serde::Deserialize;

use crate::error::{refuse, Error};
use crate::files::read_input;

/// Extension of an entry file.
const ENTRY_EXTENSION: &str = "toml";

/// Largest accepted entry file. An entry is under 1 KiB; 16 KiB leaves room
/// for comments without admitting an arbitrary file into the parser.
const MAX_ENTRY_FILE_BYTES: u64 = 16 * 1024;

/// Length of a git commit id in lowercase hex characters.
const COMMIT_HEX_LEN: usize = 40;

/// One entry file: the signed fields and the admission metadata.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryFile {
    /// Package the entry authorizes.
    pub package_id: PackageId,
    /// `runtime` (the default) or `context`.
    #[serde(default)]
    pub kind: Option<IndexEntryKind>,
    /// The runtime a `runtime` entry provides; a `context` entry has none.
    #[serde(default)]
    pub runtime_id: Option<RuntimeId>,
    /// Package version the entry authorizes.
    pub version: PackageVersion,
    /// Digest of the package archive bytes.
    pub digest: PackageDigest,
    /// Where the archive is downloaded from.
    pub url: String,
    /// Platforms the package supports.
    pub platforms: Vec<String>,
    /// Semver range of core versions the package supports.
    pub core: String,
    /// Where the entry's package was built from; checked, not signed.
    pub source: Source,
}

/// Where an entry's package was built from; checked at admission, not signed.
///
/// The admission metadata of a real index entry names the repository and the
/// commit the package was built from. A kit `prove --dir` synthesizes its
/// entry from the directory, so there is no admission metadata to check;
/// [`Source::prove_placeholder`] names the proof itself instead.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// `owner/name` of the repository the package was built from.
    pub repo: String,
    /// GitHub repository id, never 0.
    pub repo_id: u64,
    /// `v` plus the entry version.
    pub tag: String,
    /// 40 lowercase hex characters.
    pub commit: String,
    /// Release tag `vX.Y.Z` the package was built with.
    pub built_with: String,
}

impl Source {
    /// The admission metadata of a `prove --dir` entry: the proof builds the
    /// archive from the directory, so no repository or commit is named and
    /// the tag names the version under proof. Passes [`Source::validate`]
    /// by construction.
    #[must_use]
    pub fn prove_placeholder(version: &PackageVersion) -> Self {
        Self {
            repo: "local/prove".to_owned(),
            repo_id: 1,
            tag: format!("v{}", version.as_str()),
            commit: "0".repeat(COMMIT_HEX_LEN),
            built_with: "v0.0.0".to_owned(),
        }
    }

    /// The `key=value` lines `--print-source` prints.
    #[must_use]
    pub fn lines(&self) -> String {
        format!(
            "repo={}\nrepo_id={}\ntag={}\ncommit={}\nbuilt_with={}\n",
            self.repo, self.repo_id, self.tag, self.commit, self.built_with
        )
    }

    /// Checks the fields against the entry they belong to.
    ///
    /// `repo` is `owner/name` over the characters GitHub allows, `repo_id` is
    /// a GitHub repository id (never 0), `tag` is `v` plus the entry version,
    /// `commit` is 40 lowercase hex digits and `built_with` is a release tag
    /// `vX.Y.Z`. A commit sha in `built_with` is refused: it could name a
    /// commit of a fork network instead of a release of this repository.
    pub fn validate(&self, version: &PackageVersion) -> Result<(), Error> {
        let refuse = |reason: &str| refuse(reason.to_owned());
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
                "[source] repo must be `owner/name` of letters, digits, `-`, `.` and `_`",
            ));
        }
        if self.repo_id == 0 {
            return Err(refuse("[source] repo_id must be greater than 0"));
        }
        if self.tag != format!("v{}", version.as_str()) {
            return Err(refuse(
                "[source] tag must be `v` followed by the entry version",
            ));
        }
        if self.commit.len() != COMMIT_HEX_LEN
            || !self
                .commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(refuse(
                "[source] commit must be 40 lowercase hex characters",
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
            return Err(refuse("[source] built_with must be a release tag `vX.Y.Z`"));
        }
        Ok(())
    }
}

/// Whether `text` is one or more characters from `allowed`.
fn all_of(text: &str, allowed: impl Fn(char) -> bool) -> bool {
    !text.is_empty() && text.chars().all(allowed)
}

impl EntryFile {
    /// What the entry authorizes; `runtime` when the file names no kind.
    #[must_use]
    pub fn kind(&self) -> IndexEntryKind {
        self.kind.unwrap_or(IndexEntryKind::Runtime)
    }

    /// The signed entry of this file in a document of `schema_version`;
    /// `[source]` stays behind. A schema 1 entry has no `kind`.
    ///
    /// # Errors
    ///
    /// Returns a usage error for a context entry in a schema 1 document, which
    /// cannot express it.
    pub fn signed_entry(&self, schema_version: u64) -> Result<IndexEntry, Error> {
        let kind = self.kind();
        if schema_version == package::INDEX_SCHEMA_VERSION && kind == IndexEntryKind::Context {
            return Err(refuse(format!(
                "`{}` is a context entry, which a schema {} index cannot list; build with --schema-version {}",
                self.package_id,
                package::INDEX_SCHEMA_VERSION,
                package::INDEX_SCHEMA_VERSION_2
            )));
        }
        Ok(IndexEntry {
            package_id: self.package_id.clone(),
            kind: (schema_version == package::INDEX_SCHEMA_VERSION_2).then_some(kind),
            runtime_id: self.runtime_id.clone(),
            version: self.version.clone(),
            digest: self.digest.clone(),
            url: self.url.clone(),
            platforms: self.platforms.clone(),
            core: self.core.clone(),
        })
    }
}

/// Parses and validates one entry file: TOML shape, `[source]` admission
/// metadata, and the kind/`runtime_id` pairing.
pub fn read_entry_file(path: &Path) -> Result<EntryFile, Error> {
    let bytes = read_input(path, MAX_ENTRY_FILE_BYTES)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_cause| refuse(format!("`{}` is not UTF-8", path.display())))?;
    if !path.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .ends_with(&format!(".{ENTRY_EXTENSION}"))
    }) {
        return Err(refuse(format!(
            "`{}` is not a `.{ENTRY_EXTENSION}` entry file",
            path.display()
        )));
    }
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
    match (entry.kind(), &entry.runtime_id) {
        (IndexEntryKind::Runtime, Some(_)) | (IndexEntryKind::Context, None) => {}
        (IndexEntryKind::Runtime, None) => {
            return Err(refuse(format!(
                "`{}`: a runtime entry needs a runtime_id",
                path.display()
            )));
        }
        (IndexEntryKind::Context, _) => {
            return Err(refuse(format!(
                "`{}`: a context entry has no runtime_id",
                path.display()
            )));
        }
    }
    Ok(entry)
}

/// The signed entry of the single entry file `path` in a schema 2 document,
/// checked like `index build` checks an entry of a tree.
pub fn entry_of(path: &Path) -> Result<IndexEntry, Error> {
    let entry = read_entry_file(path)?;
    entry.signed_entry(package::INDEX_SCHEMA_VERSION_2)
}
