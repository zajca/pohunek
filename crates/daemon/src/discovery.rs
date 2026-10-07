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
use protocol::{HostRecord, ProtocolVersionRange, SUPPORTED_PROTOCOL_VERSIONS};
use tokio::sync::Mutex;

/// A short-lived, process-local cache of discovery records.
#[derive(Clone, Debug)]
pub struct DiscoveryCache {
    /// Snapshots keyed by the protocol range they were probed and classified for.
    cache: Arc<Mutex<HashMap<ProtocolVersionRange, CacheEntry>>>,
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
    /// Peers are probed and classified for `asked`, the protocol range the
    /// asking client advertised, narrowed to this daemon's window
    /// ([`clamp_to_window`]). Snapshots are cached per narrowed range because
    /// the classification depends on it; the window bounds how many ranges
    /// exist, so a peer cannot grow the cache or force rescans by varying the
    /// range it advertises. The lock deliberately covers a refresh, coalescing
    /// concurrent daemon RPC calls into one bounded mesh scan.
    ///
    /// # Errors
    ///
    /// Returns [`pohunek_client::ClientError::InvalidDiscoveryOptions`] when
    /// `asked` does not overlap the window, and the discovery errors otherwise.
    pub async fn records(
        &self,
        force: bool,
        asked: ProtocolVersionRange,
    ) -> Result<Vec<HostRecord>, pohunek_client::ClientError> {
        let range = clamp_to_window(asked).ok_or_else(|| {
            pohunek_client::ClientError::InvalidDiscoveryOptions {
                detail: "the asking client's protocol range does not overlap this daemon's window"
                    .to_owned(),
            }
        })?;
        let mut guard = self.cache.lock().await;
        if !force {
            if let Some(entry) = guard.get(&range) {
                if entry.fetched.elapsed() < DISCOVERY_CACHE_TTL {
                    return Ok(entry.records.clone());
                }
            }
        }
        let options = DiscoveryOptions::new().with_protocol_range(range);
        let records = discover_hosts_with_options(&self.registry, options).await?;
        guard.insert(
            range,
            CacheEntry {
                fetched: Instant::now(),
                records: records.clone(),
            },
        );
        Ok(records)
    }
}

/// Narrows `asked` to this daemon's protocol window.
///
/// Returns `None` when the ranges do not overlap. The result is one of the
/// few ranges inside the window, which keeps the per-range cache bounded.
fn clamp_to_window(asked: ProtocolVersionRange) -> Option<ProtocolVersionRange> {
    ProtocolVersionRange::new(
        asked.minimum().max(SUPPORTED_PROTOCOL_VERSIONS.minimum()),
        asked.maximum().min(SUPPORTED_PROTOCOL_VERSIONS.maximum()),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{HostClass, CLIENT_PROTOCOL_VERSIONS, CURRENT_PROTOCOL_VERSIONS};

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
            CURRENT_PROTOCOL_VERSIONS,
            CacheEntry {
                fetched: Instant::now(),
                records: records.clone(),
            },
        );
        assert_eq!(
            cache
                .records(false, CURRENT_PROTOCOL_VERSIONS)
                .await
                .expect("cached records"),
            records
        );
        // A snapshot classified for one range is not served to another.
        assert!(cache
            .cache
            .lock()
            .await
            .get(&CLIENT_PROTOCOL_VERSIONS)
            .is_none());
        assert_eq!(
            cache
                .records(false, CLIENT_PROTOCOL_VERSIONS)
                .await
                .expect("refreshed for the other range"),
            Vec::new()
        );
    }

    fn range(minimum: u32, maximum: u32) -> ProtocolVersionRange {
        ProtocolVersionRange::new(
            protocol::ProtocolVersion::new(minimum).expect("nonzero"),
            protocol::ProtocolVersion::new(maximum).expect("nonzero"),
        )
        .expect("ordered range")
    }

    #[test]
    fn overlapping_ranges_collapse_into_the_window() {
        let window = SUPPORTED_PROTOCOL_VERSIONS;
        let (low, high) = (window.minimum().get(), window.maximum().get());
        for asked in [
            range(low, high),
            range(low - 1, high),
            range(low, high + 7),
            range(low - 1, high + 7),
        ] {
            assert_eq!(clamp_to_window(asked), Some(window), "{asked:?}");
        }
        assert_eq!(clamp_to_window(range(high, high)), Some(range(high, high)));
        assert_eq!(clamp_to_window(range(low, low)), Some(range(low, low)));
        assert_eq!(clamp_to_window(range(low - 1, low - 1)), None);
        assert_eq!(clamp_to_window(range(high + 1, high + 9)), None);
    }

    #[tokio::test]
    async fn varied_advertised_ranges_share_one_cache_entry() {
        let window = SUPPORTED_PROTOCOL_VERSIONS;
        let cache = DiscoveryCache::new(crate::test_support::overlay_registry());
        for asked in [
            range(window.minimum().get() - 1, window.maximum().get()),
            range(window.minimum().get(), window.maximum().get() + 5),
            window,
        ] {
            cache.records(false, asked).await.expect("discovery");
        }
        let keys: Vec<ProtocolVersionRange> = cache.cache.lock().await.keys().copied().collect();
        assert_eq!(keys, vec![window], "one snapshot serves every wider range");
    }

    #[tokio::test]
    async fn a_range_outside_the_window_is_refused() {
        let window = SUPPORTED_PROTOCOL_VERSIONS;
        let cache = DiscoveryCache::new(crate::test_support::overlay_registry());
        let beyond = window.maximum().get() + 1;
        let error = cache
            .records(false, range(beyond, beyond + 1))
            .await
            .expect_err("no overlap with the window");
        assert!(
            matches!(
                error,
                pohunek_client::ClientError::InvalidDiscoveryOptions { .. }
            ),
            "{error:?}"
        );
        assert!(cache.cache.lock().await.is_empty());
    }
}
