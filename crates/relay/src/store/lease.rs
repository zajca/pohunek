//! Implements the database-backed relay fence.

// Rust guideline compliant 2026-09-08

use std::time::Duration;

use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{Store, StoreError};

/// RFC-required lifetime for an active relay fence.
const LEASE_DURATION: Duration = Duration::from_secs(5);

/// Proves that this process currently owns a relay fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseGuard {
    relay_id: String,
    process_instance_id: Uuid,
    fence_token: Uuid,
    recovery_generation: i64,
    expires_at: OffsetDateTime,
}

impl Store {
    /// Locks one relay identity for a transition that excludes lease acquisition.
    ///
    /// Every lease acquisition takes this lock before it observes or replaces a
    /// lease row. Recovery transitions take the same lock before they reject a
    /// live lease, so the decision and the following mutation have one fence.
    ///
    /// # Errors
    /// Returns [`StoreError::StaleState`] when the relay identity is absent.
    pub(crate) async fn lock_relay_identity_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        relay_id: &str,
    ) -> Result<(), StoreError> {
        let locked = sqlx::query_scalar::<_, String>(
            "SELECT relay_id FROM relay_identity WHERE relay_id = $1 FOR UPDATE",
        )
        .bind(relay_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::Database)?;
        if locked.is_some() {
            Ok(())
        } else {
            Err(StoreError::StaleState)
        }
    }

    /// Verifies the exact unexpired process fence while a mutation transaction holds it.
    pub(crate) async fn verify_lease_in_transaction(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        guard: &LeaseGuard,
    ) -> Result<(), StoreError> {
        let valid = sqlx::query_scalar::<_, Uuid>(
            "SELECT fence_token FROM relay_lease WHERE relay_id=$1 AND process_instance_id=$2 AND fence_token=$3 AND recovery_generation=$4 AND expires_at > clock_timestamp() FOR KEY SHARE",
        )
        .bind(&guard.relay_id)
        .bind(guard.process_instance_id)
        .bind(guard.fence_token)
        .bind(guard.recovery_generation)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(StoreError::Database)?;
        if valid.is_some() {
            Ok(())
        } else {
            Err(StoreError::StaleState)
        }
    }

    /// Acquires an expired relay lease or creates its first lease row.
    ///
    /// # Errors
    /// Returns [`StoreError::Forbidden`] while a different current fence exists.
    pub async fn acquire_lease(
        &self,
        relay_id: &str,
        process_instance_id: Uuid,
        recovery_generation: i64,
    ) -> Result<LeaseGuard, StoreError> {
        let fence_token = Uuid::now_v7();
        let mut transaction = self
            .fence_pool
            .begin()
            .await
            .map_err(StoreError::Database)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *transaction)
            .await
            .map_err(StoreError::Database)?;
        self.lock_relay_identity_in_transaction(&mut transaction, relay_id)
            .await?;
        let generation_is_current = sqlx::query_scalar::<_, bool>(
            "SELECT recovery_generation = $2 AND state = 'normal' FROM relay_identity WHERE relay_id = $1",
        )
        .bind(relay_id)
        .bind(recovery_generation)
        .fetch_one(&mut *transaction)
        .await
        .map_err(StoreError::Database)?;
        if !generation_is_current {
            return Err(StoreError::StaleState);
        }
        let row = sqlx::query(
            "INSERT INTO relay_lease (relay_id, process_instance_id, fence_token, recovery_generation, expires_at, heartbeat_sequence) \
             VALUES ($1, $2, $3, $4, clock_timestamp() + interval '5 seconds', 1) \
             ON CONFLICT (relay_id) DO UPDATE SET process_instance_id = EXCLUDED.process_instance_id, \
             fence_token = EXCLUDED.fence_token, recovery_generation = EXCLUDED.recovery_generation, \
             expires_at = clock_timestamp() + interval '5 seconds', heartbeat_sequence = relay_lease.heartbeat_sequence + 1, updated_at = clock_timestamp() \
             WHERE relay_lease.expires_at <= clock_timestamp() \
             RETURNING fence_token, expires_at",
        ).bind(relay_id).bind(process_instance_id).bind(fence_token).bind(recovery_generation)
            .fetch_optional(&mut *transaction).await.map_err(StoreError::Database)?
            .ok_or(StoreError::Forbidden)?;
        transaction.commit().await.map_err(StoreError::Database)?;
        Ok(LeaseGuard {
            relay_id: relay_id.to_owned(),
            process_instance_id,
            fence_token: row.get("fence_token"),
            recovery_generation,
            expires_at: row.get("expires_at"),
        })
    }

    /// Renews the fence only while its previous deadline is current.
    pub async fn renew_lease(&self, guard: &LeaseGuard) -> Result<LeaseGuard, StoreError> {
        let row = sqlx::query(
            "UPDATE relay_lease SET expires_at = clock_timestamp() + interval '5 seconds', heartbeat_sequence = heartbeat_sequence + 1, updated_at = clock_timestamp() \
             WHERE relay_id = $1 AND process_instance_id = $2 AND fence_token = $3 AND recovery_generation = $4 AND expires_at > clock_timestamp() \
             RETURNING expires_at",
        ).bind(&guard.relay_id).bind(guard.process_instance_id).bind(guard.fence_token).bind(guard.recovery_generation)
            .fetch_optional(&self.fence_pool).await.map_err(StoreError::Database)?
            .ok_or(StoreError::StaleState)?;
        Ok(LeaseGuard {
            relay_id: guard.relay_id.clone(),
            process_instance_id: guard.process_instance_id,
            fence_token: guard.fence_token,
            recovery_generation: guard.recovery_generation,
            expires_at: row.get("expires_at"),
        })
    }

    /// Verifies request authority using only application connection capacity.
    pub async fn validate_lease(&self, guard: &LeaseGuard) -> Result<(), StoreError> {
        validate_lease(&self.pool, guard).await
    }

    /// Uses reserved capacity exclusively for the process watchdog and lifecycle.
    pub(crate) async fn validate_control_lease(
        &self,
        guard: &LeaseGuard,
    ) -> Result<(), StoreError> {
        validate_lease(&self.fence_pool, guard).await
    }

    /// Expires this fence without releasing a successor's fence.
    ///
    /// Retaining the identity row preserves the RFC heartbeat sequence across
    /// ordinary restarts. A lost fence is never treated as a clean release.
    ///
    /// # Errors
    /// Returns [`StoreError::StaleState`] when this process no longer owns the
    /// exact current fence.
    pub async fn release_lease(&self, guard: &LeaseGuard) -> Result<(), StoreError> {
        let released = sqlx::query(
            "UPDATE relay_lease SET expires_at=clock_timestamp(),updated_at=clock_timestamp() WHERE relay_id=$1 AND process_instance_id=$2 AND fence_token=$3 AND recovery_generation=$4",
        )
        .bind(&guard.relay_id)
        .bind(guard.process_instance_id)
        .bind(guard.fence_token)
        .bind(guard.recovery_generation)
        .execute(&self.fence_pool)
        .await
        .map_err(StoreError::Database)?;
        if released.rows_affected() == 1 {
            Ok(())
        } else {
            Err(StoreError::StaleState)
        }
    }
}

impl LeaseGuard {
    /// Returns the process-local current deadline.
    #[must_use]
    pub const fn expires_at(&self) -> OffsetDateTime {
        self.expires_at
    }

    /// Returns the configured RFC lease duration.
    #[must_use]
    pub const fn duration() -> Duration {
        LEASE_DURATION
    }
}

async fn validate_lease(pool: &sqlx::PgPool, guard: &LeaseGuard) -> Result<(), StoreError> {
    if guard.expires_at <= OffsetDateTime::now_utc() {
        return Err(StoreError::StaleState);
    }
    let valid = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM relay_lease WHERE relay_id = $1 AND process_instance_id = $2 AND fence_token = $3 AND recovery_generation = $4 AND expires_at > clock_timestamp())",
        ).bind(&guard.relay_id).bind(guard.process_instance_id).bind(guard.fence_token).bind(guard.recovery_generation)
            .fetch_one(pool).await.map_err(StoreError::Database)?;
    if valid {
        Ok(())
    } else {
        Err(StoreError::StaleState)
    }
}
