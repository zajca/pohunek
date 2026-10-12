//! Per-session background work through the real daemon, workers, state hook
//! and CLI (#890 S3).
//!
//! The fake Claude agent runs the installed state hook with payloads shaped
//! like Claude's own hook inputs. The subagent rows and the `Stop` snapshot
//! are the fixtures in `compat/claude/subagent-hook-sequences.json`; the
//! expected values below come from those payloads and the spec, not from the
//! rendering code.

#![cfg(unix)]

#[path = "support/background_host.rs"]
mod background_host;

use std::fs;
use std::path::Path;

use background_host::Host;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::fs::write_file;
use serde_json::{json, Value};

/// Hook fixture recorded from Claude Code (see its `source` field), relative
/// to the workspace root.
const FIXTURE: &str = "compat/claude/subagent-hook-sequences.json";

/// Session id of the Claude `SessionStart` input.
const CLAUDE_SESSION_ID: &str = "3f9c1d2e-6b7a-4c8d-9e0f-1a2b3c4d5e6f";

/// Background subagent of the fixture `helper_and_snapshot` sequence.
const AGENT_ONE: &str = "a7e0d3c6f9b2a5e8d";
const AGENT_ONE_LABEL: &str = "Collect the helper-stop timings";
/// Second background subagent of the same fixture sequence.
const AGENT_TWO: &str = "a8f1c4b7e0d3a6f9c";
const AGENT_TWO_LABEL: &str = "Cross-check the transcript counter";

/// Shell of the fixture `lost_stop_corrected_by_snapshot` sequence; its
/// program is `cargo`.
const SHELL_CARGO: &str = "b4q8w2e6r";
const SHELL_CARGO_COMMAND: &str = "cargo nextest run --workspace --no-fail-fast";
const SHELL_CARGO_LABEL: &str = "Run the workspace tests";

/// Shell added to the Part A snapshot; its first command word after the
/// assignment and the `cd` prefix is `bacon`.
const SHELL_BACON: &str = "b9c2m6x0q";
const SHELL_BACON_COMMAND: &str = "RUST_LOG=warn cd /mnt/work && bacon --headless --job check";
const SHELL_BACON_LABEL: &str = "Watch the build";

/// Monitor task added to the Part A snapshot.
const MONITOR: &str = "m5h8k2r7w";
const MONITOR_LABEL: &str = "Watch the CI log";

/// Secret-shaped text of Part B. Nothing of it may reach any sink or output.
const SECRET_COMMAND: &str =
    "API_KEY=sk-live-0123456789abcdef cd /work && env FOO=bar /usr/bin/cargo test --token=sk-live-0123456789abcdef";
const SECRET_VALUE: &str = "sk-live-0123456789abcdef";
const SECRET_ASSISTANT_TEXT: &str = "SENTINEL_ASSISTANT_7d21";
const SECRET_EXTRA_FIELD: &str = "SENTINEL_EXTRA_9b04";
const SECRET_SHELL: &str = "b3s9e2k7d";

/// Fixture sequence names used below.
const HELPER_AND_SNAPSHOT: &str = "helper_and_snapshot";
const LOST_STOP_SNAPSHOT: &str = "lost_stop_corrected_by_snapshot";

/// Hook input file names inside the payload directory.
const SESSION_INPUT: &str = "session.json";
const START_ONE_INPUT: &str = "start-one.json";
const START_TWO_INPUT: &str = "start-two.json";
const STOP_INPUT: &str = "stop.json";
const SECRET_STOP_INPUT: &str = "secret-stop.json";
/// Hook input of a `Stop` that lists no background task.
const EMPTY_STOP_INPUT: &str = "empty-stop.json";

/// One background item the Part A snapshot must carry.
struct Expected {
    id: &'static str,
    kind: &'static str,
    agent_type: Option<&'static str>,
    label: &'static str,
    program: Option<&'static str>,
}

/// Loads the recorded hook fixture.
fn fixture() -> Value {
    serde_json::from_slice(
        &fs::read(pohunek_test_support::workspace_root().join(FIXTURE))
            .expect("read the hook fixture"),
    )
    .expect("the hook fixture is JSON")
}

/// Event `index` of fixture sequence `name`.
fn fixture_event(sequence: &str, index: usize) -> Value {
    fixture()["sequences"][sequence]["events"][index].clone()
}

/// Writes one hook payload under the payload directory.
fn write_payload(dir: &Path, name: &str, payload: &Value) {
    write_file(dir.join(name), payload.to_string()).expect("write a hook payload");
}

/// Background task `task` of the `Stop` input at event `event` of a fixture sequence.
fn fixture_task(sequence: &str, event: usize, task: usize) -> Value {
    fixture_event(sequence, event)["background_tasks"][task].clone()
}

/// The Part A `Stop` input: the two fixture subagents, the fixture `cargo`
/// shell, a `bacon` shell and a monitor. The `bacon` and monitor tasks are
/// added here; the rest are the recorded tasks.
fn part_a_stop() -> Value {
    json!({
        "hook_event_name": "Stop",
        "background_tasks": [
            fixture_task(HELPER_AND_SNAPSHOT, 1, 0),
            fixture_task(HELPER_AND_SNAPSHOT, 1, 1),
            fixture_task(LOST_STOP_SNAPSHOT, 1, 1),
            {
                "id": SHELL_BACON,
                "type": "shell",
                "status": "running",
                "description": SHELL_BACON_LABEL,
                "command": SHELL_BACON_COMMAND,
            },
            {
                "id": MONITOR,
                "type": "monitor",
                "status": "running",
                "description": MONITOR_LABEL,
            },
        ],
    })
}

/// The Part A expectations, derived from [`part_a_stop`] and the spec.
fn part_a_expected() -> Vec<Expected> {
    vec![
        Expected {
            id: AGENT_ONE,
            kind: "subagent",
            agent_type: Some("general-purpose"),
            label: AGENT_ONE_LABEL,
            program: None,
        },
        Expected {
            id: AGENT_TWO,
            kind: "subagent",
            agent_type: Some("general-purpose"),
            label: AGENT_TWO_LABEL,
            program: None,
        },
        Expected {
            id: SHELL_CARGO,
            kind: "shell",
            agent_type: None,
            label: SHELL_CARGO_LABEL,
            program: Some("cargo"),
        },
        Expected {
            id: SHELL_BACON,
            kind: "shell",
            agent_type: None,
            label: SHELL_BACON_LABEL,
            program: Some("bacon"),
        },
        Expected {
            id: MONITOR,
            kind: "monitor",
            agent_type: None,
            label: MONITOR_LABEL,
            program: None,
        },
    ]
}

/// Writes every Part A payload into `dir`.
fn write_part_a(dir: &Path) {
    write_payload(dir, SESSION_INPUT, &session_start());
    write_payload(dir, START_ONE_INPUT, &fixture_event(HELPER_AND_SNAPSHOT, 0));
    write_payload(
        dir,
        START_TWO_INPUT,
        &json!({
            "hook_event_name": "SubagentStart",
            "agent_id": AGENT_TWO,
            "agent_type": "general-purpose",
        }),
    );
    write_payload(dir, STOP_INPUT, &part_a_stop());
}

/// The `SessionStart` input of the Claude agent.
fn session_start() -> Value {
    json!({
        "hook_event_name": "SessionStart",
        "session_id": CLAUDE_SESSION_ID,
        "source": "startup",
    })
}

/// Part B `Stop` input: the secret-shaped shell command, a secret in the final
/// assistant message and a secret in an unknown extra task field.
fn secret_stop() -> Value {
    json!({
        "hook_event_name": "Stop",
        "last_assistant_message": format!("The token is {SECRET_ASSISTANT_TEXT}."),
        "background_tasks": [{
            "id": SECRET_SHELL,
            "type": "shell",
            "status": "running",
            "description": "Run the tests",
            "command": SECRET_COMMAND,
            "credential": SECRET_EXTRA_FIELD,
        }],
    })
}

/// Whether `token` is a compact age such as `0s`, `3m`, `2h` or `1d`.
fn is_age(token: &str) -> bool {
    let (digits, unit) = token.split_at(token.len().saturating_sub(1));
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) && "smhd".contains(unit)
}

/// Index of the single line that starts with `prefix` and continues with an age
/// token, and that line's age.
fn age_line(lines: &[&str], prefix: &str) -> usize {
    let matches: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| {
            line.strip_prefix(prefix)
                .is_some_and(|rest| is_age(rest.split_whitespace().next().unwrap_or_default()))
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(matches.len(), 1, "one line starting {prefix:?}: {lines:#?}");
    matches[0]
}

/// Asserts that the JSON background model carries every expected item with its
/// kind, status, labels and program, and nothing else.
fn assert_background_model(background: &Value, expected: &[Expected]) {
    let revision = background["revision"]
        .as_str()
        .expect("revision is a string");
    assert!(
        revision.parse::<u64>().is_ok(),
        "revision is a decimal string: {revision}"
    );
    assert!(background["observed_at_ms"]
        .as_u64()
        .is_some_and(|ms| ms > 0));
    let items = background["items"].as_array().expect("items array");
    assert_eq!(items.len(), expected.len(), "items: {items:#?}");
    for want in expected {
        let item = items
            .iter()
            .find(|item| item["id"] == want.id)
            .unwrap_or_else(|| panic!("item {} is listed: {items:#?}", want.id));
        assert_eq!(item["kind"], want.kind, "kind of {}", want.id);
        assert_eq!(item["status"], "running", "status of {}", want.id);
        assert_eq!(item["label"], want.label, "label of {}", want.id);
        match want.agent_type {
            Some(agent_type) => assert_eq!(item["agent_type"], agent_type, "{}", want.id),
            None => assert!(item.get("agent_type").is_none(), "{}", want.id),
        }
        match want.program {
            Some(program) => assert_eq!(item["program"], program, "program of {}", want.id),
            None => assert!(item.get("program").is_none(), "no program for {}", want.id),
        }
        let first_seen = item["first_seen_ms"].as_u64().expect("first_seen_ms");
        let last_seen = item["last_seen_ms"].as_u64().expect("last_seen_ms");
        assert!(first_seen <= last_seen, "first before last for {}", want.id);
    }
}

/// Starts a daemon with the scenario profile and payload directory.
fn start_host(payloads: &Path) -> Host {
    let env = TestEnv::new().expect("hermetic test environment");
    Host::start(env, payloads)
}

#[test]
fn background_work_is_listed_and_inspected_with_labels() {
    let payloads = pohunek_test_support::tempdir().expect("payload directory");
    write_part_a(payloads.path());
    let mut host = start_host(payloads.path());
    let id = host.create_session("background-work");
    host.wait_screen_line(&id, "fake-agent ready");
    host.run_hook(&id, "session", SESSION_INPUT);
    host.run_hook(&id, "subagent-start", START_ONE_INPUT);
    host.run_hook(&id, "subagent-start", START_TWO_INPUT);
    host.wait_subagents(&id, 2);
    host.run_hook(&id, "stop", STOP_INPUT);
    host.wait_background(&id, 5);

    // Human list: SUBAGENTS counts the two running rows, BACKGROUND the snapshot.
    let list = String::from_utf8_lossy(&host.run(&["session", "list"]).stdout).into_owned();
    let list_lines: Vec<&str> = list.lines().collect();
    let header = list_lines[0];
    let subagents_at = header.find("SUBAGENTS").expect("SUBAGENTS header");
    let background_at = header.find("BACKGROUND").expect("BACKGROUND header");
    assert!(subagents_at < background_at, "{header}");
    assert!(
        background_at < header.find("CWD").expect("CWD header"),
        "{header}"
    );
    let row = list_lines
        .iter()
        .find(|line| line.starts_with(&id))
        .expect("the session row");
    assert!(row.contains("2/2"), "SUBAGENTS count: {row}");
    assert_eq!(background_cell(&list, &id), "2a 2s 1m", "{list}");

    // Human inspect: subagent rows with their labels, then the other items.
    let inspect =
        String::from_utf8_lossy(&host.run(&["session", "inspect", &id]).stdout).into_owned();
    let lines: Vec<&str> = inspect.lines().collect();
    let items_row = lines
        .iter()
        .find(|line| line.starts_with("background_items"))
        .expect("background_items row");
    assert_eq!(
        items_row.split_whitespace().collect::<Vec<_>>(),
        ["background_items", "5"],
        "{inspect}"
    );
    let observed = lines
        .iter()
        .find(|line| line.starts_with("background_observed"))
        .expect("background_observed row");
    assert!(observed.ends_with(" ago"), "{observed}");
    let subagent_one = age_line(
        &lines,
        &format!("subagent [claude:{AGENT_ONE}]: general-purpose running started "),
    );
    let subagent_two = age_line(
        &lines,
        &format!("subagent [claude:{AGENT_TWO}]: general-purpose running started "),
    );
    let cargo = age_line(
        &lines,
        &format!("background [shell]: running cargo {SHELL_CARGO_LABEL} first seen >= "),
    );
    let bacon = age_line(
        &lines,
        &format!("background [shell]: running bacon {SHELL_BACON_LABEL} first seen >= "),
    );
    let monitor = age_line(
        &lines,
        &format!("background [monitor]: running - {MONITOR_LABEL} first seen >= "),
    );
    // Subagent rows keep the order of the session's subagent list; the other
    // items follow them in snapshot order.
    assert!(
        subagent_one.max(subagent_two) < cargo && cargo < bacon && bacon < monitor,
        "subagent rows precede the other background items, which keep snapshot order: {inspect}"
    );
    assert!(
        lines[subagent_one].ends_with(&format!(" ago label: {AGENT_ONE_LABEL}")),
        "{}",
        lines[subagent_one]
    );
    assert!(
        lines[subagent_two].ends_with(&format!(" ago label: {AGENT_TWO_LABEL}")),
        "{}",
        lines[subagent_two]
    );
    // Both subagent items join their subagent rows, so none is printed again
    // as a background line.
    assert!(
        !inspect.contains("background [subagent]"),
        "joined subagent items get no background line: {inspect}"
    );

    // JSON list and inspect carry the same full model.
    let (code, list_json) = host.json(&["session", "list"]);
    assert_eq!(code, 0, "{list_json}");
    let listed = &list_json["ok"][0];
    assert_eq!(listed["subagents"].as_array().map(Vec::len), Some(2));
    assert_background_model(&listed["background"], &part_a_expected());
    let (code, inspect_json) = host.json(&["session", "inspect", &id]);
    assert_eq!(code, 0, "{inspect_json}");
    assert_eq!(inspect_json["ok"]["background"], listed["background"]);

    // Full command lines are never listed, in any output.
    let outputs = format!("{list}{inspect}{list_json}{inspect_json}");
    for command in [SHELL_CARGO_COMMAND, SHELL_BACON_COMMAND] {
        assert!(!outputs.contains(command), "command line leaked: {command}");
    }

    host.stop_session(&id);
    host.stop_daemon();
    let leaks = host.leaks(&[SHELL_CARGO_COMMAND, SHELL_BACON_COMMAND]);
    assert!(leaks.is_empty(), "command lines reached a sink: {leaks:#?}");
}

#[test]
fn secret_shaped_command_text_reaches_no_sink() {
    let payloads = pohunek_test_support::tempdir().expect("payload directory");
    write_payload(payloads.path(), SESSION_INPUT, &session_start());
    write_payload(payloads.path(), SECRET_STOP_INPUT, &secret_stop());
    let mut host = start_host(payloads.path());
    let id = host.create_session("background-secret");
    host.wait_screen_line(&id, "fake-agent ready");
    host.run_hook(&id, "session", SESSION_INPUT);
    host.run_hook(&id, "stop", SECRET_STOP_INPUT);

    // Positive evidence first: the snapshot is visible, with the program only.
    let snapshot = host.wait_background(&id, 1);
    let item = &snapshot["background"]["items"][0];
    assert_eq!(item["program"], "cargo", "{item}");
    assert_eq!(item["label"], "Run the tests", "{item}");

    let list = String::from_utf8_lossy(&host.run(&["session", "list"]).stdout).into_owned();
    let (code, list_json) = host.json(&["session", "list"]);
    assert_eq!(code, 0, "{list_json}");
    let inspect =
        String::from_utf8_lossy(&host.run(&["session", "inspect", &id]).stdout).into_owned();
    let (code, inspect_json) = host.json(&["session", "inspect", &id]);
    assert_eq!(code, 0, "{inspect_json}");
    assert!(
        inspect.contains("background [shell]: running cargo Run the tests first seen >= "),
        "{inspect}"
    );
    assert_eq!(background_cell(&list, &id), "1s", "{list}");

    host.stop_session(&id);
    host.stop_daemon();

    // Control: the scanner reads the session id from the file sinks, so an
    // empty result below means the secrets are absent, not unread.
    let control = host.leaks(&[&id]);
    assert!(
        control.iter().any(|hit| !hit.starts_with("CLI output")),
        "the file sinks were scanned and hold the session id: {control:#?}"
    );
    let needles = [
        SECRET_COMMAND,
        SECRET_VALUE,
        SECRET_ASSISTANT_TEXT,
        SECRET_EXTRA_FIELD,
        "API_KEY=",
        "--token=",
        "/usr/bin/cargo",
    ];
    let leaks = host.leaks(&needles);
    assert!(leaks.is_empty(), "secret text reached a sink: {leaks:#?}");
}

#[test]
fn empty_snapshot_renders_zero_while_no_snapshot_renders_none() {
    // The recorded `Stop` with an empty task list: Claude reports that no
    // background task runs, which is not the same as having no snapshot.
    let payloads = pohunek_test_support::tempdir().expect("payload directory");
    write_payload(payloads.path(), SESSION_INPUT, &session_start());
    write_payload(
        payloads.path(),
        EMPTY_STOP_INPUT,
        &fixture_event(HELPER_AND_SNAPSHOT, 4),
    );
    let mut host = start_host(payloads.path());
    let id = host.create_session("background-empty");
    host.wait_screen_line(&id, "fake-agent ready");
    host.run_hook(&id, "session", SESSION_INPUT);

    // Control: before any snapshot, the column shows `-` and the inspect row `<none>`.
    let before = host.inspect(&id);
    assert!(before["background"].is_null(), "{before}");
    let (_, before_list) = host.json(&["session", "list"]);
    assert_eq!(before_list["ok"][0]["background"], Value::Null);
    let inspect =
        String::from_utf8_lossy(&host.run(&["session", "inspect", &id]).stdout).into_owned();
    assert!(
        inspect
            .lines()
            .any(|line| split(line) == ["background_items", "<none>"]),
        "{inspect}"
    );
    assert_eq!(background_cell(&list_output(&host), &id), "-");

    // The empty snapshot: the column and the count show `0`, with no line per item.
    host.run_hook(&id, "stop", EMPTY_STOP_INPUT);
    let snapshot = host.wait_background(&id, 0);
    assert!(snapshot["background"]["items"]
        .as_array()
        .is_some_and(Vec::is_empty));
    assert_eq!(background_cell(&list_output(&host), &id), "0");
    let inspect =
        String::from_utf8_lossy(&host.run(&["session", "inspect", &id]).stdout).into_owned();
    let lines: Vec<&str> = inspect.lines().collect();
    assert!(
        lines
            .iter()
            .any(|line| split(line) == ["background_items", "0"]),
        "{inspect}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("background_observed") && line.ends_with(" ago")),
        "{inspect}"
    );
    assert!(
        !lines.iter().any(|line| line.starts_with("background [")),
        "an empty snapshot prints no background line: {inspect}"
    );

    host.stop_session(&id);
    host.stop_daemon();
}

/// Whitespace-separated tokens of one output line.
fn split(line: &str) -> Vec<&str> {
    line.split_whitespace().collect()
}

/// The `session list` table as text.
fn list_output(host: &Host) -> String {
    String::from_utf8_lossy(&host.run(&["session", "list"]).stdout).into_owned()
}

/// The BACKGROUND cell of the session's row in a `session list` table.
///
/// The cell may hold spaces (`2a 2s 1m`), so it is cut by the header's column
/// offsets: from the start of `BACKGROUND` to the start of `CWD`, trimmed. The
/// table writes every column at its header offset, so the same slice holds for
/// every row.
fn background_cell(list: &str, id: &str) -> String {
    let header = list.lines().next().expect("the list header");
    let start = header.find("BACKGROUND").expect("BACKGROUND header");
    let end = header.find("CWD").expect("CWD header");
    let row = list
        .lines()
        .find(|line| line.starts_with(id))
        .expect("the session row");
    row.get(start..end)
        .unwrap_or_else(|| panic!("row {row:?} is shorter than the header {header:?}"))
        .trim()
        .to_owned()
}
