//! Parser coverage for `pohunek assistant`.
//!
//! These tests are hermetic: they drive `pohunek_cli::command()` through
//! `try_get_matches_from`, which never opens a socket or touches the
//! filesystem. They pin the assistant flag surface on the default form and on
//! every intent wrapper, plus the parse errors clap must raise.

use pohunek_cli::command;

/// The intent wrapper subcommands, which share the default form's flags.
const INTENT_WRAPPERS: [&str; 5] = ["setup", "project", "update", "debug", "help"];

/// Parse the given argument list against the full pohunek CLI.  Returns `Ok`
/// when clap accepts the input, `Err` otherwise.
fn try_parse<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<(), clap::Error> {
    command()
        .try_get_matches_from(std::iter::once("pohunek").chain(args))
        .map(|_| ())
}

/// Every assistant flag parses together on the default form and on each
/// intent wrapper, with either repository selector and a free-form request.
#[test]
fn assistant_flags_parse_on_the_default_form_and_every_intent_wrapper() {
    let project_form = [
        "--agent",
        "codex",
        "--project",
        "ui",
        "--branch",
        "feat/x",
        "--base-branch",
        "main",
        "--yes",
        "--json",
        "--print-prompt",
        "--no-snapshot",
        "--degraded",
        "--no-start-daemon",
        "--intent",
        "debug",
        "some",
        "request",
    ];
    let repo_form = ["--agent", "hermes", "--repo", "/code/ui", "configure", "it"];
    for form in [&project_form[..], &repo_form[..]] {
        try_parse(std::iter::once("assistant").chain(form.iter().copied()))
            .unwrap_or_else(|error| panic!("assistant {form:?} should parse: {error}"));
        for wrapper in INTENT_WRAPPERS {
            let form_without_intent = form
                .iter()
                .copied()
                .filter(|arg| !matches!(*arg, "--intent" | "debug"));
            try_parse(
                ["assistant", wrapper]
                    .into_iter()
                    .chain(form_without_intent),
            )
            .unwrap_or_else(|error| panic!("assistant {wrapper} {form:?} should parse: {error}"));
        }
    }
}

#[test]
fn assistant_intent_flag_parses_all_values() {
    for value in INTENT_WRAPPERS {
        try_parse(["assistant", "--intent", value])
            .unwrap_or_else(|e| panic!("--intent {value} should parse: {e}"));
    }
}

#[test]
fn assistant_intent_flag_rejects_unknown_value() {
    try_parse(["assistant", "--intent", "nonsense"])
        .expect_err("unknown --intent value must be rejected");
}

#[test]
fn assistant_help_wrapper_does_not_collide_with_clap_help() {
    // `assistant help` must parse as the `help` intent wrapper, not trigger
    // clap's built-in --help display (which would exit non-zero in try_parse).
    try_parse(["assistant", "help"])
        .expect("assistant help parses as the intent wrapper, not as clap built-in help");
}
