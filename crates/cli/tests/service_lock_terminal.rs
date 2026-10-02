//! `pohunek service lock` on a real terminal: foreground handoff and job control.
//!
//! A pseudo-terminal becomes the controlling terminal of a shell that runs
//! the real `pohunek service lock` in the foreground, as an operator's shell
//! would. The tests type into the terminal (`Ctrl-C`, `Ctrl-Z`, a line) and
//! observe the processes through files the scripts write and `ps`, which
//! reports process states the same way on Linux and macOS.

// Rust guideline compliant 2026-09-29

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use pohunek_test_support::env::TestEnv;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

/// The real CLI under test.
/// Path of the `pohunek` binary under test, resolved at run time.
fn pohunek_bin() -> std::path::PathBuf {
    pohunek_test_support::bin_exe("pohunek")
}

/// How long a second, unwanted signal delivery gets to show up.
const SETTLE: Duration = Duration::from_millis(300);

/// Terminal size of the test terminal; any size works.
const SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// `Ctrl-C` as the terminal's interrupt character.
const CTRL_C: &[u8] = b"\x03";

/// `Ctrl-Z` as the terminal's suspend character.
const CTRL_Z: &[u8] = b"\x1a";

/// The command `service lock` runs: it reports its identity, counts
/// `SIGINT`, reads two lines from the terminal, and waits for `stop`.
const INNER: &str = r#"d=$1
echo $PPID > "$d/lock.pid"
trap 'echo INT >> "$d/count"' INT
echo $$ > "$d/child.pid"
read first
echo "$first" > "$d/first"
while [ ! -e "$d/phase2" ]; do sleep 0.05; done
read second
echo "$second" > "$d/second"
while [ ! -e "$d/stop" ]; do sleep 0.05; done
"#;

/// The shell that owns the terminal: it runs the lock in its foreground,
/// then reports that the lock ended and waits for `end`.
const OUTER: &str = r#"d=$1
"$POHUNEK" service lock -- sh "$d/inner.sh" "$d"
echo $? > "$d/lock.status"
while [ ! -e "$d/end" ]; do sleep 0.05; done
"#;

/// Waits until `check` holds; the hang guard names the wait if it never does.
fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    pohunek_test_support::wait::poll_until(what, || check().then_some(()));
}

/// Reads the first line of `path` once it is complete.
fn line(path: &Path) -> String {
    let mut text = String::new();
    wait_until(&path.display().to_string(), || {
        text = fs::read_to_string(path).unwrap_or_default();
        text.ends_with('\n')
    });
    text.lines().next().unwrap_or_default().to_owned()
}

/// Whether `ps` reports `pid` stopped.
fn stopped(pid: i32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim_start()
                .starts_with('T')
        })
}

/// A shell on its own pseudo-terminal running `service lock`.
struct Session {
    _env: TestEnv,
    dir: PathBuf,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    shell: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Session {
    fn start() -> Self {
        // `TestEnv` roots are canonical, so no symlinked ancestor (macOS `/var`)
        // is refused.
        let env = TestEnv::new().expect("create the hermetic test environment");
        let root = env.root().to_path_buf();
        let dir = root.join("d");
        fs::create_dir(&dir).expect("create directory");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .expect("make directory private");
        fs::write(dir.join("inner.sh"), INNER).expect("write inner script");
        fs::write(dir.join("outer.sh"), OUTER).expect("write outer script");

        let pair = native_pty_system().openpty(SIZE).expect("open a pty");
        let mut command = CommandBuilder::new("sh");
        command.args([dir.join("outer.sh"), dir.clone()]);
        command.env_clear();
        for (name, value) in env.environment() {
            command.env(name, value);
        }
        command.env("POHUNEK", pohunek_bin());
        command.cwd(env.cwd());
        let shell = pair.slave.spawn_command(command).expect("start the shell");
        drop(pair.slave);
        // Drain the terminal's output so echoes never fill its buffer.
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        std::thread::spawn(move || {
            let mut sink = [0_u8; 1024];
            while reader.read(&mut sink).is_ok_and(|read| read > 0) {}
        });
        let writer = pair.master.take_writer().expect("pty writer");
        Self {
            _env: env,
            dir,
            master: pair.master,
            writer,
            shell,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn pid(&self, name: &str) -> i32 {
        line(&self.path(name)).parse().expect("pid")
    }

    fn touch(&self, name: &str) {
        fs::write(self.path(name), "").expect("touch");
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("type");
        self.writer.flush().expect("flush");
    }

    /// The terminal's foreground process group.
    fn foreground(&self) -> Option<i32> {
        self.master.process_group_leader()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Every loop ends on these files, so no process outlives a failure.
        for name in ["phase2", "stop", "end"] {
            let _ = fs::write(self.path(name), "");
        }
        // A failed test may leave the command stopped or reading, where it
        // never sees those files.
        // The command leads its own group; the lock process is one process.
        for (name, target) in [("child.pid", "-"), ("lock.pid", "")] {
            if let Some(pid) = fs::read_to_string(self.path(name))
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("{target}{pid}")])
                    .status();
            }
        }
        let _ = self.shell.kill();
        let _ = self.shell.wait();
    }
}

#[test]
fn the_command_owns_the_terminal_and_gets_each_interrupt_once() {
    let mut session = Session::start();
    let child = session.pid("child.pid");
    let lock = session.pid("lock.pid");
    let shell = i32::try_from(session.shell.process_id().expect("shell pid")).expect("pid");

    // The command's own group owns the terminal and reads from it.
    wait_until("the handoff", || session.foreground() == Some(child));
    session.type_bytes(b"hello\n");
    assert_eq!(line(&session.path("first")), "hello");

    // `Ctrl-C` reaches the command's group once, and not the lock process,
    // which would forward a second one.
    session.type_bytes(CTRL_C);
    assert_eq!(line(&session.path("count")), "INT");
    std::thread::sleep(SETTLE);
    let count = fs::read_to_string(session.path("count")).expect("count");
    assert_eq!(count.lines().count(), 1, "{count}");
    assert!(!stopped(lock));

    session.touch("phase2");
    session.type_bytes(b"again\n");
    assert_eq!(line(&session.path("second")), "again");
    session.touch("stop");
    assert_eq!(line(&session.path("lock.status")), "0");
    // The shell's group, which the lock process belongs to, owns the
    // terminal again.
    assert_eq!(session.foreground(), Some(shell));
}

#[test]
fn ctrl_z_stops_the_job_and_sigcont_resumes_it_with_the_terminal() {
    let mut session = Session::start();
    let child = session.pid("child.pid");
    let lock = session.pid("lock.pid");
    let shell = i32::try_from(session.shell.process_id().expect("shell pid")).expect("pid");
    wait_until("the handoff", || session.foreground() == Some(child));
    session.type_bytes(b"hello\n");
    assert_eq!(line(&session.path("first")), "hello");

    // `Ctrl-Z` stops the command; the lock process takes the terminal back
    // for its own group and stops as well, as a stopped job does.
    session.type_bytes(CTRL_Z);
    wait_until("the command to stop", || stopped(child));
    wait_until("the lock process to stop", || stopped(lock));
    assert_eq!(session.foreground(), Some(shell));

    // Continuing the lock process continues the command in the foreground.
    let sent = Command::new("kill")
        .args(["-CONT", &lock.to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success());
    wait_until("the command to own the terminal", || {
        session.foreground() == Some(child)
    });
    wait_until("the command to run", || !stopped(child));
    assert!(!stopped(lock));

    session.touch("phase2");
    session.type_bytes(b"after\n");
    assert_eq!(line(&session.path("second")), "after");
    session.touch("stop");
    assert_eq!(line(&session.path("lock.status")), "0");
    assert_eq!(session.foreground(), Some(shell));
}
