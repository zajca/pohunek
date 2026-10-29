//! The xtask binary acts on the checkout of its working directory, not on the
//! checkout that compiled it: `scripts/release --dry-run` runs a binary built in
//! the real clone from an exported copy of it.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Fragment of the knowledge log only the fixture checkout holds.
const MARKER_FRAGMENT: &str = "9999-checkout-root-marker.md";

/// Runs the built xtask binary with `args` from `cwd`.
fn xtask(cwd: &Path, args: &[&str]) -> Output {
    Command::new(pohunek_test_support::bin_exe("xtask"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run xtask")
}

/// A checkout that holds the committed knowledge log and one extra fragment.
fn fixture_checkout() -> tempfile::TempDir {
    let checkout = pohunek_test_support::tempdir_with_prefix("pxd-root-").expect("fixture root");
    let root = checkout.path();
    fs::create_dir_all(root.join("crates/xtask")).expect("create the xtask crate directory");
    fs::write(root.join("crates/xtask/Cargo.toml"), "").expect("write the xtask marker");
    let knowledge = root.join("docs/knowledge");
    fs::create_dir_all(knowledge.join("log.d")).expect("create the fragment directory");
    fs::copy(
        pohunek_test_support::workspace_root().join("docs/knowledge/log.md"),
        knowledge.join("log.md"),
    )
    .expect("copy the committed log");
    fs::write(
        knowledge.join("log.d").join(MARKER_FRAGMENT),
        "## Unreleased (2026-10-29, checkout root marker, #9999)\n\n- A fragment only the fixture checkout holds.\n",
    )
    .expect("write the marker fragment");
    checkout
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn log_fold_acts_on_the_checkout_of_the_working_directory() {
    let checkout = fixture_checkout();
    let output = xtask(
        checkout.path(),
        &["docs", "log", "fold", "--version", "9.9.9", "--dry-run"],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout(&output).contains(MARKER_FRAGMENT),
        "the fold ignored the fixture checkout: {}",
        stdout(&output)
    );
}

#[test]
fn log_fold_run_from_a_subdirectory_acts_on_the_enclosing_checkout() {
    let checkout = fixture_checkout();
    let output = xtask(
        &checkout.path().join("crates/xtask"),
        &["docs", "log", "fold", "--version", "9.9.9", "--dry-run"],
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout(&output).contains(MARKER_FRAGMENT),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_directory_outside_every_checkout_is_refused_by_name() {
    let outside = pohunek_test_support::tempdir_with_prefix("pxd-none-").expect("fixture dir");
    let output = xtask(
        outside.path(),
        &["docs", "log", "fold", "--version", "9.9.9", "--dry-run"],
    );

    assert!(!output.status.success(), "{}", stdout(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&*outside.path().to_string_lossy()),
        "the refusal does not name the working directory: {stderr}"
    );
}

#[test]
fn a_directory_without_the_xtask_crate_is_not_taken_for_a_checkout() {
    let decoy = fixture_checkout();
    fs::remove_file(decoy.path().join("crates/xtask/Cargo.toml")).expect("remove the xtask marker");
    let output = xtask(
        decoy.path(),
        &["docs", "log", "fold", "--version", "9.9.9", "--dry-run"],
    );

    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(
        !stdout(&output).contains(MARKER_FRAGMENT),
        "the fold acted on a directory that holds no xtask crate: {}",
        stdout(&output)
    );
}
