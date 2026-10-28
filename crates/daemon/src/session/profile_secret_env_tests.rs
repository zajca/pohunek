//! Tests of the secret variables a `session.new` fills into a profile's
//! `secret_env` slots: what the agent receives, which requests are refused and
//! that no value is ever stored, listed or echoed.
//!
//! Every test drives a real session registry over a real agent script that
//! records the variables it received, so what the launched child saw is read
//! from the child, never inferred.

// Rust guideline compliant 2026-10-28

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use pohunek_test_support::wait::HANG_GUARD;
use protocol::{ForkCwdMode, SessionForkParams, SessionId, SessionNewParams, SessionSecretEnv};

use super::tests::{
    complete_lines, durable_recovery, hermetic_shell, hook_gate, params, temp_dir, temp_store_path,
    wait_for_file_contains, write_executable,
};
use super::{SessionRegistry, SessionRegistryConfig};
use crate::agent::host::fixture::{
    pi_shaped_host, pi_shaped_integration_host_as, pi_shaped_profile_pin, PI_SHAPED_NO_CHECK,
};

/// Variable the profile declares as its credential slot.
const SLOT: &str = "METER_TOKEN";

/// A second slot, for profiles that need two.
const SECOND_SLOT: &str = "METER_REGION";

/// The secret value every scenario scans for.
const SENTINEL: &str = "SENTINEL_METER_5d41402a";

/// A value for the second slot.
const SECOND_SENTINEL: &str = "SENTINEL_REGION_7b52009b";

/// A profile `[env]` value, scanned for alongside the request secrets.
const PROFILE_ENV_SENTINEL: &str = "SENTINEL_PROFILE_ENV_c9f0f895";

/// The package fixture, an agents directory and the files the launches record
/// into.
struct Slots {
    dir: PathBuf,
    agents: PathBuf,
    store_path: PathBuf,
    marker: PathBuf,
    script: PathBuf,
    /// Keeps the first run alive until released; dropping it releases it.
    gate: fs::File,
}

impl Slots {
    fn new(tag: &str) -> Self {
        let dir = temp_dir(&format!("{tag}-run"));
        let agents = temp_dir(&format!("{tag}-agents"));
        let marker = dir.join("launches.txt");
        let script = dir.join("slot-agent");
        let gate_path = dir.join("exit.gate");
        let gate = hook_gate(&gate_path);
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\n{{ printf 'token=%s\\n' \"${{{SLOT}-unset}}\"; printf 'region=%s\\n' \"${{{SECOND_SLOT}-unset}}\"; printf 'ready\\n'; }} >> '{marker}'\ncase \" $* \" in *\" --session-id \"*) read _ < '{gate}'; exit 0 ;; *) read _ < '{gate}'; exit 0 ;; esac\n",
                marker = marker.display(),
                gate = gate_path.display(),
            ),
        );
        let slots = Self {
            dir,
            agents,
            store_path: temp_store_path(tag),
            marker,
            script,
            gate,
        };
        slots.write_profile("work", &format!("secret_env = [\"{SLOT}\"]\n"));
        slots
    }

    /// Writes the profile `name` with `extra` placed after the `program` key.
    fn write_profile(&self, name: &str, extra: &str) {
        fs::write(
            self.agents.join(format!("{name}.toml")),
            format!(
                "base = \"pi\"\n{}program = \"{}\"\n{extra}",
                pi_shaped_profile_pin(),
                self.script.display()
            ),
        )
        .expect("write profile");
    }

    fn config(&self) -> SessionRegistryConfig {
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            store_path: Some(self.store_path.clone()),
            agents_dir: Some(self.agents.clone()),
            ..SessionRegistryConfig::default()
        }
    }

    fn registry(&self) -> SessionRegistry {
        SessionRegistry::new_with_runtimes(
            self.config(),
            pi_shaped_host(&self.script, PI_SHAPED_NO_CHECK),
        )
    }

    fn new_params(&self, agent: &str, secrets: &[(&str, &str)]) -> SessionNewParams {
        SessionNewParams {
            agent: agent.to_owned(),
            cwd: Some(self.dir.clone()),
            secret_env: secret_env(secrets),
            ..params()
        }
    }

    /// The lines the launches recorded so far.
    fn recorded(&self) -> Vec<String> {
        complete_lines(&fs::read_to_string(&self.marker).unwrap_or_default())
            .map(str::to_owned)
            .collect()
    }

    fn release(&self) {
        use std::io::Write as _;
        let mut writer = &self.gate;
        writer.write_all(b"go\n").expect("release the exit gate");
    }
}

fn secret_env(pairs: &[(&str, &str)]) -> SessionSecretEnv {
    SessionSecretEnv::new(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    )
    .expect("the test secrets are within bounds")
}

/// Asserts `error` is the refusal `code` and quotes neither a request variable
/// name nor a value.
fn assert_refusal(error: &protocol::ProtocolError, code: &str) {
    assert_eq!(error.code, code, "{error:?}");
    let rendered = format!("{error:?} {} {:?}", error.msg, error.recover);
    for forbidden in [SENTINEL, SECOND_SENTINEL, "UNDECLARED_NAME"] {
        assert!(
            !rendered.contains(forbidden),
            "the refusal leaked {forbidden}: {rendered}"
        );
    }
}

#[tokio::test]
async fn a_filled_slot_reaches_the_agent_and_no_artifact() {
    let slots = Slots::new("secret-env-reach");
    let registry = slots.registry();

    let created = registry
        .create(slots.new_params("work", &[(SLOT, SENTINEL)]))
        .await
        .expect("create a session filling its slot");
    wait_for_file_contains(&slots.marker, "ready").await;

    assert_eq!(
        slots.recorded()[0],
        format!("token={SENTINEL}"),
        "the agent process holds the exact value"
    );
    let listed = serde_json::to_string(&registry.list().await).expect("serialize the list");
    let inspected = serde_json::to_string(
        &registry
            .inspect(&created.id)
            .await
            .expect("inspect the session"),
    )
    .expect("serialize the session");
    let store = fs::read_to_string(&slots.store_path).expect("read store");
    for (what, text) in [
        ("session.list", &listed),
        ("session.inspect", &inspected),
        ("the metadata store", &store),
    ] {
        assert!(
            !text.contains(SENTINEL) && !text.contains(SLOT),
            "{what} carries the secret: {text}"
        );
    }
    slots.release();
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
}

#[tokio::test]
async fn a_request_naming_an_undeclared_variable_is_refused() {
    let slots = Slots::new("secret-env-undeclared");
    slots.write_profile("slotless", "");
    let registry = slots.registry();

    for (agent, secrets) in [
        (
            "work",
            vec![(SLOT, SENTINEL), ("UNDECLARED_NAME", SECOND_SENTINEL)],
        ),
        ("work", vec![("UNDECLARED_NAME", SECOND_SENTINEL)]),
        ("slotless", vec![(SLOT, SENTINEL)]),
        ("pi", vec![(SLOT, SENTINEL)]),
        ("shell", vec![(SLOT, SENTINEL)]),
    ] {
        let refused = registry
            .create(slots.new_params(agent, &secrets))
            .await
            .expect_err("an undeclared variable launches nothing");
        assert_refusal(&refused, "secret_env_not_declared");
    }
    assert!(registry.list().await.is_empty(), "no session was created");
    assert!(slots.recorded().is_empty(), "no agent was launched");
}

#[tokio::test]
async fn a_launch_leaving_a_declared_slot_unfilled_is_refused() {
    let slots = Slots::new("secret-env-missing");
    slots.write_profile(
        "pair",
        &format!("secret_env = [\"{SLOT}\", \"{SECOND_SLOT}\"]\n"),
    );
    let registry = slots.registry();

    for (agent, secrets) in [
        ("work", vec![]),
        ("pair", vec![]),
        ("pair", vec![(SLOT, SENTINEL)]),
        ("pair", vec![(SECOND_SLOT, SECOND_SENTINEL)]),
    ] {
        let refused = registry
            .create(slots.new_params(agent, &secrets))
            .await
            .expect_err("an unfilled slot launches nothing");
        assert_refusal(&refused, "secret_env_missing");
    }
    assert!(registry.list().await.is_empty(), "no session was created");
    assert!(slots.recorded().is_empty(), "no agent was launched");

    let created = registry
        .create(slots.new_params("pair", &[(SLOT, SENTINEL), (SECOND_SLOT, SECOND_SENTINEL)]))
        .await
        .expect("every slot filled launches");
    wait_for_file_contains(&slots.marker, "ready").await;
    assert_eq!(
        slots.recorded()[..2],
        [
            format!("token={SENTINEL}"),
            format!("region={SECOND_SENTINEL}")
        ]
    );
    slots.release();
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
}

#[tokio::test]
async fn resume_and_fork_of_a_slot_profile_session_never_inherit_its_secrets() {
    let slots = Slots::new("secret-env-relaunch");
    let registry = slots.registry();
    let created = registry
        .create(slots.new_params("work", &[(SLOT, SENTINEL)]))
        .await
        .expect("create a session filling its slot");
    wait_for_file_contains(&slots.marker, "ready").await;
    slots.release();
    registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("session exits");
    let launches = slots.recorded().len();

    let resumed = registry
        .resume(&created.id)
        .await
        .expect_err("a relaunch cannot fill the slots");
    assert_refusal(&resumed, "secret_env_missing");
    let forked = registry
        .fork(fork_params(&created.id))
        .await
        .expect_err("a fork cannot fill the slots");
    assert_refusal(&forked, "secret_env_missing");

    assert_eq!(slots.recorded().len(), launches, "nothing was relaunched");
    let store = fs::read_to_string(&slots.store_path).expect("read store");
    assert!(
        !store.contains(SENTINEL),
        "the store keeps no secret value: {store}"
    );
    assert!(
        durable_recovery(&slots.store_path, &created.id)
            .profile_revision
            .is_some(),
        "the session still froze its profile revision"
    );
}

fn fork_params(id: &SessionId) -> SessionForkParams {
    SessionForkParams {
        session_id: id.clone(),
        name: None,
        cwd_mode: ForkCwdMode::Same,
        cols: 80,
        rows: 24,
        accept_profile_change: false,
    }
}

#[tokio::test]
async fn a_slot_the_launch_owns_makes_the_profile_invalid() {
    let slots = Slots::new("secret-env-invalid-profile");
    let registry = slots.registry();
    let too_many = (0..=protocol::MAX_SESSION_SECRET_ENV_ENTRIES)
        .map(|index| format!("\"SLOT_{index}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let cases = [
        ("POHUNEK_ENV", "secret_env = [\"POHUNEK_ENV\"]\n".to_owned()),
        ("LD_PRELOAD", "secret_env = [\"LD_PRELOAD\"]\n".to_owned()),
        (
            "DYLD_INSERT_LIBRARIES",
            "secret_env = [\"DYLD_INSERT_LIBRARIES\"]\n".to_owned(),
        ),
        (
            "INVOCATION_ID",
            "secret_env = [\"INVOCATION_ID\"]\n".to_owned(),
        ),
        ("TMPDIR", "secret_env = [\"TMPDIR\"]\n".to_owned()),
        ("TERM", "secret_env = [\"TERM\"]\n".to_owned()),
        ("PATH", "secret_env = [\"PATH\"]\n".to_owned()),
        ("HOME", "secret_env = [\"HOME\"]\n".to_owned()),
        ("lower_case", "secret_env = [\"lower_case\"]\n".to_owned()),
        (
            "METER_TOKEN",
            format!("secret_env = [\"{SLOT}\", \"{SLOT}\"]\n"),
        ),
        (
            "METER_TOKEN",
            format!("secret_env = [\"{SLOT}\"]\n[env]\n{SLOT} = \"{PROFILE_ENV_SENTINEL}\"\n"),
        ),
        ("SLOT_0", format!("secret_env = [{too_many}]\n")),
    ];
    for (index, (offender, body)) in cases.into_iter().enumerate() {
        let name = format!("bad{index}");
        slots.write_profile(&name, &body);
        let refused = registry
            .create(slots.new_params(&name, &[(SLOT, SENTINEL)]))
            .await
            .expect_err("an invalid profile launches nothing");
        assert_eq!(refused.code, "invalid_profile", "{offender}: {refused:?}");
        assert!(
            refused.msg.contains("secret_env"),
            "the refusal names the key: {refused:?}"
        );
        for secret in [SENTINEL, PROFILE_ENV_SENTINEL] {
            assert!(
                !refused.msg.contains(secret),
                "the refusal leaked a value: {refused:?}"
            );
        }
    }
    assert!(registry.list().await.is_empty(), "no session was created");
}

#[tokio::test]
async fn a_slot_naming_the_runtimes_config_home_variable_cannot_be_filled() {
    let slots = Slots::new("secret-env-config-home");
    // The fixture runtime declares CLAUDE_CONFIG_DIR as its config home.
    slots.write_profile("homed", "secret_env = [\"CLAUDE_CONFIG_DIR\"]\n");
    let registry = SessionRegistry::new_with_runtimes(
        slots.config(),
        pi_shaped_integration_host_as(
            &slots.script,
            "pi",
            "claude-hook-v1",
            "identity-subagent-v1",
        ),
    );

    let refused = registry
        .create(slots.new_params("homed", &[("CLAUDE_CONFIG_DIR", SENTINEL)]))
        .await
        .expect_err("the runtime's own home variable is not a slot");
    assert_refusal(&refused, "secret_env_not_declared");
    assert!(slots.recorded().is_empty(), "no agent was launched");
}
