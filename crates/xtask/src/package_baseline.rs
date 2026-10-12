//! `cargo xtask package baseline`: which official packages the signed release proves unchanged.
//!
//! The repository is the Git top level of the working directory. For every
//! official package `<p>` (a directory under `runtime-packages/`) the command
//! decides whether the package released as `v<version>` is a trusted baseline
//! for the checkout, so a CI run may skip that package's job when nothing else
//! requires it. The checks run in this order, and the first failure makes the
//! package `missing` with a stable reason code ([`Reason`]):
//!
//! 1. The catalog verifies against the trust anchor at the current time
//!    (signature, signer window, revocation, expiry), names release
//!    `<version>` and carries the sequence that version implies. A failure
//!    here makes every package missing.
//! 2. The package is rebuilt from the checkout in memory, and the catalog has
//!    an entry with runtime id `<p>` and exactly that digest. The version is
//!    never matched: equal content is the only proof.
//! 3. The released archive in the release directory hashes to that digest.
//! 4. The entry's attestations cover every matrix row of `<p>`, and each
//!    referenced attestation file hashes to its catalog digest and attests
//!    that package digest, the digest of `compat/<p>/compatibility-lock.json`,
//!    the matrix suite version, the release commit and binary set.
//! 5. `runtime-packages/<p>` and `compat/<p>` in the work tree equal the
//!    release commit: no tracked difference and no untracked file, ignored
//!    ones included. A release commit missing from the repository makes the
//!    package missing.
//!
//! Everything read from the release side (the catalog file and the release
//! directory) only ever makes a package missing, so the caller rebuilds and
//! tests it. Inputs of the checkout itself (the package directories, locks,
//! matrix, trust anchor and the `--version` value) fail the command with a
//! non-zero exit and nothing on stdout, and the caller treats that as every
//! package missing.

// Rust guideline compliant 2026-10-12

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use package::{read_archive, verify_catalog, Limits, VerifiedCatalog, VerifiedEntry};
use serde::Serialize;

use crate::affected::{command_in, parse_nul_list, printable, GIT};
use crate::attestation::{
    lock_digest, lock_path, read_matrix, Attestation, MATRIX_PATH, MAX_DOCUMENT_BYTES,
    SCHEMA_VERSION,
};
use crate::catalog::{archive_identity, read_input, trust_anchor, unix_now};
use crate::package_affected::{git_toplevel, official_packages, LOCKS_DIR, PACKAGES_DIR};
use crate::release::{parse_version, sequence_of};
use crate::release_policy::{attestation_name, package_name};
use crate::release_tree::sha256_bytes;
use crate::runtime_package::build_bytes;
use crate::XtaskError;

/// Exit status of `git diff --quiet` when the compared trees are equal.
const DIFF_EQUAL: i32 = 0;

/// Exit status of `git diff --quiet` when the compared trees differ.
const DIFF_DIFFERENT: i32 = 1;

/// Options for one `cargo xtask package baseline` invocation.
#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Signed runtime catalog of the release (`runtime-catalog.json`); an
    /// absent file makes every package missing.
    #[arg(long, value_name = "FILE")]
    catalog: PathBuf,
    /// Trust anchor the catalog must verify against.
    #[arg(long, value_name = "FILE")]
    anchor: PathBuf,
    /// Directory holding the release's package archives and attestations.
    #[arg(long, value_name = "DIR")]
    release_dir: PathBuf,
    /// Release the baseline must come from, as `X.Y.Z`: the workspace version.
    #[arg(long, value_name = "X.Y.Z")]
    version: String,
    /// Print only the verdicts as one JSON object on stdout; the report goes
    /// to stderr.
    #[arg(long)]
    json: bool,
}

/// Why a package has no trusted baseline; JSON spells it as [`Reason::code`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reason {
    /// The catalog file does not exist.
    CatalogAbsent,
    /// The catalog file exists but cannot be read within the size limit.
    CatalogUnreadable,
    /// The catalog fails verification against the trust anchor now.
    CatalogUnverified,
    /// The verified catalog names another release version or sequence.
    ReleaseMismatch,
    /// The catalog has no entry for the package's runtime id.
    NotInCatalog,
    /// The catalog has entries for the runtime, none with the rebuilt digest.
    DigestMismatch,
    /// The released package archive does not exist.
    ArchiveAbsent,
    /// The released package archive cannot be read within the size limit.
    ArchiveUnreadable,
    /// The released package archive does not hash to the rebuilt digest.
    ArchiveMismatch,
    /// A matrix row of the package has no attestation in the entry.
    PlatformsUncovered,
    /// A referenced attestation file does not exist.
    AttestationAbsent,
    /// A referenced attestation file cannot be read within the size limit.
    AttestationUnreadable,
    /// An attestation file does not hash to its catalog digest.
    AttestationDigestMismatch,
    /// An attestation does not parse or names another runtime, platform,
    /// package id, commit or binary set than the catalog.
    AttestationInvalid,
    /// An attestation attests another package digest.
    AttestationPackageMismatch,
    /// An attestation attests another compatibility lock.
    AttestationLockMismatch,
    /// An attestation was produced by another consumer suite version.
    AttestationSuiteMismatch,
    /// The release commit is not in the repository.
    ReleaseCommitUnreachable,
    /// The package or compatibility directory differs from the release commit.
    ChangedSinceRelease,
}

impl Reason {
    /// The stable code CI matches on.
    fn code(self) -> &'static str {
        match self {
            Self::CatalogAbsent => "catalog_absent",
            Self::CatalogUnreadable => "catalog_unreadable",
            Self::CatalogUnverified => "catalog_unverified",
            Self::ReleaseMismatch => "release_mismatch",
            Self::NotInCatalog => "not_in_catalog",
            Self::DigestMismatch => "digest_mismatch",
            Self::ArchiveAbsent => "archive_absent",
            Self::ArchiveUnreadable => "archive_unreadable",
            Self::ArchiveMismatch => "archive_mismatch",
            Self::PlatformsUncovered => "platforms_uncovered",
            Self::AttestationAbsent => "attestation_absent",
            Self::AttestationUnreadable => "attestation_unreadable",
            Self::AttestationDigestMismatch => "attestation_digest_mismatch",
            Self::AttestationInvalid => "attestation_invalid",
            Self::AttestationPackageMismatch => "attestation_package_mismatch",
            Self::AttestationLockMismatch => "attestation_lock_mismatch",
            Self::AttestationSuiteMismatch => "attestation_suite_mismatch",
            Self::ReleaseCommitUnreachable => "release_commit_unreachable",
            Self::ChangedSinceRelease => "changed_since_release",
        }
    }
}

impl Serialize for Reason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

/// Whether a package has a trusted baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Verified,
    Missing,
}

/// The decision for one package, as printed by `--json`.
#[derive(Debug, Serialize)]
struct Outcome {
    verdict: Status,
    /// Why the package is missing; `null` when verified.
    reason: Option<Reason>,
    /// The verified baseline digest; `null` when missing.
    digest: Option<String>,
    /// The catalog lists another digest of this runtime under the version the
    /// checkout's descriptor declares: content changed without a version bump.
    version_not_bumped: bool,
    /// Human explanation, for the report only.
    #[serde(skip)]
    detail: String,
}

impl Outcome {
    fn verified(digest: String) -> Self {
        Self {
            verdict: Status::Verified,
            reason: None,
            digest: Some(digest),
            version_not_bumped: false,
            detail: String::new(),
        }
    }

    fn missing(reason: Reason, detail: impl Into<String>) -> Self {
        Self {
            verdict: Status::Missing,
            reason: Some(reason),
            digest: None,
            version_not_bumped: false,
            detail: detail.into(),
        }
    }
}

/// Every verdict, as printed by `--json`.
#[derive(Debug, Serialize)]
struct Verdicts {
    /// The release the baseline was looked up in.
    version: String,
    /// One verdict per official package, by directory name.
    packages: BTreeMap<String, Outcome>,
}

/// What the checkout says about one official package.
#[derive(Debug)]
struct Head {
    /// Digest of the archive rebuilt from `runtime-packages/<p>`.
    digest: String,
    /// Version the package descriptor declares.
    version: String,
    /// Digest of `compat/<p>/compatibility-lock.json`.
    lock_digest: String,
    /// Targets of the package's matrix rows.
    targets: BTreeSet<String>,
}

/// The checkout's packages and the matrix suite version.
#[derive(Debug)]
struct Checkout {
    root: PathBuf,
    suite_version: u32,
    packages: BTreeMap<String, Head>,
}

/// Prints the baseline verdict of every official package.
pub(crate) fn run(options: &Options) -> Result<(), XtaskError> {
    let cwd = std::env::current_dir().map_err(|source| XtaskError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let root = git_toplevel(&cwd)?;
    let version = parse_version(&options.version)?;
    let anchor = trust_anchor(&read_input(
        &options.anchor,
        u64::try_from(package::MAX_ANCHOR_BYTES).unwrap_or(u64::MAX),
    )?)?;
    let checkout = read_checkout(root)?;

    let catalog = load_catalog(
        &options.catalog,
        &anchor,
        &options.version,
        sequence_of(version),
        unix_now()?,
    );
    let mut packages = BTreeMap::new();
    for (name, head) in &checkout.packages {
        let verdict = match &catalog {
            Ok(catalog) => check_package(&checkout, name, head, catalog, options)?,
            Err((reason, detail)) => Outcome::missing(*reason, detail.clone()),
        };
        packages.insert(name.clone(), verdict);
    }
    let verdicts = Verdicts {
        version: options.version.clone(),
        packages,
    };
    let report = render_report(&verdicts);
    if options.json {
        let json = serde_json::to_string(&verdicts).map_err(XtaskError::Json)?;
        eprint!("{report}");
        println!("{json}");
    } else {
        print!("{report}");
    }
    Ok(())
}

/// Rebuilds every official package and reads its lock and matrix rows.
fn read_checkout(root: PathBuf) -> Result<Checkout, XtaskError> {
    let matrix = read_matrix(&root.join(MATRIX_PATH))?;
    let mut packages = BTreeMap::new();
    for name in official_packages(&root)? {
        let dir = root.join(PACKAGES_DIR).join(&name);
        let bytes = build_bytes(&dir, &Limits::DEFAULT)?;
        let archive = read_archive(&bytes, &Limits::DEFAULT).map_err(XtaskError::Package)?;
        let identity = archive_identity(&archive, &dir)?;
        let targets = matrix
            .rows
            .iter()
            .filter(|row| row.runtime == name)
            .map(|row| row.target.clone())
            .collect();
        let head = Head {
            digest: archive.digest().as_str().to_owned(),
            version: identity.version.as_str().to_owned(),
            lock_digest: lock_digest(&lock_path(&root, &name))?,
            targets,
        };
        packages.insert(name, head);
    }
    Ok(Checkout {
        root,
        suite_version: matrix.suite_version,
        packages,
    })
}

/// Reads and verifies the catalog, or returns why every package is missing.
fn load_catalog(
    path: &Path,
    anchor: &package::TrustAnchor,
    version: &str,
    sequence: u64,
    now: u64,
) -> Result<VerifiedCatalog, (Reason, String)> {
    let document = read_input(
        path,
        u64::try_from(package::MAX_CATALOG_BYTES).unwrap_or(u64::MAX),
    )
    .map_err(|error| match error {
        XtaskError::Io { ref source, .. } if source.kind() == ErrorKind::NotFound => {
            (Reason::CatalogAbsent, format!("catalog: {error}"))
        }
        other => (Reason::CatalogUnreadable, format!("catalog: {other}")),
    })?;
    let catalog = verify_catalog(&document, anchor, now, None)
        .map_err(|error| (Reason::CatalogUnverified, format!("catalog: {error}")))?;
    let released = catalog.release().version().to_string();
    if released != version {
        return Err((
            Reason::ReleaseMismatch,
            format!("catalog is for release {released}, not {version}"),
        ));
    }
    if catalog.sequence() != sequence {
        return Err((
            Reason::ReleaseMismatch,
            format!(
                "catalog sequence {} is not the sequence {sequence} of release {version}",
                catalog.sequence()
            ),
        ));
    }
    Ok(catalog)
}

/// Checks 2 to 5 for the package `name` against a verified catalog.
///
/// Fails only when Git cannot compare an existing release commit with the
/// work tree.
fn check_package(
    checkout: &Checkout,
    name: &str,
    head: &Head,
    catalog: &VerifiedCatalog,
    options: &Options,
) -> Result<Outcome, XtaskError> {
    let entries: Vec<&VerifiedEntry> = catalog
        .entries()
        .iter()
        .filter(|entry| entry.runtime_id().as_str() == name)
        .collect();
    if entries.is_empty() {
        return Ok(Outcome::missing(
            Reason::NotInCatalog,
            "the catalog has no entry for this runtime",
        ));
    }
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.digest().as_str() == head.digest)
    else {
        let mut verdict = Outcome::missing(
            Reason::DigestMismatch,
            format!("no catalog entry has the rebuilt digest {}", head.digest),
        );
        verdict.version_not_bumped = entries
            .iter()
            .any(|entry| entry.version().as_str() == head.version);
        if verdict.version_not_bumped {
            // Writing to a String cannot fail.
            let _ = write!(
                verdict.detail,
                "; content changed but version {} was not bumped",
                head.version
            );
        }
        return Ok(verdict);
    };
    if let Some(verdict) = check_archive(&options.release_dir, name, &options.version, head) {
        return Ok(verdict);
    }
    if let Some(verdict) = check_attestations(checkout, name, head, entry, catalog, options) {
        return Ok(verdict);
    }
    if let Some(verdict) = check_unchanged(&checkout.root, name, catalog.release().commit())? {
        return Ok(verdict);
    }
    Ok(Outcome::verified(head.digest.clone()))
}

/// Check 3: the released archive must hash to the rebuilt digest.
fn check_archive(release_dir: &Path, name: &str, version: &str, head: &Head) -> Option<Outcome> {
    let file = package_name(name, version);
    let bytes = match read_input(
        &release_dir.join(&file),
        Limits::DEFAULT.max_compressed_bytes,
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            let reason = if is_not_found(&error) {
                Reason::ArchiveAbsent
            } else {
                Reason::ArchiveUnreadable
            };
            return Some(Outcome::missing(reason, format!("{file}: {error}")));
        }
    };
    let digest = format!("sha256:{}", sha256_bytes(&bytes));
    (digest != head.digest).then(|| {
        Outcome::missing(
            Reason::ArchiveMismatch,
            format!("{file} hashes to {digest}, not {}", head.digest),
        )
    })
}

/// Check 4: the attestations cover the matrix rows and bind this package.
fn check_attestations(
    checkout: &Checkout,
    name: &str,
    head: &Head,
    entry: &VerifiedEntry,
    catalog: &VerifiedCatalog,
    options: &Options,
) -> Option<Outcome> {
    let attested: BTreeSet<&str> = entry
        .attestations()
        .iter()
        .map(|reference| reference.platform.as_str())
        .collect();
    let uncovered: Vec<&str> = head
        .targets
        .iter()
        .map(String::as_str)
        .filter(|target| !attested.contains(target))
        .collect();
    if head.targets.is_empty() || !uncovered.is_empty() {
        return Some(Outcome::missing(
            Reason::PlatformsUncovered,
            format!(
                "matrix rows without an attestation: [{}]",
                uncovered.join(", ")
            ),
        ));
    }
    let release = catalog.release();
    for reference in entry.attestations() {
        let platform = reference.platform.as_str();
        let file = attestation_name(name, platform);
        let bytes = match read_input(&options.release_dir.join(&file), MAX_DOCUMENT_BYTES) {
            Ok(bytes) => bytes,
            Err(error) => {
                let reason = if is_not_found(&error) {
                    Reason::AttestationAbsent
                } else {
                    Reason::AttestationUnreadable
                };
                return Some(Outcome::missing(reason, format!("{file}: {error}")));
            }
        };
        let digest = format!("sha256:{}", sha256_bytes(&bytes));
        if digest != reference.digest.as_str() {
            return Some(Outcome::missing(
                Reason::AttestationDigestMismatch,
                format!(
                    "{file} hashes to {digest}, the catalog says {}",
                    reference.digest
                ),
            ));
        }
        let Ok(document) = serde_json::from_slice::<Attestation>(&bytes) else {
            return Some(Outcome::missing(
                Reason::AttestationInvalid,
                format!("{file} is not an attestation document"),
            ));
        };
        let consistent = document.schema == SCHEMA_VERSION
            && document.runtime == name
            && document.target == platform
            && document.platform == platform
            && document.package_id == entry.package_id().as_str()
            && document.commit == release.commit()
            && release
                .binary_set_digest(platform)
                .is_some_and(|set| set.as_str() == document.binary_set_digest);
        let mismatch = if !consistent {
            Some((
                Reason::AttestationInvalid,
                "names another runtime, platform, package, commit or binary set",
            ))
        } else if document.package_digest != head.digest {
            Some((
                Reason::AttestationPackageMismatch,
                "attests another package digest",
            ))
        } else if document.upstream_lock_digest != head.lock_digest {
            Some((
                Reason::AttestationLockMismatch,
                "attests another compatibility lock",
            ))
        } else if document.suite_version != checkout.suite_version {
            Some((
                Reason::AttestationSuiteMismatch,
                "comes from another consumer suite version",
            ))
        } else {
            None
        };
        if let Some((reason, why)) = mismatch {
            return Some(Outcome::missing(reason, format!("{file} {why}")));
        }
    }
    None
}

/// Check 5: the package and compatibility directories equal the release commit.
///
/// The work tree is compared, not `HEAD`, because the rebuilt archive and the
/// lock digest come from the work tree too. Any untracked file below either
/// directory counts as a change, also one `.gitignore` excludes: a fixture
/// the release never saw costs a package job, never a skipped one.
fn check_unchanged(root: &Path, name: &str, commit: &str) -> Result<Option<Outcome>, XtaskError> {
    let object = format!("{commit}^{{commit}}");
    if !git_status(root, &["cat-file", "-e", &object])?.success() {
        return Ok(Some(Outcome::missing(
            Reason::ReleaseCommitUnreachable,
            format!("release commit {commit} is not in the repository"),
        )));
    }
    let package_dir = format!("{PACKAGES_DIR}/{name}");
    let lock_dir = format!("{LOCKS_DIR}/{name}");
    let diff = [
        "diff",
        "--quiet",
        "--no-ext-diff",
        "--no-textconv",
        commit,
        "--",
        &package_dir,
        &lock_dir,
    ];
    match git_status(root, &diff)?.code() {
        Some(DIFF_EQUAL) => {}
        Some(DIFF_DIFFERENT) => {
            return Ok(Some(Outcome::missing(
                Reason::ChangedSinceRelease,
                format!("{package_dir} or {lock_dir} differs from release commit {commit}"),
            )));
        }
        _ => {
            return Err(XtaskError::Usage(format!(
                "package baseline: `git {}` failed",
                diff.join(" ")
            )));
        }
    }
    let untracked = git_stdout(
        root,
        &["ls-files", "-z", "--others", "--", &package_dir, &lock_dir],
    )?;
    if let Some(path) = parse_nul_list(&untracked).next() {
        return Ok(Some(Outcome::missing(
            Reason::ChangedSinceRelease,
            format!("untracked file {} is not in the release", printable(&path)),
        )));
    }
    Ok(None)
}

/// Runs Git in `root` with output discarded and returns its exit status.
fn git_status(root: &Path, args: &[&str]) -> Result<std::process::ExitStatus, XtaskError> {
    command_in(root, GIT)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|source| XtaskError::Io {
            path: PathBuf::from(GIT),
            source,
        })
}

/// Runs Git in `root` and returns stdout, failing on a non-zero exit.
fn git_stdout(root: &Path, args: &[&str]) -> Result<Vec<u8>, XtaskError> {
    let output = command_in(root, GIT)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|source| XtaskError::Io {
            path: PathBuf::from(GIT),
            source,
        })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(XtaskError::Usage(format!(
            "package baseline: `git {}` failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn is_not_found(error: &XtaskError) -> bool {
    matches!(error, XtaskError::Io { source, .. } if source.kind() == ErrorKind::NotFound)
}

/// Human report: one line per package with its verdict and the reason.
fn render_report(verdicts: &Verdicts) -> String {
    let mut out = format!("package baseline: release v{}\n", verdicts.version);
    for (name, verdict) in &verdicts.packages {
        let digest = verdict.digest.as_deref().unwrap_or_default();
        let code = verdict.reason.map(Reason::code).unwrap_or_default();
        // Writing to a String cannot fail.
        let _ = match verdict.verdict {
            Status::Verified => writeln!(out, "package baseline: {name} verified {digest}"),
            Status::Missing => writeln!(
                out,
                "package baseline: {name} missing ({code}): {}",
                verdict.detail
            ),
        };
    }
    out
}
