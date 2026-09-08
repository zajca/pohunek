//! Exercises PostgreSQL parent constraints for relay authentication rows.

// Rust guideline compliant 2026-09-08

use std::{sync::Arc, time::Duration};

use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    service::tests::{authority, cleanup, fixture, limits},
    AuthError, AuthService, DigestKey, RelayBearerCredential,
};
use crate::{
    authorization::verify_current_actor,
    config::LoginPolicy,
    store::{ActorKind, Store, StoreError},
};

async fn insert_principal(store: &Store, principal_id: Uuid, kind: &str) {
    sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,$2,'active',1)")
        .bind(principal_id)
        .bind(kind)
        .execute(store.pool())
        .await
        .expect("seed principal");
}

async fn insert_identity(store: &Store, identity_id: Uuid, principal_id: Uuid, subject: &str) {
    sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer',$2,$3,1)")
        .bind(identity_id)
        .bind(subject)
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed OIDC identity");
}

async fn insert_team(store: &Store, team_id: Uuid, name: &str) {
    sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,$2,'active',1,1)")
        .bind(team_id)
        .bind(name)
        .execute(store.pool())
        .await
        .expect("seed team");
}

async fn insert_credential(
    store: &Store,
    digest_key: &DigestKey,
    credential_id: Uuid,
    principal_id: Uuid,
    identity_id: Option<Uuid>,
    kind: &str,
    secret: &str,
) {
    sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,$5,'test',$6,1,$1,1,clock_timestamp()+interval '1 hour')")
        .bind(credential_id)
        .bind(credential_id.to_string())
        .bind(digest_key.digest(secret).as_slice())
        .bind(principal_id)
        .bind(identity_id)
        .bind(kind)
        .execute(store.pool())
        .await
        .expect("seed credential");
}

async fn insert_service_credential_for_race(
    transaction: &mut Transaction<'_, Postgres>,
    credential_id: Uuid,
    principal_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$2,$3,$4,'test','service',1,$1,1,clock_timestamp()+interval '1 hour')")
        .bind(credential_id)
        .bind(credential_id.to_string())
        .bind(vec![1_u8; 32])
        .bind(principal_id)
        .execute(&mut **transaction)
        .await
        .map(|_| ())
}

async fn seed_service_account_for_race(store: &Store) -> Uuid {
    let principal_id = Uuid::now_v7();
    let team_id = Uuid::now_v7();
    insert_principal(store, principal_id, "service").await;
    insert_team(store, team_id, "service-account-race-team").await;
    sqlx::query("INSERT INTO service_accounts (principal_id,team_id,display_name) VALUES ($1,$2,'service-account-race')")
        .bind(principal_id)
        .bind(team_id)
        .execute(store.pool())
        .await
        .expect("seed service account for race");
    principal_id
}

async fn assert_no_orphaned_service_credentials(store: &Store) {
    let orphaned_credentials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM relay_credentials c LEFT JOIN service_accounts s ON s.principal_id = c.principal_id WHERE c.credential_kind = 'service' AND s.principal_id IS NULL",
    )
    .fetch_one(store.pool())
    .await
    .expect("count orphaned service credentials");
    assert_eq!(
        orphaned_credentials, 0,
        "service credentials retain parents"
    );
}

#[tokio::test]
async fn service_credential_foreign_key_rejects_delete_first_race() {
    // This duration only detects that PostgreSQL is waiting on the conflicting
    // foreign-key operation; it is not a production timing guarantee.
    const BLOCK_DETECTION_TIMEOUT: Duration = Duration::from_millis(100);

    let (store, schema, bootstrap) = fixture().await;
    let principal_id = seed_service_account_for_race(&store).await;
    let mut delete_transaction = store.pool().begin().await.expect("begin parent delete");
    sqlx::query("DELETE FROM service_accounts WHERE principal_id = $1")
        .bind(principal_id)
        .execute(&mut *delete_transaction)
        .await
        .expect("delete parent before conflicting credential insert");

    let insert_pool = store.pool().clone();
    let mut insert_credential = tokio::spawn(async move {
        let mut transaction = insert_pool.begin().await.expect("begin credential insert");
        insert_service_credential_for_race(&mut transaction, Uuid::now_v7(), principal_id).await?;
        transaction.commit().await
    });
    assert!(
        tokio::time::timeout(BLOCK_DETECTION_TIMEOUT, &mut insert_credential)
            .await
            .is_err(),
        "credential insert waits for the uncommitted parent deletion"
    );

    delete_transaction
        .commit()
        .await
        .expect("commit parent deletion");
    assert!(
        insert_credential
            .await
            .expect("join blocked credential insert")
            .is_err(),
        "credential insert fails after its parent deletion commits"
    );
    assert_no_orphaned_service_credentials(&store).await;
    cleanup(&bootstrap, &schema).await;
}

#[tokio::test]
async fn service_credential_foreign_key_rejects_insert_first_race() {
    // This duration only detects that PostgreSQL is waiting on the conflicting
    // foreign-key operation; it is not a production timing guarantee.
    const BLOCK_DETECTION_TIMEOUT: Duration = Duration::from_millis(100);

    let (store, schema, bootstrap) = fixture().await;
    let principal_id = seed_service_account_for_race(&store).await;
    let mut insert_transaction = store.pool().begin().await.expect("begin credential insert");
    insert_service_credential_for_race(&mut insert_transaction, Uuid::now_v7(), principal_id)
        .await
        .expect("insert credential before conflicting parent deletion");

    let delete_pool = store.pool().clone();
    let mut delete_parent = tokio::spawn(async move {
        sqlx::query("DELETE FROM service_accounts WHERE principal_id = $1")
            .bind(principal_id)
            .execute(&delete_pool)
            .await
    });
    assert!(
        tokio::time::timeout(BLOCK_DETECTION_TIMEOUT, &mut delete_parent)
            .await
            .is_err(),
        "parent deletion waits for the uncommitted credential insert"
    );

    insert_transaction
        .commit()
        .await
        .expect("commit credential insert");
    assert!(
        delete_parent
            .await
            .expect("join blocked parent deletion")
            .is_err(),
        "parent deletion fails after its credential insert commits"
    );
    assert_no_orphaned_service_credentials(&store).await;
    cleanup(&bootstrap, &schema).await;
}

#[tokio::test]
async fn postgres_auth_parents_reject_invalid_identity_and_link_coordinates() {
    let (store, schema, bootstrap) = fixture().await;
    let human_principal_id = Uuid::now_v7();
    let other_human_principal_id = Uuid::now_v7();
    let infrastructure_principal_id = Uuid::now_v7();
    let service_principal_id = Uuid::now_v7();
    insert_principal(&store, human_principal_id, "human").await;
    insert_principal(&store, other_human_principal_id, "human").await;
    insert_principal(&store, infrastructure_principal_id, "infrastructure").await;
    insert_principal(&store, service_principal_id, "service").await;
    let human_identity_id = Uuid::now_v7();
    let other_human_identity_id = Uuid::now_v7();
    let infrastructure_identity_id = Uuid::now_v7();
    insert_identity(
        &store,
        human_identity_id,
        human_principal_id,
        "human-parent",
    )
    .await;
    insert_identity(
        &store,
        other_human_identity_id,
        other_human_principal_id,
        "other-human-parent",
    )
    .await;
    insert_identity(
        &store,
        infrastructure_identity_id,
        infrastructure_principal_id,
        "infrastructure-parent",
    )
    .await;
    sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'issuer','service-parent',$2,1)")
        .bind(Uuid::now_v7())
        .bind(service_principal_id)
        .execute(store.pool())
        .await
        .expect_err("service principals cannot own OIDC identities");
    sqlx::query("UPDATE principals SET kind = 'service' WHERE id = $1")
        .bind(human_principal_id)
        .execute(store.pool())
        .await
        .expect_err("principal kind is immutable after creation");
    sqlx::query("INSERT INTO account_link_transactions (link_id,principal_id,source_identity_id,account_link_generation,recovery_generation,expires_at) VALUES ($1,$2,$3,1,1,clock_timestamp()+interval '1 hour')")
        .bind(Uuid::now_v7())
        .bind(human_principal_id)
        .bind(other_human_identity_id)
        .execute(store.pool())
        .await
        .expect_err("link source identity must belong to its principal");
    sqlx::query("INSERT INTO account_link_transactions (link_id,principal_id,source_identity_id,account_link_generation,recovery_generation,expires_at) VALUES ($1,$2,$3,1,1,clock_timestamp()+interval '1 hour')")
        .bind(Uuid::now_v7())
        .bind(human_principal_id)
        .bind(human_identity_id)
        .execute(store.pool())
        .await
        .expect("matching account-link source identity is valid");
    let identity_principals: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM oidc_identities WHERE principal_id IN ($1, $2, $3)",
    )
    .bind(human_principal_id)
    .bind(other_human_principal_id)
    .bind(infrastructure_principal_id)
    .fetch_one(store.pool())
    .await
    .expect("count valid OIDC parent identities");
    assert_eq!(identity_principals, 3);
    cleanup(&bootstrap, &schema).await;
}

#[tokio::test]
async fn service_account_parent_integrity_denies_deprovisioned_or_missing_credentials() {
    let (store, schema, bootstrap) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let digest_key = DigestKey::new("test".into(), b"parent-integrity-key".to_vec());
    let service = AuthService::new(
        store.clone(),
        digest_key.clone(),
        2,
        limits(),
        LoginPolicy::AnyAuthenticatedSubject,
        Arc::clone(&authority),
    );
    let human_principal_id = Uuid::now_v7();
    let infrastructure_principal_id = Uuid::now_v7();
    let service_principal_id = Uuid::now_v7();
    let replacement_service_principal_id = Uuid::now_v7();
    let team_id = Uuid::now_v7();
    let replacement_team_id = Uuid::now_v7();
    insert_principal(&store, human_principal_id, "human").await;
    insert_principal(&store, infrastructure_principal_id, "infrastructure").await;
    insert_principal(&store, service_principal_id, "service").await;
    insert_principal(&store, replacement_service_principal_id, "service").await;
    insert_team(&store, team_id, "parent-integrity-team").await;
    insert_team(
        &store,
        replacement_team_id,
        "replacement-parent-integrity-team",
    )
    .await;
    let human_identity_id = Uuid::now_v7();
    let infrastructure_identity_id = Uuid::now_v7();
    insert_identity(&store, human_identity_id, human_principal_id, "valid-human").await;
    insert_identity(
        &store,
        infrastructure_identity_id,
        infrastructure_principal_id,
        "valid-infrastructure",
    )
    .await;
    sqlx::query("INSERT INTO service_accounts (principal_id,team_id,display_name) VALUES ($1,$2,'parent-integrity-service')")
        .bind(service_principal_id)
        .bind(team_id)
        .execute(store.pool())
        .await
        .expect("seed service account");
    let human_credential_id = Uuid::now_v7();
    let infrastructure_credential_id = Uuid::now_v7();
    let service_credential_id = Uuid::now_v7();
    insert_credential(
        &store,
        &digest_key,
        human_credential_id,
        human_principal_id,
        Some(human_identity_id),
        "human",
        "valid-human-credential",
    )
    .await;
    insert_credential(
        &store,
        &digest_key,
        infrastructure_credential_id,
        infrastructure_principal_id,
        Some(infrastructure_identity_id),
        "human",
        "valid-infrastructure-credential",
    )
    .await;
    insert_credential(
        &store,
        &digest_key,
        service_credential_id,
        service_principal_id,
        None,
        "service",
        "valid-service-credential",
    )
    .await;
    let human_actor = service
        .authenticate_bearer(RelayBearerCredential::new(format!(
            "{human_credential_id}.valid-human-credential"
        )))
        .await
        .expect("human credential remains valid");
    assert_eq!(human_actor.actor().kind(), ActorKind::Human);
    let infrastructure_actor = service
        .authenticate_bearer(RelayBearerCredential::new(format!(
            "{infrastructure_credential_id}.valid-infrastructure-credential"
        )))
        .await
        .expect("infrastructure credential remains valid");
    assert_eq!(
        infrastructure_actor.actor().kind(),
        ActorKind::Infrastructure
    );
    let service_actor = service
        .authenticate_bearer(RelayBearerCredential::new(format!(
            "{service_credential_id}.valid-service-credential"
        )))
        .await
        .expect("active service account credential is valid");
    assert_eq!(service_actor.actor().kind(), ActorKind::Service);
    sqlx::query("UPDATE service_accounts SET display_name = 'renamed-parent-integrity-service' WHERE principal_id = $1")
        .bind(service_principal_id)
        .execute(store.pool())
        .await
        .expect("service account metadata remains mutable");
    sqlx::query("UPDATE service_accounts SET principal_id = $1 WHERE principal_id = $2")
        .bind(replacement_service_principal_id)
        .bind(service_principal_id)
        .execute(store.pool())
        .await
        .expect_err("service account principal is immutable");
    sqlx::query("UPDATE service_accounts SET team_id = $1 WHERE principal_id = $2")
        .bind(replacement_team_id)
        .bind(service_principal_id)
        .execute(store.pool())
        .await
        .expect_err("service account team is immutable");
    sqlx::query("DELETE FROM service_accounts WHERE principal_id = $1")
        .bind(service_principal_id)
        .execute(store.pool())
        .await
        .expect_err("deleting a service account cannot strand its credentials");
    sqlx::query(
        "UPDATE service_accounts SET deprovisioned_at = clock_timestamp() WHERE principal_id = $1",
    )
    .bind(service_principal_id)
    .execute(store.pool())
    .await
    .expect("deprovision service account");
    assert!(matches!(
        service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{service_credential_id}.valid-service-credential"
            )))
            .await,
        Err(AuthError::CredentialInvalid)
    ));
    let mut transaction = store
        .begin_serializable()
        .await
        .expect("begin currentness transaction");
    let currentness = verify_current_actor(&mut transaction, service_actor.actor()).await;
    assert!(
        matches!(currentness, Err(StoreError::Forbidden)),
        "deprovisioned service actor currentness result: {currentness:?}"
    );
    transaction
        .rollback()
        .await
        .expect("release currentness transaction");
    assert!(
        !authority.is_closed(),
        "a deprovisioned service actor cannot gain implicit authority"
    );
    sqlx::query("UPDATE service_accounts SET deprovisioned_at = NULL WHERE principal_id = $1")
        .bind(service_principal_id)
        .execute(store.pool())
        .await
        .expect("restore active service account for corruption simulation");
    // A normal writer cannot delete this row. A privileged recovery simulation
    // suppresses constraints only inside this transaction, then authentication
    // must reject the deliberately restored dangling credential.
    let mut corruption_transaction = store
        .pool()
        .begin()
        .await
        .expect("begin restored-corruption fixture");
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *corruption_transaction)
        .await
        .expect("suppress constraints only for restored-corruption fixture");
    sqlx::query("DELETE FROM service_accounts WHERE principal_id = $1")
        .bind(service_principal_id)
        .execute(&mut *corruption_transaction)
        .await
        .expect("remove parent only for restored-corruption fixture");
    corruption_transaction
        .commit()
        .await
        .expect("commit restored-corruption fixture");
    assert!(matches!(
        service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{service_credential_id}.valid-service-credential"
            )))
            .await,
        Err(AuthError::CredentialInvalid)
    ));
    cleanup(&bootstrap, &schema).await;
}
