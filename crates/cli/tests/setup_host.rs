//! Bare `pohunek setup`: the consent gate and the plan gate, through the built
//! binary.
//!
//! Every scenario starts from a private environment and checks that the host
//! is unchanged after the run: the file tree under the test root is the same
//! before and after. The refusal and plan scenarios need no service manager,
//! because they stop before the first write. The apply path needs a real
//! service manager and the release daemon's `--detect` planner, so it is
//! covered by the release harness rather than here.

#![cfg(unix)]

// Rust guideline compliant 2026-10-14

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use pohunek_test_support::env::TestEnv;
use serde_json::Value;

/// Mode of the fixture daemon: owner and group and other may execute it.
const FIXTURE_EXECUTABLE_MODE: u32 = 0o755;

/// Directory, below the test root, that holds the copied CLI and its daemon.
const RELEASE_DIR: &str = "release";

/// File, beside the fixture daemon, that holds the report it prints.
const FIXTURE_REPORT_FILE: &str = "fixture-report.json";

/// Report the fixture daemon prints for a plan it refuses.
const REFUSED_REPORT: &str = r#"{
  "report_version": 1,
  "daemon_version": "fixture",
  "dry_run": true,
  "packages": [],
  "catalog": "unavailable",
  "profiles": [],
  "integrations": [],
  "refusals": [
    {"subject": "claude", "code": "official_package_archive_invalid", "detail": "fixture refusal: archive missing"}
  ]
}"#;

/// Report with a version this CLI does not read, so the plan is unusable.
const UNREADABLE_REPORT: &str = r#"{
  "report_version": 2,
  "daemon_version": "fixture",
  "dry_run": true,
  "packages": [],
  "catalog": "unavailable",
  "profiles": [],
  "integrations": [],
  "refusals": []
}"#;

/// Relative path to every entry under `root`, with its size for files, so a
/// change of any file shows up in the comparison.
fn tree(root: &Path) -> BTreeMap<PathBuf, Option<u64>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(listing) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in listing {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("below root").to_path_buf();
            let kind = entry.file_type().expect("file type");
            if kind.is_dir() {
                entries.insert(relative, None);
                pending.push(path);
            } else {
                let size = entry.metadata().expect("metadata").len();
                entries.insert(relative, Some(size));
            }
        }
    }
    entries
}

/// Runs `pohunek` with `arguments` from `binary`, with no terminal on stdin.
fn run_setup(env: &TestEnv, binary: &Path, arguments: &[&str]) -> Output {
    env.command(binary)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .expect("spawn pohunek")
}

/// Copies the CLI under test into a release directory with a fixture daemon.
///
/// The CLI runs from that copy, so the staged release it plans against is the
/// fixture's directory and never the build tree. The daemon prints its report
/// with shell builtins only, because the test environment's `PATH` is scrubbed.
fn release_with_fixture(env: &TestEnv, report: &str) -> PathBuf {
    let release = env.root().join(RELEASE_DIR);
    fs::create_dir_all(&release).expect("create release directory");
    let binary = release.join("pohunek");
    fs::copy(pohunek_test_support::bin_exe("pohunek"), &binary).expect("copy the CLI");
    // `read` in the fixture drops a final line without its newline.
    fs::write(release.join(FIXTURE_REPORT_FILE), format!("{report}\n"))
        .expect("write fixture report");
    let daemon = release.join(pohunek_paths::DAEMON_EXECUTABLE_NAME);
    let script = format!(
        "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' \"$line\"; done < \"${{0%/*}}/{FIXTURE_REPORT_FILE}\"\n"
    );
    fs::write(&daemon, script).expect("write fixture daemon");
    fs::set_permissions(&daemon, fs::Permissions::from_mode(FIXTURE_EXECUTABLE_MODE))
        .expect("make fixture daemon executable");
    binary
}

/// The stdout of a run as one JSON document.
fn json_document(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout is one JSON document")
}

/// Without a terminal and without `--yes`, setup is refused before it reads
/// the plan or writes anything: the human error names the consent and the
/// hint, and the host is unchanged.
#[test]
fn bare_setup_without_a_terminal_is_refused_and_changes_nothing() {
    let env = TestEnv::new().expect("private environment");
    let binary = pohunek_test_support::bin_exe("pohunek");
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup"]);

    assert!(!output.status.success(), "a refused setup exits non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("needs your consent") && stderr.contains("nothing was changed"),
        "stderr names the refusal: {stderr}"
    );
    assert!(stderr.contains("--yes"), "stderr names the hint: {stderr}");
    assert_eq!(tree(env.root()), before, "a refused setup writes nothing");
}

/// With `--json` setup cannot ask, so the refusal is one JSON error document
/// with its stable code and no change to the host.
#[test]
fn json_setup_without_yes_is_refused_with_a_stable_code() {
    let env = TestEnv::new().expect("private environment");
    let binary = pohunek_test_support::bin_exe("pohunek");
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--json"]);

    assert!(!output.status.success(), "a refused setup exits non-zero");
    let document = json_document(&output);
    let error = &document["err"];
    assert_eq!(error["code"], "setup_consent_required");
    assert_eq!(error["class"], "configuration");
    assert!(
        error["recover"]
            .as_str()
            .is_some_and(|hint| hint.contains("--yes")),
        "the recovery hint names --yes: {error}"
    );
    assert_eq!(tree(env.root()), before, "a refused setup writes nothing");
}

/// A plan the release daemon refuses stops setup before the service, the
/// packages, the hooks and the config are touched, even with `--yes`.
#[test]
fn a_refused_plan_stops_setup_before_any_write() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, REFUSED_REPORT);
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(!output.status.success(), "a refused plan exits non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("setup stopped before any change")
            && stderr.contains("fixture refusal: archive missing"),
        "stderr carries the daemon's refusal: {stderr}"
    );
    assert_eq!(tree(env.root()), before, "a refused plan writes nothing");
}

/// A plan report of a version this CLI does not read is an unusable plan, so
/// setup stops before any write, with the reason.
#[test]
fn an_unreadable_plan_stops_setup_before_any_write() {
    let env = TestEnv::new().expect("private environment");
    let binary = release_with_fixture(&env, UNREADABLE_REPORT);
    let before = tree(env.root());

    let output = run_setup(&env, &binary, &["setup", "--yes"]);

    assert!(
        !output.status.success(),
        "an unreadable plan exits non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("its report has version 2"),
        "stderr names the report version: {stderr}"
    );
    assert_eq!(
        tree(env.root()),
        before,
        "an unreadable plan writes nothing"
    );
}

/// `setup config` keeps its own behavior: it writes the config files on a
/// fresh host and keeps an existing one, with no plan or consent involved.
#[test]
fn setup_config_still_writes_only_config_files() {
    let env = TestEnv::new().expect("private environment");
    let binary = pohunek_test_support::bin_exe("pohunek");

    let output = run_setup(&env, &binary, &["setup", "config", "--json"]);
    assert!(output.status.success(), "setup config succeeds");
    let first = json_document(&output);
    let created = first["ok"]["created"].as_array().expect("created list");
    assert_eq!(created.len(), 3, "a fresh host gets the three files");

    let attach = env.config_home().join("pohunek").join("attach.conf");
    fs::write(&attach, "user-edited").expect("user edit");
    let output = run_setup(&env, &binary, &["setup", "config", "--json"]);
    assert!(output.status.success(), "a second setup config succeeds");
    let second = json_document(&output);
    assert!(second["ok"]["created"]
        .as_array()
        .expect("created list")
        .is_empty());
    assert_eq!(
        fs::read_to_string(&attach).expect("read attach.conf"),
        "user-edited",
        "an existing file is kept"
    );
}
