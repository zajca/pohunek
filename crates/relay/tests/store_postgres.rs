//! Runs real `PostgreSQL` authority invariants when explicitly selected.

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

// PostgreSQL error classes asserted by the admission/evidence invariants below.
const CHECK_VIOLATION: &str = "23514";

/// Creates one fully migrated, isolated schema with two teams, one human
/// principal, and one relay identity so invariant tests only seed rule rows.
async fn invariant_fixture() -> (sqlx::PgPool, sqlx::PgPool, String, Uuid, Uuid, Uuid) {
    let url = env::var("POHUNEK_RELAY_TEST_DATABASE_URL")
        .expect("postgres-tests requires POHUNEK_RELAY_TEST_DATABASE_URL");
    let bootstrap = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect dedicated PostgreSQL fixture");
    let schema = format!("relay_invariants_{}", Uuid::now_v7().simple());
    sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&bootstrap)
        .await
        .expect("create isolated PostgreSQL schema");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!("{url}?options[search_path]={schema}"))
        .await
        .expect("connect isolated PostgreSQL fixture");
    let store = Store::connect(&format!("{url}?options[search_path]={schema}"), 4)
        .await
        .expect("connect isolated PostgreSQL fixture");
    store
        .migrate()
        .await
        .expect("apply embedded relay migrations");
    drop(store);
    let team = Uuid::now_v7();
    let other_team = Uuid::now_v7();
    for id in [team, other_team] {
        exec_ok(
            &pool,
            &format!(
                "INSERT INTO teams (team_id,display_name,state,policy_generation,revision) VALUES ('{id}','invariant team','active',1,1)"
            ),
        )
        .await;
    }
    let principal = Uuid::now_v7();
    exec_ok(
        &pool,
        &format!(
            "INSERT INTO principals (id,kind,state,generation) VALUES ('{principal}','human','active',1)"
        ),
    )
    .await;
    exec_ok(
        &pool,
        "INSERT INTO relay_identity (relay_id,recovery_generation,state,revision) VALUES ('relay-invariant',1,'normal',1)",
    )
    .await;
    (pool, bootstrap, schema, team, other_team, principal)
}

async fn exec_ok(pool: &sqlx::PgPool, statement: &str) {
    sqlx::query(AssertSqlSafe(statement.to_owned()))
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("statement must succeed: {statement}: {error}"));
}

/// Asserts one statement fails with exactly the expected PostgreSQL error class.
async fn exec_code(pool: &sqlx::PgPool, statement: &str, expected: &str, context: &str) {
    match sqlx::query(AssertSqlSafe(statement.to_owned()))
        .execute(pool)
        .await
    {
        Ok(_) => panic!("{context}: statement must fail: {statement}"),
        Err(error) => {
            let database = error
                .as_database_error()
                .unwrap_or_else(|| panic!("{context}: expected database error, got {error}"));
            assert_eq!(
                database.code().as_deref(),
                Some(expected),
                "{context}: unexpected PostgreSQL error: {error}"
            );
        }
    }
}

async fn drop_schema(pool: &sqlx::PgPool, schema: &str) {
    sqlx::query(AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(pool)
        .await
        .expect("remove isolated PostgreSQL schema");
}

/// Seeds one GitHub rule version row and returns nothing.
async fn seed_version(pool: &sqlx::PgPool, rule: Uuid, team: Uuid, revision: i64, state: &str) {
    exec_ok(
        pool,
        &format!(
            "INSERT INTO admission_rule_versions (admission_rule_id,revision,team_id,provider,method,match_value,state) VALUES ('{rule}',{revision},'{team}','github','github_organization','example-org','{state}')"
        ),
    )
    .await;
}

/// Seeds the live projection mirroring revision 1 in `active` state.
async fn seed_projection(pool: &sqlx::PgPool, rule: Uuid, team: Uuid) {
    exec_ok(
        pool,
        &format!(
            "INSERT INTO admission_rules (admission_rule_id,team_id,provider,method,match_value,state,current_revision) VALUES ('{rule}','{team}','github','github_organization','example-org','active',1)"
        ),
    )
    .await;
}

/// Seeds one challenge bound to the given frozen rule revision.
async fn seed_challenge(
    pool: &sqlx::PgPool,
    challenge: Uuid,
    principal: Uuid,
    team: Uuid,
    rule: Uuid,
    revision: i64,
) {
    exec_ok(
        pool,
        &challenge_statement(challenge, principal, team, rule, revision),
    )
    .await;
}

fn challenge_statement(
    challenge: Uuid,
    principal: Uuid,
    team: Uuid,
    rule: Uuid,
    revision: i64,
) -> String {
    challenge_statement_with_audience(
        challenge,
        principal,
        team,
        rule,
        revision,
        "relay-invariant",
    )
}

fn challenge_statement_with_audience(
    challenge: Uuid,
    principal: Uuid,
    team: Uuid,
    rule: Uuid,
    revision: i64,
    audience: &str,
) -> String {
    format!(
        "INSERT INTO evidence_challenges (challenge_id,relay_id,principal_id,audience,nonce_digest,team_id,admission_rule_id,admission_rule_revision,transaction_id,issuer,keycloak_subject,provider,expected_provider_subject,method,account_link_generation,provider_identity_generation,recovery_generation,checked_monotonic_epoch,binding_transaction_digest,expires_at) VALUES ('{challenge}','relay-invariant','{principal}','{audience}',decode(repeat('ab',32),'hex'),'{team}','{rule}',{revision},'{challenge}','https://issuer.test','broker-subject','github','example-user','github_organization',1,1,1,'{challenge}','binding-digest',clock_timestamp()+interval '10 minutes')"
    )
}

async fn invalidate_challenge(pool: &sqlx::PgPool, challenge: Uuid) {
    exec_ok(
        pool,
        &format!(
            "UPDATE evidence_challenges SET consumed_at=clock_timestamp(),consumption_state='invalidated' WHERE challenge_id='{challenge}'"
        ),
    )
    .await;
}

#[tokio::test]
async fn admission_rule_versions_are_frozen_and_revision_monotonic() {
    let (pool, bootstrap, schema, team, _other_team, _principal) = invariant_fixture().await;
    let rule = Uuid::now_v7();
    seed_version(&pool, rule, team, 1, "active").await;
    for (update, context) in [
        (
            format!("UPDATE admission_rule_versions SET match_value='changed' WHERE admission_rule_id='{rule}' AND revision=1"),
            "match_value rewrite must be rejected",
        ),
        (
            format!("UPDATE admission_rule_versions SET state='disabled' WHERE admission_rule_id='{rule}' AND revision=1"),
            "state rewrite must be rejected",
        ),
        (
            format!("DELETE FROM admission_rule_versions WHERE admission_rule_id='{rule}' AND revision=1"),
            "revision delete must be rejected",
        ),
    ] {
        exec_code(&pool, &update, CHECK_VIOLATION, context).await;
    }
    seed_version(&pool, rule, team, 2, "disabled").await;
    exec_code(
        &pool,
        &format!(
            "INSERT INTO admission_rule_versions (admission_rule_id,revision,team_id,provider,method,match_value,state) VALUES ('{rule}',1,'{team}','github','github_organization','example-org','active')"
        ),
        CHECK_VIOLATION,
        "stale revision reinsert must be rejected",
    )
    .await;
    let versions: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT count(*) FROM admission_rule_versions WHERE admission_rule_id='{rule}'"
    )))
    .fetch_one(&pool)
    .await
    .expect("count versions");
    assert_eq!(versions, 2, "exactly the monotonic revisions exist");
    drop_schema(&bootstrap, &schema).await;
}

#[tokio::test]
async fn admission_rule_projection_must_mirror_its_frozen_revision() {
    let (pool, bootstrap, schema, team, other_team, _principal) = invariant_fixture().await;
    let rule = Uuid::now_v7();
    seed_version(&pool, rule, team, 1, "active").await;
    for (projection, context) in [
        (
            format!("INSERT INTO admission_rules (admission_rule_id,team_id,provider,method,match_value,state,current_revision) VALUES ('{rule}','{team}','github','github_organization','other-org','active',1)"),
            "identity mismatch must be rejected",
        ),
        (
            format!("INSERT INTO admission_rules (admission_rule_id,team_id,provider,method,match_value,state,current_revision) VALUES ('{rule}','{team}','github','github_organization','example-org','disabled',1)"),
            "state mismatch must be rejected",
        ),
        (
            format!("INSERT INTO admission_rules (admission_rule_id,team_id,provider,method,match_value,state,current_revision) VALUES ('{rule}','{other_team}','github','github_organization','example-org','active',1)"),
            "team mismatch must be rejected",
        ),
    ] {
        exec_code(&pool, &projection, CHECK_VIOLATION, context).await;
    }
    seed_projection(&pool, rule, team).await;
    exec_code(
        &pool,
        &format!("UPDATE admission_rules SET state='disabled' WHERE admission_rule_id='{rule}'"),
        CHECK_VIOLATION,
        "in-place state change must require a new revision",
    )
    .await;
    exec_code(
        &pool,
        &format!("UPDATE admission_rules SET current_revision=current_revision-1 WHERE admission_rule_id='{rule}'"),
        CHECK_VIOLATION,
        "projection revision must never move backwards",
    )
    .await;
    exec_code(
        &pool,
        &format!(
            "UPDATE admission_rules SET team_id='{other_team}' WHERE admission_rule_id='{rule}'"
        ),
        CHECK_VIOLATION,
        "projection identity rewrite must be rejected",
    )
    .await;
    seed_version(&pool, rule, team, 2, "disabled").await;
    exec_ok(
        &pool,
        &format!("UPDATE admission_rules SET current_revision=2,state='disabled' WHERE admission_rule_id='{rule}'"),
    )
    .await;
    drop_schema(&bootstrap, &schema).await;
}

#[tokio::test]
async fn evidence_challenges_bind_rule_team_and_frozen_revision() {
    let (pool, bootstrap, schema, team, other_team, principal) = invariant_fixture().await;
    let rule = Uuid::now_v7();
    seed_version(&pool, rule, team, 1, "active").await;
    seed_projection(&pool, rule, team).await;
    seed_challenge(&pool, Uuid::now_v7(), principal, team, rule, 1).await;
    exec_code(
        &pool,
        &challenge_statement_with_audience(Uuid::now_v7(), principal, team, rule, 1, "other-relay"),
        CHECK_VIOLATION,
        "challenge audience must match its relay",
    )
    .await;
    seed_version(&pool, rule, team, 2, "disabled").await;
    exec_code(
        &pool,
        &challenge_statement(Uuid::now_v7(), principal, team, rule, 2),
        CHECK_VIOLATION,
        "challenge must reference the current rule revision",
    )
    .await;
    exec_ok(
        &pool,
        &format!("UPDATE admission_rules SET current_revision=2,state='disabled' WHERE admission_rule_id='{rule}'"),
    )
    .await;
    exec_code(
        &pool,
        &challenge_statement(Uuid::now_v7(), principal, team, rule, 2),
        CHECK_VIOLATION,
        "disabled rule must not issue a challenge",
    )
    .await;
    let other_rule = Uuid::now_v7();
    seed_version(&pool, other_rule, team, 1, "active").await;
    seed_projection(&pool, other_rule, team).await;
    seed_version(&pool, other_rule, other_team, 2, "disabled").await;
    exec_code(
        &pool,
        &challenge_statement(Uuid::now_v7(), principal, other_team, other_rule, 1),
        CHECK_VIOLATION,
        "challenge for another team must not reference this rule",
    )
    .await;
    exec_code(
        &pool,
        &challenge_statement(Uuid::now_v7(), principal, team, other_rule, 2),
        CHECK_VIOLATION,
        "challenge must not mix its team with another team's frozen revision",
    )
    .await;
    drop_schema(&bootstrap, &schema).await;
}

/// One evidence-result insert with overridable coordinates. `transaction: None`
/// models a writer that drops a frozen claims value, which the table must
/// reject; the default instance describes a matching, committable attestation.
struct EvidenceInsert {
    evidence_id: Uuid,
    challenge: Uuid,
    principal: Uuid,
    team: Uuid,
    rule: Uuid,
    revision: i64,
    provider: &'static str,
    provider_subject: &'static str,
    method: &'static str,
    audience: &'static str,
    provider_identity_generation: i64,
    account_link_generation: i64,
    recovery_generation: i64,
    binding_transaction_digest: &'static str,
    checked: &'static str,
    expires: &'static str,
    transaction: Option<Uuid>,
}

impl EvidenceInsert {
    fn matching(challenge: Uuid, principal: Uuid, team: Uuid, rule: Uuid) -> Self {
        Self {
            evidence_id: Uuid::now_v7(),
            challenge,
            principal,
            team,
            rule,
            revision: 1,
            provider: "github",
            provider_subject: "example-user",
            method: "github_organization",
            audience: "relay-invariant",
            provider_identity_generation: 1,
            account_link_generation: 1,
            recovery_generation: 1,
            binding_transaction_digest: "binding-digest",
            checked: "clock_timestamp()",
            expires: "clock_timestamp()+interval '29 minutes'",
            transaction: Some(challenge),
        }
    }

    fn statement(&self) -> String {
        let (column, value) = self.transaction.map_or_else(
            || (String::new(), String::new()),
            |id| (", transaction_id".to_owned(), format!(", '{id}'")),
        );
        format!(
            "INSERT INTO evidence_results (evidence_id,challenge_id,principal_id,team_id,admission_rule_id,admission_rule_revision,provider,provider_subject,outcome,method,checked_at,expires_at,signing_key_id,evidence_version,audience,issuer,keycloak_subject,provider_identity_generation,account_link_generation,recovery_generation,checked_monotonic_epoch,binding_transaction_digest,upstream_exchange_digest{column}) VALUES ('{}','{}','{}','{}','{}',{},'{}','{}','eligible','{}',{},{},'signing-key-1',1,'{}','https://issuer.test','broker-subject',{},{},{},'{}','{}','upstream-digest'{value})",
            self.evidence_id,
            self.challenge,
            self.principal,
            self.team,
            self.rule,
            self.revision,
            self.provider,
            self.provider_subject,
            self.method,
            self.checked,
            self.expires,
            self.audience,
            self.provider_identity_generation,
            self.account_link_generation,
            self.recovery_generation,
            self.challenge,
            self.binding_transaction_digest,
        )
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "One database test exercises the full one-use evidence binding and immutability contract"
)]
async fn evidence_results_bind_challenge_coordinates_and_stay_append_only() {
    let (pool, bootstrap, schema, team, other_team, principal) = invariant_fixture().await;
    let rule = Uuid::now_v7();
    seed_version(&pool, rule, team, 1, "active").await;
    seed_projection(&pool, rule, team).await;
    let challenge = Uuid::now_v7();
    seed_challenge(&pool, challenge, principal, team, rule, 1).await;
    let base = || EvidenceInsert::matching(challenge, principal, team, rule);
    for (insert, expected, context) in [
        (
            EvidenceInsert {
                team: other_team,
                ..base()
            },
            CHECK_VIOLATION,
            "team mismatch must be rejected",
        ),
        (
            EvidenceInsert {
                provider: "google",
                ..base()
            },
            CHECK_VIOLATION,
            "provider/method mismatch must be rejected",
        ),
        (
            EvidenceInsert {
                provider_subject: "other-user",
                ..base()
            },
            CHECK_VIOLATION,
            "provider-subject mismatch must be rejected",
        ),
        (
            EvidenceInsert {
                account_link_generation: 2,
                ..base()
            },
            CHECK_VIOLATION,
            "account-link generation mismatch must be rejected",
        ),
        (
            EvidenceInsert {
                binding_transaction_digest: "other-binding",
                ..base()
            },
            CHECK_VIOLATION,
            "binding transaction mismatch must be rejected",
        ),
        (
            EvidenceInsert {
                checked: "clock_timestamp()+interval '1 year'",
                expires: "clock_timestamp()+interval '1 year 29 minutes'",
                ..base()
            },
            CHECK_VIOLATION,
            "future checked-at must not extend the evidence validity window",
        ),
        (
            EvidenceInsert {
                expires: "clock_timestamp()+interval '61 minutes'",
                ..base()
            },
            CHECK_VIOLATION,
            "evidence lifetime ceiling must be enforced",
        ),
        (
            EvidenceInsert {
                transaction: Some(Uuid::now_v7()),
                ..base()
            },
            CHECK_VIOLATION,
            "transaction mismatch must be rejected",
        ),
        (
            EvidenceInsert {
                transaction: None,
                ..base()
            },
            CHECK_VIOLATION,
            "dropping a frozen claims coordinate must be rejected",
        ),
    ] {
        exec_code(&pool, &insert.statement(), expected, context).await;
    }
    exec_code(
        &pool,
        &format!("DELETE FROM evidence_challenges WHERE challenge_id='{challenge}'"),
        CHECK_VIOLATION,
        "issued challenge must be append-only",
    )
    .await;
    exec_ok(&pool, &base().statement()).await;
    let consumption_state: String = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT consumption_state FROM evidence_challenges WHERE challenge_id='{challenge}'"
    )))
    .fetch_one(&pool)
    .await
    .expect("read atomically claimed challenge");
    assert_eq!(consumption_state, "evidence");
    for (statement, context) in [
        (
            format!(
                "UPDATE evidence_results SET outcome='ineligible' WHERE challenge_id='{challenge}'"
            ),
            "committed evidence must be append-only",
        ),
        (
            format!("DELETE FROM evidence_results WHERE challenge_id='{challenge}'"),
            "committed evidence must never be deleted",
        ),
        (
            format!(
                "UPDATE evidence_challenges SET audience='other-relay' WHERE challenge_id='{challenge}'"
            ),
            "committed challenge binding must stay immutable",
        ),
    ] {
        exec_code(&pool, &statement, CHECK_VIOLATION, context).await;
    }
    let stale_challenge = Uuid::now_v7();
    seed_challenge(&pool, stale_challenge, principal, team, rule, 1).await;
    let recovery_rule = Uuid::now_v7();
    seed_version(&pool, recovery_rule, other_team, 1, "active").await;
    seed_projection(&pool, recovery_rule, other_team).await;
    let recovery_challenge = Uuid::now_v7();
    seed_challenge(
        &pool,
        recovery_challenge,
        principal,
        other_team,
        recovery_rule,
        1,
    )
    .await;
    seed_version(&pool, rule, team, 2, "disabled").await;
    exec_ok(
        &pool,
        &format!("UPDATE admission_rules SET current_revision=2,state='disabled' WHERE admission_rule_id='{rule}'"),
    )
    .await;
    exec_code(
        &pool,
        &EvidenceInsert::matching(stale_challenge, principal, team, rule).statement(),
        CHECK_VIOLATION,
        "rule disable must invalidate an outstanding challenge",
    )
    .await;
    exec_ok(
        &pool,
        "UPDATE relay_identity SET recovery_generation=2,revision=revision+1 WHERE relay_id='relay-invariant'",
    )
    .await;
    invalidate_challenge(&pool, recovery_challenge).await;
    exec_code(
        &pool,
        &EvidenceInsert::matching(recovery_challenge, principal, other_team, recovery_rule)
            .statement(),
        CHECK_VIOLATION,
        "recovery-invalidated challenge must not accept evidence",
    )
    .await;
    exec_code(
        &pool,
        &challenge_statement(Uuid::now_v7(), principal, other_team, recovery_rule, 1),
        CHECK_VIOLATION,
        "old recovery generation must not issue a challenge",
    )
    .await;
    drop_schema(&bootstrap, &schema).await;
}
