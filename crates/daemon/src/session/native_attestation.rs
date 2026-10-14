//! Attests the caller of `session.report_native_id` against the session's PTY root.
//!
//! The control socket carries no identity of its own, so a process that can
//! reach it and knows the root's pid and start identity could otherwise replace
//! the session's native reference. A report is accepted only from the process it
//! names, read from the kernel peer of the connection.

// Rust guideline compliant 2026-10-13

use std::os::fd::AsFd;

use pohunek_platform::peer;
use protocol::{ErrorClass, ProtocolError, SessionId, SessionReportNativeIdParams};
use tracing::{debug, warn};

use crate::procwatch::{HostInspector, ProcessIdentity, ProcessInspector};

/// Wire code of a native-id report the daemon refuses because the caller is
/// not attested as the session's PTY root.
pub(crate) const UNATTESTED_CODE: &str = "native_report_unattested";

/// Fixed message of the refusal. It names no request value, so every refusal
/// answers the same way whatever the caller claimed.
const UNATTESTED_MESSAGE: &str =
    "native-id report refused: the caller is not the session's PTY root";

/// Recovery hint of the refusal.
const UNATTESTED_RECOVER: &str =
    "report the native reference through the worker hook socket from the launch process";

/// The rule a refused report failed.
///
/// The codes are the only detail that reaches a log, so they stay payload-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeReportRefusal {
    /// The connection carries no attested peer: a remote connection, an
    /// in-process caller, or a peer the kernel could not attest.
    NoPeer,
    /// The peer runs under a user other than the daemon's.
    PeerUid,
    /// The peer is not the process the report names, or its start identity
    /// differs from the one the report carries.
    PeerNotRoot,
    /// The kernel now reports a different peer, or the process has exited or
    /// been replaced since the connection was accepted.
    PeerIdentityChanged,
    /// The session's hook schema takes its launch identity only from the worker.
    FrozenRootSchema,
}

impl NativeReportRefusal {
    /// The stable reason code of the refusal.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::NoPeer => "no_peer",
            Self::PeerUid => "peer_uid",
            Self::PeerNotRoot => "peer_not_root",
            Self::PeerIdentityChanged => "peer_identity_changed",
            Self::FrozenRootSchema => "frozen_root_schema",
        }
    }
}

/// Kernel-attested identity of the process on the other end of one control
/// connection.
///
/// The connection-time owner is frozen by the kernel. The process id and its
/// start identity are read once when the connection is accepted, and the
/// attestation re-reads them before each decision that depends on them.
#[derive(Debug)]
pub struct ControlPeer {
    binding: peer::Binding,
    identity: ProcessIdentity,
    /// The test peer's end of the socket pair, kept open so the kernel keeps
    /// attesting the pair. A test peer is not a process the daemon can recheck.
    #[cfg(test)]
    _client_end: Option<std::os::unix::net::UnixStream>,
    /// Whether [`ControlPeer::attest`] rechecks the operating-system record.
    #[cfg(test)]
    recheck_process: bool,
}

impl ControlPeer {
    /// Captures the peer of an accepted control connection.
    ///
    /// Returns `None` when the kernel cannot attest the socket, or the process
    /// it names is not running. The report method then refuses the connection
    /// with [`NativeReportRefusal::NoPeer`]; no other method depends on it.
    #[must_use]
    pub fn capture(socket: &impl AsFd) -> Option<Self> {
        let binding = match peer::Binding::capture(socket) {
            Ok(binding) => binding,
            Err(error) => {
                debug!(
                    name: "session.native_report.peer_unattestable",
                    error = %error,
                    "control peer is not attestable"
                );
                return None;
            }
        };
        let identity = live_identity(binding.snapshot().pid)?;
        Some(Self {
            binding,
            identity,
            #[cfg(test)]
            _client_end: None,
            #[cfg(test)]
            recheck_process: true,
        })
    }

    /// Requires the caller to be the process `params` names, as the daemon's user.
    ///
    /// The kernel peer is re-read here, so a socket handed to another process
    /// after the connection was accepted is refused.
    ///
    /// # Errors
    ///
    /// Returns the first rule the caller fails.
    pub(crate) fn attest(
        &self,
        params: &SessionReportNativeIdParams,
    ) -> Result<(), NativeReportRefusal> {
        let daemon_uid = rustix::process::geteuid().as_raw();
        if self.binding.snapshot().uid != daemon_uid {
            return Err(NativeReportRefusal::PeerUid);
        }
        if self.binding.revalidate().is_err() {
            return Err(NativeReportRefusal::PeerIdentityChanged);
        }
        if self.identity.pid != params.pid()
            || self.identity.start_identity.get() != params.pid_start_identity().get()
        {
            return Err(NativeReportRefusal::PeerNotRoot);
        }
        #[cfg(test)]
        let recheck = self.recheck_process;
        #[cfg(not(test))]
        let recheck = true;
        if recheck && live_identity(self.identity.pid) != Some(self.identity) {
            return Err(NativeReportRefusal::PeerIdentityChanged);
        }
        Ok(())
    }

    /// Builds a peer that claims `identity` without a live process behind it,
    /// for tests that drive the registry without a socket.
    ///
    /// The owner is the test process's own, which the daemon accepts.
    #[cfg(test)]
    pub(crate) fn for_test(identity: ProcessIdentity) -> Self {
        let (accepted, client_end) =
            std::os::unix::net::UnixStream::pair().expect("test Unix socket pair");
        Self {
            binding: peer::Binding::capture(&accepted).expect("test peer binding"),
            identity,
            _client_end: Some(client_end),
            recheck_process: false,
        }
    }
}

/// Attests `peer` for a report naming `params`; an absent peer is refused.
pub(crate) fn attest_report_caller(
    peer: Option<&ControlPeer>,
    params: &SessionReportNativeIdParams,
) -> Result<(), NativeReportRefusal> {
    peer.ok_or(NativeReportRefusal::NoPeer)?.attest(params)
}

/// Logs a refused report once and returns the typed refusal for the caller.
pub(crate) fn unattested(session_id: &SessionId, refusal: NativeReportRefusal) -> ProtocolError {
    warn!(
        name: "session.native_report.unattested",
        session_id = %session_id.0,
        reason = refusal.code(),
        "native-id report refused: the caller is not attested as the session's PTY root"
    );
    ProtocolError::new(
        ErrorClass::Runtime,
        UNATTESTED_CODE,
        UNATTESTED_MESSAGE,
        Some(UNATTESTED_RECOVER.to_owned()),
    )
}

/// Reads the identity of one running process, or `None` if it cannot vouch.
///
/// A zombie keeps its process id and start identity, so the running check is
/// part of the identity: a connector that exited without being reaped must not
/// be attested.
fn live_identity(pid: u32) -> Option<ProcessIdentity> {
    let inspector = HostInspector::new();
    let identity = inspector.identity(pid).ok().flatten()?;
    inspector.is_running(identity).ok()?.then_some(identity)
}
