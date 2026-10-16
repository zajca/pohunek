//! Scenarios for `cargo xtask affected ci` and `cargo xtask affected guard`,
//! driven through the real binary.
//!
//! Each scenario builds its own Git repository holding a Cargo workspace with
//! the package names the selection rules name, a vendored registry crate, and
//! the non-Rust paths the rules classify. It commits a change on a topic
//! branch and runs the `xtask` binary inside it. The expected selections are
//! written out from the selection rules and the fixture's dependency graph,
//! never computed by the code under test:
//!
//! ```text
//! pohunek-protocol      <- pohunek-client (+ registry crate leftpad)
//!                       <- pohunek-knowledge (dev: pohunek-test-support) <- xtask
//! pohunek-client        <- pohunek-daemon (dev: pohunek-test-support) <- pohunek-cli
//!                                                (+ registry crate rightpad)
//! pohunek-netbird       <- pohunek-relay (dev only) <- pohunek-relay-client
//! ```
//!
//! - a changed member selects itself and every member whose library depends on
//!   it, transitively; a dev-dependency selects its dependent for one hop only;
//! - a lockfile change selects the members that resolve the changed crate;
//! - a change to `pohunek-cli` also selects the daemon test that runs the
//!   `pohunek` binary, with no Cargo edge from the daemon to the CLI;
//! - a path of the runtime-edge map selects the tests it names;
//! - an unmapped path, the CI configuration and the scripts of the CI test
//!   jobs select every test;
//! - prose selects no Rust test and marks the change docs-only;
//! - a failure exits non-zero and writes no selection;
//! - the guard fails on a test read of a path that does not select the reader.

// Rust guideline compliant 2026-10-16

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Output;

use pohunek_test_support::env::TestEnv;
use serde_json::Value;

/// Registry crates of the fixture and the member that depends on each.
const VENDORED: [(&str, &str); 2] = [("leftpad", "pohunek-client"), ("rightpad", "pohunek-cli")];

/// Versions of each registry crate that the vendored directory source holds.
const VENDORED_VERSIONS: [&str; 2] = ["1.0.0", "1.0.1"];

/// Package checksum of every vendored crate; a directory source compares it
/// with the lockfile only, so any 64 hex digits serve.
const VENDORED_CHECKSUM: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// `(directory, package, dependencies, dev-dependencies)` of the fixture's members.
const MEMBERS: &[(&str, &str, &[&str], &[&str])] = &[
    ("crates/protocol", "pohunek-protocol", &[], &[]),
    ("crates/test-support", "pohunek-test-support", &[], &[]),
    ("crates/netbird", "pohunek-netbird", &[], &[]),
    (
        "crates/client",
        "pohunek-client",
        &["pohunek-protocol"],
        &[],
    ),
    (
        "crates/knowledge",
        "pohunek-knowledge",
        &["pohunek-protocol"],
        &["pohunek-test-support"],
    ),
    (
        "crates/daemon",
        "pohunek-daemon",
        &["pohunek-client"],
        &["pohunek-test-support"],
    ),
    ("crates/cli", "pohunek-cli", &["pohunek-daemon"], &[]),
    ("crates/xtask", "xtask", &["pohunek-knowledge"], &[]),
    ("crates/relay", "pohunek-relay", &[], &["pohunek-netbird"]),
    (
        "crates/relay-client",
        "pohunek-relay-client",
        &["pohunek-relay"],
        &[],
    ),
];

/// Non-Rust files of the fixture's base commit.
const OTHER_FILES: &[(&str, &str)] = &[
    ("README.md", "# fixture\n"),
    ("docs/guide.md", "guide\n"),
    ("docs/public-api.md", "| code |\n"),
    (".github/workflows/ci.yml", "name: CI\n"),
    ("scripts/test-partitions", "#!/bin/sh\n"),
    ("compat/codex/subagent-hooks.json", "{}\n"),
    (
        ".cargo/config.toml",
        "[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\ndirectory = \"vendor\"\n",
    ),
];

/// Fixture repository on branch `topic`, forked from a `main` with the workspace.
struct Repo {
    env: TestEnv,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            env: TestEnv::new().expect("hermetic test environment"),
        };
        repo.git(&["init", "--quiet", "--initial-branch=main"]);
        repo.git(&["config", "user.email", "test@example.invalid"]);
        repo.git(&["config", "user.name", "test"]);
        repo.write(
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n\n\
             [workspace.dependencies]\nleftpad = \"1\"\nrightpad = \"1\"\n",
        );
        let toolchain = std::fs::read_to_string(
            pohunek_test_support::workspace_root().join("rust-toolchain.toml"),
        )
        .expect("read the repository's toolchain pin");
        repo.write("rust-toolchain.toml", &toolchain);
        for (dir, name, deps, dev) in MEMBERS {
            repo.write(&format!("{dir}/Cargo.toml"), &manifest(name, deps, dev));
            repo.write(&format!("{dir}/src/lib.rs"), "pub fn f() {}\n");
        }
        repo.write("crates/cli/src/bin/pohunek.rs", "fn main() {}\n");
        for (krate, _) in VENDORED {
            for version in VENDORED_VERSIONS {
                let dir = format!("vendor/{krate}-{version}");
                repo.write(
                    &format!("{dir}/Cargo.toml"),
                    &format!(
                        "[package]\nname = \"{krate}\"\nversion = \"{version}\"\nedition = \"2021\"\n"
                    ),
                );
                repo.write(&format!("{dir}/src/lib.rs"), "pub fn pad() {}\n");
                repo.write(
                    &format!("{dir}/.cargo-checksum.json"),
                    &format!("{{\"files\":{{}},\"package\":\"{VENDORED_CHECKSUM}\"}}"),
                );
            }
        }
        for (path, content) in OTHER_FILES {
            repo.write(path, content);
        }
        repo.cargo(&["generate-lockfile", "--offline"]);
        repo.commit("base");
        repo.git(&["checkout", "--quiet", "-b", "topic"]);
        repo
    }

    fn dir(&self) -> &Path {
        self.env.cwd()
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.dir().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, content).expect("write file");
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.dir().join(path)).expect("read file")
    }

    /// Commits a lockfile that resolves registry crate `krate` to 1.0.0
    /// instead of 1.0.1, with no manifest change.
    fn downgrade(&self, krate: &str) {
        let lock = self.read("Cargo.lock");
        let resolved = format!("name = \"{krate}\"\nversion = \"1.0.1\"");
        assert!(lock.contains(&resolved), "{lock}");
        self.write(
            "Cargo.lock",
            &lock.replace(
                &resolved,
                &format!("name = \"{krate}\"\nversion = \"1.0.0\""),
            ),
        );
        self.commit(&format!("downgrade {krate}"));
    }

    /// Appends a line to `path` (creating it), so every call is a change.
    fn touch(&self, path: &str) {
        let full = self.dir().join(path);
        let mut content = std::fs::read_to_string(&full).unwrap_or_default();
        content.push_str("// changed\n");
        self.write(path, &content);
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "-m", message]);
    }

    /// Commits a change to each of `paths` on the topic branch.
    fn change(&self, paths: &[&str]) {
        for path in paths {
            self.touch(path);
        }
        self.commit("change");
    }

    /// Runs git with system and global configuration disabled, so the
    /// developer's identity, hooks and signing settings never apply.
    fn git(&self, args: &[&str]) {
        let output = self
            .env
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

    fn cargo(&self, args: &[&str]) {
        let output = toolchain(self.env.command("cargo"))
            .args(args)
            .output()
            .expect("run cargo");
        assert!(
            output.status.success(),
            "cargo {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Runs the real `xtask affected <args>` in the repository.
    fn xtask(&self, args: &[&str]) -> Output {
        toolchain(self.env.command(pohunek_test_support::bin_exe("xtask")))
            .arg("affected")
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run xtask")
    }

    fn out_dir(&self) -> PathBuf {
        self.env.root().join("selection")
    }

    /// Runs `affected ci --base main` and returns `selection.json`.
    fn select(&self) -> Value {
        let out = self.out_dir();
        let output = self.xtask(&[
            "ci",
            "--base",
            "main",
            "--out",
            out.to_str().expect("utf-8"),
        ]);
        assert!(
            output.status.success(),
            "xtask affected ci failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = std::fs::read_to_string(out.join("selection.json")).expect("selection.json");
        serde_json::from_str(&text).expect("selection JSON")
    }
}

/// `command` with the toolchain of the test process: the rustup home and the
/// selected toolchain pass through the scrubbed environment; the Cargo home is
/// the environment's own, so the fixture never touches the developer's registry.
fn toolchain(mut command: std::process::Command) -> std::process::Command {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let rustup = std::env::var_os("RUSTUP_HOME")
        .or_else(|| home.map(|home| home.join(".rustup").into_os_string()));
    if let Some(rustup) = rustup {
        command.env("RUSTUP_HOME", rustup);
    }
    if let Some(selected) = std::env::var_os("RUSTUP_TOOLCHAIN") {
        command.env("RUSTUP_TOOLCHAIN", selected);
    }
    command
}

/// The manifest of member `name` with path dependencies on the named members.
fn manifest(name: &str, deps: &[&str], dev: &[&str]) -> String {
    let dir_of = |dep: &str| {
        MEMBERS
            .iter()
            .find(|(_, member, ..)| *member == dep)
            .map(|(dir, ..)| dir.trim_start_matches("crates/"))
            .expect("dependency is a member")
    };
    let mut text = format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n"
    );
    for (krate, consumer) in VENDORED {
        if name == consumer {
            let _ = writeln!(text, "{krate} = {{ workspace = true }}");
        }
    }
    for dep in deps {
        let _ = writeln!(text, "{dep} = {{ path = \"../{}\" }}", dir_of(dep));
    }
    text.push_str("\n[dev-dependencies]\n");
    for dep in dev {
        let _ = writeln!(text, "{dep} = {{ path = \"../{}\" }}", dir_of(dep));
    }
    text
}

/// Tests of xtask that scan every crate, which any change below `crates/` selects.
const XTASK_SCANNERS: &str = "binary(=hermetic_scan)";

/// The selected packages of `selection` as `(name, tests)`, sorted, without
/// the xtask scanners that every crate change selects.
fn packages(selection: &Value) -> Vec<(String, String)> {
    all_packages(selection)
        .into_iter()
        .filter(|(name, tests)| !(name == "xtask" && tests.contains(XTASK_SCANNERS)))
        .collect()
}

/// Whether `selection` runs the xtask scanners of every crate.
fn scans_crates(selection: &Value) -> bool {
    all_packages(selection)
        .iter()
        .any(|(name, tests)| name == "xtask" && (tests == "all" || tests.contains(XTASK_SCANNERS)))
}

/// The selected packages of `selection` as `(name, tests)`, sorted.
fn all_packages(selection: &Value) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = selection["packages"]
        .as_array()
        .expect("packages array")
        .iter()
        .map(|package| {
            (
                package["name"].as_str().expect("name").to_owned(),
                package["tests"].as_str().expect("tests").to_owned(),
            )
        })
        .collect();
    found.sort();
    found
}

fn whole(names: &[&str]) -> Vec<(String, String)> {
    let mut expected: Vec<(String, String)> = names
        .iter()
        .map(|name| ((*name).to_owned(), "all".to_owned()))
        .collect();
    expected.sort();
    expected
}

/// Reasons recorded for package `name`.
fn reasons(selection: &Value, name: &str) -> Vec<String> {
    selection["packages"]
        .as_array()
        .expect("packages array")
        .iter()
        .find(|package| package["name"] == name)
        .unwrap_or_else(|| panic!("{name} is selected"))["reasons"]
        .as_array()
        .expect("reasons")
        .iter()
        .map(|reason| reason.as_str().expect("reason").to_owned())
        .collect()
}

fn assert_full(selection: &Value, cause: &str) {
    assert_eq!(selection["mode"], "full", "{selection:#}");
    assert!(selection["filterset"].is_null(), "{selection:#}");
    let causes = selection["full_causes"].to_string();
    assert!(causes.contains(cause), "cause {cause:?} in {causes}");
}

#[test]
fn a_change_to_a_crate_without_dependents_selects_only_that_crate() {
    let repo = Repo::new();
    repo.change(&["crates/relay-client/src/lib.rs"]);
    let selection = repo.select();
    assert_eq!(selection["mode"], "packages");
    assert_eq!(packages(&selection), whole(&["pohunek-relay-client"]));
    assert!(scans_crates(&selection), "{selection:#}");
    let filterset = selection["filterset"].as_str().expect("filterset");
    assert!(
        filterset.starts_with("package(=pohunek-relay-client) | (package(=xtask) and (("),
        "{filterset}"
    );
    assert_eq!(selection["docs_only"], false);
    let outputs = std::fs::read_to_string(repo.out_dir().join("outputs.env")).expect("outputs");
    assert_eq!(
        outputs,
        format!(
            "mode=packages\ndocs_only=false\nscript_tests=false\nsdk_gates=false\n\
             filterset={filterset}\n"
        )
    );
    let summary = std::fs::read_to_string(repo.out_dir().join("summary.md")).expect("summary");
    assert!(
        summary.contains(
            "| pohunek-relay-client | all | crates/relay-client/src/lib.rs (package directory) |"
        ),
        "{summary}"
    );
}

#[test]
fn a_library_change_selects_its_reverse_dependencies_transitively() {
    let repo = Repo::new();
    repo.change(&["crates/protocol/src/lib.rs"]);
    let selection = repo.select();
    assert_eq!(
        packages(&selection),
        whole(&[
            "pohunek-protocol",
            "pohunek-client",
            "pohunek-knowledge",
            "pohunek-daemon",
            "pohunek-cli",
            "xtask",
        ])
    );
    assert_eq!(
        reasons(&selection, "pohunek-cli"),
        ["depends on pohunek-daemon"]
    );
}

#[test]
fn a_dev_dependency_selects_its_dependent_for_one_hop_only() {
    let repo = Repo::new();
    repo.change(&["crates/netbird/src/lib.rs"]);
    let selection = repo.select();
    // pohunek-relay-client depends on pohunek-relay's library, which a
    // dev-dependency never enters.
    assert_eq!(
        packages(&selection),
        whole(&["pohunek-netbird", "pohunek-relay"])
    );
    assert_eq!(
        reasons(&selection, "pohunek-relay"),
        ["dev-depends on pohunek-netbird"]
    );
}

#[test]
fn a_dependency_only_lockfile_change_selects_the_members_resolving_the_crate() {
    let repo = Repo::new();
    repo.downgrade("leftpad");
    let selection = repo.select();
    assert_eq!(selection["mode"], "packages", "{selection:#}");
    let mut expected = whole(&["pohunek-client", "pohunek-daemon", "pohunek-cli"]);
    expected.push(("xtask".to_owned(), "binary(=dependency_policy)".to_owned()));
    expected.sort();
    assert_eq!(packages(&selection), expected);
    let client = reasons(&selection, "pohunek-client").join("; ");
    assert!(client.contains("leftpad@1.0.0"), "{client}");
}

#[test]
fn a_lockfile_change_rebuilding_the_cli_selects_the_daemon_test_that_runs_it() {
    let repo = Repo::new();
    repo.downgrade("rightpad");
    let selection = repo.select();
    assert_eq!(selection["mode"], "packages", "{selection:#}");
    let mut expected = whole(&["pohunek-cli"]);
    expected.push((
        "pohunek-daemon".to_owned(),
        "binary(=supervised_lifecycle)".to_owned(),
    ));
    expected.push(("xtask".to_owned(), "binary(=dependency_policy)".to_owned()));
    expected.sort();
    assert_eq!(packages(&selection), expected);
    assert!(
        reasons(&selection, "pohunek-daemon")
            .iter()
            .any(|reason| reason.starts_with("the `pohunek` binary of pohunek-cli")),
        "{selection:#}"
    );
}

#[test]
fn a_root_dependency_entry_change_selects_its_inheritors_without_a_resolve_difference() {
    let repo = Repo::new();
    let manifest = repo.read("Cargo.toml");
    // `leftpad` has no default features, so the resolve does not change.
    repo.write(
        "Cargo.toml",
        &manifest.replace(
            "leftpad = \"1\"",
            "leftpad = { version = \"1\", default-features = false }",
        ),
    );
    repo.commit("leftpad without default features");
    let selection = repo.select();
    assert_eq!(selection["mode"], "packages", "{selection:#}");
    let selected = packages(&selection);
    for name in ["pohunek-client", "pohunek-daemon", "pohunek-cli"] {
        assert!(
            selected.contains(&(name.to_owned(), "all".to_owned())),
            "{name}: {selection:#}"
        );
    }
    assert!(
        !selected.iter().any(|(name, _)| name == "pohunek-relay"),
        "{selection:#}"
    );
    assert_eq!(
        reasons(&selection, "pohunek-client"),
        ["dependency diff: root [workspace.dependencies] entry `leftpad` changed"]
    );
}

/// Writes vendored registry crate `name` at `version` with `manifest_rest`
/// after its `[package]` table.
fn vendor(repo: &Repo, name: &str, version: &str, manifest_rest: &str) {
    let dir = format!("vendor/{name}-{version}");
    repo.write(
        &format!("{dir}/Cargo.toml"),
        &format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n\n{manifest_rest}"
        ),
    );
    repo.write(&format!("{dir}/src/lib.rs"), "pub fn f() {}\n");
    repo.write(
        &format!("{dir}/.cargo-checksum.json"),
        &format!("{{\"files\":{{}},\"package\":\"{VENDORED_CHECKSUM}\"}}"),
    );
}

#[test]
fn a_feature_change_of_one_build_hidden_by_the_merged_resolve_selects_its_consumers() {
    let repo = Repo::new();
    // `bar` 1.0.1 enables feature `f` of `foo`. pohunek-test-support builds
    // `foo` with `f` for the target, so the resolve, which merges host and
    // target features, lists `foo` with `f` before and after. Only the host
    // build of `foo`, a build-dependency of pohunek-netbird, gains `f` when the
    // lockfile moves `bar` (a build-dependency of pohunek-relay-client) to 1.0.1.
    repo.git(&["checkout", "--quiet", "main"]);
    vendor(&repo, "foo", "1.0.0", "[features]\nf = []\n");
    vendor(&repo, "bar", "1.0.0", "[dependencies]\nfoo = \"1\"\n");
    vendor(
        &repo,
        "bar",
        "1.0.1",
        "[dependencies]\nfoo = { version = \"1\", features = [\"f\"] }\n",
    );
    for (dir, build_dependency) in [("crates/relay-client", "bar"), ("crates/netbird", "foo")] {
        let manifest = repo.read(&format!("{dir}/Cargo.toml"));
        repo.write(
            &format!("{dir}/Cargo.toml"),
            &format!("{manifest}\n[build-dependencies]\n{build_dependency} = \"1\"\n"),
        );
        repo.write(&format!("{dir}/build.rs"), "fn main() {}\n");
    }
    let manifest = repo.read("crates/test-support/Cargo.toml");
    repo.write(
        "crates/test-support/Cargo.toml",
        &manifest.replacen(
            "[dependencies]\n",
            "[dependencies]\nfoo = { version = \"1\", features = [\"f\"] }\n",
            1,
        ),
    );
    repo.cargo(&["generate-lockfile", "--offline"]);
    let lock = repo.read("Cargo.lock");
    let (old, new) = (
        "name = \"bar\"\nversion = \"1.0.0\"",
        "name = \"bar\"\nversion = \"1.0.1\"",
    );
    assert!(lock.contains(new), "{lock}");
    repo.write("Cargo.lock", &lock.replace(new, old));
    repo.commit("bar 1.0.0 and the foo builds");
    repo.git(&["checkout", "--quiet", "topic"]);
    repo.git(&["reset", "--quiet", "--hard", "main"]);
    repo.write("Cargo.lock", &lock);
    repo.commit("upgrade bar");

    let selection = repo.select();
    assert_eq!(selection["mode"], "packages", "{selection:#}");
    let mut expected = whole(&["pohunek-netbird", "pohunek-relay", "pohunek-relay-client"]);
    expected.push(("xtask".to_owned(), "binary(=dependency_policy)".to_owned()));
    expected.sort();
    assert_eq!(packages(&selection), expected, "{selection:#}");
    let netbird = reasons(&selection, "pohunek-netbird").join("; ");
    assert!(
        netbird.contains(
            "dependency diff: the build resolves other units: +foo v1.0.0 f, -foo v1.0.0"
        ),
        "{netbird}"
    );
}

#[test]
fn a_root_manifest_change_outside_workspace_dependencies_selects_everything() {
    let repo = Repo::new();
    let manifest = repo.read("Cargo.toml");
    repo.write(
        "Cargo.toml",
        &format!("{manifest}\n[profile.dev]\nopt-level = 1\n"),
    );
    repo.commit("profile");
    assert_full(
        &repo.select(),
        "root manifest changed outside [workspace.dependencies]",
    );
}

#[test]
fn a_mapped_runtime_edge_selects_only_the_tests_it_names() {
    let repo = Repo::new();
    repo.change(&["docs/public-api.md"]);
    let selection = repo.select();
    assert_eq!(selection["mode"], "packages");
    assert_eq!(selection["docs_only"], false);
    assert_eq!(
        packages(&selection),
        [(
            "pohunek-protocol".to_owned(),
            "binary(=task_errors)".to_owned()
        )]
    );
    assert_eq!(
        selection["filterset"],
        "(package(=pohunek-protocol) and ((binary(=task_errors))))"
    );
    assert!(!scans_crates(&selection), "{selection:#}");

    let repo = Repo::new();
    repo.change(&["compat/codex/subagent-hooks.json"]);
    assert_eq!(
        packages(&repo.select()),
        whole(&["pohunek-daemon", "pohunek-cli", "xtask"])
    );
}

#[test]
fn an_unmapped_path_selects_everything() {
    let repo = Repo::new();
    repo.change(&["newdir/notes.txt", "crates/relay-client/src/lib.rs"]);
    assert_full(&repo.select(), "newdir/notes.txt");
}

#[test]
fn the_ci_configuration_and_the_test_job_scripts_select_everything() {
    for path in [".github/workflows/ci.yml", "scripts/test-partitions"] {
        let repo = Repo::new();
        repo.change(&[path]);
        let selection = repo.select();
        assert_full(&selection, path);
        assert_eq!(selection["script_tests"], true, "{path}");
    }
}

#[test]
fn a_docs_only_change_selects_no_rust_test() {
    let repo = Repo::new();
    repo.change(&["docs/guide.md", "README.md"]);
    let selection = repo.select();
    assert_eq!(selection["mode"], "none");
    assert_eq!(selection["docs_only"], true);
    assert!(selection["filterset"].is_null());
    assert_eq!(all_packages(&selection), Vec::<(String, String)>::new());
}

#[test]
fn a_hostile_file_name_cannot_forge_summary_rows() {
    let repo = Repo::new();
    repo.change(&["docs/a|b`c\n| forged | all | x |.md"]);
    let selection = repo.select();
    assert_eq!(selection["docs_only"], true);
    let summary = std::fs::read_to_string(repo.out_dir().join("summary.md")).expect("summary");
    assert!(!summary.contains("\n| forged"), "{summary}");
    assert!(
        summary.contains(r"a\|b'c\n\| forged \| all \| x \|.md"),
        "{summary}"
    );
}

#[test]
fn a_failed_selection_exits_non_zero_and_writes_nothing() {
    let repo = Repo::new();
    repo.change(&["crates/relay-client/src/lib.rs"]);
    // An earlier run's outputs in a reused directory are not left behind.
    repo.select();
    let out = repo.out_dir();
    let output = repo.xtask(&[
        "ci",
        "--base",
        "no-such-ref",
        "--out",
        out.to_str().expect("utf-8"),
    ]);
    assert!(!output.status.success());
    for name in ["selection.json", "outputs.env", "summary.md"] {
        assert!(!out.join(name).exists(), "{name} is left");
    }
}

/// Runs the guard and returns `(success, stderr)`.
fn guard(repo: &Repo) -> (bool, String) {
    let output = repo.xtask(&["guard"]);
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn the_guard_fails_on_a_test_read_of_a_path_that_does_not_select_the_reader() {
    let repo = Repo::new();
    repo.write(
        "crates/daemon/tests/reads.rs",
        "#[test]\nfn reads() {\n    let text = std::fs::read_to_string(\n        \
         pohunek_test_support::workspace_root().join(\"docs/guide.md\"),\n    );\n    \
         drop(text);\n}\n",
    );
    repo.commit("planted read");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    assert!(
        stderr.contains("crates/daemon/tests/reads.rs:4: pohunek-daemon reads docs/guide.md"),
        "{stderr}"
    );
}

#[test]
fn the_guard_accepts_a_mapped_read_and_refuses_an_undeclared_root_scan() {
    let repo = Repo::new();
    repo.write(
        "crates/daemon/tests/reads.rs",
        "const HOOKS: &str = \"compat/codex/subagent-hooks.json\";\n\
         #[test]\nfn reads() {\n    let root = pohunek_test_support::workspace_root();\n    \
         drop(std::fs::read(root.join(HOOKS)));\n}\n",
    );
    repo.commit("mapped read");
    let (success, stderr) = guard(&repo);
    assert!(success, "{stderr}");

    repo.write(
        "crates/daemon/tests/scan.rs",
        "#[test]\nfn scans() {\n    let root = pohunek_test_support::workspace_root();\n    \
         walk(&root);\n}\nfn walk(_: &std::path::Path) {}\n",
    );
    repo.commit("root scan");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    assert!(
        stderr.contains(
            "crates/daemon/tests/scan.rs:4: pohunek-daemon reads the repository root: \
             the file uses the repository root itself"
        ),
        "{stderr}"
    );
}

#[test]
fn every_repository_read_of_the_test_code_selects_its_reader() {
    let output = toolchain(std::process::Command::new(pohunek_test_support::bin_exe(
        "xtask",
    )))
    .args(["affected", "guard"])
    .current_dir(pohunek_test_support::workspace_root())
    .output()
    .expect("run xtask affected guard");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Jobs of `.github/workflows/ci.yml` whose runs the selection narrows.
const SELECTED_JOBS: [&str; 4] = [
    "build-tests",
    "fast-tests",
    "integration",
    "systemd-supervision",
];

/// Script paths those jobs name that need not select every test, with the reason.
const SCRIPT_EXEMPTIONS: &[(&str, &str)] = &[(
    "scripts/tests",
    "the script regressions run whenever a script changes (the ScriptTests reminder)",
)];

/// The `scripts/...` paths named by the `run` steps of `SELECTED_JOBS`.
fn test_job_scripts() -> std::collections::BTreeSet<String> {
    let text = std::fs::read_to_string(
        pohunek_test_support::workspace_root().join(".github/workflows/ci.yml"),
    )
    .expect("read the CI workflow");
    let workflow: serde_yaml::Value = serde_yaml::from_str(&text).expect("parse the CI workflow");
    let script =
        regex::Regex::new(r"scripts/[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*").expect("script pattern");
    let mut scripts = std::collections::BTreeSet::new();
    for job in SELECTED_JOBS {
        let steps = workflow["jobs"][job]["steps"]
            .as_sequence()
            .unwrap_or_else(|| panic!("job {job} has steps"));
        for step in steps {
            if let Some(run) = step["run"].as_str() {
                scripts.extend(script.find_iter(run).map(|found| found.as_str().to_owned()));
            }
        }
    }
    scripts
}

#[test]
fn every_script_the_ci_test_jobs_run_selects_everything() {
    let scripts = test_job_scripts();
    assert!(scripts.contains("scripts/test-partitions"), "{scripts:?}");
    for path in scripts {
        if SCRIPT_EXEMPTIONS.iter().any(|(exempt, _)| *exempt == path) {
            continue;
        }
        let repo = Repo::new();
        repo.change(&[path.as_str()]);
        assert_full(&repo.select(), &path);
    }
}

#[test]
fn a_narrowed_rule_covers_only_the_test_sources_it_names() {
    let repo = Repo::new();
    let read = "#[test]\nfn reads() {\n    drop(std::fs::read(\n        \
                pohunek_test_support::workspace_root().join(\"docs/public-api.md\"),\n    ));\n}\n";
    // The rule for docs/public-api.md selects pohunek-protocol's task_errors
    // binary, so a read there is covered.
    repo.write("crates/protocol/tests/task_errors.rs", read);
    repo.commit("named reader");
    let (success, stderr) = guard(&repo);
    assert!(success, "{stderr}");

    // Another test binary of the same package is not selected by that rule.
    repo.write("crates/protocol/tests/other.rs", read);
    repo.commit("unnamed reader");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    assert!(
        stderr.contains(
            "crates/protocol/tests/other.rs:4: pohunek-protocol reads docs/public-api.md"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "no rule selects the test in crates/protocol/tests/other.rs that makes the read"
        ),
        "{stderr}"
    );
}

#[test]
fn a_cli_change_selects_the_daemon_test_that_runs_the_cli_binary() {
    let repo = Repo::new();
    repo.change(&["crates/cli/src/lib.rs"]);
    let selection = repo.select();
    assert_eq!(selection["mode"], "packages");
    assert!(
        packages(&selection).contains(&(
            "pohunek-daemon".to_owned(),
            "binary(=supervised_lifecycle)".to_owned()
        )),
        "{selection}"
    );
    assert!(
        reasons(&selection, "pohunek-daemon")
            .iter()
            .any(|reason| reason.starts_with("the `pohunek` binary of pohunek-cli")),
        "{selection}"
    );
}

#[test]
fn the_guard_requires_a_binary_use_for_a_binary_run_outside_cargo_edges() {
    let repo = Repo::new();
    let spawn = "#[test]\nfn spawns() {\n    let cli = pohunek_test_support::bin_exe(\"pohunekd\")\n        \
                 .with_file_name(\"pohunek\");\n    drop(cli);\n}\n";
    repo.write("crates/daemon/tests/supervised_lifecycle.rs", spawn);
    repo.commit("declared binary run");
    let (success, stderr) = guard(&repo);
    assert!(success, "{stderr}");

    repo.write("crates/daemon/tests/spawns.rs", spawn);
    repo.commit("undeclared binary run");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    assert!(
        stderr.contains(
            "crates/daemon/tests/spawns.rs:4: pohunek-daemon runs the `pohunek` binary of \
             pohunek-cli, but a change to pohunek-cli does not select it"
        ),
        "{stderr}"
    );
}

#[test]
fn a_narrowed_rule_covers_only_the_test_its_filter_selects() {
    let repo = Repo::new();
    let reader = |name: &str| {
        format!(
            "    #[test]\n    fn {name}() {{\n        drop(std::fs::read(\n            \
             pohunek_test_support::workspace_root().join(\"crates/xtask/src/affected/mod.rs\"),\n        \
             ));\n    }}\n"
        )
    };
    let selected = reader("the_allow_list_contains_no_variable_xtask_treats_as_redirecting");
    repo.write("crates/daemon/src/lib.rs", "pub fn f() {}\nmod git;\n");
    repo.write(
        "crates/daemon/src/git.rs",
        &format!("#[cfg(test)]\nmod tests {{\n{selected}}}\n"),
    );
    repo.commit("the selected reader");
    let (success, stderr) = guard(&repo);
    assert!(success, "{stderr}");

    let other = reader("another_reader_of_the_same_path");
    repo.write(
        "crates/daemon/src/git.rs",
        &format!("#[cfg(test)]\nmod tests {{\n{selected}{other}}}\n"),
    );
    repo.commit("a reader the filter does not select");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    assert!(
        stderr.contains(
            "crates/daemon/src/git.rs:12: pohunek-daemon reads crates/xtask/src/affected/mod.rs"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("crates/daemon/src/git.rs:6:"), "{stderr}");
}

/// The daemon's git test that reads xtask's module, as the body of `name`.
fn git_reader(name: &str) -> String {
    format!(
        "    #[test]\n    fn {name}() {{\n        drop(std::fs::read(\n            \
         pohunek_test_support::workspace_root().join(\"crates/xtask/src/affected/mod.rs\"),\n        \
         ));\n    }}\n"
    )
}

/// The daemon git test the narrowed rule for xtask's module selects.
const GIT_TEST: &str = "the_allow_list_contains_no_variable_xtask_treats_as_redirecting";

#[test]
fn a_helper_read_is_covered_only_by_a_filter_that_selects_its_whole_crate() {
    let repo = Repo::new();
    // The rule for runtime-packages selects xtask tests by name prefix
    // (`release_tests::` among them), which says nothing about who calls a
    // helper.
    repo.write(
        "crates/xtask/src/lib.rs",
        "pub fn f() {}\nmod release_tests;\n",
    );
    let module = |body: &str| format!("#[cfg(test)]\nmod tests {{\n{body}}}\n");
    let read = "drop(std::fs::read(\n            \
                pohunek_test_support::workspace_root().join(\"runtime-packages/pi\"),\n        ));";
    repo.write(
        "crates/xtask/src/release_tests.rs",
        &module(&format!(
            "    #[test]\n    fn reads() {{\n        {read}\n    }}\n"
        )),
    );
    repo.commit("a test read");
    let (success, stderr) = guard(&repo);
    assert!(success, "{stderr}");

    repo.write(
        "crates/xtask/src/release_tests.rs",
        &module(&format!(
            "    fn read() {{\n        {read}\n    }}\n    #[test]\n    fn reads() {{\n        read();\n    }}\n"
        )),
    );
    repo.commit("a helper read");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    assert!(
        stderr.contains("crates/xtask/src/release_tests.rs:5: xtask reads runtime-packages/pi"),
        "{stderr}"
    );
}

#[test]
fn a_test_module_is_located_through_its_path_attribute_and_must_be_reachable() {
    let repo = Repo::new();
    repo.write(
        "crates/daemon/src/git.rs",
        "#[cfg(test)]\n#[path = \"git_tests.rs\"]\nmod tests;\n",
    );
    repo.write("crates/daemon/src/git_tests.rs", &git_reader(GIT_TEST));
    // Not declared by the library: no test of the daemon runs the read.
    repo.commit("an unreachable test module");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    assert!(
        stderr.contains("crates/daemon/src/git_tests.rs:4: pohunek-daemon reads"),
        "{stderr}"
    );

    repo.write("crates/daemon/src/lib.rs", "pub fn f() {}\nmod git;\n");
    repo.commit("declare the module");
    let (success, stderr) = guard(&repo);
    assert!(success, "{stderr}");
}

#[test]
fn a_binary_lookup_the_guard_cannot_resolve_fails() {
    let repo = Repo::new();
    repo.write(
        "crates/daemon/tests/lookups.rs",
        "fn binary(name: &str) -> std::ffi::OsString {\n    std::env::var_os(name).expect(\"set\")\n}\n\
         #[test]\nfn runs() {\n    drop(binary(\"POHUNEK_CLI_BIN\"));\n}\n",
    );
    repo.commit("an undecided lookup");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    assert!(
        stderr.contains(
            "crates/daemon/tests/lookups.rs:2: pohunek-daemon calls `var_os(..)` with an \
             argument that is neither a literal nor a constant of the package"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("declare `binary` in LOOKUP_FUNCTIONS"),
        "{stderr}"
    );
}

#[test]
fn a_binary_named_by_another_file_s_constant_or_with_a_trailing_comma_is_found() {
    let repo = Repo::new();
    repo.write(
        "crates/daemon/tests/support/mod.rs",
        "pub const CLI: &str = \"pohunek\";\n",
    );
    repo.write(
        "crates/daemon/tests/spawns.rs",
        "mod support;\n#[test]\nfn spawns() {\n    let daemon = pohunek_test_support::bin_exe(\"pohunekd\");\n    \
         drop(daemon.with_file_name(support::CLI));\n    drop(daemon.with_file_name(\"pohunek\",));\n}\n",
    );
    repo.commit("binary runs through a constant and a trailing comma");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    for line in [5, 6] {
        assert!(
            stderr.contains(&format!(
                "crates/daemon/tests/spawns.rs:{line}: pohunek-daemon runs the `pohunek` binary"
            )),
            "{line}: {stderr}"
        );
    }
}

#[test]
fn a_constant_named_in_two_scopes_with_different_paths_resolves_nowhere() {
    let repo = Repo::new();
    // Each module reads through its own `PATH`; only the second path selects
    // the daemon, so the first read cannot be taken for it.
    repo.write(
        "crates/daemon/tests/reads.rs",
        "mod guide {\n    const PATH: &str = \"docs/guide.md\";\n    #[test]\n    fn reads() {\n        \
         drop(std::fs::read(pohunek_test_support::workspace_root().join(PATH)));\n    }\n}\n\
         mod hooks {\n    const PATH: &str = \"compat/codex/subagent-hooks.json\";\n    #[test]\n    \
         fn reads() {\n        drop(std::fs::read(pohunek_test_support::workspace_root().join(PATH)));\n    \
         }\n}\n",
    );
    repo.commit("two constants named PATH");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    for line in [5, 12] {
        assert!(
            stderr.contains(&format!(
                "crates/daemon/tests/reads.rs:{line}: pohunek-daemon reads the repository root"
            )),
            "{line}: {stderr}"
        );
    }
}

#[test]
fn a_binary_constant_named_in_two_scopes_with_different_binaries_is_undecided() {
    let repo = Repo::new();
    // Another file's `BIN` names the daemon's own binary; neither it nor the
    // last module's value may stand for the first module's.
    repo.write(
        "crates/daemon/tests/support.rs",
        "pub const BIN: &str = \"pohunekd\";\n",
    );
    repo.write(
        "crates/daemon/tests/spawns.rs",
        "mod cli {\n    const BIN: &str = \"pohunek\";\n    #[test]\n    fn spawns() {\n        \
         drop(pohunek_test_support::bin_exe(BIN));\n    }\n}\n\
         mod daemon {\n    const BIN: &str = \"pohunekd\";\n    #[test]\n    fn spawns() {\n        \
         drop(pohunek_test_support::bin_exe(BIN));\n    }\n}\n",
    );
    repo.commit("two constants named BIN");
    let (success, stderr) = guard(&repo);
    assert!(!success, "{stderr}");
    for line in [5, 12] {
        assert!(
            stderr.contains(&format!(
                "crates/daemon/tests/spawns.rs:{line}: pohunek-daemon calls `bin_exe(..)` with an \
                 argument that is neither a literal nor a constant of the package"
            )),
            "{line}: {stderr}"
        );
    }
}
