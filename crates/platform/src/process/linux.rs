//! Linux `/proc` and pidfd process inspection.

// Rust guideline compliant 2026-09-14

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

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
/// `/proc/<pid>/stat` process-group field number (`pgrp`).
const STAT_PGRP_FIELD: usize = 5;
/// `/proc/<pid>/stat` parent-process field number (`ppid`).
const STAT_PPID_FIELD: usize = 4;
/// `/proc/<pid>/stat` process-state field number.
const STAT_STATE_FIELD: usize = 3;
/// `/proc/<pid>/stat` controlling-terminal foreground group field number
/// (`tpgid`). The value is signed; `-1` means no controlling terminal.
const STAT_TPGID_FIELD: usize = 8;
/// `/proc/<pid>/stat` process start-time field number (`starttime`).
const STAT_STARTTIME_FIELD: usize = 22;
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

    /// Returns the executable path for one same-user process.
    ///
    /// # Errors
    ///
    /// Returns typed process inspection failures.
    pub fn executable(&self, pid: Pid) -> Result<Option<PathBuf>, Error> {
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
        ownership_markers(pid).map_err(|source| process_error("read_ownership_markers", source))
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
    let stat = match fs::read_to_string(proc_path_at(root, process_id).join("stat")) {
        Ok(stat) => stat,
        Err(err) if is_process_race(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let command_start = stat.find('(').ok_or_else(invalid_process_stat_error)?;
    let command_end = stat.rfind(')').ok_or_else(invalid_process_stat_error)?;
    let comm = stat
        .get(command_start + 1..command_end)
        .ok_or_else(invalid_process_stat_error)?
        .to_owned();
    let state = parse_stat_field(&stat, STAT_STATE_FIELD).ok_or_else(invalid_process_stat_error)?;
    let parent_id =
        parse_stat_pid_field(&stat, STAT_PPID_FIELD).ok_or_else(invalid_process_stat_error)?;
    let pgid =
        parse_stat_pid_field(&stat, STAT_PGRP_FIELD).ok_or_else(invalid_process_stat_error)?;
    let start_identity =
        parse_stat_field(&stat, STAT_STARTTIME_FIELD).ok_or_else(invalid_process_stat_error)?;
    Ok(Some(StatFact {
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

/// Reads `POHUNEK_DAEMON_ID` / `POHUNEK_SESSION_ID` from `/proc/<pid>/environ`.
///
/// `environ` holds the environment the process was `execve`d with, which is
/// exactly where a pohunek daemon's PTY markers live — they are injected before
/// exec and inherited by every child. Entries are NUL-separated `KEY=VALUE`
/// pairs; values are decoded lossily because marker ids are ASCII.
fn ownership_markers(pid: Pid) -> io::Result<OwnershipMarkers> {
    let bytes = match fs::read(proc_path(pid).join("environ")) {
        Ok(bytes) => bytes,
        Err(err) if is_process_race(&err) => return Ok(OwnershipMarkers::default()),
        Err(err) => return Err(err),
    };
    let mut markers = OwnershipMarkers::default();
    for entry in bytes.split(|byte| *byte == 0) {
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (key, rest) = entry.split_at(separator);
        let value = || String::from_utf8_lossy(&rest[1..]).into_owned();
        if key == ENV_DAEMON_ID.as_bytes() {
            markers.daemon_id = Some(value());
        } else if key == ENV_SESSION_ID.as_bytes() {
            markers.session_id = Some(value());
        }
        if markers.daemon_id.is_some() && markers.session_id.is_some() {
            break;
        }
    }
    Ok(markers)
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
    fn stat_foreground_group_reads_tpgid_after_command() {
        assert_eq!(
            parse_stat_pid_field(
                "123 (agent with spaces) S 1 2 3 456 789 8",
                STAT_TPGID_FIELD,
            ),
            Some(789)
        );
    }

    #[test]
    fn stat_process_group_reads_pgrp_after_command() {
        assert_eq!(
            parse_stat_pid_field("123 (agent with spaces) S 1 456 3 4 789 8", STAT_PGRP_FIELD,),
            Some(456)
        );
    }

    #[test]
    fn stat_start_identity_uses_shared_production_parser() {
        let stat = concat!(
            "123 (agent) S 1 456 3 4 789 0 0 0 0 0 0 0 0 0 ",
            "20 0 1 0 987654"
        );
        assert_eq!(
            parse_stat_field::<u64>(stat, STAT_STARTTIME_FIELD),
            Some(987_654)
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
        let root = tempfile::tempdir().expect("temporary proc root");
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
        let root = tempfile::tempdir().expect("temporary proc root");
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

    #[test]
    fn narrow_identity_reports_malformed_process_stat() {
        let root = tempfile::tempdir().expect("temporary proc root");
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
        let root = tempfile::tempdir().expect("temporary proc root");
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
    fn stat_parser_rejects_fields_before_state() {
        assert_eq!(parse_stat_pid_field("123 (agent) S 1 2 3 4 5", 2), None);
    }

    #[test]
    fn stat_foreground_group_uses_production_offset_arithmetic() {
        assert_eq!(
            parse_stat_pid_field("123 (agent) S 1 2 3 456 7 8", STAT_TPGID_FIELD),
            Some(7)
        );
    }

    #[test]
    fn stat_foreground_group_parses_signed_absent_value() {
        assert_eq!(
            parse_stat_pid_field("123 (agent) S 1 2 3 456 -1", STAT_TPGID_FIELD),
            None
        );
    }

    #[test]
    fn stat_foreground_group_handles_escaped_parenthesis_in_command() {
        assert_eq!(
            parse_stat_pid_field("123 (escaped \\() name) S 1 2 3 456 789", STAT_TPGID_FIELD,),
            Some(789)
        );
    }

    #[test]
    fn malformed_stat_has_no_process_group() {
        assert_eq!(
            parse_stat_pid_field(
                "123 (agent with \\() parenthesis) S 1 2 3",
                STAT_TPGID_FIELD,
            ),
            None
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
        let mut child = std::process::Command::new("/bin/sleep")
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
    fn missing_boot_identity_facility_is_unavailable() {
        let error = io::Error::from_raw_os_error(nix::errno::Errno::ENOENT as i32);
        assert!(matches!(
            facility_error("read_boot_identity", error),
            Error::Unavailable {
                operation: "read_boot_identity"
            }
        ));
    }
}
