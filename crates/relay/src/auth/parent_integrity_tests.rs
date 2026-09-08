//! Exercises PostgreSQL parent constraints for relay authentication rows.

// Rust guideline compliant 2026-09-08

use std::sync::Arc;

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
    // A normal writer cannot delete this row. Simulate a restored corrupt database
    // explicitly so authentication also rejects a dangling service credential.
    sqlx::query(
        "ALTER TABLE service_accounts DISABLE TRIGGER service_accounts_prevent_credential_orphan",
    )
    .execute(store.pool())
    .await
    .expect("allow explicit restored-corruption fixture");
    sqlx::query("DELETE FROM service_accounts WHERE principal_id = $1")
        .bind(service_principal_id)
        .execute(store.pool())
        .await
        .expect("remove parent only for restored-corruption fixture");
    sqlx::query(
        "ALTER TABLE service_accounts ENABLE TRIGGER service_accounts_prevent_credential_orphan",
    )
    .execute(store.pool())
    .await
    .expect("restore service-account orphan protection");
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
