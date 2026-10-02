//! A durable authentication failure keeps its cause as an error source while
//! its client-facing text stays fixed.

// Rust guideline compliant 2026-10-02

use std::error::Error as _;

use uuid::Uuid;

use super::{
    tests::{authority, cleanup, fixture, limits},
    AuthError, AuthService, DigestKey,
};
use crate::config::LoginPolicy;

/// The fixed client-facing text of a durable failure.
const CLIENT_TEXT: &str = "durable authentication state is unavailable";

async fn service() -> (
    AuthService,
    std::sync::Arc<crate::admission::Authority>,
    String,
    sqlx::PgPool,
    tempfile::TempDir,
    crate::store::Store,
) {
    let (store, schema, bootstrap) = fixture().await;
    let (authority, directory) = authority(store.clone()).await;
    let service = AuthService::new(
        store.clone(),
        DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
        2,
        limits(),
        LoginPolicy::AnyAuthenticatedSubject,
        std::sync::Arc::clone(&authority),
    );
    (service, authority, schema, bootstrap, directory, store)
}

async fn issue_session(service: &AuthService) -> Result<(), AuthError> {
    service
        .issue_browser_session(
            "issuer".into(),
            "subject".into(),
            "client".into(),
            "https://relay.example/callback".into(),
            Uuid::now_v7(),
            1,
        )
        .await
        .map(|_session| ())
}

#[tokio::test]
async fn a_lapsed_authority_fence_is_the_recoverable_cause() {
    let (service, authority, schema, bootstrap, _directory, _store) = service().await;
    authority.begin_stop();

    let error = issue_session(&service)
        .await
        .expect_err("a closed fence denies the mutation");

    assert!(matches!(error, AuthError::Durable(Some(_))), "{error:?}");
    assert_eq!(error.to_string(), CLIENT_TEXT);
    let cause = error.source().expect("the durable failure keeps its cause");
    assert_eq!(cause.to_string(), "relay access was cancelled");
    assert!(cause.source().is_none());
    cleanup(&bootstrap, &schema).await;
}

#[tokio::test]
async fn a_database_failure_keeps_its_class_but_not_its_message() {
    let (service, _authority, schema, bootstrap, _directory, store) = service().await;
    sqlx::raw_sql("DROP TABLE browser_logins CASCADE")
        .execute(store.pool())
        .await
        .expect("drop the table the session issue reads");

    let error = issue_session(&service)
        .await
        .expect_err("a missing table is a durable failure");

    assert_eq!(error.to_string(), CLIENT_TEXT);
    let cause = error.source().expect("the durable failure keeps its cause");
    assert_eq!(
        cause.to_string(),
        "database error (sqlstate 42P01, constraint none)"
    );
    assert!(!cause.to_string().contains("browser_logins"));
    cleanup(&bootstrap, &schema).await;
}

#[tokio::test]
async fn a_closed_authority_denies_bearer_authentication_with_a_reason() {
    let (service, authority, schema, bootstrap, _directory, _store) = service().await;
    authority.begin_stop();

    let error = service
        .authenticate_bearer(super::RelayBearerCredential::new(format!(
            "{}.secret",
            Uuid::now_v7()
        )))
        .await
        .expect_err("a closed authority denies authentication");

    assert_eq!(error.to_string(), CLIENT_TEXT);
    let cause = error
        .source()
        .expect("a causeless failure still records why");
    assert_eq!(cause.to_string(), "the relay authority is closed");
    cleanup(&bootstrap, &schema).await;
}
