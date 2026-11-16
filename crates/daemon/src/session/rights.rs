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
//! - [`CallerClass::Unattributed`]: a process no session root attributes while
//!   a live session has no verified root, so it may belong to that session.
//!   Every rights-checked mutation is refused, and the doctor names the
//!   session.
//!
//! This is a guardrail, not a wall. A process of the same user that leaves the
//! session tree keeps owner rights: `setsid` plus a double fork where only
//! ancestry decides, and `systemd-run --user --scope` where the worker unit
//! decides as well. Isolation of a hostile session is a separate track.

// Rust guideline compliant 2026-11-15

use std::collections::{HashMap, HashSet};

use protocol::{method, AgentActivity, ProtocolError, RuntimeRef, RuntimeState, SessionId};
use tracing::warn;

use super::native_attestation::{ControlPeer, PeerStanding};
use super::{is_terminal, runtime_error, RuntimeHandle, SessionRegistry};
use crate::agent::profile::rights::{rights_of, ProfileRights};
use crate::procwatch::{Pid, ProcessIdentity, ProcessInspector};
use crate::store::DesiredState;
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
    /// A caller outside every verified root while a live session has none.
    Unattributed,
}

impl CallerClass {
    /// The stable name written to the audit log.
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::OwnerShell(_) => "owner_shell",
            Self::Session(_) => "session",
            Self::Unknown => "unknown",
            Self::Unattributed => "unattributed",
        }
    }

    /// The session the caller's process tree belongs to, if any.
    pub(crate) const fn session(&self) -> Option<&SessionId> {
        match self {
            Self::OwnerShell(id) | Self::Session(id) => Some(id),
            Self::Owner | Self::Unknown | Self::Unattributed => None,
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
    /// The caller is outside every verified session root while a live session
    /// has none, so it may belong to that session.
    UnattributedCaller,
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
            Self::UnattributedCaller => "unattributed_caller",
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
    /// PTY and worker roots by process id; the identity pins the process
    /// generation. A pid can hold several identities, since a stale entry
    /// may name a process whose id a live root has since reused.
    by_pid: HashMap<Pid, Vec<(SessionId, ProcessIdentity)>>,
    /// Every live session, whether or not its root process is known.
    live: HashSet<SessionId>,
    /// The live sessions of the `shell` runtime.
    shell: HashSet<SessionId>,
    /// The live sessions with no verified root, so a process in their tree
    /// would not be placed.
    unverified: Vec<SessionId>,
    /// The sessions whose runtime the daemon refuses to adopt but whose
    /// recorded worker still places their processes.
    unadopted: Vec<SessionId>,
    /// The sessions whose runtime the daemon refuses to adopt and whose worker
    /// it holds no identity for, so their processes keep owner rights.
    unplaced: Vec<SessionId>,
}

/// What the doctor reports about the roots of the live sessions.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct RootFindings {
    /// Live sessions whose processes the daemon cannot place.
    pub(crate) unverified: Vec<SessionId>,
    /// Sessions with a runtime the daemon does not adopt, attributed through
    /// their recorded worker.
    pub(crate) unadopted: Vec<SessionId>,
    pub(crate) unplaced: Vec<SessionId>,
}

impl Roots {
    fn add(&mut self, id: &SessionId, root: ProcessIdentity) {
        self.by_pid
            .entry(root.pid)
            .or_default()
            .push((id.clone(), root));
    }

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
    /// connection. A caller outside every root while a live session has no
    /// verified root is `Unattributed` and is not kept: the next request
    /// places it again.
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
            return if roots.unverified.is_empty() {
                (CallerClass::Owner, true)
            } else {
                (CallerClass::Unattributed, false)
            };
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
    /// `None` for a session the registry does not hold. A session whose record
    /// predates the recorded launch rights takes the file as it reads at its
    /// first use.
    pub(crate) async fn session_profile_rights(&self, id: &SessionId) -> Option<ProfileRights> {
        let agent = self.session_agent(id).await?;
        let live = self.profile_rights(&agent).await;
        let (rights, first_use) = {
            let mut sessions = self.inner.sessions.lock().await;
            let entry = sessions.get_mut(id)?;
            let first_use = entry.launch_rights.is_none();
            let held = entry
                .launch_rights
                .get_or_insert_with(|| Box::new(live.clone()));
            let rights = held.narrowed_by(&live);
            (
                rights,
                first_use && entry.desired_state == DesiredState::Running && !entry.stopping,
            )
        };
        if first_use {
            // The snapshot is stored at once, so a restart after a profile edit
            // cannot widen the session. Only the rights field of the durable
            // record is written, so a running stop's intent and an unpersisted
            // classification stay as they are.
            let held = self
                .inner
                .sessions
                .lock()
                .await
                .get(id)
                .and_then(|entry| entry.launch_rights.as_deref().map(ProfileRights::to_record));
            if let Some(held) = held {
                if let Err(error) = self.store_launch_rights(id, held).await {
                    warn!(
                        name: "session.rights.snapshot_unpersisted",
                        session_id = %id.0,
                        error = %error.msg,
                        "the launch rights snapshot of a session could not be stored"
                    );
                }
            }
        }
        Some(rights)
    }

    /// Writes the launch rights of `id` into its durable record.
    async fn store_launch_rights(
        &self,
        id: &SessionId,
        rights: crate::store::LaunchRights,
    ) -> Result<(), ProtocolError> {
        let Some(store) = self.store_for_write()? else {
            return Ok(());
        };
        let session_id = id.0.clone();
        self.inner
            .background
            .spawn_blocking("set_launch_rights", move || {
                store.set_launch_rights(&session_id, &rights)
            })
            .await
            .map_err(|_join_error| {
                runtime_error(
                    "session_store_failed",
                    format!("launch rights write task panicked for {}", id.0),
                )
            })?
            .map_err(|error| {
                runtime_error(
                    "session_store_failed",
                    format!("failed to store the launch rights of {}: {error}", id.0),
                )
            })?;
        Ok(())
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

    /// Whether a control group places a caller in its session, so that a
    /// session without a known root process is still attributed.
    fn places_by_control_group(&self) -> bool {
        cfg!(target_os = "linux")
            && self
                .active_supervision()
                .is_some_and(|supervision| supervision.native)
    }

    async fn live_roots(&self) -> Roots {
        let by_group = self.places_by_control_group();
        let sessions = self.inner.sessions.lock().await;
        let mut roots = Roots {
            by_pid: HashMap::new(),
            live: HashSet::new(),
            shell: HashSet::new(),
            unverified: Vec::new(),
            unadopted: Vec::new(),
            unplaced: Vec::new(),
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
            // The worker is the ancestor of every process of the session, so
            // it places a caller before the PTY root is known and after the
            // PTY root has exited.
            let attested = [entry.root_process, entry.worker_root];
            for root in attested.into_iter().flatten() {
                roots.add(id, root);
            }
            // A runtime the daemon holds no worker for may still run
            // processes: it is reconnecting or starting. Without a recorded
            // worker identity, and on a host whose control groups do not place
            // the session, its processes cannot be attributed. A recorded
            // worker that is gone is not a fault: its tree has nothing left to
            // place. A refused runtime (`Conflict`, `Incompatible`) is placed
            // through the worker identity the daemon observed or recorded; one
            // without it is no fault either, so the owner keeps the documented
            // recovery of removing the session.
            let bears_process = match &entry.runtime {
                RuntimeHandle::Worker(_) => true,
                RuntimeHandle::Unavailable(state) => {
                    matches!(state, RuntimeState::Starting | RuntimeState::Reconnecting)
                }
            };
            if bears_process && !by_group && attested.iter().all(Option::is_none) {
                roots.unverified.push(id.clone());
            }
            if matches!(
                &entry.runtime,
                RuntimeHandle::Unavailable(RuntimeState::Conflict | RuntimeState::Incompatible)
            ) {
                if entry.worker_root.is_some() {
                    roots.unadopted.push(id.clone());
                } else {
                    roots.unplaced.push(id.clone());
                }
            }
        }
        // A launch records its worker before the worker forks the PTY child,
        // so a process of a session still being launched is placed below it.
        // A resume or relaunch adds the new worker beside the previous
        // runtime's stale identities.
        for (id, reservation) in self
            .inner
            .launching
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            roots.live.insert(id.clone());
            if let Some(worker) = reservation.worker_root {
                roots.add(id, worker);
            }
        }
        roots
    }

    /// The sessions the doctor names for their roots.
    pub(crate) async fn root_findings(&self) -> RootFindings {
        let roots = self.live_roots().await;
        RootFindings {
            unverified: roots.unverified,
            unadopted: roots.unadopted,
            unplaced: roots.unplaced,
        }
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
            CallerClass::Unattributed => return Err(Denial::UnattributedCaller),
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
        if let Some((id, _)) = roots
            .by_pid
            .get(&current.pid)
            .and_then(|held| held.iter().find(|(_, root)| *root == current))
        {
            return Some(id.clone());
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
