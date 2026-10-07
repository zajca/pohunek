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

/// Resolve the remote control port.
///
/// Returns [`DEFAULT_REMOTE_PORT`] when [`REMOTE_PORT_ENV`] is unset. When the
/// variable is set it must parse as a non-zero `u16`: a present-but-invalid
/// value is a configuration error rather than a silent fallback to the default,
/// so a typo in configuration fails loudly.
pub fn remote_port() -> Result<u16, NetbirdError> {
    port_from_lookup(std::env::var(REMOTE_PORT_ENV))
}

/// Resolves the port from the result of looking up [`REMOTE_PORT_ENV`].
///
/// Takes the lookup result as an argument so tests cover every case without
/// changing the process environment.
fn port_from_lookup(lookup: Result<String, std::env::VarError>) -> Result<u16, NetbirdError> {
    match lookup {
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_REMOTE_PORT),
        Err(std::env::VarError::NotUnicode(_)) => Err(NetbirdError::InvalidConfig(format!(
            "invalid {REMOTE_PORT_ENV}: value is not valid Unicode"
        ))),
        Ok(raw) => parse_port(&raw),
    }
}

/// Parse a configured port string into a non-zero `u16`.
///
/// Factored out so it is unit-testable without touching the process environment.
fn parse_port(raw: &str) -> Result<u16, NetbirdError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pohunek_test_support::process_env::ProcessEnv;

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
        for bad in [
            "",
            "   ",
            "0",
            "-1",
            "not-a-number",
            "70000",
            "80.5",
            "18722x",
        ] {
            let err = parse_port(bad).unwrap_err();
            assert!(
                matches!(err, NetbirdError::InvalidConfig(_)),
                "expected InvalidConfig for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn non_unicode_lookup_is_a_configuration_error() {
        let err = port_from_lookup(Err(std::env::VarError::NotUnicode(
            std::ffi::OsString::new(),
        )))
        .unwrap_err();
        assert!(matches!(err, NetbirdError::InvalidConfig(_)));
    }

    #[test]
    fn remote_port_reads_the_process_environment() {
        let mut env = ProcessEnv::lock();
        env.remove(REMOTE_PORT_ENV);
        assert_eq!(remote_port().unwrap(), DEFAULT_REMOTE_PORT);

        env.set(REMOTE_PORT_ENV, "19000");
        assert_eq!(remote_port().unwrap(), 19000);
    }
}
