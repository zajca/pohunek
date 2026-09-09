//! Resumable local operations bound to independently signed operation coordinates.

use sqlx::AssertSqlSafe;

use super::{
    validate_bootstrap, BootstrapRequest, InitialProvisionRequest, Lifecycle, LifecycleError,
    Postgres, Store, Transaction, Uuid, WitnessRecord,
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
        let request_commitment = self
            .witness
            .bootstrap_commitment(&canonical_bootstrap_request(&request))?;
        let (commitment_version, commitment_key_id) =
            self.witness.bootstrap_commitment_coordinate();
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
                    && matches!(&checkpoint.event, WitnessEvent::Bootstrap { commitment_version: expected_version, commitment_key_id: expected_key_id, request_commitment: expected, .. } if *expected_version == commitment_version && expected_key_id == &commitment_key_id && *expected == request_commitment) =>
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
                        commitment_version,
                        commitment_key_id,
                        request_commitment,
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
        } = &active.event
        else {
            return Err(LifecycleError::InvalidState);
        };
        let parameter = format!(
            "principal={principal_id};identity={identity_id};commitment={}",
            hex::encode(request_commitment)
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

    /// Creates the first explicit team owner and service account while the relay is stopped.
    #[expect(
        clippy::too_many_lines,
        reason = "The witnessed precondition, mutation, and exact replay checks form one recovery boundary."
    )]
    pub async fn provision_initial_operator_local(
        &self,
        request: InitialProvisionRequest,
    ) -> Result<WitnessRecord, LifecycleError> {
        validate_initial_provision(&request)?;
        let request_commitment = self
            .witness
            .provision_commitment(&canonical_initial_provision_request(&request))?;
        let (commitment_version, commitment_key_id) =
            self.witness.provision_commitment_coordinate();
        let checkpoint = self.witness.latest()?.ok_or(LifecycleError::InvalidState)?;
        if checkpoint.relay_id != request.relay_id || checkpoint.recovery_pending_review {
            return Err(LifecycleError::InvalidState);
        }
        let mut tx = self.begin().await?;
        self.store
            .lock_relay_identity_in_transaction(&mut tx, &request.relay_id)
            .await
            .map_err(|_error| LifecycleError::InvalidState)?;
        self.require_no_live_lease(&mut tx, &request.relay_id)
            .await?;
        let occupied = locked_tables(&mut tx).await?;
        let owner: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT p.id,i.identity_id FROM principals p JOIN oidc_identities i ON i.principal_id=p.id \
             WHERE i.issuer=$1 AND i.subject=$2 AND i.link_generation=1 AND i.removed_at IS NULL \
             AND p.kind='infrastructure' AND p.state='active' AND p.generation=1 FOR SHARE",
        )
        .bind(&request.issuer)
        .bind(&request.subject)
        .fetch_all(&mut *tx)
        .await
        .map_err(|_error| LifecycleError::Durable)?;
        let [(owner_principal_id, owner_identity_id)] = owner.as_slice() else {
            return Err(LifecycleError::InvalidState);
        };
        let active = match checkpoint {
            checkpoint if checkpoint.active_run => {
                let WitnessEvent::Provision {
                    commitment_version: expected_version,
                    commitment_key_id: expected_key_id,
                    request_commitment: expected_commitment,
                    artifact_digest,
                    credential_secret_digest,
                    team_id,
                    owner_membership_id,
                    service_principal_id,
                    service_membership_id,
                    credential_id,
                    audit_id,
                } = &checkpoint.event
                else {
                    return Err(LifecycleError::InvalidState);
                };
                if *expected_version != commitment_version
                    || expected_key_id != &commitment_key_id
                    || *expected_commitment != request_commitment
                    || *artifact_digest != request.artifact_digest
                    || *credential_secret_digest != request.credential_secret_digest
                    || *team_id != request.team_id
                    || *owner_membership_id != request.owner_membership_id
                    || *service_principal_id != request.service_principal_id
                    || *service_membership_id != request.service_membership_id
                    || *credential_id != request.credential_id
                    || *audit_id != request.audit_id
                {
                    return Err(LifecycleError::InvalidState);
                }
                checkpoint
            }
            checkpoint if matches!(checkpoint.event, WitnessEvent::Provision { .. }) => {
                let WitnessEvent::Provision {
                    commitment_version: expected_version,
                    commitment_key_id: expected_key_id,
                    request_commitment: expected_commitment,
                    artifact_digest,
                    credential_secret_digest,
                    team_id,
                    owner_membership_id,
                    service_principal_id,
                    service_membership_id,
                    credential_id,
                    audit_id,
                } = &checkpoint.event
                else {
                    return Err(LifecycleError::InvalidState);
                };
                if *expected_version != commitment_version
                    || expected_key_id != &commitment_key_id
                    || *expected_commitment != request_commitment
                    || *artifact_digest != request.artifact_digest
                    || *credential_secret_digest != request.credential_secret_digest
                    || *team_id != request.team_id
                    || *owner_membership_id != request.owner_membership_id
                    || *service_principal_id != request.service_principal_id
                    || *service_membership_id != request.service_membership_id
                    || *credential_id != request.credential_id
                    || *audit_id != request.audit_id
                {
                    return Err(LifecycleError::InvalidState);
                }
                checkpoint
            }
            checkpoint => {
                require_empty(
                    &occupied,
                    &[
                        "relay_identity",
                        "principals",
                        "oidc_identities",
                        "audit_events",
                        "relay_lease",
                    ],
                )?;
                if !bootstrap_baseline_matches(
                    &mut tx,
                    &request,
                    *owner_principal_id,
                    *owner_identity_id,
                    checkpoint.recovery_generation,
                )
                .await?
                    || self.witness.latest()?.as_ref() != Some(&checkpoint)
                {
                    return Err(LifecycleError::InvalidState);
                }
                self.witness.begin_local(
                    Some(&checkpoint),
                    &request.relay_id,
                    checkpoint.recovery_generation,
                    WitnessEvent::Provision {
                        commitment_version,
                        commitment_key_id,
                        request_commitment,
                        artifact_digest: request.artifact_digest,
                        credential_secret_digest: request.credential_secret_digest,
                        team_id: request.team_id,
                        owner_membership_id: request.owner_membership_id,
                        service_principal_id: request.service_principal_id,
                        service_membership_id: request.service_membership_id,
                        credential_id: request.credential_id,
                        audit_id: request.audit_id,
                    },
                )?
            }
        };
        let parameter = format!(
            "owner={owner_principal_id};identity={owner_identity_id};commitment={}",
            hex::encode(request_commitment)
        );
        if active.active_run {
            let exact = provision_rows_match(
                &mut tx,
                &request,
                *owner_principal_id,
                *owner_identity_id,
                active.recovery_generation,
                &parameter,
                &occupied,
            )
            .await?;
            if !exact {
                if !bootstrap_baseline_matches(
                    &mut tx,
                    &request,
                    *owner_principal_id,
                    *owner_identity_id,
                    active.recovery_generation,
                )
                .await?
                {
                    return Err(LifecycleError::InvalidState);
                }
                sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,$2,'active',1,1)")
                    .bind(request.team_id).bind(&request.team_name).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
                sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
                    .bind(request.owner_membership_id).bind(request.team_id).bind(*owner_principal_id).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
                sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,'service','active',1)")
                    .bind(request.service_principal_id).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
                sqlx::query("INSERT INTO service_accounts (principal_id,team_id,display_name) VALUES ($1,$2,$3)")
                    .bind(request.service_principal_id).bind(request.team_id).bind(&request.service_account_name).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
                sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'member','active',1,1)")
                    .bind(request.service_membership_id).bind(request.team_id).bind(request.service_principal_id).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
                sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'service',1,$1,$6,$7)")
                    .bind(request.credential_id).bind(request.credential_id.to_string()).bind(request.credential_secret_digest.as_slice()).bind(request.service_principal_id).bind(&request.digest_key_id).bind(active.recovery_generation).bind(request.expires_at).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
                sqlx::query("INSERT INTO audit_events (audit_id,actor_principal_id,actor_kind,team_id,action,decision,policy_generation,recovery_generation,correlation_id,parameter_code,parameter_value,outcome) VALUES ($1,$2,'infrastructure',$3,'lifecycle.provision','changed',1,$4,$1,'lifecycle',$5,'committed')")
                    .bind(request.audit_id).bind(*owner_principal_id).bind(request.team_id).bind(active.recovery_generation).bind(&parameter).execute(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
            }
        } else if !provision_rows_match(
            &mut tx,
            &request,
            *owner_principal_id,
            *owner_identity_id,
            active.recovery_generation,
            &parameter,
            &occupied,
        )
        .await?
        {
            return Err(LifecycleError::InvalidState);
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
    #[expect(
        clippy::too_many_lines,
        reason = "The pre-DDL and post-DDL verification boundaries must remain visibly adjacent."
    )]
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
        let target =
            Store::migration_target_prefix().map_err(|error| migration_store_error(&error))?;
        let mut tx = self.begin().await?;
        let generation: Option<i64> = sqlx::query_scalar("SELECT recovery_generation FROM relay_identity WHERE relay_id=$1 AND state='normal' AND recovery_generation=$2 FOR UPDATE")
            .bind(expected_relay_id).bind(checkpoint.recovery_generation).fetch_optional(&mut *tx).await.map_err(|_error| LifecycleError::Durable)?;
        if generation.is_none() || self.witness.latest()?.as_ref() != Some(&checkpoint) {
            return Err(LifecycleError::InvalidState);
        }
        self.require_no_live_lease(&mut tx, expected_relay_id)
            .await?;
        // This takes `SHARE ROW EXCLUSIVE` locks before the database-derived
        // digest. A pending migration therefore cannot be resumed from a
        // swapped authority state before SQLx receives any DDL.
        locked_tables(&mut tx).await?;
        let observed = self
            .store
            .migration_prefix_in_transaction(&mut tx)
            .await
            .map_err(|error| migration_store_error(&error))?;
        let authority_digest = self
            .store
            .migration_authority_digest_in_transaction(&mut tx)
            .await
            .map_err(|error| migration_store_error(&error))?;
        let active = if checkpoint.active_run {
            let WitnessEvent::Migration {
                base_migration_count,
                base_plan_digest,
                target_migration_count,
                target_plan_digest,
                authority_digest: expected_authority,
            } = &checkpoint.event
            else {
                return Err(LifecycleError::InvalidState);
            };
            let signed_base = Store::embedded_migration_prefix_for_count(*base_migration_count)
                .map_err(|error| migration_store_error(&error))?;
            if signed_base.digest() != *base_plan_digest
                || *target_migration_count != target.count()
                || *target_plan_digest != target.digest()
                || observed.count() < *base_migration_count
                || observed.count() > *target_migration_count
                || observed.count() > target.count()
                || authority_digest != *expected_authority
                || (observed.count() == *base_migration_count
                    && observed.digest() != *base_plan_digest)
            {
                return Err(LifecycleError::InvalidState);
            }
            checkpoint
        } else {
            if self.witness.latest()?.as_ref() != Some(&checkpoint) {
                return Err(LifecycleError::InvalidState);
            }
            self.witness.begin_local(
                Some(&checkpoint),
                expected_relay_id,
                checkpoint.recovery_generation,
                WitnessEvent::Migration {
                    base_migration_count: observed.count(),
                    base_plan_digest: observed.digest(),
                    target_migration_count: target.count(),
                    target_plan_digest: target.digest(),
                    authority_digest,
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
        locked_tables(&mut tx).await?;
        let observed = self
            .store
            .migration_prefix_in_transaction(&mut tx)
            .await
            .map_err(|error| migration_store_error(&error))?;
        let authority_digest = self
            .store
            .migration_authority_digest_in_transaction(&mut tx)
            .await
            .map_err(|error| migration_store_error(&error))?;
        if observed != target
            || !matches!(&active.event, WitnessEvent::Migration { authority_digest: expected, .. } if authority_digest == *expected)
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

pub(crate) fn canonical_bootstrap_request(request: &BootstrapRequest) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [&request.relay_id, &request.issuer, &request.subject] {
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes
}

fn validate_initial_provision(request: &InitialProvisionRequest) -> Result<(), LifecycleError> {
    const MAX_NAME_BYTES: usize = 256;
    const MAX_KEY_ID_BYTES: usize = 64;
    if request.relay_id.is_empty()
        || request.issuer.is_empty()
        || request.subject.is_empty()
        || request.team_name.is_empty()
        || request.team_name.len() > MAX_NAME_BYTES
        || request.service_account_name.is_empty()
        || request.service_account_name.len() > MAX_NAME_BYTES
        || request.digest_key_id.is_empty()
        || request.digest_key_id.len() > MAX_KEY_ID_BYTES
        || request.expires_at <= time::OffsetDateTime::now_utc()
        || request.expires_at.nanosecond() % 1_000 != 0
        || [
            &request.relay_id,
            &request.issuer,
            &request.subject,
            &request.team_name,
            &request.service_account_name,
            &request.digest_key_id,
        ]
        .into_iter()
        .any(|value| value.contains('\0'))
    {
        return Err(LifecycleError::InvalidState);
    }
    Ok(())
}

pub(crate) fn canonical_initial_provision_request(request: &InitialProvisionRequest) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"pohunek.relay.initial-provision.v1\0");
    for value in [
        &request.relay_id,
        &request.issuer,
        &request.subject,
        &request.team_name,
        &request.service_account_name,
        &request.digest_key_id,
    ] {
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }
    bytes.extend_from_slice(&request.expires_at.unix_timestamp().to_be_bytes());
    bytes.extend_from_slice(&request.expires_at.nanosecond().to_be_bytes());
    bytes
}

async fn bootstrap_baseline_matches(
    tx: &mut Transaction<'_, Postgres>,
    request: &InitialProvisionRequest,
    owner_principal_id: Uuid,
    owner_identity_id: Uuid,
    recovery_generation: i64,
) -> Result<bool, LifecycleError> {
    sqlx::query_scalar(
        "SELECT \
         (SELECT count(*)=1 FROM relay_identity) \
         AND EXISTS (SELECT 1 FROM relay_identity WHERE relay_id=$1 AND state='normal' AND recovery_generation=$2) \
         AND (SELECT count(*)=1 FROM principals) \
         AND EXISTS (SELECT 1 FROM principals WHERE id=$3 AND kind='infrastructure' AND state='active' AND generation=1) \
         AND (SELECT count(*)=1 FROM oidc_identities) \
         AND EXISTS (SELECT 1 FROM oidc_identities WHERE identity_id=$4 AND principal_id=$3 AND issuer=$5 AND subject=$6 AND link_generation=1 AND removed_at IS NULL) \
         AND (SELECT count(*)=1 FROM audit_events) \
         AND EXISTS (SELECT 1 FROM audit_events WHERE actor_principal_id IS NULL AND actor_kind='system' AND team_id IS NULL AND action='lifecycle.bootstrap' AND decision='changed' AND policy_generation=1 AND recovery_generation=$2 AND parameter_code='lifecycle' AND outcome='committed')",
    )
    .bind(&request.relay_id)
    .bind(recovery_generation)
    .bind(owner_principal_id)
    .bind(owner_identity_id)
    .bind(&request.issuer)
    .bind(&request.subject)
    .fetch_one(&mut **tx)
    .await
    .map_err(|_error| LifecycleError::Durable)
}

#[expect(
    clippy::too_many_arguments,
    reason = "The full signed provisioning coordinate must be verified in one durable query."
)]
async fn provision_rows_match(
    tx: &mut Transaction<'_, Postgres>,
    request: &InitialProvisionRequest,
    owner_principal_id: Uuid,
    owner_identity_id: Uuid,
    recovery_generation: i64,
    parameter: &str,
    occupied: &[(String, bool)],
) -> Result<bool, LifecycleError> {
    require_empty(
        occupied,
        &[
            "relay_identity",
            "principals",
            "oidc_identities",
            "audit_events",
            "teams",
            "memberships",
            "service_accounts",
            "relay_credentials",
            "relay_lease",
        ],
    )?;
    sqlx::query_scalar(
        "SELECT \
         (SELECT count(*)=1 FROM relay_identity) \
         AND EXISTS (SELECT 1 FROM relay_identity WHERE relay_id=$1 AND state='normal' AND recovery_generation=$2) \
         AND (SELECT count(*)=2 FROM principals) \
         AND EXISTS (SELECT 1 FROM principals WHERE id=$3 AND kind='infrastructure' AND state='active' AND generation=1) \
         AND EXISTS (SELECT 1 FROM principals WHERE id=$4 AND kind='service' AND state='active' AND generation=1) \
         AND (SELECT count(*)=1 FROM oidc_identities) \
         AND EXISTS (SELECT 1 FROM oidc_identities WHERE identity_id=$5 AND principal_id=$3 AND issuer=$6 AND subject=$7 AND link_generation=1 AND removed_at IS NULL) \
         AND (SELECT count(*)=1 FROM teams) \
         AND EXISTS (SELECT 1 FROM teams WHERE team_id=$8 AND display_name=$9 AND state='active' AND policy_generation=1 AND revision=1) \
         AND (SELECT count(*)=2 FROM memberships) \
         AND EXISTS (SELECT 1 FROM memberships WHERE membership_id=$10 AND team_id=$8 AND principal_id=$3 AND builtin_role='owner' AND state='active' AND revision=1 AND local_deny_generation=1) \
         AND EXISTS (SELECT 1 FROM memberships WHERE membership_id=$11 AND team_id=$8 AND principal_id=$4 AND builtin_role='member' AND state='active' AND revision=1 AND local_deny_generation=1) \
         AND (SELECT count(*)=1 FROM service_accounts) \
         AND EXISTS (SELECT 1 FROM service_accounts WHERE principal_id=$4 AND team_id=$8 AND display_name=$12 AND deprovisioned_at IS NULL) \
         AND (SELECT count(*)=1 FROM relay_credentials) \
         AND EXISTS (SELECT 1 FROM relay_credentials WHERE credential_id=$13 AND public_id=$13::text AND secret_digest=$14 AND principal_id=$4 AND identity_id IS NULL AND digest_key_id=$15 AND credential_kind='service' AND credential_generation=1 AND rotation_family_id=$13 AND predecessor_credential_id IS NULL AND recovery_generation=$2 AND expires_at=$16 AND rotation_overlap_ends_at IS NULL AND revoked_at IS NULL) \
         AND (SELECT count(*)=2 FROM audit_events) \
         AND EXISTS (SELECT 1 FROM audit_events WHERE audit_id=$17 AND actor_principal_id=$3 AND actor_kind='infrastructure' AND team_id=$8 AND host_id IS NULL AND idempotency_key IS NULL AND action='lifecycle.provision' AND decision='changed' AND policy_generation=1 AND recovery_generation=$2 AND correlation_id=$17 AND parameter_code='lifecycle' AND parameter_value=$18 AND outcome='committed')",
    )
    .bind(&request.relay_id)
    .bind(recovery_generation)
    .bind(owner_principal_id)
    .bind(request.service_principal_id)
    .bind(owner_identity_id)
    .bind(&request.issuer)
    .bind(&request.subject)
    .bind(request.team_id)
    .bind(&request.team_name)
    .bind(request.owner_membership_id)
    .bind(request.service_membership_id)
    .bind(&request.service_account_name)
    .bind(request.credential_id)
    .bind(request.credential_secret_digest.as_slice())
    .bind(&request.digest_key_id)
    .bind(request.expires_at)
    .bind(request.audit_id)
    .bind(parameter)
    .fetch_one(&mut **tx)
    .await
    .map_err(|_error| LifecycleError::Durable)
}

fn migration_store_error(error: &crate::store::StoreError) -> LifecycleError {
    match error {
        crate::store::StoreError::StaleState => LifecycleError::InvalidState,
        _ => LifecycleError::Durable,
    }
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

pub(crate) async fn locked_tables(
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
