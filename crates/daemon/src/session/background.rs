//! Background tasks of a session registry and their shutdown.
//!
//! Every task the registry spawns for its own bookkeeping (watchers, exit
//! committers, forwarders, retries) goes through [`BackgroundTasks`], and so
//! does every blocking store write. [`BackgroundTasks::shutdown`] closes
//! admission, cancels what is cooperative, aborts what is not, and waits until
//! no tracked work is left; after that the registry is quiescent and a store
//! write through it is a defect (see `SessionRegistry::store_for_write`).

// Rust guideline compliant 2026-10-01

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How a tracked task ends when the registry shuts down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OnShutdown {
    /// The task is aborted at its next await point. For work blocked on I/O
    /// that has no cancellation of its own, and for waiters on other tasks.
    Abort,
    /// The task watches a token that shutdown cancels (a child of the root
    /// token, or the daemon-wide shutdown token) and ends by itself; shutdown
    /// waits for it instead of cutting it off mid-commit.
    Cooperative,
    /// The task is a finite transaction with the worker or the store that must
    /// not be cut in half: shutdown waits for it, and a test can wait for the
    /// transactions in flight with [`BackgroundTasks::settle`].
    Transaction,
}

/// What [`BackgroundTasks::shutdown`] found.
#[must_use = "a report that is not quiescent means a task outlived the deadline"]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Names of the tasks still running when the deadline passed.
    pub still_running: Vec<&'static str>,
    /// Tracked work (tasks and blocking store writes) still outstanding, plus
    /// the external observer's tasks when they outlived the deadline.
    pub outstanding: usize,
}

impl ShutdownReport {
    /// Whether every tracked task ended before the deadline.
    #[must_use]
    pub fn is_quiescent(&self) -> bool {
        self.outstanding == 0
    }
}

#[derive(Debug)]
struct LiveTask {
    name: &'static str,
    abort: Option<AbortHandle>,
}

type LiveTasks = Arc<Mutex<BTreeMap<u64, LiveTask>>>;

/// Removes the task from the live set when its future ends or is dropped.
struct LiveGuard {
    live: LiveTasks,
    id: u64,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// Counts one transaction or blocking store write in flight until dropped.
struct InFlight {
    count: Arc<watch::Sender<usize>>,
}

impl InFlight {
    fn enter(count: &Arc<watch::Sender<usize>>) -> Self {
        count.send_modify(|in_flight| *in_flight += 1);
        Self {
            count: Arc::clone(count),
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.count
            .send_modify(|in_flight| *in_flight = in_flight.saturating_sub(1));
    }
}

/// Tracker, root cancellation token and quiescence state of one registry.
#[derive(Debug)]
pub(super) struct BackgroundTasks {
    tracker: TaskTracker,
    /// Serializes admission against closing the tracker.
    admission: Mutex<()>,
    /// Parent of every per-session token; cancelled when shutdown starts.
    root: CancellationToken,
    /// Set once shutdown has waited for the tracked work.
    quiesced: AtomicBool,
    next_id: AtomicU64,
    live: LiveTasks,
    /// Transactions and blocking store writes in flight.
    in_flight: Arc<watch::Sender<usize>>,
}

impl Default for BackgroundTasks {
    fn default() -> Self {
        Self {
            tracker: TaskTracker::new(),
            admission: Mutex::new(()),
            root: CancellationToken::new(),
            quiesced: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            live: Arc::new(Mutex::new(BTreeMap::new())),
            in_flight: Arc::new(watch::channel(0).0),
        }
    }
}

impl BackgroundTasks {
    /// A token that shutdown cancels.
    pub(super) fn child_token(&self) -> CancellationToken {
        self.root.child_token()
    }

    /// Whether shutdown has waited for the tracked work.
    pub(super) fn is_quiescent(&self) -> bool {
        self.quiesced.load(Ordering::Acquire)
    }

    /// Spawns `task` as `name`.
    ///
    /// After shutdown started the task is refused: the returned handle is
    /// already aborted and the future never runs.
    pub(super) fn spawn<F>(
        &self,
        name: &'static str,
        on_shutdown: OnShutdown,
        task: F,
    ) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.tracker.is_closed() {
            // The handle's output type is the task's, so the refusal is a task
            // of that type that drops the future unpolled and parks until the
            // abort below lands.
            let refused = self.tracker.spawn(async move {
                drop(task);
                std::future::pending::<F::Output>().await
            });
            refused.abort();
            return refused;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, LiveTask { name, abort: None });
        let guard = LiveGuard {
            live: Arc::clone(&self.live),
            id,
        };
        let in_flight =
            (on_shutdown == OnShutdown::Transaction).then(|| InFlight::enter(&self.in_flight));
        let handle = self.tracker.spawn(async move {
            let _guard = guard;
            let _in_flight = in_flight;
            task.await
        });
        if on_shutdown == OnShutdown::Abort {
            if let Some(live) = self
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_mut(&id)
            {
                live.abort = Some(handle.abort_handle());
            }
        }
        handle
    }

    /// Runs `work` on the blocking pool as `name`, tracked so shutdown waits
    /// for it and names it in the report while it has not returned.
    pub(super) fn spawn_blocking<F, R>(&self, name: &'static str, work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, LiveTask { name, abort: None });
        let guard = LiveGuard {
            live: Arc::clone(&self.live),
            id,
        };
        let in_flight = InFlight::enter(&self.in_flight);
        self.tracker.spawn_blocking(move || {
            let _guard = guard;
            let _in_flight = in_flight;
            work()
        })
    }

    /// Waits until no transaction and no blocking store write is in flight.
    ///
    /// Unlike [`Self::shutdown`] it leaves the registry running: a transaction
    /// a transaction spawns is counted before its parent ends, so the count
    /// does not reach zero in between.
    #[cfg(test)]
    pub(super) async fn settle(&self) {
        let mut in_flight = self.in_flight.subscribe();
        let _ = in_flight.wait_for(|count| *count == 0).await;
    }

    /// Whether no transaction and no blocking store write is in flight now.
    #[cfg(test)]
    pub(super) fn is_settled(&self) -> bool {
        *self.in_flight.borrow() == 0
    }

    /// Refuses every later [`Self::spawn`].
    fn close_admission(&self) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.tracker.close();
    }

    /// Stops admission, cancels and aborts the tracked tasks, and waits up to
    /// `timeout` for all tracked work. Afterwards the registry is quiescent
    /// even when the report names stragglers: their later store writes are
    /// refused.
    pub(super) async fn shutdown(&self, timeout: Duration) -> ShutdownReport {
        self.close_admission();
        self.root.cancel();
        let aborts: Vec<AbortHandle> = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter_map(|task| task.abort.clone())
            .collect();
        for abort in aborts {
            abort.abort();
        }
        let _ = tokio::time::timeout(timeout, self.tracker.wait()).await;
        self.quiesced.store(true, Ordering::Release);
        let mut still_running: Vec<&'static str> = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|task| task.name)
            .collect();
        still_running.sort_unstable();
        ShutdownReport {
            still_running,
            outstanding: self.tracker.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry bound is the behavior under test; it elapses on the paused
    // clock, which `advance` moves even while a blocking task is in flight.
    // timing-allowed: #1725 the registry bound is the policy under test
    const BOUND: Duration = Duration::from_secs(5);

    #[tokio::test(start_paused = true)]
    async fn wedged_blocking_write_is_named_in_the_registry_report() {
        let tasks = BackgroundTasks::default();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let (started, running) = std::sync::mpsc::channel::<()>();
        let write = tasks.spawn_blocking("record_session", move || {
            started.send(()).expect("test is waiting");
            let _ = held.recv();
        });
        running
            .recv_timeout(pohunek_test_support::wait::HANG_GUARD)
            .expect("the write started");

        let shutdown = tasks.shutdown(BOUND);
        tokio::pin!(shutdown);
        tokio::select! {
            _ = &mut shutdown => panic!("the shutdown ended while the write is wedged"),
            () = tokio::task::yield_now() => {}
        }
        tokio::time::advance(BOUND).await;
        let report = tokio::select! {
            report = &mut shutdown => report,
            () = tokio::task::yield_now() => panic!("the shutdown ignored its bound"),
        };

        assert_eq!(report.still_running, ["record_session"]);
        assert_eq!(report.outstanding, 1);
        assert!(tasks.is_quiescent(), "quiescent even with a straggler");
        release.send(()).expect("the write is waiting");
        write.await.expect("the write ends once released");
        let report = tasks.shutdown(BOUND).await;
        assert!(report.still_running.is_empty(), "{report:?}");
        assert!(report.is_quiescent());
    }
}
