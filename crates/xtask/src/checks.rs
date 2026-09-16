//! Drift checks for docs and command references.
use std::path::Path;

use clap::error::ErrorKind;
use regex::Regex;

use crate::agent_skill;
use crate::hermes_skill;
use crate::{
    collect_files, create_dir_all, remove_dir_all, repo_root, validate_docs, BuildOptions,
    XtaskError,
};

use cli::command;

/// Parse a single `pohunek ...` command against the live CLI parser.
///
/// `pohunek --help` and `pohunek --version` are valid outcomes.
pub(crate) fn parse_pohunek_command(cmd_str: &str) -> Result<(), String> {
    let tokens: Vec<String> = cmd_str.split_whitespace().map(str::to_string).collect();
    if tokens.first().map(String::as_str) != Some("pohunek") {
        return Err("command must start with exact `pohunek` binary".to_owned());
    }

    match command().try_get_matches_from(&tokens) {
        Ok(_) => Ok(()),
        Err(err) => {
            if err.kind() == ErrorKind::DisplayHelp || err.kind() == ErrorKind::DisplayVersion {
                Ok(())
            } else {
                Err(err
                    .kind()
                    .as_str()
                    .unwrap_or("unknown parse error")
                    .to_string())
            }
        }
    }
}

pub(crate) fn check_docs(
    source_dir: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
) -> Result<bool, XtaskError> {
    let source_dir = source_dir.as_ref();
    let output_root = output_root.as_ref();
    let repo = repo_root();

    let mut all_pass = true;
    all_pass &= check_schema_validation(source_dir);
    all_pass &= check_deterministic_build(source_dir, output_root)?;
    all_pass &= check_source_map_paths(source_dir, &repo);
    all_pass &= hermes_skill::check(&repo)?;
    all_pass &= check_generated_skill_documentation(&repo)?;
    all_pass &= agent_skill::check(&repo)?;
    all_pass &= check_agent_skill_commands(&repo)?;
    all_pass &= check_runbook_commands(source_dir)?;
    all_pass &= check_secret_scan(source_dir, output_root)?;
    all_pass &= check_release_extras(&repo);

    Ok(all_pass)
}

fn check_schema_validation(source_dir: &Path) -> bool {
    match validate_docs(source_dir) {
        Ok(summary) => {
            println!(
                "[PASS] schema-validation: {} files, {} concepts, schema v{}",
                summary.files_checked, summary.concept_count, summary.schema_version
            );
            true
        }
        Err(err) => {
            println!("[FAIL] schema-validation: {err}");
            false
        }
    }
}

fn check_deterministic_build(source_dir: &Path, output_root: &Path) -> Result<bool, XtaskError> {
    let mut all_pass = true;
    let build1_root = output_root.join("check-build-1");
    let build2_root = output_root.join("check-build-2");

    if build1_root.exists() {
        remove_dir_all(&build1_root)?;
    }
    if build2_root.exists() {
        remove_dir_all(&build2_root)?;
    }
    create_dir_all(&build1_root)?;
    create_dir_all(&build2_root)?;

    let version = env!("CARGO_PKG_VERSION").to_string();
    let build1 = crate::build_docs(BuildOptions {
        source_dir: source_dir.to_path_buf(),
        output_root: build1_root.clone(),
        pohunek_version: version.clone(),
    });
    let build2 = crate::build_docs(BuildOptions {
        source_dir: source_dir.to_path_buf(),
        output_root: build2_root.clone(),
        pohunek_version: version,
    });

    match (build1, build2) {
        (Ok(s1), Ok(s2)) => {
            if s1.content_hash == s2.content_hash {
                println!(
                    "[PASS] deterministic-build: both runs produced hash {}",
                    s1.content_hash
                );
            } else {
                println!(
                    "[FAIL] deterministic-build: hashes differ: {} vs {}",
                    s1.content_hash, s2.content_hash
                );
                all_pass = false;
            }
        }
        (Err(err), _) | (_, Err(err)) => {
            println!("[FAIL] deterministic-build: build failed: {err}");
            all_pass = false;
        }
    }

    if build1_root.exists() {
        remove_dir_all(&build1_root)?;
    }
    if build2_root.exists() {
        remove_dir_all(&build2_root)?;
    }

    Ok(all_pass)
}

fn check_source_map_paths(source_dir: &Path, repo: &Path) -> bool {
    let source_map_path = source_dir.join("assistant/source-map.md");
    match std::fs::read_to_string(&source_map_path) {
        Ok(content) => {
            let missing = missing_documented_paths(&content, repo);

            if missing.is_empty() {
                println!("[PASS] source-map-paths: all referenced paths exist");
                true
            } else {
                println!(
                    "[FAIL] source-map-paths: {} missing path(s):",
                    missing.len()
                );
                for path in &missing {
                    println!("        {path}");
                }
                false
            }
        }
        Err(source) => {
            println!(
                "[FAIL] source-map-paths: could not read {}: {source}",
                source_map_path.display()
            );
            false
        }
    }
}

fn check_generated_skill_documentation(repo: &Path) -> Result<bool, XtaskError> {
    let Some(bytes) = hermes_skill::read_checked(repo)? else {
        // `hermes_skill::check` reports the missing artifact and the remediation.
        return Ok(true);
    };
    let Ok(content) = std::str::from_utf8(&bytes) else {
        println!("[FAIL] hermes-skill-documentation: generated skill is not UTF-8");
        return Ok(false);
    };
    let missing = missing_documented_paths(content, repo);
    let secret_hits = secret_hits(content, hermes_skill::GENERATED_PATH);
    if missing.is_empty() && secret_hits.is_empty() {
        println!("[PASS] hermes-skill-documentation: backtick paths and secret scan passed");
        return Ok(true);
    }
    if !missing.is_empty() {
        println!(
            "[FAIL] hermes-skill-documentation: {} missing backtick path(s):",
            missing.len()
        );
        for path in missing {
            println!("        {path}");
        }
    }
    if !secret_hits.is_empty() {
        println!(
            "[FAIL] hermes-skill-documentation: {} potential secret(s):",
            secret_hits.len()
        );
        for hit in secret_hits {
            println!("        {hit}");
        }
    }
    Ok(false)
}

fn missing_documented_paths(content: &str, repo: &Path) -> Vec<String> {
    let backtick_re = Regex::new(r"`([^`]+)`").expect("valid backtick regex");
    backtick_re
        .captures_iter(content)
        .filter_map(|captures| {
            let candidate = captures[1].to_string();
            (candidate.starts_with("crates/") || candidate.starts_with("docs/"))
                .then_some(candidate)
        })
        .filter(|candidate| !repo.join(candidate).exists())
        .collect()
}

/// Checks agent-skill source examples against the live CLI parser and the
/// generated artifact for secret patterns and nonexistent backtick paths.
///
/// Every inline backticked or fenced `pohunek ...` example in the skill source
/// must parse against the live clap tree, and the checked-in generated artifact
/// must carry no secret-pattern hit and no backticked `crates/` or `docs/` path
/// that does not exist. A missing artifact is reported by `agent_skill::check`,
/// so the artifact scan is skipped here when it does not exist yet.
fn check_agent_skill_commands(repo: &Path) -> Result<bool, XtaskError> {
    let mut all_pass = true;

    let source_path = repo.join(agent_skill::SOURCE_PATH);
    match std::fs::read_to_string(&source_path) {
        Ok(content) => {
            let examples = collect_pohunek_examples(&content);
            let failures: Vec<String> = examples
                .iter()
                .filter_map(|example| {
                    parse_pohunek_command(example)
                        .err()
                        .map(|message| format!("command `{example}` failed to parse: {message}"))
                })
                .collect();
            if failures.is_empty() {
                println!(
                    "[PASS] agent-skill-commands: {} example(s) parsed successfully",
                    examples.len()
                );
            } else {
                println!(
                    "[FAIL] agent-skill-commands: {} example(s) failed to parse (checked {}):",
                    failures.len(),
                    examples.len()
                );
                for failure in &failures {
                    println!("        {failure}");
                }
                all_pass = false;
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "[FAIL] agent-skill-commands: could not read {}: file not found",
                source_path.display()
            );
            all_pass = false;
        }
        Err(source) => {
            return Err(XtaskError::Io {
                path: source_path,
                source,
            })
        }
    }

    let Some(bytes) = agent_skill::read_checked(repo)? else {
        // `agent_skill::check` reports the missing artifact and the remediation.
        return Ok(all_pass);
    };
    let Ok(content) = std::str::from_utf8(&bytes) else {
        println!("[FAIL] agent-skill-documentation: generated skill is not UTF-8");
        return Ok(false);
    };
    let missing = missing_documented_paths(content, repo);
    let secret_hits = secret_hits(content, agent_skill::GENERATED_PATH);
    if missing.is_empty() && secret_hits.is_empty() {
        println!("[PASS] agent-skill-documentation: backtick paths and secret scan passed");
        return Ok(all_pass);
    }
    if !missing.is_empty() {
        println!(
            "[FAIL] agent-skill-documentation: {} missing backtick path(s):",
            missing.len()
        );
        for path in missing {
            println!("        {path}");
        }
    }
    if !secret_hits.is_empty() {
        println!(
            "[FAIL] agent-skill-documentation: {} potential secret(s):",
            secret_hits.len()
        );
        for hit in secret_hits {
            println!("        {hit}");
        }
    }
    Ok(false)
}

/// Collects every `pohunek ...` example in the skill source: inline backticked
/// spans anywhere plus bare command lines inside fenced code blocks. Prose
/// outside fences never becomes a candidate, so wrapped sentences starting
/// with the binary name are not misread as commands.
fn collect_pohunek_examples(content: &str) -> Vec<String> {
    let backtick_cmd_re =
        Regex::new(r"`(pohunek [^`]+)`").expect("valid agent-skill backtick command regex");
    let fence_re = Regex::new(r"^(```|~~~)").expect("valid code fence regex");
    let mut in_fence = false;
    let mut examples = Vec::new();
    for line in content.lines() {
        if fence_re.is_match(line.trim_start()) {
            in_fence = !in_fence;
            continue;
        }
        for capture in backtick_cmd_re.captures_iter(line) {
            examples.push(capture[1].to_string());
        }
        if in_fence {
            let trimmed = line.trim_start();
            if trimmed.starts_with("pohunek ") {
                examples.push(trimmed.to_string());
            }
        }
    }
    examples
}

const REQUIRED_RELEASE_EXTRAS: [&str; 2] = ["README.md", "LICENSE"];

fn check_release_extras(repo: &Path) -> bool {
    let missing = missing_release_extras(repo);
    if missing.is_empty() {
        println!("[PASS] release-extras: required release files exist");
        true
    } else {
        println!(
            "[FAIL] release-extras: {} required file(s) missing:",
            missing.len()
        );
        for path in missing {
            println!("        {path}");
        }
        false
    }
}

fn missing_release_extras(repo: &Path) -> Vec<&'static str> {
    REQUIRED_RELEASE_EXTRAS
        .iter()
        .copied()
        .filter(|relative| !repo.join(relative).is_file())
        .collect()
}

fn check_runbook_commands(source_dir: &Path) -> Result<bool, XtaskError> {
    let mut all_pass = true;

    let runbooks_dir = source_dir.join("runbooks");
    let mut runbook_failures: Vec<String> = Vec::new();
    let mut runbook_checked = 0usize;
    let backtick_cmd_re =
        Regex::new(r"`(pohunek [^`]+)`").expect("valid runbook backtick command regex");

    if runbooks_dir.exists() {
        let entries = collect_files(&runbooks_dir)?;
        for entry in entries {
            if entry.source_path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let content = match std::fs::read_to_string(&entry.source_path) {
                Ok(c) => c,
                Err(source) => {
                    runbook_failures.push(format!(
                        "{}: could not read file: {source}",
                        entry.source_path.display()
                    ));
                    continue;
                }
            };

            for line in content.lines() {
                let mut candidates: Vec<String> = Vec::new();
                for cap in backtick_cmd_re.captures_iter(line) {
                    candidates.push(cap[1].to_string());
                }

                let trimmed = line
                    .trim_start_matches(|c: char| {
                        c.is_ascii_whitespace()
                            || c == '-'
                            || c == '*'
                            || c == '+'
                            || c.is_ascii_digit()
                            || c == '.'
                    })
                    .trim_start();
                if trimmed.starts_with("pohunek ") && !backtick_cmd_re.is_match(line) {
                    candidates.push(trimmed.to_string());
                }

                for cmd_str in candidates {
                    if cmd_str.contains('<') {
                        continue;
                    }
                    runbook_checked += 1;
                    if let Err(message) = parse_pohunek_command(&cmd_str) {
                        runbook_failures.push(format!(
                            "{}: command `{cmd_str}` failed to parse: {message}",
                            entry.source_path.display(),
                        ));
                    }
                }
            }
        }
    }

    if runbook_failures.is_empty() {
        println!("[PASS] runbook-commands: {runbook_checked} command(s) parsed successfully");
    } else {
        println!(
            "[FAIL] runbook-commands: {} command(s) failed to parse (checked {runbook_checked}):",
            runbook_failures.len()
        );
        for failure in &runbook_failures {
            println!("        {failure}");
        }
        all_pass = false;
    }

    Ok(all_pass)
}

fn check_secret_scan(source_dir: &Path, output_root: &Path) -> Result<bool, XtaskError> {
    let mut all_pass = true;

    let bundle_root = output_root.join("check-secret-scan");
    if bundle_root.exists() {
        remove_dir_all(&bundle_root)?;
    }
    create_dir_all(&bundle_root)?;

    let secret_scan_result = crate::build_docs(BuildOptions {
        source_dir: source_dir.to_path_buf(),
        output_root: bundle_root.clone(),
        pohunek_version: env!("CARGO_PKG_VERSION").to_string(),
    });

    match secret_scan_result {
        Err(err) => {
            println!("[FAIL] secret-scan: build for scan failed: {err}");
            all_pass = false;
        }
        Ok(build_summary) => {
            let bundle_files = collect_files(&build_summary.bundle_dir)?;
            let mut hits: Vec<String> = Vec::new();
            for file in &bundle_files {
                if file.source_path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&file.source_path) else {
                    continue;
                };
                hits.extend(secret_hits(
                    &content,
                    &file.source_path.display().to_string(),
                ));
            }

            if bundle_root.exists() {
                remove_dir_all(&bundle_root)?;
            }

            if hits.is_empty() {
                println!(
                    "[PASS] secret-scan: no credential patterns found in {} bundle file(s)",
                    bundle_files.len()
                );
            } else {
                println!(
                    "[FAIL] secret-scan: {} potential secret(s) found:",
                    hits.len()
                );
                for hit in &hits {
                    println!("        {hit}");
                }
                all_pass = false;
            }
        }
    }

    Ok(all_pass)
}

const SECRET_PATTERNS: [(&str, &str); 5] = [
    (r"(?i)api[_-]?key\s*[:=]\s*\S+", "api-key assignment"),
    (r"(?i)token\s*[:=]\s*\S+", "token assignment"),
    (
        r"-----BEGIN [A-Z ]+PRIVATE KEY-----",
        "PEM private key header",
    ),
    (r"(?i)^\s*\[env\]\s*$", "TOML [env] section header"),
    (
        r"(?i)(secret|password|passwd|api_key|private_key|auth_token|access_token)\s*[:=]\s*\S{8,}",
        "credential assignment",
    ),
];

fn secret_hits(content: &str, display_path: &str) -> Vec<String> {
    let compiled: Vec<(Regex, &str)> = SECRET_PATTERNS
        .iter()
        .map(|(pattern, label)| {
            (
                Regex::new(pattern).expect("secret scan regex is valid"),
                *label,
            )
        })
        .collect();
    let mut hits = Vec::new();
    for (index, line) in content.lines().enumerate() {
        for (regex, label) in &compiled {
            if regex.is_match(line) {
                hits.push(format!("{display_path}:{}: [{label}]", index + 1));
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs};

    use super::{
        check_agent_skill_commands, collect_pohunek_examples, missing_release_extras,
        parse_pohunek_command, secret_hits,
    };

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "pohunek-xtask-checks-{tag}-{nanos}-{}",
            std::process::id()
        ))
    }

    #[test]
    fn collect_pohunek_examples_covers_inline_spans_and_fenced_lines_only() {
        const INLINE_COMMAND: &str = "pohunek doctor --json";
        let content = format!(
            "---\ntype: Guide\n---\n\nIntro with `{INLINE_COMMAND}` inline.\n\n```sh\npohunek host list --json\n# resolve first\npohunek session list --json\n```\n\nProse wrap: {INLINE_COMMAND} stays plain text and is not a candidate.\n\n```sh\npohunek made-up-command\n```\n"
        );

        let examples = collect_pohunek_examples(&content);

        assert_eq!(
            examples,
            vec![
                INLINE_COMMAND.to_owned(),
                "pohunek host list --json".to_owned(),
                "pohunek session list --json".to_owned(),
                "pohunek made-up-command".to_owned(),
            ]
        );
    }

    #[test]
    fn check_agent_skill_commands_reports_example_and_artifact_drift() {
        const SENTINEL: &str = "never-expose-this-secret-sentinel";
        let root = temp_root("agent-skill");
        fs::create_dir_all(&root).expect("create temp root");

        assert!(
            !check_agent_skill_commands(&root).expect("missing source is a failure, not an error")
        );

        let source_path = root.join(crate::agent_skill::SOURCE_PATH);
        fs::create_dir_all(source_path.parent().expect("source parent")).expect("source parent");
        fs::write(
            &source_path,
            "# skill\n\nRun `pohunek doctor --json` first.\n\n```sh\npohunek host list --json\n```\n",
        )
        .expect("write source");
        assert!(check_agent_skill_commands(&root).expect("valid examples pass without artifact"));

        fs::write(&source_path, "# skill\n\n`pohunek made-up-command`\n")
            .expect("write unparsable source");
        assert!(!check_agent_skill_commands(&root)
            .expect("unparsable example is a failure, not an error"));

        fs::write(
            &source_path,
            "# skill\n\nRun `pohunek doctor --json` first.\n",
        )
        .expect("write valid source");
        let artifact_path = root.join(crate::agent_skill::GENERATED_PATH);
        fs::create_dir_all(artifact_path.parent().expect("artifact parent"))
            .expect("artifact parent");
        fs::write(&artifact_path, "see `crates/does-not-exist.md`\n").expect("write artifact");
        assert!(!check_agent_skill_commands(&root).expect("missing backtick path is a failure"));

        fs::write(&artifact_path, format!("env\napi_key={SENTINEL}\n")).expect("write artifact");
        assert!(!check_agent_skill_commands(&root).expect("secret hit is a failure"));

        fs::write(&artifact_path, "clean generated body\n").expect("write clean artifact");
        assert!(check_agent_skill_commands(&root).expect("clean artifact passes"));

        fs::remove_dir_all(&root).expect("remove temp root");
    }

    #[test]
    fn missing_release_extras_reports_required_files() {
        let root = temp_root("release-extras");
        fs::create_dir_all(&root).expect("create temp root");
        assert_eq!(missing_release_extras(&root), vec!["README.md", "LICENSE"]);

        let readme = root.join("README.md");
        fs::write(&readme, "readme\n").expect("write README");
        assert_eq!(missing_release_extras(&root), vec!["LICENSE"]);

        let license = root.join("LICENSE");
        fs::write(&license, "license\n").expect("write LICENSE");
        assert!(missing_release_extras(&root).is_empty());

        fs::remove_dir_all(&root).expect("remove temp root");
    }

    #[test]
    fn parse_pohunek_command_rejects_unknown_binaries_and_preserves_help_as_success() {
        assert_eq!(parse_pohunek_command("pohunek doctor --json"), Ok(()));
        assert_eq!(parse_pohunek_command("pohunek --help"), Ok(()));
        assert_eq!(parse_pohunek_command("pohunek --version"), Ok(()));
        assert_eq!(
            parse_pohunek_command("other doctor").expect_err("rejects non-pohunek binary"),
            "command must start with exact `pohunek` binary"
        );
        let err = parse_pohunek_command("pohunek made-up-command")
            .expect_err("rejects unknown subcommand");
        assert!(
            err.contains("unrecognized"),
            "unknown command error should come from clap: {err}"
        );
    }

    #[test]
    fn secret_hits_never_include_matched_content() {
        const SENTINEL: &str = "never-expose-this-secret-sentinel";
        let content = format!("safe line\napi_key={SENTINEL}\n");

        let hits = secret_hits(&content, "docs/example.md");

        assert!(!hits.is_empty());
        assert!(hits
            .iter()
            .all(|hit| hit.starts_with("docs/example.md:2: [")));
        assert!(hits.iter().all(|hit| !hit.contains(SENTINEL)));
        assert!(hits.iter().all(|hit| !hit.contains("api_key")));
    }
}
