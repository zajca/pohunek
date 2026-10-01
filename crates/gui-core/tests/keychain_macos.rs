//! Exercises the real macOS Keychain backend behind `KeyringTokenSource`.
//!
//! The test writes to, locks and deletes a keychain, so it only runs against a
//! throwaway one: `GUI_CORE_TEST_KEYCHAIN` must name a keychain file whose name
//! carries [`KEYCHAIN_MARKER`], that lives outside the user's and the system's
//! `Library/Keychains`, and that is already the user-domain default (the CI job
//! creates and selects one). Every check runs before the first write, so an
//! operator's login keychain is refused rather than modified.
//!
//! On CI (`CI` set) a missing guard variable fails the test; elsewhere the test
//! prints a `SKIPPED` line and returns, because libtest cannot report a
//! runtime skip.
//!
//! One test walks the states in order because they share process-global
//! keychain state: not found, success, then locked.
//!
//! The locked case goes last and runs through the production bounded path
//! (the store lookup under a short caller timeout). Depending on the session it
//! reports `Locked` (no unlock UI may be shown) or the bound elapses because
//! the Security framework waits on an unlock prompt, which is the `Timeout`
//! contract. In the second case the blocked read keeps the store's single
//! lookup permit, so no lookup may follow in this process; the process
//! refuses further lookups until restart, and the test ends there. The
//! interactive prompt itself is not driven; the permit and timeout behavior is
//! covered by the injected-closure tests in `credential_store`.
//!
//! A deleted keychain is not exercised: the Security framework keeps serving an
//! already-opened database after `security delete-keychain`, so the state is
//! not observable from a running process. The `Unavailable` mapping for
//! `errSecNoSuchKeychain` is covered by the unit tests over keyring's
//! `decode_error`.
#![forbid(unsafe_code)]

// Rust guideline compliant 2026-10-01

use std::path::Path;

/// File-name marker every throwaway keychain carries. The CI job reads this
/// constant from this file to name the keychain it creates.
const KEYCHAIN_MARKER: &str = "pohunek-gui-test";

/// Why a keychain path is not an acceptable test target, `Ok` otherwise.
///
/// Both paths must already be canonical so symlinks cannot hide a location.
fn validate_throwaway_keychain(keychain: &Path, home: &Path) -> Result<(), String> {
    let name = keychain
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "the keychain path has no UTF-8 file name".to_owned())?;
    if !name.contains(KEYCHAIN_MARKER) {
        return Err(format!(
            "keychain file name must contain `{KEYCHAIN_MARKER}`; refusing to touch it"
        ));
    }
    for protected in [home.join("Library/Keychains"), "/Library/Keychains".into()] {
        if keychain.starts_with(&protected) {
            return Err(format!(
                "keychain lives under {}, which holds real keychains; refusing to touch it",
                protected.display()
            ));
        }
    }
    Ok(())
}

#[test]
fn guard_accepts_a_marked_keychain_outside_library_keychains() {
    let home = Path::new("/Users/runner");
    let keychain = Path::new("/private/var/folders/x/T/pohunek-gui-test.keychain-db");
    assert_eq!(validate_throwaway_keychain(keychain, home), Ok(()));
}

#[test]
fn guard_refuses_the_login_keychain() {
    let home = Path::new("/Users/runner");
    let login = Path::new("/Users/runner/Library/Keychains/login.keychain-db");
    assert!(validate_throwaway_keychain(login, home).is_err());
}

#[test]
fn guard_refuses_a_marked_name_inside_library_keychains() {
    let home = Path::new("/Users/runner");
    let inside = Path::new("/Users/runner/Library/Keychains/pohunek-gui-test.keychain-db");
    assert!(validate_throwaway_keychain(inside, home).is_err());
    let system = Path::new("/Library/Keychains/pohunek-gui-test.keychain-db");
    assert!(validate_throwaway_keychain(system, home).is_err());
}

#[test]
fn guard_refuses_an_unmarked_temp_path() {
    let home = Path::new("/Users/runner");
    let keychain = Path::new("/private/var/folders/x/T/other.keychain-db");
    let error = validate_throwaway_keychain(keychain, home).expect_err("marker missing");
    assert!(error.contains(KEYCHAIN_MARKER), "{error}");
}

#[cfg(target_os = "macos")]
mod real_keychain {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use pohunek_gui_core::providers::linear::{
        KeyringTokenSource, TokenError, TokenErrorKind, TokenSource,
    };

    use super::validate_throwaway_keychain;

    /// Names the throwaway keychain file the test may write to.
    const KEYCHAIN_ENV: &str = "GUI_CORE_TEST_KEYCHAIN";

    /// Set by GitHub Actions and most CI systems.
    const CI_ENV: &str = "CI";

    /// Longest wait for one lookup against a healthy keychain.
    const LOOKUP_DEADLINE: Duration = Duration::from_secs(30);

    /// Caller timeout of the locked lookup: the bound the product contract
    /// promises, short because a stuck unlock prompt never answers.
    const LOCKED_BOUND: Duration = Duration::from_secs(3);

    /// Slack on top of [`LOCKED_BOUND`] for scheduling on a loaded runner.
    const BOUND_SLACK: Duration = Duration::from_secs(5);

    /// Grace period for runtime shutdown when a blocking lookup never returns.
    const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

    /// Fixture value; it is not a credential.
    const FIXTURE_VALUE: &str = "keychain-fixture-value";

    /// Shuts the runtime down with a bounded wait on every exit path, so a
    /// lookup stuck on an unlock dialog cannot hold the test process forever.
    struct BoundedRuntime(Option<tokio::runtime::Runtime>);

    impl BoundedRuntime {
        fn new() -> Self {
            Self(Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .expect("runtime"),
            ))
        }

        fn get(&self) -> &tokio::runtime::Runtime {
            self.0.as_ref().expect("runtime is live until drop")
        }
    }

    impl Drop for BoundedRuntime {
        fn drop(&mut self) {
            if let Some(runtime) = self.0.take() {
                runtime.shutdown_timeout(SHUTDOWN_GRACE);
            }
        }
    }

    fn security(arguments: &[&str]) -> std::process::Output {
        Command::new("/usr/bin/security")
            .args(arguments)
            .output()
            .expect("/usr/bin/security runs")
    }

    /// Returns the canonical throwaway keychain path, or `None` when the test
    /// is skipped.
    fn throwaway_keychain() -> Option<PathBuf> {
        let Some(path) = std::env::var_os(KEYCHAIN_ENV) else {
            assert!(
                std::env::var_os(CI_ENV).is_none(),
                "{KEYCHAIN_ENV} must name the throwaway keychain on CI"
            );
            eprintln!("SKIPPED: {KEYCHAIN_ENV} is not set; no real Keychain test ran");
            return None;
        };
        let keychain = Path::new(&path)
            .canonicalize()
            .unwrap_or_else(|error| panic!("{KEYCHAIN_ENV} does not name a file: {error}"));
        let home = std::env::var_os("HOME").expect("HOME is set");
        let home = Path::new(&home).canonicalize().expect("canonical HOME");
        if let Err(reason) = validate_throwaway_keychain(&keychain, &home) {
            panic!("{KEYCHAIN_ENV}: {reason}");
        }
        Some(keychain)
    }

    /// Fails unless `keychain` is the user-domain default keychain.
    fn require_default_keychain(keychain: &Path) {
        let output = security(&["default-keychain", "-d", "user"]);
        assert!(output.status.success(), "security default-keychain failed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let default = PathBuf::from(stdout.trim().trim_matches('"'));
        assert!(
            default
                .canonicalize()
                .is_ok_and(|default| default == keychain),
            "the default keychain is not the keychain named by {KEYCHAIN_ENV}; refusing to write"
        );
    }

    fn run_lookup(
        runtime: &BoundedRuntime,
        source: &KeyringTokenSource,
        key: &str,
    ) -> Result<String, TokenError> {
        runtime.get().block_on(async {
            tokio::time::timeout(LOOKUP_DEADLINE, source.token(key))
                .await
                .expect("keychain lookup did not return within the deadline")
        })
    }

    #[test]
    fn real_keychain_reports_missing_success_and_bounded_locked() {
        let Some(keychain) = throwaway_keychain() else {
            return;
        };
        require_default_keychain(&keychain);
        let keychain_arg = keychain.to_str().expect("UTF-8 keychain path");

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
        let runtime = BoundedRuntime::new();

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

        // Locked, last: lock the keychain that still exists.
        let lock = security(&["lock-keychain", keychain_arg]);
        assert!(lock.status.success(), "security lock-keychain failed");

        // The production bounded path: `Locked` when no unlock UI may be shown,
        // otherwise the caller timeout elapses while the framework waits on
        // the prompt. Nothing may run after this in the process.
        let started = Instant::now();
        let outcome = runtime
            .get()
            .block_on(async { tokio::time::timeout(LOCKED_BOUND, source.token(key)).await });
        assert!(
            started.elapsed() < LOCKED_BOUND + BOUND_SLACK,
            "the locked lookup was not bounded"
        );
        match outcome {
            Err(_elapsed) => {}
            Ok(Err(error)) => {
                assert_eq!(error.kind(), TokenErrorKind::Locked, "{error}");
                assert!(!error.to_string().contains(FIXTURE_VALUE));
            }
            Ok(Ok(_)) => panic!("a locked keychain returned a value"),
        }
    }
}
