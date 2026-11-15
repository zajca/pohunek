//! Caller classification by kernel peer and the per-session target rule.
//!
//! A control connection that arrives on the local socket has a kernel-attested
//! peer process. The daemon places that process in one of four classes:
//!
//! - [`CallerClass::Session`]: the process descends from a managed session's
//!   PTY root, or on Linux runs in a session's worker unit. It may mutate only
//!   the descendants of that session, and may send input to its parent.
//! - [`CallerClass::OwnerShell`]: the same, for a `shell` runtime session no
//!   session spawned, and for a session whose profile sets
//!   `[rights] owner_equivalent`; both keep owner rights. A shell a session
//!   spawned is a `Session` of its own tree.
//! - [`CallerClass::Owner`]: everything else, including the owner's terminal,
//!   services outside every session tree, and a process that left its tree.
//! - [`CallerClass::Unknown`]: a peer of another user, or one the kernel can
//!   no longer attest. Every rights-checked mutation is refused.
//!
//! This is a guardrail, not a wall. A process of the same user that leaves the
//! session tree keeps owner rights: `setsid` plus a double fork where only
//! ancestry decides, and `systemd-run --user --scope` where the worker unit
//! decides as well. Isolation of a hostile session is a separate track.

// Rust guideline compliant 2026-11-15

use std::collections::{HashMap, HashSet};

use protocol::{method, AgentActivity, ProtocolError, RuntimeRef, SessionId};
use tracing::warn;

use super::native_attestation::{ControlPeer, PeerStanding};
use super::{is_terminal, SessionRegistry};
use crate::agent::profile::rights::{rights_of, ProfileRights};
use crate::procwatch::{Pid, ProcessIdentity, ProcessInspector};
use pohunek_platform::supervisor::Namespace;

/// Most parent links one ancestry walk follows.
///
/// Process trees under a session are far shallower. The bound only stops a
/// walk through an inspector that reports a cycle, so reaching it classifies
/// the caller as outside every session tree.
pub(crate) const MAX_ANCESTRY_DEPTH: usize = 128;

/// The process ids at or below this value are the kernel and init, never a
/// session's PTY root, so a walk stops there.
const LAST_RESERVED_PID: Pid = 1;

/// Which of the rights classes a control caller falls in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallerClass {
    /// Full owner rights.
    Owner,
    /// Owner rights, held by a process of a `shell` runtime session.
    OwnerShell(SessionId),
    /// Rights of one managed session's process tree.
    Session(SessionId),
    /// A caller the daemon cannot place; mutations are refused.
    Unknown,
}

impl CallerClass {
    /// The stable name written to the audit log.
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::OwnerShell(_) => "owner_shell",
            Self::Session(_) => "session",
            Self::Unknown => "unknown",
        }
    }

    /// The session the caller's process tree belongs to, if any.
    pub(crate) const fn session(&self) -> Option<&SessionId> {
        match self {
            Self::OwnerShell(id) | Self::Session(id) => Some(id),
            Self::Owner | Self::Unknown => None,
        }
    }
}

/// Why a rights-checked request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Denial {
    /// The target is neither a descendant of the caller's session nor, for
    /// input, its parent.
    NotDescendant,
    /// The target is the caller session's parent, which waits on an owner
    /// dialog.
    ParentBlocked,
    /// The caller could not be placed.
    UnknownCaller,
    /// The target is the caller's own session, which the origin-session guard
    /// refuses with its own code whether or not the request carries markers.
    SelfTarget,
}

impl Denial {
    /// The `reason` the `session_right_denied` error and the audit line carry.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::NotDescendant => "not_descendant",
            Self::ParentBlocked => "parent_blocked",
            Self::UnknownCaller => "unknown_caller",
            Self::SelfTarget => "self_target",
        }
    }

    /// The error a refused request answers with.
    pub(crate) fn error(self) -> ProtocolError {
        match self {
            Self::SelfTarget => ProtocolError::plugin_self_target_denied(),
            other => ProtocolError::session_right_denied(other.code()),
        }
    }
}

/// How the daemon places a caller, for the doctor.
///
/// The control group counts only where the service manager runs each worker as
/// its own systemd user unit, which `units` says. Daemons that run workers as
/// direct children, and macOS, place a caller by ancestry alone.
pub(crate) const fn platform_mode(units: bool) -> &'static str {
    if cfg!(target_os = "linux") && units {
        "ancestry+cgroup"
    } else {
        "ancestry"
    }
}

/// The live sessions a caller can be placed in.
struct Roots {
    /// PTY roots by process id; the identity pins the process generation.
    by_pid: HashMap<Pid, (SessionId, ProcessIdentity)>,
    /// Every live session, whether or not its root process is known.
    live: HashSet<SessionId>,
    /// The live sessions of the `shell` runtime.
    shell: HashSet<SessionId>,
    /// Whether a live session has no known PTY root, so a process in its tree
    /// would not be placed.
    rootless: bool,
}

impl Roots {
    fn class_of(&self, id: SessionId) -> CallerClass {
        if self.shell.contains(&id) {
            CallerClass::OwnerShell(id)
        } else {
            CallerClass::Session(id)
        }
    }
}

impl SessionRegistry {
    /// Places the peer of a local control connection in a rights class.
    ///
    /// The second value says whether the class may be kept for the life of the
    /// connection. An `Owner` placement made while a live session has no known
    /// root is not: the caller may belong to that session, and the next
    /// request places it again.
    pub(crate) async fn classify_caller(&self, peer: &ControlPeer) -> (CallerClass, bool) {
        let process = match peer.standing() {
            PeerStanding::Process(process) => process,
            PeerStanding::ForeignUser | PeerStanding::Unattested => {
                return (CallerClass::Unknown, true)
            }
        };
        let roots = self.live_roots().await;
        let inspector = self.inner.inspector.as_ref();
        let Some(id) = ancestry_session(inspector, process, &roots)
            .or_else(|| worker_unit_session(inspector, process, &roots))
        else {
            return (CallerClass::Owner, !roots.rootless);
        };
        if self
            .session_profile_rights(&id)
            .await
            .is_some_and(|rights| rights.owner_equivalent)
        {
            return (CallerClass::OwnerShell(id), true);
        }
        (roots.class_of(id), true)
    }

    /// The rights of the live session `id`: those its profile held at launch,
    /// narrowed by the profile file as it reads now.
    ///
    /// `None` for a session the registry does not hold. A session whose launch
    /// rights were not recorded, such as one restored after a daemon restart,
    /// takes the file as it reads at its first use.
    pub(crate) async fn session_profile_rights(&self, id: &SessionId) -> Option<ProfileRights> {
        let agent = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .map(|entry| entry.info.agent.clone())?;
        let live = self.profile_rights(&agent).await;
        let mut launch = self
            .inner
            .launch_rights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let held = launch.entry(id.clone()).or_insert_with(|| live.clone());
        Some(held.narrowed_by(&live))
    }

    /// The profile name the session `id` runs, if the registry holds it.
    pub(crate) async fn session_agent(&self, id: &SessionId) -> Option<String> {
        self.inner
            .sessions
            .lock()
            .await
            .get(id)
            .map(|entry| entry.info.agent.clone())
    }

    /// Records the rights the profile of the session `id` holds now as the
    /// rights the session was launched with.
    pub(crate) async fn record_launch_rights(&self, id: &SessionId) {
        let Some(agent) = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .map(|entry| entry.info.agent.clone())
        else {
            return;
        };
        let live = self.profile_rights(&agent).await;
        self.inner
            .launch_rights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), live);
    }

    /// The `[rights]` of the profile `agent`, read from its file.
    ///
    /// A profile the daemon cannot read grants nothing.
    pub(crate) async fn profile_rights(&self, agent: &str) -> ProfileRights {
        let profiles = self.inner.profiles.clone();
        let agent = agent.to_owned();
        self.inner
            .background
            .spawn_blocking("profile_rights", move || rights_of(&profiles, &agent))
            .await
            .unwrap_or_else(|_join_error| ProfileRights::closed())
    }

    async fn live_roots(&self) -> Roots {
        let sessions = self.inner.sessions.lock().await;
        let mut roots = Roots {
            by_pid: HashMap::new(),
            live: HashSet::new(),
            shell: HashSet::new(),
            rootless: false,
        };
        for (id, entry) in sessions.iter() {
            if is_terminal(entry.info.state) {
                continue;
            }
            roots.live.insert(id.clone());
            // A shell session a session spawned gets the rights of its own
            // tree only, so a worker cannot reach owner rights by typing into
            // a shell it started.
            if entry.info.agent_base == RuntimeRef::shell() && entry.info.spawned_by.is_none() {
                roots.shell.insert(id.clone());
            }
            match entry.root_process {
                Some(root) => {
                    roots.by_pid.insert(root.pid, (id.clone(), root));
                }
                None => roots.rootless = true,
            }
        }
        roots
    }

    /// Applies the target rule to a caller of `class` that requests `method`
    /// on the session `target`.
    ///
    /// A session the registry does not hold passes, so the method answers its
    /// own not-found error.
    pub(crate) async fn check_session_right(
        &self,
        class: &CallerClass,
        method: &str,
        target: &str,
    ) -> Result<(), Denial> {
        let caller = match class {
            CallerClass::Owner | CallerClass::OwnerShell(_) => return Ok(()),
            CallerClass::Unknown => return Err(Denial::UnknownCaller),
            CallerClass::Session(caller) => caller,
        };
        let target = SessionId(target.to_owned());
        if target == *caller {
            // Attaching to the caller's own session keeps its own guard
            // (`attach_self_feedback`), which reads the client's origin.
            return if method == method::SESSION_ATTACH {
                Ok(())
            } else {
                Err(Denial::SelfTarget)
            };
        }
        let sessions = self.inner.sessions.lock().await;
        let Some(target_entry) = sessions.get(&target) else {
            return Ok(());
        };
        if descends_from(
            |id| sessions.get(id).map(|entry| &entry.info),
            &target,
            caller,
        ) {
            return Ok(());
        }
        let is_parent = sessions
            .get(caller)
            .and_then(|entry| entry.info.spawned_by.as_ref())
            .is_some_and(|spawned| spawned.session_id == target);
        if !is_parent || method != method::SESSION_INPUT {
            return Err(Denial::NotDescendant);
        }
        if target_entry.info.activity == Some(AgentActivity::Blocked) {
            return Err(Denial::ParentBlocked);
        }
        Ok(())
    }
}

/// Whether `session` is a strict descendant of `ancestor` along `spawned_by`.
///
/// A visited set bounds the walk, so a malformed loop in the records cannot
/// hang the caller.
pub(super) fn descends_from<'a>(
    lookup: impl Fn(&SessionId) -> Option<&'a protocol::SessionInfo>,
    session: &SessionId,
    ancestor: &SessionId,
) -> bool {
    let mut visited = HashSet::new();
    visited.insert(session.clone());
    let mut current = lookup(session)
        .and_then(|info| info.spawned_by.as_ref())
        .map(|spawned| spawned.session_id.clone());
    while let Some(id) = current {
        if id == *ancestor {
            return true;
        }
        if !visited.insert(id.clone()) {
            return false;
        }
        current = lookup(&id)
            .and_then(|info| info.spawned_by.as_ref())
            .map(|spawned| spawned.session_id.clone());
    }
    false
}

/// The session whose PTY root `process` descends from, following parent
/// links.
///
/// A match needs the root's pid and its start identity, so a recycled pid is
/// never taken for the root. A process the inspector cannot read ends the
/// walk without a match.
fn ancestry_session(
    inspector: &dyn ProcessInspector,
    process: ProcessIdentity,
    roots: &Roots,
) -> Option<SessionId> {
    let mut current = process;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if let Some((id, root)) = roots.by_pid.get(&current.pid) {
            if *root == current {
                return Some(id.clone());
            }
        }
        let parent = match inspector.parent_pid(current.pid) {
            Ok(Some(parent)) if parent > LAST_RESERVED_PID => parent,
            Ok(_) => return None,
            Err(error) => {
                warn_unreadable(&error);
                return None;
            }
        };
        current = match inspector.identity(parent) {
            Ok(Some(identity)) => identity,
            Ok(None) => return None,
            Err(error) => {
                warn_unreadable(&error);
                return None;
            }
        };
    }
    None
}

/// The live session whose worker unit holds `process`, on a host with control
/// groups.
fn worker_unit_session(
    inspector: &dyn ProcessInspector,
    process: ProcessIdentity,
    roots: &Roots,
) -> Option<SessionId> {
    let group = inspector.control_group(process.pid).ok().flatten()?;
    // The group belongs to the peer only while the peer is still the process
    // that was attested.
    if inspector.identity(process.pid).ok().flatten() != Some(process) {
        return None;
    }
    group.as_str().split('/').find_map(|segment| {
        let (_, key) = Namespace::parse_any_worker_unit(segment).ok()?;
        let id = SessionId(key.session_id().to_owned());
        roots.live.contains(&id).then_some(id)
    })
}

fn warn_unreadable(error: &crate::procwatch::Error) {
    warn!(
        name: "session.rights.ancestry_unreadable",
        error = %error,
        "a caller's process ancestry could not be read; the walk stops there"
    );
}
