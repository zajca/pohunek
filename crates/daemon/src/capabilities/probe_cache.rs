//! Cache of the version probes behind `host.inspect`.
//!
//! A probe is identified by the program it runs, the identity of that file on
//! disk, the arguments, the `PATH` the probe sees and the runtime's
//! `[launch_env]` pairs. Only the raw stdout (or
//! the fact that the probe failed) is stored, so the supported range and the
//! output grammar of a runtime are applied on every read and never go stale.
//! An inventory hands over all of its requests at once: the distinct ones that
//! have no fresh entry run in parallel, once each, and concurrent inventories
//! wait for each other instead of probing the same program twice.

// Rust guideline compliant 2026-10-26

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use pohunek_service_config::InventoryProbe;

/// One version probe to run: the resolved program, its probe arguments, the
/// `PATH` the probed program sees and the runtime's `[launch_env]` pairs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProbeRequest {
    program: PathBuf,
    args: Vec<String>,
    launch_path: Option<OsString>,
    launch_env: Vec<(String, String)>,
}

impl ProbeRequest {
    pub(crate) fn new(
        program: &Path,
        args: &[String],
        launch_path: Option<&OsStr>,
        launch_env: &[(String, String)],
    ) -> Self {
        Self {
            program: program.to_path_buf(),
            args: args.to_vec(),
            launch_path: launch_path.map(OsStr::to_os_string),
            launch_env: launch_env.to_vec(),
        }
    }

    pub(super) fn program(&self) -> &Path {
        &self.program
    }

    pub(super) fn args(&self) -> &[String] {
        &self.args
    }

    pub(super) fn launch_path(&self) -> Option<&OsStr> {
        self.launch_path.as_deref()
    }

    pub(super) fn launch_env(&self) -> &[(String, String)] {
        &self.launch_env
    }
}

/// What identifies the probed file on disk.
///
/// Any change to the file's inode, size or timestamps makes the cached result
/// unreachable, so a program replaced in place, relinked or touched is probed
/// again at once.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileIdentity {
    #[cfg(unix)]
    fn of(program: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(program).ok()?;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    /// No identity: every request of a platform without file ids is probed.
    #[cfg(not(unix))]
    fn of(_program: &Path) -> Option<Self> {
        None
    }
}

/// A cache key: the request plus the identity of its program.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProbeKey {
    request: ProbeRequest,
    identity: FileIdentity,
}

/// What is known about the probe of one request of an inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    /// No fresh result is stored.
    Missing,
    /// The probe's output, or `None` for a probe that failed.
    Probed(Option<String>),
}

/// A stored probe result and when its probe started.
#[derive(Debug)]
struct Entry {
    /// The probe's stdout, or `None` when the probe failed.
    output: Option<String>,
    /// The instant the probe started; the lifetime counts from here.
    started: Instant,
}

/// Version-probe results shared by every `host.inspect` of one daemon.
#[derive(Debug)]
pub(crate) struct ProbeCache {
    settings: InventoryProbe,
    entries: Mutex<HashMap<ProbeKey, Entry>>,
    /// Held while misses are probed, so concurrent inventories probe each
    /// program once: the later one finds the entries the earlier one stored.
    flight: Mutex<()>,
}

impl ProbeCache {
    pub(crate) fn new(settings: InventoryProbe) -> Self {
        Self {
            settings,
            entries: Mutex::new(HashMap::new()),
            flight: Mutex::new(()),
        }
    }

    /// The probe output of each request, in order; `None` for a probe that
    /// failed.
    ///
    /// Requests that resolve to the same key run once. `now` is the instant
    /// the inventory started, against which entries are aged.
    pub(crate) fn fetch(&self, requests: &[&ProbeRequest], now: Instant) -> Vec<Option<String>> {
        let keys: Vec<Option<ProbeKey>> =
            requests.iter().map(|request| Self::key(request)).collect();
        let mut slots = self.lookup(&keys, now);
        if slots.iter().all(|slot| matches!(slot, Slot::Probed(_))) {
            return flatten(slots);
        }

        let _flight = match self.flight.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                tracing::debug!("inventory probe joined in-flight");
                self.flight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            }
        };
        slots = self.lookup(&keys, now);
        let to_run = distinct_missing(&keys, &slots);
        if !to_run.is_empty() {
            let probed = self.run_parallel(requests, &to_run);
            let mut fresh: HashMap<ProbeKey, Option<String>> = HashMap::new();
            for (&index, output) in to_run.iter().zip(&probed) {
                slots[index] = Slot::Probed(output.clone());
                if let Some(key) = &keys[index] {
                    fresh.insert(key.clone(), output.clone());
                }
            }
            // A request that shares its key with a probed one takes its output.
            for (slot, key) in slots.iter_mut().zip(&keys) {
                if *slot == Slot::Missing {
                    *slot = key
                        .as_ref()
                        .and_then(|key| fresh.get(key).cloned())
                        .map_or(Slot::Missing, Slot::Probed);
                }
            }
            self.store(fresh, now);
        }
        flatten(slots)
    }

    fn key(request: &ProbeRequest) -> Option<ProbeKey> {
        Some(ProbeKey {
            request: request.clone(),
            identity: FileIdentity::of(request.program())?,
        })
    }

    /// The fresh stored result of each key, [`Slot::Missing`] for a key
    /// without one.
    fn lookup(&self, keys: &[Option<ProbeKey>], now: Instant) -> Vec<Slot> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        keys.iter()
            .map(|key| {
                key.as_ref()
                    .and_then(|key| entries.get(key))
                    .filter(|entry| self.is_fresh(entry, now))
                    .map_or(Slot::Missing, |entry| Slot::Probed(entry.output.clone()))
            })
            .collect()
    }

    fn is_fresh(&self, entry: &Entry, now: Instant) -> bool {
        let ttl = if entry.output.is_some() {
            self.settings.cache_ttl
        } else {
            self.settings.failure_ttl
        };
        now.saturating_duration_since(entry.started) < ttl
    }

    fn store(&self, results: HashMap<ProbeKey, Option<String>>, now: Instant) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (key, output) in results {
            entries.insert(
                key,
                Entry {
                    output,
                    started: now,
                },
            );
        }
        let limit = usize::try_from(self.settings.cache_max_entries).unwrap_or(usize::MAX);
        if entries.len() > limit {
            entries.retain(|_, entry| self.is_fresh(entry, now));
        }
        if entries.len() > limit {
            let mut ages: Vec<(Instant, ProbeKey)> = entries
                .iter()
                .map(|(key, entry)| (entry.started, key.clone()))
                .collect();
            ages.sort_by_key(|(started, _)| *started);
            let excess = entries.len() - limit;
            for (_, key) in ages.into_iter().take(excess) {
                entries.remove(&key);
            }
        }
    }

    /// Runs the requests at `missing` on up to `parallelism` threads and
    /// returns their outputs in the same order.
    fn run_parallel(&self, requests: &[&ProbeRequest], missing: &[usize]) -> Vec<Option<String>> {
        let workers = usize::try_from(self.settings.parallelism)
            .unwrap_or(usize::MAX)
            .min(missing.len())
            .max(1);
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<Option<String>>> = Mutex::new(vec![None; missing.len()]);
        let work = || loop {
            let slot = next.fetch_add(1, Ordering::Relaxed);
            let Some(index) = missing.get(slot) else {
                return;
            };
            let output = super::run_request(requests[*index]);
            results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[slot] = output;
        };
        std::thread::scope(|scope| {
            // The calling thread works too, so a thread that cannot be spawned
            // only lowers the parallelism.
            for _ in 1..workers {
                if std::thread::Builder::new()
                    .spawn_scoped(scope, work)
                    .is_err()
                {
                    break;
                }
            }
            work();
        });
        results
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The index of the first request of every distinct key (and of every request
/// without a key) that has no stored result.
fn distinct_missing(keys: &[Option<ProbeKey>], slots: &[Slot]) -> Vec<usize> {
    let mut seen: HashSet<&ProbeKey> = HashSet::new();
    let mut missing = Vec::new();
    for (index, slot) in slots.iter().enumerate() {
        if matches!(slot, Slot::Probed(_)) {
            continue;
        }
        let first = keys[index].as_ref().is_none_or(|key| seen.insert(key));
        if first {
            missing.push(index);
        }
    }
    missing
}

fn flatten(slots: Vec<Slot>) -> Vec<Option<String>> {
    slots
        .into_iter()
        .map(|slot| match slot {
            Slot::Probed(output) => output,
            Slot::Missing => None,
        })
        .collect()
}
