//! Shared validated overlay registry for daemon integration tests.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
    OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
};
use pohunek_daemon::session::{SessionRegistryConfig, ShellCommand};

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

/// Shell that a registry fixture pins instead of the host user's `$SHELL`.
///
/// A login shell of the host runs its startup files, which can start helpers
/// that keep the PTY open past a kill.
pub(crate) fn hermetic_shell() -> ShellCommand {
    ShellCommand::new("/bin/sh", std::iter::empty::<&str>())
}

/// Registry configuration that pins [`hermetic_shell`] and leaves every other
/// field at its default.
pub(crate) fn hermetic_registry_config() -> SessionRegistryConfig {
    SessionRegistryConfig {
        shell_command: hermetic_shell(),
        ..SessionRegistryConfig::default()
    }
}

pub(crate) fn overlay_registry() -> OverlayRegistry {
    let transport = Arc::new(EmptyTransport {
        id: OverlayId::new("test").expect("overlay id"),
    });
    let configured = ConfiguredTransport::new(transport, 18_722).expect("configured overlay");
    OverlayRegistry::new(vec![configured]).expect("registry")
}

// Rust guideline compliant 2026-08-31
