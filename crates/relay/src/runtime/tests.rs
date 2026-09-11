use super::*;
use crate::lifecycle::BootstrapRequest;
use sqlx::AssertSqlSafe;
use std::os::unix::fs::PermissionsExt;

#[derive(Debug)]
struct Fixture {
    store: Store,
    bootstrap: Store,
    schema: String,
    witness: Arc<WitnessStore>,
    relay_id: String,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let url = std::env::var("POHUNEK_RELAY_TEST_DATABASE_URL")
            .expect("explicit PostgreSQL fixture required");
        let bootstrap = Store::connect(&url, 1).await.expect("fixture PostgreSQL");
        let schema = format!("relay_runtime_{}", Uuid::now_v7().simple());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(bootstrap.pool())
            .await
            .expect("isolated schema");
        let store = Store::connect(&format!("{url}?options[search_path]={schema}"), 1)
            .await
            .expect("scoped pool");
        store.migrate().await.expect("embedded migrations");
        let directory = tempfile::tempdir().expect("witness directory");
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let witness = Arc::new(
            WitnessStore::open(
                directory.path(),
                ed25519_dalek::SigningKey::from_bytes(&[3; 32]),
                "test".to_owned(),
            )
            .expect("witness"),
        );
        let relay_id = format!("relay_{}", "A".repeat(43));
        Lifecycle::new(store.clone(), Arc::clone(&witness))
            .bootstrap_local(BootstrapRequest {
                relay_id: relay_id.clone(),
                issuer: "https://issuer.example/realms/test".to_owned(),
                subject: "bootstrap-subject".to_owned(),
            })
            .await
            .expect("bootstrap");
        Self {
            store,
            bootstrap,
            schema,
            witness,
            relay_id,
            directory,
        }
    }

    async fn acquire(&self) -> RuntimePreparation {
        RuntimePreparation::acquire(
            self.store.clone(),
            Arc::clone(&self.witness),
            AuthorityLimits {
                global: 4,
                per_team: 4,
                per_principal: 2,
            },
            &self.relay_id,
        )
        .await
        .expect("acquire runtime")
    }

    async fn cleanup(self) {
        self.store.pool().close().await;
        sqlx::query(AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(self.bootstrap.pool())
        .await
        .expect("remove schema");
    }
}

#[tokio::test]
async fn background_renewal_outlives_initial_lease_and_clean_shutdown_restarts() {
    let fixture = Fixture::new().await;
    let runtime = fixture.acquire().await;
    let authority = runtime.authority();
    tokio::select! {
        result = renew(&authority, Duration::from_millis(100)) => panic!("renewal stopped: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(6)) => (),
    }
    authority
        .validate_fence()
        .await
        .expect("renewed beyond original deadline");
    runtime.shutdown().await.expect("clean shutdown");
    assert!(
        !fixture
            .witness
            .latest()
            .expect("checkpoint")
            .expect("present")
            .active_run
    );
    fixture
        .acquire()
        .await
        .shutdown()
        .await
        .expect("restart after clean stop");
    fixture.cleanup().await;
}

#[tokio::test]
async fn idle_fence_loss_closes_authority_and_preserves_dirty_latch() {
    let fixture = Fixture::new().await;
    let runtime = fixture.acquire().await;
    let authority = runtime.authority();
    sqlx::query("UPDATE relay_lease SET fence_token = $1")
        .bind(Uuid::now_v7())
        .execute(fixture.store.pool())
        .await
        .expect("simulate replaced fence");
    timeout(
        Duration::from_secs(1),
        watch(&authority, Duration::from_millis(100)),
    )
    .await
    .expect("watch is bounded")
    .expect_err("changed fence rejected");
    assert!(authority.is_closed());
    runtime
        .shutdown()
        .await
        .expect_err("failed process cannot clear witness");
    assert!(
        fixture
            .witness
            .latest()
            .expect("checkpoint")
            .expect("present")
            .active_run
    );
    fixture.cleanup().await;
}

#[tokio::test]
async fn configured_identity_mismatch_does_not_modify_the_checkpoint() {
    let fixture = Fixture::new().await;
    let before = fixture.witness.latest().expect("checkpoint");
    let error = RuntimePreparation::acquire(
        fixture.store.clone(),
        Arc::clone(&fixture.witness),
        AuthorityLimits {
            global: 1,
            per_team: 1,
            per_principal: 1,
        },
        "another-relay",
    )
    .await
    .expect_err("identity mismatch");
    assert!(matches!(error, RuntimeError::IdentityMismatch));
    assert_eq!(fixture.witness.latest().expect("checkpoint"), before);
    fixture.cleanup().await;
}

#[tokio::test]
async fn occupied_application_pool_and_transaction_fence_do_not_starve_control() {
    let fixture = Fixture::new().await;
    let runtime = fixture.acquire().await;
    let authority = runtime.authority();
    let mut transaction = fixture
        .store
        .begin_serializable()
        .await
        .expect("occupy the only application connection");
    authority
        .verify_fence_in_transaction(&mut transaction)
        .await
        .expect("hold fence key lock");
    let request_a = Arc::clone(&authority);
    let request_a = tokio::spawn(async move { request_a.validate_fence().await });
    let request_b = Arc::clone(&authority);
    let request_b = tokio::spawn(async move { request_b.validate_fence().await });
    tokio::select! {
        result = renew(&authority, Duration::from_millis(100)) => panic!("application work blocked renewal: {result:?}"),
        result = watch(&authority, Duration::from_millis(100)) => panic!("application work blocked watchdog: {result:?}"),
        () = tokio::time::sleep(Duration::from_millis(350)) => (),
    }
    assert!(
        !request_a.is_finished(),
        "request validation waits on application capacity"
    );
    assert!(
        !request_b.is_finished(),
        "request validations cannot occupy control slots"
    );
    assert!(!authority.is_closed());
    transaction
        .rollback()
        .await
        .expect("release application connection");
    request_a
        .await
        .expect("request A task")
        .expect("request A current fence");
    request_b
        .await
        .expect("request B task")
        .expect("request B current fence");
    runtime.shutdown().await.expect("clean shutdown");
    fixture.cleanup().await;
}

#[tokio::test]
async fn invalid_checkpoint_cannot_run_pending_database_migrations() {
    let fixture = Fixture::new().await;
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version>=2")
        .execute(fixture.store.pool())
        .await
        .expect("represent pending migration");
    RuntimePreparation::acquire(
        fixture.store.clone(),
        Arc::clone(&fixture.witness),
        AuthorityLimits {
            global: 1,
            per_team: 1,
            per_principal: 1,
        },
        "wrong-relay",
    )
    .await
    .expect_err("mismatched identity rejected before migrations");
    let checkpoint = fixture
        .witness
        .latest()
        .expect("checkpoint")
        .expect("present");
    fixture
        .witness
        .begin_run(Some(&checkpoint), &fixture.relay_id, 1)
        .expect("simulate dirty checkpoint");
    RuntimePreparation::acquire(
        fixture.store.clone(),
        Arc::clone(&fixture.witness),
        AuthorityLimits {
            global: 1,
            per_team: 1,
            per_principal: 1,
        },
        &fixture.relay_id,
    )
    .await
    .expect_err("dirty checkpoint rejected before migrations");
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(fixture.store.pool())
            .await
            .expect("migration metadata");
    assert_eq!(versions, [1]);
    fixture.cleanup().await;
}

#[tokio::test]
async fn migration_failure_after_startup_latch_requires_review() {
    let fixture = Fixture::new().await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum=$1 WHERE version=2")
        .bind(vec![0_u8; 48])
        .execute(fixture.store.pool())
        .await
        .expect("simulate migration mismatch");
    RuntimePreparation::acquire(
        fixture.store.clone(),
        Arc::clone(&fixture.witness),
        AuthorityLimits {
            global: 1,
            per_team: 1,
            per_principal: 1,
        },
        &fixture.relay_id,
    )
    .await
    .expect_err("migration fails under active latch");
    assert!(
        fixture
            .witness
            .latest()
            .expect("checkpoint")
            .expect("present")
            .active_run
    );
    let leases: i64 = sqlx::query_scalar("SELECT count(*) FROM relay_lease")
        .fetch_one(fixture.store.pool())
        .await
        .expect("lease count");
    assert_eq!(leases, 0);
    fixture.cleanup().await;
}

mod ingress;

#[tokio::test]
async fn runtime_debug_does_not_reveal_nested_witness_paths() {
    let fixture = Fixture::new().await;
    let runtime = fixture.acquire().await;
    let rendered = format!("{runtime:?}");
    assert!(!rendered.contains(fixture.directory.path().to_str().expect("fixture path")));
    runtime.shutdown().await.expect("clean shutdown");
    fixture.cleanup().await;
}

#[tokio::test]
async fn transaction_fence_allows_heartbeat_but_blocks_expired_lease_takeover() {
    let fixture = Fixture::new().await;
    let runtime = fixture.acquire().await;
    let authority = runtime.authority();
    let mut transaction = fixture
        .store
        .begin_serializable()
        .await
        .expect("mutation transaction");
    authority
        .verify_fence_in_transaction(&mut transaction)
        .await
        .expect("mutation owns fence key lock");
    timeout(Duration::from_millis(250), authority.renew_once())
        .await
        .expect("heartbeat is compatible")
        .expect("renewed held key");
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {}.relay_lease SET expires_at = clock_timestamp() - interval '1 second'",
        fixture.schema
    )))
    .execute(fixture.bootstrap.pool())
    .await
    .expect("simulate expired lease without changing key");
    let url = std::env::var("POHUNEK_RELAY_TEST_DATABASE_URL").expect("explicit fixture URL");
    let application = format!("takeover_{}", Uuid::now_v7().simple());
    let successor = Store::connect(
        &format!(
            "{url}?options[search_path]={}&application_name={application}",
            fixture.schema
        ),
        1,
    )
    .await
    .expect("independent successor pools");
    let relay_id = fixture.relay_id.clone();
    let taking_over = successor.clone();
    let takeover = tokio::spawn(async move {
        taking_over
            .acquire_lease(&relay_id, Uuid::now_v7(), 1)
            .await
    });
    timeout(Duration::from_secs(1), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock')")
                .bind(&application).fetch_one(fixture.bootstrap.pool()).await.expect("observe successor lock wait");
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.expect("successor reaches actual PostgreSQL lock wait");
    assert!(
        !takeover.is_finished(),
        "takeover cannot cross the old transaction's fence check and commit"
    );
    transaction
        .rollback()
        .await
        .expect("finish old mutation transaction");
    let successor_lease = timeout(Duration::from_secs(1), takeover)
        .await
        .expect("takeover unblocks")
        .expect("successor task")
        .expect("successor lease acquired");
    successor
        .validate_lease(&successor_lease)
        .await
        .expect("successor owns current fence");
    authority
        .validate_fence()
        .await
        .expect_err("old process is fenced out");
    runtime
        .shutdown()
        .await
        .expect_err("stale runtime cannot mark witness clean");
    successor
        .release_lease(&successor_lease)
        .await
        .expect("release successor fixture");
    fixture.cleanup().await;
}
