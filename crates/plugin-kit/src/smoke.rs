//! Kit `smoke`: a model-free start of the built runtime on an isolated daemon.
//!
//! The contract matches K3: a throwaway index names only the built archive on
//! an isolated `--local-only` daemon, `session new` starts the runtime,
//! `session detection` leaves `unknown` within a bounded named timeout,
//! `session screen` is non-empty, and the generic smoke stops after the
//! screen check. No full agent run: the planner ruled the generic smoke stops
//! at the screen, with package suites (`smoke-script`) running after it on
//! the same isolated root.

use std::path::{Path, PathBuf};
use std::time::Duration;

use package::Limits;
use protocol::AgentActivity;

use crate::archive::{archive_identity, archive_package, build_bytes};
use crate::entry::EntryFile;
use crate::error::{refuse, Error};
use crate::isolated::{Host, Root};
use crate::throwaway::{self, ThrowawayArgs};

/// How long a smoke waits for the detector to leave `unknown` after start.
/// A prompt prints immediately; a runtime that needs longer to print its
/// first screen fails the smoke.
const SMOKE_DETECTION_TIMEOUT: Duration = Duration::from_secs(60);

/// Delay between the detection polls while waiting to leave `unknown`.
const SMOKE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The inputs of a smoke.
#[derive(Debug)]
pub struct SmokeArgs {
    /// Package directory to build and smoke.
    pub dir: PathBuf,
    /// Release tree the isolated daemon copies its executables from.
    pub release_dir: PathBuf,
    /// Upstream directory (or executable inside one) put in front of the
    /// isolated daemon's `PATH`, so the runtime program resolves to the
    /// locked upstream install there.
    pub upstream: Option<PathBuf>,
}

/// What a passing smoke reports.
#[derive(Debug, serde::Serialize)]
pub struct SmokeReport {
    /// Package id smoked.
    pub package_id: protocol::PackageId,
    /// Package version smoked.
    pub version: protocol::PackageVersion,
    /// Activity the detector reported when the smoke stopped.
    pub activity: AgentActivity,
    /// Non-empty screen lines the smoke saw.
    pub screen_lines: usize,
    /// Isolation root the smoke ran in, removed before the report.
    pub root: PathBuf,
}

impl SmokeReport {
    /// The single stdout line of a passing smoke:
    /// `plugin smoke ok: <package_id> <version> <activity> <n> screen lines`.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "plugin smoke ok: {} {} {} {} screen lines",
            self.package_id,
            self.version,
            activity_label(self.activity),
            self.screen_lines
        )
    }
}

/// The wire name of `activity`, the same words the CLI prints.
fn activity_label(activity: AgentActivity) -> &'static str {
    match activity {
        AgentActivity::Working => "working",
        AgentActivity::Blocked => "blocked",
        AgentActivity::Idle => "idle",
        AgentActivity::Unknown => "unknown",
    }
}

/// Runs the generic smoke: build, install on an isolated daemon through a
/// throwaway index, start, wait for detection past `unknown`, require a
/// non-empty screen, then stop and remove the session.
///
/// With `--upstream`, the directory is put in front of the isolated
/// daemon's `PATH` before it starts, so the runtime program the package
/// declares resolves to the locked upstream install there.
pub fn smoke(args: &SmokeArgs) -> Result<SmokeReport, Error> {
    let bytes = build_bytes(&args.dir, &Limits::DEFAULT)?;
    let verified = package::read_archive(&bytes, &Limits::DEFAULT).map_err(Error::Package)?;
    let (_package_id, version) = archive_package(&verified, &args.dir)?;
    let identity = archive_identity(&verified, &args.dir)?;

    let root = Root::create().map_err(Error::Usage)?;
    let outcome = smoke_in(
        &root,
        &args.release_dir,
        args.upstream.as_deref(),
        &bytes,
        &identity,
        &version,
    );
    let removal = root.remove().map_err(Error::Usage);
    let report = outcome?;
    removal?;
    Ok(report)
}

/// Steps on an existing root: trust, host, install, start, detect, screen,
/// then stop and remove the session. Every step runs through the isolated
/// `pohunek` CLI, so the smoke exercises the same surface an owner uses.
fn smoke_in(
    root: &Root,
    release_dir: &Path,
    upstream: Option<&Path>,
    archive: &[u8],
    identity: &crate::archive::Identity,
    version: &protocol::PackageVersion,
) -> Result<SmokeReport, Error> {
    let digest = package::read_archive(archive, &Limits::DEFAULT)
        .map_err(Error::Package)?
        .digest()
        .clone();
    let entry = EntryFile {
        package_id: identity.package_id.clone(),
        kind: None,
        runtime_id: Some(identity.runtime_id.clone()),
        version: version.clone(),
        digest: digest.clone(),
        url: "https://localhost/kit-prove".to_owned(),
        platforms: vec![package::host_platform()
            .ok_or_else(|| refuse("this platform has no plugin index platform name"))?],
        core: ">=0.0.0".to_owned(),
        source: crate::entry::Source::prove_placeholder(version),
    }
    .signed_entry(package::INDEX_SCHEMA_VERSION)
    .map_err(|error| refuse(error.to_string()))?;

    let trust = root.path().join("trust");
    std::fs::create_dir_all(&trust).map_err(crate::files::io_error(&trust))?;
    throwaway::write(&ThrowawayArgs {
        entries: vec![entry],
        output: trust.clone(),
        schema_version: package::INDEX_SCHEMA_VERSION,
    })?;

    let mut host = Host::prepare(
        root,
        release_dir,
        &trust.join(crate::throwaway::ANCHOR_FILE),
        &trust.join(package::INDEX_CHANNEL_FILE_NAME),
    )
    .map_err(Error::Usage)?;
    if let Some(upstream) = upstream {
        host.prepend_path(upstream).map_err(Error::Usage)?;
    }
    host.launch_local_only(root).map_err(Error::Usage)?;

    let smoke = smoke_session(&mut host, &identity.runtime_id, archive, &digest);
    let stop = host.stop().map_err(Error::Usage);
    let (activity, screen_lines) = smoke?;
    stop?;
    Ok(SmokeReport {
        package_id: identity.package_id.clone(),
        version: version.clone(),
        activity,
        screen_lines,
        root: root.path().to_path_buf(),
    })
}

/// Installs the archive under the throwaway trust, starts one session on the
/// installed runtime, waits for the published activity to leave `unknown`,
/// requires a non-empty screen, then stops and removes the session. The
/// generic smoke stops after the screen check: no full agent run.
fn smoke_session(
    host: &mut Host,
    runtime_id: &protocol::RuntimeId,
    archive: &[u8],
    digest: &package::PackageDigest,
) -> Result<(AgentActivity, usize), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = host.root().join("archives");
    std::fs::create_dir_all(&directory).map_err(crate::files::io_error(&directory))?;
    let archive_path = directory.join("package.tar.zst");
    std::fs::write(&archive_path, archive)
        .and_then(|()| {
            std::fs::set_permissions(&archive_path, std::fs::Permissions::from_mode(0o644))
        })
        .map_err(|error| refuse(format!("cannot write the archive copy: {error}")))?;
    let archive_arg = archive_path.to_string_lossy();
    let index = host
        .root()
        .join("trust")
        .join(package::OFFICIAL_INDEX_FILE_NAME);
    let index_arg = index.to_string_lossy();
    let output = host
        .cli(&[
            "plugin",
            "install",
            &archive_arg,
            "--index",
            &index_arg,
            "--yes",
            "--json",
        ])
        .map_err(Error::Usage)?;
    let installed: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_error| refuse("`pohunek plugin install`: stdout is not one JSON document"))?;
    if output.code != Some(0)
        || installed["ok"]["package"]["digest"] != digest.as_str()
        || installed["ok"]["package"]["enabled"] != true
    {
        return Err(refuse(
            "the install did not report the archive as an enabled package",
        ));
    }

    let output = host
        .cli(&["session", "new", "--agent", runtime_id.as_str(), "--json"])
        .map_err(Error::Usage)?;
    let started: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_error| refuse("`pohunek session new`: stdout is not one JSON document"))?;
    let session = started["ok"]["id"]
        .as_str()
        .ok_or_else(|| refuse("`pohunek session new` reported no session id"))?
        .to_owned();
    let wait = wait_for_screen(host, &session);
    let cleanup = stop_session(host, &session);
    let (activity, screen_lines) = wait?;
    cleanup?;
    Ok((activity, screen_lines))
}

/// Stops and removes the smoke session: both run and a non-zero exit of
/// either fails the smoke while the first error is kept.
fn stop_session(host: &mut Host, session: &str) -> Result<(), Error> {
    check_session_command(host, &["session", "stop", session])
        .and(check_session_command(host, &["session", "rm", session]))
}

/// Runs one session cleanup command and fails on a non-zero exit.
fn check_session_command(host: &mut Host, argv: &[&str]) -> Result<(), Error> {
    let output = host.cli(argv).map_err(Error::Usage)?;
    if output.code != Some(0) {
        return Err(refuse(format!(
            "`pohunek {}` exited {}",
            argv.join(" "),
            output
                .code
                .map_or_else(|| "on a signal".to_owned(), |code| code.to_string())
        )));
    }
    Ok(())
}
/// `session list --json`, then requires a non-empty `session screen`.
fn wait_for_screen(host: &mut Host, session: &str) -> Result<(AgentActivity, usize), Error> {
    let deadline = std::time::Instant::now() + SMOKE_DETECTION_TIMEOUT;
    let activity = loop {
        let output = host
            .cli(&["session", "list", "--json"])
            .map_err(Error::Usage)?;
        let listed: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_error| refuse("`pohunek session list`: stdout is not one JSON document"))?;
        let activity = listed["ok"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .find(|info| info["id"] == session)
            .and_then(|info| info["activity"].as_str())
            .unwrap_or("unknown");
        match activity {
            "working" => break AgentActivity::Working,
            "blocked" => break AgentActivity::Blocked,
            "idle" => break AgentActivity::Idle,
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(refuse(format!(
                "the detector stayed `unknown` for {SMOKE_DETECTION_TIMEOUT:?} after start"
            )));
        }
        std::thread::sleep(SMOKE_POLL_INTERVAL);
    };
    let output = host
        .cli(&["session", "screen", session, "--json"])
        .map_err(Error::Usage)?;
    let screen: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_error| refuse("`pohunek session screen`: stdout is not one JSON document"))?;
    let lines = screen["ok"]["visible_lines"].as_array().map_or(0, Vec::len);
    if lines == 0 {
        return Err(refuse("the session screen stayed empty"));
    }
    Ok((activity, lines))
}
