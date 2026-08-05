//! Daemon-local cache for shared `NetBird` host discovery.
//!
//! The protocol-aware peer probe lives in `pohunek-client` so standalone CLI
//! calls do not need a local daemon. The daemon keeps this in-memory cache for
//! its GUI, web, and `host.discover` RPC consumers.

use std::sync::Arc;
use std::time::Instant;

use pohunek_client::{discover_hosts, DISCOVERY_CACHE_TTL};
use protocol::HostRecord;
use tokio::sync::Mutex;

/// A short-lived, process-local cache of discovery records.
#[derive(Clone, Default, Debug)]
pub struct DiscoveryCache(Arc<Mutex<Option<CacheEntry>>>);

/// One completed discovery snapshot.
#[derive(Debug)]
struct CacheEntry {
    fetched: Instant,
    records: Vec<HostRecord>,
}

impl DiscoveryCache {
    /// Return cached records or refresh the shared discovery engine.
    ///
    /// The lock deliberately covers a refresh, coalescing concurrent daemon RPC
    /// calls into one bounded mesh scan.
    pub async fn records(
        &self,
        force: bool,
    ) -> Result<Vec<HostRecord>, pohunek_client::ClientError> {
        let mut guard = self.0.lock().await;
        if !force {
            if let Some(entry) = guard.as_ref() {
                if entry.fetched.elapsed() < DISCOVERY_CACHE_TTL {
                    return Ok(entry.records.clone());
                }
            }
        }
        let records = discover_hosts().await?;
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
            netbird_ip: Some("100.92.30.40".to_owned()),
            class: HostClass::Unreachable,
        }];
        let cache = DiscoveryCache::default();
        *cache.0.lock().await = Some(CacheEntry {
            fetched: Instant::now(),
            records: records.clone(),
        });
        assert_eq!(cache.records(false).await.expect("cached records"), records);
    }
}
