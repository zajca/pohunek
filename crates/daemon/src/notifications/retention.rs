//! Daemon-owned notification retention sweep.

// Rust guideline compliant 2026-08-13

use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::NotificationService;
use crate::shutdown::{DaemonShutdown, DriverHandle, Stage};

/// Spawns the retention sweep.
///
/// The first sweep runs immediately so an upgraded daemon removes the records
/// the inbox does not hold without waiting a full interval. The interval is
/// the service's `record_sweep_interval_secs`. The task is a driver of `tasks`:
/// daemon shutdown cancels it and waits for an in-flight sweep, which only
/// touches the owner-private local JSONL store.
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
            let interval = Duration::from_secs(notifications.settings().record_sweep_interval_secs);
            loop {
                let service = notifications.clone();
                match tokio::task::spawn_blocking(move || {
                    service.run_retention_at(time::OffsetDateTime::now_utc())
                })
                .await
                {
                    Ok(Ok(outcome)) => {
                        if outcome.removed > 0 {
                            info!(
                                notification.removed = outcome.removed,
                                notification.announced = outcome.announced,
                                "completed notification retention sweep"
                            );
                        }
                    }
                    Ok(Err(err)) => {
                        warn!(error = %err, "notification retention sweep failed");
                    }
                    Err(err) => {
                        warn!(error = %err, "notification retention task panicked");
                    }
                }
                tokio::select! {
                    () = task_shutdown.cancelled() => break,
                    () = tokio::time::sleep(interval) => {}
                }
            }
        },
    )
}
