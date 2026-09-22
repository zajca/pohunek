//! Binds one private worker connection to a kernel-attested peer process.
//!
//! [`PeerContext`] is the worker's transport metadata. It is captured from the
//! accepted socket before any request byte is parsed and carried into every
//! private path — control, identity hook, and data — so no decision that needs
//! process identity has to fall back to a caller-supplied field.
//!
//! The type deliberately lives here rather than in `pohunek-worker-protocol`.
//! That crate is the pure wire contract: it depends only on serde, thiserror,
//! and tokio and forbids unsafe code, while this type owns an operating-system
//! descriptor and asks [`HostInspector`] for process facts. Keeping it beside
//! [`crate::LeaseOwner`], the other peer-derived worker type, also makes it
//! structurally impossible for a wire payload to deserialize into it.
//!
//! Peer identity has two halves with different guarantees, and the worker
//! depends on both. The owner is frozen by the kernel at connect time. The
//! process id and its start identity are inspection-time facts, so
//! [`PeerContext::reverify`] re-reads them before each authority decision:
//! Darwin can report a different peer after a descriptor hand-off, and either
//! kernel can be describing a process that has since exited or been replaced.
//!
//! "Each authority decision" is meant literally, because a connection outlives
//! the moment it was authorized: every control request is re-checked, not only
//! the one that acquires the lease, and a data stream is re-checked in both
//! directions for as long as it runs — including an observation stream that
//! sends nothing until its wait ends. Checking once would leave a descriptor
//! handed on afterwards still issuing input and still receiving PTY bytes under
//! the original peer's authority.
//!
//! Silence counts too. A leased connection that stops speaking is re-checked on
//! a timer, because the lease is exclusive: a descriptor inherited from a
//! daemon that has since exited would otherwise hold it until the process dies,
//! and a replacement daemon would keep being told the controller is busy.
//!
//! One thing this cannot observe is an `exec` in place. `execve` keeps both the
//! process id and the kernel start time, on Linux and Darwin alike, so a peer
//! that replaces its own image without exiting keeps the identity it captured.
//! That is accepted inside the single-account trust boundary; executable
//! identity is the launch-claim path's concern, not the transport's.
//!
//! The explicit in-process identity mode in
//! <https://github.com/zajca/pohunek/issues/52> reuses this type unchanged and
//! adds the stricter rule that the peer must *be* the reported process. That
//! rule stays with #52 because it needs a wire shape distinguishing a
//! self-report from a child hook: the shipped Codex and Claude hooks run as a
//! child of the agent and report the agent's id, so demanding equality here
//! would reject every one of them.
//!
//! [`crate::LeaseOwner`]: crate::LeaseOwner

// Rust guideline compliant 2026-09-22

use pohunek_platform::peer;
use pohunek_platform::process::{
    HostInspector, ProcessIdentity as OsProcessIdentity, ProcessInspector,
};

use crate::WorkerError;

/// Stable reason code for a rejected private worker identity claim.
///
/// The codes are the only rejection detail that reaches a log. They carry no
/// payload, no native reference, and no protocol frame, and the wire response
/// stays as generic as it is today so the set cannot become a probing oracle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The kernel could not attest the connected socket at all.
    PeerUnavailable,
    /// The peer runs under a different user than the worker.
    PeerForeignOwner,
    /// The kernel reported no peer process identifier.
    PeerPidUnavailable,
    /// The kernel now reports a different peer than the captured one.
    PeerChanged,
    /// The peer process exited or was replaced after capture.
    PeerExited,
    /// The peer is neither the reported process nor a descendant of it.
    PeerOutsideSubject,
    /// The reported process is gone, replaced, or outside the managed PTY tree.
    ReportedProcessInvalid,
    /// The redeeming peer is not the process that acquired the lease.
    LeaseOwnerMismatch,
    /// The claim names a provider the worker does not accept identity from.
    ProviderNotAllowed,
    /// The claim names a runtime generation this worker is not serving.
    RuntimeMismatch,
    /// The runtime is not running, so it has no identity to change.
    PhaseNotRunning,
    /// The claim is already expired or exceeds the shared expiry ceiling.
    ClaimExpired,
    /// The claim's sequence is not ahead of the recorded ordering.
    SequenceStale,
    /// The subagent claim carries identifiers outside the accepted shape.
    SubagentClaimInvalid,
}

impl RejectReason {
    /// Returns the stable code used in structured logs.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::PeerUnavailable => "peer_unavailable",
            Self::PeerForeignOwner => "peer_foreign_owner",
            Self::PeerPidUnavailable => "peer_pid_unavailable",
            Self::PeerChanged => "peer_changed",
            Self::PeerExited => "peer_exited",
            Self::PeerOutsideSubject => "peer_outside_subject",
            Self::ReportedProcessInvalid => "reported_process_invalid",
            Self::LeaseOwnerMismatch => "lease_owner_mismatch",
            Self::ProviderNotAllowed => "provider_not_allowed",
            Self::RuntimeMismatch => "runtime_mismatch",
            Self::PhaseNotRunning => "phase_not_running",
            Self::ClaimExpired => "claim_expired",
            Self::SequenceStale => "sequence_stale",
            Self::SubagentClaimInvalid => "subagent_claim_invalid",
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// Kernel-attested identity of the process on the other end of one connection.
#[derive(Debug)]
pub(crate) struct PeerContext {
    binding: peer::Binding,
    identity: OsProcessIdentity,
    /// Whether [`PeerContext::reverify`] rechecks the operating-system record.
    ///
    /// Always `true` for a captured peer. Tests that stand in for a process
    /// they do not own set it to `false`, because there is no live process
    /// behind their synthetic identity to recheck.
    recheck_process: bool,
}

impl PeerContext {
    /// Captures the peer of an accepted socket before any request is parsed.
    ///
    /// The owner check runs here, so a foreign-user connection never reaches
    /// request dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::PeerIdentity`] when the kernel cannot attest the
    /// socket, the peer runs under another user, or the peer process is already
    /// gone.
    pub(crate) fn capture(socket: &impl std::os::fd::AsFd) -> Result<Self, WorkerError> {
        let binding = peer::Binding::capture(socket).map_err(|error| reject(&error))?;
        let snapshot = binding.snapshot();
        if !owner_matches(snapshot.uid, rustix::process::geteuid().as_raw()) {
            return Err(WorkerError::PeerIdentity {
                reason: RejectReason::PeerForeignOwner,
            });
        }
        let identity = live_identity(snapshot.pid).ok_or(WorkerError::PeerIdentity {
            reason: RejectReason::PeerExited,
        })?;
        Ok(Self {
            binding,
            identity,
            recheck_process: true,
        })
    }

    /// Returns the PID-reuse-safe identity captured when the peer connected.
    pub(crate) const fn identity(&self) -> OsProcessIdentity {
        self.identity
    }

    /// Returns the kernel interface that attested this peer.
    ///
    /// Constant for a given target, but reported per context so a rejection
    /// event says which contract the peer was read under: a drift under
    /// `LOCAL_PEERCRED` is a descriptor hand-off worth investigating, while the
    /// same code under `SO_PEERCRED` would mean a broken kernel promise.
    #[expect(
        clippy::unused_self,
        reason = "reads as a property of the bound peer, and stays correct if provenance ever becomes per-socket"
    )]
    pub(crate) fn provenance(&self) -> peer::Provenance {
        peer::provenance()
    }

    /// Re-reads the kernel answer and requires the peer to be unchanged.
    ///
    /// Call this before every decision that grants authority. It fails when the
    /// socket now reports a different peer, and when the captured process has
    /// exited or been replaced by a reused identifier.
    ///
    /// # Errors
    ///
    /// Returns [`RejectReason::PeerChanged`], [`RejectReason::PeerExited`], or
    /// the classified lookup failure.
    pub(crate) fn reverify(&self) -> Result<(), RejectReason> {
        self.binding.revalidate().map_err(|error| match error {
            peer::Error::Changed => RejectReason::PeerChanged,
            other => reason(&other),
        })?;
        if !self.recheck_process || live_identity(self.identity.pid) == Some(self.identity) {
            Ok(())
        } else {
            Err(RejectReason::PeerExited)
        }
    }

    /// Builds a context from an explicit identity, for tests only.
    ///
    /// Production code reaches a context exclusively through
    /// [`PeerContext::capture`], so no wire field can ever supply one. Tests
    /// that drive a handler without a real socket use this instead.
    #[cfg(test)]
    pub(crate) fn for_test(identity: OsProcessIdentity) -> Self {
        Self {
            recheck_process: false,
            ..Self::for_test_live(identity)
        }
    }

    /// Builds a test context that still rechecks the operating-system record.
    ///
    /// Use this where the supplied identity names a real live process, so the
    /// recheck is meaningful.
    #[cfg(test)]
    pub(crate) fn for_test_live(identity: OsProcessIdentity) -> Self {
        let (accepted, client) =
            std::os::unix::net::UnixStream::pair().expect("test Unix socket pair");
        // The peer end stays open so the kernel keeps attesting the pair.
        std::mem::forget(client);
        Self {
            binding: peer::Binding::capture(&accepted).expect("test peer binding"),
            identity,
            recheck_process: true,
        }
    }
}

/// Returns whether a peer runs under the same user as the worker.
///
/// A same-process socket always reports the worker's own user, so the rule is
/// kept as a pure function: it is the only way to exercise a foreign owner
/// deterministically on a single-account host, which the issue requires
/// everywhere even where no multi-account runner exists.
const fn owner_matches(peer_uid: u32, worker_uid: u32) -> bool {
    peer_uid == worker_uid
}

/// Reads the identity of one *running* process, or `None` if it cannot vouch.
///
/// The liveness check matters as much as the identity: a zombie keeps its
/// process id and start identity, so comparing those alone would accept a
/// connector that exited without being reaped after handing its descriptor to
/// another process. `is_running` is the contract that excludes that state on
/// both targets.
///
/// An observation failure is treated as absence on purpose: the caller must not
/// act on a process the worker cannot currently see.
fn live_identity(pid: u32) -> Option<OsProcessIdentity> {
    let inspector = HostInspector::new();
    let identity = inspector.identity(pid).ok().flatten()?;
    inspector.is_running(identity).ok()?.then_some(identity)
}

/// Classifies a peer lookup failure into a rejection reason.
fn reason(error: &peer::Error) -> RejectReason {
    match error {
        peer::Error::PidUnavailable | peer::Error::InvalidPid => RejectReason::PeerPidUnavailable,
        peer::Error::Changed => RejectReason::PeerChanged,
        peer::Error::Socket(_) | peer::Error::Unavailable | peer::Error::Malformed => {
            RejectReason::PeerUnavailable
        }
    }
}

/// Wraps a peer lookup failure as the worker's typed connection error.
fn reject(error: &peer::Error) -> WorkerError {
    WorkerError::PeerIdentity {
        reason: reason(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{owner_matches, PeerContext, RejectReason};
    use pohunek_platform::process::{ProcessIdentity as OsProcessIdentity, StartIdentity};

    #[test]
    fn reason_codes_are_stable_and_distinct() {
        let codes = [
            RejectReason::PeerUnavailable,
            RejectReason::PeerForeignOwner,
            RejectReason::PeerPidUnavailable,
            RejectReason::PeerChanged,
            RejectReason::PeerExited,
            RejectReason::PeerOutsideSubject,
            RejectReason::ReportedProcessInvalid,
            RejectReason::LeaseOwnerMismatch,
            RejectReason::ProviderNotAllowed,
            RejectReason::RuntimeMismatch,
            RejectReason::PhaseNotRunning,
            RejectReason::ClaimExpired,
            RejectReason::SequenceStale,
            RejectReason::SubagentClaimInvalid,
        ]
        .map(RejectReason::code);
        let unique = codes.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), codes.len(), "reason codes must be distinct");
        assert!(
            codes.iter().all(|code| code
                .chars()
                .all(|byte| byte.is_ascii_lowercase() || byte == '_')),
            "reason codes must stay snake_case and payload-free: {codes:?}"
        );
    }

    #[test]
    fn a_peer_under_another_user_is_rejected() {
        let worker = rustix::process::geteuid().as_raw();
        assert!(owner_matches(worker, worker), "the owner must be accepted");
        for foreign in [worker.wrapping_add(1), 0, u32::MAX] {
            if foreign == worker {
                continue;
            }
            assert!(
                !owner_matches(foreign, worker),
                "uid {foreign} must not pass the owner rule for worker uid {worker}"
            );
        }
    }

    #[test]
    fn capture_binds_the_connecting_process() {
        let (accepted, _client) =
            std::os::unix::net::UnixStream::pair().expect("test Unix socket pair");
        let context = PeerContext::capture(&accepted).expect("capture peer");
        assert_eq!(context.identity().pid, std::process::id());
        context.reverify().expect("peer identity is unchanged");
    }

    /// A peer that exited without being reaped must not keep vouching.
    ///
    /// A zombie keeps its process id and start identity, so comparing those
    /// alone would accept a connector that exited after handing its descriptor
    /// on — on Linux the socket record still names it.
    #[test]
    fn a_zombie_peer_is_not_live() {
        use pohunek_platform::process::{HostInspector, ProcessInspector};

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn a process that exits immediately");
        let pid = child.id();
        let inspector = HostInspector::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let zombie = loop {
            // The child is unreaped here on purpose: `wait` is only called once
            // the assertions below are done with the zombie.
            if let Ok(Some(identity)) = inspector.identity(pid) {
                if inspector.is_running(identity).is_ok_and(|running| !running) {
                    break identity;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child never became an unreaped zombie"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(
            inspector.identity(pid).expect("inspect the zombie"),
            Some(zombie),
            "a zombie still reports its identity, which is what makes this trap real"
        );
        assert_eq!(
            super::live_identity(pid),
            None,
            "a zombie must not be treated as a live peer"
        );
        child.wait().expect("reap the zombie");
    }

    #[test]
    fn reverify_rejects_a_process_that_is_no_longer_there() {
        // A start identity the live process cannot have makes the recheck fail
        // exactly as a reused process id would.
        let context = PeerContext::for_test_live(OsProcessIdentity {
            pid: std::process::id(),
            start_identity: StartIdentity::new(u64::MAX),
        });
        assert_eq!(context.reverify(), Err(RejectReason::PeerExited));
    }
}
