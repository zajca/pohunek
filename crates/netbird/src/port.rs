//! Resolving the remote control port from the environment.

use crate::status::NetbirdError;

/// Default TCP port the daemon's remote control listener binds on over NetBird.
///
/// Override with the [`REMOTE_PORT_ENV`] environment variable. Chosen below the
/// Linux ephemeral range (`32768`+) so it does not collide with outbound
/// ephemeral source ports.
pub const DEFAULT_REMOTE_PORT: u16 = 18722;

/// Environment variable that overrides [`DEFAULT_REMOTE_PORT`].
pub const REMOTE_PORT_ENV: &str = "ZAGENTMESH_REMOTE_PORT";

/// Resolve the remote control port.
///
/// Returns [`DEFAULT_REMOTE_PORT`] when [`REMOTE_PORT_ENV`] is unset. When the
/// variable is set it must parse as a non-zero `u16`: a present-but-invalid
/// value is an error ([`NetbirdError::StateUnavailable`]) rather than a silent
/// fallback to the default, so a typo in configuration fails loudly.
pub fn remote_port() -> Result<u16, NetbirdError> {
    match std::env::var(REMOTE_PORT_ENV) {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_REMOTE_PORT),
        Err(std::env::VarError::NotUnicode(_)) => Err(NetbirdError::StateUnavailable(format!(
            "invalid {REMOTE_PORT_ENV}: value is not valid Unicode"
        ))),
        Ok(raw) => parse_port(&raw),
    }
}

/// Parse a configured port string into a non-zero `u16`.
///
/// Factored out so it is unit-testable without mutating process environment
/// (which would race parallel tests).
fn parse_port(raw: &str) -> Result<u16, NetbirdError> {
    let trimmed = raw.trim();
    let invalid = || {
        NetbirdError::StateUnavailable(format!(
            "invalid {REMOTE_PORT_ENV}={raw:?}: expected a port number in 1..=65535"
        ))
    };
    let port: u16 = trimmed.parse().map_err(|_| invalid())?;
    if port == 0 {
        return Err(invalid());
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Documented invariants, checked at compile time: the default port is
    // non-zero and below the Linux default ephemeral floor (32768).
    const _: () = assert!(DEFAULT_REMOTE_PORT > 0);
    const _: () = assert!(DEFAULT_REMOTE_PORT < 32768);

    #[test]
    fn parses_valid_port() {
        assert_eq!(parse_port("18722").unwrap(), 18722);
        assert_eq!(parse_port("1").unwrap(), 1);
        assert_eq!(parse_port("65535").unwrap(), 65535);
        // Surrounding whitespace is tolerated.
        assert_eq!(parse_port("  9000 ").unwrap(), 9000);
    }

    #[test]
    fn rejects_invalid_port_values() {
        for bad in ["", "   ", "0", "-1", "not-a-number", "70000", "80.5", "18722x"] {
            let err = parse_port(bad).unwrap_err();
            assert!(
                matches!(err, NetbirdError::StateUnavailable(_)),
                "expected StateUnavailable for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn remote_port_with_unset_env_returns_default() {
        // `remote_port()` reads process env, so guard the variable across the
        // call. SAFETY note: env mutation is process-global; this test does not
        // run in parallel with another that touches REMOTE_PORT_ENV (no other
        // test mutates it — they exercise the pure `parse_port` helper).
        let previous = std::env::var_os(REMOTE_PORT_ENV);
        std::env::remove_var(REMOTE_PORT_ENV);
        let resolved = remote_port();
        // Restore before asserting so a failure cannot leak env into siblings.
        match previous {
            Some(value) => std::env::set_var(REMOTE_PORT_ENV, value),
            None => std::env::remove_var(REMOTE_PORT_ENV),
        }
        assert_eq!(resolved.unwrap(), DEFAULT_REMOTE_PORT);
    }
}
