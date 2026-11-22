//! Unit tests for the `assistant_package_missing` recovery: the stable-code
//! match and the remedy replacement against a synthetic N-1 daemon error.
//!
//! Kept in its own `*_tests.rs` file (rather than in the inline `tests`
//! module) so the recovery tooling can tell test code from production code.

use super::{is_missing_package, outcome_recover, with_recover};
use crate::commands::setup_assistant::{AssistantStep, GapCause};
use crate::error::CliError;

/// A synthetic N-1 daemon error carrying the historical placeholder remedy:
/// the match keys on the stable code, so both the bare and the wrapped form
/// recover.
fn old_hint_error() -> CliError {
    CliError::Protocol(protocol::ProtocolError::new(
        protocol::ErrorClass::Runtime,
        "assistant_package_missing",
        "the assistant context package `pohunek.context.assistant` cannot be used: it is not installed",
        Some(
            "install it with `pohunek plugin install <archive> --sha256 <digest>`".to_owned(),
        ),
    ))
}

#[test]
fn missing_package_matches_the_stable_code_only() {
    assert!(is_missing_package(&old_hint_error()));
    let wrapped = CliError::Client(pohunek_client::ClientError::Protocol(
        protocol::ProtocolError::new(
            protocol::ErrorClass::Runtime,
            "assistant_package_missing",
            "the assistant context package cannot be used",
            None,
        ),
    ));
    assert!(is_missing_package(&wrapped));
    let other = CliError::Protocol(protocol::ProtocolError::new(
        protocol::ErrorClass::Runtime,
        "materialization_failed",
        "the bundle failed",
        Some("install it with `pohunek plugin install <archive> --sha256 <digest>`".to_owned()),
    ));
    assert!(!is_missing_package(&other));
}

#[test]
fn with_recover_replaces_the_stale_placeholder_remedy() {
    let error = old_hint_error();
    let message = "run `pohunek assistant setup` again later".to_owned();
    let recovered = with_recover(error, Some(message.clone()));
    let CliError::Protocol(error) = recovered else {
        panic!("the error stays a protocol error");
    };
    assert_eq!(error.code, "assistant_package_missing");
    let recover = error.recover.expect("the recover is replaced");
    assert_eq!(recover, message);
    assert!(!recover.contains('<'), "no placeholder remains: {recover}");
}

#[test]
fn outcome_recover_names_the_daemon_version_without_placeholders() {
    let step = AssistantStep::Gap {
        detail: "the index lists nothing".to_owned(),
        command: "pohunek assistant setup (or `pohunek setup`)".to_owned(),
    };
    let recover = outcome_recover(&step, Some(GapCause::NotListed), Some("0.42.0"))
        .expect("a not-listed gap has a recover");
    assert!(
        recover.contains("not available for core v0.42.0"),
        "the recover names the daemon version: {recover}"
    );
    assert!(!recover.contains('<'), "no placeholder remains: {recover}");
}
