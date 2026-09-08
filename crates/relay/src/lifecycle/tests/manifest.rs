//! Recovery review binds credential provenance and service account authority.

use super::*;

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "One recovery snapshot tests every credential provenance field and exact restart replay"
)]
async fn durable_quarantine_and_manifest_bind_service_and_credential_authority() {
    let (store, schema, pool, witness, directory) = fixture().await;
    let lifecycle = Lifecycle::new(store.clone(), Arc::clone(&witness));
    let clean = lifecycle
        .bootstrap_local(request())
        .await
        .expect("bootstrap");
    let principal: Uuid = sqlx::query_scalar("SELECT id FROM principals")
        .fetch_one(store.pool())
        .await
        .expect("bootstrap principal");
    let identity: Uuid = sqlx::query_scalar("SELECT identity_id FROM oidc_identities")
        .fetch_one(store.pool())
        .await
        .expect("bootstrap identity");
    let second_identity = Uuid::now_v7();
    sqlx::query("INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,'https://issuer.test','second-subject',$2,1)")
        .bind(second_identity).bind(principal).execute(store.pool()).await.expect("second identity");
    let team = Uuid::now_v7();
    let other_team = Uuid::now_v7();
    for id in [team, other_team] {
        sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'Review team','active',1,1)")
            .bind(id).execute(store.pool()).await.expect("team");
    }
    let service = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'service','active',1)",
    )
    .bind(service)
    .execute(store.pool())
    .await
    .expect("service principal");
    sqlx::query("INSERT INTO service_accounts (principal_id,team_id,display_name) VALUES ($1,$2,'Review service')")
        .bind(service).bind(team).execute(store.pool()).await.expect("service account");
    let credential = Uuid::now_v7();
    let predecessor = Uuid::now_v7();
    for (id, byte) in [(credential, 11_u8), (predecessor, 12_u8)] {
        sqlx::query("INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) VALUES ($1,$1::text,$2,$3,$4,'test','human',1,$1,1,clock_timestamp()+interval '1 hour')")
            .bind(id).bind(vec![byte;32]).bind(principal).bind(identity).execute(store.pool()).await.expect("human credential");
    }
    let session = Uuid::now_v7();
    sqlx::query("INSERT INTO browser_sessions (session_id,cookie_digest,csrf_digest,principal_id,identity_id,digest_key_id,session_generation,recovery_generation,expires_at,idle_deadline) VALUES ($1,$2,$3,$4,$5,'test',1,1,clock_timestamp()+interval '1 hour',clock_timestamp()+interval '30 minutes')")
        .bind(session).bind(vec![13_u8;32]).bind(vec![14_u8;32]).bind(principal).bind(identity).execute(store.pool()).await.expect("browser session");
    let before = lifecycle
        .snapshot_authority_manifest()
        .await
        .expect("populated manifest");
    let advanced = lifecycle
        .advance_restore_witness(&clean, &before)
        .expect("restore checkpoint");
    drop(lifecycle);
    drop(witness);
    let reopened_witness = Arc::new(
        WitnessStore::open(
            directory.path(),
            SigningKey::from_bytes(&[7; 32]),
            "test".into(),
        )
        .expect("reopen durable witness"),
    );
    let lifecycle = Lifecycle::new(store.clone(), reopened_witness);
    lifecycle
        .quarantine_pending()
        .await
        .expect("quarantine without pre-restore process state");
    let reviewed = lifecycle
        .snapshot_authority_manifest()
        .await
        .expect("quarantine review");
    drop(lifecycle);
    let reopened_witness = Arc::new(
        WitnessStore::open(
            directory.path(),
            SigningKey::from_bytes(&[7; 32]),
            "test".into(),
        )
        .expect("reopen after committed quarantine"),
    );
    let lifecycle = Lifecycle::new(store.clone(), reopened_witness);
    lifecycle
        .quarantine_pending()
        .await
        .expect("retry committed quarantine after restart");
    assert_eq!(
        reviewed,
        lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("retry leaves generations unchanged")
    );
    let receipts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM restore_reviews WHERE action='quarantine'")
            .fetch_one(store.pool())
            .await
            .expect("quarantine receipt count");
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='lifecycle.restore.quarantine'",
    )
    .fetch_one(store.pool())
    .await
    .expect("quarantine audit count");
    assert_eq!((receipts, audits), (1, 1));
    let duplicate = Uuid::now_v7();
    let mut tx = lifecycle
        .begin()
        .await
        .expect("duplicate receipt transaction");
    let duplicate_audit = audit(
        &mut tx,
        "lifecycle.restore.quarantine",
        "revoke",
        advanced.recovery_generation,
        "recovery",
        "committed",
    )
    .await
    .expect("separate duplicate audit");
    sqlx::query("INSERT INTO restore_reviews (review_id,relay_id,witness_generation,manifest_digest,action,audit_id) SELECT $1,relay_id,witness_generation,manifest_digest,action,$2 FROM restore_reviews WHERE action='quarantine'")
        .bind(duplicate).bind(duplicate_audit).execute(&mut *tx).await.expect("duplicate receipt fixture");
    tx.commit().await.expect("duplicate receipt commit");
    assert!(matches!(
        lifecycle.quarantine_pending().await,
        Err(LifecycleError::InvalidState)
    ));
    sqlx::query("DELETE FROM restore_reviews WHERE review_id=$1")
        .bind(duplicate)
        .execute(store.pool())
        .await
        .expect("remove duplicate fixture");
    sqlx::query("UPDATE restore_reviews SET manifest_digest=$1 WHERE action='quarantine'")
        .bind(vec![255_u8; 32])
        .execute(store.pool())
        .await
        .expect("mutate receipt");
    assert!(matches!(
        lifecycle.quarantine_pending().await,
        Err(LifecycleError::InvalidState)
    ));
    sqlx::query(
        "UPDATE restore_reviews SET manifest_digest=decode($1,'hex') WHERE action='quarantine'",
    )
    .bind(&advanced.manifest_digest)
    .execute(store.pool())
    .await
    .expect("restore receipt");
    for settings in [
        "SET LOCAL TimeZone='UTC'; SET LOCAL DateStyle='ISO, MDY'",
        "SET LOCAL TimeZone='Europe/Prague'; SET LOCAL DateStyle='German, DMY'",
    ] {
        let mut tx = lifecycle.begin().await.expect("locale transaction");
        sqlx::raw_sql(AssertSqlSafe(settings))
            .execute(&mut *tx)
            .await
            .expect("different PostgreSQL locale");
        let incidents = lifecycle
            .witness
            .incident_review(&advanced)
            .expect("witness incidents");
        assert_eq!(
            reviewed,
            semantic_manifest(&mut tx, incidents, false)
                .await
                .expect("canonical manifest")
        );
        tx.commit().await.expect("locale transaction commit");
    }
    assert_eq!(reviewed.version(), 2);
    assert_eq!(
        reviewed,
        lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("stable manifest")
    );
    let wire = serde_json::to_string(&reviewed).expect("review JSON");
    for byte in [11_u8, 12, 13, 14] {
        assert!(
            !wire.contains(&hex::encode([byte; 32])),
            "raw verifier must not be review output"
        );
    }
    assert!(wire.contains("secret_digest_commitment"));
    assert!(wire.contains("service_account"));
    // Normal writers cannot reparent a service account. This explicitly models
    // a restored corrupt database so recovery still detects that parent change.
    set_restored_service_account_team(&store, service, other_team).await;
    assert!(matches!(
        lifecycle.reopen_local(&advanced, &reviewed).await,
        Err(LifecycleError::ManifestMismatch)
    ));
    set_restored_service_account_team(&store, service, team).await;
    let changes = [
        (
            "UPDATE relay_credentials SET identity_id=$1 WHERE credential_id=$2",
            second_identity,
            credential,
            "UPDATE relay_credentials SET identity_id=$1 WHERE credential_id=$2",
            identity,
        ),
        (
            "UPDATE relay_credentials SET rotation_family_id=$1 WHERE credential_id=$2",
            predecessor,
            credential,
            "UPDATE relay_credentials SET rotation_family_id=$1 WHERE credential_id=$2",
            credential,
        ),
        (
            "UPDATE browser_sessions SET identity_id=$1 WHERE session_id=$2",
            second_identity,
            session,
            "UPDATE browser_sessions SET identity_id=$1 WHERE session_id=$2",
            identity,
        ),
    ];
    for (change, value, id, undo, original) in changes {
        sqlx::query(AssertSqlSafe(change))
            .bind(value)
            .bind(id)
            .execute(store.pool())
            .await
            .expect("change authority");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        sqlx::query(AssertSqlSafe(undo))
            .bind(original)
            .bind(id)
            .execute(store.pool())
            .await
            .expect("restore authority");
    }
    let changes = [
        ("UPDATE relay_credentials SET secret_digest=decode(repeat('15',32),'hex') WHERE credential_id=$1", credential,
         "UPDATE relay_credentials SET secret_digest=decode(repeat('0b',32),'hex') WHERE credential_id=$1"),
        ("UPDATE relay_credentials SET digest_key_id='changed' WHERE credential_id=$1", credential,
         "UPDATE relay_credentials SET digest_key_id='test' WHERE credential_id=$1"),
        ("UPDATE relay_credentials SET issued_at=issued_at-interval '1 second' WHERE credential_id=$1", credential,
         "UPDATE relay_credentials SET issued_at=issued_at+interval '1 second' WHERE credential_id=$1"),
        ("UPDATE browser_sessions SET cookie_digest=decode(repeat('15',32),'hex') WHERE session_id=$1", session,
         "UPDATE browser_sessions SET cookie_digest=decode(repeat('0d',32),'hex') WHERE session_id=$1"),
        ("UPDATE browser_sessions SET csrf_digest=decode(repeat('15',32),'hex') WHERE session_id=$1", session,
         "UPDATE browser_sessions SET csrf_digest=decode(repeat('0e',32),'hex') WHERE session_id=$1"),
        ("UPDATE browser_sessions SET digest_key_id='changed' WHERE session_id=$1", session,
         "UPDATE browser_sessions SET digest_key_id='test' WHERE session_id=$1"),
        ("UPDATE service_accounts SET deprovisioned_at=clock_timestamp() WHERE principal_id=$1", service,
         "UPDATE service_accounts SET deprovisioned_at=NULL WHERE principal_id=$1"),
    ];
    for (change, id, undo) in changes {
        sqlx::query(AssertSqlSafe(change))
            .bind(id)
            .execute(store.pool())
            .await
            .expect("change authority");
        assert!(matches!(
            lifecycle.reopen_local(&advanced, &reviewed).await,
            Err(LifecycleError::ManifestMismatch)
        ));
        sqlx::query(AssertSqlSafe(undo))
            .bind(id)
            .execute(store.pool())
            .await
            .expect("restore authority");
    }
    sqlx::query("UPDATE relay_credentials SET last_used_at=clock_timestamp()")
        .execute(store.pool())
        .await
        .expect("operational metadata");
    sqlx::query("UPDATE browser_sessions SET last_used_at=clock_timestamp()")
        .execute(store.pool())
        .await
        .expect("session operational metadata");
    assert_eq!(
        reviewed,
        lifecycle
            .snapshot_authority_manifest()
            .await
            .expect("last use does not confer authority")
    );
    lifecycle
        .reopen_local(&advanced, &reviewed)
        .await
        .expect("reviewed reopen");
    cleanup(&pool, &schema).await;
}

async fn set_restored_service_account_team(store: &Store, principal_id: Uuid, team_id: Uuid) {
    sqlx::query("ALTER TABLE service_accounts DISABLE TRIGGER service_accounts_identity_immutable")
        .execute(store.pool())
        .await
        .expect("allow explicit restored-corruption fixture");
    sqlx::query("UPDATE service_accounts SET team_id=$1 WHERE principal_id=$2")
        .bind(team_id)
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("set restored-corruption service-account team");
    sqlx::query("ALTER TABLE service_accounts ENABLE TRIGGER service_accounts_identity_immutable")
        .execute(store.pool())
        .await
        .expect("restore service-account reparenting protection");
}
