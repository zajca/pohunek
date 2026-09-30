//! Deterministic runner tests over a scripted backend and a recording sink.
//!
//! Time is paused and every wait is an explicit clock advance or a bounded
//! number of scheduler yields, so the tests do not depend on wall-clock timing.

// Rust guideline compliant 2026-09-29

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::AgentKind;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{
    start, RootRegistration, RootScan, TranscriptSink, TranscriptWatch, WatchBackend, WatchEvent,
    WatcherDegraded, WatcherHandle, WatcherUnavailable, MAX_CONCURRENT_TRANSCRIPT_PARSES,
    MAX_PENDING_TRANSCRIPT_PATHS, RECONCILE_INTERVAL, RESCAN_MIN_INTERVAL, RETRY_BACKOFF_MAX,
    RETRY_BACKOFF_MIN, TRANSCRIPT_WRITE_DEBOUNCE,
};
use crate::external::TranscriptRoot;

/// Scheduler yields that let every runnable task make progress between steps.
///
/// The runner needs several hops (event, timer, parse task, completion) to reach
/// a quiescent state; the count only has to exceed the longest chain.
const SETTLE_YIELDS: usize = 64;

type Input = mpsc::UnboundedSender<Result<Vec<WatchEvent>, io::ErrorKind>>;

#[derive(Debug, Clone, Copy)]
enum FakeRegistration {
    Complete,
    Partial,
    Missing,
    Failed,
}

#[derive(Debug, Default)]
struct FakeShared {
    registrations: Mutex<HashMap<PathBuf, FakeRegistration>>,
    register_calls: AtomicUsize,
    restart_calls: AtomicUsize,
    restart_ok: AtomicBool,
    unregister_calls: AtomicUsize,
}

struct FakeBackend {
    input: mpsc::UnboundedReceiver<Result<Vec<WatchEvent>, io::ErrorKind>>,
    shared: Arc<FakeShared>,
}

impl WatchBackend for FakeBackend {
    const OFFLOAD_REGISTRATION: bool = false;

    fn register_root(&mut self, root: &Path, _cancel: &CancellationToken) -> RootRegistration {
        self.shared.register_calls.fetch_add(1, Ordering::SeqCst);
        let registration = self
            .shared
            .registrations
            .lock()
            .expect("registrations")
            .get(root)
            .copied()
            .unwrap_or(FakeRegistration::Complete);
        match registration {
            FakeRegistration::Complete => RootRegistration::Complete,
            FakeRegistration::Partial => RootRegistration::Partial {
                failed_dirs: 1,
                first_error: io::Error::from(io::ErrorKind::PermissionDenied),
            },
            FakeRegistration::Missing => RootRegistration::Missing,
            FakeRegistration::Failed => {
                RootRegistration::Failed(io::Error::from(io::ErrorKind::PermissionDenied))
            }
        }
    }

    fn watch_new_directory(
        &mut self,
        dir: &Path,
        out: &mut Vec<WatchEvent>,
        _cancel: &CancellationToken,
    ) {
        out.push(WatchEvent::Changed(dir.join("found.jsonl")));
    }

    fn restart(&mut self) -> io::Result<()> {
        self.shared.restart_calls.fetch_add(1, Ordering::SeqCst);
        if self.shared.restart_ok.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(io::Error::other("scripted restart failure"))
        }
    }

    fn unregister_all(&mut self) {
        self.shared.unregister_calls.fetch_add(1, Ordering::SeqCst);
    }

    async fn next_events(&mut self, out: &mut Vec<WatchEvent>) -> io::Result<()> {
        match self.input.recv().await {
            Some(Ok(events)) => {
                out.extend(events);
                Ok(())
            }
            Some(Err(kind)) => Err(io::Error::from(kind)),
            None => std::future::pending().await,
        }
    }
}

#[derive(Default)]
struct FakeSink {
    upserts: Mutex<Vec<PathBuf>>,
    agents: Mutex<Vec<(AgentKind, PathBuf)>>,
    /// What the filesystem holds, and what the last reconcile copied from it.
    disk: Mutex<BTreeSet<PathBuf>>,
    indexed: Mutex<BTreeSet<PathBuf>>,
    /// The roots each reconcile pass was given.
    passes: Mutex<Vec<Vec<PathBuf>>>,
    active: AtomicUsize,
    peak: AtomicUsize,
    gate: Option<Arc<Semaphore>>,
    reconciles: AtomicUsize,
    scans: Mutex<Option<Vec<RootScan>>>,
    changed: AtomicUsize,
    fail_next: AtomicUsize,
}

impl TranscriptSink for FakeSink {
    async fn upsert(&self, agent_base: AgentKind, path: PathBuf) -> io::Result<bool> {
        self.agents
            .lock()
            .expect("agents")
            .push((agent_base, path.clone()));
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        self.upserts.lock().expect("upserts").push(path);
        // A compare-exchange loop: `fetch_update` is deprecated on newer toolchains.
        let mut left = self.fail_next.load(Ordering::SeqCst);
        let failing = loop {
            if left == 0 {
                break false;
            }
            match self.fail_next.compare_exchange(
                left,
                left - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break true,
                Err(actual) => left = actual,
            }
        };
        if failing {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return Err(io::Error::other("scripted parse failure"));
        }
        if let Some(gate) = &self.gate {
            gate.acquire().await.expect("gate open").forget();
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(true)
    }

    fn reconcile(&self, roots: Vec<TranscriptRoot>) -> impl Future<Output = Vec<RootScan>> + Send {
        self.reconciles.fetch_add(1, Ordering::SeqCst);
        let disk = self.disk.lock().expect("disk").clone();
        *self.indexed.lock().expect("indexed") = disk;
        self.passes
            .lock()
            .expect("passes")
            .push(roots.iter().map(|root| root.path.clone()).collect());
        let scans = self
            .scans
            .lock()
            .expect("scans")
            .clone()
            .unwrap_or_else(|| vec![RootScan::Complete; roots.len()]);
        std::future::ready(scans)
    }

    fn index_changed(&self) {
        self.changed.fetch_add(1, Ordering::SeqCst);
    }
}

struct Harness {
    handle: WatcherHandle,
    input: Input,
    shared: Arc<FakeShared>,
    sink: Arc<FakeSink>,
    shutdown: CancellationToken,
    /// Keeps the root directory alive for the harness's lifetime.
    _root: tempfile::TempDir,
    root_path: PathBuf,
}

impl Harness {
    async fn start(initial: FakeRegistration, gate: Option<Arc<Semaphore>>) -> Self {
        let root = pohunek_test_support::tempdir().expect("transcript root");
        let root_path = root.path().to_path_buf();
        let shared = Arc::new(FakeShared::default());
        shared
            .registrations
            .lock()
            .expect("registrations")
            .insert(root_path.clone(), initial);
        let (input, receiver) = mpsc::unbounded_channel();
        let backend = FakeBackend {
            input: receiver,
            shared: Arc::clone(&shared),
        };
        let sink = Arc::new(FakeSink {
            gate,
            ..FakeSink::default()
        });
        let shutdown = CancellationToken::new();
        let roots = vec![claude_root(&root_path)];
        let handle = start(backend, roots, Arc::clone(&sink), shutdown.clone(), None).await;
        Self {
            handle,
            input,
            shared,
            sink,
            shutdown,
            _root: root,
            root_path,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root_path.join(name)
    }

    fn send(&self, events: Vec<WatchEvent>) {
        let _ = self.input.send(Ok(events));
    }

    fn set_registration(&self, registration: FakeRegistration) {
        self.shared
            .registrations
            .lock()
            .expect("registrations")
            .insert(self.root_path.clone(), registration);
    }

    fn set_scan(&self, scan: RootScan) {
        *self.sink.scans.lock().expect("scans") = Some(vec![scan]);
    }

    fn upserts(&self) -> Vec<PathBuf> {
        self.sink.upserts.lock().expect("upserts").clone()
    }

    fn reconciles(&self) -> usize {
        self.sink.reconciles.load(Ordering::SeqCst)
    }

    fn state(&self) -> TranscriptWatch {
        self.handle.state()
    }
}

fn claude_root(path: &Path) -> TranscriptRoot {
    TranscriptRoot {
        agent_base: AgentKind::Claude,
        path: path.to_path_buf(),
    }
}

async fn settle() {
    for _ in 0..SETTLE_YIELDS {
        tokio::task::yield_now().await;
    }
}

async fn advance(duration: Duration) {
    tokio::time::advance(duration).await;
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn a_burst_of_hints_for_one_path_yields_one_parse() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;
    let path = harness.path("a.jsonl");

    harness.send(vec![WatchEvent::Changed(path.clone()); 50]);
    settle().await;
    assert!(harness.upserts().is_empty(), "parse waits for the debounce");
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;

    assert_eq!(harness.upserts(), vec![path.clone()]);
    assert_eq!(harness.sink.changed.load(Ordering::SeqCst), 2);

    harness.send(vec![WatchEvent::Changed(path.clone())]);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;
    assert_eq!(harness.upserts(), vec![path.clone(), path]);
}

#[tokio::test(start_paused = true)]
async fn hints_arriving_during_a_parse_cause_exactly_one_reparse() {
    let gate = Arc::new(Semaphore::new(0));
    let harness = Harness::start(FakeRegistration::Complete, Some(Arc::clone(&gate))).await;
    let path = harness.path("a.jsonl");

    harness.send(vec![WatchEvent::Changed(path.clone())]);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;
    assert_eq!(harness.upserts().len(), 1, "first parse is running");

    harness.send(vec![WatchEvent::Changed(path.clone()); 5]);
    settle().await;
    assert_eq!(harness.upserts().len(), 1, "no second concurrent parse");

    gate.add_permits(1);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;
    assert_eq!(harness.upserts().len(), 2, "one reparse for the burst");

    gate.add_permits(1);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE * 4).await;
    assert_eq!(harness.upserts().len(), 2, "no further reparse");
}

#[tokio::test(start_paused = true)]
async fn concurrent_parses_are_capped() {
    let gate = Arc::new(Semaphore::new(0));
    let harness = Harness::start(FakeRegistration::Complete, Some(Arc::clone(&gate))).await;
    let total = MAX_CONCURRENT_TRANSCRIPT_PARSES + 3;

    harness.send(
        (0..total)
            .map(|index| WatchEvent::Changed(harness.path(&format!("s{index}.jsonl"))))
            .collect(),
    );
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;
    assert_eq!(harness.upserts().len(), MAX_CONCURRENT_TRANSCRIPT_PARSES);
    assert_eq!(
        harness.sink.active.load(Ordering::SeqCst),
        MAX_CONCURRENT_TRANSCRIPT_PARSES
    );

    gate.add_permits(total);
    settle().await;

    assert_eq!(harness.upserts().len(), total);
    assert_eq!(
        harness.sink.peak.load(Ordering::SeqCst),
        MAX_CONCURRENT_TRANSCRIPT_PARSES
    );
}

#[tokio::test(start_paused = true)]
async fn pending_path_overflow_falls_back_to_one_bounded_pass() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;

    harness.send(
        (0..=MAX_PENDING_TRANSCRIPT_PATHS)
            .map(|index| WatchEvent::Changed(harness.path(&format!("s{index}.jsonl"))))
            .collect(),
    );
    settle().await;
    assert_eq!(
        harness.reconciles(),
        1,
        "the pass is deferred by the interval"
    );
    advance(RESCAN_MIN_INTERVAL).await;
    loop {
        let parsed = harness.upserts().len();
        settle().await;
        if harness.upserts().len() == parsed {
            break;
        }
    }

    assert_eq!(harness.reconciles(), 2);
    assert_eq!(harness.upserts().len(), MAX_PENDING_TRANSCRIPT_PATHS);
}

#[tokio::test(start_paused = true)]
async fn lost_state_hints_coalesce_into_one_rate_limited_pass() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;

    for _ in 0..10 {
        harness.send(vec![WatchEvent::Rescan]);
    }
    settle().await;
    assert_eq!(harness.reconciles(), 1);
    advance(RESCAN_MIN_INTERVAL).await;
    assert_eq!(harness.reconciles(), 2);
    assert_eq!(harness.shared.unregister_calls.load(Ordering::SeqCst), 2);
    assert_eq!(harness.shared.register_calls.load(Ordering::SeqCst), 2);

    harness.send(vec![WatchEvent::Rescan]);
    settle().await;
    advance(RESCAN_MIN_INTERVAL.saturating_sub(Duration::from_millis(1))).await;
    assert_eq!(harness.reconciles(), 2, "spaced from the previous pass");
    advance(Duration::from_millis(1)).await;
    assert_eq!(harness.reconciles(), 3);
}

#[tokio::test(start_paused = true)]
async fn hints_outside_the_roots_and_non_transcripts_are_ignored() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;
    let outside = pohunek_test_support::tempdir().expect("outside");

    harness.send(vec![
        WatchEvent::Changed(outside.path().join("other.jsonl")),
        WatchEvent::Changed(harness.path("notes.txt")),
        WatchEvent::TreeChanged(outside.path().join("dir")),
    ]);
    settle().await;
    advance(RESCAN_MIN_INTERVAL + TRANSCRIPT_WRITE_DEBOUNCE).await;

    assert!(harness.upserts().is_empty());
    assert_eq!(harness.reconciles(), 1);
}

#[tokio::test(start_paused = true)]
async fn tree_changes_below_or_above_a_root_request_a_pass() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;

    harness.send(vec![WatchEvent::TreeChanged(harness.path("project"))]);
    settle().await;
    advance(RESCAN_MIN_INTERVAL).await;
    assert_eq!(harness.reconciles(), 2);

    let parent = harness.root_path.parent().expect("parent").to_path_buf();
    harness.send(vec![WatchEvent::TreeChanged(parent)]);
    settle().await;
    advance(RESCAN_MIN_INTERVAL).await;
    assert_eq!(harness.reconciles(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_healthy_watcher_still_reconciles_every_interval() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;
    assert_eq!(harness.reconciles(), 1);

    advance(RECONCILE_INTERVAL.saturating_sub(Duration::from_secs(1))).await;
    assert_eq!(harness.reconciles(), 1);
    advance(Duration::from_secs(1)).await;
    assert_eq!(harness.reconciles(), 2, "the pass does not depend on hints");
    advance(RECONCILE_INTERVAL).await;
    assert_eq!(harness.reconciles(), 3);
    assert_eq!(harness.state(), TranscriptWatch::Active);
}

#[tokio::test(start_paused = true)]
async fn registration_health_is_republished_by_every_pass() {
    let harness = Harness::start(FakeRegistration::Missing, None).await;
    assert_eq!(
        harness.state(),
        TranscriptWatch::Degraded(WatcherDegraded::RootsMissing)
    );

    harness.set_registration(FakeRegistration::Failed);
    advance(RECONCILE_INTERVAL).await;
    assert_eq!(
        harness.state(),
        TranscriptWatch::Degraded(WatcherDegraded::RegistrationFailed)
    );

    harness.set_registration(FakeRegistration::Partial);
    advance(RECONCILE_INTERVAL).await;
    assert_eq!(
        harness.state(),
        TranscriptWatch::Degraded(WatcherDegraded::RegistrationFailed)
    );

    harness.set_registration(FakeRegistration::Complete);
    advance(RECONCILE_INTERVAL).await;
    assert_eq!(harness.state(), TranscriptWatch::Active);
}

#[tokio::test(start_paused = true)]
async fn a_root_that_vanished_before_the_scan_is_reported_missing() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;
    assert_eq!(harness.state(), TranscriptWatch::Active);

    harness.set_scan(RootScan::Missing);
    advance(RECONCILE_INTERVAL).await;

    assert_eq!(
        harness.state(),
        TranscriptWatch::Degraded(WatcherDegraded::RootsMissing)
    );
}

#[tokio::test(start_paused = true)]
async fn a_hint_under_a_symlinked_nested_root_is_owned_by_the_nested_provider() {
    let base = pohunek_test_support::tempdir().expect("base");
    let claude_dir = base.path().join("claude");
    std::fs::create_dir_all(claude_dir.join("codex")).expect("tree");
    let link = base.path().join("codex-link");
    std::os::unix::fs::symlink(claude_dir.join("codex"), &link).expect("symlink");
    let (input, receiver) = mpsc::unbounded_channel();
    let backend = FakeBackend {
        input: receiver,
        shared: Arc::new(FakeShared::default()),
    };
    let sink = Arc::new(FakeSink::default());
    let roots = vec![
        claude_root(&claude_dir),
        TranscriptRoot {
            agent_base: AgentKind::Codex,
            path: link,
        },
    ];
    start(
        backend,
        roots,
        Arc::clone(&sink),
        CancellationToken::new(),
        None,
    )
    .await;

    let inside_claude_spelling = claude_dir.join("codex/2026/x.jsonl");
    let _ = input.send(Ok(vec![
        WatchEvent::Changed(inside_claude_spelling.clone()),
        WatchEvent::Changed(claude_dir.join("p/c.jsonl")),
    ]));
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;

    let mut agents = sink.agents.lock().expect("agents").clone();
    agents.sort_by(|left, right| left.1.cmp(&right.1));
    assert_eq!(
        agents,
        vec![
            (AgentKind::Codex, inside_claude_spelling),
            (AgentKind::Claude, claude_dir.join("p/c.jsonl")),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn an_incomplete_scan_is_degraded_until_a_later_pass_completes() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;
    harness.set_scan(RootScan::Incomplete);
    advance(RECONCILE_INTERVAL).await;
    assert_eq!(
        harness.state(),
        TranscriptWatch::Degraded(WatcherDegraded::ScanIncomplete)
    );

    harness.set_scan(RootScan::Complete);
    advance(RECONCILE_INTERVAL).await;

    assert_eq!(harness.state(), TranscriptWatch::Active);
}

#[tokio::test(start_paused = true)]
async fn a_rescan_waits_for_in_flight_parses_before_reconciling() {
    let gate = Arc::new(Semaphore::new(0));
    let harness = Harness::start(FakeRegistration::Complete, Some(Arc::clone(&gate))).await;

    harness.send(vec![WatchEvent::Changed(harness.path("a.jsonl"))]);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;
    assert_eq!(harness.upserts().len(), 1, "parse is blocked mid-flight");

    harness.send(vec![WatchEvent::Rescan]);
    settle().await;
    advance(RESCAN_MIN_INTERVAL).await;
    assert_eq!(
        harness.reconciles(),
        1,
        "a pass must not overlap an in-flight parse"
    );

    gate.add_permits(1);
    settle().await;
    assert_eq!(
        harness.reconciles(),
        2,
        "the pass runs once the parse is done"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_fast_path_parse_is_only_logged_and_the_next_pass_repairs_it() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;
    let path = harness.path("a.jsonl");
    harness.sink.fail_next.store(1, Ordering::SeqCst);

    harness.send(vec![WatchEvent::Changed(path.clone())]);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;
    advance(RETRY_BACKOFF_MAX).await;

    assert_eq!(harness.upserts(), vec![path], "no retry state exists");
    assert_eq!(harness.state(), TranscriptWatch::Active);
    assert!(harness.reconciles() >= 2, "the periodic pass covers it");
}

#[tokio::test(start_paused = true)]
async fn interrupted_and_would_block_reads_do_not_end_the_watcher() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;
    let path = harness.path("a.jsonl");

    let _ = harness.input.send(Err(io::ErrorKind::Interrupted));
    let _ = harness.input.send(Err(io::ErrorKind::WouldBlock));
    harness.send(vec![WatchEvent::Changed(path.clone())]);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;

    assert_eq!(harness.state(), TranscriptWatch::Active);
    assert_eq!(harness.upserts(), vec![path]);
}

#[tokio::test(start_paused = true)]
async fn the_index_converges_by_periodic_passes_while_the_backend_is_dead_and_recovers() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;
    let created = harness.path("created.jsonl");
    let _ = harness.input.send(Err(io::ErrorKind::Other));
    settle().await;
    assert_eq!(
        harness.state(),
        TranscriptWatch::Unavailable(WatcherUnavailable::BackendFailed)
    );

    harness
        .sink
        .disk
        .lock()
        .expect("disk")
        .insert(created.clone());
    advance(RECONCILE_INTERVAL).await;
    assert!(
        harness
            .sink
            .indexed
            .lock()
            .expect("indexed")
            .contains(&created),
        "a transcript created while the backend is dead is indexed by the periodic pass"
    );

    harness.sink.disk.lock().expect("disk").remove(&created);
    advance(RECONCILE_INTERVAL).await;
    assert!(
        !harness
            .sink
            .indexed
            .lock()
            .expect("indexed")
            .contains(&created),
        "and a removed one leaves it"
    );
    assert_eq!(
        harness.state(),
        TranscriptWatch::Unavailable(WatcherUnavailable::BackendFailed)
    );
    assert!(
        harness.shared.restart_calls.load(Ordering::SeqCst) >= 2,
        "restarts are attempted with backoff"
    );

    harness.shared.restart_ok.store(true, Ordering::SeqCst);
    advance(RETRY_BACKOFF_MAX).await;
    assert_eq!(harness.state(), TranscriptWatch::Active);

    let path = harness.path("live.jsonl");
    harness.send(vec![WatchEvent::Changed(path.clone())]);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;
    assert_eq!(harness.upserts(), vec![path], "live events flow again");
}

#[tokio::test(start_paused = true)]
async fn an_open_failure_starts_unavailable_and_recovers_on_restart() {
    let root = pohunek_test_support::tempdir().expect("root");
    let shared = Arc::new(FakeShared::default());
    let (_input, receiver) = mpsc::unbounded_channel();
    let backend = FakeBackend {
        input: receiver,
        shared: Arc::clone(&shared),
    };
    let sink = Arc::new(FakeSink::default());

    let handle = start(
        backend,
        vec![claude_root(root.path())],
        Arc::clone(&sink),
        CancellationToken::new(),
        Some(io::Error::other("cannot open")),
    )
    .await;

    assert_eq!(
        handle.state(),
        TranscriptWatch::Unavailable(WatcherUnavailable::OpenFailed)
    );
    assert_eq!(sink.reconciles.load(Ordering::SeqCst), 1);
    shared.restart_ok.store(true, Ordering::SeqCst);
    advance(RETRY_BACKOFF_MIN).await;
    assert_eq!(handle.state(), TranscriptWatch::Active);
    assert_eq!(sink.reconciles.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn nested_roots_own_their_own_transcripts_and_duplicates_are_dropped() {
    let base = pohunek_test_support::tempdir().expect("base");
    let outer = base.path().join("claude");
    let inner = outer.join("codex");
    std::fs::create_dir_all(&inner).expect("nested roots");
    let shared = Arc::new(FakeShared::default());
    let (input, receiver) = mpsc::unbounded_channel();
    let backend = FakeBackend {
        input: receiver,
        shared: Arc::clone(&shared),
    };
    let sink = Arc::new(FakeSink::default());
    let roots = vec![
        claude_root(&outer),
        TranscriptRoot {
            agent_base: AgentKind::Codex,
            path: inner.clone(),
        },
        TranscriptRoot {
            agent_base: AgentKind::Codex,
            path: outer.clone(),
        },
    ];
    start(
        backend,
        roots,
        Arc::clone(&sink),
        CancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(
        shared.register_calls.load(Ordering::SeqCst),
        2,
        "the duplicate root is not registered twice"
    );

    let claude = outer.join("p/c.jsonl");
    let codex = inner.join("2026/x.jsonl");
    let _ = input.send(Ok(vec![
        WatchEvent::Changed(claude.clone()),
        WatchEvent::Changed(codex.clone()),
    ]));
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;

    let mut agents = sink.agents.lock().expect("agents").clone();
    agents.sort_by(|left, right| left.1.cmp(&right.1));
    assert_eq!(
        agents,
        vec![(AgentKind::Codex, codex), (AgentKind::Claude, claude)]
    );
}

#[tokio::test(start_paused = true)]
async fn a_root_that_stops_being_an_alias_is_picked_up_by_the_next_pass() {
    let base = pohunek_test_support::tempdir().expect("base");
    std::fs::create_dir(base.path().join("a")).expect("a");
    std::fs::create_dir(base.path().join("b")).expect("b");
    let link = base.path().join("link");
    std::os::unix::fs::symlink(base.path().join("a"), &link).expect("link to a");
    let (_input, receiver) = mpsc::unbounded_channel();
    let backend = FakeBackend {
        input: receiver,
        shared: Arc::new(FakeShared::default()),
    };
    let sink = Arc::new(FakeSink::default());
    let roots = vec![claude_root(&base.path().join("a")), claude_root(&link)];
    start(
        backend,
        roots,
        Arc::clone(&sink),
        CancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(
        sink.passes.lock().expect("passes").last().cloned(),
        Some(vec![base.path().join("a")]),
        "the alias is dropped while it points at the same directory"
    );

    let staged = base.path().join("link.new");
    std::os::unix::fs::symlink(base.path().join("b"), &staged).expect("stage link");
    std::fs::rename(&staged, &link).expect("retarget atomically");
    advance(RECONCILE_INTERVAL).await;

    assert_eq!(
        sink.passes.lock().expect("passes").last().cloned(),
        Some(vec![base.path().join("a"), link.clone()]),
        "the retargeted root is a root of its own in the next pass"
    );

    std::fs::remove_file(&link).expect("remove link");
    std::os::unix::fs::symlink(base.path().join("a"), &link).expect("alias again");
    advance(RECONCILE_INTERVAL).await;
    assert_eq!(
        sink.passes.lock().expect("passes").last().cloned(),
        Some(vec![base.path().join("a")]),
        "and an alias again once it points back"
    );
}

#[tokio::test(start_paused = true)]
async fn a_new_directory_hint_registers_it_and_parses_the_transcripts_found() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;

    harness.send(vec![WatchEvent::NewDirectory(harness.path("moved-in"))]);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE).await;

    assert_eq!(
        harness.upserts(),
        vec![harness.path("moved-in/found.jsonl")]
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_the_runner() {
    let harness = Harness::start(FakeRegistration::Complete, None).await;

    harness.shutdown.cancel();
    settle().await;
    harness.send(vec![WatchEvent::Changed(harness.path("a.jsonl"))]);
    settle().await;
    advance(TRANSCRIPT_WRITE_DEBOUNCE * 2).await;

    assert!(harness.upserts().is_empty());
}

/// Backend whose registration blocks its thread until released, recording the
/// thread it ran on.
struct BlockingProbe {
    inner: FakeBackend,
    threads: Arc<Mutex<Vec<std::thread::ThreadId>>>,
    release: Option<std::sync::mpsc::Receiver<()>>,
}

impl WatchBackend for BlockingProbe {
    fn register_root(&mut self, root: &Path, cancel: &CancellationToken) -> RootRegistration {
        self.threads
            .lock()
            .expect("threads")
            .push(std::thread::current().id());
        if let Some(release) = &self.release {
            let _ = release.recv();
        }
        self.inner.register_root(root, cancel)
    }

    fn restart(&mut self) -> io::Result<()> {
        self.inner.restart()
    }

    fn unregister_all(&mut self) {
        self.inner.unregister_all();
    }

    fn next_events(
        &mut self,
        out: &mut Vec<WatchEvent>,
    ) -> impl Future<Output = io::Result<()>> + Send {
        self.inner.next_events(out)
    }
}

fn blocking_probe(
    root: &Path,
    release: Option<std::sync::mpsc::Receiver<()>>,
) -> (BlockingProbe, Arc<Mutex<Vec<std::thread::ThreadId>>>) {
    let (_input, receiver) = mpsc::unbounded_channel();
    let threads = Arc::new(Mutex::new(Vec::new()));
    let probe = BlockingProbe {
        inner: FakeBackend {
            input: receiver,
            shared: Arc::new(FakeShared::default()),
        },
        threads: Arc::clone(&threads),
        release,
    };
    let _ = root;
    (probe, threads)
}

#[tokio::test]
async fn registration_walks_run_off_the_runner_thread() {
    let root = pohunek_test_support::tempdir().expect("root");
    let (backend, threads) = blocking_probe(root.path(), None);

    start(
        backend,
        vec![claude_root(root.path())],
        Arc::new(FakeSink::default()),
        CancellationToken::new(),
        None,
    )
    .await;

    let threads = threads.lock().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_ne!(
        threads[0],
        std::thread::current().id(),
        "the walk must not run on the async worker"
    );
}

#[tokio::test]
async fn shutdown_interrupts_a_registration_that_is_still_walking() {
    let root = pohunek_test_support::tempdir().expect("root");
    let (release, blocked) = std::sync::mpsc::channel();
    let (backend, _threads) = blocking_probe(root.path(), Some(blocked));
    let shutdown = CancellationToken::new();
    let started = tokio::spawn(start(
        backend,
        vec![claude_root(root.path())],
        Arc::new(FakeSink::default()),
        shutdown.clone(),
        None,
    ));

    shutdown.cancel();
    let finished = tokio::time::timeout(Duration::from_secs(30), started).await;
    let _ = release.send(());

    assert!(
        finished.is_ok(),
        "start returns although the walk is blocked"
    );
}

/// Real index behind a scripted backend, on the real clock.
struct RealSinkHarness {
    input: Input,
    sessions: crate::external::ExternalSessions,
    index: crate::external::TranscriptIndex,
    root: tempfile::TempDir,
    shutdown: CancellationToken,
}

/// Upper bound on waiting for the real index to reflect a hint.
const REAL_SINK_TIMEOUT: Duration = Duration::from_secs(30);

impl RealSinkHarness {
    async fn start(prepare: impl FnOnce(&Path)) -> Self {
        let root = pohunek_test_support::tempdir().expect("root");
        prepare(root.path());
        let (input, receiver) = mpsc::unbounded_channel();
        let backend = FakeBackend {
            input: receiver,
            shared: Arc::new(FakeShared::default()),
        };
        let sessions = crate::external::ExternalSessions::new();
        let index = crate::external::TranscriptIndex::default();
        let sink = Arc::new(crate::external::IndexSink {
            index: index.clone(),
            sessions: sessions.clone(),
        });
        let shutdown = CancellationToken::new();
        start(
            backend,
            vec![claude_root(root.path())],
            sink,
            shutdown.clone(),
            None,
        )
        .await;
        Self {
            input,
            sessions,
            index,
            root,
            shutdown,
        }
    }

    fn is_indexed(&self, path: &Path) -> bool {
        self.index.inner.lock().expect("index").contains_key(path)
    }

    async fn wait_until_gone(&self, path: &Path) {
        let wait = async {
            while self.is_indexed(path) {
                self.sessions.inner.rescan.notified().await;
            }
        };
        tokio::time::timeout(REAL_SINK_TIMEOUT, wait)
            .await
            .unwrap_or_else(|_elapsed| panic!("{} stayed indexed", path.display()));
    }
}

impl Drop for RealSinkHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn replace_transcript_with_directory(root: &Path) -> PathBuf {
    let path = root.join("p/s.jsonl");
    std::fs::remove_file(&path).expect("remove transcript");
    std::fs::create_dir(&path).expect("directory takes the name");
    path
}

fn prepare_transcript(root: &Path) {
    std::fs::create_dir_all(root.join("p")).expect("project");
    std::fs::write(
        root.join("p/s.jsonl"),
        "{\"session_id\":\"stale\",\"cwd\":\"/tmp\"}\n",
    )
    .expect("transcript");
}

#[tokio::test]
async fn a_file_hint_for_a_transcript_replaced_by_a_directory_drops_the_candidate() {
    let harness = RealSinkHarness::start(prepare_transcript).await;
    let path = harness.root.path().join("p/s.jsonl");
    assert!(harness.is_indexed(&path));

    replace_transcript_with_directory(harness.root.path());
    let _ = harness
        .input
        .send(Ok(vec![WatchEvent::Changed(path.clone())]));

    harness.wait_until_gone(&path).await;
}

#[tokio::test]
async fn a_tree_hint_for_a_directory_over_a_transcript_drops_the_candidate() {
    let harness = RealSinkHarness::start(prepare_transcript).await;
    let path = harness.root.path().join("p/s.jsonl");
    assert!(harness.is_indexed(&path));

    replace_transcript_with_directory(harness.root.path());
    let _ = harness
        .input
        .send(Ok(vec![WatchEvent::TreeChanged(path.clone())]));

    harness.wait_until_gone(&path).await;
}
