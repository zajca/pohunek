//! Defines portable process observation contracts.
//!
//! A process is authoritative only when its numeric ID and opaque start
//! identity both match within the current boot. Start identities are comparable
//! for equality only; they are not portable timestamps. Persisted callers that
//! cross a reboot boundary must additionally bind records to a [`BootIdentity`].

// Rust guideline compliant 2026-09-22

use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
#[doc(inline)]
pub use linux::LinuxInspector;

// The pure kernel-layout decoding is built on every host so its region
// separation, bounds, and integer conversions are covered by the ordinary
// workspace test gate instead of only by the macOS runner.
#[cfg(all(unix, any(target_os = "macos", test)))]
mod darwin_layout;

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(unix)]
mod sweep;

#[cfg(unix)]
#[doc(inline)]
pub use sweep::{
    sweep_runtime, SkipReason, Skipped, SweepError, SweepReport, SweepRequest,
    MAX_RUNTIME_ID_BYTES, MAX_SWEEP_GRACE,
};

#[cfg(target_os = "macos")]
#[doc(inline)]
pub use darwin::DarwinInspector;

/// Process inspector backing this host.
///
/// Daemon, worker, and client code name this alias instead of a concrete
/// backend, so exactly one implementation answers every process-identity
/// question on a given target.
#[cfg(target_os = "linux")]
pub type HostInspector = LinuxInspector;

/// Process inspector backing this host.
///
/// Daemon, worker, and client code name this alias instead of a concrete
/// backend, so exactly one implementation answers every process-identity
/// question on a given target.
#[cfg(target_os = "macos")]
pub type HostInspector = DarwinInspector;

/// Operating-system process identifier.
pub type Pid = u32;

/// Opaque same-boot process start identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StartIdentity(u64);

impl StartIdentity {
    /// Creates an identity from its canonical integer representation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the canonical integer representation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for StartIdentity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for StartIdentity {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<u64>()
            .map(Self)
            .map_err(|source| Error::InvalidStartIdentity { source })
    }
}

/// Opaque operating-system boot identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BootIdentity(String);

impl BootIdentity {
    /// Parses a nonempty bounded boot identity.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidBootIdentity`] for invalid input.
    pub fn parse(value: impl Into<String>) -> Result<Self, Error> {
        /// Prevents an operating-system value from becoming unbounded metadata.
        const MAX_BOOT_ID_BYTES: usize = 128;

        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_BOOT_ID_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(Error::InvalidBootIdentity);
        }
        Ok(Self(value))
    }

    /// Returns the opaque operating-system value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BootIdentity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// PID-reuse-safe identity within one boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    /// Numeric process identifier.
    pub pid: Pid,
    /// Opaque process start identity.
    pub start_identity: StartIdentity,
}

/// Process facts read from the operating system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessFact {
    /// Process id.
    pub pid: Pid,
    /// Process-group id used to identify terminal foreground members exactly.
    pub pgid: Pid,
    /// Parent process id.
    pub ppid: Pid,
    /// Opaque start identity used with `pid` to reject PID reuse.
    pub start_identity: StartIdentity,
    /// Kernel task command name.
    pub comm: String,
    /// Argument vector reported by the operating system.
    pub cmdline: Vec<String>,
}

impl ProcessFact {
    /// Returns the PID-reuse-safe identity for this observation.
    #[must_use]
    pub const fn identity(&self) -> ProcessIdentity {
        ProcessIdentity {
            pid: self.pid,
            start_identity: self.start_identity,
        }
    }
}

/// Allowlisted Pohunek ownership markers from a process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnershipMarkers {
    /// Value of `POHUNEK_DAEMON_ID`, when present.
    pub daemon_id: Option<String>,
    /// Value of `POHUNEK_SESSION_ID`, when present.
    pub session_id: Option<String>,
    /// Value of `POHUNEK_RUNTIME_ID`, when present.
    ///
    /// A session worker injects it into every child it launches, so it names
    /// exactly one worker runtime generation.
    pub runtime_id: Option<String>,
}

impl OwnershipMarkers {
    /// Returns whether any ownership marker is present.
    #[must_use]
    pub fn is_marked(&self) -> bool {
        self.daemon_id.is_some() || self.session_id.is_some() || self.runtime_id.is_some()
    }
}

/// Typed process-observation failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The target lacks the requested operating-system facility.
    #[error("process operation `{operation}` is unavailable on this system")]
    Unavailable {
        /// Stable operation label.
        operation: &'static str,
    },
    /// The operating system denied access to process evidence.
    #[error("process operation `{operation}` was denied")]
    PermissionDenied {
        /// Stable operation label.
        operation: &'static str,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// The process exited or changed identity during observation.
    #[error("process identity changed during `{operation}`")]
    Race {
        /// Stable operation label.
        operation: &'static str,
    },
    /// Process data did not match the operating-system format.
    #[error("process data from `{operation}` is malformed")]
    InvalidData {
        /// Stable operation label.
        operation: &'static str,
    },
    /// A native identifier cannot be represented by the shared contract.
    #[error("native process value from `{operation}` is out of range")]
    OutOfRange {
        /// Stable operation label.
        operation: &'static str,
    },
    /// A decimal start identity was invalid.
    #[error("process start identity is not an unsigned decimal integer")]
    InvalidStartIdentity {
        /// Decimal parser failure.
        #[source]
        source: std::num::ParseIntError,
    },
    /// A boot identity violated the bounded opaque-value contract.
    #[error("boot identity is empty, oversized, or contains control characters")]
    InvalidBootIdentity,
    /// Another operating-system I/O failure occurred.
    #[error("process operation `{operation}` failed: {source}")]
    Io {
        /// Stable operation label.
        operation: &'static str,
        /// Underlying operating-system error.
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// Classifies a general operating-system error without exposing contents.
    ///
    /// Process backends must classify target-specific disappearance errors at
    /// their operation call sites before using this fallback.
    #[must_use]
    pub fn from_io(operation: &'static str, source: std::io::Error) -> Self {
        match source.kind() {
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied { operation, source },
            std::io::ErrorKind::Unsupported => Self::Unavailable { operation },
            std::io::ErrorKind::InvalidData => Self::InvalidData { operation },
            _ => Self::Io { operation, source },
        }
    }

    /// Returns whether the failure is a normal process-exit race.
    #[must_use]
    pub const fn is_race(&self) -> bool {
        matches!(self, Self::Race { .. })
    }
}

/// Future-backed process-exit notification.
pub struct ExitWatch {
    future: Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>,
}

impl Debug for ExitWatch {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExitWatch").finish_non_exhaustive()
    }
}

impl ExitWatch {
    /// Creates an exit watch from a bounded backend future.
    pub fn from_future(future: impl Future<Output = Result<(), Error>> + Send + 'static) -> Self {
        Self {
            future: Box::pin(future),
        }
    }

    /// Waits for the exact watched process to exit.
    ///
    /// # Errors
    ///
    /// Returns the typed backend observation failure.
    pub async fn wait(self) -> Result<(), Error> {
        self.future.await
    }
}

/// Injectable process observer used by lifecycle reconciliation.
pub trait ProcessInspector: Debug + Send + Sync + 'static {
    /// Returns the PID-reuse-safe identity without reading unrelated facts.
    ///
    /// An exited process its parent has not reaped yet keeps its identity, and
    /// [`ProcessInspector::is_running`] reports it as not running.
    ///
    /// # Errors
    ///
    /// Returns typed failures when the minimal platform process record cannot be inspected.
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, Error>;

    /// Returns whether an exact process identity is still executing.
    ///
    /// Exited processes retained as zombies are not running. The default is
    /// conservative for platforms that cannot distinguish that state.
    ///
    /// # Errors
    ///
    /// Returns typed failures when the minimal process record cannot be inspected.
    fn is_running(&self, identity: ProcessIdentity) -> Result<bool, Error> {
        Ok(self.identity(identity.pid)? == Some(identity))
    }

    /// Returns the current parent PID without reading unrelated process facts.
    ///
    /// The returned PID is an instantaneous relationship, not a stable identity.
    ///
    /// # Errors
    ///
    /// Returns typed failures when the minimal platform process record cannot be inspected.
    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, Error>;

    /// Returns current facts for one same-user process, or `None` if it exited.
    ///
    /// # Errors
    ///
    /// Returns typed failures when evidence cannot be inspected safely.
    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, Error>;

    /// Returns facts for processes owned by the current effective user.
    ///
    /// # Errors
    ///
    /// Returns typed process-table inspection failures.
    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error>;

    /// Returns direct and transitive descendants, excluding `root`.
    ///
    /// # Errors
    ///
    /// Returns typed ancestry inspection failures.
    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, Error>;

    /// Returns minimal identities for descendants of an exact process root.
    ///
    /// This operation avoids optional process metadata so lifecycle checks do
    /// not depend on readable command lines. Descendants that exit while the
    /// snapshot is being collected are omitted.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Race`] when `root` changes identity during the scan,
    /// or another typed process observation failure.
    fn descendant_identities(&self, root: ProcessIdentity) -> Result<Vec<ProcessIdentity>, Error> {
        const OPERATION: &str = "inspect_descendant_identities";

        if self.identity(root.pid)? != Some(root) {
            return Err(Error::Race {
                operation: OPERATION,
            });
        }
        let identities = self
            .descendants(root.pid)?
            .into_iter()
            .map(|fact| fact.identity())
            .collect();
        if self.identity(root.pid)? != Some(root) {
            return Err(Error::Race {
                operation: OPERATION,
            });
        }
        Ok(identities)
    }

    /// Returns the current working directory for `pid`.
    ///
    /// # Errors
    ///
    /// Returns typed process inspection failures.
    fn cwd(&self, pid: Pid) -> Result<PathBuf, Error>;

    /// Returns the executable path of one same-user process, or `None` if it exited.
    ///
    /// The path is instantaneous evidence: a process can `exec` a different
    /// image at any time, so callers that act on it must recheck the process
    /// identity afterwards.
    ///
    /// # Errors
    ///
    /// Returns typed process inspection failures.
    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, Error>;

    /// Arms an exit watch for one exact process identity.
    ///
    /// Implementations must not require an active async runtime while arming
    /// the watch. Runtime or reactor limitations are reported as typed errors.
    ///
    /// # Errors
    ///
    /// Returns typed identity, facility, or registration failures.
    fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, Error>;

    /// Returns only the allowlisted Pohunek environment markers.
    ///
    /// # Errors
    ///
    /// Returns typed process inspection failures.
    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, Error>;

    /// Returns the foreground process group for `root_pid`.
    ///
    /// # Errors
    ///
    /// Returns typed process inspection failures.
    fn foreground_process_group(&self, root_pid: Pid) -> Result<Option<Pid>, Error>;
}

#[cfg(test)]
mod tests {
    use super::{
        BootIdentity, Error, ExitWatch, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
        ProcessInspector, StartIdentity,
    };
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::str::FromStr;

    /// Inspector whose per-call identity answers are supplied by the test.
    ///
    /// Substituting a start identity between calls reproduces PID reuse without
    /// depending on a real operating-system race.
    #[derive(Debug)]
    struct ScriptedInspector {
        identities: std::sync::Mutex<VecDeque<Option<ProcessIdentity>>>,
        descendants: Vec<ProcessFact>,
    }

    impl ScriptedInspector {
        fn new(
            identities: impl IntoIterator<Item = Option<ProcessIdentity>>,
            descendants: Vec<ProcessFact>,
        ) -> Self {
            Self {
                identities: std::sync::Mutex::new(identities.into_iter().collect()),
                descendants,
            }
        }
    }

    impl ProcessInspector for ScriptedInspector {
        fn identity(&self, _pid: Pid) -> Result<Option<ProcessIdentity>, Error> {
            Ok(self
                .identities
                .lock()
                .expect("scripted identities are not poisoned")
                .pop_front()
                .flatten())
        }

        fn parent_pid(&self, _pid: Pid) -> Result<Option<Pid>, Error> {
            Ok(None)
        }

        fn process(&self, _pid: Pid) -> Result<Option<ProcessFact>, Error> {
            Ok(None)
        }

        fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error> {
            Ok(self.descendants.clone())
        }

        fn descendants(&self, _root: Pid) -> Result<Vec<ProcessFact>, Error> {
            Ok(self.descendants.clone())
        }

        fn cwd(&self, _pid: Pid) -> Result<PathBuf, Error> {
            Err(Error::Unavailable {
                operation: "scripted_cwd",
            })
        }

        fn executable(&self, _pid: Pid) -> Result<Option<PathBuf>, Error> {
            Ok(None)
        }

        fn exit_watch(&self, _identity: ProcessIdentity) -> Result<ExitWatch, Error> {
            Err(Error::Unavailable {
                operation: "scripted_exit_watch",
            })
        }

        fn ownership_markers(&self, _pid: Pid) -> Result<OwnershipMarkers, Error> {
            Ok(OwnershipMarkers::default())
        }

        fn foreground_process_group(&self, _root_pid: Pid) -> Result<Option<Pid>, Error> {
            Ok(None)
        }
    }

    fn identity(pid: Pid, start: u64) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_identity: StartIdentity::new(start),
        }
    }

    fn fact(pid: Pid, start: u64) -> ProcessFact {
        ProcessFact {
            pid,
            pgid: pid,
            ppid: 1,
            start_identity: StartIdentity::new(start),
            comm: "agent".to_owned(),
            cmdline: Vec::new(),
        }
    }

    #[test]
    fn liveness_rejects_a_substituted_start_identity() {
        let expected = identity(4_242, 100);
        let inspector = ScriptedInspector::new([Some(identity(4_242, 101))], Vec::new());

        assert!(!inspector.is_running(expected).expect("scripted liveness"));
    }

    #[test]
    fn descendant_identities_reject_a_root_substituted_before_the_scan() {
        let expected = identity(4_242, 100);
        let inspector =
            ScriptedInspector::new([Some(identity(4_242, 101))], vec![fact(4_243, 200)]);

        assert!(matches!(
            inspector.descendant_identities(expected),
            Err(Error::Race { .. })
        ));
    }

    #[test]
    fn descendant_identities_reject_a_root_substituted_after_the_scan() {
        let expected = identity(4_242, 100);
        let inspector = ScriptedInspector::new(
            [Some(expected), Some(identity(4_242, 101))],
            vec![fact(4_243, 200)],
        );

        assert!(matches!(
            inspector.descendant_identities(expected),
            Err(Error::Race { .. })
        ));
    }

    #[test]
    fn descendant_identities_accept_a_stable_root() {
        let expected = identity(4_242, 100);
        let inspector =
            ScriptedInspector::new([Some(expected), Some(expected)], vec![fact(4_243, 200)]);

        assert_eq!(
            inspector
                .descendant_identities(expected)
                .expect("stable root"),
            vec![identity(4_243, 200)]
        );
    }

    #[test]
    fn a_disappeared_root_is_a_race_not_a_healthy_absence() {
        let expected = identity(4_242, 100);
        let inspector = ScriptedInspector::new([None], vec![fact(4_243, 200)]);

        assert!(matches!(
            inspector.descendant_identities(expected),
            Err(Error::Race { .. })
        ));
        let inspector = ScriptedInspector::new([None], Vec::new());
        assert!(!inspector.is_running(expected).expect("scripted liveness"));
    }

    #[test]
    fn start_identity_roundtrips_full_wire_range() {
        for value in [0, 1, u64::MAX] {
            let identity = StartIdentity::new(value);
            assert_eq!(identity.to_string(), value.to_string());
            assert_eq!(
                StartIdentity::from_str(&identity.to_string()).expect("valid identity"),
                identity
            );
        }
        assert!(matches!(
            StartIdentity::from_str("18446744073709551616"),
            Err(Error::InvalidStartIdentity { .. })
        ));
    }

    #[test]
    fn boot_identity_is_bounded_and_opaque() {
        let identity = BootIdentity::parse("boot-a").expect("valid boot identity");
        assert_eq!(identity.as_str(), "boot-a");
        for invalid in ["", "line\nbreak"] {
            assert!(matches!(
                BootIdentity::parse(invalid),
                Err(Error::InvalidBootIdentity)
            ));
        }
        assert!(matches!(
            BootIdentity::parse("x".repeat(129)),
            Err(Error::InvalidBootIdentity)
        ));
    }

    #[test]
    fn io_errors_keep_security_relevant_classes() {
        assert!(matches!(
            Error::from_io(
                "inspect",
                std::io::Error::from(std::io::ErrorKind::PermissionDenied)
            ),
            Error::PermissionDenied { .. }
        ));
        assert!(matches!(
            Error::from_io(
                "inspect",
                std::io::Error::from(std::io::ErrorKind::NotFound)
            ),
            Error::Io { .. }
        ));
        assert!(matches!(
            Error::from_io(
                "inspect",
                std::io::Error::from(std::io::ErrorKind::Unsupported)
            ),
            Error::Unavailable { .. }
        ));
    }
}
