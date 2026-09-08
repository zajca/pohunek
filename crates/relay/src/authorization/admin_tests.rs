//! `PostgreSQL` integration coverage for team administration invariants.

use std::{env, os::unix::fs::PermissionsExt as _, sync::Arc};

use ed25519_dalek::SigningKey;
use sqlx::{AssertSqlSafe, PgPool};
use tempfile::TempDir;
use uuid::Uuid;

use super::{
    CreateGrant, CreateGroup, CreateRole, GrantSubject, GroupMemberChange, RoleAssignmentChange,
    UpdateTeam,
};
use crate::store::{ActorKind, AuthenticationBinding, Store};
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
