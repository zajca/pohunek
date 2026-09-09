//! Restart and tamper coverage for independently witnessed local operations.

use super::*;
use crate::lifecycle::local::{canonical_bootstrap_request, locked_tables};
use crate::recovery::WitnessEvent;

fn reopened(store: &Store, directory: &std::path::Path) -> Lifecycle {
    let witness = WitnessStore::open(directory, SigningKey::from_bytes(&[7; 32]), "test".into())
        .expect("reopen independent witness after process loss");
    Lifecycle::new(store.clone(), Arc::new(witness))
}

async fn migration_coordinates(store: &Store) -> (crate::store::MigrationPrefix, [u8; 32]) {
    let mut tx = store
        .begin_serializable()
        .await
        .expect("begin migration coordinate snapshot");
    locked_tables(&mut tx)
        .await
        .expect("lock application tables");
    let prefix = store
        .migration_prefix_in_transaction(&mut tx)
        .await
        .expect("embedded migration prefix");
    let authority = store
        .migration_authority_digest_in_transaction(&mut tx)
        .await
        .expect("SQL-side authority digest");
    tx.commit()
        .await
        .expect("commit migration coordinate snapshot");
    (prefix, authority)
}

async fn migration_coordinates_with_session_defaults(
    store: &Store,
    time_zone: &str,
    bytea_output: &str,
) -> (crate::store::MigrationPrefix, [u8; 32]) {
    let mut tx = store
        .begin_serializable()
        .await
        .expect("begin digest snapshot");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL TimeZone='{time_zone}'"
    )))
    .execute(&mut *tx)
    .await
    .expect("set test timezone");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL bytea_output='{bytea_output}'"
    )))
    .execute(&mut *tx)
    .await
    .expect("set test bytea output");
    locked_tables(&mut tx)
        .await
        .expect("lock application tables");
    let prefix = store
        .migration_prefix_in_transaction(&mut tx)
        .await
        .expect("prefix");
    let digest = store
        .migration_authority_digest_in_transaction(&mut tx)
        .await
        .expect("digest");
    tx.commit().await.expect("commit digest snapshot");
    (prefix, digest)
}

fn migration_event(
    base: crate::store::MigrationPrefix,
    authority_digest: [u8; 32],
) -> WitnessEvent {
    let target = Store::migration_target_prefix().expect("embedded migration target");
    WitnessEvent::Migration {
        base_migration_count: base.count(),
        base_plan_digest: base.digest(),
        target_migration_count: target.count(),
        target_plan_digest: target.digest(),
        authority_digest,
    }
}

async fn foundation_fixture() -> (
    Store,
    String,
    sqlx::PgPool,
    Arc<WitnessStore>,
    tempfile::TempDir,
) {
    let url = std::env::var("POHUNEK_RELAY_TEST_DATABASE_URL")
        .expect("postgres-tests requires POHUNEK_RELAY_TEST_DATABASE_URL");
    let bootstrap = Store::connect(&url, 1).await.expect("connect PostgreSQL");
    let schema = format!("relay_migration_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(bootstrap.pool())
        .await
        .expect("create schema");
    let store = Store::connect(&format!("{url}?options[search_path]={schema}"), 4)
        .await
        .expect("connect scoped schema");
    store
        .migrate_to_for_test(1)
        .await
        .expect("apply foundation prefix");
    let directory = tempfile::tempdir().expect("witness directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
        .expect("private witness directory");
    let witness = Arc::new(
        WitnessStore::open(
            directory.path(),
            SigningKey::from_bytes(&[7; 32]),
            "test".into(),
        )
        .expect("open witness"),
    );
    sqlx::query("INSERT INTO relay_identity (relay_id,recovery_generation,state,revision) VALUES ('relay-test',1,'normal',1)")
        .execute(store.pool())
        .await
        .expect("seed foundation relay identity");
    let active = witness
        .begin_run(None, "relay-test", 1)
        .expect("begin foundation witness");
    witness.end_run(&active).expect("clean foundation witness");
    (store, schema, bootstrap.pool().clone(), witness, directory)
}

#[tokio::test]
async fn bootstrap_rejects_unwitnessed_authority_and_modified_permission_catalog() {
    let (store, schema, pool, witness, directory) = fixture().await;
    let principal = Uuid::now_v7();
    sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)")
        .bind(principal)
        .execute(store.pool())
        .await
        .expect("restored principal");
    sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'https://issuer.test','old',$1,1)")
        .bind(principal).execute(store.pool()).await.expect("restored identity");
    sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'old','active',1,1)")
        .bind(principal).execute(store.pool()).await.expect("restored team");
    sqlx::query("INSERT INTO memberships (membership_id,team_id,principal_id,builtin_role,state,revision,local_deny_generation) VALUES ($1,$1,$1,'owner','active',1,1)")
        .bind(principal).execute(store.pool()).await.expect("restored authority");
    let lifecycle = reopened(&store, directory.path());
    assert!(matches!(
        lifecycle.migrate_local("relay-test").await,
        Err(LifecycleError::InvalidState)
    ));
    assert!(matches!(
        lifecycle.bootstrap_local(request()).await,
        Err(LifecycleError::InvalidState)
    ));
    assert!(witness.latest().expect("witness").is_none());
    let preserved: bool = sqlx::query_scalar("SELECT (SELECT count(*)=1 FROM principals) AND (SELECT count(*)=1 FROM oidc_identities) AND (SELECT count(*)=1 FROM teams) AND (SELECT count(*)=1 FROM memberships) AND (SELECT count(*)=0 FROM relay_identity) AND (SELECT count(*)=0 FROM audit_events)")
        .fetch_one(store.pool()).await.expect("unchanged authority");
    assert!(preserved);
    sqlx::raw_sql("DELETE FROM memberships; DELETE FROM teams; DELETE FROM oidc_identities; DELETE FROM principals; INSERT INTO permissions VALUES ('unexpected.permission');")
        .execute(store.pool()).await.expect("tampered immutable catalog");
    assert!(matches!(
        lifecycle.bootstrap_local(request()).await,
        Err(LifecycleError::InvalidState)
    ));
    assert!(witness.latest().expect("witness").is_none());
    sqlx::query("DELETE FROM permissions WHERE permission='unexpected.permission'")
        .execute(store.pool())
        .await
        .expect("repair catalog");
    lifecycle
        .bootstrap_local(request())
        .await
        .expect("empty authority bootstrap");
    cleanup(&pool, &schema).await;
}

#[tokio::test]
async fn bootstrap_validates_nul_before_writes_and_resumes_after_precommit_failure() {
    let (store, schema, pool, witness, directory) = fixture().await;
    let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
    let mut invalid = request();
    invalid.subject.push('\0');
    assert!(matches!(
        lifecycle.bootstrap_local(invalid).await,
        Err(LifecycleError::InvalidState)
    ));
    assert!(witness.latest().expect("witness").is_none());
    sqlx::raw_sql("CREATE FUNCTION reject_bootstrap() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture audit unavailable'; END $$; CREATE TRIGGER reject_bootstrap BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_bootstrap();")
        .execute(store.pool()).await.expect("precommit database failure");
    assert!(matches!(
        lifecycle.bootstrap_local(request()).await,
        Err(LifecycleError::Durable)
    ));
    drop(lifecycle);
    let pending = witness
        .latest()
        .expect("witness")
        .expect("signed bootstrap");
    assert!(pending.active_run);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM principals")
        .fetch_one(store.pool())
        .await
        .expect("rollback");
    assert_eq!(count, 0);
    let lifecycle = reopened(&store, directory.path());
    let mut mismatch = request();
    mismatch.subject.push_str("-other");
    assert!(matches!(
        lifecycle.bootstrap_local(mismatch).await,
        Err(LifecycleError::InvalidState)
    ));
    assert_eq!(witness.latest().expect("witness"), Some(pending));
    sqlx::query("DROP TRIGGER reject_bootstrap ON audit_events")
        .execute(store.pool())
        .await
        .expect("repair failure");
    let clean = lifecycle
        .bootstrap_local(request())
        .await
        .expect("resume exact bootstrap");
    assert!(!clean.active_run);
    assert_eq!(
        reopened(&store, directory.path())
            .bootstrap_local(request())
            .await
            .expect("lost success response"),
        clean
    );
    cleanup(&pool, &schema).await;
}

#[tokio::test]
async fn bootstrap_commitment_is_keyed_redacted_and_exactly_replayable() {
    let (store, schema, pool, witness, directory) = fixture().await;
    let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
    let canonical = canonical_bootstrap_request(&request());
    let first = witness
        .bootstrap_commitment(&canonical)
        .expect("derive commitment");
    assert_eq!(
        first,
        witness
            .bootstrap_commitment(&canonical)
            .expect("derive exact commitment")
    );
    let other_directory = tempfile::tempdir().expect("independent witness directory");
    std::fs::set_permissions(
        other_directory.path(),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("private independent directory");
    let other = WitnessStore::open(
        other_directory.path(),
        SigningKey::from_bytes(&[8; 32]),
        "other-key".into(),
    )
    .expect("open independent witness");
    assert_ne!(
        first,
        other
            .bootstrap_commitment(&canonical)
            .expect("derive separated commitment")
    );
    let clean = lifecycle
        .bootstrap_local(request())
        .await
        .expect("bootstrap with keyed commitment");
    assert!(matches!(
        &clean.event,
        WitnessEvent::Bootstrap {
            commitment_version: 1,
            commitment_key_id,
            request_commitment,
            ..
        } if commitment_key_id == "test" && *request_commitment == first
    ));
    let serialized = serde_json::to_string(&clean).expect("serialize safe witness");
    assert!(!serialized.contains("operator"));
    let parameter: String = sqlx::query_scalar(
        "SELECT parameter_value FROM audit_events WHERE action='lifecycle.bootstrap'",
    )
    .fetch_one(store.pool())
    .await
    .expect("read audit commitment");
    assert!(parameter.contains(&hex::encode(first)));
    assert!(!parameter.contains("operator"));
    assert_eq!(
        reopened(&store, directory.path())
            .bootstrap_local(request())
            .await
            .expect("exact bootstrap replay"),
        clean
    );
    cleanup(&pool, &schema).await;
}

#[tokio::test]
async fn bootstrap_repairs_postcommit_clean_publication_and_rejects_extra_authority() {
    let (store, schema, pool, witness, directory) = fixture().await;
    let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
    witness.set_failpoint(Some("preunlink"));
    assert!(matches!(
        lifecycle.bootstrap_local(request()).await,
        Err(LifecycleError::Witness(_))
    ));
    drop(lifecycle);
    let pending = witness.latest().expect("witness").expect("pending");
    assert!(pending.active_run);
    let lifecycle = reopened(&store, directory.path());
    sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)")
        .bind(Uuid::now_v7())
        .execute(store.pool())
        .await
        .expect("unexpected authority");
    assert!(matches!(
        lifecycle.bootstrap_local(request()).await,
        Err(LifecycleError::InvalidState)
    ));
    sqlx::query("DELETE FROM principals WHERE kind='human'")
        .execute(store.pool())
        .await
        .expect("remove unexpected authority");
    let clean = lifecycle
        .bootstrap_local(request())
        .await
        .expect("repair clean publication after database commit");
    assert_eq!(clean.sequence, pending.sequence + 1);
    assert!(!clean.active_run);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(store.pool())
        .await
        .expect("audit count");
    assert_eq!(count, 1);
    cleanup(&pool, &schema).await;
}

#[tokio::test]
async fn migration_restarts_from_exact_foundation_prefix_and_repairs_publication() {
    let (store, schema, pool, witness, directory) = foundation_fixture().await;
    let clean = witness
        .latest()
        .expect("witness")
        .expect("clean foundation");
    let (base, authority_digest) = migration_coordinates(&store).await;
    let pending = witness
        .begin_local(
            Some(&clean),
            "relay-test",
            1,
            migration_event(base, authority_digest),
        )
        .expect("process crashes after exact pre-DDL latch");
    assert!(pending.active_run);
    reopened(&store, directory.path())
        .migrate_local("relay-test")
        .await
        .expect("restart resumes foundation to auth prefix");
    let clean = witness.latest().expect("witness").expect("clean migration");
    assert!(!clean.active_run);
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(store.pool())
            .await
            .expect("read committed prefix");
    assert_eq!(versions, vec![1, 2]);
    let auth_table: bool = sqlx::query_scalar("SELECT to_regclass('oidc_identities') IS NOT NULL")
        .fetch_one(store.pool())
        .await
        .expect("auth schema visible");
    assert!(auth_table);
    reopened(&store, directory.path())
        .migrate_local("relay-test")
        .await
        .expect("restart after committed migration prefix");
    witness.set_failpoint(Some("preunlink"));
    assert!(matches!(
        Lifecycle::new(store.clone(), Arc::clone(&witness))
            .migrate_local("relay-test")
            .await,
        Err(LifecycleError::Witness(_))
    ));
    reopened(&store, directory.path())
        .migrate_local("relay-test")
        .await
        .expect("repair postcommit migration publication");
    let clean = witness.latest().expect("witness").expect("clean");
    witness.set_failpoint(None);
    let (base, authority_digest) = migration_coordinates(&store).await;
    let mut different = Store::migration_plan_digest();
    different[0] ^= 1;
    let pending = witness
        .begin_local(
            Some(&clean),
            "relay-test",
            1,
            WitnessEvent::Migration {
                base_migration_count: base.count(),
                base_plan_digest: base.digest(),
                target_migration_count: Store::migration_target_prefix().expect("target").count(),
                target_plan_digest: different,
                authority_digest,
            },
        )
        .expect("different binary plan");
    assert!(matches!(
        reopened(&store, directory.path())
            .migrate_local("relay-test")
            .await,
        Err(LifecycleError::InvalidState)
    ));
    assert_eq!(witness.latest().expect("witness"), Some(pending));
    cleanup(&pool, &schema).await;
}

#[tokio::test]
async fn migration_rejects_swapped_authority_before_auth_ddl() {
    let (store, schema, pool, witness, directory) = foundation_fixture().await;
    let clean = witness
        .latest()
        .expect("witness")
        .expect("clean foundation");
    let (base, authority_digest) = migration_coordinates(&store).await;
    witness
        .begin_local(
            Some(&clean),
            "relay-test",
            1,
            migration_event(base, authority_digest),
        )
        .expect("persist migration latch before process loss");
    sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)")
        .bind(Uuid::now_v7())
        .execute(store.pool())
        .await
        .expect("swap authority after signed pre-state");
    assert!(matches!(
        reopened(&store, directory.path())
            .migrate_local("relay-test")
            .await,
        Err(LifecycleError::InvalidState)
    ));
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(store.pool())
            .await
            .expect("read unchanged migration prefix");
    assert_eq!(versions, vec![1]);
    let auth_table: bool = sqlx::query_scalar("SELECT to_regclass('oidc_identities') IS NOT NULL")
        .fetch_one(store.pool())
        .await
        .expect("verify auth schema was not created");
    assert!(!auth_table);
    cleanup(&pool, &schema).await;
}

#[tokio::test]
async fn migration_hash_is_stable_across_session_rendering_defaults() {
    let (store, schema, pool, witness, directory) = foundation_fixture().await;
    let principal = Uuid::now_v7();
    let audit = Uuid::now_v7();
    sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)")
        .bind(principal)
        .execute(store.pool())
        .await
        .expect("seed timestamp row");
    sqlx::query("INSERT INTO audit_events (audit_id,actor_kind,action,decision,policy_generation,recovery_generation,correlation_id,parameter_code,parameter_value,outcome) VALUES ($1,'system','migration.digest','changed',1,1,$1,'test','digest','committed')")
        .bind(audit).execute(store.pool()).await.expect("seed audit receipt parent");
    sqlx::query("INSERT INTO mutation_receipts (actor_principal_id,action,idempotency_key,request_digest,audit_id,outcome_code) VALUES ($1,'migration.digest',$2,decode(repeat('ab',32),'hex'),$3,'committed')")
        .bind(principal).bind(Uuid::now_v7()).bind(audit).execute(store.pool()).await.expect("seed bytea row");
    let (prague_prefix, prague_digest) =
        migration_coordinates_with_session_defaults(&store, "Europe/Prague", "escape").await;
    let (utc_prefix, utc_digest) =
        migration_coordinates_with_session_defaults(&store, "UTC", "hex").await;
    assert_eq!(prague_prefix, utc_prefix);
    assert_eq!(prague_digest, utc_digest);
    let clean = witness.latest().expect("witness").expect("clean");
    witness
        .begin_local(
            Some(&clean),
            "relay-test",
            1,
            migration_event(prague_prefix, prague_digest),
        )
        .expect("pending migration");
    reopened(&store, directory.path())
        .migrate_local("relay-test")
        .await
        .expect("retry under canonical defaults");
    cleanup(&pool, &schema).await;
}

#[tokio::test]
async fn migration_rejects_a_signed_target_plan_mismatch_before_ddl() {
    let (store, schema, pool, witness, directory) = foundation_fixture().await;
    let clean = witness
        .latest()
        .expect("witness")
        .expect("clean foundation");
    let (base, authority_digest) = migration_coordinates(&store).await;
    let target = Store::migration_target_prefix().expect("embedded target");
    let mut wrong_target = target.digest();
    wrong_target[0] ^= 1;
    witness
        .begin_local(
            Some(&clean),
            "relay-test",
            1,
            WitnessEvent::Migration {
                base_migration_count: base.count(),
                base_plan_digest: base.digest(),
                target_migration_count: target.count(),
                target_plan_digest: wrong_target,
                authority_digest,
            },
        )
        .expect("persist mismatched plan latch");
    assert!(matches!(
        reopened(&store, directory.path())
            .migrate_local("relay-test")
            .await,
        Err(LifecycleError::InvalidState)
    ));
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(store.pool())
            .await
            .expect("read unchanged migration prefix");
    assert_eq!(versions, vec![1]);
    cleanup(&pool, &schema).await;
}
