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

/// One plugin invocation shape: the action, its argument ids, and the
/// arguments that follow `pohunek --host host-a session <action>` in the legacy
/// form and in the hardened form with the operands after `--`.
struct Shape<'a> {
    action: &'a str,
    ids: &'a [&'a str],
    legacy: &'a [&'a str],
    hardened: &'a [&'a str],
}

const OUTPUT_OPTIONS: [&str; 10] = [
    "--worker-instance-id",
    "r-1",
    "--runtime-generation",
    "2",
    "--after-offset",
    "3",
    "--max-bytes",
    "64",
    "--wait-ms",
    "10",
];

const WAIT_OPTIONS: [&str; 16] = [
    "--worker-instance-id",
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
];

fn full_argv<'a>(action: &'a str, tail: &[&'a str]) -> Vec<&'a str> {
    let mut argv = vec!["pohunek", "--host", "host-a", "session", action];
    argv.extend_from_slice(tail);
    argv
}

#[test]
fn separator_preserves_legacy_argument_values() {
    let output_legacy = [&["s-42"][..], &OUTPUT_OPTIONS, &["--json"]].concat();
    let output_hardened = [&OUTPUT_OPTIONS[..], &["--json", "--", "s-42"]].concat();
    let wait_legacy = [&["s-42"][..], &WAIT_OPTIONS, &["--json"]].concat();
    let wait_hardened = [&WAIT_OPTIONS[..], &["--json", "--", "s-42"]].concat();
    let shapes = [
        Shape {
            action: "inspect",
            ids: &["target"],
            legacy: &["s-42", "--json"],
            hardened: &["--json", "--", "s-42"],
        },
        Shape {
            action: "screen",
            ids: &["target"],
            legacy: &["s-42", "--json"],
            hardened: &["--json", "--", "s-42"],
        },
        Shape {
            action: "output",
            ids: &[
                "target",
                "worker_instance_id",
                "runtime_generation",
                "after_offset",
                "max_bytes",
                "wait_ms",
            ],
            legacy: &output_legacy,
            hardened: &output_hardened,
        },
        Shape {
            action: "wait",
            ids: &[
                "target",
                "worker_instance_id",
                "runtime_generation",
                "after_updated_at",
                "after_terminal_watermark",
                "after_output_offset",
                "states",
                "activities",
                "timeout_ms",
            ],
            legacy: &wait_legacy,
            hardened: &wait_hardened,
        },
        Shape {
            action: "diff",
            ids: &["target", "base"],
            legacy: &["s-42", "--base", "main", "--json"],
            hardened: &["--base", "main", "--json", "--", "s-42"],
        },
        Shape {
            action: "rename",
            ids: &["target", "name"],
            legacy: &["s-42", "renamed", "--json"],
            hardened: &["--json", "--", "s-42", "renamed"],
        },
    ];
    for shape in shapes {
        let legacy = full_argv(shape.action, shape.legacy);
        let hardened = full_argv(shape.action, shape.hardened);
        assert_eq!(
            parse(&hardened, shape.action, shape.ids),
            parse(&legacy, shape.action, shape.ids),
            "{}",
            shape.action
        );
    }
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
