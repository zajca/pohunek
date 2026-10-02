//! File writers that never leave a write descriptor in this process.
//!
//! A test process starts children from several threads at once. A file written
//! in-process with [`std::fs::write`] is open for writing until the write
//! returns, and a sibling thread that spawns a process in that window gives its
//! child a copy of the descriptor until the child's own `exec`. Executing the
//! new file meanwhile fails with `ETXTBSY` ("Text file busy") on Linux and
//! macOS, and the failure disappears on the next run.
//!
//! [`write_executable`] and [`write_file`] avoid the window: a short-lived `sh`
//! child opens and fills the file and exits before the call returns, so no
//! descriptor open for writing exists in this process, or in any process it
//! forks, at any time.
//!
//! # Examples
//!
//! ```
//! use std::process::Command;
//!
//! let dir = pohunek_test_support::tempdir()?;
//! let script = dir.path().join("hello");
//! pohunek_test_support::fs::write_executable(&script, "#!/bin/sh\nexit 0\n")?;
//! assert!(Command::new(&script).status()?.success());
//! # Ok::<(), std::io::Error>(())
//! ```

// Rust guideline compliant 2026-10-02

use std::io::{Error, Result, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Shell that opens and fills the file; a POSIX `sh` exists at this path on
/// Linux and macOS.
const WRITER_SHELL: &str = "/bin/sh";

/// Script the writer shell runs: `cat` copies stdin into the file named by the
/// first positional argument, creating or truncating it.
const WRITER_SCRIPT: &str = "cat > \"$1\"";

/// Value of `$0` inside [`WRITER_SCRIPT`], shown in the shell's own error
/// messages.
const WRITER_NAME: &str = "write-file";

/// The only environment variable the writer gets: the directories `cat` is
/// looked up in. Both hold it on Linux and macOS, and nothing else from the
/// test process leaks into the writer.
const WRITER_PATH: &str = "/usr/bin:/bin";

/// Mode [`write_executable`] sets.
const EXECUTABLE_MODE: u32 = 0o755;

/// Writes `content` to `path` through a child process, creating or truncating it.
///
/// This process never holds a write descriptor on the file, which avoids the
/// `ETXTBSY` race described in the [module documentation](self). The file mode
/// is whatever the process umask gives a new file; an existing file keeps its
/// mode.
///
/// # Errors
///
/// Returns an error naming `path` when the child cannot be started, when
/// writing its input fails, or when the child exits unsuccessfully (for example
/// because the parent directory does not exist), with the child's stderr in the
/// message.
///
/// # Examples
///
/// ```
/// let dir = pohunek_test_support::tempdir()?;
/// let file = dir.path().join("data");
/// pohunek_test_support::fs::write_file(&file, b"payload")?;
/// assert_eq!(std::fs::read(&file)?, b"payload");
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn write_file(path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    let failure = |detail: &dyn std::fmt::Display| {
        Error::other(format!(
            "writing {} through a child process: {detail}",
            path.display()
        ))
    };
    let mut writer = Command::new(WRITER_SHELL)
        .args(["-c", WRITER_SCRIPT, WRITER_NAME])
        .arg(path)
        .env_clear()
        .env("PATH", WRITER_PATH)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failure(&error))?;
    let mut stdin = writer.stdin.take().expect("the writer's stdin is piped");
    // A write error means the writer already exited; its status explains why.
    let sent = stdin.write_all(content.as_ref());
    drop(stdin);
    let output = writer.wait_with_output().map_err(|error| failure(&error))?;
    if !output.status.success() {
        return Err(failure(&format_args!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    sent.map_err(|error| failure(&error))
}

/// Writes `content` to `path` as an executable file (mode `0755`).
///
/// The content is written by [`write_file`], so this process holds no write
/// descriptor on the file when it can first be executed. The mode is then set
/// with a path-based `chmod`, which opens no descriptor and does not depend on
/// the umask.
///
/// # Errors
///
/// Returns the errors of [`write_file`], and an error naming `path` when the
/// mode cannot be set.
///
/// # Examples
///
/// ```
/// use std::os::unix::fs::PermissionsExt as _;
///
/// let dir = pohunek_test_support::tempdir()?;
/// let tool = dir.path().join("tool");
/// pohunek_test_support::fs::write_executable(&tool, "#!/bin/sh\nexit 0\n")?;
/// assert_eq!(std::fs::metadata(&tool)?.permissions().mode() & 0o777, 0o755);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn write_executable(path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    write_file(path, content)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(EXECUTABLE_MODE)).map_err(
        |error| {
            Error::new(
                error.kind(),
                format!("setting the mode of {}: {error}", path.display()),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Sets the flag when dropped, including while a panic unwinds, so the
    /// scoped spawn loops stop and `std::thread::scope` can return. It is
    /// created before the first sibling spawns, so a failed `Scope::spawn`
    /// also stops the siblings that already started.
    struct StopOnDrop<'a>(&'a AtomicBool);

    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Scripts written and started while sibling threads keep spawning
    /// processes. With `fs::write` and `set_permissions` instead of the helper,
    /// this loop fails with `ETXTBSY` in nearly every run on a Linux host
    /// (about 3 failures per 300 scripts with four siblings).
    const SCRIPT_RUNS: usize = 500;

    /// Threads that spawn processes continuously; more threads widen the window
    /// in which a fork copies a descriptor.
    const SIBLING_THREADS: usize = 4;

    /// Content larger than a pipe buffer, so the writer must drain stdin while
    /// the parent is still sending.
    const LARGE_CONTENT_BYTES: usize = 1 << 20;

    #[test]
    fn a_fresh_executable_runs_while_sibling_threads_keep_spawning() {
        let dir = crate::tempdir().expect("fixture directory");
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let _stop_siblings = StopOnDrop(&stop);
            for _ in 0..SIBLING_THREADS {
                scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        Command::new("/bin/sh")
                            .args(["-c", "exit 0"])
                            .status()
                            .expect("sibling spawn");
                    }
                });
            }
            for run in 0..SCRIPT_RUNS {
                let path = dir.path().join(format!("fresh-{run}"));
                write_executable(&path, "#!/bin/sh\nexit 0\n").expect("write script");
                let status = Command::new(&path)
                    .status()
                    .unwrap_or_else(|error| panic!("run {run}: {error}"));
                assert!(status.success(), "run {run}: {status}");
            }
        });
    }

    #[test]
    fn executable_has_exact_content_and_mode() {
        let dir = crate::tempdir().expect("fixture directory");
        let path = dir.path().join("exact");
        let content = "#!/bin/sh\nprintf '%s' \"$1\"\n";
        write_executable(&path, content).expect("write script");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), content);
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, EXECUTABLE_MODE);
        let output = Command::new(&path).arg("ok").output().expect("run script");
        assert_eq!(output.stdout, b"ok");
    }

    #[test]
    fn file_content_is_byte_exact_for_binary_and_large_input() {
        let dir = crate::tempdir().expect("fixture directory");
        let binary = dir.path().join("binary");
        let bytes = [0_u8, 255, 10, 0, 13, 128];
        write_file(&binary, bytes).expect("write binary");
        assert_eq!(std::fs::read(&binary).expect("read"), bytes);
        let large = dir.path().join("large");
        let content = vec![b'x'; LARGE_CONTENT_BYTES];
        write_file(&large, &content).expect("write large");
        assert_eq!(std::fs::read(&large).expect("read"), content);
        write_file(&large, b"").expect("truncate");
        assert!(std::fs::read(&large).expect("read").is_empty());
    }

    #[test]
    fn an_existing_file_is_replaced_not_appended() {
        let dir = crate::tempdir().expect("fixture directory");
        let path = dir.path().join("replaced");
        write_file(&path, "a much longer first content").expect("first write");
        write_file(&path, "short").expect("second write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "short");
    }

    #[test]
    fn a_missing_directory_is_an_error_naming_the_path() {
        let dir = crate::tempdir().expect("fixture directory");
        let path = dir.path().join("missing-dir").join("file");
        let error = write_executable(&path, "x").unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(!path.exists());
    }
}
