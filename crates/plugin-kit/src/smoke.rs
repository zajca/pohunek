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

/// Default bound for the detector to leave `unknown` after start.
/// A prompt prints immediately; a runtime that needs longer to print its
/// first screen fails the smoke.
const DETECTION_TIMEOUT_DEFAULT: Duration = Duration::from_secs(60);

/// Shortest `--detection-timeout` a smoke accepts.
const DETECTION_TIMEOUT_MIN: Duration = Duration::from_secs(1);

/// Longest `--detection-timeout` a smoke accepts: the default. The option
/// only tightens the bounded-detection contract; a loosened bound would hide
/// a runtime that is too slow to show its first screen.
const DETECTION_TIMEOUT_MAX: Duration = DETECTION_TIMEOUT_DEFAULT;

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
    /// Smoke script: a plugin-owned executable from the plugin repository,
    /// run once inside the live isolated root after the generic smoke
    /// checks pass.
    pub script: Option<PathBuf>,
    /// Bound for the smoke script in whole seconds; `None` keeps
    /// [`SCRIPT_TIMEOUT_DEFAULT`](crate::isolated::SCRIPT_TIMEOUT_DEFAULT).
    /// Validated to `1..=900` in [`smoke`] before the rig starts.
    pub script_timeout_secs: Option<u64>,
    /// Bound for the detector to leave `unknown` in whole seconds; `None`
    /// keeps the 60 s default. Validated to `1..=60` in [`smoke`] before the
    /// rig starts, so it can only tighten the bound.
    pub detection_timeout_secs: Option<u64>,
}

/// What a passing smoke reports.
#[derive(Debug, serde::Serialize)]
pub struct SmokeReport {
    /// Package id smoked.
    pub package_id: protocol::PackageId,
    /// Package version smoked.
    pub version: protocol::PackageVersion,
    /// Activity the detector reported when the smoke stopped; absent for a
    /// commands-only package, which starts no session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<AgentActivity>,
    /// Non-empty screen lines the smoke saw; absent for a commands-only
    /// package.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_lines: Option<usize>,
    /// Effective smoke-script bound in whole seconds; present on every
    /// runtime smoke run that had a script (the default when the caller
    /// passed no `--script-timeout`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_timeout_s: Option<u64>,
    /// Effective detection bound in whole seconds; present on every runtime
    /// smoke run (the default when the caller passed no
    /// `--detection-timeout`), absent for a commands-only package.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detection_timeout_s: Option<u64>,
    /// Commands a commands-only package exports, each run once and exited 0;
    /// empty for a runtime package.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    /// Isolation root the smoke ran in, removed before the report.
    pub root: PathBuf,
}

impl SmokeReport {
    /// The single stdout line of a passing smoke:
    /// `plugin smoke ok: <package_id> <version> <activity> <n> screen lines`
    /// for a runtime package,
    /// `plugin smoke ok: <package_id> <version> commands <n> commands run`
    /// for a commands-only package.
    #[must_use]
    pub fn line(&self) -> String {
        match (self.activity, self.screen_lines) {
            (Some(activity), Some(screen_lines)) => format!(
                "plugin smoke ok: {} {} {} {} screen lines",
                self.package_id,
                self.version,
                activity_label(activity),
                screen_lines
            ),
            _ => format!(
                "plugin smoke ok: {} {} commands {} commands run",
                self.package_id,
                self.version,
                self.commands.len()
            ),
        }
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
    let (package_id, version) = archive_package(&verified, &args.dir)?;
    let commands = commands_only_exports(&verified, &args.dir)?;
    if commands.is_some()
        && (args.upstream.is_some()
            || args.script.is_some()
            || args.script_timeout_secs.is_some()
            || args.detection_timeout_secs.is_some())
    {
        return Err(refuse(
            "a commands-only package serves no runtime: --upstream, --script, --script-timeout and --detection-timeout apply to a runtime package only",
        ));
    }
    if let Some(seconds) = args.detection_timeout_secs {
        let (min, max) = (
            DETECTION_TIMEOUT_MIN.as_secs(),
            DETECTION_TIMEOUT_MAX.as_secs(),
        );
        if seconds < min || seconds > max {
            return Err(refuse(format!(
                "--detection-timeout must be {min}..={max} seconds: {seconds}"
            )));
        }
    }
    if let Some(seconds) = args.script_timeout_secs {
        let (min, max) = (
            crate::isolated::SCRIPT_TIMEOUT_MIN.as_secs(),
            crate::isolated::SCRIPT_TIMEOUT_MAX.as_secs(),
        );
        if seconds < min || seconds > max {
            return Err(refuse(format!(
                "--script-timeout must be {min}..={max} seconds: {seconds}"
            )));
        }
        if args.script.is_none() {
            return Err(refuse(
                "--script-timeout needs --script: no smoke script runs without one",
            ));
        }
    }
    let identity = match commands {
        Some(_) => None,
        None => Some(archive_identity(&verified, &args.dir)?),
    };

    let root = Root::create().map_err(Error::Usage)?;
    let path = root.path().to_path_buf();
    let timeout = Duration::from_secs(
        args.script_timeout_secs
            .unwrap_or(crate::isolated::SCRIPT_TIMEOUT_DEFAULT.as_secs()),
    );
    let detection_timeout = args
        .detection_timeout_secs
        .map_or(DETECTION_TIMEOUT_DEFAULT, Duration::from_secs);
    let outcome = match (&identity, &commands) {
        (Some(identity), _) => smoke_in(
            &root,
            &args.release_dir,
            args.upstream.as_deref(),
            args.script.as_deref(),
            timeout,
            detection_timeout,
            &bytes,
            identity,
            &version,
        ),
        (None, Some(commands)) => smoke_commands_in(
            &root,
            &args.release_dir,
            &bytes,
            &package_id,
            &version,
            commands,
        ),
        (None, None) => unreachable!("a package without an identity exports commands only"),
    };
    let removal = root.remove().map_err(Error::Usage);
    // Every failure past this point names the used isolation root. The
    // removal ran on every path, so the named root is gone unless the
    // removal itself is the failure.
    let report = outcome.map_err(|error| Error::Smoke {
        root: path.clone(),
        error: Box::new(error),
    })?;
    removal.map_err(|error| Error::Smoke {
        root: path,
        error: Box::new(error),
    })?;
    Ok(report)
}

/// The command names of a descriptor that exports commands and neither a
/// runtime nor a context; `None` for any other shape.
fn commands_only_exports(
    verified: &package::VerifiedArchive,
    archive: &Path,
) -> Result<Option<Vec<String>>, Error> {
    let descriptor = verified
        .entries()
        .iter()
        .find(|entry| entry.path == crate::archive::DESCRIPTOR_PATH)
        .ok_or_else(|| {
            Error::Usage(format!(
                "package archive `{}` has no `{}`",
                archive.display(),
                crate::archive::DESCRIPTOR_PATH
            ))
        })?;
    let (_id, _version, table) = crate::archive::descriptor_package(&descriptor.contents)?;
    if table.contains_key("runtime") || table.contains_key("context") {
        return Ok(None);
    }
    Ok(table
        .get("commands")
        .and_then(toml::Value::as_table)
        .map(|commands| commands.keys().cloned().collect()))
}

/// Steps on an existing root: trust, host, install, start, detect, screen,
/// then stop and remove the session. Every step runs through the isolated
/// `pohunek` CLI, so the smoke exercises the same surface an owner uses.
#[expect(
    clippy::too_many_arguments,
    reason = "threads one bound alongside the path arguments"
)]
fn smoke_in(
    root: &Root,
    release_dir: &Path,
    upstream: Option<&Path>,
    script: Option<&Path>,
    script_timeout: Duration,
    detection_timeout: Duration,
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

    let smoke = smoke_session(
        &mut host,
        &identity.runtime_id,
        archive,
        &digest,
        script,
        script_timeout,
        detection_timeout,
    );
    let stop = host.stop().map_err(Error::Usage);
    let (activity, screen_lines) = smoke?;
    stop?;
    Ok(SmokeReport {
        package_id: identity.package_id.clone(),
        version: version.clone(),
        activity: Some(activity),
        screen_lines: Some(screen_lines),
        script_timeout_s: script.map(|_| script_timeout.as_secs()),
        detection_timeout_s: Some(detection_timeout.as_secs()),
        commands: Vec::new(),
        root: root.path().to_path_buf(),
    })
}

/// Commands-only smoke on an existing root: an empty throwaway trust, an
/// isolated daemon, an owner-trusted install of the archive by its digest, and
/// one `host command run local` per exported command. Every command must exit
/// 0 and the install must report the package enabled.
fn smoke_commands_in(
    root: &Root,
    release_dir: &Path,
    archive: &[u8],
    package_id: &protocol::PackageId,
    version: &protocol::PackageVersion,
    commands: &[String],
) -> Result<SmokeReport, Error> {
    let digest = package::read_archive(archive, &Limits::DEFAULT)
        .map_err(Error::Package)?
        .digest()
        .clone();
    let trust = root.path().join("trust");
    std::fs::create_dir_all(&trust).map_err(crate::files::io_error(&trust))?;
    throwaway::write(&ThrowawayArgs {
        entries: Vec::new(),
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
    host.launch_local_only(root).map_err(Error::Usage)?;
    let ran = run_commands(&mut host, package_id, archive, &digest, commands);
    let stop = host.stop().map_err(Error::Usage);
    ran?;
    stop?;
    Ok(SmokeReport {
        package_id: package_id.clone(),
        version: version.clone(),
        activity: None,
        screen_lines: None,
        script_timeout_s: None,
        detection_timeout_s: None,
        commands: commands.to_vec(),
        root: root.path().to_path_buf(),
    })
}

/// Installs the archive under the owner's digest trust and runs every
/// exported command once through the isolated CLI.
fn run_commands(
    host: &mut Host,
    package_id: &protocol::PackageId,
    archive: &[u8],
    digest: &package::PackageDigest,
    commands: &[String],
) -> Result<(), Error> {
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
    let output = host
        .cli(&[
            "plugin",
            "install",
            &archive_arg,
            "--sha256",
            digest.as_str(),
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
    for command in commands {
        let output = host
            .cli(&[
                "host",
                "command",
                "run",
                "local",
                package_id.as_str(),
                command,
            ])
            .map_err(Error::Usage)?;
        if output.code != Some(0) {
            return Err(refuse(format!(
                "the command `{command}` of `{package_id}` exited {}",
                output
                    .code
                    .map_or_else(|| "on a signal".to_owned(), |code| code.to_string())
            )));
        }
    }
    Ok(())
}

/// Installs the archive under the throwaway trust, starts one session on the
/// installed runtime, waits for the published activity to leave `unknown`,
/// requires a non-empty screen, then runs the smoke script (when one is
/// given) against that live session, and stops and removes the session.
/// The generic smoke stops after the screen check: no full agent run.
fn smoke_session(
    host: &mut Host,
    runtime_id: &protocol::RuntimeId,
    archive: &[u8],
    digest: &package::PackageDigest,
    script: Option<&Path>,
    script_timeout: Duration,
    detection_timeout: Duration,
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
    let wait = wait_for_screen(host, &session, detection_timeout);
    // The script runs only after every generic smoke check passed: a
    // failed wait never starts it, and teardown still happens below.
    let scripted = match (&wait, script) {
        (Ok(_), Some(script)) => Some(run_script(host, script, &session, script_timeout)),
        _ => None,
    };
    let cleanup = stop_session(host, &session);
    let (activity, screen_lines) = wait?;
    if let Some(scripted) = scripted {
        scripted?;
    }
    cleanup?;
    Ok((activity, screen_lines))
}

/// Runs the plugin-owned smoke script inside the live isolated root after
/// the generic smoke checks passed, against the live session id exported
/// under `POHUNEK_SMOKE_SESSION`. The script reaches the isolated daemon
/// through the isolated `XDG_RUNTIME_DIR`: a bare `pohunek` on its `PATH`
/// resolves its socket there, so only the isolated daemon's sessions
/// answer. `POHUNEK_SOCKET_PATH` is exported only as information for
/// hook-style clients. The inherited origin pair stays absent, so the
/// script connects as an owner with an explicit session id. The script
/// runs in the smoke's own cwd with its stdout and stderr both going to
/// the kit's stderr unchanged, so `--json` stdout stays one JSON document.
/// A non-zero script exit fails the smoke with an error naming the script
/// and the exit code.
fn run_script(
    host: &mut Host,
    script: &Path,
    session: &str,
    script_timeout: Duration,
) -> Result<(), Error> {
    let status = host
        .run_script(script, session, script_timeout)
        .map_err(Error::Usage)?;
    if status.success() {
        return Ok(());
    }
    Err(refuse(format!(
        "the smoke script `{}` exited {}",
        script.display(),
        status
            .code()
            .map_or_else(|| "on a signal".to_owned(), |code| code.to_string())
    )))
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
fn wait_for_screen(
    host: &mut Host,
    session: &str,
    timeout: Duration,
) -> Result<(AgentActivity, usize), Error> {
    let deadline = std::time::Instant::now() + timeout;
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
                "the detector stayed `unknown` for {timeout:?} after start"
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
