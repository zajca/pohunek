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
        BindAddrError, ConfiguredTransport, DiscoveredPeer, OverlayError, OverlayFuture, OverlayId,
        OverlayRegistry, OverlayTransport, ResolvedPeer,
    };

    pub(crate) static XDG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
