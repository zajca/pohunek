//! Launch contract of a PTY child: working directory, session, descriptors,
//! and descriptor-limit behaviour, observed through real PTY spawns.

// Rust guideline compliant 2026-10-14

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use pohunek_session_worker::{Command, EnvBase, OutputEvent, PtyError, PtyOwner, WorkerConfig};
use pohunek_test_support::tempdir;
use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};

/// Upper bound for any single wait on child output or exit.
const OUTPUT_WAIT: Duration = Duration::from_secs(10);
/// Descriptor limit the large-limit scenario raises the soft limit to.
const LARGE_DESCRIPTOR_LIMIT: u64 = 1_048_576;
/// Longest acceptable launch under the large descriptor limit.
const LAUNCH_BUDGET: Duration = Duration::from_millis(100);
/// Environment flag that marks the re-executed child of a descriptor-limit test.
const LIMIT_CHILD_FLAG: &str = "POHUNEK_SESSION_WORKER_LIMIT_CHILD";
/// Descriptor number the parent uses for its non-close-on-exec probe.
const PROBE_DESCRIPTOR: libc::c_int = 200;
/// Environment passed to every scenario shell: a fixed search path, no inheritance.
const SHELL_PATH: &str = "/usr/bin:/bin";

fn shell_command(script: &str, cwd: &Path, home: &Path) -> Command {
    Command {
        program: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), script.to_owned()],
        base: EnvBase::Empty,
        env: vec![
            ("PATH".to_owned(), SHELL_PATH.to_owned()),
            ("HOME".to_owned(), home.to_string_lossy().into_owned()),
        ],
        cwd: cwd.to_path_buf(),
        cols: 80,
        rows: 24,
    }
}

fn spawn(command: Command) -> Result<PtyOwner, PtyError> {
    let config = WorkerConfig::new();
    PtyOwner::spawn(
        command,
        config.history_bytes,
        config.subscriber_bytes,
        config.input_dedup_entries,
        config.input_stall,
    )
}

/// Collects retained and live output until `needle` appears, and returns it.
async fn output_until(pty: &PtyOwner, needle: &str) -> String {
    let mut subscriber = pty.subscribe_output(None).expect("subscribe to output");
    let mut collected = Vec::new();
    tokio::time::timeout(OUTPUT_WAIT, async {
        while !String::from_utf8_lossy(&collected).contains(needle) {
            match subscriber.recv().await {
                Some(OutputEvent::Replay(chunk) | OutputEvent::Output(chunk)) => {
                    collected.extend(chunk.bytes);
                }
                Some(_) => {}
                None => break,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"));
    String::from_utf8_lossy(&collected).into_owned()
}

/// A missing working directory fails the spawn and starts no process.
#[tokio::test]
async fn a_missing_working_directory_refuses_the_launch() {
    let home = tempdir().expect("home directory");
    let missing: PathBuf = home.path().join("worktree-removed");

    let error = spawn(shell_command("touch \"$HOME/ran\"", &missing, home.path()))
        .expect_err("a missing working directory must refuse the spawn");

    assert!(matches!(error, PtyError::CwdMissing), "{error:?}");
    assert!(
        !home.path().join("ran").exists(),
        "no process may run when the working directory is missing"
    );
}

/// The child's working directory is the requested one, with no fallback.
#[tokio::test]
async fn the_child_starts_in_the_requested_directory() {
    let home = tempdir().expect("home directory");
    let cwd = tempdir().expect("working directory");
    let canonical = cwd
        .path()
        .canonicalize()
        .expect("canonical working directory");

    let pty = spawn(shell_command(
        "pwd -P; printf 'done\\n'",
        cwd.path(),
        home.path(),
    ))
    .expect("spawn in an existing directory");
    let output = output_until(&pty, "done").await;

    let printed = output
        .lines()
        .find(|line| line.starts_with('/'))
        .expect("pwd printed a path");
    assert_eq!(Path::new(printed), canonical.as_path());
}

/// The child leads its own session and owns the PTY as its controlling
/// terminal, and a resize delivers SIGWINCH to it.
#[tokio::test]
async fn the_child_leads_the_session_and_receives_resize_signals() {
    let home = tempdir().expect("home directory");
    let cwd = tempdir().expect("working directory");
    let script = concat!(
        "trap 'printf \"winch-seen\\n\"' WINCH; ",
        "printf 'ids %s %s\\n' \"$$\" \"$(cut -d' ' -f5-7 /proc/$$/stat)\"; ",
        "printf 'ready\\n'; ",
        "while :; do sleep 0.1; done"
    );
    let pty = spawn(shell_command(script, cwd.path(), home.path())).expect("spawn shell");
    let pid = pty.identity().pid;

    let output = output_until(&pty, "ready").await;
    let ids = output
        .lines()
        .find_map(|line| line.strip_prefix("ids "))
        .expect("the shell reported its ids");
    let fields: Vec<&str> = ids.split_whitespace().collect();
    assert_eq!(fields.len(), 4, "{ids:?}");
    let shell_pid: u32 = fields[0].parse().expect("shell pid");
    let process_group: i64 = fields[1].parse().expect("process group");
    let session: i64 = fields[2].parse().expect("session");
    let tty_number: i64 = fields[3].parse().expect("controlling terminal");
    assert_eq!(shell_pid, pid, "the shell is the managed child");
    assert_eq!(
        process_group,
        i64::from(pid),
        "the child leads its process group"
    );
    assert_eq!(session, i64::from(pid), "the child leads its session");
    assert_ne!(tty_number, 0, "the PTY is the controlling terminal");

    pty.resize("launch-test", 1, 100, 30)
        .await
        .expect("resize the PTY");
    output_until(&pty, "winch-seen").await;

    pty.stop("launch-test", Duration::from_secs(1))
        .await
        .expect("stop the shell");
}

/// The child inherits only the three PTY descriptors, not a worker descriptor
/// that lacks close-on-exec, and a missing absolute program fails the spawn.
#[tokio::test]
async fn the_child_sees_only_the_pty_descriptors() {
    let home = tempdir().expect("home directory");
    let cwd = tempdir().expect("working directory");

    // A duplicate above the range the child's shell uses for its own
    // descriptors, with close-on-exec cleared so that it would be inherited.
    let probe = rustix::io::fcntl_dupfd_cloexec(
        std::fs::File::open("/dev/null").expect("open probe"),
        PROBE_DESCRIPTOR,
    )
    .expect("duplicate probe descriptor");
    rustix::io::fcntl_setfd(&probe, rustix::io::FdFlags::empty())
        .expect("clear close-on-exec on the probe");
    assert!(
        rustix::io::fcntl_getfd(&probe)
            .expect("read descriptor flags")
            .is_empty(),
        "the probe must lack close-on-exec to test inheritance"
    );
    let probe_number = probe.as_raw_fd();

    let pty = spawn(shell_command(
        "for f in /proc/$$/fd/*; do printf 'fd %s\\n' \"${f##*/}\"; done; printf 'listed\\n'",
        cwd.path(),
        home.path(),
    ))
    .expect("spawn descriptor listing");
    let output = output_until(&pty, "listed").await;
    drop(probe);

    let listed: Vec<&str> = output
        .lines()
        .filter_map(|line| line.strip_prefix("fd "))
        .collect();
    for standard in ["0", "1", "2"] {
        assert!(
            listed.contains(&standard),
            "descriptor {standard} missing: {listed:?}"
        );
    }
    assert!(
        !listed.contains(&probe_number.to_string().as_str()),
        "inherited descriptor {probe_number} leaked into the child: {listed:?}"
    );

    let missing = spawn(Command {
        program: home
            .path()
            .join("no-such-program")
            .to_string_lossy()
            .into_owned(),
        args: vec!["no-such-program".to_owned()],
        base: EnvBase::Empty,
        env: vec![],
        cwd: cwd.path().to_path_buf(),
        cols: 80,
        rows: 24,
    })
    .expect_err("a missing absolute program must fail the spawn");
    assert!(
        matches!(
            missing,
            PtyError::Spawn {
                not_found: true,
                ..
            }
        ),
        "{missing:?}"
    );
}

/// Launching under a 2^20 descriptor limit stays inside the launch budget.
#[test]
fn a_launch_under_a_large_descriptor_limit_meets_the_budget() {
    if std::env::var_os(LIMIT_CHILD_FLAG).is_none() {
        // Re-execute this test in its own process so the raised limit cannot
        // leak into other tests of this binary.
        let output = ProcessCommand::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "a_launch_under_a_large_descriptor_limit_meets_the_budget",
                "--nocapture",
            ])
            .env(LIMIT_CHILD_FLAG, "1")
            .output()
            .expect("run the limit child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "limit child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("launch-within-budget"), "{stdout}");
        return;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let limit = getrlimit(Resource::Nofile);
        let hard = limit.maximum.unwrap_or(LARGE_DESCRIPTOR_LIMIT);
        let soft = LARGE_DESCRIPTOR_LIMIT.min(hard);
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(soft),
                maximum: limit.maximum,
            },
        )
        .expect("raise the soft descriptor limit");

        let home = tempdir().expect("home directory");
        let cwd = tempdir().expect("working directory");
        let started = Instant::now();
        let pty = spawn(shell_command("printf 'ran\\n'", cwd.path(), home.path()))
            .expect("spawn under a large descriptor limit");
        let elapsed = started.elapsed();
        output_until(&pty, "ran").await;
        assert!(
            elapsed < LAUNCH_BUDGET,
            "launch took {elapsed:?} under a soft limit of {soft}"
        );
        println!("launch-within-budget {elapsed:?}");
    });
}
