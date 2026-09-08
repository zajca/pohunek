//! Shared current-state authorization checks for team administration.

use sqlx::Transaction;
use uuid::Uuid;

use crate::{
    authorization::revalidate_authentication,
    store::{ActorContext, AuthorizationRequest, ResourceScope, StoreError},
};

/// Stable administrative permissions are deliberately distinct from content permissions.
pub(crate) const TEAM_ADMIN_PERMISSION: &str = "team.admin";
pub(crate) const TEAM_MEMBERSHIP_READ_PERMISSION: &str = "team.membership.read";
pub(crate) const TEAM_MEMBERSHIP_MANAGE_PERMISSION: &str = "team.membership.manage";
pub(crate) const TEAM_GROUP_MANAGE_PERMISSION: &str = "team.group.manage";
pub(crate) const TEAM_ROLE_MANAGE_PERMISSION: &str = "team.role.manage";
pub(crate) const TEAM_GRANT_MANAGE_PERMISSION: &str = "team.grant.manage";

/// Requires one current team permission within an existing mutation transaction.
pub(crate) async fn require_team_permission(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    actor: ActorContext,
    team_id: Uuid,
    permission: &'static str,
) -> Result<(i64, i64), StoreError> {
    let request = AuthorizationRequest {
        actor,
        team_id,
        permission: permission.to_owned(),
        resource: ResourceScope::Team,
        correlation_id: Uuid::now_v7(),
    };
    revalidate_authentication(transaction, &request).await?;
    let row = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT t.policy_generation, r.recovery_generation, m.builtin_role FROM teams t \
         JOIN memberships m ON m.team_id = t.team_id \
         JOIN relay_identity r ON r.state = 'normal' \
         WHERE t.team_id = $1 AND t.state = 'active' AND m.principal_id = $2 \
         AND m.state = 'active' AND r.recovery_generation = $3 FOR SHARE",
    )
    .bind(team_id)
    .bind(actor.principal_id())
    .bind(actor.recovery_generation())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(StoreError::Database)?
    .ok_or(StoreError::Forbidden)?;
    let allowed = super::builtin_permission(&row.2, permission, actor.kind())
        || super::has_custom_role_permission(
            transaction,
            team_id,
            actor.principal_id(),
            permission,
        )
        .await?
        || super::has_matching_grant(
            transaction,
            team_id,
            actor.principal_id(),
            permission,
            "team",
            "*",
        )
        .await?;
    if !allowed {
        return Err(StoreError::Forbidden);
    }
    Ok((row.0, row.1))
}

/// Requires a current permission before an exact receipt replay.
///
/// The caller may bypass the normal active-team requirement only after an exact
/// receipt match. Active membership, current authentication, and current RBAC
/// permission remain mandatory.
pub(crate) async fn require_team_permission_for_replay(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    actor: ActorContext,
    team_id: Uuid,
    permission: &'static str,
) -> Result<(i64, i64), StoreError> {
    let request = AuthorizationRequest {
        actor,
        team_id,
        permission: permission.to_owned(),
        resource: ResourceScope::Team,
        correlation_id: Uuid::now_v7(),
    };
    revalidate_authentication(transaction, &request).await?;
    let row = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT t.policy_generation, r.recovery_generation, m.builtin_role FROM teams t \
         JOIN memberships m ON m.team_id = t.team_id \
         JOIN relay_identity r ON r.state = 'normal' \
         WHERE t.team_id = $1 AND m.principal_id = $2 AND m.state = 'active' \
         AND r.recovery_generation = $3 FOR SHARE",
    )
    .bind(team_id)
    .bind(actor.principal_id())
    .bind(actor.recovery_generation())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(StoreError::Database)?
    .ok_or(StoreError::Forbidden)?;
    let allowed = super::builtin_permission(&row.2, permission, actor.kind())
        || super::has_custom_role_permission(
            transaction,
            team_id,
            actor.principal_id(),
            permission,
        )
        .await?
        || super::has_matching_grant(
            transaction,
            team_id,
            actor.principal_id(),
            permission,
            "team",
            "*",
        )
        .await?;
    if !allowed {
        return Err(StoreError::Forbidden);
    }
    Ok((row.0, row.1))
}

/// Requires the legacy broad administrative permission within a transaction.
pub(crate) async fn require_team_admin(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    actor: ActorContext,
    team_id: Uuid,
) -> Result<(i64, i64), StoreError> {
    require_team_permission(transaction, actor, team_id, TEAM_ADMIN_PERMISSION).await
}

#[expect(
    clippy::too_many_arguments,
    reason = "Audit rows deliberately carry each bounded durable coordinate."
)]
pub(crate) async fn audit_change(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    actor: ActorContext,
    team_id: Uuid,
    action: &str,
    correlation_id: Uuid,
    idempotency_key: Uuid,
    parameter_code: &str,
    parameter_value: &str,
) -> Result<Uuid, StoreError> {
    let audit_id = Uuid::now_v7();
    sqlx::query("INSERT INTO audit_events (audit_id,actor_principal_id,actor_kind,team_id,action,decision,policy_generation,recovery_generation,correlation_id,idempotency_key,parameter_code,parameter_value,outcome) SELECT $1,$2,$3,$4,$5,'changed',policy_generation,$6,$7,$8,$9,$10,'committed' FROM teams WHERE team_id=$4")
        .bind(audit_id).bind(actor.principal_id()).bind(crate::store::actor_kind_name(actor.kind())).bind(team_id).bind(action).bind(actor.recovery_generation()).bind(correlation_id).bind(idempotency_key).bind(parameter_code).bind(parameter_value)
        .execute(&mut **transaction)
        .await
        .map_err(|error| {
            let database = error.as_database_error();
            match database.and_then(|database| database.code()).as_deref() {
                Some("40001" | "40P01") => StoreError::Database(error),
                Some("23505")
                    if database.is_some_and(|database| {
                        database.constraint() == Some("audit_events_idempotency_idx")
                    }) =>
                {
                    StoreError::Contended
                }
                _ => StoreError::AuditUnavailable,
            }
        })?;
    Ok(audit_id)
}
