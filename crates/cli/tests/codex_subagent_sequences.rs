//! The subagent hook sequences of the pinned Codex release
//! (`compat/codex/subagent-hook-sequences.json`).
//!
//! The pure tests check the committed file. The opt-in test drives the real
//! `codex` through every scripted flow of the loopback Responses stub, with a
//! probe in place of the state hook, and compares what the probe saw with the
//! file: the event names in order, the key sets and how the transcript paths
//! relate. Nothing of a transcript or a prompt is kept; the model is a stub, so
//! no provider or credential is involved.

// Rust guideline compliant 2026-11-01

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::{guard, HANG_GUARD};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

#[path = "support/process_guard.rs"]
mod process_guard;
#[path = "support/responses_stub.rs"]
mod responses_stub;

use process_guard::ProcessGuard;
use responses_stub::ResponsesStub;

/// The committed sequences.
const SEQUENCES: &str = include_str!("../../../compat/codex/subagent-hook-sequences.json");

/// Opt-in variable of the tests that drive a real `codex`.
const E2E_VARIABLE: &str = "POHUNEK_CODEX_E2E";

/// The hook events the probe records, with the names Codex uses in its trust
/// keys.
const PROBED: [(&str, &str); 5] = [
    ("SessionStart", "session_start"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("SubagentStart", "subagent_start"),
    ("SubagentStop", "subagent_stop"),
    ("Stop", "stop"),
];

/// The id the recordings give the one child agent of a flow.
const SYNTHETIC_AGENT_ID: &str = "agent-child-1";

/// Seconds Codex gives a hook command: the hang guard, since the probe returns
/// at once.
fn hook_timeout_secs() -> u64 {
    HANG_GUARD.as_secs()
}

/// What the outputs of a flow's tool calls must hold, so a call that Codex
/// refused (an unknown target, an unsupported tool) cannot pass for the
/// behavior the flow records: a task that reached the child, and a close or an
/// interrupt that found the child's turn running.
const TOOL_EVIDENCE: [(&str, &str); 6] = [
    ("followup_v1", "submission_id"),
    ("resume_after_close", "submission_id"),
    ("interrupted_v1", "submission_id"),
    ("interrupted_v2", "\"previous_status\":\"running\""),
    ("close_running_v1", "\"previous_status\":\"running\""),
    ("resume_after_close", "pending_init"),
];

/// Flows that need the `multi_agent_v2` tools.
const V2_FLOWS: [&str; 2] = ["followup_v2", "interrupted_v2"];

fn sequences() -> Value {
    serde_json::from_str(SEQUENCES).expect("the sequences file is JSON")
}

#[test]
fn every_scripted_flow_has_a_committed_sequence_and_the_other_way_round() {
    let committed = sequences();
    let committed: Vec<&str> = committed["sequences"]
        .as_object()
        .expect("sequences object")
        .keys()
        .map(String::as_str)
        .collect();
    let mut scripted = responses_stub::FLOW_NAMES.to_vec();
    scripted.sort_unstable();
    assert_eq!(committed, scripted);
}

/// The hash Codex expects in `[hooks.state."<hooks.json>:<event>:0:0"]` for a
/// command hook of one group and one handler.
fn trusted_hash(trust_event: &str, command: &str) -> String {
    // Keys in sorted order, as the canonical form of the hook identity.
    let identity = format!(
        "{{\"event_name\":{event},\"hooks\":[{{\"async\":false,\"command\":{command},\"timeout\":{timeout},\"type\":\"command\"}}]}}",
        event = Value::from(trust_event),
        command = Value::from(command),
        timeout = hook_timeout_secs(),
    );
    let digest = Sha256::digest(identity.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(hex, "{byte:02x}").expect("writing to a String is infallible");
    }
    format!("sha256:{hex}")
}

/// A Codex home that reports every probed hook to a log file.
struct ProbedHome {
    home: PathBuf,
    log: PathBuf,
}

fn write_probed_home(root: &Path, stub: &ResponsesStub, flow: &str) -> ProbedHome {
    let home = root.join("codex-home");
    fs::create_dir_all(&home).expect("create the Codex home");
    let log = root.join("hooks.log");
    let probe = root.join("probe.sh");
    fs::write(
        &probe,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$(cat)\" >> '{}'\n",
            log.display()
        ),
    )
    .expect("write the probe");
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o700)).expect("make the probe run");
    let command = probe.to_str().expect("utf-8 path").to_owned();

    let hooks_path = home.join("hooks.json");
    let groups: BTreeMap<&str, Value> = PROBED
        .iter()
        .map(|(event, _)| {
            (
                *event,
                json!([{ "hooks": [{ "type": "command", "command": command, "timeout": hook_timeout_secs() }] }]),
            )
        })
        .collect();
    fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&json!({ "hooks": groups })).expect("hooks.json"),
    )
    .expect("write hooks.json");

    let v2 = if V2_FLOWS.contains(&flow) {
        "multi_agent_v2 = true\n"
    } else {
        ""
    };
    let mut config = format!(
        "model = \"{model}\"\nmodel_provider = \"stub\"\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\ncheck_for_update_on_startup = false\n\n[analytics]\nenabled = false\n\n[features]\nhooks = true\nplugins = false\nremote_plugin = false\nplugin_sharing = false\napps = false\nin_app_updates = false\n{v2}\n[model_providers.stub]\nname = \"stub\"\nbase_url = \"{base}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n",
        model = responses_stub::MODEL_ID,
        base = stub.base_url(),
    );
    for (_, trust_event) in PROBED {
        write!(
            config,
            "\n[hooks.state.\"{path}:{trust_event}:0:0\"]\ntrusted_hash = \"{hash}\"\n",
            path = hooks_path.display(),
            hash = trusted_hash(trust_event, &command),
        )
        .expect("writing to a String is infallible");
    }
    fs::write(home.join("config.toml"), config).expect("write config.toml");
    ProbedHome { home, log }
}

/// What the probe saw of one flow, in the shape of a committed sequence plus
/// the key sets and path relations it implies.
struct Observed {
    sequence: Value,
    key_sets: BTreeMap<String, Vec<String>>,
    path_relations: BTreeMap<&'static str, bool>,
}

/// How the transcript paths of the recorded hook inputs relate. A relation holds
/// when it holds for every event it covers.
fn transcript_relations(records: &[Value]) -> BTreeMap<&'static str, bool> {
    let first = |event: &str, child: bool| {
        records.iter().find(|record| {
            record["hook_event_name"] == event
                && record.get("agent_id").is_some_and(|id| !id.is_null()) == child
        })
    };
    let path = |record: Option<&Value>, key: &str| {
        record
            .and_then(|record| record[key].as_str())
            .map(str::to_owned)
    };
    let root_transcript = path(first("SessionStart", false), "transcript_path")
        .expect("the root session names its transcript");
    let start = first("SubagentStart", true).expect("every flow starts a child");
    let start_transcript =
        path(Some(start), "transcript_path").expect("the child's start names its transcript");
    // A relation holds when it holds for every event it covers.
    let mut path_relations = BTreeMap::new();
    let mut relate = |name: &'static str, holds: bool| {
        path_relations
            .entry(name)
            .and_modify(|all| *all &= holds)
            .or_insert(holds);
    };
    relate(
        "start_transcript_is_the_childs_own",
        root_transcript != start_transcript,
    );
    for stop in records
        .iter()
        .filter(|record| record["hook_event_name"] == "SubagentStop")
    {
        relate(
            "stop_transcript_is_the_parents",
            path(Some(stop), "transcript_path").as_ref() == Some(&root_transcript),
        );
        relate(
            "stop_agent_transcript_is_the_childs",
            path(Some(stop), "agent_transcript_path").as_ref() == Some(&start_transcript),
        );
    }
    path_relations
}

fn observe(log: &Path) -> Observed {
    let text = fs::read_to_string(log).expect("the probe wrote a log");
    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a hook input is one JSON line"))
        .collect();
    let mut root_events = Vec::new();
    let mut child_events = Vec::new();
    let mut key_sets = BTreeMap::new();
    let mut agent_ids = Vec::new();
    for record in &records {
        let event = record["hook_event_name"].as_str().expect("event name");
        let child = record.get("agent_id").is_some_and(|id| !id.is_null());
        let label = match (event, child) {
            ("UserPromptSubmit", true) => "UserPromptSubmit (child)".to_owned(),
            (name, _) => name.to_owned(),
        };
        let mut keys: Vec<String> = record
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect();
        keys.sort_unstable();
        if let Some(previous) = key_sets.insert(label.clone(), keys.clone()) {
            assert_eq!(
                previous, keys,
                "the key set of {label} changes within a flow"
            );
        }
        if child {
            child_events.push(event.to_owned());
            agent_ids.push(record["agent_id"].as_str().expect("agent id").to_owned());
        } else {
            root_events.push(event.to_owned());
        }
    }
    agent_ids.dedup();
    assert!(
        agent_ids.len() <= 1,
        "one child agent per flow: {agent_ids:?}"
    );

    let path_relations = transcript_relations(&records);
    // The subagent lifecycle in the shape of the Claude recordings, with a
    // synthetic id for the one child.
    let events: Vec<Value> = records
        .iter()
        .filter(|record| {
            matches!(
                record["hook_event_name"].as_str(),
                Some("SubagentStart" | "SubagentStop")
            )
        })
        .map(|record| {
            json!({
                "hook_event_name": record["hook_event_name"],
                "agent_id": SYNTHETIC_AGENT_ID,
                "agent_type": record["agent_type"],
            })
        })
        .collect();
    Observed {
        sequence: json!({ "root_events": root_events, "child_events": child_events, "events": events }),
        key_sets,
        path_relations,
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

async fn run_flow(flow: &str) -> Observed {
    let env = TestEnv::new().expect("create the test environment");
    // Dropped before the environment, so a hang or a failed assertion leaves no
    // process of the flow behind (the npm shim starts the native binary).
    let _processes = ProcessGuard::new(env.root());
    let stub = ResponsesStub::start();
    let probed = write_probed_home(env.root(), &stub, flow);
    let mut command = env.tokio_command(find_on_path("codex").expect("codex on PATH"));
    command
        .env("CODEX_HOME", &probed.home)
        .args(["exec", "--skip-git-repo-check"])
        .arg(format!("{}{flow}] go", responses_stub::FLOW_PREFIX))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn codex");
    let status = guard("the flow to finish", child.wait())
        .await
        .expect("wait for codex");
    assert!(status.success(), "{flow}: codex exited with {status:?}");
    assert!(
        !stub.saw_credential(),
        "{flow}: a credential reached the stub"
    );
    let outputs = stub.flow_tool_outputs();
    for (name, needle) in TOOL_EVIDENCE {
        if name == flow {
            assert!(
                outputs.iter().any(|output| output.contains(needle)),
                "{flow}: no tool output holds {needle}: {outputs:?}"
            );
        }
    }
    observe(&probed.log)
}

/// Asserts that the `codex` on `PATH` is the release the compatibility lock
/// pins, so the file is never compared with another release's behavior.
fn assert_locked_release() {
    let lock: Value = serde_json::from_str(include_str!(
        "../../../compat/codex/compatibility-lock.json"
    ))
    .expect("the compatibility lock is JSON");
    let release = lock["upstream"]["release"]
        .as_str()
        .expect("locked release");
    let template = lock["upstream"]["version_output"]
        .as_str()
        .expect("locked version output");
    let expected = template.replace("{release}", release);
    let env = TestEnv::new().expect("create the test environment");
    let output = env
        .command(find_on_path("codex").expect("codex on PATH"))
        .arg("--version")
        .output()
        .expect("run codex --version");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        expected,
        "the `codex` on PATH is not the locked release"
    );
}

/// Drives the real `codex` through every flow and compares the recordings with
/// the committed file. The observed recording is printed in the file's shape
/// (`--no-capture`); after a deliberate change of the pinned release or the
/// flows, that output is merged into
/// `compat/codex/subagent-hook-sequences.json` (the top-level `source`,
/// `captured_at` and `format`, and the `tools` and `description` of each flow,
/// are hand-written).
///
/// Needs the pinned `codex` on `PATH` and `POHUNEK_CODEX_E2E=1`.
#[tokio::test]
#[ignore = "needs a real `codex` on PATH; run with POHUNEK_CODEX_E2E=1 and --ignored"]
async fn a_real_codex_produces_the_committed_subagent_hook_sequences() {
    assert_eq!(
        std::env::var(E2E_VARIABLE).as_deref(),
        Ok("1"),
        "the real-Codex tests need {E2E_VARIABLE}=1 and the pinned `codex` on PATH"
    );
    let committed = sequences();
    let mut key_sets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut observed_sequences = serde_json::Map::new();
    let mut relations: BTreeMap<&str, bool> = BTreeMap::new();
    assert_locked_release();
    for flow in responses_stub::FLOW_NAMES {
        let observed = run_flow(flow).await;
        for (label, keys) in observed.key_sets {
            if let Some(previous) = key_sets.insert(label.clone(), keys.clone()) {
                assert_eq!(
                    previous, keys,
                    "{flow}: the key set of {label} differs between flows"
                );
            }
        }
        let has_stop = observed.sequence["child_events"]
            .as_array()
            .is_some_and(|events| events.contains(&json!("SubagentStop")));
        let expected_relations = if has_stop { 3 } else { 1 };
        assert_eq!(
            observed.path_relations.len(),
            expected_relations,
            "{flow}: every transcript relation of the recorded events is checked"
        );
        for (relation, holds) in observed.path_relations {
            assert!(holds, "{flow}: {relation} does not hold");
            relations.insert(relation, true);
        }
        observed_sequences.insert(flow.to_owned(), observed.sequence);
    }
    let observed = json!({
        "key_sets": key_sets,
        "path_relations": relations,
        "sequences": observed_sequences,
    });
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&observed).expect("observed")
    );

    assert_eq!(
        committed["key_sets"], observed["key_sets"],
        "the key sets of the hook inputs"
    );
    assert_eq!(
        committed["path_relations"], observed["path_relations"],
        "the transcript path relations"
    );
    for flow in responses_stub::FLOW_NAMES {
        let expected = &committed["sequences"][flow];
        let seen = &observed["sequences"][flow];
        assert_eq!(
            expected["root_events"], seen["root_events"],
            "{flow}: root events"
        );
        assert_eq!(
            expected["child_events"], seen["child_events"],
            "{flow}: child events"
        );
        assert_eq!(
            expected["events"], seen["events"],
            "{flow}: subagent events"
        );
    }
}
