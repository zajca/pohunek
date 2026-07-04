//! Behavior eval for the universal assistant.
//!
//! This is a MANUAL release gate, not a CI check. It does not drive a live
//! agent (no token cost, no non-determinism). The harness writes a concrete
//! local eval package and validates captured human-run transcripts when present.
//!
//! To use as a pre-release gate:
//!   1. Run `cargo xtask eval` to write `target/pohunek-eval/`.
//!   2. For each fixture, manually run the fixture's assistant command with the
//!      described seeded state.
//!   3. Save the response to `target/pohunek-eval/transcripts/<fixture-id>.md`.
//!   4. Re-run `cargo xtask eval` to validate commands and required terms.
//!
//! CI checks (schema validation, deterministic build, source-map paths,
//! runbook-vs-parser, secret scan) stay in `cargo xtask docs check`.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::checks::parse_pohunek_command;

const EVAL_OUTPUT_ROOT: &str = "target/pohunek-eval";
const TRANSCRIPTS_DIR: &str = "transcripts";
const FIXTURES_DIR: &str = "fixtures";

/// A seeded environment state used as an eval fixture.
#[derive(Debug, Clone)]
pub(crate) struct FixtureState {
    /// Short identifier for this fixture.
    pub id: &'static str,
    /// Human description of the environment state.
    pub description: &'static str,
    /// What the assistant should do or recommend (expected outcome).
    pub expected_outcome: &'static str,
    /// Exact command to launch the assistant for this fixture.
    pub assistant_command: &'static str,
    /// Transcript file path relative to the repository root.
    pub transcript_path: &'static str,
    /// Required outcome terms that must appear in the saved transcript.
    pub required_terms: &'static [&'static str],
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
        assistant_command: "pohunek assistant debug daemon down",
        transcript_path: "target/pohunek-eval/transcripts/daemon-down.md",
        required_terms: &["daemon", "start", "health"],
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
        assistant_command: "pohunek assistant setup launcher misconfigured",
        transcript_path: "target/pohunek-eval/transcripts/launcher-misconfigured.md",
        required_terms: &["setup", "scripts", "doctor"],
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
        assistant_command: "pohunek assistant project project not registered",
        transcript_path: "target/pohunek-eval/transcripts/project-not-registered.md",
        required_terms: &["project", "add", "register"],
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
        assistant_command: "pohunek assistant update stale setup assets",
        transcript_path: "target/pohunek-eval/transcripts/stale-setup-assets.md",
        required_terms: &["scripts", "health", "capabilities"],
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
/// accepts it (even with `--help`/`--version` which exit with `DisplayHelp`/
/// `DisplayVersion`). Only genuine parse failures count as hallucinations.
pub(crate) fn check_commands(commands: &[&str]) -> Vec<CommandCheckResult> {
    check_commands_inner(commands, true)
}

fn check_transcript_commands(commands: &[&str]) -> Vec<CommandCheckResult> {
    check_commands_inner(commands, false)
}

fn check_commands_inner(commands: &[&str], allow_placeholders: bool) -> Vec<CommandCheckResult> {
    commands
        .iter()
        .map(|cmd_str| {
            let command = cmd_str.to_string();
            if allow_placeholders && command.contains('<') {
                return CommandCheckResult {
                    command,
                    valid: true,
                    error: None,
                };
            }

            match parse_pohunek_command(&command) {
                Ok(()) => CommandCheckResult {
                    command,
                    valid: true,
                    error: None,
                },
                Err(error) => CommandCheckResult {
                    command,
                    valid: false,
                    error: Some(error),
                },
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptValidationResult {
    pub(crate) passed: bool,
    pub(crate) checked: usize,
    pub(crate) failures: Vec<String>,
}

pub(crate) fn extract_pohunek_commands(transcript: &str) -> Vec<String> {
    let mut commands = Vec::new();

    for line in transcript.lines() {
        let trimmed = line.trim();
        if let Some(command) = command_from_line(trimmed) {
            commands.push(command);
        }

        let mut remaining = trimmed;
        while let Some(start) = remaining.find('`') {
            let after_start = &remaining[start + 1..];
            let Some(end) = after_start.find('`') else {
                break;
            };
            let code = &after_start[..end];
            if let Some(command) = command_from_line(code.trim()) {
                if !commands.contains(&command) {
                    commands.push(command);
                }
            }
            remaining = &after_start[end + 1..];
        }
    }

    commands
}

fn command_from_line(line: &str) -> Option<String> {
    let cleaned = line.trim_matches('`').trim();
    let cleaned = cleaned
        .strip_prefix("$ ")
        .or_else(|| cleaned.strip_prefix("> "))
        .unwrap_or(cleaned)
        .trim_start();
    let cleaned = cleaned
        .strip_prefix("- ")
        .or_else(|| cleaned.strip_prefix("* "))
        .or_else(|| cleaned.strip_prefix("+ "))
        .unwrap_or(cleaned)
        .trim_start();
    if !cleaned.starts_with("pohunek") {
        return None;
    }

    let command = cleaned
        .trim_end_matches(['.', ',', ';', ':', ')', ']'])
        .to_string();

    Some(command)
}

pub(crate) fn missing_required_terms<'a>(
    fixture: &'a FixtureState,
    transcript: &str,
) -> Vec<&'a str> {
    let transcript_lower = transcript.to_lowercase();
    fixture
        .required_terms
        .iter()
        .copied()
        .filter(|term| !transcript_lower.contains(&term.to_lowercase()))
        .collect()
}

pub(crate) fn transcript_contains_required_terms(fixture: &FixtureState, transcript: &str) -> bool {
    missing_required_terms(fixture, transcript).is_empty()
}

pub(crate) fn write_eval_package(
    output_root: impl AsRef<Path>,
    fixtures: &[FixtureState],
) -> io::Result<()> {
    let output_root = output_root.as_ref();
    let fixtures_dir = output_root.join(FIXTURES_DIR);

    fs::create_dir_all(output_root)?;
    if fixtures_dir.exists() {
        fs::remove_dir_all(&fixtures_dir)?;
    }
    fs::create_dir_all(&fixtures_dir)?;

    fs::write(output_root.join("README.md"), render_readme(fixtures))?;

    for fixture in fixtures {
        fs::write(
            fixtures_dir.join(format!("{}.md", fixture.id)),
            render_fixture_artifact(fixture),
        )?;
    }

    Ok(())
}

fn render_readme(fixtures: &[FixtureState]) -> String {
    let mut content = String::from(
        "# Universal Assistant Behavior Eval\n\n\
This package is a manual release gate for captured human-run assistant transcripts. \
It does not run a live agent and is not intended for CI automation.\n\n\
## Workflow\n\n\
1. Seed the local state described by each fixture file under `fixtures/`.\n\
2. Run the exact assistant command from the fixture.\n\
3. Save the response transcript at the fixture's transcript path.\n\
4. Re-run `cargo xtask eval` to validate transcript commands and required terms.\n\n\
## Required Transcripts\n\n",
    );

    for fixture in fixtures {
        let _ = writeln!(content, "- `{}`", fixture.transcript_path);
    }

    content
}

fn render_fixture_artifact(fixture: &FixtureState) -> String {
    let mut content = format!(
        "# Fixture: {}\n\n\
## Seeded State\n\n{}\n\n\
## Expected Outcome\n\n{}\n\n\
## Assistant Launch Command\n\n```sh\n{}\n```\n\n\
## Transcript Path\n\n`{}`\n\n\
## Required Outcome Terms\n\n",
        fixture.id,
        fixture.description,
        fixture.expected_outcome,
        fixture.assistant_command,
        fixture.transcript_path
    );

    for term in fixture.required_terms {
        let _ = writeln!(content, "- `{term}`");
    }

    content.push_str("\n## Accepted Example Commands\n\n");
    for command in fixture.example_commands {
        let _ = writeln!(content, "- `{command}`");
    }

    content
}

pub(crate) fn validate_transcripts(
    output_root: impl AsRef<Path>,
    fixtures: &[FixtureState],
) -> TranscriptValidationResult {
    let output_root = output_root.as_ref();
    let transcripts_dir = output_root.join(TRANSCRIPTS_DIR);
    let mut failures = Vec::new();
    let mut checked = 0usize;

    for fixture in fixtures {
        let transcript_path = transcripts_dir.join(format!("{}.md", fixture.id));
        let transcript = match fs::read_to_string(&transcript_path) {
            Ok(transcript) => {
                checked += 1;
                transcript
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                failures.push(format!(
                    "missing transcript for fixture `{}`: {}",
                    fixture.id,
                    transcript_path.display()
                ));
                continue;
            }
            Err(error) => {
                failures.push(format!(
                    "failed to read transcript for fixture `{}` at {}: {error}",
                    fixture.id,
                    transcript_path.display()
                ));
                continue;
            }
        };

        let commands = extract_pohunek_commands(&transcript);
        let command_refs: Vec<&str> = commands.iter().map(String::as_str).collect();
        for result in check_transcript_commands(&command_refs) {
            if !result.valid {
                failures.push(format!(
                    "invalid command in `{}`: `{}` ({})",
                    transcript_path.display(),
                    result.command,
                    result.error.as_deref().unwrap_or("parse error")
                ));
            }
        }

        if !transcript_contains_required_terms(fixture, &transcript) {
            let missing_terms = missing_required_terms(fixture, &transcript);
            failures.push(format!(
                "transcript `{}` is missing required term(s): {}",
                transcript_path.display(),
                missing_terms.join(", ")
            ));
        }
    }

    TranscriptValidationResult {
        passed: failures.is_empty(),
        checked,
        failures,
    }
}

/// Run the behavior-eval scaffold.
///
/// Writes the local eval package, validates fixture example commands, and when
/// transcripts exist validates captured human-run transcripts strictly.
pub(crate) fn run_eval() -> bool {
    let output_root = PathBuf::from(EVAL_OUTPUT_ROOT);

    println!("==========================================================");
    println!("  cargo xtask eval -- universal assistant behavior eval");
    println!("==========================================================");
    println!();
    println!("This is a MANUAL release gate, not a CI check.");
    println!("It does not drive a live agent (no token cost).");
    println!("Eval package: {}", output_root.display());
    println!();

    if let Err(error) = write_eval_package(&output_root, FIXTURES) {
        println!(
            "eval setup failed: could not write {}: {error}",
            output_root.display()
        );
        println!("==========================================================");
        return false;
    }

    println!("Wrote eval README and fixture artifacts.");
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
        println!("  Assistant command: {}", fixture.assistant_command);
        println!("  Transcript path:  {}", fixture.transcript_path);
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
    if !all_fixtures_pass {
        println!(
            "eval summary: {passed}/{fixture_count} fixture(s) passed -- {failed_fixture_count} fixture(s) had hallucinated example commands"
        );
        println!("==========================================================");
        return false;
    }

    println!(
        "example command summary: {passed}/{fixture_count} fixture(s) passed -- all examples parse correctly"
    );

    let transcripts_dir = output_root.join(TRANSCRIPTS_DIR);
    if !transcripts_dir.exists() {
        println!();
        println!("Transcript validation pending.");
        println!(
            "Create `{}` and save one `<fixture-id>.md` transcript per fixture.",
            transcripts_dir.display()
        );
        println!("Then re-run `cargo xtask eval` to validate captured commands and required outcome terms.");
        println!("==========================================================");
        return true;
    }

    println!();
    println!("Validating transcripts in {}.", transcripts_dir.display());
    let transcript_result = validate_transcripts(&output_root, FIXTURES);

    if transcript_result.passed {
        println!(
            "transcript summary: {}/{} transcript(s) passed",
            transcript_result.checked, fixture_count
        );
        println!("==========================================================");
        true
    } else {
        println!(
            "transcript summary: {}/{} transcript(s) checked -- {} failure(s)",
            transcript_result.checked,
            fixture_count,
            transcript_result.failures.len()
        );
        for failure in &transcript_result.failures {
            println!("  [FAIL] {failure}");
        }
        println!("==========================================================");
        false
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_eval_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        dir.push(format!("pohunek-xtask-eval-test-{name}-{unique}"));
        dir
    }

    fn test_fixture() -> FixtureState {
        FixtureState {
            id: "daemon-down",
            description: "The daemon is down.",
            expected_outcome: "Start the daemon and verify health.",
            assistant_command: "pohunek assistant debug daemon down",
            transcript_path: "target/pohunek-eval/transcripts/daemon-down.md",
            required_terms: &["daemon", "health"],
            example_commands: &["pohunek daemon start --detach", "pohunek health --json"],
        }
    }

    #[test]
    fn fixture_launch_commands_run_real_assistant_not_print_prompt() {
        for fixture in FIXTURES {
            assert!(
                !fixture.assistant_command.contains("--print-prompt"),
                "fixture `{}` must launch the real assistant, not only print the prompt",
                fixture.id
            );

            let result = check_commands(&[fixture.assistant_command])
                .into_iter()
                .next()
                .expect("one command result");
            assert!(
                result.valid,
                "fixture `{}` assistant command must parse: {:?}",
                fixture.id, result.error
            );
        }
    }

    fn write_transcript(root: &Path, fixture_id: &str, content: &str) {
        let transcripts_dir = root.join("transcripts");
        fs::create_dir_all(&transcripts_dir).expect("create transcripts dir");
        fs::write(transcripts_dir.join(format!("{fixture_id}.md")), content)
            .expect("write transcript");
    }

    #[test]
    fn extracts_pohunek_commands_from_transcript_and_checks_parser_validity() {
        let transcript = r"
Assistant:
```sh
pohunek health --json
pohunek made-up-command
```
The fallback is `pohunek doctor --json`.
";

        let commands = extract_pohunek_commands(transcript);
        assert_eq!(
            commands,
            vec![
                "pohunek health --json".to_string(),
                "pohunek made-up-command".to_string(),
                "pohunek doctor --json".to_string(),
            ]
        );

        let command_refs: Vec<&str> = commands.iter().map(String::as_str).collect();
        let results = check_commands(&command_refs);

        assert!(results[0].valid);
        assert!(!results[1].valid);
        assert!(results[2].valid);
    }

    #[test]
    fn extracts_pohunek_commands_from_shell_prompts_and_lists() {
        let transcript = r"
$ pohunek health --json
- pohunek doctor --json
* pohunek daemon start --detach
";

        let commands = extract_pohunek_commands(transcript);

        assert_eq!(
            commands,
            vec![
                "pohunek health --json".to_string(),
                "pohunek doctor --json".to_string(),
                "pohunek daemon start --detach".to_string(),
            ]
        );
    }

    #[test]
    fn validates_required_terms_case_insensitively() {
        let fixture = test_fixture();
        let transcript = "The ASSISTANT says to start the Daemon, then inspect HEALTH.";

        assert!(transcript_contains_required_terms(&fixture, transcript));

        let missing = missing_required_terms(&fixture, "Only daemon is mentioned.");
        assert_eq!(missing, vec!["health"]);
    }

    #[test]
    fn writes_eval_package_with_readme_and_fixture_artifact() {
        let output_root = temp_eval_dir("artifacts");
        let fixture = test_fixture();

        write_eval_package(&output_root, std::slice::from_ref(&fixture))
            .expect("write eval package");

        let readme = fs::read_to_string(output_root.join("README.md")).expect("read README");
        assert!(readme.contains("manual release gate"));
        assert!(readme.contains("transcripts/daemon-down.md"));

        let artifact_path = output_root.join("fixtures").join("daemon-down.md");
        let artifact = fs::read_to_string(&artifact_path).expect("read fixture artifact");
        assert!(artifact.contains("# Fixture: daemon-down"));
        assert!(artifact.contains("pohunek assistant debug daemon down"));
        assert!(artifact.contains("Start the daemon and verify health."));
        assert!(artifact.contains("pohunek daemon start --detach"));
    }

    #[test]
    fn transcript_validation_fails_strictly_when_transcript_is_missing() {
        let output_root = temp_eval_dir("missing-transcript");
        let fixtures = [test_fixture()];
        fs::create_dir_all(output_root.join("transcripts")).expect("create transcripts dir");

        let result = validate_transcripts(&output_root, &fixtures);

        assert!(!result.passed);
        assert_eq!(result.checked, 0);
        assert_eq!(result.failures.len(), 1);
        assert!(result.failures[0].contains("missing transcript"));
        assert!(result.failures[0].contains("daemon-down.md"));
    }

    #[test]
    fn transcript_validation_accepts_required_terms_and_valid_commands() {
        let output_root = temp_eval_dir("valid-transcript");
        let fixtures = [test_fixture()];
        write_transcript(
            &output_root,
            "daemon-down",
            "Start the daemon with `pohunek daemon start --detach`, then run `pohunek health --json`.",
        );

        let result = validate_transcripts(&output_root, &fixtures);

        assert!(result.passed);
        assert_eq!(result.checked, 1);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn transcript_validation_rejects_invalid_placeholder_command() {
        let output_root = temp_eval_dir("invalid-placeholder-command");
        let fixtures = [test_fixture()];
        write_transcript(
            &output_root,
            "daemon-down",
            "Start the daemon and check health with `pohunek made-up-command <arg>`.",
        );

        let result = validate_transcripts(&output_root, &fixtures);

        assert!(!result.passed);
        assert_eq!(result.checked, 1);
        assert!(
            result
                .failures
                .iter()
                .any(|failure| failure.contains("pohunek made-up-command <arg>")),
            "invalid placeholder command must fail strictly: {:?}",
            result.failures
        );
    }

    #[test]
    fn transcript_validation_rejects_pohunek_like_binary_names() {
        let output_root = temp_eval_dir("pohunekd-command");
        let fixtures = [test_fixture()];
        write_transcript(
            &output_root,
            "daemon-down",
            "Start the daemon and check health with `pohunekd health --json`.",
        );

        let result = validate_transcripts(&output_root, &fixtures);

        assert!(!result.passed);
        assert_eq!(result.checked, 1);
        assert!(
            result
                .failures
                .iter()
                .any(|failure| failure.contains("pohunekd health --json")),
            "pohunek-like binary names must not parse as pohunek commands: {:?}",
            result.failures
        );
    }
}
