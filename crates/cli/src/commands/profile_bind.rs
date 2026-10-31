//! The profile bind that `pohunek setup` and `pohunek plugin profile migrate`
//! share. The caller names the pin it read, so a daemon that checks it refuses
//! the bind when the profile was pinned since; a daemon that cannot check it is
//! bound without the condition.

use protocol::{
    method, ExpectedPin, PackageBindProfileParams, PackageBindProfileResult, PackageDigest,
};

use crate::client::Client;
use crate::error::CliError;

/// Note printed when the daemon cannot check the pin a bind replaces.
pub(crate) const UNCHECKED_PIN_NOTE: &str = "note: this daemon cannot check the pin the profile \
     held when it was read, so a pin made since is overwritten by this bind";

/// Code of the daemon's refusal of a bind whose expected pin is not the
/// profile's current one.
const PIN_CHANGED_CODE: &str = "package_profile_pin_changed";

/// Whether the daemon refuses a profile bind whose expected pin is not the
/// profile's current one.
///
/// An older daemon rejects the unknown parameter, so the condition is sent only
/// to a host that advertises it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PinCheck {
    daemon_checks: bool,
}

impl PinCheck {
    /// Asks the daemon which binds it can check.
    ///
    /// # Errors
    ///
    /// Returns the error of the `host.inspect` call.
    pub(crate) async fn probe(client: &mut Client) -> Result<Self, CliError> {
        let capabilities = client
            .host_inspect_with(protocol::HostInspectParams {
                skip_runtime_probes: Some(true),
            })
            .await?;
        Ok(Self {
            daemon_checks: capabilities.package_bind_profile_expected_supported,
        })
    }

    /// Whether the daemon checks the previous pin of a bind.
    pub(crate) fn daemon_checks(self) -> bool {
        self.daemon_checks
    }
}

/// Binds `profile` to the package `digest`, expecting it to hold `previous`
/// (none for an unpinned profile) when the daemon can check that.
///
/// # Errors
///
/// Returns the daemon's refusal, `package_profile_pin_changed` when the
/// profile holds another pin than `previous`.
pub(crate) async fn bind(
    client: &mut Client,
    check: PinCheck,
    profile: &str,
    digest: PackageDigest,
    previous: Option<PackageDigest>,
) -> Result<PackageBindProfileResult, CliError> {
    let expected_previous = check.daemon_checks.then_some(match previous {
        Some(digest) => ExpectedPin::Pinned { digest },
        None => ExpectedPin::Unpinned,
    });
    client
        .call::<method::PackageBindProfile>(PackageBindProfileParams {
            expected_previous,
            profile: profile.to_owned(),
            digest: Some(digest),
            dry_run: false,
        })
        .await
}

/// Whether `error` is the daemon's refusal of a profile pinned since it was read.
pub(crate) fn is_pin_changed(error: &CliError) -> bool {
    error.to_protocol_error().code == PIN_CHANGED_CODE
}
