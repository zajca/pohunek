//! `PostgreSQL` integration coverage for team administration invariants.

use std::{env, os::unix::fs::PermissionsExt as _, sync::Arc};

use ed25519_dalek::SigningKey;
use sqlx::{AssertSqlSafe, PgPool};
use tempfile::TempDir;
use uuid::Uuid;

use super::{
    CreateGrant, CreateGroup, CreateRole, DisableTeam, GrantSubject, GroupMemberChange,
    RemoveGrant, RemoveMember, RemoveRole, RoleAssignmentChange, UpdateGroup, UpdateTeam,
};
use crate::store::{
    ActorKind, AuthenticationBinding, AuthorizationRequest, ResourceScope, Store, StoreError,
};
use crate::{
    admission::{Authority, AuthorityLimits, RevocationCommand},
    authorization::CreateTeam,
    recovery::WitnessStore,
};

const BOOTSTRAP_CONNECTIONS: u32 = 1;
const TEST_CONNECTIONS: u32 = 4;

async fn seed_identity(store: &Store, principal: Uuid) {
    sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'https://issuer.test',$2,$1,1)")
        .bind(principal).bind(principal.to_string()).execute(store.pool()).await.expect("seed credential provenance");
}

async fn fixture() -> (Store, String, Uuid, Uuid, Uuid) {
    let url = env::var("POHUNEK_RELAY_TEST_DATABASE_URL")
        .expect("postgres-tests requires POHUNEK_RELAY_TEST_DATABASE_URL");
    let bootstrap = Store::connect(&url, BOOTSTRAP_CONNECTIONS)
        .await
        .expect("connect PostgreSQL fixture");
    let schema = format!("relay_admin_{}", Uuid::now_v7().simple());
    // `schema` is derived only from a fresh UUID and never from test input.
    sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(bootstrap.pool())
        .await
        .expect("create isolated schema");
    sqlx::raw_sql(AssertSqlSafe(format!(
        "SET search_path TO {schema};{};{}",
        include_str!("../../migrations/0001_relay_foundation.sql"),
        include_str!("../../migrations/0002_auth.sql")
    )))
    .execute(bootstrap.pool())
    .await
    .expect("run relay migrations");
    let scoped_url = format!("{url}?options[search_path]={schema}");
    let store = Store::connect(&scoped_url, TEST_CONNECTIONS)
        .await
        .expect("connect schema-scoped PostgreSQL fixture");
    let relay_id = "test-relay";
    sqlx::query("INSERT INTO relay_identity (relay_id,recovery_generation,state,revision) VALUES ($1,1,'normal',1)").bind(relay_id).execute(store.pool()).await.expect("seed relay");
    let owner = Uuid::now_v7();
    let member = Uuid::now_v7();
    let credential = Uuid::now_v7();
    for principal in [owner, member] {
        sqlx::query(
            "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)",
        )
        .bind(principal)
        .execute(store.pool())
        .await
        .expect("seed principal");
        seed_identity(&store, principal).await;
    }
    sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,credential_kind,credential_generation,recovery_generation,expires_at,identity_id,digest_key_id,rotation_family_id) VALUES ($1,$2,decode(repeat('00',32),'hex'),$3,'human',1,1,clock_timestamp()+interval '1 hour',$3,'test',$1)").bind(credential).bind(credential.to_string()).bind(owner).execute(store.pool()).await.expect("seed credential");
    (store, schema, owner, member, credential)
}

async fn cleanup(pool: &PgPool, schema: &str) {
    sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(pool)
        .await
        .expect("remove isolated schema");
}

fn owner_actor(owner: Uuid, credential: Uuid) -> crate::store::ActorContext {
    Store::authenticated_actor(
        owner,
        credential,
        1,
        1,
        ActorKind::Human,
        AuthenticationBinding::Credential,
    )
}

async fn authority(store: Store) -> (Authority, TempDir) {
    let directory = tempfile::tempdir().expect("create witness directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("make witness directory private");
    let witness = Arc::new(
        WitnessStore::open(
            directory.path(),
            SigningKey::from_bytes(&[9; 32]),
            "test-key".to_owned(),
        )
        .expect("open witness"),
    );
    let current = witness
        .begin_run(None, "test-relay", 1)
        .expect("begin witness run");
    let lease = store
        .acquire_lease("test-relay", Uuid::now_v7(), 1)
        .await
        .expect("acquire lease");
    (
        Authority::new(
            store,
            lease,
            witness,
            current,
            AuthorityLimits {
                global: 4,
                per_team: 4,
                per_principal: 4,
            },
        )
        .expect("create authority"),
        directory,
    )
}

async fn team(store: &Store, owner: Uuid) -> Uuid {
    let team = Uuid::now_v7();
    sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'team','active',1,1)").bind(team).execute(store.pool()).await.expect("seed team");
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)").bind(Uuid::now_v7()).bind(team).bind(owner).execute(store.pool()).await.expect("seed owner");
    team
}

async fn human_actor(store: &Store, principal: Uuid) -> crate::store::ActorContext {
    let credential = Uuid::now_v7();
    sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,credential_kind,credential_generation,recovery_generation,expires_at,identity_id,digest_key_id,rotation_family_id) VALUES ($1,$2,decode(repeat('44',32),'hex'),$3,'human',1,1,clock_timestamp()+interval '1 hour',$3,'test',$1)")
        .bind(credential).bind(credential.to_string()).bind(principal).execute(store.pool()).await.expect("seed human credential");
    Store::authenticated_actor(
        principal,
        credential,
        1,
        1,
        ActorKind::Human,
        AuthenticationBinding::Credential,
    )
}

async fn service_actor(store: &Store, team_id: Uuid) -> crate::store::ActorContext {
    let principal = Uuid::now_v7();
    let credential = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'service','active',1)",
    )
    .bind(principal)
    .execute(store.pool())
    .await
    .expect("seed service principal");
    sqlx::query("INSERT INTO service_accounts (principal_id,team_id,display_name) VALUES ($1,$2,'test service')")
        .bind(principal).bind(team_id).execute(store.pool()).await.expect("seed service account");
    sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,credential_kind,credential_generation,recovery_generation,expires_at,identity_id,digest_key_id,rotation_family_id) VALUES ($1,$2,decode(repeat('33',32),'hex'),$3,'service',1,1,clock_timestamp()+interval '1 hour',NULL,'test',$1)")
        .bind(credential).bind(credential.to_string()).bind(principal).execute(store.pool()).await.expect("seed service credential");
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
        .bind(Uuid::now_v7()).bind(team_id).bind(principal).execute(store.pool()).await.expect("seed service membership");
    Store::authenticated_actor(
        principal,
        credential,
        1,
        1,
        ActorKind::Service,
        AuthenticationBinding::Credential,
    )
}

async fn management_state(store: &Store, team_id: Uuid) -> (String, i64, i64, i64, i64, i64, i64) {
    let team: (String, i64, i64) =
        sqlx::query_as("SELECT state,policy_generation,revision FROM teams WHERE team_id=$1")
            .bind(team_id)
            .fetch_one(store.pool())
            .await
            .expect("read management team state");
    let memberships: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memberships WHERE team_id=$1 AND state='active'")
            .bind(team_id)
            .fetch_one(store.pool())
            .await
            .expect("count active memberships");
    let group_members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM group_members WHERE team_id=$1")
            .bind(team_id)
            .fetch_one(store.pool())
            .await
            .expect("count group memberships");
    let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(store.pool())
        .await
        .expect("count audit events");
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM mutation_receipts")
        .fetch_one(store.pool())
        .await
        .expect("count mutation receipts");
    (
        team.0,
        team.1,
        team.2,
        memberships,
        group_members,
        audits,
        receipts,
    )
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "One transaction test keeps its setup, mutation, and assertions together."
)]
async fn group_role_and_grant_are_team_scoped_and_idempotent() {
    let (store, schema, owner, member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let actor = owner_actor(owner, credential);
    let (authority, _witness_directory) = authority(store.clone()).await;
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'member','active',1,1)").bind(Uuid::now_v7()).bind(team_id).bind(member).execute(store.pool()).await.expect("seed member");
    let key = Uuid::now_v7();
    let group = authority
        .create_group(
            actor,
            CreateGroup {
                team_id,
                display_name: "operators".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: key,
            },
        )
        .await
        .expect("create group");
    let replay = authority
        .create_group(
            actor,
            CreateGroup {
                team_id,
                display_name: "operators".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: key,
            },
        )
        .await
        .expect("replay group");
    assert_eq!(group, replay);
    authority
        .change_group_member(
            actor,
            GroupMemberChange {
                team_id,
                group_id: group.group_id,
                principal_id: member,
                add: true,
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("add member");
    let role = authority
        .create_role(
            actor,
            CreateRole {
                team_id,
                display_name: "operators".into(),
                permissions: vec!["team.admin".into()],
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create role");
    authority
        .change_role_assignment(
            actor,
            RoleAssignmentChange {
                team_id,
                role_id: role.role_id,
                principal_id: member,
                assign: true,
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("assign role");
    authority
        .create_grant(
            actor,
            CreateGrant {
                team_id,
                subject: GrantSubject::Group(group.group_id),
                resource_kind: "session",
                resource_id: "session-1".into(),
                permission: "session.terminal.observe".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create narrowed group grant");
    sqlx::query("UPDATE relay_credentials SET credential_generation=credential_generation+1 WHERE credential_id=$1")
        .bind(credential)
        .execute(store.pool())
        .await
        .expect("revoke actor context generation");
    let error = authority
        .update_team(
            actor,
            UpdateTeam {
                team_id,
                expected_revision: 1,
                display_name: Some("stale".into()),
                disable: false,
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect_err("stale authentication context denied");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(crate::store::StoreError::Forbidden)
    ));
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "The replay cases share one PostgreSQL authority and durable-effect snapshot."
)]
async fn exact_management_replays_skip_removed_targets_without_new_effects() {
    let (store, schema, owner, member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let actor = owner_actor(owner, credential);
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'member','active',1,1)")
        .bind(Uuid::now_v7())
        .bind(team_id)
        .bind(member)
        .execute(store.pool())
        .await
        .expect("seed removable member");
    let (authority, _witness_directory) = authority(store.clone()).await;
    let role = authority
        .create_role(
            actor,
            CreateRole {
                team_id,
                display_name: "removable role".into(),
                permissions: vec!["team.group.manage".into()],
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create removable role");
    let grant = authority
        .create_grant(
            actor,
            CreateGrant {
                team_id,
                subject: GrantSubject::Principal(owner),
                resource_kind: "team",
                resource_id: "*".into(),
                permission: "team.membership.read".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create removable grant");
    let member_command = RemoveMember {
        team_id,
        principal_id: member,
        expected_revision: 1,
        correlation_id: Uuid::now_v7(),
        idempotency_key: Uuid::now_v7(),
    };
    let role_command = RemoveRole {
        team_id,
        role_id: role.role_id,
        expected_revision: role.revision,
        correlation_id: Uuid::now_v7(),
        idempotency_key: Uuid::now_v7(),
    };
    let grant_command = RemoveGrant {
        team_id,
        grant_id: grant.grant_id,
        expected_revision: grant.revision,
        correlation_id: Uuid::now_v7(),
        idempotency_key: Uuid::now_v7(),
    };
    authority
        .remove_member(actor, member_command)
        .await
        .expect("remove member");
    authority
        .remove_role(actor, role_command)
        .await
        .expect("remove role");
    authority
        .remove_grant(actor, grant_command)
        .await
        .expect("remove grant");
    let guard = authority
        .admit(AuthorizationRequest {
            actor,
            team_id,
            permission: "session.metadata.read".to_owned(),
            resource: ResourceScope::Team,
            correlation_id: Uuid::now_v7(),
        })
        .await
        .expect("admit owner after original removals");
    let state_before = management_state(&store, team_id).await;
    let witness_before = authority.witness_sequence().expect("read witness sequence");
    let epoch_before = authority.admission_epoch().expect("read admission epoch");

    authority
        .remove_member(actor, member_command)
        .await
        .expect("replay removed member");
    authority
        .remove_role(actor, role_command)
        .await
        .expect("replay removed role");
    authority
        .remove_grant(actor, grant_command)
        .await
        .expect("replay removed grant");

    assert_eq!(management_state(&store, team_id).await, state_before);
    assert_eq!(
        authority.witness_sequence().expect("read witness sequence"),
        witness_before
    );
    assert_eq!(
        authority.admission_epoch().expect("read admission epoch"),
        epoch_before
    );
    guard
        .validate()
        .await
        .expect("replay keeps admitted work live");

    let conflict = authority
        .remove_role(
            actor,
            RemoveRole {
                expected_revision: role.revision + 1,
                ..role_command
            },
        )
        .await
        .expect_err("changed request fingerprint conflicts before the removed target check");
    assert!(matches!(
        conflict,
        crate::admission::AuthorityError::Store(StoreError::IdempotencyConflict)
    ));
    assert!(!authority.is_closed());
    guard
        .validate()
        .await
        .expect("conflict keeps admitted work live");
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
async fn disabled_team_receipt_replay_requires_current_actor_permission() {
    let (store, schema, owner, member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let actor = owner_actor(owner, credential);
    let (authority, _witness_directory) = authority(store.clone()).await;
    let command = DisableTeam {
        team_id,
        expected_revision: 1,
        correlation_id: Uuid::now_v7(),
        idempotency_key: Uuid::now_v7(),
    };
    let disabled = authority
        .disable_team(actor, command)
        .await
        .expect("disable active team");
    let state_before = management_state(&store, team_id).await;
    let witness_before = authority.witness_sequence().expect("read witness sequence");
    let epoch_before = authority.admission_epoch().expect("read admission epoch");
    let replay = authority
        .disable_team(actor, command)
        .await
        .expect("exact disabled-team replay");
    assert_eq!(replay, disabled);
    assert_eq!(management_state(&store, team_id).await, state_before);
    assert_eq!(
        authority.witness_sequence().expect("read witness sequence"),
        witness_before
    );
    assert_eq!(
        authority.admission_epoch().expect("read admission epoch"),
        epoch_before
    );

    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
        .bind(Uuid::now_v7())
        .bind(team_id)
        .bind(member)
        .execute(store.pool())
        .await
        .expect("seed independent owner after disable");
    sqlx::query("UPDATE memberships SET state='removed', revision=revision+1 WHERE team_id=$1 AND principal_id=$2")
        .bind(team_id)
        .bind(owner)
        .execute(store.pool())
        .await
        .expect("remove original actor membership");
    let denied = authority
        .disable_team(actor, command)
        .await
        .expect_err("removed actor cannot replay a disabled-team receipt");
    assert!(matches!(
        denied,
        crate::admission::AuthorityError::Store(StoreError::Forbidden)
    ));
    assert!(!authority.is_closed());
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
async fn absent_management_removals_do_not_create_audit_or_cancellation() {
    let (store, schema, owner, member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let actor = owner_actor(owner, credential);
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'member','active',1,1)")
        .bind(Uuid::now_v7())
        .bind(team_id)
        .bind(member)
        .execute(store.pool())
        .await
        .expect("seed member without assignments");
    let (authority, _witness_directory) = authority(store.clone()).await;
    let group = authority
        .create_group(
            actor,
            CreateGroup {
                team_id,
                display_name: "unassigned group".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create group");
    let role = authority
        .create_role(
            actor,
            CreateRole {
                team_id,
                display_name: "unassigned role".into(),
                permissions: vec!["team.group.manage".into()],
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create role");
    let state_before = management_state(&store, team_id).await;
    let witness_before = authority.witness_sequence().expect("read witness sequence");
    let epoch_before = authority.admission_epoch().expect("read admission epoch");
    for error in [
        authority
            .change_group_member(
                actor,
                GroupMemberChange {
                    team_id,
                    group_id: group.group_id,
                    principal_id: member,
                    add: false,
                    correlation_id: Uuid::now_v7(),
                    idempotency_key: Uuid::now_v7(),
                },
            )
            .await
            .expect_err("absent group assignment rejected"),
        authority
            .change_role_assignment(
                actor,
                RoleAssignmentChange {
                    team_id,
                    role_id: role.role_id,
                    principal_id: member,
                    assign: false,
                    correlation_id: Uuid::now_v7(),
                    idempotency_key: Uuid::now_v7(),
                },
            )
            .await
            .expect_err("absent role assignment rejected"),
        authority
            .remove_member(
                actor,
                RemoveMember {
                    team_id,
                    principal_id: Uuid::now_v7(),
                    expected_revision: 1,
                    correlation_id: Uuid::now_v7(),
                    idempotency_key: Uuid::now_v7(),
                },
            )
            .await
            .expect_err("absent member rejected"),
    ] {
        assert!(matches!(
            error,
            crate::admission::AuthorityError::Store(StoreError::StaleState | StoreError::Forbidden)
        ));
    }
    assert!(!authority.is_closed());
    assert_eq!(management_state(&store, team_id).await, state_before);
    assert_eq!(
        authority.witness_sequence().expect("read witness sequence"),
        witness_before
    );
    assert_eq!(
        authority.admission_epoch().expect("read admission epoch"),
        epoch_before
    );
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
async fn self_member_removal_blocks_replay_while_another_owner_remains_authorized() {
    let (store, schema, owner, member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let actor = owner_actor(owner, credential);
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)")
        .bind(Uuid::now_v7())
        .bind(team_id)
        .bind(member)
        .execute(store.pool())
        .await
        .expect("seed independent owner");
    let (authority, _witness_directory) = authority(store.clone()).await;
    let command = RemoveMember {
        team_id,
        principal_id: owner,
        expected_revision: 1,
        correlation_id: Uuid::now_v7(),
        idempotency_key: Uuid::now_v7(),
    };
    authority
        .remove_member(actor, command)
        .await
        .expect("remove the original actor while an independent owner remains");
    let denied = authority
        .remove_member(actor, command)
        .await
        .expect_err("removed actor cannot replay their own membership removal");
    assert!(matches!(
        denied,
        crate::admission::AuthorityError::Store(StoreError::Forbidden)
    ));
    assert!(!authority.is_closed());
    authority
        .create_group(
            human_actor(&store, member).await,
            CreateGroup {
                team_id,
                display_name: "independent owner remains authorized".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("independent owner retains management permission");
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "The permission boundaries share one PostgreSQL authority fixture."
)]
async fn management_permissions_are_narrow_for_custom_and_service_actors() {
    let (store, schema, owner, member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let owner_actor = owner_actor(owner, credential);
    let (authority, _witness_directory) = authority(store.clone()).await;
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'member','active',1,1)")
        .bind(Uuid::now_v7()).bind(team_id).bind(member).execute(store.pool()).await.expect("seed member");
    let group_role = authority
        .create_role(
            owner_actor,
            CreateRole {
                team_id,
                display_name: "group-manager".into(),
                permissions: vec!["team.group.manage".into()],
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create narrow group role");
    authority
        .change_role_assignment(
            owner_actor,
            RoleAssignmentChange {
                team_id,
                role_id: group_role.role_id,
                principal_id: member,
                assign: true,
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("assign narrow group role");
    let member_actor = human_actor(&store, member).await;
    store
        .list_groups(
            member_actor,
            team_id,
            super::GroupPage {
                after: None,
                limit: 1,
            },
        )
        .await
        .expect("custom group permission lists groups");
    authority
        .create_group(
            member_actor,
            CreateGroup {
                team_id,
                display_name: "custom-group".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("custom group permission creates group");
    let error = authority
        .create_role(
            member_actor,
            CreateRole {
                team_id,
                display_name: "forbidden-role".into(),
                permissions: vec!["team.role.manage".into()],
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect_err("custom group permission cannot manage roles");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(crate::store::StoreError::Forbidden)
    ));

    let service_actor = service_actor(&store, team_id).await;
    let error = authority
        .create_group(
            service_actor,
            CreateGroup {
                team_id,
                display_name: "builtin-service-group".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect_err("service owner membership has no built-in group permission");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(crate::store::StoreError::Forbidden)
    ));
    let service_role = authority
        .create_role(
            owner_actor,
            CreateRole {
                team_id,
                display_name: "service-group-manager".into(),
                permissions: vec!["team.group.manage".into()],
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create service custom role");
    authority
        .change_role_assignment(
            owner_actor,
            RoleAssignmentChange {
                team_id,
                role_id: service_role.role_id,
                principal_id: service_actor.principal_id(),
                assign: true,
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("assign service custom role");
    authority
        .create_group(
            service_actor,
            CreateGroup {
                team_id,
                display_name: "custom-service-group".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("service custom permission creates group");
    let error = authority
        .create_role(
            service_actor,
            CreateRole {
                team_id,
                display_name: "ungiven-service-role".into(),
                permissions: vec!["team.role.manage".into()],
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect_err("service custom group permission cannot manage roles");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(crate::store::StoreError::Forbidden)
    ));
    authority
        .create_grant(
            owner_actor,
            CreateGrant {
                team_id,
                subject: GrantSubject::ServiceAccount(service_actor.principal_id()),
                resource_kind: "team",
                resource_id: "*".into(),
                permission: "team.role.manage".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("grant service role permission");
    authority
        .create_role(
            service_actor,
            CreateRole {
                team_id,
                display_name: "granted-service-role".into(),
                permissions: vec!["team.group.manage".into()],
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("service team grant manages roles");
    cleanup(store.pool(), &schema).await;
}

#[derive(Debug, Clone, Copy)]
enum ManagementBusinessDenial {
    StaleRevision,
    MissingTarget,
    CrossTeamTarget,
    InputTooLarge,
    IdempotencyConflict,
    LastOwner,
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Each denial uses an isolated PostgreSQL schema to prove the full authority boundary."
)]
async fn management_business_denials_preserve_live_access_and_durable_state() {
    for denial in [
        ManagementBusinessDenial::StaleRevision,
        ManagementBusinessDenial::MissingTarget,
        ManagementBusinessDenial::CrossTeamTarget,
        ManagementBusinessDenial::InputTooLarge,
        ManagementBusinessDenial::IdempotencyConflict,
        ManagementBusinessDenial::LastOwner,
    ] {
        let (store, schema, owner, member, credential) = fixture().await;
        let team_id = team(&store, owner).await;
        let second_team = team(&store, owner).await;
        let actor = owner_actor(owner, credential);
        sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'member','active',1,1)")
            .bind(Uuid::now_v7())
            .bind(team_id)
            .bind(member)
            .execute(store.pool())
            .await
            .expect("seed removable member");
        let group_id = Uuid::now_v7();
        sqlx::query("INSERT INTO groups (team_id,group_id,display_name,state,revision) VALUES ($1,$2,'staged','active',1)")
            .bind(team_id)
            .bind(group_id)
            .execute(store.pool())
            .await
            .expect("seed first-team group");
        sqlx::query(
            "INSERT INTO group_members (team_id,group_id,principal_id,revision) VALUES ($1,$2,$3,1)",
        )
            .bind(team_id)
            .bind(group_id)
            .bind(member)
            .execute(store.pool())
            .await
            .expect("seed staged group membership");
        let foreign_group_id = Uuid::now_v7();
        sqlx::query("INSERT INTO groups (team_id,group_id,display_name,state,revision) VALUES ($1,$2,'foreign','active',1)")
            .bind(second_team)
            .bind(foreign_group_id)
            .execute(store.pool())
            .await
            .expect("seed foreign group");
        let (authority, _witness_directory) = authority(store.clone()).await;
        let conflict_key = Uuid::now_v7();
        if matches!(denial, ManagementBusinessDenial::IdempotencyConflict) {
            authority
                .create_group(
                    actor,
                    CreateGroup {
                        team_id,
                        display_name: "original".into(),
                        correlation_id: Uuid::now_v7(),
                        idempotency_key: conflict_key,
                    },
                )
                .await
                .expect("record initial idempotency receipt");
        }
        let guard = authority
            .admit(AuthorizationRequest {
                actor,
                team_id,
                permission: "session.metadata.read".to_owned(),
                resource: ResourceScope::Team,
                correlation_id: Uuid::now_v7(),
            })
            .await
            .expect("admit unrelated live guard");
        let state_before = management_state(&store, team_id).await;
        let witness_before = authority.witness_sequence().expect("read witness sequence");
        let error = match denial {
            ManagementBusinessDenial::StaleRevision => {
                authority
                    .remove_member(
                        actor,
                        RemoveMember {
                            team_id,
                            principal_id: member,
                            expected_revision: 0,
                            correlation_id: Uuid::now_v7(),
                            idempotency_key: Uuid::now_v7(),
                        },
                    )
                    .await
            }
            ManagementBusinessDenial::MissingTarget => authority
                .update_group(
                    actor,
                    UpdateGroup {
                        team_id,
                        group_id: Uuid::now_v7(),
                        expected_revision: 1,
                        display_name: "missing".into(),
                        correlation_id: Uuid::now_v7(),
                        idempotency_key: Uuid::now_v7(),
                    },
                )
                .await
                .map(|_record| ()),
            ManagementBusinessDenial::CrossTeamTarget => authority
                .update_group(
                    actor,
                    UpdateGroup {
                        team_id,
                        group_id: foreign_group_id,
                        expected_revision: 1,
                        display_name: "foreign".into(),
                        correlation_id: Uuid::now_v7(),
                        idempotency_key: Uuid::now_v7(),
                    },
                )
                .await
                .map(|_record| ()),
            ManagementBusinessDenial::InputTooLarge => authority
                .create_group(
                    actor,
                    CreateGroup {
                        team_id,
                        display_name: "x".repeat(257),
                        correlation_id: Uuid::now_v7(),
                        idempotency_key: Uuid::now_v7(),
                    },
                )
                .await
                .map(|_record| ()),
            ManagementBusinessDenial::IdempotencyConflict => authority
                .create_group(
                    actor,
                    CreateGroup {
                        team_id,
                        display_name: "conflicting".into(),
                        correlation_id: Uuid::now_v7(),
                        idempotency_key: conflict_key,
                    },
                )
                .await
                .map(|_record| ()),
            ManagementBusinessDenial::LastOwner => {
                authority
                    .remove_member(
                        actor,
                        RemoveMember {
                            team_id,
                            principal_id: owner,
                            expected_revision: 1,
                            correlation_id: Uuid::now_v7(),
                            idempotency_key: Uuid::now_v7(),
                        },
                    )
                    .await
            }
        }
        .expect_err("predictable management denial");
        match denial {
            ManagementBusinessDenial::StaleRevision => {
                assert!(matches!(
                    error,
                    crate::admission::AuthorityError::Store(StoreError::StaleState)
                ));
            }
            ManagementBusinessDenial::InputTooLarge => {
                assert!(matches!(
                    error,
                    crate::admission::AuthorityError::Store(StoreError::InputTooLarge)
                ));
            }
            ManagementBusinessDenial::IdempotencyConflict => {
                assert!(matches!(
                    error,
                    crate::admission::AuthorityError::Store(StoreError::IdempotencyConflict)
                ));
            }
            ManagementBusinessDenial::MissingTarget
            | ManagementBusinessDenial::CrossTeamTarget
            | ManagementBusinessDenial::LastOwner => {
                assert!(matches!(
                    error,
                    crate::admission::AuthorityError::Store(StoreError::Forbidden)
                ));
            }
        }
        assert!(
            !authority.is_closed(),
            "{denial:?} must keep authority live"
        );
        assert_eq!(management_state(&store, team_id).await, state_before);
        assert_eq!(
            authority.witness_sequence().expect("read witness sequence"),
            witness_before
        );
        guard
            .validate()
            .await
            .expect("unrelated guard must survive denial");
        cleanup(store.pool(), &schema).await;
    }
}

#[tokio::test]
async fn management_audit_failure_remains_fail_stop_and_rolls_back() {
    let (store, schema, owner, _member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let actor = owner_actor(owner, credential);
    let (authority, _witness_directory) = authority(store.clone()).await;
    let guard = authority
        .admit(AuthorizationRequest {
            actor,
            team_id,
            permission: "session.metadata.read".to_owned(),
            resource: ResourceScope::Team,
            correlation_id: Uuid::now_v7(),
        })
        .await
        .expect("admit live guard");
    let state_before = management_state(&store, team_id).await;
    let witness_before = authority.witness_sequence().expect("read witness sequence");
    sqlx::raw_sql(AssertSqlSafe("CREATE FUNCTION reject_group_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit unavailable'; END; $$; CREATE TRIGGER reject_group_audit BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.action = 'group.create') EXECUTE FUNCTION reject_group_audit();".to_owned()))
        .execute(store.pool())
        .await
        .expect("install management audit failure trigger");
    let error = authority
        .create_group(
            actor,
            CreateGroup {
                team_id,
                display_name: "cannot-commit".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect_err("audit failure must not acknowledge management mutation");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(StoreError::AuditUnavailable)
    ));
    assert!(authority.is_closed());
    assert!(guard.cancelled());
    assert!(matches!(
        authority.release_clean().await,
        Err(crate::admission::AuthorityError::Cancelled)
    ));
    assert_eq!(management_state(&store, team_id).await, state_before);
    assert_eq!(
        authority.witness_sequence().expect("read witness sequence"),
        witness_before
    );
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
async fn foreign_group_and_last_owner_removal_are_rejected() {
    let (store, schema, owner, member, credential) = fixture().await;
    let first = team(&store, owner).await;
    let second = team(&store, owner).await;
    let actor = owner_actor(owner, credential);
    let (authority, _witness_directory) = authority(store.clone()).await;
    let group = authority
        .create_group(
            actor,
            CreateGroup {
                team_id: first,
                display_name: "first".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect("create first group");
    let error = authority
        .create_grant(
            actor,
            CreateGrant {
                team_id: second,
                subject: GrantSubject::Group(group.group_id),
                resource_kind: "team",
                resource_id: "*".into(),
                permission: "team.membership.read".into(),
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect_err("foreign group denied");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(crate::store::StoreError::Forbidden)
    ));
    let error =
        sqlx::query("UPDATE memberships SET state='removed' WHERE team_id=$1 AND principal_id=$2")
            .bind(first)
            .bind(owner)
            .execute(store.pool())
            .await
            .expect_err("last owner invariant");
    assert!(error
        .to_string()
        .contains("team must retain an active owner"));
    let _ = member;
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
async fn concurrent_owner_removals_preserve_one_owner() {
    let (store, schema, owner, member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let actor = owner_actor(owner, credential);
    let (authority, _witness_directory) = authority(store.clone()).await;
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$2,$3,'owner','active',1,1)").bind(Uuid::now_v7()).bind(team_id).bind(member).execute(store.pool()).await.expect("seed second owner");
    let a = authority.change_member(
        actor,
        super::MembershipChange {
            team_id,
            principal_id: owner,
            expected_revision: 1,
            role: None,
            correlation_id: Uuid::now_v7(),
            idempotency_key: Uuid::now_v7(),
        },
    );
    let b = authority.change_member(
        actor,
        super::MembershipChange {
            team_id,
            principal_id: member,
            expected_revision: 1,
            role: None,
            correlation_id: Uuid::now_v7(),
            idempotency_key: Uuid::now_v7(),
        },
    );
    let (left, right) = tokio::join!(a, b);
    assert!(
        left.is_ok() ^ right.is_ok(),
        "one concurrent owner removal must fail: {left:?} {right:?}"
    );
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
async fn team_revocation_rejects_a_mismatched_published_scope() {
    let (store, schema, owner, _member, credential) = fixture().await;
    let team_id = team(&store, owner).await;
    let (authority, _witness_directory) = authority(store.clone()).await;
    let error = authority
        .revoke(RevocationCommand {
            actor: owner_actor(owner, credential),
            team_id: Some(team_id),
            scope_kind: "team",
            scope_id: Uuid::now_v7().to_string(),
            reason_code: "operator_revoke",
            correlation_id: Uuid::now_v7(),
        })
        .await
        .expect_err("mismatched target and published scope denied");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(crate::store::StoreError::Forbidden)
    ));
    let state: String = sqlx::query_scalar("SELECT state FROM teams WHERE team_id=$1")
        .bind(team_id)
        .fetch_one(store.pool())
        .await
        .expect("read untouched team");
    assert_eq!(state, "active");
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
async fn stale_bootstrap_actor_cannot_create_a_team() {
    let (store, schema, _owner, _member, _credential) = fixture().await;
    let principal = Uuid::now_v7();
    let credential = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'infrastructure','active',1)",
    )
    .bind(principal)
    .execute(store.pool())
    .await
    .expect("seed infrastructure principal");
    seed_identity(&store, principal).await;
    sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,credential_kind,credential_generation,recovery_generation,expires_at,identity_id,digest_key_id,rotation_family_id) VALUES ($1,$2,decode(repeat('11',32),'hex'),$3,'human',1,1,clock_timestamp()+interval '1 hour',$3,'test',$1)")
        .bind(credential).bind(credential.to_string()).bind(principal).execute(store.pool()).await.expect("seed infrastructure credential");
    let actor = Store::authenticated_actor(
        principal,
        credential,
        2,
        1,
        ActorKind::Infrastructure,
        AuthenticationBinding::Credential,
    );
    let (authority, _witness_directory) = authority(store.clone()).await;
    let error = authority
        .create_team(
            actor,
            CreateTeam {
                display_name: "denied".into(),
                initial_owner: principal,
                correlation_id: Uuid::now_v7(),
                idempotency_key: Uuid::now_v7(),
            },
        )
        .await
        .expect_err("stale actor denied");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(crate::store::StoreError::Forbidden)
    ));
    cleanup(store.pool(), &schema).await;
}

#[tokio::test]
async fn principal_revocation_cannot_remove_the_last_effective_owner() {
    let (store, schema, owner, _member, _credential) = fixture().await;
    let _team_id = team(&store, owner).await;
    let infrastructure = Uuid::now_v7();
    let credential = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'infrastructure','active',1)",
    )
    .bind(infrastructure)
    .execute(store.pool())
    .await
    .expect("seed infrastructure principal");
    seed_identity(&store, infrastructure).await;
    sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,credential_kind,credential_generation,recovery_generation,expires_at,identity_id,digest_key_id,rotation_family_id) VALUES ($1,$2,decode(repeat('22',32),'hex'),$3,'human',1,1,clock_timestamp()+interval '1 hour',$3,'test',$1)")
        .bind(credential).bind(credential.to_string()).bind(infrastructure).execute(store.pool()).await.expect("seed infrastructure credential");
    let actor = Store::authenticated_actor(
        infrastructure,
        credential,
        1,
        1,
        ActorKind::Infrastructure,
        AuthenticationBinding::Credential,
    );
    let (authority, _witness_directory) = authority(store.clone()).await;
    let error = authority
        .revoke(RevocationCommand {
            actor,
            team_id: None,
            scope_kind: "principal",
            scope_id: owner.to_string(),
            reason_code: "operator_revoke",
            correlation_id: Uuid::now_v7(),
        })
        .await
        .expect_err("last effective owner retained");
    assert!(matches!(
        error,
        crate::admission::AuthorityError::Store(crate::store::StoreError::Forbidden)
    ));
    let state: String = sqlx::query_scalar("SELECT state FROM principals WHERE id=$1")
        .bind(owner)
        .fetch_one(store.pool())
        .await
        .expect("read owner state");
    assert_eq!(state, "active");
    cleanup(store.pool(), &schema).await;
}
