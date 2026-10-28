//! pohunek host daemon library.
//!
//! The daemon owns logical session state, supervises durable per-session PTY
//! workers, and serves the control protocol over a local Unix socket and
//! optional overlay TCP. This crate exposes the control-plane runtime so the
//! `pohunekd` binary and integration tests can drive it.
//!
//! Current scope: bind the Unix socket with correct permissions,
//! single-instance lock, stale-socket recovery, `daemon.health`, durable-worker
//! reconciliation, raw attach streaming over a separate connection, agents,
//! detection (the state engine), worktree-per-session binding, a unified
//! JSON-lines logical metadata store, an append-only event log, and direct
//! remote transport over a configured overlay.

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
pub(crate) mod bounded_run;
pub mod capabilities;
pub mod catalog_anchor;
pub mod discovery;
pub mod doctor;

pub mod runtime;
pub mod session;
pub mod shutdown;

pub mod agent;
pub mod detect;
pub mod events;
mod external;
pub mod integration;
pub mod notifications;
pub mod notify;
pub mod package_command;
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
pohunek_test_support::hold_image!();

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
    /// reference accept it directly. Dropping it first ends every process that
    /// works in the directory, on success and while a panic unwinds, so a
    /// fixture that fails midway leaves nothing running behind it.
    #[derive(Debug)]
    pub(crate) struct ScopedDir {
        path: std::path::PathBuf,
        /// Declared before the guard so it drops while the directory exists.
        _reaper: crate::procwatch::scoped_host::ScopeReaper,
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
        static THREAD_DIRS: std::cell::RefCell<Vec<ScopedDir>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Creates a private directory that lives until the calling thread ends.
    ///
    /// For fixture helpers that return a bare path and are called from many
    /// tests. The test harness runs every test on a thread of its own, so the
    /// end of that thread is the end of the test.
    pub(crate) fn thread_scoped_dir(prefix: &str) -> std::path::PathBuf {
        let directory = scoped_dir(prefix);
        let path = directory.path.clone();
        THREAD_DIRS.with(|dirs| dirs.borrow_mut().push(directory));
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
    ///
    /// Directories registered with [`prepend_thread_launch_path`] come first in
    /// `PATH`, so a fixture executable shadows the host's real one.
    pub(crate) fn thread_environment_source() -> crate::runtime::EnvironmentSource {
        THREAD_ENV.with(|cell| {
            let env = cell.get_or_init(|| {
                pohunek_test_support::env::TestEnv::new()
                    .expect("create the thread-scoped test environment")
            });
            let mut variables = env.environment().clone();
            let inherited = variables.get(std::ffi::OsStr::new("PATH")).cloned();
            if let Some(path) = with_thread_launch_path(inherited) {
                variables.insert("PATH".into(), path);
            }
            crate::runtime::EnvironmentSource::fixed(variables)
        })
    }

    std::thread_local! {
        /// Directories placed before the inherited `PATH` of the current test
        /// thread's launch environment.
        static THREAD_PATH_PREFIX: std::cell::RefCell<Vec<std::path::PathBuf>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    /// `inherited` with the directories of [`prepend_thread_launch_path`] in
    /// front, or `inherited` itself when this thread registered none.
    pub(crate) fn with_thread_launch_path(
        inherited: Option<std::ffi::OsString>,
    ) -> Option<std::ffi::OsString> {
        THREAD_PATH_PREFIX.with(|prefix| {
            let prefix = prefix.borrow();
            if prefix.is_empty() {
                return inherited;
            }
            let joined = std::env::join_paths(
                prefix
                    .iter()
                    .cloned()
                    .chain(inherited.iter().flat_map(std::env::split_paths)),
            )
            .expect("the launch path joins");
            Some(joined)
        })
    }

    /// Puts `directory` first in the `PATH` that
    /// [`thread_environment_source`] hands out on this test thread, and in the
    /// search path the daemon resolves agent executables on. Call it before the
    /// registry under test is built.
    pub(crate) fn prepend_thread_launch_path(directory: &std::path::Path) {
        THREAD_PATH_PREFIX.with(|prefix| prefix.borrow_mut().insert(0, directory.to_path_buf()));
    }

    /// A launch-input contributor and the integration handler id whose runtimes
    /// it serves.
    type ThreadContributor = (
        &'static str,
        &'static dyn crate::integration::launch_input::LaunchContributor,
    );

    std::thread_local! {
        /// The contributor runtimes launched on the current test thread get.
        static THREAD_CONTRIBUTOR: std::cell::Cell<Option<ThreadContributor>> =
            const { std::cell::Cell::new(None) };
    }

    /// Makes `contributor` the launch-input contributor of every runtime whose
    /// definition names the integration handler `handler` on this test thread;
    /// `None` clears it. Call it before the registry under test is built.
    pub(crate) fn set_thread_launch_contributor(
        handler: &'static str,
        contributor: Option<&'static dyn crate::integration::launch_input::LaunchContributor>,
    ) {
        THREAD_CONTRIBUTOR.with(|cell| cell.set(contributor.map(|c| (handler, c))));
    }

    /// The contributor of [`set_thread_launch_contributor`], if this thread set one.
    pub(crate) fn thread_launch_contributor() -> Option<ThreadContributor> {
        THREAD_CONTRIBUTOR.with(std::cell::Cell::get)
    }

    /// The value of `name` in the current test thread's hermetic environment.
    pub(crate) fn thread_environment_variable(name: &str) -> Option<std::ffi::OsString> {
        THREAD_ENV.with(|cell| {
            cell.get_or_init(|| {
                pohunek_test_support::env::TestEnv::new()
                    .expect("create the thread-scoped test environment")
            })
            .environment()
            .get(std::ffi::OsStr::new(name))
            .cloned()
        })
    }

    /// The `HOME` of the current test thread's hermetic environment.
    pub(crate) fn thread_environment_home() -> std::path::PathBuf {
        THREAD_ENV.with(|cell| {
            cell.get_or_init(|| {
                pohunek_test_support::env::TestEnv::new()
                    .expect("create the thread-scoped test environment")
            })
            .home()
            .to_path_buf()
        })
    }

    /// Creates a private, short-named [`ScopedDir`] whose name starts with
    /// `prefix`.
    pub(crate) fn scoped_dir(prefix: &str) -> ScopedDir {
        let guard = pohunek_test_support::tempdir_with_prefix(prefix)
            .expect("create the scoped test directory");
        ScopedDir {
            path: guard.path().to_path_buf(),
            _reaper: crate::procwatch::scoped_host::ScopeReaper::new(guard.path()),
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
