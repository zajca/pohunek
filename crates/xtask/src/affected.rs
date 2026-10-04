//! `cargo xtask affected`: the fast test loop narrowed to what a change can break.
//!
//! The changed files are the union of the committed diff against the merge base
//! (`git diff <base>...HEAD`), the uncommitted staged and unstaged changes, and
//! the untracked files. Every file is classified independently:
//!
//! - a workspace-wide build input (root manifest, lockfile, Cargo or nextest
//!   configuration, toolchain or lint configuration) selects every test;
//! - a file inside a package directory selects the package with the longest
//!   manifest-directory prefix;
//! - a file that a package embeds or reads in its tests from outside its own
//!   directory selects that package too ([`RULES`]);
//! - a file on the reviewed allowlist of non-Rust paths selects nothing;
//! - any other file cannot be proven harmless and selects every test.
//!
//! Selected packages become the nextest filterset `rdeps(=a) | rdeps(=b)`, which
//! also runs every workspace package that depends on them. The command is the
//! `cargo t` alias from `.cargo/config.toml` plus that filterset, so the profile
//! and features always match the documented fast loop, and nextest intersects
//! the filterset with the fast profile's `default-filter`. Filtersets narrow
//! only the tests that run, not the build; Cargo's incremental build already
//! recompiles only the changed packages and their dependents.
//!
//! This is an inner-loop accelerator. It never replaces the full gate set.

// Rust guideline compliant 2026-10-03

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::{relative_path_string, XtaskError};

/// Revisions tried in order when `--base` is absent.
///
/// `origin/main` is preferred because it is what a pull request is compared
/// against; a clone without that remote falls back to the local `main`.
const DEFAULT_BASES: [&str; 2] = ["origin/main", "main"];

/// Cargo alias that runs the cost-filtered fast nextest loop.
///
/// Defined in `.cargo/config.toml`; reusing it keeps the profile and feature
/// flags identical to `cargo t` without restating them here.
const FAST_LOOP_ALIAS: &str = "t";

/// Command that runs the Python unit tests of `scripts/`.
const SCRIPT_TESTS_COMMAND: &str = "python3 -m unittest discover -s scripts/tests -p 'test_*.py'";

/// Options for one `cargo xtask affected` invocation.
#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Revision to diff against through its merge base with HEAD
    /// [default: origin/main, else main; fails when neither exists].
    #[arg(long, value_name = "REF")]
    base: Option<String>,
    /// Print the per-file reasons and the command without running it.
    #[arg(long)]
    print: bool,
    /// Extra arguments appended to the nextest command.
    #[arg(last = true, value_name = "NEXTEST_ARGS")]
    nextest_args: Vec<OsString>,
}

/// Workspace package name and its manifest directory relative to the workspace root.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Package {
    name: String,
    /// `/`-separated, without a trailing slash; empty for a root package.
    dir: String,
}

/// Path shape matched by a classification rule.
#[derive(Clone, Copy, Debug)]
enum Pattern {
    /// Exactly this repository-relative file.
    File(&'static str),
    /// Any file below this repository-relative directory.
    Dir(&'static str),
    /// A root-level file whose name starts with this prefix.
    RootPrefix(&'static str),
    /// A root-level file whose name ends with this suffix.
    RootSuffix(&'static str),
}

impl Pattern {
    fn matches(self, path: &str) -> bool {
        match self {
            Self::File(file) => path == file,
            Self::Dir(dir) => is_below(path, dir),
            Self::RootPrefix(prefix) => !path.contains('/') && path.starts_with(prefix),
            Self::RootSuffix(suffix) => !path.contains('/') && path.ends_with(suffix),
        }
    }
}

/// What a matching rule contributes to the selection.
#[derive(Clone, Copy, Debug)]
enum Effect {
    /// The path is a build or test input of every package.
    Everything,
    /// The path is embedded or read by the tests of these packages.
    Packages(&'static [&'static str]),
    /// The path provably feeds no Rust test.
    NoRustTests,
}

/// Follow-up check that the Rust tests do not cover.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Reminder {
    DocsCheck,
    ScriptTests,
    SdkGates,
}

impl Reminder {
    fn message(self) -> String {
        match self {
            Self::DocsCheck => {
                "documentation inputs changed: also run `cargo xtask docs check`".to_owned()
            }
            Self::ScriptTests => {
                format!("scripts changed: also run `{SCRIPT_TESTS_COMMAND}`")
            }
            Self::SdkGates => "SDK workspace changed: also run the Bun gates from the \
                               repository root (AGENTS.md \"SDK workspace gates\") and \
                               `cargo xtask ts check`"
                .to_owned(),
        }
    }
}

/// One reviewed classification rule; `why` is printed as the per-file reason.
#[derive(Debug)]
struct Rule {
    pattern: Pattern,
    effect: Effect,
    reminders: &'static [Reminder],
    why: &'static str,
}

const fn rule(pattern: Pattern, effect: Effect, why: &'static str) -> Rule {
    Rule {
        pattern,
        effect,
        reminders: &[],
        why,
    }
}

const fn reminding(
    pattern: Pattern,
    effect: Effect,
    reminders: &'static [Reminder],
    why: &'static str,
) -> Rule {
    Rule {
        pattern,
        effect,
        reminders,
        why,
    }
}

/// Reviewed rules for paths whose effect a package directory does not express.
///
/// `Packages` entries list every crate whose sources `include_*!` the path or
/// whose tests read it at run time; the `include_*!` part is enforced by the
/// `every_escaping_include_is_covered` test. `NoRustTests` entries must never
/// be read by a Rust build or test. When no rule and no package matches, the
/// path selects every test.
const RULES: &[Rule] = &[
    // Workspace-wide inputs.
    rule(Pattern::File("Cargo.toml"), Effect::Everything, "workspace manifest"),
    rule(Pattern::File("Cargo.lock"), Effect::Everything, "dependency lockfile"),
    rule(Pattern::Dir(".cargo"), Effect::Everything, "Cargo configuration and aliases"),
    rule(
        Pattern::File(".config/nextest.toml"),
        Effect::Everything,
        "nextest profiles and test filters",
    ),
    rule(Pattern::RootPrefix("rust-toolchain"), Effect::Everything, "toolchain pin"),
    rule(Pattern::File(".clippy.toml"), Effect::Everything, "clippy configuration"),
    rule(Pattern::File("clippy.toml"), Effect::Everything, "clippy configuration"),
    rule(Pattern::File("rustfmt.toml"), Effect::Everything, "rustfmt configuration"),
    rule(Pattern::File(".rustfmt.toml"), Effect::Everything, "rustfmt configuration"),
    // Files embedded or read from outside the owning package directory.
    reminding(
        Pattern::Dir("docs/knowledge"),
        Effect::Packages(&["pohunek-knowledge", "xtask"]),
        &[Reminder::DocsCheck],
        "knowledge bundle embedded by pohunek-knowledge's build script and read by xtask tests",
    ),
    rule(
        Pattern::Dir("compat/hermes"),
        Effect::Packages(&["pohunek-daemon", "xtask"]),
        "Hermes compatibility lock embedded by the daemon; lock and goldens read by xtask",
    ),
    rule(
        Pattern::Dir("compat/codex"),
        Effect::Packages(&["pohunek-daemon"]),
        "Codex subagent hook contract embedded by the daemon",
    ),
    reminding(
        Pattern::Dir("scripts"),
        Effect::Packages(&["pohunek-cli"]),
        &[Reminder::ScriptTests],
        "scripts executed by pohunek-cli tests (the Hermes plugin release smoke suite)",
    ),
    rule(
        Pattern::Dir("packaging"),
        Effect::Packages(&["pohunek-cli", "xtask"]),
        "daemon packaging tested by pohunek-cli; xtask embeds stage-archive's release files",
    ),
    rule(
        Pattern::File(".github/workflows/release.yml"),
        Effect::Packages(&["pohunek-cli"]),
        "release workflow asserted by pohunek-cli's daemon_packaging test",
    ),
    rule(
        Pattern::File("crates/relay/src/lib.rs"),
        Effect::Packages(&["xtask"]),
        "relay sources scanned by xtask's dependency_policy test",
    ),
    rule(
        Pattern::File("crates/relay/src/bin/pohunek-relayd.rs"),
        Effect::Packages(&["xtask"]),
        "relay sources scanned by xtask's dependency_policy test",
    ),
    // Reviewed non-Rust paths. More specific rules above take precedence.
    reminding(
        Pattern::Dir("sdk/ts"),
        Effect::NoRustTests,
        &[Reminder::SdkGates],
        "Bun workspace; xtask only writes its generated bindings, checked by `cargo xtask ts check`",
    ),
    reminding(
        Pattern::File("package.json"),
        Effect::NoRustTests,
        &[Reminder::SdkGates],
        "Bun workspace root manifest; read only by the Bun gates",
    ),
    reminding(
        Pattern::File("bun.lock"),
        Effect::NoRustTests,
        &[Reminder::SdkGates],
        "Bun workspace lockfile; read only by the Bun gates",
    ),
    reminding(
        Pattern::File(".bun-version"),
        Effect::NoRustTests,
        &[Reminder::SdkGates],
        "Bun version pin; read only by the Bun gates",
    ),
    reminding(
        Pattern::File("eslint.config.js"),
        Effect::NoRustTests,
        &[Reminder::SdkGates],
        "ESLint configuration of the Bun workspace",
    ),
    reminding(
        Pattern::RootPrefix("tsconfig"),
        Effect::NoRustTests,
        &[Reminder::SdkGates],
        "TypeScript configuration of the Bun workspace",
    ),
    reminding(
        Pattern::Dir("docs"),
        Effect::NoRustTests,
        &[Reminder::DocsCheck],
        "prose outside docs/knowledge; only `cargo xtask docs check` reads it",
    ),
    rule(
        Pattern::Dir(".github"),
        Effect::NoRustTests,
        "CI configuration outside the release workflow",
    ),
    rule(Pattern::Dir(".claude"), Effect::NoRustTests, "agent skills and settings"),
    rule(Pattern::Dir(".agents"), Effect::NoRustTests, "vendored agent guidelines"),
    rule(Pattern::Dir("assets"), Effect::NoRustTests, "images referenced by prose only"),
    reminding(
        Pattern::RootSuffix(".md"),
        Effect::NoRustTests,
        &[Reminder::DocsCheck],
        "root prose; README.md is read only by `cargo xtask docs check`",
    ),
    reminding(
        Pattern::File("LICENSE"),
        Effect::NoRustTests,
        &[Reminder::DocsCheck],
        "license text; read only by `cargo xtask docs check`",
    ),
    rule(Pattern::File("bacon.toml"), Effect::NoRustTests, "optional watcher configuration"),
];

/// Why one changed file selects what it selects.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Decision {
    Everything(&'static str),
    Packages {
        names: BTreeSet<String>,
        why: String,
    },
    NoRustTests(&'static str),
    Unmapped,
}

impl Decision {
    fn describe(&self) -> String {
        match self {
            Self::Everything(why) => format!("everything ({why})"),
            Self::Packages { names, why } => {
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                format!("{} ({why})", names.join(", "))
            }
            Self::NoRustTests(why) => format!("no Rust tests ({why})"),
            Self::Unmapped => {
                "everything (no package or reviewed rule covers this path)".to_owned()
            }
        }
    }
}

/// Tests selected by the whole change.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Selection {
    Nothing,
    Packages(BTreeSet<String>),
    Everything,
}

/// Classification of a whole change.
#[derive(Debug)]
struct Plan {
    decisions: Vec<(String, Decision)>,
    selection: Selection,
    reminders: BTreeSet<Reminder>,
}

/// Runs, or with `--print` describes, the fast tests affected by the current change.
pub(crate) fn run(root: &Path, options: &Options) -> Result<(), XtaskError> {
    let metadata = command_output(
        root,
        "cargo",
        &[
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ],
    )?;
    let (workspace_root, packages) = parse_metadata(&metadata)?;
    ensure_git_toplevel(&workspace_root)?;

    let base = resolve_base(&workspace_root, options.base.as_deref())?;
    let changed = changed_files(&workspace_root, &base)?;
    let plan = plan(&changed, &packages)?;
    let args = nextest_args(&plan.selection, &options.nextest_args);

    if options.print {
        print!("{}", render_report(&base, &plan, args.as_deref()));
        return Ok(());
    }

    eprint!("{}", render_summary(&base, &plan, args.as_deref()));
    let Some(args) = args else {
        return Ok(());
    };
    let status = Command::new("cargo")
        .current_dir(&workspace_root)
        .args(&args)
        .status()
        .map_err(|source| XtaskError::Io {
            path: PathBuf::from("cargo"),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(XtaskError::ChildExit {
            command: format!("cargo {FAST_LOOP_ALIAS}"),
            status,
        })
    }
}

#[derive(Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    packages: Vec<MetadataPackage>,
}

#[derive(Deserialize)]
struct MetadataPackage {
    name: String,
    manifest_path: PathBuf,
}

/// Parses `cargo metadata --no-deps` into the workspace root and its packages.
fn parse_metadata(json: &[u8]) -> Result<(PathBuf, Vec<Package>), XtaskError> {
    let metadata: Metadata = serde_json::from_slice(json).map_err(XtaskError::Json)?;
    let mut packages = Vec::with_capacity(metadata.packages.len());
    for package in metadata.packages {
        let dir = package
            .manifest_path
            .parent()
            .and_then(|dir| dir.strip_prefix(&metadata.workspace_root).ok())
            .ok_or_else(|| XtaskError::InvalidPath(package.manifest_path.clone()))?;
        packages.push(Package {
            name: package.name,
            dir: relative_path_string(dir)?,
        });
    }
    Ok((metadata.workspace_root, packages))
}

/// Classifies every changed file and combines the decisions into one selection.
///
/// Fails when a rule names a package that is not in the workspace, so a renamed
/// crate cannot silently drop out of the selection.
fn plan(changed: &BTreeSet<String>, packages: &[Package]) -> Result<Plan, XtaskError> {
    for rule in RULES {
        if let Effect::Packages(names) = rule.effect {
            for name in names {
                if !packages.iter().any(|package| package.name == *name) {
                    return Err(XtaskError::Usage(format!(
                        "affected rule for {:?} names unknown workspace package `{name}`; \
                         update RULES in crates/xtask/src/affected.rs",
                        rule.pattern
                    )));
                }
            }
        }
    }

    let mut decisions = Vec::with_capacity(changed.len());
    let mut reminders = BTreeSet::new();
    let mut everything = false;
    let mut selected = BTreeSet::new();
    for path in changed {
        let (decision, reminder) = classify(path, packages);
        reminders.extend(reminder);
        match &decision {
            Decision::Everything(_) | Decision::Unmapped => everything = true,
            Decision::Packages { names, .. } => selected.extend(names.iter().cloned()),
            Decision::NoRustTests(_) => {}
        }
        decisions.push((path.clone(), decision));
    }

    let selection = if everything {
        Selection::Everything
    } else if selected.is_empty() {
        Selection::Nothing
    } else {
        Selection::Packages(selected)
    };
    Ok(Plan {
        decisions,
        selection,
        reminders,
    })
}

/// Classifies one repository-relative, `/`-separated path.
fn classify(path: &str, packages: &[Package]) -> (Decision, BTreeSet<Reminder>) {
    let matching: Vec<&Rule> = RULES
        .iter()
        .filter(|rule| rule.pattern.matches(path))
        .collect();
    // Every matching rule contributes its reminders, including rules whose
    // effect is shadowed by a more specific one.
    let reminder: BTreeSet<Reminder> = matching
        .iter()
        .flat_map(|rule| rule.reminders.iter().copied())
        .collect();

    if let Some(rule) = matching
        .iter()
        .find(|rule| matches!(rule.effect, Effect::Everything))
    {
        return (Decision::Everything(rule.why), reminder);
    }

    let mut names = BTreeSet::new();
    let mut reasons = Vec::new();
    if let Some(owner) = owning_package(path, packages) {
        names.insert(owner.name.clone());
        reasons.push("package directory");
    }
    for rule in &matching {
        if let Effect::Packages(embedders) = rule.effect {
            names.extend(embedders.iter().map(|name| (*name).to_owned()));
            reasons.push(rule.why);
        }
    }
    if !names.is_empty() {
        let why = reasons.join("; ");
        return (Decision::Packages { names, why }, reminder);
    }

    match matching
        .iter()
        .find(|rule| matches!(rule.effect, Effect::NoRustTests))
    {
        Some(rule) => (Decision::NoRustTests(rule.why), reminder),
        None => (Decision::Unmapped, reminder),
    }
}

/// Returns the package whose manifest directory is the longest prefix of `path`.
fn owning_package<'a>(path: &str, packages: &'a [Package]) -> Option<&'a Package> {
    packages
        .iter()
        .filter(|package| package.dir.is_empty() || is_below(path, &package.dir))
        .max_by_key(|package| package.dir.len())
}

/// Whether `path` lies below `dir`, compared by whole path components.
fn is_below(path: &str, dir: &str) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Builds the nextest filterset selecting `names` and their reverse dependencies.
///
/// `=` is nextest's exact name matcher; the default for `rdeps` is a glob.
/// `BTreeSet` iteration keeps the output deterministic.
fn filterset(names: &BTreeSet<String>) -> String {
    names
        .iter()
        .map(|name| format!("rdeps(={name})"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Arguments for `cargo`, or `None` when no Rust test is affected.
fn nextest_args(selection: &Selection, extra: &[OsString]) -> Option<Vec<OsString>> {
    let mut args = vec![OsString::from(FAST_LOOP_ALIAS)];
    match selection {
        Selection::Nothing => return None,
        Selection::Everything => {}
        Selection::Packages(names) => {
            args.push(OsString::from("-E"));
            args.push(OsString::from(filterset(names)));
        }
    }
    args.extend(extra.iter().cloned());
    Some(args)
}

fn selection_line(selection: &Selection) -> String {
    match selection {
        Selection::Nothing => "selection: no Rust tests are affected".to_owned(),
        Selection::Everything => "selection: everything (fail-safe escalation)".to_owned(),
        Selection::Packages(names) => {
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            format!("selection: {} plus their dependents", names.join(", "))
        }
    }
}

fn command_line(args: Option<&[OsString]>) -> String {
    match args {
        Some(args) => {
            let mut line = String::from("cargo");
            for arg in args {
                line.push(' ');
                line.push_str(&shell_quote(&arg.to_string_lossy()));
            }
            line
        }
        None => "command: none".to_owned(),
    }
}

/// Full `--print` output: base, one reason per file, selection, reminders, command.
fn render_report(base: &str, plan: &Plan, args: Option<&[OsString]>) -> String {
    let mut out = format!("base: {}\n", printable(base));
    if plan.decisions.is_empty() {
        out.push_str("no changed files\n");
    }
    for (path, decision) in &plan.decisions {
        let _ = writeln!(out, "{} -> {}", printable(path), decision.describe());
    }
    let _ = writeln!(out, "{}", selection_line(&plan.selection));
    for reminder in &plan.reminders {
        let _ = writeln!(out, "reminder: {}", reminder.message());
    }
    let _ = writeln!(out, "{}", command_line(args));
    out
}

/// Short summary printed before running: base, escalation causes, selection, reminders.
fn render_summary(base: &str, plan: &Plan, args: Option<&[OsString]>) -> String {
    let mut out = format!(
        "affected: base {}, {} changed files\n",
        printable(base),
        plan.decisions.len()
    );
    for (path, decision) in &plan.decisions {
        if matches!(decision, Decision::Everything(_) | Decision::Unmapped) {
            let _ = writeln!(
                out,
                "affected: {} -> {}",
                printable(path),
                decision.describe()
            );
        }
    }
    let _ = writeln!(out, "affected: {}", selection_line(&plan.selection));
    for reminder in &plan.reminders {
        let _ = writeln!(out, "affected: reminder: {}", reminder.message());
    }
    if args.is_some() {
        let _ = writeln!(out, "affected: running {}", command_line(args));
    }
    out
}

/// Escapes control characters in a git-supplied path or ref before it reaches
/// the terminal.
///
/// File names may contain newlines or ESC; printed raw they could forge report
/// lines or drive terminal escape sequences. Other characters stay readable.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Quotes `arg` for display in a POSIX shell when it contains special characters.
fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+".contains(c));
    if plain {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Fails unless the Git top level is the Cargo workspace root, so Git paths map onto packages.
fn ensure_git_toplevel(workspace_root: &Path) -> Result<(), XtaskError> {
    let output = command_output(workspace_root, GIT, &["rev-parse", "--show-toplevel"])?;
    let toplevel = PathBuf::from(String::from_utf8_lossy(&output).trim_end_matches('\n'));
    let canonical = |path: &Path| {
        path.canonicalize().map_err(|source| XtaskError::Io {
            path: path.to_path_buf(),
            source,
        })
    };
    if canonical(&toplevel)? == canonical(workspace_root)? {
        Ok(())
    } else {
        Err(XtaskError::Usage(format!(
            "affected: git top level `{}` is not the Cargo workspace root `{}`",
            toplevel.display(),
            workspace_root.display()
        )))
    }
}

/// Resolves `--base`, or the first of [`DEFAULT_BASES`] that names a commit.
fn resolve_base(root: &Path, explicit: Option<&str>) -> Result<String, XtaskError> {
    let names_commit = |candidate: &str| -> Result<bool, XtaskError> {
        let revision = format!("{candidate}^{{commit}}");
        let status = command_in(root, GIT)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &revision,
            ])
            .stdout(std::process::Stdio::null())
            .status()
            .map_err(|source| XtaskError::Io {
                path: PathBuf::from("git"),
                source,
            })?;
        Ok(status.success())
    };

    if let Some(base) = explicit {
        return if names_commit(base)? {
            Ok(base.to_owned())
        } else {
            Err(XtaskError::Usage(format!(
                "affected: --base `{base}` does not name a commit"
            )))
        };
    }
    for candidate in DEFAULT_BASES {
        if names_commit(candidate)? {
            return Ok(candidate.to_owned());
        }
    }
    Err(XtaskError::Usage(format!(
        "affected: none of {} names a commit; pass --base <REF>",
        DEFAULT_BASES.join(", ")
    )))
}

/// Collects committed, uncommitted, and untracked changed paths relative to `root`.
///
/// `--no-renames` reports both sides of a move, so the package that lost a file
/// is selected too.
fn changed_files(root: &Path, base: &str) -> Result<BTreeSet<String>, XtaskError> {
    let range = format!("{base}...HEAD");
    let listings = [
        command_output(
            root,
            GIT,
            &["diff", "--name-only", "--no-renames", "-z", &range, "--"],
        )?,
        command_output(
            root,
            GIT,
            &["diff", "--name-only", "--no-renames", "-z", "HEAD", "--"],
        )?,
        command_output(
            root,
            GIT,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?,
    ];
    Ok(listings
        .iter()
        .flat_map(|listing| parse_nul_list(listing))
        .collect())
}

/// Splits NUL-separated Git output into paths.
///
/// Invalid UTF-8 is replaced lossily: the directory components that decide the
/// classification stay intact, and an unmatched path still selects everything.
fn parse_nul_list(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
}

/// The Git executable the changed-file queries run.
const GIT: &str = "git";

/// Variables that move Git to another repository, index or object store than
/// the one in the working directory.
///
/// These are the repository-location variables of git(1) "ENVIRONMENT
/// VARIABLES" ("The Git Repository") and the variables that substitute the
/// history Git reads (`GIT_SHALLOW_FILE`, `GIT_GRAFT_FILE`,
/// `GIT_REPLACE_REF_BASE`, `GIT_NO_REPLACE_OBJECTS`), which would change the
/// merge base of the changed-file query. Git sets `GIT_DIR` and `GIT_INDEX_FILE`
/// itself while it runs a hook, so a loop started from a hook would otherwise
/// ask about the hook's repository instead of the workspace. The discovery
/// limits (`GIT_CEILING_DIRECTORIES`, `GIT_DISCOVERY_ACROSS_FILESYSTEM`) and the
/// variables that only shape a newly created repository or index
/// (`GIT_DEFAULT_HASH`, `GIT_DEFAULT_REF_FORMAT`, `GIT_INDEX_VERSION`) stay
/// because the commands start at the workspace root itself, and the developer's
/// configuration and identity variables stay because they are legitimate input.
const REPOSITORY_REDIRECTING_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_COMMON_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_REFERENCE_BACKEND",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
];

/// Builds a command for `program` that runs in `dir`; a Git command also drops
/// the [`REPOSITORY_REDIRECTING_VARS`] it would inherit.
fn command_in(dir: &Path, program: &str) -> Command {
    let mut command = Command::new(program);
    command.current_dir(dir);
    if program == GIT {
        for var in REPOSITORY_REDIRECTING_VARS {
            command.env_remove(var);
        }
    }
    command
}

/// Runs `program` with `args` in `dir` and returns stdout, failing on a non-zero exit.
fn command_output(dir: &Path, program: &str, args: &[&str]) -> Result<Vec<u8>, XtaskError> {
    let output = command_in(dir, program)
        .args(args)
        .output()
        .map_err(|source| XtaskError::Io {
            path: PathBuf::from(program),
            source,
        })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(XtaskError::Usage(format!(
            "affected: `{program} {}` failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use pohunek_test_support::env::TestEnv;

    use super::*;

    fn package(name: &str, dir: &str) -> Package {
        Package {
            name: name.to_owned(),
            dir: dir.to_owned(),
        }
    }

    /// Workspace fixture with nested and prefix-sharing package directories.
    fn fixture() -> Vec<Package> {
        vec![
            package("pohunek-relay", "crates/relay"),
            package("pohunek-relay-client", "crates/relay-client"),
            package("pohunek-knowledge", "crates/knowledge"),
            package("pohunek-daemon", "crates/daemon"),
            package("pohunek-cli", "crates/cli"),
            package("xtask", "crates/xtask"),
            package("nested-fixture", "crates/xtask/tests/fixtures/nested"),
        ]
    }

    fn changed(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn selection(paths: &[&str]) -> Selection {
        plan(&changed(paths), &fixture()).expect("plan").selection
    }

    #[test]
    fn a_file_maps_to_the_package_with_the_longest_directory_prefix() {
        assert_eq!(
            selection(&["crates/xtask/tests/fixtures/nested/src/lib.rs"]),
            Selection::Packages(names(&["nested-fixture"]))
        );
        assert_eq!(
            selection(&["crates/xtask/src/lib.rs"]),
            Selection::Packages(names(&["xtask"]))
        );
    }

    #[test]
    fn a_directory_prefix_matches_whole_components_only() {
        assert_eq!(
            selection(&["crates/relay-client/src/lib.rs"]),
            Selection::Packages(names(&["pohunek-relay-client"]))
        );
        assert_eq!(
            selection(&["crates/relayx/src/lib.rs"]),
            Selection::Everything
        );
    }

    #[test]
    fn a_package_manifest_selects_only_its_package() {
        assert_eq!(
            selection(&["crates/cli/Cargo.toml"]),
            Selection::Packages(names(&["pohunek-cli"]))
        );
    }

    #[test]
    fn every_workspace_wide_input_escalates_to_everything() {
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/config.toml",
            ".config/nextest.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
            ".clippy.toml",
            "clippy.toml",
            "rustfmt.toml",
            ".rustfmt.toml",
        ] {
            assert_eq!(
                selection(&[path, "crates/cli/src/lib.rs"]),
                Selection::Everything,
                "{path}"
            );
        }
    }

    #[test]
    fn a_workspace_wide_input_below_the_root_is_not_escalated_by_name_alone() {
        assert_eq!(
            selection(&["crates/cli/rust-toolchain.md"]),
            Selection::Packages(names(&["pohunek-cli"]))
        );
    }

    #[test]
    fn an_unknown_path_escalates_to_everything() {
        for path in [
            "idea.txt",
            ".lh-harness/state",
            "crates/README.md",
            "compat/other/x.json",
        ] {
            let plan = plan(&changed(&[path]), &fixture()).expect("plan");
            assert_eq!(plan.selection, Selection::Everything, "{path}");
            assert_eq!(plan.decisions[0].1, Decision::Unmapped, "{path}");
        }
    }

    #[test]
    fn allowlisted_paths_select_no_rust_tests() {
        for path in [
            "sdk/ts/sdk/src/client.ts",
            "package.json",
            "bun.lock",
            ".bun-version",
            "eslint.config.js",
            "tsconfig.base.json",
            "docs/ROADMAP.md",
            ".github/workflows/ci.yml",
            ".claude/skills/gates/SKILL.md",
            ".agents/rust-guidelines/SKILL.md",
            "assets/pohunek_icon_mark.png",
            "README.md",
            "AGENTS.md",
            "LICENSE",
            "bacon.toml",
        ] {
            assert_eq!(selection(&[path]), Selection::Nothing, "{path}");
        }
        assert_eq!(nextest_args(&Selection::Nothing, &[]), None);
    }

    #[test]
    fn allowlisted_paths_do_not_mask_package_changes() {
        assert_eq!(
            selection(&["README.md", "crates/daemon/src/lib.rs"]),
            Selection::Packages(names(&["pohunek-daemon"]))
        );
    }

    #[test]
    fn root_prose_suffix_applies_only_at_the_root() {
        assert_eq!(selection(&["notes/todo.md"]), Selection::Everything);
    }

    #[test]
    fn embedded_paths_map_to_their_embedding_packages() {
        let cases: [(&str, &[&str]); 8] = [
            (
                "docs/knowledge/guides/agent-skill.md",
                &["pohunek-knowledge", "xtask"],
            ),
            (
                "compat/hermes/compatibility-lock.json",
                &["pohunek-daemon", "xtask"],
            ),
            ("compat/codex/subagent-hooks.json", &["pohunek-daemon"]),
            (
                "scripts/tests/smoke-hermes-plugin-release.sh",
                &["pohunek-cli"],
            ),
            ("packaging/install-daemon.sh", &["pohunek-cli", "xtask"]),
            ("packaging/stage-archive", &["pohunek-cli", "xtask"]),
            (".github/workflows/release.yml", &["pohunek-cli"]),
            ("crates/relay/src/lib.rs", &["pohunek-relay", "xtask"]),
        ];
        for (path, expected) in cases {
            assert_eq!(
                selection(&[path]),
                Selection::Packages(names(expected)),
                "{path}"
            );
        }
    }

    #[test]
    fn reminders_follow_their_paths() {
        let plan = plan(
            &changed(&[
                "docs/knowledge/concepts/session.md",
                "scripts/tests/test_ci_timings.py",
                "sdk/ts/protocol/src/index.ts",
                "Cargo.lock",
            ]),
            &fixture(),
        )
        .expect("plan");
        assert_eq!(
            plan.reminders,
            BTreeSet::from([
                Reminder::DocsCheck,
                Reminder::ScriptTests,
                Reminder::SdkGates
            ])
        );
        let report = render_report("main", &plan, None);
        assert!(report.contains("cargo xtask docs check"), "{report}");
        assert!(report.contains(SCRIPT_TESTS_COMMAND), "{report}");
        assert!(report.contains("cargo xtask ts check"), "{report}");
    }

    #[test]
    fn every_docs_check_input_reminds_the_docs_check() {
        for path in ["docs/runbooks/upgrade.md", "README.md", "LICENSE"] {
            let plan = plan(&changed(&[path]), &fixture()).expect("plan");
            assert_eq!(plan.selection, Selection::Nothing, "{path}");
            assert_eq!(
                plan.reminders,
                BTreeSet::from([Reminder::DocsCheck]),
                "{path}"
            );
        }
    }

    #[test]
    fn a_rule_naming_an_unknown_package_fails() {
        let packages = vec![package("pohunek-cli", "crates/cli")];
        let error = plan(&changed(&["crates/cli/src/lib.rs"]), &packages)
            .expect_err("rules name packages missing from this workspace");
        assert!(
            error.to_string().contains("unknown workspace package"),
            "{error}"
        );
    }

    #[test]
    fn filterset_is_sorted_and_uses_exact_package_matchers() {
        let selected = BTreeSet::from(["xtask".to_owned(), "pohunek-cli".to_owned()]);
        assert_eq!(filterset(&selected), "rdeps(=pohunek-cli) | rdeps(=xtask)");

        let from_other_order = selection(&["crates/xtask/src/lib.rs", "crates/cli/src/lib.rs"]);
        let Selection::Packages(from_other_order) = from_other_order else {
            panic!("expected a package selection");
        };
        assert_eq!(filterset(&from_other_order), filterset(&selected));
    }

    #[test]
    fn nextest_args_use_the_fast_alias_and_append_passthrough() {
        let extra = [OsString::from("--no-fail-fast")];
        assert_eq!(
            nextest_args(&Selection::Packages(names(&["pohunek-cli"])), &extra),
            Some(vec![
                OsString::from("t"),
                OsString::from("-E"),
                OsString::from("rdeps(=pohunek-cli)"),
                OsString::from("--no-fail-fast"),
            ])
        );
        assert_eq!(
            nextest_args(&Selection::Everything, &extra),
            Some(vec![OsString::from("t"), OsString::from("--no-fail-fast")])
        );
    }

    #[test]
    fn printed_command_is_shell_quoted() {
        let args = nextest_args(&Selection::Packages(names(&["pohunek-cli", "xtask"])), &[])
            .expect("args");
        assert_eq!(
            command_line(Some(&args)),
            "cargo t -E 'rdeps(=pohunek-cli) | rdeps(=xtask)'"
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn report_lists_a_reason_per_file() {
        let plan = plan(
            &changed(&["crates/cli/src/lib.rs", "sdk/ts/x.ts"]),
            &fixture(),
        )
        .expect("plan");
        let args = nextest_args(&plan.selection, &[]);
        let report = render_report("origin/main", &plan, args.as_deref());
        assert_eq!(
            report,
            "base: origin/main\n\
             crates/cli/src/lib.rs -> pohunek-cli (package directory)\n\
             sdk/ts/x.ts -> no Rust tests (Bun workspace; xtask only writes its generated \
             bindings, checked by `cargo xtask ts check`)\n\
             selection: pohunek-cli plus their dependents\n\
             reminder: SDK workspace changed: also run the Bun gates from the repository \
             root (AGENTS.md \"SDK workspace gates\") and `cargo xtask ts check`\n\
             cargo t -E 'rdeps(=pohunek-cli)'\n"
        );
    }

    #[test]
    fn control_characters_in_paths_and_base_are_escaped() {
        // Unmapped, so the pre-run summary prints it as an escalation cause too.
        let hostile = "a\nforged -> nothing\x1b[2J.rs";
        let plan = plan(&changed(&[hostile]), &fixture()).expect("plan");
        let args = nextest_args(&plan.selection, &[]);
        for output in [
            render_report("main\x1b]0;x\x07", &plan, args.as_deref()),
            render_summary("main\x1b]0;x\x07", &plan, args.as_deref()),
        ] {
            assert!(
                !output.chars().any(|c| c.is_control() && c != '\n'),
                "{output:?}"
            );
            assert!(!output.contains("\nforged"), "{output:?}");
            assert!(
                output.contains(r"a\nforged -> nothing\u{1b}[2J.rs"),
                "{output:?}"
            );
            assert!(output.contains(r"main\u{1b}]0;x\u{7}"), "{output:?}");
        }
        assert_eq!(printable("crates/ünïcode.rs"), "crates/ünïcode.rs");
    }

    #[test]
    fn nul_separated_listings_are_split_and_lossy_decoded() {
        let paths: Vec<String> = parse_nul_list(b"a.rs\0crates/cli/\xff.rs\0\0").collect();
        assert_eq!(
            paths,
            vec!["a.rs".to_owned(), "crates/cli/\u{fffd}.rs".to_owned()]
        );
    }

    #[test]
    fn metadata_directories_are_relative_to_the_workspace_root() {
        let json = br#"{
            "workspace_root": "/repo",
            "packages": [
                {"name": "pohunek-cli", "manifest_path": "/repo/crates/cli/Cargo.toml", "version": "0.1.0"},
                {"name": "xtask", "manifest_path": "/repo/crates/xtask/Cargo.toml"}
            ]
        }"#;
        let (root, packages) = parse_metadata(json).expect("metadata parses");
        assert_eq!(root, Path::new("/repo"));
        assert_eq!(
            packages,
            vec![
                package("pohunek-cli", "crates/cli"),
                package("xtask", "crates/xtask")
            ]
        );

        let outside = br#"{"workspace_root": "/repo",
            "packages": [{"name": "x", "manifest_path": "/elsewhere/Cargo.toml"}]}"#;
        parse_metadata(outside).expect_err("a package outside the workspace is rejected");
    }

    /// Workspace packages read from the real manifests.
    fn repo_packages(root: &Path) -> Vec<Package> {
        let manifest: toml::Value = toml::from_str(
            &std::fs::read_to_string(root.join("Cargo.toml")).expect("read workspace manifest"),
        )
        .expect("parse workspace manifest");
        let members = manifest["workspace"]["members"]
            .as_array()
            .expect("workspace members");
        members
            .iter()
            .map(|member| {
                let dir = member.as_str().expect("member path").to_owned();
                let crate_manifest: toml::Value = toml::from_str(
                    &std::fs::read_to_string(root.join(&dir).join("Cargo.toml"))
                        .expect("read member manifest"),
                )
                .expect("parse member manifest");
                let name = crate_manifest["package"]["name"]
                    .as_str()
                    .expect("package name")
                    .to_owned();
                Package { name, dir }
            })
            .collect()
    }

    #[test]
    fn rules_name_only_real_workspace_packages() {
        let packages = repo_packages(&pohunek_test_support::workspace_root());
        plan(&BTreeSet::new(), &packages).expect("every rule names a workspace package");
    }

    /// Resolves `.` and `..` lexically; `None` when the path climbs above the root.
    fn normalize(path: &Path) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(part) => parts.push(part.to_str()?),
                std::path::Component::ParentDir => {
                    parts.pop()?;
                }
                std::path::Component::CurDir => {}
                _ => return None,
            }
        }
        Some(parts.join("/"))
    }

    fn rust_sources(dir: &Path, sources: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read source directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "target") {
                    rust_sources(&path, sources);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                sources.push(path);
            }
        }
    }

    /// Every `include_str!`/`include_bytes!` that leaves its package must select that package.
    ///
    /// Run-time reads by tests cannot be found this way; they are listed in
    /// [`RULES`] by review.
    #[test]
    fn every_escaping_include_is_covered() {
        let root = pohunek_test_support::workspace_root();
        let packages = repo_packages(&root);
        let include =
            regex::Regex::new(r#"include_(?:str|bytes)!\(\s*"([^"$]+)""#).expect("include pattern");
        let mut uncovered = Vec::new();
        for package in &packages {
            let mut sources = Vec::new();
            rust_sources(&root.join(&package.dir), &mut sources);
            for source in sources {
                let text = std::fs::read_to_string(&source).expect("read Rust source");
                let relative = source.strip_prefix(&root).expect("source below root");
                for capture in include.captures_iter(&text) {
                    let target = relative
                        .parent()
                        .expect("source has a parent")
                        .join(&capture[1]);
                    let target = normalize(&target).expect("include stays inside the repository");
                    if is_below(&target, &package.dir) {
                        continue;
                    }
                    let selected = match classify(&target, &packages).0 {
                        Decision::Everything(_) => true,
                        Decision::Packages { names, .. } => names.contains(&package.name),
                        Decision::NoRustTests(_) | Decision::Unmapped => false,
                    };
                    if !selected {
                        uncovered.push(format!("{} -> {target}", relative.display()));
                    }
                }
            }
        }
        assert!(
            uncovered.is_empty(),
            "add these embedded paths to RULES in affected.rs: {uncovered:#?}"
        );
    }

    /// Runs git in the environment's private working directory with a scrubbed
    /// environment; system and global configuration are disabled so the
    /// developer's identity, hooks and signing settings never apply.
    fn git(env: &TestEnv, args: &[&str]) {
        let output = env
            .command("git")
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn changed_files_unite_committed_staged_unstaged_and_untracked_paths() {
        changed_files_scenario();
    }

    /// Variables the child of the ambient-redirect test starts with: each
    /// points Git at a repository, index or worktree that does not exist.
    const AMBIENT_GIT_VARS: [(&str, &str); 4] = [
        ("GIT_DIR", "/nonexistent/pohunek/.git"),
        ("GIT_WORK_TREE", "/nonexistent/pohunek"),
        ("GIT_INDEX_FILE", "/nonexistent/pohunek/index"),
        ("GIT_REFERENCE_BACKEND", "bogus"),
    ];

    /// Child half of [`ambient_git_repository_variables_do_not_redirect_the_queries`].
    #[test]
    #[ignore = "child process of ambient_git_repository_variables_do_not_redirect_the_queries"]
    fn changed_files_scenario_under_ambient_git_variables() {
        changed_files_scenario();
    }

    /// The queries run in a process whose environment points Git elsewhere, as
    /// in a git hook. The environment is set on a child process because
    /// mutating this process's environment would race the other tests.
    #[test]
    fn ambient_git_repository_variables_do_not_redirect_the_queries() {
        let env = TestEnv::new().expect("hermetic test environment");
        let exe = std::env::current_exe().expect("test executable");
        let output = env
            .command(exe)
            .args([
                "--ignored",
                "--exact",
                "affected::tests::changed_files_scenario_under_ambient_git_variables",
            ])
            .envs(AMBIENT_GIT_VARS)
            .output()
            .expect("run scenario child");
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn git_queries_drop_repository_redirecting_variables_and_nothing_else() {
        let command = command_in(Path::new("."), GIT);
        let mut removed: Vec<&str> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(name, _)| name.to_str())
            .collect();
        removed.sort_unstable();
        let mut expected = REPOSITORY_REDIRECTING_VARS.to_vec();
        expected.sort_unstable();
        assert_eq!(removed, expected);
        let other = command_in(Path::new("."), "cargo");
        assert_eq!(other.get_envs().count(), 0);
    }

    fn changed_files_scenario() {
        let env = TestEnv::new().expect("hermetic test environment");
        let dir = env.cwd();
        let write = |path: &str, content: &str| {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
            std::fs::write(path, content).expect("write file");
        };
        git(&env, &["init", "--quiet", "--initial-branch=main"]);
        git(&env, &["config", "user.email", "test@example.invalid"]);
        git(&env, &["config", "user.name", "test"]);
        write("a/moved.rs", "moved\n");
        write("staged.rs", "1\n");
        write("unstaged.rs", "1\n");
        write(".gitignore", "ignored.rs\n");
        git(&env, &["add", "."]);
        git(&env, &["commit", "--quiet", "-m", "base"]);
        git(&env, &["checkout", "--quiet", "-b", "topic"]);
        std::fs::create_dir(dir.join("b")).expect("create move target");
        git(&env, &["mv", "a/moved.rs", "b/moved.rs"]);
        git(&env, &["commit", "--quiet", "-m", "move"]);
        write("staged.rs", "2\n");
        git(&env, &["add", "staged.rs"]);
        write("unstaged.rs", "2\n");
        write("sub/untracked.rs", "new\n");
        write("ignored.rs", "ignored\n");

        resolve_base(dir, Some("missing-ref")).expect_err("an unknown base is rejected");
        assert_eq!(resolve_base(dir, None).expect("main resolves"), "main");
        assert_eq!(
            changed_files(dir, "main").expect("changed files"),
            changed(&[
                "a/moved.rs",
                "b/moved.rs",
                "staged.rs",
                "sub/untracked.rs",
                "unstaged.rs"
            ])
        );
    }
}
