//! `cargo xtask affected guard`: every repository path a test reads selects it.
//!
//! Test code reaches repository files at run time through
//! `pohunek_test_support::workspace_root()` and `manifest_dir()` (the
//! `test_artifact_lookup_guard` keeps compile-time lookups out of test code),
//! or through a path relative to the package directory, which nextest makes
//! the working directory. The guard finds those reads in the test code of every
//! package, as `xtask::test_code` classifies it:
//!
//! - an anchor call followed by a chain of `.join(..)` calls. The anchors are
//!   `workspace_root()`, `manifest_dir()`, and every argument-less test
//!   function of the same package whose body is an anchor expression, with
//!   every path its definitions in the package can be. A string literal or a
//!   string constant of the package is a path segment, and a constant the
//!   package defines with several values reads each of them; a plain variable
//!   is the wildcard `*`, one path component; any other argument ends the
//!   chain, and the resolved prefix is read as a directory;
//! - a `let` binding of such an expression: each later use of the bound name
//!   reads the bound path, extended by the `.join(..)` chain that follows it;
//! - a string literal that starts with `../`, relative to the package directory.
//!
//! A read path is matched against the tracked files: the files it names and
//! the files below it. Each of them must select the reader when it changes: it
//! lies in the reader's directory, or its classification ([`RULES`] through
//! the closure) selects the reader, or it selects every test. A rule that
//! names tests of the reader covers the read only when its filter provably
//! selects the test that makes the read: the guard places each source file
//! in the module trees of the package's targets (`#[path]` included, see
//! [`crate::test_code`]) and locates each read in its crate, module and test
//! function. A read in a helper outside a test function is covered only by a
//! filter that selects its whole crate or binary. A use that resolves to the repository root itself (a whole-repository
//! scan, or the root passed on) is not a path the guard can judge: its file
//! must be listed in [`SCANS`], whose declared scopes are checked in its place.
//!
//! Test code also runs workspace binaries by path. The guard finds each lookup
//! of one: `bin_exe(..)` or `.with_file_name(..)` with a literal or string
//! constant that names a workspace binary, `worker_binary()`, and a
//! `var_os(..)`/`var(..)` read of a variable in [`BINARY_VARIABLES`]; a
//! constant may come from any file of the package, and one defined with
//! several values looks up each of them. A change to the binary's
//! package must select the test: through a Cargo edge (the test's package
//! depends on the binary's), or through an entry in [`BINARY_USES`] whose
//! filter selects it. A `bin_exe`, `var_os` or `var` call whose argument is
//! neither a literal nor such a constant may run any binary, so it fails
//! unless its function is in [`LOOKUP_FUNCTIONS`] with the binaries it may
//! look up.
//!
//! Limits: a path built at run time (`format!`, a field, a value read from a
//! file) is not found. The runtime guards of the CI test jobs cover those.
//!
//! [`RULES`]: super::RULES
//! [`BINARY_USES`]: super::BINARY_USES

// Rust guideline compliant 2026-10-16

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, PathBuf};

use super::{
    binary_owner, classify, command_output, ensure_git_toplevel, graph, is_below, owning_package,
    parse_nul_list, printable, workspace, BinaryUse, Decision, Effect, Mode, Package, Pattern,
    Reason, BINARY_USES, GIT, LANE_NEUTRAL, RULES,
};
use crate::test_code::{
    classify as classify_sources, is_ident, read_sources, skip_whitespace, string_literal_at,
    CrateRoot, SourceFile,
};
use crate::XtaskError;

/// Native macOS lane packages; the structural lane-neutral test checks this
/// list against `scripts/test-partitions`, which owns the lane partition.
pub(crate) const LANE_PACKAGES: &[&str] = &[
    "pohunek-platform",
    "pohunek-paths",
    "pohunek-worker-protocol",
    "pohunek-session-worker",
    "pohunek-service-config",
    "pohunek-test-support",
    "pohunek-daemon",
    "pohunek-cli",
    "pohunek-hostcheck",
];

/// Test-support functions that return a repository directory, and what they return.
const ANCHORS: [(&str, Base); 2] = [
    ("workspace_root", Base::Root),
    ("manifest_dir", Base::Package),
];

/// Method that extends a path by one argument.
const JOIN: &str = ".join(";

/// Path component that matches any one component.
const WILDCARD: &str = "*";

/// Last path component of a path that stands for any path below the ones
/// before it: `.join(..)` does not extend it, and it is read as that directory.
const ANY_BELOW: &str = "**";

/// Most paths one name or read may stand for. Past it the guard cannot tell
/// which path is read, and judges the read like the repository root itself.
const MAX_PATHS: usize = 256;

/// Calls whose string argument names a binary beside the test's own.
const BINARY_LOOKUPS: [&str; 2] = ["bin_exe(", ".with_file_name("];

/// A test function whose binary lookups take an argument the guard cannot
/// resolve. Each declared binary counts as a run of that binary there.
pub(crate) struct LookupFunction {
    pub(crate) file: &'static str,
    pub(crate) function: &'static str,
    /// The binaries the function may look up.
    pub(crate) binaries: &'static [&'static str],
    pub(crate) why: &'static str,
}

/// The reviewed [`LookupFunction`]s; a stale entry, whose function no longer
/// has such a lookup, is a violation.
pub(crate) const LOOKUP_FUNCTIONS: &[LookupFunction] = &[
    LookupFunction {
        file: "crates/cli/tests/release_consumer.rs",
        function: "the_release_binaries_serve_the_runtime_out_of_process",
        binaries: &["pohunek", "pohunekd", "pohunek-sessiond"],
        why: "reads the POHUNEK_CONSUMER_* inputs, whose bin directory holds the release binaries",
    },
    LookupFunction {
        file: "crates/cli/tests/release_upgrade.rs",
        function: "from_env",
        binaries: &["pohunek", "pohunekd", "pohunek-sessiond"],
        why: "reads the user-manager variables and the bin directories of the previous and head releases",
    },
    LookupFunction {
        file: "crates/cli/tests/release_upgrade.rs",
        function: "run",
        binaries: &[],
        why: "passes the CHILD_ENV variables (PATH, HOME and the user-manager ones) to the CLI it runs",
    },
    LookupFunction {
        file: "crates/cli/tests/support/native.rs",
        function: "binary",
        binaries: &["pohunekd", "pohunek-sessiond"],
        why: "callers pass POHUNEK_DAEMON_BIN or POHUNEK_WORKER_BIN with the binary beside the CLI",
    },
    LookupFunction {
        file: "crates/cli/tests/support/setup_release.rs",
        function: "stage_binaries",
        binaries: &["pohunekd", "pohunek-sessiond"],
        why: "stages pohunekd and pohunek-sessiond from POHUNEK_DAEMON_BIN and POHUNEK_WORKER_BIN",
    },
    LookupFunction {
        file: "crates/daemon/tests/systemd_durable_worker.rs",
        function: "required_binary",
        binaries: &["pohunekd", "pohunek-sessiond"],
        why: "callers pass POHUNEK_DAEMON_BIN or POHUNEK_WORKER_BIN",
    },
    LookupFunction {
        file: "crates/test-support/src/process_env.rs",
        function: "var",
        binaries: &[],
        why: "reads the probe variables of the process-environment tests",
    },
    LookupFunction {
        file: "crates/xtask/tests/dependency_policy.rs",
        function: "host_cargo",
        binaries: &[],
        why: "passes CARGO_HOME and RUSTUP_HOME on to the fixture's cargo",
    },
];

/// Test-support function that returns the session worker, and its binary.
const WORKER_LOOKUP: (&str, &str) = ("worker_binary", "pohunek-sessiond");

/// Calls that read an environment variable.
const VARIABLE_READS: [&str; 2] = ["var_os(", "var("];

/// Environment variables through which test code takes a workspace binary.
pub(crate) const BINARY_VARIABLES: [(&str, &str); 3] = [
    ("POHUNEK_CLI_BIN", "pohunek"),
    ("POHUNEK_DAEMON_BIN", "pohunekd"),
    ("POHUNEK_WORKER_BIN", "pohunek-sessiond"),
];

/// Test files that use the repository root itself, with the paths they read.
///
/// Each entry is `(file, scopes, why)`. A scope is a read path as the guard
/// spells it (workspace-relative, `*` for one component) and is checked like a
/// read found in the file. A file that uses the bare root and is missing here
/// is a violation, as is an entry whose tracked file no longer uses it.
pub(crate) const SCANS: &[(&str, &[&str], &str)] = &[
    (
        "crates/cli/tests/claude_package.rs",
        &["runtime-packages/claude", "compat/claude"],
        "goldens and TOML files of the Claude package, read through helpers that take the path",
    ),
    (
        "crates/cli/tests/codex_package.rs",
        &["runtime-packages/codex", "compat/codex"],
        "goldens and TOML files of the Codex package, read through helpers that take the path",
    ),
    (
        "crates/cli/tests/hermes_package.rs",
        &["runtime-packages/hermes", "compat/hermes"],
        "goldens and TOML files of the Hermes package, read through helpers that take the path",
    ),
    (
        "crates/xtask/src/affected/mod.rs",
        &["Cargo.toml", "crates", "scripts/test-partitions"],
        "member manifests and Rust sources scanned by the include check, and the macOS lane partition",
    ),
    (
        "crates/xtask/src/agent_skill.rs",
        &[
            "docs/knowledge/guides/agent-skill.md",
            "crates/cli/src/commands/agent_skill/SKILL.md",
        ],
        "agent skill source and its generated copy",
    ),
    (
        "crates/xtask/src/attestation.rs",
        &["compat/matrix.json", "compat/*/compatibility-lock.json", "runtime-packages"],
        "matrix check over the compatibility matrix, the locks and the package list",
    ),
    (
        "crates/xtask/src/test_runtime.rs",
        &["runtime-packages/claude", "runtime-packages/codex"],
        "test-runtime scenarios that build the Claude and Codex packages",
    ),
    (
        "crates/xtask/src/upstream_stage_tests.rs",
        &[
            "compat/*/compatibility-lock.json",
            "compat/*/npm",
            "scripts/provision-hermes-compat",
            "runtime-packages",
        ],
        "upstream staging scenarios over the locks, npm projects, the Hermes provisioner and the package list",
    ),
    (
        "crates/xtask/tests/affected_ci.rs",
        &["crates", "rust-toolchain.toml"],
        "the guard run over every crate's test code, and the toolchain pin the fixtures copy",
    ),
    (
        "crates/xtask/tests/dependency_policy.rs",
        &["Cargo.toml", "Cargo.lock", ".clippy.toml", ".cargo", "crates"],
        "workspace resolution and member manifests",
    ),
    (
        "crates/xtask/tests/docs_build.rs",
        &[
            "docs/knowledge",
            "crates/protocol/src/lib.rs",
            "crates/knowledge/src/lib.rs",
        ],
        "knowledge bundle validation and the crate roots it links",
    ),
    (
        "crates/xtask/tests/hermetic_scan.rs",
        &["crates"],
        "scan of every crate's Rust sources",
    ),
    (
        "crates/xtask/tests/no_special_dispatch.rs",
        &["crates"],
        "scan of every crate's Rust sources",
    ),
    (
        "crates/xtask/tests/official_lock_shape.rs",
        &["compat/*/compatibility-lock.json"],
        "shape check of every official compatibility lock",
    ),
    (
        "crates/xtask/tests/test_artifact_lookup_guard.rs",
        &["crates"],
        "scan of every crate's Rust sources",
    ),
    (
        "crates/xtask/tests/exact_filter_guard.rs",
        &["crates"],
        "scan of every crate's Rust sources for literal --exact test filters",
    ),
    (
        "crates/xtask/tests/timing_ratchet.rs",
        &["crates"],
        "scan of every crate's Rust sources against the timing baseline",
    ),
];

/// Resolved reads that are not reads, as `(file, path, why)`.
///
/// A stale entry, whose read the file no longer has, is a violation.
pub(crate) const NOT_READS: &[(&str, &str, &str)] = &[(
    "crates/xtask/src/test_runtime.rs",
    "crates",
    "an output directory inside the checkout that the command refuses before reading",
)];

/// What a read path is relative to.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Base {
    Root,
    Package,
}

/// A function or binding whose value is a repository path.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Anchor {
    /// Each path the value can be: its base and the segments below it.
    paths: BTreeSet<(Base, Vec<String>)>,
}

/// One repository path read by test code.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Read {
    /// Workspace-relative source file of the read.
    pub(crate) file: String,
    pub(crate) line: usize,
    /// Workspace-relative path read, `/`-separated, `*` for one component; empty
    /// for the repository root.
    pub(crate) path: String,
    /// Found from an anchor, so it must name a tracked path; a `../` literal
    /// may name anything.
    pub(crate) anchored: bool,
    pub(crate) location: Location,
}

/// Where a use sits among the tests nextest runs: one site per place of its
/// file in a crate's module tree; none when no target root reaches the file.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Location {
    pub(crate) sites: Vec<Site>,
}

/// One place of a use in a test crate.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Site {
    pub(crate) root: CrateRoot,
    /// Module path of the position in the crate, inline modules included.
    pub(crate) scope: String,
    /// Full nextest name of the test function holding the use, if any.
    pub(crate) test: Option<String>,
}

/// A read whose path does not select the reading package.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Violation {
    pub(crate) read: Read,
    pub(crate) reader: String,
    /// What is wrong: the uncovered file and its decision, or the missing scan.
    pub(crate) problem: String,
}

/// One lookup of a workspace binary by test code.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct BinaryRun {
    /// Workspace-relative source file of the lookup.
    pub(crate) file: String,
    pub(crate) line: usize,
    pub(crate) binary: String,
    pub(crate) location: Location,
}

/// Options for one `cargo xtask affected guard` invocation.
#[derive(Debug, clap::Args)]
pub(crate) struct Options {
    /// Print every read found, with its reader, before the verdict.
    #[arg(long)]
    list: bool,
}

/// Checks the repository of the working directory; fails listing every violation.
pub(crate) fn run(options: &Options) -> Result<(), XtaskError> {
    let cwd = std::env::current_dir().map_err(|source| XtaskError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let (root, packages) = workspace(&cwd)?;
    ensure_git_toplevel(&root)?;
    let tracked: BTreeSet<String> =
        parse_nul_list(&command_output(&root, GIT, &["ls-files", "-z"])?).collect();
    let sources = read_sources(&root)?;
    let classified = classify_sources(&sources);
    let reads = find_reads(&classified, &packages);
    let BinaryLookups { runs, undecided } =
        find_binary_runs(&classified, &packages, LOOKUP_FUNCTIONS);
    if options.list {
        for (reader, read) in &reads {
            println!(
                "{}:{}: {reader} reads {}",
                printable(&read.file),
                read.line,
                shown(&read.path)
            );
        }
        for (reader, run) in &runs {
            println!(
                "{}:{}: {reader} runs the `{}` binary",
                printable(&run.file),
                run.line,
                run.binary
            );
        }
    }
    let violations = check(&reads, &packages, &tracked, SCANS, NOT_READS);
    let mut binary_violations = check_binary_runs(&runs, &packages, &tracked, BINARY_USES);
    binary_violations.extend(check_undecided(&undecided, &tracked, LOOKUP_FUNCTIONS));
    for problem in &binary_violations {
        eprintln!("{problem}");
    }
    if violations.is_empty() && binary_violations.is_empty() {
        println!(
            "affected guard ok: {} repository reads and {} binary runs in test code select their readers",
            reads.len(),
            runs.len()
        );
        return Ok(());
    }
    for violation in &violations {
        eprintln!(
            "{}:{}: {} reads {}: {}",
            printable(&violation.read.file),
            violation.read.line,
            violation.reader,
            shown(&violation.read.path),
            violation.problem
        );
    }
    Err(XtaskError::Usage(format!(
        "affected guard: {} repository reads and {} binary runs of test code are not \
         covered; map each path to its reader in RULES, declare a root scan in SCANS, \
         or add the binary run to BINARY_USES (crates/xtask/src/affected/)",
        violations.len(),
        binary_violations.len()
    )))
}

/// A binary lookup whose argument the guard cannot resolve.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct UndecidedLookup {
    pub(crate) file: String,
    pub(crate) line: usize,
    /// The call, such as `var_os(`.
    pub(crate) call: &'static str,
    /// The function that holds the call, if any.
    pub(crate) function: Option<String>,
}

/// The binary lookups of the test code, each with the package that makes it.
pub(crate) struct BinaryLookups {
    pub(crate) runs: Vec<(String, BinaryRun)>,
    pub(crate) undecided: Vec<(String, UndecidedLookup)>,
}

/// Every lookup of a workspace binary in the test code of `files`, with its
/// package, and every lookup the guard cannot resolve. A lookup in a function
/// of `lookup_functions` counts as a run of each binary its entry declares.
pub(crate) fn find_binary_runs(
    files: &BTreeMap<String, SourceFile>,
    packages: &[Package],
    lookup_functions: &[LookupFunction],
) -> BinaryLookups {
    let mut by_package: BTreeMap<&Package, Vec<&SourceFile>> = BTreeMap::new();
    for file in files.values() {
        if let Some(owner) = owning_package(file.path(), packages) {
            by_package.entry(owner).or_default().push(file);
        }
    }
    let mut runs = BTreeSet::new();
    let mut undecided = BTreeSet::new();
    for (package, package_files) in &by_package {
        // A name may reach an item of another file through a path or a `use`,
        // so it stands for every value the package gives it.
        let constants = package_constants(package_files);
        for file in package_files {
            let mut run = |offset: usize, binary: String| {
                runs.insert((
                    package.name.clone(),
                    BinaryRun {
                        file: file.path().to_owned(),
                        line: file.line_of(offset),
                        binary,
                        location: locate(file, offset),
                    },
                ));
            };
            for (offset, lookup) in binary_lookups(file, &constants) {
                match lookup {
                    Lookup::Binary(binary) => {
                        if binary_owner(&binary, packages).is_some() {
                            run(offset, binary);
                        }
                    }
                    Lookup::Undecided(call) => {
                        let function = enclosing_function(file, offset);
                        let declared = lookup_functions.iter().find(|entry| {
                            entry.file == file.path() && Some(entry.function) == function.as_deref()
                        });
                        if let Some(entry) = declared {
                            for binary in entry.binaries {
                                run(offset, (*binary).to_owned());
                            }
                        }
                        undecided.insert((
                            package.name.clone(),
                            UndecidedLookup {
                                file: file.path().to_owned(),
                                line: file.line_of(offset),
                                call,
                                function,
                            },
                        ));
                    }
                }
            }
        }
    }
    BinaryLookups {
        runs: runs.into_iter().collect(),
        undecided: undecided.into_iter().collect(),
    }
}

/// Every value the files of one package give each string constant name.
fn package_constants(files: &[&SourceFile]) -> BTreeMap<String, BTreeSet<String>> {
    let mut values: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for file in files {
        for (name, value) in string_constants(file) {
            values.entry(name).or_default().extend(value);
        }
    }
    values
}

/// A lookup found in test code.
enum Lookup {
    /// A lookup of the named binary.
    Binary(String),
    /// A `bin_exe` or variable read whose argument is neither a literal nor a
    /// constant the package defines.
    Undecided(&'static str),
}

/// What a call's single argument is.
enum Argument {
    /// A literal, or a constant with each of its values.
    Values(BTreeSet<String>),
    Other,
}

/// The single argument of the call in `file` whose `(` ends at `open`: a
/// literal or a (path-qualified) constant, before an optional trailing comma.
fn call_argument(
    file: &SourceFile,
    constants: &BTreeMap<String, BTreeSet<String>>,
    open: usize,
) -> Argument {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    let at = skip_whitespace(views.code.as_bytes(), open);
    let resolved = if views.code.as_bytes().get(at) == Some(&b'"') && skeleton[at] == b' ' {
        string_literal_at(&views.code, at).map(|literal| {
            (
                BTreeSet::from([literal.to_owned()]),
                literal_end(&views.skeleton, at),
            )
        })
    } else {
        let mut end = at;
        let mut last = at;
        loop {
            let ident = ident_end(skeleton, end);
            if ident == end {
                break;
            }
            last = end;
            end = ident;
            if !views.skeleton[end..].starts_with("::") {
                break;
            }
            end += 2;
        }
        constants
            .get(&views.skeleton[last..end])
            .map(|value| (value.clone(), end))
    };
    let Some((values, end)) = resolved else {
        return Argument::Other;
    };
    let mut next = skip_whitespace(skeleton, end);
    if skeleton.get(next) == Some(&b',') {
        next = skip_whitespace(skeleton, next + 1);
    }
    if skeleton.get(next) == Some(&b')') {
        Argument::Values(values)
    } else {
        Argument::Other
    }
}

/// The binary lookups in the test code of `file`, one for each value an
/// argument can have; the names are not yet checked against the workspace.
/// `constants` resolve identifier arguments.
fn binary_lookups(
    file: &SourceFile,
    constants: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<(usize, Lookup)> {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    let call_at = |offset: usize, call: &str| {
        file.in_test_code(offset)
            && !(offset > 0 && !call.starts_with('.') && is_ident(skeleton[offset - 1]))
            && !views.skeleton[..offset].trim_end().ends_with("fn")
    };
    let argument = |open: usize| call_argument(file, constants, open);

    let mut found = Vec::new();
    for call in BINARY_LOOKUPS {
        for (offset, _) in views.skeleton.match_indices(call) {
            if !call_at(offset, call) {
                continue;
            }
            match argument(offset + call.len()) {
                Argument::Values(binaries) => {
                    // A constant that is not a literal may name any binary.
                    if call == "bin_exe(" && binaries.contains(WILDCARD) {
                        found.push((offset, Lookup::Undecided(call)));
                    }
                    found.extend(
                        binaries
                            .into_iter()
                            .map(|binary| (offset, Lookup::Binary(binary))),
                    );
                }
                // A path method takes any file name; only `bin_exe` names a binary.
                Argument::Other if call == "bin_exe(" => {
                    found.push((offset, Lookup::Undecided(call)));
                }
                Argument::Other => {}
            }
        }
    }
    for call in VARIABLE_READS {
        for (offset, _) in views.skeleton.match_indices(call) {
            if !call_at(offset, call) {
                continue;
            }
            match argument(offset + call.len()) {
                Argument::Values(variables) => {
                    if variables.contains(WILDCARD) {
                        found.push((offset, Lookup::Undecided(call)));
                    }
                    found.extend(
                        BINARY_VARIABLES
                            .iter()
                            .filter(|(name, _)| variables.contains(*name))
                            .map(|(_, binary)| (offset, Lookup::Binary((*binary).to_owned()))),
                    );
                }
                Argument::Other => found.push((offset, Lookup::Undecided(call))),
            }
        }
    }
    let (function, binary) = WORKER_LOOKUP;
    for (offset, _) in views.skeleton.match_indices(function) {
        let open = offset + function.len();
        let called = views.skeleton[open..].starts_with('(')
            && skeleton.get(skip_whitespace(skeleton, open + 1)) == Some(&b')');
        if called
            && call_at(offset, function)
            && !(offset > 0 && is_ident(skeleton[offset - 1]))
            && !is_ident(*skeleton.get(open).unwrap_or(&b' '))
        {
            found.push((offset, Lookup::Binary(binary.to_owned())));
        }
    }
    found
}

/// Name of the innermost function whose body holds `offset`.
fn enclosing_function(file: &SourceFile, offset: usize) -> Option<String> {
    let skeleton = &file.views().skeleton;
    let bytes = skeleton.as_bytes();
    let mut best: Option<(usize, String)> = None;
    for (at, _) in skeleton.match_indices("fn") {
        if at >= offset {
            break;
        }
        if (at > 0 && is_ident(bytes[at - 1]))
            || !bytes.get(at + 2).is_some_and(u8::is_ascii_whitespace)
        {
            continue;
        }
        let start = skip_whitespace(bytes, at + 2);
        let end = ident_end(bytes, start);
        if end == start {
            continue;
        }
        let mut depth = 0_usize;
        let mut open = None;
        for (index, byte) in bytes.iter().enumerate().skip(end) {
            match byte {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth = depth.saturating_sub(1),
                b'{' if depth == 0 => {
                    open = Some(index);
                    break;
                }
                b';' if depth == 0 => break,
                _ => {}
            }
        }
        let Some(open) = open else {
            continue;
        };
        if open < offset && offset < block_end(skeleton, open + 1) {
            best = Some((open, skeleton[start..end].to_owned()));
        }
    }
    best.map(|(_, name)| name)
}

/// The undecided lookups outside the functions of `lookup_functions`, and the
/// stale entries of it, as printable lines.
pub(crate) fn check_undecided(
    undecided: &[(String, UndecidedLookup)],
    tracked: &BTreeSet<String>,
    lookup_functions: &[LookupFunction],
) -> Vec<String> {
    let mut problems = BTreeSet::new();
    let mut matched = BTreeSet::new();
    for (reader, lookup) in undecided {
        let entry = lookup_functions.iter().position(|entry| {
            entry.file == lookup.file && Some(entry.function) == lookup.function.as_deref()
        });
        match entry {
            Some(index) => {
                matched.insert(index);
            }
            None => {
                problems.insert(format!(
                    "{}:{}: {reader} calls `{}..)` with an argument that is neither a literal \
                     nor a constant of the package, so the binary it may run is unknown; \
                     declare {} in LOOKUP_FUNCTIONS (crates/xtask/src/affected/guard.rs)",
                    printable(&lookup.file),
                    lookup.line,
                    lookup.call,
                    lookup
                        .function
                        .as_deref()
                        .map_or_else(|| "its function".to_owned(), |name| format!("`{name}`")),
                ));
            }
        }
    }
    for (index, entry) in lookup_functions.iter().enumerate() {
        if tracked.contains(entry.file) && !matched.contains(&index) {
            problems.insert(format!(
                "LOOKUP_FUNCTIONS: the entry for `{}` in {} ({}) is stale: it has no \
                 undecided lookup",
                entry.function, entry.file, entry.why
            ));
        }
    }
    problems.into_iter().collect()
}

/// The binary runs whose binary's package does not select the running test,
/// and the stale entries of `uses`, as printable lines.
pub(crate) fn check_binary_runs(
    runs: &[(String, BinaryRun)],
    packages: &[Package],
    tracked: &BTreeSet<String>,
    uses: &[BinaryUse],
) -> Vec<String> {
    let mut problems = BTreeSet::new();
    let mut matched = BTreeSet::new();
    for (reader, run) in runs {
        let Some(owner) = binary_owner(&run.binary, packages) else {
            continue;
        };
        let seeds = BTreeMap::from([(
            owner.name.clone(),
            BTreeSet::from([Reason::Changed(String::new())]),
        )]);
        if owner.name == *reader || graph::close(&seeds, packages).contains_key(reader) {
            continue;
        }
        let entry = uses.iter().position(|edge| {
            edge.binary == run.binary
                && edge.readers.package == reader
                && filter_covers(edge.readers.filter, &run.location)
        });
        match entry {
            Some(index) => {
                matched.insert(index);
            }
            None => {
                problems.insert(format!(
                    "{}:{}: {reader} runs the `{}` binary of {}, but a change to {} does not \
                     select it: no Cargo edge and no BINARY_USES entry selects the test in {}",
                    printable(&run.file),
                    run.line,
                    run.binary,
                    owner.name,
                    owner.name,
                    printable(&run.file)
                ));
            }
        }
    }
    for (index, edge) in uses.iter().enumerate() {
        let present = edge.sources.iter().any(|named| {
            tracked
                .iter()
                .any(|file| file == named || is_below(file, named))
        });
        if present && !matched.contains(&index) {
            problems.insert(format!(
                "BINARY_USES: the entry for `{}` in {} is stale: no test source it names runs \
                 the binary outside a Cargo edge",
                edge.binary, edge.readers.package
            ));
        }
    }
    problems.into_iter().collect()
}

/// A read path for display; the empty path is the repository root.
fn shown(path: &str) -> String {
    if path.is_empty() {
        "the repository root".to_owned()
    } else {
        printable(path)
    }
}

/// Every repository read in the test code of `files`, with its reading package.
pub(crate) fn find_reads(
    files: &BTreeMap<String, SourceFile>,
    packages: &[Package],
) -> Vec<(String, Read)> {
    let mut by_package: BTreeMap<&Package, Vec<&SourceFile>> = BTreeMap::new();
    for file in files.values() {
        if let Some(owner) = owning_package(file.path(), packages) {
            by_package.entry(owner).or_default().push(file);
        }
    }
    let mut reads = BTreeSet::new();
    for (package, files) in by_package {
        // A name may reach an item of another file through a path or a `use`,
        // so it stands for every value the package gives it.
        let constants = package_constants(&files);
        let (anchors, bodies) = package_anchors(&files, &constants);
        for file in &files {
            for (offset, path, anchored) in
                reads_in(file, &package.dir, &anchors, &constants, &bodies)
            {
                reads.insert((
                    package.name.clone(),
                    Read {
                        file: file.path().to_owned(),
                        line: file.line_of(offset),
                        path,
                        anchored,
                        location: locate(file, offset),
                    },
                ));
            }
        }
    }
    reads.into_iter().collect()
}

/// The anchors of one package: [`ANCHORS`] and the wrapper functions of its test code.
///
/// A wrapper is an argument-less function in test code whose body is an anchor
/// expression. A name stands for every path any of its definitions in the
/// package can be, since a call may reach any of them through a path or a
/// `use`; the search repeats until no name gains a path, so wrappers of
/// wrappers resolve.
///
/// Names whose definitions call each other form a cycle, whose paths grow
/// without end. Each of those paths is a path the cycle is entered with,
/// extended by the segments of its definitions, so every name of the cycle
/// stands for any path below each of them ([`ANY_BELOW`]). A definition in a
/// cycle that can climb (`..` or an absolute segment) can lead anywhere, so the
/// cycle also stands for a root path whose first component is unknown, which
/// is judged like the root itself.
///
/// Also returns the `(file, offset)` of each wrapper's body call, which
/// defines the wrapper and reads nothing itself.
fn package_anchors(
    files: &[&SourceFile],
    constants: &BTreeMap<String, BTreeSet<String>>,
) -> (BTreeMap<String, Anchor>, BTreeSet<(String, usize)>) {
    let mut anchors: BTreeMap<String, Anchor> = ANCHORS
        .iter()
        .map(|(name, base)| {
            (
                (*name).to_owned(),
                Anchor {
                    paths: BTreeSet::from([(*base, Vec::new())]),
                },
            )
        })
        .collect();
    let definitions: Vec<(&SourceFile, Wrapper)> = files
        .iter()
        .flat_map(|file| {
            wrappers(file, constants)
                .into_iter()
                .map(move |w| (*file, w))
        })
        .collect();
    let mut calls: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (_, wrapper) in &definitions {
        calls
            .entry(wrapper.name.as_str())
            .or_default()
            .insert(wrapper.called.as_str());
    }
    let in_cycle = |wrapper: &Wrapper| reaches(&calls, &wrapper.called, &wrapper.name);
    // Each cycle as its names, and whether one of its definitions can climb.
    let mut cycles: BTreeMap<BTreeSet<&str>, bool> = BTreeMap::new();
    for (_, wrapper) in definitions.iter().filter(|(_, wrapper)| in_cycle(wrapper)) {
        let names: BTreeSet<&str> = calls
            .keys()
            .copied()
            .filter(|name| {
                reaches(&calls, &wrapper.name, name) && reaches(&calls, name, &wrapper.name)
            })
            .collect();
        *cycles.entry(names).or_default() |= wrapper
            .segments
            .iter()
            .flatten()
            .any(|segment| climbs(segment));
    }
    loop {
        let mut next = anchors.clone();
        for (_, wrapper) in definitions.iter().filter(|(_, wrapper)| !in_cycle(wrapper)) {
            if let Some(called) = anchors.get(&wrapper.called) {
                let anchor = next.entry(wrapper.name.clone()).or_insert_with(|| Anchor {
                    paths: BTreeSet::new(),
                });
                anchor
                    .paths
                    .extend(extended(&called.paths, &wrapper.segments));
                anchor.paths = bounded(std::mem::take(&mut anchor.paths));
            }
        }
        for (names, climbs) in &cycles {
            let mut paths: BTreeSet<(Base, Vec<String>)> = names
                .iter()
                .filter_map(|name| next.get(*name))
                .flat_map(|anchor| anchor.paths.iter().cloned())
                .map(|(base, mut path)| {
                    if path.last().map(String::as_str) != Some(ANY_BELOW) {
                        path.push(ANY_BELOW.to_owned());
                    }
                    (base, path)
                })
                .collect();
            if paths.is_empty() {
                continue;
            }
            if *climbs {
                paths.insert(unknown_path());
            }
            let paths = bounded(paths);
            for name in names {
                next.insert(
                    (*name).to_owned(),
                    Anchor {
                        paths: paths.clone(),
                    },
                );
            }
        }
        if next == anchors {
            break;
        }
        anchors = next;
    }
    let bodies = definitions
        .iter()
        .filter(|(_, wrapper)| anchors.contains_key(&wrapper.called))
        .map(|(file, wrapper)| (file.path().to_owned(), wrapper.body))
        .collect();
    (anchors, bodies)
}

/// Whether a chain of calls leads from the function named `from` to `to`.
fn reaches(calls: &BTreeMap<&str, BTreeSet<&str>>, from: &str, to: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut pending = vec![from];
    while let Some(name) = pending.pop() {
        if name == to {
            return true;
        }
        if seen.insert(name) {
            pending.extend(calls.get(name).into_iter().flatten().copied());
        }
    }
    false
}

/// One definition of a wrapper.
struct Wrapper {
    name: String,
    /// The function its body calls.
    called: String,
    /// The `.join(..)` arguments after the call, each with its values.
    segments: Vec<BTreeSet<String>>,
    /// Offset of the body's call.
    body: usize,
}

/// Argument-less test functions of `file` whose body is a call of an
/// argument-less function followed by a `.join(..)` chain; they are wrappers
/// when the called function is an anchor.
fn wrappers(file: &SourceFile, constants: &BTreeMap<String, BTreeSet<String>>) -> Vec<Wrapper> {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    let mut found = Vec::new();
    for (offset, _) in views.skeleton.match_indices("fn ") {
        if !file.in_test_code(offset) || (offset > 0 && is_ident(skeleton[offset - 1])) {
            continue;
        }
        let name_start = skip_whitespace(skeleton, offset + 3);
        let name_end = ident_end(skeleton, name_start);
        let open = skip_whitespace(skeleton, name_end);
        if name_end == name_start || !views.skeleton[open..].starts_with("()") {
            continue;
        }
        let Some(body) = views.skeleton[open..].find('{').map(|i| open + i) else {
            continue;
        };
        let start = skip_whitespace(skeleton, body + 1);
        let Some((called, call_end)) = call_at(&views.skeleton, start) else {
            continue;
        };
        let (segments, end) = join_chain(file, constants, call_end);
        let close = skip_whitespace(skeleton, end);
        if !views.skeleton[close..].starts_with('}') {
            continue;
        }
        found.push(Wrapper {
            name: views.skeleton[name_start..name_end].to_owned(),
            called: called.to_owned(),
            segments,
            body: start,
        });
    }
    found
}

/// The anchor called at `start` (with an optional path prefix such as
/// `pohunek_test_support::`) and the offset after its `()`.
fn anchor_call_at<'a>(
    skeleton: &str,
    start: usize,
    anchors: &'a BTreeMap<String, Anchor>,
) -> Option<(&'a Anchor, usize)> {
    let (name, end) = call_at(skeleton, start)?;
    anchors.get(name).map(|anchor| (anchor, end))
}

/// The name of the argument-less function called at `start` (with an optional
/// path prefix) and the offset after its `()`.
fn call_at(skeleton: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = skeleton.as_bytes();
    let mut at = start;
    loop {
        let end = ident_end(bytes, at);
        if end == at {
            return None;
        }
        if skeleton[end..].starts_with("::") {
            at = end + 2;
            continue;
        }
        let open = skip_whitespace(bytes, end);
        if !skeleton[open..].starts_with('(') {
            return None;
        }
        let close = skip_whitespace(bytes, open + 1);
        return skeleton[close..]
            .starts_with(')')
            .then_some((&skeleton[at..end], close + 1));
    }
}

/// Index one past the identifier starting at `start` (equal to `start` when none does).
fn ident_end(bytes: &[u8], start: usize) -> usize {
    start
        + bytes[start.min(bytes.len())..]
            .iter()
            .take_while(|byte| is_ident(**byte))
            .count()
}

/// Reads in the test code of `file`, as `(offset, workspace-relative path,
/// anchored)`.
fn reads_in(
    file: &SourceFile,
    package_dir: &str,
    anchors: &BTreeMap<String, Anchor>,
    constants: &BTreeMap<String, BTreeSet<String>>,
    bodies: &BTreeSet<(String, usize)>,
) -> Vec<(usize, String, bool)> {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    // (offset, base, segments)
    let mut found: Vec<(usize, Base, Vec<String>)> = Vec::new();
    // (offset, name, anchor)
    let mut bindings: Vec<(usize, String, Anchor)> = Vec::new();

    for name in anchors.keys() {
        for (offset, _) in views.skeleton.match_indices(name.as_str()) {
            let before = &views.skeleton[..offset];
            if !file.in_test_code(offset)
                || (offset > 0 && is_ident(skeleton[offset - 1]))
                || before.trim_end().ends_with("fn")
                || before.ends_with('.')
            {
                continue;
            }
            let mut start = offset;
            while let Some(qualified) = anchor_start(&views.skeleton[..start]) {
                start = qualified;
            }
            if bodies.contains(&(file.path().to_owned(), start)) {
                continue;
            }
            let Some((anchor, call_end)) = anchor_call_at(&views.skeleton, offset, anchors) else {
                continue;
            };
            let (segments, end) = join_chain(file, constants, call_end);
            let paths = extended(&anchor.paths, &segments);
            match bound_name(&views.skeleton, offset, end) {
                Some(binding) => bindings.push((offset, binding, Anchor { paths })),
                None => found.extend(paths.into_iter().map(|(base, path)| (offset, base, path))),
            }
        }
    }

    // Every later use of a bound name in its block, until it is bound again.
    bindings.sort_by_key(|(offset, ..)| *offset);
    for (index, (offset, name, anchor)) in bindings.iter().enumerate() {
        let until = bindings[index + 1..]
            .iter()
            .find(|(_, other, _)| other == name)
            .map_or(views.skeleton.len(), |(other, ..)| *other)
            .min(block_end(&views.skeleton, *offset));
        let statement_end = views.skeleton[*offset..until]
            .find(';')
            .map_or(until, |i| offset + i);
        for (use_offset, _) in views.skeleton[statement_end..until].match_indices(name.as_str()) {
            let at = statement_end + use_offset;
            let end = at + name.len();
            let whole = (at == 0 || !is_ident(skeleton[at - 1]))
                && !skeleton.get(end).copied().is_some_and(is_ident)
                && !views.skeleton[..at].ends_with('.')
                && !views.skeleton[..at].ends_with("::");
            if !whole || !file.in_test_code(at) {
                continue;
            }
            // Formatting a path for a message reads nothing.
            if views.skeleton[end..].starts_with(".display()") {
                continue;
            }
            let (segments, _) = join_chain(file, constants, end);
            found.extend(
                extended(&anchor.paths, &segments)
                    .into_iter()
                    .map(|(base, path)| (at, base, path)),
            );
        }
    }

    let mut reads: Vec<(usize, String, bool)> = found
        .into_iter()
        .filter_map(|(offset, base, mut segments)| {
            if segments.last().map(String::as_str) == Some(ANY_BELOW) {
                segments.pop();
            }
            let start = match base {
                // A root path whose first component is unknown can be any
                // path: it is judged like the root itself.
                Base::Root if segments.first().is_some_and(|first| first == WILDCARD) => {
                    return Some((offset, String::new(), true));
                }
                Base::Root => "",
                Base::Package => package_dir,
            };
            Some((offset, normalize(start, &segments)?, true))
        })
        .collect();

    // Literals relative to the package directory, the test's working directory.
    for (offset, literal) in literals(file) {
        if literal.starts_with("../") {
            if let Some(path) = normalize(package_dir, &[literal.to_owned()]) {
                if !path.is_empty() {
                    reads.push((offset, path, false));
                }
            }
        }
    }
    reads
}

/// Offset of the `}` that closes the innermost block containing `offset`, or
/// the text length when the block is not closed.
fn block_end(skeleton: &str, offset: usize) -> usize {
    let bytes = skeleton.as_bytes();
    let mut depth = 0_usize;
    for (index, byte) in bytes.iter().enumerate().skip(offset) {
        match byte {
            b'{' => depth += 1,
            b'}' if depth == 0 => return index,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    bytes.len()
}

/// Start of the path-qualified call ending at `before` (which ends with `::`).
fn anchor_start(before: &str) -> Option<usize> {
    let trimmed = before.strip_suffix("::")?;
    let start = trimmed
        .bytes()
        .rposition(|byte| !is_ident(byte))
        .map_or(0, |index| index + 1);
    (start < trimmed.len()).then_some(start)
}

/// Whether joining `segment` can leave the path it extends: a `..` or an
/// absolute path.
fn climbs(segment: &str) -> bool {
    PathBuf::from(segment)
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::RootDir))
}

/// A root path whose first component is unknown: any path of the repository.
fn unknown_path() -> (Base, Vec<String>) {
    (Base::Root, vec![WILDCARD.to_owned(), ANY_BELOW.to_owned()])
}

/// `paths`, or only [`unknown_path`] when they hold it or exceed [`MAX_PATHS`].
fn bounded(paths: BTreeSet<(Base, Vec<String>)>) -> BTreeSet<(Base, Vec<String>)> {
    if paths.len() > MAX_PATHS || paths.contains(&unknown_path()) {
        BTreeSet::from([unknown_path()])
    } else {
        paths
    }
}

/// Every path `paths` extended by `segments` can be: one for each choice of a
/// value per segment, within [`MAX_PATHS`] (see [`bounded`]). A path that ends
/// in [`ANY_BELOW`] already stands for every extension that stays below it; one
/// that can climb out of it can lead anywhere.
fn extended(
    paths: &BTreeSet<(Base, Vec<String>)>,
    segments: &[BTreeSet<String>],
) -> BTreeSet<(Base, Vec<String>)> {
    segments
        .iter()
        .fold(bounded(paths.clone()), |paths, values| {
            // Checked before the product is built, so it is never allocated.
            if paths.len().saturating_mul(values.len()) > MAX_PATHS {
                return BTreeSet::from([unknown_path()]);
            }
            paths
                .iter()
                .flat_map(|(base, path)| {
                    if path.last().map(String::as_str) == Some(ANY_BELOW) {
                        if values.iter().any(|value| climbs(value)) {
                            return vec![unknown_path()];
                        }
                        return vec![(*base, path.clone())];
                    }
                    values
                        .iter()
                        .map(|value| {
                            let mut path = path.clone();
                            path.push(value.clone());
                            (*base, path)
                        })
                        .collect()
                })
                .collect()
        })
}

/// Arguments of the `.join(..)` chain starting at `from`, each with the values
/// it can have, and the offset after the last call taken.
///
/// A string literal is a segment, a string constant of the package is each of
/// its values, and a plain variable is [`WILDCARD`]; any other argument ends
/// the chain, so the path read is the prefix resolved so far. A variable never
/// shares a constant's name: CI denies the `non_snake_case` and
/// `non_upper_case_globals` lints that keep the two apart.
fn join_chain(
    file: &SourceFile,
    constants: &BTreeMap<String, BTreeSet<String>>,
    mut from: usize,
) -> (Vec<BTreeSet<String>>, usize) {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    // Whitespace before an argument is skipped in the code view: the skeleton
    // blanks a literal into spaces, which would be skipped with it.
    let code = views.code.as_bytes();
    let mut segments = Vec::new();
    loop {
        let at = skip_whitespace(skeleton, from);
        if !views.skeleton[at..].starts_with(JOIN) {
            return (segments, from);
        }
        let mut arg = skip_whitespace(code, at + JOIN.len());
        if views.skeleton[arg..].starts_with('&') {
            arg = skip_whitespace(code, arg + 1);
        }
        let (values, end) = if let Some(literal) = string_literal_at(&views.code, arg) {
            (
                BTreeSet::from([literal.to_owned()]),
                literal_end(&views.skeleton, arg),
            )
        } else {
            let end = ident_end(skeleton, arg);
            if end == arg {
                return (segments, from);
            }
            let values = constants
                .get(&views.skeleton[arg..end])
                .cloned()
                .unwrap_or_else(|| BTreeSet::from([WILDCARD.to_owned()]));
            (values, end)
        };
        let close = skip_whitespace(skeleton, end);
        if !views.skeleton[close..].starts_with(')') {
            return (segments, from);
        }
        segments.push(values);
        from = close + 1;
    }
}

/// Offset one past the string literal starting at `start`.
///
/// The skeleton blanks a literal, quotes included, into spaces, so its end is
/// where the blanks stop.
fn literal_end(skeleton: &str, start: usize) -> usize {
    let bytes = skeleton.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end] == b' ' {
        end += 1;
    }
    end
}

/// The name bound by `let [mut] <name> = <expression from anchor to end>;`.
fn bound_name(skeleton: &str, anchor: usize, end: usize) -> Option<String> {
    let mut start = anchor;
    while let Some(qualified) = anchor_start(&skeleton[..start]) {
        start = qualified;
    }
    let before = skeleton[..start].trim_end().strip_suffix('=')?.trim_end();
    let name_start = before
        .bytes()
        .rposition(|byte| !is_ident(byte))
        .map_or(0, |index| index + 1);
    let name = &before[name_start..];
    let head = before[..name_start].trim_end();
    let head = head.strip_suffix("mut").map_or(head, str::trim_end);
    if name.is_empty() || !head.ends_with("let") {
        return None;
    }
    let after = skip_whitespace(skeleton.as_bytes(), end);
    skeleton[after..].starts_with(';').then(|| name.to_owned())
}

/// Every value of each `const` and `static` string item of `file`, by name;
/// items in different scopes can share a name. An item whose value is not a
/// literal (`concat!(..)`, another constant) has the value [`WILDCARD`], so
/// a same-named literal elsewhere cannot stand for it.
fn string_constants(file: &SourceFile) -> BTreeMap<String, BTreeSet<String>> {
    let views = file.views();
    let skeleton = views.skeleton.as_bytes();
    let mut values: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for keyword in ["const ", "static "] {
        for (offset, _) in views.skeleton.match_indices(keyword) {
            if offset > 0 && is_ident(skeleton[offset - 1]) {
                continue;
            }
            let name_start = skip_whitespace(skeleton, offset + keyword.len());
            let name_end = ident_end(skeleton, name_start);
            let colon = skip_whitespace(skeleton, name_end);
            if name_end == name_start || !views.skeleton[colon..].starts_with(':') {
                continue;
            }
            let Some(eq) = views.skeleton[colon..].find('=').map(|i| colon + i) else {
                continue;
            };
            let ty = views.skeleton[colon + 1..eq].trim();
            if ty != "&str" && ty != "&'static str" {
                continue;
            }
            let value_at = skip_whitespace(views.code.as_bytes(), eq + 1);
            let value = string_literal_at(&views.code, value_at).unwrap_or(WILDCARD);
            values
                .entry(views.skeleton[name_start..name_end].to_owned())
                .or_default()
                .insert(value.to_owned());
        }
    }
    values
}

/// String literals in the test code of `file`, as `(offset, content)`.
fn literals(file: &SourceFile) -> Vec<(usize, &str)> {
    let views = file.views();
    let code = views.code.as_bytes();
    let skeleton = views.skeleton.as_bytes();
    let mut found = Vec::new();
    let mut offset = 0;
    while offset < code.len() {
        let starts = code[offset] == b'"' && skeleton[offset] == b' ';
        if starts && file.in_test_code(offset) {
            if let Some(literal) = string_literal_at(&views.code, offset) {
                found.push((offset, literal));
                offset = literal_end(&views.skeleton, offset).max(offset + 1);
                continue;
            }
        }
        offset += 1;
    }
    found
}

/// `segments` joined below `start` with `.` and `..` resolved, or `None` when
/// a segment is absolute or the path leaves the repository. The repository
/// root itself is the empty string.
fn normalize(start: &str, segments: &[String]) -> Option<String> {
    let mut joined = PathBuf::from(start);
    for segment in segments {
        joined.push(segment);
    }
    let mut parts: Vec<String> = Vec::new();
    for component in joined.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

/// Whether the tracked `file` is named by `pattern` or lies below a path it names.
fn matches(pattern: &str, file: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let file: Vec<&str> = file.split('/').collect();
    file.len() >= pattern.len()
        && pattern
            .iter()
            .zip(&file)
            .all(|(want, have)| *want == WILDCARD || want == have)
}

/// The violations among `reads`, judged against the tracked files and `scans`.
pub(crate) fn check(
    reads: &[(String, Read)],
    packages: &[Package],
    tracked: &BTreeSet<String>,
    scans: &[(&str, &[&str], &str)],
    not_reads: &[(&str, &str, &str)],
) -> Vec<Violation> {
    let mut violations = BTreeSet::new();
    let mut scanned = BTreeSet::new();
    let mut excused = BTreeSet::new();
    for (reader, read) in reads {
        if let Some(entry) = not_reads
            .iter()
            .find(|(file, path, _)| *file == read.file && *path == read.path)
        {
            excused.insert((entry.0, entry.1));
            continue;
        }
        let paths: Vec<String> = if read.path.is_empty() {
            if let Some((file, scopes, _)) = scans.iter().find(|(file, ..)| *file == read.file) {
                scanned.insert(*file);
                scopes.iter().map(|scope| (*scope).to_owned()).collect()
            } else {
                violations.insert(Violation {
                    read: read.clone(),
                    reader: reader.clone(),
                    problem: "the file uses the repository root itself; declare what it \
                              reads in SCANS (crates/xtask/src/affected/guard.rs)"
                        .to_owned(),
                });
                continue;
            }
        } else {
            vec![read.path.clone()]
        };
        for path in paths {
            if lane_uncovered(&path, reader, tracked) {
                violations.insert(Violation {
                    read: Read {
                        path: path.clone(),
                        ..read.clone()
                    },
                    reader: reader.clone(),
                    problem: "the macOS lane omits changes to this LANE_NEUTRAL path; \
                              direct runtime reads by lane package tests are not covered"
                        .to_owned(),
                });
            }
            let mut named = tracked
                .iter()
                .filter(|file| matches(&path, file))
                .peekable();
            // A path that names no tracked file (an absence check, or an input
            // that moved) must still select the reader once a file is there.
            let absent = (read.anchored && named.peek().is_none() && !path.contains(WILDCARD))
                .then(|| uncovered(&path, reader, read, packages).map(|why| (&path, why)))
                .flatten();
            let uncovered = absent.or_else(|| {
                named
                    .find_map(|file| uncovered(file, reader, read, packages).map(|why| (file, why)))
            });
            if let Some((file, why)) = uncovered {
                violations.insert(Violation {
                    read: Read {
                        path: path.clone(),
                        ..read.clone()
                    },
                    reader: reader.clone(),
                    problem: format!("a change to {} does not select it: {why}", printable(file)),
                });
            }
        }
    }
    // An entry for a file that is not tracked here judges nothing; only an
    // entry whose file exists and no longer has the use is stale.
    for (file, path, _) in not_reads {
        if tracked.contains(*file) && !excused.contains(&(*file, *path)) {
            violations.insert(Violation {
                read: Read {
                    file: (*file).to_owned(),
                    line: 0,
                    path: (*path).to_owned(),
                    anchored: false,
                    location: Location::default(),
                },
                reader: String::from("NOT_READS"),
                problem: "the entry is stale: the file no longer has this read".to_owned(),
            });
        }
    }
    for (file, _, _) in scans {
        if tracked.contains(*file) && !scanned.contains(file) {
            violations.insert(Violation {
                read: Read {
                    file: (*file).to_owned(),
                    line: 0,
                    path: String::new(),
                    anchored: false,
                    location: Location::default(),
                },
                reader: String::from("SCANS"),
                problem: "the entry is stale: the file no longer uses the repository root"
                    .to_owned(),
            });
        }
    }
    violations.into_iter().collect()
}

/// Whether a direct read reaches data omitted from the reader's macOS lane.
fn lane_uncovered(path: &str, reader: &str, tracked: &BTreeSet<String>) -> bool {
    LANE_PACKAGES.contains(&reader)
        && LANE_NEUTRAL.iter().any(|data| {
            data.pattern.matches(path)
                || matches!(data.pattern, Pattern::Dir(dir) if path == dir)
                || tracked
                    .iter()
                    .any(|file| matches(path, file) && data.pattern.matches(file))
        })
}

/// Whether `path` lies in the directory of package `reader`.
fn is_own(path: &str, reader: &str, packages: &[Package]) -> bool {
    packages.iter().any(|package| {
        package.name == reader
            && (package.dir.is_empty() || path == package.dir || is_below(path, &package.dir))
    })
}

/// The decision of `file` when its change does not select the test code of
/// package `reader` that makes `read`.
///
/// A narrowed rule covers the read only when it names the read's source among
/// the sources its filter selects and its filter selects the test holding the
/// read; naming the package or the file is not enough.
fn uncovered(file: &str, reader: &str, read: &Read, packages: &[Package]) -> Option<String> {
    let source = read.file.as_str();
    if is_own(file, reader, packages) {
        return None;
    }
    let (decision, _) = classify(file, packages, Mode::Local);
    match &decision {
        Decision::Everything(_) | Decision::Unmapped => None,
        Decision::Packages { names, tests, .. } => {
            let seeds = names
                .iter()
                .map(|name| {
                    (
                        name.clone(),
                        BTreeSet::from([Reason::Changed(String::new())]),
                    )
                })
                .collect();
            let selected = graph::close(&seeds, packages);
            let narrowed = tests.iter().any(|(package, ..)| package == reader)
                && narrowed_covers(file, reader, read);
            let covered = selected.contains_key(reader) || narrowed;
            (!covered).then(|| {
                format!(
                    "{} (no rule selects the test in {} that makes the read)",
                    decision.describe(),
                    printable(source)
                )
            })
        }
        Decision::Dependencies(_) | Decision::NoRustTests(_) => Some(decision.describe()),
    }
}

/// Whether a narrowed rule for `file` selects the test of `reader` that makes
/// `read`.
fn narrowed_covers(file: &str, reader: &str, read: &Read) -> bool {
    RULES
        .iter()
        .filter(|rule| rule.pattern.matches(file))
        .flat_map(|rule| match rule.effect {
            Effect::Tests(entries) | Effect::AlsoTests(entries) => entries,
            _ => &[],
        })
        .filter(|entry| entry.package == reader)
        .any(|entry| filter_covers(entry.filter, &read.location))
}

/// Where the use at `offset` of `file` sits among the tests of its package.
fn locate(file: &SourceFile, offset: usize) -> Location {
    let inline = file.modules_at(offset);
    let function = file.test_function_at(offset);
    let sites = file
        .placements()
        .iter()
        .map(|placement| {
            let scope = std::iter::once(placement.module.as_str())
                .chain(inline.iter().map(String::as_str))
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("::");
            let test = function.map(|name| {
                if scope.is_empty() {
                    name.to_owned()
                } else {
                    format!("{scope}::{name}")
                }
            });
            Site {
                root: placement.root.clone(),
                scope,
                test,
            }
        })
        .collect();
    Location { sites }
}

/// Whether the nextest `filter` provably selects the use at `location`: at
/// every site, one of its `|` alternatives holds in every conjunct.
///
/// The conjuncts understood are `binary(=name)`, `kind(lib|bin|test)`,
/// `test(=name)` and `test(/^prefix/)` with an optional `(a|b)` group in the
/// prefix. A test-name conjunct holds only for a use inside a test function;
/// a use in a helper is selected only by an alternative that selects its
/// whole crate or binary, since the guard does not know the helper's callers.
/// Any other form proves nothing, and a file no target root reaches is never
/// covered.
fn filter_covers(filter: &str, location: &Location) -> bool {
    !location.sites.is_empty()
        && location.sites.iter().all(|site| {
            split_top(strip_parens(filter), " | ")
                .into_iter()
                .any(|alternative| {
                    split_top(strip_parens(alternative), " and ")
                        .into_iter()
                        .all(|atom| atom_covers(strip_parens(atom), site))
                })
        })
}

fn atom_covers(atom: &str, site: &Site) -> bool {
    match atom {
        "kind(lib)" => return site.root == CrateRoot::Lib,
        "kind(bin)" => return matches!(site.root, CrateRoot::Bin(_)),
        "kind(test)" => return matches!(site.root, CrateRoot::Test(_)),
        _ => {}
    }
    if let Some(name) = atom
        .strip_prefix("binary(=")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return matches!(&site.root, CrateRoot::Test(binary) | CrateRoot::Bin(binary) if binary == name);
    }
    let Some(test) = site.test.as_deref() else {
        return false;
    };
    if let Some(name) = atom
        .strip_prefix("test(=")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return test == name;
    }
    atom.strip_prefix("test(/^")
        .and_then(|rest| rest.strip_suffix("/)"))
        .and_then(regex_prefixes)
        .is_some_and(|prefixes| {
            prefixes
                .iter()
                .any(|prefix| test.starts_with(prefix.as_str()))
        })
}

/// The literal prefixes a `^`-anchored regex body stands for: plain text with
/// at most one `(a|b|..)` group; `None` for any other regex syntax.
fn regex_prefixes(body: &str) -> Option<Vec<String>> {
    let plain = |text: &str| {
        !text.contains([
            '.', '*', '+', '?', '[', ']', '(', ')', '|', '\\', '$', '^', '{', '}',
        ])
    };
    let Some((head, rest)) = body.split_once('(') else {
        return plain(body).then(|| vec![body.to_owned()]);
    };
    let (group, tail) = rest.split_once(')')?;
    if !plain(head) || !plain(tail) {
        return None;
    }
    group
        .split('|')
        .map(|choice| plain(choice).then(|| format!("{head}{choice}{tail}")))
        .collect()
}

/// `text` without parentheses that enclose all of it.
fn strip_parens(text: &str) -> &str {
    let mut text = text.trim();
    while text.starts_with('(') && text.ends_with(')') && closes_at_end(text) {
        text = text[1..text.len() - 1].trim();
    }
    text
}

/// Whether the `(` that opens `text` is closed by its last byte.
fn closes_at_end(text: &str) -> bool {
    let mut depth = 0_usize;
    for (index, byte) in text.bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return index == text.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// `text` split at each `separator` outside parentheses and regex slashes.
fn split_top<'a>(text: &'a str, separator: &str) -> Vec<&'a str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let (mut depth, mut in_regex, mut start, mut index) = (0_usize, false, 0, 0);
    while index < bytes.len() {
        match bytes[index] {
            b'/' => in_regex = !in_regex,
            b'(' if !in_regex => depth += 1,
            b')' if !in_regex => depth = depth.saturating_sub(1),
            _ if depth == 0 && !in_regex && text[index..].starts_with(separator) => {
                parts.push(&text[start..index]);
                index += separator.len();
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(&text[start..]);
    parts
}
