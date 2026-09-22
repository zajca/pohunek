//! Adversarial coverage for the account-linking lifecycle.
//!
//! Every test drives the real `PostgreSQL` schema through the public service
//! surface. A denial is pinned by its exact typed error so two materially
//! different refusals can never collapse into one variant unnoticed.

// Rust guideline compliant 2026-09-22

use std::{sync::Arc, time::Duration};

use sqlx::AssertSqlSafe;
use zeroize::Zeroizing;

use crate::auth::service::random_token;

use super::*;
use crate::{
    admission::Authority,
    auth::{
        service::tests::{authority, cleanup, fixture, limits},
        BrowserCookie, DigestKey, RelayBearerCredential,
    },
    config::LoginPolicy,
    store::Store,
};

/// Issuer the fixture `OidcClient` is configured with.
const TEST_ISSUER: &str = "https://issuer.example";
/// Client and audience the fixture `OidcClient` is configured with.
const TEST_CLIENT: &str = "client";

/// One seeded account with both possession channels already authenticated.
struct Account {
    principal_id: Uuid,
    identity_id: Uuid,
    credential_id: Uuid,
    bearer: AuthenticatedActor,
    browser: AuthenticatedActor,
}

fn idempotency() -> Idempotency {
    Idempotency {
        correlation_id: Uuid::now_v7(),
        idempotency_key: Uuid::now_v7(),
    }
}

fn link_request() -> AccountLinkRequest {
    AccountLinkRequest {
        idempotency: idempotency(),
    }
}

fn service(store: &Store, authority: Arc<Authority>) -> AuthService {
    AuthService::new(
        store.clone(),
        DigestKey::new("test".into(), b"ephemeral-test-key".to_vec()),
        4,
        limits(),
        LoginPolicy::AnyAuthenticatedSubject,
        authority,
    )
}

/// Seeds one active human principal with a linked identity, a bearer credential
/// and a browser session, then authenticates both through the real resolvers.
async fn seed_account(store: &Store, service: &AuthService, subject: &str) -> Account {
    let principal_id = Uuid::now_v7();
    let identity_id = Uuid::now_v7();
    let credential_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();

    let secret = format!("secret-{subject}");
    let cookie = format!("cookie-{subject}");
    sqlx::query("INSERT INTO principals (id,kind,state,generation) VALUES ($1,'human','active',1)")
        .bind(principal_id)
        .execute(store.pool())
        .await
        .expect("seed principal");
    sqlx::query(
        "INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) \
         VALUES ($1,$2,$3,$4,1)",
    )
    .bind(identity_id)
    .bind(TEST_ISSUER)
    .bind(subject)
    .bind(principal_id)
    .execute(store.pool())
    .await
    .expect("seed source identity");
    sqlx::query(
        "INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) \
         VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,clock_timestamp()+interval '1 hour')",
    )
    .bind(credential_id)
    .bind(credential_id.to_string())
    .bind(service.digest_key.digest(&secret).as_slice())
    .bind(principal_id)
    .bind(identity_id)
    .execute(store.pool())
    .await
    .expect("seed bearer credential");
    sqlx::query(
        "INSERT INTO browser_sessions (session_id,cookie_digest,csrf_digest,principal_id,identity_id,digest_key_id,session_generation,recovery_generation,expires_at,idle_deadline) \
         VALUES ($1,$2,$3,$4,$5,'test',1,1,now()+interval '1 hour',now()+interval '30 minutes')",
    )
    .bind(session_id)
    .bind(service.digest_key.digest(&cookie).as_slice())
    .bind(service.digest_key.digest(&format!("csrf-{subject}")).as_slice())
    .bind(principal_id)
    .bind(identity_id)
    .execute(store.pool())
    .await
    .expect("seed browser session");
    let bearer = service
        .authenticate_bearer(RelayBearerCredential::new(format!(
            "{credential_id}.{secret}"
        )))
        .await
        .expect("authenticate seeded bearer credential");
    let browser = service
        .authenticate_browser(BrowserCookie::new(cookie))
        .await
        .expect("authenticate seeded browser session");
    Account {
        principal_id,
        identity_id,
        credential_id,
        bearer,
        browser,
    }
}

/// Proves an identity without a provider round trip so the durable collision,
/// self-link, generation and audit effects can be exercised directly.
fn proven(subject: &str) -> OidcIdentity {
    OidcIdentity {
        issuer: TEST_ISSUER.to_owned(),
        subject: subject.to_owned(),
    }
}

/// One pending transaction plus the provider row its channel would have created.
struct OpenLink {
    record: AccountLinkRecord,
    login_id: Uuid,
    possession: String,
    poll_lease_token: [u8; RANDOM_SECRET_BYTES],
}

/// Opens a pending transaction through the production path and seeds its
/// provider row.
///
/// The fixture issuer is not reachable, so `begin_device_link` cannot run in a
/// unit test; this drives the same `link_source` and `open_link_transaction`
/// code a start uses rather than restating their effects.
async fn open_link(
    service: &AuthService,
    store: &Store,
    actor: AuthenticatedActor,
    channel: AccountLinkChannel,
) -> Result<OpenLink, AuthError> {
    open_link_for_audience(service, store, actor, channel, TEST_CLIENT).await
}

/// Opens a pending transaction bound to one exact client and audience.
async fn open_link_for_audience(
    service: &AuthService,
    store: &Store,
    actor: AuthenticatedActor,
    channel: AccountLinkChannel,
    client_id: &str,
) -> Result<OpenLink, AuthError> {
    let possession = random_secret()?;
    let mut transaction = service.begin_link_transaction(actor).await?;
    let (source_identity_id, account_link_generation, recovery_generation) =
        service.link_source(&mut transaction, actor).await?;
    let record = service
        .open_link_transaction(
            &mut transaction,
            LinkInsert {
                link_id: Uuid::now_v7(),
                actor,
                source_identity_id,
                account_link_generation,
                recovery_generation,
                channel,
                possession: &possession,
                issuer: TEST_ISSUER,
                client_id,
                idempotency: idempotency(),
            },
        )
        .await?;
    service.commit_current(transaction).await?;
    let login_id = Uuid::now_v7();
    let poll_lease_token = random_token()?;
    match channel {
        AccountLinkChannel::Device => {
            sqlx::query(
                "INSERT INTO device_logins (login_id,device_code_digest,poll_secret_digest,issuer,client_id,audience,action,link_id,account_link_generation,recovery_generation,expires_at,poll_interval_seconds,next_poll_at,poll_lease_until,poll_lease_token) \
                 VALUES ($1,$9,$2,$3,$4,$4,'account_link',$5,$6,$7,now()+interval '1 hour',5,now(),now()+interval '1 hour',$8)",
            )
            .bind(login_id)
            .bind(service.digest_key.digest(&possession).as_slice())
            .bind(TEST_ISSUER)
            .bind(client_id)
            .bind(record.link_id)
            .bind(record.account_link_generation)
            .bind(recovery_generation)
            .bind(poll_lease_token.as_slice())
            .bind(service.digest_key.digest(&login_id.to_string()).as_slice())
            .execute(store.pool())
            .await
            .expect("seed the device provider row");
        }
        AccountLinkChannel::Browser => {
            // A browser callback consumes its row before the code exchange and
            // leaves it 'failed' until the commit succeeds.
            sqlx::query(
                "INSERT INTO browser_logins (login_id,state_digest,nonce_digest,pkce_verifier_digest,login_binding_digest,issuer,client_id,audience,redirect_uri,action,link_id,account_link_generation,recovery_generation,expires_at,consumed_at,outcome) \
                 VALUES ($1,decode(repeat('03',32),'hex'),decode(repeat('04',32),'hex'),decode(repeat('05',32),'hex'),$2,$3,$4,$4,'https://relay.example/callback','account_link',$5,$6,$7,now()+interval '1 hour',now(),'failed')",
            )
            .bind(login_id)
            .bind(service.digest_key.digest(&possession).as_slice())
            .bind(TEST_ISSUER)
            .bind(client_id)
            .bind(record.link_id)
            .bind(record.account_link_generation)
            .bind(recovery_generation)
            .execute(store.pool())
            .await
            .expect("seed the browser provider row");
        }
    }
    Ok(OpenLink {
        record,
        login_id,
        possession,
        poll_lease_token,
    })
}

/// Builds the commit coordinate a proven channel exchange would present.
fn commit_for(open: &OpenLink, recovery_generation: i64) -> LinkCommit<'_> {
    LinkCommit {
        link_id: open.record.link_id,
        login_id: open.login_id,
        channel: open.record.channel,
        recovery_generation,
        account_link_generation: open.record.account_link_generation,
        poll_lease_token: Some(&open.poll_lease_token),
    }
}

async fn link_state(store: &Store, link_id: Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM account_link_transactions WHERE link_id = $1")
        .bind(link_id)
        .fetch_one(store.pool())
        .await
        .expect("read link state")
}

async fn audited(store: &Store, action: &str, decision: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = $1 AND decision = $2")
        .bind(action)
        .bind(decision)
        .fetch_one(store.pool())
        .await
        .expect("read audit count")
}

/// Installs a trigger that fails every audit insert for one action.
async fn reject_audit(store: &Store, action: &str) {
    let name = action.replace('.', "_");
    sqlx::raw_sql(AssertSqlSafe(format!(
        "CREATE FUNCTION reject_{name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit unavailable'; END; $$; \
         CREATE TRIGGER reject_{name} BEFORE INSERT ON audit_events FOR EACH ROW \
         WHEN (NEW.action = '{action}') EXECUTE FUNCTION reject_{name}();"
    )))
    .execute(store.pool())
    .await
    .expect("install audit rejection trigger");
}

/// Seeds one active service account and authenticates its bearer credential.
async fn seed_service_actor(store: &Store, service: &AuthService) -> AuthenticatedActor {
    let principal_id = Uuid::now_v7();
    let credential_id = Uuid::now_v7();
    let team_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO principals (id,kind,state,generation) VALUES ($1,'service','active',1)",
    )
    .bind(principal_id)
    .execute(store.pool())
    .await
    .expect("seed service principal");
    sqlx::query("INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ($1,'link-team','active',1,1)")
        .bind(team_id)
        .execute(store.pool())
        .await
        .expect("seed team");
    sqlx::query("INSERT INTO service_accounts (principal_id,team_id,display_name) VALUES ($1,$2,'link-service')")
        .bind(principal_id)
        .bind(team_id)
        .execute(store.pool())
        .await
        .expect("seed service account");
    sqlx::query(
        "INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) \
         VALUES ($1,$2,$3,$4,'test','service',1,$1,1,now()+interval '1 hour')",
    )
    .bind(credential_id)
    .bind(credential_id.to_string())
    .bind(service.digest_key.digest("service-secret").as_slice())
    .bind(principal_id)
    .execute(store.pool())
    .await
    .expect("seed service credential");
    service
        .authenticate_bearer(RelayBearerCredential::new(format!(
            "{credential_id}.service-secret"
        )))
        .await
        .expect("authenticate the service credential")
}

/// Inserts one already-expired pending transaction bound to this actor.
///
/// A live transaction's expiry is immutable in the database, so an expired one
/// is materialized directly rather than by rewriting a started transaction.
async fn insert_expired_link(
    store: &Store,
    service: &AuthService,
    actor: AuthenticatedActor,
) -> OpenLink {
    let possession = random_secret().expect("possession secret");
    let link_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO account_link_transactions (link_id,principal_id,source_identity_id,channel,source_authentication_id,source_authentication_generation,issuer,client_id,audience,possession_digest,digest_key_id,state,revision,correlation_id,idempotency_key,account_link_generation,recovery_generation,created_at,expires_at) \
         SELECT $1,$2,$3,'device',$4,$5,$6,$7,$7,$8,$9,'pending',1,$10,$11,p.account_link_generation,r.recovery_generation,now()-interval '2 hours',now()-interval '1 hour' \
         FROM principals p JOIN relay_identity r ON r.state = 'normal' WHERE p.id = $2",
    )
    .bind(link_id)
    .bind(actor.actor().principal_id())
    .bind(actor.identity_id().expect("human actor has an identity"))
    .bind(actor.actor().authentication_id())
    .bind(actor.actor().authentication_generation())
    .bind(TEST_ISSUER)
    .bind(TEST_CLIENT)
    .bind(service.digest_key.digest(&possession).as_slice())
    .bind(service.digest_key.key_id())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(store.pool())
    .await
    .expect("seed an expired transaction");
    let record = sqlx::query(
        "SELECT link_id, principal_id, channel, state, source_identity_id, linked_identity_id, \
         revision, account_link_generation, created_at, expires_at, completed_at \
         FROM account_link_transactions WHERE link_id = $1",
    )
    .bind(link_id)
    .fetch_one(store.pool())
    .await
    .expect("read the expired transaction");
    OpenLink {
        record: link_record(&record).expect("safe expired record"),
        login_id: Uuid::now_v7(),
        possession,
        poll_lease_token: random_token().expect("poll lease token"),
    }
}

async fn identity_count(store: &Store, principal_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM oidc_identities WHERE principal_id = $1 AND removed_at IS NULL",
    )
    .bind(principal_id)
    .fetch_one(store.pool())
    .await
    .expect("count active identities")
}

async fn account_link_generation(store: &Store, principal_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT account_link_generation FROM principals WHERE id = $1")
        .bind(principal_id)
        .fetch_one(store.pool())
        .await
        .expect("read the account link generation")
}

/// A start must come through the channel that matches the caller's proof, and
/// an actor with no stable OIDC identity can never open a transaction.
#[tokio::test]
async fn link_start_binds_the_channel_to_the_callers_proof() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "channel-owner").await;

    assert!(matches!(
        service
            .begin_browser_link(&oidc, account.bearer, link_request())
            .await,
        Err(AuthError::LinkChannelMismatch)
    ));
    assert!(matches!(
        service
            .begin_device_link(&oidc, account.browser, link_request())
            .await,
        Err(AuthError::LinkChannelMismatch)
    ));

    let service_actor = seed_service_actor(&store, &service).await;
    assert!(matches!(
        open_link(&service, &store, service_actor, AccountLinkChannel::Device).await,
        Err(AuthError::LinkUnsupportedActor)
    ));
    let opened: i64 = sqlx::query_scalar("SELECT count(*) FROM account_link_transactions")
        .fetch_one(store.pool())
        .await
        .expect("count transactions");
    assert_eq!(opened, 0, "a refused start opens no transaction");
    cleanup(&pool, &schema).await;
}

/// One account holds at most one provable transaction, and a repeated retry
/// coordinate is refused rather than re-arming or minting a second proof.
#[tokio::test]
async fn concurrent_and_replayed_starts_cannot_mint_a_second_transaction() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "single-pending").await;

    let repeated = link_request();
    let first = service
        .begin_browser_link(&oidc, account.browser, repeated.clone())
        .await
        .expect("start the first browser link");
    assert_eq!(first.record.state, AccountLinkState::Pending);

    // The same retry coordinate names a live transaction, so a replay is
    // refused rather than arming a second one with fresh possession material.
    assert!(matches!(
        service
            .begin_browser_link(&oidc, account.browser, repeated.clone())
            .await,
        Err(AuthError::LinkPending)
    ));
    // A different retry coordinate still meets the one-pending-per-account
    // index instead of opening a second provable transaction.
    assert!(matches!(
        open_link(
            &service,
            &store,
            account.browser,
            AccountLinkChannel::Browser
        )
        .await,
        Err(AuthError::LinkPending)
    ));
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM account_link_transactions WHERE principal_id = $1 AND state = 'pending'",
    )
    .bind(account.principal_id)
    .fetch_one(store.pool())
    .await
    .expect("count pending transactions");
    assert_eq!(pending, 1);

    // A retry coordinate that reaches a closed transaction is a replay, not a
    // second start.
    service
        .cancel_account_link(
            account.browser,
            first.record.link_id,
            CancelAccountLinkRequest {
                idempotency: idempotency(),
            },
        )
        .await
        .expect("cancel the pending transaction");
    assert!(matches!(
        service
            .begin_browser_link(&oidc, account.browser, repeated.clone())
            .await,
        Err(AuthError::LinkReplayed)
    ));

    // Reusing one retry key under a different correlation id is a conflict.
    assert!(matches!(
        service
            .begin_browser_link(
                &oidc,
                account.browser,
                AccountLinkRequest {
                    idempotency: Idempotency {
                        correlation_id: Uuid::now_v7(),
                        idempotency_key: repeated.idempotency.idempotency_key,
                    },
                },
            )
            .await,
        Err(AuthError::IdempotencyConflict)
    ));
    cleanup(&pool, &schema).await;
}

/// A completion must present the exact transaction. Every wrong coordinate —
/// unknown, cross-principal, wrong channel, stale generation, cancelled and
/// expired — gets its own typed refusal and leaves no identity behind.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "Every wrong coordinate must stay visibly adjacent to the others it is distinguished from."
)]
async fn completion_rejects_every_wrong_transaction_coordinate() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let owner = seed_account(&store, &service, "completion-owner").await;
    let stranger = seed_account(&store, &service, "completion-stranger").await;

    let open = open_link(&service, &store, owner.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");

    // Unknown transaction.
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                LinkCommit {
                    link_id: Uuid::now_v7(),
                    ..commit_for(&open, 1)
                },
                &proven("unknown-transaction"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkNotFound)
    ));

    // Another account's credential cannot finish this transaction.
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&open, 1),
                &proven("cross-principal"),
                Some(stranger.bearer),
            )
            .await,
        Err(AuthError::LinkCrossPrincipal)
    ));

    // A device transaction cannot be finished through the browser channel, and
    // a browser session cannot stand in for the bearer credential that opened
    // it.
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                LinkCommit {
                    channel: AccountLinkChannel::Browser,
                    ..commit_for(&open, 1)
                },
                &proven("wrong-channel"),
                Some(owner.browser),
            )
            .await,
        Err(AuthError::LinkChannelMismatch)
    ));

    // A completion with no authenticated actor at all is refused.
    assert!(matches!(
        service
            .commit_link_once(&oidc, commit_for(&open, 1), &proven("anonymous"), None)
            .await,
        Err(AuthError::LinkChannelMismatch)
    ));

    // Stale recovery and account-link generations on the commit coordinate.
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&open, 2),
                &proven("stale-recovery"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkStale)
    ));
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                LinkCommit {
                    account_link_generation: open.record.account_link_generation + 1,
                    ..commit_for(&open, 1)
                },
                &proven("stale-generation"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkStale)
    ));

    // A proof presented without the transaction's own provider row is refused.
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                LinkCommit {
                    login_id: Uuid::now_v7(),
                    ..commit_for(&open, 1)
                },
                &proven("unbound-login"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkStale)
    ));

    // A device completion with no poll lease token is refused.
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                LinkCommit {
                    poll_lease_token: None,
                    ..commit_for(&open, 1)
                },
                &proven("no-lease"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkStale)
    ));

    assert_eq!(identity_count(&store, owner.principal_id).await, 1);
    assert_eq!(link_state(&store, open.record.link_id).await, "pending");

    // Cancelled and expired each get their own refusal.
    service
        .cancel_account_link(
            owner.bearer,
            open.record.link_id,
            CancelAccountLinkRequest {
                idempotency: idempotency(),
            },
        )
        .await
        .expect("cancel the device transaction");
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&open, 1),
                &proven("after-cancel"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkCancelled)
    ));

    let expired = insert_expired_link(&store, &service, owner.bearer).await;
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&expired, 1),
                &proven("after-expiry"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkExpired)
    ));
    assert_eq!(identity_count(&store, owner.principal_id).await, 1);
    cleanup(&pool, &schema).await;
}

/// The proof must carry the issuer, client and audience the transaction was
/// opened with; a proof from any other issuer or audience cannot complete it.
#[tokio::test]
async fn completion_rejects_a_proof_from_another_issuer_or_audience() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "issuer-bound").await;
    let open = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");

    let foreign = OidcIdentity {
        issuer: "https://other-issuer.example".to_owned(),
        subject: "foreign-subject".to_owned(),
    };
    assert!(matches!(
        service
            .commit_link_once(&oidc, commit_for(&open, 1), &foreign, Some(account.bearer))
            .await,
        Err(AuthError::LinkStale)
    ));

    // A transaction bound to another audience cannot be completed by this
    // client's proof, so an audience-confused token links nothing.
    service
        .cancel_account_link(
            account.bearer,
            open.record.link_id,
            CancelAccountLinkRequest {
                idempotency: idempotency(),
            },
        )
        .await
        .expect("close the first transaction");
    let other_audience = open_link_for_audience(
        &service,
        &store,
        account.bearer,
        AccountLinkChannel::Device,
        "other-audience",
    )
    .await
    .expect("open a transaction bound to another audience");
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&other_audience, 1),
                &proven("audience-confused"),
                Some(account.bearer),
            )
            .await,
        Err(AuthError::LinkStale)
    ));
    assert_eq!(identity_count(&store, account.principal_id).await, 1);
    cleanup(&pool, &schema).await;
}

/// A device poll needs the one-use possession secret handed to the starter; a
/// wrong secret is denied, audited, and never advances the transaction.
#[tokio::test]
async fn device_poll_requires_the_one_use_possession_secret() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "possession").await;
    let open = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");

    assert!(matches!(
        service
            .poll_device_link(
                &oidc,
                account.bearer,
                open.record.link_id,
                DevicePollSecret::new("not-the-poll-secret".to_owned()),
            )
            .await,
        Err(AuthError::CredentialInvalid)
    ));
    assert!(
        audited(&store, "auth.link.poll", "deny").await >= 1,
        "a failed possession check is durably audited"
    );
    assert_eq!(link_state(&store, open.record.link_id).await, "pending");

    // A browser session cannot poll a device transaction even with the secret.
    assert!(matches!(
        service
            .poll_device_link(
                &oidc,
                account.browser,
                open.record.link_id,
                DevicePollSecret::new(open.possession.clone()),
            )
            .await,
        Err(AuthError::LinkChannelMismatch)
    ));

    // Another account's credential cannot poll this transaction either.
    let stranger = seed_account(&store, &service, "possession-stranger").await;
    assert!(matches!(
        service
            .poll_device_link(
                &oidc,
                stranger.bearer,
                open.record.link_id,
                DevicePollSecret::new(open.possession.clone()),
            )
            .await,
        Err(AuthError::LinkCrossPrincipal)
    ));
    assert_eq!(identity_count(&store, account.principal_id).await, 1);
    cleanup(&pool, &schema).await;
}

/// `PostgreSQL` owns identity uniqueness: an identity already held by this
/// account is a self-link and one held by another account is a collision, each
/// with its own typed error, its own failed transaction, and its own audit.
#[tokio::test]
async fn proven_identity_collisions_are_refused_by_the_database() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let owner = seed_account(&store, &service, "collision-owner").await;
    let other = seed_account(&store, &service, "collision-other").await;

    let self_link = open_link(&service, &store, owner.bearer, AccountLinkChannel::Device)
        .await
        .expect("open the self-link transaction");
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&self_link, 1),
                &proven("collision-owner"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkSelf)
    ));
    assert_eq!(link_state(&store, self_link.record.link_id).await, "failed");

    let collision = open_link(&service, &store, owner.bearer, AccountLinkChannel::Device)
        .await
        .expect("open the collision transaction");
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&collision, 1),
                &proven("collision-other"),
                Some(owner.bearer),
            )
            .await,
        Err(AuthError::LinkCollision)
    ));
    assert_eq!(link_state(&store, collision.record.link_id).await, "failed");
    assert!(audited(&store, LINK_COMPLETE_ACTION, "deny").await >= 2);
    assert_eq!(identity_count(&store, owner.principal_id).await, 1);

    // The other account keeps exactly its own identity.
    let still_owned: Uuid = sqlx::query_scalar(
        "SELECT principal_id FROM oidc_identities WHERE issuer = $1 AND subject = 'collision-other' AND removed_at IS NULL",
    )
    .bind(TEST_ISSUER)
    .fetch_one(store.pool())
    .await
    .expect("read the contested identity owner");
    assert_eq!(still_owned, other.principal_id);

    // The active-coordinate uniqueness is a real index, not an application
    // check: a direct insert of the same active coordinate is refused.
    let direct = sqlx::query(
        "INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,$2,'collision-other',$3,1)",
    )
    .bind(Uuid::now_v7())
    .bind(TEST_ISSUER)
    .bind(owner.principal_id)
    .execute(store.pool())
    .await
    .expect_err("the active identity coordinate is unique");
    assert_eq!(
        direct
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some(UNIQUE_VIOLATION)
    );
    cleanup(&pool, &schema).await;
}

/// A proven link advances the account-link generation, records its audit, and
/// cannot be replayed into a second identity from the same transaction.
#[tokio::test]
async fn a_proven_link_advances_the_generation_and_cannot_be_replayed() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "generation-owner").await;
    let open = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");

    let linked = service
        .commit_link_once(
            &oidc,
            commit_for(&open, 1),
            &proven("second-identity"),
            Some(account.bearer),
        )
        .await
        .expect("commit the proven link");
    assert_eq!(linked.record.state, AccountLinkState::Completed);
    assert_eq!(linked.identity.subject, "second-identity");
    assert_eq!(linked.identity.issuer, TEST_ISSUER);
    assert!(linked.record.revision > open.record.revision);
    assert_eq!(
        linked.record.linked_identity_id,
        Some(linked.identity.identity_id)
    );

    let generation = account_link_generation(&store, account.principal_id).await;
    assert_eq!(generation, open.record.account_link_generation + 1);
    assert!(audited(&store, LINK_COMPLETE_ACTION, "changed").await >= 1);

    // The same transaction cannot produce a second identity.
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&open, 1),
                &proven("replayed-identity"),
                Some(account.bearer),
            )
            .await,
        Err(AuthError::LinkReplayed)
    ));
    assert_eq!(identity_count(&store, account.principal_id).await, 2);

    // A completed transaction is terminal in the database, so a direct rewrite
    // cannot revive it either.
    sqlx::query("UPDATE account_link_transactions SET state = 'pending', revision = revision + 1 WHERE link_id = $1")
        .bind(open.record.link_id)
        .execute(store.pool())
        .await
        .expect_err("a terminal transaction cannot be revived");

    // One completed transaction owns at most one identity, in the database.
    sqlx::query(
        "INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation,linked_via_link_id) VALUES ($1,$2,'link-once',$3,1,$4)",
    )
    .bind(Uuid::now_v7())
    .bind(TEST_ISSUER)
    .bind(account.principal_id)
    .bind(open.record.link_id)
    .execute(store.pool())
    .await
    .expect_err("one transaction links at most one identity");
    cleanup(&pool, &schema).await;
}

/// Unlink removes exactly one identity, revokes its credentials and sessions in
/// the same transaction, cancels pending links, and refuses to strand an
/// account with no way to authenticate.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "The removal, its revocations, its retry, and the relink are one reviewed effect."
)]
async fn unlink_revokes_bound_access_and_never_removes_the_last_identity() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "unlink-owner").await;
    let stranger = seed_account(&store, &service, "unlink-stranger").await;

    // The only identity an account has cannot be removed.
    assert!(matches!(
        service
            .unlink_identity(
                account.bearer,
                account.identity_id,
                UnlinkIdentityRequest {
                    idempotency: idempotency()
                },
            )
            .await,
        Err(AuthError::UnlinkLastIdentity)
    ));

    let open = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");
    let linked = service
        .commit_link_once(
            &oidc,
            commit_for(&open, 1),
            &proven("unlink-target"),
            Some(account.bearer),
        )
        .await
        .expect("link the second identity");
    let target = linked.identity.identity_id;

    // Bind a credential and a session to the linked identity so the unlink has
    // current access to close.
    let bound_credential = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO relay_credentials (credential_id,public_id,secret_digest,principal_id,identity_id,digest_key_id,credential_kind,credential_generation,rotation_family_id,recovery_generation,expires_at) \
         VALUES ($1,$2,$3,$4,$5,'test','human',1,$1,1,now()+interval '1 hour')",
    )
    .bind(bound_credential)
    .bind(bound_credential.to_string())
    .bind(service.digest_key.digest("bound-secret").as_slice())
    .bind(account.principal_id)
    .bind(target)
    .execute(store.pool())
    .await
    .expect("seed a credential on the linked identity");
    sqlx::query(
        "INSERT INTO browser_sessions (session_id,cookie_digest,csrf_digest,principal_id,identity_id,digest_key_id,session_generation,recovery_generation,expires_at,idle_deadline) \
         VALUES ($1,$2,$3,$4,$5,'test',1,1,now()+interval '1 hour',now()+interval '30 minutes')",
    )
    .bind(Uuid::now_v7())
    .bind(service.digest_key.digest("bound-cookie").as_slice())
    .bind(service.digest_key.digest("bound-csrf").as_slice())
    .bind(account.principal_id)
    .bind(target)
    .execute(store.pool())
    .await
    .expect("seed a session on the linked identity");

    // A stranger cannot remove this account's identity. The stranger holds a
    // second identity of its own so the refusal comes from ownership rather
    // than from the last-identity guard.
    sqlx::query(
        "INSERT INTO oidc_identities (identity_id,issuer,subject,principal_id,link_generation) VALUES ($1,$2,'unlink-stranger-second',$3,1)",
    )
    .bind(Uuid::now_v7())
    .bind(TEST_ISSUER)
    .bind(stranger.principal_id)
    .execute(store.pool())
    .await
    .expect("seed a second stranger identity");
    assert!(matches!(
        service
            .unlink_identity(
                stranger.bearer,
                target,
                UnlinkIdentityRequest {
                    idempotency: idempotency()
                },
            )
            .await,
        Err(AuthError::IdentityNotFound)
    ));

    let pending = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a transaction the unlink must cancel");

    let request = UnlinkIdentityRequest {
        idempotency: idempotency(),
    };
    let removed = service
        .unlink_identity(account.bearer, target, request)
        .await
        .expect("remove the linked identity");
    assert_eq!(removed.identity_id, target);
    assert_eq!(removed.revoked_credentials, 1);
    assert_eq!(removed.revoked_sessions, 1);
    assert!(audited(&store, UNLINK_ACTION, "changed").await >= 1);
    assert_eq!(
        link_state(&store, pending.record.link_id).await,
        "cancelled",
        "an unlink closes every in-flight transaction"
    );
    assert_eq!(
        removed.account_link_generation,
        account_link_generation(&store, account.principal_id).await
    );

    // The revoked credential and session are no longer current.
    assert!(matches!(
        service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{bound_credential}.bound-secret"
            )))
            .await,
        Err(AuthError::CredentialInvalid)
    ));
    assert!(matches!(
        service
            .authenticate_browser(BrowserCookie::new("bound-cookie".to_owned()))
            .await,
        Err(AuthError::CredentialInvalid)
    ));

    // An exact retry returns the committed result instead of removing again.
    let replay = service
        .unlink_identity(account.bearer, target, request)
        .await
        .expect("retry the exact unlink coordinate");
    assert_eq!(replay.identity_id, removed.identity_id);
    assert_eq!(
        replay.account_link_generation,
        removed.account_link_generation
    );
    assert_eq!(identity_count(&store, account.principal_id).await, 1);

    // A relink of the freed coordinate mints a new identity; the removed row is
    // never revived, so credentials bound to it stay dead.
    let relink = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a relink transaction");
    let relinked = service
        .commit_link_once(
            &oidc,
            commit_for(&relink, 1),
            &proven("unlink-target"),
            Some(account.bearer),
        )
        .await
        .expect("relink the freed coordinate");
    assert_ne!(
        relinked.identity.identity_id, target,
        "a relink mints a new identity rather than reviving the removed row"
    );
    assert!(matches!(
        service
            .authenticate_bearer(RelayBearerCredential::new(format!(
                "{bound_credential}.bound-secret"
            )))
            .await,
        Err(AuthError::CredentialInvalid)
    ));
    cleanup(&pool, &schema).await;
}

/// An audit outage on the unlink path fails closed: no identity is removed, no
/// authority change is acknowledged, and active access is closed.
#[tokio::test]
async fn unlink_audit_outage_fails_closed_without_an_authority_change() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "audit-outage").await;
    let open = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");
    let linked = service
        .commit_link_once(
            &oidc,
            commit_for(&open, 1),
            &proven("audit-outage-target"),
            Some(account.bearer),
        )
        .await
        .expect("link the second identity");
    let generation = account_link_generation(&store, account.principal_id).await;

    reject_audit(&store, UNLINK_ACTION).await;
    assert!(matches!(
        service
            .unlink_identity(
                account.bearer,
                linked.identity.identity_id,
                UnlinkIdentityRequest {
                    idempotency: idempotency()
                },
            )
            .await,
        Err(AuthError::Durable)
    ));

    let still_active: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT removed_at FROM oidc_identities WHERE identity_id = $1")
            .bind(linked.identity.identity_id)
            .fetch_one(store.pool())
            .await
            .expect("read the identity after the outage");
    assert!(
        still_active.is_none(),
        "a missing audit must not leave a removed identity"
    );
    assert_eq!(
        account_link_generation(&store, account.principal_id).await,
        generation,
        "no authority change is acknowledged"
    );
    assert!(
        service.authority.is_closed(),
        "a missing unlink audit closes active access"
    );
    cleanup(&pool, &schema).await;
}

/// An audit outage on the completion path fails closed: no identity is linked
/// and the account's generation does not move.
#[tokio::test]
async fn completion_audit_outage_links_no_identity() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "commit-outage").await;
    let open = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");
    let generation = account_link_generation(&store, account.principal_id).await;

    reject_audit(&store, LINK_COMPLETE_ACTION).await;
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&open, 1),
                &proven("unaudited-identity"),
                Some(account.bearer),
            )
            .await,
        Err(AuthError::Durable | AuthError::Retryable)
    ));
    assert_eq!(identity_count(&store, account.principal_id).await, 1);
    assert_eq!(
        account_link_generation(&store, account.principal_id).await,
        generation,
        "an unaudited completion changes no authority"
    );
    assert_eq!(link_state(&store, open.record.link_id).await, "pending");
    cleanup(&pool, &schema).await;
}

/// Restore quarantine and a stale source credential both stop a link before it
/// can touch durable authority.
#[tokio::test]
async fn quarantine_and_stale_sources_cannot_start_or_finish_a_link() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "quarantine-owner").await;
    let open = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");

    // Rotating the source credential's generation makes the transaction's
    // recorded authentication binding stale.
    sqlx::query(
        "UPDATE relay_credentials SET credential_generation = credential_generation + 1 WHERE credential_id = $1",
    )
    .bind(account.credential_id)
    .execute(store.pool())
    .await
    .expect("rotate the source credential");
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&open, 1),
                &proven("stale-source"),
                Some(account.bearer),
            )
            .await,
        Err(AuthError::LinkStale)
    ));
    assert_eq!(link_state(&store, open.record.link_id).await, "pending");

    // Removing the source identity also stops the transaction that named it.
    sqlx::query(
        "UPDATE relay_credentials SET credential_generation = credential_generation - 1 WHERE credential_id = $1",
    )
    .bind(account.credential_id)
    .execute(store.pool())
    .await
    .expect("restore the source credential generation");
    sqlx::query("UPDATE oidc_identities SET removed_at = now() WHERE identity_id = $1")
        .bind(account.identity_id)
        .execute(store.pool())
        .await
        .expect("remove the source identity");
    assert!(matches!(
        open_link(&service, &store, account.bearer, AccountLinkChannel::Device).await,
        Err(AuthError::LinkStale)
    ));
    sqlx::query("UPDATE oidc_identities SET removed_at = NULL WHERE identity_id = $1")
        .bind(account.identity_id)
        .execute(store.pool())
        .await
        .expect("restore the source identity");

    // A relay whose recovery generation has moved on refuses every link action.
    sqlx::query(
        "UPDATE relay_identity SET recovery_generation = recovery_generation + 1 WHERE relay_id = 'test-relay'",
    )
    .execute(store.pool())
    .await
    .expect("advance the recovery generation");
    assert!(matches!(
        service
            .begin_browser_link(&oidc, account.browser, link_request())
            .await,
        Err(AuthError::LinkQuarantined)
    ));
    assert!(matches!(
        service
            .commit_link_once(
                &oidc,
                commit_for(&open, 1),
                &proven("quarantined"),
                Some(account.bearer),
            )
            .await,
        Err(AuthError::LinkQuarantined)
    ));
    assert!(matches!(
        service
            .unlink_identity(
                account.bearer,
                account.identity_id,
                UnlinkIdentityRequest {
                    idempotency: idempotency()
                },
            )
            .await,
        Err(AuthError::LinkQuarantined)
    ));
    assert_eq!(identity_count(&store, account.principal_id).await, 1);
    cleanup(&pool, &schema).await;
}

/// The status page is bounded and shows only the calling account's history.
#[tokio::test]
async fn the_status_page_is_bounded_and_scoped_to_one_account() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let account = seed_account(&store, &service, "page-owner").await;
    let stranger = seed_account(&store, &service, "page-stranger").await;

    let mine = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open the caller's transaction");
    let theirs = open_link(
        &service,
        &store,
        stranger.bearer,
        AccountLinkChannel::Device,
    )
    .await
    .expect("open the stranger's transaction");

    let page = service
        .list_account_links(
            account.bearer,
            PageRequest {
                after: None,
                limit: MAX_LINK_PAGE,
            },
        )
        .await
        .expect("list the caller's transactions");
    assert!(page
        .records
        .iter()
        .any(|link| link.link_id == mine.record.link_id));
    assert!(
        !page
            .records
            .iter()
            .any(|link| link.link_id == theirs.record.link_id),
        "the page never leaks another account's transaction"
    );

    assert!(matches!(
        service
            .list_account_links(
                account.bearer,
                PageRequest {
                    after: None,
                    limit: MAX_LINK_PAGE + 1,
                },
            )
            .await,
        Err(AuthError::Malformed)
    ));
    cleanup(&pool, &schema).await;
}

/// No start value, transaction row or page ever renders its one-use secret.
#[tokio::test]
async fn link_start_values_never_render_their_possession_secret() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "redaction").await;

    let browser = service
        .begin_browser_link(&oidc, account.browser, link_request())
        .await
        .expect("start a browser link");
    let binding = browser.binding().to_owned();
    assert!(!binding.is_empty());
    let rendered = format!("{browser:?}");
    assert!(
        !rendered.contains(&binding),
        "a browser start redacts its binding"
    );
    assert!(
        !rendered.contains(browser.authorization_url.as_str()),
        "a browser start does not render its state and nonce query"
    );

    let device = DeviceLinkStart {
        record: browser.record,
        authorization: OidcDeviceAuthorization {
            verification_uri: url::Url::parse("https://issuer.example/device")
                .expect("valid fixture URL"),
            verification_uri_complete: None,
            user_code: "sentinel-user-code".to_owned(),
            expires_in: Duration::from_mins(1),
            interval: Duration::from_secs(1),
            device_code: Zeroizing::new("sentinel-device-code".to_owned()),
            verifier: Zeroizing::new("sentinel-verifier".to_owned()),
        },
        poll_secret: SecretValue::new("sentinel-poll-secret".to_owned()),
    };
    let rendered = format!("{device:?}");
    for secret in [
        "sentinel-user-code",
        "sentinel-device-code",
        "sentinel-verifier",
        "sentinel-poll-secret",
    ] {
        assert!(
            !rendered.contains(secret),
            "a device start redacts {secret}"
        );
    }

    // The durable row stores only a keyed digest of the possession value.
    let stored: Vec<u8> = sqlx::query_scalar(
        "SELECT possession_digest FROM account_link_transactions WHERE link_id = $1",
    )
    .bind(device.record.link_id)
    .fetch_one(store.pool())
    .await
    .expect("read the possession digest");
    assert_eq!(stored.len(), 32);
    assert_ne!(stored.as_slice(), binding.as_bytes());

    // A safe page carries no possession material at all.
    let page = service
        .list_account_links(
            account.browser,
            PageRequest {
                after: None,
                limit: MAX_LINK_PAGE,
            },
        )
        .await
        .expect("list transactions");
    let serialized = serde_json::to_string(&page).expect("serialize the page");
    assert!(
        !serialized.contains(binding.as_str()),
        "a safe page never carries possession material"
    );
    cleanup(&pool, &schema).await;
}

/// Cancelling is idempotent, closes the transaction's provider row, and cannot
/// reach another account's transaction.
#[tokio::test]
async fn cancel_is_idempotent_closes_the_provider_row_and_is_account_scoped() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let account = seed_account(&store, &service, "cancel-owner").await;
    let stranger = seed_account(&store, &service, "cancel-stranger").await;
    let open = open_link(&service, &store, account.bearer, AccountLinkChannel::Device)
        .await
        .expect("open a device transaction");

    assert!(matches!(
        service
            .cancel_account_link(
                stranger.bearer,
                open.record.link_id,
                CancelAccountLinkRequest {
                    idempotency: idempotency()
                },
            )
            .await,
        // Cancel is scoped by principal, so a stranger is told the transaction
        // does not exist rather than that it belongs to someone else.
        Err(AuthError::LinkNotFound)
    ));

    let cancelled = service
        .cancel_account_link(
            account.bearer,
            open.record.link_id,
            CancelAccountLinkRequest {
                idempotency: idempotency(),
            },
        )
        .await
        .expect("cancel the transaction");
    assert_eq!(cancelled.state, AccountLinkState::Cancelled);
    let open_logins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM device_logins WHERE link_id = $1 AND consumed_at IS NULL",
    )
    .bind(open.record.link_id)
    .fetch_one(store.pool())
    .await
    .expect("count unconsumed provider rows");
    assert_eq!(open_logins, 0, "cancelling closes the provider row");

    let repeat = service
        .cancel_account_link(
            account.bearer,
            open.record.link_id,
            CancelAccountLinkRequest {
                idempotency: idempotency(),
            },
        )
        .await
        .expect("a retry after a lost response is safe");
    assert_eq!(repeat.state, AccountLinkState::Cancelled);
    assert_eq!(repeat.revision, cancelled.revision);
    assert!(audited(&store, LINK_CANCEL_ACTION, "changed").await >= 1);
    cleanup(&pool, &schema).await;
}

/// Every start is bounded by the configured login lifetime, not an inline value.
#[tokio::test]
async fn link_transactions_expire_on_the_configured_bound() {
    let (store, schema, pool) = fixture().await;
    let (authority, _directory) = authority(store.clone()).await;
    let service = service(&store, authority);
    let oidc = OidcClient::test_client();
    let account = seed_account(&store, &service, "bounded").await;
    let link = service
        .begin_browser_link(&oidc, account.browser, link_request())
        .await
        .expect("start a browser link")
        .record;

    let row = sqlx::query(
        "SELECT expires_at - created_at AS window FROM account_link_transactions WHERE link_id = $1",
    )
    .bind(link.link_id)
    .fetch_one(store.pool())
    .await
    .expect("read the configured window");
    let span: sqlx::postgres::types::PgInterval = row.get("window");
    let window = Duration::from_micros(u64::try_from(span.microseconds).expect("positive window"));
    let configured = limits().login_lifetime;
    // `created_at` and `expires_at` each read `clock_timestamp()`, which
    // advances inside the statement, so the stored window trails the configured
    // one by the statement's own execution time.
    let trailing = configured
        .checked_sub(window)
        .expect("the stored window never exceeds the configured one");
    assert!(
        trailing < Duration::from_secs(1),
        "the stored window {window:?} must be the configured {configured:?}"
    );
    cleanup(&pool, &schema).await;
}
