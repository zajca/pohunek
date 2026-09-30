//! Exercises the real macOS Keychain backend behind `KeyringTokenSource`.
//!
//! The test writes to the user's default keychain, so it only runs against a
//! throwaway keychain: `GUI_CORE_TEST_KEYCHAIN` must name a keychain file
//! that is already the user-domain default (the CI job creates and selects
//! one). It refuses to run when the default is any other keychain, so the
//! operator's login keychain is never touched.
//!
//! On CI (`CI` set) a missing guard variable fails the test; elsewhere the test
//! prints a `SKIPPED` line and returns, because libtest cannot report a
//! runtime skip.
//!
//! One test walks the states in order because they share process-global
//! keychain state: success, entry not found, locked, keychain gone.
#![cfg(target_os = "macos")]
// Rust guideline compliant 2026-09-30
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pohunek_gui_core::providers::linear::{KeyringTokenSource, TokenErrorKind, TokenSource};
use security_framework::os::macos::keychain::SecKeychain;

/// Names the throwaway keychain file the test may write to.
const KEYCHAIN_ENV: &str = "GUI_CORE_TEST_KEYCHAIN";

/// Set by GitHub Actions and most CI systems.
const CI_ENV: &str = "CI";

/// Longest wait for one lookup against a healthy or locked keychain.
const LOOKUP_DEADLINE: Duration = Duration::from_secs(30);

/// Grace period for runtime shutdown when a blocking lookup never returns.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Fixture value; it is not a credential.
const FIXTURE_VALUE: &str = "keychain-fixture-value";

fn security(arguments: &[&str]) -> std::process::Output {
    Command::new("/usr/bin/security")
        .args(arguments)
        .output()
        .expect("/usr/bin/security runs")
}

/// Returns the throwaway keychain path, or `None` when the test is skipped.
fn throwaway_keychain() -> Option<PathBuf> {
    match std::env::var_os(KEYCHAIN_ENV) {
        Some(path) => Some(PathBuf::from(path)),
        None => {
            assert!(
                std::env::var_os(CI_ENV).is_none(),
                "{KEYCHAIN_ENV} must name the throwaway keychain on CI"
            );
            eprintln!("SKIPPED: {KEYCHAIN_ENV} is not set; no real Keychain test ran");
            None
        }
    }
}

/// Fails unless `keychain` is the user-domain default keychain.
fn require_default_keychain(keychain: &Path) {
    let output = security(&["default-keychain", "-d", "user"]);
    assert!(output.status.success(), "security default-keychain failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let default = PathBuf::from(stdout.trim().trim_matches('"'));
    let same = match (default.canonicalize(), keychain.canonicalize()) {
        (Ok(default), Ok(named)) => default == named,
        _ => false,
    };
    assert!(
        same,
        "the default keychain is not the throwaway keychain named by {KEYCHAIN_ENV}; refusing to write"
    );
}

fn run_lookup(
    runtime: &tokio::runtime::Runtime,
    source: &KeyringTokenSource,
    key: &str,
) -> Result<String, pohunek_gui_core::providers::linear::TokenError> {
    runtime.block_on(async {
        tokio::time::timeout(LOOKUP_DEADLINE, source.token(key))
            .await
            .expect("keychain lookup did not return within the deadline")
    })
}

#[test]
fn real_keychain_reports_success_missing_locked_and_gone() {
    let Some(keychain) = throwaway_keychain() else {
        return;
    };
    require_default_keychain(&keychain);

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let service = format!(
        "pohunek-gui-core-keychain-test-{}-{nonce}",
        std::process::id()
    );
    let key = "linear-token-ref";
    let source = KeyringTokenSource::new(service.clone());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");

    // Missing entry.
    let missing = run_lookup(&runtime, &source, key).expect_err("no entry yet");
    assert_eq!(missing.kind(), TokenErrorKind::NotFound);
    assert!(missing.to_string().contains(key));

    // Success: the value round-trips through the real backend.
    let entry = keyring::Entry::new(&service, key).expect("entry");
    entry.set_password(FIXTURE_VALUE).expect("store fixture");
    let value = run_lookup(&runtime, &source, key).expect("stored entry is readable");
    assert!(
        value == FIXTURE_VALUE,
        "read value differs from stored value"
    );

    // Locked: with user interaction disabled a locked keychain answers
    // errSecInteractionNotAllowed instead of waiting on an unlock dialog.
    let lock = security(&["lock-keychain", keychain.to_str().expect("utf-8 path")]);
    assert!(lock.status.success(), "security lock-keychain failed");
    let interaction = SecKeychain::disable_user_interaction().expect("disable interaction");
    let locked = run_lookup(&runtime, &source, key).expect_err("locked keychain");
    drop(interaction);
    assert_eq!(locked.kind(), TokenErrorKind::Locked, "{locked}");
    assert!(locked.to_string().contains("unlock the login keychain"));
    assert!(!locked.to_string().contains(FIXTURE_VALUE));

    // Gone: with the keychain file deleted the store is unavailable or, when
    // the search list falls back to another keychain, the entry is not found.
    // Either way no value is returned.
    let delete = security(&["delete-keychain", keychain.to_str().expect("utf-8 path")]);
    assert!(delete.status.success(), "security delete-keychain failed");
    let gone = run_lookup(&runtime, &source, key).expect_err("keychain deleted");
    assert!(
        matches!(
            gone.kind(),
            TokenErrorKind::Unavailable | TokenErrorKind::NotFound
        ),
        "unexpected kind {:?}",
        gone.kind()
    );

    runtime.shutdown_timeout(SHUTDOWN_GRACE);
}
