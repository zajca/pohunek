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
    INDEX_SCHEMA_VERSION, OFFICIAL_RUNTIME_PACKAGE_PREFIX, OFFICIAL_SCOPE,
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

fn refuse(predicate: &str, detail: impl std::fmt::Display) -> XtaskError {
    XtaskError::Admission(format!("predicate={predicate}: {detail}"))
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
    let mut tree = read_tree(root).map_err(|error| refuse("schema", error))?;
    match (tree.pop(), tree.is_empty()) {
        (Some(entry), true) => Ok(entry),
        _ => Err(refuse("schema", "--entries must hold exactly one entry")),
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
    let existing = read_tree(existing_root).map_err(|error| refuse("existing-tree", error))?;

    let entry = IndexEntry {
        package_id: added.package_id.clone(),
        runtime_id: added.runtime_id.clone(),
        version: added.version.clone(),
        digest: added.digest.clone(),
        url: added.url.clone(),
        platforms: added.platforms.clone(),
        core: added.core.clone(),
    };
    probe(std::slice::from_ref(&entry)).map_err(|error| refuse("schema", error))?;

    let expected_package = format!("{OFFICIAL_RUNTIME_PACKAGE_PREFIX}{}", added.runtime_id);
    if added.package_id.as_str() != expected_package {
        return Err(refuse(
            "package-id-binding",
            format!(
                "package_id `{}` must be `{expected_package}`",
                added.package_id
            ),
        ));
    }
    if INDEX_REFUSED_RUNTIME_IDS.contains(&added.runtime_id.as_str()) {
        return Err(refuse(
            "refused-runtime-id",
            format!("runtime id `{}` cannot be claimed", added.runtime_id),
        ));
    }
    if !added.url.bytes().all(is_url_safe) {
        return Err(refuse(
            "url-charset",
            "url may hold only letters, digits and `:/._+-`",
        ));
    }
    for other in &existing {
        let same_package = other.package_id == added.package_id;
        let same_runtime = other.runtime_id == added.runtime_id;
        if same_package != same_runtime {
            return Err(refuse(
                "identity-conflict",
                format!(
                    "`{}` and runtime id `{}` are already bound to `{}` and `{}`",
                    added.package_id, added.runtime_id, other.package_id, other.runtime_id
                ),
            ));
        }
    }
    if existing.iter().any(|other| other.digest == added.digest) {
        return Err(refuse("duplicate-digest", "digest is already admitted"));
    }

    version_order(added, &existing)?;

    let merged: Vec<IndexEntry> = existing
        .into_iter()
        .map(index_entry)
        .chain(std::iter::once(entry))
        .collect();
    probe(&merged).map_err(|error| refuse("schema", error))?;

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
            "version-not-semver",
            format!("version `{}`: {error}", added.version),
        )
    })?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(refuse(
            "version-not-release",
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
                "existing-tree",
                format!("admitted version `{}`: {error}", other.version),
            )
        })?;
        if admitted == version {
            return Err(refuse(
                "duplicate-version",
                format!("version `{version}` is already admitted"),
            ));
        }
        if admitted > version {
            return Err(refuse(
                "version-not-greater",
                format!("version `{version}` is not above admitted `{admitted}`"),
            ));
        }
    }
    Ok(())
}

/// Runs the daemon's structural entry rules over `entries`.
fn probe(entries: &[IndexEntry]) -> Result<(), XtaskError> {
    let now = unix_now()?;
    let body = IndexBody {
        schema_version: INDEX_SCHEMA_VERSION,
        scope: OFFICIAL_SCOPE.to_owned(),
        sequence: now,
        expires_at: now + PROBE_LIFETIME_SECONDS,
        entries: entries.to_vec(),
    };
    check_index_body(&body, now).map_err(XtaskError::Index)
}

/// The smallest version `comparator` admits, or `None` when it has no lower
/// bound.
fn lower_bound(comparator: &Comparator) -> Option<Version> {
    let (minor, patch) = (comparator.minor.unwrap_or(0), comparator.patch.unwrap_or(0));
    match comparator.op {
        Op::Exact | Op::GreaterEq | Op::Tilde | Op::Caret | Op::Wildcard => {
            Some(Version::new(comparator.major, minor, patch))
        }
        Op::Greater => Some(match (comparator.minor, comparator.patch) {
            (Some(minor), Some(patch)) => Version::new(comparator.major, minor, patch + 1),
            (Some(minor), None) => Version::new(comparator.major, minor + 1, 0),
            (None, _) => Version::new(comparator.major + 1, 0, 0),
        }),
        _ => None,
    }
}

fn core_range(added: &EntryFile, core_version: &Version) -> Result<String, XtaskError> {
    let requirement = parse_core_range(&added.core).ok_or_else(|| {
        refuse(
            "core-range-unparsed",
            "core must be a semver range with at least one comparator",
        )
    })?;
    if requirement
        .comparators
        .iter()
        .any(|comparator| !comparator.pre.is_empty())
    {
        return Err(refuse(
            "core-prerelease",
            "a core range must not name a pre-release",
        ));
    }
    if !requirement.matches(core_version) {
        return Err(refuse(
            "core-excludes-core-tag",
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
            "core-lower-bound",
            format!("core range admits {lowest}, below the first index core {FIRST_INDEX_CORE}"),
        ));
    }
    Ok(format!("core range ok: lower_bound={lowest}\n"))
}
