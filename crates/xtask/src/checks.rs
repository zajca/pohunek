//! Drift checks for docs and command references.
use std::path::Path;

use clap::error::ErrorKind;
use regex::Regex;

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
            let backtick_re = Regex::new(r"`([^`]+)`").expect("valid backtick regex");
            let mut missing: Vec<String> = Vec::new();

            for captures in backtick_re.captures_iter(&content) {
                let candidate = captures[1].to_string();
                if candidate.starts_with("crates/") || candidate.starts_with("docs/") {
                    let full_path = repo.join(&candidate);
                    if !full_path.exists() {
                        missing.push(candidate);
                    }
                }
            }

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
            let patterns: &[(&str, &str)] = &[
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

            let compiled: Vec<(Regex, &str)> = patterns
                .iter()
                .map(|(pat, label)| (Regex::new(pat).expect("secret scan regex is valid"), *label))
                .collect();

            let bundle_files = collect_files(&build_summary.bundle_dir)?;
            let mut secret_hits: Vec<String> = Vec::new();
            for file in &bundle_files {
                if file.source_path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Ok(content) = std::fs::read_to_string(&file.source_path) else {
                    continue;
                };
                for (i, line) in content.lines().enumerate() {
                    for (re, label) in &compiled {
                        if re.is_match(line) {
                            secret_hits.push(format!(
                                "{}:{}: [{}] {}",
                                file.source_path.display(),
                                i + 1,
                                label,
                                line.trim()
                            ));
                        }
                    }
                }
            }

            if bundle_root.exists() {
                remove_dir_all(&bundle_root)?;
            }

            if secret_hits.is_empty() {
                println!(
                    "[PASS] secret-scan: no credential patterns found in {} bundle file(s)",
                    bundle_files.len()
                );
            } else {
                println!(
                    "[FAIL] secret-scan: {} potential secret(s) found:",
                    secret_hits.len()
                );
                for hit in &secret_hits {
                    println!("        {hit}");
                }
                all_pass = false;
            }
        }
    }

    Ok(all_pass)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, fs};

    use super::missing_release_extras;
    use super::parse_pohunek_command;

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
}
