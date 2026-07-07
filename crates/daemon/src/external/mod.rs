//! Opt-in observation of agent processes outside pohunek-owned PTYs.
//!
//! The observer combines a same-user process sweep with lightweight transcript
//! indexing. It never attaches to, writes to, resizes, stops, or otherwise
//! controls external processes; it only publishes read-only `SessionInfo`
//! snapshots for UI and CLI visibility.

// Rust guideline compliant 2026-07-07

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{AgentKind, SessionId, SessionInfo};
use serde_json::Value;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::procwatch::{Pid, ProcessFact};
use crate::session::SessionRegistry;

/// Prefix for synthetic session ids assigned to external processes.
pub(crate) const EXTERNAL_SESSION_ID_PREFIX: &str = "ext-";
/// External agents have no PTY, so their terminal geometry is explicitly zero.
pub(crate) const EXTERNAL_TERMINAL_COLS: u16 = 0;
/// External agents have no PTY, so their terminal geometry is explicitly zero.
pub(crate) const EXTERNAL_TERMINAL_ROWS: u16 = 0;

/// Short debounce for transcript writes before parsing a JSONL file.
///
/// Provider CLIs can append multiple small JSON records in quick succession at
/// session start. Delaying slightly avoids parsing a partially written line
/// while still making the observer responsive to new files.
const TRANSCRIPT_WRITE_DEBOUNCE: Duration = Duration::from_millis(75);
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
/// Buffer used for one nonblocking `read(2)` from the inotify fd.
///
/// The value fits many events at once while staying small enough for a stackless
/// heap allocation in the watcher task.
const INOTIFY_BUFFER_BYTES: usize = 16 * 1024;
/// Flags used when opening the inotify instance.
const INOTIFY_INIT_FLAGS: libc::c_int = libc::IN_NONBLOCK | libc::IN_CLOEXEC;
/// Inotify mask for recursively watched transcript directories.
const INOTIFY_WATCH_MASK: u32 = libc::IN_CREATE
    | libc::IN_MOVED_TO
    | libc::IN_CLOSE_WRITE
    | libc::IN_MODIFY
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;
/// Claude config directory override.
const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
/// Codex home directory override.
const CODEX_HOME_ENV: &str = "CODEX_HOME";
/// Home directory environment variable used for provider defaults.
const HOME_ENV: &str = "HOME";
/// Claude's default config directory relative to `$HOME`.
const CLAUDE_HOME_RELATIVE: &str = ".claude";
/// Codex's default home directory relative to `$HOME`.
const CODEX_HOME_RELATIVE: &str = ".codex";
/// Claude transcript root below its config directory.
const CLAUDE_TRANSCRIPT_SUBDIR: &str = "projects";
/// Codex transcript root below its home directory.
const CODEX_TRANSCRIPT_SUBDIR: &str = "sessions";
/// JSONL transcript extension.
const JSONL_EXTENSION: &str = "jsonl";

/// In-memory external session store and observer signals.
#[derive(Debug, Clone)]
pub(crate) struct ExternalSessions {
    inner: Arc<ExternalSessionsInner>,
}

#[derive(Debug)]
struct ExternalSessionsInner {
    entries: AsyncMutex<HashMap<Pid, SessionInfo>>,
    rescan: Notify,
    shutdown: CancellationToken,
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
    agent_base: AgentKind,
    path: PathBuf,
}

/// Runtime observer configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalObserverConfig {
    roots: Vec<TranscriptRoot>,
    sweep_interval: Duration,
}

/// Parsed transcript metadata used to enrich an external process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptCandidate {
    /// Agent kind inferred from the transcript tree.
    pub(crate) agent_base: AgentKind,
    /// Native provider session id, when present.
    pub(crate) native_session_id: Option<String>,
    /// Native transcript path.
    pub(crate) native_session_path: String,
    /// Working directory reported by the provider transcript.
    pub(crate) cwd: Option<PathBuf>,
    updated_at: SystemTime,
}

/// Shared transcript candidate index.
#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptIndex {
    inner: Arc<Mutex<HashMap<PathBuf, TranscriptCandidate>>>,
}

#[derive(Debug)]
struct InotifyWatcher {
    fd: AsyncFd<OwnedFd>,
    paths_by_wd: HashMap<libc::c_int, PathBuf>,
    watched_paths: HashSet<PathBuf>,
}

#[derive(Debug)]
struct InotifyEvent {
    wd: libc::c_int,
    mask: u32,
    name: Option<OsString>,
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

    /// Builds observer config from provider transcript environment.
    #[must_use]
    pub(crate) fn observer_config(sweep_interval: Duration) -> ExternalObserverConfig {
        ExternalObserverConfig::from_env(sweep_interval)
    }

    /// Spawns the external observer task.
    pub(crate) fn spawn_observer(&self, registry: SessionRegistry, config: ExternalObserverConfig) {
        let sessions = self.clone();
        tokio::spawn(async move {
            run_observer(registry, sessions, config).await;
        });
    }

    /// Stops the observer loop and inotify watcher.
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
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.id.0.cmp(&right.id.0));
        sessions
    }

    /// Finds one external session by id.
    pub(crate) async fn inspect(&self, id: &SessionId) -> Option<SessionInfo> {
        let pid = external_pid(id)?;
        self.inner.entries.lock().await.get(&pid).cloned()
    }

    /// Whether `id` currently belongs to an observed external process.
    pub(crate) async fn contains_id(&self, id: &SessionId) -> bool {
        self.inspect(id).await.is_some()
    }

    /// Returns currently known external pids.
    pub(crate) async fn pids(&self) -> HashSet<Pid> {
        self.inner.entries.lock().await.keys().copied().collect()
    }

    /// Inserts or refreshes an external session snapshot.
    pub(crate) async fn upsert(&self, mut info: SessionInfo) -> Option<ExternalSessionChange> {
        let mut entries = self.inner.entries.lock().await;
        let Some(existing) = entries.get(&info.pid) else {
            entries.insert(info.pid, info.clone());
            return Some(ExternalSessionChange::Created(info));
        };

        info.created_at.clone_from(&existing.created_at);
        if external_info_matches(existing, &info) {
            return None;
        }
        entries.insert(info.pid, info.clone());
        Some(ExternalSessionChange::Updated(info))
    }

    /// Removes entries whose pids were absent from a successful sweep.
    pub(crate) async fn remove_unobserved(&self, observed: &HashSet<Pid>) -> Vec<SessionInfo> {
        let mut entries = self.inner.entries.lock().await;
        let stale = entries
            .keys()
            .copied()
            .filter(|pid| !observed.contains(pid))
            .collect::<Vec<_>>();
        stale
            .into_iter()
            .filter_map(|pid| entries.remove(&pid))
            .collect()
    }

    /// Removes one external entry after its exit watch fires.
    pub(crate) async fn remove_pid(&self, pid: Pid) -> Option<SessionInfo> {
        self.inner.entries.lock().await.remove(&pid)
    }
}

impl Default for ExternalSessions {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalObserverConfig {
    fn from_env(sweep_interval: Duration) -> Self {
        let mut roots = Vec::new();
        if let Some(path) = provider_root(
            CLAUDE_CONFIG_DIR_ENV,
            CLAUDE_HOME_RELATIVE,
            CLAUDE_TRANSCRIPT_SUBDIR,
        ) {
            roots.push(TranscriptRoot {
                agent_base: AgentKind::Claude,
                path,
            });
        }
        if let Some(path) =
            provider_root(CODEX_HOME_ENV, CODEX_HOME_RELATIVE, CODEX_TRANSCRIPT_SUBDIR)
        {
            roots.push(TranscriptRoot {
                agent_base: AgentKind::Codex,
                path,
            });
        }
        Self {
            roots,
            sweep_interval,
        }
    }
}

impl TranscriptIndex {
    /// Scans every configured root once.
    pub(crate) async fn scan_roots(&self, roots: Vec<TranscriptRoot>) {
        let index = self.clone();
        if let Err(err) = tokio::task::spawn_blocking(move || {
            for root in roots {
                if let Err(err) = index.scan_root(&root) {
                    debug!(
                        agent = ?root.agent_base,
                        error = %err,
                        "failed to scan external transcript root"
                    );
                }
            }
        })
        .await
        {
            warn!(error = %err, "external transcript scan task panicked");
        }
    }

    /// Parses one transcript path and updates the candidate index.
    pub(crate) fn upsert_path(&self, agent_base: AgentKind, path: &Path) -> io::Result<bool> {
        if let Some(candidate) = parse_transcript(agent_base, path)? {
            let mut inner = self.inner.lock().unwrap_or_else(MutexError::into_inner);
            let changed = inner.get(path) != Some(&candidate);
            inner.insert(path.to_path_buf(), candidate);
            Ok(changed)
        } else {
            let removed = self
                .inner
                .lock()
                .unwrap_or_else(MutexError::into_inner)
                .remove(path)
                .is_some();
            Ok(removed)
        }
    }

    /// Finds the best transcript candidate for `fact` and `cwd`.
    pub(crate) fn best_match(
        &self,
        agent_base: AgentKind,
        cwd: &Path,
        fact: &ProcessFact,
    ) -> Option<TranscriptCandidate> {
        self.inner
            .lock()
            .unwrap_or_else(MutexError::into_inner)
            .values()
            .filter(|candidate| candidate.agent_base == agent_base)
            .filter(|candidate| transcript_matches_process(candidate, cwd, fact))
            .max_by_key(|candidate| candidate.updated_at)
            .cloned()
    }

    fn scan_root(&self, root: &TranscriptRoot) -> io::Result<()> {
        if !root.path.is_dir() {
            return Ok(());
        }
        let mut queue = VecDeque::from([root.path.clone()]);
        while let Some(dir) = queue.pop_front() {
            for entry in fs::read_dir(&dir)? {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                };
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                };
                if file_type.is_dir() {
                    queue.push_back(path);
                } else if is_jsonl_path(&path) {
                    self.upsert_path(root.agent_base, &path)?;
                }
            }
        }
        Ok(())
    }
}

impl InotifyWatcher {
    fn open(roots: &[TranscriptRoot]) -> io::Result<Self> {
        let fd = inotify_init()?;
        let mut watcher = Self {
            fd: AsyncFd::new(fd)?,
            paths_by_wd: HashMap::new(),
            watched_paths: HashSet::new(),
        };
        for root in roots {
            if let Err(err) = watcher.add_tree(&root.path) {
                debug!(
                    agent = ?root.agent_base,
                    error = %err,
                    "failed to watch external transcript root"
                );
            }
        }
        Ok(watcher)
    }

    async fn run(
        mut self,
        roots: Vec<TranscriptRoot>,
        index: TranscriptIndex,
        sessions: ExternalSessions,
    ) {
        loop {
            tokio::select! {
                () = sessions.inner.shutdown.cancelled() => break,
                readiness = self.fd.readable() => {
                    let Ok(mut guard) = readiness else {
                        break;
                    };
                    match guard.try_io(|inner| read_inotify_events(inner.get_ref().as_raw_fd())) {
                        Ok(Ok(events)) => self.handle_events(events, &roots, &index, &sessions),
                        Ok(Err(err)) => {
                            debug!(error = %err, "failed to read external transcript inotify events");
                        }
                        Err(_would_block) => {}
                    }
                }
            }
        }
    }

    fn handle_events(
        &mut self,
        events: Vec<InotifyEvent>,
        roots: &[TranscriptRoot],
        index: &TranscriptIndex,
        sessions: &ExternalSessions,
    ) {
        for event in events {
            let Some(path) = self.event_path(&event) else {
                continue;
            };
            if event.mask & libc::IN_ISDIR != 0 {
                if create_or_move_event(event.mask) {
                    if let Err(err) = self.add_tree(&path) {
                        debug!(
                            path = %path.display(),
                            error = %err,
                            "failed to watch new external transcript directory"
                        );
                    }
                }
                continue;
            }
            if !write_event(event.mask) || !is_jsonl_path(&path) {
                continue;
            }
            let Some(agent_base) = root_agent_for_path(roots, &path) else {
                continue;
            };
            schedule_transcript_parse(index.clone(), sessions.clone(), agent_base, path);
        }
    }

    fn add_tree(&mut self, root: &Path) -> io::Result<()> {
        if !root.is_dir() {
            return Ok(());
        }
        let mut queue = VecDeque::from([root.to_path_buf()]);
        while let Some(dir) = queue.pop_front() {
            self.add_dir(&dir)?;
            for entry in fs::read_dir(&dir)? {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                };
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err),
                };
                if file_type.is_dir() {
                    queue.push_back(entry.path());
                }
            }
        }
        Ok(())
    }

    fn add_dir(&mut self, path: &Path) -> io::Result<()> {
        if self.watched_paths.contains(path) {
            return Ok(());
        }
        let wd = inotify_add_watch(self.fd.get_ref().as_raw_fd(), path)?;
        self.paths_by_wd.insert(wd, path.to_path_buf());
        self.watched_paths.insert(path.to_path_buf());
        Ok(())
    }

    fn event_path(&self, event: &InotifyEvent) -> Option<PathBuf> {
        let base = self.paths_by_wd.get(&event.wd)?;
        Some(match &event.name {
            Some(name) => base.join(name),
            None => base.clone(),
        })
    }
}

async fn run_observer(
    registry: SessionRegistry,
    sessions: ExternalSessions,
    config: ExternalObserverConfig,
) {
    let index = TranscriptIndex::default();
    if config.roots.is_empty() {
        debug!("external observer has no transcript roots to watch");
    }
    if let Ok(watcher) = InotifyWatcher::open(&config.roots) {
        let watcher_roots = config.roots.clone();
        let watcher_index = index.clone();
        let watcher_sessions = sessions.clone();
        tokio::spawn(async move {
            watcher
                .run(watcher_roots, watcher_index, watcher_sessions)
                .await;
        });
    } else {
        debug!("external transcript inotify watcher unavailable; process sweep still runs");
    }
    index.scan_roots(config.roots.clone()).await;

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

fn parse_transcript(agent_base: AgentKind, path: &Path) -> io::Result<Option<TranscriptCandidate>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
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
    Ok(Some(TranscriptCandidate {
        agent_base,
        native_session_id,
        native_session_path,
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

fn root_agent_for_path(roots: &[TranscriptRoot], path: &Path) -> Option<AgentKind> {
    roots
        .iter()
        .find(|root| path.starts_with(&root.path))
        .map(|root| root.agent_base)
}

fn provider_root(env_var: &str, home_relative: &str, transcript_subdir: &str) -> Option<PathBuf> {
    if let Some(value) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
        return Some(expand_tilde(PathBuf::from(value)).join(transcript_subdir));
    }
    std::env::var_os(HOME_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(home_relative).join(transcript_subdir))
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path;
    };
    let Some(home) = std::env::var_os(HOME_ENV).filter(|value| !value.is_empty()) else {
        return path;
    };
    if raw == "~" {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return PathBuf::from(home).join(rest);
    }
    path
}

fn schedule_transcript_parse(
    index: TranscriptIndex,
    sessions: ExternalSessions,
    agent_base: AgentKind,
    path: PathBuf,
) {
    tokio::spawn(async move {
        tokio::time::sleep(TRANSCRIPT_WRITE_DEBOUNCE).await;
        match index.upsert_path(agent_base, &path) {
            Ok(true) => sessions.notify_rescan(),
            Ok(false) => {}
            Err(err) => {
                debug!(
                    path = %path.display(),
                    error = %err,
                    "failed to parse external transcript candidate"
                );
            }
        }
    });
}

fn create_or_move_event(mask: u32) -> bool {
    mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
}

fn write_event(mask: u32) -> bool {
    mask & (libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_MODIFY) != 0
}

#[expect(unsafe_code, reason = "inotify requires Linux syscalls")]
fn inotify_init() -> io::Result<OwnedFd> {
    // SAFETY: `inotify_init1` returns a new file descriptor or -1 with errno set.
    // `INOTIFY_INIT_FLAGS` only requests nonblocking close-on-exec behavior.
    let fd = unsafe { libc::inotify_init1(INOTIFY_INIT_FLAGS) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let raw_fd = RawFd::try_from(fd)
        .map_err(|err| io::Error::other(format!("invalid inotify fd {fd}: {err}")))?;
    // SAFETY: the descriptor was just returned by `inotify_init1` and is owned by
    // this process; `OwnedFd` closes it exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

#[expect(unsafe_code, reason = "inotify requires Linux syscalls")]
fn inotify_add_watch(fd: RawFd, path: &Path) -> io::Result<libc::c_int> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_err| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "inotify path contains an interior NUL byte",
        )
    })?;
    // SAFETY: `c_path` is a valid NUL-terminated path for the duration of the
    // call. `fd` is the live inotify descriptor owned by `InotifyWatcher`.
    let wd = unsafe { libc::inotify_add_watch(fd, c_path.as_ptr(), INOTIFY_WATCH_MASK) };
    if wd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(wd)
}

#[expect(unsafe_code, reason = "inotify requires Linux syscalls")]
fn read_inotify_events(fd: RawFd) -> io::Result<Vec<InotifyEvent>> {
    let mut buffer = vec![0_u8; INOTIFY_BUFFER_BYTES];
    // SAFETY: `buffer` is valid for writes of `buffer.len()` bytes, and `fd` is a
    // nonblocking inotify descriptor. The return value is checked before use.
    let bytes = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if bytes == -1 {
        return Err(io::Error::last_os_error());
    }
    let bytes = usize::try_from(bytes)
        .map_err(|err| io::Error::other(format!("invalid inotify read length: {err}")))?;
    buffer.truncate(bytes);
    parse_inotify_events(&buffer)
}

#[expect(unsafe_code, reason = "inotify event headers are C structs")]
fn parse_inotify_events(buffer: &[u8]) -> io::Result<Vec<InotifyEvent>> {
    let header_len = mem::size_of::<libc::inotify_event>();
    let mut offset = 0_usize;
    let mut events = Vec::new();
    while offset + header_len <= buffer.len() {
        // SAFETY: the bounds check above guarantees a full header is present.
        // `read_unaligned` is used because inotify event records are byte-packed.
        let raw = unsafe {
            std::ptr::read_unaligned(buffer[offset..].as_ptr().cast::<libc::inotify_event>())
        };
        let name_len = usize::try_from(raw.len)
            .map_err(|err| io::Error::other(format!("invalid inotify event name length: {err}")))?;
        let name_start = offset + header_len;
        let name_end = name_start.saturating_add(name_len);
        if name_end > buffer.len() {
            break;
        }
        let name = event_name(&buffer[name_start..name_end]);
        events.push(InotifyEvent {
            wd: raw.wd,
            mask: raw.mask,
            name,
        });
        offset = name_end;
    }
    Ok(events)
}

fn event_name(bytes: &[u8]) -> Option<OsString> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    (end > 0).then(|| OsStr::from_bytes(&bytes[..end]).to_owned())
}

type MutexError<T> = std::sync::PoisonError<T>;

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use protocol::AgentKind;

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
        let dir = temp_dir("oversized-transcript");
        let fifo = dir.join("transcript.jsonl");
        create_fifo(&fifo);

        let (result_tx, result_rx) = mpsc::channel();
        let parser_path = fifo.clone();
        let parser = thread::spawn(move || {
            let result = parse_transcript(AgentKind::Claude, &parser_path);
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

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "pohunek-external-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }
}
