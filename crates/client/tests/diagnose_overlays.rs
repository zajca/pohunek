//! `diagnose_overlays` bounds every provider on its own.
//!
//! A provider whose state never resolves must be reported as unavailable after
//! the budget, while a healthy provider next to it still reports its listener.
//! The runtime clock is paused, so the budget is exact and nothing sleeps.

use std::future::pending;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use overlay::{
    BindAddrError, ConfiguredTransport, DiscoveredPeer, ExternalIdentity, OverlayDiagnostic,
    OverlayError, OverlayFuture, OverlayId, OverlayRegistry, OverlayTransport, ResolvedPeer,
};
use pohunek_client::diagnose_overlays;
use tokio::time::Instant;

/// Budget of one provider probe in the scenario.
const BUDGET: Duration = Duration::from_secs(4);

/// Port every fixture overlay is configured with.
const PORT: u16 = 18_722;

/// Address the healthy provider reports.
const READY_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 7));

/// A provider that answers its listener address at once or never.
#[derive(Debug)]
struct Provider {
    id: OverlayId,
    answers: bool,
}

impl OverlayTransport for Provider {
    fn id(&self) -> &OverlayId {
        &self.id
    }

    fn validate_bind_addr(&self, _addr: IpAddr) -> Result<(), BindAddrError> {
        Ok(())
    }

    fn listener_addr(&self) -> OverlayFuture<'_, IpAddr> {
        if self.answers {
            Box::pin(async { Ok(READY_ADDRESS) })
        } else {
            Box::pin(pending())
        }
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

fn registry() -> OverlayRegistry {
    let entries = [("stuck", false), ("ready", true)]
        .into_iter()
        .map(|(id, answers)| {
            let provider = Provider {
                id: OverlayId::new(id).expect("overlay id"),
                answers,
            };
            ConfiguredTransport::new(Arc::new(provider), PORT).expect("configured overlay")
        })
        .collect();
    OverlayRegistry::new(entries).expect("registry")
}

fn diagnostic<'a>(diagnostics: &'a [OverlayDiagnostic], id: &str) -> &'a OverlayDiagnostic {
    diagnostics
        .iter()
        .find(|diagnostic| diagnostic.overlay.as_str() == id)
        .unwrap_or_else(|| panic!("diagnostic of {id} in {diagnostics:?}"))
}

#[tokio::test(start_paused = true)]
async fn a_stuck_provider_times_out_alone_and_a_healthy_one_still_reports() {
    let registry = registry();
    let started = Instant::now();

    // The outer limit turns a missing provider bound into a failure instead of
    // a hang: without a per-provider budget the stuck provider never returns.
    let diagnostics = tokio::time::timeout(BUDGET * 2, diagnose_overlays(&registry, BUDGET))
        .await
        .expect("every provider is bounded by the budget");

    assert_eq!(
        started.elapsed(),
        BUDGET,
        "the stuck provider takes the budget"
    );
    assert_eq!(diagnostics.len(), 2);
    let ready = diagnostic(&diagnostics, "ready");
    assert_eq!(ready.listener, Some((READY_ADDRESS, PORT).into()));
    assert!(ready.error.is_none(), "{ready:?}");
    let stuck = diagnostic(&diagnostics, "stuck");
    assert_eq!(stuck.listener, None);
    assert!(
        matches!(&stuck.error, Some(OverlayError::StateUnavailable { detail, .. }) if detail.contains("did not answer")),
        "{stuck:?}"
    );
}
