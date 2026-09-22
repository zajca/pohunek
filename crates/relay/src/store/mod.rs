//! Persists relay authority in `PostgreSQL`.
//!
//! This module is the only SQL boundary for team authority. Callers pass an
//! authenticated context created inside `auth`, never request-body actor data.

// Rust guideline compliant 2026-09-08

use std::time::Duration;

use sha2::{Digest as _, Sha256};
use sqlx::{postgres::PgPoolOptions, AssertSqlSafe, PgPool, Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;

pub mod lease;

/// Embedded, versioned `PostgreSQL` schema contract for the relay authority.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// A renewal and watchdog must each retain one independent database slot.
const MIN_FENCE_CONNECTIONS: u32 = 2;
/// Cancellation needs at least one slot independent of public requests.
const MIN_RESERVED_CONNECTIONS: u32 = 1;
/// Limits migration catalog input independently from a restored database.
const MAX_MIGRATION_TABLES: usize = 128;
/// Binds prefix hashes to the exact embedded migration representation.
const MIGRATION_PREFIX_DOMAIN: &[u8] = b"pohunek.relay.migration-prefix.v1\0";
/// Binds SQL-side row digests to the local additive migration invariant.
const MIGRATION_AUTHORITY_DOMAIN: &[u8] = b"pohunek.relay.migration-authority.v1\0";
/// Domain separator for the column-projected authority digest.
const MIGRATION_PROJECTION_DOMAIN: &[u8] = b"pohunek.relay.migration-projection.v1\0";

/// Authority tables and the exact columns the projected digest binds.
///
/// Each column list is the one the migration that created the table declared,
/// so a later plan step that adds a column does not change the projection. That
/// is what makes this digest reproducible on both sides of a migration, unlike
/// the whole-row digest, which only holds while the base schema is intact.
///
/// A table absent from the schema hashes identically to a table with no rows.
/// A plan step therefore may create a table, which starts empty, but nothing may
/// insert into it before the migration completes.
///
/// A plan that must rewrite a projected value is refused by this binding. Such a
/// change has to ship as a schema-only migration followed by an audited
/// application mutation, because only the application path records who changed
/// authority and why.
const AUTHORITY_PROJECTION: &[(&str, &str)] = &[
    ("account_link_transactions", "link_id,principal_id,source_identity_id,account_link_generation,recovery_generation,created_at,expires_at,completed_at"),
    ("admission_rule_versions", "admission_rule_id,revision,team_id,provider,method,match_value,state,created_at"),
    ("admission_rules", "admission_rule_id,team_id,provider,method,match_value,state,current_revision,created_at,updated_at"),
    ("audit_events", "audit_id,occurred_at,actor_principal_id,actor_kind,team_id,action,decision,policy_generation,recovery_generation,parameter_code,parameter_value,outcome"),
    ("browser_sessions", "session_id,cookie_digest,csrf_digest,principal_id,identity_id,digest_key_id,session_generation,recovery_generation,expires_at,idle_deadline,revoked_at"),
    ("cancellation_outbox", "revocation_id,scope_kind,scope_id,team_id,committed_at,delivered_at"),
    ("custom_role_permissions", "team_id,role_id,permission"),
    ("custom_roles", "team_id,role_id,display_name,state,revision,created_at"),
    ("evidence_results", "evidence_id,challenge_id,principal_id,team_id,admission_rule_id,admission_rule_revision,account_link_generation,recovery_generation,outcome,checked_at,expires_at"),
    ("grants", "team_id,grant_id,subject_kind,subject_id,resource_kind,resource_id,permission,state,revision,created_at"),
    ("group_members", "team_id,group_id,principal_id,revision,created_at"),
    ("groups", "team_id,group_id,display_name,state,revision,created_at"),
    ("host_ownership_references", "host_id,relay_id,owner_principal_id,owner_team_id,owner_revision,state,created_at"),
    ("membership_custom_roles", "team_id,principal_id,role_id"),
    ("memberships", "membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation,created_at,removed_at"),
    ("mutation_receipts", "actor_principal_id,action,idempotency_key,request_digest,audit_id,outcome_code,result_id,result_revision,created_at"),
    ("oidc_identities", "identity_id,issuer,subject,principal_id,link_generation,created_at,removed_at"),
    ("principals", "id,kind,state,generation,created_at,deprovisioned_at"),
    ("relay_credentials", "credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,predecessor_credential_id,recovery_generation,issued_at,expires_at,rotation_overlap_ends_at,revoked_at"),
    ("relay_identity", "relay_id,recovery_generation,state,revision"),
    ("restore_reviews", "review_id,relay_id,witness_generation,manifest_digest,decided_by_principal_id,action,audit_id,decided_at"),
    ("revocations", "revocation_id,scope_kind,scope_id,team_id,policy_generation,recovery_generation,reason_code,committed_at"),
    ("service_accounts", "principal_id,team_id,display_name,created_at,deprovisioned_at"),
    ("teams", "team_id,display_name,state,policy_generation,revision,created_at,updated_at"),
];

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

/// Identifies one exact checksum-valid prefix of the embedded migration plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MigrationPrefix {
    count: u16,
    digest: [u8; 32],
}

impl MigrationPrefix {
    /// Returns the number of embedded migrations in this prefix.
    #[must_use]
    pub(crate) const fn count(self) -> u16 {
        self.count
    }

    /// Returns the canonical checksum-bound prefix digest.
    #[must_use]
    pub(crate) const fn digest(self) -> [u8; 32] {
        self.digest
    }
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

    #[cfg(all(test, feature = "postgres-tests"))]
    pub(crate) async fn migrate_to_for_test(&self, target: i64) -> Result<(), StoreError> {
        let mut connection = self.pool.acquire().await.map_err(StoreError::Database)?;
        connection.close_on_drop();
        MIGRATOR
            .run_direct(Some(target), &mut *connection, false)
            .await
            .map_err(StoreError::Migration)
    }

    /// Commits a local migration latch to the exact ordered embedded migration set.
    #[cfg(all(test, feature = "postgres-tests"))]
    pub(crate) fn migration_plan_digest() -> [u8; 32] {
        Self::embedded_migration_prefix(MIGRATOR.iter().count())
            .expect("embedded migration count fits witness format")
            .digest
    }

    /// Returns the complete prefix represented by this relay binary.
    pub(crate) fn migration_target_prefix() -> Result<MigrationPrefix, StoreError> {
        Self::embedded_migration_prefix(MIGRATOR.iter().count())
    }

    /// Returns one exact prefix from the embedded migration source.
    pub(crate) fn embedded_migration_prefix_for_count(
        count: u16,
    ) -> Result<MigrationPrefix, StoreError> {
        if usize::from(count) > MIGRATOR.iter().count() {
            return Err(StoreError::StaleState);
        }
        Self::embedded_migration_prefix(usize::from(count))
    }

    /// Validates and returns the database's exact embedded migration prefix.
    pub(crate) async fn migration_prefix_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<MigrationPrefix, StoreError> {
        let applied: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
            "SELECT version,checksum,success FROM _sqlx_migrations ORDER BY version",
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(StoreError::Database)?;
        if applied.len() > MIGRATOR.iter().count() || applied.iter().any(|(_, _, success)| !success)
        {
            return Err(StoreError::StaleState);
        }
        for ((version, checksum, _success), embedded) in applied.iter().zip(MIGRATOR.iter()) {
            if *version != embedded.version || checksum.as_slice() != embedded.checksum.as_ref() {
                return Err(StoreError::StaleState);
            }
        }
        Self::embedded_migration_prefix(applied.len())
    }

    /// Hashes current nonempty application rows after callers lock every table.
    ///
    /// `PostgreSQL` serializes `to_jsonb` and hashes each row before returning its
    /// digest, so verifier values never enter application memory.
    pub(crate) async fn migration_authority_digest_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<[u8; 32], StoreError> {
        // `to_jsonb(... )::text` renders these values through session GUCs.
        // Pin the current schema's timestamp and bytea forms before hashing so
        // a retry after a role/default change has the same authority digest.
        for statement in [
            "SET LOCAL TimeZone='UTC'",
            "SET LOCAL DateStyle='ISO, YMD'",
            "SET LOCAL bytea_output='hex'",
        ] {
            sqlx::query(statement)
                .execute(&mut **tx)
                .await
                .map_err(StoreError::Database)?;
        }
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::text FROM pg_tables WHERE schemaname=current_schema() AND tablename <> '_sqlx_migrations' ORDER BY tablename COLLATE \"C\" LIMIT $1",
        )
        .bind(i64::try_from(MAX_MIGRATION_TABLES + 1).map_err(|_error| StoreError::StaleState)?)
        .fetch_all(&mut **tx)
        .await
        .map_err(StoreError::Database)?;
        if tables.len() > MAX_MIGRATION_TABLES {
            return Err(StoreError::StaleState);
        }
        let mut digest = Sha256::new();
        digest.update(MIGRATION_AUTHORITY_DOMAIN);
        for table in tables {
            let statement = format!(
                "SELECT encode(sha256(convert_to(string_agg(octet_length(convert_to(row_json,'UTF8'))::text || ':' || row_json,E'\\n' ORDER BY row_json COLLATE \"C\"),'UTF8')),'hex') FROM (SELECT to_jsonb(row_value)::text AS row_json FROM {} AS row_value) AS canonical_rows",
                quote_identifier(&table),
            );
            let row_digest: Option<String> = sqlx::query_scalar(AssertSqlSafe(statement))
                .fetch_one(&mut **tx)
                .await
                .map_err(StoreError::Database)?;
            let Some(row_digest) = row_digest else {
                continue;
            };
            update_digest_field(&mut digest, table.as_bytes());
            let row_digest = hex::decode(row_digest).map_err(|_error| StoreError::StaleState)?;
            update_digest_field(&mut digest, &row_digest);
        }
        Ok(digest.finalize().into())
    }

    /// Hashes the authority tables through a fixed column projection.
    ///
    /// Callers lock every table first. Unlike
    /// [`Store::migration_authority_digest_in_transaction`], this selects a named
    /// column list per table rather than the whole row, so adding a column leaves
    /// the digest unchanged. That makes it verifiable on both sides of a
    /// migration and closes the window in which the relay holds no lock while
    /// SQLx applies the plan on another connection.
    ///
    /// See [`AUTHORITY_PROJECTION`] for the bound tables and columns, how an
    /// absent table is treated, and which plans this binding refuses.
    ///
    /// # Errors
    /// Returns [`StoreError::Database`] when a projection cannot be read.
    pub(crate) async fn migration_projection_digest_in_transaction(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<[u8; 32], StoreError> {
        // Pin the same session forms the whole-row digest pins, so a role or
        // default change cannot alter the rendering between the two phases.
        for statement in [
            "SET LOCAL TimeZone='UTC'",
            "SET LOCAL DateStyle='ISO, YMD'",
            "SET LOCAL bytea_output='hex'",
        ] {
            sqlx::query(statement)
                .execute(&mut **tx)
                .await
                .map_err(StoreError::Database)?;
        }
        let mut digest = Sha256::new();
        digest.update(MIGRATION_PROJECTION_DOMAIN);
        for (table, columns) in AUTHORITY_PROJECTION {
            let present: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
                .bind(*table)
                .fetch_one(&mut **tx)
                .await
                .map_err(StoreError::Database)?;
            update_digest_field(&mut digest, table.as_bytes());
            if !present {
                continue;
            }
            let statement = format!(
                "SELECT encode(sha256(convert_to(string_agg(octet_length(convert_to(row_json,'UTF8'))::text || ':' || row_json,E'\\n' ORDER BY row_json COLLATE \"C\"),'UTF8')),'hex') FROM (SELECT to_jsonb(projected)::text AS row_json FROM (SELECT {columns} FROM {}) AS projected) AS canonical_rows",
                quote_identifier(table),
            );
            let row_digest: Option<String> = sqlx::query_scalar(AssertSqlSafe(statement))
                .fetch_one(&mut **tx)
                .await
                .map_err(StoreError::Database)?;
            // An empty table yields NULL, exactly like an absent one, so a plan
            // may create a table but may not populate it before completing.
            let Some(row_digest) = row_digest else {
                continue;
            };
            let row_digest = hex::decode(row_digest).map_err(|_error| StoreError::StaleState)?;
            update_digest_field(&mut digest, &row_digest);
        }
        Ok(digest.finalize().into())
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

impl Store {
    fn embedded_migration_prefix(count: usize) -> Result<MigrationPrefix, StoreError> {
        let count = u16::try_from(count).map_err(|_error| StoreError::StaleState)?;
        let mut digest = Sha256::new();
        digest.update(MIGRATION_PREFIX_DOMAIN);
        digest.update(count.to_be_bytes());
        for migration in MIGRATOR.iter().take(usize::from(count)) {
            digest.update(migration.version.to_be_bytes());
            update_digest_field(&mut digest, migration.checksum.as_ref());
        }
        Ok(MigrationPrefix {
            count,
            digest: digest.finalize().into(),
        })
    }
}

fn update_digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
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
