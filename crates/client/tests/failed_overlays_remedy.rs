//! A restricted `NetBird` overview next to a missing Tailscale CLI.
//!
//! The multi-failure discovery branch is unreachable through the CLI: the
//! overlay config allows exactly one provider per host. External Rust clients
//! build an [`OverlayRegistry`] from several [`ConfiguredTransport`]s, so the
//! branch is still public behaviour and is proved here at the client boundary.

use std::net::IpAddr;
use std::sync::Arc;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayError,
    OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
};
use pohunek_client::DiscoveryOptions;

/// Port the fixture overlays are configured with.
const PORT: u16 = 18_722;

/// A provider whose discovery fails with its own typed error.
#[derive(Debug)]
struct FailingProvider {
    id: OverlayId,
    error: OverlayError,
}

impl OverlayTransport for FailingProvider {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, _addr: IpAddr) -> Result<(), BindAddrError> {
        Ok(())
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
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
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
}

#[tokio::test]
async fn a_restricted_netbird_among_failed_overlays_keeps_its_remedy() {
    let netbird_id = OverlayId::new("netbird").expect("overlay id");
    let remedy = "list the host under [netbird.static_peers.<name>] in overlays.toml \
        with its address, fqdn and the host_id that `pohunek host governance inspect local --json` \
        prints on that host; or, if this account is the only user of the NetBird connection, \
        have root run `sudo netbird profile claim <profile> --owner <account>` \
        (`netbird profile list --show-owner` shows the active profile and its owner); \
        see docs/knowledge/guides/remote-hosts.md";
    let netbird = FailingProvider {
        error: OverlayError::StateRestricted {
            overlay: netbird_id.clone(),
            detail:
                "this account does not own the active NetBird profile (daemon status: Connected)"
                    .to_owned(),
            remedy: remedy.to_owned(),
        },
        id: netbird_id,
    };
    let tailscale_id = OverlayId::new("tailscale").expect("overlay id");
    let tailscale = FailingProvider {
        error: OverlayError::CliMissing {
            overlay: tailscale_id.clone(),
            refused: None,
        },
        id: tailscale_id,
    };
    let registry = OverlayRegistry::new(vec![
        ConfiguredTransport::new(Arc::new(netbird), PORT).expect("configured netbird"),
        ConfiguredTransport::new(Arc::new(tailscale), PORT).expect("configured tailscale"),
    ])
    .expect("two-provider registry");
    let error = pohunek_client::discover_hosts_with_options(&registry, DiscoveryOptions::new())
        .await
        .expect_err("every overlay fails discovery");
    let protocol = error.to_protocol_error();
    assert_eq!(protocol.code, "overlay_discovery_failed", "{protocol:?}");
    assert!(
        protocol.msg.contains("netbird (netbird_state_restricted)"),
        "{protocol:?}"
    );
    assert!(
        protocol.msg.contains("tailscale (tailscale_cli_missing)"),
        "{protocol:?}"
    );
    let recover = protocol.recover.expect("recover hint");
    assert!(
        recover.contains("netbird: list the host under [netbird.static_peers.<name>]"),
        "{recover}"
    );
}
