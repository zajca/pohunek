//! Linux `/proc` and pidfd process inspection.

// Rust guideline compliant 2026-10-18

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_io::Async;
use rustix::process::{pidfd_open, Pid as NativePid, PidfdFlags};

use super::{
    BootIdentity, Error, ExitWatch, OwnershipMarkers, Pid, ProcessFact, ProcessIdentity,
    ProcessInspector, StartIdentity,
};

/// Linux proc filesystem root.
///
/// All process facts come from procfs because it is available to unprivileged
/// same-user processes and does not require kernel capabilities.
const PROC_ROOT: &str = "/proc";
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
const ENV_DAEMON_ID: &str = "POHUNEK_DAEMON_ID";
const ENV_SESSION_ID: &str = "POHUNEK_SESSION_ID";
const ENV_WORKER_ID: &str = "POHUNEK_WORKER_ID";
const ENV_WORKER_INSTANCE_ID: &str = "POHUNEK_WORKER_INSTANCE_ID";
/// `/proc/<pid>/stat` process-group field number (`pgrp`).
const STAT_PGRP_FIELD: usize = 5;
/// `/proc/<pid>/stat` parent-process field number (`ppid`).
const STAT_PPID_FIELD: usize = 4;
/// `/proc/<pid>/stat` process-state field number.
const STAT_STATE_FIELD: usize = 3;
/// `/proc/<pid>/stat` task-flags field number (`flags`).
const STAT_FLAGS_FIELD: usize = 9;
/// Linux `PF_KTHREAD`: a kernel thread cannot carry a userspace environment.
///
/// These tasks also have zero environment bounds, but unlike a userspace task
/// between exec images they can never inherit a worker's ownership markers.
const PF_KTHREAD: u32 = 0x0020_0000;
/// `/proc/<pid>/stat` controlling-terminal foreground group field number
/// (`tpgid`). The value is signed; `-1` means no controlling terminal.
const STAT_TPGID_FIELD: usize = 8;
/// `/proc/<pid>/stat` process start-time field number (`starttime`).
const STAT_STARTTIME_FIELD: usize = 22;
/// `/proc/<pid>/stat` environment-region start field number (`env_start`).
///
/// The bounds name the region `/proc/<pid>/environ` reads from. Both fields
/// exist since Linux 3.5 (2012), and the inspector already requires a newer
/// kernel for pidfds, so every host this runs on reports them.
const STAT_ENV_START_FIELD: usize = 50;
/// `/proc/<pid>/stat` environment-region end field number (`env_end`).
const STAT_ENV_END_FIELD: usize = 51;
/// Upper bound on consecutive `/proc/<pid>/stat` reads while the kernel reports
/// placeholder fields (`ppid` 0 with `pgrp` -1) for a task being forked or torn
/// down. The window closes within microseconds, so a few reads settle it; a
/// higher value only delays failing closed on a task that never settles.
const STAT_TRANSIENT_MAX_READS: usize = 4;
/// First `/proc/<pid>/stat` field that appears after the parenthesized command.
///
/// Kernel numbering starts at field 1 (`pid`), but the parser removes fields 1–2
/// with the command, leaving field 3 (`state`) at offset zero.
const FIRST_FIELD_AFTER_COMMAND: usize = 3;

/// Linux process inspector backed by procfs and pidfds.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxInspector;

impl LinuxInspector {
    /// Creates a Linux process inspector.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Returns the current opaque Linux boot identity.
    ///
    /// # Errors
    ///
    /// Returns typed boot-identity inspection failures.
    pub fn boot_identity(&self) -> Result<BootIdentity, Error> {
        let value = fs::read_to_string(BOOT_ID_PATH)
            .map_err(|source| facility_error("read_boot_identity", source))?;
        BootIdentity::parse(value.trim())
    }

    /// Returns whether a process belongs to a cgroup or one of its subgroups.
    ///
    /// The check reads only `/proc/<pid>/cgroup`. Callers that require a stable
    /// binding must compare process identities before and after this operation.
    ///
    /// # Errors
    ///
    /// Returns typed failures when cgroup membership cannot be inspected.
    pub fn is_in_control_group(&self, pid: Pid, control_group: &str) -> Result<bool, Error> {
        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        read_control_group_membership_at(Path::new(PROC_ROOT), pid, euid, control_group)
            .map_err(|source| process_error("inspect_control_group", source))
    }
}

impl ProcessInspector for LinuxInspector {
    fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, Error> {
        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        read_process_identity_at(Path::new(PROC_ROOT), pid, euid)
            .map_err(|source| process_error("inspect_process_identity", source))
    }

    fn is_running(&self, identity: ProcessIdentity) -> Result<bool, Error> {
        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        process_is_running_at(Path::new(PROC_ROOT), identity, euid)
            .map_err(|source| process_error("inspect_process_liveness", source))
    }

    fn process_age(&self, identity: ProcessIdentity) -> Result<Option<Duration>, Error> {
        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        let running = process_is_running_at(Path::new(PROC_ROOT), identity, euid)
            .map_err(|source| process_error("inspect_process_age", source))?;
        if !running {
            return Ok(None);
        }
        Ok(Some(age_from_ticks(
            identity.start_identity.get(),
            boot_ticks_now(),
            rustix::param::clock_ticks_per_second(),
        )))
    }

    fn parent_pid(&self, pid: Pid) -> Result<Option<Pid>, Error> {
        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        read_process_parent_at(Path::new(PROC_ROOT), pid, euid)
            .map_err(|source| process_error("inspect_process_parent", source))
    }

    fn process(&self, pid: Pid) -> Result<Option<ProcessFact>, Error> {
        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        read_process_fact(pid, euid).map_err(|source| process_error("inspect_process", source))
    }

    fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error> {
        same_user_processes().map_err(|source| Error::from_io("inspect_process_table", source))
    }

    fn descendants(&self, root: Pid) -> Result<Vec<ProcessFact>, Error> {
        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        let pids = match descendants_from_children(root, euid)
            .map_err(|source| Error::from_io("inspect_descendants", source))?
        {
            Some(pids) => pids,
            None => descendants_from_ppid_scan(root, euid)
                .map_err(|source| Error::from_io("inspect_descendants", source))?,
        };
        let mut facts = Vec::with_capacity(pids.len());
        for pid in pids {
            if let Some(fact) = read_process_fact(pid, euid)
                .map_err(|source| Error::from_io("inspect_descendant", source))?
            {
                facts.push(fact);
            }
        }
        Ok(facts)
    }

    fn descendant_identities(&self, root: ProcessIdentity) -> Result<Vec<ProcessIdentity>, Error> {
        const OPERATION: &str = "inspect_descendant_identities";

        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        require_identity(root, euid, OPERATION)?;
        let pids = match descendants_from_children(root.pid, euid)
            .map_err(|source| process_error(OPERATION, source))?
        {
            Some(pids) => pids,
            None => descendants_from_ppid_scan(root.pid, euid)
                .map_err(|source| process_error(OPERATION, source))?,
        };
        let identities = collect_live_identities(&pids, |pid| {
            read_process_identity_at(Path::new(PROC_ROOT), pid, euid)
                .map_err(|source| process_error(OPERATION, source))
        })?;
        require_identity(root, euid, OPERATION)?;
        Ok(identities)
    }

    fn cwd(&self, pid: Pid) -> Result<PathBuf, Error> {
        fs::read_link(proc_path(pid).join("cwd"))
            .map_err(|source| process_error("read_cwd", source))
    }

    fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, Error> {
        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        if !same_user(pid, euid) {
            return Ok(None);
        }
        match fs::read_link(proc_path(pid).join("exe")) {
            Ok(path) => Ok(Some(path)),
            Err(error) if is_process_race(&error) => Ok(None),
            Err(source) => Err(Error::from_io("read_executable", source)),
        }
    }

    fn exit_watch(&self, identity: ProcessIdentity) -> Result<ExitWatch, Error> {
        const OPERATION: &str = "open_exit_watch";

        let euid = current_euid().map_err(|source| Error::from_io("read_effective_uid", source))?;
        require_identity(identity, euid, OPERATION)?;
        let native = NativePid::from_raw(i32::try_from(identity.pid).map_err(|_range_error| {
            Error::OutOfRange {
                operation: OPERATION,
            }
        })?)
        .ok_or(Error::OutOfRange {
            operation: OPERATION,
        })?;
        let fd = pidfd_open(native, PidfdFlags::empty())
            .map_err(|source| process_error(OPERATION, source.into()))?;
        require_identity(identity, euid, OPERATION)?;
        let fd = Async::new_nonblocking(fd)
            .map_err(|source| Error::from_io("register_exit_watch", source))?;
        Ok(ExitWatch::from_future(async move {
            fd.readable()
                .await
                .map_err(|source| Error::from_io("wait_for_exit", source))?;
            Ok(())
        }))
    }

    fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, Error> {
        ownership_markers(pid)
    }

    fn foreground_process_group(&self, root_pid: Pid) -> Result<Option<Pid>, Error> {
        foreground_process_group(root_pid)
            .map_err(|source| process_error("read_foreground_process_group", source))
    }
}

fn require_identity(
    expected: ProcessIdentity,
    euid: u32,
    operation: &'static str,
) -> Result<(), Error> {
    let actual = read_process_identity_at(Path::new(PROC_ROOT), expected.pid, euid)
        .map_err(|source| process_error(operation, source))?;
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(Error::Race { operation })
    }
}

/// Reads the terminal foreground group without signaling the target process.
fn foreground_process_group(root_pid: Pid) -> io::Result<Option<Pid>> {
    let euid = current_euid()?;
    if !same_user(root_pid, euid) {
        return Ok(None);
    }
    read_stat_process_group(root_pid)
}

fn read_stat_process_group(process_id: Pid) -> io::Result<Option<Pid>> {
    read_stat_pid_field(process_id, STAT_TPGID_FIELD)
}

fn parse_stat_pid_field(stat: &str, field_number: usize) -> Option<Pid> {
    parse_stat_field::<i32>(stat, field_number).and_then(|value| Pid::try_from(value).ok())
}

fn parse_stat_field<T>(stat: &str, field_number: usize) -> Option<T>
where
    T: std::str::FromStr,
{
    let command_end = stat.rfind(')')?;
    let fields = stat.get(command_end + 1..)?;
    fields
        .split_whitespace()
        .nth(field_number.checked_sub(FIRST_FIELD_AFTER_COMMAND)?)
        .and_then(|value| value.parse().ok())
}

/// Reads one process-id `/proc/<pid>/stat` field by its kernel-defined number.
/// A malformed, negative, or unparsable value yields `None`.
fn read_stat_pid_field(process_id: Pid, field_number: usize) -> io::Result<Option<Pid>> {
    let stat = match fs::read_to_string(proc_path(process_id).join("stat")) {
        Ok(stat) => stat,
        Err(err) if is_process_race(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(parse_stat_pid_field(&stat, field_number))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StatFact {
    comm: String,
    state: char,
    parent_id: Pid,
    pgid: Pid,
    start_identity: StartIdentity,
}

fn read_process_stat(process_id: Pid) -> io::Result<Option<StatFact>> {
    read_process_stat_at(Path::new(PROC_ROOT), process_id)
}

fn process_is_running_at(root: &Path, identity: ProcessIdentity, euid: u32) -> io::Result<bool> {
    if checked_same_user_at(root, identity.pid, euid)? != Some(true) {
        return Ok(false);
    }
    let Some(stat) = read_process_stat_at(root, identity.pid)? else {
        return Ok(false);
    };
    Ok(stat.start_identity == identity.start_identity && !matches!(stat.state, 'Z' | 'X' | 'x'))
}

fn read_process_stat_at(root: &Path, process_id: Pid) -> io::Result<Option<StatFact>> {
    let stat_path = proc_path_at(root, process_id).join("stat");
    read_settled_stat(|| fs::read_to_string(&stat_path))
}

/// Result of parsing one `/proc/<pid>/stat` sample.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StatSample {
    Settled(StatFact),
    /// The kernel's placeholder values for a task that is being created or
    /// torn down.
    Transient,
}

/// Reads a stat sample, resampling while the kernel reports placeholder values.
///
/// A vanished process is a race (`Ok(None)`). A sample that stays transient
/// through [`STAT_TRANSIENT_MAX_READS`] reads fails closed with `InvalidData`,
/// and a malformed sample fails immediately.
fn read_settled_stat(
    mut read_stat: impl FnMut() -> io::Result<String>,
) -> io::Result<Option<StatFact>> {
    for _ in 0..STAT_TRANSIENT_MAX_READS {
        let stat = match read_stat() {
            Ok(stat) => stat,
            Err(err) if is_process_race(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        match parse_stat_sample(&stat)? {
            StatSample::Settled(fact) => return Ok(Some(fact)),
            StatSample::Transient => {}
        }
    }
    Err(invalid_process_stat_error())
}

fn parse_stat_sample(stat: &str) -> io::Result<StatSample> {
    let command_start = stat.find('(').ok_or_else(invalid_process_stat_error)?;
    let command_end = stat.rfind(')').ok_or_else(invalid_process_stat_error)?;
    let comm = stat
        .get(command_start + 1..command_end)
        .ok_or_else(invalid_process_stat_error)?
        .to_owned();
    let state = parse_stat_field(stat, STAT_STATE_FIELD).ok_or_else(invalid_process_stat_error)?;
    let raw_parent_id: i32 =
        parse_stat_field(stat, STAT_PPID_FIELD).ok_or_else(invalid_process_stat_error)?;
    let raw_pgid: i32 =
        parse_stat_field(stat, STAT_PGRP_FIELD).ok_or_else(invalid_process_stat_error)?;
    if raw_parent_id == 0 && raw_pgid == -1 {
        return Ok(StatSample::Transient);
    }
    let parent_id = Pid::try_from(raw_parent_id)
        .ok()
        .ok_or_else(invalid_process_stat_error)?;
    let pgid = Pid::try_from(raw_pgid)
        .ok()
        .ok_or_else(invalid_process_stat_error)?;
    let start_identity =
        parse_stat_field(stat, STAT_STARTTIME_FIELD).ok_or_else(invalid_process_stat_error)?;
    Ok(StatSample::Settled(StatFact {
        comm,
        state,
        parent_id,
        pgid,
        start_identity: StartIdentity::new(start_identity),
    }))
}

fn read_process_identity_at(
    root: &Path,
    process_id: Pid,
    euid: u32,
) -> io::Result<Option<ProcessIdentity>> {
    if checked_same_user_at(root, process_id, euid)? != Some(true) {
        return Ok(None);
    }
    Ok(
        read_process_stat_at(root, process_id)?.map(|stat| ProcessIdentity {
            pid: process_id,
            start_identity: stat.start_identity,
        }),
    )
}

fn collect_live_identities(
    process_ids: &[Pid],
    mut inspect: impl FnMut(Pid) -> Result<Option<ProcessIdentity>, Error>,
) -> Result<Vec<ProcessIdentity>, Error> {
    let mut identities = Vec::with_capacity(process_ids.len());
    for &process_id in process_ids {
        match inspect(process_id) {
            Ok(Some(identity)) => identities.push(identity),
            Ok(None) | Err(Error::Race { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(identities)
}

fn read_process_parent_at(root: &Path, process_id: Pid, euid: u32) -> io::Result<Option<Pid>> {
    if checked_same_user_at(root, process_id, euid)? != Some(true) {
        return Ok(None);
    }
    Ok(read_process_stat_at(root, process_id)?.map(|stat| stat.parent_id))
}

fn read_control_group_membership_at(
    root: &Path,
    process_id: Pid,
    euid: u32,
    expected: &str,
) -> io::Result<bool> {
    if expected.is_empty() || checked_same_user_at(root, process_id, euid)? != Some(true) {
        return Ok(false);
    }
    let cgroups = fs::read_to_string(proc_path_at(root, process_id).join("cgroup"))?;
    Ok(cgroups.lines().any(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next();
        let controllers = fields.next();
        let path = fields.next();
        let is_systemd_hierarchy = matches!((hierarchy, controllers), (Some("0"), Some("")))
            || controllers.is_some_and(|value| value.split(',').any(|name| name == "name=systemd"));
        is_systemd_hierarchy && path.is_some_and(|path| cgroup_path_matches(path, expected))
    }))
}

fn cgroup_path_matches(actual: &str, expected: &str) -> bool {
    (expected == "/" && actual.starts_with('/'))
        || actual == expected
        || actual
            .strip_prefix(expected)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// Stable operation label of the ownership-marker read.
const OWNERSHIP_MARKERS_OPERATION: &str = "read_ownership_markers";

/// Reads the allowlisted ownership markers from `/proc/<pid>/environ`.
///
/// `environ` holds the environment the process was `execve`d with, which is
/// exactly where a pohunek daemon's PTY markers live — they are injected before
/// exec and inherited by every child. A zero-byte read is ambiguous — the
/// kernel also reports end-of-file for a task without a memory map, and while
/// a freshly exec'd image has not published its environment region yet — so
/// the `/proc/<pid>/stat` region bounds decide it (see
/// [`ownership_markers_from`]).
fn ownership_markers(pid: Pid) -> Result<OwnershipMarkers, Error> {
    ownership_markers_from(
        || fs::read(proc_path(pid).join("environ")),
        || fs::read_to_string(proc_path(pid).join("stat")),
        OWNERSHIP_MARKERS_OPERATION,
    )
}

/// Upper bound on rereads of `/proc/<pid>/environ` after a settled nonempty
/// environment region contradicts an empty read.
///
/// The contradiction means the image finished installing its environment
/// between the two observations, so one reread usually recovers the markers;
/// a second covers a process that re-executed into the window again. The
/// reads never wait, so the bound only caps the work before failing closed.
const ENVIRON_REREADS: usize = 2;

/// Resolves one marker answer from raw `environ` and `stat` observations.
///
/// The `read_environ` observation yields the raw NUL-separated environment;
/// `read_stat` yields raw `/proc/<pid>/stat` text. A nonempty environment
/// parses directly. An empty one is
/// ambiguous, because the kernel reports end-of-file both for a task without a
/// memory map and while a freshly exec'd image has not published its
/// environment region, so the stat sample decides:
///
/// - an ended or vanished task has nothing left to reap and reads as unmarked;
/// - a kernel thread (`PF_KTHREAD`) has no userspace markers and reads as unmarked;
/// - a live task with a published zero-length region (`env_start` equals
///   `env_end`, both nonzero) genuinely has no environment and reads as
///   unmarked;
/// - every other live task is [`Error::Unobservable`] — the region is not
///   published yet, the region is reversed or half-published, or a published
///   nonempty region contradicts the empty read. The contradiction means the
///   image finished installing its markers between the two observations, so
///   the environment is reread a bounded number of times first; exhaustion
///   stays unobservable rather than reporting an unreadable environment as
///   unmarked.
fn ownership_markers_from(
    mut read_environ: impl FnMut() -> io::Result<Vec<u8>>,
    mut read_stat: impl FnMut() -> io::Result<String>,
    operation: &'static str,
) -> Result<OwnershipMarkers, Error> {
    match read_environ() {
        Ok(bytes) if !bytes.is_empty() => return Ok(parse_ownership_markers(&bytes)),
        // A task that vanished before the read has nothing left to reap,
        // which reads as unmarked exactly like the race it replaced.
        Ok(_) => {}
        Err(source) if is_process_race(&source) => return Ok(OwnershipMarkers::default()),
        Err(source) => return Err(process_error(operation, source)),
    }
    let stat = match read_stat() {
        Ok(stat) => stat,
        // The task vanished after the empty read: unmarked, as above.
        Err(source) if is_process_race(&source) => return Ok(OwnershipMarkers::default()),
        Err(source) => return Err(process_error(operation, source)),
    };
    let state: char =
        parse_stat_field(&stat, STAT_STATE_FIELD).ok_or(Error::InvalidData { operation })?;
    if matches!(state, 'Z' | 'X' | 'x') {
        // An ended task loses its memory map, so its environment reads as
        // empty for the same reason a vanished one does.
        return Ok(OwnershipMarkers::default());
    }
    let flags: u32 =
        parse_stat_field(&stat, STAT_FLAGS_FIELD).ok_or(Error::InvalidData { operation })?;
    if flags & PF_KTHREAD != 0 {
        return Ok(OwnershipMarkers::default());
    }
    let env_start: u64 =
        parse_stat_field(&stat, STAT_ENV_START_FIELD).ok_or(Error::InvalidData { operation })?;
    let env_end: u64 =
        parse_stat_field(&stat, STAT_ENV_END_FIELD).ok_or(Error::InvalidData { operation })?;
    if env_start == env_end {
        // A published zero-length region records an empty environment.
        if env_start != 0 {
            return Ok(OwnershipMarkers::default());
        }
        // The region is not published yet: the task may still be inside the
        // exec that is about to install its markers.
        return Err(Error::Unobservable { operation });
    }
    if env_start > env_end || env_start == 0 || env_end == 0 {
        // The kernel never reports a reversed or half-published region, so
        // the observation is ambiguous and fails closed.
        return Err(Error::Unobservable { operation });
    }
    // A published nonempty region contradicts the empty read: the image
    // finished installing its environment between the two observations. The
    // markers are usually readable by now.
    for _ in 0..ENVIRON_REREADS {
        match read_environ() {
            Ok(bytes) if !bytes.is_empty() => return Ok(parse_ownership_markers(&bytes)),
            Err(source) if is_process_race(&source) => return Ok(OwnershipMarkers::default()),
            Err(source) => return Err(process_error(operation, source)),
            Ok(_) => {}
        }
    }
    Err(Error::Unobservable { operation })
}

/// Extracts the allowlisted markers from NUL-separated `KEY=VALUE` entries.
///
/// No other entry is retained. The first entry for a key wins, matching what
/// `getenv` reports to the process itself. Values are decoded lossily because
/// marker ids are ASCII.
fn parse_ownership_markers(environ: &[u8]) -> OwnershipMarkers {
    let mut markers = OwnershipMarkers::default();
    for entry in environ.split(|byte| *byte == 0) {
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (key, rest) = entry.split_at(separator);
        let slot = if key == ENV_DAEMON_ID.as_bytes() {
            &mut markers.daemon_id
        } else if key == ENV_SESSION_ID.as_bytes() {
            &mut markers.session_id
        } else if key == ENV_WORKER_ID.as_bytes() {
            &mut markers.worker_id
        } else if key == ENV_WORKER_INSTANCE_ID.as_bytes() {
            &mut markers.worker_instance_id
        } else {
            continue;
        };
        if slot.is_none() {
            *slot = Some(String::from_utf8_lossy(&rest[1..]).into_owned());
        }
        if markers.daemon_id.is_some()
            && markers.session_id.is_some()
            && markers.worker_id.is_some()
            && markers.worker_instance_id.is_some()
        {
            break;
        }
    }
    markers
}

fn same_user_processes() -> io::Result<Vec<ProcessFact>> {
    let euid = current_euid()?;
    let mut facts = Vec::new();
    for entry in fs::read_dir(PROC_ROOT)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if is_process_race(&err) => continue,
            Err(err) => return Err(err),
        };
        let Some(process_id) = parse_pid(entry.file_name().as_ref()) else {
            continue;
        };
        if let Some(fact) = read_process_fact(process_id, euid)? {
            facts.push(fact);
        }
    }
    Ok(facts)
}

fn descendants_from_children(root: Pid, euid: u32) -> io::Result<Option<Vec<Pid>>> {
    let mut descendants = Vec::new();
    let mut queue = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    let mut saw_children_file = false;

    while let Some(parent) = queue.pop_front() {
        let task_dir = proc_path(parent).join("task");
        let tasks = match fs::read_dir(&task_dir) {
            Ok(tasks) => tasks,
            Err(err) if is_process_race(&err) => continue,
            Err(err) => return Err(err),
        };

        for task in tasks {
            let task = match task {
                Ok(task) => task,
                Err(err) if is_process_race(&err) => continue,
                Err(err) => return Err(err),
            };
            let Some(tid) = parse_pid(task.file_name().as_ref()) else {
                continue;
            };
            let children_path = proc_path(parent)
                .join("task")
                .join(tid.to_string())
                .join("children");
            let children = match fs::read_to_string(children_path) {
                Ok(children) => children,
                Err(err) if is_process_race(&err) => continue,
                Err(err) => return Err(err),
            };
            saw_children_file = true;

            for child in children.split_whitespace().filter_map(parse_pid_str) {
                if seen.insert(child) && same_user(child, euid) {
                    descendants.push(child);
                    queue.push_back(child);
                }
            }
        }
    }

    if saw_children_file {
        Ok(Some(descendants))
    } else {
        Ok(None)
    }
}

fn descendants_from_ppid_scan(root: Pid, euid: u32) -> io::Result<Vec<Pid>> {
    let mut children_by_parent: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for entry in fs::read_dir(PROC_ROOT)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) if is_process_race(&err) => continue,
            Err(err) => return Err(err),
        };
        let Some(process_id) = parse_pid(entry.file_name().as_ref()) else {
            continue;
        };
        if !same_user(process_id, euid) {
            continue;
        }
        let Some((_, parent_id)) = read_status(process_id)? else {
            continue;
        };
        children_by_parent
            .entry(parent_id)
            .or_default()
            .push(process_id);
    }

    let mut descendants = Vec::new();
    let mut queue = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    while let Some(parent) = queue.pop_front() {
        let Some(children) = children_by_parent.get(&parent) else {
            continue;
        };
        for &child in children {
            if seen.insert(child) {
                descendants.push(child);
                queue.push_back(child);
            }
        }
    }
    Ok(descendants)
}

fn read_process_fact(process_id: Pid, euid: u32) -> io::Result<Option<ProcessFact>> {
    if checked_same_user_at(Path::new(PROC_ROOT), process_id, euid)? != Some(true) {
        return Ok(None);
    }
    let fact = sample_process_fact(
        process_id,
        || read_process_stat(process_id),
        || read_cmdline(process_id),
    )?;
    if fact.is_none() || checked_same_user_at(Path::new(PROC_ROOT), process_id, euid)? != Some(true)
    {
        return Ok(None);
    }
    Ok(fact)
}

fn sample_process_fact(
    process_id: Pid,
    mut read_stat: impl FnMut() -> io::Result<Option<StatFact>>,
    read_cmdline: impl FnOnce() -> io::Result<Vec<String>>,
) -> io::Result<Option<ProcessFact>> {
    let Some(initial) = read_stat()? else {
        return Ok(None);
    };
    let cmdline = read_cmdline()?;
    let Some(final_stat) = read_stat()? else {
        return Ok(None);
    };
    if final_stat.start_identity != initial.start_identity {
        return Ok(None);
    }
    Ok(Some(ProcessFact {
        pid: process_id,
        pgid: initial.pgid,
        ppid: initial.parent_id,
        start_identity: initial.start_identity,
        comm: initial.comm,
        cmdline,
    }))
}

fn read_status(process_id: Pid) -> io::Result<Option<(String, Pid)>> {
    let status = match fs::read_to_string(proc_path(process_id).join("status")) {
        Ok(status) => status,
        Err(err) if is_process_race(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut comm = None;
    let mut parent_id = None;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("Name:") {
            comm = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("PPid:") {
            parent_id = value.trim().parse::<Pid>().ok();
        }
        if comm.is_some() && parent_id.is_some() {
            break;
        }
    }
    Ok(comm.zip(parent_id))
}

fn read_cmdline(pid: Pid) -> io::Result<Vec<String>> {
    let bytes = match fs::read(proc_path(pid).join("cmdline")) {
        Ok(bytes) => bytes,
        Err(err) if is_process_race(&err) => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect())
}

/// Clock ticks elapsed on the boot clock, the clock `/proc/<pid>/stat` start
/// times count in.
fn boot_ticks_now() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanoseconds = u64::try_from(now.tv_nsec).unwrap_or(0);
    let ticks_per_second = rustix::param::clock_ticks_per_second();
    seconds
        .saturating_mul(ticks_per_second)
        .saturating_add(nanoseconds.saturating_mul(ticks_per_second) / 1_000_000_000)
}

/// Age of a process that started at `start_ticks` when the boot clock reads
/// `now_ticks`, at `ticks_per_second` ticks a second.
fn age_from_ticks(start_ticks: u64, now_ticks: u64, ticks_per_second: u64) -> Duration {
    let elapsed = now_ticks.saturating_sub(start_ticks);
    Duration::from_secs(elapsed / ticks_per_second.max(1))
        + Duration::from_nanos(
            (elapsed % ticks_per_second.max(1)) * 1_000_000_000 / ticks_per_second.max(1),
        )
}

fn current_euid() -> io::Result<u32> {
    Ok(fs::metadata(proc_path(std::process::id()))?.uid())
}

fn same_user(pid: Pid, euid: u32) -> bool {
    same_user_at(Path::new(PROC_ROOT), pid, euid)
}

fn same_user_at(root: &Path, pid: Pid, euid: u32) -> bool {
    fs::metadata(proc_path_at(root, pid)).is_ok_and(|metadata| metadata.uid() == euid)
}

fn checked_same_user_at(root: &Path, pid: Pid, euid: u32) -> io::Result<Option<bool>> {
    match fs::metadata(proc_path_at(root, pid)) {
        Ok(metadata) => Ok(Some(metadata.uid() == euid)),
        Err(error) if is_process_race(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn invalid_process_stat_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "process stat is missing a required numeric field",
    )
}

fn proc_path(pid: Pid) -> PathBuf {
    proc_path_at(Path::new(PROC_ROOT), pid)
}

fn proc_path_at(root: &Path, pid: Pid) -> PathBuf {
    root.join(pid.to_string())
}

fn parse_pid(value: &OsStr) -> Option<Pid> {
    value.to_str().and_then(parse_pid_str)
}

fn parse_pid_str(value: &str) -> Option<Pid> {
    value.parse::<Pid>().ok()
}

fn is_process_race(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound
        || err.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32)
}

fn process_error(operation: &'static str, source: io::Error) -> Error {
    if is_process_race(&source) {
        Error::Race { operation }
    } else {
        Error::from_io(operation, source)
    }
}

fn facility_error(operation: &'static str, source: io::Error) -> Error {
    if source.kind() == io::ErrorKind::NotFound {
        Error::Unavailable { operation }
    } else {
        Error::from_io(operation, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_age_counts_boot_clock_ticks_since_its_start() {
        assert_eq!(age_from_ticks(100, 100, 100), Duration::ZERO);
        assert_eq!(
            age_from_ticks(100, 1_050, 100),
            Duration::from_millis(9_500)
        );
        assert_eq!(age_from_ticks(500, 100, 100), Duration::ZERO);
    }

    #[test]
    fn the_age_of_the_running_test_process_is_known_and_a_substituted_identity_has_none() {
        let inspector = LinuxInspector;
        let me = inspector
            .identity(std::process::id())
            .expect("identity read")
            .expect("the test process exists");

        let age = inspector.process_age(me).expect("age read");

        assert!(
            age.is_some_and(|age| age < Duration::from_hours(24)),
            "{age:?}"
        );
        let substituted = ProcessIdentity {
            start_identity: StartIdentity::new(me.start_identity.get() + 1),
            ..me
        };
        assert_eq!(inspector.process_age(substituted).expect("age read"), None);
    }

    #[test]
    fn stat_fields_are_counted_after_the_command_name() {
        for (case, stat, field, expected) in [
            (
                "tpgid after a command with spaces",
                "123 (agent with spaces) S 1 2 3 456 789 8",
                STAT_TPGID_FIELD,
                Some(789),
            ),
            (
                "pgrp after a command with spaces",
                "123 (agent with spaces) S 1 456 3 4 789 8",
                STAT_PGRP_FIELD,
                Some(456),
            ),
            (
                "tpgid after an escaped parenthesis in the command",
                "123 (escaped \\() name) S 1 2 3 456 789",
                STAT_TPGID_FIELD,
                Some(789),
            ),
            (
                "a field before the state",
                "123 (agent) S 1 2 3 4 5",
                2,
                None,
            ),
            (
                "tpgid -1 without a controlling terminal",
                "123 (agent) S 1 2 3 456 -1",
                STAT_TPGID_FIELD,
                None,
            ),
            (
                "a truncated stat after an escaped parenthesis",
                "123 (agent with \\() parenthesis) S 1 2 3",
                STAT_TPGID_FIELD,
                None,
            ),
        ] {
            assert_eq!(parse_stat_pid_field(stat, field), expected, "{case}");
        }
        assert_eq!(
            parse_stat_field::<u64>(
                "123 (agent) S 1 456 3 4 789 0 0 0 0 0 0 0 0 0 20 0 1 0 987654",
                STAT_STARTTIME_FIELD,
            ),
            Some(987_654),
            "starttime"
        );
    }

    #[test]
    fn process_fact_sampling_discards_a_reused_pid_after_metadata_reads() {
        let reads = std::cell::RefCell::new(Vec::new());
        let stats = std::cell::RefCell::new(VecDeque::from([
            StatFact {
                comm: "old-agent".to_owned(),
                state: 'S',
                parent_id: 7,
                pgid: 456,
                start_identity: StartIdentity::new(987_654),
            },
            StatFact {
                comm: "new-agent".to_owned(),
                state: 'S',
                parent_id: 8,
                pgid: 457,
                start_identity: StartIdentity::new(987_655),
            },
        ]));

        let fact = sample_process_fact(
            123,
            || {
                reads.borrow_mut().push("stat");
                Ok(stats.borrow_mut().pop_front())
            },
            || {
                reads.borrow_mut().push("cmdline");
                Ok(vec!["new-agent".to_owned()])
            },
        )
        .expect("process fact sampling");

        assert_eq!(fact, None);
        assert_eq!(*reads.borrow(), ["stat", "cmdline", "stat"]);
    }

    #[test]
    fn process_fact_sampling_uses_one_stable_stat_generation() {
        let stat = StatFact {
            comm: "agent".to_owned(),
            state: 'S',
            parent_id: 7,
            pgid: 456,
            start_identity: StartIdentity::new(987_654),
        };
        let stats = std::cell::RefCell::new(VecDeque::from([stat.clone(), stat]));

        assert_eq!(
            sample_process_fact(
                123,
                || Ok(stats.borrow_mut().pop_front()),
                || Ok(vec!["agent".to_owned(), "--resume".to_owned()]),
            )
            .expect("process fact sampling"),
            Some(ProcessFact {
                pid: 123,
                pgid: 456,
                ppid: 7,
                start_identity: StartIdentity::new(987_654),
                comm: "agent".to_owned(),
                cmdline: vec!["agent".to_owned(), "--resume".to_owned()],
            })
        );
    }

    #[test]
    fn narrow_identity_and_parent_reads_ignore_unreadable_cmdline() {
        let root = pohunek_test_support::tempdir().expect("temporary proc root");
        let process_id = 123;
        let process_dir = root.path().join(process_id.to_string());
        fs::create_dir(&process_dir).expect("process directory");
        fs::write(
            process_dir.join("stat"),
            concat!(
                "123 (agent) S 7 456 3 4 789 0 0 0 0 0 0 0 0 0 ",
                "20 0 1 0 987654"
            ),
        )
        .expect("process stat");
        fs::create_dir(process_dir.join("cmdline")).expect("unreadable cmdline stand-in");
        let euid = fs::metadata(&process_dir).expect("process metadata").uid();

        assert_eq!(
            read_process_identity_at(root.path(), process_id, euid).expect("identity inspection"),
            Some(ProcessIdentity {
                pid: process_id,
                start_identity: StartIdentity::new(987_654),
            })
        );
        assert_eq!(
            read_process_parent_at(root.path(), process_id, euid).expect("parent inspection"),
            Some(7)
        );
    }

    #[test]
    fn exact_liveness_rejects_zombies_and_reused_identities() {
        let root = pohunek_test_support::tempdir().expect("temporary proc root");
        let process_id = 123;
        let process_dir = root.path().join(process_id.to_string());
        fs::create_dir(&process_dir).expect("process directory");
        let euid = fs::metadata(&process_dir).expect("process metadata").uid();
        let identity = ProcessIdentity {
            pid: process_id,
            start_identity: StartIdentity::new(987_654),
        };
        let write_state = |state| {
            fs::write(
                process_dir.join("stat"),
                format!("123 (agent) {state} 7 456 3 4 789 0 0 0 0 0 0 0 0 0 20 0 1 0 987654"),
            )
            .expect("process stat");
        };

        write_state('S');
        assert!(process_is_running_at(root.path(), identity, euid).expect("running process"));
        write_state('Z');
        assert!(!process_is_running_at(root.path(), identity, euid).expect("zombie process"));
        write_state('S');
        assert!(!process_is_running_at(
            root.path(),
            ProcessIdentity {
                start_identity: StartIdentity::new(987_655),
                ..identity
            },
            euid,
        )
        .expect("reused process"));
    }

    #[test]
    fn descendant_identity_scan_skips_a_child_that_already_disappeared() {
        let live_process_id = 123;
        let exited_process_id = 456;
        let live_identity = ProcessIdentity {
            pid: live_process_id,
            start_identity: StartIdentity::new(987_654),
        };

        assert_eq!(
            collect_live_identities(&[live_process_id, exited_process_id], |pid| {
                Ok((pid == live_process_id).then_some(live_identity))
            })
            .expect("descendant identity scan"),
            vec![live_identity]
        );
    }

    #[test]
    fn descendant_identity_scan_skips_an_explicit_process_race() {
        let live_identity = ProcessIdentity {
            pid: 123,
            start_identity: StartIdentity::new(987_654),
        };

        assert_eq!(
            collect_live_identities(&[live_identity.pid, 456], |pid| {
                if pid == live_identity.pid {
                    Ok(Some(live_identity))
                } else {
                    Err(Error::Race {
                        operation: "inspect_descendant_identities",
                    })
                }
            })
            .expect("descendant identity scan"),
            vec![live_identity]
        );
    }

    #[test]
    fn descendant_identity_scan_propagates_non_race_failures() {
        let error = collect_live_identities(&[123], |_pid| {
            Err(Error::from_io(
                "inspect_descendant_identities",
                io::Error::from(io::ErrorKind::PermissionDenied),
            ))
        })
        .expect_err("permission failure must remain visible");

        assert!(matches!(
            error,
            Error::PermissionDenied {
                operation: "inspect_descendant_identities",
                ..
            }
        ));
    }

    fn stat_text(ppid: i32, pgrp: i32) -> String {
        format!(
            "123 (agent) S {ppid} {pgrp} {pgrp} 0 {pgrp} 4227084 0 0 0 0 0 0 0 0 20 0 1 0 987654"
        )
    }

    #[test]
    fn transient_stat_sample_resolves_on_resample() {
        let mut samples = VecDeque::from([stat_text(0, -1), stat_text(0, -1), stat_text(7, 456)]);
        let mut reads = 0;
        let fact = read_settled_stat(|| {
            reads += 1;
            Ok(samples.pop_front().expect("no more samples than reads"))
        })
        .expect("stat read")
        .expect("settled stat");

        assert_eq!(reads, 3);
        assert_eq!((fact.parent_id, fact.pgid), (7, 456));
        assert_eq!(fact.start_identity, StartIdentity::new(987_654));
    }

    #[test]
    fn transient_stat_sample_of_a_vanished_process_is_a_race() {
        let mut reads = 0;
        let fact = read_settled_stat(|| {
            reads += 1;
            if reads == 1 {
                Ok(stat_text(0, -1))
            } else {
                Err(io::Error::from(io::ErrorKind::NotFound))
            }
        })
        .expect("vanished process is not an error");

        assert_eq!(fact, None);
        assert_eq!(reads, 2);
    }

    #[test]
    fn persistently_transient_stat_sample_fails_closed_after_the_bound() {
        let mut reads = 0;
        let error = read_settled_stat(|| {
            reads += 1;
            Ok(stat_text(0, -1))
        })
        .expect_err("a task that never settles must stay visible");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(reads, STAT_TRANSIENT_MAX_READS);
    }

    #[test]
    fn malformed_stat_sample_fails_without_resampling() {
        let mut reads = 0;
        let error = read_settled_stat(|| {
            reads += 1;
            Ok("123 (truncated) S 7".to_owned())
        })
        .expect_err("malformed stat must fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(reads, 1);
    }

    #[test]
    fn negative_pgrp_with_a_parent_is_malformed_not_transient() {
        let mut reads = 0;
        let error = read_settled_stat(|| {
            reads += 1;
            Ok(stat_text(7, -1))
        })
        .expect_err("only the ppid 0 / pgrp -1 pair is a placeholder");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(reads, 1);
    }

    #[test]
    fn process_without_a_parent_reads_as_settled() {
        let fact = read_settled_stat(|| Ok(stat_text(0, 1)))
            .expect("stat read")
            .expect("settled stat");

        assert_eq!((fact.parent_id, fact.pgid), (0, 1));
    }

    #[test]
    fn stat_file_reader_resolves_a_transient_sample_from_procfs_layout() {
        let root = pohunek_test_support::tempdir().expect("temporary proc root");
        let process_dir = root.path().join("123");
        fs::create_dir(&process_dir).expect("process directory");
        fs::write(process_dir.join("stat"), stat_text(7, 456)).expect("stat");

        let fact = read_process_stat_at(root.path(), 123)
            .expect("stat read")
            .expect("settled stat");
        assert_eq!((fact.parent_id, fact.pgid), (7, 456));
    }

    #[test]
    fn narrow_identity_reports_malformed_process_stat() {
        let root = pohunek_test_support::tempdir().expect("temporary proc root");
        let process_id = 123;
        let process_dir = root.path().join(process_id.to_string());
        fs::create_dir(&process_dir).expect("process directory");
        fs::write(process_dir.join("stat"), "123 (truncated) S 7").expect("malformed process stat");
        let euid = fs::metadata(&process_dir).expect("process metadata").uid();

        let error = read_process_identity_at(root.path(), process_id, euid)
            .expect_err("malformed stat must remain visible");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn control_group_membership_requires_a_component_boundary() {
        let root = pohunek_test_support::tempdir().expect("temporary proc root");
        let process_id = 123;
        let process_dir = root.path().join(process_id.to_string());
        fs::create_dir(&process_dir).expect("process directory");
        fs::write(
            process_dir.join("cgroup"),
            concat!(
                "2:cpu:/user.slice/pohunek-session@s-4\n",
                "0::/user.slice/pohunek-session@s-42.service/agent.scope\n"
            ),
        )
        .expect("process cgroup");
        let euid = fs::metadata(&process_dir).expect("process metadata").uid();

        assert!(read_control_group_membership_at(
            root.path(),
            process_id,
            euid,
            "/user.slice/pohunek-session@s-42.service",
        )
        .expect("cgroup membership"));
        assert!(!read_control_group_membership_at(
            root.path(),
            process_id,
            euid,
            "/user.slice/pohunek-session@s-4",
        )
        .expect("sibling cgroup rejection"));
        assert!(
            read_control_group_membership_at(root.path(), process_id, euid, "/")
                .expect("root cgroup membership")
        );
    }

    #[test]
    fn exited_process_errno_is_a_race() {
        let error = io::Error::from_raw_os_error(nix::errno::Errno::ESRCH as i32);
        assert!(process_error("open_exit_watch", error).is_race());
    }

    #[test]
    fn exit_watch_rejects_a_stale_process_generation() {
        let inspector = LinuxInspector::new();
        let current = inspector
            .identity(std::process::id())
            .expect("inspect current process")
            .expect("current process exists");
        let stale = ProcessIdentity {
            pid: current.pid,
            start_identity: StartIdentity::new(current.start_identity.get().saturating_add(1)),
        };

        assert!(matches!(
            inspector.exit_watch(stale),
            Err(Error::Race {
                operation: "open_exit_watch"
            })
        ));
    }

    #[test]
    fn exit_watch_created_without_tokio_observes_exit_in_runtime_without_io() {
        let inspector = LinuxInspector::new();
        let env = pohunek_test_support::env::TestEnv::new().expect("test environment");
        let mut child = env
            .command("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn watched child");
        let identity = inspector
            .identity(child.id())
            .expect("inspect watched child")
            .expect("watched child exists");

        let watch = inspector
            .exit_watch(identity)
            .expect("create exit watch outside Tokio");
        child.kill().expect("terminate watched child");
        child.wait().expect("reap watched child");

        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime without I/O driver")
            .block_on(watch.wait())
            .expect("observe exit without Tokio I/O driver");
    }

    #[test]
    fn environ_parser_retains_only_allowlisted_markers() {
        let environ = b"AWS_SECRET_ACCESS_KEY=super-secret\0POHUNEK_DAEMON_ID=d-1\0\
POHUNEK_RUNTIME_ID=r-legacy\0POHUNEK_SESSION_ID=s-1\0POHUNEK_WORKER_INSTANCE_ID=r-1\0\
POHUNEK_WORKER_INSTANCE_IDX=r-2\0HOME=/home/u\0";

        let markers = parse_ownership_markers(environ);

        assert_eq!(
            markers,
            OwnershipMarkers {
                daemon_id: Some("d-1".to_owned()),
                session_id: Some("s-1".to_owned()),
                worker_id: None,
                worker_instance_id: Some("r-1".to_owned()),
            }
        );
        assert!(!format!("{markers:?}").contains("super-secret"));
    }

    #[test]
    fn environ_parser_keeps_the_first_duplicate_like_getenv() {
        let markers = parse_ownership_markers(
            b"POHUNEK_WORKER_INSTANCE_ID=first\0POHUNEK_WORKER_INSTANCE_ID=second\0",
        );

        assert_eq!(markers.worker_instance_id.as_deref(), Some("first"));
        assert!(markers.is_marked());
    }

    #[test]
    fn environ_parser_ignores_entries_without_a_separator() {
        let markers = parse_ownership_markers(b"POHUNEK_WORKER_INSTANCE_ID\0\0garbage");

        assert_eq!(markers, OwnershipMarkers::default());
        assert!(!markers.is_marked());
    }

    #[test]
    fn runtime_marker_is_read_from_a_live_process_environment() {
        let inspector = LinuxInspector::new();
        let env = pohunek_test_support::env::TestEnv::new().expect("test environment");
        let mut child = env
            .command("/bin/sleep")
            .arg("30")
            .env("POHUNEK_WORKER_INSTANCE_ID", "runtime-live")
            .env("POHUNEK_TEST_SECRET", "not-a-marker")
            .spawn()
            .expect("spawn marked child");

        // `spawn` can return while `execve` is still installing the new image,
        // whose argument and environment regions are then briefly empty.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while inspector
            .process(child.id())
            .expect("inspect marked child")
            .is_none_or(|fact| fact.cmdline != ["/bin/sleep", "30"])
        {
            assert!(std::time::Instant::now() < deadline, "child never exec'd");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let markers = inspector.ownership_markers(child.id());
        child.kill().expect("terminate marked child");
        child.wait().expect("reap marked child");

        let markers = markers.expect("inspect child markers");
        assert_eq!(markers.worker_instance_id.as_deref(), Some("runtime-live"));
        assert!(!format!("{markers:?}").contains("not-a-marker"));
    }

    #[test]
    fn missing_boot_identity_facility_is_unavailable() {
        let error = io::Error::from_raw_os_error(nix::errno::Errno::ENOENT as i32);
        assert!(matches!(
            facility_error("read_boot_identity", error),
            Error::Unavailable {
                operation: "read_boot_identity"
            }
        ));
    }
    /// Regression scenarios for the exec-window ambiguity of an empty
    /// `/proc/<pid>/environ` read (#1176).
    ///
    /// The production marker reader decides each empty read from controlled
    /// raw procfs observations, and the actual sweep selection
    /// (`survey_runtime`) classifies what it answers, so the scenarios never
    /// mock a marker answer or an error.
    mod exec_window {
        use std::sync::Mutex;

        use super::*;

        use crate::process::{
            survey_runtime, SkipReason, Skipped, SweepError, SweepRequest, SweepSurvey,
        };

        /// Worker instance ID the scenarios sweep for.
        const SCENARIO_RUNTIME: &str = "runtime-1176";

        /// Start identity shared by the scenario processes and their stat
        /// fixtures (`starttime`).
        const SCENARIO_START: u64 = 987_654;

        /// A published environment region observed by the scenarios, shaped
        /// after a real process's stat read on a 2026 kernel.
        const PUBLISHED_ENV_START: u64 = 140_731_459_362_880;
        const PUBLISHED_ENV_END: u64 = 140_731_459_366_893;

        /// Scripted outcome of one raw `/proc/<pid>/environ` read.
        #[derive(Debug, Clone)]
        enum EnvironSample {
            /// The read succeeded with these raw NUL-separated entries.
            Bytes(Vec<u8>),
            /// The read failed with `ENOENT` because the task is gone.
            Vanished,
        }

        /// Scripted outcome of one raw `/proc/<pid>/stat` read.
        #[derive(Debug, Clone)]
        enum StatSample {
            /// The read succeeded with this raw stat line.
            Text(String),
            /// The read failed with `ENOENT` because the task is gone.
            Vanished,
        }

        /// Scripted raw procfs observations and read counts, per process id.
        ///
        /// Each observation sequence repeats its last entry once exhausted, so
        /// a reader that settles a stat or rereads an environment within its
        /// bounds keeps observing the scripted state.
        #[derive(Debug, Default)]
        struct Scripts {
            environ: HashMap<Pid, VecDeque<EnvironSample>>,
            stat: HashMap<Pid, VecDeque<StatSample>>,
            environ_reads: HashMap<Pid, usize>,
            stat_reads: HashMap<Pid, usize>,
        }

        impl Scripts {
            fn read_environ(&mut self, pid: Pid) -> io::Result<Vec<u8>> {
                *self.environ_reads.entry(pid).or_default() += 1;
                let sample = repeated(
                    self.environ
                        .get_mut(&pid)
                        .expect("every process scripts an environ observation"),
                );
                match sample {
                    EnvironSample::Bytes(bytes) => Ok(bytes),
                    EnvironSample::Vanished => Err(io::Error::from(io::ErrorKind::NotFound)),
                }
            }

            fn read_stat(&mut self, pid: Pid) -> io::Result<String> {
                *self.stat_reads.entry(pid).or_default() += 1;
                let sample = repeated(
                    self.stat
                        .get_mut(&pid)
                        .expect("every process scripts a stat observation"),
                );
                match sample {
                    StatSample::Text(text) => Ok(text),
                    StatSample::Vanished => Err(io::Error::from(io::ErrorKind::NotFound)),
                }
            }
        }

        /// Returns the next observation, repeating the last one in the end.
        fn repeated<T: Clone>(samples: &mut VecDeque<T>) -> T {
            if samples.len() > 1 {
                samples.pop_front().expect("nonempty observation sequence")
            } else {
                samples
                    .front()
                    .expect("nonempty observation sequence")
                    .clone()
            }
        }

        /// Inspector whose raw procfs observations are scripted.
        ///
        /// Marker answers are never scripted: [`ownership_markers_from`], the
        /// production reader, produces them from the scripted observations,
        /// and the scenarios run them through the actual sweep selection, so
        /// the regression covers the reader and the sweep together.
        #[derive(Debug, Default)]
        struct ObservedInspector {
            facts: Vec<ProcessFact>,
            scripts: Mutex<Scripts>,
        }

        impl ObservedInspector {
            fn with_process(
                mut self,
                pid: Pid,
                start: u64,
                environ: Vec<EnvironSample>,
                stat_samples: Vec<StatSample>,
            ) -> Self {
                self.facts.push(ProcessFact {
                    pid,
                    pgid: pid,
                    ppid: 1,
                    start_identity: StartIdentity::new(start),
                    comm: "agent".to_owned(),
                    cmdline: Vec::new(),
                });
                let scripts = self.scripts.get_mut().expect("scripts are not poisoned");
                scripts.environ.insert(pid, environ.into());
                scripts.stat.insert(pid, stat_samples.into());
                self
            }
        }

        impl ProcessInspector for ObservedInspector {
            fn identity(&self, pid: Pid) -> Result<Option<ProcessIdentity>, Error> {
                Ok(self
                    .facts
                    .iter()
                    .find(|fact| fact.pid == pid)
                    .map(ProcessFact::identity))
            }

            fn is_running(&self, identity: ProcessIdentity) -> Result<bool, Error> {
                // Every scenario process is live as long as it is scripted;
                // an ended control is decided by the reader before liveness
                // matters anywhere in the sweep.
                Ok(self.facts.iter().any(|fact| fact.identity() == identity))
            }

            fn parent_pid(&self, _pid: Pid) -> Result<Option<Pid>, Error> {
                Ok(None)
            }

            fn process(&self, _pid: Pid) -> Result<Option<ProcessFact>, Error> {
                Ok(None)
            }

            fn same_user_processes(&self) -> Result<Vec<ProcessFact>, Error> {
                Ok(self.facts.clone())
            }

            fn descendants(&self, _root: Pid) -> Result<Vec<ProcessFact>, Error> {
                Ok(Vec::new())
            }

            fn cwd(&self, _pid: Pid) -> Result<PathBuf, Error> {
                Err(Error::Unavailable {
                    operation: "scripted_cwd",
                })
            }

            fn executable(&self, _pid: Pid) -> Result<Option<PathBuf>, Error> {
                Ok(None)
            }

            fn exit_watch(&self, _identity: ProcessIdentity) -> Result<ExitWatch, Error> {
                Err(Error::Unavailable {
                    operation: "scripted_exit_watch",
                })
            }

            fn ownership_markers(&self, pid: Pid) -> Result<OwnershipMarkers, Error> {
                ownership_markers_from(
                    || {
                        self.scripts
                            .lock()
                            .expect("scripts are not poisoned")
                            .read_environ(pid)
                    },
                    || {
                        self.scripts
                            .lock()
                            .expect("scripts are not poisoned")
                            .read_stat(pid)
                    },
                    OWNERSHIP_MARKERS_OPERATION,
                )
            }

            fn foreground_process_group(&self, _root_pid: Pid) -> Result<Option<Pid>, Error> {
                Ok(None)
            }
        }

        /// Builds a settled `/proc/<pid>/stat` line for a scenario process:
        /// fields 3–22 mirror the existing fixtures, and the appended tail
        /// carries the kernel padding plus the environment-region bounds.
        fn observed_stat(state: char, env_start: u64, env_end: u64) -> StatSample {
            observed_stat_flags(state, env_start, env_end, 0)
        }

        fn observed_stat_flags(
            state: char,
            env_start: u64,
            env_end: u64,
            flags: u32,
        ) -> StatSample {
            let padding = "0 ".repeat(STAT_ENV_START_FIELD - STAT_STARTTIME_FIELD - 1);
            StatSample::Text(format!(
                "987 (agent) {state} 1 987 999 0 -1 {flags} 0 0 0 0 0 0 0 0 20 0 1 0 {SCENARIO_START} {padding}{env_start} {env_end}"
            ))
        }

        fn empty_environ() -> EnvironSample {
            EnvironSample::Bytes(Vec::new())
        }

        fn marked_environ(worker_instance_id: &str) -> EnvironSample {
            EnvironSample::Bytes(
                format!("POHUNEK_WORKER_INSTANCE_ID={worker_instance_id}\0").into_bytes(),
            )
        }

        fn scenario_identity(pid: Pid) -> ProcessIdentity {
            ProcessIdentity {
                pid,
                start_identity: StartIdentity::new(SCENARIO_START),
            }
        }

        /// Runs the actual sweep selection over a scripted scenario table.
        ///
        /// The grace and poll intervals only have to validate; the survey
        /// signals nothing.
        fn run_survey(inspector: &ObservedInspector) -> Result<SweepSurvey, SweepError> {
            let request = SweepRequest::new(
                SCENARIO_RUNTIME,
                rustix::process::geteuid().as_raw(),
                Duration::from_millis(50),
                Duration::from_millis(5),
            )
            .expect("valid request");
            survey_runtime(inspector, &request)
        }

        #[test]
        fn a_kernel_thread_newer_than_the_worker_does_not_block_cleanup() {
            let kernel_thread = 999_931;
            let inspector = ObservedInspector::default().with_process(
                kernel_thread,
                SCENARIO_START,
                vec![empty_environ()],
                // PF_KTHREAD from Linux's task flags, plus an unrelated flag.
                vec![observed_stat_flags('I', 0, 0, 0x0020_0000 | 0x0000_0040)],
            );
            let request = SweepRequest::new(
                SCENARIO_RUNTIME,
                rustix::process::geteuid().as_raw(),
                Duration::from_millis(50),
                Duration::from_millis(5),
            )
            .expect("valid request")
            .with_worker_start_identity(Some(StartIdentity::new(SCENARIO_START - 1)));

            // UID 0 enumerates these same-user tasks too. A kernel thread
            // cannot carry runtime markers, even if it started after the worker.
            let survey = survey_runtime(&inspector, &request).expect("survey");
            assert!(
                survey.is_empty(),
                "kernel thread must not block cleanup: {survey:?}"
            );
        }

        #[test]
        fn exec_window_empty_environ_with_an_unpublished_region_is_skipped_unreadable() {
            let unpublished = 999_920;
            let inspector = ObservedInspector::default().with_process(
                unpublished,
                SCENARIO_START,
                vec![empty_environ()],
                vec![observed_stat('S', 0, 0)],
            );

            let survey = run_survey(&inspector).expect("survey");

            // The empty read must not read as an unmarked process: the task
            // may still be inside the exec that installs its markers.
            assert_eq!(
                survey.skipped,
                vec![Skipped {
                    identity: scenario_identity(unpublished),
                    reason: SkipReason::MarkersUnreadable,
                }]
            );
            assert!(survey.attributed.is_empty());
        }

        #[test]
        fn exec_window_empty_environ_contradicting_a_settled_region_rereads_within_the_bound() {
            let contradicting = 999_921;
            let inspector = ObservedInspector::default().with_process(
                contradicting,
                SCENARIO_START,
                vec![empty_environ()],
                vec![observed_stat('S', PUBLISHED_ENV_START, PUBLISHED_ENV_END)],
            );

            let survey = run_survey(&inspector).expect("survey");

            // The contradiction means the image finished installing its
            // environment between the two observations; the bounded rereads
            // still leave the task unobservable, so the cleanup stays
            // unconfirmed instead of foreign.
            assert_eq!(
                survey.skipped,
                vec![Skipped {
                    identity: scenario_identity(contradicting),
                    reason: SkipReason::MarkersUnreadable,
                }]
            );
            assert!(survey.attributed.is_empty());
            // The rereads are bounded even though every one stays empty: one
            // initial read plus the reread bound.
            let scripts = inspector.scripts.lock().expect("scripts are not poisoned");
            assert_eq!(scripts.environ_reads[&contradicting], 1 + ENVIRON_REREADS);
            assert_eq!(scripts.stat_reads[&contradicting], 1);
        }

        #[test]
        fn exec_window_markers_recovered_by_a_reread_are_attributed() {
            let recovered = 999_922;
            let inspector = ObservedInspector::default().with_process(
                recovered,
                SCENARIO_START,
                vec![
                    empty_environ(),
                    empty_environ(),
                    marked_environ(SCENARIO_RUNTIME),
                ],
                vec![observed_stat('S', PUBLISHED_ENV_START, PUBLISHED_ENV_END)],
            );

            let survey = run_survey(&inspector).expect("survey");

            assert_eq!(survey.attributed, vec![scenario_identity(recovered)]);
            assert!(survey.skipped.is_empty());
        }

        #[test]
        fn a_live_process_with_a_genuinely_empty_environment_is_foreign() {
            let genuinely_empty = 999_923;
            let inspector = ObservedInspector::default().with_process(
                genuinely_empty,
                SCENARIO_START,
                vec![empty_environ()],
                vec![observed_stat('S', PUBLISHED_ENV_START, PUBLISHED_ENV_START)],
            );

            let survey = run_survey(&inspector).expect("survey");

            // A published zero-length region records an empty environment,
            // which is exactly unmarked, and the sweep leaves it out
            // silently.
            assert!(survey.attributed.is_empty());
            assert!(survey.skipped.is_empty());
        }

        #[test]
        fn ended_and_vanished_tasks_with_empty_environ_reads_stay_foreign() {
            let ended = 999_924;
            let vanished = 999_925;
            let inspector = ObservedInspector::default()
                .with_process(
                    ended,
                    SCENARIO_START,
                    vec![empty_environ()],
                    vec![observed_stat('Z', 0, 0)],
                )
                .with_process(
                    vanished,
                    SCENARIO_START,
                    vec![EnvironSample::Vanished],
                    vec![StatSample::Vanished],
                )
                .with_process(
                    999_930,
                    SCENARIO_START,
                    vec![empty_environ()],
                    vec![StatSample::Vanished],
                );

            let survey = run_survey(&inspector).expect("survey");

            assert!(survey.attributed.is_empty());
            assert!(survey.skipped.is_empty());
        }

        #[test]
        fn reversed_and_half_published_environment_regions_fail_closed() {
            let reversed = 999_926;
            let startless = 999_927;
            let endless = 999_928;
            let inspector = ObservedInspector::default()
                .with_process(
                    reversed,
                    SCENARIO_START,
                    vec![empty_environ()],
                    vec![observed_stat('S', PUBLISHED_ENV_END, PUBLISHED_ENV_START)],
                )
                .with_process(
                    startless,
                    SCENARIO_START,
                    vec![empty_environ()],
                    vec![observed_stat('S', 0, PUBLISHED_ENV_END)],
                )
                .with_process(
                    endless,
                    SCENARIO_START,
                    vec![empty_environ()],
                    vec![observed_stat('S', PUBLISHED_ENV_START, 0)],
                );

            let survey = run_survey(&inspector).expect("survey");

            // The kernel never reports these regions, and none of them may
            // read as an unmarked process.
            assert!(survey.attributed.is_empty());
            assert_eq!(
                survey.skipped,
                vec![
                    Skipped {
                        identity: scenario_identity(reversed),
                        reason: SkipReason::MarkersUnreadable,
                    },
                    Skipped {
                        identity: scenario_identity(startless),
                        reason: SkipReason::MarkersUnreadable,
                    },
                    Skipped {
                        identity: scenario_identity(endless),
                        reason: SkipReason::MarkersUnreadable,
                    },
                ]
            );
        }

        #[test]
        fn a_malformed_stat_of_an_empty_environ_aborts_the_survey() {
            let malformed = 999_929;
            let inspector = ObservedInspector::default().with_process(
                malformed,
                SCENARIO_START,
                vec![empty_environ()],
                vec![StatSample::Text("123 (truncated) S 7".to_owned())],
            );

            let error = run_survey(&inspector).expect_err("malformed stat must fail closed");

            assert!(matches!(
                error,
                SweepError::Inspection {
                    source: Error::InvalidData {
                        operation: OWNERSHIP_MARKERS_OPERATION,
                    },
                    ..
                }
            ));
        }
    }
}
