//! Linux `/proc` and pidfd process inspection.

// Rust guideline compliant 2026-07-07

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use super::{ExitWatch, Pid, ProcessFact, ProcessInspector};

/// Linux proc filesystem root.
///
/// All process facts come from procfs because it is available to unprivileged
/// same-user processes and does not require kernel capabilities.
const PROC_ROOT: &str = "/proc";
/// Flags passed to `pidfd_open(2)`.
///
/// The syscall currently defines no behavioral flags for our use case; `0`
/// requests the default pidfd suitable for readiness polling.
const PIDFD_OPEN_FLAGS: libc::c_uint = 0;

/// Linux process inspector backed by procfs and pidfds.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinuxInspector;

impl LinuxInspector {
    /// Creates a Linux process inspector.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ProcessInspector for LinuxInspector {
    fn same_user_processes(&self) -> io::Result<Vec<ProcessFact>> {
        same_user_processes()
    }

    fn descendants(&self, root: Pid) -> io::Result<Vec<ProcessFact>> {
        let euid = current_euid()?;
        let pids = match descendants_from_children(root, euid)? {
            Some(pids) => pids,
            None => descendants_from_ppid_scan(root, euid)?,
        };
        let mut facts = Vec::with_capacity(pids.len());
        for pid in pids {
            if let Some(fact) = read_process_fact(pid, euid)? {
                facts.push(fact);
            }
        }
        Ok(facts)
    }

    fn cwd(&self, pid: Pid) -> io::Result<PathBuf> {
        fs::read_link(proc_path(pid).join("cwd"))
    }

    fn exit_watch(&self, pid: Pid) -> io::Result<ExitWatch> {
        ExitWatch::from_fd(pidfd_open(pid)?)
    }
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
    if !same_user(process_id, euid) {
        return Ok(None);
    }
    let Some((comm, parent_id)) = read_status(process_id)? else {
        return Ok(None);
    };
    Ok(Some(ProcessFact {
        pid: process_id,
        ppid: parent_id,
        comm,
        cmdline: read_cmdline(process_id)?,
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
    fs::metadata(proc_path(pid)).is_ok_and(|metadata| metadata.uid() == euid)
}

fn proc_path(pid: Pid) -> PathBuf {
    PathBuf::from(PROC_ROOT).join(pid.to_string())
}

fn parse_pid(value: &OsStr) -> Option<Pid> {
    value.to_str().and_then(parse_pid_str)
}

fn parse_pid_str(value: &str) -> Option<Pid> {
    value.parse::<Pid>().ok()
}

fn is_process_race(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::NotFound || err.raw_os_error() == Some(libc::ESRCH)
}

#[expect(unsafe_code, reason = "pidfd_open requires a raw Linux syscall")]
fn pidfd_open(pid: Pid) -> io::Result<OwnedFd> {
    #[expect(
        clippy::cast_possible_wrap,
        reason = "process id is bounded by PID_MAX, far below i32::MAX"
    )]
    let pid = pid as libc::pid_t;
    // SAFETY: `syscall` is invoked with the Linux `pidfd_open` number, a pid from
    // the OS process table, and documented zero flags. It returns either `-1`
    // with errno set or a new owned file descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, PIDFD_OPEN_FLAGS) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let raw_fd = RawFd::try_from(fd).map_err(|err| {
        io::Error::other(format!(
            "pidfd_open returned invalid file descriptor {fd}: {err}"
        ))
    })?;
    // SAFETY: `pidfd_open` returned a fresh file descriptor owned by this process,
    // and `OwnedFd` takes responsibility for closing it exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}
