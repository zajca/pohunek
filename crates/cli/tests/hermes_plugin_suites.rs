//! Exercises the Hermes operator plugin suites through their native runtimes.
//!
//! The plugin under test is the official package's operator payload
//! (`runtime-packages/hermes/operator/`) plus the core hook reporter the CLI
//! installs beside it, read at test run time. The interpreter and Bash come
//! from `support/interpreter.rs`, which accepts any Python 3.10+ with `PyYAML`
//! on `PATH` (or `POHUNEK_PYTHON_BIN`).

// Rust guideline compliant 2026-10-13

use std::fs;
use std::path::{Path, PathBuf};

use pohunek_test_support::env::TestEnv;

#[path = "support/interpreter.rs"]
mod interpreter;

const LOCALE: &str = "C";
const SYSTEM_PATH: &str = "/usr/bin:/bin";
const SMOKE_SUCCESS: &str = "controlled Hermes release-plugin smoke passed";
/// Operator payload of the official Hermes package, relative to the workspace root.
const OPERATOR_DIR: &str = "runtime-packages/hermes/operator";
/// Core hook reporter the CLI compiles in, relative to the CLI crate.
const CORE_REPORTER: &str = "src/hermes_integration/assets/pohunek/_pohunek_reporter.py";
/// Plugin runtime suite, relative to the CLI crate.
const RUNTIME_SUITE: &str = "src/hermes_integration/assets/tests/test_plugin_runtime.py";

fn manifest_dir() -> PathBuf {
    pohunek_test_support::manifest_dir()
}

fn repository_root() -> PathBuf {
    manifest_dir()
        .parent()
        .and_then(Path::parent)
        .expect("CLI crate must be nested under the workspace crates directory")
        .to_path_buf()
}

/// Copies every file below `source` into `destination`, keeping relative paths.
fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create plugin directory");
    for entry in fs::read_dir(source).expect("read operator payload directory") {
        let entry = entry.expect("operator payload entry");
        let target = destination.join(entry.file_name());
        if entry
            .file_type()
            .expect("operator payload entry type")
            .is_dir()
        {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy operator payload file");
        }
    }
}

/// Assembles the installed plugin layout the suite imports, `pohunek/` beside
/// `tests/`, from the package payload and the core reporter.
#[test]
fn operator_plugin_runtime_suite_passes() {
    // The suite creates its scratch directories through `tempfile`, which follows
    // the private `TMPDIR` of the environment.
    let env = TestEnv::new().expect("hermetic test environment");
    let plugin = env.root().join("pohunek");
    copy_tree(&repository_root().join(OPERATOR_DIR), &plugin);
    fs::copy(
        manifest_dir().join(CORE_REPORTER),
        plugin.join("_pohunek_reporter.py"),
    )
    .expect("copy the core reporter");
    let suite = env.root().join("tests/test_plugin_runtime.py");
    fs::create_dir_all(suite.parent().expect("suite directory")).expect("create suite directory");
    fs::copy(manifest_dir().join(RUNTIME_SUITE), &suite).expect("copy the runtime suite");
    let output = env
        .command(interpreter::python())
        .args(["-I", "-B"])
        .arg(&suite)
        .env("LANG", LOCALE)
        .output()
        .expect("start controlled local Python for the Hermes plugin runtime suite");

    assert!(
        output.status.success(),
        "Hermes plugin runtime suite failed: status={:?}, stdout={}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn release_plugin_smoke_self_test_passes() {
    let suite = repository_root().join("scripts/tests/smoke-hermes-plugin-release.sh");
    let env = TestEnv::new().expect("hermetic test environment");
    let output = env
        .command(interpreter::bash())
        .arg(&suite)
        .env("LANG", LOCALE)
        .env("LC_ALL", LOCALE)
        .env("PATH", SYSTEM_PATH)
        .output()
        .expect("start controlled Bash for the Hermes release-plugin smoke self-test");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success() && stdout.lines().any(|line| line == SMOKE_SUCCESS),
        "Hermes release-plugin smoke self-test failed or omitted its success line: status={:?}, stdout={}, stderr={}",
        output.status.code(),
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
}
