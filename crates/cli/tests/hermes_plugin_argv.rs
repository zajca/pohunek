//! Clap coverage for hardened Hermes plugin invocation shapes.

// Rust guideline compliant 2026-08-07

use clap::ArgMatches;

#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    host: Vec<String>,
    json: bool,
    values: Vec<Vec<String>>,
}

fn raw_values(matches: &ArgMatches, id: &str) -> Vec<String> {
    matches
        .get_raw(id)
        .map(|values| {
            values
                .map(|value| value.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn parse(args: &[&str], action: &str, ids: &[&str]) -> Parsed {
    let root = pohunek_cli::command()
        .try_get_matches_from(args)
        .unwrap_or_else(|error| panic!("Hermes plugin argv should parse: {error}"));
    let session = root
        .subcommand_matches("session")
        .expect("session command should be selected");
    let matches = session
        .subcommand_matches(action)
        .unwrap_or_else(|| panic!("{action} action should be selected"));
    Parsed {
        host: raw_values(&root, "host"),
        json: matches.get_flag("json"),
        values: ids.iter().map(|id| raw_values(matches, id)).collect(),
    }
}

fn assert_equivalent(action: &str, legacy: &[&str], hardened: &[&str], ids: &[&str]) {
    assert_eq!(parse(hardened, action, ids), parse(legacy, action, ids));
}

#[test]
fn inspect_separator_preserves_legacy_argument_values() {
    assert_equivalent(
        "inspect",
        &[
            "pohunek", "--host", "host-a", "session", "inspect", "s-42", "--json",
        ],
        &[
            "pohunek", "--host", "host-a", "session", "inspect", "--json", "--", "s-42",
        ],
        &["target"],
    );
}

#[test]
fn screen_separator_preserves_legacy_argument_values() {
    assert_equivalent(
        "screen",
        &[
            "pohunek", "--host", "host-a", "session", "screen", "s-42", "--json",
        ],
        &[
            "pohunek", "--host", "host-a", "session", "screen", "--json", "--", "s-42",
        ],
        &["target"],
    );
}

#[test]
fn output_separator_preserves_legacy_argument_values() {
    assert_equivalent(
        "output",
        &[
            "pohunek",
            "--host",
            "host-a",
            "session",
            "output",
            "s-42",
            "--runtime-id",
            "r-1",
            "--runtime-generation",
            "2",
            "--after-offset",
            "3",
            "--max-bytes",
            "64",
            "--wait-ms",
            "10",
            "--json",
        ],
        &[
            "pohunek",
            "--host",
            "host-a",
            "session",
            "output",
            "--runtime-id",
            "r-1",
            "--runtime-generation",
            "2",
            "--after-offset",
            "3",
            "--max-bytes",
            "64",
            "--wait-ms",
            "10",
            "--json",
            "--",
            "s-42",
        ],
        &[
            "target",
            "runtime_id",
            "runtime_generation",
            "after_offset",
            "max_bytes",
            "wait_ms",
        ],
    );
}

#[test]
fn wait_separator_preserves_legacy_argument_values() {
    assert_equivalent(
        "wait",
        &[
            "pohunek",
            "--host",
            "host-a",
            "session",
            "wait",
            "s-42",
            "--runtime-id",
            "r-1",
            "--runtime-generation",
            "2",
            "--after-updated-at",
            "2026-08-07T00:00:00Z",
            "--after-terminal-watermark",
            "3",
            "--after-output-offset",
            "4",
            "--state",
            "done",
            "--activity",
            "blocked",
            "--timeout-ms",
            "10",
            "--json",
        ],
        &[
            "pohunek",
            "--host",
            "host-a",
            "session",
            "wait",
            "--runtime-id",
            "r-1",
            "--runtime-generation",
            "2",
            "--after-updated-at",
            "2026-08-07T00:00:00Z",
            "--after-terminal-watermark",
            "3",
            "--after-output-offset",
            "4",
            "--state",
            "done",
            "--activity",
            "blocked",
            "--timeout-ms",
            "10",
            "--json",
            "--",
            "s-42",
        ],
        &[
            "target",
            "runtime_id",
            "runtime_generation",
            "after_updated_at",
            "after_terminal_watermark",
            "after_output_offset",
            "states",
            "activities",
            "timeout_ms",
        ],
    );
}

#[test]
fn diff_separator_preserves_legacy_argument_values() {
    assert_equivalent(
        "diff",
        &[
            "pohunek", "--host", "host-a", "session", "diff", "s-42", "--base", "main", "--json",
        ],
        &[
            "pohunek", "--host", "host-a", "session", "diff", "--base", "main", "--json", "--",
            "s-42",
        ],
        &["target", "base"],
    );
}

#[test]
fn rename_separator_preserves_legacy_argument_values() {
    assert_equivalent(
        "rename",
        &[
            "pohunek", "--host", "host-a", "session", "rename", "s-42", "renamed", "--json",
        ],
        &[
            "pohunek", "--host", "host-a", "session", "rename", "--json", "--", "s-42", "renamed",
        ],
        &["target", "name"],
    );
}

#[test]
fn separator_preserves_leading_hyphen_operands() {
    for action in ["inspect", "screen", "output", "diff"] {
        let args = [
            "pohunek",
            "session",
            action,
            "--json",
            "--",
            "--host=other.example",
        ];
        assert_eq!(
            parse(&args, action, &["target"]).values,
            vec![vec!["--host=other.example".to_owned()]],
        );
    }

    let wait = [
        "pohunek",
        "session",
        "wait",
        "--timeout-ms",
        "10",
        "--json",
        "--",
        "--host=other.example",
    ];
    assert_eq!(
        parse(&wait, "wait", &["target"]).values,
        vec![vec!["--host=other.example".to_owned()]],
    );

    let rename = [
        "pohunek",
        "session",
        "rename",
        "--json",
        "--",
        "--host=other.example",
        "--json",
    ];
    assert_eq!(
        parse(&rename, "rename", &["target", "name"]).values,
        vec![
            vec!["--host=other.example".to_owned()],
            vec!["--json".to_owned()],
        ],
    );
}
