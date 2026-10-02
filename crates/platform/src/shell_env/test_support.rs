//! Fixtures shared by the `shell_env` unit tests.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// A canonical temporary root with mode 0700.
///
/// macOS temporary directories sit below the `/var` symlink; the canonical
/// path keeps fixtures and assertions on one spelling.
pub(super) struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    /// The canonical root path.
    pub(super) fn path(&self) -> &Path {
        &self.root
    }
}

/// Creates a canonical temporary root.
pub(super) fn fixture() -> Fixture {
    let temp = pohunek_test_support::tempdir().expect("tempdir");
    let root = fs::canonicalize(temp.path()).expect("canonical temp root");
    Fixture { _temp: temp, root }
}

/// Creates `path` below `root`, every new component with mode 0755 regardless
/// of the process umask.
pub(super) fn make_dir(root: &Path, path: &Path) {
    let relative = path.strip_prefix(root).expect("path below the root");
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        if !current.exists() {
            fs::create_dir(&current).expect("create directory");
            fs::set_permissions(&current, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }
}

/// Writes an executable script with mode 0755 and returns its path.
///
/// The file is written through `pohunek_test_support::fs::write_executable`,
/// which holds no write descriptor in this process, so a sibling test thread
/// that spawns a process cannot make `exec` of the script fail with `ETXTBSY`.
pub(super) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    pohunek_test_support::fs::write_executable(&path, format!("#!/bin/sh\n{body}\n"))
        .expect("write script");
    path
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::test_spawn::while_a_sibling_spawns;

    /// Scripts started while a sibling thread keeps spawning processes.
    const SCRIPT_RUNS: usize = 200;

    #[test]
    fn a_fresh_script_runs_while_a_sibling_thread_keeps_spawning() {
        let dir = fixture();
        while_a_sibling_spawns(dir.path(), |sibling| {
            sibling.repeat_while_spawning(SCRIPT_RUNS, |run| {
                let path = script(dir.path(), &format!("fresh-{run}"), "exit 0");
                let status = Command::new(&path)
                    .status()
                    .unwrap_or_else(|error| panic!("run {run}: {error}"));
                assert!(status.success());
            });
        });
    }
}
