//! Commits revision-checked team administration mutations.

// Rust guideline compliant 2026-09-08

use sqlx::Transaction;
use uuid::Uuid;

use crate::{
    authorization::{
        admin::{receipt, record_receipt, request_digest},
        rbac::TEAM_MEMBERSHIP_READ_PERMISSION,
        revalidate_authentication,
    },
    store::{
        actor_kind_name, ActorContext, AuthorizationRequest, ResourceScope, Store, StoreError,
    },
};

/// Requests creation of a bounded team record.
#[derive(Debug, Clone)]
pub struct CreateTeam {
    /// Bounded display name.
    pub display_name: String,
    /// Active principal that receives the initial owner membership.
    pub initial_owner: Uuid,
    /// Request correlation coordinate.
    pub correlation_id: Uuid,
    /// Durable retry coordinate for this mutation.
    pub idempotency_key: Uuid,
}

/// Returns the created team coordinate and first revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeamRecord {
    /// Team primary coordinate.
    pub team_id: Uuid,
    /// Current optimistic revision.
    pub revision: i64,
}

/// Selects one bounded page of team records.
#[derive(Debug, Clone, Copy)]
pub struct TeamPage {
    /// Exclusive UUID cursor from the preceding page.
    pub after: Option<Uuid>,
    /// Maximum returned rows, limited to one hundred.
    pub limit: i64,
}

/// Changes a team display name or disables it at an expected revision.
#[derive(Debug, Clone)]
pub struct UpdateTeam {
    /// Target team coordinate.
    pub team_id: Uuid,
    /// Revision observed by the caller.
    pub expected_revision: i64,
    /// New bounded display name.
    pub display_name: Option<String>,
    /// Whether to disable the team and revoke future authority.
    pub disable: bool,
    /// Request correlation coordinate.
    pub correlation_id: Uuid,
    /// Durable retry coordinate for this mutation.
    pub idempotency_key: Uuid,
}

/// Changes a membership using an expected revision.
#[derive(Debug, Clone, Copy)]
pub struct MembershipChange {
    /// Target team coordinate.
    pub team_id: Uuid,
    /// Target principal coordinate.
    pub principal_id: Uuid,
    /// Current revision required by the caller.
    pub expected_revision: i64,
    /// New built-in role, or removal when absent.
    pub role: Option<&'static str>,
    /// Request correlation coordinate.
    pub correlation_id: Uuid,
    /// Durable retry coordinate for this mutation.
    pub idempotency_key: Uuid,
}

/// Explicit disable command for callers that do not update display metadata.
#[derive(Debug, Clone, Copy)]
pub struct DisableTeam {
    /// Team coordinate to disable.
    pub team_id: Uuid,
    /// Revision observed by the caller.
    pub expected_revision: i64,
    /// Request correlation coordinate.
    pub correlation_id: Uuid,
    /// Durable retry coordinate for this mutation.
    pub idempotency_key: Uuid,
}

/// Explicit member-removal command preserving the last-owner constraint.
#[derive(Debug, Clone, Copy)]
pub struct RemoveMember {
    /// Team coordinate.
    pub team_id: Uuid,
    /// Member to remove.
    pub principal_id: Uuid,
    /// Revision observed by the caller.
    pub expected_revision: i64,
    /// Request correlation coordinate.
    pub correlation_id: Uuid,
    /// Durable retry coordinate for this mutation.
    pub idempotency_key: Uuid,
}

impl Store {
    /// Applies a previously authorized team disable inside the authority transaction.
    pub(crate) async fn disable_team_in_transaction(
        &self,
        transaction: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: DisableTeam,
    ) -> Result<TeamRecord, StoreError> {
        self.update_team_in_transaction(
            transaction,
            actor,
            UpdateTeam {
                team_id: command.team_id,
                expected_revision: command.expected_revision,
                display_name: None,
                disable: true,
                correlation_id: command.correlation_id,
                idempotency_key: command.idempotency_key,
            },
        )
        .await
    }

    /// Removes a member using its current revision.
    pub(crate) async fn remove_member_in_transaction(
        &self,
        transaction: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: RemoveMember,
    ) -> Result<(), StoreError> {
        self.change_member_in_transaction(
            transaction,
            actor,
            MembershipChange {
                team_id: command.team_id,
                principal_id: command.principal_id,
                expected_revision: command.expected_revision,
                role: None,
                correlation_id: command.correlation_id,
                idempotency_key: command.idempotency_key,
            },
        )
        .await
    }

    /// Lists teams in a bounded stable UUID page.
    pub async fn list_teams(
        &self,
        actor: ActorContext,
        page: TeamPage,
    ) -> Result<Vec<TeamRecord>, StoreError> {
        if page.limit < 1 || page.limit > 100 {
            return Err(StoreError::InputTooLarge);
        }
        let mut tx = self.begin_serializable().await?;
        revalidate_authentication(
            &mut tx,
            &AuthorizationRequest {
                actor,
                team_id: Uuid::nil(),
                permission: TEAM_MEMBERSHIP_READ_PERMISSION.to_owned(),
                resource: ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            },
        )
        .await?;
        let rows = sqlx::query("SELECT DISTINCT t.team_id,t.revision FROM teams t JOIN memberships m ON m.team_id=t.team_id JOIN relay_identity r ON r.state='normal' WHERE m.principal_id=$1 AND m.state='active' AND t.state='active' AND r.recovery_generation=$2 AND ($3::uuid IS NULL OR t.team_id>$3) ORDER BY t.team_id LIMIT $4")
            .bind(actor.principal_id()).bind(actor.recovery_generation()).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await.map_err(StoreError::Database)?;
        tx.commit().await.map_err(StoreError::Database)?;
        Ok(rows
            .into_iter()
            .map(|row| TeamRecord {
                team_id: sqlx::Row::get(&row, "team_id"),
                revision: sqlx::Row::get(&row, "revision"),
            })
            .collect())
    }

    /// Applies a previously authorized team update inside the authority transaction.
    pub(crate) async fn update_team_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: UpdateTeam,
    ) -> Result<TeamRecord, StoreError> {
        if command
            .display_name
            .as_ref()
            .is_some_and(|name| name.is_empty() || name.len() > 256)
        {
            return Err(StoreError::InputTooLarge);
        }
        let digest = request_digest(&format!(
            "{}:{}:{:?}:{}",
            command.team_id, command.expected_revision, command.display_name, command.disable
        ));
        if let Some((team_id, revision)) =
            receipt(tx, actor, "team.update", command.idempotency_key, &digest).await?
        {
            return Ok(TeamRecord { team_id, revision });
        }
        let row = sqlx::query("UPDATE teams SET display_name=COALESCE($1,display_name),state=CASE WHEN $2 THEN 'disabled' ELSE state END,policy_generation=policy_generation+CASE WHEN $2 THEN 1 ELSE 0 END,revision=revision+1,updated_at=clock_timestamp() WHERE team_id=$3 AND revision=$4 RETURNING revision")
            .bind(command.display_name).bind(command.disable).bind(command.team_id).bind(command.expected_revision).fetch_optional(&mut **tx).await.map_err(StoreError::Database)?.ok_or(StoreError::StaleState)?;
        let audit_id = audit_team_change(
            tx,
            actor,
            command.team_id,
            command.correlation_id,
            if command.disable {
                "team.disable"
            } else {
                "team.update"
            },
        )
        .await?;
        let revision = sqlx::Row::get(&row, "revision");
        record_receipt(
            tx,
            actor,
            "team.update",
            command.idempotency_key,
            &digest,
            audit_id,
            command.team_id,
            revision,
        )
        .await?;
        Ok(TeamRecord {
            team_id: command.team_id,
            revision,
        })
    }

    /// Creates a team and its first owner in one audited transaction.
    ///
    /// # Errors
    /// Returns [`StoreError::Forbidden`] unless the actor is infrastructure authority.
    pub(crate) async fn create_team_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: CreateTeam,
    ) -> Result<TeamRecord, StoreError> {
        if !matches!(actor.kind(), crate::store::ActorKind::Infrastructure)
            || command.display_name.is_empty()
            || command.display_name.len() > 256
        {
            return Err(StoreError::Forbidden);
        }
        let digest = request_digest(&format!(
            "{}:{}",
            command.display_name, command.initial_owner
        ));
        if let Some((team_id, revision)) =
            receipt(tx, actor, "team.create", command.idempotency_key, &digest).await?
        {
            return Ok(TeamRecord { team_id, revision });
        }
        let team_id = Uuid::now_v7();
        let membership_id = Uuid::now_v7();
        let owner_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM principals WHERE id=$1 AND state='active')",
        )
        .bind(command.initial_owner)
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::Database)?;
        if !owner_exists {
            return Err(StoreError::Forbidden);
        }
        sqlx::query("INSERT INTO teams (team_id, display_name, state, policy_generation, revision) VALUES ($1,$2,'active',1,1)")
            .bind(team_id).bind(&command.display_name).execute(&mut **tx).await.map_err(StoreError::Database)?;
        sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
            .bind(membership_id).bind(team_id).bind(command.initial_owner).execute(&mut **tx).await.map_err(StoreError::Database)?;
        let audit_id =
            audit_team_change(tx, actor, team_id, command.correlation_id, "team.create").await?;
        record_receipt(
            tx,
            actor,
            "team.create",
            command.idempotency_key,
            &digest,
            audit_id,
            team_id,
            1,
        )
        .await?;
        Ok(TeamRecord {
            team_id,
            revision: 1,
        })
    }

    /// Changes or removes a member while preserving the deferred last-owner invariant.
    pub(crate) async fn change_member_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: MembershipChange,
    ) -> Result<(), StoreError> {
        let digest = request_digest(&format!(
            "{}:{}:{}:{:?}",
            command.team_id, command.principal_id, command.expected_revision, command.role
        ));
        if receipt(
            tx,
            actor,
            "team.member.change",
            command.idempotency_key,
            &digest,
        )
        .await?
        .is_some()
        {
            return Ok(());
        }
        if !matches!(command.role, None | Some("owner" | "admin" | "member")) {
            return Err(StoreError::InputTooLarge);
        }
        let actor_role = sqlx::query_scalar::<_, String>("SELECT builtin_role FROM memberships WHERE team_id=$1 AND principal_id=$2 AND state='active' FOR SHARE")
            .bind(command.team_id).bind(actor.principal_id()).fetch_optional(&mut **tx).await.map_err(StoreError::Database)?.ok_or(StoreError::Forbidden)?;
        if command.principal_id == actor.principal_id()
            && command.role == Some("owner")
            && actor_role != "owner"
        {
            return Err(StoreError::Forbidden);
        }
        if command.role.is_none() {
            sqlx::query("DELETE FROM group_members WHERE team_id=$1 AND principal_id=$2")
                .bind(command.team_id)
                .bind(command.principal_id)
                .execute(&mut **tx)
                .await
                .map_err(StoreError::Database)?;
            sqlx::query("DELETE FROM membership_custom_roles WHERE team_id=$1 AND principal_id=$2")
                .bind(command.team_id)
                .bind(command.principal_id)
                .execute(&mut **tx)
                .await
                .map_err(StoreError::Database)?;
        }
        let updated = sqlx::query("UPDATE memberships SET state=CASE WHEN $1::text IS NULL THEN 'removed' ELSE 'active' END, builtin_role=COALESCE($1,builtin_role), revision=revision+1, removed_at=CASE WHEN $1::text IS NULL THEN clock_timestamp() ELSE NULL END WHERE team_id=$2 AND principal_id=$3 AND state='active' AND revision=$4")
            .bind(command.role).bind(command.team_id).bind(command.principal_id).bind(command.expected_revision).execute(&mut **tx).await.map_err(StoreError::Database)?;
        if updated.rows_affected() != 1 {
            return Err(StoreError::StaleState);
        }
        let audit_id = audit_team_change(
            tx,
            actor,
            command.team_id,
            command.correlation_id,
            "team.member.change",
        )
        .await?;
        record_receipt(
            tx,
            actor,
            "team.member.change",
            command.idempotency_key,
            &digest,
            audit_id,
            command.principal_id,
            command.expected_revision + 1,
        )
        .await?;
        Ok(())
    }
}

pub(crate) fn valid_team_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() || name.len() > 256 {
        Err(StoreError::InputTooLarge)
    } else {
        Ok(())
    }
}

async fn audit_team_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: ActorContext,
    team_id: Uuid,
    correlation_id: Uuid,
    action: &str,
) -> Result<Uuid, StoreError> {
    let audit_id = Uuid::now_v7();
    sqlx::query("INSERT INTO audit_events (audit_id,actor_principal_id,actor_kind,team_id,action,decision,policy_generation,recovery_generation,correlation_id,parameter_code,parameter_value,outcome) SELECT $1,$2,$3,$4,$5,'changed',t.policy_generation,r.recovery_generation,$6,'team','metadata','committed' FROM teams t JOIN relay_identity r ON r.state='normal' WHERE t.team_id=$4")
        .bind(audit_id).bind(actor.principal_id()).bind(actor_kind_name(actor.kind())).bind(team_id).bind(action).bind(correlation_id).execute(&mut **tx).await.map_err(|_error| StoreError::AuditUnavailable)?;
    Ok(audit_id)
}
