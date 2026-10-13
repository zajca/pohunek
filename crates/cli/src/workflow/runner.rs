//! Runs one declared action of an installed, enabled workflow plugin.
//!
//! Before anything starts, the plugin's files are verified again and the run
//! is refused when the plugin is disabled or tampered with, the action is
//! unknown, extra arguments are not accepted, the session origin markers are
//! incomplete, the declared timeout is above the owner's ceiling, or the
//! context is above its cap. The action then runs without a shell: argv is
//! `<files>/<argv0>` (or the absolute host path) with the manifest arguments
//! and, for `args = "passthrough"`, the extra arguments verbatim. The child
//! runs in the package's `files/` directory with stdin from `/dev/null`, in a
//! process group of its own, with only stdio inherited, and with exactly the
//! environment [`build_environment`] sets.
//!
//! Stdout and stderr stream to this process's own stdout and stderr, each up
//! to its cap, however slowly their consumer reads, unless this process is
//! signalled. Once the group is gone, a pipe that a descendant outside the
//! group keeps open is cut after [`OUTPUT_IDLE`] of time not spent writing to
//! that consumer, whether the descendant writes or not, with a content-free
//! warning. A run ends in one outcome, decided in this order: an output
//! stream that passed its cap (`output_limit`), a terminating signal this
//! process received (`interrupted`), the deadline (`timeout`), a failure to
//! observe the child (`failed`), then the child's own status. Every outcome
//! but a normal exit stops the group with `SIGTERM`, the owner's grace (which
//! ends early when the direct child exits), then `SIGKILL`; after a normal
//! exit the rest of the group is killed. Each run of
//! an installed plugin, refused or not, appends one record to its run log,
//! except a run refused before the log could be opened: invalid owner knobs
//! (they set the log's retention) or a log that cannot be opened.

// Rust guideline compliant 2026-10-13

use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use package::registry::PackageRecord;
use pohunek_platform::supervisor::BOOTSTRAP_ENV_ALLOWLIST;
use protocol::{
    PackageDigest, PackageId, PackageVersion, ProtocolVersionRange, ENV_DAEMON_ID, ENV_SESSION_ID,
};
use serde::Serialize;

use super::config::Knobs;
use super::manifest::ArgsMode;
use super::runlog::{Outcome, Record, RunLog};
use super::store::Store;
use super::Error;
use crate::process::bounded::{
    configure_child, descriptor_scan_bound, drain_capped, spawn_capped_reader, supervise_child,
    Cut, Drain, Ended, Error as Lifecycle,
};

/// Invocation kind of `workflow run`, in the context and the run log.
const KIND_ACTION: &str = "action";

/// Output mode of a text-mode run, in the context.
const OUTPUT_TEXT: &str = "text";

/// Schema of the context JSON.
const CONTEXT_SCHEMA: u32 = 1;

/// How long an output pipe may stay open once the child's group is gone,
/// counting only time the reader is not blocked writing to this process's
/// stdout or stderr.
///
/// Group members are dead by then, so a pipe reaches end of file as soon as
/// its buffered bytes are read, in far less than this; only a descendant that
/// left the group with `setsid` can hold it open, and it is cut after this
/// much counted time whether it writes or not. Time spent writing to a slow
/// consumer is never counted, so the action's own output is delivered up to
/// its cap unless this process is signalled. Raising the value only delays
/// the end of a run that left such a descendant behind.
const OUTPUT_IDLE: Duration = Duration::from_secs(1);

/// The plugin id. This and the next six variables are what the runner sets
/// besides the bootstrap roots and the origin pair.
const ENV_PLUGIN_ID: &str = "POHUNEK_PLUGIN_ID";
/// The installed plugin version.
const ENV_PLUGIN_VERSION: &str = "POHUNEK_PLUGIN_VERSION";
/// The installed archive digest.
const ENV_PLUGIN_DIGEST: &str = "POHUNEK_PLUGIN_DIGEST";
/// The plugin's config directory.
const ENV_PLUGIN_CONFIG_DIR: &str = "POHUNEK_PLUGIN_CONFIG_DIR";
/// The plugin's state directory.
const ENV_PLUGIN_STATE_DIR: &str = "POHUNEK_PLUGIN_STATE_DIR";
/// The context JSON.
const ENV_PLUGIN_CONTEXT: &str = "POHUNEK_PLUGIN_CONTEXT";
/// The canonical path of the running `pohunek`.
const ENV_CLI: &str = "POHUNEK_CLI";

/// One `workflow run` request.
#[derive(Debug)]
pub(crate) struct Invocation<'a> {
    /// The plugin id.
    pub(crate) plugin: &'a PackageId,
    /// The requested action name, not yet checked against the manifest.
    pub(crate) action: &'a str,
    /// Extra arguments, passed verbatim when the action accepts them.
    pub(crate) args: &'a [OsString],
}

/// Everything a checked run needs; built before anything is started.
struct Prepared {
    record: PackageRecord,
    name: String,
    argv: Vec<String>,
    files: PathBuf,
    timeout_ms: u64,
    caps: [u64; 2],
    context: String,
    origin: Option<(OsString, OsString)>,
}

/// The run log and the facts every record of this run shares.
struct Run {
    log: RunLog,
    ts: String,
    id: String,
    started: Instant,
    arg_count: usize,
}

impl Run {
    /// Appends this run's record; a failed append is reported on stderr,
    /// since the run itself already happened or was refused.
    fn append(
        &mut self,
        record: &PackageRecord,
        name: Option<&str>,
        outcome: Outcome,
        status: Option<ExitStatus>,
        error: Option<&Error>,
        bytes: [u64; 2],
    ) {
        let entry = Record {
            ts: self.ts.clone(),
            run_id: &self.id,
            kind: KIND_ACTION,
            name,
            version: &record.identity().version,
            digest: record.digest(),
            outcome,
            error_code: error.map(Error::code),
            exit_code: status.and_then(|status| status.code()),
            signal: status.and_then(|status| status.signal()),
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            stdout_bytes: bytes[0],
            stderr_bytes: bytes[1],
            arg_count: self.arg_count,
        };
        if let Err(error) = self.log.append(&entry) {
            eprintln!("warning: {error}");
        }
    }
}

/// The context JSON a plugin reads from `POHUNEK_PLUGIN_CONTEXT`.
#[derive(Serialize)]
struct Context<'a> {
    schema: u32,
    plugin: ContextPlugin<'a>,
    invocation: ContextInvocation<'a>,
    output: &'static str,
    cli: ContextCli,
}

#[derive(Serialize)]
struct ContextPlugin<'a> {
    id: &'a PackageId,
    version: &'a PackageVersion,
    digest: &'a PackageDigest,
}

#[derive(Serialize)]
struct ContextInvocation<'a> {
    kind: &'static str,
    name: &'a str,
    run_id: &'a str,
}

#[derive(Serialize)]
struct ContextCli {
    version: &'static str,
    protocol: ProtocolVersionRange,
}

/// Runs `invocation` under `knobs` and returns the status of a child that
/// ended by itself, zero or not.
///
/// # Errors
///
/// Returns [`Error::PluginUnknown`] (nothing is logged), [`Error::RunLog`]
/// before anything starts when the run log cannot be opened, a refusal
/// ([`Error::PluginDisabled`], [`Error::RootInvalid`],
/// [`Error::ActionUnknown`], [`Error::ArgsRefused`],
/// [`Error::OriginIncomplete`], [`Error::TimeoutRefused`],
/// [`Error::ContextTooLarge`]), or a run that ended early
/// ([`Error::SpawnFailed`], [`Error::OutputLimit`], [`Error::Interrupted`],
/// [`Error::ActionTimeout`], [`Error::RunFailed`]).
///
/// # Panics
///
/// Panics when called outside a Tokio runtime, which registers the signal
/// handlers.
pub(crate) fn run_action(
    store: &Store,
    knobs: &Knobs,
    invocation: &Invocation<'_>,
) -> Result<ExitStatus, Error> {
    let id = invocation.plugin;
    let installed = store
        .installed(id)?
        .ok_or_else(|| Error::PluginUnknown { id: id.to_string() })?;
    let ts = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|error| Error::RunLog {
            detail: error.to_string(),
        })?;
    let mut run = Run {
        log: RunLog::open(&store.state_dir(id), knobs.logs)?,
        ts,
        id: ulid::Ulid::new().to_string(),
        started: Instant::now(),
        arg_count: invocation.args.len(),
    };
    match prepare(store, knobs, invocation, &installed, &run.id) {
        Ok(prepared) => execute(&mut run, store, knobs, invocation, &prepared),
        Err((error, name)) => {
            let name = name.as_deref();
            run.append(
                &installed,
                name,
                Outcome::Refused,
                None,
                Some(&error),
                [0; 2],
            );
            Err(error)
        }
    }
}

/// Applies every refusal rule; on failure returns the error and the action
/// name when it is one the verified manifest declares.
fn prepare(
    store: &Store,
    knobs: &Knobs,
    invocation: &Invocation<'_>,
    installed: &PackageRecord,
    run_id: &str,
) -> Result<Prepared, (Error, Option<String>)> {
    let id = invocation.plugin.to_string();
    if !installed.enabled() {
        return Err((Error::PluginDisabled { id }, None));
    }
    let (record, root, manifest) = store
        .inspect(invocation.plugin)
        .map_err(|error| (error, None))?;
    if !record.enabled() {
        return Err((Error::PluginDisabled { id }, None));
    }
    let Some((name, action)) = manifest.actions.get_key_value(invocation.action) else {
        let action = invocation.action.to_owned();
        return Err((Error::ActionUnknown { id, action }, None));
    };
    let refuse = |error: Error| (error, Some(name.clone()));
    if action.args == ArgsMode::None && !invocation.args.is_empty() {
        return Err(refuse(Error::ArgsRefused {
            id,
            action: name.clone(),
            count: invocation.args.len(),
        }));
    }
    let origin = origin_pair().map_err(refuse)?;
    let limits = &knobs.limits;
    let timeout_ms = match action.timeout_ms.map(u64::from) {
        Some(requested) if requested > limits.action_timeout_max_ms => {
            return Err(refuse(Error::TimeoutRefused {
                id,
                action: name.clone(),
                requested,
                max: limits.action_timeout_max_ms,
            }));
        }
        Some(requested) => requested,
        None => limits.action_timeout_default_ms,
    };
    let declared = action.max_output_bytes.map_or(u64::MAX, u64::from);
    let caps = [
        limits.max_stdout_bytes.min(declared),
        limits.max_stderr_bytes.min(declared),
    ];
    let context = serde_json::to_string(&Context {
        schema: CONTEXT_SCHEMA,
        plugin: ContextPlugin {
            id: &record.identity().id,
            version: &record.identity().version,
            digest: record.digest(),
        },
        invocation: ContextInvocation {
            kind: KIND_ACTION,
            name,
            run_id,
        },
        output: OUTPUT_TEXT,
        cli: ContextCli {
            version: env!("CARGO_PKG_VERSION"),
            protocol: protocol::CLIENT_PROTOCOL_VERSIONS,
        },
    })
    .expect("the context holds only strings and integers, which always serialize");
    if u64::try_from(context.len()).unwrap_or(u64::MAX) > limits.max_context_bytes {
        return Err(refuse(Error::ContextTooLarge {
            id,
            bytes: context.len(),
            max: limits.max_context_bytes,
        }));
    }
    let files = root.files_path().to_path_buf();
    Ok(Prepared {
        argv: action.resolved_argv(&files),
        record,
        name: name.clone(),
        files,
        timeout_ms,
        caps,
        context,
        origin,
    })
}

/// The origin pair to forward: both markers or neither.
fn origin_pair() -> Result<Option<(OsString, OsString)>, Error> {
    match (
        std::env::var_os(ENV_SESSION_ID),
        std::env::var_os(ENV_DAEMON_ID),
    ) {
        (Some(session), Some(daemon)) => Ok(Some((session, daemon))),
        (None, None) => Ok(None),
        _ => Err(Error::OriginIncomplete),
    }
}

/// Sets the child's whole environment: the bootstrap roots that are set,
/// the origin pair when present, and the plugin variables. Nothing else is
/// inherited.
fn build_environment(
    command: &mut Command,
    store: &Store,
    prepared: &Prepared,
    cli: &std::path::Path,
) {
    command.env_clear();
    for key in BOOTSTRAP_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    if let Some((session, daemon)) = &prepared.origin {
        command
            .env(ENV_SESSION_ID, session)
            .env(ENV_DAEMON_ID, daemon);
    }
    let identity = prepared.record.identity();
    command
        .env(ENV_PLUGIN_ID, identity.id.as_str())
        .env(ENV_PLUGIN_VERSION, identity.version.to_string())
        .env(ENV_PLUGIN_DIGEST, prepared.record.digest().as_str())
        .env(ENV_PLUGIN_CONFIG_DIR, store.config_dir(&identity.id))
        .env(ENV_PLUGIN_STATE_DIR, store.state_dir(&identity.id))
        .env(ENV_PLUGIN_CONTEXT, &prepared.context)
        .env(ENV_CLI, cli);
}

/// How a started run ended, as observed by the runner.
struct Observed {
    ended: Result<Ended, Lifecycle>,
    /// A terminating signal this process received during the run.
    received: Option<i32>,
    /// Whether stdout and stderr passed their caps.
    overflowed: [bool; 2],
    /// Whether reading an output stream failed.
    read_failed: bool,
}

/// Starts the checked run, waits for it, and records how it ended.
fn execute(
    run: &mut Run,
    store: &Store,
    knobs: &Knobs,
    invocation: &Invocation<'_>,
    prepared: &Prepared,
) -> Result<ExitStatus, Error> {
    let (mut child, mut interrupts) = match start(store, invocation, prepared) {
        Ok(started) => started,
        Err(cause) => {
            let error = Error::SpawnFailed {
                id: prepared.record.identity().id.to_string(),
                action: prepared.name.clone(),
                cause,
            };
            let name = Some(prepared.name.as_str());
            let outcome = Outcome::SpawnFailed;
            run.append(&prepared.record, name, outcome, None, Some(&error), [0; 2]);
            return Err(error);
        }
    };
    let deadline = Instant::now() + Duration::from_millis(prepared.timeout_ms);
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        unreachable!("both output streams were configured as pipes");
    };
    let (stdout_tally, stdout_done) = spawn_capped_reader(stdout, io::stdout(), prepared.caps[0]);
    let (stderr_tally, stderr_done) = spawn_capped_reader(stderr, io::stderr(), prepared.caps[1]);
    let mut received = None;
    let ended = supervise_child(
        &mut child,
        deadline,
        Duration::from_millis(knobs.limits.kill_grace_ms),
        &mut || {
            if stdout_tally.overflowed() || stderr_tally.overflowed() {
                return true;
            }
            received = received.or_else(|| interrupts.poll());
            received.is_some()
        },
    );
    // A signal while the output drains ends the drain and makes the run
    // `interrupted`, as it does while the child runs.
    let mut stop = || {
        received = received.or_else(|| interrupts.poll());
        received.is_some()
    };
    let drained = [
        (
            "stdout",
            drain_capped(&stdout_done, &stdout_tally, OUTPUT_IDLE, &mut stop)
                .map(|drain| matches!(drain, Drain::Idle)),
        ),
        (
            "stderr",
            drain_capped(&stderr_done, &stderr_tally, OUTPUT_IDLE, &mut stop)
                .map(|drain| matches!(drain, Drain::Idle)),
        ),
    ];
    let mut read_failed = false;
    for (stream, drained) in drained {
        match drained {
            Ok(false) => {}
            // An escaped descendant holds the pipe open: the stream is cut,
            // the run keeps its outcome, and the cut is reported without any
            // of the stream's bytes.
            Ok(true) => eprintln!(
                "warning: the {stream} of action {} of workflow plugin {} was cut: a process \
                 that left the action's process group still holds it open",
                prepared.name,
                prepared.record.identity().id
            ),
            Err(_lost) => read_failed = true,
        }
    }
    let observed = Observed {
        ended,
        received,
        overflowed: [stdout_tally.overflowed(), stderr_tally.overflowed()],
        read_failed,
    };
    let (outcome, result) = classify(prepared, &observed);
    let status = observed.ended.as_ref().ok().map(|ended| ended.status);
    let bytes = [stdout_tally.delivered(), stderr_tally.delivered()];
    let name = Some(prepared.name.as_str());
    let error = result.as_ref().err();
    run.append(&prepared.record, name, outcome, status, error, bytes);
    result
}

/// Spawns the action with its streams piped; `Err` is the cause of a spawn
/// failure.
fn start(
    store: &Store,
    invocation: &Invocation<'_>,
    prepared: &Prepared,
) -> Result<(Child, Interrupts), io::ErrorKind> {
    let cli = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|error| error.kind())?;
    let (program, fixed) = prepared
        .argv
        .split_first()
        .expect("a validated manifest argv has a program");
    // The child closes the standard library's exec-error pipe, so an `exec`
    // failure would only show as a non-zero exit; a missing or
    // non-executable program is reported as a spawn failure instead.
    executable_file(program)?;
    let interrupts = Interrupts::new().map_err(|error| error.kind())?;
    let mut command = Command::new(program);
    command
        .args(fixed)
        .args(invocation.args)
        .current_dir(&prepared.files)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    build_environment(&mut command, store, prepared, &cli);
    configure_child(&mut command, descriptor_scan_bound());
    let child = command.spawn().map_err(|error| error.kind())?;
    Ok((child, interrupts))
}

/// Decides the outcome of a started run, in the documented precedence:
/// output limit, interruption, deadline, observation failure, exit status.
fn classify(prepared: &Prepared, observed: &Observed) -> (Outcome, Result<ExitStatus, Error>) {
    let id = prepared.record.identity().id.to_string();
    let action = prepared.name.clone();
    if let Some(index) = observed
        .overflowed
        .iter()
        .position(|overflowed| *overflowed)
    {
        let stream = ["stdout", "stderr"][index];
        let limit = prepared.caps[index];
        return (
            Outcome::OutputLimit,
            Err(Error::OutputLimit {
                id,
                action,
                stream,
                limit,
            }),
        );
    }
    if let Some(signal) = observed.received {
        return (
            Outcome::Interrupted,
            Err(Error::Interrupted { id, action, signal }),
        );
    }
    match &observed.ended {
        Ok(ended) if ended.cut == Some(Cut::Deadline) => (
            Outcome::Timeout,
            Err(Error::ActionTimeout {
                id,
                action,
                timeout_ms: prepared.timeout_ms,
            }),
        ),
        Ok(ended) if !observed.read_failed => {
            let outcome = if ended.status.success() {
                Outcome::Ok
            } else {
                Outcome::ExitNonzero
            };
            (outcome, Ok(ended.status))
        }
        _ => (Outcome::Failed, Err(Error::RunFailed { id, action })),
    }
}

/// Checks that `program` is a regular file with an executable bit.
fn executable_file(program: &str) -> Result<(), io::ErrorKind> {
    use std::os::unix::fs::PermissionsExt as _;

    /// Any of the owner, group and other execute bits.
    const EXECUTE_BITS: u32 = 0o111;

    let metadata = std::fs::metadata(program).map_err(|error| error.kind())?;
    if metadata.is_file() && metadata.permissions().mode() & EXECUTE_BITS != 0 {
        Ok(())
    } else {
        Err(io::ErrorKind::PermissionDenied)
    }
}

/// The terminating signals this process handles while an action runs, so
/// the action's group is stopped instead of being left behind: the child is
/// in a group of its own, so a terminal's `Ctrl-C` reaches only this process.
struct Interrupts {
    streams: Vec<(i32, tokio::signal::unix::Signal)>,
}

impl Interrupts {
    /// Registers `SIGINT`, `SIGTERM` and `SIGHUP` with the CLI's runtime.
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{signal, SignalKind};

        let mut streams = Vec::new();
        for (kind, number) in [
            (SignalKind::interrupt(), libc::SIGINT),
            (SignalKind::terminate(), libc::SIGTERM),
            (SignalKind::hangup(), libc::SIGHUP),
        ] {
            streams.push((number, signal(kind)?));
        }
        Ok(Self { streams })
    }

    /// A signal received since the last poll, without waiting.
    fn poll(&mut self) -> Option<i32> {
        self.streams
            .iter_mut()
            .find_map(|(number, stream)| stream.recv().now_or_never().flatten().map(|()| *number))
    }
}
