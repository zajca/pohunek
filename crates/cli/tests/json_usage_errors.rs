//! End-to-end: clap argument-parse failures honor `--json`.
//!
//! These drive the real `pohunek` binary (via Cargo's `CARGO_BIN_EXE_*`) at
//! the argument-parsing layer only. Every command here fails to parse — or is a
//! `--help` display — *before* any daemon connection or filesystem access, so
//! the tests are hermetic: no socket, no state directory, no env setup.
//!
//! They lock in the milestone-10 DoD #2 contract for the one path that used to
//! escape it: a usage error under `--json` must print a single structured
//! `{class, code, msg, recover?}` document to stdout (nothing human leaking) and
//! exit non-zero, so automation can branch on `code`.

use std::process::Command;

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
    assert_eq!(doc["code"], "cli_usage");
    assert_eq!(doc["class"], "configuration");
    assert!(doc["msg"].is_string() && !doc["msg"].as_str().unwrap().is_empty());
    assert!(doc.get("recover").is_some(), "usage error carries recover");
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
    assert_eq!(doc["code"], "cli_usage");
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
    assert_eq!(doc["code"], "cli_usage");
    assert_eq!(doc["class"], "configuration");
    assert!(
        doc["msg"]
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
    assert_eq!(doc["code"], "cli_usage");
    assert_eq!(doc["class"], "configuration");
    assert!(
        doc["msg"]
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
    assert_eq!(doc["code"], "cli_usage");
    assert_eq!(doc["class"], "configuration");
    assert!(
        doc["msg"]
            .as_str()
            .is_some_and(|msg| msg.contains("cannot be used with")),
        "usage message should name the argument conflict: {doc:?}"
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
