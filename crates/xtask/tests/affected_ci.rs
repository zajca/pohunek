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
//! pohunek-netbird       <- pohunek-relay (dev only) <- pohunek-relay-client
//! ```
//!
//! - a changed member selects itself and every member whose library depends on
//!   it, transitively; a dev-dependency selects its dependent for one hop only;
//! - a lockfile change selects the members that resolve the changed crate;
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

/// Registry crate the fixture's `pohunek-client` depends on, in both versions
/// the vendored directory source holds.
const LEFTPAD_VERSIONS: [&str; 2] = ["1.0.0", "1.0.1"];

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
             [workspace.dependencies]\nleftpad = \"1\"\n",
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
        for version in LEFTPAD_VERSIONS {
            let dir = format!("vendor/leftpad-{version}");
            repo.write(
                &format!("{dir}/Cargo.toml"),
                &format!(
                    "[package]\nname = \"leftpad\"\nversion = \"{version}\"\nedition = \"2021\"\n"
                ),
            );
            repo.write(&format!("{dir}/src/lib.rs"), "pub fn pad() {}\n");
            repo.write(
                &format!("{dir}/.cargo-checksum.json"),
                &format!("{{\"files\":{{}},\"package\":\"{VENDORED_CHECKSUM}\"}}"),
            );
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
    if name == "pohunek-client" {
        text.push_str("leftpad = { workspace = true }\n");
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
    let lock = repo.read("Cargo.lock");
    assert!(lock.contains("version = \"1.0.1\""), "{lock}");
    repo.write(
        "Cargo.lock",
        &lock.replace("version = \"1.0.1\"", "version = \"1.0.0\""),
    );
    repo.commit("downgrade leftpad");
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
    let out = repo.out_dir();
    let output = repo.xtask(&[
        "ci",
        "--base",
        "no-such-ref",
        "--out",
        out.to_str().expect("utf-8"),
    ]);
    assert!(!output.status.success());
    assert!(!out.join("selection.json").exists());
    assert!(!out.join("outputs.env").exists());
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
