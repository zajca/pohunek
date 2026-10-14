//! The event reaction dispatcher, `pohunek workflow __dispatch`.
//!
//! One persistent process per namespace runs every reaction of the enabled
//! plugins. It is not part of the daemon: it subscribes to the local daemon's
//! public `subscribe` stream with the CLI's own client, filters each event
//! against the reaction allowlists of the plugins loaded at its start, and
//! runs the matching reactions through the runner.
//!
//! - **Reading never waits for reactions.** The socket reader only routes:
//!   it serializes an event once when some reaction wants it and offers it to
//!   each matching plugin's queue with `try_send`. A full queue drops the
//!   event and bumps that plugin's drop counter, so a slow plugin cannot back
//!   up the stream.
//! - **One worker per plugin** runs that plugin's reactions one at a time, so
//!   a plugin never runs two of its reactions concurrently. Before it takes
//!   the next event, it writes the drops counted since its last record as one
//!   `dropped` run record, so one overflow episode yields one record with the
//!   count.
//! - **Each run is verified again** by the runner against the digest the
//!   dispatcher loaded. A plugin that changed or was disabled is refused and
//!   recorded; the lifecycle commands restart the dispatcher when the set of
//!   reacting plugins changes.
//! - **The stream reconnects** with exponential backoff between the owner's
//!   `reactions.reconnect_initial_ms` and `reactions.reconnect_max_ms`. It only
//!   connects, it never starts the daemon. Events published while it is
//!   disconnected are not replayed; a reaction that needs them reads the state
//!   through the CLI.
//! - **A stop request** (`SIGTERM`, `SIGINT`, `SIGHUP`) ends the stream, starts
//!   no further run and stops a running reaction's process group within the
//!   owner's kill grace, through the runner's interrupt source.
//!
//! The dispatcher resolves its paths from its environment the way the unit
//! trampoline does, ignoring an inherited socket or configuration override.

// Rust guideline compliant 2026-10-13

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pohunek_client::{Client, ClientError, ClientOptions, OriginSource, Subscription};
use pohunek_platform::supervisor::MAX_JOB_VALUE_BYTES;
use protocol::{method, Event, PackageDigest, PackageId, Request};
use serde::Serialize;
use serde_json::value::RawValue;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch};

use super::config::{self, Knobs, DEFAULT_RESTART_MAX_MS};
use super::manifest::reactable;
use super::runner::{self, Delivery, Interrupts, ReactionEnv};
use super::store::Store;
use super::supervised::trampoline_paths;
use super::Error;
use crate::paths::WorkflowDirs;

/// Pause before a dispatcher with no reacting plugin exits, and before a
/// dispatcher that cannot start exits. It is the longest service restart delay
/// the owner may set, so a unit that restarts the dispatcher in a loop does not
/// busy-spin. Restarts that follow a refusal are paced the same way.
const REFUSED_EXIT_PAUSE: Duration = Duration::from_millis(DEFAULT_RESTART_MAX_MS);

/// Exit status of a dispatcher that could not start.
const START_FAILED: u8 = 1;

/// Argument name of the daemon's `session_id` in an event payload, and the
/// nested path `session.id` used by session events.
const SESSION_ID_FIELD: &str = "session_id";

/// The `session.id` path of session events, as a JSON pointer.
const SESSION_POINTER: &str = "/session/id";

/// An event as it is written into a reaction's context.
#[derive(Serialize)]
struct EventDocument<'a> {
    event: &'a str,
    payload: &'a serde_json::Value,
}

/// One reaction of a plugin, with the events it subscribes to.
#[derive(Debug)]
struct Reacting {
    name: String,
    events: BTreeSet<String>,
}

/// One event waiting for a reaction of its plugin.
#[derive(Debug)]
struct Pending {
    reaction: String,
    event: Arc<RawValue>,
    session_id: Option<String>,
}

/// A plugin the dispatcher reacts for, as loaded at its start.
#[derive(Debug)]
struct Plugin {
    id: PackageId,
    /// The archive digest the plugin was verified at; a run whose files changed
    /// since is refused by the runner.
    digest: PackageDigest,
    reactions: Vec<Reacting>,
}

/// A plugin's queue and drop counter, as the reader routes into them.
#[derive(Debug)]
struct Route {
    reactions: Vec<Reacting>,
    queue: mpsc::Sender<Pending>,
    dropped: Arc<AtomicU64>,
}

/// Runs the dispatcher until a stop request arrives, and returns its exit
/// status.
///
/// Must run inside the CLI's Tokio runtime, which registers the stop signals.
pub(crate) async fn execute() -> ExitCode {
    let uid = nix::unistd::Uid::effective().as_raw();
    let (paths, runtime_dir_ignored) = match trampoline_paths(uid) {
        Ok(resolved) => resolved,
        Err(error) => return refuse_start(&error.to_string()).await,
    };
    let dirs = match WorkflowDirs::new(&paths.state_dir, &paths.config_dir) {
        Ok(dirs) => dirs,
        Err(error) => return refuse_start(&error.to_string()).await,
    };
    let store = Arc::new(Store::new(dirs));
    let knobs = match config::load(store.knobs_path()) {
        Ok(knobs) => knobs,
        Err(error) => return refuse_start(&error.to_string()).await,
    };
    let cli = match std::env::current_exe() {
        Ok(cli) => cli,
        Err(error) => return refuse_start(&error.to_string()).await,
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let signals = match stop_signals() {
        Ok(signals) => signals,
        Err(error) => return refuse_start(&error.to_string()).await,
    };
    tokio::spawn(forward_stop(signals, stop_tx));

    let plugins = match load_plugins(&store) {
        Ok(plugins) => plugins,
        Err(error) => return refuse_start(&error.to_string()).await,
    };
    if plugins.is_empty() {
        eprintln!("no enabled workflow plugin declares a reaction; the dispatcher stops");
        wait_or_stop(REFUSED_EXIT_PAUSE, stop_rx).await;
        return ExitCode::SUCCESS;
    }

    let mut workers = Vec::with_capacity(plugins.len());
    let mut routes = Vec::with_capacity(plugins.len());
    for plugin in plugins {
        let (queue, pending) = mpsc::channel(queue_capacity(&knobs));
        let dropped = Arc::new(AtomicU64::new(0));
        let worker = Worker {
            id: plugin.id,
            digest: plugin.digest,
            dropped: Arc::clone(&dropped),
            pending,
            store: Arc::clone(&store),
            knobs,
            cli: cli.clone(),
            runtime_dir_ignored,
            stop: stop_rx.clone(),
        };
        workers.push(tokio::spawn(worker.run()));
        routes.push(Route {
            reactions: plugin.reactions,
            queue,
            dropped,
        });
    }

    read_stream(&paths.socket, &routes, &knobs, stop_rx).await;
    // Dropping the queues ends each worker once its current run has stopped.
    // A run still going was already stopped by the shutdown value.
    drop(routes);
    for worker in workers {
        if let Err(error) = worker.await {
            eprintln!("warning: a reaction worker ended abnormally: {error}");
        }
    }
    ExitCode::SUCCESS
}

/// Reports why the dispatcher cannot start, waits the pause a refused unit
/// waits, and returns the failure status. A stop during the pause ends it at
/// once.
async fn refuse_start(reason: &str) -> ExitCode {
    eprintln!("workflow reactions cannot start: {reason}");
    let (_stop_tx, stop_rx) = watch::channel(false);
    wait_or_stop(REFUSED_EXIT_PAUSE, stop_rx).await;
    ExitCode::from(START_FAILED)
}

/// Sleeps for `pause`, or until `stop` holds `true`.
async fn wait_or_stop(pause: Duration, mut stop: watch::Receiver<bool>) {
    tokio::select! {
        () = tokio::time::sleep(pause) => {}
        _ = stop.wait_for(|stopped| *stopped) => {}
    }
}

/// The stop signals the dispatcher handles.
struct StopSignals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

fn stop_signals() -> std::io::Result<StopSignals> {
    Ok(StopSignals {
        terminate: signal(SignalKind::terminate())?,
        interrupt: signal(SignalKind::interrupt())?,
        hangup: signal(SignalKind::hangup())?,
    })
}

/// Sets the shutdown value on the first stop signal.
async fn forward_stop(mut signals: StopSignals, stop: watch::Sender<bool>) {
    tokio::select! {
        _ = signals.terminate.recv() => {}
        _ = signals.interrupt.recv() => {}
        _ = signals.hangup.recv() => {}
    }
    stop.send_replace(true);
}

/// The queue capacity of one plugin, from the owner's knob.
fn queue_capacity(knobs: &Knobs) -> usize {
    usize::try_from(knobs.reactions.queue_depth).unwrap_or(usize::MAX)
}

/// Loads every enabled plugin that declares reactions and verifies it.
///
/// A plugin whose files fail verification is left out and reported, so it
/// reacts to nothing; the runner would refuse its runs anyway.
fn load_plugins(store: &Store) -> Result<Vec<Plugin>, Error> {
    let mut plugins = Vec::new();
    for record in store.list()? {
        if !record.enabled() {
            continue;
        }
        let id = record.identity().id.clone();
        let (verified, _root, manifest) = match store.inspect(&id) {
            Ok(inspected) => inspected,
            Err(error) => {
                eprintln!(
                    "warning: workflow plugin {id} is not verified and reacts to nothing: {}",
                    error.code()
                );
                continue;
            }
        };
        if !verified.enabled() || manifest.reactions.is_empty() {
            continue;
        }
        let reactions = manifest
            .reactions
            .iter()
            .map(|(name, reaction)| Reacting {
                name: name.clone(),
                events: reaction.events.iter().cloned().collect(),
            })
            .collect();
        plugins.push(Plugin {
            id,
            digest: record.digest().clone(),
            reactions,
        });
    }
    Ok(plugins)
}

/// Connects to the daemon's public stream and reads events until a stop
/// request, reconnecting with backoff whenever the stream ends.
async fn read_stream(
    socket: &Path,
    routes: &[Route],
    knobs: &Knobs,
    mut stop: watch::Receiver<bool>,
) {
    let initial = Duration::from_millis(knobs.reactions.reconnect_initial_ms);
    let longest = Duration::from_millis(knobs.reactions.reconnect_max_ms);
    let mut delay = initial;
    loop {
        if *stop.borrow() {
            return;
        }
        match subscribe(socket).await {
            Ok(mut subscription) => {
                let mut delivered = false;
                let outcome =
                    route_events(&mut subscription, routes, &mut stop, &mut delivered).await;
                // A subscription that accepted and closed without an event does
                // not restart the backoff, so such a daemon is retried ever more slowly.
                if delivered {
                    delay = initial;
                }
                if outcome == Stream::Stopped {
                    return;
                }
                eprintln!("the daemon event stream ended; reconnecting");
            }
            Err(error) => {
                eprintln!(
                    "the daemon is not reachable, retrying in {} ms: {}",
                    delay.as_millis(),
                    error
                );
            }
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            _ = stop.wait_for(|stopped| *stopped) => return,
        }
        delay = delay.saturating_mul(2).min(longest);
    }
}

/// How a read of the event stream ended.
#[derive(Debug, PartialEq, Eq)]
enum Stream {
    /// The stop request arrived.
    Stopped,
    /// The connection ended or failed.
    Ended,
}

/// Connects to the local daemon and starts its event stream.
///
/// The connection carries no origin pair, whatever this process's environment
/// holds, and never starts the daemon.
async fn subscribe(socket: &Path) -> Result<Subscription, ClientError> {
    let options = ClientOptions::default().with_origin_source(OriginSource::Omitted);
    let client = Client::connect_local_with_options(socket, options).await?;
    let request = Request::new(
        pohunek_client::next_request_id(method::SUBSCRIBE),
        method::SUBSCRIBE,
        serde_json::Value::Null,
    )
    .map_err(ClientError::from)?;
    client.subscribe(&request).await
}

/// Routes the events of one subscription until the stream ends or a stop
/// request arrives. `delivered` is set once the subscription has delivered an
/// event.
async fn route_events(
    subscription: &mut Subscription,
    routes: &[Route],
    stop: &mut watch::Receiver<bool>,
    delivered: &mut bool,
) -> Stream {
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => return Stream::Stopped,
            next = subscription.next_event() => match next {
                Ok(Some(event)) => {
                    *delivered = true;
                    route(&event, routes);
                }
                Ok(None) | Err(_) => return Stream::Ended,
            },
        }
    }
}

/// Offers `event` to every reaction that subscribes to it.
///
/// The event is serialized at most once, and only when some reaction wants
/// it. The send never waits: a full queue counts a drop instead.
fn route(event: &Event, routes: &[Route]) {
    let name = event.event();
    if !reactable(name) {
        return;
    }
    let wanted: Vec<(&Route, &str)> = routes
        .iter()
        .flat_map(|route| {
            route
                .reactions
                .iter()
                .filter(|reacting| reacting.events.contains(name))
                .map(move |reacting| (route, reacting.name.as_str()))
        })
        .collect();
    if wanted.is_empty() {
        return;
    }
    let payload = event.payload();
    let document = match serde_json::value::to_raw_value(&EventDocument {
        event: name,
        payload,
    }) {
        Ok(document) => Arc::<RawValue>::from(document),
        Err(error) => {
            eprintln!("warning: an event could not be serialized: {error}");
            return;
        }
    };
    let session_id = session_id_of(payload);
    for (route, reaction) in wanted {
        let pending = Pending {
            reaction: reaction.to_owned(),
            event: Arc::clone(&document),
            session_id: session_id.clone(),
        };
        if let Err(mpsc::error::TrySendError::Full(_)) = route.queue.try_send(pending) {
            route.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// The session an event concerns: the payload's `session_id`, or its
/// `session.id` for the session events.
fn session_id_of(payload: &serde_json::Value) -> Option<String> {
    payload
        .get(SESSION_ID_FIELD)
        .or_else(|| payload.pointer(SESSION_POINTER))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= MAX_JOB_VALUE_BYTES)
        .map(str::to_owned)
}

/// One plugin's reaction worker.
struct Worker {
    id: PackageId,
    digest: PackageDigest,
    dropped: Arc<AtomicU64>,
    pending: mpsc::Receiver<Pending>,
    store: Arc<Store>,
    knobs: Knobs,
    cli: PathBuf,
    runtime_dir_ignored: bool,
    stop: watch::Receiver<bool>,
}

impl Worker {
    /// Runs the queued reactions one at a time, writing the drop record of
    /// each overflow episode before it takes the next event.
    async fn run(mut self) {
        loop {
            self.record_drops().await;
            if *self.stop.borrow() {
                break;
            }
            let Some(pending) = self.pending.recv().await else {
                break;
            };
            if *self.stop.borrow() {
                break;
            }
            self.run_one(pending).await;
        }
        self.record_drops().await;
    }

    /// Writes one `dropped` record for the drops counted since the last one.
    async fn record_drops(&self) {
        let count = self.dropped.swap(0, Ordering::SeqCst);
        if count == 0 {
            return;
        }
        let (store, knobs, id) = (Arc::clone(&self.store), self.knobs, self.id.clone());
        match tokio::task::spawn_blocking(move || {
            runner::record_dropped(&store, &knobs, &id, count)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!(
                "warning: {count} dropped events of workflow plugin {} were not recorded: {}",
                self.id,
                error.code()
            ),
            Err(error) => eprintln!(
                "warning: {count} dropped events of workflow plugin {} were not recorded: {error}",
                self.id
            ),
        }
    }

    /// Runs one reaction for `pending` on a blocking thread, since the runner
    /// waits for its child.
    async fn run_one(&self, pending: Pending) {
        let (store, knobs, id, digest, cli, runtime_dir_ignored) = (
            Arc::clone(&self.store),
            self.knobs,
            self.id.clone(),
            self.digest.clone(),
            self.cli.clone(),
            self.runtime_dir_ignored,
        );
        let stop = self.stop.clone();
        let name = pending.reaction.clone();
        let result = tokio::task::spawn_blocking(move || {
            // Without interrupt handlers no run can be stopped, so none starts.
            let mut interrupts = match Interrupts::new() {
                Ok(interrupts) => interrupts.with_shutdown(stop),
                Err(error) => {
                    return Err(format!("the stop signals cannot be registered: {error}"));
                }
            };
            let delivery = Delivery {
                reaction: &pending.reaction,
                event: &pending.event,
                session_id: pending.session_id.as_deref(),
            };
            let env = ReactionEnv {
                cli: &cli,
                runtime_dir_ignored,
            };
            runner::run_reaction(
                &store,
                &knobs,
                &id,
                &digest,
                &delivery,
                &env,
                &mut interrupts,
            )
            .map(drop)
            .map_err(|error| error.code().to_owned())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => eprintln!(
                "reaction {name} of workflow plugin {} did not run: {reason}",
                self.id
            ),
            Err(error) => eprintln!(
                "reaction {name} of workflow plugin {} panicked: {error}",
                self.id
            ),
        }
    }
}
