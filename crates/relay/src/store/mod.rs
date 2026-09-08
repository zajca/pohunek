//! Persists relay authority in `PostgreSQL`.
//!
//! This module is the only SQL boundary for team authority. Callers pass an
//! authenticated context created inside `auth`, never request-body actor data.

// Rust guideline compliant 2026-09-08

use std::time::Duration;

use sqlx::{postgres::PgPoolOptions, PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

pub mod lease;

/// Embedded, versioned `PostgreSQL` schema contract for the relay authority.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// A renewal and watchdog must each retain one independent database slot.
const MIN_FENCE_CONNECTIONS: u32 = 2;
/// Cancellation needs at least one slot independent of public requests.
const MIN_RESERVED_CONNECTIONS: u32 = 1;

/// Holds `PostgreSQL` state for one relay process.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
    fence_pool: PgPool,
    reserved_pool: PgPool,
}

/// Identifies an authenticated caller without exposing construction to HTTP input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActorContext {
    principal_id: Uuid,
    authentication_id: Uuid,
    authentication_generation: i64,
    recovery_generation: i64,
    kind: ActorKind,
    binding: AuthenticationBinding,
}

/// Distinguishes authenticated principal sources in durable audit records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    /// A validated human credential.
    Human,
    /// A validated service credential.
    Service,
    /// A protected local infrastructure procedure.
    Infrastructure,
}

/// Names the durable authentication row which created an actor context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationBinding {
    /// A native or service credential row.
    Credential,
    /// An opaque browser session row.
    BrowserSession,
}

/// Identifies a requested authorization resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceScope {
    /// A permission applying to the whole team.
    Team,
    /// A host reference reserved for later enrollment.
    Host(String),
    /// An owner-approved host share reference.
    HostShare(String),
    /// A team project reference.
    Project(String),
    /// A relay session reference.
    Session(String),
}

/// Contains a bounded audit-safe authorization request.
#[derive(Debug, Clone)]
pub struct AuthorizationRequest {
    /// Current authenticated caller.
    pub actor: ActorContext,
    /// Team which owns the requested resource.
    pub team_id: Uuid,
    /// Stable permission name seeded by migration.
    pub permission: String,
    /// Requested resource scope.
    pub resource: ResourceScope,
    /// Correlates one request without carrying user content.
    pub correlation_id: Uuid,
}

/// Returns the committed current authorization coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationDecision {
    /// Durable decision record.
    pub audit_id: Uuid,
    /// Current team policy generation.
    pub policy_generation: i64,
    /// Current relay recovery generation.
    pub recovery_generation: i64,
}

/// Reports a bounded `PostgreSQL` authority failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// `PostgreSQL` was unavailable or rejected an operation.
    #[error("relay database operation failed")]
    Database(#[source] sqlx::Error),
    /// The embedded schema was incompatible with the `PostgreSQL` database.
    #[error("relay database migration failed")]
    Migration(#[source] sqlx::migrate::MigrateError),
    /// A required durable audit record could not be committed.
    #[error("durable audit is unavailable")]
    AuditUnavailable,
    /// The current actor no longer has authority.
    #[error("current actor is not authorized")]
    Forbidden,
    /// An optimistic revision or recovery generation changed.
    #[error("relay state changed before the operation completed")]
    StaleState,
    /// A retry-safe operation continued to conflict.
    #[error("relay transaction remained contended")]
    Contended,
    /// Caller data exceeded a fixed safe bound.
    #[error("relay input exceeds its allowed bound")]
    InputTooLarge,
    /// An idempotency key was replayed with a different request body.
    #[error("idempotency key does not match the original request")]
    IdempotencyConflict,
}

impl Store {
    /// Connects a bounded `PostgreSQL` pool.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] when `PostgreSQL` cannot be connected.
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self, StoreError> {
        // Library fixtures and local tooling retain the minimum two concurrent
        // fence slots (renewal + watchdog) and one reserved cancellation slot.
        Self::connect_with_limits(
            database_url,
            max_connections,
            MIN_FENCE_CONNECTIONS,
            MIN_RESERVED_CONNECTIONS,
        )
        .await
    }

    /// Connects independent application, fence, and cancellation/audit pools.
    pub async fn connect_with_limits(
        database_url: &str,
        application: u32,
        fence: u32,
        reserved: u32,
    ) -> Result<Self, StoreError> {
        if application == 0 || fence < MIN_FENCE_CONNECTIONS || reserved < MIN_RESERVED_CONNECTIONS
        {
            return Err(StoreError::InputTooLarge);
        }
        let pool = connect_pool(database_url, application, 1).await?;
        let fence_pool = connect_pool(database_url, fence, fence).await?;
        let reserved_pool = connect_pool(database_url, reserved, 1).await?;
        Ok(Self {
            pool,
            fence_pool,
            reserved_pool,
        })
    }

    /// Begins a reserved transaction for cancellation and its durable audit.
    pub(crate) async fn begin_reserved(&self) -> Result<Transaction<'_, Postgres>, StoreError> {
        let mut transaction = self
            .reserved_pool
            .begin()
            .await
            .map_err(StoreError::Database)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *transaction)
            .await
            .map_err(StoreError::Database)?;
        Ok(transaction)
    }

    /// Verifies that `PostgreSQL` remains reachable.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] when `PostgreSQL` rejects the health query.
    pub async fn healthcheck(&self) -> Result<(), StoreError> {
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map(|_| ())
            .map_err(StoreError::Database)
    }

    /// Applies the embedded `PostgreSQL` schema migrations.
    ///
    /// The migration set is compiled into the relay binary so startup never
    /// depends on a source checkout or a mutable external SQL directory.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] when a migration cannot complete.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let mut connection = self.pool.acquire().await.map_err(StoreError::Database)?;
        // SQLx early migration errors can leave a session advisory lock held.
        // Retire this connection even if its migration future is cancelled.
        connection.close_on_drop();
        MIGRATOR
            .run_direct(None, &mut *connection, false)
            .await
            .map_err(StoreError::Migration)
    }

    /// Commits a local migration latch to the exact ordered embedded migration set.
    pub(crate) fn migration_plan_digest() -> [u8; 32] {
        use sha2::{Digest as _, Sha256};
        let mut digest = Sha256::new();
        digest.update(b"pohunek.relay.migration-plan.v1\0");
        for migration in MIGRATOR.iter() {
            digest.update(migration.version.to_be_bytes());
            digest.update((migration.checksum.len() as u64).to_be_bytes());
            digest.update(&migration.checksum);
        }
        digest.finalize().into()
    }

    /// Returns the crate-internal pool for authenticated repository operations.
    #[must_use]
    pub(crate) const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Begins a serializable mutation transaction.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] when `PostgreSQL` cannot begin the transaction.
    pub(crate) async fn begin_serializable(&self) -> Result<Transaction<'_, Postgres>, StoreError> {
        let mut transaction = self.pool.begin().await.map_err(StoreError::Database)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *transaction)
            .await
            .map_err(StoreError::Database)?;
        Ok(transaction)
    }

    /// Constructs context only after revalidating its auth row inside `auth`.
    pub(crate) const fn authenticated_actor(
        principal_id: Uuid,
        authentication_id: Uuid,
        authentication_generation: i64,
        recovery_generation: i64,
        kind: ActorKind,
        binding: AuthenticationBinding,
    ) -> ActorContext {
        ActorContext {
            principal_id,
            authentication_id,
            authentication_generation,
            recovery_generation,
            kind,
            binding,
        }
    }
}

async fn connect_pool(
    database_url: &str,
    maximum: u32,
    minimum: u32,
) -> Result<PgPool, StoreError> {
    // Local tooling and startup connections are bounded independently of the
    // tighter runtime renewal/watchdog deadlines that wrap every fence query.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    PgPoolOptions::new()
        .max_connections(maximum)
        .min_connections(minimum)
        .acquire_timeout(CONNECT_TIMEOUT)
        .connect(database_url)
        .await
        .map_err(StoreError::Database)
}

impl ActorContext {
    /// Returns the authenticated principal coordinate.
    #[must_use]
    pub const fn principal_id(self) -> Uuid {
        self.principal_id
    }

    /// Returns the source authentication coordinate validated by auth.
    #[must_use]
    pub const fn authentication_id(self) -> Uuid {
        self.authentication_id
    }

    /// Returns the current source authentication generation.
    #[must_use]
    pub const fn authentication_generation(self) -> i64 {
        self.authentication_generation
    }

    /// Returns the recovery generation validated by auth.
    #[must_use]
    pub const fn recovery_generation(self) -> i64 {
        self.recovery_generation
    }

    /// Returns the authenticated actor kind.
    #[must_use]
    pub const fn kind(self) -> ActorKind {
        self.kind
    }

    /// Returns the durable source record type.
    #[must_use]
    pub const fn binding(self) -> AuthenticationBinding {
        self.binding
    }
}

pub(crate) fn actor_kind_name(kind: ActorKind) -> &'static str {
    match kind {
        ActorKind::Human => "human",
        ActorKind::Service => "service",
        ActorKind::Infrastructure => "infrastructure",
    }
}

pub(crate) fn bounded_coordinate(value: &str) -> Result<&str, StoreError> {
    const MAX_COORDINATE_BYTES: usize = 256;
    if value.is_empty() || value.len() > MAX_COORDINATE_BYTES {
        return Err(StoreError::InputTooLarge);
    }
    Ok(value)
}
