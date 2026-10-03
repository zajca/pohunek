//! `pohunek setup config` — materialize the default config and prompt templates.
//!
//! All operations are local filesystem writes (no daemon involvement), mirroring
//! the `doctor` command's shape (sync, takes `&Paths`, renders human or `--json`).
//! The command embeds the default config and prompt templates at build time
//! (string constants) and writes them into the user's XDG config dir:
//!
//! - `setup config` writes a default `attach.conf` plus the `prompts/issue.tmpl`
//!   and `prompts/pr.tmpl` templates the daemon resolves for project actions,
//!   never overwriting an existing file unless `--force` is given.
//! - `setup` (no subcommand) is `setup config` without `--force`.
//! - `setup completions` delegates to the completion module and writes one
//!   shell's generated script into its conventional per-user directory.

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use serde::Serialize;

use crate::commands::attach::ATTACH_CONF_FILE;
use crate::error::CliError;
use crate::paths::Paths;

// Rust guideline compliant 2026-09-30

/// Default `attach.conf` contents read by `pohunek attach`. The keys match the
/// ones `AttachConfig::load_from_config_dir` accepts and every value is
/// commented out at its built-in default, so a fresh install changes no behavior.
pub(crate) const ATTACH_CONF: &str = "# pohunek attach configuration.
# Lines are key=value; '#' starts a comment. Uncomment a line to override it.

# Attach reconnect: after an unexpected stream close, retry with one shared time
# window, linear backoff from the interval, and a bounded number of attempts.
# Set seconds to 0 to disable.
#attach_reconnect_seconds=20
#attach_reconnect_interval_seconds=0.5
#attach_reconnect_max_attempts=3
";

/// Default `prompts/issue.tmpl`. May only reference the variables
/// `pohunek prompt render` supplies: `provider, id, number, title, body, branch, url`.
const ISSUE_TMPL: &str = "You are working on ${provider} issue ${id}: ${title}

## Context
${body}

## Working agreement
- Work on branch `${branch}` — it is already checked out in this worktree.
- Treat the description above as the source of truth for acceptance criteria.
  If any criteria are only implicit, restate them explicitly before you start.
- Implement the change end to end: code, tests, and any docs it requires.
- Run the project's checks (build, lint, tests) and make them pass before
  you consider the work done.

## When done
Summarize what you changed, how you verified it, and anything still open.
Link: ${url}
";

/// Default `prompts/pr.tmpl`. Same variable constraint as [`ISSUE_TMPL`].
const PR_TMPL: &str = "You are continuing ${provider} PR #${number}: ${title}

${body}

Branch: ${branch}
Link: ${url}

Please address the outstanding work on this PR, then summarize what you did.
";

/// Result of `setup config`: which files were created vs left untouched.
#[derive(Debug, Serialize)]
struct ConfigResult {
    created: Vec<String>,
    skipped: Vec<String>,
}

/// Write a default `attach.conf` and prompt templates.
///
/// # Errors
///
/// Returns [`CliError::Io`] if a directory cannot be created or a file cannot be
/// written.
pub(crate) fn run_config(paths: &Paths, force: bool, json: bool) -> Result<(), CliError> {
    let result = install_config(paths, force)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_config_human(&result));
    }
    Ok(())
}

// --- core (filesystem) logic ------------------------------------------------

/// Write the default config + prompt templates, skipping any file that already
/// exists unless `force` is set. Tracks created vs skipped so a re-run is
/// transparent about what it touched.
fn install_config(paths: &Paths, force: bool) -> Result<ConfigResult, CliError> {
    let prompts_dir = paths.config_dir.join("prompts");
    fs::create_dir_all(&prompts_dir)?;

    let files: &[(PathBuf, &str)] = &[
        (paths.config_dir.join(ATTACH_CONF_FILE), ATTACH_CONF),
        (prompts_dir.join("issue.tmpl"), ISSUE_TMPL),
        (prompts_dir.join("pr.tmpl"), PR_TMPL),
    ];

    let mut created = Vec::new();
    let mut skipped = Vec::new();
    for (path, body) in files {
        // Preserve user edits: only write when the file is absent or `force` is
        // set. `try_exists` distinguishes "absent" from "cannot tell", surfacing
        // the latter as an error rather than silently overwriting.
        if !force && path.try_exists()? {
            skipped.push(path.display().to_string());
            continue;
        }
        fs::write(path, body)?;
        created.push(path.display().to_string());
    }
    Ok(ConfigResult { created, skipped })
}

// --- human rendering --------------------------------------------------------

/// Render the config result, distinguishing freshly created from skipped files.
fn render_config_human(result: &ConfigResult) -> String {
    let mut out = String::new();
    for path in &result.created {
        let _ = writeln!(out, "created: {path}");
    }
    for path in &result.skipped {
        let _ = writeln!(out, "skipped (exists): {path}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Paths` rooted in a private temp dir, which the guard removes when it
    /// drops, also when a test fails.
    struct TempPaths {
        paths: Paths,
        _guard: tempfile::TempDir,
    }

    /// Build a `Paths` whose dirs all live under a fresh temp directory, so tests
    /// write real files without touching the user's environment.
    fn temp_paths() -> TempPaths {
        let guard = pohunek_test_support::tempdir().expect("create fixture directory");
        let root = guard.path().to_path_buf();
        let config_home = root.join("config");
        let data_dir = root.join("data");
        let paths = Paths {
            runtime_dir: root.join("runtime"),
            socket: root.join("runtime").join("daemon.sock"),
            data_dir: data_dir.clone(),
            log_dir: root.join("logs"),
            cache_dir: root.join("cache"),
            config_home: config_home.clone(),
            config_dir: config_home.join("pohunek"),
            origin_source: pohunek_client::OriginSource::Omitted,
        };
        TempPaths {
            paths,
            _guard: guard,
        }
    }

    #[test]
    fn install_config_creates_then_skips_then_force_rewrites() {
        let tp = temp_paths();

        // First run creates all three files.
        let first = install_config(&tp.paths, false).expect("first config");
        assert_eq!(first.created.len(), 3, "first run creates 3 files");
        assert!(first.skipped.is_empty());

        let conf = tp.paths.config_dir.join(ATTACH_CONF_FILE);
        assert!(conf.is_file());
        let prompts_dir = tp.paths.config_dir.join("prompts");
        assert!(prompts_dir.join("issue.tmpl").is_file());
        assert!(prompts_dir.join("pr.tmpl").is_file());

        // A user edit must survive a non-forced re-run.
        fs::write(&conf, "user-edited").expect("user edit");
        let second = install_config(&tp.paths, false).expect("second config");
        assert!(second.created.is_empty(), "non-forced run creates nothing");
        assert_eq!(second.skipped.len(), 3, "all three are skipped");
        assert_eq!(
            fs::read_to_string(&conf).expect("read conf"),
            "user-edited",
            "non-forced run preserved the user edit"
        );

        // `force` rewrites, restoring the default content.
        let third = install_config(&tp.paths, true).expect("forced config");
        assert_eq!(third.created.len(), 3, "forced run rewrites all three");
        assert!(third.skipped.is_empty());
        assert_eq!(
            fs::read_to_string(&conf).expect("read conf"),
            ATTACH_CONF,
            "forced run restored the default config"
        );
    }

    #[test]
    fn attach_conf_template_comments_out_every_default() {
        for key in [
            "attach_reconnect_seconds=20",
            "attach_reconnect_interval_seconds=0.5",
            "attach_reconnect_max_attempts=3",
        ] {
            assert!(
                ATTACH_CONF.contains(&format!("#{key}")),
                "attach.conf template lacks commented default {key:?}"
            );
        }
        assert!(
            ATTACH_CONF.lines().all(|line| {
                let line = line.trim();
                line.is_empty() || line.starts_with('#')
            }),
            "every attach.conf template line must be a comment"
        );
    }

    #[test]
    fn templates_only_reference_known_variables() {
        // `pohunek prompt render` rejects any ${var} outside this set, so guard
        // it here rather than discovering it when a launcher renders the prompt.
        const KNOWN: &[&str] = &["provider", "id", "number", "title", "body", "branch", "url"];
        for (label, tmpl) in [("issue", ISSUE_TMPL), ("pr", PR_TMPL)] {
            let mut rest = tmpl;
            while let Some(start) = rest.find("${") {
                let after = &rest[start + 2..];
                let end = after.find('}').expect("unterminated ${ in template");
                let var = &after[..end];
                assert!(
                    KNOWN.contains(&var),
                    "{label}.tmpl references unknown variable: {var}"
                );
                rest = &after[end + 1..];
            }
        }
    }
}
