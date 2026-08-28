//! Daemon-local cache for configured overlay discovery.
//!
//! The protocol-aware peer probe lives in `pohunek-client` so standalone CLI
//! calls do not need a local daemon. The daemon keeps this in-memory cache for
//! its GUI, web, and `host.discover` RPC consumers.

use std::sync::Arc;
use std::time::Instant;

use pohunek_client::{discover_hosts, OverlayRegistry, DISCOVERY_CACHE_TTL};
use protocol::HostRecord;
use tokio::sync::Mutex;

/// A short-lived, process-local cache of discovery records.
#[derive(Clone, Debug, Default)]
pub struct DiscoveryCache {
    cache: Arc<Mutex<Option<CacheEntry>>>,
    registry: Option<OverlayRegistry>,
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
            cache: Arc::new(Mutex::new(None)),
            registry: Some(registry),
        }
    }

    /// Return cached records or refresh the shared discovery engine.
    ///
    /// The lock deliberately covers a refresh, coalescing concurrent daemon RPC
    /// calls into one bounded mesh scan.
    pub async fn records(
        &self,
        force: bool,
    ) -> Result<Vec<HostRecord>, pohunek_client::ClientError> {
        let registry = self.registry.as_ref().ok_or_else(|| {
            pohunek_client::ClientError::OverlayRegistry(overlay::RegistryError::Empty)
        })?;
        let mut guard = self.cache.lock().await;
        if !force {
            if let Some(entry) = guard.as_ref() {
                if entry.fetched.elapsed() < DISCOVERY_CACHE_TTL {
                    return Ok(entry.records.clone());
                }
            }
        }
        let records = discover_hosts(registry).await?;
        *guard = Some(CacheEntry {
            fetched: Instant::now(),
            records: records.clone(),
        });
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::HostClass;

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
        let cache = DiscoveryCache::default();
        *cache.cache.lock().await = Some(CacheEntry {
            fetched: Instant::now(),
            records: records.clone(),
        });
        assert_eq!(cache.records(false).await.expect("cached records"), records);
    }
}
