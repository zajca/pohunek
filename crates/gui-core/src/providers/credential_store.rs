//! Credential-store lookup behind [`KeyringTokenSource`](super::linear::KeyringTokenSource).
//!
//! One lookup reads one entry from the platform store (macOS Keychain, Linux
//! Secret Service) on a blocking thread. Failures are classified into
//! [`TokenErrorKind`] with fixed, remediation-bearing messages: the token value
//! never reaches an error, and only the entry name (a reference) does.
//!
//! A `get_password` call may block for as long as the platform waits on an
//! unlock prompt, and it cannot be cancelled. The caller's timeout stops the
//! wait but not the blocking thread. A locked keychain blocks every entry, so
//! the whole store has one lookup in flight at most. Further lookups wait for
//! a permit asynchronously: a waiter holds no blocking-pool thread, honors the
//! caller's timeout, and proceeds once the running lookup returns. A lookup
//! stuck on an unlock prompt keeps the permit, so later lookups of the process
//! time out until it ends.

// Rust guideline compliant 2026-10-01

use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;
use tokio::task::JoinError;

use super::linear::{TokenError, TokenErrorKind};

/// `errSecInteractionNotAllowed` in `SecBase.h`: the keychain is locked and no
/// unlock UI may be shown.
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;

/// `errSecUserCanceled` in `SecBase.h`: the user dismissed the unlock prompt.
const ERR_SEC_USER_CANCELED: i32 = -128;

/// `errSecAuthFailed` in `SecBase.h`: the store refused access to the entry.
const ERR_SEC_AUTH_FAILED: i32 = -25293;

/// Kind a Keychain status code stands for, `None` when it has no dedicated
/// meaning.
fn status_kind(code: i32) -> Option<TokenErrorKind> {
    match code {
        ERR_SEC_INTERACTION_NOT_ALLOWED | ERR_SEC_USER_CANCELED => Some(TokenErrorKind::Locked),
        ERR_SEC_AUTH_FAILED => Some(TokenErrorKind::Unavailable),
        _ => None,
    }
}

/// Returns the Keychain status code inside a boxed platform failure.
///
/// The downcast targets `security_framework::base::Error` at the major version
/// keyring links; a mismatch would leave the downcast failing silently, which
/// the `keyring_platform_failure_downcasts_to_locked` test rules out.
#[cfg(target_os = "macos")]
fn platform_failure_status(source: &(dyn std::error::Error + 'static)) -> Option<i32> {
    source
        .downcast_ref::<security_framework::base::Error>()
        .map(|error| error.code())
}

/// Keyring reports no recognizable platform status off macOS: a locked Secret
/// Service collection surfaces as an unavailable store.
#[cfg(not(target_os = "macos"))]
fn platform_failure_status(_source: &(dyn std::error::Error + 'static)) -> Option<i32> {
    None
}

/// Reads `service`/`key` from the platform keyring.
pub(super) fn read_keyring(service: &str, key: &str) -> Result<String, TokenError> {
    let entry = keyring::Entry::new(service, key)
        .map_err(|source| classify_keyring_error(&source, service, key))?;
    entry
        .get_password()
        .map_err(|source| classify_keyring_error(&source, service, key))
}

/// Maps a keyring failure to a classified [`TokenError`].
fn classify_keyring_error(source: &keyring::Error, service: &str, key: &str) -> TokenError {
    match source {
        keyring::Error::NoEntry => not_found(service, key),
        // Backend error text is untrusted and may carry sensitive data, so no
        // branch formats it; only an OSStatus number can appear.
        keyring::Error::PlatformFailure(detail) => {
            let status = platform_failure_status(detail.as_ref());
            match status.and_then(status_kind) {
                Some(TokenErrorKind::Locked) => locked(),
                Some(TokenErrorKind::Unavailable) => denied(),
                _ => unavailable(status),
            }
        }
        keyring::Error::BadEncoding(_) => invalid("the stored value is not valid UTF-8"),
        keyring::Error::TooLong(..) | keyring::Error::Invalid(..) => {
            invalid("the entry name is not accepted by the credential store")
        }
        keyring::Error::Ambiguous(matches) => invalid(&format!(
            "{} credentials match the entry name; keep exactly one",
            matches.len()
        )),
        // `NoStorageAccess` and any future variant: no usable detail.
        _ => unavailable(None),
    }
}

fn not_found(service: &str, key: &str) -> TokenError {
    TokenError::with_kind(
        TokenErrorKind::NotFound,
        format!(
            "no credential named `{key}` in service `{service}`; \
             add an entry with account name `{key}` to that service"
        ),
    )
}

fn locked() -> TokenError {
    TokenError::with_kind(
        TokenErrorKind::Locked,
        "the credential store is locked; unlock the login keychain and retry",
    )
}

fn denied() -> TokenError {
    TokenError::with_kind(
        TokenErrorKind::Unavailable,
        "the credential store refused access to the entry; allow access in Keychain Access \
         or unlock the login keychain with its password, then retry",
    )
}

fn unavailable(status: Option<i32>) -> TokenError {
    let status = status.map_or_else(String::new, |code| format!(" (status {code})"));
    TokenError::with_kind(
        TokenErrorKind::Unavailable,
        format!(
            "the credential store is unavailable{status}; check that the login keychain \
             exists and is accessible (Linux: that a Secret Service provider is running and \
             its collection is unlocked)"
        ),
    )
}

fn invalid(detail: &str) -> TokenError {
    TokenError::with_kind(
        TokenErrorKind::Invalid,
        format!("the credential cannot be used: {detail}"),
    )
}

/// Maps a failed lookup task to an error. Neither the panic payload nor the
/// join error text reaches the message: a payload is arbitrary text.
fn task_failed(error: &JoinError) -> TokenError {
    tracing::error!(
        panicked = error.is_panic(),
        cancelled = error.is_cancelled(),
        "credential lookup task did not complete"
    );
    TokenError::with_kind(
        TokenErrorKind::Other,
        "the credential lookup ended unexpectedly; retry, and report this if it persists",
    )
}

/// Single permit shared by every lookup of the process.
fn store_permit() -> &'static Arc<Semaphore> {
    static PERMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMIT.get_or_init(|| Arc::new(Semaphore::new(1)))
}

/// Runs `read` on a blocking thread as the store's only in-flight lookup.
///
/// The wait for the permit is async, so a caller timeout drops the waiter
/// without it ever occupying a blocking thread. The permit moves into the
/// blocking closure and is released when the read returns, panics, or is
/// dropped unrun at runtime shutdown, never when the caller gives up.
pub(super) async fn lookup<F>(read: F) -> Result<String, TokenError>
where
    F: FnOnce() -> Result<String, TokenError> + Send + 'static,
{
    let permit = Arc::clone(store_permit())
        .acquire_owned()
        .await
        .map_err(|_closed| {
            TokenError::with_kind(
                TokenErrorKind::Other,
                "the credential lookup gate is closed; report this",
            )
        })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        read()
    })
    .await
    .map_err(|error| task_failed(&error))?
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const SERVICE: &str = "pohunek-credential-store-test";
    const SECRET: &str = "secret-fixture-value";

    /// The in-flight flag is process-wide, so tests that call `lookup` take
    /// turns.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn status_kind_maps_locked_and_denied_codes() {
        assert_eq!(status_kind(-25308), Some(TokenErrorKind::Locked));
        assert_eq!(status_kind(-128), Some(TokenErrorKind::Locked));
        assert_eq!(status_kind(-25293), Some(TokenErrorKind::Unavailable));
        assert_eq!(status_kind(-25300), None);
        assert_eq!(status_kind(-25294), None);
    }

    #[test]
    fn no_entry_is_not_found_and_names_the_key() {
        let error = classify_keyring_error(&keyring::Error::NoEntry, SERVICE, "linear-token-ref");
        assert_eq!(error.kind(), TokenErrorKind::NotFound);
        let message = error.to_string();
        assert!(message.contains("linear-token-ref"), "{message}");
        assert!(message.contains("add an entry"), "{message}");
    }

    #[test]
    fn no_storage_access_is_unavailable_and_never_echoes_backend_text() {
        let source = keyring::Error::NoStorageAccess(format!("backend said {SECRET}").into());
        let error = classify_keyring_error(&source, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Unavailable);
        assert!(error.to_string().contains("login keychain"));
        assert_no_secret(&error);
    }

    #[test]
    fn opaque_platform_failure_is_unavailable_and_never_echoes_backend_text() {
        let source = keyring::Error::PlatformFailure(format!("backend said {SECRET}").into());
        let error = classify_keyring_error(&source, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Unavailable);
        assert_no_secret(&error);
    }

    #[test]
    fn unrecognized_keyring_variants_never_echo_stored_data() {
        let source = keyring::Error::BadEncoding(SECRET.as_bytes().to_vec());
        let error = classify_keyring_error(&source, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Invalid);
        assert_no_secret(&error);
    }

    /// The sentinel must not surface in the error nor in the Linear error that
    /// wraps it on its way to the UI and logs.
    fn assert_no_secret(error: &TokenError) {
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
        let wrapped = super::super::linear::LinearError::TokenLookup {
            token_key: "k".to_owned(),
            source: error.clone(),
        };
        assert!(!wrapped.to_string().contains(SECRET));
        assert!(!format!("{wrapped:?}").contains(SECRET));
    }

    #[test]
    fn locked_and_denied_messages_carry_remediation() {
        let locked = locked();
        assert_eq!(locked.kind(), TokenErrorKind::Locked);
        assert!(locked.to_string().contains("unlock the login keychain"));
        let denied = denied();
        assert_eq!(denied.kind(), TokenErrorKind::Unavailable);
        assert!(denied.to_string().contains("allow access"));
    }

    #[test]
    fn unavailable_message_carries_only_the_status_number() {
        let with_status = unavailable(Some(-25295));
        assert!(with_status.to_string().contains("(status -25295)"));
        assert!(!unavailable(None).to_string().contains("status"));
    }

    /// Proves the downcast target matches the `security-framework` type keyring
    /// boxes: `decode_error` only accepts keyring's own version of the type, so
    /// this fails to compile on a major-version mismatch.
    #[cfg(target_os = "macos")]
    #[test]
    fn keyring_platform_failure_downcasts_to_locked() {
        let decoded = keyring::macos::decode_error(security_framework::base::Error::from_code(
            ERR_SEC_INTERACTION_NOT_ALLOWED,
        ));
        assert!(matches!(decoded, keyring::Error::PlatformFailure(_)));
        let error = classify_keyring_error(&decoded, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Locked);
    }

    /// `errSecAuthFailed` surfaces as a denied, unavailable store.
    #[cfg(target_os = "macos")]
    #[test]
    fn keyring_auth_failure_is_unavailable_with_remediation() {
        let decoded = keyring::macos::decode_error(security_framework::base::Error::from_code(
            ERR_SEC_AUTH_FAILED,
        ));
        let error = classify_keyring_error(&decoded, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Unavailable);
        assert!(error.to_string().contains("allow access"));
    }

    /// `errSecNoSuchKeychain` (-25294) surfaces as `NoStorageAccess`.
    #[cfg(target_os = "macos")]
    #[test]
    fn keyring_missing_keychain_is_unavailable() {
        let decoded =
            keyring::macos::decode_error(security_framework::base::Error::from_code(-25294));
        let error = classify_keyring_error(&decoded, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Unavailable);
    }

    const WAITER_PATIENCE: Duration = Duration::from_millis(50);

    /// Closure that reports entry, then waits for the release signal.
    fn gated(
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    ) -> impl FnOnce() -> Result<String, TokenError> + Send + 'static {
        move || {
            entered.send(()).expect("test holds the entered receiver");
            release.recv().expect("test holds the release sender");
            Ok(SECRET.to_owned())
        }
    }

    async fn wait_until_entered(entered: mpsc::Receiver<()>) {
        tokio::task::spawn_blocking(move || entered.recv().expect("lookup enters"))
            .await
            .expect("wait for entry");
    }

    #[tokio::test]
    async fn overlapping_healthy_lookups_both_succeed_in_turn() {
        let _turn = SERIAL.lock().await;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = tokio::spawn(lookup(gated(entered_tx, release_rx)));
        wait_until_entered(entered_rx).await;

        let ran = Arc::new(AtomicUsize::new(0));
        let second_ran = Arc::clone(&ran);
        let mut second = tokio::spawn(lookup(move || {
            second_ran.fetch_add(1, Ordering::SeqCst);
            Ok("second".to_owned())
        }));
        // The second lookup waits for the permit while the first is running.
        tokio::time::timeout(WAITER_PATIENCE, &mut second)
            .await
            .expect_err("the second lookup waits for the permit");
        assert_eq!(ran.load(Ordering::SeqCst), 0);

        release_tx.send(()).expect("release first lookup");
        assert_eq!(first.await.expect("join").expect("first"), SECRET);
        assert_eq!(second.await.expect("join").expect("second"), "second");
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stuck_lookup_makes_waiters_time_out_without_running_or_spawning() {
        let _turn = SERIAL.lock().await;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first = tokio::spawn(lookup(gated(entered_tx, release_rx)));
        wait_until_entered(entered_rx).await;

        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..8 {
            let ran = Arc::clone(&ran);
            tokio::time::timeout(
                WAITER_PATIENCE,
                lookup(move || {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(SECRET.to_owned())
                }),
            )
            .await
            .expect_err("waiter must time out while the permit is held");
        }
        // No waiter ever reached a blocking thread.
        assert_eq!(ran.load(Ordering::SeqCst), 0);

        release_tx.send(()).expect("release first lookup");
        first.await.expect("join").expect("first");
        // Timed-out waiters left no claim behind.
        let again = lookup(|| Ok("after".to_owned())).await.expect("lookup");
        assert_eq!(again, "after");
    }

    #[tokio::test]
    async fn panicking_read_is_reported_without_payload_and_releases_the_permit() {
        let _turn = SERIAL.lock().await;
        let error = lookup(|| -> Result<String, TokenError> { panic!("payload {SECRET}") })
            .await
            .expect_err("panicking read fails");
        assert_eq!(error.kind(), TokenErrorKind::Other);
        assert_no_secret(&error);

        let again = lookup(|| Ok("after-panic".to_owned()))
            .await
            .expect("permit released by the unwinding thread");
        assert_eq!(again, "after-panic");
    }

    #[tokio::test]
    async fn classified_read_errors_pass_through() {
        let _turn = SERIAL.lock().await;
        let error = lookup(|| Err(not_found(SERVICE, "k")))
            .await
            .expect_err("not found");
        assert_eq!(error.kind(), TokenErrorKind::NotFound);
    }
}
