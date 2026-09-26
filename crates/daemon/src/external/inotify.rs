//! Live transcript discovery through Linux inotify.
//!
//! The watcher adds every transcript directory below the provider roots and
//! parses JSONL files shortly after they are written, so a newly started
//! external agent is enriched without waiting for the next process sweep.

// Rust guideline compliant 2026-09-24

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::AgentKind;
use tokio::io::unix::AsyncFd;
use tracing::debug;

use super::{is_jsonl_path, ExternalSessions, TranscriptIndex, TranscriptRoot};

/// Short debounce for transcript writes before parsing a JSONL file.
///
/// Provider CLIs can append multiple small JSON records in quick succession at
/// session start. Delaying slightly avoids parsing a partially written line
/// while still making the observer responsive to new files.
const TRANSCRIPT_WRITE_DEBOUNCE: Duration = Duration::from_millis(75);

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

/// Recursive inotify watch over the provider transcript roots.
#[derive(Debug)]
pub(super) struct InotifyWatcher {
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

impl InotifyWatcher {
    /// Opens the watcher and adds every existing directory below `roots`.
    pub(super) fn open(roots: &[TranscriptRoot]) -> io::Result<Self> {
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

    /// Parses written transcripts until the observer shuts down.
    pub(super) async fn run(
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

fn root_agent_for_path(roots: &[TranscriptRoot], path: &Path) -> Option<AgentKind> {
    roots
        .iter()
        .find(|root| path.starts_with(&root.path))
        .map(|root| root.agent_base.clone())
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
