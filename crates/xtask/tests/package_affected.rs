//! Scenarios for `cargo xtask package affected` driven through the real binary.
//!
//! Each scenario builds its own Git repository with the layout the selector
//! reads (`runtime-packages/<p>/`, `compat/<p>/`, a crate that embeds a
//! package-directory file, prose), commits a change on a topic branch and runs
//! the `xtask` binary inside it. The expected selections are written out from
//! the selection rules, never computed by the code under test:
//!
//! - a file below `runtime-packages/<p>/` or `compat/<p>/` of an official
//!   package selects that package only;
//! - a file a Rust source embeds, a file of a non-official package directory,
//!   and every path outside the reviewed prose allowlist is a core change and
//!   selects every directory under `runtime-packages/`;
//! - prose (`docs/` outside `docs/knowledge`, root `*.md`, GitHub issue forms)
//!   selects nothing;
//! - a failure prints nothing on stdout and exits non-zero.

// Rust guideline compliant 2026-10-12

use std::path::Path;
use std::process::Output;

use pohunek_test_support::env::TestEnv;

/// Every official package of the fixture repository, ascending.
const ALL: &[&str] = &["claude", "codex", "pi"];

/// Files of the fixture's base commit.
///
/// The daemon source embeds `compat/codex/subagent-hooks.json` the way the
/// real daemon does, so that file is a core input although it lives in a
/// package's lock directory.
const BASE_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", "[workspace]\n"),
    ("README.md", "# fixture\n"),
    ("docs/guide.md", "guide\n"),
    ("docs/knowledge/concepts/session.md", "session\n"),
    (".github/workflows/ci.yml", "name: CI\n"),
    (".github/ISSUE_TEMPLATE/work-item.yml", "name: work item\n"),
    (
        "crates/daemon/src/lib.rs",
        "pub const HOOKS: &str = include_str!(\n    \"../../../compat/codex/subagent-hooks.json\"\n);\n",
    ),
    ("crates/package/src/lib.rs", "pub fn handle() {}\n"),
    ("crates/protocol/src/lib.rs", "pub struct Request;\n"),
    ("runtime-packages/README.md", "official packages\n"),
    ("runtime-packages/claude/runtime.toml", "id = \"claude\"\n"),
    ("runtime-packages/codex/runtime.toml", "id = \"codex\"\n"),
    ("runtime-packages/pi/runtime.toml", "id = \"pi\"\n"),
    ("compat/matrix.json", "{}\n"),
    ("compat/claude/compatibility-lock.json", "{}\n"),
    ("compat/codex/compatibility-lock.json", "{}\n"),
    ("compat/codex/subagent-hooks.json", "{}\n"),
    ("compat/pi/compatibility-lock.json", "{}\n"),
    ("compat/hermes/compatibility-lock.json", "{}\n"),
];

/// Fixture repository on branch `topic`, forked from a `main` holding [`BASE_FILES`].
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
        // Marks the fixture as a checkout, which the xtask binary requires.
        repo.write("crates/xtask/Cargo.toml", "");
        for (path, content) in BASE_FILES {
            repo.write(path, content);
        }
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

    /// Appends a line to `path` (creating it), so every call is a change.
    fn touch(&self, path: &str) {
        let full = self.dir().join(path);
        let mut content = std::fs::read_to_string(&full).unwrap_or_default();
        content.push_str("changed\n");
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
            .git_command()
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

    /// Runs the real `xtask package affected` in the repository.
    fn affected(&self, args: &[&str]) -> Output {
        self.env
            .command(pohunek_test_support::bin_exe("xtask"))
            .args(["package", "affected"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("run xtask")
    }

    /// Runs `--base main --json` and returns `(core_changed, packages)`.
    fn select(&self) -> (bool, Vec<String>) {
        let output = self.affected(&["--base", "main", "--json"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "xtask failed: {stderr}");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        assert_eq!(stdout.lines().count(), 1, "one JSON line: {stdout:?}");
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("selection JSON");
        let object = value.as_object().expect("selection object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["core_changed", "packages"], "{stdout}");
        let core_changed = object["core_changed"].as_bool().expect("core_changed bool");
        let packages = object["packages"]
            .as_array()
            .expect("packages array")
            .iter()
            .map(|name| name.as_str().expect("package name").to_owned())
            .collect();
        (core_changed, packages)
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn assert_selects(paths: &[&str], core_changed: bool, packages: &[&str]) {
    let repo = Repo::new();
    repo.change(paths);
    assert_eq!(
        repo.select(),
        (core_changed, names(packages)),
        "changed: {paths:?}"
    );
}

#[test]
fn a_package_only_change_selects_that_package() {
    assert_selects(&["runtime-packages/pi/runtime.toml"], false, &["pi"]);
    assert_selects(
        &["compat/claude/compatibility-lock.json"],
        false,
        &["claude"],
    );
}

#[test]
fn changes_to_two_packages_select_both() {
    assert_selects(
        &[
            "runtime-packages/codex/detect.toml",
            "compat/claude/screens/idle.txt",
        ],
        false,
        &["claude", "codex"],
    );
}

#[test]
fn a_core_only_change_selects_every_package() {
    assert_selects(&["crates/daemon/src/lib.rs"], true, ALL);
    assert_selects(&["Cargo.toml"], true, ALL);
    assert_selects(&[".github/workflows/ci.yml"], true, ALL);
}

#[test]
fn a_combined_core_and_package_change_selects_every_package() {
    assert_selects(
        &[
            "crates/protocol/src/lib.rs",
            "runtime-packages/pi/runtime.toml",
        ],
        true,
        ALL,
    );
}

#[test]
fn a_package_api_or_handler_change_selects_every_package() {
    assert_selects(&["crates/package/src/lib.rs"], true, ALL);
}

#[test]
fn an_unknown_top_level_path_selects_every_package() {
    assert_selects(&["newdir/notes.txt"], true, ALL);
}

#[test]
fn a_docs_only_change_selects_nothing() {
    assert_selects(
        &[
            "docs/guide.md",
            "README.md",
            ".github/ISSUE_TEMPLATE/work-item.yml",
        ],
        false,
        &[],
    );
}

#[test]
fn the_embedded_knowledge_bundle_is_a_core_input() {
    assert_selects(&["docs/knowledge/concepts/session.md"], true, ALL);
}

#[test]
fn a_package_directory_file_embedded_by_a_crate_is_a_core_input() {
    assert_selects(&["compat/codex/subagent-hooks.json"], true, ALL);
}

#[test]
fn files_outside_official_package_directories_are_core_inputs() {
    // `hermes` has a lock directory but no runtime package directory.
    assert_selects(&["compat/hermes/compatibility-lock.json"], true, ALL);
    assert_selects(&["runtime-packages/README.md"], true, ALL);
    assert_selects(&["compat/matrix.json"], true, ALL);
}

#[test]
fn the_official_packages_follow_the_runtime_packages_directory() {
    let repo = Repo::new();
    repo.git(&["checkout", "--quiet", "main"]);
    repo.write("runtime-packages/hermes/runtime.toml", "id = \"hermes\"\n");
    repo.commit("add the hermes package");
    repo.git(&["checkout", "--quiet", "-b", "hermes-topic"]);

    repo.change(&["compat/hermes/compatibility-lock.json"]);
    assert_eq!(repo.select(), (false, names(&["hermes"])));

    repo.change(&["crates/daemon/src/lib.rs"]);
    assert_eq!(
        repo.select(),
        (true, names(&["claude", "codex", "hermes", "pi"]))
    );
}

#[test]
fn uncommitted_and_untracked_changes_are_selected() {
    let repo = Repo::new();
    repo.touch("runtime-packages/codex/runtime.toml");
    repo.write("compat/pi/screens/new.txt", "new\n");
    assert_eq!(repo.select(), (false, names(&["codex", "pi"])));
}

#[test]
fn a_missing_base_fails_without_a_selection() {
    let repo = Repo::new();
    repo.change(&["runtime-packages/pi/runtime.toml"]);
    let output = repo.affected(&["--base", "missing-ref", "--json"]);
    assert!(!output.status.success(), "a missing base must fail");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing-ref"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_directory_outside_a_repository_fails_without_a_selection() {
    let env = TestEnv::new().expect("hermetic test environment");
    // Marks the directory as a checkout, which the xtask binary requires.
    std::fs::create_dir_all(env.cwd().join("crates/xtask")).expect("mkdir xtask");
    std::fs::write(env.cwd().join("crates/xtask/Cargo.toml"), "").expect("write marker");
    let output = env
        .command(pohunek_test_support::bin_exe("xtask"))
        .args(["package", "affected", "--base", "main", "--json"])
        .env("GIT_CEILING_DIRECTORIES", env.root())
        .output()
        .expect("run xtask");
    assert!(!output.status.success(), "a non-repository must fail");
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("git") && !stderr.contains("not inside a pohunek checkout"),
        "the run did not reach the non-repository path: {stderr}"
    );
}

#[test]
fn a_repository_without_package_directories_fails_without_a_selection() {
    let repo = Repo::new();
    repo.git(&["rm", "--quiet", "-r", "runtime-packages"]);
    repo.write("runtime-packages/README.md", "no packages\n");
    repo.commit("drop the packages");
    let output = repo.affected(&["--base", "main", "--json"]);
    assert!(
        !output.status.success(),
        "an empty package listing must fail"
    );
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}
