//! Exercises the embedded Hermes plugin suites through their native runtimes.
//!
//! The interpreter and Bash come from `support/interpreter.rs`, which accepts
//! any Python 3.10+ with `PyYAML` on `PATH` (or `POHUNEK_PYTHON_BIN`).

// Rust guideline compliant 2026-08-07

use std::path::{Path, PathBuf};

use pohunek_test_support::env::TestEnv;

#[path = "support/interpreter.rs"]
mod interpreter;

const LOCALE: &str = "C";
const SYSTEM_PATH: &str = "/usr/bin:/bin";
const SMOKE_SUCCESS: &str = "controlled Hermes release-plugin smoke passed";

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

#[test]
fn embedded_plugin_runtime_suite_passes() {
    let suite = manifest_dir().join("src/hermes_integration/assets/tests/test_plugin_runtime.py");
    // The suite creates its scratch directories through `tempfile`, which follows
    // the private `TMPDIR` of the environment.
    let env = TestEnv::new().expect("hermetic test environment");
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
