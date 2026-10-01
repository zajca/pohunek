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
//! the whole store has one lookup in flight at most: a further lookup of any
//! key fails fast with [`TokenErrorKind::Timeout`] instead of parking another
//! blocking-pool thread.

// Rust guideline compliant 2026-10-01

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::task::JoinError;

use super::linear::{TokenError, TokenErrorKind};

/// `errSecInteractionNotAllowed` in `SecBase.h`: the keychain is locked and no
/// unlock UI may be shown.
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;

/// `errSecUserCanceled` in `SecBase.h`: the user dismissed the unlock prompt.
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_USER_CANCELED: i32 = -128;

/// `errSecAuthFailed` in `SecBase.h`: the store refused access to the entry.
#[cfg(any(target_os = "macos", test))]
const ERR_SEC_AUTH_FAILED: i32 = -25293;

/// Kind a Keychain status code stands for, `None` when it has no dedicated
/// meaning.
#[cfg(any(target_os = "macos", test))]
fn status_kind(code: i32) -> Option<TokenErrorKind> {
    match code {
        ERR_SEC_INTERACTION_NOT_ALLOWED | ERR_SEC_USER_CANCELED => Some(TokenErrorKind::Locked),
        ERR_SEC_AUTH_FAILED => Some(TokenErrorKind::Unavailable),
        _ => None,
    }
}

/// Classifies a boxed platform failure that keyring reports as
/// `PlatformFailure`.
///
/// The downcast targets `security_framework::base::Error` at the major version
/// keyring links; a mismatch would leave the downcast failing silently, which
/// the `keyring_platform_failure_downcasts_to_locked` test rules out.
#[cfg(target_os = "macos")]
fn platform_failure_kind(source: &(dyn std::error::Error + 'static)) -> Option<TokenErrorKind> {
    source
        .downcast_ref::<security_framework::base::Error>()
        .and_then(|error| status_kind(error.code()))
}

/// Keyring reports no recognizable platform status off macOS: a locked Secret
/// Service collection surfaces as an unavailable store.
#[cfg(not(target_os = "macos"))]
fn platform_failure_kind(_source: &(dyn std::error::Error + 'static)) -> Option<TokenErrorKind> {
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
        keyring::Error::NoStorageAccess(detail) => unavailable(&detail.to_string()),
        keyring::Error::PlatformFailure(detail) => match platform_failure_kind(detail.as_ref()) {
            Some(TokenErrorKind::Locked) => locked(),
            Some(TokenErrorKind::Unavailable) => denied(),
            _ => unavailable(&detail.to_string()),
        },
        keyring::Error::BadEncoding(_) => invalid("the stored value is not valid UTF-8"),
        keyring::Error::TooLong(..) | keyring::Error::Invalid(..) => {
            invalid("the entry name is not accepted by the credential store")
        }
        keyring::Error::Ambiguous(matches) => invalid(&format!(
            "{} credentials match the entry name; keep exactly one",
            matches.len()
        )),
        // `keyring::Error` is non-exhaustive.
        _ => unavailable("unrecognized credential store failure"),
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

fn unavailable(detail: &str) -> TokenError {
    TokenError::with_kind(
        TokenErrorKind::Unavailable,
        format!(
            "the credential store is unavailable ({detail}); check that the login keychain \
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

fn busy(key: &str) -> TokenError {
    TokenError::with_kind(
        TokenErrorKind::Timeout,
        format!(
            "an earlier credential store lookup is still pending, so `{key}` was not read; \
             the store may be waiting for an unlock prompt: unlock the login keychain and retry"
        ),
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

/// Set while a lookup runs on a blocking thread.
static LOOKUP_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Claim on the store's single in-flight lookup. The blocking thread owns it,
/// so it is released when the read returns, panics, or is dropped unrun at
/// runtime shutdown, never when the caller gives up.
struct LookupClaim;

impl LookupClaim {
    fn acquire() -> Option<Self> {
        // Built lazily: dropping a claim releases the flag, which a refused
        // lookup must not do.
        LOOKUP_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then(|| Self)
    }
}

impl Drop for LookupClaim {
    fn drop(&mut self) {
        LOOKUP_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Runs `read` on a blocking thread as the store's only in-flight lookup.
///
/// Fails with [`TokenErrorKind::Timeout`] without spawning a thread while an
/// earlier lookup has not returned. `key` only names the entry in messages.
pub(super) async fn lookup<F>(key: &str, read: F) -> Result<String, TokenError>
where
    F: FnOnce() -> Result<String, TokenError> + Send + 'static,
{
    let Some(claim) = LookupClaim::acquire() else {
        return Err(busy(key));
    };
    tokio::task::spawn_blocking(move || {
        let _claim = claim;
        read()
    })
    .await
    .map_err(|error| task_failed(&error))?
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

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
    fn no_storage_access_is_unavailable_with_remediation() {
        let source = keyring::Error::NoStorageAccess("backend down".into());
        let error = classify_keyring_error(&source, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Unavailable);
        assert!(error.to_string().contains("backend down"));
        assert!(error.to_string().contains("login keychain"));
    }

    #[test]
    fn opaque_platform_failure_is_unavailable() {
        let source = keyring::Error::PlatformFailure("opaque".into());
        let error = classify_keyring_error(&source, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Unavailable);
    }

    #[test]
    fn bad_encoding_never_echoes_the_stored_bytes() {
        let source = keyring::Error::BadEncoding(SECRET.as_bytes().to_vec());
        let error = classify_keyring_error(&source, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Invalid);
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));
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

    #[tokio::test]
    async fn stuck_lookup_refuses_every_key_until_it_returns() {
        let _turn = SERIAL.lock().await;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let first = tokio::spawn(lookup("first", move || {
            entered_tx
                .send(())
                .expect("test holds the entered receiver");
            release_rx.recv().expect("test holds the release sender");
            Ok(SECRET.to_owned())
        }));
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("first lookup enters"))
            .await
            .expect("wait for entry");

        // The first read is stuck: lookups of the same and of other keys fail
        // fast without running their closure.
        let ran = std::sync::Arc::new(AtomicUsize::new(0));
        for key in ["first", "second", "third", "first"] {
            let ran = std::sync::Arc::clone(&ran);
            let error = lookup(key, move || {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok(SECRET.to_owned())
            })
            .await
            .expect_err("busy lookup fails fast");
            assert_eq!(error.kind(), TokenErrorKind::Timeout);
            assert!(error.to_string().contains(key));
            assert!(!error.to_string().contains(SECRET));
        }
        assert_eq!(ran.load(Ordering::SeqCst), 0);

        release_tx.send(()).expect("release first lookup");
        let value = first.await.expect("join").expect("first lookup succeeds");
        assert!(value == SECRET, "unexpected value");

        // The blocking thread dropped its claim before the join completed.
        let again = lookup("second", || Ok("next".to_owned()))
            .await
            .expect("lookup after release");
        assert_eq!(again, "next");
    }

    #[tokio::test]
    async fn panicking_read_is_reported_without_payload_and_releases_the_claim() {
        let _turn = SERIAL.lock().await;
        let error = lookup("k", || -> Result<String, TokenError> {
            panic!("payload {SECRET}")
        })
        .await
        .expect_err("panicking read fails");
        assert_eq!(error.kind(), TokenErrorKind::Other);
        assert!(!error.to_string().contains(SECRET));
        assert!(!format!("{error:?}").contains(SECRET));

        let again = lookup("k", || Ok("after-panic".to_owned()))
            .await
            .expect("claim released by the unwinding thread");
        assert_eq!(again, "after-panic");
    }

    #[tokio::test]
    async fn classified_read_errors_pass_through() {
        let _turn = SERIAL.lock().await;
        let error = lookup("k", || Err(not_found(SERVICE, "k")))
            .await
            .expect_err("not found");
        assert_eq!(error.kind(), TokenErrorKind::NotFound);
    }
}
