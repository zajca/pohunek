//! Daemon-owned automatic notification retention.

// Rust guideline compliant 2026-08-13

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::NotificationService;
use crate::shutdown::{DaemonShutdown, DriverHandle, Stage};

/// Spawns automatic notification maintenance.
///
/// The first sweep runs immediately so an upgraded daemon cleans stale
/// records without waiting a full interval. Each later delay is loaded from
/// the current policy, allowing policy changes without restarting. The task is
/// a driver of `tasks`: daemon shutdown cancels it and waits for an in-flight
/// sweep, which only touches the owner-private local JSONL store.
#[expect(
    clippy::must_use_candidate,
    reason = "the handle only stops the task before daemon shutdown does"
)]
pub fn spawn_notification_retention(
    tasks: &DaemonShutdown,
    notifications: NotificationService,
) -> DriverHandle {
    let shutdown = CancellationToken::new();
    let task_shutdown = shutdown.clone();
    tasks.spawn(
        "notification_retention",
        Stage::Driver,
        move || shutdown.cancel(),
        async move {
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
        },
    )
}
