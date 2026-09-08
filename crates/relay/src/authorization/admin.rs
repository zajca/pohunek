//! Transactional team group, role, and grant administration.

use sha2::{Digest, Sha256};
use sqlx::{Row, Transaction};
use uuid::Uuid;

use crate::{
    authorization::rbac::{audit_change, require_team_admin},
    store::{bounded_coordinate, ActorContext, Store, StoreError},
};

const MAX_PAGE_SIZE: i64 = 100;
const MAX_PERMISSION_COUNT: usize = 100;
const MAX_DISPLAY_NAME_BYTES: usize = 256;

/// A stable page request for one team-owned collection.
#[derive(Debug, Clone, Copy)]
pub struct GroupPage {
    pub after: Option<Uuid>,
    pub limit: i64,
}
/// A stable page request for custom roles.
#[derive(Debug, Clone, Copy)]
pub struct RolePage {
    pub after: Option<Uuid>,
    pub limit: i64,
}
/// A stable page request for grants.
#[derive(Debug, Clone, Copy)]
pub struct GrantPage {
    pub after: Option<Uuid>,
    pub limit: i64,
}

/// A group coordinate and revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupRecord {
    pub group_id: Uuid,
    pub revision: i64,
}
/// A custom-role coordinate and revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleRecord {
    pub role_id: Uuid,
    pub revision: i64,
}
/// A grant coordinate and revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantRecord {
    pub grant_id: Uuid,
    pub revision: i64,
}

/// Creates a team-scoped group.
#[derive(Debug, Clone)]
pub struct CreateGroup {
    pub team_id: Uuid,
    pub display_name: String,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}
/// Updates a group display name.
#[derive(Debug, Clone)]
pub struct UpdateGroup {
    pub team_id: Uuid,
    pub group_id: Uuid,
    pub expected_revision: i64,
    pub display_name: String,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}
/// Removes a group at its observed revision.
#[derive(Debug, Clone, Copy)]
pub struct RemoveGroup {
    pub team_id: Uuid,
    pub group_id: Uuid,
    pub expected_revision: i64,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}
/// Adds or removes an active team member from a group.
#[derive(Debug, Clone, Copy)]
pub struct GroupMemberChange {
    pub team_id: Uuid,
    pub group_id: Uuid,
    pub principal_id: Uuid,
    pub add: bool,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}

/// Creates a custom role from stable permissions.
#[derive(Debug, Clone)]
pub struct CreateRole {
    pub team_id: Uuid,
    pub display_name: String,
    pub permissions: Vec<String>,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}
/// Replaces a custom role's name and permission set.
#[derive(Debug, Clone)]
pub struct UpdateRole {
    pub team_id: Uuid,
    pub role_id: Uuid,
    pub expected_revision: i64,
    pub display_name: String,
    pub permissions: Vec<String>,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}
/// Removes a custom role at its observed revision.
#[derive(Debug, Clone, Copy)]
pub struct RemoveRole {
    pub team_id: Uuid,
    pub role_id: Uuid,
    pub expected_revision: i64,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}
/// Assigns or removes a custom role for an active membership.
#[derive(Debug, Clone, Copy)]
pub struct RoleAssignmentChange {
    pub team_id: Uuid,
    pub role_id: Uuid,
    pub principal_id: Uuid,
    pub assign: bool,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}

/// Selects a grant subject that is validated against the same team.
#[derive(Debug, Clone, Copy)]
pub enum GrantSubject {
    Principal(Uuid),
    ServiceAccount(Uuid),
    Group(Uuid),
}
/// Creates a narrowed stable-permission grant.
#[derive(Debug, Clone)]
pub struct CreateGrant {
    pub team_id: Uuid,
    pub subject: GrantSubject,
    pub resource_kind: &'static str,
    pub resource_id: String,
    pub permission: String,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}
/// Replaces a narrowed grant at its observed revision.
#[derive(Debug, Clone)]
pub struct UpdateGrant {
    pub team_id: Uuid,
    pub grant_id: Uuid,
    pub expected_revision: i64,
    pub subject: GrantSubject,
    pub resource_kind: &'static str,
    pub resource_id: String,
    pub permission: String,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}
/// Removes a grant at its observed revision.
#[derive(Debug, Clone, Copy)]
pub struct RemoveGrant {
    pub team_id: Uuid,
    pub grant_id: Uuid,
    pub expected_revision: i64,
    pub correlation_id: Uuid,
    pub idempotency_key: Uuid,
}

impl Store {
    /// Lists active groups after validating the caller's current team authority.
    pub async fn list_groups(
        &self,
        actor: ActorContext,
        team_id: Uuid,
        page: GroupPage,
    ) -> Result<Vec<GroupRecord>, StoreError> {
        valid_page(page.limit)?;
        let mut tx = self.begin_serializable().await?;
        require_team_admin(&mut tx, actor, team_id).await?;
        let rows = sqlx::query("SELECT group_id,revision FROM groups WHERE team_id=$1 AND state='active' AND ($2::uuid IS NULL OR group_id>$2) ORDER BY group_id LIMIT $3")
            .bind(team_id).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await.map_err(StoreError::Database)?;
        tx.commit().await.map_err(StoreError::Database)?;
        Ok(rows
            .into_iter()
            .map(|row| GroupRecord {
                group_id: row.get("group_id"),
                revision: row.get("revision"),
            })
            .collect())
    }

    /// Creates a group and its durable idempotency receipt.
    pub(crate) async fn create_group_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: CreateGroup,
    ) -> Result<GroupRecord, StoreError> {
        valid_name(&command.display_name)?;
        let digest = request_digest(&format!("{}:{}", command.team_id, command.display_name));
        if let Some(record) =
            receipt(tx, actor, "group.create", command.idempotency_key, &digest).await?
        {
            return Ok(GroupRecord {
                group_id: record.0,
                revision: record.1,
            });
        }
        let group_id = Uuid::now_v7();
        sqlx::query("INSERT INTO groups (team_id,group_id,display_name,state,revision) VALUES ($1,$2,$3,'active',1)").bind(command.team_id).bind(group_id).bind(&command.display_name).execute(&mut **tx).await.map_err(StoreError::Database)?;
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            "group.create",
            command.correlation_id,
            command.idempotency_key,
            "group",
            "created",
        )
        .await?;
        record_receipt(
            tx,
            actor,
            "group.create",
            command.idempotency_key,
            &digest,
            audit_id,
            group_id,
            1,
        )
        .await?;
        Ok(GroupRecord {
            group_id,
            revision: 1,
        })
    }

    /// Updates a group with an optimistic revision check.
    pub(crate) async fn update_group_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: UpdateGroup,
    ) -> Result<GroupRecord, StoreError> {
        valid_name(&command.display_name)?;
        let digest = request_digest(&format!(
            "{}:{}:{}:{}",
            command.team_id, command.group_id, command.expected_revision, command.display_name
        ));
        if let Some(record) =
            receipt(tx, actor, "group.update", command.idempotency_key, &digest).await?
        {
            return Ok(GroupRecord {
                group_id: record.0,
                revision: record.1,
            });
        }
        let row = sqlx::query("UPDATE groups SET display_name=$1,revision=revision+1 WHERE team_id=$2 AND group_id=$3 AND state='active' AND revision=$4 RETURNING revision")
            .bind(&command.display_name).bind(command.team_id).bind(command.group_id).bind(command.expected_revision)
            .fetch_optional(&mut **tx).await.map_err(StoreError::Database)?.ok_or(StoreError::StaleState)?;
        let revision: i64 = row.get("revision");
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            "group.update",
            command.correlation_id,
            command.idempotency_key,
            "group",
            "updated",
        )
        .await?;
        record_receipt(
            tx,
            actor,
            "group.update",
            command.idempotency_key,
            &digest,
            audit_id,
            command.group_id,
            revision,
        )
        .await?;
        Ok(GroupRecord {
            group_id: command.group_id,
            revision,
        })
    }

    /// Removes a group and immediately removes it from authorization matches.
    pub(crate) async fn remove_group_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: RemoveGroup,
    ) -> Result<(), StoreError> {
        let digest = request_digest(&format!(
            "{}:{}:{}",
            command.team_id, command.group_id, command.expected_revision
        ));
        if receipt(tx, actor, "group.remove", command.idempotency_key, &digest)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let row = sqlx::query("UPDATE groups SET state='removed',revision=revision+1 WHERE team_id=$1 AND group_id=$2 AND state='active' AND revision=$3 RETURNING revision").bind(command.team_id).bind(command.group_id).bind(command.expected_revision).fetch_optional(&mut **tx).await.map_err(StoreError::Database)?.ok_or(StoreError::StaleState)?;
        let revision: i64 = row.get("revision");
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            "group.remove",
            command.correlation_id,
            command.idempotency_key,
            "group",
            "removed",
        )
        .await?;
        record_receipt(
            tx,
            actor,
            "group.remove",
            command.idempotency_key,
            &digest,
            audit_id,
            command.group_id,
            revision,
        )
        .await?;
        Ok(())
    }

    /// Changes group membership only for an active same-team member and group.
    pub(crate) async fn change_group_member_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: GroupMemberChange,
    ) -> Result<(), StoreError> {
        let digest = request_digest(&format!(
            "{}:{}:{}:{}",
            command.team_id, command.group_id, command.principal_id, command.add
        ));
        if receipt(
            tx,
            actor,
            "group.member.change",
            command.idempotency_key,
            &digest,
        )
        .await?
        .is_some()
        {
            return Ok(());
        }
        if command.add {
            let inserted = sqlx::query("INSERT INTO group_members (team_id,group_id,principal_id,revision) SELECT $1,$2,$3,1 WHERE EXISTS (SELECT 1 FROM groups WHERE team_id=$1 AND group_id=$2 AND state='active') AND EXISTS (SELECT 1 FROM memberships WHERE team_id=$1 AND principal_id=$3 AND state='active') ON CONFLICT DO NOTHING").bind(command.team_id).bind(command.group_id).bind(command.principal_id).execute(&mut **tx).await.map_err(StoreError::Database)?;
            if inserted.rows_affected() == 0 {
                let current = sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM group_members WHERE team_id=$1 AND group_id=$2 AND principal_id=$3)").bind(command.team_id).bind(command.group_id).bind(command.principal_id).fetch_one(&mut **tx).await.map_err(StoreError::Database)?;
                if !current {
                    return Err(StoreError::Forbidden);
                }
            }
        } else {
            sqlx::query(
                "DELETE FROM group_members WHERE team_id=$1 AND group_id=$2 AND principal_id=$3",
            )
            .bind(command.team_id)
            .bind(command.group_id)
            .bind(command.principal_id)
            .execute(&mut **tx)
            .await
            .map_err(StoreError::Database)?;
        }
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            "group.member.change",
            command.correlation_id,
            command.idempotency_key,
            "group_member",
            if command.add { "added" } else { "removed" },
        )
        .await?;
        record_receipt(
            tx,
            actor,
            "group.member.change",
            command.idempotency_key,
            &digest,
            audit_id,
            command.group_id,
            1,
        )
        .await?;
        Ok(())
    }

    /// Lists active custom roles after current authorization validation.
    pub async fn list_roles(
        &self,
        actor: ActorContext,
        team_id: Uuid,
        page: RolePage,
    ) -> Result<Vec<RoleRecord>, StoreError> {
        valid_page(page.limit)?;
        let mut tx = self.begin_serializable().await?;
        require_team_admin(&mut tx, actor, team_id).await?;
        let rows = sqlx::query("SELECT role_id,revision FROM custom_roles WHERE team_id=$1 AND state='active' AND ($2::uuid IS NULL OR role_id>$2) ORDER BY role_id LIMIT $3").bind(team_id).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await.map_err(StoreError::Database)?;
        tx.commit().await.map_err(StoreError::Database)?;
        Ok(rows
            .into_iter()
            .map(|row| RoleRecord {
                role_id: row.get("role_id"),
                revision: row.get("revision"),
            })
            .collect())
    }

    /// Creates a custom role from known stable permissions.
    pub(crate) async fn create_role_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: CreateRole,
    ) -> Result<RoleRecord, StoreError> {
        self.mutate_role_in_transaction(tx, actor, None, command, "role.create")
            .await
    }
    /// Updates a custom role from known stable permissions.
    pub(crate) async fn update_role_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: UpdateRole,
    ) -> Result<RoleRecord, StoreError> {
        self.mutate_role_in_transaction(
            tx,
            actor,
            Some((command.role_id, command.expected_revision)),
            CreateRole {
                team_id: command.team_id,
                display_name: command.display_name,
                permissions: command.permissions,
                correlation_id: command.correlation_id,
                idempotency_key: command.idempotency_key,
            },
            "role.update",
        )
        .await
    }

    async fn mutate_role_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        existing: Option<(Uuid, i64)>,
        command: CreateRole,
        action: &'static str,
    ) -> Result<RoleRecord, StoreError> {
        valid_name(&command.display_name)?;
        valid_permissions(&command.permissions)?;
        let digest = request_digest(&format!(
            "{}:{:?}:{}:{:?}",
            command.team_id, existing, command.display_name, command.permissions
        ));
        if let Some(record) = receipt(tx, actor, action, command.idempotency_key, &digest).await? {
            return Ok(RoleRecord {
                role_id: record.0,
                revision: record.1,
            });
        }
        let (role_id, revision) = if let Some((role_id, expected_revision)) = existing {
            let row = sqlx::query("UPDATE custom_roles SET display_name=$1,revision=revision+1 WHERE team_id=$2 AND role_id=$3 AND state='active' AND revision=$4 RETURNING revision").bind(&command.display_name).bind(command.team_id).bind(role_id).bind(expected_revision).fetch_optional(&mut **tx).await.map_err(StoreError::Database)?.ok_or(StoreError::StaleState)?;
            sqlx::query("DELETE FROM custom_role_permissions WHERE team_id=$1 AND role_id=$2")
                .bind(command.team_id)
                .bind(role_id)
                .execute(&mut **tx)
                .await
                .map_err(StoreError::Database)?;
            (role_id, row.get("revision"))
        } else {
            let role_id = Uuid::now_v7();
            sqlx::query("INSERT INTO custom_roles (team_id,role_id,display_name,state,revision) VALUES ($1,$2,$3,'active',1)").bind(command.team_id).bind(role_id).bind(&command.display_name).execute(&mut **tx).await.map_err(StoreError::Database)?;
            (role_id, 1)
        };
        insert_permissions(tx, command.team_id, role_id, &command.permissions).await?;
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            action,
            command.correlation_id,
            command.idempotency_key,
            "role",
            "committed",
        )
        .await?;
        record_receipt(
            tx,
            actor,
            action,
            command.idempotency_key,
            &digest,
            audit_id,
            role_id,
            revision,
        )
        .await?;
        Ok(RoleRecord { role_id, revision })
    }

    /// Removes a role and prevents it from contributing to future decisions.
    pub(crate) async fn remove_role_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: RemoveRole,
    ) -> Result<(), StoreError> {
        let digest = request_digest(&format!(
            "{}:{}:{}",
            command.team_id, command.role_id, command.expected_revision
        ));
        if receipt(tx, actor, "role.remove", command.idempotency_key, &digest)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let row=sqlx::query("UPDATE custom_roles SET state='removed',revision=revision+1 WHERE team_id=$1 AND role_id=$2 AND state='active' AND revision=$3 RETURNING revision").bind(command.team_id).bind(command.role_id).bind(command.expected_revision).fetch_optional(&mut **tx).await.map_err(StoreError::Database)?.ok_or(StoreError::StaleState)?;
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            "role.remove",
            command.correlation_id,
            command.idempotency_key,
            "role",
            "removed",
        )
        .await?;
        record_receipt(
            tx,
            actor,
            "role.remove",
            command.idempotency_key,
            &digest,
            audit_id,
            command.role_id,
            row.get("revision"),
        )
        .await?;
        Ok(())
    }

    /// Changes an assignment only when both the role and membership share the team.
    pub(crate) async fn change_role_assignment_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: RoleAssignmentChange,
    ) -> Result<(), StoreError> {
        let digest = request_digest(&format!(
            "{}:{}:{}:{}",
            command.team_id, command.role_id, command.principal_id, command.assign
        ));
        if receipt(
            tx,
            actor,
            "role.assignment.change",
            command.idempotency_key,
            &digest,
        )
        .await?
        .is_some()
        {
            return Ok(());
        }
        if command.assign {
            let inserted=sqlx::query("INSERT INTO membership_custom_roles (team_id,principal_id,role_id) SELECT $1,$2,$3 WHERE EXISTS (SELECT 1 FROM memberships WHERE team_id=$1 AND principal_id=$2 AND state='active') AND EXISTS (SELECT 1 FROM custom_roles WHERE team_id=$1 AND role_id=$3 AND state='active') ON CONFLICT DO NOTHING").bind(command.team_id).bind(command.principal_id).bind(command.role_id).execute(&mut **tx).await.map_err(StoreError::Database)?;
            if inserted.rows_affected() == 0 {
                return Err(StoreError::Forbidden);
            }
        } else {
            sqlx::query("DELETE FROM membership_custom_roles WHERE team_id=$1 AND principal_id=$2 AND role_id=$3").bind(command.team_id).bind(command.principal_id).bind(command.role_id).execute(&mut **tx).await.map_err(StoreError::Database)?;
        }
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            "role.assignment.change",
            command.correlation_id,
            command.idempotency_key,
            "role_assignment",
            if command.assign {
                "assigned"
            } else {
                "removed"
            },
        )
        .await?;
        record_receipt(
            tx,
            actor,
            "role.assignment.change",
            command.idempotency_key,
            &digest,
            audit_id,
            command.role_id,
            1,
        )
        .await?;
        Ok(())
    }

    /// Lists active grants after current authorization validation.
    pub async fn list_grants(
        &self,
        actor: ActorContext,
        team_id: Uuid,
        page: GrantPage,
    ) -> Result<Vec<GrantRecord>, StoreError> {
        valid_page(page.limit)?;
        let mut tx = self.begin_serializable().await?;
        require_team_admin(&mut tx, actor, team_id).await?;
        let rows=sqlx::query("SELECT grant_id,revision FROM grants WHERE team_id=$1 AND state='active' AND ($2::uuid IS NULL OR grant_id>$2) ORDER BY grant_id LIMIT $3").bind(team_id).bind(page.after).bind(page.limit).fetch_all(&mut *tx).await.map_err(StoreError::Database)?;
        tx.commit().await.map_err(StoreError::Database)?;
        Ok(rows
            .into_iter()
            .map(|row| GrantRecord {
                grant_id: row.get("grant_id"),
                revision: row.get("revision"),
            })
            .collect())
    }

    /// Creates a validated narrowed grant.
    pub(crate) async fn create_grant_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: CreateGrant,
    ) -> Result<GrantRecord, StoreError> {
        self.mutate_grant_in_transaction(tx, actor, None, command, "grant.create")
            .await
    }
    /// Updates a validated narrowed grant.
    pub(crate) async fn update_grant_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: UpdateGrant,
    ) -> Result<GrantRecord, StoreError> {
        self.mutate_grant_in_transaction(
            tx,
            actor,
            Some((command.grant_id, command.expected_revision)),
            CreateGrant {
                team_id: command.team_id,
                subject: command.subject,
                resource_kind: command.resource_kind,
                resource_id: command.resource_id,
                permission: command.permission,
                correlation_id: command.correlation_id,
                idempotency_key: command.idempotency_key,
            },
            "grant.update",
        )
        .await
    }

    async fn mutate_grant_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        existing: Option<(Uuid, i64)>,
        command: CreateGrant,
        action: &'static str,
    ) -> Result<GrantRecord, StoreError> {
        let resource_id = bounded_coordinate(&command.resource_id)?;
        valid_resource_kind(command.resource_kind)?;
        bounded_coordinate(&command.permission)?;
        let (subject_kind, subject_id) = subject_coordinates(command.subject);
        let digest = request_digest(&format!(
            "{}:{:?}:{}:{}:{}:{}:{}",
            command.team_id,
            existing,
            subject_kind,
            subject_id,
            command.resource_kind,
            resource_id,
            command.permission
        ));
        if let Some(record) = receipt(tx, actor, action, command.idempotency_key, &digest).await? {
            return Ok(GrantRecord {
                grant_id: record.0,
                revision: record.1,
            });
        }
        verify_subject(tx, command.team_id, subject_kind, subject_id).await?;
        verify_permission(tx, &command.permission).await?;
        let (grant_id, revision) = if let Some((grant_id, expected_revision)) = existing {
            let row=sqlx::query("UPDATE grants SET subject_kind=$1,subject_id=$2,resource_kind=$3,resource_id=$4,permission=$5,revision=revision+1 WHERE team_id=$6 AND grant_id=$7 AND state='active' AND revision=$8 RETURNING revision").bind(subject_kind).bind(subject_id).bind(command.resource_kind).bind(resource_id).bind(&command.permission).bind(command.team_id).bind(grant_id).bind(expected_revision).fetch_optional(&mut **tx).await.map_err(StoreError::Database)?.ok_or(StoreError::StaleState)?;
            (grant_id, row.get("revision"))
        } else {
            let grant_id = Uuid::now_v7();
            sqlx::query("INSERT INTO grants (team_id,grant_id,subject_kind,subject_id,resource_kind,resource_id,permission,state,revision) VALUES ($1,$2,$3,$4,$5,$6,$7,'active',1)").bind(command.team_id).bind(grant_id).bind(subject_kind).bind(subject_id).bind(command.resource_kind).bind(resource_id).bind(&command.permission).execute(&mut **tx).await.map_err(StoreError::Database)?;
            (grant_id, 1)
        };
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            action,
            command.correlation_id,
            command.idempotency_key,
            "grant",
            "committed",
        )
        .await?;
        record_receipt(
            tx,
            actor,
            action,
            command.idempotency_key,
            &digest,
            audit_id,
            grant_id,
            revision,
        )
        .await?;
        Ok(GrantRecord { grant_id, revision })
    }

    /// Removes a narrowed grant at its observed revision.
    pub(crate) async fn remove_grant_in_transaction(
        &self,
        tx: &mut Transaction<'_, sqlx::Postgres>,
        actor: ActorContext,
        command: RemoveGrant,
    ) -> Result<(), StoreError> {
        let digest = request_digest(&format!(
            "{}:{}:{}",
            command.team_id, command.grant_id, command.expected_revision
        ));
        if receipt(tx, actor, "grant.remove", command.idempotency_key, &digest)
            .await?
            .is_some()
        {
            return Ok(());
        }
        let row=sqlx::query("UPDATE grants SET state='removed',revision=revision+1 WHERE team_id=$1 AND grant_id=$2 AND state='active' AND revision=$3 RETURNING revision").bind(command.team_id).bind(command.grant_id).bind(command.expected_revision).fetch_optional(&mut **tx).await.map_err(StoreError::Database)?.ok_or(StoreError::StaleState)?;
        let audit_id = audit_change(
            tx,
            actor,
            command.team_id,
            "grant.remove",
            command.correlation_id,
            command.idempotency_key,
            "grant",
            "removed",
        )
        .await?;
        record_receipt(
            tx,
            actor,
            "grant.remove",
            command.idempotency_key,
            &digest,
            audit_id,
            command.grant_id,
            row.get("revision"),
        )
        .await?;
        Ok(())
    }
}

fn valid_page(limit: i64) -> Result<(), StoreError> {
    if (1..=MAX_PAGE_SIZE).contains(&limit) {
        Ok(())
    } else {
        Err(StoreError::InputTooLarge)
    }
}
pub(crate) fn valid_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() || name.len() > MAX_DISPLAY_NAME_BYTES {
        Err(StoreError::InputTooLarge)
    } else {
        Ok(())
    }
}
pub(crate) fn valid_permissions(permissions: &[String]) -> Result<(), StoreError> {
    if permissions.is_empty()
        || permissions.len() > MAX_PERMISSION_COUNT
        || permissions.iter().any(|p| bounded_coordinate(p).is_err())
    {
        Err(StoreError::InputTooLarge)
    } else {
        Ok(())
    }
}
pub(crate) fn valid_resource_kind(kind: &str) -> Result<(), StoreError> {
    match kind {
        "team" | "host" | "host_share" | "project" | "session" => Ok(()),
        _ => Err(StoreError::InputTooLarge),
    }
}
fn subject_coordinates(subject: GrantSubject) -> (&'static str, Uuid) {
    match subject {
        GrantSubject::Principal(id) => ("principal", id),
        GrantSubject::ServiceAccount(id) => ("service_account", id),
        GrantSubject::Group(id) => ("group", id),
    }
}
pub(crate) fn request_digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

async fn verify_permission(
    tx: &mut Transaction<'_, sqlx::Postgres>,
    permission: &str,
) -> Result<(), StoreError> {
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM permissions WHERE permission=$1)",
    )
    .bind(permission)
    .fetch_one(&mut **tx)
    .await
    .map_err(StoreError::Database)?
    {
        Ok(())
    } else {
        Err(StoreError::Forbidden)
    }
}
async fn insert_permissions(
    tx: &mut Transaction<'_, sqlx::Postgres>,
    team_id: Uuid,
    role_id: Uuid,
    permissions: &[String],
) -> Result<(), StoreError> {
    for permission in permissions {
        verify_permission(tx, permission).await?;
        sqlx::query(
            "INSERT INTO custom_role_permissions (team_id,role_id,permission) VALUES ($1,$2,$3)",
        )
        .bind(team_id)
        .bind(role_id)
        .bind(permission)
        .execute(&mut **tx)
        .await
        .map_err(StoreError::Database)?;
    }
    Ok(())
}
async fn verify_subject(
    tx: &mut Transaction<'_, sqlx::Postgres>,
    team_id: Uuid,
    kind: &str,
    id: Uuid,
) -> Result<(), StoreError> {
    let sql=match kind{"group"=>"SELECT EXISTS (SELECT 1 FROM groups WHERE team_id=$1 AND group_id=$2 AND state='active')", "principal"=>"SELECT EXISTS (SELECT 1 FROM memberships m JOIN principals p ON p.id=m.principal_id WHERE m.team_id=$1 AND m.principal_id=$2 AND m.state='active' AND p.state='active' AND p.kind='human')", "service_account"=>"SELECT EXISTS (SELECT 1 FROM memberships m JOIN principals p ON p.id=m.principal_id WHERE m.team_id=$1 AND m.principal_id=$2 AND m.state='active' AND p.state='active' AND p.kind='service')", _=>return Err(StoreError::Forbidden)};
    if sqlx::query_scalar::<_, bool>(sql)
        .bind(team_id)
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::Database)?
    {
        Ok(())
    } else {
        Err(StoreError::Forbidden)
    }
}
pub(crate) async fn receipt(
    tx: &mut Transaction<'_, sqlx::Postgres>,
    actor: ActorContext,
    action: &str,
    key: Uuid,
    digest: &[u8; 32],
) -> Result<Option<(Uuid, i64)>, StoreError> {
    let row=sqlx::query("SELECT request_digest,result_id,result_revision FROM mutation_receipts WHERE actor_principal_id=$1 AND action=$2 AND idempotency_key=$3 FOR KEY SHARE").bind(actor.principal_id()).bind(action).bind(key).fetch_optional(&mut **tx).await.map_err(StoreError::Database)?;
    match row {
        Some(row) => {
            let stored: Vec<u8> = row.get("request_digest");
            if stored.as_slice() != digest {
                return Err(StoreError::IdempotencyConflict);
            }
            Ok(Some((row.get("result_id"), row.get("result_revision"))))
        }
        None => Ok(None),
    }
}
#[expect(
    clippy::too_many_arguments,
    reason = "Receipt coordinates are independently validated durable fields."
)]
pub(crate) async fn record_receipt(
    tx: &mut Transaction<'_, sqlx::Postgres>,
    actor: ActorContext,
    action: &str,
    key: Uuid,
    digest: &[u8; 32],
    audit_id: Uuid,
    result_id: Uuid,
    revision: i64,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO mutation_receipts (actor_principal_id,action,idempotency_key,request_digest,audit_id,outcome_code,result_id,result_revision) VALUES ($1,$2,$3,$4,$5,'committed',$6,$7)").bind(actor.principal_id()).bind(action).bind(key).bind(digest.as_slice()).bind(audit_id).bind(result_id).bind(revision).execute(&mut **tx).await.map_err(StoreError::Database)?;
    Ok(())
}
