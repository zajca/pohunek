//! Defines target-neutral durable service supervision.
//!
//! A [`Supervisor`] registers one explicit [`JobDefinition`] per worker
//! generation with the native service manager (systemd transient units on
//! Linux, launchd jobs on macOS), then discovers, inspects, and retires those
//! jobs. Observations are evidence, never readiness: callers still prove a
//! worker through its private authenticated socket.
//!
//! Labels and unit names are derived by [`namespace`], which binds every job to
//! one installation so foreign or other-namespace jobs are never touched.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use crate::process::ProcessIdentity;

// The launchd module is also built for tests on every host so its
// target-neutral `launchctl` runner and plist rendering are covered by the
// ordinary test gate; the backends inside it exist only on macOS.
#[cfg(any(target_os = "macos", test))]
pub mod launchd;
pub mod namespace;
#[cfg(target_os = "linux")]
pub mod systemd;

#[doc(inline)]
pub use namespace::{Namespace, WorkerKey};

// Rust guideline compliant 2026-09-24

/// Longest accepted service identifier.
///
/// Fits a session ID, a generation, and separators with room to spare while
/// keeping derived launchd labels and systemd unit names well below the
/// 255-byte file-name limit that both managers inherit.
const MAX_SERVICE_ID_BYTES: usize = 128;

/// Environment keys a job definition may carry.
///
/// Job definitions hold only the non-secret bootstrap contract: the XDG roots
/// and home the worker needs to resolve its private paths. The agent's session
/// environment travels separately in memory through the worker protocol.
pub const BOOTSTRAP_ENV_ALLOWLIST: [&str; 6] = [
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "HOME",
];

/// Most arguments one job may pass to its executable.
///
/// Workers and the daemon use fewer than ten; the bound keeps a corrupted
/// definition from producing an oversized plist or D-Bus message.
pub const MAX_JOB_ARGUMENTS: usize = 64;

/// Longest accepted argument, path, or environment value in bytes.
///
/// Matches `PATH_MAX` on Linux and exceeds Darwin's 1024-byte `PATH_MAX`, so
/// every real path fits while hostile values stay bounded.
pub const MAX_JOB_VALUE_BYTES: usize = 4_096;

/// Longest accepted start or exit timeout.
///
/// Ten minutes is far above the configured 45 s start and 30 s stop budgets;
/// longer values would only hide a wedged job from reconciliation.
pub const MAX_JOB_TIMEOUT: Duration = Duration::from_secs(600);

/// Smallest accepted open-file limit.
///
/// A worker holds a PTY pair, its sockets, journal, and log files; below 256
/// descriptors an agent child that inherits the limit fails unpredictably.
pub const MIN_OPEN_FILES: u64 = 256;

/// Largest accepted open-file limit.
///
/// Mirrors the common kernel ceiling (`fs.nr_open` on Linux); larger requests
/// are rejected by the native managers anyway.
pub const MAX_OPEN_FILES: u64 = 1_048_576;

/// Validated logical identifier in one supervisor-owned namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ServiceId(String);

impl ServiceId {
    /// Parses an identifier that cannot escape a backend namespace.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidServiceId`] for empty, oversized, or unsafe
    /// identifiers.
    pub fn parse(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        if value.is_empty()
            || matches!(value.as_str(), "." | "..")
            || value.len() > MAX_SERVICE_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Error::InvalidServiceId(value));
        }
        Ok(Self(value))
    }

    /// Returns the backend-independent identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ServiceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Portable lifecycle state reported by a native supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// Activation was accepted but the service is not ready yet.
    Starting,
    /// The service is active.
    Running,
    /// Deactivation is in progress.
    Stopping,
    /// The service is loaded but inactive.
    Stopped,
    /// The supervisor recorded a failed activation or process.
    Failed,
    /// The native backend returned a state without a portable equivalent.
    Unknown,
}

/// How the native manager treats a job whose process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Never restart; used for session workers, which own exactly one PTY.
    Never,
    /// Restart after an unsuccessful exit, at most once per `throttle`.
    OnFailure {
        /// Minimum delay between restarts.
        throttle: Duration,
    },
}

/// Files receiving a job's standard output and standard error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLogs {
    /// Absolute standard-output file.
    pub stdout: PathBuf,
    /// Absolute standard-error file.
    pub stderr: PathBuf,
}

/// Unvalidated input for [`JobDefinition::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// Absolute executable; no shell is ever involved.
    pub executable: PathBuf,
    /// Arguments passed after the executable.
    pub arguments: Vec<String>,
    /// Non-secret bootstrap environment, limited to [`BOOTSTRAP_ENV_ALLOWLIST`].
    pub environment: BTreeMap<String, String>,
    /// Absolute working directory.
    pub working_directory: PathBuf,
    /// Log files where the backend supports them (launchd); systemd keeps the
    /// journal.
    pub logs: Option<JobLogs>,
    /// Bound on activation before the manager marks the job failed.
    pub start_timeout: Duration,
    /// Grace between `SIGTERM` and `SIGKILL` when the job is stopped.
    pub exit_timeout: Duration,
    /// Restart behavior after the process exits.
    pub restart: RestartPolicy,
    /// Soft and hard open-file limit applied to the job.
    pub open_files: u64,
}

/// Validated, target-neutral description of one native job generation.
///
/// Every path is absolute and normalized, no value contains NUL, sizes are
/// bounded, and the environment carries only bootstrap keys, so a backend can
/// serialize it without escaping decisions of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDefinition {
    spec: JobSpec,
}

impl JobDefinition {
    /// Validates a job description.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDefinition`] for a relative or non-normalized
    /// path, a NUL byte, an oversized or excessive value, an environment key
    /// outside [`BOOTSTRAP_ENV_ALLOWLIST`], a zero or excessive timeout, or an
    /// open-file limit outside `MIN_OPEN_FILES..=MAX_OPEN_FILES`.
    pub fn new(spec: JobSpec) -> Result<Self, Error> {
        validate_path("executable", &spec.executable)?;
        validate_path("working_directory", &spec.working_directory)?;
        if let Some(logs) = &spec.logs {
            validate_path("stdout", &logs.stdout)?;
            validate_path("stderr", &logs.stderr)?;
        }
        if spec.arguments.len() > MAX_JOB_ARGUMENTS {
            return Err(invalid(format!(
                "job has {} arguments; maximum is {MAX_JOB_ARGUMENTS}",
                spec.arguments.len()
            )));
        }
        for argument in &spec.arguments {
            validate_value("argument", argument)?;
        }
        for (key, value) in &spec.environment {
            if !BOOTSTRAP_ENV_ALLOWLIST.contains(&key.as_str()) {
                return Err(invalid(format!(
                    "environment key `{key}` is outside the bootstrap allowlist"
                )));
            }
            validate_value("environment value", value)?;
            validate_path("environment path", Path::new(value))?;
        }
        validate_timeout("start_timeout", spec.start_timeout)?;
        validate_timeout("exit_timeout", spec.exit_timeout)?;
        if let RestartPolicy::OnFailure { throttle } = spec.restart {
            validate_timeout("restart throttle", throttle)?;
        }
        if !(MIN_OPEN_FILES..=MAX_OPEN_FILES).contains(&spec.open_files) {
            return Err(invalid(format!(
                "open_files {} is outside {MIN_OPEN_FILES}..={MAX_OPEN_FILES}",
                spec.open_files
            )));
        }
        Ok(Self { spec })
    }

    /// Returns the absolute executable.
    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.spec.executable
    }

    /// Returns the arguments passed after the executable.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.spec.arguments
    }

    /// Returns the bootstrap environment.
    #[must_use]
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.spec.environment
    }

    /// Returns the absolute working directory.
    #[must_use]
    pub fn working_directory(&self) -> &Path {
        &self.spec.working_directory
    }

    /// Returns the log files, when the definition names them.
    #[must_use]
    pub fn logs(&self) -> Option<&JobLogs> {
        self.spec.logs.as_ref()
    }

    /// Returns the activation bound.
    #[must_use]
    pub fn start_timeout(&self) -> Duration {
        self.spec.start_timeout
    }

    /// Returns the `SIGTERM`-to-`SIGKILL` grace.
    #[must_use]
    pub fn exit_timeout(&self) -> Duration {
        self.spec.exit_timeout
    }

    /// Returns the restart policy.
    #[must_use]
    pub fn restart(&self) -> RestartPolicy {
        self.spec.restart
    }

    /// Returns the open-file limit.
    #[must_use]
    pub fn open_files(&self) -> u64 {
        self.spec.open_files
    }

    /// Returns the executable and arguments as recorded facts.
    #[must_use]
    pub fn facts(&self) -> DefinitionFacts {
        DefinitionFacts {
            executable: self.spec.executable.clone(),
            arguments: self.spec.arguments.clone(),
        }
    }
}

/// Executable and arguments a backend can prove for an observed job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionFacts {
    /// Absolute executable named by the job.
    pub executable: PathBuf,
    /// Arguments named by the job.
    pub arguments: Vec<String>,
}

/// Native supervisor observation without backend-specific handles or names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceObservation {
    /// Logical identifier within the configured supervisor namespace.
    pub id: ServiceId,
    /// Portable lifecycle state; evidence, not readiness.
    pub state: ServiceState,
    /// Stable process identity when the service currently has a main process.
    pub process: Option<ProcessIdentity>,
    /// Recorded executable and arguments, where the backend can prove them.
    pub definition: Option<DefinitionFacts>,
}

/// Errors shared by native supervisor backends.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A logical identifier could escape the configured supervisor namespace.
    #[error("invalid supervisor service id: {0}")]
    InvalidServiceId(String),
    /// A job definition violates the target-neutral contract.
    #[error("invalid job definition: {detail}")]
    InvalidDefinition {
        /// Non-sensitive validation detail.
        detail: String,
    },
    /// The requested logical service does not exist.
    #[error("supervisor service `{0}` was not found")]
    NotFound(ServiceId),
    /// The native manager already holds a job under this identifier.
    #[error("supervisor service `{0}` is already registered")]
    AlreadyRegistered(ServiceId),
    /// The native manager's user domain is absent, such as `gui/<uid>` outside
    /// a logged-in GUI session.
    #[error("supervisor domain `{domain}` is unavailable")]
    DomainUnavailable {
        /// Native domain that could not be found.
        domain: String,
    },
    /// A native manager command exceeded its configured deadline.
    #[error("supervisor operation `{operation}` timed out")]
    Timeout {
        /// Stable operation label.
        operation: &'static str,
    },
    /// The service changed generation during one observation.
    #[error("supervisor service changed during `{operation}`")]
    Race {
        /// Stable operation label.
        operation: &'static str,
    },
    /// The native supervisor or one of its required facilities is unavailable.
    #[error("supervisor operation `{operation}` is unavailable: {source}")]
    Unavailable {
        /// Stable operation label.
        operation: &'static str,
        /// Backend error retained for diagnostics.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The native supervisor rejected an otherwise valid operation.
    #[error("supervisor operation `{operation}` failed: {source}")]
    Operation {
        /// Stable operation label.
        operation: &'static str,
        /// Backend error retained for diagnostics.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A native response violated the shared supervisor contract.
    #[error("invalid supervisor response during `{operation}`: {detail}")]
    InvalidData {
        /// Stable operation label.
        operation: &'static str,
        /// Non-sensitive validation detail.
        detail: String,
    },
}

/// Boxed native supervisor operation.
pub type Operation<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// Registers, discovers, inspects, and retires durable job generations.
///
/// One [`ServiceId`] names exactly one generation. Replacing a generation is a
/// caller-level sequence (start the new one, prove the old one ended, retire
/// it), never a backend primitive.
pub trait Supervisor: std::fmt::Debug + Send + Sync {
    /// Registers and starts exactly this generation.
    ///
    /// Returns once the manager accepted the job; it never waits for worker
    /// readiness, which comes only from the worker's private socket.
    fn start<'a>(&'a self, id: &'a ServiceId, definition: &'a JobDefinition) -> Operation<'a, ()>;

    /// Discovers jobs in this backend's validated namespace.
    ///
    /// Best effort for reconciliation: a backend may skip a job whose
    /// inspection raced, and the caller learns nothing about it because
    /// discovery is repeated anyway.
    fn discover(&self) -> Operation<'_, Vec<ServiceObservation>>;

    /// Discovers jobs, refusing a result that may be incomplete.
    ///
    /// Destructive callers (version garbage collection, uninstall) must use
    /// this instead of [`Supervisor::discover`]: a job omitted from a partial
    /// result may still be registered and execute, and treating the list as
    /// complete would let such a caller delete the version that job runs
    /// from. Backends whose enumeration can skip a job fail with
    /// [`Error::Race`]; the default wraps [`Supervisor::discover`], which
    /// reports every job of the namespace.
    fn discover_strict(&self) -> Operation<'_, Vec<ServiceObservation>> {
        self.discover()
    }

    /// Inspects one job; an absent job is [`Error::NotFound`].
    fn inspect<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ServiceObservation>;

    /// Stops and unregisters one job; an absent job is [`Error::NotFound`].
    fn retire<'a>(&'a self, id: &'a ServiceId) -> Operation<'a, ()>;
}

/// Manages the single long-lived daemon job of one installation.
///
/// The daemon is the only job registered for login: a launchd agent in
/// `~/Library/LaunchAgents` or an enabled systemd user unit. It restarts on
/// failure with the definition's throttle. Operations here never touch worker
/// jobs, so upgrading or removing the daemon leaves live workers running.
pub trait DaemonSupervisor: std::fmt::Debug + Send + Sync {
    /// Writes the persistent daemon definition, enables it for login, and
    /// starts it.
    ///
    /// Fails with [`Error::AlreadyRegistered`] when a daemon job is loaded.
    fn install<'a>(&'a self, definition: &'a JobDefinition) -> Operation<'a, ()>;

    /// Rewrites the daemon definition, enables it for login, and restarts
    /// only the daemon job.
    ///
    /// Enabling is idempotent and always performed, so `replace` also
    /// completes an install interrupted after the definition was written.
    /// Fails with [`Error::NotFound`] when no daemon definition exists.
    fn replace<'a>(&'a self, definition: &'a JobDefinition) -> Operation<'a, ()>;

    /// Inspects the daemon job; an absent job is [`Error::NotFound`].
    fn inspect(&self) -> Operation<'_, ServiceObservation>;

    /// Stops the daemon job, disables it, and removes its definition.
    ///
    /// An already absent job is not an error, so an interrupted uninstall can
    /// be resumed.
    fn uninstall(&self) -> Operation<'_, ()>;
}

fn invalid(detail: String) -> Error {
    Error::InvalidDefinition { detail }
}

fn validate_value(field: &str, value: &str) -> Result<(), Error> {
    if value.len() > MAX_JOB_VALUE_BYTES {
        return Err(invalid(format!(
            "{field} has {} bytes; maximum is {MAX_JOB_VALUE_BYTES}",
            value.len()
        )));
    }
    if value.contains('\0') {
        return Err(invalid(format!("{field} contains a NUL byte")));
    }
    Ok(())
}

fn validate_path(field: &str, path: &Path) -> Result<(), Error> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.len() > MAX_JOB_VALUE_BYTES {
        return Err(invalid(format!(
            "{field} has {} bytes; maximum is {MAX_JOB_VALUE_BYTES}",
            bytes.len()
        )));
    }
    if bytes.contains(&0) {
        return Err(invalid(format!("{field} contains a NUL byte")));
    }
    if path.to_str().is_none() {
        return Err(invalid(format!("{field} is not valid UTF-8")));
    }
    // `Path::components` folds `.` segments away, so inspect raw segments to
    // reject any path that is not already normalized.
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        || bytes
            .split(|byte| *byte == b'/')
            .any(|segment| segment == b".")
    {
        return Err(invalid(format!(
            "{field} must be an absolute normalized path"
        )));
    }
    Ok(())
}

fn validate_timeout(field: &str, value: Duration) -> Result<(), Error> {
    if value.is_zero() || value > MAX_JOB_TIMEOUT {
        return Err(invalid(format!(
            "{field} must be within (0, {}s]",
            MAX_JOB_TIMEOUT.as_secs()
        )));
    }
    Ok(())
}

/// Logs one structured warning per discovered entry a backend rejected.
///
/// A rejected entry is a definition file or unit of this namespace that does
/// not parse or whose label does not match its name; it is never inspected or
/// retired. Only its name is logged, never its contents.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn warn_rejected(backend: &'static str, names: &[String]) {
    for name in names {
        tracing::event!(
            name: "supervisor.discover.rejected",
            tracing::Level::WARN,
            supervisor.backend = backend,
            supervisor.entry = name.as_str(),
            "rejected a discovered worker entry of this namespace; it is never touched"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects formatted log output of the current thread.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capture buffer")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("capture buffer").clone())
                .expect("UTF-8 log output")
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn every_rejected_entry_is_one_named_warning() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || writer.clone())
            .finish();
        let names = ["first.plist".to_owned(), "second.plist".to_owned()];

        tracing::subscriber::with_default(subscriber, || warn_rejected("launchd", &names));

        let text = captured.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), names.len(), "{text}");
        for (line, name) in lines.iter().zip(&names) {
            assert!(line.contains(r#""level":"WARN""#), "{line}");
            assert!(
                line.contains(&format!(r#""supervisor.entry":"{name}""#)),
                "{line}"
            );
            assert!(line.contains(r#""supervisor.backend":"launchd""#), "{line}");
        }
    }

    #[test]
    fn service_ids_are_bounded_and_namespace_safe() {
        for valid in [
            "s-42",
            ".worker",
            "worker.",
            "a..b",
            "pohunek.worker_1",
            "s-01KYAPVPFVHD56Z69B9CX3XWN2.abcd2345",
            &"a".repeat(128),
        ] {
            assert_eq!(
                ServiceId::parse(valid).expect("valid identifier").as_str(),
                valid
            );
        }
        for invalid in [
            "",
            ".",
            "..",
            "../worker",
            "worker/service",
            &"a".repeat(129),
        ] {
            assert!(matches!(
                ServiceId::parse(invalid),
                Err(Error::InvalidServiceId(value)) if value == invalid
            ));
        }
    }

    fn spec() -> JobSpec {
        JobSpec {
            executable: PathBuf::from("/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond"),
            arguments: vec!["--session-id".to_owned(), "s-42".to_owned()],
            environment: BTreeMap::from([(
                "XDG_STATE_HOME".to_owned(),
                "/home/u/.local/state".to_owned(),
            )]),
            working_directory: PathBuf::from("/home/u"),
            logs: Some(JobLogs {
                stdout: PathBuf::from("/home/u/.local/state/pohunek/logs/launchd/a.out.log"),
                stderr: PathBuf::from("/home/u/.local/state/pohunek/logs/launchd/a.err.log"),
            }),
            start_timeout: Duration::from_secs(45),
            exit_timeout: Duration::from_secs(30),
            restart: RestartPolicy::Never,
            open_files: 8_192,
        }
    }

    fn rejects(mutate: impl FnOnce(&mut JobSpec)) {
        let mut candidate = spec();
        mutate(&mut candidate);
        assert!(
            matches!(
                JobDefinition::new(candidate.clone()),
                Err(Error::InvalidDefinition { .. })
            ),
            "accepted {candidate:?}"
        );
    }

    #[test]
    fn valid_definitions_keep_every_field() {
        let definition = JobDefinition::new(spec()).expect("valid definition");
        assert_eq!(definition.arguments(), ["--session-id", "s-42"]);
        assert_eq!(definition.open_files(), 8_192);
        assert_eq!(definition.restart(), RestartPolicy::Never);
        assert_eq!(
            definition.facts().executable,
            Path::new("/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond")
        );
    }

    #[test]
    fn definitions_reject_relative_and_non_normalized_paths() {
        rejects(|spec| spec.executable = PathBuf::from("pohunek-sessiond"));
        rejects(|spec| spec.executable = PathBuf::from("/opt/../bin/sh"));
        rejects(|spec| spec.executable = PathBuf::from("/opt/./bin/sh"));
        rejects(|spec| spec.working_directory = PathBuf::from("./home"));
        rejects(|spec| {
            spec.logs = Some(JobLogs {
                stdout: PathBuf::from("relative.log"),
                stderr: PathBuf::from("/abs.log"),
            });
        });
        rejects(|spec| {
            spec.environment
                .insert("HOME".to_owned(), "relative/home".to_owned());
        });
    }

    #[test]
    fn definitions_reject_nul_and_oversized_values() {
        rejects(|spec| spec.arguments.push("a\0b".to_owned()));
        rejects(|spec| spec.arguments.push("a".repeat(MAX_JOB_VALUE_BYTES + 1)));
        rejects(|spec| spec.arguments = vec![String::new(); MAX_JOB_ARGUMENTS + 1]);
        rejects(|spec| {
            spec.environment.insert(
                "HOME".to_owned(),
                format!("/{}", "a".repeat(MAX_JOB_VALUE_BYTES)),
            );
        });
        rejects(|spec| spec.executable = PathBuf::from("/opt/a\0b"));
    }

    #[test]
    fn definitions_reject_keys_outside_the_bootstrap_allowlist() {
        for key in [
            "PATH",
            "POHUNEK_CONTROLLER_TOKEN",
            "SSH_AUTH_SOCK",
            "xdg_state_home",
        ] {
            rejects(|spec| {
                spec.environment.insert(key.to_owned(), "/x".to_owned());
            });
        }
    }

    #[test]
    fn definitions_reject_unbounded_timeouts_and_limits() {
        rejects(|spec| spec.start_timeout = Duration::ZERO);
        rejects(|spec| spec.exit_timeout = MAX_JOB_TIMEOUT + Duration::from_secs(1));
        rejects(|spec| {
            spec.restart = RestartPolicy::OnFailure {
                throttle: Duration::ZERO,
            };
        });
        rejects(|spec| spec.open_files = MIN_OPEN_FILES - 1);
        rejects(|spec| spec.open_files = MAX_OPEN_FILES + 1);
    }
}
