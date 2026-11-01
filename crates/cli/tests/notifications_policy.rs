//! Notification policy and source ranking against a real daemon.
//!
//! A `ControlServer` with a durable notification inbox and the attention
//! debounce coordinator is served from this test process, as the daemon binary
//! wires them. Policy is read and changed through the `pohunek` binary, and
//! records are created through the public `notification.create` method, so each
//! outcome is what an operator or a hook reporter observes.

// Rust guideline compliant 2026-10-12

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;

use package::{build_archive, read_archive, ArchiveEntry, Limits};
use pohunek_client::Client;
use protocol::{
    NotificationCreateParams, NotificationCreateResult, NotificationKind, NotificationPolicyParams,
    NotificationSeverity, NotificationSource, ProtocolError, SessionId,
};
use serde_json::{json, Value};

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, stderr_text, Harness};

/// A session id the daemon does not hold, for reports of producers outside
/// every hook schema.
const SESSION: &str = "s-01J00000000000000000000003";

/// Producer name outside every hook schema: a user or external source.
const PRODUCER: &str = "policy-scenario";

/// A provider every shipped hook schema lists as an identity provider.
const HOOK_PROVIDER: &str = "hermes";

/// A runtime id that no hook schema lists and the CLI has no keyword for.
const RUNTIME: &str = "pi";

/// The documented default per-kind flags (`docs/public-api.md`,
/// `NotificationPolicy`): attention and errors on, everything else off.
fn documented_default_kinds() -> Value {
    json!({
        "agent_blocked": true,
        "approval_required": true,
        "turn_completed": false,
        "session_finished": false,
        "error": true,
        "system": false,
    })
}

/// Creates one record for `session` through the public `notification.create`
/// method.
async fn create(
    harness: &Harness,
    session: &str,
    provider: &str,
    kind: NotificationKind,
    dedupe_key: Option<String>,
) -> Result<NotificationCreateResult, ProtocolError> {
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    client
        .create_notification(NotificationCreateParams {
            source: NotificationSource {
                provider: provider.to_owned(),
                provider_event: "scenario".to_owned(),
                host_local_source_id: format!("{provider}-{session}"),
            },
            kind,
            severity: NotificationSeverity::ActionRequired,
            title: format!("{provider} report"),
            body: String::new(),
            metadata: BTreeMap::new(),
            session_id: Some(SessionId(session.to_owned())),
            agent_kind: None,
            source_id: None,
            dedupe_key,
            project_id: None,
            retryable_admission: false,
        })
        .await
        .map_err(|error| match error {
            pohunek_client::ClientError::Protocol(error) => error,
            other => panic!("transport failure creating a notification: {other}"),
        })
}

/// `pohunek notifications policy get --json`, as the policy object.
async fn policy_get(harness: &Harness) -> Value {
    let (code, result) = harness.json(&["notifications", "policy", "get"]).await;
    assert_eq!(code, 0, "{result}");
    let policy = result["ok"]["policy"].clone();
    assert!(policy.is_object(), "a policy object: {result}");
    policy
}

/// The persisted records of `session`, as `pohunek notifications list` prints them.
async fn session_records(harness: &Harness, session: &str) -> Vec<Value> {
    let (code, listed) = harness
        .json(&["notifications", "list", "--session", session])
        .await;
    assert_eq!(code, 0, "{listed}");
    listed["ok"]["notifications"]
        .as_array()
        .expect("notification records")
        .clone()
}

/// A fresh daemon has no per-provider overrides: every provider, built-in or
/// installed, is governed by the base flags.
#[tokio::test]
async fn default_policy_has_no_provider_overrides() {
    let harness = Harness::start_with_debounced_notifications().await;

    let policy = policy_get(&harness).await;
    // An empty override map is omitted on the wire.
    assert!(
        policy
            .get("providers")
            .is_none_or(|providers| providers.as_object().is_some_and(serde_json::Map::is_empty)),
        "no provider overrides: {policy}"
    );
    assert_eq!(policy["enabled"], documented_default_kinds(), "{policy}");
    assert_eq!(policy["attention_dedupe_window_secs"], 120, "{policy}");
    assert_eq!(policy["attention_debounce_secs"], 5, "{policy}");

    harness.stop().await;
}

/// `policy set --provider <runtime id>` accepts a runtime the CLI has no
/// keyword for, seeds its override from the base flags, and the daemon applies
/// it to that provider's creates only. A value that is not a runtime id is
/// still refused.
#[tokio::test]
async fn policy_set_overrides_any_runtime_id() {
    let harness = Harness::start_with_debounced_notifications().await;

    let output = harness
        .run(&[
            "notifications",
            "policy",
            "set",
            "--provider",
            RUNTIME,
            "--kind",
            "turn_completed",
            "--enabled",
        ])
        .await;
    assert!(output.status.success(), "{}", stderr_text(&output));

    let policy = policy_get(&harness).await;
    let mut expected = documented_default_kinds();
    expected["turn_completed"] = json!(true);
    assert_eq!(
        policy["providers"],
        json!({ RUNTIME: expected }),
        "{policy}"
    );
    assert_eq!(policy["enabled"], documented_default_kinds(), "{policy}");

    let created = create(
        &harness,
        SESSION,
        RUNTIME,
        NotificationKind::TurnCompleted,
        None,
    )
    .await
    .expect("the override enables turn_completed for the runtime");
    assert!(created.created, "{created:?}");
    let refused = create(
        &harness,
        SESSION,
        PRODUCER,
        NotificationKind::TurnCompleted,
        None,
    )
    .await
    .expect_err("the base flags still disable turn_completed");
    assert_eq!(refused.code, "notification_kind_disabled", "{refused:?}");

    let invalid = harness
        .run(&[
            "notifications",
            "policy",
            "set",
            "--provider",
            "Not A Runtime",
            "--kind",
            "turn_completed",
            "--enabled",
        ])
        .await;
    assert!(!invalid.status.success(), "an invalid provider is refused");
    assert!(
        stderr_text(&invalid).contains("invalid notification policy provider"),
        "{}",
        stderr_text(&invalid)
    );

    harness.stop().await;
}

/// `notification.policy.set` answers with the policy it stored: a kind the
/// params leave out carries the daemon's default, so the result and a later
/// `notification.policy.get` both hold every known kind.
#[tokio::test]
async fn policy_set_returns_the_completed_policy() {
    let harness = Harness::start_with_debounced_notifications().await;
    let params: NotificationPolicyParams = serde_json::from_value(json!({
        "policy": {
            "attention_dedupe_window_secs": 300,
            "enabled": {"system": true},
        }
    }))
    .expect("the daemon decodes params that leave kinds out");
    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");

    let result = client
        .set_notification_policy(params)
        .await
        .expect("set the policy");

    for kind in NotificationKind::KNOWN {
        assert!(
            result.policy.enabled.get(kind).is_some(),
            "the result holds {}: {result:?}",
            kind.as_str()
        );
    }
    let mut expected = documented_default_kinds();
    expected["system"] = json!(true);
    let returned = serde_json::to_value(&result.policy).expect("encode the result");
    assert_eq!(returned["enabled"], expected, "{returned}");
    let policy = policy_get(&harness).await;
    assert_eq!(policy["enabled"], expected, "{policy}");

    harness.stop().await;
}

/// A hook provider's admitted attention report outranks a user report for the
/// same session moment: while both are pending inside the debounce window, the
/// committed record is the hook provider's.
///
/// The session is held and its runtime's hook schema admits `hermes`, so the
/// report passes the schema check before it is ranked.
#[tokio::test]
async fn a_hook_provider_report_outranks_a_pending_user_report() {
    let harness = Harness::start_with_debounced_notifications().await;
    let session = start_fixture_session(&harness, HOOKED_RUNTIME, IDENTITY_V1_INTEGRATION).await;
    let key = Some(format!("attention:{session}"));

    let hook = create(
        &harness,
        &session,
        HOOK_PROVIDER,
        NotificationKind::AgentBlocked,
        key.clone(),
    )
    .await
    .expect("the session's hook schema admits the report");
    let user = create(
        &harness,
        &session,
        PRODUCER,
        NotificationKind::AgentBlocked,
        key,
    )
    .await
    .expect("the user report is accepted");
    assert!(hook.created && user.created, "{hook:?} {user:?}");

    // Both reports share one pending entry, so the debounce flush commits
    // exactly one record for the key.
    let records = pohunek_test_support::wait::wait_until("the attention record commits", || {
        let harness = &harness;
        let session = &session;
        async move {
            let records = session_records(harness, session).await;
            (!records.is_empty()).then_some(records)
        }
    })
    .await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0]["source"]["provider"], HOOK_PROVIDER,
        "{records:?}"
    );
    assert_eq!(records[0]["id"], hook.record.id.0, "{records:?}");

    harness.stop().await;
}

/// Runtime id of the fixture runtime that names no hook schema.
const HOOKLESS_RUNTIME: &str = "hookless";

/// Runtime id of the fixture runtime that names the `identity-v1` hook schema,
/// which admits `shell`, `codex`, `claude` and `hermes` reports.
const HOOKED_RUNTIME: &str = "hooked";

/// The `[integration]` table of [`HOOKED_RUNTIME`]: the compiled Hermes
/// handler, the one handler that drives `identity-v1`.
const IDENTITY_V1_INTEGRATION: &str = r#"
[integration]
handler = "hermes-hook-v1"
hook_schema = "identity-v1"
"#;

/// A runtime package serving `runtime` with `integration` appended. The
/// program only has to stay alive, silent, until the harness removes the
/// session.
fn runtime_descriptor(runtime: &str, integration: &str) -> String {
    format!(
        r#"schema = 1
id = "acme.runtime.{runtime}"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "{runtime}"
name = "Fixture"
program = "/bin/sh"
args = ["-c", "exec sleep 600"]
detect_manifest = "detect.toml"
prompt_arg = false

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = false

[fork]
supported = false

[native_reference]
strategy = "none"
{integration}"#
    )
}

/// The detection manifest the fixture runtimes name.
const FIXTURE_DETECT: &str = r#"[[rules]]
id = "idle_prompt"
state = "idle"
priority = 100
region = "whole_recent"
any = [{ contains = "ready" }]
"#;

/// Installs a fixture runtime package serving `runtime` with `integration`,
/// starts a session of it, and returns the session id.
async fn start_fixture_session(harness: &Harness, runtime: &str, integration: &str) -> String {
    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: runtime_descriptor(runtime, integration).into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: FIXTURE_DETECT.as_bytes().to_vec(),
            executable: false,
        },
    ];
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    let archive = harness.env.root().join(format!("{runtime}.tar.zst"));
    fs::write(&archive, bytes).expect("write the archive");
    let (code, installed) = harness
        .json(&[
            "plugin",
            "install",
            path_str(&archive),
            "--sha256",
            digest.as_str(),
            "--yes",
        ])
        .await;
    assert_eq!(code, 0, "{installed}");

    let cwd = harness.env.cwd().to_path_buf();
    let (code, created) = harness
        .json(&[
            "session",
            "new",
            "--agent",
            runtime,
            "--cwd",
            path_str(&cwd),
        ])
        .await;
    assert_eq!(code, 0, "{created}");
    created["ok"]["session"]["id"]
        .as_str()
        .or_else(|| created["ok"]["id"].as_str())
        .expect("created session id")
        .to_owned()
}

/// Creates one `error` record from each of `providers` for `session` and
/// returns each outcome: the stored provider, or the refusal code. Every
/// provider is tried before a caller asserts, so a failure names each one.
async fn error_outcomes<'a>(
    harness: &Harness,
    session: &str,
    providers: &[&'a str],
) -> Vec<(&'a str, Result<String, String>)> {
    let mut outcomes = Vec::new();
    for &provider in providers {
        let outcome = create(harness, session, provider, NotificationKind::Error, None)
            .await
            .map(|created| created.record.source.provider)
            .map_err(|refused| refused.code);
        outcomes.push((provider, outcome));
    }
    outcomes
}

/// A session report is admitted under the provider name it is stored and
/// ranked as. For a held session whose runtime refuses hook reports, a
/// spelling that stores as `hermes` or `codex` is refused like the canonical
/// name, so it never becomes a provider-ranked record of that session.
#[tokio::test]
async fn a_hook_provider_spelling_is_admitted_as_it_is_stored() {
    let harness = Harness::start_with_debounced_notifications().await;
    let session = start_fixture_session(&harness, HOOKLESS_RUNTIME, "").await;

    let outcomes = error_outcomes(
        &harness,
        &session,
        &[HOOK_PROVIDER, "hermes\u{7}", " codex"],
    )
    .await;
    assert!(
        outcomes.iter().all(|(_, outcome)| outcome
            .as_ref()
            .is_err_and(|code| code == "hook_not_admitted")),
        "every spelling is refused with hook_not_admitted: {outcomes:?}"
    );
    // A user report for the same session is stored, so the inbox is live and
    // the refused reports are absent rather than unlisted.
    let user = create(&harness, &session, PRODUCER, NotificationKind::Error, None)
        .await
        .expect("a user report is not a hook claim");
    let records = session_records(&harness, &session).await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["id"], user.record.id.0, "{records:?}");
    assert_eq!(records[0]["source"]["provider"], PRODUCER, "{records:?}");

    harness.stop().await;
}

/// A hook provider's report for a session id the daemon does not hold is
/// refused with `hook_not_admitted`, retryable or not, and stores nothing, so
/// no provider-ranked record exists for an id the daemon cannot check. A user
/// report for the same id is not a hook claim and is stored, and the same hook
/// provider is admitted for a held session.
#[tokio::test]
async fn a_hook_provider_report_for_an_unheld_session_is_refused() {
    let harness = Harness::start_with_debounced_notifications().await;

    let outcomes = error_outcomes(&harness, SESSION, &[HOOK_PROVIDER, "codex"]).await;
    assert!(
        outcomes.iter().all(|(_, outcome)| outcome
            .as_ref()
            .is_err_and(|code| code == "hook_not_admitted")),
        "an unheld session is refused with hook_not_admitted: {outcomes:?}"
    );
    assert!(
        session_records(&harness, SESSION).await.is_empty(),
        "a refused report stores nothing"
    );

    let mut client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    let retryable = client
        .create_notification(NotificationCreateParams {
            source: NotificationSource {
                provider: HOOK_PROVIDER.to_owned(),
                provider_event: "scenario".to_owned(),
                host_local_source_id: format!("{HOOK_PROVIDER}-retry"),
            },
            kind: NotificationKind::Error,
            severity: NotificationSeverity::ActionRequired,
            title: "retryable report".to_owned(),
            body: String::new(),
            metadata: BTreeMap::new(),
            session_id: Some(SessionId(SESSION.to_owned())),
            agent_kind: None,
            source_id: None,
            dedupe_key: None,
            project_id: None,
            retryable_admission: true,
        })
        .await;
    assert!(
        matches!(
            &retryable,
            Err(pohunek_client::ClientError::Protocol(error)) if error.code == "hook_not_admitted"
        ),
        "an unheld session is a final refusal, not a retryable one: {retryable:?}"
    );

    create(&harness, SESSION, PRODUCER, NotificationKind::Error, None)
        .await
        .expect("a user report is not a hook claim");
    assert_eq!(session_records(&harness, SESSION).await.len(), 1);

    let held = start_fixture_session(&harness, HOOKED_RUNTIME, IDENTITY_V1_INTEGRATION).await;
    let admitted = error_outcomes(&harness, &held, &[HOOK_PROVIDER]).await;
    assert_eq!(admitted[0].1.as_deref(), Ok(HOOK_PROVIDER), "{admitted:?}");

    harness.stop().await;
}

/// A hook claim must name its provider verbatim. For a held session whose
/// hook schema admits `codex` and `hermes`, the canonical names are stored,
/// while a spelling that only normalizes to one of them is refused.
#[tokio::test]
async fn a_hook_claim_must_spell_its_provider_canonically() {
    let harness = Harness::start_with_debounced_notifications().await;
    let session = start_fixture_session(&harness, HOOKED_RUNTIME, IDENTITY_V1_INTEGRATION).await;

    let canonical = error_outcomes(&harness, &session, &["codex", HOOK_PROVIDER]).await;
    assert!(
        canonical
            .iter()
            .all(|(provider, outcome)| outcome.as_deref() == Ok(*provider)),
        "the schema admits the canonical names: {canonical:?}"
    );
    let spelled = error_outcomes(&harness, &session, &[" codex", "hermes\u{7}"]).await;
    assert!(
        spelled.iter().all(|(_, outcome)| outcome
            .as_ref()
            .is_err_and(|code| code == "hook_not_admitted")),
        "every non-canonical spelling is refused with hook_not_admitted: {spelled:?}"
    );
    let records = session_records(&harness, &session).await;
    assert_eq!(
        records.len(),
        2,
        "only the canonical reports are stored: {records:?}"
    );

    harness.stop().await;
}
