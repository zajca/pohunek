//! Fixtures shared by the `shell_env` unit tests.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    let temp = tempfile::tempdir().expect("tempdir");
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
/// A separate process writes the file. While this process held a writable
/// descriptor, a sibling test thread that spawned a process would give its
/// child a copy until the child's own `exec`, and executing the script
/// meanwhile would fail with `ETXTBSY`. The writer exits before the script is
/// returned, so no descriptor open for writing remains anywhere.
pub(super) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    write_without_local_writer(&path, format!("#!/bin/sh\n{body}\n").as_bytes());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    path
}

/// Search path of the fixture's `sh` children: the system directories that
/// hold `cat` on Linux and macOS, so no developer `PATH` entry is consulted.
const CHILD_PATH: &str = "/usr/bin:/bin";

/// A `sh` command with an empty environment apart from [`CHILD_PATH`], in the
/// private directory `cwd`.
fn scrubbed_command(cwd: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    command.env_clear().env("PATH", CHILD_PATH).current_dir(cwd);
    command
}

/// Creates `path` with `content` through a `sh` child that owns the write
/// descriptor.
fn write_without_local_writer(path: &Path, content: &[u8]) {
    let mut writer = scrubbed_command(path.parent().expect("script path has a directory"))
        .args(["-c", "cat > \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn script writer");
    writer
        .stdin
        .take()
        .expect("script writer stdin")
        .write_all(content)
        .expect("send script content");
    assert!(
        writer.wait().expect("wait for script writer").success(),
        "the script writer failed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_spawn::while_a_sibling_spawns;

    /// Scripts started while a sibling thread keeps spawning processes.
    const SCRIPT_RUNS: usize = 200;

    #[test]
    fn a_fresh_script_runs_while_a_sibling_thread_keeps_spawning() {
        let dir = fixture();
        while_a_sibling_spawns(dir.path(), |_sibling| {
            for run in 0..SCRIPT_RUNS {
                let path = script(dir.path(), &format!("fresh-{run}"), "exit 0");
                let status = Command::new(&path)
                    .status()
                    .unwrap_or_else(|error| panic!("run {run}: {error}"));
                assert!(status.success());
            }
        });
    }
}
