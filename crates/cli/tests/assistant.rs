//! CLI-level tests for `pohunek assistant`.
//!
//! ## Design constraints
//!
//! These tests are hermetic — they do NOT require a live daemon.  Every test
//! here exercises one of:
//!
//! 1. **Parser-level correctness** — every `assistant` flag and intent wrapper
//!    accepted by clap without error; error cases clap rejects.  Driven via
//!    `pohunek_cli::command().try_get_matches_from(...)` which never opens a
//!    socket.
//!
//! 2. **Prompt composition** — unit tests for `compose_degraded` (pure
//!    function, no daemon needed).  These assert the structural guarantees the
//!    design requires: snapshot path present, source-map pointer present, no
//!    bundle-TOC section, explicit "degraded" label in the header.
//!
//! Tests that need a live daemon (e.g. full `--print-prompt` round-trip) are
//! deliberately NOT included here — they cannot be made hermetic without a
//! test fixture daemon, which is out of scope for this phase.

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

use pohunek_cli::command;

/// Parse the given argument list against the full pohunek CLI.  Returns `Ok`
/// when clap accepts the input, `Err` otherwise.
fn try_parse<'a>(args: impl IntoIterator<Item = &'a str>) -> Result<(), clap::Error> {
    command()
        .try_get_matches_from(std::iter::once("pohunek").chain(args))
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// Parser tests: default form flags
// ---------------------------------------------------------------------------

#[test]
fn assistant_bare_parses() {
    try_parse(["assistant"]).expect("bare assistant parses");
}

#[test]
fn assistant_print_prompt_flag_parses() {
    try_parse(["assistant", "--print-prompt"]).expect("--print-prompt parses");
}

#[test]
fn assistant_no_snapshot_flag_parses() {
    try_parse(["assistant", "--no-snapshot"]).expect("--no-snapshot parses");
}

#[test]
fn assistant_degraded_flag_parses() {
    try_parse(["assistant", "--degraded"]).expect("--degraded parses");
}

#[test]
fn assistant_no_start_daemon_flag_parses() {
    try_parse(["assistant", "--no-start-daemon"]).expect("--no-start-daemon parses");
}

#[test]
fn assistant_yes_flag_parses() {
    try_parse(["assistant", "--yes"]).expect("--yes parses");
}

#[test]
fn assistant_json_flag_parses() {
    try_parse(["assistant", "--json"]).expect("--json parses");
}

#[test]
fn assistant_agent_flag_parses() {
    try_parse(["assistant", "--agent", "pohunek-assistant"]).expect("--agent parses");
}

#[test]
fn assistant_hermes_agent_flag_parses() {
    try_parse(["assistant", "--agent", "hermes"]).expect("--agent hermes parses");
}

#[test]
fn assistant_project_flag_parses() {
    try_parse(["assistant", "--project", "ui"]).expect("--project parses");
}

#[test]
fn assistant_repo_flag_parses() {
    try_parse(["assistant", "--repo", "/code/ui"]).expect("--repo parses");
}

#[test]
fn assistant_branch_flag_parses() {
    try_parse(["assistant", "--branch", "feature/x"]).expect("--branch parses");
}

#[test]
fn assistant_base_branch_flag_parses() {
    try_parse(["assistant", "--base-branch", "main"]).expect("--base-branch parses");
}

#[test]
fn assistant_intent_flag_parses_all_values() {
    for value in ["setup", "project", "update", "debug", "help"] {
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
fn assistant_request_positional_args_parse() {
    // Free-form request words are joined by the resolver.
    try_parse(["assistant", "configure", "the", "launcher"]).expect("positional request parses");
}

#[test]
fn assistant_all_flags_together_parse() {
    try_parse([
        "assistant",
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
    ])
    .expect("all assistant flags together parse");
}

// ---------------------------------------------------------------------------
// Parser tests: intent wrapper subcommands
// ---------------------------------------------------------------------------

#[test]
fn assistant_setup_wrapper_parses() {
    try_parse(["assistant", "setup"]).expect("assistant setup parses");
}

#[test]
fn assistant_project_wrapper_parses() {
    try_parse(["assistant", "project"]).expect("assistant project parses");
}

#[test]
fn assistant_update_wrapper_parses() {
    try_parse(["assistant", "update"]).expect("assistant update parses");
}

#[test]
fn assistant_debug_wrapper_parses() {
    try_parse(["assistant", "debug"]).expect("assistant debug parses");
}

#[test]
fn assistant_help_wrapper_parses() {
    try_parse(["assistant", "help"]).expect("assistant help parses");
}

#[test]
fn assistant_wrapper_flags_pass_through() {
    // Each wrapper accepts the same AssistantArgs flags.
    for wrapper in ["setup", "project", "update", "debug", "help"] {
        try_parse([
            "assistant",
            wrapper,
            "--agent",
            "codex",
            "--no-snapshot",
            "--degraded",
            "--print-prompt",
        ])
        .unwrap_or_else(|e| panic!("assistant {wrapper} flags should parse: {e}"));
    }
}

#[test]
fn assistant_help_wrapper_does_not_collide_with_clap_help() {
    // `assistant help` must parse as the `help` intent wrapper, not trigger
    // clap's built-in --help display (which would exit non-zero in try_parse).
    try_parse(["assistant", "help"])
        .expect("assistant help parses as the intent wrapper, not as clap built-in help");
}

// ---------------------------------------------------------------------------
// Parser tests: root subcommand list includes `assistant`
// ---------------------------------------------------------------------------

#[test]
fn root_subcommands_include_assistant() {
    let cmd = command();
    assert!(
        cmd.get_subcommands()
            .any(|sub| sub.get_name() == "assistant"),
        "assistant must be a root-level subcommand"
    );
}

// ---------------------------------------------------------------------------
// Unit tests: degraded prompt composition
// ---------------------------------------------------------------------------

// These tests import from the library directly.  `compose_degraded` and
// `ComposeDegradedParams` are `pub(crate)` inside the `commands::assistant`
// module, so we cannot import them from an integration-test file.  Instead we
// embed the assertions in this file and exercise the shape via a public
// re-export or through the binary output.  Since the types are not re-exported,
// we duplicate the relevant logic tests in the unit-test section of
// `prompt.rs`.  The integration-test file tests the parser and binary behavior;
// unit tests for `compose_degraded` live in `prompt.rs` itself.
//
// This section documents the *contract* the binary must satisfy when
// `--degraded --print-prompt` is used, and provides a reference for future
// end-to-end expansion once a fixture daemon is available.

/// Structural contract for `--degraded --print-prompt` output (human text):
/// - Line "knowledge: <version> (degraded)" must appear.
/// - Line "snapshot: <path>" must appear.
/// - No line "bundle: " must appear (bundle was not materialized).
///
/// This test is integration-only and requires no daemon; it validates the
/// parser accepts the flags.  A full end-to-end test (which would need a
/// daemon) is deferred.
#[test]
fn degraded_print_prompt_flags_parse_together() {
    try_parse([
        "assistant",
        "--degraded",
        "--print-prompt",
        "--no-start-daemon",
    ])
    .expect("--degraded --print-prompt --no-start-daemon should parse");
}

#[test]
fn degraded_with_intent_wrapper_parses() {
    try_parse(["assistant", "setup", "--degraded", "--print-prompt"])
        .expect("assistant setup --degraded --print-prompt parses");
}
