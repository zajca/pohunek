//! Backend-neutral live transcript watching.
//!
//! The transcript index is converged by a bounded reconciliation pass that runs
//! every `RECONCILE_INTERVAL`, on a rescan hint, and after a backend restart,
//! independent of watcher health. A platform backend (inotify on Linux,
//! `FSEvents` on macOS) only reduces latency: it reports file-change *hints* that
//! trigger a debounced, coalesced parse of one transcript. Any failure of that
//! fast path is only logged, because the next pass repairs it; a stream that
//! silently stopped delivering costs latency, never correctness.
//!
//! Every pass asks the [`RootProvider`] for the roots to watch, so a root that
//! appears or disappears between passes is followed without a restart, then
//! re-resolves them (aliases, retargeted symlinks, nested roots), registers them
//! with the backend, and scans them. Registration failures and incomplete scans
//! are published as degraded states.

// Rust guideline compliant 2026-10-05

use std::collections::{HashMap, VecDeque};
use std::future::{poll_fn, Future};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::{sleep_until, Instant};
use tokio_util::sync::CancellationToken;
use tokio_util::time::DelayQueue;
use tracing::{debug, info, warn};

use super::{
    canonical_roots, is_jsonl_path, owning_root, platform, TranscriptRoot, PRIVATE_PATH_LABEL,
};

/// Debounce applied to the first hint for a transcript path before it is parsed.
///
/// Provider CLIs can append several small JSON records in quick succession at
/// session start. Delaying slightly avoids parsing a partially written line
/// while keeping newly started agents responsive. Hints arriving while the path
/// is already waiting collapse into the pending parse.
pub(super) const TRANSCRIPT_WRITE_DEBOUNCE: Duration = Duration::from_millis(75);

/// Maximum number of distinct transcript paths waiting for or undergoing a parse.
///
/// A hint for a new path beyond this bound is dropped and replaced by a
/// reconciliation pass, which recovers the path from the filesystem.
pub(super) const MAX_PENDING_TRANSCRIPT_PATHS: usize = 4096;

/// Maximum number of transcript parses running at once.
///
/// Each parse reads at most the transcript scan byte limit; capping concurrency
/// bounds the file descriptors and blocking threads a burst of hints can use.
pub(super) const MAX_CONCURRENT_TRANSCRIPT_PARSES: usize = 4;

/// Minimum time between the end of one reconciliation pass and the start of the
/// next when hints request one.
///
/// Overflow storms and directory churn request passes repeatedly; spacing them
/// keeps the scan from monopolizing I/O. A request made inside the window is
/// deferred to its end, never dropped.
pub(super) const RESCAN_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Time between unconditional reconciliation passes.
///
/// This bounds how stale the index can be when the backend missed or lost events,
/// failed to register a directory, or stopped delivering: a new or changed
/// transcript is indexed at the latest one interval later.
pub(super) const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// First delay before a failed backend is restarted.
pub(super) const RETRY_BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Longest delay between backend restart attempts. The delay doubles after each
/// failed attempt up to this bound and resets after a successful restart.
pub(super) const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Stable reason reported whenever live transcript watching is not running.
///
/// The observer then relies on the reconciliation passes and the periodic
/// process sweep.
pub(crate) const TRANSCRIPT_WATCHER_UNAVAILABLE: &str = "external_transcript_watcher_unavailable";

/// Stable reason reported while live transcript watching runs but does not cover
/// every transcript root completely, so some transcripts are indexed only by
/// reconciliation passes.
pub(crate) const TRANSCRIPT_WATCHER_DEGRADED: &str = "external_transcript_watcher_degraded";

/// Whether new transcripts are indexed as they are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptWatch {
    /// A live watcher follows every transcript root.
    Active,
    /// A live watcher runs but does not cover every root completely.
    Degraded(WatcherDegraded),
    /// No live watcher runs; transcripts are indexed by reconciliation only.
    Unavailable(WatcherUnavailable),
}

/// Why live transcript watching covers only part of the transcript roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherDegraded {
    /// A provider transcript root does not exist yet or was removed.
    RootsMissing,
    /// The backend could not register a root or some of its directories.
    RegistrationFailed,
    /// The last reconciliation could not read or finish part of a root: a
    /// directory was unreadable, or the tree exceeds the scan bounds.
    ScanIncomplete,
}

/// Why live transcript watching is not running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatcherUnavailable {
    /// The platform watcher could not be opened.
    OpenFailed,
    /// The platform watcher failed after it was opened.
    BackendFailed,
}

impl WatcherDegraded {
    /// Stable machine-readable cause.
    pub(crate) const fn cause(self) -> &'static str {
        match self {
            Self::RootsMissing => "roots_missing",
            Self::RegistrationFailed => "registration_failed",
            Self::ScanIncomplete => "scan_incomplete",
        }
    }
}

impl WatcherUnavailable {
    /// Stable machine-readable cause.
    pub(crate) const fn cause(self) -> &'static str {
        match self {
            Self::OpenFailed => platform::OPEN_FAILED_CAUSE,
            Self::BackendFailed => "watcher_backend_failed",
        }
    }
}

/// Logs that live watching is unavailable and returns the matching state.
pub(super) fn report_watcher_unavailable(
    cause: WatcherUnavailable,
    error: Option<&io::Error>,
) -> TranscriptWatch {
    warn!(
        name: "external.transcript_watcher.unavailable",
        reason = TRANSCRIPT_WATCHER_UNAVAILABLE,
        cause = cause.cause(),
        error = error.map(tracing::field::display),
        "external transcript watcher unavailable ({{cause}}); reconciliation and process sweep still run"
    );
    TranscriptWatch::Unavailable(cause)
}

/// Hint produced by a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WatchEvent {
    /// A file at this path may have been created, written, renamed or removed.
    Changed(PathBuf),
    /// A directory at this path may have been created, removed or renamed, so the
    /// registrations and index below it may be stale.
    TreeChanged(PathBuf),
    /// A directory appeared below a watched directory and needs its own watches
    /// (see [`WatchBackend::watch_new_directory`]).
    #[cfg_attr(
        all(target_os = "macos", not(test)),
        expect(
            dead_code,
            reason = "FSEvents watches trees natively, so only per-directory backends report new directories"
        )
    )]
    NewDirectory(PathBuf),
    /// The backend lost events or its own state; everything must be reconciled.
    Rescan,
}

/// Outcome of registering one transcript root with a backend.
#[derive(Debug)]
pub(super) enum RootRegistration {
    /// The root and every directory below it are covered.
    Complete,
    /// The root is covered but `failed_dirs` directories below it are not.
    #[cfg_attr(
        all(target_os = "macos", not(test)),
        expect(
            dead_code,
            reason = "FSEvents registers a whole tree or nothing, so only per-directory backends report partial coverage"
        )
    )]
    Partial {
        /// Number of directories that could not be registered.
        failed_dirs: usize,
        /// First error encountered, for the operator-facing diagnostic.
        first_error: io::Error,
    },
    /// The root does not exist or is not a directory.
    Missing,
    /// The root exists but nothing could be registered.
    Failed(io::Error),
}

/// Result of scanning one transcript root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootScan {
    /// Every directory was listed within the scan bounds.
    Complete,
    /// The root does not exist or is not a directory.
    Missing,
    /// Part of the root could not be read or exceeds the scan bounds.
    Incomplete,
}

/// Platform file-watch backend.
///
/// Implementations report hints only and never read transcript contents. The
/// registration methods walk the filesystem, so they check `cancel` per entry.
pub(super) trait WatchBackend: Send + 'static {
    /// Whether registration walks the filesystem and therefore runs on the
    /// blocking pool instead of the runner task. A backend whose registration
    /// performs no I/O opts out.
    const OFFLOAD_REGISTRATION: bool = true;

    /// Registers `root` and everything below it. Registering an already covered
    /// root is a cheap no-op that only fills in missing directories.
    fn register_root(&mut self, root: &Path, cancel: &CancellationToken) -> RootRegistration;

    /// Registers the watches for a directory that appeared below a watched one
    /// and appends hints for the transcripts already inside it, which produced no
    /// event of their own. Backends that watch trees natively never emit
    /// [`WatchEvent::NewDirectory`] and keep the default.
    fn watch_new_directory(
        &mut self,
        _dir: &Path,
        _out: &mut Vec<WatchEvent>,
        _cancel: &CancellationToken,
    ) {
    }

    /// Recreates the underlying watcher after a failure, dropping every
    /// registration. The next pass registers the roots again.
    fn restart(&mut self) -> io::Result<()>;

    /// Drops every registration so a pass can rebuild them from the filesystem.
    fn unregister_all(&mut self);

    /// Appends the next hints to `out`, waiting until at least one state change
    /// is observed. Must be cancel-safe: the runner drops the future whenever
    /// another event source wins.
    fn next_events(
        &mut self,
        out: &mut Vec<WatchEvent>,
    ) -> impl Future<Output = io::Result<()>> + Send;
}

/// Target of parse requests and reconciliation passes.
pub(super) trait TranscriptSink: Send + Sync + 'static {
    /// Parses one transcript and returns whether the index changed.
    fn upsert(
        &self,
        owner: TranscriptRoot,
        path: PathBuf,
    ) -> impl Future<Output = io::Result<bool>> + Send;

    /// Reconciles the index with the filesystem under `roots`, returning one
    /// result per root in order.
    fn reconcile(&self, roots: Vec<TranscriptRoot>) -> impl Future<Output = Vec<RootScan>> + Send;

    /// Signals that the index changed and observers should re-sweep.
    fn index_changed(&self);
}

/// Where the roots of each reconciliation pass come from.
pub(super) trait RootProvider: Send + Sync + 'static {
    /// The roots to watch in the pass that is starting.
    ///
    /// Called at the start of every pass, so an implementation may do bounded
    /// filesystem work and must answer from the current state of the host.
    fn roots(&self) -> impl Future<Output = Vec<TranscriptRoot>> + Send;
}

/// A fixed set of roots.
impl RootProvider for Vec<TranscriptRoot> {
    fn roots(&self) -> impl Future<Output = Vec<TranscriptRoot>> + Send {
        std::future::ready(self.clone())
    }
}

/// Handle to a running transcript watcher.
///
/// The observer only needs the watcher to run; health and on-demand passes are
/// read by tests.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the observer does not read watcher health")
)]
pub(crate) struct WatcherHandle {
    state: watch::Receiver<TranscriptWatch>,
    nudge: Arc<Notify>,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the observer does not read watcher health")
)]
impl WatcherHandle {
    /// Current watcher health.
    pub(crate) fn state(&self) -> TranscriptWatch {
        *self.state.borrow()
    }

    /// Receiver that observes every health change.
    #[cfg(test)]
    pub(super) fn subscribe(&self) -> watch::Receiver<TranscriptWatch> {
        self.state.clone()
    }

    /// Asks for a reconciliation pass now, like a rescan hint.
    #[cfg(test)]
    pub(super) fn request_reconcile(&self) {
        self.nudge.notify_one();
    }
}

/// Registration health of one root in the last pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Registration {
    Complete,
    Partial,
    Missing,
    Failed,
}

impl Registration {
    fn from_outcome(outcome: &RootRegistration) -> Self {
        match outcome {
            RootRegistration::Complete => Self::Complete,
            RootRegistration::Partial { .. } => Self::Partial,
            RootRegistration::Missing => Self::Missing,
            RootRegistration::Failed(_) => Self::Failed,
        }
    }
}

/// Health of one root in the last pass.
#[derive(Debug, Clone, Copy)]
struct RootHealth {
    registration: Registration,
    scan: RootScan,
}

/// Exponential delay between backend restart attempts.
#[derive(Debug, Clone, Copy)]
struct Backoff {
    delay: Duration,
}

impl Backoff {
    const fn new() -> Self {
        Self {
            delay: RETRY_BACKOFF_MIN,
        }
    }

    /// Returns the current delay and doubles it up to `RETRY_BACKOFF_MAX`.
    fn next_delay(&mut self) -> Duration {
        let delay = self.delay;
        self.delay = (delay * 2).min(RETRY_BACKOFF_MAX);
        delay
    }
}

#[derive(Debug)]
enum Phase {
    /// Waiting for the debounce timer or for a free parse slot.
    Queued,
    /// A parse is running; `dirty` records hints that arrived meanwhile.
    InFlight { dirty: bool },
}

#[derive(Debug)]
struct PathState {
    /// The root that owns the path in the current pass; the parse is made and
    /// classified for it.
    owner: TranscriptRoot,
    phase: Phase,
}

type ParseDone = io::Result<bool>;

struct Watcher<B, S, P> {
    /// Absent only while an offloaded operation owns it.
    backend: Option<B>,
    /// Supplies the roots of every pass.
    provider: P,
    /// The configured roots that survived alias resolution in the last pass.
    roots: Vec<TranscriptRoot>,
    /// The canonical directory of each entry of `roots`.
    canonical: Vec<PathBuf>,
    sink: Arc<S>,
    health: Vec<RootHealth>,
    events: Vec<WatchEvent>,
    paths: HashMap<PathBuf, PathState>,
    timers: DelayQueue<PathBuf>,
    ready: VecDeque<PathBuf>,
    parses: JoinSet<(PathBuf, ParseDone)>,
    parse_paths: HashMap<Id, PathBuf>,
    unavailable: Option<WatcherUnavailable>,
    restart_at: Option<Instant>,
    restart: Backoff,
    reconcile_at: Instant,
    last_reconcile: Instant,
    state: watch::Sender<TranscriptWatch>,
    nudge: Arc<Notify>,
    shutdown: CancellationToken,
}

/// Runs the first reconciliation pass and spawns the runner.
///
/// `open_error` carries the failure of opening the backend, in which case the
/// runner starts unavailable and keeps trying to restart it while the passes
/// continue.
pub(super) async fn start<B, S, P>(
    backend: B,
    roots: P,
    sink: Arc<S>,
    shutdown: CancellationToken,
    open_error: Option<io::Error>,
) -> WatcherHandle
where
    B: WatchBackend,
    S: TranscriptSink,
    P: RootProvider,
{
    let (state_tx, state_rx) = watch::channel(TranscriptWatch::Active);
    let nudge = Arc::new(Notify::new());
    let now = Instant::now();
    let mut watcher = Watcher {
        backend: Some(backend),
        provider: roots,
        roots: Vec::new(),
        canonical: Vec::new(),
        sink,
        health: Vec::new(),
        events: Vec::new(),
        paths: HashMap::new(),
        timers: DelayQueue::new(),
        ready: VecDeque::new(),
        parses: JoinSet::new(),
        parse_paths: HashMap::new(),
        unavailable: None,
        restart_at: None,
        restart: Backoff::new(),
        reconcile_at: now,
        last_reconcile: now,
        state: state_tx,
        nudge: Arc::clone(&nudge),
        shutdown: shutdown.clone(),
    };
    if let Some(error) = &open_error {
        report_watcher_unavailable(WatcherUnavailable::OpenFailed, Some(error));
        watcher.unavailable = Some(WatcherUnavailable::OpenFailed);
        watcher.schedule_restart();
    }
    watcher.reconcile().await;
    tokio::spawn(watcher.run());
    WatcherHandle {
        state: state_rx,
        nudge,
    }
}

impl<B: WatchBackend, S: TranscriptSink, P: RootProvider> Watcher<B, S, P> {
    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                () = self.shutdown.cancelled() => break,
                () = self.nudge.notified() => self.request_reconcile(),
                result = next_backend_events(&mut self.backend, &mut self.events), if self.unavailable.is_none() && self.backend.is_some() => {
                    if let Err(error) = result {
                        self.backend_failed(&error);
                    }
                }
                Some(expired) = poll_fn(|cx| self.timers.poll_expired(cx)), if !self.timers.is_empty() => {
                    self.ready.push_back(expired.into_inner());
                }
                Some(joined) = self.parses.join_next_with_id(), if !self.parses.is_empty() => {
                    self.finish_parse(joined);
                }
                () = sleep_until(self.restart_at.unwrap_or_else(Instant::now)), if self.restart_at.is_some() => {
                    self.recover_backend().await;
                }
                () = sleep_until(self.reconcile_at) => self.reconcile().await,
            }
            self.drain_events().await;
            self.spawn_ready();
        }
    }

    /// Runs `work` on the backend, on the blocking pool when the backend walks the
    /// filesystem, so a large tree never blocks a runtime worker. The backend is
    /// moved for the duration; nothing else touches it meanwhile because the
    /// runner awaits the result. Returns `None` when shutdown interrupts the wait.
    async fn offload<R: Send + 'static>(
        &mut self,
        work: impl FnOnce(&mut B) -> R + Send + 'static,
    ) -> Option<R> {
        let mut backend = self.backend.take().expect(BACKEND_PRESENT);
        if !B::OFFLOAD_REGISTRATION {
            let result = work(&mut backend);
            self.backend = Some(backend);
            return Some(result);
        }
        let task = tokio::task::spawn_blocking(move || {
            let result = work(&mut backend);
            (backend, result)
        });
        tokio::select! {
            () = self.shutdown.cancelled() => None,
            joined = task => {
                let (backend, result) = joined.expect("backend registration task does not panic");
                self.backend = Some(backend);
                Some(result)
            }
        }
    }

    async fn drain_events(&mut self) {
        let mut queue = VecDeque::from(std::mem::take(&mut self.events));
        while let Some(event) = queue.pop_front() {
            if let WatchEvent::NewDirectory(dir) = event {
                let cancel = self.shutdown.clone();
                let Some(found) = self
                    .offload(move |backend| {
                        let mut found = Vec::new();
                        backend.watch_new_directory(&dir, &mut found, &cancel);
                        found
                    })
                    .await
                else {
                    return;
                };
                queue.extend(found);
            } else {
                self.handle_event(event);
            }
        }
    }

    fn handle_event(&mut self, event: WatchEvent) {
        match event {
            WatchEvent::Rescan => self.request_reconcile(),
            WatchEvent::TreeChanged(path) => {
                if is_jsonl_path(&path) {
                    // A directory now (or no longer) at a transcript's path: the
                    // parse drops the candidate if the path is not a transcript.
                    self.schedule_parse(path.clone());
                }
                let concerns_roots = self
                    .roots
                    .iter()
                    .any(|root| path.starts_with(&root.path) || root.path.starts_with(&path));
                if concerns_roots {
                    self.request_reconcile();
                }
            }
            WatchEvent::Changed(path) => self.schedule_parse(path),
            // Consumed by `drain_events`, which owns the offloaded walk.
            WatchEvent::NewDirectory(_) => {}
        }
    }

    fn schedule_parse(&mut self, path: PathBuf) {
        if !is_jsonl_path(&path) {
            return;
        }
        let Some(owner) = owning_root(&self.roots, &self.canonical, &path).cloned() else {
            return;
        };
        if let Some(state) = self.paths.get_mut(&path) {
            if let Phase::InFlight { dirty } = &mut state.phase {
                *dirty = true;
            }
            return;
        }
        if self.paths.len() >= MAX_PENDING_TRANSCRIPT_PATHS {
            self.request_reconcile();
            return;
        }
        self.timers.insert(path.clone(), TRANSCRIPT_WRITE_DEBOUNCE);
        self.paths.insert(
            path,
            PathState {
                owner,
                phase: Phase::Queued,
            },
        );
    }

    fn spawn_ready(&mut self) {
        while self.parses.len() < MAX_CONCURRENT_TRANSCRIPT_PARSES {
            let Some(path) = self.ready.pop_front() else {
                break;
            };
            let Some(state) = self.paths.get_mut(&path) else {
                continue;
            };
            state.phase = Phase::InFlight { dirty: false };
            let owner = state.owner.clone();
            let sink = Arc::clone(&self.sink);
            let task_path = path.clone();
            let handle = self.parses.spawn(async move {
                let result = sink.upsert(owner, task_path.clone()).await;
                (task_path, result)
            });
            self.parse_paths.insert(handle.id(), path);
        }
    }

    /// Completes one fast-path parse. A failure is only logged: the next pass
    /// re-reads the transcript.
    fn finish_parse(&mut self, joined: Result<(Id, (PathBuf, ParseDone)), JoinError>) {
        let (id, outcome) = match joined {
            Ok((id, (_path, outcome))) => (id, Some(outcome)),
            Err(error) => {
                warn!(
                    name: "external.transcript_parse.panicked",
                    error = %error,
                    "external transcript parse task failed: {{error}}"
                );
                (error.id(), None)
            }
        };
        let Some(path) = self.parse_paths.remove(&id) else {
            return;
        };
        match outcome {
            Some(Ok(true)) => self.sink.index_changed(),
            Some(Ok(false)) | None => {}
            Some(Err(error)) => {
                debug!(
                    path = %self.log_path(&path),
                    error = %error,
                    "failed to parse external transcript candidate"
                );
            }
        }
        let Some(mut state) = self.paths.remove(&path) else {
            return;
        };
        if matches!(state.phase, Phase::InFlight { dirty: true }) {
            self.timers.insert(path.clone(), TRANSCRIPT_WRITE_DEBOUNCE);
            state.phase = Phase::Queued;
            self.paths.insert(path, state);
        }
    }

    /// Forgets every pending parse whose path lies below no current root, so a
    /// root that left the configuration is never parsed again, and hands the
    /// others to the root that owns them now. Timer and queue entries that
    /// outlive their path are skipped when they come due.
    fn drop_unowned_paths(&mut self) {
        let (roots, canonical) = (&self.roots, &self.canonical);
        self.paths
            .retain(|path, state| match owning_root(roots, canonical, path) {
                Some(owner) => {
                    state.owner.clone_from(owner);
                    true
                }
                None => false,
            });
    }

    /// The path as a log line may show it, by the provenance of its pending
    /// parse; a path no parse is pending for is shown by label.
    fn log_path(&self, path: &Path) -> String {
        self.paths.get(path).map_or_else(
            || PRIVATE_PATH_LABEL.to_owned(),
            |state| state.owner.log_path_of(path),
        )
    }

    /// Brings the next pass forward, spaced from the previous one.
    fn request_reconcile(&mut self) {
        let earliest = self.last_reconcile + RESCAN_MIN_INTERVAL;
        self.reconcile_at = self.reconcile_at.min(earliest.max(Instant::now()));
    }

    /// Handles an error from the backend's event stream.
    ///
    /// Interrupted and would-block reads are transient. Anything else marks the
    /// backend unavailable and schedules a restart; the passes continue meanwhile.
    fn backend_failed(&mut self, error: &io::Error) {
        if matches!(
            error.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
        ) {
            return;
        }
        self.state.send_replace(report_watcher_unavailable(
            WatcherUnavailable::BackendFailed,
            Some(error),
        ));
        self.unavailable = Some(WatcherUnavailable::BackendFailed);
        self.restart = Backoff::new();
        self.schedule_restart();
    }

    fn schedule_restart(&mut self) {
        self.restart_at = Some(Instant::now() + self.restart.next_delay());
    }

    /// Restarts an unavailable backend; success is followed by a pass that
    /// registers the roots again.
    async fn recover_backend(&mut self) {
        self.restart_at = None;
        match self.backend.as_mut().expect(BACKEND_PRESENT).restart() {
            Ok(()) => {
                self.unavailable = None;
                self.restart = Backoff::new();
                self.reconcile().await;
            }
            Err(error) => {
                debug!(error = %error, "external transcript watcher restart failed");
                self.schedule_restart();
            }
        }
    }

    /// Awaits every in-flight parse so none can write to the index after the
    /// scan that follows.
    ///
    /// At most `MAX_CONCURRENT_TRANSCRIPT_PARSES` bounded reads are awaited, and
    /// no new parse starts until the caller returns.
    async fn drain_parses(&mut self) {
        while let Some(joined) = self.parses.join_next_with_id().await {
            self.finish_parse(joined);
        }
    }

    /// One reconciliation pass: ask the provider for the roots, resolve them,
    /// register them with the backend when it is available, then scan them.
    ///
    /// Registration happens before the scan so a transcript created in between
    /// is seen by at least one of them. Nothing resolved here outlives the pass.
    async fn reconcile(&mut self) {
        self.drain_parses().await;
        let configured = self.provider.roots().await;
        self.roots = resolve_roots(&configured);
        self.canonical = canonical_roots(&self.roots);
        self.drop_unowned_paths();
        self.health = vec![
            RootHealth {
                registration: Registration::Complete,
                scan: RootScan::Complete,
            };
            self.roots.len()
        ];
        if self.unavailable.is_none() {
            self.backend
                .as_mut()
                .expect(BACKEND_PRESENT)
                .unregister_all();
            for index in 0..self.roots.len() {
                if !self.register(index).await {
                    return;
                }
            }
        }
        let scans = self.sink.reconcile(self.roots.clone()).await;
        for (health, scan) in self.health.iter_mut().zip(&scans) {
            health.scan = *scan;
        }
        let now = Instant::now();
        self.last_reconcile = now;
        self.reconcile_at = now + RECONCILE_INTERVAL;
        self.sink.index_changed();
        self.publish_state();
    }

    /// Registers one resolved root and records its health. Returns `false` when
    /// shutdown interrupted the registration.
    async fn register(&mut self, index: usize) -> bool {
        let path = self.roots[index].path.clone();
        let cancel = self.shutdown.clone();
        let Some(outcome) = self
            .offload(move |backend| backend.register_root(&path, &cancel))
            .await
        else {
            return false;
        };
        let root = &self.roots[index];
        match &outcome {
            RootRegistration::Complete | RootRegistration::Missing => {}
            RootRegistration::Partial {
                failed_dirs,
                first_error,
            } => warn!(
                name: "external.transcript_watcher.register.partial",
                agent = ?root.agent_base,
                root = %root.log_path(),
                failed_dirs = *failed_dirs,
                error = %first_error,
                "cannot watch {{failed_dirs}} directories below transcript root {{root}}: {{error}}; check directory permissions and the OS file watch limit"
            ),
            RootRegistration::Failed(error) => warn!(
                name: "external.transcript_watcher.register.failed",
                agent = ?root.agent_base,
                root = %root.log_path(),
                error = %error,
                "cannot watch transcript root {{root}}: {{error}}; check directory permissions and the OS file watch limit"
            ),
        }
        self.health[index].registration = Registration::from_outcome(&outcome);
        true
    }

    fn compute_state(&self) -> TranscriptWatch {
        if let Some(cause) = self.unavailable {
            return TranscriptWatch::Unavailable(cause);
        }
        let any = |predicate: fn(&RootHealth) -> bool| self.health.iter().any(predicate);
        if any(|health| {
            matches!(
                health.registration,
                Registration::Failed | Registration::Partial
            )
        }) {
            TranscriptWatch::Degraded(WatcherDegraded::RegistrationFailed)
        } else if any(|health| {
            health.registration == Registration::Missing || health.scan == RootScan::Missing
        }) {
            TranscriptWatch::Degraded(WatcherDegraded::RootsMissing)
        } else if any(|health| health.scan == RootScan::Incomplete) {
            TranscriptWatch::Degraded(WatcherDegraded::ScanIncomplete)
        } else {
            TranscriptWatch::Active
        }
    }

    fn publish_state(&self) {
        let next = self.compute_state();
        let previous = *self.state.borrow();
        if next == previous {
            return;
        }
        match next {
            TranscriptWatch::Degraded(cause) => info!(
                name: "external.transcript_watcher.degraded",
                reason = TRANSCRIPT_WATCHER_DEGRADED,
                cause = cause.cause(),
                "external transcript watcher degraded ({{cause}}); every reconciliation pass retries"
            ),
            TranscriptWatch::Active => info!(
                name: "external.transcript_watcher.recovered",
                "external transcript watcher covers every transcript root"
            ),
            TranscriptWatch::Unavailable(_) => {}
        }
        self.state.send_replace(next);
    }
}

/// Panic message for the invariant that the backend is present outside offloaded calls.
const BACKEND_PRESENT: &str = "backend is present outside offloaded operations";

/// Awaits the next hints from the backend.
async fn next_backend_events<B: WatchBackend>(
    backend: &mut Option<B>,
    events: &mut Vec<WatchEvent>,
) -> io::Result<()> {
    backend
        .as_mut()
        .expect(BACKEND_PRESENT)
        .next_events(events)
        .await
}

/// Resolves the configured roots for one pass: roots that repeat an earlier root
/// (same path, same canonical directory, or the same device and inode) are
/// dropped and the first configured root keeps the directory, so an alias never registers or scans a tree twice. Roots that cannot
/// be resolved are kept; registration and scan report them missing.
pub(super) fn resolve_roots(configured: &[TranscriptRoot]) -> Vec<TranscriptRoot> {
    let mut kept: Vec<(TranscriptRoot, Option<(u64, u64)>)> = Vec::new();
    for root in configured {
        let identity = std::fs::canonicalize(&root.path)
            .and_then(std::fs::metadata)
            .ok()
            .map(|metadata| (metadata.dev(), metadata.ino()));
        let duplicate = kept.iter().any(|(existing, existing_identity)| {
            existing.path == root.path || (identity.is_some() && *existing_identity == identity)
        });
        if duplicate {
            debug!(
                root = %root.log_path(),
                "ignoring transcript root that duplicates another configured root"
            );
        } else {
            kept.push((root.clone(), identity));
        }
    }
    kept.into_iter().map(|(root, _identity)| root).collect()
}

#[cfg(test)]
mod tests;
