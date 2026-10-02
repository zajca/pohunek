//! Shared validated overlay registry for daemon integration tests.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
    OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
};
use pohunek_daemon::runtime::EnvironmentSource;
use pohunek_daemon::session::{SessionRegistryConfig, ShellCommand};
use pohunek_test_support::env::TestEnv;

std::thread_local! {
    /// The hermetic environment of the current test thread.
    static THREAD_ENV: std::cell::OnceCell<TestEnv> = const { std::cell::OnceCell::new() };
}

/// Variables of the current test thread's [`TestEnv`] as the source of the
/// agent children's base environment.
///
/// A session child then sees the fixture's private `HOME` and none of the
/// developer's variables. The environment lives until the test thread ends.
pub(crate) fn hermetic_environment_source() -> EnvironmentSource {
    THREAD_ENV.with(|cell| {
        let env = cell.get_or_init(|| TestEnv::new().expect("create the thread test environment"));
        EnvironmentSource::fixed(env.environment().clone())
    })
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
