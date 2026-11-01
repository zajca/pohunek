//! Discovery with a provider whose own-identity lookup never answers.
//!
//! A provider that does not read its peers and its own identity in one pass
//! contributes no local host record, and its stalled identity lookup must not
//! delay or drop the peers it enumerated.

// Rust guideline compliant 2026-10-20

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, LocalIdentity,
    OverlayError, OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
};
use pohunek_client::{discover_hosts_with_options, DiscoveryOptions, OriginSource};

/// Discovery deadline of the scenario: short, so a regression that waits for
/// the stalled lookup fails without a long wait.
const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct StalledIdentity {
    id: OverlayId,
}

impl OverlayTransport for StalledIdentity {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, addr: IpAddr) -> Result<(), BindAddrError> {
        Err(BindAddrError::NotMember(addr))
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        Box::pin(async { Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)) })
    }

    fn resolve_peer<'a>(&'a self, host: &'a str) -> OverlayFuture<'a, ResolvedPeer> {
        let error = OverlayError::HostUnknown {
            host: host.to_owned(),
            overlay: self.id.clone(),
        };
        Box::pin(async move { Err(error) })
    }

    fn resolve_peer_identity<'a>(
        &'a self,
        identity: &'a ExternalIdentity,
    ) -> OverlayFuture<'a, ResolvedPeer> {
        let error = OverlayError::HostUnknown {
            host: identity.value().to_owned(),
            overlay: self.id.clone(),
        };
        Box::pin(async move { Err(error) })
    }

    fn discover_peers(&self) -> OverlayFuture<'_, Vec<DiscoveredPeer>> {
        let peer = DiscoveredPeer {
            peer_id: Some("peer-key".to_owned()),
            display_name: Some("peer-b".to_owned()),
            fqdn: Some("peer-b.overlay.test".to_owned()),
            address: None,
            address_ambiguous: false,
        };
        Box::pin(async move { Ok(vec![peer]) })
    }

    fn local_identity(&self) -> OverlayFuture<'_, Option<LocalIdentity>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_stalled_identity_lookup_neither_delays_nor_drops_the_peers() {
    let transport = StalledIdentity {
        id: OverlayId::new("stub").expect("overlay id"),
    };
    let registry =
        OverlayRegistry::new(vec![
            ConfiguredTransport::new(Arc::new(transport), 18_722).expect("configured transport")
        ])
        .expect("registry");
    let options = DiscoveryOptions::new()
        .with_origin_source(OriginSource::Omitted)
        .with_deadline(DEADLINE)
        .expect("deadline");

    let records = discover_hosts_with_options(&registry, options)
        .await
        .expect("the enumerated peers are returned");

    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].peer_id.as_deref(), Some("peer-key"));
    assert!(
        !records[0].local,
        "a provider without the snapshot adds no local record"
    );
}
