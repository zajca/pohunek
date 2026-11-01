//! The tasks that serve accepted control connections, tracked per transport.
//!
//! A [`ConnectionTasks`] owns the connection tasks of one transport (the local
//! socket, or every remote listener) and the token that tells them the daemon
//! is stopping. Registered as a shutdown step of the drivers stage, it cancels
//! the token, closes its tracker and waits for the tasks, so a connection is
//! quiet before the session registry shuts down; a connection still running at
//! the stage's bound is named in the shutdown report and in a log line that
//! lists the open connections.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use protocol::SessionId;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::warn;

use crate::shutdown::{DaemonShutdown, Stage};

/// What one connection is doing, as the log of a straggler names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ConnectionState {
    /// Waiting for the next request line.
    Idle,
    /// Serving one request of the named method.
    Request(String),
    /// Streaming session and notification events.
    Subscription,
    /// Bridging a raw attach stream to the session.
    Attach(SessionId),
}

impl ConnectionState {
    fn describe(&self) -> String {
        match self {
            Self::Idle => "idle".to_owned(),
            Self::Request(method) => format!("request {method}"),
            Self::Subscription => "subscription".to_owned(),
            Self::Attach(session) => format!("attach {}", session.0),
        }
    }
}

#[derive(Debug)]
struct Live {
    peer: Option<String>,
    state: ConnectionState,
}

#[derive(Debug)]
struct Inner {
    name: &'static str,
    tracker: TaskTracker,
    shutdown: CancellationToken,
    next_id: AtomicU64,
    live: Mutex<BTreeMap<u64, Live>>,
}

impl Inner {
    fn live(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Live>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The connection tasks of one transport and their shutdown token.
#[derive(Debug, Clone)]
pub struct ConnectionTasks {
    inner: Arc<Inner>,
}

impl ConnectionTasks {
    /// Creates the tracker of the transport `name`. Its connections stop when
    /// `shutdown` is cancelled, which `pohunekd` does at the stop signal.
    #[must_use]
    pub fn new(name: &'static str, shutdown: &CancellationToken) -> Self {
        Self {
            inner: Arc::new(Inner {
                name,
                tracker: TaskTracker::new(),
                shutdown: shutdown.child_token(),
                next_id: AtomicU64::new(0),
                live: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Registers the step that stops and awaits these connections in the
    /// drivers stage of `tasks`, under the step name given to [`Self::new`].
    pub fn register_shutdown_step(&self, tasks: &DaemonShutdown) {
        let connections = self.clone();
        tasks.add_step(self.inner.name, Stage::Driver, move || async move {
            let mut unfinished = Unfinished {
                connections: &connections,
                finished: false,
            };
            connections.wait().await;
            unfinished.finished = true;
        });
    }

    /// Cancels the connections' token, closes the tracker and waits for every
    /// connection task to end.
    async fn wait(&self) {
        self.inner.shutdown.cancel();
        self.inner.tracker.close();
        self.inner.tracker.wait().await;
    }

    /// Serves one accepted connection on a tracked task. `serve` receives the
    /// connection's handle; the connection is listed as open until its future
    /// ends or is dropped.
    pub(super) fn spawn<F, Fut>(&self, peer: Option<String>, serve: F)
    where
        F: FnOnce(Connection) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.inner.live().insert(
            id,
            Live {
                peer,
                state: ConnectionState::Idle,
            },
        );
        let connection = Connection {
            id,
            inner: Arc::clone(&self.inner),
        };
        let listed = ListedConnection(connection.clone());
        let serving = serve(connection);
        self.inner.tracker.spawn(async move {
            let _listed = listed;
            serving.await;
        });
    }

    /// The open connections as `peer: state` lines.
    fn describe_open(&self) -> Vec<String> {
        self.inner
            .live()
            .values()
            .map(|live| {
                format!(
                    "{}: {}",
                    live.peer.as_deref().unwrap_or("local"),
                    live.state.describe()
                )
            })
            .collect()
    }
}

/// Logs the open connections when the shutdown step is dropped unfinished,
/// which is how the drivers stage ends a step that outlived its bound.
struct Unfinished<'a> {
    connections: &'a ConnectionTasks,
    finished: bool,
}

impl Drop for Unfinished<'_> {
    fn drop(&mut self) {
        if !self.finished {
            warn!(
                transport = self.connections.inner.name,
                open = ?self.connections.describe_open(),
                "connections were still open when the shutdown bound passed"
            );
        }
    }
}

/// The handle one connection task holds: its shutdown token and its state.
#[derive(Debug, Clone)]
pub(super) struct Connection {
    id: u64,
    inner: Arc<Inner>,
}

impl Connection {
    /// Cancelled when the daemon is stopping.
    pub(super) fn token(&self) -> &CancellationToken {
        &self.inner.shutdown
    }

    /// Records what the connection is doing now.
    pub(super) fn set_state(&self, state: ConnectionState) {
        if let Some(live) = self.inner.live().get_mut(&self.id) {
            live.state = state;
        }
    }
}

/// Removes the connection from the open list when its task ends or is dropped.
struct ListedConnection(Connection);

impl Drop for ListedConnection {
    fn drop(&mut self) {
        self.0.inner.live().remove(&self.0.id);
    }
}
