//! Opt-in observation of agent processes outside pohunek-owned PTYs.
//!
//! The observer combines a same-user process sweep with lightweight transcript
//! indexing. It never attaches to, writes to, resizes, stops, or otherwise
//! controls external processes; it only publishes read-only `SessionInfo`
//! snapshots for UI and CLI visibility.

// Rust guideline compliant 2026-10-05

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::future::Future;
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{ProtocolError, RuntimeRef, SessionId, SessionInfo};
use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use watch::{RootProvider, RootScan, TranscriptSink, WatchBackend, WatcherHandle};

use crate::integration::{ConfigHomes, LaunchHome};
use crate::procwatch::{Pid, ProcessFact, ProcessIdentity};
use crate::session::SessionRegistry;

#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(target_os = "linux")]
mod inotify;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unsupported;
mod watch;

#[cfg(target_os = "macos")]
use fsevents as platform;
#[cfg(target_os = "linux")]
use inotify as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use unsupported as platform;

#[cfg(target_os = "macos")]
use fsevents::FsEventsBackend as PlatformBackend;
#[cfg(target_os = "linux")]
use inotify::InotifyBackend as PlatformBackend;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use unsupported::UnsupportedBackend as PlatformBackend;

/// Prefix for synthetic session ids assigned to external processes.
pub(crate) const EXTERNAL_SESSION_ID_PREFIX: &str = "ext-";
/// External agents have no PTY, so their terminal geometry is explicitly zero.
pub(crate) const EXTERNAL_TERMINAL_COLS: u16 = 0;
/// External agents have no PTY, so their terminal geometry is explicitly zero.
pub(crate) const EXTERNAL_TERMINAL_ROWS: u16 = 0;

/// Maximum number of initial JSONL records read from a transcript.
///
/// Session identity and cwd are expected near the beginning; bounding this keeps
/// an opt-in home-directory watcher from reading large transcripts wholesale.
const TRANSCRIPT_SCAN_LINE_LIMIT: usize = 32;
/// Maximum bytes read while scanning initial JSONL transcript records.
///
/// The file reader is capped before line splitting, bounding memory and I/O
/// even if an initial transcript record has no newline.
const TRANSCRIPT_SCAN_BYTE_LIMIT: usize = 64 * 1024;
/// Claude transcript root below its config home.
pub(crate) const CLAUDE_TRANSCRIPT_SUBDIR: &str = "projects";
/// Codex transcript root below its config home.
const CODEX_TRANSCRIPT_SUBDIR: &str = "sessions";
/// JSONL transcript extension.
const JSONL_EXTENSION: &str = "jsonl";

/// Maximum number of directory entries one tree walk inspects.
///
/// Both the reconciliation scan and the backends' registration walks stop here,
/// because they run repeatedly (tree changes, overflow, restarts, periodic
/// reconcile) and a large or fast-growing provider tree must not turn each run
/// into unbounded I/O. A walk that stops early is reported as partial or
/// incomplete and the remainder is reached by live events.
const MAX_WALK_ENTRIES: usize = 100_000;
/// Maximum number of directories one tree walk visits or watches.
///
/// Bounds the walk's queue memory and the OS watch budget a backend consumes.
const MAX_WALK_DIRECTORIES: usize = 8192;
/// Maximum number of transcripts one reconciliation scan parses per root.
///
/// Each parse reads at most `TRANSCRIPT_SCAN_BYTE_LIMIT` bytes, so this bounds the
/// bytes a scan reads.
const MAX_SCANNED_TRANSCRIPTS: usize = 20_000;

/// Work bounds for scanning one root.
#[derive(Debug, Clone, Copy)]
struct ScanLimits {
    entries: usize,
    directories: usize,
    transcripts: usize,
}

/// Bounds applied to every production scan.
const SCAN_LIMITS: ScanLimits = ScanLimits {
    entries: MAX_WALK_ENTRIES,
    directories: MAX_WALK_DIRECTORIES,
    transcripts: MAX_SCANNED_TRANSCRIPTS,
};

/// In-memory external session store and observer signals.
#[derive(Debug, Clone)]
pub(crate) struct ExternalSessions {
    inner: Arc<ExternalSessionsInner>,
}

#[derive(Debug)]
struct ExternalSessionsInner {
    entries: AsyncMutex<HashMap<Pid, ExternalEntry>>,
    rescan: Notify,
    shutdown: CancellationToken,
}

#[derive(Debug, Clone)]
struct ExternalEntry {
    identity: ProcessIdentity,
    info: SessionInfo,
}

/// Atomic result of inserting or refreshing an external session.
#[derive(Debug)]
pub(crate) struct ExternalUpsert {
    /// Exact process generation that needs a new exit watch.
    pub(crate) watch_identity: Option<ProcessIdentity>,
}

/// Change produced by inserting or refreshing an external session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalSessionChange {
    /// A new external session was observed.
    Created(SessionInfo),
    /// A known external session changed metadata.
    Updated(SessionInfo),
}

/// Transcript root watched for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptRoot {
    agent_base: RuntimeRef,
    path: PathBuf,
    /// Whether the directory derives from a host profile's environment. Such a
    /// path never reaches a log line or a remote response.
    private: bool,
}

/// What stands for a private root's path in log lines.
const PRIVATE_PATH_LABEL: &str = "<profile config home>";

impl TranscriptRoot {
    /// The root's path as a log line may show it.
    fn log_path(&self) -> String {
        self.log_path_of(&self.path)
    }

    /// `path`, a location below this root, as a log line may show it.
    fn log_path_of(&self, path: &Path) -> String {
        if self.private {
            PRIVATE_PATH_LABEL.to_owned()
        } else {
            path.display().to_string()
        }
    }
}

/// Where the observer takes its transcript roots from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ObservedRoots {
    /// The config homes the host's launches give their agents: each runtime's
    /// own home and the home of every host profile, re-read on every pass.
    LaunchHomes,
    /// A fixed set of roots.
    #[cfg_attr(not(test), expect(dead_code, reason = "tests pin the roots"))]
    Fixed(Vec<TranscriptRoot>),
}

/// Runtime observer configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalObserverConfig {
    roots: ObservedRoots,
    sweep_interval: Duration,
}

/// Parsed transcript metadata used to enrich an external process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptCandidate {
    /// Agent kind inferred from the transcript tree.
    pub(crate) agent_base: RuntimeRef,
    /// Native provider session id, when present.
    pub(crate) native_session_id: Option<String>,
    /// Native transcript path.
    pub(crate) native_session_path: String,
    /// The root that owned the transcript when it was indexed. Whether the
    /// path may be published, and which runtime the candidate belongs to, are
    /// decided by this provenance and never re-derived from the roots of a
    /// later pass.
    owner: Arc<TranscriptRoot>,
    /// Working directory reported by the provider transcript.
    pub(crate) cwd: Option<PathBuf>,
    updated_at: SystemTime,
}

impl TranscriptCandidate {
    /// The transcript path a published session may carry: none for a transcript
    /// below a profile-derived root, whose directory is private.
    pub(crate) fn publishable_path(&self) -> Option<&str> {
        (!self.owner.private).then_some(self.native_session_path.as_str())
    }

    /// Whether the transcript was indexed below a profile-derived root.
    #[cfg(test)]
    pub(crate) fn is_private(&self) -> bool {
        self.owner.private
    }
}

/// Shared transcript candidate index.
#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptIndex {
    inner: Arc<Mutex<HashMap<PathBuf, TranscriptCandidate>>>,
    /// Signature and owner of every transcript parsed, with or without a
    /// resulting candidate, so an unchanged file under an unchanged owner is
    /// not parsed again by a scan.
    parsed: Arc<Mutex<HashMap<PathBuf, ParsedEntry>>>,
    /// The roots of the last scan, published before the scan prunes: a
    /// candidate is only offered while this snapshot assigns its path to the
    /// root that indexed it.
    roots: Arc<Mutex<Arc<RootSnapshot>>>,
    #[cfg(test)]
    parses: Arc<std::sync::atomic::AtomicUsize>,
}

/// One generation of the configured roots and their canonical directories.
#[derive(Debug, Default)]
struct RootSnapshot {
    roots: Vec<TranscriptRoot>,
    canonical: Vec<PathBuf>,
}

impl RootSnapshot {
    fn new(roots: Vec<TranscriptRoot>) -> Self {
        let canonical = canonical_roots(&roots);
        Self { roots, canonical }
    }

    /// Whether this generation assigns `path` to `owner`, the root that
    /// indexed it: same root, runtime and privacy.
    fn assigns(&self, path: &Path, owner: &TranscriptRoot) -> bool {
        owning_root(&self.roots, &self.canonical, path) == Some(owner)
    }
}

/// What a scan remembers of a parsed transcript.
#[derive(Debug, Clone)]
struct ParsedEntry {
    signature: FileSignature,
    /// The root the transcript was parsed for.
    owner: Arc<TranscriptRoot>,
}

/// Identity and state a transcript had when it was last parsed: size,
/// modification time, device and inode, and status-change time. An atomic
/// replacement that keeps size and mtime still changes the inode and the ctime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    len: u64,
    modified: Option<SystemTime>,
    dev: u64,
    ino: u64,
    changed: (i64, i64),
}

impl FileSignature {
    fn read(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

impl ExternalSessions {
    /// Creates an empty external session store.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(ExternalSessionsInner {
                entries: AsyncMutex::new(HashMap::new()),
                rescan: Notify::new(),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    /// Builds the observer config: transcript roots follow the host's config
    /// homes, swept every `sweep_interval`.
    #[must_use]
    pub(crate) fn observer_config(sweep_interval: Duration) -> ExternalObserverConfig {
        ExternalObserverConfig {
            roots: ObservedRoots::LaunchHomes,
            sweep_interval,
        }
    }

    /// Spawns the external observer task.
    pub(crate) fn spawn_observer(&self, registry: SessionRegistry, config: ExternalObserverConfig) {
        let sessions = self.clone();
        tokio::spawn(async move {
            run_observer(registry, sessions, config).await;
        });
    }

    /// Stops the observer loop and the transcript watcher.
    pub(crate) fn shutdown(&self) {
        self.inner.shutdown.cancel();
    }

    /// Returns a cancellation token fired when the observer shuts down.
    pub(crate) fn shutdown_token(&self) -> CancellationToken {
        self.inner.shutdown.clone()
    }

    /// Wakes the observer for an immediate sweep.
    pub(crate) fn notify_rescan(&self) {
        self.inner.rescan.notify_one();
    }

    /// Lists all current external session snapshots.
    pub(crate) async fn list(&self) -> Vec<SessionInfo> {
        let mut sessions = self
            .inner
            .entries
            .lock()
            .await
            .values()
            .map(|entry| entry.info.clone())
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        sessions
    }

    /// Finds one external session by id.
    pub(crate) async fn inspect(&self, id: &SessionId) -> Option<SessionInfo> {
        let pid = external_pid(id)?;
        self.inner
            .entries
            .lock()
            .await
            .get(&pid)
            .map(|entry| entry.info.clone())
    }

    /// Whether `id` currently belongs to an observed external process.
    pub(crate) async fn contains_id(&self, id: &SessionId) -> bool {
        self.inspect(id).await.is_some()
    }

    /// Inserts or refreshes an external session snapshot while it is current.
    ///
    /// `publish` runs synchronously while the entry lock is held so a competing
    /// exit or sweep cannot publish a later lifecycle event first. It must not
    /// block or re-enter this store.
    pub(crate) async fn upsert_if_current<E>(
        &self,
        identity: ProcessIdentity,
        mut info: SessionInfo,
        validate: impl FnOnce() -> Result<bool, E>,
        publish: impl FnOnce(&ExternalSessionChange),
    ) -> Result<Option<ExternalUpsert>, E> {
        let mut entries = self.inner.entries.lock().await;
        if !validate()? {
            return Ok(None);
        }
        let Some(existing) = entries.get(&info.pid) else {
            entries.insert(
                info.pid,
                ExternalEntry {
                    identity,
                    info: info.clone(),
                },
            );
            publish(&ExternalSessionChange::Created(info));
            return Ok(Some(ExternalUpsert {
                watch_identity: Some(identity),
            }));
        };

        if existing.identity == identity {
            info.created_at.clone_from(&existing.info.created_at);
            if external_info_matches(&existing.info, &info) {
                return Ok(Some(ExternalUpsert {
                    watch_identity: None,
                }));
            }
        }
        let watch_identity = (existing.identity != identity).then_some(identity);
        entries.insert(
            info.pid,
            ExternalEntry {
                identity,
                info: info.clone(),
            },
        );
        publish(&ExternalSessionChange::Updated(info));
        Ok(Some(ExternalUpsert { watch_identity }))
    }

    /// Removes and publishes entries whose pids were absent from a successful sweep.
    ///
    /// `publish` has the same nonblocking, non-reentrant contract as the upsert
    /// publisher and runs before another mutation can acquire the entry lock.
    pub(crate) async fn remove_unobserved(
        &self,
        observed: &HashSet<Pid>,
        mut publish: impl FnMut(&SessionInfo),
    ) {
        let mut entries = self.inner.entries.lock().await;
        let stale = entries
            .keys()
            .copied()
            .filter(|pid| !observed.contains(pid))
            .collect::<Vec<_>>();
        for pid in stale {
            if let Some(entry) = entries.remove(&pid) {
                publish(&entry.info);
            }
        }
    }

    /// Removes and publishes one entry only if its exact exit watch still owns it.
    ///
    /// `publish` has the same nonblocking, non-reentrant contract as the upsert
    /// publisher and runs before another mutation can acquire the entry lock.
    pub(crate) async fn remove_identity(
        &self,
        identity: ProcessIdentity,
        publish: impl FnOnce(&SessionInfo),
    ) -> bool {
        let mut entries = self.inner.entries.lock().await;
        if entries.get(&identity.pid).map(|entry| entry.identity) != Some(identity) {
            return false;
        }
        let entry = entries
            .remove(&identity.pid)
            .expect("validated external identity must remain present while locked");
        publish(&entry.info);
        true
    }
}

impl Default for ExternalSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptIndex {
    /// Reconciles the index with every configured root and returns one result
    /// per root, in order.
    ///
    /// Transcripts that no longer exist are dropped from the index, and so is
    /// every transcript below a root that is no longer configured; transcripts
    /// that cannot be read or were not reached within `SCAN_LIMITS` are kept. The
    /// scan runs on the blocking pool and stops early once `cancel` fires.
    pub(crate) async fn scan_roots(
        &self,
        roots: Vec<TranscriptRoot>,
        cancel: CancellationToken,
    ) -> Vec<RootScan> {
        let index = self.clone();
        let count = roots.len();
        match tokio::task::spawn_blocking(move || {
            *index.roots.lock().unwrap_or_else(MutexError::into_inner) =
                Arc::new(RootSnapshot::new(roots.clone()));
            let scans = roots
                .iter()
                .map(|root| index.scan_root(root, &roots, SCAN_LIMITS, &cancel))
                .collect::<Vec<_>>();
            index.retain_owned(&roots);
            scans
        })
        .await
        {
            Ok(scans) => scans,
            Err(err) => {
                warn!(error = %err, "external transcript scan task panicked");
                vec![RootScan::Incomplete; count]
            }
        }
    }

    /// Drops every entry whose provenance is not the root that owns its path
    /// now, so a root that left the configuration takes its transcripts with it
    /// and a transcript whose owner changed is parsed again by the next scan.
    fn retain_owned(&self, roots: &[TranscriptRoot]) {
        let canonical = canonical_roots(roots);
        let owned_by = |path: &Path, owner: &TranscriptRoot| {
            owning_root(roots, &canonical, path) == Some(owner)
        };
        self.inner
            .lock()
            .unwrap_or_else(MutexError::into_inner)
            .retain(|path, candidate| owned_by(path, &candidate.owner));
        self.parsed
            .lock()
            .unwrap_or_else(MutexError::into_inner)
            .retain(|path, entry| owned_by(path, &entry.owner));
    }

    /// Parses one transcript path and updates the candidate index.
    ///
    /// A path that is gone or is no longer a regular file (removed, replaced by
    /// a directory, or below a parent replaced by a file) drops its candidate; the
    /// existence check is repeated under the lock so a transcript recreated
    /// meanwhile is kept.
    pub(crate) fn upsert_path(&self, owner: &TranscriptRoot, path: &Path) -> io::Result<bool> {
        let gone = transcript_is_gone(path);
        let signature = if gone {
            None
        } else {
            FileSignature::read(path)
        };
        let candidate = if gone {
            None
        } else {
            #[cfg(test)]
            self.parses
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match parse_transcript(owner.agent_base.clone(), path) {
                Ok(candidate) => candidate.map(|candidate| TranscriptCandidate {
                    owner: Arc::new(owner.clone()),
                    ..candidate
                }),
                Err(_err) if transcript_is_gone(path) => None,
                Err(err) => return Err(err),
            }
        };
        {
            let mut parsed = self.parsed.lock().unwrap_or_else(MutexError::into_inner);
            match signature {
                Some(signature) => {
                    parsed.insert(
                        path.to_path_buf(),
                        ParsedEntry {
                            signature,
                            owner: Arc::new(owner.clone()),
                        },
                    );
                }
                None => {
                    parsed.remove(path);
                }
            }
        }
        let mut inner = self.inner.lock().unwrap_or_else(MutexError::into_inner);
        if let Some(candidate) = candidate {
            let changed = inner.get(path) != Some(&candidate);
            inner.insert(path.to_path_buf(), candidate);
            return Ok(changed);
        }
        if gone && !transcript_is_gone(path) {
            return Ok(false);
        }
        Ok(inner.remove(path).is_some())
    }

    /// Finds the best transcript candidate for `fact` and `cwd`.
    pub(crate) fn best_match(
        &self,
        agent_base: &RuntimeRef,
        cwd: &Path,
        fact: &ProcessFact,
    ) -> Option<TranscriptCandidate> {
        // One generation of the roots serves the selection and the
        // classification. A candidate counts only while that generation still
        // assigns its path to the root that indexed it, so a scan that has
        // published new roots but not yet pruned or reparsed never offers an
        // old attribution or a path that is private now.
        let roots = Arc::clone(&self.roots.lock().unwrap_or_else(MutexError::into_inner));
        self.inner
            .lock()
            .unwrap_or_else(MutexError::into_inner)
            .iter()
            .filter(|(_path, candidate)| &candidate.agent_base == agent_base)
            .filter(|(_path, candidate)| transcript_matches_process(candidate, cwd, fact))
            .filter(|(path, candidate)| roots.assigns(path, &candidate.owner))
            .max_by_key(|(_path, candidate)| candidate.updated_at)
            .map(|(_path, candidate)| candidate.clone())
    }

    /// Scans one root and, when the pass covered it completely, drops the entries
    /// it owns that the pass did not see.
    ///
    /// A pass that ends early (an unreadable directory, an exhausted bound,
    /// cancellation) prunes nothing: what it did not inspect stays as it is, and
    /// only an explicit hint for a gone path removes a single candidate. A root
    /// that does not exist prunes everything it owned.
    fn scan_root(
        &self,
        root: &TranscriptRoot,
        all_roots: &[TranscriptRoot],
        limits: ScanLimits,
        cancel: &CancellationToken,
    ) -> RootScan {
        let canonical = canonical_roots(all_roots);
        let (scan, visited) = match fs::metadata(&root.path) {
            Ok(metadata) if metadata.is_dir() => {
                self.scan_existing_root(root, all_roots, &canonical, limits, cancel)
            }
            Ok(_) => (RootScan::Missing, HashSet::new()),
            Err(err) if is_absent_error(&err) => (RootScan::Missing, HashSet::new()),
            Err(err) => {
                debug!(
                    agent = ?root.agent_base,
                    error = %err,
                    "failed to inspect external transcript root"
                );
                (RootScan::Incomplete, HashSet::new())
            }
        };
        if scan != RootScan::Incomplete {
            let unseen = |path: &Path| {
                owning_root(all_roots, &canonical, path)
                    .is_some_and(|owner| owner.path == root.path)
                    && !visited.contains(path)
            };
            self.inner
                .lock()
                .unwrap_or_else(MutexError::into_inner)
                .retain(|path, _candidate| !unseen(path));
            self.parsed
                .lock()
                .unwrap_or_else(MutexError::into_inner)
                .retain(|path, _signature| !unseen(path));
        }
        scan
    }

    /// Walks one existing root within `limits`, indexing new and changed
    /// transcripts, and returns the transcripts it saw.
    ///
    /// Transcripts whose size and modification time are unchanged since they
    /// were parsed cost only an entry visit, so repeated passes spend the parse
    /// budget on new and changed files. A tree beyond the limits ends the pass as
    /// incomplete; the pass restarts from the root every time.
    fn scan_existing_root(
        &self,
        root: &TranscriptRoot,
        all_roots: &[TranscriptRoot],
        canonical: &[PathBuf],
        limits: ScanLimits,
        cancel: &CancellationToken,
    ) -> (RootScan, HashSet<PathBuf>) {
        let root_canonical = all_roots
            .iter()
            .position(|other| other.path == root.path)
            .and_then(|index| canonical.get(index))
            .cloned()
            .unwrap_or_else(|| root.path.clone());
        let mut incomplete = false;
        let mut visited = HashSet::new();
        let mut entries_left = limits.entries;
        let mut directories_left = limits.directories;
        let mut transcripts_left = limits.transcripts;
        let mut queue = VecDeque::from([root.path.clone()]);
        'walk: while let Some(dir) = queue.pop_front() {
            if cancel.is_cancelled() {
                incomplete = true;
                break;
            }
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(err) if is_absent_error(&err) => continue,
                Err(err) => {
                    debug!(
                        agent = ?root.agent_base,
                        error = %err,
                        "failed to list a directory below an external transcript root"
                    );
                    incomplete = true;
                    continue;
                }
            };
            for entry in entries {
                if entries_left == 0 {
                    incomplete = true;
                    break 'walk;
                }
                entries_left -= 1;
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) if is_absent_error(&err) => continue,
                    Err(_err) => {
                        incomplete = true;
                        continue;
                    }
                };
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(err) if is_absent_error(&err) => continue,
                    Err(_err) => {
                        incomplete = true;
                        continue;
                    }
                };
                if file_type.is_dir() {
                    // A nested provider root, however it is spelled, is scanned as
                    // its own root so its transcripts carry its own agent kind.
                    let resolved =
                        root_canonical.join(path.strip_prefix(&root.path).unwrap_or(&path));
                    if canonical
                        .iter()
                        .zip(all_roots)
                        .any(|(other_canonical, other)| {
                            *other_canonical == resolved && other.path != root.path
                        })
                    {
                        continue;
                    }
                    if directories_left == 0 {
                        incomplete = true;
                        break 'walk;
                    }
                    directories_left -= 1;
                    queue.push_back(path);
                } else if is_jsonl_path(&path) {
                    if !self.is_unchanged(&path, root) {
                        if transcripts_left == 0 {
                            incomplete = true;
                            break 'walk;
                        }
                        transcripts_left -= 1;
                        if let Err(err) = self.upsert_path(root, &path) {
                            debug!(
                                agent = ?root.agent_base,
                                error = %err,
                                "failed to parse external transcript candidate"
                            );
                            incomplete = true;
                        }
                    }
                    visited.insert(path);
                }
            }
        }
        let scan = if incomplete {
            RootScan::Incomplete
        } else {
            RootScan::Complete
        };
        (scan, visited)
    }

    /// Whether `path` has the size and modification time it had when it was
    /// parsed for `owner`.
    fn is_unchanged(&self, path: &Path, owner: &TranscriptRoot) -> bool {
        let Some(signature) = FileSignature::read(path) else {
            return false;
        };
        self.parsed
            .lock()
            .unwrap_or_else(MutexError::into_inner)
            .get(path)
            .is_some_and(|entry| entry.signature == signature && *entry.owner == *owner)
    }
}

/// Whether an indexed transcript no longer exists as a regular file.
///
/// A parent replaced by a file (`ENOTDIR`) or the transcript replaced by a
/// directory or other non-regular file also count as gone; any other error
/// keeps the candidate.
fn transcript_is_gone(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => !metadata.is_file(),
        Err(err) => is_absent_error(&err),
    }
}

/// The canonical directory of every root, in order; a root that cannot be
/// resolved keeps its configured path.
fn canonical_roots(roots: &[TranscriptRoot]) -> Vec<PathBuf> {
    roots
        .iter()
        .map(|root| fs::canonicalize(&root.path).unwrap_or_else(|_| root.path.clone()))
        .collect()
}

/// Finds the root that owns `path` by resolved identity: the path is mapped
/// through the configured root that spells it to its canonical location, and the
/// root with the longest canonical prefix owns it. A root reached through a
/// symlink into another root's tree therefore owns its own transcripts, and every
/// canonical file has exactly one owner. `canonical` holds `canonical_roots`.
fn owning_root<'a>(
    roots: &'a [TranscriptRoot],
    canonical: &[PathBuf],
    path: &Path,
) -> Option<&'a TranscriptRoot> {
    let (spelled_by, _) = roots
        .iter()
        .enumerate()
        .filter(|(_, root)| path.starts_with(&root.path))
        .max_by_key(|(_, root)| root.path.components().count())?;
    let resolved = canonical[spelled_by].join(path.strip_prefix(&roots[spelled_by].path).ok()?);
    canonical
        .iter()
        .zip(roots)
        .filter(|(root_canonical, _)| resolved.starts_with(root_canonical))
        .max_by_key(|(root_canonical, _)| root_canonical.components().count())
        .map(|(_, root)| root)
}

/// Whether a filesystem error proves the path does not exist.
fn is_absent_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Platform backend that may not be open yet, so a failed open is retried by the
/// runner's restart loop like any later backend failure.
struct ReopenableBackend {
    inner: Option<PlatformBackend>,
}

fn backend_not_open() -> io::Error {
    io::Error::other("transcript watcher backend is not open")
}

impl WatchBackend for ReopenableBackend {
    fn register_root(
        &mut self,
        root: &Path,
        cancel: &CancellationToken,
    ) -> watch::RootRegistration {
        match &mut self.inner {
            Some(backend) => backend.register_root(root, cancel),
            None => watch::RootRegistration::Failed(backend_not_open()),
        }
    }

    fn watch_new_directory(
        &mut self,
        dir: &Path,
        out: &mut Vec<watch::WatchEvent>,
        cancel: &CancellationToken,
    ) {
        if let Some(backend) = &mut self.inner {
            backend.watch_new_directory(dir, out, cancel);
        }
    }

    fn restart(&mut self) -> io::Result<()> {
        self.inner = Some(PlatformBackend::open()?);
        Ok(())
    }

    fn unregister_all(&mut self) {
        if let Some(backend) = &mut self.inner {
            backend.unregister_all();
        }
    }

    async fn next_events(&mut self, out: &mut Vec<watch::WatchEvent>) -> io::Result<()> {
        match &mut self.inner {
            Some(backend) => backend.next_events(out).await,
            None => std::future::pending().await,
        }
    }
}

/// Production sink: parses into the shared index and wakes the observer.
struct IndexSink {
    index: TranscriptIndex,
    sessions: ExternalSessions,
}

impl TranscriptSink for IndexSink {
    fn upsert(
        &self,
        owner: TranscriptRoot,
        path: PathBuf,
    ) -> impl Future<Output = io::Result<bool>> + Send {
        let index = self.index.clone();
        async move {
            tokio::task::spawn_blocking(move || index.upsert_path(&owner, &path))
                .await
                .map_err(io::Error::other)?
        }
    }

    fn reconcile(&self, roots: Vec<TranscriptRoot>) -> impl Future<Output = Vec<RootScan>> + Send {
        let index = self.index.clone();
        let shutdown = self.sessions.shutdown_token();
        async move { index.scan_roots(roots, shutdown).await }
    }

    fn index_changed(&self) {
        self.sessions.notify_rescan();
    }
}

async fn run_observer(
    registry: SessionRegistry,
    sessions: ExternalSessions,
    config: ExternalObserverConfig,
) {
    let (index, _watcher) = match &config.roots {
        ObservedRoots::LaunchHomes => {
            start_transcript_index_with(LaunchHomeRoots::new(registry.clone()), &sessions).await
        }
        ObservedRoots::Fixed(roots) => start_transcript_index(roots, &sessions).await,
    };

    let mut tick = tokio::time::interval(config.sweep_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = sessions.inner.shutdown.cancelled() => break,
            _ = tick.tick() => registry.rescan_external_agents(&index).await,
            () = sessions.inner.rescan.notified() => registry.rescan_external_agents(&index).await,
        }
    }
}

/// [`start_transcript_index_with`] over a fixed set of roots.
async fn start_transcript_index(
    roots: &[TranscriptRoot],
    sessions: &ExternalSessions,
) -> (TranscriptIndex, WatcherHandle) {
    start_transcript_index_with(roots.to_vec(), sessions).await
}

/// Starts transcript watching, reports its health, and indexes the transcripts
/// that already exist.
///
/// `roots` is asked for the roots to watch at the start of every pass.
async fn start_transcript_index_with(
    roots: impl RootProvider,
    sessions: &ExternalSessions,
) -> (TranscriptIndex, WatcherHandle) {
    let index = TranscriptIndex::default();
    let sink = Arc::new(IndexSink {
        index: index.clone(),
        sessions: sessions.clone(),
    });
    let (backend, open_error) = match PlatformBackend::open() {
        Ok(backend) => (
            ReopenableBackend {
                inner: Some(backend),
            },
            None,
        ),
        Err(error) => (ReopenableBackend { inner: None }, Some(error)),
    };
    let watcher = watch::start(backend, roots, sink, sessions.shutdown_token(), open_error).await;
    (index, watcher)
}

/// The transcript tree below a config home, for a runtime whose agent writes
/// transcripts the observer reads.
fn transcript_subdir(runtime: &RuntimeRef) -> Option<&'static str> {
    if *runtime == RuntimeRef::claude() {
        Some(CLAUDE_TRANSCRIPT_SUBDIR)
    } else if *runtime == RuntimeRef::codex() {
        Some(CODEX_TRANSCRIPT_SUBDIR)
    } else {
        None
    }
}

/// Why a config home contributes no transcript root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SkippedHome {
    /// The profile that names the home; `None` for a runtime's own home.
    profile: Option<String>,
    /// The error code of the home that does not resolve, or `None` when it
    /// resolves to a directory without a transcript tree.
    code: Option<String>,
}

/// The transcript roots of `homes`, and the homes that contribute none.
///
/// A runtime's own home is always a root: a transcript tree created later is
/// reached by the pass that finds it. A profile's home is a root only while its
/// transcript tree exists, so a profile that was never used does not hold the
/// watcher in a missing-root state. Homes that share a directory yield one
/// root, kept for the first of them. The paths of a profile's home derive from
/// its environment and never reach a log line.
fn observed_roots(homes: Vec<LaunchHome>) -> (Vec<TranscriptRoot>, BTreeSet<SkippedHome>) {
    let mut roots = Vec::new();
    let mut skipped = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for launch in homes {
        let Some(subdir) = transcript_subdir(&launch.runtime) else {
            continue;
        };
        let home = match launch.home {
            Ok(home) => home,
            Err(error) => {
                skipped.insert(SkippedHome {
                    profile: launch.profile,
                    code: Some(error.code),
                });
                continue;
            }
        };
        let path = home.dir.join(subdir);
        let private = launch.profile.is_some();
        // Only a tree that is genuinely absent is skipped. A tree that cannot
        // be inspected (permissions, I/O, a symlink loop) stays a root, so its
        // candidates stay indexed and the scan and the watcher report it as
        // degraded instead of the profile silently dropping out.
        if private && transcript_tree_is_absent(&path) {
            skipped.insert(SkippedHome {
                profile: launch.profile,
                code: None,
            });
            continue;
        }
        // Two runtimes may share one directory; each keeps its own tree.
        if seen.insert((launch.runtime.as_wire().to_owned(), home.identity())) {
            roots.push(TranscriptRoot {
                agent_base: launch.runtime,
                path,
                private,
            });
        }
    }
    (roots, skipped)
}

/// Whether `path` is known not to be a directory: missing, or something else.
/// An error that says nothing about existence is not absence.
fn transcript_tree_is_absent(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => !metadata.is_dir(),
        Err(error) => is_absent_error(&error),
    }
}

/// Where the provider gets the config homes of the current pass.
type HomesSource = Arc<dyn Fn() -> Result<ConfigHomes, ProtocolError> + Send + Sync>;

/// Production roots: the config homes of the host's launches, resolved against
/// the host's profiles and launch environment on every pass.
struct LaunchHomeRoots {
    homes: HomesSource,
    /// What the previous pass settled on.
    state: Arc<Mutex<LaunchHomeState>>,
}

/// What [`LaunchHomeRoots`] remembers between passes.
#[derive(Debug, Default)]
struct LaunchHomeState {
    /// The roots of the last pass that resolved, kept while a pass cannot
    /// resolve so a transient failure never empties the watched set.
    roots: Vec<TranscriptRoot>,
    /// The homes that contributed no root in the previous pass, so a home that
    /// stays unusable is logged once rather than on every pass.
    reported: BTreeSet<SkippedHome>,
}

impl LaunchHomeRoots {
    fn new(registry: SessionRegistry) -> Self {
        Self::from_source(Arc::new(move || registry.integration_homes()))
    }

    fn from_source(homes: HomesSource) -> Self {
        Self {
            homes,
            state: Arc::default(),
        }
    }
}

impl RootProvider for LaunchHomeRoots {
    async fn roots(&self) -> Vec<TranscriptRoot> {
        let source = Arc::clone(&self.homes);
        let state = Arc::clone(&self.state);
        let resolved = tokio::task::spawn_blocking(move || {
            let mut state = state.lock().unwrap_or_else(MutexError::into_inner);
            let homes = match source() {
                Ok(homes) => homes,
                Err(error) => {
                    debug!(
                        name: "external.transcript_roots.environment_unavailable",
                        code = %error.code,
                        "keeping the previous transcript roots: the launch environment cannot be built"
                    );
                    return state.roots.clone();
                }
            };
            let (roots, skipped) = observed_roots(homes.launch_homes());
            for home in skipped.difference(&state.reported) {
                debug!(
                    name: "external.transcript_roots.skipped",
                    profile = home.profile.as_deref().unwrap_or("-"),
                    code = home.code.as_deref().unwrap_or("transcripts_absent"),
                    "config home contributes no transcript root"
                );
            }
            state.reported = skipped;
            state.roots.clone_from(&roots);
            roots
        })
        .await;
        match resolved {
            Ok(roots) => roots,
            Err(error) => {
                warn!(
                    name: "external.transcript_roots.panicked",
                    error = %error,
                    "resolving transcript roots panicked: {{error}}"
                );
                self.state
                    .lock()
                    .unwrap_or_else(MutexError::into_inner)
                    .roots
                    .clone()
            }
        }
    }
}

fn external_info_matches(existing: &SessionInfo, incoming: &SessionInfo) -> bool {
    let mut existing = existing.clone();
    existing.created_at.clone_from(&incoming.created_at);
    existing.updated_at.clone_from(&incoming.updated_at);
    existing == *incoming
}

fn external_pid(id: &SessionId) -> Option<Pid> {
    id.0.strip_prefix(EXTERNAL_SESSION_ID_PREFIX)
        .and_then(|raw| raw.parse::<Pid>().ok())
}

/// Builds the synthetic id for an external process.
#[must_use]
pub(crate) fn external_session_id(pid: Pid) -> SessionId {
    SessionId(format!("{EXTERNAL_SESSION_ID_PREFIX}{pid}"))
}

fn transcript_matches_process(
    candidate: &TranscriptCandidate,
    cwd: &Path,
    fact: &ProcessFact,
) -> bool {
    let cwd_matches = candidate.cwd.as_deref() == Some(cwd);
    let native_matches = candidate.native_session_id.as_ref().is_some_and(|native| {
        fact.cmdline
            .iter()
            .any(|arg| arg == native || arg.contains(native))
    });
    cwd_matches || native_matches
}

fn parse_transcript(
    agent_base: RuntimeRef,
    path: &Path,
) -> io::Result<Option<TranscriptCandidate>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if is_absent_error(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut native_session_id = None;
    let mut native_session_path = None;
    let mut cwd = None;
    let byte_limit =
        u64::try_from(TRANSCRIPT_SCAN_BYTE_LIMIT).expect("transcript byte limit fits in u64");
    for line in BufReader::new(file.take(byte_limit))
        .lines()
        .take(TRANSCRIPT_SCAN_LINE_LIMIT)
    {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if native_session_id.is_none() {
            native_session_id = first_string(
                &value,
                &[
                    "session_id",
                    "sessionId",
                    "conversation_id",
                    "conversationId",
                ],
            );
        }
        if native_session_path.is_none() {
            native_session_path = first_string(&value, &["transcript_path", "transcriptPath"]);
        }
        if cwd.is_none() {
            cwd = first_string(&value, &["cwd", "working_directory", "workingDirectory"])
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| normalize_path(&path));
        }
        if native_session_id.is_some() && native_session_path.is_some() && cwd.is_some() {
            break;
        }
    }

    if native_session_id.is_none() && native_session_path.is_none() && cwd.is_none() {
        return Ok(None);
    }
    let native_session_path =
        native_session_path.unwrap_or_else(|| path.to_string_lossy().into_owned());
    let updated_at = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    let owner = Arc::new(TranscriptRoot {
        agent_base: agent_base.clone(),
        path: PathBuf::new(),
        private: true,
    });
    Ok(Some(TranscriptCandidate {
        agent_base,
        native_session_id,
        native_session_path,
        // Unattributed until a root claims it; a candidate nobody claimed is
        // never published.
        owner,
        cwd,
        updated_at,
    }))
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_jsonl_path(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str) == Some(JSONL_EXTENSION)
}

type MutexError<T> = std::sync::PoisonError<T>;

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use protocol::RuntimeRef;

    use super::{parse_transcript, TRANSCRIPT_SCAN_BYTE_LIMIT};

    /// FIFO permissions used by the bounded-read test fixture.
    const TRANSCRIPT_FIFO_MODE: libc::mode_t = 0o600;
    /// Bytes written past the scan cap to prove the parser does not need a newline.
    const OVERSIZED_TRANSCRIPT_EXTRA_BYTES: usize = 1024;
    /// Maximum time a bounded transcript parser may need to return from a FIFO.
    const BOUNDED_TRANSCRIPT_TEST_TIMEOUT: Duration = Duration::from_secs(2);

    #[cfg(unix)]
    #[test]
    fn parse_transcript_returns_when_oversized_line_has_no_newline() {
        let dir = pohunek_test_support::tempdir().expect("transcript directory");
        let fifo = dir.path().join("transcript.jsonl");
        create_fifo(&fifo);

        let (result_tx, result_rx) = mpsc::channel();
        let parser_path = fifo.clone();
        let parser = thread::spawn(move || {
            let result = parse_transcript(RuntimeRef::claude(), &parser_path);
            let _ = result_tx.send(result);
        });

        let (release_tx, release_rx) = mpsc::channel();
        let writer_path = fifo.clone();
        let writer = thread::spawn(move || {
            let mut writer = OpenOptions::new()
                .write(true)
                .open(&writer_path)
                .expect("open fifo writer");
            let oversized_line =
                vec![b'x'; TRANSCRIPT_SCAN_BYTE_LIMIT + OVERSIZED_TRANSCRIPT_EXTRA_BYTES];
            match writer.write_all(&oversized_line) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {}
                Err(err) => panic!("write oversized transcript line: {err}"),
            }
            let _ = release_rx.recv();
        });

        let received = result_rx.recv_timeout(BOUNDED_TRANSCRIPT_TEST_TIMEOUT);
        let _ = release_tx.send(());
        writer.join().expect("writer thread");
        match received {
            Ok(result) => assert_eq!(result.expect("parse transcript"), None),
            Err(err) => {
                parser.join().expect("parser thread");
                panic!("bounded transcript parse did not return before timeout: {err}");
            }
        }
        parser.join().expect("parser thread");
    }

    #[cfg(unix)]
    #[expect(unsafe_code, reason = "mkfifo is required to test FIFO read bounds")]
    fn create_fifo(path: &Path) {
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("fifo path has no NUL");
        // SAFETY: `c_path` is a valid NUL-terminated path for this call, and the
        // returned status is checked before the path is used as a FIFO.
        let result = unsafe { libc::mkfifo(c_path.as_ptr(), TRANSCRIPT_FIFO_MODE) };
        assert_eq!(
            result,
            0,
            "create transcript FIFO {}: {}",
            path.display(),
            io::Error::last_os_error()
        );
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod launch_roots_tests;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod live_tests;

#[cfg(test)]
mod observer_tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use protocol::RuntimeRef;

    use super::watch::{TranscriptWatch, WatcherUnavailable};
    use super::{
        start_transcript_index, ExternalObserverConfig, ExternalSessions, ObservedRoots,
        TranscriptRoot,
    };
    use crate::procwatch::{
        Error, ExitWatch, HostInspector, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
        ProcessInspector,
    };
    use crate::session::{SessionRegistry, SessionRegistryConfig};

    /// Sweep interval short enough to observe several sweeps quickly.
    const SWEEP_INTERVAL: Duration = Duration::from_millis(20);

    /// Host inspector that counts process sweeps.
    #[derive(Debug, Default)]
    struct CountingInspector {
        host: HostInspector,
        sweeps: AtomicUsize,
    }

    impl ProcessInspector for CountingInspector {
        fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, Error> {
            self.host.identity(pid)
        }

        fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, Error> {
            self.host.parent_pid(pid)
        }

        fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, Error> {
            self.host.process(pid)
        }

        fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error> {
            self.sweeps.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, Error> {
            self.host.descendants(root)
        }

        fn cwd(&self, pid: Pid) -> Result<PathBuf, Error> {
            self.host.cwd(pid)
        }

        fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, Error> {
            self.host.executable(pid)
        }

        fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, Error> {
            self.host.exit_watch(identity)
        }

        fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, Error> {
            self.host.ownership_markers(pid)
        }

        fn foreground_process_group(&self, root_pid: Pid) -> Result<Option<Pid>, Error> {
            self.host.foreground_process_group(root_pid)
        }
    }

    fn transcript_root() -> (tempfile::TempDir, PathBuf) {
        let root = pohunek_test_support::tempdir().expect("transcript root");
        let transcript = root.path().join("work/session.jsonl");
        std::fs::create_dir_all(transcript.parent().expect("parent")).expect("project dir");
        std::fs::write(
            &transcript,
            format!(
                "{{\"session_id\":\"native-observer\",\"cwd\":\"/work\",\"transcript_path\":\"{}\"}}\n",
                transcript.display()
            ),
        )
        .expect("write transcript");
        (root, transcript)
    }

    /// Targets with a live backend report it active; the others report the open
    /// failure and rely on scans.
    fn expected_watch_state() -> TranscriptWatch {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            TranscriptWatch::Active
        } else {
            TranscriptWatch::Unavailable(WatcherUnavailable::OpenFailed)
        }
    }

    #[tokio::test]
    async fn the_observer_reports_its_watcher_and_still_scans_and_sweeps() {
        let (root, transcript) = transcript_root();
        let roots = vec![TranscriptRoot {
            agent_base: RuntimeRef::claude(),
            path: root.path().to_path_buf(),
            private: false,
        }];
        let sessions = ExternalSessions::new();

        let (index, watcher) = start_transcript_index(&roots, &sessions).await;

        assert_eq!(watcher.state(), expected_watch_state());
        let indexed = index.inner.lock().expect("index").contains_key(&transcript);
        assert!(indexed, "the startup scan indexes existing transcripts");

        let inspector = Arc::new(CountingInspector::default());
        let registry = SessionRegistry::new_with_inspector(
            SessionRegistryConfig::default(),
            Arc::clone(&inspector) as Arc<dyn ProcessInspector>,
        );
        sessions.spawn_observer(
            registry,
            ExternalObserverConfig {
                roots: ObservedRoots::Fixed(roots),
                sweep_interval: SWEEP_INTERVAL,
            },
        );
        pohunek_test_support::wait::wait_until("the process sweep to keep running", || async {
            (inspector.sweeps.load(Ordering::SeqCst) >= 2).then_some(())
        })
        .await;
        sessions.shutdown();
    }
}

#[cfg(test)]
mod scan_tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use protocol::RuntimeRef;
    use tokio_util::sync::CancellationToken;

    use super::watch::RootScan;
    use super::{ScanLimits, TranscriptIndex, TranscriptRoot, SCAN_LIMITS};

    const GENEROUS: ScanLimits = SCAN_LIMITS;

    fn write_transcript(path: &Path, session_id: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(
            path,
            format!("{{\"session_id\":\"{session_id}\",\"cwd\":\"/work\"}}\n"),
        )
        .expect("write transcript");
    }

    fn root(path: &Path) -> TranscriptRoot {
        TranscriptRoot {
            agent_base: RuntimeRef::claude(),
            path: path.to_path_buf(),
            private: false,
        }
    }

    fn scan(index: &TranscriptIndex, root: &TranscriptRoot, limits: ScanLimits) -> RootScan {
        index.scan_root(
            root,
            std::slice::from_ref(root),
            limits,
            &CancellationToken::new(),
        )
    }

    fn indexed(index: &TranscriptIndex, path: &Path) -> bool {
        index.inner.lock().expect("index").contains_key(path)
    }

    /// Two project directories, each with one transcript, already indexed.
    fn indexed_tree() -> (tempfile::TempDir, TranscriptIndex, PathBuf, PathBuf) {
        let dir = pohunek_test_support::tempdir().expect("root");
        let first = dir.path().join("a/s1.jsonl");
        let second = dir.path().join("b/s2.jsonl");
        write_transcript(&first, "one");
        write_transcript(&second, "two");
        let index = TranscriptIndex::default();
        assert_eq!(
            scan(&index, &root(dir.path()), GENEROUS),
            RootScan::Complete
        );
        assert!(indexed(&index, &first) && indexed(&index, &second));
        (dir, index, first, second)
    }

    #[test]
    fn an_entry_limit_ends_the_scan_incomplete_and_keeps_uninspected_candidates() {
        let (dir, index, first, second) = indexed_tree();
        let fresh = dir.path().join("b/new.jsonl");
        write_transcript(&fresh, "fresh");

        let scan = scan(
            &index,
            &root(dir.path()),
            ScanLimits {
                entries: 1,
                ..GENEROUS
            },
        );

        assert_eq!(scan, RootScan::Incomplete);
        assert!(indexed(&index, &first) && indexed(&index, &second));
        assert!(
            !indexed(&index, &fresh),
            "the uninspected part is untouched"
        );
    }

    #[test]
    fn a_directory_limit_ends_the_scan_incomplete() {
        let (dir, index, first, second) = indexed_tree();

        let scan = scan(
            &index,
            &root(dir.path()),
            ScanLimits {
                directories: 1,
                ..GENEROUS
            },
        );

        assert_eq!(scan, RootScan::Incomplete);
        assert!(indexed(&index, &first) && indexed(&index, &second));
    }

    #[test]
    fn a_transcript_limit_bounds_the_parses_of_one_scan() {
        let dir = pohunek_test_support::tempdir().expect("root");
        for name in ["a", "b", "c", "d"] {
            write_transcript(&dir.path().join(format!("{name}/s.jsonl")), name);
        }
        let index = TranscriptIndex::default();

        let scan = scan(
            &index,
            &root(dir.path()),
            ScanLimits {
                transcripts: 2,
                ..GENEROUS
            },
        );

        assert_eq!(scan, RootScan::Incomplete);
        assert_eq!(index.inner.lock().expect("index").len(), 2);
    }

    #[test]
    fn a_cancelled_scan_stops_without_touching_the_index() {
        let (dir, index, first, second) = indexed_tree();
        let fresh = dir.path().join("a/new.jsonl");
        write_transcript(&fresh, "fresh");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let root = root(dir.path());

        let scan = index.scan_root(&root, std::slice::from_ref(&root), GENEROUS, &cancel);

        assert_eq!(scan, RootScan::Incomplete);
        assert!(indexed(&index, &first) && indexed(&index, &second));
        assert!(!indexed(&index, &fresh));
    }

    /// Scans until a pass reaches the end of the tree, returning the pass count.
    fn passes_to_complete(
        index: &TranscriptIndex,
        root: &TranscriptRoot,
        limits: ScanLimits,
    ) -> usize {
        for pass in 1..=MAX_PASSES {
            if scan(index, root, limits) == RootScan::Complete {
                return pass;
            }
        }
        panic!("the scan did not converge within {MAX_PASSES} passes");
    }

    /// Upper bound on passes any convergence test may need.
    const MAX_PASSES: usize = 40;

    fn nested_tree(dir: &Path, count: usize) {
        for index in 0..count {
            write_transcript(
                &dir.join(format!("d{}/sub/s{index}.jsonl", index % 6)),
                &format!("session-{index}"),
            );
        }
    }

    fn parses(index: &TranscriptIndex) -> usize {
        index.parses.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn repeated_passes_under_a_parse_limit_cover_a_tree_larger_than_the_limit() {
        let dir = pohunek_test_support::tempdir().expect("root");
        nested_tree(dir.path(), 23);
        let index = TranscriptIndex::default();

        let passes = passes_to_complete(
            &index,
            &root(dir.path()),
            ScanLimits {
                transcripts: 5,
                ..GENEROUS
            },
        );

        assert_eq!(passes, 5, "23 transcripts at 5 per pass");
        assert_eq!(index.inner.lock().expect("index").len(), 23);
        assert_eq!(parses(&index), 23, "no transcript is parsed twice");
    }

    #[test]
    fn a_directory_moved_in_after_convergence_is_covered_without_reparsing_the_rest() {
        let dir = pohunek_test_support::tempdir().expect("root");
        nested_tree(dir.path(), 23);
        let index = TranscriptIndex::default();
        let limits = ScanLimits {
            transcripts: 5,
            ..GENEROUS
        };
        passes_to_complete(&index, &root(dir.path()), limits);
        let staging = pohunek_test_support::tempdir().expect("staging");
        for number in 0..12 {
            write_transcript(
                &staging.path().join(format!("x/y{number}.jsonl")),
                &format!("moved-{number}"),
            );
        }
        fs::rename(staging.path().join("x"), dir.path().join("moved")).expect("move in");

        passes_to_complete(&index, &root(dir.path()), limits);

        assert_eq!(index.inner.lock().expect("index").len(), 35);
        assert_eq!(parses(&index), 35, "only the moved-in files were parsed");
    }

    #[test]
    fn an_unchanged_transcript_is_not_parsed_again_but_a_changed_one_is() {
        let (dir, index, first, _second) = indexed_tree();
        let before = parses(&index);
        assert_eq!(
            scan(&index, &root(dir.path()), GENEROUS),
            RootScan::Complete
        );
        assert_eq!(parses(&index), before, "unchanged files are skipped");

        write_transcript(&first, "changed-and-longer");
        assert_eq!(
            scan(&index, &root(dir.path()), GENEROUS),
            RootScan::Complete
        );

        assert_eq!(parses(&index), before + 1);
    }

    #[test]
    fn an_incomplete_scan_prunes_nothing_even_for_files_that_are_gone() {
        let (dir, index, first, second) = indexed_tree();
        fs::remove_file(&first).expect("remove");

        let scan = scan(
            &index,
            &root(dir.path()),
            ScanLimits {
                entries: 1,
                ..GENEROUS
            },
        );

        assert_eq!(scan, RootScan::Incomplete);
        assert!(indexed(&index, &first) && indexed(&index, &second));
    }

    #[test]
    fn a_complete_scan_prunes_what_it_did_not_see() {
        let (dir, index, first, second) = indexed_tree();
        fs::remove_file(&first).expect("remove");

        assert_eq!(
            scan(&index, &root(dir.path()), GENEROUS),
            RootScan::Complete
        );

        assert!(!indexed(&index, &first));
        assert!(indexed(&index, &second));
        assert!(
            !index.parsed.lock().expect("parsed").contains_key(&first),
            "the parse signature goes with it"
        );
    }

    #[test]
    fn a_missing_root_prunes_everything_it_owned() {
        let (dir, index, first, second) = indexed_tree();
        let missing = dir.path().join("gone-root");
        let ghost = missing.join("s.jsonl");
        let candidate = index_candidate(&first, &index);
        index
            .inner
            .lock()
            .expect("index")
            .insert(ghost.clone(), candidate);

        assert_eq!(scan(&index, &root(&missing), GENEROUS), RootScan::Missing);

        assert!(!indexed(&index, &ghost));
        assert!(indexed(&index, &first) && indexed(&index, &second));
    }

    fn index_candidate(path: &Path, index: &TranscriptIndex) -> super::TranscriptCandidate {
        index
            .inner
            .lock()
            .expect("index")
            .get(path)
            .expect("indexed")
            .clone()
    }

    #[test]
    fn a_complete_scan_of_an_outer_root_leaves_a_nested_roots_entries_alone() {
        let dir = pohunek_test_support::tempdir().expect("root");
        let outer = root(dir.path());
        let inner_path = dir.path().join("codex");
        let inner = TranscriptRoot {
            agent_base: RuntimeRef::codex(),
            path: inner_path.clone(),
            private: false,
        };
        let outer_file = dir.path().join("p/c.jsonl");
        let inner_file = inner_path.join("x/s.jsonl");
        write_transcript(&outer_file, "claude");
        write_transcript(&inner_file, "codex");
        let both = [outer.clone(), inner.clone()];
        let index = TranscriptIndex::default();
        let token = CancellationToken::new();
        for root in &both {
            index.scan_root(root, &both, GENEROUS, &token);
        }
        assert!(indexed(&index, &outer_file) && indexed(&index, &inner_file));

        assert_eq!(
            index.scan_root(&outer, &both, GENEROUS, &token),
            RootScan::Complete
        );

        assert!(indexed(&index, &inner_file), "owned by the nested root");
        assert!(indexed(&index, &outer_file));
    }

    #[test]
    fn an_atomic_replacement_keeping_size_and_mtime_is_parsed_again() {
        let dir = pohunek_test_support::tempdir().expect("root");
        let path = dir.path().join("p/s.jsonl");
        write_transcript(&path, "aaaa");
        let index = TranscriptIndex::default();
        assert_eq!(
            scan(&index, &root(dir.path()), GENEROUS),
            RootScan::Complete
        );
        let before = parses(&index);
        let mtime = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("mtime");

        let staged = dir.path().join("p/s.tmp");
        write_transcript(&staged, "bbbb");
        fs::OpenOptions::new()
            .write(true)
            .open(&staged)
            .and_then(|file| file.set_modified(mtime))
            .expect("copy mtime");
        fs::rename(&staged, &path).expect("replace atomically");
        assert_eq!(
            fs::metadata(&path).map(|metadata| metadata.len()).ok(),
            Some(
                u64::try_from("{\"session_id\":\"aaaa\",\"cwd\":\"/work\"}\n".len()).expect("len")
            ),
            "the replacement has the same size"
        );

        assert_eq!(
            scan(&index, &root(dir.path()), GENEROUS),
            RootScan::Complete
        );

        assert_eq!(parses(&index), before + 1);
        let session = index
            .inner
            .lock()
            .expect("index")
            .get(&path)
            .and_then(|candidate| candidate.native_session_id.clone());
        assert_eq!(session.as_deref(), Some("bbbb"));
    }

    #[test]
    fn a_root_that_is_a_symlink_into_another_roots_tree_owns_its_own_transcripts() {
        let dir = pohunek_test_support::tempdir().expect("root");
        let claude_dir = dir.path().join("claude");
        let claude_file = claude_dir.join("p/c.jsonl");
        let real_codex_file = claude_dir.join("codex/x/s.jsonl");
        write_transcript(&claude_file, "claude-session");
        write_transcript(&real_codex_file, "codex-session");
        let link = dir.path().join("codex-link");
        std::os::unix::fs::symlink(claude_dir.join("codex"), &link).expect("symlink");
        let claude = root(&claude_dir);
        let codex = TranscriptRoot {
            agent_base: RuntimeRef::codex(),
            path: link.clone(),
            private: false,
        };
        let both = [claude.clone(), codex.clone()];
        let index = TranscriptIndex::default();
        let token = CancellationToken::new();

        // The outer root is scanned last so it cannot be repaired by a later
        // prune of the nested root.
        for root in [&codex, &claude] {
            assert_eq!(
                index.scan_root(root, &both, GENEROUS, &token),
                RootScan::Complete
            );
        }

        assert!(indexed(&index, &claude_file));
        assert!(
            !indexed(&index, &real_codex_file),
            "the outer walk leaves the nested root's files to it"
        );
        let agent = index
            .inner
            .lock()
            .expect("index")
            .get(&link.join("x/s.jsonl"))
            .map(|candidate| candidate.agent_base.clone());
        assert_eq!(agent, Some(RuntimeRef::codex()));
    }

    #[test]
    fn ownership_follows_canonical_identity_not_the_spelling_of_the_path() {
        let dir = pohunek_test_support::tempdir().expect("root");
        let claude_dir = dir.path().join("claude");
        fs::create_dir_all(claude_dir.join("codex")).expect("tree");
        let link = dir.path().join("codex-link");
        std::os::unix::fs::symlink(claude_dir.join("codex"), &link).expect("symlink");
        let roots = [
            root(&claude_dir),
            TranscriptRoot {
                agent_base: RuntimeRef::codex(),
                path: link.clone(),
                private: false,
            },
        ];
        let canonical = super::canonical_roots(&roots);
        let owner = |path: &Path| {
            super::owning_root(&roots, &canonical, path).map(|root| root.agent_base.clone())
        };

        assert_eq!(
            owner(&claude_dir.join("codex/x/s.jsonl")),
            Some(RuntimeRef::codex())
        );
        assert_eq!(owner(&link.join("x/s.jsonl")), Some(RuntimeRef::codex()));
        assert_eq!(
            owner(&claude_dir.join("p/c.jsonl")),
            Some(RuntimeRef::claude())
        );
    }
}
