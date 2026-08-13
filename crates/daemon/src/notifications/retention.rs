//! Daemon-owned automatic notification retention.

// Rust guideline compliant 2026-08-13

use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::NotificationService;

/// Maximum time to wait for a running retention sweep during daemon shutdown.
///
/// Sweeps only touch the owner-private local JSONL store. Five seconds matches
/// the other notification and event-log shutdown budgets while bounding exit.
const RETENTION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Background task that applies the current notification retention policy.
#[derive(Debug)]
pub struct NotificationRetentionTask {
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
}

impl NotificationRetentionTask {
    /// Spawn automatic notification maintenance.
    ///
    /// The first sweep runs immediately so an upgraded daemon cleans stale
    /// records without waiting a full interval. Each later delay is loaded from
    /// the current policy, allowing policy changes without restarting.
    #[must_use]
    pub fn spawn(notifications: NotificationService) -> Self {
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let handle = tokio::spawn(async move {
            loop {
                let service = notifications.clone();
                match tokio::task::spawn_blocking(move || {
                    service.run_auto_retention_at(time::OffsetDateTime::now_utc())
                })
                .await
                {
                    Ok(Ok(result)) => {
                        if result.pruned > 0 || result.compacted {
                            info!(
                                notification.pruned = result.pruned,
                                notification.compacted = result.compacted,
                                "completed automatic notification retention"
                            );
                        }
                    }
                    Ok(Err(err)) => {
                        warn!(error = %err, "automatic notification retention failed");
                    }
                    Err(err) => {
                        warn!(error = %err, "automatic notification retention task panicked");
                    }
                }

                let interval = Duration::from_secs(u64::from(
                    notifications.policy().retention.sweep_interval_secs,
                ));
                tokio::select! {
                    () = task_shutdown.cancelled() => break,
                    () = tokio::time::sleep(interval) => {}
                }
            }
        });
        Self { shutdown, handle }
    }

    /// Stop the task and wait for any in-flight sweep to finish.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        match tokio::time::timeout(RETENTION_SHUTDOWN_TIMEOUT, self.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(error = %err, "notification retention task failed during shutdown");
            }
            Err(_) => {
                warn!("notification retention task did not finish within the shutdown timeout");
            }
        }
    }
}
