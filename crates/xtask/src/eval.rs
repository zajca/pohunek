//! Behavior-eval scaffold for the universal assistant.
//!
//! This is a MANUAL release gate, not a CI check. It does not drive a live
//! agent (no token cost, no non-determinism). The harness defines fixture
//! environment states and provides a hallucinated-command checker.
//!
//! To use as a pre-release gate:
//!   1. Run `cargo xtask eval` to see all fixtures and the checker API.
//!   2. For each fixture, manually run `pohunek assistant --print-prompt`
//!      with the environment described by the fixture.
//!   3. Feed any `pohunek ...` commands emitted by the assistant to
//!      `check_commands()` to verify no hallucinated commands are present.
//!   4. Confirm the expected outcome matches the assistant's response.
//!
//! CI checks (schema validation, deterministic build, source-map paths,
//! runbook-vs-parser, secret scan) stay in `cargo xtask docs check`.

/// A seeded environment state used as an eval fixture.
#[derive(Debug, Clone)]
pub(crate) struct FixtureState {
    /// Short identifier for this fixture.
    pub id: &'static str,
    /// Human description of the environment state.
    pub description: &'static str,
    /// What the assistant should do or recommend (expected outcome).
    pub expected_outcome: &'static str,
    /// Example `pohunek ...` commands the assistant might emit.
    /// These are checked by the hallucinated-command checker.
    pub example_commands: &'static [&'static str],
}

/// All fixture states for the universal-assistant behavior eval.
pub(crate) static FIXTURES: &[FixtureState] = &[
    FixtureState {
        id: "daemon-down",
        description: "The local pohunek daemon is not running. \
                       `pohunek health` returns a connection error.",
        expected_outcome: "Assistant recommends starting the daemon with \
                           `pohunek daemon start --detach` and verifying with \
                           `pohunek health --json`.",
        example_commands: &[
            "pohunek daemon start --detach",
            "pohunek health --json",
            "pohunek doctor --json",
        ],
    },
    FixtureState {
        id: "launcher-misconfigured",
        description: "The daemon is running but launcher scripts are missing or \
                       stale. `pohunek doctor` reports missing launcher binaries.",
        expected_outcome: "Assistant recommends `pohunek setup scripts` and \
                           then verifying with `pohunek doctor --json`.",
        example_commands: &[
            "pohunek setup scripts",
            "pohunek setup config",
            "pohunek doctor --json",
            "pohunek health --json",
        ],
    },
    FixtureState {
        id: "project-not-registered",
        description: "The user wants to start a session for a project that has \
                       not been registered with pohunek. `pohunek project list` \
                       returns no results.",
        expected_outcome: "Assistant recommends `pohunek project add <path>` to \
                           register the project, then verifying with \
                           `pohunek project show <id-or-label> --json`.",
        // Note: `project show <ref>` and `project actions <ref>` require a
        // reference argument; use placeholder forms that the checker skips so
        // the fixture demonstrates what the assistant would suggest without
        // hard-coding a specific project id.
        example_commands: &[
            "pohunek project list --json",
            "pohunek project add",
            "pohunek project show <id-or-label> --json",
            "pohunek project actions <id-or-label> --json",
        ],
    },
    FixtureState {
        id: "stale-setup-assets",
        description: "The binary was updated but setup assets are stale. \
                       `pohunek doctor` reports version mismatch in launcher \
                       scripts.",
        expected_outcome: "Assistant recommends running the update-after-release \
                           runbook: refresh scripts, verify health, check \
                           capabilities.",
        example_commands: &[
            "pohunek setup scripts",
            "pohunek setup config",
            "pohunek health --json",
            "pohunek host inspect local --json",
            "pohunek doctor --json",
        ],
    },
];

/// Result of checking a single command against the CLI parser.
#[derive(Debug, Clone)]
pub(crate) struct CommandCheckResult {
    pub(crate) command: String,
    pub(crate) valid: bool,
    /// Error message when invalid, None when valid.
    pub(crate) error: Option<String>,
}

/// Check a list of `pohunek ...` command strings against the CLI parser.
///
/// Each command must start with `"pohunek"`. Commands containing `<...>`
/// placeholder tokens are skipped (they are not real command lines).
///
/// Returns one result per command. A command is valid if the CLI parser
/// accepts it (even with `--help`/`--version` which exit with DisplayHelp/
/// DisplayVersion). Only genuine parse failures count as hallucinations.
pub(crate) fn check_commands(commands: &[&str]) -> Vec<CommandCheckResult> {
    commands
        .iter()
        .map(|cmd_str| {
            let command = cmd_str.to_string();

            // Skip placeholder lines (contain `<...>` tokens).
            if command.contains('<') {
                return CommandCheckResult {
                    command,
                    valid: true,
                    error: None,
                };
            }

            let tokens: Vec<String> = command.split_whitespace().map(str::to_string).collect();

            match cli::command().try_get_matches_from(&tokens) {
                Ok(_) => CommandCheckResult {
                    command,
                    valid: true,
                    error: None,
                },
                Err(err) => {
                    use clap::error::ErrorKind;
                    // DisplayHelp and DisplayVersion are valid outcomes
                    // (e.g. `pohunek --help`); only real parse errors count
                    // as hallucinations.
                    if err.kind() == ErrorKind::DisplayHelp
                        || err.kind() == ErrorKind::DisplayVersion
                    {
                        CommandCheckResult {
                            command,
                            valid: true,
                            error: None,
                        }
                    } else {
                        CommandCheckResult {
                            command,
                            valid: false,
                            error: Some(
                                err.kind()
                                    .as_str()
                                    .unwrap_or("unknown parse error")
                                    .to_string(),
                            ),
                        }
                    }
                }
            }
        })
        .collect()
}

/// Run the behavior-eval scaffold.
///
/// Prints all fixtures with their descriptions, expected outcomes, and
/// validates the fixture's own example commands against the CLI parser.
/// Returns true if all fixture example commands parse correctly.
pub(crate) fn run_eval() -> bool {
    println!("==========================================================");
    println!("  cargo xtask eval -- universal assistant behavior eval");
    println!("==========================================================");
    println!();
    println!("This is a MANUAL release gate, not a CI check.");
    println!("It does not drive a live agent (no token cost).");
    println!();
    println!("How to use as a pre-release gate:");
    println!("  1. Review each fixture below and its expected outcome.");
    println!("  2. For each fixture, manually run:");
    println!("       pohunek assistant --print-prompt");
    println!("     with the environment state described by the fixture.");
    println!("  3. Feed any `pohunek ...` commands from the assistant to");
    println!("     check_commands() to verify no hallucinations.");
    println!("  4. Confirm the actual response matches the expected outcome.");
    println!();
    println!("----------------------------------------------------------");

    let mut all_fixtures_pass = true;
    let fixture_count = FIXTURES.len();
    let mut failed_fixture_count = 0usize;

    for (index, fixture) in FIXTURES.iter().enumerate() {
        println!();
        println!("Fixture {}/{}: {}", index + 1, fixture_count, fixture.id);
        println!("  Description:      {}", fixture.description);
        println!("  Expected outcome: {}", fixture.expected_outcome);
        println!("  Example commands:");

        let results = check_commands(fixture.example_commands);
        let mut fixture_pass = true;

        for result in &results {
            if result.valid {
                println!("    [PASS] {}", result.command);
            } else {
                println!(
                    "    [FAIL] {} -- {}",
                    result.command,
                    result.error.as_deref().unwrap_or("parse error")
                );
                fixture_pass = false;
            }
        }

        if !fixture_pass {
            all_fixtures_pass = false;
            failed_fixture_count += 1;
        }
    }

    println!();
    println!("----------------------------------------------------------");
    let passed = fixture_count - failed_fixture_count;
    if all_fixtures_pass {
        println!(
            "eval summary: {}/{} fixture(s) passed -- all example commands parse correctly",
            passed, fixture_count
        );
    } else {
        println!(
            "eval summary: {}/{} fixture(s) passed -- {} fixture(s) had hallucinated commands",
            passed, fixture_count, failed_fixture_count
        );
    }
    println!("==========================================================");

    all_fixtures_pass
}
