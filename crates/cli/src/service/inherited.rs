//! The transaction lock a `pohunek service lock` ancestor holds for its command.
//!
//! `pohunek service lock -- <command>` keeps the transaction lock itself and
//! never hands its locked descriptor to `<command>`: a descendant that
//! outlives the holder could otherwise keep the lock forever. Instead it
//! records itself and a fresh random token in an owner-private holder record
//! next to the lock (see [`super::record::Store::hand_off`]) and passes only
//! the token, in [`LOCK_TOKEN_ENV`].
//!
//! Every `pohunek` process captures that variable once, at the start of
//! `main` ([`capture`]), before any other thread exists, and removes it from
//! its environment, so no process it spawns sees it. `pohunek service
//! install|upgrade|uninstall|check|lock` then adopt the holder's lock with the
//! captured token ([`super::record::Store::adopt`]) instead of taking a new
//! one. A token that does not prove a live holder fails those commands; they
//! never fall back to a lock of their own, because the ancestor that set the
//! variable relies on its lock covering them.

// Rust guideline compliant 2026-09-29

use std::sync::{Mutex, PoisonError};

use subtle::ConstantTimeEq as _;

use super::error::Error;

/// Environment variable carrying the holder's token to its command.
///
/// Set by `pohunek service lock` for its child and read by every `pohunek`
/// the child runs. The value is [`TOKEN_BYTES`] random bytes in lowercase hex.
pub const LOCK_TOKEN_ENV: &str = "POHUNEK_SERVICE_LOCK_TOKEN";

/// Random bytes in one token.
///
/// 32 bytes (256 bits) cannot be guessed; the token only has to be
/// unforgeable by another process of the same user that did not inherit it.
pub const TOKEN_BYTES: usize = 32;

/// A holder token, compared in constant time.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// Draws a fresh random token.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the system random source fails.
    pub fn generate() -> Result<Self, Error> {
        use std::fmt::Write as _;

        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::getrandom(&mut bytes).map_err(|source| Error::Io {
            operation: "draw a transaction lock token",
            path: std::path::PathBuf::new(),
            source: std::io::Error::other(source.to_string()),
        })?;
        let mut text = String::with_capacity(TOKEN_BYTES * 2);
        for byte in bytes {
            let _ = write!(text, "{byte:02x}");
        }
        Ok(Self(text))
    }

    /// Parses a token in its canonical lowercase hex form.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InheritedLock`] for any other value.
    pub fn parse(value: &str) -> Result<Self, Error> {
        let canonical = value.len() == TOKEN_BYTES * 2
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if canonical {
            Ok(Self(value.to_owned()))
        } else {
            Err(Error::InheritedLock {
                detail: format!("{LOCK_TOKEN_ENV} is not a transaction lock token"),
            })
        }
    }

    /// Returns the canonical hex form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `other` is the same token, in time independent of the contents.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.0.as_bytes().ct_eq(other.0.as_bytes()).into()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

/// What [`capture`] found in the environment.
#[derive(Debug)]
enum Captured {
    /// The variable was not set, or [`take`] already consumed the capture.
    Absent,
    /// The variable held this token.
    Token(Token),
    /// The variable was set to something other than a token.
    Invalid,
}

/// The capture of this process, consumed by the first [`take`].
static CAPTURED: Mutex<Captured> = Mutex::new(Captured::Absent);

/// Captures the holder token and removes [`LOCK_TOKEN_ENV`] from the environment.
///
/// Call it as the first statement of `main`, before the async runtime or any
/// other thread starts: changing the environment then races no reader. An
/// empty value counts as unset; any other value that is not a token is
/// reported by the first command that needs the lock.
pub fn capture() {
    let Some(value) = std::env::var_os(LOCK_TOKEN_ENV) else {
        return;
    };
    std::env::remove_var(LOCK_TOKEN_ENV);
    if value.is_empty() {
        return;
    }
    let captured = value
        .to_str()
        .and_then(|value| Token::parse(value).ok())
        .map_or(Captured::Invalid, Captured::Token);
    *CAPTURED.lock().unwrap_or_else(PoisonError::into_inner) = captured;
}

/// Returns the captured token, if [`capture`] found one; later calls see none.
///
/// # Errors
///
/// Returns [`Error::InheritedLock`] when [`LOCK_TOKEN_ENV`] was set to
/// something other than a token.
pub(crate) fn take() -> Result<Option<Token>, Error> {
    let captured = std::mem::replace(
        &mut *CAPTURED.lock().unwrap_or_else(PoisonError::into_inner),
        Captured::Absent,
    );
    match captured {
        Captured::Absent => Ok(None),
        Captured::Token(token) => Ok(Some(token)),
        Captured::Invalid => Err(Error::InheritedLock {
            detail: format!("{LOCK_TOKEN_ENV} is not a transaction lock token"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_canonical_distinct_and_redacted() {
        let first = Token::generate().expect("token");
        let second = Token::generate().expect("token");
        assert_eq!(first.as_str().len(), TOKEN_BYTES * 2);
        assert_eq!(Token::parse(first.as_str()).expect("parses"), first);
        assert!(first.matches(&first.clone()));
        assert!(!first.matches(&second));
        assert_eq!(format!("{first:?}"), "Token(..)");
    }

    #[test]
    fn only_canonical_hex_tokens_parse() {
        let valid = "0123456789abcdef".repeat(4);
        Token::parse(&valid).expect("canonical");
        for rejected in [
            String::new(),
            valid.to_uppercase(),
            valid[1..].to_owned(),
            format!("{valid}0"),
            "g".repeat(TOKEN_BYTES * 2),
        ] {
            let error = Token::parse(&rejected).expect_err("rejected");
            assert_eq!(error.code(), "service_inherited_lock_invalid");
            assert!(!error.to_string().contains(&rejected) || rejected.is_empty());
        }
    }

    #[test]
    fn take_reports_an_invalid_capture_once_and_then_nothing() {
        *CAPTURED.lock().expect("capture") = Captured::Invalid;
        let error = take().expect_err("invalid capture");
        assert_eq!(error.code(), "service_inherited_lock_invalid");
        assert!(take().expect("consumed").is_none());
    }
}
