//! Per-session process watcher and active-agent reconciliation.

// Rust guideline compliant 2026-10-12

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{event, AgentActivity, CwdSource, RuntimeRef, SessionInfo};
use tracing::{debug, warn};

use crate::agent::host::LaunchProgram;
use crate::agent::host::RuntimeDefinition;
use crate::detect::{identify_definition, DetectorConfig};
use crate::procwatch::{ExitWatch, Pid, ProcessFact, ProcessIdentity, StartIdentity};

use super::settle::{Applying, CloseOnDrop, RescanSignal};
use super::{
    agent_kind_label, clear_active_agent, is_terminal, report_is_current, same_pin,
    send_detector_config, timestamp_now, ActiveAgentReport, CancellationToken, ClaimOrigin,
    MatchedDefinition, ObservedAgent, Ordering, RuntimeWatchIdentity, SessionEntry, SessionId,
    SessionRegistry, TurnEvents,
};

const PROCWATCH_SOURCE: &str = "pohunek:procwatch";
type ObservedIdentity = (Pid, u64);

/// Process ancestry of one scan, used to bind a hook-reported claim to the
/// process generation the scan proves.
///
/// The evidence is local to the scan that collected it. It only ever proves a
/// claim (a running reporter below an anchor); a scan without it, an exit
/// callback included, leaves an unbound claim to main's replacement.
struct ScanLineage {
    /// The launch root, which the descendant listing excludes.
    root: ProcessIdentity,
    /// Identity and parent of the root and of every listed descendant.
    processes: HashMap<Pid, LineageEntry>,
    /// The reporter of the session's reported claim, when this scan read it
    /// running after collecting the facts and it is not owned by another
    /// session.
    reporter: Option<ProcessIdentity>,
    /// Processes between the reporter and the root that another session or
    /// worker owns. Such an agent is never observed, yet a claim cannot pass
    /// through it.
    foreign: HashSet<Pid>,
    /// The pid of the claim this scan examined.
    claimed: Pid,
    /// The scan could not decide whether the claim's reporter runs below an
    /// anchor: its facts or a liveness read were unavailable.
    inconclusive: bool,
    /// The exit watch armed for `reporter` before the session mutex was
    /// taken; it moves into the binding when the scan binds the reporter.
    reporter_watch: std::sync::Mutex<Option<ExitWatch>>,
}

impl ScanLineage {
    /// Lineage of a scan whose collection failed: it proves nothing and
    /// leaves the reporter undecided.
    fn unavailable(root: ProcessIdentity, claimed: Pid) -> Self {
        Self {
            root,
            claimed,
            processes: HashMap::new(),
            reporter: None,
            foreign: HashSet::new(),
            inconclusive: true,
            reporter_watch: std::sync::Mutex::new(None),
        }
    }
}

/// The reporter of a hook-reported claim, proven by a scan to run below a
/// same-runtime anchor.
///
/// The record is keyed by the reporter's process generation and lives beside
/// the claim, so a rebuilt claim of the same process keeps matching it. Once
/// bound, only the exit of that generation ends the claim: scans, probe
/// errors and unrelated exits neither refute nor renew it.
#[derive(Debug, Clone)]
pub(super) struct BoundReporter {
    identity: ProcessIdentity,
    /// Runtime of the claim the scan bound.
    base: RuntimeRef,
    /// Same-runtime observed agents the reporter descends from.
    observed_anchors: Vec<ProcessIdentity>,
    /// Whether the reporter descends from the launch root, which is an agent
    /// of the claim's runtime.
    root_anchor: bool,
    /// Whether an exit watch reports the exit of `identity`. Without one, or
    /// once its task was cancelled with the runtime watchers, every scan reads
    /// the generation's liveness.
    pub(super) watched: bool,
    /// Ends the watch task when the record is dropped.
    pub(super) cancel: CancellationToken,
}

impl BoundReporter {
    /// Whether the reporter descends from the agent `anchor` names.
    fn anchored_at(&self, anchor: &ReporterAnchor) -> bool {
        match anchor {
            ReporterAnchor::Observed(observed) => {
                observed.agent_base == self.base
                    && self.observed_anchors.contains(&ProcessIdentity {
                        pid: observed.pid,
                        start_identity: StartIdentity::new(observed.start_identity),
                    })
            }
            ReporterAnchor::LaunchRoot => self.root_anchor,
            ReporterAnchor::Any => true,
        }
    }
}

/// Ends the binding of `entry`, if any, and its watch task.
pub(super) fn drop_bound_reporter(entry: &mut SessionEntry) {
    if let Some(bound) = entry.bound_reporter.take() {
        bound.cancel.cancel();
    }
}

/// The binding the session's current reported claim matches: same process
/// generation (a claim start identity is absent or equal) and same runtime.
fn bound_reporter_of(entry: &SessionEntry) -> Option<&BoundReporter> {
    let bound = entry.bound_reporter.as_ref()?;
    let active = entry
        .active_agent
        .as_ref()
        .filter(|active| !active.observed)?;
    (active.pid == Some(bound.identity.pid)
        && active
            .start_identity
            .is_none_or(|start| start == bound.identity.start_identity.get())
        && entry.info.active_agent_base.as_ref() == Some(&bound.base))
    .then_some(bound)
}

/// The process a refreshed report of a bound claim names: a report of the same
/// source and agent that names no process, or the bound process, keeps the
/// proven generation.
pub(super) fn keep_bound_reporter(
    entry: &SessionEntry,
    source: &str,
    agent: &str,
    base: &RuntimeRef,
    reported_pid: Option<Pid>,
    bound_process: (Option<Pid>, Option<u64>),
) -> (Option<Pid>, Option<u64>) {
    let Some(bound) = bound_reporter_of(entry) else {
        return bound_process;
    };
    let same_claim = entry
        .active_agent
        .as_ref()
        .is_some_and(|active| active.source == source && active.agent == agent)
        && &bound.base == base;
    // A start identity the daemon read for the reported pid that differs from
    // the bound one names another process generation.
    let same_generation = reported_pid.is_none()
        || (reported_pid == Some(bound.identity.pid)
            && bound_process
                .1
                .is_none_or(|start| start == bound.identity.start_identity.get()));
    let keeps = same_claim && same_generation;
    if keeps {
        (
            Some(bound.identity.pid),
            Some(bound.identity.start_identity.get()),
        )
    } else {
        bound_process
    }
}

/// What reading a reporter's liveness and arming its exit watch concluded.
enum ReporterLiveness {
    Running { watch: Option<ExitWatch> },
    Gone,
    Unknown,
}

struct LineageEntry {
    identity: ProcessIdentity,
    parent: Pid,
}

/// The agent a verified reporter has to descend from.
enum ReporterAnchor {
    /// Exactly this observed agent: the foreground selected it.
    Observed(ObservedAgent),
    /// The launch root, when it is an agent of the claim's runtime.
    LaunchRoot,
    /// The launch root or any observed agent of the claim's runtime.
    Any,
}

struct ObservedRefresh {
    agents: HashMap<Pid, ObservedAgent>,
    lineage: Option<ScanLineage>,
}

/// What one rescan found, for pacing the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts of one scan that the pacing loop reads separately"
)]
pub(super) struct ScanOutcome {
    /// Whether the watch keeps running.
    pub(super) alive: bool,
    /// Digest of the process tree the scan read; `None` when it read none.
    tree: Option<u64>,
    /// Whether the scan changed the session or opened an exit watch.
    changed: bool,
    /// Whether the session was not working when the scan applied.
    quiet: bool,
    /// Whether the scan was skipped because a stop is in flight; the loop then
    /// waits for a rescan request instead of its timer.
    paused: bool,
}

impl ScanOutcome {
    const fn stopped() -> Self {
        Self {
            alive: false,
            tree: None,
            changed: false,
            quiet: false,
            paused: false,
        }
    }

    const fn unchanged() -> Self {
        Self {
            alive: true,
            tree: None,
            changed: false,
            quiet: false,
            paused: false,
        }
    }

    /// The session is alive but stopping: nothing was read or applied.
    const fn paused() -> Self {
        Self {
            alive: true,
            tree: None,
            changed: false,
            quiet: false,
            paused: true,
        }
    }
}

/// The inspector reads of one scan, taken together off the async executor.
struct ScanCollection {
    exited_reporter: Option<ProcessIdentity>,
    descendants: DescendantsScan,
    foreground_probe: ForegroundProbe,
    root_running: Result<bool, String>,
}

enum DescendantsScan {
    Read {
        scan_pin: Option<Arc<RuntimeDefinition>>,
        refresh: ObservedRefresh,
        tree: u64,
    },
    Failed(Option<ScanLineage>),
}

/// Whether a scan of `entry` may be followed by a slower one: the agent is not
/// working and no reported claim still waits for a scan to bind or expire it.
fn scan_is_quiet(entry: &SessionEntry) -> bool {
    let unbound_claim = entry
        .active_agent
        .as_ref()
        .is_some_and(|active| !active.observed)
        && bound_reporter_of(entry).is_none();
    entry.info.activity != Some(AgentActivity::Working) && !unbound_claim
}

/// Digest of the identity and command of every process in the tree.
fn tree_digest(facts: &[ProcessFact]) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut ordered: Vec<&ProcessFact> = facts.iter().collect();
    ordered.sort_by_key(|fact| fact.pid);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for fact in ordered {
        (
            fact.pid,
            fact.ppid,
            fact.pgid,
            fact.start_identity,
            &fact.comm,
            &fact.cmdline,
        )
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// How process recognition treats a session's own runtime.
#[derive(Debug, Clone)]
pub(super) enum OwnRuntime {
    /// The definition the session's launch pin names; its process matchers
    /// recognize the session's runtime also when the registry serves another
    /// source for the runtime id.
    Pinned(Arc<RuntimeDefinition>),
    /// This daemon serves no definition of the session's launch pin. A
    /// process the registry identifies as this runtime is not attributed to
    /// the session, so another source's detection rules never stand in.
    Unserved(RuntimeRef),
}

impl OwnRuntime {
    /// The own-runtime rule of `entry`.
    pub(super) fn of(entry: &SessionEntry) -> Self {
        entry.pinned.clone().map_or_else(
            || Self::Unserved(entry.info.agent_base.clone()),
            Self::Pinned,
        )
    }

    /// The pinned definition this rule matches with.
    fn pinned(&self) -> Option<&Arc<RuntimeDefinition>> {
        match self {
            Self::Pinned(definition) => Some(definition),
            Self::Unserved(_) => None,
        }
    }
}

/// Result of one terminal-foreground probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundProbe {
    /// The probe succeeded, including the no-controlling-terminal case.
    Observed(Option<Pid>),
    /// The OS inspection failed and the previous observation must remain valid.
    TransientError,
}

enum ForegroundDecision {
    /// No foreground identity is available; use descendant reconciliation.
    Fallback,
    /// Foreground identity is authoritative and current state remains valid.
    Preserve,
    /// Foreground identity changed the externally visible session state.
    Updated,
}

impl SessionRegistry {
    pub(super) fn spawn_procwatch(
        &self,
        id: SessionId,
        expected: RuntimeWatchIdentity,
        root: ProcessIdentity,
        cancel: CancellationToken,
        rescan: Arc<RescanSignal>,
    ) {
        let registry = self.clone();
        // Closes the rescan gate when the loop ends or is never started, so a
        // request aimed at it cannot stay pending.
        let closed = CloseOnDrop(Arc::clone(rescan.gate()));
        self.spawn_cooperative("procwatch_scan", async move {
            let _closed = closed;
            let base = registry.inner.config.procwatch_poll;
            let ceiling = registry.inner.config.procwatch_idle_poll_max.max(base);
            // The first scan runs at once; later ones wait `delay`, which grows
            // while a quiet session's process tree stays the same.
            let mut delay = Duration::ZERO;
            let mut last_tree = None;
            // A stopping session has nothing to scan; the loop then waits for
            // the rescan its failed stop requests, or for cancellation at the
            // terminal commit.
            let mut paused = false;
            loop {
                // Cancellation wins over a ready timer or rescan so a stopped
                // loop never starts another scan. The guard counts the scan as
                // pending input from its first read until its result is applied
                // to the loop's own state below.
                let (outcome, _applying): (ScanOutcome, Applying) = tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    () = async {
                        if paused {
                            std::future::pending::<()>().await;
                        } else {
                            tokio::time::sleep(delay).await;
                        }
                    } => {
                        let applying = rescan.begin();
                        #[cfg(test)]
                        registry.hold_input(&id, "procwatch_scan").await;
                        let outcome = registry
                            .rescan_live_root(&id, &expected, root, crate::time::now())
                            .await;
                        let steady = outcome.quiet
                            && !outcome.changed
                            && outcome.tree.is_some()
                            && outcome.tree == last_tree;
                        delay = if steady { (delay * 2).clamp(base, ceiling) } else { base };
                        (outcome, applying)
                    }
                    applying = rescan.requested() => {
                        delay = base;
                        #[cfg(test)]
                        registry.hold_input(&id, "procwatch_scan").await;
                        let outcome = registry
                            .rescan_live_root(&id, &expected, root, crate::time::now())
                            .await;
                        (outcome, applying)
                    }
                };
                if !outcome.alive {
                    cancel.cancel();
                    break;
                }
                paused = outcome.paused;
                last_tree = outcome.tree;
            }
        });
    }

    pub(super) async fn rescan_live_root(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        root: ProcessIdentity,
        now: Instant,
    ) -> ScanOutcome {
        if self.procwatch_stop_in_flight(id, expected).await {
            return ScanOutcome::paused();
        }
        let running = self
            .off_executor(move |registry| registry.inner.inspector.is_running(root))
            .await;
        match running {
            Ok(true) => {
                self.rescan_guarded_procwatch_at(id, expected, root, now)
                    .await
            }
            Ok(false) => {
                self.retire_procwatch_root(id, expected, now).await;
                ScanOutcome::stopped()
            }
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    root_pid = root.pid,
                    error = %err,
                    "failed to validate procwatch root identity"
                );
                ScanOutcome::unchanged()
            }
        }
    }

    /// Whether a stop of the runtime `expected` is in flight: the scans leave
    /// the session alone until the stop commits or fails.
    async fn procwatch_stop_in_flight(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
    ) -> bool {
        self.inner
            .sessions
            .lock()
            .await
            .get(id)
            .is_some_and(|entry| {
                entry.stopping && !is_terminal(entry.info.state) && expected.matches(entry)
            })
    }

    /// Runs synchronous process-inspector reads on a blocking thread, so they
    /// never occupy a worker that serves control requests.
    async fn off_executor<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Self) -> T + Send + 'static,
    ) -> T {
        let registry = self.clone();
        match tokio::task::spawn_blocking(move || work(&registry)).await {
            Ok(value) => value,
            Err(error) => match error.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                // The runtime is shutting down; the caller's task is dropped
                // with it, so this value is never observed.
                Err(_cancelled) => std::future::pending().await,
            },
        }
    }

    pub(super) async fn retire_procwatch_root(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        now: Instant,
    ) {
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                return;
            };
            if !expected.matches(entry) {
                return;
            }
            entry.observed_agents.clear();
            drop_bound_reporter(entry);
            entry.foreground_process_group = None;
            entry.active_agent.clone().map(|active| {
                clear_active_agent(
                    entry,
                    self.procwatch_tombstone_for(&active, now),
                    &self.inner.turns,
                )
            })
        };
        if let Some(info) = updated {
            self.emit(event::SESSION_UPDATED, &info);
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the scan's inputs are read under the session mutex and handed to a blocking thread together"
    )]
    fn collect_procwatch_scan(
        &self,
        id: &SessionId,
        worker_instance_id: &str,
        root: ProcessIdentity,
        own: Option<&OwnRuntime>,
        claimed_reporter: Option<Pid>,
        unwatched_reporter: Option<ProcessIdentity>,
        now: Instant,
    ) -> ScanCollection {
        let exited_reporter = unwatched_reporter.filter(|identity| {
            match self.inner.inspector.is_running(*identity) {
                Ok(running) => !running,
                Err(err) => {
                    debug!(
                        session_id = %id.0,
                        pid = identity.pid,
                        error = %err,
                        "bound reporter liveness read failed; the claim stays"
                    );
                    false
                }
            }
        });
        let descendants = match self.inner.inspector.descendants(root.pid) {
            Ok(facts) => {
                let tree = tree_digest(&facts);
                let scan_pin = own.and_then(OwnRuntime::pinned).cloned();
                let lineage = claimed_reporter
                    .map(|pid| self.scan_lineage(id, worker_instance_id, root, &facts, pid));
                let agents = self.observed_agents_from_facts_for_runtime(
                    id,
                    facts,
                    now,
                    own,
                    Some(worker_instance_id),
                );
                DescendantsScan::Read {
                    scan_pin,
                    refresh: ObservedRefresh { agents, lineage },
                    tree,
                }
            }
            Err(err) => {
                warn!(
                    session_id = %id.0,
                    root_pid = root.pid,
                    error = %err,
                    "failed to inspect session process descendants"
                );
                DescendantsScan::Failed(
                    claimed_reporter.map(|pid| ScanLineage::unavailable(root, pid)),
                )
            }
        };
        let foreground_probe = self.probe_foreground_group(id, root.pid);
        let root_running = self
            .inner
            .inspector
            .is_running(root)
            .map_err(|err| err.to_string());
        ScanCollection {
            exited_reporter,
            descendants,
            foreground_probe,
            root_running,
        }
    }

    fn probe_foreground_group(&self, id: &SessionId, root_pid: Pid) -> ForegroundProbe {
        match self.inner.inspector.foreground_process_group(root_pid) {
            Ok(foreground) => ForegroundProbe::Observed(foreground),
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    root_pid,
                    error = %err,
                    "failed to inspect foreground process group"
                );
                ForegroundProbe::TransientError
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one identity bracket keeps process collection, state projection, and cwd validation ordered"
    )]
    async fn rescan_guarded_procwatch_at(
        &self,
        id: &SessionId,
        expected: &RuntimeWatchIdentity,
        root: ProcessIdentity,
        now: Instant,
    ) -> ScanOutcome {
        let root_pid = root.pid;
        let scan_gate = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .map(|entry| Arc::clone(&entry.procwatch_scan));
        let Some(scan_gate) = scan_gate else {
            return ScanOutcome::stopped();
        };
        let _scan = scan_gate.lock().await;
        if self.procwatch_stop_in_flight(id, expected).await {
            return ScanOutcome::paused();
        }
        // The registry snapshot the scan identifies other runtimes with.
        let scan_registry = self.inner.profiles.runtimes().registry();
        // The claim a scan may prove is the one current before it collects: a
        // claim admitted during or after collection gets no proof from it. A
        // bound claim needs none.
        let (claim_before_scan, claimed_reporter, unwatched_reporter) = self
            .inner
            .sessions
            .lock()
            .await
            .get(id)
            .map(|entry| {
                let bound = bound_reporter_of(entry);
                (
                    claim_key(entry),
                    bound
                        .is_none()
                        .then(|| reported_descendant_pid(entry))
                        .flatten(),
                    bound
                        .filter(|bound| !bound.watched || bound.cancel.is_cancelled())
                        .map(|bound| bound.identity),
                )
            })
            .unwrap_or_default();
        let own = self.inner.sessions.lock().await.get(id).map(OwnRuntime::of);
        // Procfs inspection is synchronous and walks the whole process tree.
        // It runs on a blocking thread, outside the global session mutex, so a
        // slow or permission-gated probe stalls neither the executor nor
        // unrelated RPCs. The facts are collected before the probes that
        // verify them.
        let collection = self
            .off_executor({
                let id = id.clone();
                let worker_instance_id = expected.worker_instance_id.clone();
                move |registry| {
                    registry.collect_procwatch_scan(
                        &id,
                        &worker_instance_id,
                        root,
                        own.as_ref(),
                        claimed_reporter,
                        unwatched_reporter,
                        now,
                    )
                }
            })
            .await;
        let ScanCollection {
            exited_reporter,
            descendants,
            foreground_probe,
            root_running,
        } = collection;
        let mut collection_failed = None;
        let (scan_pin, observed_refresh, tree) = match descendants {
            DescendantsScan::Read {
                scan_pin,
                refresh,
                tree,
            } => (scan_pin, Some(refresh), Some(tree)),
            DescendantsScan::Failed(lineage) => {
                collection_failed = lineage;
                (None, None, None)
            }
        };
        match root_running {
            Ok(true) => {}
            Ok(false) => {
                self.retire_procwatch_root(id, expected, now).await;
                return ScanOutcome::stopped();
            }
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    root_pid,
                    error = %err,
                    "failed to close procwatch root identity bracket"
                );
                return ScanOutcome::unchanged();
            }
        }
        let mut exit_watches = match observed_refresh.as_ref() {
            Some(observed) => {
                let existing = self.existing_observed_identities(id).await;
                self.open_exit_watches(id, observed.agents.values(), &existing)
            }
            None => HashMap::new(),
        };
        // A scan whose claim changes before it commits binds nothing, and the
        // reporter watch it armed is dropped unused. The hold parks a scan at
        // this point, but only one that opened an observed-agent watch.
        #[cfg(test)]
        if !exit_watches.is_empty() {
            self.hold_input(id, "observed_watch_open").await;
        }

        let (to_spawn, updated, focus, foreground_group, foreground_changed, quiet) = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, "procwatch rescan for unknown session");
                return ScanOutcome::stopped();
            };
            if !expected.matches(entry) {
                return ScanOutcome::stopped();
            }
            if is_terminal(entry.info.state) {
                return ScanOutcome::stopped();
            }
            if entry.stopping {
                return ScanOutcome::paused();
            }
            // A package change replaced a definition this scan matched with:
            // the session's own pin, or the registry other runtimes resolve
            // through. The next scan observes again.
            let current_registry = self.inner.profiles.runtimes().registry();
            let observed_refresh = match observed_refresh {
                Some(_)
                    if !same_pin(entry.pinned.as_ref(), scan_pin.as_ref())
                        || !Arc::ptr_eq(&scan_registry, &current_registry) =>
                {
                    entry.procwatch_rescan.request();
                    None
                }
                observed_refresh => observed_refresh,
            };

            let (agents, lineage) = match observed_refresh {
                Some(ObservedRefresh { agents, lineage }) => (Some(agents), lineage),
                None => (None, None),
            };
            let lineage = lineage.or(collection_failed);
            let to_spawn = apply_observed_refresh(entry, agents, &mut exit_watches);
            // A claim admitted after this scan began was not observed by it:
            // the scan neither refutes nor proves it, and the rescan its
            // admission requested decides.
            let claim_admitted_during_scan = claim_key(entry) != claim_before_scan;
            // A reporter read gone ends its claim here, whatever the
            // foreground probe says.
            let dead_reporter = bound_reporter_of(entry)
                .map(|bound| bound.identity)
                .filter(|identity| Some(*identity) == exited_reporter);
            if dead_reporter.is_some() || bound_reporter_of(entry).is_none() {
                drop_bound_reporter(entry);
            }
            let retired = dead_reporter.and_then(|identity| {
                forget_observed_generation(entry, identity);
                self.reconcile_active_agent(entry, now, None)
            });
            if !claim_admitted_during_scan {
                self.bind_proven_reporter(id, entry, lineage.as_ref());
            }

            let foreground_changed = apply_foreground_probe(entry, foreground_probe);
            let decision = if claim_admitted_during_scan {
                ForegroundDecision::Preserve
            } else {
                self.reconcile_foreground_agent(
                    entry,
                    root_pid,
                    now,
                    foreground_probe,
                    lineage.as_ref(),
                )
            };
            let updated = match decision {
                ForegroundDecision::Fallback => {
                    self.reconcile_active_agent(entry, now, lineage.as_ref())
                }
                ForegroundDecision::Preserve => None,
                ForegroundDecision::Updated => Some(entry.info.clone()),
            }
            .or(retired);
            let focus = entry
                .active_agent
                .as_ref()
                .and_then(|active| {
                    let pid = active.pid?;
                    let start_identity = active.start_identity.or_else(|| {
                        entry
                            .observed_agents
                            .iter()
                            .find(|observed| observed.pid == pid)
                            .map(|observed| observed.start_identity)
                    })?;
                    Some((pid, start_identity))
                })
                .map_or(root, |(pid, start_identity)| ProcessIdentity {
                    pid,
                    start_identity: StartIdentity::new(start_identity),
                });
            (
                to_spawn,
                updated,
                focus,
                entry.foreground_process_group,
                foreground_changed,
                scan_is_quiet(entry),
            )
        };
        let changed = foreground_changed || updated.is_some() || !to_spawn.is_empty();
        let outcome = ScanOutcome {
            alive: true,
            tree,
            changed,
            quiet,
            paused: false,
        };

        for (identity, watch, cancel) in to_spawn {
            self.spawn_observed_exit_watch(id.clone(), identity, watch, cancel);
        }
        if foreground_changed || updated.is_some() {
            debug!(
                session_id = %id.0,
                root_pid,
                focus_pid = focus.pid,
                foreground_pgid = foreground_group,
                "session process focus reconciled"
            );
        }
        if let Some(info) = updated {
            self.emit(event::SESSION_UPDATED, &info);
        }
        // The focus process is re-identified around the cwd read so a reused
        // pid never lends its cwd to the session.
        let observed_at = crate::time::now();
        let focus_cwd = self
            .off_executor(move |registry| {
                let inspector = &registry.inner.inspector;
                let current = || {
                    inspector
                        .identity(focus.pid)
                        .is_ok_and(|current| current == Some(focus))
                };
                if !current() {
                    return None;
                }
                Some(
                    inspector
                        .cwd(focus.pid)
                        .map(|cwd| (cwd, current()))
                        .map_err(|err| err.to_string()),
                )
            })
            .await;
        match focus_cwd {
            Some(Ok((cwd, true))) => {
                self.apply_cwd_change(id, cwd, CwdSource::Procwatch, Some(expected), observed_at)
                    .await;
            }
            Some(Err(err)) => {
                debug!(
                    session_id = %id.0,
                    focus_pid = focus.pid,
                    error = %err,
                    "failed to inspect focus process cwd"
                );
            }
            Some(Ok((_, false))) | None => {}
        }
        outcome
    }

    #[cfg(test)]
    pub(super) async fn rescan_procwatch_at(&self, id: &SessionId, root_pid: Pid, now: Instant) {
        let expected = {
            let sessions = self.inner.sessions.lock().await;
            sessions
                .get(id)
                .and_then(|entry| RuntimeWatchIdentity::from_info(&entry.info))
                .expect("test procwatch session has a live runtime identity")
        };
        let root = self
            .inner
            .inspector
            .identity(root_pid)
            .expect("test procwatch root identity probe")
            .expect("test procwatch root process");
        let _ = self
            .rescan_guarded_procwatch_at(id, &expected, root, now)
            .await;
    }

    pub(super) async fn on_observed_agent_exit(&self, id: &SessionId, identity: ObservedIdentity) {
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                debug!(session_id = %id.0, pid = identity.0, "procwatch exit for unknown session");
                return;
            };
            if entry.stopping || is_terminal(entry.info.state) {
                return;
            }

            let before = entry.observed_agents.len();
            entry
                .observed_agents
                .retain(|observed| (observed.pid, observed.start_identity) != identity);
            if before == entry.observed_agents.len() {
                return;
            }

            self.reconcile_without_scan(entry)
        };

        if let Some(info) = updated {
            self.emit(event::SESSION_UPDATED, &info);
        }
    }

    /// Reconciles the claim against the recorded foreground and observed
    /// agents, with no scan evidence.
    fn reconcile_without_scan(&self, entry: &mut SessionEntry) -> Option<SessionInfo> {
        let now = crate::time::now();
        match self.reconcile_foreground_agent(
            entry,
            entry.info.pid,
            now,
            ForegroundProbe::Observed(entry.foreground_process_group),
            None,
        ) {
            ForegroundDecision::Fallback => self.reconcile_active_agent(entry, now, None),
            ForegroundDecision::Preserve => None,
            ForegroundDecision::Updated => Some(entry.info.clone()),
        }
    }

    /// The exit of the bound reporter generation `identity` ends its claim;
    /// the observed agents then decide as without the claim. An exit of any
    /// other generation leaves the claim alone.
    pub(super) async fn on_bound_reporter_exit(&self, id: &SessionId, identity: ProcessIdentity) {
        let updated = {
            let mut sessions = self.inner.sessions.lock().await;
            let Some(entry) = sessions.get_mut(id) else {
                return;
            };
            if is_terminal(entry.info.state) {
                return;
            }
            if entry.stopping {
                // The watch task has ended; the first scan after the stop
                // reads the generation's liveness.
                if let Some(bound) = entry
                    .bound_reporter
                    .as_mut()
                    .filter(|bound| bound.identity == identity)
                {
                    bound.watched = false;
                }
                return;
            }
            if entry
                .bound_reporter
                .as_ref()
                .is_none_or(|bound| bound.identity != identity)
            {
                return;
            }
            let names_claim = bound_reporter_of(entry).is_some();
            drop_bound_reporter(entry);
            if !names_claim {
                return;
            }
            forget_observed_generation(entry, identity);
            self.reconcile_without_scan(entry)
        };

        if let Some(info) = updated {
            self.emit(event::SESSION_UPDATED, &info);
        }
    }

    /// A failed exit watch leaves the generation's liveness to the scans.
    async fn on_bound_reporter_watch_failed(&self, id: &SessionId, identity: ProcessIdentity) {
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(bound) = sessions
            .get_mut(id)
            .and_then(|entry| entry.bound_reporter.as_mut())
            .filter(|bound| bound.identity == identity)
        {
            bound.watched = false;
        }
    }

    fn spawn_bound_reporter_watch(
        &self,
        id: SessionId,
        identity: ProcessIdentity,
        watch: ExitWatch,
        cancel: CancellationToken,
    ) {
        let registry = self.clone();
        let gate = self.inner.settle.gate(&id);
        self.spawn_cooperative("bound_reporter_exit_watch", async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                result = watch.wait() => {
                    // The delivered exit is pending input until it is applied.
                    let _applying = gate.begin();
                    #[cfg(test)]
                    registry.hold_input(&id, "exit_watch").await;
                    match result {
                    Ok(()) => registry.on_bound_reporter_exit(&id, identity).await,
                    Err(err) => {
                        debug!(
                            session_id = %id.0,
                            pid = identity.pid,
                            error = %err,
                            "bound reporter exit watch failed; scans read its liveness"
                        );
                        registry.on_bound_reporter_watch_failed(&id, identity).await;
                    }
                    }
                }
            }
        });
    }

    #[cfg(test)]
    pub(super) fn observed_agents_from_facts(
        &self,
        id: &SessionId,
        facts: Vec<ProcessFact>,
        now: Instant,
        own: Option<&OwnRuntime>,
    ) -> HashMap<Pid, ObservedAgent> {
        self.observed_agents_from_facts_for_runtime(id, facts, now, own, None)
    }

    fn observed_agents_from_facts_for_runtime(
        &self,
        id: &SessionId,
        facts: Vec<ProcessFact>,
        now: Instant,
        own: Option<&OwnRuntime>,
        worker_instance_id: Option<&str>,
    ) -> HashMap<Pid, ObservedAgent> {
        let (pinned, unserved) = match own {
            Some(OwnRuntime::Pinned(definition)) => (Some(definition), None),
            Some(OwnRuntime::Unserved(base)) => (None, Some(base)),
            None => (None, None),
        };
        facts
            .into_iter()
            .filter_map(|fact| {
                // The session's own runtime is recognized by the process
                // matchers it was launched with, also when the registry serves
                // another source for its runtime id.
                let own = pinned.filter(|definition| {
                    definition
                        .manifest()
                        .process_matchers()
                        .is_some_and(|matchers| matchers.matches(&fact))
                });
                let definition = match own {
                    Some(definition) => Arc::clone(definition),
                    None => identify_definition(self.inner.profiles.runtimes(), &fact)?,
                };
                let agent_base = RuntimeRef::from(definition.runtime_id().clone());
                if unserved == Some(&agent_base) {
                    return None;
                }
                if self.is_foreign_owned_agent(id, fact.pid, worker_instance_id) {
                    return None;
                }
                let cwd = self.inner.inspector.cwd(fact.pid).ok();
                Some((
                    fact.pid,
                    ObservedAgent {
                        pid: fact.pid,
                        pgid: fact.pgid,
                        start_identity: fact.start_identity.get(),
                        agent_base,
                        definition: MatchedDefinition(definition),
                        first_seen: now,
                        cwd,
                    },
                ))
            })
            .collect()
    }

    /// Rejects agents owned by another session or worker runtime.
    ///
    /// A live worker keeps its launch daemon ID after daemon restart. Its
    /// worker instance marker remains stable, while a nested daemon's worker
    /// has a different marker. A stale daemon ID is accepted only when both
    /// the session and worker instance markers match. Missing or unreadable
    /// markers retain the established observation behavior.
    fn is_foreign_owned_agent(
        &self,
        id: &SessionId,
        pid: Pid,
        worker_instance_id: Option<&str>,
    ) -> bool {
        let markers = match self.inner.inspector.ownership_markers(pid) {
            Ok(markers) => markers,
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    pid,
                    error = %err,
                    "failed to read process ownership markers; keeping it observable"
                );
                return false;
            }
        };
        let foreign_session = markers
            .session_id
            .as_deref()
            .is_some_and(|session_id| session_id != id.0);
        if foreign_session {
            return true;
        }
        if let Some(expected) = worker_instance_id {
            match markers.worker_instance_id.as_deref() {
                Some(actual) if actual != expected => return true,
                Some(_) if markers.session_id.as_deref() == Some(id.0.as_str()) => {
                    return false;
                }
                Some(_) | None => {}
            }
        }
        markers
            .daemon_id
            .as_deref()
            .is_some_and(|daemon_id| daemon_id != self.inner.daemon_instance_id)
    }

    async fn existing_observed_identities(&self, id: &SessionId) -> HashSet<ObservedIdentity> {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .get(id)
            .map(|entry| {
                entry
                    .observed_agents
                    .iter()
                    .map(|observed| (observed.pid, observed.start_identity))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_exit_watches<'a>(
        &self,
        id: &SessionId,
        observed: impl Iterator<Item = &'a ObservedAgent>,
        existing: &HashSet<ObservedIdentity>,
    ) -> HashMap<ObservedIdentity, ExitWatch> {
        let mut watches = HashMap::new();
        for observation in observed {
            let identity = (observation.pid, observation.start_identity);
            if existing.contains(&identity) {
                continue;
            }
            let process_identity = ProcessIdentity {
                pid: observation.pid,
                start_identity: StartIdentity::new(observation.start_identity),
            };
            match self.inner.inspector.exit_watch(process_identity) {
                Ok(watch) => {
                    watches.insert(identity, watch);
                }
                Err(err) => {
                    debug!(
                        session_id = %id.0,
                        pid = observation.pid,
                        process_start_identity = observation.start_identity,
                        error = %err,
                        "failed to arm process exit watch; falling back to poll cleanup"
                    );
                }
            }
        }
        watches
    }

    fn spawn_observed_exit_watch(
        &self,
        id: SessionId,
        identity: ObservedIdentity,
        watch: ExitWatch,
        cancel: CancellationToken,
    ) {
        let registry = self.clone();
        let gate = self.inner.settle.gate(&id);
        self.spawn_cooperative("observed_exit_watch", async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                result = watch.wait() => {
                    let _applying = gate.begin();
                    #[cfg(test)]
                    registry.hold_input(&id, "exit_watch").await;
                    if let Err(err) = result {
                        debug!(
                            session_id = %id.0,
                            pid = identity.0,
                            process_start_identity = identity.1,
                            error = %err,
                            "process exit watch failed"
                        );
                        return;
                    }
                    registry.on_observed_agent_exit(&id, identity).await;
                }
            }
        });
    }

    fn reconcile_active_agent(
        &self,
        entry: &mut SessionEntry,
        now: Instant,
        lineage: Option<&ScanLineage>,
    ) -> Option<SessionInfo> {
        let Some(active) = entry.active_agent.clone() else {
            let observed = choose_observed_agent(entry)?;
            return self.auto_report_observed_agent(entry, &observed, now);
        };
        let Some(active_base) = entry.info.active_agent_base.clone() else {
            return Some(clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
                &self.inner.turns,
            ));
        };

        if let Some(pid) = active.pid {
            if pid == entry.info.pid {
                // SAFETY/why: descendant scans intentionally exclude the PTY-root
                // process. While the session is running, `record_exit` owns root
                // liveness and clears `active_agent` when that process exits.
                return None;
            }
            if observed_process_matches(entry, pid, active.start_identity, &active_base) {
                return None;
            }
            if self.keep_reported_descendant(entry, lineage, now, &ReporterAnchor::Any) {
                return None;
            }
            if let Some(observed) = first_observed_agent_for_base(entry, &active_base) {
                return self.auto_report_observed_agent(entry, &observed, now);
            }
            if let Some(observed) = choose_observed_agent(entry) {
                return self.auto_report_observed_agent(entry, &observed, now);
            }
            return Some(clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
                &self.inner.turns,
            ));
        }

        if let Some(observed) = single_observed_for_base(entry, &active_base) {
            if let Some(active) = entry.active_agent.as_mut() {
                active.pid = Some(observed.pid);
                active.start_identity = Some(observed.start_identity);
            }
            entry.info.active_agent_pid = Some(observed.pid);
            entry.info.updated_at = timestamp_now();
            return Some(entry.info.clone());
        }

        if now.saturating_duration_since(active.reported_at)
            >= self.inner.config.active_agent_claim_ttl
        {
            return Some(clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
                &self.inner.turns,
            ));
        }

        None
    }

    fn auto_report_observed_agent(
        &self,
        entry: &mut SessionEntry,
        observed: &ObservedAgent,
        now: Instant,
    ) -> Option<SessionInfo> {
        let agent = agent_kind_label(&observed.agent_base).to_owned();
        let seq = Some(self.next_procwatch_seq());
        if !report_is_current(
            entry.last_agent_report.as_ref(),
            PROCWATCH_SOURCE,
            &agent,
            seq,
        ) {
            return None;
        }

        let report = ActiveAgentReport {
            source: PROCWATCH_SOURCE.to_owned(),
            agent: agent.clone(),
            seq,
            pid: Some(observed.pid),
            start_identity: Some(observed.start_identity),
            reported_at: now,
            activity_reported: false,
            observed: true,
            origin: ClaimOrigin::memory(),
        };
        Some(apply_observed_transition(
            entry,
            observed,
            report,
            agent,
            &self.inner.turns,
        ))
    }

    fn procwatch_tombstone_for(
        &self,
        active: &ActiveAgentReport,
        now: Instant,
    ) -> ActiveAgentReport {
        let seq = self.next_procwatch_seq();
        ActiveAgentReport {
            source: active.source.clone(),
            agent: active.agent.clone(),
            seq: Some(active.seq.map_or(seq, |active_seq| active_seq.max(seq))),
            pid: None,
            start_identity: None,
            reported_at: now,
            activity_reported: false,
            observed: active.observed,
            origin: ClaimOrigin::memory(),
        }
    }

    /// Whether a session launched as `agent_base` runs an agent as its root
    /// process; a host-shell runtime only hosts agents, and a kind without an
    /// installed definition is treated as an agent.
    pub(super) fn launch_root_is_agent(&self, agent_base: &RuntimeRef) -> bool {
        self.inner
            .profiles
            .runtimes()
            .resolve_ref(agent_base)
            .map_or(true, |definition| {
                !matches!(definition.program(), LaunchProgram::HostShell(_))
            })
    }

    fn reconcile_foreground_agent(
        &self,
        entry: &mut SessionEntry,
        root_pid: Pid,
        now: Instant,
        probe: ForegroundProbe,
        lineage: Option<&ScanLineage>,
    ) -> ForegroundDecision {
        let ForegroundProbe::Observed(Some(foreground_group)) = probe else {
            return match probe {
                ForegroundProbe::Observed(None) => ForegroundDecision::Fallback,
                ForegroundProbe::TransientError => ForegroundDecision::Preserve,
                ForegroundProbe::Observed(Some(_)) => {
                    unreachable!("foreground group was matched by the enclosing pattern")
                }
            };
        };

        if foreground_group == root_pid {
            let active_is_launch_root = entry.active_agent.as_ref().is_some_and(|active| {
                active.pid == Some(root_pid)
                    && self.launch_root_is_agent(&entry.info.agent_base)
                    && entry.info.active_agent_base.as_ref() == Some(&entry.info.agent_base)
            });
            if active_is_launch_root {
                return ForegroundDecision::Preserve;
            }
        }

        if let Some(observed) = choose_foreground_observed(entry, foreground_group) {
            if bind_matching_observed_identity(entry, &observed) {
                return ForegroundDecision::Preserve;
            }
            if self.keep_reported_descendant(
                entry,
                lineage,
                now,
                &ReporterAnchor::Observed(observed.clone()),
            ) {
                return ForegroundDecision::Preserve;
            }
            let agent = agent_kind_label(&observed.agent_base).to_owned();
            let report = ActiveAgentReport {
                source: PROCWATCH_SOURCE.to_owned(),
                agent: agent.clone(),
                seq: Some(self.next_procwatch_seq()),
                pid: Some(observed.pid),
                start_identity: Some(observed.start_identity),
                reported_at: now,
                activity_reported: false,
                observed: true,
                origin: ClaimOrigin::memory(),
            };
            let _ = apply_observed_transition(entry, &observed, report, agent, &self.inner.turns);
            return ForegroundDecision::Updated;
        }

        if foreground_group == root_pid {
            if self.keep_reported_descendant(entry, lineage, now, &ReporterAnchor::LaunchRoot) {
                return ForegroundDecision::Preserve;
            }
            let Some(active) = entry.active_agent.clone() else {
                return ForegroundDecision::Preserve;
            };
            if unbound_claim_is_current(&active, now, self.inner.config.active_agent_claim_ttl) {
                return ForegroundDecision::Preserve;
            }
            let _ = clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
                &self.inner.turns,
            );
            return ForegroundDecision::Updated;
        }

        let Some(active) = entry.active_agent.clone() else {
            return ForegroundDecision::Preserve;
        };
        let active_is_live = active.pid == Some(entry.info.pid)
            || entry
                .info
                .active_agent_base
                .as_ref()
                .is_some_and(|active_base| {
                    active.pid.is_some_and(|pid| {
                        observed_process_matches(entry, pid, active.start_identity, active_base)
                    })
                });
        let unbound_is_current =
            unbound_claim_is_current(&active, now, self.inner.config.active_agent_claim_ttl);
        if active_is_live
            || unbound_is_current
            || self.keep_reported_descendant(entry, lineage, now, &ReporterAnchor::Any)
        {
            ForegroundDecision::Preserve
        } else {
            let _ = clear_active_agent(
                entry,
                self.procwatch_tombstone_for(&active, now),
                &self.inner.turns,
            );
            ForegroundDecision::Updated
        }
    }

    /// Reads the ancestry of `facts` and, for the reporter `pid` of the
    /// session's reported claim, whether it runs now. A running reporter gets
    /// its exit watch armed here, outside the session mutex.
    fn scan_lineage(
        &self,
        id: &SessionId,
        worker_instance_id: &str,
        root: ProcessIdentity,
        facts: &[ProcessFact],
        pid: Pid,
    ) -> ScanLineage {
        let mut processes: HashMap<Pid, LineageEntry> = facts
            .iter()
            .map(|fact| {
                (
                    fact.pid,
                    LineageEntry {
                        identity: fact.identity(),
                        parent: fact.ppid,
                    },
                )
            })
            .collect();
        processes.insert(
            root.pid,
            LineageEntry {
                identity: root,
                parent: root.pid,
            },
        );
        let mut inconclusive = false;
        let mut reporter_watch = None;
        let reporter = facts
            .iter()
            .find(|fact| fact.pid == pid)
            .map(ProcessFact::identity)
            .filter(|identity| {
                if self.is_foreign_owned_agent(id, pid, Some(worker_instance_id)) {
                    return false;
                }
                match self.read_reporter_liveness(id, *identity) {
                    ReporterLiveness::Running { watch } => {
                        reporter_watch = watch;
                        true
                    }
                    ReporterLiveness::Gone => false,
                    ReporterLiveness::Unknown => {
                        inconclusive = true;
                        false
                    }
                }
            });
        let mut foreign = HashSet::new();
        if reporter.is_some() {
            let mut current = processes.get(&pid).map(|entry| entry.parent);
            // Bounded like the ancestry walk: a cyclic table ends the loop.
            for _ in 0..processes.len() {
                let Some(ancestor) = current.filter(|ancestor| *ancestor != root.pid) else {
                    break;
                };
                if self.is_foreign_owned_agent(id, ancestor, Some(worker_instance_id)) {
                    foreign.insert(ancestor);
                }
                // An ancestor missing from the scan leaves the ancestry open.
                current = processes.get(&ancestor).map(|entry| entry.parent);
                inconclusive |= current.is_none();
            }
        }
        ScanLineage {
            root,
            claimed: pid,
            processes,
            reporter,
            foreign,
            inconclusive,
            reporter_watch: std::sync::Mutex::new(reporter_watch),
        }
    }

    /// Reads whether `identity` runs and arms an exit watch for it. A watch
    /// that cannot be armed leaves the reporter to per-scan liveness reads,
    /// unless the process is already gone.
    fn read_reporter_liveness(
        &self,
        id: &SessionId,
        identity: ProcessIdentity,
    ) -> ReporterLiveness {
        let running = |context: &'static str| match self.inner.inspector.is_running(identity) {
            Ok(running) => Some(running),
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    pid = identity.pid,
                    error = %err,
                    context,
                    "reported agent liveness read failed; the claim gets no scan proof"
                );
                None
            }
        };
        match running("scan") {
            Some(true) => {}
            Some(false) => return ReporterLiveness::Gone,
            None => return ReporterLiveness::Unknown,
        }
        match self.inner.inspector.exit_watch(identity) {
            Ok(watch) => ReporterLiveness::Running { watch: Some(watch) },
            Err(err) => {
                debug!(
                    session_id = %id.0,
                    pid = identity.pid,
                    error = %err,
                    "failed to arm the reported agent exit watch; scans read its liveness"
                );
                match running("after watch failure") {
                    Some(true) => ReporterLiveness::Running { watch: None },
                    Some(false) => ReporterLiveness::Gone,
                    None => ReporterLiveness::Unknown,
                }
            }
        }
    }

    /// Binds the reporter of the session's reported claim when this scan
    /// proves it: the reporter runs, still has the claim's start identity, and
    /// descends from a same-runtime anchor through processes of this scan
    /// without passing an agent of another runtime or another session's
    /// process. Binds the reporter's own start identity to the claim. Any
    /// other case leaves the claim unbound.
    fn bind_proven_reporter(
        &self,
        id: &SessionId,
        entry: &mut SessionEntry,
        lineage: Option<&ScanLineage>,
    ) {
        let Some((lineage, reporter)) =
            lineage.and_then(|lineage| Some((lineage, lineage.reporter?)))
        else {
            return;
        };
        if bound_reporter_of(entry).is_some() {
            return;
        }
        let Some(active) = entry
            .active_agent
            .as_ref()
            .filter(|active| !active.observed && active.pid == Some(reporter.pid))
        else {
            return;
        };
        if active
            .start_identity
            .is_some_and(|start| start != reporter.start_identity.get())
        {
            return;
        }
        let Some(base) = entry.info.active_agent_base.clone() else {
            return;
        };
        let (observed_anchors, root_anchor) =
            self.reporter_anchors(entry, lineage, reporter, &base);
        if observed_anchors.is_empty() && !root_anchor {
            return;
        }
        if !bind_report_start_identity(entry, reporter.start_identity.get()) {
            return;
        }
        let watch = lineage
            .reporter_watch
            .lock()
            .ok()
            .and_then(|mut watch| watch.take());
        drop_bound_reporter(entry);
        let cancel = entry.procwatch_cancel.child_token();
        let watched = watch.is_some();
        if let Some(watch) = watch {
            self.spawn_bound_reporter_watch(id.clone(), reporter, watch, cancel.clone());
        }
        entry.bound_reporter = Some(BoundReporter {
            identity: reporter,
            base,
            observed_anchors,
            root_anchor,
            watched,
            cancel,
        });
    }

    /// The same-runtime agents `reporter` descends from in this scan: observed
    /// agents, and whether the launch root is one. The walk ends at a process
    /// another session owns or an agent of another runtime.
    fn reporter_anchors(
        &self,
        entry: &SessionEntry,
        lineage: &ScanLineage,
        reporter: ProcessIdentity,
        base: &RuntimeRef,
    ) -> (Vec<ProcessIdentity>, bool) {
        let launch_root = (self.launch_root_is_agent(&entry.info.agent_base)
            && &entry.info.agent_base == base)
            .then_some(lineage.root);
        let mut observed_anchors = Vec::new();
        let mut root_anchor = false;
        let mut process = lineage.processes.get(&reporter.pid);
        // A malformed or cyclic process table never proves ancestry.
        for step in 0..lineage.processes.len() {
            let Some(current) = process else {
                break;
            };
            let identity = current.identity;
            if lineage.foreign.contains(&identity.pid) {
                break;
            }
            // A claim is not carried through a different recognized runtime.
            if entry
                .observed_agents
                .iter()
                .any(|observed| observed.pid == identity.pid && &observed.agent_base != base)
            {
                break;
            }
            // The reporter itself is no anchor of its claim.
            if step > 0 {
                if launch_root == Some(identity) {
                    root_anchor = true;
                } else if entry.observed_agents.iter().any(|observed| {
                    &observed.agent_base == base
                        && identity.pid == observed.pid
                        && identity.start_identity.get() == observed.start_identity
                }) {
                    observed_anchors.push(identity);
                }
            }
            if identity.pid == lineage.root.pid {
                break;
            }
            process = lineage.processes.get(&current.parent);
        }
        (observed_anchors, root_anchor)
    }

    /// Whether the reported claim of a process that is not an observed agent
    /// stays instead of main's replacement: its bound reporter descends from
    /// `anchor`, or this scan could not decide the reporter's ancestry and the
    /// unbound claim is still younger than the claim TTL.
    fn keep_reported_descendant(
        &self,
        entry: &SessionEntry,
        lineage: Option<&ScanLineage>,
        now: Instant,
        anchor: &ReporterAnchor,
    ) -> bool {
        if let Some(bound) = bound_reporter_of(entry) {
            return bound.anchored_at(anchor);
        }
        lineage.is_some_and(|lineage| {
            lineage.inconclusive && reported_descendant_pid(entry) == Some(lineage.claimed)
        }) && entry.active_agent.as_ref().is_some_and(|active| {
            now.saturating_duration_since(active.reported_at)
                < self.inner.config.active_agent_claim_ttl
        })
    }

    fn next_procwatch_seq(&self) -> u64 {
        self.inner
            .procwatch_seq
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }
}

/// Removes the observation of the exact process generation `identity`: its
/// exit is established, so no claim may stand on it.
fn forget_observed_generation(entry: &mut SessionEntry, identity: ProcessIdentity) {
    entry.observed_agents.retain(|observed| {
        (observed.pid, observed.start_identity) != (identity.pid, identity.start_identity.get())
    });
}

/// The identity of the claim a scan reads: its source, agent, sequence,
/// process, the process generation and origin.
type ClaimKey = (
    String,
    String,
    Option<u64>,
    Option<Pid>,
    Option<u64>,
    bool,
    Option<super::claim::ClaimKey>,
);

fn claim_key(entry: &SessionEntry) -> Option<ClaimKey> {
    entry.active_agent.as_ref().map(|active| {
        (
            active.source.clone(),
            active.agent.clone(),
            active.seq,
            active.pid,
            active.start_identity,
            active.observed,
            active.origin.key().cloned(),
        )
    })
}

fn unbound_claim_is_current(
    active: &ActiveAgentReport,
    now: Instant,
    ttl: std::time::Duration,
) -> bool {
    active.pid.is_none() && now.saturating_duration_since(active.reported_at) < ttl
}

fn apply_observed_refresh(
    entry: &mut SessionEntry,
    observed_refresh: Option<HashMap<Pid, ObservedAgent>>,
    exit_watches: &mut HashMap<ObservedIdentity, ExitWatch>,
) -> Vec<(ObservedIdentity, ExitWatch, CancellationToken)> {
    let Some(observed) = observed_refresh else {
        return Vec::new();
    };
    let observed_pids = observed.keys().copied().collect::<HashSet<_>>();
    entry
        .observed_agents
        .retain(|agent| observed_pids.contains(&agent.pid));

    let mut to_spawn = Vec::new();
    for observation in observed.into_values() {
        if let Some(existing) = entry
            .observed_agents
            .iter_mut()
            .find(|agent| agent.pid == observation.pid)
        {
            let identity = (observation.pid, observation.start_identity);
            let identity_changed = existing.refresh_from(observation);
            if identity_changed {
                if let Some(watch) = exit_watches.remove(&identity) {
                    to_spawn.push((identity, watch, entry.procwatch_cancel.clone()));
                }
            }
            continue;
        }

        let identity = (observation.pid, observation.start_identity);
        entry.observed_agents.push(observation);
        if let Some(watch) = exit_watches.remove(&identity) {
            to_spawn.push((identity, watch, entry.procwatch_cancel.clone()));
        }
    }
    to_spawn
}

fn choose_observed_agent(entry: &SessionEntry) -> Option<ObservedAgent> {
    entry
        .observed_agents
        .iter()
        .min_by_key(|observed| (observed.first_seen, observed.pid))
        .cloned()
}

fn apply_foreground_probe(entry: &mut SessionEntry, probe: ForegroundProbe) -> bool {
    let ForegroundProbe::Observed(foreground_group) = probe else {
        return false;
    };
    if entry.foreground_process_group == foreground_group {
        return false;
    }
    entry.foreground_process_group = foreground_group;
    true
}

fn choose_foreground_observed(
    entry: &SessionEntry,
    foreground_group: Pid,
) -> Option<ObservedAgent> {
    entry
        .observed_agents
        .iter()
        .find(|observed| observed.pid == foreground_group)
        .cloned()
        .or_else(|| {
            entry
                .observed_agents
                .iter()
                .filter(|observed| observed.pgid == foreground_group)
                .min_by_key(|observed| (observed.first_seen, observed.pid))
                .cloned()
        })
}

/// The pid of the session's hook-reported claim when it names a process below
/// the launch root.
fn reported_descendant_pid(entry: &SessionEntry) -> Option<Pid> {
    entry
        .active_agent
        .as_ref()
        .filter(|active| !active.observed)
        .and_then(|active| active.pid)
        .filter(|pid| *pid != entry.info.pid)
}

fn bind_matching_observed_identity(entry: &mut SessionEntry, observed: &ObservedAgent) -> bool {
    let Some(active) = entry.active_agent.as_ref() else {
        return false;
    };
    if active.pid != Some(observed.pid)
        || entry.info.active_agent_base.as_ref() != Some(&observed.agent_base)
    {
        return false;
    }
    bind_report_start_identity(entry, observed.start_identity)
}

/// Binds `start_identity` to the active claim, or confirms the one it carries.
fn bind_report_start_identity(entry: &mut SessionEntry, start_identity: u64) -> bool {
    let Some(active) = entry.active_agent.as_ref() else {
        return false;
    };
    if let Some(claimed_start) = active.start_identity {
        return claimed_start == start_identity;
    }

    let claim = (
        active.source.clone(),
        active.agent.clone(),
        active.seq,
        active.pid,
    );
    entry
        .active_agent
        .as_mut()
        .expect("active report was checked above")
        .start_identity = Some(start_identity);
    if let Some(last) = entry.last_agent_report.as_mut() {
        let last_claim = (last.source.clone(), last.agent.clone(), last.seq, last.pid);
        if last.start_identity.is_none() && last_claim == claim {
            last.start_identity = Some(start_identity);
        }
    }
    true
}

pub(super) fn apply_observed_transition(
    entry: &mut SessionEntry,
    observed: &ObservedAgent,
    report: ActiveAgentReport,
    agent: String,
    turns: &TurnEvents,
) -> SessionInfo {
    if entry
        .active_agent
        .as_ref()
        .is_none_or(|active| report.replaces(active))
    {
        entry.agent_left(turns);
    }
    let report_activity_was_current = entry
        .active_agent
        .as_ref()
        .is_some_and(|active| active.activity_reported);
    entry.active_agent = Some(report.clone());
    entry.last_agent_report = Some(report);
    entry.info.active_agent = Some(agent);
    entry.info.active_agent_base = Some(observed.agent_base.clone());
    entry.info.active_agent_pid = Some(observed.pid);
    entry.info.active_agent_session_id = None;
    entry.info.active_agent_session_path = None;
    if report_activity_was_current || entry.info.state_source == protocol::StateSource::OscProgress
    {
        entry.info.activity = None;
        entry.info.state_source = protocol::StateSource::Process;
    }
    let config = DetectorConfig::for_definition(&observed.definition.0);
    send_detector_config(entry, config);
    entry.info.updated_at = timestamp_now();
    entry.info.clone()
}

fn first_observed_agent_for_base(
    entry: &SessionEntry,
    agent_base: &RuntimeRef,
) -> Option<ObservedAgent> {
    entry
        .observed_agents
        .iter()
        .filter(|observed| &observed.agent_base == agent_base)
        .min_by_key(|observed| (observed.first_seen, observed.pid))
        .cloned()
}

fn single_observed_for_base(
    entry: &SessionEntry,
    agent_base: &RuntimeRef,
) -> Option<ObservedAgent> {
    let mut matching = entry
        .observed_agents
        .iter()
        .filter(|observed| &observed.agent_base == agent_base);
    let first = matching.next()?;
    matching.next().is_none().then(|| first.clone())
}

fn observed_process_matches(
    entry: &SessionEntry,
    pid: Pid,
    start_identity: Option<u64>,
    agent_base: &RuntimeRef,
) -> bool {
    entry.observed_agents.iter().any(|observed| {
        observed.pid == pid
            && &observed.agent_base == agent_base
            && start_identity.is_none_or(|identity| observed.start_identity == identity)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use pohunek_test_support::wait::wait_until;
    use protocol::{ReportSequence, SessionNewParams, SessionReportAgentParams};

    use crate::session::{SessionRegistry, SessionRegistryConfig, ShellCommand};

    /// Interval of the procwatch loop under test.
    const TEST_PROCWATCH_POLL: Duration = Duration::from_millis(200);
    /// Lifetime of an unbound agent claim in the registry under test.
    const TEST_CLAIM_TTL: Duration = Duration::from_secs(2);
    /// Hook source and agent profile of the reported claim.
    const TEST_AGENT: &str = "codex";
    const TEST_AGENT_SOURCE: &str = "pohunek:codex";
    /// Hook sequence of the reported claim.
    const TEST_REPORT_SEQ: u64 = 1_000;
    /// Scheduler yields that let the procwatch task run its ticks while the
    /// claim is still younger than its TTL.
    const TEST_YIELD_BUDGET: usize = 256;

    /// The procwatch loop must read tokio's clock: an unbound claim older than
    /// its TTL on the paused clock is released by a loop tick, which a loop
    /// reading `std::time::Instant::now()` never does while time is virtual.
    #[tokio::test]
    async fn procwatch_loop_expires_an_unbound_claim_after_virtual_time_advances() {
        let registry = SessionRegistry::new(SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            procwatch_poll: TEST_PROCWATCH_POLL,
            active_agent_claim_ttl: TEST_CLAIM_TTL,
            ..SessionRegistryConfig::default()
        });
        let cwd = pohunek_test_support::tempdir().expect("private session cwd");
        let created = registry
            .create(SessionNewParams {
                name: None,
                agent: "shell".to_owned(),
                cwd: Some(cwd.path().to_path_buf()),
                cols: 80,
                rows: 24,
                project: None,
                repo: None,
                branch: None,
                base_branch: None,
                input: None,
                extended_input_ready_wait: None,
                branch_source: None,
                on_diverged: None,
                input_ack_timeout_ms: None,
                metadata: BTreeMap::new(),
            })
            .await
            .expect("create shell session");

        tokio::time::pause();
        let report = registry
            .report_agent(SessionReportAgentParams {
                session_id: created.id.clone(),
                source: TEST_AGENT_SOURCE.to_owned(),
                agent: TEST_AGENT.to_owned(),
                activity: None,
                seq: Some(ReportSequence::new(TEST_REPORT_SEQ)),
                pid: None,
                agent_session_id: None,
                agent_session_path: None,
                pid_start_identity: None,
                monotonic_sequence: None,
                retryable_admission: false,
            })
            .await;
        assert!(report.recorded);

        tokio::time::advance(TEST_CLAIM_TTL.saturating_sub(TEST_PROCWATCH_POLL)).await;
        for _ in 0..TEST_YIELD_BUDGET {
            tokio::task::yield_now().await;
        }
        let retained = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(
            retained.active_agent.as_deref(),
            Some(TEST_AGENT),
            "a claim younger than its TTL survives procwatch ticks"
        );

        tokio::time::advance(TEST_CLAIM_TTL).await;
        // The tick's scan reads the process tree on a blocking thread, so the
        // release lands whenever that read returns; no yield count bounds it.
        wait_until("a procwatch tick released the expired claim", || async {
            let info = registry.inspect(&created.id).await.expect("inspect");
            info.active_agent.is_none().then_some(())
        })
        .await;

        tokio::time::resume();
        registry.stop(&created.id).await.expect("stop the session");
    }
}
