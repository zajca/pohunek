//! Resolving the remote control port from the environment.

use crate::status::NetbirdError;

/// Default TCP port the daemon's remote control listener binds on over `NetBird`.
///
/// Override with the [`REMOTE_PORT_ENV`] environment variable. Chosen below the
/// Linux ephemeral range (`32768`+) so it does not collide with outbound
/// ephemeral source ports.
pub const DEFAULT_REMOTE_PORT: u16 = 18722;

/// Environment variable that overrides [`DEFAULT_REMOTE_PORT`].
pub const REMOTE_PORT_ENV: &str = "POHUNEK_REMOTE_PORT";

// Documented invariants, checked at compile time: the default port is non-zero
// and below the Linux default ephemeral floor (32768).
const _: () = assert!(DEFAULT_REMOTE_PORT > 0);
const _: () = assert!(DEFAULT_REMOTE_PORT < 32768);

/// Resolve the remote control port.
///
/// Returns [`DEFAULT_REMOTE_PORT`] when [`REMOTE_PORT_ENV`] is unset.
///
/// When the variable is set it must parse as a non-zero `u16`: a present-but-
/// invalid value is a configuration error rather than a silent fallback to the
/// default, so a typo in configuration fails loudly. A value that is not valid
/// Unicode is the same configuration error, not a panicking lookup.
///
/// # Errors
///
/// Returns [`NetbirdError::InvalidConfig`] for a set-but-unusable value of
/// [`REMOTE_PORT_ENV`].
pub fn remote_port() -> Result<u16, NetbirdError> {
    match std::env::var(REMOTE_PORT_ENV) {
        Ok(raw) => {
            // Surrounding whitespace is tolerated, mirroring how operators
            // quote values in shell configuration.
            let trimmed = raw.trim();
            let invalid = || {
                NetbirdError::InvalidConfig(format!(
                    "invalid {REMOTE_PORT_ENV}={raw:?}: expected a port number in 1..=65535"
                ))
            };
            let port: u16 = trimmed.parse().ok().ok_or_else(invalid)?;
            if port == 0 {
                return Err(invalid());
            }
            Ok(port)
        }
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_REMOTE_PORT),
        Err(std::env::VarError::NotUnicode(_)) => Err(NetbirdError::InvalidConfig(format!(
            "invalid {REMOTE_PORT_ENV}: value is not valid Unicode"
        ))),
    }
}
