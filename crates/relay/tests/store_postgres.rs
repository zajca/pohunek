//! Runs real PostgreSQL authority invariants when explicitly selected.

#![cfg(feature = "postgres-tests")]

use std::env;

use pohunek_relay::store::Store;
use sqlx::{postgres::PgPoolOptions, AssertSqlSafe};
use uuid::Uuid;

#[tokio::test]
async fn explicit_postgres_fixture_is_required() {
    let url = env::var("POHUNEK_RELAY_TEST_DATABASE_URL")
        .expect("postgres-tests requires POHUNEK_RELAY_TEST_DATABASE_URL");
    let bootstrap = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect dedicated PostgreSQL fixture");
    let schema = format!("relay_store_{}", Uuid::now_v7().simple());
    sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&bootstrap)
        .await
        .expect("create isolated PostgreSQL schema");
    let store = Store::connect(&format!("{url}?options[search_path]={schema}"), 2)
        .await
        .expect("connect isolated PostgreSQL fixture");
    store
        .migrate()
        .await
        .expect("apply embedded relay migrations");
    store.healthcheck().await.expect("query PostgreSQL fixture");
    sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&bootstrap)
        .await
        .expect("remove isolated PostgreSQL schema");
}
