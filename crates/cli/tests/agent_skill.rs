//! Guards the checked-in agent skill artifact that the CLI embeds verbatim.
//!
//! These assertions run under `cargo test -p pohunek-cli`, independent of
//! xtask, so a stale or truncated artifact fails the CLI suite directly, and
//! every `pohunek ...` example inside the artifact must parse against the live
//! clap tree.

// Rust guideline compliant 2026-09-16

use std::ffi::OsStr;
use std::process::Command;

use sha2::{Digest, Sha256};

const EMBEDDED_SKILL: &str = include_str!("../src/commands/agent_skill/SKILL.md");

/// The artifact frontmatter must name the skill exactly as agents consume it.
const SKILL_FRONTMATTER_PREFIX: &str = "---\nname: pohunek\n";
const GENERATED_NOTICE: &str =
    "<!-- @generated: do not edit; run `cargo xtask agent-skill generate` -->";
const SOURCE_NOTICE: &str = "<!-- Source: docs/knowledge/guides/agent-skill.md -->";

/// Section headings the embedded skill must cover: the mission boundary, the
/// eight issue coverage areas, and the explicit safety boundaries.
const REQUIRED_SECTIONS: [&str; 10] = [
    "## Mission and trust boundary",
    "## Discovery",
    "## Safe targeting",
    "## Reading state",
    "## Subscribing to events",
    "## Sending prompts and waiting",
    "## Diffs and worktrees",
    "## Destructive operations",
    "## Blocked agents and approvals",
    "## Explicit safety boundaries",
];

/// Lower bound on the examples the extractor must find in the artifact. If the
/// extractor collects fewer, it is broken (not the artifact): fail loudly
/// instead of passing a vacuous parse check.
const MIN_EXPECTED_EXAMPLES: usize = 20;

fn artifact_body() -> &'static str {
    let without_prefix = EMBEDDED_SKILL
        .strip_prefix(SKILL_FRONTMATTER_PREFIX)
        .expect("embedded skill must start with the `name: pohunek` frontmatter");
    let body_start = without_prefix
        .find("\n---\n")
        .expect("embedded skill must terminate its frontmatter with `---`");
    &without_prefix[body_start + "\n---\n".len()..]
}

#[test]
fn embedded_artifact_has_valid_structure() {
    assert!(EMBEDDED_SKILL.starts_with(SKILL_FRONTMATTER_PREFIX));
    assert!(EMBEDDED_SKILL.contains(GENERATED_NOTICE));
    assert!(EMBEDDED_SKILL.contains(SOURCE_NOTICE));
    assert!(
        !artifact_body().trim().is_empty(),
        "embedded skill body must be nonempty"
    );
}

#[test]
fn embedded_artifact_covers_required_sections() {
    for section in REQUIRED_SECTIONS {
        assert!(
            EMBEDDED_SKILL.contains(section),
            "embedded skill is missing required section heading: {section}"
        );
    }
}

// --- CLI command output (no daemon running, no filesystem/network access) ----

fn agent_skill_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pohunek"));
    command.arg("agent-skill");
    command
}

#[test]
fn agent_skill_prints_the_embedded_bytes_exactly() {
    let output = agent_skill_command()
        .output()
        .expect("spawn pohunek agent-skill");
    assert!(
        output.status.success(),
        "agent-skill must exit successfully; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "agent-skill must write no human text: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        EMBEDDED_SKILL.as_bytes(),
        "agent-skill stdout must be byte-identical to the embedded artifact"
    );
}

#[test]
fn agent_skill_output_is_deterministic_across_invocations() {
    let first = agent_skill_command()
        .output()
        .expect("spawn first agent-skill run");
    let second = agent_skill_command()
        .output()
        .expect("spawn second agent-skill run");
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.stdout, EMBEDDED_SKILL.as_bytes());
}

#[test]
fn agent_skill_accepts_and_ignores_host() {
    let output = agent_skill_command()
        .args(["--host", "buildbox"])
        .output()
        .expect("spawn pohunek agent-skill with --host");
    assert!(
        output.status.success(),
        "agent-skill must accept the global --host flag; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        EMBEDDED_SKILL.as_bytes(),
        "agent-skill output must be identical with --host: the skill is embedded locally"
    );
}

#[test]
fn agent_skill_json_envelope_is_one_document_with_skill_and_hash() {
    let output = agent_skill_command()
        .arg("--json")
        .output()
        .expect("spawn pohunek agent-skill --json");
    assert!(
        output.status.success(),
        "agent-skill --json must exit successfully; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "agent-skill --json stdout stays machine-only, stderr carries no human text: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let doc: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|err| {
        panic!("stdout must be exactly one JSON document ({err}): {stdout:?}")
    });
    assert_eq!(doc["cli_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(doc["protocol"]["minimum"], protocol::PROTOCOL_VERSION.get());
    assert_eq!(doc["protocol"]["maximum"], protocol::PROTOCOL_VERSION.get());
    assert_eq!(doc["ok"]["skill"], EMBEDDED_SKILL);
    assert_eq!(doc["ok"]["content_sha256"], expected_content_sha256());
    assert!(doc.get("err").is_none());
}

fn expected_content_sha256() -> String {
    format!("{:x}", Sha256::digest(EMBEDDED_SKILL.as_bytes()))
}

// --- Parser coverage ---------------------------------------------------------

#[test]
fn agent_skill_command_tree_accepts_host_and_json() {
    for args in [
        vec!["pohunek", "agent-skill"],
        vec!["pohunek", "agent-skill", "--json"],
        vec!["pohunek", "--host", "buildbox", "agent-skill", "--json"],
    ] {
        pohunek_cli::command()
            .try_get_matches_from(args)
            .expect("agent-skill should parse, including with the global --host flag");
    }
}

#[test]
fn embedded_artifact_examples_parse_through_the_live_clap_tree() {
    let examples = collect_pohunek_examples(EMBEDDED_SKILL);
    let found = examples.len();
    assert!(
        found >= MIN_EXPECTED_EXAMPLES,
        "example extractor broke: only {found} examples in the artifact"
    );
    for example in &examples {
        parse_example(example.line, &example.command);
    }
}

/// Returns the `session <SUBCOMMAND>` matches when an example invokes one.
fn session_args(matches: &clap::ArgMatches) -> Option<(&str, &clap::ArgMatches)> {
    let (_, session) = matches
        .subcommand()
        .filter(|(name, _)| *name == "session")?;
    session.subcommand()
}

/// True when parsed `session new` matches pin an explicit coding-agent
/// profile: `--agent` supplied on the command line with a coding-agent value.
/// The flag has a `shell` default, so presence in the matches alone also
/// holds when the flag was never provided.
fn pins_coding_agent(new_args: &clap::ArgMatches) -> bool {
    new_args.value_source("agent") == Some(clap::parser::ValueSource::CommandLine)
        && matches!(
            new_args.get_one::<String>("agent").map(String::as_str),
            Some("codex" | "claude" | "hermes")
        )
}

/// True when parsed `session new` matches supply `--branch`, so the session
/// gets a dedicated worktree instead of running in the main checkout.
fn pins_dedicated_worktree(new_args: &clap::ArgMatches) -> bool {
    supplied_on_command_line(new_args, "branch")
}

/// True when parsed `session input` matches target the inspected
/// coding-agent placeholder exactly.
fn targets_inspected_coding_agent(input_args: &clap::ArgMatches) -> bool {
    let Some(values) = input_args.get_raw("target") else {
        return false;
    };
    let raw: Vec<&OsStr> = values.collect();
    raw == vec![OsStr::new("<coding-agent-target>")]
}

/// True when a flag was supplied on the command line. `contains_id` alone is
/// unreliable for guarded flags: `ArgAction::SetTrue` carries an implicit
/// `false` default, and any later `default_value` would silently weaken the
/// guard, so every safety predicate asserts the command-line source instead.
fn supplied_on_command_line(args: &clap::ArgMatches, id: &str) -> bool {
    args.value_source(id) == Some(clap::parser::ValueSource::CommandLine)
}

/// Input examples must never hand text to a shell: a `session new` that
/// injects text must pin an explicit agent profile, and a `session input` must
/// target the `<coding-agent-target>` placeholder an agent confirms with
/// `session inspect` first. The default `shell` agent and an unverified target
/// would execute injected text as shell commands.
#[test]
fn input_examples_pin_coding_agent_targets() {
    for example in collect_pohunek_examples(EMBEDDED_SKILL) {
        let Some(matches) = parse_example(example.line, &example.command) else {
            continue;
        };
        let Some((subcommand, args)) = session_args(&matches) else {
            continue;
        };
        match subcommand {
            "new" => {
                let sends_input = supplied_on_command_line(args, "input")
                    || supplied_on_command_line(args, "input_stdin");
                if sends_input {
                    assert!(
                        pins_coding_agent(args),
                        "artifact example at line {} sends input without an explicit \
                         coding-agent --agent and would feed the default `shell` agent \
                         with untrusted text",
                        example.line
                    );
                }
            }
            "input" => {
                assert!(
                    targets_inspected_coding_agent(args),
                    "artifact example at line {} must target an inspected \
                     coding-agent session; a shell session executes injected text as \
                     shell commands",
                    example.line
                );
            }
            _ => {}
        }
    }
}

/// `session rm` destroys the session's Pohunek-owned worktree with
/// `git worktree remove --force`, so uncommitted changes vanish. The artifact
/// must say so, treat `session diff` as incomplete (truncation omits files,
/// ignored files are never listed), and require a diff plus separate owner
/// confirmation first.
#[test]
fn session_rm_explanation_warns_about_worktree_destruction() {
    let explanation_start = EMBEDDED_SKILL
        .find("`session rm` removes the logical session")
        .expect("artifact must explain what session rm does");
    let explanation = &EMBEDDED_SKILL[explanation_start..];
    for required in [
        "Pohunek-owned worktree",
        "git worktree remove --force",
        "session diff",
        "ok.truncated",
        "git-ignored",
    ] {
        assert!(
            explanation.contains(required),
            "the session rm explanation must warn about `{required}`: removing a \
             session irreversibly deletes uncommitted worktree changes"
        );
    }
}

/// Discovery examples must re-probe explicitly: `host list` and
/// `host discover` serve a TTL-fresh cache without `--refresh`, so a
/// discovery that looks like a refresh can return the same stale list.
#[test]
fn discovery_reprobe_examples_pin_refresh() {
    for example in collect_pohunek_examples(EMBEDDED_SKILL) {
        let Some(matches) = parse_example(example.line, &example.command) else {
            continue;
        };
        let Some(("discover", args)) = matches
            .subcommand()
            .filter(|(name, _)| *name == "host")
            .and_then(|(_, host)| host.subcommand())
        else {
            continue;
        };
        assert!(
            supplied_on_command_line(args, "refresh"),
            "artifact example at line {} promises a re-probe but serves the \
             TTL-fresh cache without --refresh",
            example.line
        );
    }
}

/// `session new` examples must take `--branch`: without it the agent runs
/// in-place in the project's main checkout, where it can collide with the
/// owner's own edits or another session. In-place starts need explicit owner
/// consent and are outside the skill's recommended flows.
#[test]
fn session_new_examples_get_dedicated_worktrees() {
    for example in collect_pohunek_examples(EMBEDDED_SKILL) {
        let Some(matches) = parse_example(example.line, &example.command) else {
            continue;
        };
        let Some(("new", args)) = session_args(&matches) else {
            continue;
        };
        assert!(
            pins_dedicated_worktree(args),
            "artifact example at line {} omits --branch and would run the agent \
             in-place in the project's main checkout",
            example.line
        );
    }
}

/// The safety predicates must evaluate the real clap matches, so a remote
/// form (`--host` before the subcommand) cannot bypass them, and agent or
/// branch text inside a flag value cannot fake a match.
#[test]
fn safety_predicates_survive_remote_and_value_forms() {
    let remote_form = parse_example(0, "pohunek --host buildbox session new --input hi --json")
        .expect("the remote form parses against the live clap tree");
    let Some(("new", args)) = session_args(&remote_form) else {
        panic!("the remote form must resolve the session new subcommand");
    };
    assert!(args.contains_id("input"));
    assert!(
        !pins_coding_agent(args),
        "no explicit coding agent was provided, so the remote form must fail the \
         agent check"
    );
    assert!(
        !pins_dedicated_worktree(args),
        "no --branch was provided, so the remote form must fail the worktree check"
    );

    let shell_agent = parse_example(0, "pohunek session new --agent shell --input hi --json")
        .expect("the shell-agent form parses against the live clap tree");
    let Some(("new", args)) = session_args(&shell_agent) else {
        panic!("the shell-agent form must resolve the session new subcommand");
    };
    assert!(
        !pins_coding_agent(args),
        "--agent shell is not a coding agent"
    );

    let smuggled_value = parse_example(0, "pohunek session new --input=--agent --json")
        .expect("the smuggled-value form parses against the live clap tree");
    let Some(("new", args)) = session_args(&smuggled_value) else {
        panic!("the smuggled-value form must resolve the session new subcommand");
    };
    assert!(
        !pins_coding_agent(args),
        "agent text inside a flag value must not satisfy the agent check"
    );

    let value_form = parse_example(
        0,
        "pohunek session new --agent=codex --branch=main --input=hi --json",
    )
    .expect("the flag=value form parses against the live clap tree");
    let Some(("new", args)) = session_args(&value_form) else {
        panic!("the flag=value form must resolve the session new subcommand");
    };
    assert!(
        pins_coding_agent(args),
        "the flag=value form must pin the coding agent"
    );
    assert!(
        pins_dedicated_worktree(args),
        "the flag=value form must pin the worktree branch"
    );
}

/// Waited input (`--until`/`--timeout`) fails with
/// `session_input_wait_unsupported` for the default coding agents: codex,
/// claude, and hermes submit with a non-zero delay, and only zero-delay
/// profiles such as `shell` support it. The artifact must explain the
/// limitation and never recommend the waited form for a generic target.
#[test]
fn session_input_examples_avoid_waited_input_for_coding_agents() {
    for example in collect_pohunek_examples(EMBEDDED_SKILL) {
        let Some(matches) = parse_example(example.line, &example.command) else {
            continue;
        };
        let Some(("input", args)) = session_args(&matches) else {
            continue;
        };
        assert!(
            !supplied_on_command_line(args, "wait_until"),
            "artifact example at line {} uses waited input, which fails with \
             session_input_wait_unsupported for the default coding agents",
            example.line
        );
    }
    assert!(
        EMBEDDED_SKILL.contains("session_input_wait_unsupported"),
        "artifact must explain the waited-input limitation for non-zero-delay \
         agent profiles"
    );
}

/// The two observation traps: `session read` currently serves every requested
/// source from the visible screen (the fallback lands in `source_used`), and
/// `session wait` evaluates the session's current snapshot, so it is not a
/// delivery report for a prompt just sent.
#[test]
fn observation_warnings_are_pinned_in_the_artifact() {
    for required in ["source_used", "non-causal observation"] {
        assert!(
            EMBEDDED_SKILL.contains(required),
            "artifact must warn about `{required}`: a recent read may be the \
             visible screen and session wait can return before the new prompt \
             is processed"
        );
    }
}

/// Collects every `pohunek ...` example in the artifact: inline backticked
/// spans anywhere plus bare command lines inside fenced code blocks. This
/// mirrors the xtask `collect_pohunek_examples` scan without a regex
/// dependency so the CLI suite stays independent of xtask.
///
/// Each example carries its 1-based artifact line so failures can name the
/// location without echoing command text, which may hold sensitive values.
struct Example {
    line: usize,
    command: String,
}

fn collect_pohunek_examples(content: &str) -> Vec<Example> {
    let mut in_fence = false;
    let mut examples = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find("`pohunek ") {
            let span = &rest[start + "`".len()..];
            let Some(end) = span.find('`') else { break };
            examples.push(Example {
                line: line_number,
                command: span[..end].to_owned(),
            });
            rest = &span[end + "`".len()..];
        }
        if in_fence && trimmed.starts_with("pohunek ") {
            examples.push(Example {
                line: line_number,
                command: trimmed.to_owned(),
            });
        }
    }
    examples
}

/// Parses one artifact example against the live clap tree. Help and version
/// displays yield `None`; any other parse failure panics with the source
/// position and the clap error kind only, never the command text, which may
/// hold sensitive values.
fn parse_example(line: usize, example: &str) -> Option<clap::ArgMatches> {
    let tokens: Vec<&str> = example.split_whitespace().collect();
    match pohunek_cli::command().try_get_matches_from(&tokens) {
        Ok(matches) => Some(matches),
        Err(err)
            if err.kind() == clap::error::ErrorKind::DisplayHelp
                || err.kind() == clap::error::ErrorKind::DisplayVersion =>
        {
            None
        }
        Err(err) => panic!(
            "agent-skill artifact line {line} does not parse against the live clap \
             tree: {}",
            err.kind().as_str().unwrap_or("unknown parse error")
        ),
    }
}

#[test]
fn example_collector_finds_inline_spans_and_fenced_lines() {
    let content = concat!(
        "prose with `pohunek doctor --json` inline.\n",
        "```sh\n",
        "pohunek host list --json\n",
        "# not a command\n",
        "  pohunek session list --json\n",
        "```\n",
        "tail mentioning `--host` only.\n",
    );
    let examples = collect_pohunek_examples(content);
    let commands: Vec<&str> = examples
        .iter()
        .map(|example| example.command.as_str())
        .collect();
    assert_eq!(
        commands,
        [
            "pohunek doctor --json",
            "pohunek host list --json",
            "pohunek session list --json"
        ]
    );
    let lines: Vec<usize> = examples.iter().map(|example| example.line).collect();
    assert_eq!(lines, [1, 3, 5]);
}

#[test]
fn example_collector_ignores_prose_and_unterminated_spans() {
    // Prose outside fences never becomes a candidate, and a backticked span
    // with no closing backtick is not a complete example.
    let content = "pohunek in prose is not a command\n`pohunek unterminated\n";
    assert!(collect_pohunek_examples(content).is_empty());
}
