//! `plugin features-check`: the feature-removal check of #1659 D4.
//!
//! A runtime feature is removed only when no package still requires it. The
//! check collects every `requires` from two sources: the descriptors under
//! `runtime-packages/` and every `runtime` entry of an official index
//! document (a context entry has no runtime descriptor and is skipped). For
//! an index entry the descriptor comes from the archive its `url` names: the
//! bytes verify against the entry's `digest` before anything is decoded. A
//! required feature that is missing from the registry fails, naming the
//! feature and the requiring package.
//!
//! The registry is `crates/daemon/src/agent/host/fixtures/features.txt`, the
//! same snapshot the append-only guard compares `RUNTIME_FEATURES` against:
//! one `<id> <since>` row per line, the id in the first column.
//!
//! The index signature is not verified here: the check reads requirements, it
//! authorizes nothing.
//!
//! The `requires` key is read as raw TOML on purpose: the core loader would
//! refuse an unknown feature before the check could name it, while here an
//! unknown value is exactly what must fail with the feature and the package
//! named.
//!
//! Exit statuses: `0` on a pass; `1` when a required feature is missing or
//! when content is refused (a malformed index document, an archive whose
//! digest differs from the entry, an archive over a size cap, an unreadable
//! local file, or a descriptor whose identity differs from the entry);
//! `2` on a refused invocation. Network: the official index and the archives
//! its entries list are fetched with `curl`. A fetch that fails prints a
//! `warn` line naming the URL (plus a `::warning` run annotation on GitHub
//! Actions) and the check continues with the other sources; it never fails
//! on network alone. In tests the index comes from `--index <file>` and an
//! entry `url` may be a `file://` path to a fixture archive, so no test
//! needs the network.

// Rust guideline compliant 2026-11-16

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use package::{
    parse_index_envelope, read_archive_with_digest, IndexEntryKind, Limits, MAX_COMPRESSED_BYTES,
    MAX_INDEX_BYTES,
};

use crate::catalog::{descriptor_package, io_error, read_input, DESCRIPTOR_PATH};
use crate::XtaskError;

/// The registry snapshot, relative to the checkout root: the same file the
/// append-only guard compares `RUNTIME_FEATURES` against.
const REGISTRY_SNAPSHOT: &str = "crates/daemon/src/agent/host/fixtures/features.txt";

/// Descriptors the check reads, relative to the checkout root.
const PACKAGES_DIR: &str = "runtime-packages";

/// Descriptor file name inside a package directory.
const DESCRIPTOR_FILE: &str = "runtime.toml";

/// The official index v2, fetched when `--index` names no file. Public, no
/// token. Same value as `OFFICIAL_INDEX_URL_V2` in
/// `crates/cli/src/commands/plugin_index.rs`, kept in sync by hand because
/// `xtask` must not depend on the CLI crate.
const OFFICIAL_INDEX_URL: &str = "https://github.com/pohunekd/pohunek-plugin-index/releases/latest/download/official-index-v2.json";

/// Bound for every file read from disk (the snapshot, local descriptors, a
/// `--index` document, a `file://` entry archive): the index limit the
/// package crate enforces.
const MAX_INPUT_BYTES: u64 = MAX_INDEX_BYTES as u64;

/// Bound for an entry archive fetched over the network: the archive limit the
/// package crate's reader enforces.
const MAX_ARCHIVE_BYTES: u64 = MAX_COMPRESSED_BYTES;

/// Longest one fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// The inputs of `plugin features-check`.
pub(crate) struct FeaturesCheckArgs {
    /// Checkout root the registry snapshot and `runtime-packages/` resolve
    /// against.
    pub(crate) root: PathBuf,
    /// Index document to read instead of fetching the official one.
    pub(crate) index: Option<PathBuf>,
}

/// One `requires` entry: the feature and the package that requires it.
struct Requirement {
    feature: String,
    package_id: String,
    version: String,
    source: String,
}

/// A fetch that failed: either the check warns naming the URL and continues
/// with the other sources, or the content is refused and the check fails.
enum FetchFailure {
    /// A lost or refused transfer: warn and continue.
    Warn(String),
    /// Content over a size cap: fail like refused content.
    Fail(String),
}

/// Reads the registry snapshot: the feature id in the first column of every
/// non-empty line.
fn registry_ids(snapshot: &Path) -> Result<BTreeSet<String>, XtaskError> {
    let bytes = read_input(snapshot, MAX_INPUT_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|_cause| {
        XtaskError::Usage(format!(
            "registry snapshot `{}` is not UTF-8",
            snapshot.display()
        ))
    })?;
    let mut ids = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let id = line.split_whitespace().next().unwrap_or("");
        if id.is_empty() {
            return Err(XtaskError::Usage(format!(
                "registry snapshot `{}` has an empty row",
                snapshot.display()
            )));
        }
        ids.insert(id.to_owned());
    }
    Ok(ids)
}

/// The `requires` of one descriptor: an absent key means no requirements, any
/// other shape is a refusal naming the descriptor.
fn descriptor_requires(descriptor: &[u8], what: &str) -> Result<Vec<String>, XtaskError> {
    let invalid = |reason: &str| XtaskError::Usage(format!("package descriptor `{what}` {reason}"));
    let text = std::str::from_utf8(descriptor).map_err(|_cause| invalid("is not UTF-8"))?;
    let document: toml::Table = text
        .parse()
        .map_err(|_cause| invalid("is not valid TOML"))?;
    let Some(requires) = document.get("requires") else {
        return Ok(Vec::new());
    };
    let requires = requires
        .as_array()
        .ok_or_else(|| invalid("has a `requires` that is not an array of strings"))?;
    requires
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("has a `requires` that is not an array of strings"))
        })
        .collect()
}

/// The `requires` of every descriptor under `runtime-packages/`, each named
/// by the package id and version its own descriptor declares.
fn local_requires(root: &Path) -> Result<Vec<Requirement>, XtaskError> {
    let dir = root.join(PACKAGES_DIR);
    let mut descriptors: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(io_error(&dir))?;
    for entry in entries {
        let entry = entry.map_err(io_error(&dir))?;
        let descriptor = entry.path().join(DESCRIPTOR_FILE);
        if descriptor.is_file() {
            descriptors.push(descriptor);
        }
    }
    descriptors.sort();
    let mut requirements = Vec::new();
    for descriptor in descriptors {
        let bytes = read_input(&descriptor, MAX_INPUT_BYTES)?;
        let (package_id, version, _table) =
            descriptor_package(&bytes).map_err(|error| XtaskError::Usage(error.to_string()))?;
        let what = descriptor.display().to_string();
        for feature in descriptor_requires(&bytes, &what)? {
            requirements.push(Requirement {
                feature,
                package_id: package_id.to_string(),
                version: version.to_string(),
                source: what.clone(),
            });
        }
    }
    Ok(requirements)
}

/// Fetches `url`: a `file://` URL reads the file bounded and without
/// following a final symlink, but only when the index document came from
/// `--index <file>` (tests); every other URL must be `https://` and goes
/// through `curl --fail` with pinned protocols, a timeout and a size cap. A
/// failure carries the URL: a lost or refused transfer warns and skips, while
/// content over a size cap fails like refused content.
fn fetch(url: &str, max_bytes: u64, local_files: bool) -> Result<Vec<u8>, FetchFailure> {
    if let Some(path) = url.strip_prefix("file://") {
        if !local_files {
            return Err(FetchFailure::Fail(format!(
                "refused URL `{url}`: only an `--index` document may list `file://` archives"
            )));
        }
        let path = Path::new(path);
        return read_input(path, MAX_ARCHIVE_BYTES)
            .map_err(|error| FetchFailure::Warn(format!("cannot read `{url}`: {error}")));
    }
    if !url.starts_with("https://") {
        return Err(FetchFailure::Fail(format!(
            "refused URL `{url}`: only https URLs are fetched"
        )));
    }
    let output = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--silent",
            "--show-error",
            "--max-time",
            &FETCH_TIMEOUT.as_secs().to_string(),
            "--max-filesize",
            &max_bytes.to_string(),
            url,
        ])
        .output()
        .map_err(|error| FetchFailure::Warn(format!("cannot run curl for `{url}`: {error}")))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    if output.status.code() == Some(63) {
        return Err(FetchFailure::Fail(format!(
            "fetch of `{url}` exceeds the {max_bytes}-byte limit"
        )));
    }
    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        return Err(FetchFailure::Warn(format!("curl failed for `{url}`")));
    }
    Err(FetchFailure::Warn(format!(
        "curl failed for `{url}`: {detail}"
    )))
}

/// The `requires` of every `runtime` entry of the index document `bytes`: the
/// archive each entry lists is fetched from its `url`, verified against the
/// entry's `digest` before anything is decoded, and its descriptor read. The
/// descriptor must name the entry's package id and version. A digest
/// mismatch, an unreadable archive or a differing identity fails; a failed
/// fetch warns and skips the entry, and a context entry has no runtime
/// descriptor and is skipped.
fn index_requires(
    bytes: &[u8],
    local_files: bool,
    warnings: &mut Vec<String>,
) -> Result<Vec<Requirement>, XtaskError> {
    let envelope = parse_index_envelope(bytes).map_err(XtaskError::Index)?;
    let mut requirements = Vec::new();
    for entry in &envelope.index.entries {
        if entry.kind() != IndexEntryKind::Runtime {
            continue;
        }
        let package_id = entry.package_id.clone();
        let version = entry.version.clone();
        let bytes = match fetch(entry.url.as_str(), MAX_ARCHIVE_BYTES, local_files) {
            Ok(bytes) => bytes,
            Err(FetchFailure::Warn(detail)) => {
                warnings.push(format!("warn: {detail}, skipping `{package_id}`"));
                continue;
            }
            Err(FetchFailure::Fail(detail)) => {
                return Err(XtaskError::Admission(detail));
            }
        };
        let verified =
            read_archive_with_digest(&bytes, &entry.digest, &Limits::DEFAULT).map_err(|error| {
                match error {
                    package::ArchiveError::DigestMismatch => XtaskError::Admission(format!(
                        "archive of `{package_id} {version}` differs from the entry's digest"
                    )),
                    error => XtaskError::Admission(format!(
                        "archive of `{package_id} {version}`: {error}"
                    )),
                }
            })?;
        let descriptor = verified
            .entries()
            .iter()
            .find(|entry| entry.path == DESCRIPTOR_PATH)
            .ok_or_else(|| {
                XtaskError::Admission(format!(
                    "archive of `{package_id} {version}` has no `{DESCRIPTOR_PATH}`"
                ))
            })?;
        let what = format!("{package_id} {version} (index archive)");
        let refused =
            |reason: &str| XtaskError::Admission(format!("package descriptor `{what}` {reason}"));
        let (declared_id, declared_version, _table) = descriptor_package(&descriptor.contents)
            .map_err(|error| refused(&error.to_string()))?;
        if declared_id != package_id || declared_version != version {
            return Err(refused(
                "names a package id or version the entry does not list",
            ));
        }
        let requires = descriptor_requires(&descriptor.contents, &what)
            .map_err(|error| XtaskError::Admission(error.to_string()))?;
        for feature in requires {
            requirements.push(Requirement {
                feature,
                package_id: package_id.to_string(),
                version: version.to_string(),
                source: what.clone(),
            });
        }
    }
    Ok(requirements)
}

/// Runs the feature-removal check: every required feature must be in the
/// registry. Prints one `warn` line per skipped fetch (plus a run annotation
/// under `GITHUB_ACTIONS`) and fails with the missing features, each with the
/// packages that require it.
pub(crate) fn features_check(args: &FeaturesCheckArgs) -> Result<String, XtaskError> {
    let registry = registry_ids(&args.root.join(REGISTRY_SNAPSHOT))?;
    let mut requirements = local_requires(&args.root)?;
    let mut warnings = Vec::new();
    let local_files = args.index.is_some();
    let index_bytes = match &args.index {
        Some(path) => Some(read_input(path, MAX_INPUT_BYTES)?),
        None => match fetch(OFFICIAL_INDEX_URL, MAX_INPUT_BYTES, false) {
            Ok(bytes) => Some(bytes),
            Err(FetchFailure::Warn(detail)) => {
                warnings.push(format!("warn: {detail}, skipping the official index"));
                None
            }
            Err(FetchFailure::Fail(detail)) => {
                return Err(XtaskError::Admission(detail));
            }
        },
    };
    if let Some(bytes) = index_bytes {
        requirements.extend(index_requires(&bytes, local_files, &mut warnings)?);
    }
    let mut missing: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for requirement in &requirements {
        if !registry.contains(&requirement.feature) {
            missing
                .entry(requirement.feature.as_str())
                .or_default()
                .insert(format!(
                    "{} {} ({})",
                    requirement.package_id, requirement.version, requirement.source
                ));
        }
    }
    let annotations = std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
    for warning in &warnings {
        eprintln!("{warning}");
        if annotations {
            let detail = warning.strip_prefix("warn: ").unwrap_or(warning);
            eprintln!("::warning title=features-check::{detail}");
        }
    }
    if missing.is_empty() {
        return Ok(format!(
            "plugin features-check ok: {} feature(s), {} requirement(s)",
            registry.len(),
            requirements.len()
        ));
    }
    let mut lines = Vec::new();
    for (feature, packages) in &missing {
        let mut packages: Vec<&str> = packages.iter().map(String::as_str).collect();
        packages.sort_unstable();
        lines.push(format!(
            "runtime feature `{feature}` is required by {} but missing from the registry",
            packages.join(", ")
        ));
    }
    Err(XtaskError::Admission(lines.join("; ")))
}
