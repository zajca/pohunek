//! Resolves current team authority and durably audits sensitive decisions.

// Rust guideline compliant 2026-09-08

use sqlx::{Row, Transaction};
use uuid::Uuid;

mod admin;
pub(crate) mod rbac;
mod team;

#[cfg(all(test, feature = "postgres-tests"))]
mod admin_tests;

pub(crate) use admin::{valid_name, valid_permissions, valid_resource_kind};
#[doc(inline)]
pub use admin::{
    CreateGrant, CreateGroup, CreateRole, GrantPage, GrantRecord, GrantSubject, GroupMemberChange,
    GroupPage, GroupRecord, RemoveGrant, RemoveGroup, RemoveRole, RoleAssignmentChange, RolePage,
    RoleRecord, UpdateGrant, UpdateGroup, UpdateRole,
};
pub(crate) use team::valid_team_name;
#[doc(inline)]
pub use team::{
    CreateTeam, DisableTeam, MembershipChange, RemoveMember, TeamPage, TeamRecord, UpdateTeam,
};

use crate::store::{
    actor_kind_name, bounded_coordinate, lease::LeaseGuard, AuthenticationBinding,
    AuthorizationDecision, AuthorizationRequest, ResourceScope, Store, StoreError,
};

impl Store {
    /// Authorizes a request while its exact process fence is locked in the same transaction.
    pub(crate) async fn authorize_sensitive_with_lease(
        &self,
        request: AuthorizationRequest,
        lease: &LeaseGuard,
    ) -> Result<AuthorizationDecision, StoreError> {
        self.authorize_sensitive_inner(request, Some(lease)).await
    }

    async fn authorize_sensitive_inner(
        &self,
        request: AuthorizationRequest,
        lease: Option<&LeaseGuard>,
    ) -> Result<AuthorizationDecision, StoreError> {
        let resource = resource_coordinates(&request.resource)?;
        let mut transaction = self.begin_serializable().await?;
        if let Some(lease) = lease {
            self.verify_lease_in_transaction(&mut transaction, lease)
                .await?;
        }
        revalidate_authentication(&mut transaction, &request).await?;
        let row = sqlx::query(
            "SELECT t.policy_generation, r.recovery_generation, m.builtin_role, m.local_deny_generation \
             FROM teams t JOIN memberships m ON m.team_id = t.team_id \
             JOIN relay_identity r ON r.state = 'normal' \
             WHERE t.team_id = $1 AND t.state = 'active' AND m.principal_id = $2 AND m.state = 'active' \
             AND m.local_deny_generation > 0 AND r.recovery_generation = $3",
        ).bind(request.team_id).bind(request.actor.principal_id()).bind(request.actor.recovery_generation())
            .fetch_optional(&mut *transaction).await.map_err(StoreError::Database)?
            .ok_or(StoreError::Forbidden)?;
        let policy_generation: i64 = row.get("policy_generation");
        let recovery_generation: i64 = row.get("recovery_generation");
        let builtin_role: String = row.get("builtin_role");
        let allowed = builtin_permission(&builtin_role, &request.permission, request.actor.kind())
            || has_custom_role_permission(
                &mut transaction,
                request.team_id,
                request.actor.principal_id(),
                &request.permission,
            )
            .await?
            || has_matching_grant(
                &mut transaction,
                request.team_id,
                request.actor.principal_id(),
                &request.permission,
                resource.0,
                resource.1,
            )
            .await?;
        if !allowed {
            return Err(StoreError::Forbidden);
        }
        let audit_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO audit_events (audit_id, actor_principal_id, actor_kind, team_id, action, decision, policy_generation, recovery_generation, correlation_id, parameter_code, parameter_value, outcome) \
             VALUES ($1, $2, $3, $4, 'authorization.decision', 'allow', $5, $6, $7, 'permission', $8, 'granted')",
        ).bind(audit_id).bind(request.actor.principal_id()).bind(actor_kind_name(request.actor.kind())).bind(request.team_id)
            .bind(policy_generation).bind(recovery_generation).bind(request.correlation_id).bind(&request.permission)
            .execute(&mut *transaction).await.map_err(|_error| StoreError::AuditUnavailable)?;
        transaction.commit().await.map_err(StoreError::Database)?;
        Ok(AuthorizationDecision {
            audit_id,
            policy_generation,
            recovery_generation,
        })
    }
}

pub(crate) async fn revalidate_authentication(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    request: &AuthorizationRequest,
) -> Result<(), StoreError> {
    verify_current_actor(transaction, request.actor).await
}

/// Locks and validates the full current actor coordinate in a mutation transaction.
///
/// Callers must invoke this before authorizing a sensitive mutation. It binds
/// the source authentication row to an active principal and the normal relay
/// recovery generation, so stale contexts cannot bypass a revocation race.
pub(crate) async fn verify_current_actor(
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    actor: crate::store::ActorContext,
) -> Result<(), StoreError> {
    let identity_generation = sqlx::query_scalar::<_, i64>(
        "SELECT recovery_generation FROM relay_identity WHERE state = 'normal' AND recovery_generation = $1 FOR SHARE",
    )
    .bind(actor.recovery_generation())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(StoreError::Database)?
    .ok_or(StoreError::Forbidden)?;
    if identity_generation != actor.recovery_generation() {
        return Err(StoreError::Forbidden);
    }
    let principal = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM principals WHERE id = $1 AND state = 'active' FOR SHARE",
    )
    .bind(actor.principal_id())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(StoreError::Database)?;
    if principal.is_none() {
        return Err(StoreError::Forbidden);
    }
    let present = match actor.binding() {
        AuthenticationBinding::Credential => sqlx::query_scalar::<_, Uuid>(
            "SELECT principal_id FROM relay_credentials WHERE credential_id = $1 AND principal_id = $2 AND credential_generation = $3 AND recovery_generation = $4 AND revoked_at IS NULL AND expires_at > clock_timestamp() AND (rotation_overlap_ends_at IS NULL OR rotation_overlap_ends_at > clock_timestamp()) FOR SHARE",
        ).bind(actor.authentication_id()).bind(actor.principal_id())
            .bind(actor.authentication_generation()).bind(actor.recovery_generation())
            .fetch_optional(&mut **transaction).await.map_err(StoreError::Database)?,
        AuthenticationBinding::BrowserSession => sqlx::query_scalar::<_, Uuid>(
            "SELECT principal_id FROM browser_sessions WHERE session_id = $1 AND principal_id = $2 AND session_generation = $3 AND recovery_generation = $4 AND revoked_at IS NULL AND expires_at > clock_timestamp() AND idle_deadline > clock_timestamp() FOR SHARE",
        ).bind(actor.authentication_id()).bind(actor.principal_id())
            .bind(actor.authentication_generation()).bind(actor.recovery_generation())
            .fetch_optional(&mut **transaction).await.map_err(StoreError::Database)?,
    };
    if present.is_some() {
        Ok(())
    } else {
        Err(StoreError::Forbidden)
    }
}

fn resource_coordinates(resource: &ResourceScope) -> Result<(&'static str, &str), StoreError> {
    match resource {
        ResourceScope::Team => Ok(("team", "*")),
        ResourceScope::Host(id) => Ok(("host", bounded_coordinate(id)?)),
        ResourceScope::HostShare(id) => Ok(("host_share", bounded_coordinate(id)?)),
        ResourceScope::Project(id) => Ok(("project", bounded_coordinate(id)?)),
        ResourceScope::Session(id) => Ok(("session", bounded_coordinate(id)?)),
    }
}

async fn has_matching_grant(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team_id: Uuid,
    principal_id: Uuid,
    permission: &str,
    resource_kind: &str,
    resource_id: &str,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM grants g WHERE g.team_id = $1 AND g.state = 'active' AND g.permission = $2 \
         AND (g.resource_kind = 'team' OR (g.resource_kind = $3 AND g.resource_id = $4)) \
         AND ((g.subject_kind IN ('principal', 'service_account') AND g.subject_id = $5) \
              OR (g.subject_kind = 'group' AND EXISTS (SELECT 1 FROM group_members gm JOIN groups gr ON (gr.team_id, gr.group_id) = (gm.team_id, gm.group_id) JOIN memberships m ON (m.team_id, m.principal_id) = (gm.team_id, gm.principal_id) WHERE gm.team_id = $1 AND gm.group_id = g.subject_id AND gm.principal_id = $5 AND gr.state = 'active' AND m.state = 'active'))))",
    ).bind(team_id).bind(permission).bind(resource_kind).bind(resource_id).bind(principal_id)
        .fetch_one(&mut **transaction).await.map_err(StoreError::Database)
}

async fn has_custom_role_permission(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team_id: Uuid,
    principal_id: Uuid,
    permission: &str,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM membership_custom_roles mr \
         JOIN custom_roles cr ON (cr.team_id, cr.role_id) = (mr.team_id, mr.role_id) \
         JOIN custom_role_permissions rp ON (rp.team_id, rp.role_id) = (mr.team_id, mr.role_id) \
         JOIN memberships m ON (m.team_id, m.principal_id) = (mr.team_id, mr.principal_id) \
         WHERE mr.team_id=$1 AND mr.principal_id=$2 AND cr.state='active' \
         AND m.state='active' AND rp.permission=$3)",
    )
    .bind(team_id)
    .bind(principal_id)
    .bind(permission)
    .fetch_one(&mut **transaction)
    .await
    .map_err(StoreError::Database)
}

fn builtin_permission(role: &str, permission: &str, actor_kind: crate::store::ActorKind) -> bool {
    let known = matches!(
        permission,
        "team.admin"
            | "team.membership.read"
            | "team.membership.manage"
            | "team.group.manage"
            | "team.role.manage"
            | "team.grant.manage"
            | "session.metadata.read"
            | "session.terminal.observe"
            | "session.terminal.control"
            | "session.lifecycle.control"
            | "session.share.manage"
            | "session.remove"
    );
    actor_kind != crate::store::ActorKind::Service
        && known
        && (matches!(role, "owner" | "admin")
            || (role == "member" && permission == "team.membership.read"))
}
