//! Background-work snapshot through the public daemon protocol (#890 S2).
//!
//! A real worker runs a fake Claude agent, launched from the official Claude
//! package the daemon serves `claude` from, whose state hook is the installed
//! integration. The agent reports its `SessionStart` and each turn end through
//! that hook, so the daemon sees the snapshot only through the worker's inspect
//! poll. The scenarios read it back through `session.inspect`, `session.list`
//! and `subscribe`, with a protocol 3 client on the same daemon for the
//! withholding rule.

// Rust guideline compliant 2026-10-12

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use pohunek_client::Client;
use pohunek_test_support::wait::{guard, wait_until};
use protocol::{method, Request};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

#[path = "../../daemon/tests/support/official_runtimes.rs"]
mod official_runtimes;
#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::Harness;

/// Host profile of the fake agent; the session is started with it.
const PROFILE: &str = "background-claude";

/// File name of the interpreter copy of the shell. The worker accepts an
/// agent's identity report only from a process whose name contains `claude`,
/// and a script's process name is its interpreter's.
const AGENT_INTERPRETER: &str = "fake-claude-sh";

/// Secret-shaped assignment and argument of the first snapshot's shell
/// command. The protocol exposes only the program name, so neither value may
/// appear in any client-visible JSON.
const SECRET: &str = "s3cret-bg";

/// Label of the shell task of the first snapshot. Persisted sinks hold it, so
/// the secret scan can show it reached them.
const SHELL_LABEL: &str = "Run the daemon tests";

/// Rewrites the stored session record of `id` so it holds no background
/// snapshot, as a daemon that had not yet imported the live worker's snapshot
/// would have stored it. The daemon must be stopped.
fn forget_stored_background(root: &Path, id: &str) {
    let path = root.join("metadata/metadata.jsonl");
    let text = fs::read_to_string(&path).expect("read the metadata store");
    let mut rewritten = String::new();
    let mut edited = 0;
    for line in text.lines() {
        let mut record: Value = serde_json::from_str(line).expect("a store line is JSON");
        if record.get("info").is_some() && record["session_id"] == id {
            record["info"]
                .as_object_mut()
                .expect("a session record has an info object")
                .remove("background");
            edited += 1;
        }
        rewritten.push_str(&record.to_string());
        rewritten.push('\n');
    }
    assert_eq!(edited, 1, "one stored record of the session");
    fs::write(&path, rewritten).expect("rewrite the metadata store");
}

/// Event name of the marker a collector appends when its stream ends.
const STREAM_ENDED: &str = "stream_ended";

/// Protocol version of the withholding client, the previous public version.
const PREVIOUS_VERSION: u32 = 3;

/// The fake agent. It runs the installed state hook as its direct child for
/// each `hook:<action>:<fixture>` line, feeding it the named fixture file, and
/// exits on `quit`. The hook's `$PPID` is therefore this agent. The answer to
/// the package's version probe precedes it.
const FAKE_AGENT: &str = r#"set -u
: "${FAKE_AGENT_DIR:?}" "${FAKE_AGENT_HOOKS:?}"
umask 077
hook_input="$FAKE_AGENT_DIR/hook-input.json"
while IFS= read -r line; do
    case "$line" in
        hook:*)
            rest="${line#hook:}"
            action="${rest%%:*}"
            fixture="${rest#*:}"
            cp "$FAKE_AGENT_DIR/$fixture.json" "$hook_input" || exit 1
            "$FAKE_AGENT_HOOKS/pohunek-agent-state.sh" "$action" <"$hook_input"
            printf 'fake-agent hooked %s\n' "$action"
            ;;
        quit)
            exit 0
            ;;
    esac
done
"#;

/// `SessionStart` input: the agent's identity report.
const SESSION_FIXTURE: &str =
    r#"{"hook_event_name":"SessionStart","session_id":"native-background","source":"startup"}"#;

/// First turn end: a subagent, a shell that carries a secret, and an MCP monitor.
const STOP_ONE: &str = r#"{"hook_event_name":"Stop","session_id":"native-background","background_tasks":[
  {"id":"agent-1","type":"subagent","status":"running","description":"Review the diff","agent_type":"reviewer"},
  {"id":"shell-1","type":"shell","status":"running","description":"Run the daemon tests","command":"API_TOKEN=s3cret-bg cargo test -p pohunek-daemon"},
  {"id":"mon-1","type":"monitor","status":"running","description":"Watch the CI feed"}
]}"#;

/// Second turn end: the monitor ended, a new shell started, and the first shell is still listed.
const STOP_TWO: &str = r#"{"hook_event_name":"Stop","session_id":"native-background","background_tasks":[
  {"id":"agent-1","type":"subagent","status":"running","description":"Review the diff","agent_type":"reviewer"},
  {"id":"shell-1","type":"shell","status":"running","description":"Run the daemon tests","command":"API_TOKEN=s3cret-bg cargo test -p pohunek-daemon"},
  {"id":"shell-2","type":"shell","status":"pending","description":"Keep the build green","command":"bacon --headless"}
]}"#;

/// Events a collector has read from one stream, in arrival order.
type Collected = Arc<Mutex<Vec<Value>>>;

/// Starts a daemon (with durable session records when `durable`) that serves
/// `claude` from the official package, its only source, installed through a
/// catalog signed by a throwaway key the daemon trusts.
async fn start_serving_claude(durable: bool) -> Harness {
    let catalog = official_runtimes::OfficialCatalog::new();
    let harness = if durable {
        Harness::start_durable_with_trust(catalog.trust()).await
    } else {
        Harness::start_with_trust(catalog.trust()).await
    };
    let scratch = harness.env.root().join("official-packages");
    fs::create_dir_all(&scratch).expect("create the package scratch directory");
    catalog.install(&harness.sessions, &scratch).await;
    harness
}

/// Writes the fake agent, its interpreter copy and its fixtures under the
/// hermetic root, installs the Claude state hook into the profile's config
/// home, and starts a session of the profile that has reported its identity.
async fn start_agent_session(harness: &Harness) -> (String, PathBuf) {
    let agent_dir = harness.env.root().join("fake-agent");
    let config_dir = harness.env.root().join("claude-config");
    let hooks_dir = config_dir.join("hooks");
    for dir in [&agent_dir, &config_dir] {
        fs::create_dir_all(dir).expect("create the fixture directory");
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).expect("private directory");
    }
    let interpreter = agent_dir.join(AGENT_INTERPRETER);
    pohunek_test_support::fs::copy_shell_image(
        fs::canonicalize("/bin/sh").expect("resolve the shell"),
        &interpreter,
    )
    .expect("copy the shell as the agent interpreter");
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).expect("interpreter mode");
    let script = agent_dir.join("fake-claude.sh");
    fs::write(
        &script,
        format!(
            "#!{}\n{}{FAKE_AGENT}",
            interpreter.display(),
            official_runtimes::sh_version_answer("claude")
        ),
    )
    .expect("write the fake agent");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("agent mode");
    for (name, text) in [
        ("session-startup", SESSION_FIXTURE),
        ("stop-one", STOP_ONE),
        ("stop-two", STOP_TWO),
    ] {
        fs::write(agent_dir.join(format!("{name}.json")), text).expect("write a hook fixture");
    }
    harness.profile(
        PROFILE,
        &format!(
            "base = \"claude\"\n{}program = \"{}\"\n\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n\
             FAKE_AGENT_DIR = \"{}\"\nFAKE_AGENT_HOOKS = \"{}\"\n\n[input_rules]\nbracketed_paste = false\n",
            official_runtimes::profile_pin("claude"),
            script.display(),
            config_dir.display(),
            agent_dir.display(),
            hooks_dir.display(),
        ),
    );

    let (code, installed) = harness
        .json(&[
            "integration",
            "install",
            "--agent",
            "claude",
            "--profile",
            PROFILE,
        ])
        .await;
    assert_eq!(code, 0, "{installed}");
    assert!(
        hooks_dir.join("pohunek-agent-state.sh").is_file(),
        "the Claude state hook is installed beside the profile's config home"
    );

    let agent_dir_text = agent_dir.display().to_string();
    let (code, created) = harness
        .json(&[
            "session",
            "new",
            "--agent",
            PROFILE,
            "--name",
            "background",
            "--cwd",
            &agent_dir_text,
        ])
        .await;
    assert_eq!(code, 0, "{created}");
    let id = created["ok"]["session"]["id"]
        .as_str()
        .or_else(|| created["ok"]["id"].as_str())
        .expect("created session id")
        .to_owned();

    send_hook(harness, &id, "session:session-startup").await;
    wait_until("the agent's identity report to be accepted", || async {
        let info = inspect(harness, &id).await;
        info["active_agent"].is_string().then_some(())
    })
    .await;
    (id, agent_dir)
}

/// Sends one `hook:<action>:<fixture>` line to the agent's terminal.
async fn send_hook(harness: &Harness, id: &str, hook: &str) {
    let line = format!("hook:{hook}");
    let (code, sent) = harness.json(&["session", "input", id, &line]).await;
    assert_eq!(code, 0, "{sent}");
}

/// The `SessionInfo` of `id`, from `session inspect --json`.
async fn inspect(harness: &Harness, id: &str) -> Value {
    let (code, document) = harness.json(&["session", "inspect", id]).await;
    assert_eq!(code, 0, "{document}");
    document["ok"].clone()
}

/// Opens a current-version subscription and collects its event lines.
async fn subscribe(harness: &Harness) -> Collected {
    let client = Client::connect_local(harness.socket())
        .await
        .expect("connect to the daemon");
    let request = Request::new("background-subscribe", method::SUBSCRIBE, Value::Null)
        .expect("valid subscribe request");
    let mut subscription = client.subscribe(&request).await.expect("subscribe");
    let collected: Collected = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&collected);
    tokio::spawn(async move {
        let reason = loop {
            match subscription.next_line().await {
                Ok(Some(line)) => {
                    let event: Value = serde_json::from_str(&line).expect("an event line is JSON");
                    sink.lock().expect("event sink").push(event);
                }
                Ok(None) => break "the daemon closed the subscription".to_owned(),
                Err(error) => break format!("the subscription failed: {error}"),
            }
        };
        sink.lock().expect("event sink").push(stream_ended(&reason));
    });
    collected
}

/// Marks a stream that ended, so a wait on it fails with the reason at once
/// instead of waiting out the hang guard for an event that cannot arrive.
fn stream_ended(reason: &str) -> Value {
    json!({"event": STREAM_ENDED, "reason": reason})
}

/// The events of `collected` named `name`, in arrival order.
///
/// # Panics
///
/// Panics when the stream has ended, since no further event can arrive.
fn named(collected: &Collected, name: &str) -> Vec<Value> {
    let events = collected.lock().expect("event sink");
    if let Some(ended) = events.iter().find(|event| event["event"] == STREAM_ENDED) {
        panic!("{}", ended["reason"]);
    }
    events
        .iter()
        .filter(|event| event["event"] == name)
        .cloned()
        .collect()
}

/// Returns once `collected` has a `background_state` for `id` whose
/// background satisfies `done`, and returns the events read so far.
async fn wait_for_background_state(
    collected: &Collected,
    id: &str,
    what: &str,
    done: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    wait_until(what, || async {
        named(collected, "background_state")
            .into_iter()
            .any(|event| event["session_id"] == id && done(&event["background"]))
            .then(|| named(collected, "background_state"))
    })
    .await
}

fn item<'a>(background: &'a Value, id: &str) -> &'a Value {
    background["items"]
        .as_array()
        .expect("background items")
        .iter()
        .find(|item| item["id"] == id)
        .unwrap_or_else(|| panic!("no background item {id}: {background}"))
}

/// Asserts that a value a client reads does not carry the secret value.
fn assert_no_secret(what: &str, value: &Value) {
    let text = value.to_string();
    assert!(!text.contains(SECRET), "{what} carries the secret: {text}");
}

/// Every regular file under `dir`, read as text.
fn files_under(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else if path.is_file() {
            // Sockets of the worker directories are not persisted data.
            let text = fs::read_to_string(&path).expect("read a persisted file as text");
            found.push((path, text));
        }
    }
    found
}

/// Asserts that no persisted sink under the fixture root holds the secret
/// value, and that the sinks which must hold the snapshot hold its label.
///
/// The sinks are the durable store, the daemon event log and the worker
/// directories. The fake agent's own hook fixtures are excluded: they are the
/// agent's input, not a pohunek sink.
fn assert_sinks_hold_no_secret(root: &Path) {
    let sinks = [
        root.join("metadata"),
        root.join("events"),
        root.join("worker"),
    ];
    let mut label_seen = false;
    for sink in &sinks {
        for (path, text) in files_under(sink) {
            assert!(
                !text.contains(SECRET),
                "{} carries the secret value",
                path.display()
            );
            label_seen |= text.contains(SHELL_LABEL);
        }
    }
    assert!(
        label_seen,
        "a sink holds the label, so the scan reached the data"
    );
}

#[tokio::test]
async fn a_turn_end_snapshot_is_listed_and_streamed_for_the_live_runtime() {
    let harness = start_serving_claude(true).await;
    let collected = subscribe(&harness).await;
    let (id, _) = start_agent_session(&harness).await;

    send_hook(&harness, &id, "stop:stop-one").await;
    let info = wait_until("the first background snapshot", || async {
        let info = inspect(&harness, &id).await;
        info["background"]["items"]
            .as_array()
            .is_some_and(|items| items.len() == 3)
            .then_some(info)
    })
    .await;
    let background = &info["background"];
    assert_eq!(item(background, "agent-1")["kind"], "subagent");
    assert_eq!(item(background, "agent-1")["agent_type"], "reviewer");
    assert_eq!(item(background, "agent-1")["label"], "Review the diff");
    assert!(item(background, "agent-1").get("program").is_none());
    assert_eq!(item(background, "shell-1")["kind"], "shell");
    assert_eq!(item(background, "shell-1")["label"], "Run the daemon tests");
    assert_eq!(item(background, "shell-1")["program"], "cargo");
    assert_eq!(item(background, "mon-1")["kind"], "monitor");
    assert!(item(background, "mon-1").get("program").is_none());
    assert!(background["revision"].is_string(), "{background}");
    assert!(background["observed_at_ms"].is_number(), "{background}");

    let (code, listed) = harness.json(&["session", "list"]).await;
    assert_eq!(code, 0, "{listed}");
    let row = listed["ok"]
        .as_array()
        .expect("sessions")
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("the session is listed");
    assert_eq!(row["background"], *background, "list and inspect agree");

    let states = wait_for_background_state(&collected, &id, "the background_state event", |bg| {
        bg.is_object()
    })
    .await;
    let state = states
        .iter()
        .find(|event| event["session_id"] == id.as_str())
        .expect("a background_state for the session");
    assert_eq!(
        state["background"], *background,
        "the event carries the snapshot"
    );
    assert_eq!(
        state["runtime"]["worker_instance_id"],
        info["runtime"]["worker_instance_id"]
    );
    assert_eq!(
        state["runtime"]["runtime_generation"],
        info["runtime"]["runtime_generation"]
    );

    let events = collected.lock().expect("event sink").clone();
    let state_at = events
        .iter()
        .position(|event| {
            event["event"] == "background_state" && event["session_id"] == id.as_str()
        })
        .expect("the position of the background_state");
    assert!(
        events[..state_at]
            .iter()
            .any(|event| event["event"] == "session_updated"
                && event["session"]["id"] == id.as_str()
                && event["session"]["background"] == *background),
        "a session_updated carrying the snapshot precedes its background_state"
    );
    assert_no_secret("the inspect result", &info);
    assert_no_secret("the list result", &listed);
    assert_no_secret("the event stream", &Value::Array(events));
    // The event log is written asynchronously, so wait until it holds the snapshot.
    let root = harness.env.root().to_path_buf();
    wait_until("the event log to hold the background_state", || async {
        files_under(&root.join("events"))
            .iter()
            .any(|(_, text)| text.contains("background_state"))
            .then_some(())
    })
    .await;
    assert_sinks_hold_no_secret(&root);
    harness.stop().await;
}

/// The revision of a snapshot. It is a decimal string on the wire, so the
/// comparison is numeric.
fn revision(background: &Value) -> u64 {
    background["revision"]
        .as_str()
        .expect("a decimal revision string")
        .parse()
        .expect("a decimal revision")
}

#[tokio::test]
async fn a_changed_task_set_gives_a_new_revision_and_keeps_first_seen() {
    let harness = start_serving_claude(false).await;
    let collected = subscribe(&harness).await;
    let (id, _) = start_agent_session(&harness).await;

    send_hook(&harness, &id, "stop:stop-one").await;
    let first = wait_until("the first snapshot", || async {
        let background = inspect(&harness, &id).await["background"].clone();
        background["items"]
            .as_array()
            .is_some_and(|items| items.len() == 3)
            .then_some(background)
    })
    .await;

    send_hook(&harness, &id, "stop:stop-two").await;
    let second = wait_until("the second snapshot", || async {
        let background = inspect(&harness, &id).await["background"].clone();
        (revision(&background) > revision(&first)).then_some(background)
    })
    .await;
    assert!(revision(&second) > revision(&first), "{second}");
    let ids: Vec<&str> = second["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["id"].as_str().expect("item id"))
        .collect();
    assert!(!ids.contains(&"mon-1"), "an ended task is dropped: {ids:?}");
    assert_eq!(item(&second, "shell-2")["program"], "bacon");
    assert_eq!(item(&second, "shell-2")["status"], "pending");
    assert_eq!(
        item(&second, "shell-1")["first_seen_ms"],
        item(&first, "shell-1")["first_seen_ms"],
        "a task kept across snapshots keeps its first observation"
    );
    assert!(
        item(&second, "shell-1")["last_seen_ms"].as_u64()
            >= item(&first, "shell-1")["last_seen_ms"].as_u64(),
        "{second}"
    );

    wait_for_background_state(&collected, &id, "the second background_state", |bg| {
        bg.is_object() && revision(bg) == revision(&second)
    })
    .await;
    let states: Vec<Value> = named(&collected, "background_state")
        .into_iter()
        .filter(|event| event["session_id"] == id.as_str())
        .collect();
    assert_eq!(states.len(), 2, "one event per change: {states:?}");
    assert_eq!(states[1]["background"], second);
    harness.stop().await;
}

#[tokio::test]
async fn ending_the_agent_clears_the_background_and_publishes_null() {
    let harness = start_serving_claude(false).await;
    let collected = subscribe(&harness).await;
    let (id, _) = start_agent_session(&harness).await;

    send_hook(&harness, &id, "stop:stop-one").await;
    let held = wait_until("the snapshot before the agent ends", || async {
        let info = inspect(&harness, &id).await;
        info["background"].is_object().then_some(info)
    })
    .await;
    wait_for_background_state(&collected, &id, "the snapshot event", Value::is_object).await;

    let (code, quit) = harness.json(&["session", "input", &id, "quit"]).await;
    assert_eq!(code, 0, "{quit}");
    let ended = wait_until("the background is cleared", || async {
        let info = inspect(&harness, &id).await;
        info.get("background").is_none().then_some(info)
    })
    .await;
    assert!(ended.get("background").is_none(), "{ended}");
    wait_for_background_state(
        &collected,
        &id,
        "the cleared background_state",
        Value::is_null,
    )
    .await;

    let cleared = named(&collected, "background_state")
        .into_iter()
        .rev()
        .find(|event| event["session_id"] == id.as_str())
        .expect("a background_state for the session");
    assert!(cleared["background"].is_null(), "{cleared}");
    assert_eq!(
        cleared["runtime"]["worker_instance_id"], held["runtime"]["worker_instance_id"],
        "the clear names the runtime that held the snapshot"
    );
    assert_eq!(
        cleared["runtime"]["runtime_generation"],
        held["runtime"]["runtime_generation"]
    );
    harness.stop().await;
}

/// Connects a protocol 3 client to the daemon socket.
async fn connect_previous(socket: &Path) -> Framed<UnixStream, LinesCodec> {
    wait_until("the daemon socket to accept a connection", || async {
        UnixStream::connect(socket)
            .await
            .ok()
            .map(|stream| Framed::new(stream, LinesCodec::new()))
    })
    .await
}

/// A hand-built protocol 3 request line.
fn previous_request(id: &str, method: &str, params: &Value) -> String {
    json!({
        "v": {"minimum": PREVIOUS_VERSION, "maximum": PREVIOUS_VERSION},
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string()
}

/// Sends one line and reads the reply line, which must come back as JSON.
async fn exchange(client: &mut Framed<UnixStream, LinesCodec>, line: &str) -> Value {
    client.send(line.to_owned()).await.expect("send a request");
    let reply = guard("a protocol 3 response", client.next())
        .await
        .expect("a response line")
        .expect("line framing");
    serde_json::from_str(&reply).expect("the daemon writes JSON lines")
}

#[tokio::test]
async fn a_protocol_3_subscriber_never_receives_background_state_or_the_key() {
    let harness = start_serving_claude(false).await;
    let current = subscribe(&harness).await;

    let mut previous_stream = connect_previous(&harness.socket()).await;
    let subscribed = exchange(
        &mut previous_stream,
        &previous_request("previous-subscribe", method::SUBSCRIBE, &Value::Null),
    )
    .await;
    assert!(subscribed.get("error").is_none(), "{subscribed}");
    let previous_events: Collected = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&previous_events);
    tokio::spawn(async move {
        let reason = loop {
            match previous_stream.next().await {
                Some(Ok(line)) => {
                    let event: Value = serde_json::from_str(&line).expect("an event line is JSON");
                    sink.lock().expect("event sink").push(event);
                }
                None => break "the daemon closed the protocol 3 subscription".to_owned(),
                Some(Err(error)) => {
                    break format!("the protocol 3 subscription failed: {error}");
                }
            }
        };
        sink.lock().expect("event sink").push(stream_ended(&reason));
    });

    let (id, _) = start_agent_session(&harness).await;
    send_hook(&harness, &id, "stop:stop-one").await;
    wait_for_background_state(&current, &id, "the current snapshot event", |bg| {
        bg.is_object()
    })
    .await;
    wait_until(
        "the protocol 3 subscriber to see the session update",
        || async {
            named(&previous_events, "session_updated")
                .iter()
                .any(|event| event["session"]["id"] == id.as_str())
                .then_some(())
        },
    )
    .await;

    let mut inspecting = connect_previous(&harness.socket()).await;
    let reply = exchange(
        &mut inspecting,
        &previous_request("previous-inspect", method::SESSION_INSPECT, &json!(id)),
    )
    .await;
    let info = &reply["ok"];
    assert!(info.is_object(), "{reply}");
    assert!(
        info.get("background").is_none(),
        "a protocol 3 inspect carries no background key: {reply}"
    );

    let (code, quit) = harness.json(&["session", "input", &id, "quit"]).await;
    assert_eq!(code, 0, "{quit}");
    wait_for_background_state(
        &current,
        &id,
        "the cleared background_state",
        Value::is_null,
    )
    .await;
    wait_until(
        "the protocol 3 subscriber to see the runtime end",
        || async {
            named(&previous_events, "session_updated")
                .iter()
                .any(|event| {
                    event["session"]["id"] == id.as_str() && event["session"]["state"] != "running"
                })
                .then_some(())
        },
    )
    .await;

    assert!(
        named(&previous_events, "background_state").is_empty(),
        "a protocol 3 subscriber never receives background_state"
    );
    harness.stop().await;
}

/// Every worker journal under `dir`, parsed.
fn journals_under(dir: &Path, found: &mut Vec<Value>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            journals_under(&path, found);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let text = fs::read_to_string(&path).expect("read a worker journal");
            // Only worker journals name a session and a worker pid.
            if let Ok(journal) = serde_json::from_str::<Value>(&text) {
                if journal.get("worker_pid").is_some() {
                    found.push(journal);
                }
            }
        }
    }
}

/// The pid of the worker of session `id`, from the journal the worker keeps
/// under this fixture's own worker root, so no other process is named.
async fn worker_pid(harness: &Harness, id: &str) -> u32 {
    let state = harness.env.root().join("worker");
    wait_until("the worker journal of the session", || async {
        let mut journals = Vec::new();
        journals_under(&state, &mut journals);
        journals
            .iter()
            .find(|journal| journal["session_id"] == id)
            .and_then(|journal| journal["worker_pid"].as_u64())
            .map(|pid| u32::try_from(pid).expect("a pid fits a u32"))
    })
    .await
}

/// A worker that dies without clearing its snapshot leaves the daemon to clear
/// the background it held, and publish that clear for the runtime it held.
#[tokio::test]
async fn a_killed_worker_clears_the_background_it_held() {
    let harness = start_serving_claude(false).await;
    let collected = subscribe(&harness).await;
    let (id, _) = start_agent_session(&harness).await;

    send_hook(&harness, &id, "stop:stop-one").await;
    let held = wait_until("the snapshot before the worker dies", || async {
        let info = inspect(&harness, &id).await;
        info["background"].is_object().then_some(info)
    })
    .await;
    wait_for_background_state(&collected, &id, "the snapshot event", Value::is_object).await;

    let pid = worker_pid(&harness, &id).await;
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -KILL {pid} failed");

    wait_for_background_state(
        &collected,
        &id,
        "the cleared background_state",
        Value::is_null,
    )
    .await;
    let cleared = wait_until("the background is absent from the session", || async {
        let info = inspect(&harness, &id).await;
        info.get("background").is_none().then_some(info)
    })
    .await;
    let event = named(&collected, "background_state")
        .into_iter()
        .rev()
        .find(|event| event["session_id"] == id.as_str())
        .expect("a background_state for the session");
    assert!(event["background"].is_null(), "{event}");
    assert_eq!(
        event["runtime"]["worker_instance_id"], held["runtime"]["worker_instance_id"],
        "the clear names the runtime that held the snapshot"
    );
    assert_eq!(
        event["runtime"]["runtime_generation"],
        held["runtime"]["runtime_generation"]
    );
    assert_ne!(cleared["runtime"]["state"], "live", "{cleared}");
    harness.stop().await;
}

/// A restarted daemon adopts the live worker and shows the snapshot the worker
/// still holds.
#[tokio::test]
async fn a_restarted_daemon_shows_the_background_its_live_worker_holds() {
    let harness = start_serving_claude(true).await;
    let (id, _) = start_agent_session(&harness).await;

    send_hook(&harness, &id, "stop:stop-one").await;
    let held = wait_until("the snapshot before the restart", || async {
        let background = inspect(&harness, &id).await["background"].clone();
        background.is_object().then_some(background)
    })
    .await;

    let root = harness.env.root().to_path_buf();
    let surviving = harness.shut_down().await;
    forget_stored_background(&root, &id);
    let harness = Harness::start_over(surviving).await;
    let seeded = wait_until("the restarted daemon to show the snapshot", || async {
        let background = inspect(&harness, &id).await["background"].clone();
        (background == held).then_some(background)
    })
    .await;
    assert_eq!(seeded, held, "the adopted worker's snapshot is shown again");
    harness.stop().await;
}

/// A worker that died while the daemon was down leaves no snapshot once the
/// next daemon reconciles it.
#[tokio::test]
async fn a_worker_lost_while_the_daemon_is_down_leaves_no_background() {
    let harness = start_serving_claude(true).await;
    let (id, _) = start_agent_session(&harness).await;

    send_hook(&harness, &id, "stop:stop-one").await;
    wait_until("the snapshot before the daemon stops", || async {
        inspect(&harness, &id).await["background"]
            .is_object()
            .then_some(())
    })
    .await;

    let pid = worker_pid(&harness, &id).await;
    let surviving = harness.shut_down().await;
    let status = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -KILL {pid} failed");

    let harness = Harness::start_over(surviving).await;
    let reconciled = wait_until("the reconciled session", || async {
        let info = inspect(&harness, &id).await;
        (info["runtime"]["state"] != "live").then_some(info)
    })
    .await;
    assert!(
        reconciled.get("background").is_none(),
        "a dead worker's runtime holds no background: {reconciled}"
    );
    harness.stop().await;
}
