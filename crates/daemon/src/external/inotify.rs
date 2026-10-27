//! Linux inotify backend for live transcript watching.
//!
//! inotify has no recursive watch, so the backend adds one watch per directory
//! below each provider root and adds watches for directories created later.
//! Anything that makes the path-to-watch mapping unreliable (queue overflow,
//! a watched directory deleted or renamed, an unmount) is reported as a
//! [`WatchEvent::Rescan`] or [`WatchEvent::TreeChanged`] so the runner rebuilds
//! the registrations from the filesystem.

// Rust guideline compliant 2026-09-29

use std::collections::{HashMap, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tokio::io::unix::AsyncFd;
use tokio_util::sync::CancellationToken;

use super::watch::{RootRegistration, WalkBound, WatchBackend, WatchEvent};
use super::{is_absent_error, is_jsonl_path, MAX_WALK_DIRECTORIES, MAX_WALK_ENTRIES};

/// Cause reported when the inotify instance cannot be created.
pub(super) const OPEN_FAILED_CAUSE: &str = "inotify_open_failed";

/// Maximum number of transcript paths one directory-appearance walk reports.
///
/// Files inside a directory that appears (for example moved in) produced no event
/// of their own; reporting them is bounded so a huge tree cannot flood the runner,
/// and reaching the bound is answered by one rescan instead.
const MAX_DISCOVERED_TRANSCRIPTS: usize = 1024;
/// Buffer used for one nonblocking `read(2)` from the inotify fd.
///
/// The value fits many events at once while keeping each read, and therefore the
/// batch handed to the runner, bounded.
const INOTIFY_BUFFER_BYTES: usize = 16 * 1024;
/// Flags used when opening the inotify instance.
const INOTIFY_INIT_FLAGS: libc::c_int = libc::IN_NONBLOCK | libc::IN_CLOEXEC;
/// Inotify mask for watched transcript directories.
///
/// `IN_ONLYDIR` makes a path that stopped being a directory fail instead of
/// being watched.
const INOTIFY_WATCH_MASK: u32 = libc::IN_CREATE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_CLOSE_WRITE
    | libc::IN_MODIFY
    | libc::IN_DELETE
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;
/// Events that change or remove a file entry.
const FILE_EVENT_MASK: u32 = libc::IN_CREATE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_CLOSE_WRITE
    | libc::IN_MODIFY
    | libc::IN_ATTRIB
    | libc::IN_DELETE;
/// Events that add a directory entry.
const DIR_ADDED_MASK: u32 = libc::IN_CREATE | libc::IN_MOVED_TO;
/// Events after which a watch no longer maps to a live path.
const WATCH_LOST_MASK: u32 =
    libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_IGNORED | libc::IN_UNMOUNT;

/// Recursive inotify watch over the provider transcript roots.
#[derive(Debug)]
pub(super) struct InotifyBackend {
    fd: AsyncFd<OwnedFd>,
    paths_by_wd: HashMap<libc::c_int, PathBuf>,
    wd_by_path: HashMap<PathBuf, libc::c_int>,
    lister: DirLister,
    add_watch: AddWatch,
    /// Directory levels below a root its tree registers, for roots that bound
    /// the walk.
    walk_bounds: HashMap<PathBuf, WalkBound>,
    /// The deepest-level directories of bounded roots (a session directory of
    /// a dated tree) that hold a watch, oldest first.
    recent_deepest: VecDeque<(PathBuf, PathBuf)>,
}

/// One directory entry as seen by the tree walk.
#[derive(Debug)]
struct ListedEntry {
    path: PathBuf,
    is_dir: bool,
}

/// Streams a directory's entries; entry-level failures stay in the items so the
/// walk can count them.
type DirLister = fn(&Path) -> io::Result<Box<dyn Iterator<Item = io::Result<ListedEntry>>>>;

/// Adds one inotify watch.
type AddWatch = fn(RawFd, &Path) -> io::Result<libc::c_int>;

fn list_directory(dir: &Path) -> io::Result<Box<dyn Iterator<Item = io::Result<ListedEntry>>>> {
    Ok(Box::new(fs::read_dir(dir)?.map(|entry| {
        let entry = entry?;
        let file_type = entry.file_type()?;
        Ok(ListedEntry {
            path: entry.path(),
            is_dir: file_type.is_dir(),
        })
    })))
}

/// Transcripts found while walking a directory that appeared.
#[derive(Debug, Default)]
struct Discovery {
    paths: Vec<PathBuf>,
    overflowed: bool,
}

impl Discovery {
    /// Records `path` when it is a transcript, within the discovery bound.
    fn note(&mut self, path: PathBuf) {
        if !is_jsonl_path(&path) {
            return;
        }
        if self.paths.len() < MAX_DISCOVERED_TRANSCRIPTS {
            self.paths.push(path);
        } else {
            self.overflowed = true;
        }
    }
}

/// Whether an error means the watch or walk budget is exhausted.
///
/// The kernel reports an exhausted `fs.inotify.max_user_watches` as `ENOSPC`
/// and kernel memory exhaustion as `ENOMEM`.
fn is_limit_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded | io::ErrorKind::OutOfMemory
    )
}

/// Queues `subdirectories`, found at `depth`, for the walk: breadth first, or
/// for a bounded root depth first with the highest names first, so the newest
/// directories are registered before an exhausted budget cuts the walk.
fn enqueue(
    queue: &mut VecDeque<(PathBuf, u8)>,
    subdirectories: Vec<PathBuf>,
    depth: u8,
    newest_first: bool,
) {
    if newest_first {
        for path in subdirectories.into_iter().rev() {
            queue.push_front((path, depth + 1));
        }
    } else {
        queue.extend(subdirectories.into_iter().map(|path| (path, depth + 1)));
    }
}

/// `entries` with the highest names first and entry errors last.
fn newest_entry_first(
    entries: Box<dyn Iterator<Item = io::Result<ListedEntry>>>,
) -> Box<dyn Iterator<Item = io::Result<ListedEntry>>> {
    let (mut listed, failed): (Vec<_>, Vec<_>) = entries.partition(Result::is_ok);
    listed.sort_by_key(|entry| {
        std::cmp::Reverse(
            entry
                .as_ref()
                .ok()
                .and_then(|entry| entry.path.file_name().map(std::ffi::OsStr::to_os_string)),
        )
    });
    Box::new(listed.into_iter().chain(failed))
}

/// The failures a tree walk met: how many directories failed and the first
/// error, which a partial registration reports.
#[derive(Debug, Default)]
struct Failures {
    directories: usize,
    first: Option<io::Error>,
}

impl Failures {
    fn record(&mut self, error: io::Error) {
        self.directories += 1;
        self.first.get_or_insert(error);
    }

    /// Records a cancelled walk as a failed one; returns whether to stop.
    fn cancelled(&mut self, cancel: &CancellationToken) -> bool {
        if !cancel.is_cancelled() {
            return false;
        }
        self.record(io::Error::from(io::ErrorKind::Interrupted));
        true
    }
}

/// Most deepest-level directories of one dated root that hold a watch.
///
/// A dated tree grows by a directory per session, so it would fill the
/// backend-wide [`MAX_WALK_DIRECTORIES`] budget. The newest ones are watched, a
/// new one evicts the oldest, and the date levels above them always keep their
/// watches: a new session is seen live whatever the age of the store, while
/// older sessions are reached by the periodic reconcile.
const MAX_WATCHED_DEEPEST_DIRECTORIES: usize = 1024;

fn limit_error(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::QuotaExceeded, reason)
}

#[derive(Debug)]
struct InotifyEvent {
    wd: libc::c_int,
    mask: u32,
    name: Option<OsString>,
}

impl InotifyBackend {
    /// Opens an inotify instance without watching anything yet.
    pub(super) fn open() -> io::Result<Self> {
        Ok(Self {
            fd: AsyncFd::new(inotify_init()?)?,
            paths_by_wd: HashMap::new(),
            wd_by_path: HashMap::new(),
            lister: list_directory,
            add_watch: inotify_add_watch,
            walk_bounds: HashMap::new(),
            recent_deepest: VecDeque::new(),
        })
    }

    /// Translates raw inotify records into hints, registering new directories.
    fn translate(&mut self, events: Vec<InotifyEvent>, out: &mut Vec<WatchEvent>) {
        for event in events {
            if event.mask & libc::IN_Q_OVERFLOW != 0 {
                out.push(WatchEvent::Rescan);
                continue;
            }
            let Some(path) = self.event_path(&event) else {
                continue;
            };
            if event.mask & WATCH_LOST_MASK != 0 {
                out.push(WatchEvent::TreeChanged(path));
                continue;
            }
            if event.mask & libc::IN_ISDIR != 0 {
                if event.mask & DIR_ADDED_MASK != 0 {
                    out.push(WatchEvent::NewDirectory(path.clone()));
                    if is_jsonl_path(&path) {
                        // A directory taking over a transcript's name must drop the candidate.
                        out.push(WatchEvent::Changed(path));
                    }
                } else {
                    out.push(WatchEvent::TreeChanged(path));
                }
                continue;
            }
            if event.mask & FILE_EVENT_MASK != 0 {
                out.push(WatchEvent::Changed(path));
            }
        }
    }

    /// Adds a watch for every directory in the tree at `root`.
    ///
    /// Symlinked directories below the root are not followed. Failures below the
    /// root are counted and the walk continues, so one unreadable directory does
    /// not hide its siblings. An exhausted watch limit, entry budget or directory
    /// queue ends the walk as a partial registration.
    fn register_tree(
        &mut self,
        root: &Path,
        mut discovery: Option<&mut Discovery>,
        cancel: &CancellationToken,
    ) -> RootRegistration {
        let mut failures = Failures::default();
        let mut budget = MAX_WALK_ENTRIES;
        let (bound, start_depth) = self.bound_of(root);
        let limit = bound.as_ref().map(|(_, bound)| bound.max_depth);
        // The root of a dated tree, whose deepest directories share a bounded
        // number of watches.
        let share_root = bound
            .filter(|(_, bound)| bound.dated)
            .map(|(bound_root, _)| bound_root);
        if limit.is_some_and(|max| start_depth > max) {
            // Below the bound nothing is watched, so a directory that appears
            // there is neither registered nor walked.
            return RootRegistration::Complete;
        }
        let mut queue = VecDeque::from([(root.to_path_buf(), start_depth)]);
        let mut registered_deepest = Vec::new();
        let mut is_root = true;
        'walk: while let Some((dir, depth)) = queue.pop_front() {
            if failures.cancelled(cancel) {
                break 'walk;
            }
            // Only a directory that is new, at the deepest level of a bounded
            // root, counts against the share of deepest-level watches.
            let new_deepest = share_root.is_some()
                && limit == Some(depth)
                && depth > 0
                && !self.wd_by_path.contains_key(&dir);
            if new_deepest
                && discovery.is_none()
                && share_root.as_deref().is_some_and(|share_root| {
                    self.watched_deepest(share_root) + registered_deepest.len()
                        >= MAX_WATCHED_DEEPEST_DIRECTORIES
                })
            {
                // A pass over the whole tree keeps the newest session
                // directories it met first; the rest wait for the reconcile.
                continue;
            }
            match self.add_dir(&dir) {
                Ok(true) => {
                    if new_deepest {
                        registered_deepest.push(dir.clone());
                    }
                }
                Ok(false) => {
                    // Another path already watches this directory; that path's
                    // registration covers the tree.
                    is_root = false;
                    continue;
                }
                Err(error) if is_absent_error(&error) => {
                    if is_root {
                        return RootRegistration::Missing;
                    }
                    continue;
                }
                Err(error) => {
                    let limit = is_limit_error(&error);
                    if is_root {
                        return RootRegistration::Failed(error);
                    }
                    failures.record(error);
                    if limit {
                        break 'walk;
                    }
                    continue;
                }
            }
            is_root = false;
            let Some(entries) = self.list_for_walk(&dir, share_root.is_some(), &mut failures)
            else {
                continue;
            };
            let mut subdirectories = Vec::new();
            for entry in entries {
                if failures.cancelled(cancel) {
                    break 'walk;
                }
                if budget == 0 {
                    failures.record(limit_error("directory walk entry budget exhausted"));
                    break 'walk;
                }
                budget -= 1;
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) if is_absent_error(&error) => continue,
                    Err(error) => {
                        failures.record(error);
                        continue;
                    }
                };
                if entry.is_dir {
                    if limit.is_some_and(|max| depth >= max) {
                        continue;
                    }
                    if queue.len() + subdirectories.len() + self.wd_by_path.len()
                        >= MAX_WALK_DIRECTORIES
                    {
                        failures.record(limit_error("watched directory limit reached"));
                        break 'walk;
                    }
                    subdirectories.push(entry.path);
                } else if let Some(found) = discovery.as_deref_mut() {
                    found.note(entry.path);
                }
            }
            enqueue(&mut queue, subdirectories, depth, share_root.is_some());
        }
        // The first met are the newest, so they go in last: the oldest watch is
        // the first to be evicted.
        if let Some(share_root) = share_root {
            self.recent_deepest.extend(
                registered_deepest
                    .into_iter()
                    .rev()
                    .map(|dir| (share_root.clone(), dir)),
            );
            self.evict_beyond_share(&share_root);
        }
        match failures.first {
            Some(first_error) => RootRegistration::Partial {
                failed_dirs: failures.directories,
                first_error,
            },
            None => RootRegistration::Complete,
        }
    }

    /// The entries of `dir`, highest names first for a bounded root. A
    /// directory that vanished yields nothing; one that cannot be listed
    /// yields nothing and is recorded in `failures`.
    fn list_for_walk(
        &self,
        dir: &Path,
        bounded: bool,
        failures: &mut Failures,
    ) -> Option<Box<dyn Iterator<Item = io::Result<ListedEntry>>>> {
        match (self.lister)(dir) {
            Ok(entries) if bounded => Some(newest_entry_first(entries)),
            Ok(entries) => Some(entries),
            Err(error) if is_absent_error(&error) => None,
            Err(error) => {
                failures.record(error);
                None
            }
        }
    }

    /// How many deepest-level directories of the dated tree at `root` hold a
    /// watch.
    fn watched_deepest(&self, root: &Path) -> usize {
        self.recent_deepest
            .iter()
            .filter(|(owner, _dir)| owner == root)
            .count()
    }

    /// Drops the oldest watches of the dated tree at `root` beyond
    /// [`MAX_WATCHED_DEEPEST_DIRECTORIES`].
    fn evict_beyond_share(&mut self, root: &Path) {
        while self.watched_deepest(root) > MAX_WATCHED_DEEPEST_DIRECTORIES {
            let Some(position) = self
                .recent_deepest
                .iter()
                .position(|(owner, _dir)| owner == root)
            else {
                break;
            };
            let Some((_owner, oldest)) = self.recent_deepest.remove(position) else {
                break;
            };
            if let Some(wd) = self.wd_by_path.remove(&oldest) {
                self.paths_by_wd.remove(&wd);
                inotify_rm_watch(self.fd.get_ref().as_raw_fd(), wd);
            }
        }
    }

    /// The bound of the root that holds `dir` with that root's path, and how
    /// many levels `dir` lies below it.
    fn bound_of(&self, dir: &Path) -> (Option<(PathBuf, WalkBound)>, u8) {
        self.walk_bounds
            .iter()
            .find_map(|(root, bound)| {
                let below = dir.strip_prefix(root).ok()?;
                let levels = u8::try_from(below.components().count()).unwrap_or(u8::MAX);
                Some((Some((root.clone(), *bound)), levels))
            })
            .unwrap_or((None, 0))
    }

    /// Watches `path`. Returns `false` when the directory is already watched
    /// under another path (a symlinked or aliased root): inotify hands out one
    /// descriptor per directory, so a second mapping would displace the first.
    fn add_dir(&mut self, path: &Path) -> io::Result<bool> {
        if self.wd_by_path.contains_key(path) {
            return Ok(true);
        }
        if self.wd_by_path.len() >= MAX_WALK_DIRECTORIES {
            return Err(limit_error("watched directory limit reached"));
        }
        let wd = (self.add_watch)(self.fd.get_ref().as_raw_fd(), path).map_err(|error| {
            if is_limit_error(&error) {
                limit_error("inotify watch limit reached; raise fs.inotify.max_user_watches")
            } else {
                error
            }
        })?;
        if self.paths_by_wd.contains_key(&wd) {
            return Ok(false);
        }
        self.paths_by_wd.insert(wd, path.to_path_buf());
        self.wd_by_path.insert(path.to_path_buf(), wd);
        Ok(true)
    }

    fn event_path(&self, event: &InotifyEvent) -> Option<PathBuf> {
        let base = self.paths_by_wd.get(&event.wd)?;
        Some(match &event.name {
            Some(name) => base.join(name),
            None => base.clone(),
        })
    }
}

impl WatchBackend for InotifyBackend {
    fn bound_walk(&mut self, root: &Path, bound: Option<WalkBound>) {
        match bound {
            Some(bound) => self.walk_bounds.insert(root.to_path_buf(), bound),
            None => self.walk_bounds.remove(root),
        };
    }

    fn register_root(&mut self, root: &Path, cancel: &CancellationToken) -> RootRegistration {
        match fs::metadata(root) {
            Ok(metadata) if metadata.is_dir() => self.register_tree(root, None, cancel),
            Ok(_) => RootRegistration::Missing,
            Err(error) if is_absent_error(&error) => RootRegistration::Missing,
            Err(error) => RootRegistration::Failed(error),
        }
    }

    fn watch_new_directory(
        &mut self,
        dir: &Path,
        out: &mut Vec<WatchEvent>,
        cancel: &CancellationToken,
    ) {
        let mut discovery = Discovery::default();
        let mut rescan = matches!(
            self.register_tree(dir, Some(&mut discovery), cancel),
            RootRegistration::Partial { .. } | RootRegistration::Failed(_)
        );
        rescan |= discovery.overflowed;
        out.extend(discovery.paths.into_iter().map(WatchEvent::Changed));
        if rescan {
            out.push(WatchEvent::Rescan);
        }
    }

    fn restart(&mut self) -> io::Result<()> {
        self.fd = AsyncFd::new(inotify_init()?)?;
        self.paths_by_wd.clear();
        self.wd_by_path.clear();
        self.recent_deepest.clear();
        Ok(())
    }

    fn unregister_all(&mut self) {
        let fd = self.fd.get_ref().as_raw_fd();
        self.wd_by_path.clear();
        self.recent_deepest.clear();
        // The mapping is cleared first so the `IN_IGNORED` records queued by the
        // removals below resolve to no path and are dropped.
        for (wd, _path) in self.paths_by_wd.drain() {
            inotify_rm_watch(fd, wd);
        }
    }

    async fn next_events(&mut self, out: &mut Vec<WatchEvent>) -> io::Result<()> {
        loop {
            let mut guard = self.fd.readable().await?;
            let result = guard.try_io(|inner| read_inotify_events(inner.get_ref().as_raw_fd()));
            drop(guard);
            match result {
                Ok(Ok(events)) => {
                    self.translate(events, out);
                    return Ok(());
                }
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(Err(error)) => return Err(error),
                Err(_would_block) => {}
            }
        }
    }
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
    // call. `fd` is the live inotify descriptor owned by `InotifyBackend`.
    let wd = unsafe { libc::inotify_add_watch(fd, c_path.as_ptr(), INOTIFY_WATCH_MASK) };
    if wd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(wd)
}

#[expect(unsafe_code, reason = "inotify requires Linux syscalls")]
fn inotify_rm_watch(fd: RawFd, wd: libc::c_int) {
    // SAFETY: `fd` is the live inotify descriptor owned by `InotifyBackend`. A
    // stale `wd` only makes the call fail with `EINVAL`, which is the desired
    // outcome for a watch the kernel already dropped.
    let _ = unsafe { libc::inotify_rm_watch(fd, wd) };
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

#[cfg(test)]
mod tests {
    use std::fs;

    use std::path::PathBuf;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    use super::{
        InotifyBackend, InotifyEvent, ListedEntry, WatchEvent, MAX_DISCOVERED_TRANSCRIPTS,
        MAX_WALK_DIRECTORIES,
    };
    use crate::external::watch::{RootRegistration, WatchBackend};

    fn event(wd: libc::c_int, mask: u32, name: Option<&str>) -> InotifyEvent {
        InotifyEvent {
            wd,
            mask,
            name: name.map(Into::into),
        }
    }

    #[tokio::test]
    async fn queue_overflow_becomes_a_rescan_hint() {
        let mut backend = InotifyBackend::open().expect("open inotify");
        let mut out = Vec::new();

        backend.translate(vec![event(-1, libc::IN_Q_OVERFLOW, None)], &mut out);

        assert_eq!(out, vec![WatchEvent::Rescan]);
    }

    #[tokio::test]
    async fn attribute_changes_are_file_hints_and_directory_tree_changes() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        assert!(matches!(
            backend.register_root(root.path(), &CancellationToken::new()),
            RootRegistration::Complete
        ));
        let root_wd = backend.wd_by_path[root.path()];
        let mut out = Vec::new();

        backend.translate(
            vec![
                event(root_wd, libc::IN_ATTRIB, Some("s.jsonl")),
                event(root_wd, libc::IN_ATTRIB | libc::IN_ISDIR, Some("sub")),
            ],
            &mut out,
        );

        assert_eq!(
            out,
            vec![
                WatchEvent::Changed(root.path().join("s.jsonl")),
                WatchEvent::TreeChanged(root.path().join("sub")),
            ]
        );
        assert_ne!(super::INOTIFY_WATCH_MASK & libc::IN_ATTRIB, 0);
    }

    #[tokio::test]
    async fn events_for_unknown_watches_are_dropped() {
        let mut backend = InotifyBackend::open().expect("open inotify");
        let mut out = Vec::new();

        backend.translate(
            vec![
                event(42, libc::IN_IGNORED, None),
                event(42, libc::IN_MODIFY, Some("a.jsonl")),
            ],
            &mut out,
        );

        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn watched_directory_loss_and_removals_are_reported() {
        let root = pohunek_test_support::tempdir().expect("root");
        let sub = root.path().join("sub");
        fs::create_dir(&sub).expect("sub");
        let mut backend = InotifyBackend::open().expect("open inotify");
        assert!(matches!(
            backend.register_root(root.path(), &CancellationToken::new()),
            RootRegistration::Complete
        ));
        let root_wd = backend.wd_by_path[root.path()];
        let sub_wd = backend.wd_by_path[&sub];
        let mut out = Vec::new();

        backend.translate(
            vec![
                event(sub_wd, libc::IN_DELETE_SELF, None),
                event(root_wd, libc::IN_DELETE | libc::IN_ISDIR, Some("sub")),
                event(root_wd, libc::IN_DELETE, Some("gone.jsonl")),
                event(root_wd, libc::IN_MOVED_FROM, Some("moved.jsonl")),
            ],
            &mut out,
        );

        assert_eq!(
            out,
            vec![
                WatchEvent::TreeChanged(sub.clone()),
                WatchEvent::TreeChanged(sub),
                WatchEvent::Changed(root.path().join("gone.jsonl")),
                WatchEvent::Changed(root.path().join("moved.jsonl")),
            ]
        );
    }

    #[tokio::test]
    async fn a_new_directory_is_watched_and_its_transcripts_are_reported() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        assert!(matches!(
            backend.register_root(root.path(), &CancellationToken::new()),
            RootRegistration::Complete
        ));
        let root_wd = backend.wd_by_path[root.path()];
        let moved_in = root.path().join("moved-in");
        fs::create_dir_all(moved_in.join("deep")).expect("tree");
        fs::write(moved_in.join("deep/s.jsonl"), b"{}\n").expect("transcript");
        fs::write(moved_in.join("notes.txt"), b"x").expect("other file");
        let mut out = Vec::new();

        backend.translate(
            vec![event(
                root_wd,
                libc::IN_MOVED_TO | libc::IN_ISDIR,
                Some("moved-in"),
            )],
            &mut out,
        );

        assert_eq!(out, vec![WatchEvent::NewDirectory(moved_in.clone())]);

        let mut found = Vec::new();
        backend.watch_new_directory(&moved_in, &mut found, &CancellationToken::new());

        assert_eq!(
            found,
            vec![WatchEvent::Changed(moved_in.join("deep/s.jsonl"))]
        );
        assert!(backend.wd_by_path.contains_key(&moved_in.join("deep")));
    }

    #[tokio::test]
    async fn entry_errors_during_the_walk_make_the_registration_partial() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        backend.lister = |_dir| {
            Ok(Box::new(
                vec![Err(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                ))]
                .into_iter(),
            ))
        };

        let outcome = backend.register_root(root.path(), &CancellationToken::new());

        assert!(
            matches!(outcome, RootRegistration::Partial { failed_dirs: 1, .. }),
            "unexpected outcome {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_listing_failure_makes_the_registration_partial() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        backend.lister = |_dir| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));

        let outcome = backend.register_root(root.path(), &CancellationToken::new());

        assert!(
            matches!(outcome, RootRegistration::Partial { failed_dirs: 1, .. }),
            "unexpected outcome {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_root_below_a_non_directory_is_missing_not_failed() {
        let base = pohunek_test_support::tempdir().expect("base");
        let file = base.path().join("file");
        fs::write(&file, b"x").expect("file");
        let mut backend = InotifyBackend::open().expect("open inotify");

        let outcome = backend.register_root(&file.join("projects"), &CancellationToken::new());

        assert!(
            matches!(outcome, RootRegistration::Missing),
            "unexpected outcome {outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_directory_taking_a_transcript_name_is_reported_as_a_file_change() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        assert!(matches!(
            backend.register_root(root.path(), &CancellationToken::new()),
            RootRegistration::Complete
        ));
        let root_wd = backend.wd_by_path[root.path()];
        fs::create_dir(root.path().join("s.jsonl")).expect("directory");
        let mut out = Vec::new();

        backend.translate(
            vec![event(
                root_wd,
                libc::IN_CREATE | libc::IN_ISDIR,
                Some("s.jsonl"),
            )],
            &mut out,
        );

        assert_eq!(
            out,
            vec![
                WatchEvent::NewDirectory(root.path().join("s.jsonl")),
                WatchEvent::Changed(root.path().join("s.jsonl"))
            ]
        );
    }

    #[tokio::test]
    async fn registration_stops_at_the_directory_limit_and_reports_it() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        for index in 0..MAX_WALK_DIRECTORIES {
            backend
                .wd_by_path
                .insert(root.path().join(format!("filler-{index}")), 0);
        }

        let outcome = backend.register_root(root.path(), &CancellationToken::new());

        assert!(
            matches!(outcome, RootRegistration::Failed(ref error) if error.kind() == std::io::ErrorKind::QuotaExceeded),
            "unexpected outcome {outcome:?}"
        );
    }

    #[tokio::test]
    async fn unregister_all_drops_every_watch_and_its_queued_ignore_events() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        assert!(matches!(
            backend.register_root(root.path(), &CancellationToken::new()),
            RootRegistration::Complete
        ));
        let wd = backend.wd_by_path[root.path()];

        backend.unregister_all();
        let mut out = Vec::new();
        backend.translate(vec![event(wd, libc::IN_IGNORED, None)], &mut out);

        assert!(backend.wd_by_path.is_empty());
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn discovery_is_bounded_and_overflow_yields_one_rescan() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        backend.lister = |_dir| {
            Ok(Box::new((0..MAX_DISCOVERED_TRANSCRIPTS + 500).map(
                |index| {
                    Ok(ListedEntry {
                        path: PathBuf::from(format!("/nowhere/{index}.jsonl")),
                        is_dir: false,
                    })
                },
            )))
        };
        let mut out = Vec::new();

        backend.watch_new_directory(root.path(), &mut out, &CancellationToken::new());

        let changed = out
            .iter()
            .filter(|event| matches!(event, WatchEvent::Changed(_)))
            .count();
        let rescans = out
            .iter()
            .filter(|event| matches!(event, WatchEvent::Rescan))
            .count();
        assert_eq!(changed, MAX_DISCOVERED_TRANSCRIPTS);
        assert_eq!(rescans, 1);
        assert_eq!(out.len(), MAX_DISCOVERED_TRANSCRIPTS + 1);
    }

    #[tokio::test]
    async fn an_endless_listing_ends_at_the_entry_budget() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        backend.lister = |_dir| {
            Ok(Box::new(std::iter::repeat_with(|| {
                Ok(ListedEntry {
                    path: PathBuf::from("/nowhere/file"),
                    is_dir: false,
                })
            })))
        };

        let outcome = backend.register_root(root.path(), &CancellationToken::new());

        assert!(
            matches!(outcome, RootRegistration::Partial { ref first_error, .. } if first_error.kind() == std::io::ErrorKind::QuotaExceeded),
            "unexpected outcome {outcome:?}"
        );
    }

    #[tokio::test]
    async fn an_endless_directory_listing_ends_at_the_directory_bound() {
        let root = pohunek_test_support::tempdir().expect("root");
        let mut backend = InotifyBackend::open().expect("open inotify");
        backend.lister = |_dir| {
            Ok(Box::new(std::iter::repeat_with(|| {
                Ok(ListedEntry {
                    path: PathBuf::from("/nowhere/dir"),
                    is_dir: true,
                })
            })))
        };

        let outcome = backend.register_root(root.path(), &CancellationToken::new());

        assert!(
            matches!(outcome, RootRegistration::Partial { ref first_error, .. } if first_error.kind() == std::io::ErrorKind::QuotaExceeded),
            "unexpected outcome {outcome:?}"
        );
    }

    static ADD_WATCH_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn add_watch_until_enospc(
        _fd: std::os::fd::RawFd,
        _path: &std::path::Path,
    ) -> std::io::Result<libc::c_int> {
        let call = ADD_WATCH_CALLS.fetch_add(1, Ordering::SeqCst);
        if call < 2 {
            Ok(libc::c_int::try_from(call + 1000).expect("small wd"))
        } else {
            Err(std::io::Error::from_raw_os_error(libc::ENOSPC))
        }
    }

    #[tokio::test]
    async fn kernel_watch_exhaustion_stops_the_walk() {
        let root = pohunek_test_support::tempdir().expect("root");
        for name in ["a", "b", "c", "d", "e"] {
            fs::create_dir(root.path().join(name)).expect("subdirectory");
        }
        let mut backend = InotifyBackend::open().expect("open inotify");
        backend.add_watch = add_watch_until_enospc;

        let outcome = backend.register_root(root.path(), &CancellationToken::new());

        assert!(
            matches!(outcome, RootRegistration::Partial { ref first_error, .. } if first_error.kind() == std::io::ErrorKind::QuotaExceeded),
            "unexpected outcome {outcome:?}"
        );
        assert_eq!(
            ADD_WATCH_CALLS.load(Ordering::SeqCst),
            3,
            "no add-watch attempt after the limit"
        );
    }

    #[tokio::test]
    async fn a_directory_watched_under_two_paths_keeps_the_first_mapping() {
        let base = pohunek_test_support::tempdir().expect("base");
        let real = base.path().join("real");
        fs::create_dir(&real).expect("real");
        let alias = base.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("alias");
        let mut backend = InotifyBackend::open().expect("open inotify");
        let cancel = CancellationToken::new();
        assert!(matches!(
            backend.register_root(&real, &cancel),
            RootRegistration::Complete
        ));
        let wd = backend.wd_by_path[&real];

        let outcome = backend.register_root(&alias, &cancel);

        assert!(matches!(outcome, RootRegistration::Complete), "{outcome:?}");
        assert_eq!(backend.paths_by_wd[&wd], real, "the first mapping stays");
        assert!(!backend.wd_by_path.contains_key(&alias));
        let mut out = Vec::new();
        backend.translate(vec![event(wd, libc::IN_MODIFY, Some("s.jsonl"))], &mut out);
        assert_eq!(out, vec![WatchEvent::Changed(real.join("s.jsonl"))]);
    }

    #[tokio::test]
    async fn a_cancelled_registration_walk_stops_and_reports_interruption() {
        let root = pohunek_test_support::tempdir().expect("root");
        fs::create_dir_all(root.path().join("a/b")).expect("tree");
        let mut backend = InotifyBackend::open().expect("open inotify");
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = backend.register_root(root.path(), &cancel);

        assert!(
            matches!(outcome, RootRegistration::Partial { ref first_error, .. } if first_error.kind() == std::io::ErrorKind::Interrupted),
            "unexpected outcome {outcome:?}"
        );
        assert!(
            backend.wd_by_path.is_empty(),
            "nothing is watched after cancel"
        );
    }

    #[tokio::test]
    async fn a_cancelled_new_directory_walk_reports_nothing_it_did_not_finish() {
        let root = pohunek_test_support::tempdir().expect("root");
        fs::write(root.path().join("s.jsonl"), b"{}").expect("transcript");
        let mut backend = InotifyBackend::open().expect("open inotify");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut out = Vec::new();

        backend.watch_new_directory(root.path(), &mut out, &cancel);

        assert_eq!(out, vec![WatchEvent::Rescan]);
    }
}
