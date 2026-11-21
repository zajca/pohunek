//! The executable search path of a daemon that inherits the systemd user
//! manager's environment.
//!
//! A user unit started before the desktop session imports its environment keeps
//! the `PATH` it was started with, so agents the login environment provides
//! (version-manager shims) stay invisible until the daemon restarts. A daemon
//! whose job takes the manager `PATH` therefore reads the manager again at the
//! boundary where its registry resolves or launches an agent. The operation
//! takes one [`SearchPath`] snapshot from that read and moves it to every
//! resolver, `git` spawn and hook it reaches, so a probe, a worktree bind and a
//! launch agree on what runs even while another operation refreshes. Work that
//! starts no agent (diff, retention, removal, session-stop hooks, cwd tracking,
//! project add, show and candidates) takes the registry's last read without reading the manager again. The state
//! belongs to the [`ManagerPath`] of a registry; no process-wide copy exists, so
//! two registries never share it.
//!
//! Only `PATH` is refreshed. Other allowlisted variables, such as a store
//! location, stay as the process got them. A manager that cannot answer leaves
//! the process `PATH`, which is what the service manager gave the daemon.
//! Values never reach a log or an error.

// Rust guideline compliant 2026-11-21

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use pohunek_platform::supervisor::UnitEnvironment;
use tracing::{debug, warn};

/// The executable search path of one operation, taken once from its registry
/// and moved to every resolver, `git` spawn and hook the operation reaches.
///
/// A value, not a reader: it is `Send + Sync` and cheap to clone, so it crosses
/// `spawn_blocking` and `spawn` boundaries by being moved into the closure, and
/// a concurrent refresh of the registry never changes it.
#[derive(Debug, Clone, Default)]
pub(crate) struct SearchPath {
    /// The `PATH` executables resolve on: the manager's when the daemon
    /// follows it and has read one, else the process's.
    resolved: Option<Arc<OsStr>>,
    /// The manager `PATH` of the read this snapshot was taken after; the base
    /// environment of a launch swaps it in for the process's.
    manager: Option<Arc<OsStr>>,
}

impl SearchPath {
    /// A snapshot resolving on `resolved`, with `manager` as the manager
    /// `PATH` the base environment follows.
    #[must_use]
    pub(crate) fn new(resolved: Option<OsString>, manager: Option<OsString>) -> Self {
        Self {
            resolved: resolved.map(Arc::from),
            manager: manager.map(Arc::from),
        }
    }

    /// The process `PATH`, for fixtures that never follow a service manager.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn process() -> Self {
        Self::new(crate::agent::process_search_path(), None)
    }

    /// The `PATH` executables resolve on.
    #[must_use]
    pub(crate) fn as_os_str(&self) -> Option<&OsStr> {
        self.resolved.as_deref()
    }

    /// An owned copy of [`Self::as_os_str`].
    #[must_use]
    pub(crate) fn to_os_string(&self) -> Option<OsString> {
        self.resolved.as_deref().map(OsStr::to_os_string)
    }

    /// The manager `PATH` the base environment of a launch follows.
    #[must_use]
    pub(crate) fn manager(&self) -> Option<OsString> {
        self.manager.as_deref().map(OsStr::to_os_string)
    }
}

/// Why the manager's `PATH` could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the service manager's environment could not be read: {detail}")]
pub struct ManagerReadError {
    detail: String,
}

impl ManagerReadError {
    /// Builds the error from a reason that carries no environment value.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

/// A future returned by [`ManagerEnvironment`].
pub type ManagerFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ManagerReadError>> + Send + 'a>>;

/// Reads the `PATH` the service manager gives its units.
pub trait ManagerEnvironment: std::fmt::Debug + Send + Sync {
    /// Returns the manager's `PATH`, or `None` when it has none.
    fn search_path(&self) -> ManagerFuture<'_, Option<OsString>>;

    /// Returns the environment settings the manager holds for `unit`.
    fn unit_environment<'a>(&'a self, unit: &'a str) -> ManagerFuture<'a, UnitEnvironment>;
}

/// What the daemon's own configuration says about where its `PATH` comes
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowGate {
    /// The daemon runs as a job of a service manager.
    pub service_managed: bool,
    /// The service configuration records a `search_path` for the job.
    pub search_path_recorded: bool,
}

/// The `PATH` of the daemon's service manager, read at most once per
/// `max_age`.
#[derive(Debug, Clone)]
pub struct ManagerPath {
    inner: Arc<Inner>,
}

impl PartialEq for ManagerPath {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for ManagerPath {}

#[derive(Debug)]
struct Inner {
    source: Arc<dyn ManagerEnvironment>,
    max_age: Duration,
    deadline: Duration,
    last_read: tokio::sync::Mutex<Option<Instant>>,
    /// The manager `PATH` the last read returned; `None` when the daemon uses
    /// its process environment. A std lock because sync readers run on
    /// blocking threads.
    current: RwLock<Option<OsString>>,
}

impl ManagerPath {
    /// Follows `source`; a read younger than `max_age` is reused, and a read
    /// that takes longer than `deadline` counts as unanswered.
    #[must_use]
    pub fn new(source: Arc<dyn ManagerEnvironment>, max_age: Duration, deadline: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                source,
                max_age,
                deadline,
                last_read: tokio::sync::Mutex::new(None),
                current: RwLock::new(None),
            }),
        }
    }

    /// Follows `source` when the job of `unit` takes the manager's `PATH` as it
    /// is: the daemon runs under a service manager, its configuration records
    /// no `search_path`, the unit is loaded and nothing in it or its drop-ins
    /// can set a `PATH`. Otherwise, and for an unreadable unit, it answers
    /// `None` and the daemon keeps the `PATH` it started with.
    pub async fn follow(
        source: Arc<dyn ManagerEnvironment>,
        unit: &str,
        gate: FollowGate,
        max_age: Duration,
        deadline: Duration,
    ) -> Option<Self> {
        if !gate.service_managed || gate.search_path_recorded {
            return None;
        }
        let environment = match tokio::time::timeout(deadline, source.unit_environment(unit)).await
        {
            Ok(Ok(environment)) => environment,
            Ok(Err(error)) => {
                warn!(%error, "the daemon unit's environment is unreadable; the manager PATH is not followed");
                return None;
            }
            Err(_elapsed) => {
                warn!("the daemon unit's environment was not read in time; the manager PATH is not followed");
                return None;
            }
        };
        (environment.load_state == "loaded" && !environment.may_set_search_path())
            .then(|| Self::new(source, max_age, deadline))
    }

    /// The manager `PATH` the last refresh read, when one is in use.
    #[must_use]
    pub fn manager_path(&self) -> Option<OsString> {
        self.inner
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn store(&self, path: Option<OsString>) {
        *self
            .inner
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner) = path;
    }

    /// Reads the manager `PATH` unless a recent read is still fresh, and makes
    /// it the daemon's search path; a manager that cannot answer, or that has
    /// no `PATH`, returns the daemon to its process `PATH`.
    ///
    /// Concurrent callers share one read.
    pub async fn refresh(&self) {
        let mut last_read = self.inner.last_read.lock().await;
        if last_read.is_some_and(|at| at.elapsed() < self.inner.max_age) {
            return;
        }
        let read = tokio::time::timeout(self.inner.deadline, self.inner.source.search_path())
            .await
            .unwrap_or_else(|_elapsed| Err(ManagerReadError::new("the read timed out")));
        match read {
            Ok(path) => {
                let changed = self.manager_path() != path;
                self.store(path);
                if changed {
                    debug!("the daemon search path follows the service manager");
                }
            }
            Err(error) => {
                if self.manager_path().is_some() {
                    warn!(%error, "the service manager's PATH is unreadable; using the process PATH");
                }
                self.store(None);
            }
        }
        *last_read = Some(Instant::now());
    }
}

/// Picks the `PATH` entry out of `KEY=VALUE` manager entries; a later entry
/// wins, as it does in the manager.
#[must_use]
pub fn search_path_of(entries: &[String]) -> Option<OsString> {
    entries
        .iter()
        .rev()
        .find_map(|entry| entry.strip_prefix("PATH="))
        .map(OsStr::new)
        .map(OsString::from)
}

/// The systemd user manager's environment, read over the session bus.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
pub struct SystemdUserManager {
    timeout: Duration,
}

#[cfg(target_os = "linux")]
impl SystemdUserManager {
    /// Reads the manager with every bus call bounded by `timeout`.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[cfg(target_os = "linux")]
impl ManagerEnvironment for SystemdUserManager {
    fn search_path(&self) -> ManagerFuture<'_, Option<OsString>> {
        Box::pin(async move {
            pohunek_platform::supervisor::systemd::user_manager_environment(self.timeout)
                .await
                .map(|entries| search_path_of(&entries))
                .map_err(|error| ManagerReadError::new(error.to_string()))
        })
    }

    fn unit_environment<'a>(&'a self, unit: &'a str) -> ManagerFuture<'a, UnitEnvironment> {
        Box::pin(async move {
            pohunek_platform::supervisor::systemd::unit_environment(unit, self.timeout)
                .await
                .map_err(|error| ManagerReadError::new(error.to_string()))
        })
    }
}
