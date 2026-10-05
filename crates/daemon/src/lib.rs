//! pohunek host daemon library.
//!
//! The daemon owns logical session state, supervises durable per-session PTY
//! workers, and serves the control protocol over a local Unix socket and
//! optional `NetBird` TCP. This crate exposes the control-plane runtime so the
//! `pohunekd` binary and integration tests can drive it.
//!
//! Current scope: bind the Unix socket with correct permissions,
//! single-instance lock, stale-socket recovery, `daemon.health`, durable-worker
//! reconciliation, raw attach streaming over a separate connection, agents,
//! detection (the state engine), worktree-per-session binding, a unified
//! JSON-lines logical metadata store, an append-only event log, and direct
//! remote transport over `NetBird`.

// Unsafe is denied by default; the few FFI sites (advisory flock, socket chmod,
// pidfd syscalls) opt back in with localized `#[expect(unsafe_code)]` and SAFETY
// comments.
#![deny(unsafe_code)]

pub mod error;
mod git;
pub mod governance;
pub mod host_state;
pub mod lock;
pub mod logging;
pub mod paths;

pub mod api;
pub mod assistant;
pub mod capabilities;
pub mod discovery;
pub mod doctor;

pub mod runtime;
pub mod session;

pub mod agent;
pub mod detect;
pub mod events;
mod external;
pub mod integration;
pub mod notifications;
pub mod notify;
pub mod procwatch;
pub mod project;
pub mod store;
pub(crate) mod time;
pub mod worktree;

pub use error::DaemonError;
pub use paths::Paths;

/// Daemon build version (from Cargo). Reported by `daemon.health`.
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
pub(crate) mod test_support {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;

    use overlay::{
        BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
        OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
    };

    /// A per-test directory, removed with its contents when dropped.
    ///
    /// Dereferences to its path, so callers that take a `&PathBuf` or a path
    /// reference accept it directly.
    #[derive(Debug)]
    pub(crate) struct ScopedDir {
        path: std::path::PathBuf,
        _guard: tempfile::TempDir,
    }

    impl std::ops::Deref for ScopedDir {
        type Target = std::path::PathBuf;

        fn deref(&self) -> &std::path::PathBuf {
            &self.path
        }
    }

    impl AsRef<std::path::Path> for ScopedDir {
        fn as_ref(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl AsRef<std::ffi::OsStr> for ScopedDir {
        fn as_ref(&self) -> &std::ffi::OsStr {
            self.path.as_os_str()
        }
    }

    std::thread_local! {
        /// Directories of the current test thread, removed when it ends.
        static THREAD_DIRS: std::cell::RefCell<Vec<tempfile::TempDir>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Creates a private directory that lives until the calling thread ends.
    ///
    /// For fixture helpers that return a bare path and are called from many
    /// tests. The test harness runs every test on a thread of its own, so the
    /// end of that thread is the end of the test.
    pub(crate) fn thread_scoped_dir(prefix: &str) -> std::path::PathBuf {
        let guard = pohunek_test_support::tempdir_with_prefix(prefix)
            .expect("create the thread-scoped test directory");
        let path = guard.path().to_path_buf();
        THREAD_DIRS.with(|dirs| dirs.borrow_mut().push(guard));
        path
    }

    std::thread_local! {
        /// The host-state directory of the current test thread.
        static THREAD_HOST_STATE: std::cell::OnceCell<std::path::PathBuf> =
            const { std::cell::OnceCell::new() };
    }

    /// The owner-private host-state directory shared by every default session
    /// registry configuration of the calling test thread.
    ///
    /// One directory per test means one profile-revision key per test, so a
    /// registry rebuilt from a default configuration keeps verifying the
    /// revisions it froze earlier.
    pub(crate) fn thread_host_state_dir() -> std::path::PathBuf {
        THREAD_HOST_STATE.with(|cell| {
            cell.get_or_init(|| thread_scoped_dir("pohunek-host-state-"))
                .clone()
        })
    }

    std::thread_local! {
        /// The hermetic environment of the current test thread.
        static THREAD_ENV: std::cell::OnceCell<pohunek_test_support::env::TestEnv> =
            const { std::cell::OnceCell::new() };
    }

    /// Returns the variables of the current test thread's hermetic
    /// [`TestEnv`](pohunek_test_support::env::TestEnv) as a base-environment
    /// source.
    ///
    /// Registry fixtures hand this to their supervision config so an agent
    /// child sees the fixture's private `HOME` and none of the developer's
    /// variables. The environment lives until the calling thread ends, which
    /// is the end of the test.
    pub(crate) fn thread_environment_source() -> crate::runtime::EnvironmentSource {
        THREAD_ENV.with(|cell| {
            let env = cell.get_or_init(|| {
                pohunek_test_support::env::TestEnv::new()
                    .expect("create the thread-scoped test environment")
            });
            crate::runtime::EnvironmentSource::fixed(env.environment().clone())
        })
    }

    /// Creates a private, short-named [`ScopedDir`] whose name starts with
    /// `prefix`.
    pub(crate) fn scoped_dir(prefix: &str) -> ScopedDir {
        let guard = pohunek_test_support::tempdir_with_prefix(prefix)
            .expect("create the scoped test directory");
        ScopedDir {
            path: guard.path().to_path_buf(),
            _guard: guard,
        }
    }

    #[derive(Debug)]
    struct EmptyTransport {
        id: OverlayId,
    }

    impl OverlayTransport for EmptyTransport {
        fn id(&self) -> &OverlayId {
            &self.id
        }

        fn validate_bind_addr(&self, _addr: IpAddr) -> Result<(), BindAddrError> {
            Ok(())
        }

        fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
            Box::pin(async { Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)) })
        }

        fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
            let overlay = self.id.clone();
            Box::pin(async move {
                Err(OverlayError::HostUnknown {
                    host: host.to_owned(),
                    overlay,
                })
            })
        }

        fn resolve_peer_identity<'a>(
            &'a self,
            identity: &'a ExternalIdentity,
        ) -> OverlayFuture<'a, ResolvedPeer> {
            let overlay = self.id.clone();
            Box::pin(async move {
                Err(OverlayError::HostUnknown {
                    host: identity.value().to_owned(),
                    overlay,
                })
            })
        }

        fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    pub(crate) fn overlay_registry() -> OverlayRegistry {
        let transport = Arc::new(EmptyTransport {
            id: OverlayId::new("test").expect("overlay id"),
        });
        let configured = ConfiguredTransport::new(transport, 18_722).expect("configured overlay");
        OverlayRegistry::new(vec![configured]).expect("registry")
    }
}
