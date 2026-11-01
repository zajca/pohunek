//! Host-aware target parsing.
//!
//! Sessions are addressed as `<host>/<session-id>` (or a bare `<session-id>` for
//! the implicit local host). The parser only extracts the two parts; the
//! *effective host* (which selects the transport) is decided by `main::
//! effective_host`, and whether a host string is local is decided by
//! [`is_local_host`]: `local`, an empty name, or this machine's own overlay name
//! as [`prime_local_aliases`] resolved it for the current command line.
//!
//! Grammar:
//! - `s-42`            → local session `s-42` (host `None`, falls back to the
//!   global `--host` flag)
//! - `local/s-42`      → explicit local host, session `s-42`
//! - `host-b/s-42`     → remote host `host-b`, session `s-42`

use std::collections::BTreeSet;
use std::fmt;
use std::sync::{PoisonError, RwLock};

pub(crate) use pohunek_client::LOCAL_HOST;

use crate::error::CliError;

/// This machine's own overlay names among the hosts of the current command
/// line, as the latest [`prime_local_aliases`] resolved them. The set is fixed
/// for the command it was primed for, reconnects included.
static LOCAL_ALIASES: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// Return whether `host` selects the local daemon: `local`, an empty name, or
/// this machine's own overlay name given on this command line.
pub(crate) fn is_local_host(host: &str) -> bool {
    pohunek_client::is_local_host(host)
        || LOCAL_ALIASES
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(host)
}

/// Resolve which of `hosts` name this machine by its own overlay identity, so
/// [`is_local_host`] treats them as `local`. Each call replaces the aliases a
/// previous call resolved.
///
/// Literal `local` never consults an overlay; when every host is local nothing
/// runs. Resolution follows [`pohunek_client::names_this_machine_in`]: a name
/// this machine shares with a peer is the typed ambiguous-host error, and an
/// unknown name stays remote, so its connection reports what it reports today.
///
/// # Errors
///
/// Returns [`CliError`] for an ambiguous or invalid host.
pub(crate) async fn prime_local_aliases<'a>(
    hosts: impl IntoIterator<Item = &'a str>,
) -> Result<(), CliError> {
    let aliases = own_overlay_names(hosts).await?;
    *LOCAL_ALIASES
        .write()
        .unwrap_or_else(PoisonError::into_inner) = aliases;
    Ok(())
}

/// The members of `hosts` that name this machine on a configured overlay.
async fn own_overlay_names<'a>(
    hosts: impl IntoIterator<Item = &'a str>,
) -> Result<BTreeSet<String>, CliError> {
    let candidates = hosts
        .into_iter()
        .filter(|host| !pohunek_client::is_local_host(host))
        .collect::<BTreeSet<_>>();
    let mut aliases = BTreeSet::new();
    if candidates.is_empty() {
        return Ok(aliases);
    }
    let Ok(registry) = pohunek_client::default_overlay_registry() else {
        return Ok(aliases);
    };
    let connect_timeout = pohunek_client::ClientOptions::default().connect_timeout;
    for host in candidates {
        if pohunek_client::names_this_machine_in(host, connect_timeout, &registry)
            .await
            .map_err(crate::client::map_client_error)?
        {
            aliases.insert(host.to_owned());
        }
    }
    Ok(aliases)
}

/// A parsed session target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// `None` for an implicit local target; `Some(host)` for an explicit host
    /// (which may itself be the reserved `local` name).
    pub(crate) host: Option<String>,
    /// The session identifier portion.
    pub(crate) session_id: String,
}

/// Error parsing a target string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TargetParseError {
    /// The input was empty.
    #[error("empty target")]
    Empty,
    /// The session-id portion was empty (e.g. `host/`).
    #[error("missing session id in target '{0}'")]
    MissingSessionId(String),
    /// The host portion was empty (e.g. `/s-42`).
    #[error("missing host in target '{0}'")]
    MissingHost(String),
    /// More than one `/` separator.
    #[error("invalid target '{0}': expected at most one '/' separating host and session id")]
    TooManySeparators(String),
}

impl std::str::FromStr for Target {
    type Err = TargetParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(TargetParseError::Empty);
        }

        let mut parts = s.splitn(3, '/');
        // splitn with limit 3 lets us detect a third segment as "too many".
        let first = parts.next().unwrap_or_default();
        match (parts.next(), parts.next()) {
            // No separator: bare session id, implicit local.
            (None, _) => Ok(Target {
                host: None,
                session_id: first.to_owned(),
            }),
            // One separator: host/session.
            (Some(session), None) => {
                if first.is_empty() {
                    return Err(TargetParseError::MissingHost(s.to_owned()));
                }
                if session.is_empty() {
                    return Err(TargetParseError::MissingSessionId(s.to_owned()));
                }
                Ok(Target {
                    host: Some(first.to_owned()),
                    session_id: session.to_owned(),
                })
            }
            // Two separators: malformed.
            (Some(_), Some(_)) => Err(TargetParseError::TooManySeparators(s.to_owned())),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Some(h) => write!(f, "{h}/{}", self.session_id),
            None => f.write_str(&self.session_id),
        }
    }
}
