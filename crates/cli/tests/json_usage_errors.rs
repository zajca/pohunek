//! End-to-end: clap argument-parse failures honor `--json`.
//!
//! These drive the real `pohunek` binary (via Cargo's `CARGO_BIN_EXE_*`) at
//! the argument-parsing layer only. Every command here fails to parse — or is a
//! `--help` display — *before* any daemon connection or filesystem access, so
//! the tests are hermetic: no socket, no state directory, no env setup.
//!
//! They lock in the milestone-10 `DoD` #2 contract for the one path that used to
//! escape it: a usage error under `--json` must print a single structured
//! versioned `{cli_version, protocol, err}` document to stdout (nothing human leaking) and
//! exit non-zero, so automation can branch on `code`.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// Mirrors the documented CLI stdin ceiling derived from the 1 MiB control frame.
const MAX_STDIN_INPUT_BYTES: usize = 256 * 1024;

/// A `Command` for the built `pohunek` binary under test.
fn pohunek() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pohunek"))
}

#[test]
fn missing_required_arg_under_json_is_structured_and_nonzero() {
    // `session inspect --json` — required <target> omitted (case 1 from the bug).
    let out = pohunek()
        .args(["session", "inspect", "--json"])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2), "usage errors exit with code 2");
    assert!(
        out.stderr.is_empty(),
        "no human text on stderr under --json: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    // stdout must be exactly one parseable JSON document — a successful parse of
    // the *entire* stdout proves no human lines leaked before/after it.
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be a single JSON document ({e}): {stdout:?}"));
    assert_eq!(doc["err"]["code"], "cli_usage");
    assert_eq!(doc["err"]["class"], "configuration");
    assert!(doc["err"]["msg"].is_string() && !doc["err"]["msg"].as_str().unwrap().is_empty());
    assert!(
        doc["err"].get("recover").is_some(),
        "usage error carries recover"
    );
    assert!(doc["cli_version"].is_string());
    assert!(doc["protocol"]["minimum"].is_number());
    assert!(doc["protocol"]["maximum"].is_number());
    assert!(doc.get("ok").is_none());
}

#[test]
fn invalid_enum_value_under_json_is_structured() {
    // A clap invalid-enum-value error under --json. `session new --agent` is a
    // free string since Part C (resolved daemon-side), so use the still-enum
    // `integration install --agent`, whose value_parser only accepts claude/codex.
    let out = pohunek()
        .args(["integration", "install", "--agent", "nonsense", "--json"])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be JSON ({e}): {stdout:?}"));
    assert_eq!(doc["err"]["code"], "cli_usage");
}

#[test]
fn invalid_session_list_filter_under_json_is_structured() {
    let out = pohunek()
        .args(["session", "list", "--filter", "state=paused", "--json"])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be JSON ({e}): {stdout:?}"));
    assert_eq!(doc["err"]["code"], "cli_usage");
    assert_eq!(doc["err"]["class"], "configuration");
    assert!(
        doc["err"]["msg"]
            .as_str()
            .is_some_and(|msg| msg.contains("invalid state filter value")),
        "usage message should name the filter value problem: {doc:?}"
    );
}

#[test]
fn unknown_session_list_filter_key_under_json_is_structured() {
    let out = pohunek()
        .args(["session", "list", "--filter", "cwd=/workspace", "--json"])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be JSON ({e}): {stdout:?}"));
    assert_eq!(doc["err"]["code"], "cli_usage");
    assert_eq!(doc["err"]["class"], "configuration");
    assert!(
        doc["err"]["msg"]
            .as_str()
            .is_some_and(|msg| msg.contains("unknown filter key")),
        "usage message should name the unknown filter key: {doc:?}"
    );
}

#[test]
fn session_list_json_and_quiet_conflict_under_json_is_structured() {
    let out = pohunek()
        .args(["session", "list", "--json", "-q"])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be JSON ({e}): {stdout:?}"));
    assert_eq!(doc["err"]["code"], "cli_usage");
    assert_eq!(doc["err"]["class"], "configuration");
    assert!(
        doc["err"]["msg"]
            .as_str()
            .is_some_and(|msg| msg.contains("cannot be used with")),
        "usage message should name the argument conflict: {doc:?}"
    );
}

#[test]
fn notifications_all_hosts_conflicts_with_host_under_json() {
    let out = pohunek()
        .args([
            "--host",
            "host-b",
            "notifications",
            "list",
            "--all-hosts",
            "--json",
        ])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be JSON ({e}): {stdout:?}"));
    assert_eq!(doc["err"]["code"], "cli_usage");
    assert_eq!(doc["err"]["class"], "configuration");
    assert!(
        doc["err"]["msg"]
            .as_str()
            .is_some_and(|msg| { msg.contains("--host") && msg.contains("--all-hosts") }),
        "usage message should name the conflicting arguments: {doc:?}"
    );
}

#[test]
fn usage_error_without_json_stays_human_on_stderr() {
    // No `--json`: behavior must be identical to a plain `Cli::parse()` — human
    // error on stderr, nothing on stdout, exit 2.
    let out = pohunek()
        .args(["session", "inspect"])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "human mode must not write JSON to stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("error:") || stderr.contains("Usage"),
        "human error expected on stderr: {stderr:?}"
    );
}

#[test]
fn help_exits_zero_even_with_json_present() {
    // `--help` is an explicit, successful request; the presence of `--json` must
    // not turn it into a JSON error document.
    let out = pohunek()
        .args(["session", "new", "--help", "--json"])
        .output()
        .expect("spawn pohunek");

    assert!(out.status.success(), "help exits 0");
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert!(stdout.contains("Usage"), "help text expected on stdout");
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout.trim()).is_err(),
        "help output must be text, not a JSON document"
    );
}

#[test]
fn mixed_input_sources_are_a_versioned_usage_error() {
    let out = pohunek()
        .args([
            "session",
            "input",
            "s-1",
            "argv payload",
            "--stdin",
            "--json",
        ])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["err"]["code"], "cli_usage");
    assert!(doc["cli_version"].is_string());
    assert!(doc["protocol"]["minimum"].is_number());
}

#[test]
fn stdin_control_character_is_rejected_without_echoing_payload() {
    let mut child = pohunek()
        .args(["session", "input", "s-1", "--stdin", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pohunek");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"private\0payload")
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for pohunek");

    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("UTF-8 stdout");
    assert!(!stdout.contains("private"));
    assert!(!stdout.contains("payload"));
    let doc: serde_json::Value = serde_json::from_str(&stdout).expect("one JSON document");
    assert_eq!(doc["err"]["code"], "cli_usage");
}

#[test]
fn stdin_limit_plus_one_is_rejected_before_daemon_access() {
    let mut child = pohunek()
        .args(["session", "input", "s-1", "--stdin", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pohunek");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&vec![b'x'; MAX_STDIN_INPUT_BYTES + 1])
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for pohunek");

    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["err"]["code"], "cli_usage");
    assert!(doc["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("maximum")));
}

#[test]
fn output_limit_above_protocol_maximum_is_rejected_by_clap() {
    let out = pohunek()
        .args([
            "session",
            "output",
            "s-1",
            "--max-bytes",
            "999999999",
            "--json",
        ])
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(2));
    assert!(out.stderr.is_empty());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON document");
    assert_eq!(doc["err"]["code"], "cli_usage");
    assert!(doc["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("--max-bytes")));
}

#[test]
fn incomplete_origin_environment_fails_fast_without_marker_leakage() {
    for (present, absent, marker) in [
        (
            "POHUNEK_SESSION_ID",
            "POHUNEK_DAEMON_ID",
            "private-session-marker",
        ),
        (
            "POHUNEK_DAEMON_ID",
            "POHUNEK_SESSION_ID",
            "private-daemon-marker",
        ),
    ] {
        let out = pohunek()
            .args(["session", "screen", "s-1", "--json"])
            .env(present, marker)
            .env_remove(absent)
            .output()
            .expect("spawn pohunek");

        assert_eq!(out.status.code(), Some(1));
        assert!(out.stderr.is_empty());
        let stdout = String::from_utf8(out.stdout).expect("UTF-8 stdout");
        assert!(!stdout.contains(marker));
        let document: serde_json::Value =
            serde_json::from_str(&stdout).expect("one JSON error document");
        assert_eq!(document["err"]["code"], "incomplete_origin_environment");
    }
}

#[test]
fn wait_timeout_is_required_and_bounded_by_shared_contract() {
    for arguments in [
        vec!["session", "wait", "s-1", "--state", "done", "--json"],
        vec![
            "session",
            "wait",
            "s-1",
            "--state",
            "done",
            "--timeout-ms",
            "8001",
            "--json",
        ],
    ] {
        let out = pohunek().args(arguments).output().expect("spawn pohunek");
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stderr.is_empty());
        let document: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("one JSON error document");
        assert_eq!(document["err"]["code"], "cli_usage");
    }
}
