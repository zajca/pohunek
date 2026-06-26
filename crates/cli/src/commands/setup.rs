//! `pohunek setup` — materialize the sway/rofi launcher integration locally.
//!
//! All operations are local filesystem writes (no daemon involvement), mirroring
//! the `doctor` command's shape (sync, takes `&Paths`, renders human or `--json`).
//! The command embeds the launcher shell scripts and the default config/templates
//! at build time (`include_str!`) and writes them into the user's XDG dirs:
//!
//! - `setup scripts` writes the launcher scripts into `paths.launcher_bin_dir()`,
//!   each made executable (the scripts source/spawn one another as siblings).
//! - `setup config` writes a default `launcher.conf` plus prompt templates, never
//!   overwriting an existing file unless `--force` is given.
//! - `setup sway` writes (or prints) a sway drop-in binding a key to the launcher.
//! - `setup` (no subcommand) runs all three and prints next steps.

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::CliError;
use crate::paths::Paths;

/// Mode the launcher scripts are written with: owner rwx, group/other r-x. They
/// are invoked directly (sway keybind, rofi spawn), so they must be executable.
const SCRIPT_MODE: u32 = 0o755;

/// File name of the sway drop-in written under `<sway_config_dir>/config.d/`.
const SWAY_DROPIN_FILE: &str = "pohunek.conf";

/// Documented default keybind for the launcher. The *effective* default is
/// supplied by clap in `main.rs`; this constant exists so `run_all` and the tests
/// have a single named source rather than a scattered literal.
const DEFAULT_SWAY_KEYBIND: &str = "$mod+p";

/// Documented default keybind for the Linear issue picker (`pohunek-rofi-issue`).
/// Same rationale as [`DEFAULT_SWAY_KEYBIND`]: clap supplies the effective default
/// in `main.rs`; this constant keeps `run_all` and the tests on one source.
const DEFAULT_SWAY_ISSUE_KEYBIND: &str = "$mod+i";

/// The launcher scripts, embedded at build time so `pohunek setup scripts` can
/// materialize them without shipping a separate data dir. All five are written
/// into the SAME directory because `pohunek-rofi` sources `lib.sh` and spawns
/// `pohunek-session-banner` as siblings (it resolves them from its own dir).
const SCRIPTS: &[(&str, &str)] = &[
    ("lib.sh", include_str!("../../../../scripts/lib.sh")),
    (
        "pohunek-rofi",
        include_str!("../../../../scripts/pohunek-rofi"),
    ),
    (
        "pohunek-launch-issue",
        include_str!("../../../../scripts/pohunek-launch-issue"),
    ),
    (
        "pohunek-rofi-issue",
        include_str!("../../../../scripts/pohunek-rofi-issue"),
    ),
    (
        "pohunek-launch-pr",
        include_str!("../../../../scripts/pohunek-launch-pr"),
    ),
    (
        "pohunek-session-banner",
        include_str!("../../../../scripts/pohunek-session-banner"),
    ),
];

/// Default `launcher.conf` contents read by the launcher scripts via `lib.sh`.
/// Keys must match what `pohunek_required_config`/`pohunek_optional_config` look
/// up; the `project`/`terminal` values are intentionally left blank for the user
/// to fill in (the scripts fail fast when a required value is empty).
const LAUNCHER_CONF: &str = "# pohunek launcher configuration.
# Lines are key=value; '#' starts a comment. Edit the values below.

# --- Required ---
# Default host: 'local' or a NetBird peer name
host=local
# Terminal emulator command (falls back to $TERMINAL if empty)
terminal=
# Per-host timeout (seconds) for `session list` queries in the rofi switcher
list_timeout_seconds=5
# Linear CLI binary (required only for pohunek-launch-issue / pohunek-rofi-issue)
linear_cli=linear

# --- Optional (defaults shown) ---
#pohunek_bin=pohunek
#gh_bin=gh
#rofi_bin=rofi
#swaymsg_bin=swaymsg
#yes=false
# Issue picker (pohunek-rofi-issue): assignee whose issues are listed.
# Empty = derive from `linear auth whoami`.
#linear_assignee=
# Issue picker: which Linear workflow-state types are 'actionable' (space-separated:
# triage backlog unstarted started completed canceled).
#linear_issue_states=started unstarted
#banner=false
#banner_height_px=24
#banner_interval_seconds=1
#mark_retry_count=20
#mark_retry_interval_seconds=0.1
";

/// Default `prompts/issue.tmpl`. May only reference the variables the renderer in
/// `lib.sh` knows: `provider, id, number, title, body, branch, url`.
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

/// Result of `setup scripts`: the absolute paths written.
#[derive(Debug, Serialize)]
struct ScriptsResult {
    installed: Vec<String>,
}

/// Result of `setup config`: which files were created vs left untouched.
#[derive(Debug, Serialize)]
struct ConfigResult {
    created: Vec<String>,
    skipped: Vec<String>,
}

/// Result of `setup sway`: the drop-in path, whether it was printed instead of
/// written, and whether the main sway config already includes the drop-in dir.
#[derive(Debug, Serialize)]
struct SwayResult {
    path: String,
    printed: bool,
    include_present: bool,
}

/// Aggregated result of the full `setup` (no subcommand).
#[derive(Debug, Serialize)]
struct AllResult {
    scripts: ScriptsResult,
    config: ConfigResult,
    sway: SwayResult,
    next_steps: Vec<String>,
}

/// Materialize the launcher scripts into `paths.launcher_bin_dir()`.
///
/// # Errors
///
/// Returns [`CliError::Io`] if the target directory cannot be created or a script
/// cannot be written or made executable.
pub(crate) fn run_scripts(paths: &Paths, json: bool) -> Result<(), CliError> {
    let result = install_scripts(paths)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_scripts_human(&result));
    }
    Ok(())
}

/// Write a default `launcher.conf` and prompt templates.
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

/// Write (or print) the sway drop-in that binds a key to the launcher.
///
/// # Errors
///
/// Returns [`CliError::Io`] if (when not printing) the drop-in directory cannot be
/// created or the drop-in file cannot be written.
pub(crate) fn run_sway(
    paths: &Paths,
    print: bool,
    keybind: &str,
    issue_keybind: &str,
    json: bool,
) -> Result<(), CliError> {
    let result = install_sway(paths, print, keybind, issue_keybind)?;
    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_sway_human(paths, &result));
    }
    Ok(())
}

/// Run the full setup: scripts + config + sway drop-in.
///
/// # Errors
///
/// Returns [`CliError::Io`] from any of the underlying steps.
pub(crate) fn run_all(paths: &Paths, json: bool) -> Result<(), CliError> {
    let scripts = install_scripts(paths)?;
    // `force=false`: the full setup must never clobber a user-edited config.
    let config = install_config(paths, false)?;
    // `print=false`: write the drop-in as part of materializing the integration.
    let sway = install_sway(
        paths,
        false,
        DEFAULT_SWAY_KEYBIND,
        DEFAULT_SWAY_ISSUE_KEYBIND,
    )?;
    let next_steps = next_steps(paths);
    let result = AllResult {
        scripts,
        config,
        sway,
        next_steps,
    };

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_scripts_human(&result.scripts));
        print!("{}", render_config_human(&result.config));
        print!("{}", render_sway_human(paths, &result.sway));
        println!();
        println!("Next steps:");
        for (idx, step) in result.next_steps.iter().enumerate() {
            println!("  {}. {step}", idx + 1);
        }
    }
    Ok(())
}

// --- core (filesystem) logic ------------------------------------------------

/// Write all embedded scripts into the launcher bin dir, making each executable.
/// Overwriting is intentional: the bin dir is a pohunek-managed location, so a
/// re-run installs the latest scripts.
fn install_scripts(paths: &Paths) -> Result<ScriptsResult, CliError> {
    let dir = paths.launcher_bin_dir();
    fs::create_dir_all(&dir)?;

    let mut installed = Vec::with_capacity(SCRIPTS.len());
    for (name, body) in SCRIPTS {
        let path = dir.join(name);
        fs::write(&path, body)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(SCRIPT_MODE))?;
        installed.push(path.display().to_string());
    }
    Ok(ScriptsResult { installed })
}

/// Write the default config + prompt templates, skipping any file that already
/// exists unless `force` is set. Tracks created vs skipped so a re-run is
/// transparent about what it touched.
fn install_config(paths: &Paths, force: bool) -> Result<ConfigResult, CliError> {
    let prompts_dir = paths.config_dir.join("prompts");
    fs::create_dir_all(&prompts_dir)?;

    let files: &[(PathBuf, &str)] = &[
        (paths.config_dir.join("launcher.conf"), LAUNCHER_CONF),
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

/// Build the sway drop-in snippet binding `keybind` to the session switcher at
/// `launcher` and `issue_keybind` to the Linear issue picker at `issue_launcher`.
/// Kept pure (no I/O) so it is directly unit-testable.
fn sway_snippet(
    launcher: &Path,
    issue_launcher: &Path,
    keybind: &str,
    issue_keybind: &str,
) -> String {
    format!(
        "# pohunek — generated by `pohunek setup sway`. Edit launcher.conf, not this file.\n\
         set $pohunek_launcher {}\n\
         bindsym {keybind} exec $pohunek_launcher\n\
         set $pohunek_issue_launcher {}\n\
         bindsym {issue_keybind} exec $pohunek_issue_launcher\n",
        launcher.display(),
        issue_launcher.display(),
    )
}

/// Either print the sway snippet (when `print`) or write it to the drop-in file,
/// then check whether the main sway config already includes the drop-in dir.
fn install_sway(
    paths: &Paths,
    print: bool,
    keybind: &str,
    issue_keybind: &str,
) -> Result<SwayResult, CliError> {
    let launcher = paths.launcher_bin_dir().join("pohunek-rofi");
    let issue_launcher = paths.launcher_bin_dir().join("pohunek-rofi-issue");
    let snippet = sway_snippet(&launcher, &issue_launcher, keybind, issue_keybind);

    if print {
        // Print-only: emit the snippet and touch nothing on disk. `path` reports
        // where it *would* be written so the caller can wire up the include.
        print!("{snippet}");
        let dropin = paths
            .sway_config_dir()
            .join("config.d")
            .join(SWAY_DROPIN_FILE);
        return Ok(SwayResult {
            path: dropin.display().to_string(),
            printed: true,
            // Not meaningful in print mode; report false rather than probe.
            include_present: false,
        });
    }

    let dropin_dir = paths.sway_config_dir().join("config.d");
    fs::create_dir_all(&dropin_dir)?;
    let dropin = dropin_dir.join(SWAY_DROPIN_FILE);
    // Overwrite is intentional: this file is fully generated and labeled as such.
    fs::write(&dropin, &snippet)?;

    let include_present = main_config_includes_dropin(paths)?;
    Ok(SwayResult {
        path: dropin.display().to_string(),
        printed: false,
        include_present,
    })
}

/// Best-effort check whether the user's main sway config already pulls in the
/// drop-in directory. Returns `false` if the config is missing. A line is treated
/// as an include only if it references `config.d` and is not a comment.
///
/// # Errors
///
/// Returns [`CliError::Io`] only if the config exists but cannot be read; absence
/// is not an error (it is the common first-run case).
fn main_config_includes_dropin(paths: &Paths) -> Result<bool, CliError> {
    let main_config = paths.sway_config_dir().join("config");
    if !main_config.try_exists()? {
        return Ok(false);
    }
    let contents = fs::read_to_string(&main_config)?;
    let present = contents.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with('#') && trimmed.contains("config.d")
    });
    Ok(present)
}

/// The ordered next-step lines printed (human) or emitted (json) after a full
/// setup. Built once so both renderings stay identical.
fn next_steps(paths: &Paths) -> Vec<String> {
    let sway_dir = paths.sway_config_dir();
    vec![
        format!(
            "Edit {}/launcher.conf — set 'terminal' (and 'linear_cli' for Linear).",
            paths.config_dir.display()
        ),
        "Pass a project id/label to launchers, for example `pohunek-launch-issue <project> <issue-id> [action]`.".to_owned(),
        format!(
            "Ensure your sway config has: include {}/config.d/*",
            sway_dir.display()
        ),
        format!(
            "Reload sway (swaymsg reload): {DEFAULT_SWAY_KEYBIND} opens the session switcher, \
             {DEFAULT_SWAY_ISSUE_KEYBIND} the Linear issue picker."
        ),
    ]
}

// --- human rendering --------------------------------------------------------

/// Render the scripts result as human lines (`installed script: <path>`).
fn render_scripts_human(result: &ScriptsResult) -> String {
    let mut out = String::new();
    for path in &result.installed {
        let _ = writeln!(out, "installed script: {path}");
    }
    out
}

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

/// Render the sway result. When printed, the snippet was already emitted, so this
/// stays silent; otherwise it reports the drop-in path plus, when the main config
/// does not yet include the drop-in dir, an advisory NOTE (it never edits the main
/// config).
fn render_sway_human(paths: &Paths, result: &SwayResult) -> String {
    if result.printed {
        return String::new();
    }
    let mut out = format!("wrote sway drop-in: {}\n", result.path);
    if !result.include_present {
        let _ = writeln!(
            out,
            "NOTE: add `include {}/config.d/*` to your sway config so the drop-in is loaded.",
            paths.sway_config_dir().display()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// Per-test counter so concurrently running tests never share a temp dir.
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A `Paths` rooted in a unique temp dir, plus the root for cleanup.
    struct TempPaths {
        paths: Paths,
        root: PathBuf,
    }

    impl Drop for TempPaths {
        fn drop(&mut self) {
            // Best-effort cleanup; a leftover temp dir is harmless.
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Build a `Paths` whose dirs all live under a fresh temp directory, so tests
    /// write real files without touching the user's environment.
    fn temp_paths() -> TempPaths {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("pohunek-setup-test-{}-{}", std::process::id(), n));
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
        };
        TempPaths { paths, root }
    }

    #[test]
    fn install_scripts_writes_all_embedded_scripts_executable() {
        let tp = temp_paths();
        let result = install_scripts(&tp.paths).expect("install scripts");

        assert_eq!(result.installed.len(), SCRIPTS.len());
        let dir = tp.paths.launcher_bin_dir();
        for (name, _) in SCRIPTS {
            let path = dir.join(name);
            assert!(path.is_file(), "missing script: {}", path.display());
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, SCRIPT_MODE, "mode for {name}: {mode:o}");
        }
    }

    #[test]
    fn embedded_scripts_are_nonempty_shell_scripts() {
        for (name, body) in SCRIPTS {
            assert!(!body.is_empty(), "{name} is empty");
            assert!(
                body.starts_with("#!"),
                "{name} does not start with a shebang"
            );
        }
    }

    #[test]
    fn install_config_creates_then_skips_then_force_rewrites() {
        let tp = temp_paths();

        // First run creates all three files.
        let first = install_config(&tp.paths, false).expect("first config");
        assert_eq!(first.created.len(), 3, "first run creates 3 files");
        assert!(first.skipped.is_empty());

        let conf = tp.paths.config_dir.join("launcher.conf");
        assert!(conf.is_file());
        assert!(tp
            .paths
            .config_dir
            .join("prompts")
            .join("issue.tmpl")
            .is_file());
        assert!(tp
            .paths
            .config_dir
            .join("prompts")
            .join("pr.tmpl")
            .is_file());

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
            LAUNCHER_CONF,
            "forced run restored the default config"
        );
    }

    #[test]
    fn launcher_conf_contains_only_host_level_launcher_keys() {
        for removed in ["project=", "agent=", "issue_action", "pr_action"] {
            assert!(
                !LAUNCHER_CONF.contains(removed),
                "launcher.conf must not contain per-project/per-action key {removed:?}"
            );
        }
        assert!(LAUNCHER_CONF.contains("host=local"));
        assert!(LAUNCHER_CONF.contains("terminal="));
        assert!(LAUNCHER_CONF.contains("linear_cli=linear"));
    }

    #[test]
    fn install_sway_writes_dropin_with_keybinds_and_launcher_paths() {
        let tp = temp_paths();
        let keybind = "$mod+x";
        let issue_keybind = "$mod+y";
        let result = install_sway(&tp.paths, false, keybind, issue_keybind).expect("install sway");

        assert!(!result.printed);
        let dropin = tp
            .paths
            .sway_config_dir()
            .join("config.d")
            .join(SWAY_DROPIN_FILE);
        assert_eq!(result.path, dropin.display().to_string());
        let contents = fs::read_to_string(&dropin).expect("read drop-in");
        assert!(contents.contains(keybind), "drop-in: {contents}");
        assert!(contents.contains(issue_keybind), "drop-in: {contents}");
        let launcher = tp.paths.launcher_bin_dir().join("pohunek-rofi");
        let issue_launcher = tp.paths.launcher_bin_dir().join("pohunek-rofi-issue");
        assert!(
            contents.contains(&launcher.display().to_string()),
            "drop-in must reference the absolute switcher path: {contents}"
        );
        assert!(
            contents.contains(&issue_launcher.display().to_string()),
            "drop-in must reference the absolute issue-picker path: {contents}"
        );
        // No main sway config exists in the temp dir → include is reported absent.
        assert!(!result.include_present);
    }

    #[test]
    fn install_sway_reports_include_present_when_main_config_references_dropin() {
        let tp = temp_paths();
        let sway_dir = tp.paths.sway_config_dir();
        fs::create_dir_all(&sway_dir).expect("sway dir");
        fs::write(
            sway_dir.join("config"),
            "# my sway config\ninclude ~/.config/sway/config.d/*\n",
        )
        .expect("write main config");

        let result = install_sway(
            &tp.paths,
            false,
            DEFAULT_SWAY_KEYBIND,
            DEFAULT_SWAY_ISSUE_KEYBIND,
        )
        .expect("install sway");
        assert!(
            result.include_present,
            "an uncommented config.d include must be detected"
        );
    }

    #[test]
    fn install_sway_ignores_commented_include_line() {
        let tp = temp_paths();
        let sway_dir = tp.paths.sway_config_dir();
        fs::create_dir_all(&sway_dir).expect("sway dir");
        fs::write(
            sway_dir.join("config"),
            "# include ~/.config/sway/config.d/*\n",
        )
        .expect("write main config");

        let result = install_sway(
            &tp.paths,
            false,
            DEFAULT_SWAY_KEYBIND,
            DEFAULT_SWAY_ISSUE_KEYBIND,
        )
        .expect("install sway");
        assert!(
            !result.include_present,
            "a commented include line must not count as present"
        );
    }

    #[test]
    fn sway_snippet_contains_both_bindsyms_and_launchers() {
        let launcher = Path::new("/home/u/.local/share/pohunek/bin/pohunek-rofi");
        let issue_launcher = Path::new("/home/u/.local/share/pohunek/bin/pohunek-rofi-issue");
        let snippet = sway_snippet(
            launcher,
            issue_launcher,
            DEFAULT_SWAY_KEYBIND,
            DEFAULT_SWAY_ISSUE_KEYBIND,
        );
        assert!(snippet.contains("bindsym"), "snippet: {snippet}");
        assert!(snippet.contains(DEFAULT_SWAY_KEYBIND), "snippet: {snippet}");
        assert!(
            snippet.contains(DEFAULT_SWAY_ISSUE_KEYBIND),
            "snippet: {snippet}"
        );
        assert!(
            snippet.contains("pohunek-rofi\n") || snippet.contains("pohunek-rofi "),
            "snippet must reference the switcher launcher: {snippet}"
        );
        assert!(
            snippet.contains("pohunek-rofi-issue"),
            "snippet must reference the issue-picker launcher: {snippet}"
        );
    }

    #[test]
    fn install_sway_print_mode_touches_no_filesystem() {
        let tp = temp_paths();
        let result = install_sway(
            &tp.paths,
            true,
            DEFAULT_SWAY_KEYBIND,
            DEFAULT_SWAY_ISSUE_KEYBIND,
        )
        .expect("print sway");
        assert!(result.printed);
        // Print mode must not create the drop-in directory.
        assert!(
            !tp.paths.sway_config_dir().join("config.d").exists(),
            "print mode must not write to the filesystem"
        );
    }

    #[test]
    fn templates_only_reference_known_variables() {
        // The renderer in lib.sh errors on any ${var} outside this set, so guard
        // it here rather than discovering it at launcher runtime.
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
