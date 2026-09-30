//! Credential-store lookup behind [`KeyringTokenSource`](super::linear::KeyringTokenSource).
//!
//! One lookup reads one entry from the platform store (macOS Keychain, Linux
//! Secret Service) on a blocking thread. Failures are classified into
//! [`TokenErrorKind`] with fixed, remediation-bearing messages: the token value
//! never reaches an error, and only the entry name (a reference) does.
//!
//! A `get_password` call may block for as long as the platform waits on an
//! unlock prompt, and it cannot be cancelled. The caller's timeout stops the
//! wait but not the blocking thread, so at most one lookup per
//! `(service, key)` is in flight; a further lookup fails fast instead of
//! parking another thread.

// Rust guideline compliant 2026-09-30

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use super::linear::{TokenError, TokenErrorKind};

/// Reads one credential from a platform store.
pub(super) trait CredentialBackend: Send + Sync + 'static {
    /// Returns the secret stored under `service` and `key`.
    ///
    /// Errors must be classified and must not contain the secret.
    fn read(&self, service: &str, key: &str) -> Result<String, TokenError>;
}

/// [`CredentialBackend`] over the platform keyring.
#[derive(Debug, Clone, Copy)]
pub(super) struct KeyringBackend;

impl CredentialBackend for KeyringBackend {
    fn read(&self, service: &str, key: &str) -> Result<String, TokenError> {
        let entry = keyring::Entry::new(service, key)
            .map_err(|source| classify_keyring_error(&source, service, key))?;
        entry
            .get_password()
            .map_err(|source| classify_keyring_error(&source, service, key))
    }
}

/// Keychain status codes with a dedicated meaning, decoupled from the FFI type
/// so the mapping is testable on every host.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeychainStatus {
    /// `errSecInteractionNotAllowed`: the keychain is locked and no unlock UI
    /// may be shown.
    InteractionNotAllowed,
    /// `errSecUserCanceled`: the user dismissed the unlock prompt.
    UserCanceled,
    /// Any other status.
    Other,
}

#[cfg(any(target_os = "macos", test))]
impl KeychainStatus {
    /// `errSecInteractionNotAllowed` in `SecBase.h`.
    const INTERACTION_NOT_ALLOWED_CODE: i32 = -25308;
    /// `errSecUserCanceled` in `SecBase.h`.
    const USER_CANCELED_CODE: i32 = -128;

    fn from_code(code: i32) -> Self {
        match code {
            Self::INTERACTION_NOT_ALLOWED_CODE => Self::InteractionNotAllowed,
            Self::USER_CANCELED_CODE => Self::UserCanceled,
            _ => Self::Other,
        }
    }

    fn kind(self) -> Option<TokenErrorKind> {
        match self {
            Self::InteractionNotAllowed | Self::UserCanceled => Some(TokenErrorKind::Locked),
            Self::Other => None,
        }
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
        .and_then(|error| KeychainStatus::from_code(error.code()).kind())
}

#[cfg(not(target_os = "macos"))]
fn platform_failure_kind(_source: &(dyn std::error::Error + 'static)) -> Option<TokenErrorKind> {
    None
}

/// Maps a keyring failure to a classified [`TokenError`].
fn classify_keyring_error(source: &keyring::Error, service: &str, key: &str) -> TokenError {
    match source {
        keyring::Error::NoEntry => not_found(service, key),
        keyring::Error::NoStorageAccess(detail) => unavailable(&detail.to_string()),
        keyring::Error::PlatformFailure(detail) => match platform_failure_kind(detail.as_ref()) {
            Some(TokenErrorKind::Locked) => locked(),
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
        "the credential store is locked; unlock the login keychain \
         (Linux: the default Secret Service collection) and retry",
    )
}

fn unavailable(detail: &str) -> TokenError {
    TokenError::with_kind(
        TokenErrorKind::Unavailable,
        format!(
            "the credential store is unavailable ({detail}); check that the login keychain \
             exists and is accessible (Linux: that a Secret Service provider is running)"
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
            "an earlier lookup of `{key}` is still waiting on the credential store; \
             it may be waiting for an unlock prompt: unlock the login keychain and retry"
        ),
    )
}

/// `(service, key)` pairs with a lookup running on a blocking thread.
static IN_FLIGHT: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);

/// Claim on one in-flight lookup, released when the blocking thread finishes.
struct InFlight {
    slot: (String, String),
}

impl InFlight {
    fn acquire(service: &str, key: &str) -> Option<Self> {
        let slot = (service.to_owned(), key.to_owned());
        // The claim is built only when the slot was free: dropping a claim
        // releases the slot, which a busy lookup must not do.
        let mut set = IN_FLIGHT.lock().unwrap_or_else(PoisonError::into_inner);
        set.get_or_insert_with(HashSet::new)
            .insert(slot.clone())
            .then(|| Self { slot })
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut set = IN_FLIGHT.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(set) = set.as_mut() {
            set.remove(&self.slot);
        }
    }
}

/// Reads `service`/`key` through `backend` on a blocking thread.
///
/// Fails with [`TokenErrorKind::Timeout`] without spawning a thread while an
/// earlier lookup of the same entry has not returned.
pub(super) async fn lookup(
    backend: Arc<dyn CredentialBackend>,
    service: String,
    key: String,
) -> Result<String, TokenError> {
    let Some(claim) = InFlight::acquire(&service, &key) else {
        return Err(busy(&key));
    };
    tokio::task::spawn_blocking(move || {
        let _claim = claim;
        backend.read(&service, &key)
    })
    .await
    .map_err(|_join_error| unavailable("the lookup task did not complete"))?
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    const SERVICE: &str = "pohunek-credential-store-test";
    const SECRET: &str = "secret-fixture-value";
    /// Upper bound on yields while waiting for a blocking thread to drop its
    /// claim; far above what one thread hand-off needs.
    const CLAIM_RELEASE_POLLS: usize = 1_000_000;

    #[test]
    fn keychain_status_maps_locked_codes() {
        assert_eq!(
            KeychainStatus::from_code(-25308).kind(),
            Some(TokenErrorKind::Locked)
        );
        assert_eq!(
            KeychainStatus::from_code(-128).kind(),
            Some(TokenErrorKind::Locked)
        );
        assert_eq!(KeychainStatus::from_code(-25300).kind(), None);
        assert_eq!(KeychainStatus::from_code(-25294).kind(), None);
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
    fn locked_message_tells_the_user_to_unlock() {
        let error = locked();
        assert_eq!(error.kind(), TokenErrorKind::Locked);
        assert!(error.to_string().contains("unlock the login keychain"));
    }

    /// Proves the downcast target matches the `security-framework` type keyring
    /// boxes: `decode_error` only accepts keyring's own version of the type, so
    /// this fails to compile on a major-version mismatch.
    #[cfg(target_os = "macos")]
    #[test]
    fn keyring_platform_failure_downcasts_to_locked() {
        let decoded = keyring::macos::decode_error(security_framework::base::Error::from_code(
            KeychainStatus::INTERACTION_NOT_ALLOWED_CODE,
        ));
        assert!(matches!(decoded, keyring::Error::PlatformFailure(_)));
        let error = classify_keyring_error(&decoded, SERVICE, "k");
        assert_eq!(error.kind(), TokenErrorKind::Locked);
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

    fn shared(backend: &Arc<GatedBackend>) -> Arc<dyn CredentialBackend> {
        Arc::clone(backend) as Arc<dyn CredentialBackend>
    }

    /// Backend whose reads report entry and then wait for a release signal.
    struct GatedBackend {
        entered: Mutex<mpsc::Sender<()>>,
        release: Mutex<mpsc::Receiver<()>>,
        reads: Mutex<usize>,
    }

    impl CredentialBackend for GatedBackend {
        fn read(&self, _service: &str, _key: &str) -> Result<String, TokenError> {
            *self.reads.lock().unwrap_or_else(PoisonError::into_inner) += 1;
            // Later reads happen after the test stopped listening.
            let _ = self
                .entered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(());
            self.release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv()
                .expect("test holds the release sender");
            Ok(SECRET.to_owned())
        }
    }

    #[tokio::test]
    async fn stuck_lookup_does_not_accumulate_blocking_threads() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let backend = Arc::new(GatedBackend {
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            reads: Mutex::new(0),
        });
        let key = "gated-key".to_owned();

        let first = tokio::spawn(lookup(shared(&backend), SERVICE.to_owned(), key.clone()));
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("first lookup enters"))
            .await
            .expect("wait for entry");

        // The first read is stuck: repeated lookups must fail fast without
        // reaching the backend.
        for _ in 0..8 {
            let error = lookup(shared(&backend), SERVICE.to_owned(), key.clone())
                .await
                .expect_err("busy lookup fails fast");
            assert_eq!(error.kind(), TokenErrorKind::Timeout);
            assert!(error.to_string().contains(&key));
            assert!(!error.to_string().contains(SECRET));
        }
        assert_eq!(*backend.reads.lock().expect("reads"), 1);

        release_tx.send(()).expect("release first lookup");
        let value = first.await.expect("join").expect("first lookup succeeds");
        assert!(value == SECRET, "unexpected value");

        // Once the blocking thread returned, the entry can be looked up again.
        release_tx.send(()).expect("pre-release second lookup");
        let again = lookup(shared(&backend), SERVICE.to_owned(), key)
            .await
            .expect("lookup after release");
        assert!(again == SECRET, "unexpected value");
        assert_eq!(*backend.reads.lock().expect("reads"), 2);
    }

    #[tokio::test]
    async fn dropped_caller_keeps_the_claim_until_the_thread_returns() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let backend = Arc::new(GatedBackend {
            entered: Mutex::new(entered_tx),
            release: Mutex::new(release_rx),
            reads: Mutex::new(0),
        });
        let key = "dropped-key".to_owned();

        let first = tokio::spawn(lookup(shared(&backend), SERVICE.to_owned(), key.clone()));
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("first lookup enters"))
            .await
            .expect("wait for entry");
        // A caller timeout drops the future; the blocking read keeps running.
        first.abort();
        let _ = first.await;

        let error = lookup(shared(&backend), SERVICE.to_owned(), key.clone())
            .await
            .expect_err("claim outlives the aborted caller");
        assert_eq!(error.kind(), TokenErrorKind::Timeout);

        release_tx.send(()).expect("release the stuck read");
        // The blocking thread drops the claim right after the read returns, an
        // instant the test cannot observe; yield until the entry frees.
        let mut freed = false;
        for _ in 0..CLAIM_RELEASE_POLLS {
            let _ = release_tx.send(());
            if lookup(shared(&backend), SERVICE.to_owned(), key.clone())
                .await
                .is_ok()
            {
                freed = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(freed, "the claim was never released");
    }

    #[tokio::test]
    async fn distinct_entries_do_not_block_each_other() {
        struct Immediate;
        impl CredentialBackend for Immediate {
            fn read(&self, _service: &str, key: &str) -> Result<String, TokenError> {
                Ok(format!("value-for-{key}"))
            }
        }
        let backend: Arc<dyn CredentialBackend> = Arc::new(Immediate);
        let a = lookup(
            Arc::clone(&backend),
            SERVICE.to_owned(),
            "distinct-a".to_owned(),
        );
        let b = lookup(backend, SERVICE.to_owned(), "distinct-b".to_owned());
        let (a, b) = tokio::join!(a, b);
        assert_eq!(a.expect("a"), "value-for-distinct-a");
        assert_eq!(b.expect("b"), "value-for-distinct-b");
    }
}
