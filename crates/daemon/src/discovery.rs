//! Daemon-local cache for configured overlay discovery.
//!
//! The protocol-aware peer probe lives in `pohunek-client` so standalone CLI
//! calls do not need a local daemon. The daemon keeps this in-memory cache for
//! its web and `host.discover` RPC consumers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use pohunek_client::{
    discover_hosts_with_options, DiscoveryOptions, OverlayRegistry, DISCOVERY_CACHE_TTL,
};
use protocol::{HostRecord, ProtocolVersion};
use tokio::sync::Mutex;

/// A short-lived, process-local cache of discovery records.
#[derive(Clone, Debug)]
pub struct DiscoveryCache {
    /// Snapshots keyed by the protocol version they were probed and classified for.
    cache: Arc<Mutex<HashMap<ProtocolVersion, CacheEntry>>>,
    registry: OverlayRegistry,
}

/// One completed discovery snapshot.
#[derive(Debug)]
struct CacheEntry {
    fetched: Instant,
    records: Vec<HostRecord>,
}

impl DiscoveryCache {
    /// Create a cache backed by one validated configured registry.
    #[must_use]
    pub fn new(registry: OverlayRegistry) -> Self {
        Self {
            cache: Arc::new(Mutex::new(HashMap::new())),
            registry,
        }
    }

    /// Return cached records or refresh the shared discovery engine.
    ///
    /// Peers are probed and classified for `version`, the protocol version
    /// negotiated for the asking connection, and snapshots are cached per
    /// version because the classification depends on it. The lock deliberately
    /// covers a refresh, coalescing concurrent daemon RPC calls into one
    /// bounded mesh scan.
    pub async fn records(
        &self,
        force: bool,
        version: ProtocolVersion,
    ) -> Result<Vec<HostRecord>, pohunek_client::ClientError> {
        let mut guard = self.cache.lock().await;
        if !force {
            if let Some(entry) = guard.get(&version) {
                if entry.fetched.elapsed() < DISCOVERY_CACHE_TTL {
                    return Ok(entry.records.clone());
                }
            }
        }
        let options = DiscoveryOptions::new().with_protocol_version(version);
        let records = discover_hosts_with_options(&self.registry, options).await?;
        guard.insert(
            version,
            CacheEntry {
                fetched: Instant::now(),
                records: records.clone(),
            },
        );
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{HostClass, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};

    #[tokio::test]
    async fn fresh_cache_is_served_without_refresh() {
        let records = vec![HostRecord {
            name: Some("host-b".to_owned()),
            fqdn: Some("host-b.netbird.cloud".to_owned()),
            address: Some("100.92.30.40".to_owned()),
            port: 18722,
            overlay: "netbird".to_owned(),
            peer_id: Some("100.92.30.40".to_owned()),
            class: HostClass::Unreachable,
        }];
        let cache = DiscoveryCache::new(crate::test_support::overlay_registry());
        cache.cache.lock().await.insert(
            PROTOCOL_VERSION,
            CacheEntry {
                fetched: Instant::now(),
                records: records.clone(),
            },
        );
        assert_eq!(
            cache
                .records(false, PROTOCOL_VERSION)
                .await
                .expect("cached records"),
            records
        );
        // A snapshot classified for one version is not served to another.
        assert!(cache
            .cache
            .lock()
            .await
            .get(&MIN_PROTOCOL_VERSION)
            .is_none());
        assert_eq!(
            cache
                .records(false, MIN_PROTOCOL_VERSION)
                .await
                .expect("refreshed for the other version"),
            Vec::new()
        );
    }

    #[tokio::test]
    async fn cache_miss_uses_required_registry() {
        let cache = DiscoveryCache::new(crate::test_support::overlay_registry());
        assert_eq!(
            cache
                .records(false, PROTOCOL_VERSION)
                .await
                .expect("discovery"),
            Vec::new()
        );
    }
}
