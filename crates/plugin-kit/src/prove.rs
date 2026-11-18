//! `prove`: a model-free install of one archive through the index path.
//!
//! The proof checks that the archive is the one its entry lists, signs a
//! throwaway index that lists exactly that entry, starts an isolated daemon
//! that trusts only the throwaway root, installs the archive with the real CLI
//! and inspects the result. The runtime program of the package is never
//! launched. Every outcome is written as evidence JSON; the exit status is 0
//! only for a pass.
//!
//! The kit binary proves two shapes: `--dir` builds the archive from the
//! package directory first ([`prove_package`]), while `--archive --entry`
//! proves an admitted release asset against its real entry ([`prove_archive`],
//! used by the release proof workflow in #1618). Both write the kit's own
//! digest as `kit_sha256` in the evidence; the `xtask plugin prove` wrapper
//! keeps the `xtask` CLI, flags, exit codes and evidence JSON: its evidence
//! has no kit digest, exactly the documented keys.

// Rust guideline compliant 2026-11-06

use std::fs;
use std::path::{Path, PathBuf};

use package::{
    host_platform, read_archive, Limits, ANCHOR_FILE_NAME, INDEX_CHANNEL_FILE_NAME,
    OFFICIAL_INDEX_FILE_NAME, OFFICIAL_INDEX_FILE_NAME_V2,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::archive::{archive_identity, archive_package, descriptor_package};
use crate::entry::entry_of;
use crate::error::Error;
use crate::files::{document_bytes, hex, io_error, read_input, write_new_document};
use crate::isolated::{Host, Root};
use crate::throwaway::{self, ThrowawayArgs};

/// Schema version of the evidence document.
const EVIDENCE_SCHEMA: u32 = 1;

/// Directory of the root that receives the throwaway trust documents.
const TRUST_DIRECTORY: &str = "trust";

/// Directory of the root that holds the verified archive bytes the CLI installs.
const ARCHIVE_DIRECTORY: &str = "archive";

/// Name of the verified archive copy.
const ARCHIVE_FILE: &str = "package.tar.zst";

/// Mode of the verified archive copy: readable by the daemon, as an installed
/// release file would be.
const ARCHIVE_MODE: u32 = 0o644;

/// Mode of the directories the throwaway and the archive copy live in.
const DIRECTORY_MODE: u32 = 0o755;

/// The steps of a proof, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Digest,
    Trust,
    Host,
    Install,
    Inspect,
    Cleanup,
}

impl Step {
    /// The name evidence and diagnostics use.
    fn name(self) -> &'static str {
        match self {
            Self::Digest => "digest",
            Self::Trust => "trust",
            Self::Host => "host",
            Self::Install => "install",
            Self::Inspect => "inspect",
            Self::Cleanup => "cleanup",
        }
    }
}

/// The inputs of a proof of one archive through its entry file.
///
/// The `xtask plugin prove` wrapper fills this from `--archive --entry`; the
/// kit `prove` command builds the archive from `--dir` first and synthesizes
/// the entry (see [`ProvePackageArgs`]).
#[derive(Debug)]
pub struct ProveArgs {
    /// Archive file to prove.
    pub archive: PathBuf,
    /// Entry file that lists exactly the archive.
    pub entry: PathBuf,
    /// Release tree the isolated daemon copies its executables from.
    pub release_dir: PathBuf,
    /// Evidence JSON file to create exclusively.
    pub evidence: PathBuf,
    /// Install the package disabled (it stays selected), so `inspect`
    /// must refuse it for being disabled.
    pub no_enable: bool,
    /// Install this extra archive first through a digest trust, so the
    /// proof runs alongside another installed version: a newer version of
    /// the same package leaves the proved version unselected.
    pub pre_install: Option<PathBuf>,
    /// SHA-256 of the kit binary that runs the proof, filled by the kit's
    /// own CLI only (`xtask plugin prove` evidence has no kit digest).
    pub kit_sha256: Option<String>,
}

/// The inputs of a proof of one package directory.
///
/// The archive is built from `dir` through the canonical build first, so
/// the proved bytes are the bytes the directory builds; the entry is
/// synthesized from the descriptor and the digest, with no `[source]`
/// admission metadata involved.
#[derive(Debug)]
pub struct ProvePackageArgs {
    /// Package directory to build and prove.
    pub dir: PathBuf,
    /// Release tree the isolated daemon copies its executables from.
    pub release_dir: PathBuf,
    /// Evidence JSON file to create exclusively.
    pub evidence: PathBuf,
    /// Install the package disabled (it stays selected), so `inspect`
    /// must refuse it for being disabled.
    pub no_enable: bool,
    /// SHA-256 of the kit binary that runs the proof, filled by the kit's
    /// own CLI only (`xtask plugin prove` evidence has no kit digest).
    pub kit_sha256: Option<String>,
}

/// What a passing package proof reports.
#[derive(Debug, serde::Serialize)]
pub struct ProveReport {
    /// Package id proved.
    pub package_id: protocol::PackageId,
    /// Package version proved.
    pub version: protocol::PackageVersion,
    /// Digest of the proved archive bytes.
    pub digest: package::PackageDigest,
    /// Isolation root the proof ran in, removed before the report.
    pub root: PathBuf,
}

impl ProveReport {
    /// The single stdout line of a passing proof:
    /// `plugin prove ok: <package_id> <version> <digest>`.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "plugin prove ok: {} {} {}",
            self.package_id,
            self.version,
            self.digest.as_str()
        )
    }
}

/// What a passing archive proof reports: the stdout line and the removed
/// isolation root it ran in.
#[derive(Debug)]
pub struct ArchiveReport {
    /// The single stdout line of a passing proof.
    pub line: String,
    /// Isolation root the proof ran in, removed before the report.
    pub root: PathBuf,
}

/// A failed proof: the step that failed and why. Its exit status is 1.
#[derive(Debug)]
pub struct Verdict {
    step: Step,
    detail: String,
}

impl Verdict {
    /// The name of the failed step, as evidence and diagnostics use it.
    #[must_use]
    pub fn step_name(&self) -> &'static str {
        self.step.name()
    }

    /// Why the step failed.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plugin prove failed at step `{}`: {}",
            self.step.name(),
            self.detail
        )
    }
}

impl std::error::Error for Verdict {}

/// The evidence document.
#[derive(Serialize)]
struct Evidence<'a> {
    schema: u32,
    /// First line of `pohunekd --version`; null when the proof ended before
    /// `pohunekd --version` ran.
    core_version: Option<&'a str>,
    /// SHA-256 of the kit binary that wrote this evidence (#1618): the
    /// release proof workflow checks this digest against its own build. The
    /// kit CLI fills it; the `xtask plugin prove` wrapper leaves it out.
    #[serde(skip_serializing_if = "Option::is_none")]
    kit_sha256: Option<&'a str>,
    package_id: &'a str,
    version: &'a str,
    digest: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    platform: &'a str,
    result: &'static str,
    failed_step: Option<&'static str>,
}

/// What a finished proof knows: the daemon version (when it got that far) and
/// the failure (when there was one).
struct Outcome {
    core_version: Option<String>,
    failure: Option<Verdict>,
}

/// Proves the archive and writes the evidence.
///
/// Returns `Ok` with the one-line report for a pass and `Err` for a failed
/// proof or a refused input. An invocation that cannot name its entry or
/// platform, or whose evidence path exists, writes no evidence.
///
/// A runtime entry proves a runtime package: the install must report the
/// runtime enabled and selected, and the inspect must see the runtime
/// without a fault. A context entry proves a context package: the inspect
/// must report the package enabled and selected, and an entry for
/// `pohunek.context.assistant` must let `assistant help --agent prove-assistant --print-prompt`
/// print a prompt (the explicit agent name skips auto selection and the
/// host reports no such agent).
///
/// The runtime program of the package is never launched.
pub fn prove_archive(args: &ProveArgs) -> Result<ArchiveReport, Error> {
    let entry = entry_of(&args.entry)?;
    let root = prove(
        &entry,
        &args.evidence,
        args.kit_sha256.as_deref(),
        |entry, archive, outcome, root| {
            run_package_steps(
                root,
                &args.release_dir,
                entry,
                archive,
                outcome,
                args.no_enable,
                args.pre_install.as_deref(),
            )
        },
        || read_input(&args.archive, Limits::DEFAULT.max_compressed_bytes),
    )?;
    Ok(ArchiveReport {
        line: format!(
            "plugin prove ok: {} {} {}",
            entry.package_id,
            entry.version,
            entry.digest.as_str()
        ),
        root,
    })
}

/// Proves the package directory and writes the evidence, plus the kit digest
/// (#1618): the archive is built from the directory first, so the proved
/// bytes are the canonical build of the directory.
pub fn prove_package(args: &ProvePackageArgs) -> Result<ProveReport, Error> {
    let bytes = crate::archive::build_bytes(&args.dir, &package::Limits::DEFAULT)?;
    let verified =
        package::read_archive(&bytes, &package::Limits::DEFAULT).map_err(Error::Package)?;
    let (package_id, version) = archive_package(&verified, &args.dir)?;
    let digest = verified.digest().clone();
    let runtime_id = archive_identity(&verified, &args.dir).map(|identity| identity.runtime_id)?;
    let entry = crate::entry::EntryFile {
        package_id: package_id.clone(),
        kind: None,
        runtime_id: Some(runtime_id),
        version: version.clone(),
        digest: digest.clone(),
        url: "https://localhost/kit-prove".to_owned(),
        platforms: vec![package::host_platform().ok_or_else(|| {
            Error::Usage("this platform has no plugin index platform name".to_owned())
        })?],
        core: ">=0.0.0".to_owned(),
        source: crate::entry::Source::prove_placeholder(&version),
    }
    .signed_entry(package::INDEX_SCHEMA_VERSION)?;
    let root = prove(
        &entry,
        &args.evidence,
        args.kit_sha256.as_deref(),
        |entry, archive, outcome, root| {
            run_package_steps(
                root,
                &args.release_dir,
                entry,
                archive,
                outcome,
                args.no_enable,
                None,
            )
        },
        || Ok(bytes.clone()),
    )?;
    Ok(ProveReport {
        package_id,
        version,
        digest,
        root,
    })
}

/// Shared proof driver: checks the archive against the entry, writes the
/// throwaway trust, runs the isolated daemon, and writes the evidence.
fn prove(
    entry: &package::IndexEntry,
    path: &Path,
    kit_sha256: Option<&str>,
    run: impl FnOnce(&package::IndexEntry, &[u8], &mut Outcome, &Root) -> Result<Host, Verdict>,
    archive: impl FnOnce() -> Result<Vec<u8>, Error>,
) -> Result<PathBuf, Error> {
    let is_context = entry.kind() == package::IndexEntryKind::Context;
    let platform = host_platform().ok_or_else(|| {
        Error::Usage("this platform has no plugin index platform name".to_owned())
    })?;
    check_evidence_path(path)?;
    let archive = archive()?;

    let mut outcome = Outcome {
        core_version: None,
        failure: None,
    };
    let (result, root) = run_steps(entry, &archive, &mut outcome, run);
    if let Err(failure) = result {
        outcome.failure = Some(failure);
    }

    let digest = entry.digest.as_str();
    let kind = is_context.then_some("context");
    let document = Evidence {
        schema: EVIDENCE_SCHEMA,
        core_version: outcome.core_version.as_deref(),
        kit_sha256,
        package_id: entry.package_id.as_str(),
        version: entry.version.as_str(),
        digest,
        kind,
        platform: &platform,
        result: if outcome.failure.is_some() {
            "fail"
        } else {
            "pass"
        },
        failed_step: outcome.failure.as_ref().map(|failure| failure.step.name()),
    };
    write_new_document(path, &document_bytes(&document)?)?;
    match outcome.failure {
        Some(failure) => Err(Error::Prove(failure)),
        None => Ok(root),
    }
}

/// Refuses an evidence path that exists or whose directory does not.
fn check_evidence_path(path: &Path) -> Result<(), Error> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(Error::Usage(format!(
            "the evidence file `{}` exists already",
            path.display()
        )));
    }
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let metadata = fs::metadata(parent).map_err(io_error(parent))?;
    if !metadata.is_dir() {
        return Err(Error::Usage(format!(
            "`{}` is not a directory",
            parent.display()
        )));
    }
    Ok(())
}

fn fail(step: Step) -> impl Fn(String) -> Verdict {
    move |detail| Verdict { step, detail }
}

/// SHA-256 of the running kit binary, as `sha256:<64 hex>`: the release
/// proof workflow (#1618) checks this digest against its own build.
///
/// # Errors
///
/// Returns a usage error when the binary cannot be read: the attestation
/// must not pass while its digest is unknown.
pub fn kit_digest() -> Result<String, Error> {
    let path = std::env::current_exe()
        .map_err(|error| Error::Usage(format!("cannot name the running kit binary: {error}")))?;
    let bytes = fs::read(&path).map_err(crate::files::io_error(&path))?;
    Ok(format!("sha256:{}", hex(&Sha256::digest(&bytes))))
}

/// Runs the steps in order. The isolation root exists only after the digest
/// step passed, and is removed on every path. Returns the outcome and the
/// root path: the root is gone on every path, so the caller can name it in
/// its report and the scenario can assert it is gone.
fn run_steps(
    entry: &package::IndexEntry,
    archive: &[u8],
    outcome: &mut Outcome,
    run: impl FnOnce(&package::IndexEntry, &[u8], &mut Outcome, &Root) -> Result<Host, Verdict>,
) -> (Result<(), Verdict>, PathBuf) {
    if let Err(detail) = check_archive(entry, archive) {
        return (Err(fail(Step::Digest)(detail)), PathBuf::new());
    }
    let root = match Root::create() {
        Ok(root) => root,
        Err(detail) => return (Err(fail(Step::Trust)(detail)), PathBuf::new()),
    };
    let path = root.path().to_path_buf();
    let result = run(entry, archive, outcome, &root);
    match result {
        Ok(host) => {
            // A pass needs a clean stop and a removed root.
            let result = host
                .stop()
                .map_err(fail(Step::Cleanup))
                .and(root.remove().map_err(fail(Step::Cleanup)));
            (result, path)
        }
        Err(failure) => {
            // The original failure is the verdict; a removal problem on top
            // of it only costs a temporary directory.
            let _ = root.remove();
            (Err(failure), path)
        }
    }
}

/// Step 1: the archive is the one the entry lists.
///
/// The SHA-256 of the file bytes equals the entry's digest, the archive reads
/// under the strict reader, and its descriptor names the entry's package id
/// and version, plus the runtime id for a runtime entry.
fn check_archive(entry: &package::IndexEntry, archive: &[u8]) -> Result<(), String> {
    let actual = format!("sha256:{}", hex(&Sha256::digest(archive)));
    if actual != entry.digest.as_str() {
        return Err("the archive digest differs from the entry's".to_owned());
    }
    let verified = read_archive(archive, &Limits::DEFAULT)
        .map_err(|error| format!("the archive is not a valid package: {error}"))?;
    if entry.kind() == package::IndexEntryKind::Context {
        if entry.runtime_id.is_some() {
            return Err("a context entry names no runtime".to_owned());
        }
        let descriptor = verified
            .entries()
            .iter()
            .find(|entry| entry.path == crate::archive::DESCRIPTOR_PATH)
            .ok_or_else(|| "the archive has no `runtime.toml`".to_owned())?;
        let (_package_id, _version, table) =
            descriptor_package(&descriptor.contents).map_err(|error| error.to_string())?;
        if table.get("runtime").is_some() || table.get("context").is_none() {
            return Err("the archive is not a context package".to_owned());
        }
        let (package_id, version) =
            archive_package(&verified, Path::new("archive")).map_err(|error| error.to_string())?;
        if package_id != entry.package_id || version != entry.version {
            return Err("the archive descriptor differs from the entry's identity".to_owned());
        }
        return Ok(());
    }
    let identity =
        archive_identity(&verified, Path::new("archive")).map_err(|error| error.to_string())?;
    if identity.package_id != entry.package_id
        || Some(&identity.runtime_id) != entry.runtime_id.as_ref()
        || identity.version != entry.version
    {
        return Err("the archive descriptor differs from the entry's identity".to_owned());
    }
    Ok(())
}

/// Steps 2 to 5; returns the running host for the cleanup step.
fn run_package_steps(
    root: &Root,
    release_dir: &Path,
    entry: &package::IndexEntry,
    archive: &[u8],
    outcome: &mut Outcome,
    no_enable: bool,
    pre_install: Option<&Path>,
) -> Result<Host, Verdict> {
    let trust = write_trust(entry, root).map_err(fail(Step::Trust))?;
    let mut host = Host::prepare(
        root,
        release_dir,
        &trust.join(ANCHOR_FILE_NAME),
        &trust.join(INDEX_CHANNEL_FILE_NAME),
    )
    .map_err(fail(Step::Host))?;
    outcome.core_version = Some(host.core_version().map_err(fail(Step::Host))?);
    host.launch_local_only(root).map_err(fail(Step::Host))?;

    let archive_path = write_archive(archive, root, ARCHIVE_FILE).map_err(fail(Step::Install))?;
    if let Some(pre_install) = pre_install {
        pre_install_archive(&mut host, entry, pre_install).map_err(fail(Step::Install))?;
    }
    install(&mut host, entry, &archive_path, &trust, no_enable).map_err(fail(Step::Install))?;
    inspect(&mut host, entry).map_err(fail(Step::Inspect))?;
    Ok(host)
}

/// Step 2: the throwaway trust that signs exactly this entry. Returns the
/// directory that holds the anchor, the channel and the signed index.
fn write_trust(entry: &package::IndexEntry, root: &Root) -> Result<PathBuf, String> {
    let trust = root.path().join(TRUST_DIRECTORY);
    create_directory(&trust)?;
    let schema_version = if entry.kind() == package::IndexEntryKind::Context {
        package::INDEX_SCHEMA_VERSION_2
    } else {
        package::INDEX_SCHEMA_VERSION
    };
    // The entry may come from a schema 2 file while the trust signs schema
    // 1: a schema 1 runtime entry carries no kind, so normalize it before
    // the throwaway validates the kind/`runtime_id` pairing.
    let mut entry = entry.clone();
    if schema_version != package::INDEX_SCHEMA_VERSION_2 {
        entry.kind = None;
    }
    throwaway::write(&ThrowawayArgs {
        entries: vec![entry],
        output: trust.clone(),
        schema_version,
    })
    .map_err(|error| error.to_string())?;
    Ok(trust)
}

fn create_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .mode(DIRECTORY_MODE)
        .create(path)
        .map_err(|error| format!("cannot create `{}`: {error}", path.display()))
}

/// Writes the archive bytes where the daemon can read them.
fn write_archive(archive: &[u8], root: &Root, name: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = root.path().join(ARCHIVE_DIRECTORY);
    create_directory(&directory)?;
    let path = directory.join(name);
    fs::write(&path, archive)
        .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(ARCHIVE_MODE)))
        .map_err(|error| format!("cannot write the archive copy: {error}"))?;
    Ok(path)
}

/// Step 4 setup: installs an extra archive through a digest trust, so the
/// proof runs alongside another installed version of the same package. Only
/// the proved version goes through the entry's index path; this install only
/// stages the unselected state the `install` and `inspect` flag checks must
/// refuse.
fn pre_install_archive(
    host: &mut Host,
    entry: &package::IndexEntry,
    archive: &Path,
) -> Result<(), String> {
    let bytes = fs::read(archive)
        .map_err(|error| format!("cannot read the pre-installed archive: {error}"))?;
    let verified = read_archive(&bytes, &Limits::DEFAULT)
        .map_err(|error| format!("the pre-installed archive is not a valid package: {error}"))?;
    let (package_id, _version) =
        archive_package(&verified, archive).map_err(|error| error.to_string())?;
    if package_id != entry.package_id {
        return Err(format!(
            "the pre-installed archive names `{package_id}`, not the entry's `{}`",
            entry.package_id
        ));
    }
    let digest = format!("sha256:{}", hex(&Sha256::digest(&bytes)));
    let stored = archive.to_string_lossy().into_owned();
    let output = host.cli(&[
        "plugin", "install", &stored, "--sha256", &digest, "--yes", "--json",
    ])?;
    let _ = json_result(&output, "`pohunek plugin install --sha256`")?;
    Ok(())
}

/// The `ok` member of a `--json` document, or the failure the document names.
fn json_result(output: &crate::isolated::Output, what: &str) -> Result<Value, String> {
    let document: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_error| format!("{what}: stdout is not one JSON document"))?;
    if output.code == Some(0) {
        return Ok(document["ok"].clone());
    }
    let code = document["err"]["code"].as_str().unwrap_or("no error code");
    Err(format!("{what} was refused: {code}"))
}

/// Step 4: install through the index path and require the entry's official
/// package with the entry's digest. Every entry requires `enabled` here;
/// `selected` is required here too, except for a context entry under
/// `--no-enable` (a disabled install is inspected but cannot pass the
/// inspect predicate). The runtime inspect step checks only the runtime id,
/// the digest and the fault, and the runtime install step owns the flag
/// checks for it. The context inspect step adds the `runtime_id` and
/// `fault` checks, which the context install step never asserts.
fn install(
    host: &mut Host,
    entry: &package::IndexEntry,
    archive: &Path,
    trust: &Path,
    no_enable: bool,
) -> Result<(), String> {
    let archive = archive.to_string_lossy();
    let index_name = if entry.kind() == package::IndexEntryKind::Context {
        OFFICIAL_INDEX_FILE_NAME_V2
    } else {
        OFFICIAL_INDEX_FILE_NAME
    };
    let index = trust.join(index_name);
    let index = index.to_string_lossy();
    let mut arguments = vec![
        "plugin", "install", &archive, "--index", &index, "--yes", "--json",
    ];
    if no_enable {
        arguments.push("--no-enable");
    }
    let output = host.cli(&arguments)?;
    let installed = json_result(&output, "`pohunek plugin install`")?;
    let package = &installed["package"];
    let context = entry.kind() == package::IndexEntryKind::Context;
    if installed["status"] != "installed"
        || package["origin"] != "official"
        || package["digest"] != entry.digest.as_str()
        || (!context && (package["enabled"] != true || package["selected"] != true))
        || (context && !no_enable && (package["enabled"] != true || package["selected"] != true))
    {
        return Err(
            "the install did not report the entry's archive as an official package".to_owned(),
        );
    }
    Ok(())
}

/// Step 5: the daemon reports the entry's package without a fault, and the
/// package doctor finds nothing. A context entry has no runtime, so the
/// runtime invocation is replaced by reading the package back: inspect must
/// report it enabled and selected without a fault, and the assistant entry
/// must render its prompt for the explicit `--agent prove-assistant` name, so
/// the readback touches the installed package. The explicit name skips auto
/// selection (the host reports no such agent) and `--print-prompt` starts no
/// session; a missing or damaged assistant package fails, and so does a
/// prompt the daemon cannot compose.
///
/// A `--no-enable` context install reaches this predicate disabled (but
/// still selected), and a pre-installed newer version leaves the proved
/// version unselected: both must fail here.
fn check_inspected_context(
    entry: &package::IndexEntry,
    package: &serde_json::Value,
) -> Result<(), String> {
    if !package["runtime_id"].is_null() {
        return Err("the installed package serves a runtime".to_owned());
    }
    if !package["fault"].is_null() {
        return Err("the installed package reports a fault".to_owned());
    }
    if package["digest"] != entry.digest.as_str() {
        return Err("the installed digest differs from the entry's".to_owned());
    }
    if package["enabled"] != true {
        return Err("the installed package is disabled".to_owned());
    }
    if package["selected"] != true {
        return Err("the installed package is not selected".to_owned());
    }
    Ok(())
}

fn inspect(host: &mut Host, entry: &package::IndexEntry) -> Result<(), String> {
    let output = host.cli(&["plugin", "inspect", entry.package_id.as_str(), "--json"])?;
    let inspected = json_result(&output, "`pohunek plugin inspect`")?;
    let package = &inspected["package"];
    if entry.kind() == package::IndexEntryKind::Context {
        check_inspected_context(entry, package)?;
        if entry.package_id.as_str() == protocol::ASSISTANT_CONTEXT_PACKAGE_ID {
            let output = host.cli(&[
                "assistant",
                "help",
                "--agent",
                "prove-assistant",
                "--print-prompt",
                "--json",
            ])?;
            let printed = json_result(&output, "`pohunek assistant help --print-prompt`")?;
            if printed["prompt"].as_str().is_none_or(str::is_empty) {
                return Err("the assistant package printed no prompt".to_owned());
            }
        }
    } else {
        let runtime = entry.runtime_id.as_ref().map(protocol::RuntimeId::as_str);
        if package["runtime_id"].as_str() != runtime
            || !package["fault"].is_null()
            || package["digest"] != entry.digest.as_str()
        {
            return Err(
                "the inspected package is not the entry's runtime, or reports a fault".to_owned(),
            );
        }
    }
    let doctor = host.cli(&["plugin", "doctor", entry.package_id.as_str()])?;
    if doctor.code != Some(0) {
        return Err("`pohunek plugin doctor` reported a finding".to_owned());
    }
    Ok(())
}
