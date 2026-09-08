//! Restart and tamper coverage for independently witnessed local operations.

use super::*;
use crate::recovery::WitnessEvent;

fn reopened(store: &Store, directory: &std::path::Path) -> Lifecycle {
    let witness = WitnessStore::open(directory, SigningKey::from_bytes(&[7; 32]), "test".into())
        .expect("reopen independent witness after process loss");
    Lifecycle::new(store.clone(), Arc::new(witness))
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
async fn migration_resumes_only_exact_plan_and_repairs_postcommit_publication() {
    let (store, schema, pool, witness, directory) = fixture().await;
    let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
    let clean = lifecycle
        .bootstrap_local(request())
        .await
        .expect("bootstrap");
    let manifest = lifecycle
        .snapshot_authority_manifest()
        .await
        .expect("manifest");
    let plan_digest = Store::migration_plan_digest();
    let pending = witness
        .begin_local(
            Some(&clean),
            "relay-test",
            1,
            WitnessEvent::Migration {
                plan_digest,
                authority_digest: *manifest.digest(),
            },
        )
        .expect("process crashes before migration");
    drop(lifecycle);
    // A SQLx checksum failure is recoverable only under the same signed plan.
    sqlx::query("UPDATE _sqlx_migrations SET success=false WHERE version=2")
        .execute(store.pool())
        .await
        .expect("inject migration failure");
    assert!(matches!(
        reopened(&store, directory.path())
            .migrate_local("relay-test")
            .await,
        Err(LifecycleError::Durable)
    ));
    assert_eq!(witness.latest().expect("witness"), Some(pending));
    sqlx::query("UPDATE _sqlx_migrations SET success=true WHERE version=2")
        .execute(store.pool())
        .await
        .expect("repair migration record");
    reopened(&store, directory.path())
        .migrate_local("relay-test")
        .await
        .expect("resume exact pending migration");
    let clean = witness.latest().expect("witness").expect("clean migration");
    assert!(!clean.active_run);
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
    let mut different = plan_digest;
    different[0] ^= 1;
    let pending = witness
        .begin_local(
            Some(&clean),
            "relay-test",
            1,
            WitnessEvent::Migration {
                plan_digest: different,
                authority_digest: *manifest.digest(),
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
