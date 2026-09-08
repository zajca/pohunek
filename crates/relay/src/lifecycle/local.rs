//! Resumable local operations bound to independently signed operation coordinates.

use sqlx::AssertSqlSafe;

use super::{
    semantic_manifest, validate_bootstrap, BootstrapRequest, Digest, Lifecycle, LifecycleError,
    Postgres, Sha256, Store, Transaction, Uuid, WitnessRecord,
};
use crate::recovery::WitnessEvent;

/// Caps catalog introspection independently of database contents.
const MAX_LOCAL_TABLES: usize = 128;
/// The immutable built-in vocabulary seeded by the embedded foundation migration.
const PERMISSIONS: &[&str] = &[
    "session.lifecycle.control",
    "session.metadata.read",
    "session.remove",
    "session.share.manage",
    "session.terminal.control",
    "session.terminal.observe",
    "team.admin",
    "team.grant.manage",
    "team.group.manage",
    "team.membership.manage",
    "team.membership.read",
    "team.role.manage",
];

impl Lifecycle {
    async fn begin_local_tables(&self) -> Result<Transaction<'_, Postgres>, LifecycleError> {
        let mut tx = self
            .store
            .pool()
            .begin()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        // Catalog reads precede table locks. READ COMMITTED ensures each later
        // scan includes writers that committed while those locks were waiting.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        Ok(tx)
    }

    /// Creates only the initial infrastructure identity, or resumes its exact signed operation.
    pub async fn bootstrap_local(
        &self,
        request: BootstrapRequest,
    ) -> Result<WitnessRecord, LifecycleError> {
        validate_bootstrap(&request)?;
        let request_digest = bootstrap_digest(&request);
        let checkpoint = self.witness.latest()?;
        let mut tx = self.begin_local_tables().await?;
        let occupied = locked_tables(&mut tx).await?;
        for required in [
            "permissions",
            "relay_identity",
            "principals",
            "oidc_identities",
            "audit_events",
        ] {
            if !occupied.iter().any(|(name, _occupied)| name == required) {
                return Err(LifecycleError::InvalidState);
            }
        }
        let active = match checkpoint {
            Some(checkpoint)
                if checkpoint.relay_id == request.relay_id
                    && matches!(checkpoint.event, WitnessEvent::Bootstrap { request_digest: expected, .. } if expected == request_digest) =>
            {
                checkpoint
            }
            Some(_) => return Err(LifecycleError::InvalidState),
            None => {
                require_empty(&occupied, &[])?;
                self.witness.begin_local(
                    None,
                    &request.relay_id,
                    1,
                    WitnessEvent::Bootstrap {
                        request_digest,
                        principal_id: Uuid::now_v7(),
                        identity_id: Uuid::now_v7(),
                        audit_id: Uuid::now_v7(),
                    },
                )?
            }
        };
        let WitnessEvent::Bootstrap {
            principal_id,
            identity_id,
            audit_id,
            ..
        } = active.event
        else {
            return Err(LifecycleError::InvalidState);
        };
        let parameter = format!(
            "principal={principal_id};identity={identity_id};request={}",
            hex::encode(request_digest)
        );
        if occupied.iter().all(|(_, occupied)| !occupied) && active.active_run {
            sqlx::query("INSERT INTO relay_identity (relay_id,recovery_generation,state,revision) VALUES ($1,1,'normal',1)")
                .bind(&request.relay_id).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
            sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,'infrastructure','active',1)")
                .bind(principal_id).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
            sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,$2,$3,$4,1)")
                .bind(identity_id).bind(&request.issuer).bind(&request.subject).bind(principal_id)
                .execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
            sqlx::query("INSERT INTO audit_events (audit_id,actor_kind,action,decision,policy_generation,recovery_generation,correlation_id,parameter_code,parameter_value,outcome) VALUES ($1,'system','lifecycle.bootstrap','changed',1,1,$1,'lifecycle',$2,'committed')")
                .bind(audit_id).bind(&parameter).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
        } else {
            require_empty(
                &occupied,
                &[
                    "relay_identity",
                    "principals",
                    "oidc_identities",
                    "audit_events",
                ],
            )?;
            let exact: bool = sqlx::query_scalar("SELECT (SELECT count(*)=1 FROM relay_identity) AND EXISTS (SELECT 1 FROM relay_identity WHERE relay_id=$1 AND recovery_generation=1 AND state='normal' AND revision=1) AND (SELECT count(*)=1 FROM principals) AND EXISTS (SELECT 1 FROM principals WHERE id=$2 AND kind='infrastructure' AND state='active' AND generation=1) AND (SELECT count(*)=1 FROM oidc_identities) AND EXISTS (SELECT 1 FROM oidc_identities WHERE identity_id=$3 AND principal_id=$2 AND issuer=$4 AND subject=$5 AND link_generation=1 AND removed_at IS NULL) AND (SELECT count(*)=1 FROM audit_events) AND EXISTS (SELECT 1 FROM audit_events WHERE audit_id=$6 AND actor_kind='system' AND actor_principal_id IS NULL AND team_id IS NULL AND host_id IS NULL AND idempotency_key IS NULL AND action='lifecycle.bootstrap' AND decision='changed' AND policy_generation=1 AND recovery_generation=1 AND correlation_id=$6 AND parameter_code='lifecycle' AND parameter_value=$7 AND outcome='committed')")
                .bind(&request.relay_id).bind(principal_id).bind(identity_id).bind(&request.issuer).bind(&request.subject).bind(audit_id).bind(&parameter)
                .fetch_one(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
            if !exact {
                return Err(LifecycleError::InvalidState);
            }
        }
        if self.witness.latest()?.as_ref() != Some(&active) {
            return Err(LifecycleError::InvalidState);
        }
        tx.commit()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        if active.active_run {
            self.witness.complete_local(&active).map_err(Into::into)
        } else {
            Ok(active)
        }
    }

    /// Applies or resumes exactly the signed embedded migration plan with ingress stopped.
    pub async fn migrate_local(&self, expected_relay_id: &str) -> Result<(), LifecycleError> {
        let Some(checkpoint) = self.witness.latest()? else {
            let mut tx = self.begin_local_tables().await?;
            require_empty(&locked_tables(&mut tx).await?, &[])?;
            tx.commit()
                .await
                .map_err(|_error| LifecycleError::Durable)?;
            return self
                .store
                .migrate()
                .await
                .map_err(|_error| LifecycleError::Durable);
        };
        if checkpoint.relay_id != expected_relay_id || checkpoint.recovery_pending_review {
            return Err(LifecycleError::InvalidState);
        }
        let plan_digest = Store::migration_plan_digest();
        if checkpoint.active_run
            && !matches!(checkpoint.event, WitnessEvent::Migration { plan_digest: expected, .. } if expected == plan_digest)
        {
            return Err(LifecycleError::InvalidState);
        }
        let mut tx = self.begin().await?;
        let generation: Option<i64> = sqlx::query_scalar("SELECT recovery_generation FROM relay_identity WHERE relay_id=$1 AND state='normal' AND recovery_generation=$2 FOR UPDATE")
            .bind(expected_relay_id).bind(checkpoint.recovery_generation).fetch_optional(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
        if generation.is_none() || self.witness.latest()?.as_ref() != Some(&checkpoint) {
            return Err(LifecycleError::InvalidState);
        }
        self.require_no_live_lease(&mut tx, expected_relay_id)
            .await?;
        let active = if checkpoint.active_run {
            checkpoint
        } else {
            let incidents = self.witness.incident_review(&checkpoint)?;
            let manifest = semantic_manifest(&mut tx, incidents, false).await?;
            self.witness.begin_local(
                Some(&checkpoint),
                expected_relay_id,
                checkpoint.recovery_generation,
                WitnessEvent::Migration {
                    plan_digest,
                    authority_digest: *manifest.digest(),
                },
            )?
        };
        tx.commit()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        // SQLx persists each migration transactionally. A crash can only leave a
        // prefix of this exact signed plan, which the same binary safely resumes.
        self.store
            .migrate()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        let mut tx = self.begin().await?;
        self.store
            .lock_relay_identity_in_transaction(&mut tx, expected_relay_id)
            .await
            .map_err(|_error| LifecycleError::InvalidState)?;
        self.require_no_live_lease(&mut tx, expected_relay_id)
            .await?;
        let incidents = self.witness.incident_review_at(&active)?;
        let manifest = semantic_manifest(&mut tx, incidents, false).await?;
        if !matches!(active.event, WitnessEvent::Migration { authority_digest, .. } if manifest.digest() == &authority_digest)
        {
            return Err(LifecycleError::ManifestMismatch);
        }
        tx.commit()
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        self.witness.complete_local(&active)?;
        Ok(())
    }
}

fn bootstrap_digest(request: &BootstrapRequest) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"pohunek.relay.bootstrap.v1\0");
    for value in [&request.relay_id, &request.issuer, &request.subject] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    hash.finalize().into()
}

fn require_empty(tables: &[(String, bool)], permitted: &[&str]) -> Result<(), LifecycleError> {
    if tables
        .iter()
        .any(|(name, occupied)| *occupied && !permitted.contains(&name.as_str()))
    {
        return Err(LifecycleError::InvalidState);
    }
    Ok(())
}

async fn locked_tables(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<Vec<(String, bool)>, LifecycleError> {
    let tables: Vec<(String, String, String)> = sqlx::query_as("SELECT tablename::text,format('LOCK TABLE %I.%I IN SHARE ROW EXCLUSIVE MODE',schemaname,tablename),format('SELECT EXISTS (SELECT 1 FROM %I.%I)',schemaname,tablename) FROM pg_tables WHERE schemaname=current_schema() AND tablename <> '_sqlx_migrations' ORDER BY tablename LIMIT $1")
        .bind(i64::try_from(MAX_LOCAL_TABLES + 1).map_err(|_error| LifecycleError::Durable)?).fetch_all(&mut **tx).await.map_err(|_error| LifecycleError::Durable)?;
    if tables.len() > MAX_LOCAL_TABLES {
        return Err(LifecycleError::InvalidState);
    }
    let mut occupied = Vec::with_capacity(tables.len());
    for (table, lock, query) in tables {
        // PostgreSQL's %I quotes both catalog-owned identifiers; no caller text
        // is interpolated into either executable statement.
        sqlx::query(AssertSqlSafe(lock))
            .execute(&mut **tx)
            .await
            .map_err(|_error| LifecycleError::Durable)?;
        if table == "permissions" {
            let permissions: Vec<String> = sqlx::query_scalar(
                "SELECT permission FROM permissions ORDER BY permission COLLATE \"C\" LIMIT $1",
            )
            .bind(i64::try_from(PERMISSIONS.len() + 1).expect("fixed permission catalog"))
            .fetch_all(&mut **tx)
            .await
            .map_err(|_error| LifecycleError::Durable)?;
            if permissions != PERMISSIONS {
                return Err(LifecycleError::InvalidState);
            }
            occupied.push((table, false));
        } else {
            let exists = sqlx::query_scalar(AssertSqlSafe(query))
                .fetch_one(&mut **tx)
                .await
                .map_err(|_error| LifecycleError::Durable)?;
            occupied.push((table, exists));
        }
    }
    Ok(occupied)
}
