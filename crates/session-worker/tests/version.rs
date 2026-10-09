//! `pohunek-sessiond --version` answers without any environment.

use std::process::{Command, Output};

use pohunek_test_support::bin_exe;

fn run_worker(arguments: &[&str]) -> Output {
    Command::new(bin_exe("pohunek-sessiond"))
        .args(arguments)
        .env_clear()
        .output()
        .expect("run pohunek-sessiond")
}

fn rejected_with(arguments: &[&str], expected: &str) {
    let output = run_worker(arguments);
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(expected),
        "{output:?}"
    );
}

#[test]
fn worker_process_accepts_only_managed_session_ids() {
    for valid in [
        "s-42",
        "s-18446744073709551615",
        "s-01KYAPVPFVHD56Z69B9CX3XWN2",
    ] {
        rejected_with(
            &["--session-id", valid, "--worker-generation", "abcd2345"],
            "failed to resolve worker paths",
        );
    }
    for invalid in [
        "",
        "42",
        "s-",
        "s-a",
        "s-01kyapvpfvhd56z69b9cx3xwn2",
        "s-01KYAPVPFVHD56Z69B9CX3XWNI",
        "s-184467440737095516150",
        "../s-1",
        "s-1/other",
    ] {
        rejected_with(
            &["--session-id", invalid, "--worker-generation", "abcd2345"],
            "invalid managed session id",
        );
    }
}

#[test]
fn worker_process_accepts_only_safe_generation_tokens() {
    rejected_with(&["--session-id", "s-42"], "--worker-generation is missing");
    for valid in ["abcd2345", "aaaaaaaa", "77777777", "zzzzzzzz"] {
        rejected_with(
            &["--session-id", "s-42", "--worker-generation", valid],
            "failed to resolve worker paths",
        );
    }
    for invalid in [
        "",
        "abcd234",
        "abcd23456",
        "ABCD2345",
        "abcd2301",
        "abcd-345",
        "abcd 345",
        "abcd23é",
    ] {
        rejected_with(
            &["--session-id", "s-42", "--worker-generation", invalid],
            "invalid worker generation",
        );
    }
}

#[test]
fn worker_process_requires_an_absolute_service_config() {
    rejected_with(
        &[
            "--session-id",
            "s-42",
            "--worker-generation",
            "abcd2345",
            "--service-config",
            "service.toml",
        ],
        "--service-config must be an absolute path",
    );
    rejected_with(
        &[
            "--session-id",
            "s-42",
            "--worker-generation",
            "abcd2345",
            "--service-config",
            "/work/service.toml",
        ],
        "failed to resolve worker paths",
    );
}

#[test]
fn worker_process_rejects_repeated_and_valueless_arguments() {
    rejected_with(
        &[
            "--session-id",
            "s-42",
            "--session-id",
            "s-43",
            "--worker-generation",
            "abcd2345",
        ],
        "--session-id was given more than once",
    );
    rejected_with(
        &["--session-id", "s-42", "--worker-generation"],
        "--worker-generation requires a value",
    );
}

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
