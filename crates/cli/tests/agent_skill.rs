//! Guards the checked-in agent skill artifact that the CLI embeds verbatim.
//!
//! These assertions run under `cargo test -p pohunek-cli`, independent of
//! xtask, so a stale or truncated artifact fails the CLI suite directly, and
//! every `pohunek ...` example inside the artifact must parse against the live
//! clap tree.

// Rust guideline compliant 2026-09-16

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
        let tokens: Vec<&str> = example.split_whitespace().collect();
        match pohunek_cli::command().try_get_matches_from(&tokens) {
            Ok(_) => {}
            Err(err)
                if err.kind() == clap::error::ErrorKind::DisplayHelp
                    || err.kind() == clap::error::ErrorKind::DisplayVersion => {}
            Err(err) => panic!(
                "artifact example `{example}` does not parse against the live clap tree: {}",
                err.kind().as_str().unwrap_or("unknown parse error")
            ),
        }
    }
}

/// Input examples must never hand text to a shell: a `session new` that
/// injects text must pin an explicit agent profile, and a `session input` must
/// target the `<coding-agent-target>` placeholder an agent confirms with
/// `session inspect` first. The default `shell` agent and an unverified target
/// would execute injected text as shell commands.
#[test]
fn input_examples_pin_coding_agent_targets() {
    for example in collect_pohunek_examples(EMBEDDED_SKILL) {
        if example.starts_with("pohunek session new") && example.contains("--input") {
            assert!(
                example.contains("--agent"),
                "example `{example}` sends input without an explicit --agent and would \
                 feed the default `shell` agent with untrusted text"
            );
        }
        if example.starts_with("pohunek session input") {
            assert!(
                example.contains("<coding-agent-target>"),
                "example `{example}` must target an inspected coding-agent session; \
                 a shell session executes injected text as shell commands"
            );
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
        if example.starts_with("pohunek host discover") {
            assert!(
                example.contains("--refresh"),
                "example `{example}` promises a re-probe but serves the TTL-fresh \
                 cache without --refresh"
            );
        }
    }
}

/// `session new` examples must take `--branch`: without it the agent runs
/// in-place in the project's main checkout, where it can collide with the
/// owner's own edits or another session. In-place starts need explicit owner
/// consent and are outside the skill's recommended flows.
#[test]
fn session_new_examples_get_dedicated_worktrees() {
    for example in collect_pohunek_examples(EMBEDDED_SKILL) {
        if example.starts_with("pohunek session new") {
            assert!(
                example.contains("--branch"),
                "example `{example}` omits --branch and would run the agent \
                 in-place in the project's main checkout"
            );
        }
    }
}

/// Waited input (`--until`/`--timeout`) fails with
/// `session_input_wait_unsupported` for the default coding agents: codex,
/// claude, and hermes submit with a non-zero delay, and only zero-delay
/// profiles such as `shell` support it. The artifact must explain the
/// limitation and never recommend the waited form for a generic target.
#[test]
fn session_input_examples_avoid_waited_input_for_coding_agents() {
    for example in collect_pohunek_examples(EMBEDDED_SKILL) {
        if example.starts_with("pohunek session input") {
            assert!(
                !example.contains("--until"),
                "example `{example}` uses waited input, which fails with \
                 session_input_wait_unsupported for the default coding agents"
            );
        }
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
fn collect_pohunek_examples(content: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut examples = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find("`pohunek ") {
            let span = &rest[start + "`".len()..];
            let Some(end) = span.find('`') else { break };
            examples.push(span[..end].to_owned());
            rest = &span[end + "`".len()..];
        }
        if in_fence && trimmed.starts_with("pohunek ") {
            examples.push(trimmed.to_owned());
        }
    }
    examples
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
    assert_eq!(
        collect_pohunek_examples(content),
        [
            "pohunek doctor --json".to_owned(),
            "pohunek host list --json".to_owned(),
            "pohunek session list --json".to_owned()
        ]
    );
}

#[test]
fn example_collector_ignores_prose_and_unterminated_spans() {
    // Prose outside fences never becomes a candidate, and a backticked span
    // with no closing backtick is not a complete example.
    let content = "pohunek in prose is not a command\n`pohunek unterminated\n";
    assert!(collect_pohunek_examples(content).is_empty());
}
