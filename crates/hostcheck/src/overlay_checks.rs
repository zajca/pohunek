//! Doctor checks for the configured overlay providers.
//!
//! The checks are generic over the overlay id: a caller probes its configured
//! `OverlayRegistry` and passes the [`OverlayDiagnostic`]s in, so a new provider
//! gets its checks without a change here.
//!
//! Two kinds of check come out of one diagnostic:
//!
//! - `<overlay>_cli` on every host that runs `pohunek doctor` or `daemon.doctor`:
//!   whether the provider's CLI is usable and reports a local address;
//! - `overlay_<overlay>_listener` from the daemon only: the overlay the running
//!   daemon was configured with, its port and the listener address its provider
//!   reports. The CLI reads the overlay ids and ports back with
//!   [`reported_overlays`] to compare them with its own `overlays.toml`.

use overlay::{OverlayDiagnostic, OverlayError};
use protocol::{DoctorCheck, DoctorStatus};

// Rust guideline compliant 2026-10-23

/// Suffix of the per-overlay provider CLI check name.
const CLI_CHECK_SUFFIX: &str = "_cli";

/// Prefix of the per-overlay listener check name.
const LISTENER_CHECK_PREFIX: &str = "overlay_";

/// Suffix of the per-overlay listener check name.
const LISTENER_CHECK_SUFFIX: &str = "_listener";

/// Start of a listener check detail; the configured port follows up to `;`.
const LISTENER_PORT_PREFIX: &str = "port=";

/// The configured overlays a doctor probe reports on.
#[derive(Debug, Clone, Copy)]
pub struct OverlayChecks<'a> {
    /// One diagnostic per configured overlay.
    pub diagnostics: &'a [OverlayDiagnostic],
    /// Whether the overlays are listed in `overlays.toml`. An overlay the owner
    /// listed is required, so its missing CLI fails; the `NetBird` default of an
    /// absent file is optional, so its missing CLI only warns.
    pub from_file: bool,
}

impl OverlayChecks<'static> {
    /// No overlay is probed.
    pub const NONE: Self = Self {
        diagnostics: &[],
        from_file: false,
    };
}

/// An overlay a daemon reported through its listener checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedOverlay {
    /// Overlay id.
    pub id: String,
    /// Port the daemon was configured with, when the detail carried one.
    pub port: Option<u16>,
}

/// One `<overlay>_cli` check per configured overlay.
///
/// A missing CLI fails for an overlay listed in `overlays.toml` and warns for
/// the default overlay of an absent file. An unreadable provider state or a
/// missing local address is a warning: the provider may simply not be logged in
/// yet.
#[must_use]
pub fn cli_checks(overlays: OverlayChecks<'_>) -> Vec<DoctorCheck> {
    overlays
        .diagnostics
        .iter()
        .map(|diagnostic| cli_check(diagnostic, overlays.from_file))
        .collect()
}

/// One `overlay_<overlay>_listener` check per configured overlay.
///
/// The detail starts with `port=<n>` and then states the listener address the
/// provider reports. It describes the provider's state, not whether the daemon
/// has bound the address yet.
#[must_use]
pub fn listener_checks(overlays: OverlayChecks<'_>) -> Vec<DoctorCheck> {
    overlays.diagnostics.iter().map(listener_check).collect()
}

/// The overlays a daemon reported, read back from its doctor checks.
///
/// Empty when the daemon reports none, which an older daemon does not.
#[must_use]
pub fn reported_overlays(checks: &[DoctorCheck]) -> Vec<ReportedOverlay> {
    checks
        .iter()
        .filter_map(|check| {
            let id = check
                .name
                .strip_prefix(LISTENER_CHECK_PREFIX)?
                .strip_suffix(LISTENER_CHECK_SUFFIX)?;
            if id.is_empty() {
                return None;
            }
            let port = check
                .detail
                .strip_prefix(LISTENER_PORT_PREFIX)
                .and_then(|rest| rest.split(';').next())
                .and_then(|digits| digits.parse::<u16>().ok());
            Some(ReportedOverlay {
                id: id.to_owned(),
                port,
            })
        })
        .collect()
}

fn cli_check(diagnostic: &OverlayDiagnostic, from_file: bool) -> DoctorCheck {
    let id = diagnostic.overlay.as_str();
    let name = format!("{id}{CLI_CHECK_SUFFIX}");
    if let Some(listener) = diagnostic.listener {
        return DoctorCheck::new(
            name,
            DoctorStatus::Ok,
            format!("found; this host's {id} address is {}", listener.ip()),
        );
    }
    match &diagnostic.error {
        Some(OverlayError::CliMissing(_)) if from_file => DoctorCheck::new(
            name,
            DoctorStatus::Fail,
            format!(
                "the {id} CLI is not installed or not trusted, but overlays.toml configures \
                 this overlay; install it, or remove {id} from providers in overlays.toml and \
                 restart the daemon"
            ),
        ),
        Some(OverlayError::CliMissing(_)) => DoctorCheck::new(
            name,
            DoctorStatus::Warn,
            format!(
                "the {id} CLI is not installed or not trusted; {id} is the default overlay and \
                 optional (remote hosts need it)"
            ),
        ),
        Some(OverlayError::ListenerAddressMissing {
            reason: Some(reason),
            ..
        }) => DoctorCheck::new(
            name,
            DoctorStatus::Warn,
            format!("the {id} CLI answered, but no {id} address can be used: {reason}"),
        ),
        Some(OverlayError::ListenerAddressMissing { reason: None, .. }) => DoctorCheck::new(
            name,
            DoctorStatus::Warn,
            format!("the {id} CLI answered, but no {id} address resolved (not logged in or daemon down)"),
        ),
        Some(OverlayError::StateUnavailable { detail, .. }) => DoctorCheck::new(
            name,
            DoctorStatus::Warn,
            format!("local {id} state is unavailable: {detail}"),
        ),
        Some(error) => DoctorCheck::new(
            name,
            DoctorStatus::Warn,
            format!("local {id} state is unavailable: {error}"),
        ),
        None => DoctorCheck::new(
            name,
            DoctorStatus::Warn,
            format!("the {id} provider reported no address"),
        ),
    }
}

fn listener_check(diagnostic: &OverlayDiagnostic) -> DoctorCheck {
    let id = diagnostic.overlay.as_str();
    let name = format!("{LISTENER_CHECK_PREFIX}{id}{LISTENER_CHECK_SUFFIX}");
    let port = diagnostic.port;
    match (&diagnostic.listener, &diagnostic.error) {
        (Some(listener), _) => DoctorCheck::new(
            name,
            DoctorStatus::Ok,
            format!("{LISTENER_PORT_PREFIX}{port}; the {id} provider reports listener address {listener}"),
        ),
        (None, Some(error)) => DoctorCheck::new(
            name,
            DoctorStatus::Warn,
            format!("{LISTENER_PORT_PREFIX}{port}; the {id} provider reports no listener address: {error}"),
        ),
        (None, None) => DoctorCheck::new(
            name,
            DoctorStatus::Warn,
            format!("{LISTENER_PORT_PREFIX}{port}; the {id} provider reports no listener address"),
        ),
    }
}
