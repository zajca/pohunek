//! Launch contract of a PTY child: working directory, session, descriptors,
//! and descriptor-limit behaviour, observed through real PTY spawns.
//!
//! Probes use only what Linux and macOS both provide: `ps` for process ids,
//! the shell's `[ -e /dev/fd/N ]` for descriptor numbers, and the process's own
//! `fcntl` for descriptor flags.

// Rust guideline compliant 2026-10-14

use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

use pohunek_session_worker::{Command, EnvBase, OutputEvent, PtyError, PtyOwner, WorkerConfig};
use pohunek_test_support::tempdir;
use rustix::io::FdFlags;
use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};

/// Upper bound for any single wait on child output or exit.
const OUTPUT_WAIT: Duration = Duration::from_secs(10);
/// Descriptor limit the large-limit scenario raises the soft limit to.
const LARGE_DESCRIPTOR_LIMIT: u64 = 1_048_576;
/// Smallest soft limit the budget scenario falls back to before giving up.
const MINIMUM_SOFT_LIMIT: u64 = 1024;
/// Longest acceptable launch under the large descriptor limit.
const LAUNCH_BUDGET: Duration = Duration::from_millis(100);
/// Descriptor number the lowered-limit regression holds open above the soft limit.
const HIGH_DESCRIPTOR: libc::c_int = 1024;
/// Soft limit the lowered-limit regression lowers below [`HIGH_DESCRIPTOR`].
const LOWERED_DESCRIPTOR_LIMIT: u64 = 256;
/// Soft limit the lowered-limit regression raises to so that [`HIGH_DESCRIPTOR`] is allocable.
const RAISED_DESCRIPTOR_LIMIT: u64 = 4096;
/// Lowest number the probe descriptors are allocated from.
const PROBE_MINIMUM: libc::c_int = 200;
/// Number of probe descriptors the inheritance scenario opens.
const PROBE_COUNT: usize = 3;
/// Environment flag that marks the re-executed child of a descriptor-limit test.
const LIMIT_CHILD_FLAG: &str = "POHUNEK_SESSION_WORKER_LIMIT_CHILD";
/// Environment passed to every scenario shell: a fixed search path, no inheritance.
const SHELL_PATH: &str = "/usr/bin:/bin";
/// Standard descriptors every child keeps: stdin, stdout, and stderr.
const STANDARD_DESCRIPTORS: [&str; 3] = ["0", "1", "2"];

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

/// Gives the owner back `rwx` on a directory when dropped, so the temporary
/// tree stays removable even when the scenario panics.
struct Unlock<'a>(&'a Path);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// Opens a descriptor at or above `minimum` that the child would inherit.
fn inheritable_descriptor(minimum: libc::c_int) -> OwnedFd {
    let fd = rustix::io::fcntl_dupfd_cloexec(
        File::open("/dev/null").expect("open probe source"),
        minimum,
    )
    .expect("duplicate probe descriptor");
    rustix::io::fcntl_setfd(&fd, FdFlags::empty()).expect("clear close-on-exec on the probe");
    assert!(
        rustix::io::fcntl_getfd(&fd)
            .expect("read descriptor flags")
            .is_empty(),
        "the probe must lack close-on-exec to test inheritance"
    );
    fd
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

/// Runs `body` as a re-executed child of this test, so that process-wide
/// state such as the descriptor limit stays inside that child.
fn run_in_child(test_name: &str, marker: &str, body: impl FnOnce()) {
    if std::env::var_os(LIMIT_CHILD_FLAG).is_none() {
        let output = ProcessCommand::new(std::env::current_exe().expect("test binary"))
            .args(pohunek_test_support::exact_test_filter(
                module_path!(),
                test_name,
            ))
            .arg("--nocapture")
            .env(LIMIT_CHILD_FLAG, "1")
            .output()
            .expect("run the re-executed child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "re-executed child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains(marker), "child did not finish:\n{stdout}");
        return;
    }
    body();
    println!("{marker}");
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

/// A program inside a missing working directory fails its resolution before
/// any `chdir`, and the launch is still refused as a missing directory.
#[tokio::test]
async fn a_program_inside_a_missing_working_directory_reports_the_directory_missing() {
    let home = tempdir().expect("home directory");
    let missing: PathBuf = home.path().join("worktree-removed");
    let command = Command {
        program: missing
            .join("bin")
            .join("agent")
            .to_string_lossy()
            .into_owned(),
        ..shell_command("touch \"$HOME/ran\"", &missing, home.path())
    };

    let error = spawn(command).expect_err("a missing working directory must refuse the spawn");

    assert!(matches!(error, PtyError::CwdMissing), "{error:?}");
}

/// A working directory that exists but cannot be entered fails the launch as
/// a spawn failure, not as a missing directory, and starts no process.
#[tokio::test]
async fn an_existing_working_directory_that_cannot_be_entered_is_not_reported_missing() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().expect("home directory");
    let locked = home.path().join("locked");
    let cwd = locked.join("worktree");
    std::fs::create_dir_all(&cwd).expect("create the working directory");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("make the parent unsearchable");
    let unlock = Unlock(&locked);

    let result = spawn(shell_command("touch \"$HOME/ran\"", &cwd, home.path()));
    drop(unlock);

    let error = result.expect_err("an unsearchable working directory refuses the spawn");
    assert!(
        matches!(
            error,
            PtyError::Spawn {
                not_found: false,
                ..
            }
        ),
        "an existing directory is not reported missing: {error:?}"
    );
    assert!(
        !home.path().join("ran").exists(),
        "no process may run when the working directory cannot be entered"
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

/// The child is its process group's leader and the foreground group of its
/// controlling terminal, and a resize delivers SIGWINCH to it.
///
/// `ps -o tpgid` reports the terminal's foreground group. It equals the child's
/// pid only when the child is the controlling terminal's foreground group,
/// which requires the child to have acquired the PTY as its controlling tty.
#[tokio::test]
async fn the_child_leads_the_session_and_receives_resize_signals() {
    let home = tempdir().expect("home directory");
    let cwd = tempdir().expect("working directory");
    let script = concat!(
        "trap 'printf \"winch-seen\\n\"' WINCH; ",
        "printf 'ids %s\\n' \"$(ps -o pid= -o pgid= -o tpgid= -p $$)\"; ",
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
    assert_eq!(fields.len(), 3, "{ids:?}");
    let shell_pid: u32 = fields[0].parse().expect("shell pid");
    let process_group: u32 = fields[1].parse().expect("process group");
    let foreground_group: u32 = fields[2].parse().expect("foreground process group");
    assert_eq!(shell_pid, pid, "the shell is the managed child");
    assert_eq!(process_group, pid, "the child leads its process group");
    assert_eq!(
        foreground_group, pid,
        "the child's group is the foreground group of its terminal"
    );

    pty.resize("launch-test", 1, 100, 30)
        .await
        .expect("resize the PTY");
    output_until(&pty, "winch-seen").await;

    pty.stop(
        "launch-test",
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .await
    .expect("stop the shell");
}

/// The child inherits only the three PTY descriptors, not worker descriptors
/// that lack close-on-exec, and a missing absolute program fails the spawn.
#[tokio::test]
async fn the_child_sees_only_the_pty_descriptors() {
    let home = tempdir().expect("home directory");
    let cwd = tempdir().expect("working directory");

    let probes: Vec<OwnedFd> = (0..PROBE_COUNT)
        .map(|_| inheritable_descriptor(PROBE_MINIMUM))
        .collect();
    let probe_numbers: Vec<libc::c_int> = probes.iter().map(AsRawFd::as_raw_fd).collect();

    let mut commands: Vec<String> = STANDARD_DESCRIPTORS
        .iter()
        .map(|number| format!("[ -e /dev/fd/{number} ] && printf 'open {number}\\n'"))
        .collect();
    commands.extend(
        probe_numbers
            .iter()
            .map(|number| format!("[ -e /dev/fd/{number} ] && printf 'leak {number}\\n'")),
    );
    commands.push("printf 'listed\\n'".to_owned());
    let script = commands.join("; ");

    let pty =
        spawn(shell_command(&script, cwd.path(), home.path())).expect("spawn descriptor listing");
    let output = output_until(&pty, "listed").await;
    drop(probes);

    for standard in STANDARD_DESCRIPTORS {
        assert!(
            output
                .lines()
                .any(|line| line == format!("open {standard}")),
            "descriptor {standard} missing:\n{output}"
        );
    }
    let leaked: Vec<&str> = output
        .lines()
        .filter(|line| line.starts_with("leak "))
        .collect();
    assert!(
        leaked.is_empty(),
        "inherited probe descriptors leaked into the child: {leaked:?}"
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

/// A non-close-on-exec descriptor above a lowered soft limit is not inherited.
///
/// On Linux this exercises `close_range`, which marks every descriptor from 3
/// upward regardless of the soft limit. macOS has no range call, so the
/// `platform contracts (arm64)` CI job covers the bounded per-descriptor sweep
/// there natively: the sweep must reach descriptors above the soft limit,
/// which only its precomputed bound from the open-descriptor listing allows.
#[test]
fn a_non_cloexec_descriptor_above_a_lowered_soft_limit_is_not_inherited() {
    run_in_child(
        "a_non_cloexec_descriptor_above_a_lowered_soft_limit_is_not_inherited",
        "lowered-limit-regression-clean",
        || {
            let limit = getrlimit(Resource::Nofile);
            let hard = limit.maximum.unwrap_or(u64::MAX);
            assert!(
                hard > u64::try_from(HIGH_DESCRIPTOR).expect("positive descriptor"),
                "the hard descriptor limit must exceed the probe number"
            );
            let raised = RAISED_DESCRIPTOR_LIMIT.min(hard);
            setrlimit(
                Resource::Nofile,
                Rlimit {
                    current: Some(raised),
                    maximum: limit.maximum,
                },
            )
            .expect("raise the soft descriptor limit");
            let probe = inheritable_descriptor(HIGH_DESCRIPTOR);
            let probe_number = probe.as_raw_fd();
            assert!(
                probe_number >= HIGH_DESCRIPTOR,
                "probe landed at {probe_number}"
            );
            setrlimit(
                Resource::Nofile,
                Rlimit {
                    current: Some(LOWERED_DESCRIPTOR_LIMIT),
                    maximum: limit.maximum,
                },
            )
            .expect("lower the soft descriptor limit below the probe");

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let home = tempdir().expect("home directory");
                let cwd = tempdir().expect("working directory");
                let script = format!(
                    "if [ -e /dev/fd/{probe_number} ]; then printf 'leaked\\n'; \
                     else printf 'clean\\n'; fi; printf 'done\\n'"
                );
                let pty = spawn(shell_command(&script, cwd.path(), home.path()))
                    .expect("spawn above the lowered soft limit");
                let output = output_until(&pty, "done").await;
                assert!(
                    output.lines().any(|line| line == "clean"),
                    "descriptor {probe_number} was inherited:\n{output}"
                );
            });
            drop(probe);
        },
    );
}

/// Raises the soft descriptor limit as far as the kernel accepts, and returns
/// the accepted value (`None` for `RLIM_INFINITY`).
///
/// The first attempt is `RLIM_INFINITY`, the value a runtime such as Bun can
/// set. Refused attempts fall back to [`LARGE_DESCRIPTOR_LIMIT`], then halve.
/// Darwin refuses soft limits above `kern.maxfilesperproc`, so the halving lands
/// on the largest value it allows.
fn raise_soft_limit_as_far_as_allowed() -> Option<u64> {
    let maximum = getrlimit(Resource::Nofile).maximum;
    let mut candidates = vec![None];
    let mut target = maximum.map_or(LARGE_DESCRIPTOR_LIMIT, |hard| {
        hard.min(LARGE_DESCRIPTOR_LIMIT)
    });
    while target >= MINIMUM_SOFT_LIMIT {
        candidates.push(Some(target));
        target /= 2;
    }
    for current in candidates {
        if setrlimit(Resource::Nofile, Rlimit { current, maximum }).is_ok() {
            return current;
        }
    }
    panic!("no soft descriptor limit could be set");
}

/// Launching under a large descriptor limit stays inside the launch budget.
///
/// The soft limit is raised as far as the platform allows, toward 2^20. Linux
/// marks descriptors with `close_range`, whose cost does not depend on the
/// limit. macOS sweeps, so the sweep bound must follow the open descriptors
/// rather than the limit; a bound taken from a large soft limit would make the
/// child loop over every number below it before exec.
#[test]
fn a_launch_under_a_large_descriptor_limit_meets_the_budget() {
    run_in_child(
        "a_launch_under_a_large_descriptor_limit_meets_the_budget",
        "launch-within-budget",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let soft = raise_soft_limit_as_far_as_allowed();
                let soft_label =
                    soft.map_or_else(|| "infinity".to_owned(), |value| value.to_string());

                let home = tempdir().expect("home directory");
                let cwd = tempdir().expect("working directory");
                let started = Instant::now();
                let pty = spawn(shell_command("printf 'ran\\n'", cwd.path(), home.path()))
                    .expect("spawn under a large descriptor limit");
                let elapsed = started.elapsed();
                output_until(&pty, "ran").await;
                println!("launch-elapsed {elapsed:?} soft-limit {soft_label}");
                assert!(
                    elapsed < LAUNCH_BUDGET,
                    "launch took {elapsed:?} under a soft limit of {soft_label}"
                );
            });
        },
    );
}
