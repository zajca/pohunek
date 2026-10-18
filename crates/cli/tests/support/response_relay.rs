//! A transparent Unix-socket relay that holds back one daemon response kind.
//!
//! [`ResponseRelay::install`] renames the daemon's live control-socket node to
//! a sibling path, binds a listener at the original path, and copies bytes
//! between every connecting client and the real daemon. Request lines pass
//! through untouched; a response line that answers a watched request is held
//! for the configured delay before it is forwarded. The daemon stays the real
//! product process serving real responses — the relay only postpones their
//! delivery, so a scenario can drive the built CLI past the SDK's default 5 s
//! response budget without fabricating any product behavior.
//!
//! Threads are bounded by endpoint lifetimes: a CLI child ends within the
//! bounded runner's deadline and the daemon is owned by the test's host guard,
//! so every copy thread sees EOF once both sides are gone. `Drop` wakes the
//! accepting thread, shuts down all tracked endpoints, joins every serving
//! thread, and restores the daemon socket even when a scenario unwinds.

// Rust guideline compliant 2026-10-18

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Bytes read from one socket side before complete lines are processed.
const READ_CHUNK_BYTES: usize = 16 * 1024;

/// Preserve the real daemon control socket's owner-only access mode.
const SOCKET_MODE: u32 = 0o600;

/// A transparent relay in front of a running daemon's control socket.
pub(crate) struct ResponseRelay {
    /// The path the daemon's socket node was renamed to.
    backend: PathBuf,
    /// The original socket path, where the relay's listener is bound.
    front: PathBuf,
    /// Set when the relay must stop accepting.
    stop: Arc<AtomicBool>,
    /// The accepting thread; joined on drop.
    accepter: Option<std::thread::JoinHandle<Vec<std::thread::JoinHandle<()>>>>,
    /// Endpoint clones used to interrupt every serving thread during cleanup.
    connections: Arc<Mutex<Vec<UnixStream>>>,
    /// Actual hold time of every delayed response, in arrival order.
    holds: Arc<Mutex<Vec<Duration>>>,
}

impl ResponseRelay {
    /// Installs the relay in front of the daemon socket at `front`.
    ///
    /// The caller must complete a real daemon RPC before installation, so
    /// socket activation and identity validation have finished.
    /// Responses answering a `watched_method` request are held for `delay`
    /// before delivery. Fails when the daemon's socket node cannot be renamed
    /// or the listener cannot bind the original path.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] when renaming the live socket
    /// node or binding the listener fails.
    pub(crate) fn install(
        front: &Path,
        watched_method: &'static str,
        delay: Duration,
    ) -> std::io::Result<Self> {
        let backend = sibling_path(front, "relay-backing");
        // The daemon keeps listening on the renamed node; only the path moves.
        fs::rename(front, &backend)?;
        let listener = match UnixListener::bind(front).and_then(|listener| {
            fs::set_permissions(front, fs::Permissions::from_mode(SOCKET_MODE))?;
            Ok(listener)
        }) {
            Ok(listener) => listener,
            // Leave the daemon's socket node at the original path.
            Err(error) => {
                let _ = fs::rename(&backend, front);
                return Err(error);
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let holds = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let accepter = {
            let connections = Arc::clone(&connections);
            let stop = Arc::clone(&stop);
            let holds = Arc::clone(&holds);
            let backend = backend.clone();
            std::thread::spawn(move || {
                let mut serving = Vec::new();
                while let Ok((client, _)) = listener.accept() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(control) = client.try_clone() else {
                        continue;
                    };
                    connections.lock().expect("connections lock").push(control);
                    let holds = Arc::clone(&holds);
                    let backend = backend.clone();
                    let connections = Arc::clone(&connections);
                    let stop = Arc::clone(&stop);
                    serving.push(std::thread::spawn(move || {
                        serve(
                            client,
                            &backend,
                            watched_method,
                            delay,
                            holds,
                            &connections,
                            &stop,
                        );
                    }));
                }
                serving
            })
        };
        Ok(Self {
            backend,
            front: front.to_path_buf(),
            stop,
            accepter: Some(accepter),
            connections,
            holds,
        })
    }

    /// The actual hold time of every delayed response, in arrival order.
    pub(crate) fn holds(&self) -> Vec<Duration> {
        self.holds.lock().expect("hold times lock").clone()
    }
}

impl Drop for ResponseRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the blocked accept: the accepting thread observes the flag and
        // exits without serving the dummy connection.
        drop(UnixStream::connect(&self.front));
        let serving = self
            .accepter
            .take()
            .and_then(|thread| thread.join().ok())
            .unwrap_or_default();
        // No new client can be registered after the accepter has joined. A
        // late backend connection checks `stop` under this same lock.
        for stream in self.connections.lock().expect("connections lock").drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        for thread in serving {
            let _ = thread.join();
        }
        // Restore the daemon's socket node so the original path works again.
        if self.backend.exists() {
            let _ = fs::rename(&self.backend, &self.front);
        } else {
            let _ = fs::remove_file(&self.front);
        }
    }
}

/// `parent/<name>.<suffix>` beside `path`.
fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .expect("socket path has a file name")
        .to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Copies one client connection against the daemon behind `backend`.
fn serve(
    client: UnixStream,
    backend: &Path,
    watched_method: &'static str,
    delay: Duration,
    holds: Arc<Mutex<Vec<Duration>>>,
    connections: &Mutex<Vec<UnixStream>>,
    stop: &AtomicBool,
) {
    let Ok(daemon) = UnixStream::connect(backend) else {
        return;
    };
    let Ok(control) = daemon.try_clone() else {
        return;
    };
    let mut endpoints = connections.lock().expect("connections lock");
    if stop.load(Ordering::Acquire) {
        let _ = client.shutdown(Shutdown::Both);
        let _ = daemon.shutdown(Shutdown::Both);
        return;
    }
    endpoints.push(control);
    drop(endpoints);
    let Ok(client_read) = client.try_clone() else {
        return;
    };
    let Ok(daemon_read) = daemon.try_clone() else {
        return;
    };
    // Correlation ids of the watched requests seen on this connection.
    let watched = Arc::new(Mutex::new(HashSet::new()));
    let requests = {
        let watched = Arc::clone(&watched);
        let direction = Direction::Requests {
            watched_method,
            watched,
        };
        std::thread::spawn(move || {
            pump(client_read, daemon, &direction);
        })
    };
    let responses = {
        let watched = Arc::clone(&watched);
        let direction = Direction::Responses {
            watched,
            delay,
            holds,
        };
        std::thread::spawn(move || {
            pump(daemon_read, client, &direction);
        })
    };
    let _ = requests.join();
    let _ = responses.join();
}

/// What one copy direction does with each complete line.
enum Direction {
    /// Client to daemon: record the ids of watched requests.
    Requests {
        /// The wire method whose responses must be delayed.
        watched_method: &'static str,
        /// Correlation ids of the watched requests seen so far.
        watched: Arc<Mutex<HashSet<String>>>,
    },
    /// Daemon to client: hold a response that answers a watched request.
    Responses {
        /// Correlation ids of the watched requests seen so far.
        watched: Arc<Mutex<HashSet<String>>>,
        /// How long a watched response is held before delivery.
        delay: Duration,
        /// Where the actual hold time of every held response is recorded.
        holds: Arc<Mutex<Vec<Duration>>>,
    },
}

impl Direction {
    /// Returns how long the line must be held before it is forwarded.
    fn hold(&self, line: &[u8]) -> Option<Duration> {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return None;
        };
        match self {
            Self::Requests {
                watched_method,
                watched,
            } => {
                if value.get("method").and_then(serde_json::Value::as_str) == Some(*watched_method)
                {
                    if let Some(id) = value.get("id").and_then(serde_json::Value::as_str) {
                        watched
                            .lock()
                            .expect("watched ids lock")
                            .insert(id.to_owned());
                    }
                }
                None
            }
            Self::Responses { watched, delay, .. } => {
                let id = value.get("id").and_then(serde_json::Value::as_str)?;
                let is_watched = watched.lock().expect("watched ids lock").remove(id);
                is_watched.then_some(*delay)
            }
        }
    }
}

/// Copies `read` to `write` line by line until either side closes.
///
/// Each complete newline-terminated line is inspected by `direction` and held
/// for the returned duration before it is forwarded; a held line delays the
/// lines behind it, so the response order of the connection is preserved.
/// Trailing bytes without a newline are forwarded unchanged.
fn pump(mut read: UnixStream, mut write: UnixStream, direction: &Direction) {
    let mut buffer = vec![0u8; READ_CHUNK_BYTES];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let read_bytes = match read.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        pending.extend_from_slice(&buffer[..read_bytes]);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            if let Some(hold) = direction.hold(&line) {
                let started = Instant::now();
                // timing-allowed: #1205 the delayed response is the behavior under test
                std::thread::sleep(hold);
                if let Direction::Responses { holds, .. } = &direction {
                    holds
                        .lock()
                        .expect("hold times lock")
                        .push(started.elapsed());
                }
            }
            if write.write_all(&line).is_err() {
                let _ = write.shutdown(Shutdown::Both);
                return;
            }
        }
        let _ = write.flush();
    }
    if !pending.is_empty() {
        let _ = write.write_all(&pending);
    }
    let _ = write.flush();
    // Close both ends of the forwarded stream so the peer sees the shutdown
    // instead of waiting for this process to exit.
    let _ = write.shutdown(Shutdown::Both);
}
