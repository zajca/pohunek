//! End-to-end: clap argument-parse failures honor `--json`.
//!
//! These drive the real `pohunek` binary (located through `pohunek_test_support::bin_exe`) at
//! the argument-parsing and origin-environment validation layers. Most commands
//! fail before a daemon connection. The duplicate-metadata case binds a private
//! socket to prove that no request is sent after connecting. Every case uses a
//! private environment and leaves host state untouched.
//!
//! They lock in the milestone-10 `DoD` #2 contract for the one path that used to
//! escape it: a usage error under `--json` must print a single structured
//! versioned `{cli_version, protocol, err}` document to stdout (nothing human leaking) and
//! exit non-zero, so automation can branch on `code`.

use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

use pohunek_test_support::env::TestEnv;

/// Mirrors the documented CLI stdin ceiling derived from the 1 MiB control frame.
const MAX_STDIN_INPUT_BYTES: usize = 256 * 1024;

thread_local! {
    /// The hermetic environment of the current test thread: private HOME, XDG
    /// and working directories, removed when the thread ends.
    static TEST_ENV: TestEnv = TestEnv::new().expect("create the hermetic test environment");
}

/// A `Command` for the built `pohunek` binary under test, with the test
/// thread's scrubbed environment and private working directory.
fn pohunek() -> Command {
    TEST_ENV.with(|env| env.command(pohunek_test_support::bin_exe("pohunek")))
}

fn assert_json_usage_error(arguments: &[&str], message_fragment: &str) {
    let output = pohunek().args(arguments).output().expect("spawn pohunek");
    assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
    assert!(output.stderr.is_empty(), "{arguments:?}: {output:?}");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON error document");
    assert_eq!(document["err"]["code"], "cli_usage", "{arguments:?}");
    assert_eq!(document["err"]["class"], "configuration", "{arguments:?}");
    assert!(
        document["err"]["msg"]
            .as_str()
            .is_some_and(|message| message.contains(message_fragment)),
        "{arguments:?}: expected {message_fragment:?} in {document:?}"
    );
}

#[test]
fn assistant_rejects_project_and_repo_at_the_cli_process_boundary() {
    assert_json_usage_error(
        &[
            "assistant",
            "--project",
            "ui",
            "--repo",
            "/srv/repo",
            "--json",
        ],
        "cannot be used with",
    );
}

#[test]
fn session_new_rejects_conflicting_repository_selectors_at_the_cli_process_boundary() {
    assert_json_usage_error(
        &[
            "session",
            "new",
            "--project",
            "ui",
            "--repo",
            "/x",
            "--json",
        ],
        "cannot be used with",
    );
}

#[test]
fn session_new_rejects_malformed_metadata_at_the_cli_process_boundary() {
    assert_json_usage_error(
        &["session", "new", "--meta", "no-equals-sign", "--json"],
        "--meta",
    );
}

#[test]
fn session_new_rejects_duplicate_metadata_before_sending_a_request() {
    let env = TestEnv::new().expect("private CLI environment");
    let runtime = env.runtime_dir().join("pohunek");
    std::fs::create_dir(&runtime).expect("create daemon socket directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("make socket directory private");
    let listener = UnixListener::bind(runtime.join("daemon.sock")).expect("bind daemon socket");

    let output = env
        .command(pohunek_test_support::bin_exe("pohunek"))
        .args([
            "session",
            "new",
            "--agent",
            "shell",
            "--meta",
            "link.provider=github",
            "--meta",
            "link.provider=linear",
            "--json",
        ])
        .output()
        .expect("run CLI");

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON error document");
    assert_eq!(document["err"]["class"], "configuration");
    assert_eq!(document["err"]["code"], "cli_usage");
    assert!(document["err"]["msg"]
        .as_str()
        .is_some_and(|message| message.contains("link.provider")));
    assert!(document["err"]["recover"].is_string());

    listener.set_nonblocking(true).expect("nonblocking accept");
    let (mut connection, _) = listener.accept().expect("the CLI connected to the socket");
    connection.set_nonblocking(true).expect("nonblocking read");
    let mut request = [0; 1];
    assert_eq!(connection.read(&mut request).expect("read socket"), 0);
}

#[test]
fn remote_session_confirmation_reports_distinct_machine_and_human_failures() {
    let arguments = [
        "--host",
        "host-b",
        "session",
        "new",
        "--agent",
        "shell",
        "--project",
        "ui",
    ];
    let machine = pohunek()
        .args(arguments)
        .arg("--json")
        .output()
        .expect("run noninteractive remote start");
    assert_eq!(machine.status.code(), Some(1), "{machine:?}");
    assert!(machine.stderr.is_empty(), "{machine:?}");
    let document: serde_json::Value =
        serde_json::from_slice(&machine.stdout).expect("one JSON error document");
    assert_eq!(document["err"]["class"], "configuration");
    assert_eq!(document["err"]["code"], "confirmation_required");
    assert!(document["err"]["recover"].is_string());

    let mut human = pohunek()
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start interactive remote command");
    human
        .stdin
        .take()
        .expect("prompt input")
        .write_all(b"n\n")
        .expect("decline remote start");
    let declined = human.wait_with_output().expect("collect declined command");
    assert_eq!(declined.status.code(), Some(1), "{declined:?}");
    assert!(declined.stdout.is_empty(), "{declined:?}");
    let stderr = String::from_utf8(declined.stderr).expect("UTF-8 human error");
    assert!(stderr.contains("host-b"), "{stderr}");
    assert!(stderr.contains("was not confirmed"), "{stderr}");
    assert!(stderr.contains("hint:"), "{stderr}");
}

#[test]
fn session_new_rejects_mixed_input_sources_at_the_cli_process_boundary() {
    assert_json_usage_error(
        &[
            "session",
            "new",
            "--input",
            "secret",
            "--input-stdin",
            "--json",
        ],
        "cannot be used with",
    );
}

#[test]
fn session_input_timeout_names_the_user_facing_flag_at_the_cli_process_boundary() {
    let arguments = [
        "session",
        "input",
        "s-42",
        "hello",
        "--timeout",
        "0",
        "--json",
    ];
    let output = pohunek().args(arguments).output().expect("spawn pohunek");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let document: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON error document");
    assert_eq!(document["err"]["code"], "cli_usage");
    let message = document["err"]["msg"].as_str().expect("usage message");
    assert!(message.contains("--timeout must be between 1 and 8000"));
    assert!(!message.contains("--timeout-ms"));
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
    // A clap invalid-value error under --json. `session new --agent` is a free
    // string (resolved daemon-side), so use `integration install --agent`, whose
    // value_parser accepts the built-in names and any valid runtime id.
    let out = pohunek()
        .args(["integration", "install", "--agent", "Not An Id", "--json"])
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
fn invalid_session_list_filters_under_json_are_structured() {
    for (filter, expected) in [
        ("state=paused", "invalid state filter value"),
        ("cwd=/workspace", "unknown filter key"),
    ] {
        let out = pohunek()
            .args(["session", "list", "--filter", filter, "--json"])
            .output()
            .expect("spawn pohunek");

        assert_eq!(out.status.code(), Some(2), "{filter}");
        assert!(out.stderr.is_empty(), "{filter}");
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        let doc: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("{filter}: stdout must be JSON ({e}): {stdout:?}"));
        assert_eq!(doc["err"]["code"], "cli_usage", "{filter}");
        assert_eq!(doc["err"]["class"], "configuration", "{filter}");
        assert!(
            doc["err"]["msg"]
                .as_str()
                .is_some_and(|msg| msg.contains(expected)),
            "{filter}: usage message should name the filter problem: {doc:?}"
        );
    }
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
fn positional_json_text_does_not_enable_structured_output() {
    let output = pohunek()
        .args(["session", "input", "s-1", "--", "--json"])
        .output()
        .expect("spawn pohunek");

    assert!(!output.status.success(), "the private home has no daemon");
    assert!(output.stdout.is_empty(), "a human error has no JSON stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("cannot reach the daemon"), "{stderr}");
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
fn invalid_origin_environment_fails_fast_without_marker_leakage() {
    for (invalid, marker) in [
        ("POHUNEK_SESSION_ID", "private-session-marker\n"),
        ("POHUNEK_DAEMON_ID", "private-daemon-marker\n"),
    ] {
        let out = pohunek()
            .args(["session", "screen", "s-1", "--json"])
            .env("POHUNEK_SESSION_ID", "valid-session")
            .env("POHUNEK_DAEMON_ID", "valid-daemon")
            .env(invalid, marker)
            .output()
            .expect("spawn pohunek");

        assert_eq!(out.status.code(), Some(1));
        assert!(out.stderr.is_empty());
        let stdout = String::from_utf8(out.stdout).expect("UTF-8 stdout");
        assert!(!stdout.contains(marker.trim_end()));
        let document: serde_json::Value =
            serde_json::from_str(&stdout).expect("one JSON error document");
        assert_eq!(document["err"]["code"], "invalid_origin_environment");
        assert_eq!(document["err"]["class"], "configuration");
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_origin_environment_fails_fast_without_marker_leakage() {
    use std::os::unix::ffi::OsStringExt as _;

    let marker = std::ffi::OsString::from_vec(b"private-origin-marker-\xff".to_vec());
    let out = pohunek()
        .args(["session", "screen", "s-1", "--json"])
        .env("POHUNEK_SESSION_ID", marker)
        .env("POHUNEK_DAEMON_ID", "valid-daemon")
        .output()
        .expect("spawn pohunek");

    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("UTF-8 stdout");
    assert!(!stdout.contains("private-origin-marker"));
    let document: serde_json::Value =
        serde_json::from_str(&stdout).expect("one JSON error document");
    assert_eq!(document["err"]["code"], "invalid_origin_environment");
    assert_eq!(document["err"]["class"], "configuration");
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
