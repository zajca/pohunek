//! `plugin prove`: a model-free install of one archive through the index path.
//!
//! The proof checks that the archive is the one its entry lists, signs a
//! throwaway index that lists exactly that entry, starts an isolated daemon
//! that trusts only the throwaway root, installs the archive with the real CLI
//! and inspects the result. The runtime program of the package is never
//! launched. Every outcome is written as evidence JSON; the exit status is 0
//! only for a pass.

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

use crate::catalog::{archive_identity, hex, io_error, read_input};
use crate::isolated_host::{Host, Root};
use crate::plugin_index::{document_bytes, entry_of, throwaway, write_new_document, ThrowawayArgs};
use crate::XtaskError;

/// Schema version of the evidence document.
const EVIDENCE_SCHEMA: u32 = 1;

/// Directory of the root that holds the one-entry tree the throwaway signs.
const ENTRIES_DIRECTORY: &str = "entries";

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

/// The inputs of `plugin prove`.
pub(crate) struct ProveArgs {
    pub(crate) archive: PathBuf,
    pub(crate) entry: PathBuf,
    pub(crate) release_dir: PathBuf,
    pub(crate) evidence: PathBuf,
}

/// A failed proof: the step that failed and why. Its exit status is 1.
#[derive(Debug)]
pub struct Verdict {
    step: Step,
    detail: String,
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
/// without a fault. A context entry proves a context package: the install
/// must report the package enabled and selected, and an entry for
/// `pohunek.context.assistant` must let `assistant help --print-prompt`
/// print a prompt.
pub(crate) fn prove(args: &ProveArgs) -> Result<String, XtaskError> {
    let entry = entry_of(&args.entry)?;
    let is_context = entry.kind() == package::IndexEntryKind::Context;
    let platform = host_platform().ok_or_else(|| {
        XtaskError::Usage("this platform has no plugin index platform name".to_owned())
    })?;
    check_evidence_path(&args.evidence)?;
    let archive = read_input(&args.archive, Limits::DEFAULT.max_compressed_bytes)?;

    let mut outcome = Outcome {
        core_version: None,
        failure: None,
    };
    if let Err(failure) = run_steps(args, &entry, &archive, &mut outcome) {
        outcome.failure = Some(failure);
    }

    let digest = entry.digest.as_str();
    let kind = is_context.then_some("context");
    let evidence = Evidence {
        schema: EVIDENCE_SCHEMA,
        core_version: outcome.core_version.as_deref(),
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
    write_new_document(&args.evidence, &document_bytes(&evidence)?)?;
    match outcome.failure {
        Some(failure) => Err(XtaskError::Prove(failure)),
        None => Ok(format!(
            "plugin prove ok: {} {} {digest}",
            entry.package_id, entry.version
        )),
    }
}

/// Refuses an evidence path that exists or whose directory does not.
fn check_evidence_path(path: &Path) -> Result<(), XtaskError> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(XtaskError::Usage(format!(
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
        return Err(XtaskError::Usage(format!(
            "`{}` is not a directory",
            parent.display()
        )));
    }
    Ok(())
}

fn fail(step: Step) -> impl Fn(String) -> Verdict {
    move |detail| Verdict { step, detail }
}

/// Writes a host profile that selects `program` explicitly, so the assistant
/// readback of a context proof runs against a capable agent: `no_capable_agent`
/// can no longer stand in for a package the rig never read. The profile names
/// its base runtime only to stay resolvable; `--print-prompt` starts no
/// session, so the stub program never runs.
fn write_stub_agent_profile(root: &Root, program: &Path) -> Result<(), String> {
    let agents = root.config_home().join("agents");
    create_directory(&agents)?;
    fs::write(
        agents.join("prove-assistant.toml"),
        format!("base = \"shell\"\nprogram = \"{}\"\n", program.display()),
    )
    .map_err(|error| format!("cannot write the stub agent profile: {error}"))
}

/// Writes the stub program the assistant readback selects: it exits at once,
/// so a launch that reached it would fail instead of passing silently.
fn write_stub_agent_program(root: &Root) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = root.path().join("prove-assistant-agent");
    fs::write(&path, "#!/bin/sh\nexit 3\n")
        .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(0o755)))
        .map_err(|error| format!("cannot write the stub agent program: {error}"))?;
    Ok(path)
}

/// Runs the steps in order. The isolation root exists only after the digest
/// step passed, and is removed on every path.
fn run_steps(
    args: &ProveArgs,
    entry: &package::IndexEntry,
    archive: &[u8],
    outcome: &mut Outcome,
) -> Result<(), Verdict> {
    check_archive(entry, archive).map_err(fail(Step::Digest))?;

    let root = Root::create().map_err(fail(Step::Trust))?;
    let result = prove_in(args, entry, archive, &root, outcome);
    match result {
        Ok(host) => {
            // A pass needs a clean stop and a removed root.
            host.stop().map_err(fail(Step::Cleanup))?;
            root.remove().map_err(fail(Step::Cleanup))
        }
        Err(failure) => {
            // The original failure is the verdict; a removal problem on top
            // of it only costs a temporary directory.
            let _ = root.remove();
            Err(failure)
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
            .find(|entry| entry.path == crate::catalog::DESCRIPTOR_PATH)
            .ok_or_else(|| "the archive has no `runtime.toml`".to_owned())?;
        let (_package_id, _version, table) =
            crate::catalog::descriptor_package(&descriptor.contents)
                .map_err(|error| error.to_string())?;
        if table.get("runtime").is_some() || table.get("context").is_none() {
            return Err("the archive is not a context package".to_owned());
        }
        let (package_id, version) =
            crate::catalog::archive_package(&verified, Path::new("archive"))
                .map_err(|error| error.to_string())?;
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
fn prove_in(
    args: &ProveArgs,
    entry: &package::IndexEntry,
    archive: &[u8],
    root: &Root,
    outcome: &mut Outcome,
) -> Result<Host, Verdict> {
    let trust = write_trust(&args.entry, entry, root).map_err(fail(Step::Trust))?;
    let mut host = Host::prepare(
        root,
        &args.release_dir,
        &trust.join(ANCHOR_FILE_NAME),
        &trust.join(INDEX_CHANNEL_FILE_NAME),
    )
    .map_err(fail(Step::Host))?;
    outcome.core_version = Some(host.core_version().map_err(fail(Step::Host))?);
    if entry.kind() == package::IndexEntryKind::Context {
        let stub = write_stub_agent_program(root).map_err(fail(Step::Host))?;
        write_stub_agent_profile(root, &stub).map_err(fail(Step::Host))?;
    }
    host.launch_local_only(root).map_err(fail(Step::Host))?;

    let archive_path = write_archive(archive, root).map_err(fail(Step::Install))?;
    install(&mut host, entry, &archive_path, &trust).map_err(fail(Step::Install))?;
    inspect(&mut host, entry).map_err(fail(Step::Inspect))?;
    Ok(host)
}

/// Step 2: the throwaway trust that signs exactly this entry. Returns the
/// directory that holds the anchor, the channel and the signed index.
fn write_trust(
    entry_file: &Path,
    entry: &package::IndexEntry,
    root: &Root,
) -> Result<PathBuf, String> {
    let tree = root.path().join(ENTRIES_DIRECTORY);
    let package_directory = tree.join("official").join(entry.package_id.as_str());
    fs::create_dir_all(&package_directory)
        .map_err(|error| format!("cannot create the entries tree: {error}"))?;
    fs::copy(
        entry_file,
        package_directory.join(format!("{}.toml", entry.version.as_str())),
    )
    .map_err(|error| format!("cannot copy the entry file: {error}"))?;
    let trust = root.path().join(TRUST_DIRECTORY);
    create_directory(&trust)?;
    let schema_version = if entry.kind() == package::IndexEntryKind::Context {
        package::INDEX_SCHEMA_VERSION_2
    } else {
        package::INDEX_SCHEMA_VERSION
    };
    throwaway(&ThrowawayArgs {
        entries: tree,
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

/// Writes the digest-checked archive bytes where the daemon can read them.
fn write_archive(archive: &[u8], root: &Root) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = root.path().join(ARCHIVE_DIRECTORY);
    create_directory(&directory)?;
    let path = directory.join(ARCHIVE_FILE);
    fs::write(&path, archive)
        .and_then(|()| fs::set_permissions(&path, fs::Permissions::from_mode(ARCHIVE_MODE)))
        .map_err(|error| format!("cannot write the archive copy: {error}"))?;
    Ok(path)
}

/// The `ok` member of a `--json` document, or the failure the document names.
fn json_result(output: &crate::isolated_host::Output, what: &str) -> Result<Value, String> {
    let document: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_error| format!("{what}: stdout is not one JSON document"))?;
    if output.code == Some(0) {
        return Ok(document["ok"].clone());
    }
    let code = document["err"]["code"].as_str().unwrap_or("no error code");
    Err(format!("{what} was refused: {code}"))
}

/// Step 4: install through the index path and require an enabled, selected
/// official package with the entry's digest.
fn install(
    host: &mut Host,
    entry: &package::IndexEntry,
    archive: &Path,
    trust: &Path,
) -> Result<(), String> {
    let archive = archive.to_string_lossy();
    let index_name = if entry.kind() == package::IndexEntryKind::Context {
        OFFICIAL_INDEX_FILE_NAME_V2
    } else {
        OFFICIAL_INDEX_FILE_NAME
    };
    let index = trust.join(index_name);
    let index = index.to_string_lossy();
    let output = host.cli(&[
        "plugin", "install", &archive, "--index", &index, "--yes", "--json",
    ])?;
    let installed = json_result(&output, "`pohunek plugin install`")?;
    let package = &installed["package"];
    if installed["status"] != "installed"
        || package["origin"] != "official"
        || package["enabled"] != true
        || package["selected"] != true
        || package["digest"] != entry.digest.as_str()
    {
        return Err("the install did not report the entry's archive as an enabled, selected official package".to_owned());
    }
    Ok(())
}

/// Step 5: the daemon reports the entry's package without a fault, and the
/// package doctor finds nothing. A context entry has no runtime, so the
/// runtime invocation is replaced by reading the package back: inspect must
/// report it enabled and selected without a fault, and the assistant entry
/// must render its prompt through the stub agent the rig writes before the
/// daemon starts, so the readback touches the installed package. The stub
/// never launches (`--print-prompt` starts no session); a missing or damaged
/// assistant package fails, and so does a prompt the daemon cannot compose.
fn inspect(host: &mut Host, entry: &package::IndexEntry) -> Result<(), String> {
    let output = host.cli(&["plugin", "inspect", entry.package_id.as_str(), "--json"])?;
    let inspected = json_result(&output, "`pohunek plugin inspect`")?;
    let package = &inspected["package"];
    if entry.kind() == package::IndexEntryKind::Context {
        if !package["runtime_id"].is_null()
            || !package["fault"].is_null()
            || package["digest"] != entry.digest.as_str()
            || package["enabled"] != true
            || package["selected"] != true
        {
            return Err(
                "the inspected package is not the entry's enabled, selected context, or reports a fault"
                    .to_owned(),
            );
        }
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
