//! `index check-entry`: the pure admission predicates of one added index entry.
//!
//! The index repository's admission workflow runs this command from the xtask
//! built at the pinned core tag, so the refused runtime ids, the package-id
//! binding and the core-range parser are the ones the daemon applies. Stage
//! `schema` covers step 1 of the admission (the entry builds like any entry of
//! a tree, binds its package id to its runtime id, claims no refused runtime id
//! or foreign identity, and is a new release version above every admitted one);
//! stage `core-range` covers step 7. A refusal names its predicate as
//! `predicate=<name>`, so the workflow attributes the failure to one step. On
//! success the `schema` stage prints the validated entry fields as `key=value`
//! lines for `IFS== read -r`, never for `eval`: a shell never parses the TOML,
//! and the core range never leaves this process.

// Rust guideline compliant 2026-11-06

use std::path::{Path, PathBuf};

use package::{
    check_index_body, parse_core_range, IndexBody, IndexEntry, INDEX_REFUSED_RUNTIME_IDS,
    INDEX_SCHEMA_VERSION, MAX_INDEX_BYTES, MAX_INDEX_ENTRIES, OFFICIAL_RUNTIME_PACKAGE_PREFIX,
    OFFICIAL_SCOPE,
};
use semver::{Comparator, Op, Version};

use super::build::{index_entry, read_tree, EntryFile};
use crate::catalog::unix_now;
use crate::XtaskError;

/// First core release that carries the index verifier. An entry's core range
/// must not admit anything below it.
const FIRST_INDEX_CORE: Version = Version::new(0, 40, 0);

/// Seconds the probe document used for the structural check stays valid. Any
/// positive value works: the document is never written.
const PROBE_LIFETIME_SECONDS: u64 = 60;

/// Admission stage of `index check-entry`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CheckStage {
    /// Step 1: schema, identity and version predicates.
    Schema,
    /// Step 7: the entry's core range.
    CoreRange,
}

/// The inputs of `index check-entry`.
pub(crate) struct CheckArgs {
    pub(crate) entries: PathBuf,
    pub(crate) existing: PathBuf,
    pub(crate) core_version: String,
    pub(crate) stage: CheckStage,
}

/// The name a refusal of `index check-entry` carries as `predicate=<name>`.
/// The index repository's admission workflow and the docs rely on these
/// names, so a rename is a contract change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Predicate {
    /// Refused when the entry does not build, or the tree or the merged index fails the daemon's structural rules.
    Schema,
    /// Refused when the admitted tree does not read, or holds a version that is not semver.
    ExistingTree,
    /// Refused when the package id is not `pohunek.runtime.` plus the runtime id.
    PackageIdBinding,
    /// Refused when the runtime id is one the daemon refuses.
    RefusedRuntimeId,
    /// Refused when the url holds a character outside letters, digits and `:/._+-`.
    UrlCharset,
    /// Refused when the package id or the runtime id is already bound to another one.
    IdentityConflict,
    /// Refused when the digest is already admitted.
    DuplicateDigest,
    /// Refused when the version is not semver.
    VersionNotSemver,
    /// Refused when the version carries a pre-release or build part.
    VersionNotRelease,
    /// Refused when the version is already admitted for the package id.
    DuplicateVersion,
    /// Refused when the version is not above an admitted version of the package id.
    VersionNotGreater,
    /// Refused when the core range does not parse.
    CoreRangeUnparsed,
    /// Refused when the core range names a pre-release.
    CorePrerelease,
    /// Refused when the core range does not admit the pinned core.
    CoreExcludesCoreTag,
    /// Refused when the core range admits a core below the first index core.
    CoreLowerBound,
}

impl std::fmt::Display for Predicate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Schema => "schema",
            Self::ExistingTree => "existing-tree",
            Self::PackageIdBinding => "package-id-binding",
            Self::RefusedRuntimeId => "refused-runtime-id",
            Self::UrlCharset => "url-charset",
            Self::IdentityConflict => "identity-conflict",
            Self::DuplicateDigest => "duplicate-digest",
            Self::VersionNotSemver => "version-not-semver",
            Self::VersionNotRelease => "version-not-release",
            Self::DuplicateVersion => "duplicate-version",
            Self::VersionNotGreater => "version-not-greater",
            Self::CoreRangeUnparsed => "core-range-unparsed",
            Self::CorePrerelease => "core-prerelease",
            Self::CoreExcludesCoreTag => "core-excludes-core-tag",
            Self::CoreLowerBound => "core-lower-bound",
        })
    }
}

/// A refusal of `predicate`. The detail can quote names and keys of the pull
/// request, so control characters are escaped: a line of the log never starts
/// with text the pull request chose.
fn refuse(predicate: Predicate, detail: impl std::fmt::Display) -> XtaskError {
    let detail = detail.to_string();
    XtaskError::Admission(format!("predicate={predicate}: {}", detail.escape_debug()))
}

/// Parses `text` as a release version: no pre-release, no build metadata.
fn release_version(text: &str) -> Option<Version> {
    Version::parse(text)
        .ok()
        .filter(|version| version.pre.is_empty() && version.build.is_empty())
}

/// Whether `byte` may appear in a printed `url`: the characters of a release
/// asset URL, none of which a shell treats specially.
fn is_url_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'/' | b'.' | b'_' | b'+' | b'-')
}

/// The single entry of the tree `root`.
fn only_entry(root: &Path) -> Result<EntryFile, XtaskError> {
    let mut tree = read_tree(root).map_err(|error| refuse(Predicate::Schema, error))?;
    match (tree.pop(), tree.is_empty()) {
        (Some(entry), true) => Ok(entry),
        _ => Err(refuse(
            Predicate::Schema,
            "--entries must hold exactly one entry",
        )),
    }
}

/// Checks `index check-entry` and returns the lines to print.
pub(crate) fn check_entry(args: &CheckArgs) -> Result<String, XtaskError> {
    let core_version = release_version(&args.core_version).ok_or_else(|| {
        XtaskError::Usage("--core-version must be a release version `X.Y.Z`".into())
    })?;
    let added = only_entry(&args.entries)?;
    match args.stage {
        CheckStage::Schema => schema(&added, &args.existing),
        CheckStage::CoreRange => core_range(&added, &core_version),
    }
}

fn schema(added: &EntryFile, existing_root: &Path) -> Result<String, XtaskError> {
    let existing =
        read_tree(existing_root).map_err(|error| refuse(Predicate::ExistingTree, error))?;

    let entry = IndexEntry {
        package_id: added.package_id.clone(),
        runtime_id: added.runtime_id.clone(),
        version: added.version.clone(),
        digest: added.digest.clone(),
        url: added.url.clone(),
        platforms: added.platforms.clone(),
        core: added.core.clone(),
    };
    probe(std::slice::from_ref(&entry)).map_err(|error| refuse(Predicate::Schema, error))?;

    let expected_package = format!("{OFFICIAL_RUNTIME_PACKAGE_PREFIX}{}", added.runtime_id);
    if added.package_id.as_str() != expected_package {
        return Err(refuse(
            Predicate::PackageIdBinding,
            format!(
                "package_id `{}` must be `{expected_package}`",
                added.package_id
            ),
        ));
    }
    if INDEX_REFUSED_RUNTIME_IDS.contains(&added.runtime_id.as_str()) {
        return Err(refuse(
            Predicate::RefusedRuntimeId,
            format!("runtime id `{}` cannot be claimed", added.runtime_id),
        ));
    }
    if !added.url.bytes().all(is_url_safe) {
        return Err(refuse(
            Predicate::UrlCharset,
            "url may hold only letters, digits and `:/._+-`",
        ));
    }
    for other in &existing {
        let same_package = other.package_id == added.package_id;
        let same_runtime = other.runtime_id == added.runtime_id;
        if same_package != same_runtime {
            return Err(refuse(
                Predicate::IdentityConflict,
                format!(
                    "`{}` and runtime id `{}` are already bound to `{}` and `{}`",
                    added.package_id, added.runtime_id, other.package_id, other.runtime_id
                ),
            ));
        }
    }
    if existing.iter().any(|other| other.digest == added.digest) {
        return Err(refuse(
            Predicate::DuplicateDigest,
            "digest is already admitted",
        ));
    }

    version_order(added, &existing)?;

    let merged: Vec<IndexEntry> = existing
        .into_iter()
        .map(index_entry)
        .chain(std::iter::once(entry))
        .collect();
    probe(&merged).map_err(|error| refuse(Predicate::Schema, error))?;

    Ok(format!(
        "package_id={}\nruntime_id={}\nversion={}\ndigest={}\nurl={}\nplatforms={}\n",
        added.package_id,
        added.runtime_id,
        added.version,
        added.digest,
        added.url,
        added.platforms.join(",")
    ))
}

/// Requires the added version to be a release semver above every admitted
/// version of the same package id.
fn version_order(added: &EntryFile, existing: &[EntryFile]) -> Result<(), XtaskError> {
    let version = Version::parse(added.version.as_str()).map_err(|error| {
        refuse(
            Predicate::VersionNotSemver,
            format!("version `{}`: {error}", added.version),
        )
    })?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(refuse(
            Predicate::VersionNotRelease,
            format!(
                "version `{}` carries a pre-release or build part",
                added.version
            ),
        ));
    }
    for other in existing
        .iter()
        .filter(|other| other.package_id == added.package_id)
    {
        let admitted = Version::parse(other.version.as_str()).map_err(|error| {
            refuse(
                Predicate::ExistingTree,
                format!("admitted version `{}`: {error}", other.version),
            )
        })?;
        if admitted == version {
            return Err(refuse(
                Predicate::DuplicateVersion,
                format!("version `{version}` is already admitted"),
            ));
        }
        if admitted > version {
            return Err(refuse(
                Predicate::VersionNotGreater,
                format!("version `{version}` is not above admitted `{admitted}`"),
            ));
        }
    }
    Ok(())
}

/// Runs the daemon's structural entry rules over `entries`.
fn probe(entries: &[IndexEntry]) -> Result<(), XtaskError> {
    if entries.len() > MAX_INDEX_ENTRIES {
        return Err(XtaskError::Usage(format!(
            "the index would hold {} entries, above the limit of {MAX_INDEX_ENTRIES}",
            entries.len()
        )));
    }
    let now = unix_now()?;
    let body = IndexBody {
        schema_version: INDEX_SCHEMA_VERSION,
        scope: OFFICIAL_SCOPE.to_owned(),
        sequence: now,
        expires_at: now + PROBE_LIFETIME_SECONDS,
        entries: entries.to_vec(),
    };
    check_index_body(&body, now).map_err(XtaskError::Index)?;
    let bytes = serde_json::to_vec(&body).map_err(XtaskError::Json)?;
    if bytes.len() > MAX_INDEX_BYTES {
        return Err(XtaskError::Index(package::IndexError::TooLarge));
    }
    Ok(())
}

/// The smallest version `comparator` admits, or `None` when it has no lower
/// bound.
fn lower_bound(comparator: &Comparator) -> Option<Version> {
    let (minor, patch) = (comparator.minor.unwrap_or(0), comparator.patch.unwrap_or(0));
    match comparator.op {
        Op::Exact | Op::GreaterEq | Op::Tilde | Op::Caret | Op::Wildcard => {
            Some(Version::new(comparator.major, minor, patch))
        }
        Op::Greater => greater_bound(comparator),
        _ => None,
    }
}

/// The smallest version a `>` comparator admits: the next patch, minor or
/// major of the named part, carrying into the next part when the component is
/// `u64::MAX`. `None` when nothing is above the comparator.
fn greater_bound(comparator: &Comparator) -> Option<Version> {
    let major = comparator.major;
    let next_major = || major.checked_add(1).map(|next| Version::new(next, 0, 0));
    match (comparator.minor, comparator.patch) {
        (Some(minor), Some(patch)) => patch
            .checked_add(1)
            .map(|next| Version::new(major, minor, next))
            .or_else(|| {
                minor
                    .checked_add(1)
                    .map(|next| Version::new(major, next, 0))
            })
            .or_else(next_major),
        (Some(minor), None) => minor
            .checked_add(1)
            .map(|next| Version::new(major, next, 0))
            .or_else(next_major),
        (None, _) => next_major(),
    }
}

fn core_range(added: &EntryFile, core_version: &Version) -> Result<String, XtaskError> {
    let requirement = parse_core_range(&added.core).ok_or_else(|| {
        refuse(
            Predicate::CoreRangeUnparsed,
            "core must be a semver range with at least one comparator",
        )
    })?;
    if requirement
        .comparators
        .iter()
        .any(|comparator| !comparator.pre.is_empty())
    {
        return Err(refuse(
            Predicate::CorePrerelease,
            "a core range must not name a pre-release",
        ));
    }
    if !requirement.matches(core_version) {
        return Err(refuse(
            Predicate::CoreExcludesCoreTag,
            format!("core range does not admit core {core_version}"),
        ));
    }
    let lowest = requirement
        .comparators
        .iter()
        .filter_map(lower_bound)
        .max()
        .unwrap_or_else(|| Version::new(0, 0, 0));
    if lowest < FIRST_INDEX_CORE {
        return Err(refuse(
            Predicate::CoreLowerBound,
            format!("core range admits {lowest}, below the first index core {FIRST_INDEX_CORE}"),
        ));
    }
    Ok(format!("core range ok: lower_bound={lowest}\n"))
}
