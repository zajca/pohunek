//! `cargo xtask affected`: the tests a change can break.
//!
//! One classification serves two callers. The local fast loop (`cargo ta`)
//! runs the selected fast tests; `cargo xtask affected ci` ([`ci`]) writes the
//! selection a pull request's CI test jobs run ([`guard`] checks the rules
//! against the test sources).
//!
//! Every changed file is classified independently:
//!
//! - a workspace-wide build input (Cargo or nextest configuration, toolchain or
//!   lint configuration) selects every test;
//! - the lockfile and the manifests go through the dependency diff of
//!   [`graph`], which selects the members resolving a changed crate, or every
//!   test for a root manifest change it cannot judge;
//! - a file inside a package directory selects the package with the longest
//!   manifest-directory prefix;
//! - a file that a package embeds or reads in its tests from outside its own
//!   directory selects that package, or only the named tests of it ([`RULES`]);
//! - a file on the reviewed allowlist of non-Rust paths selects nothing;
//! - any other file cannot be proven harmless and selects every test.
//!
//! In CI mode the CI configuration and the scripts that run the test jobs also
//! select every test ([`CI_RULES`]).
//!
//! The selected packages are closed over their reverse dependencies
//! ([`graph::close`]) and become an explicit nextest filterset such as
//! `package(=a) | (package(=b) and (binary(=t)))`, so each package carries the
//! reason it runs. A test that runs another package's binary by path with no
//! Cargo edge between the two ([`BINARY_USES`]) is selected whenever that
//! package's library is.
//!
//! **Selection narrows what runs, never what is built.** CI keeps building the
//! whole workspace (`cargo nextest archive --workspace --all-features`) and
//! applies the filterset when it runs the archive. Building only the selected
//! packages would build them with other dependency features: Cargo unifies
//! features over the packages selected for a build, and the pinned Cargo 1.99.0
//! ignores `resolver.feature-unification` without `-Zfeature-unification`. On
//! 2026-10-16 `cargo tree --all-features` resolved 13 dependency units (clap,
//! futures-util, nix, ...) with other feature sets for the three relay crates
//! than for `--workspace`, and 23 units for `pohunek-daemon`, `pohunek-cli`,
//! `pohunek-session-worker`, `pohunek-paths` and `xtask` (digest, ed25519,
//! hyper-util, ...). Such a build would test binaries that neither the full run
//! nor the release build produces, and it would miss the compile cache of the
//! full build.
//!
//! The local loop is an inner-loop accelerator. It never replaces the full
//! gate set.

// Rust guideline compliant 2026-10-16

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::XtaskError;

pub(crate) mod ci;
pub(crate) mod graph;
pub(crate) mod guard;
pub(crate) mod sparse;

use graph::{Kind, Reason};

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
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct Options {
    #[command(subcommand)]
    command: Option<Action>,
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

/// Subcommands of `cargo xtask affected`.
#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Write the test selection of a pull request for its CI jobs.
    Ci(ci::Options),
    /// Check that every repository path a test reads selects that test.
    Guard(guard::Options),
    /// Print the sparse-checkout patterns of the tree that runs some packages' tests.
    Sparse(sparse::Options),
}

/// Workspace package, its manifest directory and the members it depends on.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Package {
    pub(crate) name: String,
    /// `/`-separated, without a trailing slash; empty for a root package.
    pub(crate) dir: String,
    /// Members this package depends on, by name, with the kind of each edge.
    pub(crate) deps: BTreeSet<(String, Kind)>,
    /// Names of the package's binary targets.
    pub(crate) bins: BTreeSet<String>,
}

/// Path shape matched by a classification rule.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pattern {
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
    /// Whether the repository-relative, `/`-separated `path` has this shape.
    pub(crate) fn matches(self, path: &str) -> bool {
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
    /// The path is read only by the named tests of these packages.
    Tests(&'static [Readers]),
    /// Like [`Effect::Tests`], but only for a path that something else maps:
    /// the rule adds readers and never makes an unmapped path mapped.
    AlsoTests(&'static [Readers]),
    /// A manifest or lockfile; the dependency diff decides ([`graph`]).
    Dependencies,
    /// The path provably feeds no Rust test.
    NoRustTests,
}

/// Tests of one package that read a path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Readers {
    pub(crate) package: &'static str,
    /// Nextest filter selecting those tests within the package; [`guard`]
    /// accepts the rule for a read only when this filter provably selects the
    /// test that makes it.
    pub(crate) filter: &'static str,
}

const fn readers(package: &'static str, filter: &'static str) -> Readers {
    Readers { package, filter }
}

/// Tests that run another package's binary by path, with no Cargo edge
/// between the two packages that would select them.
///
/// A change that selects the owning package's library rebuilds the binary,
/// so it selects these tests. [`guard`] finds every binary lookup in test
/// code and requires a Cargo edge or an entry here.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BinaryUse {
    /// Name of the binary target, as the test looks it up.
    pub(crate) binary: &'static str,
    pub(crate) readers: Readers,
    /// Test sources that run the binary; an entry whose sources exist but no
    /// longer run it is stale.
    pub(crate) sources: &'static [&'static str],
    pub(crate) why: &'static str,
}

pub(crate) const BINARY_USES: &[BinaryUse] = &[BinaryUse {
    binary: "pohunek",
    readers: readers("pohunek-daemon", "binary(=supervised_lifecycle)"),
    sources: &["crates/daemon/tests/supervised_lifecycle.rs"],
    why: "the managed-CLI scenario runs the `pohunek` binary beside `pohunekd`",
}];

/// The workspace package that builds binary `name`.
pub(crate) fn binary_owner<'a>(name: &str, packages: &'a [Package]) -> Option<&'a Package> {
    packages.iter().find(|package| package.bins.contains(name))
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
                               repository root (.agents/instructions/verification.md \
                               \"SDK workspace gates\") and `cargo xtask ts check`"
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
    rule(Pattern::File("Cargo.toml"), Effect::Dependencies, "workspace manifest"),
    rule(Pattern::File("Cargo.lock"), Effect::Dependencies, "dependency lockfile"),
    rule(
        Pattern::File("Cargo.toml"),
        Effect::Tests(&[
            readers(
                "xtask",
                "binary(=dependency_policy)"
            ),
            readers(
                "xtask",
                "kind(lib) and test(/^affected::tests::/)"
            ),
            readers(
                "pohunek-test-support",
                "test(=tests::workspace_root_holds_the_workspace_manifest)"
            ),
        ]),
        "workspace manifest read by xtask's dependency_policy test and the members test of its affected module, and by the workspace root check of pohunek-test-support",
    ),
    rule(
        Pattern::File("Cargo.lock"),
        Effect::Tests(&[readers(
            "xtask",
            "binary(=dependency_policy)"
        )]),
        "lockfile read by xtask's dependency_policy test",
    ),
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
        Pattern::File("runtime-packages/hermes/operator.toml"),
        Effect::Packages(&["pohunek-cli"]),
        "Hermes operator manifest rendered as the package fixture bundle by pohunek-cli's Hermes integration tests",
    ),
    rule(
        Pattern::Dir("runtime-packages/hermes/operator"),
        Effect::Packages(&["pohunek-cli"]),
        "Hermes operator payload read at run time by pohunek-cli's Hermes integration tests and plugin runtime suite",
    ),
    rule(
        Pattern::Dir("compat/hermes"),
        Effect::Packages(&["pohunek-daemon", "pohunek-cli", "xtask"]),
        "Hermes compatibility lock whose pinned release the fake agents of the daemon's integration tests report to the package version probe, read by pohunek-cli's hermes_package test; lock and goldens read by xtask",
    ),
    reminding(
        Pattern::File("sdk/ts/sdk/test/fixtures/lifecycle-budgets.json"),
        Effect::Packages(&["pohunek-client"]),
        &[Reminder::SdkGates],
        "lifecycle response budgets that pohunek-client's lifecycle_budgets test compares with its own and the SDK's lifecycle-budgets test compares with its constants",
    ),
    reminding(
        Pattern::Dir("crates/protocol/tests/fixtures/compat"),
        Effect::Packages(&["pohunek-protocol", "pohunek-client", "pohunek-cli"]),
        &[Reminder::SdkGates],
        "previous-release recordings read by pohunek-protocol's adapter tests, replayed by pohunek-client's previous_daemon test and by the SDK's compat tests, and embedded by pohunek-cli's release_upgrade test",
    ),
    rule(
        Pattern::Dir("compat/codex"),
        Effect::Packages(&["pohunek-daemon", "pohunek-cli", "xtask"]),
        "Codex subagent hook contract embedded by the daemon; compatibility lock read by the daemon's integration tests, whose fake agents report its pinned release to the package version probe; compatibility lock and captured screens read by pohunek-cli's codex_package test; lock and npm project read by xtask's upstream staging scenarios",
    ),
    rule(
        Pattern::Dir("compat/claude"),
        Effect::Packages(&["pohunek-daemon", "pohunek-cli", "xtask"]),
        "Claude Code subagent hook sequences embedded by the daemon's worker hook scenarios; compatibility lock and captured screens read by pohunek-cli's claude_package test; lock and npm project read by xtask's upstream staging scenarios",
    ),
    rule(
        Pattern::File("compat/claude/compatibility-lock.json"),
        Effect::Packages(&["pohunek-daemon"]),
        "Claude Code compatibility lock whose pinned release the fake agents of the daemon's integration tests report to the package version probe",
    ),
    rule(
        Pattern::Dir("compat/codex/builtin-v0.34"),
        Effect::Packages(&["pohunek-daemon"]),
        "v0.34.2 built-in Codex goldens embedded by the daemon's in-crate compiled-in runtime fixture",
    ),
    rule(
        Pattern::Dir("compat/claude/builtin-v0.34"),
        Effect::Packages(&["pohunek-daemon"]),
        "v0.34.2 built-in Claude Code goldens embedded by the daemon's in-crate compiled-in runtime fixture",
    ),
    rule(
        Pattern::Dir("compat/hermes/builtin-v0.34"),
        Effect::Packages(&["pohunek-daemon", "pohunek-cli"]),
        "v0.34.2 built-in Hermes goldens embedded by the daemon's in-crate compiled-in runtime fixture and compared with the package by pohunek-cli's hermes_package test",
    ),
    rule(
        Pattern::Dir("compat/pi"),
        Effect::Packages(&["pohunek-cli", "pohunek-daemon", "xtask"]),
        "Pi compatibility lock and captured screens read by pohunek-cli's pi_package and pi_frame_properties tests and replayed by the daemon's program_status_detector scenarios; lock and npm project read by xtask's upstream staging scenarios",
    ),
    rule(
        Pattern::Dir("crates"),
        Effect::AlsoTests(&[readers(
            "xtask",
            "binary(=hermetic_scan) | binary(=no_special_dispatch) | \
             binary(=test_artifact_lookup_guard) | binary(=timing_ratchet) | \
             binary(=exact_filter_guard) | binary(=dependency_policy) | \
             (binary(=affected_ci) and test(=every_repository_read_of_the_test_code_selects_its_reader)) | \
             (kind(lib) and test(/^affected::tests::/))",
        )]),
        "every crate's sources and manifests, scanned by xtask's repository guards (hermetic_scan, no_special_dispatch, test_artifact_lookup_guard, timing_ratchet, exact_filter_guard, the affected guard and include scan) and resolved by its dependency_policy test",
    ),
    rule(
        Pattern::Dir("runtime-packages"),
        Effect::Tests(&[readers(
            "xtask",
            "kind(lib) and test(/^(attestation|release_tests|test_runtime|upstream_stage_tests)::/)"
        )]),
        "official package sources listed by xtask's matrix check and upstream staging scenarios and built by its release and test-runtime scenarios",
    ),
    rule(
        Pattern::File("scripts/provision-hermes-compat"),
        Effect::Tests(&[readers(
            "xtask",
            "kind(lib) and test(/^upstream_stage_tests::/)"
        )]),
        "Hermes provisioner whose bytes xtask's upstream staging scenarios read and verify",
    ),
    rule(
        Pattern::File(".github/workflows/ci.yml"),
        Effect::Tests(&[readers(
            "xtask",
            "binary(=affected_ci)"
        )]),
        "CI workflow whose test-job scripts the affected scenarios require to select every test",
    ),
    rule(
        Pattern::File("crates/xtask/src/affected/mod.rs"),
        Effect::Tests(&[readers(
            "pohunek-daemon",
            "test(=git::tests::the_allow_list_contains_no_variable_xtask_treats_as_redirecting)"
        )]),
        "xtask's list of repository-redirecting Git variables, which the daemon's git environment test compares with its own",
    ),
    rule(
        Pattern::File("docs/public-api.md"),
        Effect::Tests(&[readers(
            "pohunek-protocol",
            "binary(=task_errors)"
        )]),
        "public API reference whose shared error codes pohunek-protocol's task_errors test checks",
    ),
    rule(
        Pattern::Dir("crates/daemon/src"),
        Effect::Tests(&[readers(
            "pohunek-protocol",
            "binary(=task_errors)"
        )]),
        "daemon sources whose raised task error codes pohunek-protocol's task_errors test reads",
    ),
    rule(
        Pattern::File("crates/cli/src/hermes_integration/assets/pohunek/_pohunek_reporter.py"),
        Effect::Packages(&["pohunek-daemon"]),
        "Hermes reporter plugin that the daemon's worker hook and session scenarios run",
    ),
    rule(
        Pattern::File("compat/pi/compatibility-lock.json"),
        Effect::Packages(&["pohunek-daemon"]),
        "Pi compatibility lock whose pinned release the daemon's official runtime fixtures report to the package version probe",
    ),
    rule(
        Pattern::Dir("crates/netbird/tests/fixtures"),
        Effect::Packages(&["pohunek-netbird", "pohunek-cli"]),
        "NetBird status recordings embedded by NetBird tests and CLI discovery scenarios",
    ),
    rule(
        Pattern::Dir("runtime-packages"),
        Effect::Packages(&["pohunek-cli", "pohunek-daemon"]),
        "official runtime package sources built and parsed by pohunek-cli's pi_package, pi_frame_properties, codex_package, claude_package and hermes_package tests, and read by its Hermes plugin suites and the package fixture bundle of its hermes_integration module tests; each package directory is built into the archives the daemon's official runtime fixtures install",
    ),
    rule(
        Pattern::Dir("runtime-packages/hermes"),
        Effect::Packages(&["pohunek-daemon", "xtask"]),
        "Hermes package descriptor and detection manifest embedded by the daemon's in-crate official package fixture and built into the package its integration tests install; built by xtask's official lock shape scenario; its operator skill and sidecar are what `hermes generate-skill` and `hermes check-skill` write and check",
    ),
    rule(
        Pattern::Dir("runtime-packages/codex"),
        Effect::Packages(&["pohunek-daemon"]),
        "Codex package descriptor and detection manifest embedded by the daemon's in-crate official package fixture and built into the package its integration tests install",
    ),
    rule(
        Pattern::Dir("runtime-packages/claude"),
        Effect::Packages(&["pohunek-daemon"]),
        "Claude Code package descriptor and detection manifest embedded by the daemon's in-crate official package fixture and built into the package its integration tests install",
    ),
    rule(
        Pattern::Dir("runtime-packages/pi"),
        Effect::Packages(&["pohunek-daemon"]),
        "Pi package descriptor and detection manifest embedded by the daemon's in-crate official package fixture and built into the packages its newer-version adoption scenarios install",
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
        Pattern::File("compat/matrix.json"),
        Effect::Packages(&["xtask", "pohunek-cli"]),
        "release compatibility matrix read by xtask's attestation scenarios and embedded in pohunek-cli's consumer suite",
    ),
    rule(
        Pattern::File(".github/workflows/release.yml"),
        Effect::Packages(&["pohunek-cli"]),
        "release workflow asserted by pohunek-cli's daemon_packaging test",
    ),
    rule(
        Pattern::File(".github/workflows/release-build.yml"),
        Effect::Packages(&["pohunek-cli"]),
        "release build workflow asserted by pohunek-cli's daemon_packaging test",
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

/// Rules that apply in CI mode before [`RULES`], each selecting every test.
///
/// A pull request's CI run must prove a change to the jobs that select and run
/// its tests on the full set; locally these paths keep their [`RULES`]
/// effects. `ci_scripts_of_the_test_jobs_escalate` in [`guard`]'s scenarios
/// requires every script the test jobs of `.github/workflows/ci.yml` run to be
/// listed here.
pub(crate) const CI_RULES: &[(Pattern, &str)] = &[
    (Pattern::Dir(".github"), "CI configuration"),
    (
        Pattern::File("scripts/test-partitions"),
        "shard partitions and the runner of the CI test jobs",
    ),
    (
        Pattern::File("scripts/junit-flaky-summary"),
        "flaky-test report of the CI test jobs",
    ),
    (
        Pattern::File("scripts/check-rust-toolchain-pin"),
        "toolchain pin check of the CI test jobs",
    ),
    (
        Pattern::File("scripts/ci-test-selection"),
        "builds and runs the merge base's selector for the CI test jobs",
    ),
];

/// Platform-neutral data that the macOS lane selection of a pull request
/// leaves out (#1225).
///
/// A change to `pattern` cannot behave differently on macOS: the data reaches
/// Rust tests as `owner`'s embedded data or through its [`RULES`] readers,
/// and every dependent of `owner` is a reviewed `reader`. The Linux selection
/// still closes over all of them. A perturbed copy of the data (#1225) failed
/// only `owner`'s own tests. `lane_neutral_readers_are_every_dependent`
/// requires `readers` to name every workspace member that depends on `owner`.
pub(crate) struct LaneNeutral {
    pub(crate) pattern: Pattern,
    pub(crate) owner: &'static str,
    pub(crate) readers: &'static [(&'static str, &'static str)],
    pub(crate) why: &'static str,
}

/// The paths of [`LaneNeutral`].
pub(crate) const LANE_NEUTRAL: &[LaneNeutral] = &[LaneNeutral {
    pattern: Pattern::Dir("docs/knowledge"),
    owner: "pohunek-knowledge",
    readers: &[
        (
            "pohunek-assistant",
            "materializes the embedded bundle and hashes it for the launch id",
        ),
        (
            "pohunek-daemon",
            "materializes the embedded bundle for `assistant.materialize`",
        ),
        (
            "pohunek-cli",
            "reaches the bundle through pohunek-assistant",
        ),
        (
            "xtask",
            "depends on the bundle through the daemon and the CLI",
        ),
    ],
    why: "knowledge bundle data, embedded by pohunek-knowledge's build script",
}];

/// Which caller classifies a change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    /// The local fast loop.
    Local,
    /// A pull request's CI test jobs ([`CI_RULES`] apply).
    Ci,
}

/// Why one changed file selects what it selects.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Decision {
    Everything(&'static str),
    Packages {
        /// Packages whose tests are selected whole.
        names: BTreeSet<String>,
        /// Tests selected inside other packages, as `(package, filter, why)`.
        tests: BTreeSet<(String, &'static str, &'static str)>,
        /// Why `names` are selected.
        why: String,
    },
    Dependencies(&'static str),
    NoRustTests(&'static str),
    Unmapped,
}

impl Decision {
    fn describe(&self) -> String {
        match self {
            Self::Everything(why) => format!("everything ({why})"),
            Self::Packages { names, tests, why } => {
                let mut parts = Vec::new();
                if !names.is_empty() {
                    let names: Vec<&str> = names.iter().map(String::as_str).collect();
                    parts.push(format!("{} ({why})", names.join(", ")));
                }
                parts.extend(
                    tests
                        .iter()
                        .map(|(package, filter, why)| format!("{package} [{filter}] ({why})")),
                );
                parts.join("; ")
            }
            Self::Dependencies(why) => format!("dependency diff ({why})"),
            Self::NoRustTests(why) => format!("no Rust tests ({why})"),
            Self::Unmapped => {
                "everything (no package or reviewed rule covers this path)".to_owned()
            }
        }
    }

    /// Whether the decision selects every test on its own.
    fn is_everything(&self) -> bool {
        matches!(self, Self::Everything(_) | Self::Unmapped)
    }
}

/// Packages selected by the files of a change, before the closure.
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
    /// Packages the files select whole.
    selection: Selection,
    /// Whether a manifest or the lockfile changed, so the dependency diff runs.
    dependencies: bool,
    reminders: BTreeSet<Reminder>,
}

/// Tests that a change selects after the closure and the dependency diff.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Targets {
    /// Packages whose tests all run, with the reasons.
    pub(crate) whole: BTreeMap<String, BTreeSet<Reason>>,
    /// Packages of which only the named tests run: filter, then the paths and
    /// rules that select it.
    pub(crate) narrowed: BTreeMap<String, BTreeMap<&'static str, BTreeSet<String>>>,
}

impl Targets {
    fn is_empty(&self) -> bool {
        self.whole.is_empty() && self.narrowed.is_empty()
    }
}

/// What a change runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    /// No Rust test can be affected.
    Nothing,
    /// The selected tests.
    Tests(Targets),
    /// Every test, with the causes.
    Everything(Vec<String>),
}

/// Runs a subcommand, or the fast tests affected by the current change.
pub(crate) fn run(root: &Path, options: &Options) -> Result<(), XtaskError> {
    match &options.command {
        Some(Action::Ci(ci)) => return ci::run(ci),
        Some(Action::Guard(guard)) => return guard::run(guard),
        Some(Action::Sparse(sparse)) => return sparse::run(sparse),
        None => {}
    }
    let (workspace_root, packages) = workspace(root)?;
    ensure_git_toplevel(&workspace_root)?;

    let base = resolve_base(&workspace_root, options.base.as_deref())?;
    let changed = changed_files(&workspace_root, &base)?;
    let plan = plan(&changed, &packages, Mode::Local)?;
    let outcome = resolve(&plan, &packages, || {
        let merge_base = merge_base(&workspace_root, &base)?;
        graph::manifest_diff(&workspace_root, &merge_base)
    });
    let args = nextest_args(&outcome, &options.nextest_args);

    if options.print {
        print!("{}", render_report(&base, &plan, &outcome, args.as_deref()));
        return Ok(());
    }

    eprint!(
        "{}",
        render_summary(&base, &plan, &outcome, args.as_deref())
    );
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

/// Reads the workspace root and its members from `cargo metadata --no-deps`.
pub(crate) fn workspace(root: &Path) -> Result<(PathBuf, Vec<Package>), XtaskError> {
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
    graph::parse_members(&metadata)
}

/// The merge base of `base` and `HEAD`, as a full commit id.
pub(crate) fn merge_base(root: &Path, base: &str) -> Result<String, XtaskError> {
    let output = command_output(root, GIT, &["merge-base", "--end-of-options", base, "HEAD"])?;
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

/// Classifies every changed file and combines the decisions.
///
/// Fails when a rule names a package that is not in the workspace, so a renamed
/// crate cannot silently drop out of the selection.
fn plan(changed: &BTreeSet<String>, packages: &[Package], mode: Mode) -> Result<Plan, XtaskError> {
    for rule in RULES {
        let names: Vec<&str> = match rule.effect {
            Effect::Packages(names) => names.to_vec(),
            Effect::Tests(tests) | Effect::AlsoTests(tests) => {
                tests.iter().map(|entry| entry.package).collect()
            }
            _ => Vec::new(),
        };
        for name in names {
            if !packages.iter().any(|package| package.name == name) {
                return Err(XtaskError::Usage(format!(
                    "affected rule for {:?} names unknown workspace package `{name}`; \
                     update RULES in crates/xtask/src/affected/mod.rs",
                    rule.pattern
                )));
            }
        }
    }

    for edge in BINARY_USES {
        if binary_owner(edge.binary, packages).is_none()
            || !packages
                .iter()
                .any(|package| package.name == edge.readers.package)
        {
            return Err(XtaskError::Usage(format!(
                "affected binary use of `{}` by `{}` names no workspace binary or package; \
                 update BINARY_USES in crates/xtask/src/affected/mod.rs",
                edge.binary, edge.readers.package
            )));
        }
    }

    let mut decisions = Vec::with_capacity(changed.len());
    let mut reminders = BTreeSet::new();
    let mut everything = false;
    let mut dependencies = false;
    let mut selected = BTreeSet::new();
    for path in changed {
        let (decision, reminder) = classify(path, packages, mode);
        reminders.extend(reminder);
        if is_dependency_input(path, packages) {
            dependencies = true;
        }
        match &decision {
            Decision::Everything(_) | Decision::Unmapped => everything = true,
            Decision::Packages { names, .. } => selected.extend(names.iter().cloned()),
            Decision::Dependencies(_) | Decision::NoRustTests(_) => {}
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
        dependencies,
        reminders,
    })
}

/// Whether a change to `path` needs the dependency diff: a member manifest,
/// or a path whose rule is [`Effect::Dependencies`].
fn is_dependency_input(path: &str, packages: &[Package]) -> bool {
    let ruled = RULES
        .iter()
        .any(|rule| matches!(rule.effect, Effect::Dependencies) && rule.pattern.matches(path));
    ruled
        || packages.iter().any(|package| {
            if package.dir.is_empty() {
                path == "Cargo.toml"
            } else {
                path.strip_prefix(package.dir.as_str()) == Some("/Cargo.toml")
            }
        })
}

/// Classifies one repository-relative, `/`-separated path.
fn classify(path: &str, packages: &[Package], mode: Mode) -> (Decision, BTreeSet<Reminder>) {
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

    if mode == Mode::Ci {
        if let Some((_, why)) = CI_RULES.iter().find(|(pattern, _)| pattern.matches(path)) {
            return (Decision::Everything(why), reminder);
        }
    }
    if let Some(rule) = matching
        .iter()
        .find(|rule| matches!(rule.effect, Effect::Everything))
    {
        return (Decision::Everything(rule.why), reminder);
    }
    let mut names = BTreeSet::new();
    let mut tests = BTreeSet::new();
    let mut also = BTreeSet::new();
    let mut reasons = Vec::new();
    if let Some(owner) = owning_package(path, packages) {
        names.insert(owner.name.clone());
        reasons.push("package directory");
    }
    for rule in &matching {
        match rule.effect {
            Effect::Packages(embedders) => {
                names.extend(embedders.iter().map(|name| (*name).to_owned()));
                reasons.push(rule.why);
            }
            Effect::Tests(readers) => {
                tests.extend(
                    readers
                        .iter()
                        .map(|entry| (entry.package.to_owned(), entry.filter, rule.why)),
                );
            }
            Effect::AlsoTests(readers) => {
                also.extend(
                    readers
                        .iter()
                        .map(|entry| (entry.package.to_owned(), entry.filter, rule.why)),
                );
            }
            _ => {}
        }
    }
    let mapped = !names.is_empty()
        || !tests.is_empty()
        || matching
            .iter()
            .any(|rule| matches!(rule.effect, Effect::NoRustTests | Effect::Dependencies));
    if mapped {
        tests.extend(also);
    }
    tests.retain(|(package, ..)| !names.contains(package));
    if !names.is_empty() || !tests.is_empty() {
        let why = reasons.join("; ");
        return (Decision::Packages { names, tests, why }, reminder);
    }
    if let Some(rule) = matching
        .iter()
        .find(|rule| matches!(rule.effect, Effect::Dependencies))
    {
        return (Decision::Dependencies(rule.why), reminder);
    }

    match matching
        .iter()
        .find(|rule| matches!(rule.effect, Effect::NoRustTests))
    {
        Some(rule) => (Decision::NoRustTests(rule.why), reminder),
        None => (Decision::Unmapped, reminder),
    }
}

/// Turns `plan` into what runs: the closure of its packages, the narrowed
/// tests, and the members the dependency diff selects.
///
/// `diff` runs only when the plan needs it; its failure selects every test.
fn resolve(
    plan: &Plan,
    packages: &[Package],
    diff: impl FnOnce() -> Result<graph::ManifestDiff, XtaskError>,
) -> Outcome {
    let causes: Vec<String> = plan
        .decisions
        .iter()
        .filter(|(_, decision)| decision.is_everything())
        .map(|(path, decision)| format!("{}: {}", printable(path), decision.describe()))
        .collect();
    if plan.selection == Selection::Everything {
        return Outcome::Everything(causes);
    }

    let mut seeds: BTreeMap<String, BTreeSet<Reason>> = BTreeMap::new();
    let mut narrowed: BTreeMap<String, BTreeMap<&'static str, BTreeSet<String>>> = BTreeMap::new();
    for (path, decision) in &plan.decisions {
        if let Decision::Packages { names, tests, why } = decision {
            for name in names {
                seeds
                    .entry(name.clone())
                    .or_default()
                    .insert(Reason::Changed(format!("{} ({why})", printable(path))));
            }
            for (package, filter, test_why) in tests {
                narrowed
                    .entry(package.clone())
                    .or_default()
                    .entry(*filter)
                    .or_default()
                    .insert(format!("{} ({test_why})", printable(path)));
            }
        }
    }
    let mut whole = graph::close(&seeds, packages);
    if plan.dependencies {
        match diff() {
            Ok(graph::ManifestDiff::Members {
                resolved,
                library_seeds,
            }) => {
                let inherited = graph::close(&library_seeds, packages);
                for (name, reasons) in resolved.into_iter().chain(inherited) {
                    whole.entry(name).or_default().extend(reasons);
                }
            }
            Ok(graph::ManifestDiff::Everything(why)) => {
                return Outcome::Everything(vec![format!("dependency diff: {why}")]);
            }
            Err(error) => {
                return Outcome::Everything(vec![format!(
                    "dependency diff failed: {}",
                    printable(&error.to_string())
                )]);
            }
        }
    }
    // A binary is rebuilt when its package's library is selected; a selection
    // through a dev-dependency changes only the package's tests.
    for edge in BINARY_USES {
        let rebuilt = binary_owner(edge.binary, packages)
            .and_then(|owner| whole.get(&owner.name).map(|reasons| (owner, reasons)))
            .filter(|(_, reasons)| {
                reasons
                    .iter()
                    .any(|reason| !matches!(reason, Reason::DevDependsOn(_)))
            });
        if let Some((owner, _)) = rebuilt {
            narrowed
                .entry(edge.readers.package.to_owned())
                .or_default()
                .entry(edge.readers.filter)
                .or_default()
                .insert(format!(
                    "the `{}` binary of {} ({})",
                    edge.binary, owner.name, edge.why
                ));
        }
    }
    narrowed.retain(|package, _| !whole.contains_key(package));
    let targets = Targets { whole, narrowed };
    if targets.is_empty() {
        Outcome::Nothing
    } else {
        Outcome::Tests(targets)
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

/// The tracked files a sparse working tree keeps for `targets`: every file
/// whose change would select one of their packages, every file whose change
/// selects everything, every dependency input, and nothing else that is
/// [`Decision::NoRustTests`]. A test that reads a file outside this set reads
/// a path whose change does not select it.
pub(crate) fn sparse_paths(
    tracked: &BTreeSet<String>,
    packages: &[Package],
    targets: &Targets,
) -> BTreeSet<String> {
    let selected: BTreeSet<&str> = targets
        .whole
        .keys()
        .chain(targets.narrowed.keys())
        .map(String::as_str)
        .collect();
    let mut closures: BTreeMap<BTreeSet<String>, bool> = BTreeMap::new();
    tracked
        .iter()
        // A dependency input can select any package through the dependency
        // diff, and nextest needs the workspace manifests.
        .filter(|path| {
            is_dependency_input(path, packages)
                || match classify(path, packages, Mode::Ci).0 {
                    Decision::NoRustTests(_) => false,
                    Decision::Everything(_) | Decision::Unmapped | Decision::Dependencies(_) => {
                        true
                    }
                    Decision::Packages { names, tests, .. } => {
                        tests
                            .iter()
                            .any(|(package, ..)| selected.contains(package.as_str()))
                            || *closures.entry(names).or_insert_with_key(|names| {
                                let seeds = names
                                    .iter()
                                    .map(|name| (name.clone(), BTreeSet::new()))
                                    .collect();
                                graph::close(&seeds, packages)
                                    .keys()
                                    .any(|name| selected.contains(name.as_str()))
                            })
                    }
                }
        })
        .cloned()
        .collect()
}

/// Builds the nextest filterset of `targets`.
///
/// `=` is nextest's exact name matcher. `BTreeMap` iteration keeps the output
/// deterministic.
pub(crate) fn filterset(targets: &Targets) -> String {
    let whole = targets.whole.keys().map(|name| format!("package(={name})"));
    let narrowed = targets.narrowed.iter().map(|(name, filters)| {
        let filters: Vec<String> = filters.keys().map(|filter| format!("({filter})")).collect();
        format!("(package(={name}) and ({}))", filters.join(" | "))
    });
    whole.chain(narrowed).collect::<Vec<_>>().join(" | ")
}

/// Arguments for `cargo`, or `None` when no Rust test is affected.
fn nextest_args(outcome: &Outcome, extra: &[OsString]) -> Option<Vec<OsString>> {
    let mut args = vec![OsString::from(FAST_LOOP_ALIAS)];
    match outcome {
        Outcome::Nothing => return None,
        Outcome::Everything(_) => {}
        Outcome::Tests(targets) => {
            args.push(OsString::from("-E"));
            args.push(OsString::from(filterset(targets)));
        }
    }
    args.extend(extra.iter().cloned());
    Some(args)
}

fn outcome_line(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Nothing => "selection: no Rust tests are affected".to_owned(),
        Outcome::Everything(_) => "selection: everything (fail-safe escalation)".to_owned(),
        Outcome::Tests(targets) => {
            let mut parts: Vec<String> = targets.whole.keys().cloned().collect();
            parts.extend(
                targets
                    .narrowed
                    .keys()
                    .map(|name| format!("{name} (named tests)")),
            );
            format!("selection: {}", parts.join(", "))
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
fn render_report(base: &str, plan: &Plan, outcome: &Outcome, args: Option<&[OsString]>) -> String {
    let mut out = format!("base: {}\n", printable(base));
    if plan.decisions.is_empty() {
        out.push_str("no changed files\n");
    }
    for (path, decision) in &plan.decisions {
        let _ = writeln!(out, "{} -> {}", printable(path), decision.describe());
    }
    if let Outcome::Everything(causes) = outcome {
        for cause in causes
            .iter()
            .filter(|cause| cause.starts_with("dependency diff"))
        {
            let _ = writeln!(out, "{cause}");
        }
    }
    let _ = writeln!(out, "{}", outcome_line(outcome));
    if let Outcome::Tests(targets) = outcome {
        for (name, reasons) in &targets.whole {
            let reasons: Vec<String> = reasons.iter().map(Reason::describe).collect();
            let _ = writeln!(out, "  {name}: {}", reasons.join("; "));
        }
    }
    for reminder in &plan.reminders {
        let _ = writeln!(out, "reminder: {}", reminder.message());
    }
    let _ = writeln!(out, "{}", command_line(args));
    out
}

/// Short summary printed before running: base, escalation causes, selection, reminders.
fn render_summary(base: &str, plan: &Plan, outcome: &Outcome, args: Option<&[OsString]>) -> String {
    let mut out = format!(
        "affected: base {}, {} changed files\n",
        printable(base),
        plan.decisions.len()
    );
    if let Outcome::Everything(causes) = outcome {
        for cause in causes {
            let _ = writeln!(out, "affected: {cause}");
        }
    }
    let _ = writeln!(out, "affected: {}", outcome_line(outcome));
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
pub(crate) fn printable(text: &str) -> String {
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
pub(crate) fn resolve_base(root: &Path, explicit: Option<&str>) -> Result<String, XtaskError> {
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
pub(crate) fn changed_files(root: &Path, base: &str) -> Result<BTreeSet<String>, XtaskError> {
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
pub(crate) fn parse_nul_list(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
}

/// The Git executable the changed-file queries run.
pub(crate) const GIT: &str = "git";

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
pub(crate) const REPOSITORY_REDIRECTING_VARS: &[&str] = &[
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
pub(crate) fn command_in(dir: &Path, program: &str) -> Command {
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
pub(crate) fn command_output(
    dir: &Path,
    program: &str,
    args: &[&str],
) -> Result<Vec<u8>, XtaskError> {
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
            deps: BTreeSet::new(),
            bins: BTreeSet::new(),
        }
    }

    /// Workspace fixture with nested and prefix-sharing package directories.
    fn fixture() -> Vec<Package> {
        vec![
            package("pohunek-relay", "crates/relay"),
            package("pohunek-relay-client", "crates/relay-client"),
            package("pohunek-knowledge", "crates/knowledge"),
            package("pohunek-daemon", "crates/daemon"),
            package("pohunek-protocol", "crates/protocol"),
            package("pohunek-client", "crates/client"),
            Package {
                bins: BTreeSet::from(["pohunek".to_owned()]),
                ..package("pohunek-cli", "crates/cli")
            },
            package("pohunek-netbird", "crates/netbird"),
            package("pohunek-test-support", "crates/test-support"),
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
        plan(&changed(paths), &fixture(), Mode::Local)
            .expect("plan")
            .selection
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
    fn every_workspace_wide_input_escalates_to_everything() {
        for path in [
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
        for path in ["crates/cli/Cargo.toml", "crates/cli/rust-toolchain.md"] {
            assert_eq!(
                selection(&[path]),
                Selection::Packages(names(&["pohunek-cli"])),
                "{path}"
            );
        }
    }

    #[test]
    fn an_unknown_path_escalates_to_everything() {
        for path in [
            "idea.txt",
            ".lh-harness/state",
            "crates/README.md",
            "compat/other/x.json",
        ] {
            let plan = plan(&changed(&[path]), &fixture(), Mode::Local).expect("plan");
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
        assert_eq!(nextest_args(&Outcome::Nothing, &[]), None);
    }

    #[test]
    fn allowlisted_paths_do_not_mask_package_changes() {
        assert_eq!(
            selection(&["README.md", "crates/daemon/src/lib.rs"]),
            Selection::Packages(names(&["pohunek-daemon"]))
        );
    }

    #[test]
    fn embedded_paths_map_to_their_embedding_packages() {
        let cases: [(&str, &[&str]); 23] = [
            (
                "docs/knowledge/guides/agent-skill.md",
                &["pohunek-knowledge", "xtask"],
            ),
            (
                "compat/hermes/compatibility-lock.json",
                &["pohunek-cli", "pohunek-daemon", "xtask"],
            ),
            (
                "compat/hermes/builtin-v0.34/runtime.toml",
                &["pohunek-cli", "pohunek-daemon", "xtask"],
            ),
            (
                "compat/codex/subagent-hooks.json",
                &["pohunek-daemon", "pohunek-cli", "xtask"],
            ),
            (
                "compat/codex/screens/idle.txt",
                &["pohunek-daemon", "pohunek-cli", "xtask"],
            ),
            (
                "compat/pi/screens/widths/idle_w20.txt",
                &["pohunek-cli", "xtask"],
            ),
            ("compat/pi/npm/package-lock.json", &["pohunek-cli", "xtask"]),
            (
                "compat/claude/screens/idle.txt",
                &["pohunek-daemon", "pohunek-cli", "xtask"],
            ),
            (
                "compat/claude/compatibility-lock.json",
                &["pohunek-cli", "pohunek-daemon", "xtask"],
            ),
            (
                "compat/codex/builtin-v0.34/runtime.toml",
                &["pohunek-daemon", "pohunek-cli", "xtask"],
            ),
            (
                "compat/claude/builtin-v0.34/detect.toml",
                &["pohunek-cli", "pohunek-daemon", "xtask"],
            ),
            (
                "runtime-packages/codex/detect.toml",
                &["pohunek-cli", "pohunek-daemon"],
            ),
            (
                "runtime-packages/claude/runtime.toml",
                &["pohunek-cli", "pohunek-daemon"],
            ),
            (
                "runtime-packages/pi/runtime.toml",
                &["pohunek-cli", "pohunek-daemon"],
            ),
            (
                "runtime-packages/hermes/runtime.toml",
                &["pohunek-cli", "pohunek-daemon", "xtask"],
            ),
            (
                "runtime-packages/hermes/operator/cli.py",
                &["pohunek-cli", "pohunek-daemon", "xtask"],
            ),
            (
                "runtime-packages/README.md",
                &["pohunek-cli", "pohunek-daemon"],
            ),
            (
                "scripts/tests/smoke-hermes-plugin-release.sh",
                &["pohunek-cli"],
            ),
            ("packaging/install-daemon.sh", &["pohunek-cli", "xtask"]),
            ("packaging/stage-archive", &["pohunek-cli", "xtask"]),
            (".github/workflows/release.yml", &["pohunek-cli"]),
            (".github/workflows/release-build.yml", &["pohunek-cli"]),
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
            Mode::Local,
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
        let report = render_report("main", &plan, &Outcome::Nothing, None);
        assert!(report.contains("cargo xtask docs check"), "{report}");
        assert!(report.contains(SCRIPT_TESTS_COMMAND), "{report}");
        assert!(report.contains("cargo xtask ts check"), "{report}");
    }

    #[test]
    fn every_docs_check_input_reminds_the_docs_check() {
        for path in ["docs/runbooks/upgrade.md", "README.md", "LICENSE"] {
            let plan = plan(&changed(&[path]), &fixture(), Mode::Local).expect("plan");
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
        let error = plan(&changed(&["crates/cli/src/lib.rs"]), &packages, Mode::Local)
            .expect_err("rules name packages missing from this workspace");
        assert!(
            error.to_string().contains("unknown workspace package"),
            "{error}"
        );
    }

    /// Targets selecting `names` whole, as the closure of a fixture without edges does.
    fn targets(names: &[&str]) -> Targets {
        Targets {
            whole: names
                .iter()
                .map(|name| ((*name).to_owned(), BTreeSet::new()))
                .collect(),
            narrowed: BTreeMap::new(),
        }
    }

    #[test]
    fn filterset_is_sorted_and_uses_exact_package_matchers() {
        let mut selected = targets(&["xtask", "pohunek-cli"]);
        assert_eq!(
            filterset(&selected),
            "package(=pohunek-cli) | package(=xtask)"
        );
        selected.narrowed.insert(
            "pohunek-protocol".to_owned(),
            BTreeMap::from([
                ("binary(=b)", BTreeSet::new()),
                ("binary(=a)", BTreeSet::new()),
            ]),
        );
        assert_eq!(
            filterset(&selected),
            "package(=pohunek-cli) | package(=xtask) | \
             (package(=pohunek-protocol) and ((binary(=a)) | (binary(=b))))"
        );
    }

    #[test]
    fn nextest_args_use_the_fast_alias_and_append_passthrough() {
        let extra = [OsString::from("--no-fail-fast")];
        assert_eq!(
            nextest_args(&Outcome::Tests(targets(&["pohunek-cli"])), &extra),
            Some(vec![
                OsString::from("t"),
                OsString::from("-E"),
                OsString::from("package(=pohunek-cli)"),
                OsString::from("--no-fail-fast"),
            ])
        );
        assert_eq!(
            nextest_args(&Outcome::Everything(Vec::new()), &extra),
            Some(vec![OsString::from("t"), OsString::from("--no-fail-fast")])
        );
    }

    #[test]
    fn printed_command_is_shell_quoted() {
        let args =
            nextest_args(&Outcome::Tests(targets(&["pohunek-cli", "xtask"])), &[]).expect("args");
        assert_eq!(
            command_line(Some(&args)),
            "cargo t -E 'package(=pohunek-cli) | package(=xtask)'"
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn report_lists_a_reason_per_file() {
        let plan = plan(
            &changed(&["crates/cli/src/lib.rs", "sdk/ts/x.ts"]),
            &fixture(),
            Mode::Local,
        )
        .expect("plan");
        let outcome = resolve(&plan, &fixture(), || unreachable!("no manifest changed"));
        let args = nextest_args(&outcome, &[]);
        let report = render_report("origin/main", &plan, &outcome, args.as_deref());
        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines[0], "base: origin/main", "{report}");
        assert!(
            lines[1]
                .starts_with("crates/cli/src/lib.rs -> pohunek-cli (package directory); xtask ["),
            "{report}"
        );
        assert_eq!(
            lines[2],
            "sdk/ts/x.ts -> no Rust tests (Bun workspace; xtask only writes its generated \
             bindings, checked by `cargo xtask ts check`)"
        );
        assert_eq!(
            lines[3],
            "selection: pohunek-cli, pohunek-daemon (named tests), xtask (named tests)"
        );
        assert_eq!(
            lines[4],
            "  pohunek-cli: crates/cli/src/lib.rs (package directory)"
        );
        assert_eq!(
            lines[5],
            "reminder: SDK workspace changed: also run the Bun gates from the repository \
             root (.agents/instructions/verification.md \"SDK workspace gates\") and \
             `cargo xtask ts check`"
        );
        assert!(
            lines[6].starts_with(
                "cargo t -E 'package(=pohunek-cli) | (package(=pohunek-daemon) and \
                 ((binary(=supervised_lifecycle)))) | (package(=xtask) and ((binary(=hermetic_scan)"
            ),
            "{report}"
        );
        assert_eq!(lines.len(), 7, "{report}");
    }

    #[test]
    fn control_characters_in_paths_and_base_are_escaped() {
        // Unmapped, so the pre-run summary prints it as an escalation cause too.
        let hostile = "a\nforged -> nothing\x1b[2J.rs";
        let plan = plan(&changed(&[hostile]), &fixture(), Mode::Local).expect("plan");
        let outcome = resolve(&plan, &fixture(), || unreachable!("no manifest changed"));
        let args = nextest_args(&outcome, &[]);
        for output in [
            render_report("main\x1b]0;x\x07", &plan, &outcome, args.as_deref()),
            render_summary("main\x1b]0;x\x07", &plan, &outcome, args.as_deref()),
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
        let (root, packages) = graph::parse_members(json).expect("metadata parses");
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
        graph::parse_members(outside).expect_err("a package outside the workspace is rejected");
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
                Package {
                    name,
                    dir,
                    deps: BTreeSet::new(),
                    bins: BTreeSet::new(),
                }
            })
            .collect()
    }

    #[test]
    fn rules_name_only_real_workspace_packages() {
        let (_, packages) =
            workspace(&pohunek_test_support::workspace_root()).expect("workspace metadata");
        plan(&BTreeSet::new(), &packages, Mode::Local)
            .expect("every rule and binary use names a workspace package");
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
                    let selected = match classify(&target, &packages, Mode::Local).0 {
                        Decision::Everything(_) => true,
                        Decision::Packages { names, tests, .. } => {
                            names.contains(&package.name)
                                || tests.iter().any(|(name, ..)| *name == package.name)
                        }
                        Decision::Dependencies(_)
                        | Decision::NoRustTests(_)
                        | Decision::Unmapped => false,
                    };
                    if !selected {
                        uncovered.push(format!("{} -> {target}", relative.display()));
                    }
                }
            }
        }
        assert!(
            uncovered.is_empty(),
            "add these embedded paths to RULES in affected/mod.rs: {uncovered:#?}"
        );
    }

    /// The macOS lane leaves a [`LANE_NEUTRAL`] path out only while a review
    /// names every package that can read its data: a new dependent of the
    /// owner fails here until it is listed with the reason it reads the data.
    #[test]
    fn lane_neutral_readers_are_every_dependent() {
        let partitions = std::fs::read_to_string(
            pohunek_test_support::workspace_root().join("scripts/test-partitions"),
        )
        .expect("read the macOS lane partition");
        let lane = partitions
            .split_once("LANE_PACKAGES = (")
            .expect("lane package tuple")
            .1
            .split_once(')')
            .expect("end of lane package tuple")
            .0;
        let lane_packages: BTreeSet<&str> = lane
            .split('"')
            .enumerate()
            .filter_map(|(index, part)| (index % 2 == 1).then_some(part))
            .collect();
        assert_eq!(
            lane_packages,
            guard::LANE_PACKAGES.iter().copied().collect(),
            "the read guard must cover exactly the macOS lane packages"
        );
        let (_, packages) =
            workspace(&pohunek_test_support::workspace_root()).expect("workspace members");
        for data in LANE_NEUTRAL {
            assert!(
                packages.iter().any(|package| package.name == data.owner),
                "{:?}: owner {} is not a workspace member",
                data.pattern,
                data.owner
            );
            let seeds = BTreeMap::from([(data.owner.to_owned(), BTreeSet::new())]);
            let mut dependents: BTreeSet<String> =
                graph::close(&seeds, &packages).into_keys().collect();
            dependents.remove(data.owner);
            let readers: BTreeSet<String> = data
                .readers
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect();
            assert_eq!(
                readers, dependents,
                "{:?}: LANE_NEUTRAL readers must be exactly the dependents of {}",
                data.pattern, data.owner
            );
        }
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
