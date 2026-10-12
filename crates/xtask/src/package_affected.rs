//! `cargo xtask package affected`: which official runtime packages a change must test.
//!
//! The repository is the Git top level of the working directory. The changed
//! files are collected exactly like `cargo xtask affected` does (committed diff
//! against the merge base with `--base`, staged, unstaged and untracked files),
//! and each file is classified on its own:
//!
//! - a file that a source file of the repository embeds with `include_str!` or
//!   `include_bytes!` is a core input, wherever it lives;
//! - a file below `runtime-packages/<p>/` or `compat/<p>/` selects package `<p>`
//!   when `<p>` is an official package, and is a core input otherwise;
//! - a file on the reviewed package-neutral allowlist ([`NEUTRAL`]) selects
//!   nothing;
//! - any other file is a core input.
//!
//! A core input selects every official package, so the selection is monotonic:
//! a change never tests fewer packages than any of its files requires. The
//! official packages are the directories under `runtime-packages/`. Every
//! failure (Git, file system, an unreadable listing) exits non-zero before
//! anything is printed on stdout, so a caller never reads an empty selection
//! from a failed run.

// Rust guideline compliant 2026-10-12

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::affected::{
    changed_files, command_output, parse_nul_list, printable, resolve_base, Pattern, GIT,
};
use crate::XtaskError;

/// Directory of the official runtime packages; each subdirectory is one package.
pub(crate) const PACKAGES_DIR: &str = "runtime-packages";

/// Directory of the per-runtime compatibility locks, captured screens and npm projects.
pub(crate) const LOCKS_DIR: &str = "compat";

/// Directories whose `<dir>/<package>/` subtrees belong to one package.
const PACKAGE_ROOTS: [&str; 2] = [PACKAGES_DIR, LOCKS_DIR];

/// Paths below the allowlist that are still core inputs, with the reason.
const CORE_EXCEPTIONS: &[(Pattern, &str)] = &[(
    Pattern::Dir("docs/knowledge"),
    "knowledge bundle embedded by pohunek-knowledge, which the CLI links",
)];

/// Reviewed paths that no package build, package test or linked crate reads.
///
/// Kept deliberately small: a path belongs here only when it is proven never
/// to reach a package job's inputs (the package archive build, the CLI,
/// daemon and worker binaries, and the `*_package` tests with their fixtures).
/// Anything not listed is a core input, so a missing entry costs CI time and
/// never test evidence. [`CORE_EXCEPTIONS`] take precedence.
const NEUTRAL: &[(Pattern, &str)] = &[
    (
        Pattern::Dir("docs"),
        "prose outside docs/knowledge; read only by `cargo xtask docs check` and by tests outside the package jobs",
    ),
    (
        Pattern::RootSuffix(".md"),
        "root prose; read only by `cargo xtask docs check`",
    ),
    (
        Pattern::Dir(".github/ISSUE_TEMPLATE"),
        "issue forms; read only by GitHub",
    ),
    (
        Pattern::File(".github/pull_request_template.md"),
        "pull request template; read only by GitHub",
    ),
];

/// Matches a string-literal `include_str!`/`include_bytes!` argument.
///
/// An argument built with `concat!` or `env!` is not matched; such an embed of
/// a package file would have to be listed by review.
const INCLUDE_PATTERN: &str = r#"include_(?:str|bytes)!\(\s*"([^"]+)""#;

/// Options for one `cargo xtask package affected` invocation.
#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Revision to diff against through its merge base with HEAD
    /// [default: origin/main, else main; fails when neither exists].
    #[arg(long, value_name = "REF")]
    base: Option<String>,
    /// Print only the selection as one JSON object on stdout
    /// (`{"core_changed": bool, "packages": [..]}`); the per-file report goes
    /// to stderr.
    #[arg(long)]
    json: bool,
}

/// Why one changed file selects what it selects.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Decision {
    /// The file is an input of every package.
    Core(String),
    /// The file belongs to this official package only.
    Package(String),
    /// The file is on the reviewed package-neutral allowlist.
    Neutral(&'static str),
}

impl Decision {
    fn describe(&self) -> String {
        match self {
            Self::Core(why) => format!("core ({why})"),
            Self::Package(name) => format!("package {name}"),
            Self::Neutral(why) => format!("no package ({why})"),
        }
    }
}

/// The packages a change selects, as printed by `--json`.
#[derive(Debug, Serialize)]
struct Selection {
    /// Whether any changed file is a core input.
    core_changed: bool,
    /// Selected official packages, ascending; every package when `core_changed`.
    packages: BTreeSet<String>,
}

/// Prints the official runtime packages the current change selects.
pub(crate) fn run(options: &Options) -> Result<(), XtaskError> {
    let cwd = std::env::current_dir().map_err(|source| XtaskError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let root = git_toplevel(&cwd)?;
    let official = official_packages(&root)?;
    let base = resolve_base(&root, options.base.as_deref())?;
    let changed = changed_files(&root, &base)?;
    let embedded = embedded_paths(&root)?;

    let decisions: Vec<(String, Decision)> = changed
        .iter()
        .map(|path| (path.clone(), classify(path, &official, &embedded)))
        .collect();
    let selection = select(&decisions, &official);
    let report = render_report(&base, &decisions, &selection);

    if options.json {
        let json = serde_json::to_string(&selection).map_err(XtaskError::Json)?;
        eprint!("{report}");
        println!("{json}");
    } else {
        print!("{report}");
    }
    Ok(())
}

/// Returns the top level of the Git work tree containing `dir`.
pub(crate) fn git_toplevel(dir: &Path) -> Result<PathBuf, XtaskError> {
    let mut output = command_output(dir, GIT, &["rev-parse", "--show-toplevel"])?;
    if output.last() == Some(&b'\n') {
        output.pop();
    }
    Ok(PathBuf::from(OsString::from_vec(output)))
}

/// Lists the official packages: the directories directly under `runtime-packages/`.
///
/// A symbolic link, a name that is not UTF-8, or an empty listing fails: none
/// of them occurs in a sound checkout, and selecting from a partial listing
/// could skip a package.
pub(crate) fn official_packages(root: &Path) -> Result<BTreeSet<String>, XtaskError> {
    let dir = root.join(PACKAGES_DIR);
    let io_error = |source| XtaskError::Io {
        path: dir.clone(),
        source,
    };
    let mut packages = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let file_type = entry.file_type().map_err(io_error)?;
        if file_type.is_symlink() {
            return Err(XtaskError::Usage(format!(
                "package affected: `{}` is a symbolic link",
                entry.path().display()
            )));
        }
        if !file_type.is_dir() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|name| XtaskError::InvalidPath(dir.join(name)))?;
        packages.insert(name);
    }
    if packages.is_empty() {
        return Err(XtaskError::Usage(format!(
            "package affected: `{}` holds no package directory",
            dir.display()
        )));
    }
    Ok(packages)
}

/// Repository-relative paths that a Rust source of the repository embeds.
///
/// Scans the tracked and the untracked, not ignored `.rs` files of the work
/// tree. A listed file that no longer exists (an uncommitted deletion) embeds
/// nothing; it is a changed file and classified on its own.
fn embedded_paths(root: &Path) -> Result<BTreeSet<String>, XtaskError> {
    let include = regex::Regex::new(INCLUDE_PATTERN)
        .map_err(|error| XtaskError::Usage(format!("package affected: {error}")))?;
    let listing = command_output(
        root,
        GIT,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.rs",
        ],
    )?;
    let mut embedded = BTreeSet::new();
    for source in parse_nul_list(&listing) {
        let path = root.join(&source);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(XtaskError::Io { path, source }),
        };
        let dir = Path::new(&source).parent().unwrap_or_else(|| Path::new(""));
        for capture in include.captures_iter(&text) {
            if let Some(target) = normalize(&dir.join(&capture[1])) {
                embedded.insert(target);
            }
        }
    }
    Ok(embedded)
}

/// Resolves `.` and `..` lexically into a `/`-separated relative path.
///
/// Returns `None` for an absolute path or one that climbs above the
/// repository root: such a file is never a changed repository path.
fn normalize(path: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

/// Classifies one repository-relative, `/`-separated changed path.
fn classify(path: &str, official: &BTreeSet<String>, embedded: &BTreeSet<String>) -> Decision {
    if embedded.contains(path) {
        return Decision::Core("embedded by a Rust source of the repository".to_owned());
    }
    for dir in PACKAGE_ROOTS {
        let Some(rest) = path
            .strip_prefix(dir)
            .and_then(|rest| rest.strip_prefix('/'))
        else {
            continue;
        };
        return match rest.split_once('/') {
            Some((name, _)) if official.contains(name) => Decision::Package(name.to_owned()),
            Some((name, _)) => Decision::Core(format!(
                "`{dir}/{name}` is not an official package directory"
            )),
            None => Decision::Core(format!("`{dir}/` file outside any package directory")),
        };
    }
    if let Some((_, why)) = CORE_EXCEPTIONS
        .iter()
        .find(|(pattern, _)| pattern.matches(path))
    {
        return Decision::Core((*why).to_owned());
    }
    match NEUTRAL.iter().find(|(pattern, _)| pattern.matches(path)) {
        Some((_, why)) => Decision::Neutral(why),
        None => Decision::Core("not on the reviewed package-neutral allowlist".to_owned()),
    }
}

/// Combines the per-file decisions; a core input selects every official package.
fn select(decisions: &[(String, Decision)], official: &BTreeSet<String>) -> Selection {
    let core_changed = decisions
        .iter()
        .any(|(_, decision)| matches!(decision, Decision::Core(_)));
    let packages = if core_changed {
        official.clone()
    } else {
        decisions
            .iter()
            .filter_map(|(_, decision)| match decision {
                Decision::Package(name) => Some(name.clone()),
                Decision::Core(_) | Decision::Neutral(_) => None,
            })
            .collect()
    };
    Selection {
        core_changed,
        packages,
    }
}

/// Human report: base, one reason per file, and the selection.
fn render_report(base: &str, decisions: &[(String, Decision)], selection: &Selection) -> String {
    let mut out = format!("package affected: base {}\n", printable(base));
    if decisions.is_empty() {
        out.push_str("package affected: no changed files\n");
    }
    for (path, decision) in decisions {
        let _ = writeln!(
            out,
            "package affected: {} -> {}",
            printable(path),
            decision.describe()
        );
    }
    let names: Vec<&str> = selection.packages.iter().map(String::as_str).collect();
    let _ = writeln!(
        out,
        "package affected: core_changed {}, packages [{}]",
        selection.core_changed,
        names.join(", ")
    );
    out
}
