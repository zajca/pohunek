//! `pohunek-sessiond --version` answers without any environment.

use std::process::Command;

use pohunek_test_support::bin_exe;

#[test]
fn version_prints_the_binary_name_and_version_with_a_cleared_environment() {
    let output = Command::new(bin_exe("pohunek-sessiond"))
        .arg("--version")
        .env_clear()
        .output()
        .expect("run pohunek-sessiond --version");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("pohunek-sessiond {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn version_combined_with_other_arguments_is_rejected() {
    let output = Command::new(bin_exe("pohunek-sessiond"))
        .args(["--session-id", "s-1", "--version"])
        .env_clear()
        .output()
        .expect("run pohunek-sessiond");

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--version must be the only argument"),
        "{output:?}"
    );
}
