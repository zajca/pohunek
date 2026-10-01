//! Fixtures shared by the GUI unit tests: a canonical private temporary root,
//! executable fakes that are sealed before they run, and a watchdog that turns
//! a hung process wait into a fast failure with process-table evidence.

// Rust guideline compliant 2026-10-01
#![forbid(unsafe_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// How long any single test may run before the watchdog aborts the process.
/// The slowest legitimate test finishes in seconds; a minute leaves ample room
/// on a loaded CI machine while keeping a hang far below the job timeout.
const WATCHDOG_LIMIT: Duration = Duration::from_secs(60);

/// A canonical temporary root with mode 0700.
///
/// The trusted filesystem layer refuses symlinked path components, and macOS
/// temporary directories sit below the `/var` symlink, so the root comes from
/// `pohunek_test_support` (`/private/tmp` on macOS) and is canonicalized once;
/// fixtures and assertions then share one spelling.
pub(crate) struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    /// The canonical root path.
    pub(crate) fn path(&self) -> &Path {
        &self.root
    }
}

/// Creates a canonical, owner-private temporary root.
pub(crate) fn fixture() -> Fixture {
    let temp = pohunek_test_support::tempdir_with_prefix("pgui").expect("private temp root");
    let root = fs::canonicalize(temp.path()).expect("canonical temp root");
    Fixture { _temp: temp, root }
}

/// Makes `path` mode 0755 and flushes it to disk.
///
/// The file must already be written and closed (`fs::write` closes it), so the
/// kernel never sees a writer when the script is executed (`ETXTBSY`).
pub(crate) fn make_executable(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod executable");
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .expect("sync executable");
}

/// Aborts the test process with process-table evidence if the test outlives
/// [`WATCHDOG_LIMIT`]; dropping the guard cancels it.
///
/// A blocked wait on a child otherwise holds the whole job until the CI timeout
/// with no output. The abort message names the test and lists the test
/// process, its children, and their process groups.
#[must_use = "the watchdog is cancelled when the guard drops"]
pub(crate) fn watchdog() -> Watchdog {
    let name = thread::current()
        .name()
        .unwrap_or("unnamed test")
        .to_owned();
    let (cancel, cancelled) = mpsc::channel::<()>();
    let spawned = thread::Builder::new()
        .name("pohunek-gui-watchdog".to_owned())
        .spawn(move || {
            if cancelled.recv_timeout(WATCHDOG_LIMIT) == Err(mpsc::RecvTimeoutError::Timeout) {
                eprintln!(
                    "WATCHDOG: test `{name}` ran longer than {WATCHDOG_LIMIT:?}; \
                     processes of this test run:\n{}",
                    process_evidence()
                );
                std::process::abort();
            }
        });
    Watchdog {
        _cancel: cancel,
        _thread: spawned.ok(),
    }
}

/// Cancels its watchdog on drop.
pub(crate) struct Watchdog {
    _cancel: mpsc::Sender<()>,
    _thread: Option<thread::JoinHandle<()>>,
}

/// `ps -o pid,ppid,pgid,stat,comm` for this process, its children, and every
/// process in a child's group.
fn process_evidence() -> String {
    let me = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,stat=,comm="])
        .output();
    let Ok(output) = output else {
        return "ps could not be run".to_owned();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let rows: Vec<Vec<&str>> = text
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .filter(|fields| fields.len() >= 5)
        .collect();
    let children: Vec<&str> = rows
        .iter()
        .filter(|fields| fields[1] == me)
        .map(|fields| fields[0])
        .collect();
    let mut report = String::from("pid ppid pgid stat comm\n");
    for fields in &rows {
        if fields[0] == me || fields[1] == me || children.contains(&fields[2]) {
            report.push_str(&fields.join(" "));
            report.push('\n');
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixture_root_is_canonical_and_private() {
        let root = fixture();

        assert_eq!(
            fs::canonicalize(root.path()).expect("canonical"),
            root.path()
        );
        let mode = fs::metadata(root.path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn evidence_lists_this_process() {
        assert!(process_evidence().contains(&std::process::id().to_string()));
    }

    #[test]
    fn a_dropped_watchdog_does_not_abort() {
        drop(watchdog());
    }
}
