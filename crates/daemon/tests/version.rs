//! `pohunekd --version` answers without any environment.

use std::process::Command;

#[test]
fn version_prints_the_binary_name_and_version_with_a_cleared_environment() {
    let output = Command::new(env!("CARGO_BIN_EXE_pohunekd"))
        .arg("--version")
        .env_clear()
        .output()
        .expect("run pohunekd --version");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("pohunekd {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn version_with_other_arguments_is_an_ordinary_invocation() {
    let output = Command::new(env!("CARGO_BIN_EXE_pohunekd"))
        .args(["--version", "--service-config"])
        .env_clear()
        .output()
        .expect("run pohunekd");

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
}
