//! Runs one declared action of an installed, enabled workflow plugin, and the
//! supervised runs of its jobs and services.
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
//! signalled. In JSON mode stdout is captured instead, up to the same cap,
//! and the run succeeds only when the child exits zero and the capture is
//! exactly one JSON document, which is handed back byte for byte. Once the group is gone, a pipe that a descendant outside the
//! group keeps open is cut after [`OUTPUT_IDLE`] of time not spent writing to
//! that consumer, whether the descendant writes or not, with a content-free
//! warning; in JSON mode a cut stdout fails the run instead. A run ends in
//! one outcome, decided in this order: an output stream that passed its cap
//! (`output_limit`), a terminating signal this process received
//! (`interrupted`), the deadline (`timeout`), a failure to observe the child
//! (`failed`), then the child's own status; in JSON mode a zero status is
//! followed by a cut stdout (`output_incomplete`), then a capture that is not
//! exactly one JSON document (`output_invalid`). Every outcome
//! but a normal exit stops the group with `SIGTERM`, the owner's grace (which
//! ends early when the direct child exits), then `SIGKILL`; after a normal
//! exit the rest of the group is killed. Each run of
//! an installed plugin, refused or not, appends one record to its run log,
//! except a run refused before the log could be opened: invalid owner knobs
//! (they set the log's retention) or a log that cannot be opened.
//!
//! A supervised run ([`run_supervised`]) of a job or service follows the same
//! rules with three differences: it never forwards the session origin pair,
//! whatever this process's environment holds, because a service manager's
//! environment is not a session's; `POHUNEK_CLI` is the installed
//! `<prefix>/bin/pohunek` its unit runs; and both output streams are
//! discarded without a cap and only counted. A service has no deadline.

// Rust guideline compliant 2026-10-13

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use package::registry::PackageRecord;
use pohunek_platform::supervisor::{PluginUnitKey, PluginUnitKind, BOOTSTRAP_ENV_ALLOWLIST};
use protocol::{
    PackageDigest, PackageId, PackageVersion, ProtocolVersionRange, ENV_DAEMON_ID, ENV_SESSION_ID,
};
use serde::Serialize;
use serde_json::value::RawValue;

use super::config::Knobs;
use super::manifest::ArgsMode;
use super::runlog::{Outcome, Record, RunLog};
use super::store::Store;
use super::units::check_job;
use super::Error;
use crate::process::bounded::{
    configure_child, descriptor_scan_bound, drain_capped, spawn_capped_reader, supervise_child,
    Cut, Drain, Ended, Error as Lifecycle,
};

/// Invocation kind of `workflow run`, in the context and the run log.
const KIND_ACTION: &str = "action";

/// Invocation kind of a supervised job run.
const KIND_JOB: &str = "job";

/// Invocation kind of a supervised service run.
const KIND_SERVICE: &str = "service";

/// How far ahead a service run's deadline lies: a service has none and runs
/// until it exits or is stopped.
///
/// A century is beyond any host's uptime and still representable as an
/// [`Instant`] on every supported platform, whose monotonic clocks count
/// from boot.
const NO_DEADLINE: Duration = Duration::from_hours(100 * 365 * 24);

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

/// How a run delivers the child's stdout and stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    /// Stdout streams to this process's stdout.
    Text,
    /// Stdout is captured and must be exactly one JSON document.
    Json,
    /// Both streams are discarded without a cap and only counted; the mode
    /// of supervised runs.
    Discard,
}

impl Output {
    /// The mode's name in the context's `output` field; only actions have
    /// one.
    fn name(self) -> Option<&'static str> {
        match self {
            Self::Text => Some("text"),
            Self::Json => Some("json"),
            Self::Discard => None,
        }
    }
}

/// The run log name of a unit kind.
pub(crate) fn kind_name(kind: PluginUnitKind) -> &'static str {
    match kind {
        PluginUnitKind::Job => KIND_JOB,
        PluginUnitKind::Service => KIND_SERVICE,
    }
}

/// How a run that was not refused or stopped ended.
#[derive(Debug)]
pub(crate) enum Completed {
    /// A text-mode run whose child ended by itself, zero or not.
    Exited(ExitStatus),
    /// A JSON-mode run whose child exited zero with exactly one JSON
    /// document on stdout, without the whitespace around it.
    Document(Box<RawValue>),
}

/// One supervised run of a declared job or service.
#[derive(Debug)]
pub(crate) struct Supervised<'a> {
    /// Whether the unit is a job or a service.
    pub(crate) kind: PluginUnitKind,
    /// The declared name, valid under the manifest's name grammar.
    pub(crate) name: &'a str,
    /// The archive digest the unit names; another selected digest refuses
    /// the run.
    pub(crate) digest: &'a PackageDigest,
    /// The installed `<prefix>/bin/pohunek`, given to the child as
    /// `POHUNEK_CLI`.
    pub(crate) cli: &'a Path,
    /// Whether the trampoline ignored an inherited `XDG_RUNTIME_DIR` to find
    /// the installation; the child then does not get it either, so its
    /// `POHUNEK_CLI` reaches the same installation.
    pub(crate) runtime_dir_ignored: bool,
}

/// One `workflow run` request.
#[derive(Debug)]
pub(crate) struct Invocation<'a> {
    /// The plugin id.
    pub(crate) plugin: &'a PackageId,
    /// The requested action name, not yet checked against the manifest.
    pub(crate) action: &'a str,
    /// Extra arguments, passed verbatim when the action accepts them.
    pub(crate) args: &'a [OsString],
    /// How the action's stdout is delivered.
    pub(crate) output: Output,
}

/// Everything a checked run needs; built before anything is started.
struct Prepared {
    record: PackageRecord,
    name: String,
    argv: Vec<String>,
    files: PathBuf,
    /// The deadline; `None` for a service, which has none.
    timeout_ms: Option<u64>,
    caps: [u64; 2],
    context: String,
    origin: Option<(OsString, OsString)>,
    /// `POHUNEK_CLI`; `None` names the running executable.
    cli: Option<PathBuf>,
    /// Whether `XDG_RUNTIME_DIR` is left out of the child's environment.
    omit_runtime_dir: bool,
}

/// The run log and the facts every record of this run shares.
#[derive(Debug)]
pub(crate) struct Run {
    log: RunLog,
    ts: String,
    id: String,
    kind: &'static str,
    started: Instant,
    arg_count: usize,
    /// The restart delay a service run waited, for its record.
    pub(crate) backoff_ms: Option<u64>,
    /// How long the started child ran, from its spawn until it ended; `None`
    /// when no child was started.
    pub(crate) child_lifetime: Option<Duration>,
}

impl Run {
    /// Opens the run log of plugin `id` for one run of `kind` and stamps the
    /// run's start.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RunLog`] when the clock cannot be formatted or the
    /// run log cannot be opened.
    pub(crate) fn open(
        store: &Store,
        knobs: &Knobs,
        id: &PackageId,
        kind: &'static str,
        arg_count: usize,
    ) -> Result<Self, Error> {
        let ts = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| Error::RunLog {
                detail: error.to_string(),
            })?;
        Ok(Self {
            log: RunLog::open(&store.state_dir(id), knobs.logs)?,
            ts,
            id: ulid::Ulid::new().to_string(),
            kind,
            started: Instant::now(),
            arg_count,
            backoff_ms: None,
            child_lifetime: None,
        })
    }

    /// Starts the run's clock and start time over, for a run that waited
    /// before it started.
    ///
    /// # Errors
    ///
    /// Returns [`Error::RunLog`] when the clock cannot be formatted.
    pub(crate) fn restart_clock(&mut self) -> Result<(), Error> {
        self.ts = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| Error::RunLog {
                detail: error.to_string(),
            })?;
        self.started = Instant::now();
        Ok(())
    }

    /// Appends this run's record; a failed append is reported on stderr,
    /// since the run itself already happened or was refused.
    pub(crate) fn append(
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
            kind: self.kind,
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
            backoff_ms: self.backoff_ms,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<&'static str>,
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

/// Runs `invocation` under `knobs` and returns how a run that was not
/// refused or stopped ended: in text mode the status of a child that ended by
/// itself, zero or not; in JSON mode the document of a child that exited
/// zero.
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
/// [`Error::ActionTimeout`], [`Error::RunFailed`]); in JSON mode also a
/// child that exited non-zero ([`Error::ActionFailed`]), a stdout cut before
/// it ended ([`Error::OutputIncomplete`]) or a capture that is not exactly
/// one JSON document ([`Error::OutputInvalid`]).
///
/// # Panics
///
/// Panics when called outside a Tokio runtime, which registers the signal
/// handlers.
pub(crate) fn run_action(
    store: &Store,
    knobs: &Knobs,
    invocation: &Invocation<'_>,
) -> Result<Completed, Error> {
    let id = invocation.plugin;
    let installed = store
        .installed(id)?
        .ok_or_else(|| Error::PluginUnknown { id: id.to_string() })?;
    let mut run = Run::open(store, knobs, id, KIND_ACTION, invocation.args.len())?;
    match prepare(store, knobs, invocation, &installed, &run.id) {
        Ok(prepared) => match Interrupts::new() {
            Ok(mut interrupts) => execute(
                &mut run,
                store,
                knobs,
                invocation,
                &prepared,
                &mut interrupts,
            ),
            Err(error) => Err(spawn_failed(&mut run, &prepared, error.kind())),
        },
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
    let timeout_ms = Some(timeout_ms);
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
        output: invocation.output.name(),
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
        cli: None,
        omit_runtime_dir: false,
    })
}

/// Runs the declared job or service `unit` of the plugin `run` logs for,
/// without a session origin and with both output streams discarded, and
/// returns the status of a child that ended by itself, zero or not.
///
/// `interrupts` must be registered before the caller started waiting, so a
/// stop request that arrives at any point ends the run.
///
/// # Errors
///
/// Returns a refusal ([`Error::PluginUnknown`], [`Error::UnitStale`],
/// [`Error::PluginDisabled`], [`Error::RootInvalid`],
/// [`Error::UnitUnknown`], [`Error::UnitRefused`],
/// [`Error::ContextTooLarge`]) or a run that ended early
/// ([`Error::SpawnFailed`], [`Error::Interrupted`], [`Error::ActionTimeout`],
/// [`Error::RunFailed`]); every one is recorded in the run log.
pub(crate) fn run_supervised(
    run: &mut Run,
    store: &Store,
    knobs: &Knobs,
    plugin: &PackageId,
    unit: &Supervised<'_>,
    interrupts: &mut Interrupts,
) -> Result<Completed, Error> {
    let invocation = Invocation {
        plugin,
        action: unit.name,
        args: &[],
        output: Output::Discard,
    };
    let installed = store
        .installed(plugin)?
        .ok_or_else(|| Error::PluginUnknown {
            id: plugin.to_string(),
        })?;
    match prepare_unit(store, knobs, plugin, unit, &installed, &run.id) {
        Ok(prepared) => {
            // Verification takes time, and a stop request that arrived during it
            // must end the run before its child is started.
            if let Some(signal) = interrupts.poll() {
                let error = Error::Interrupted {
                    id: plugin.to_string(),
                    action: prepared.name.clone(),
                    signal,
                };
                run.append(
                    &installed,
                    Some(prepared.name.as_str()),
                    Outcome::Interrupted,
                    None,
                    Some(&error),
                    [0; 2],
                );
                return Err(error);
            }
            execute(run, store, knobs, &invocation, &prepared, interrupts)
        }
        Err((error, name)) => {
            // A refusal before the manifest verified still names the unit
            // when the requested name fits the unit name grammar.
            let requested = PluginUnitKey::new(plugin.as_str(), unit.kind, unit.name)
                .is_ok()
                .then_some(unit.name);
            run.append(
                &installed,
                name.as_deref().or(requested),
                Outcome::Refused,
                None,
                Some(&error),
                [0; 2],
            );
            Err(error)
        }
    }
}

/// Applies every refusal rule of a supervised run; on failure returns the
/// error and the unit name when the verified manifest declares it.
fn prepare_unit(
    store: &Store,
    knobs: &Knobs,
    plugin: &PackageId,
    unit: &Supervised<'_>,
    installed: &PackageRecord,
    run_id: &str,
) -> Result<Prepared, (Error, Option<String>)> {
    let id = plugin.to_string();
    if installed.digest() != unit.digest {
        return Err((Error::UnitStale { id }, None));
    }
    if !installed.enabled() {
        return Err((Error::PluginDisabled { id }, None));
    }
    let (record, root, manifest) = store.inspect(plugin).map_err(|error| (error, None))?;
    if !record.enabled() || record.digest() != unit.digest {
        return Err((Error::PluginDisabled { id }, None));
    }
    let kind = kind_name(unit.kind);
    let unknown = || {
        (
            Error::UnitUnknown {
                id: id.clone(),
                unit: format!("{kind} {:?}", unit.name),
            },
            None,
        )
    };
    let (name, argv, timeout_ms) = match unit.kind {
        PluginUnitKind::Job => {
            let (name, job) = manifest.jobs.get_key_value(unit.name).ok_or_else(unknown)?;
            // The owner's bounds may have changed since the unit was installed.
            check_job(plugin, name, job, knobs).map_err(|error| (error, Some(name.clone())))?;
            let timeout_ms = job
                .timeout_ms
                .map_or(knobs.jobs.timeout_default_ms, u64::from);
            (name, &job.argv, Some(timeout_ms))
        }
        PluginUnitKind::Service => {
            let (name, service) = manifest
                .services
                .get_key_value(unit.name)
                .ok_or_else(unknown)?;
            (name, &service.argv, None)
        }
    };
    let context = serde_json::to_string(&Context {
        schema: CONTEXT_SCHEMA,
        plugin: ContextPlugin {
            id: &record.identity().id,
            version: &record.identity().version,
            digest: record.digest(),
        },
        invocation: ContextInvocation { kind, name, run_id },
        output: None,
        cli: ContextCli {
            version: env!("CARGO_PKG_VERSION"),
            protocol: protocol::CLIENT_PROTOCOL_VERSIONS,
        },
    })
    .expect("the context holds only strings and integers, which always serialize");
    let max = knobs.limits.max_context_bytes;
    if u64::try_from(context.len()).unwrap_or(u64::MAX) > max {
        return Err((
            Error::ContextTooLarge {
                id,
                bytes: context.len(),
                max,
            },
            Some(name.clone()),
        ));
    }
    let files = root.files_path().to_path_buf();
    Ok(Prepared {
        argv: super::manifest::resolve_argv(argv, &files),
        record,
        name: name.clone(),
        files,
        timeout_ms,
        // Discarded streams cost nothing to keep reading, and a cap would
        // eventually stop a long-running service that logs to stdout.
        caps: [u64::MAX; 2],
        context,
        // A service manager's environment is not a session's, so its origin
        // markers are never forwarded.
        origin: None,
        cli: Some(unit.cli.to_path_buf()),
        omit_runtime_dir: unit.runtime_dir_ignored,
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
        if prepared.omit_runtime_dir && key == pohunek_paths::XDG_RUNTIME_DIR {
            continue;
        }
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
    /// The run's output mode.
    output: Output,
    /// The captured stdout of a JSON-mode run; `None` in text mode.
    captured: Option<Captured>,
}

/// The captured stdout of a JSON-mode run.
enum Captured {
    /// The stream ended; every byte up to the cap.
    Whole(Vec<u8>),
    /// The stream did not end: a process outside the group held it open, or
    /// the drain was stopped. Its bytes are never used.
    Cut,
}

/// Where a capped reader puts one of the child's streams.
enum Sink {
    /// This process's stdout, in text mode.
    Stream(io::Stdout),
    /// This process's stderr, in text and JSON mode.
    Errors(io::Stderr),
    /// Memory, in JSON mode; the capped reader bounds it to the stdout cap.
    Capture(Vec<u8>),
    /// Nowhere, in discard mode.
    Discard(io::Sink),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stream(stdout) => stdout.write(buf),
            Self::Errors(stderr) => stderr.write(buf),
            Self::Capture(buffer) => buffer.write(buf),
            Self::Discard(sink) => sink.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stream(stdout) => stdout.flush(),
            Self::Errors(stderr) => stderr.flush(),
            Self::Capture(_) | Self::Discard(_) => Ok(()),
        }
    }
}

/// Records and returns the spawn failure of a checked run.
fn spawn_failed(run: &mut Run, prepared: &Prepared, cause: io::ErrorKind) -> Error {
    let error = Error::SpawnFailed {
        id: prepared.record.identity().id.to_string(),
        action: prepared.name.clone(),
        cause,
    };
    let name = Some(prepared.name.as_str());
    let outcome = Outcome::SpawnFailed;
    run.append(&prepared.record, name, outcome, None, Some(&error), [0; 2]);
    error
}

/// Starts the checked run, waits for it, and records how it ended.
fn execute(
    run: &mut Run,
    store: &Store,
    knobs: &Knobs,
    invocation: &Invocation<'_>,
    prepared: &Prepared,
    interrupts: &mut Interrupts,
) -> Result<Completed, Error> {
    let mut child = match start(store, invocation, prepared) {
        Ok(child) => child,
        Err(cause) => return Err(spawn_failed(run, prepared, cause)),
    };
    let now = Instant::now();
    let deadline = prepared
        .timeout_ms
        .map_or(now.checked_add(NO_DEADLINE), |timeout_ms| {
            now.checked_add(Duration::from_millis(timeout_ms))
        })
        .expect("a deadline at most a century ahead is representable");
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        unreachable!("both output streams were configured as pipes");
    };
    let (sink, errors) = match invocation.output {
        Output::Text => (Sink::Stream(io::stdout()), Sink::Errors(io::stderr())),
        Output::Json => (Sink::Capture(Vec::new()), Sink::Errors(io::stderr())),
        Output::Discard => (Sink::Discard(io::sink()), Sink::Discard(io::sink())),
    };
    let (stdout_tally, stdout_done) = spawn_capped_reader(stdout, sink, prepared.caps[0]);
    let (stderr_tally, stderr_done) = spawn_capped_reader(stderr, errors, prepared.caps[1]);
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
    run.child_lifetime = Some(now.elapsed());
    // A signal while the output drains ends the drain and makes the run
    // `interrupted`, as it does while the child runs.
    let mut stop = || {
        received = received.or_else(|| interrupts.poll());
        received.is_some()
    };
    let stdout_drain = drain_capped(&stdout_done, &stdout_tally, OUTPUT_IDLE, &mut stop);
    let stderr_drain = drain_capped(&stderr_done, &stderr_tally, OUTPUT_IDLE, &mut stop);
    let mut read_failed = false;
    let mut captured = None;
    match stdout_drain {
        Ok(Drain::Ended(Sink::Capture(bytes))) => captured = Some(Captured::Whole(bytes)),
        // A cut capture is never parsed: the bytes a process outside the
        // group may still write would belong to the same document.
        Ok(Drain::Idle | Drain::Stopped) if invocation.output == Output::Json => {
            captured = Some(Captured::Cut);
        }
        Ok(Drain::Idle) => cut_warning("stdout", prepared),
        Ok(Drain::Ended(_) | Drain::Stopped) => {}
        Err(_lost) => read_failed = true,
    }
    match stderr_drain {
        Ok(Drain::Ended(_) | Drain::Stopped) => {}
        Ok(Drain::Idle) => cut_warning("stderr", prepared),
        Err(_lost) => read_failed = true,
    }
    let observed = Observed {
        ended,
        received,
        overflowed: [stdout_tally.overflowed(), stderr_tally.overflowed()],
        read_failed,
        output: invocation.output,
        captured,
    };
    let status = observed.ended.as_ref().ok().map(|ended| ended.status);
    let (outcome, result) = classify(prepared, observed);
    let bytes = [stdout_tally.delivered(), stderr_tally.delivered()];
    let name = Some(prepared.name.as_str());
    let error = result.as_ref().err();
    run.append(&prepared.record, name, outcome, status, error, bytes);
    result
}

/// Reports that an escaped descendant holds an output pipe open, so the
/// stream is cut; the run keeps its outcome and none of the stream's bytes
/// are shown.
fn cut_warning(stream: &str, prepared: &Prepared) {
    eprintln!(
        "warning: the {stream} of action {} of workflow plugin {} was cut: a process \
         that left the action's process group still holds it open",
        prepared.name,
        prepared.record.identity().id
    );
}

/// Spawns the action with its streams piped; `Err` is the cause of a spawn
/// failure.
fn start(
    store: &Store,
    invocation: &Invocation<'_>,
    prepared: &Prepared,
) -> Result<Child, io::ErrorKind> {
    let cli = match &prepared.cli {
        Some(cli) => cli.clone(),
        None => std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| error.kind())?,
    };
    let (program, fixed) = prepared
        .argv
        .split_first()
        .expect("a validated manifest argv has a program");
    // The child closes the standard library's exec-error pipe, so an `exec`
    // failure would only show as a non-zero exit; a missing or
    // non-executable program is reported as a spawn failure instead.
    executable_file(program)?;
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
    command.spawn().map_err(|error| error.kind())
}

/// Decides the outcome of a started run, in the documented precedence:
/// output limit, interruption, deadline, observation failure, exit status;
/// in JSON mode a zero exit status is then judged by its capture.
fn classify(prepared: &Prepared, observed: Observed) -> (Outcome, Result<Completed, Error>) {
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
    let status = match &observed.ended {
        Ok(ended) if ended.cut == Some(Cut::Deadline) => {
            return (
                Outcome::Timeout,
                Err(Error::ActionTimeout {
                    id,
                    action,
                    timeout_ms: prepared.timeout_ms.unwrap_or(u64::MAX),
                }),
            );
        }
        Ok(ended) if !observed.read_failed => ended.status,
        _ => return (Outcome::Failed, Err(Error::RunFailed { id, action })),
    };
    let captured = match (observed.output, observed.captured) {
        (Output::Text | Output::Discard, _) => {
            let outcome = if status.success() {
                Outcome::Ok
            } else {
                Outcome::ExitNonzero
            };
            return (outcome, Ok(Completed::Exited(status)));
        }
        (Output::Json, Some(captured)) => captured,
        // A JSON-mode run without a capture was not observed in full.
        (Output::Json, None) => return (Outcome::Failed, Err(Error::RunFailed { id, action })),
    };
    if !status.success() {
        return (
            Outcome::ExitNonzero,
            Err(Error::ActionFailed { id, action, status }),
        );
    }
    let bytes = match captured {
        Captured::Whole(bytes) => bytes,
        Captured::Cut => {
            return (
                Outcome::OutputIncomplete,
                Err(Error::OutputIncomplete { id, action }),
            );
        }
    };
    match parse_document(&bytes) {
        Ok(document) => (Outcome::Ok, Ok(Completed::Document(document))),
        Err(reason) => (
            Outcome::OutputInvalid,
            Err(Error::OutputInvalid { id, action, reason }),
        ),
    }
}

/// Deepest array and object nesting a `--json` document may have.
///
/// `serde_json`'s default parser, which pohunek and most consumers of the
/// envelope use, refuses more than 127 nested levels, and the envelope puts
/// the document one level down, so a deeper document would make the whole
/// envelope unreadable to them. The `RawValue` parse that keeps the document's
/// bytes has no depth limit of its own, so [`parse_document`] enforces this
/// one. Raising it breaks those consumers; lowering it refuses documents they
/// can read.
const MAX_DOCUMENT_DEPTH: usize = 126;

/// Parses captured stdout as exactly one JSON document, with JSON whitespace
/// allowed around it, nested at most [`MAX_DOCUMENT_DEPTH`] levels; `Err` is
/// a content-free reason.
///
/// The document is kept as its own bytes, so key order, number spelling and
/// inner whitespace reach the caller unchanged.
fn parse_document(bytes: &[u8]) -> Result<Box<RawValue>, &'static str> {
    let error = match serde_json::from_slice::<Box<RawValue>>(bytes) {
        Ok(document) if nesting_depth(document.get()) > MAX_DOCUMENT_DEPTH => {
            return Err("it is not JSON");
        }
        Ok(document) => return Ok(document),
        Err(error) => error,
    };
    if bytes
        .iter()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
    {
        return Err("it is empty");
    }
    if error.is_eof() {
        return Err("it ends inside its JSON document");
    }
    let mut documents = serde_json::Deserializer::from_slice(bytes).into_iter::<Box<RawValue>>();
    if matches!(documents.next(), Some(Ok(_))) {
        Err("it has more data after its first JSON document")
    } else {
        Err("it is not JSON")
    }
}

/// The deepest array and object nesting of `document`, which must be valid
/// JSON; brackets inside strings do not count.
fn nesting_depth(document: &str) -> usize {
    let (mut depth, mut deepest) = (0_usize, 0_usize);
    let (mut in_string, mut escaped) = (false, false);
    for byte in document.bytes() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
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
///
/// Registration replaces the default action of the signals for the rest of
/// the process's life, and a stream sees only signals that arrive after it
/// was created, so a caller that waits before the run registers first.
#[derive(Debug)]
pub(crate) struct Interrupts {
    streams: Vec<(i32, tokio::signal::unix::Signal)>,
}

impl Interrupts {
    /// Registers `SIGINT`, `SIGTERM` and `SIGHUP` with the CLI's runtime.
    ///
    /// # Errors
    ///
    /// Returns the error of a registration the runtime refused.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime.
    pub(crate) fn new() -> io::Result<Self> {
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
    pub(crate) fn poll(&mut self) -> Option<i32> {
        self.streams
            .iter_mut()
            .find_map(|(number, stream)| stream.recv().now_or_never().flatten().map(|()| *number))
    }
}
