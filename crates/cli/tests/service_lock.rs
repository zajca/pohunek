//! `pohunek service lock` and the lock it hands down, through the real binary.
//!
//! Every run uses an isolated `HOME` and XDG layout below a canonical
//! temporary root and a cleared environment, so the tests never see the
//! operator's installation, a service manager, or a `POHUNEK_*` variable of
//! the calling shell. Nothing here installs a service or needs `/dev/fd` or
//! `/proc`: the lock touches only the application state directory, and
//! adoption is observed through the commands' own results.

// Rust guideline compliant 2026-09-29

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The real CLI under test.
/// Path of the `pohunek` binary under test, resolved at run time.
fn pohunek_bin() -> std::path::PathBuf {
    pohunek_test_support::bin_exe("pohunek")
}

/// Variable the lock passes its holder token in.
const LOCK_TOKEN_ENV: &str = "POHUNEK_SERVICE_LOCK_TOKEN";

/// Bound on waiting for a helper process to report.
const WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Poll interval of that wait.
const POLL: Duration = Duration::from_millis(20);

/// An isolated user environment.
struct Host {
    _temp: tempfile::TempDir,
    root: PathBuf,
}

impl Host {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        // macOS temporary directories live below the `/var` symlink, which the
        // trusted-directory checks refuse.
        let root = fs::canonicalize(temp.path()).expect("canonical temp dir");
        for directory in ["home", "run"] {
            fs::create_dir(root.join(directory)).expect("create directory");
            fs::set_permissions(root.join(directory), fs::Permissions::from_mode(0o700))
                .expect("make directory private");
        }
        Self { _temp: temp, root }
    }

    /// `pohunek` with `args` in this host's environment.
    fn pohunek(&self, args: &[&str]) -> Command {
        let mut command = Command::new(pohunek_bin());
        command.args(args);
        self.isolate(&mut command);
        command
    }

    /// Replaces `command`'s environment with this host's.
    fn isolate(&self, command: &mut Command) {
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("PATH"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"));
    }

    /// `pohunek service lock -- sh -c <script>`, with `$POHUNEK` naming the CLI.
    fn locked_shell(&self, script: &str) -> Output {
        self.pohunek(&["service", "lock", "--", "sh", "-c", script])
            .env("POHUNEK", pohunek_bin())
            .output()
            .expect("run pohunek service lock")
    }

    fn holder_record(&self) -> PathBuf {
        self.root.join("state/pohunek/service-install.lock.holder")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn assert_code(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

/// Waits until `path` holds a line and returns it.
fn wait_for_line(path: &Path) -> String {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if text.ends_with('\n') {
                return text.trim_end().to_owned();
            }
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(POLL);
    }
}

#[test]
fn the_locked_command_runs_its_own_service_commands_under_the_lock() {
    let host = Host::new();
    // A nested lock adopts the held lock with the token instead of waiting
    // for it, and hands the same token on.
    let output = host.locked_shell(
        "\"$POHUNEK\" service lock -- sh -c '\"$POHUNEK\" service lock -- sh -c \"exit 5\"'",
    );
    assert_code(&output, 5);

    // `service check` adopts the lock before its own checks, so under the
    // lock it reaches the prefix validation, and outside it a forged token
    // fails first.
    let output = host.locked_shell("\"$POHUNEK\" service check --prefix relative --json");
    assert_code(&output, 1);
    assert!(
        text(&output.stdout).contains("\"cli_usage\""),
        "{}",
        text(&output.stdout)
    );
    let output = host
        .pohunek(&["service", "check", "--prefix", "relative", "--json"])
        .env(LOCK_TOKEN_ENV, "0".repeat(64))
        .output()
        .expect("run check");
    assert_code(&output, 1);
    assert!(
        text(&output.stdout).contains("service_inherited_lock_invalid"),
        "{}",
        text(&output.stdout)
    );
    assert!(
        !host.holder_record().exists(),
        "the holder record outlived the lock"
    );
}

#[test]
fn a_background_descendant_neither_keeps_nor_adopts_the_lock() {
    let host = Host::new();
    let adopted = host.root.join("adopted");
    let released = host.root.join("released");
    // The command leaves a process behind that retries `service lock` with
    // the token once the holder is gone; its own stdio must not keep the
    // test's pipes open.
    let script = format!(
        "( while [ ! -e '{released}' ]; do sleep 0.05; done; \
           \"$POHUNEK\" service lock -- true; echo \"$?\" > '{adopted}' ) \
           </dev/null >/dev/null 2>'{adopted}.err' & exit 0",
        released = released.display(),
        adopted = adopted.display(),
    );
    let output = host.locked_shell(&script);
    assert_code(&output, 0);
    assert!(
        !host.holder_record().exists(),
        "the holder record outlived the lock"
    );

    // The lock is free although the descendant still runs.
    let output = host
        .pohunek(&["service", "lock", "--", "true"])
        .output()
        .expect("lock after the holder exited");
    assert_code(&output, 0);

    fs::write(&released, "").expect("release the descendant");
    assert_eq!(
        wait_for_line(&adopted),
        "1",
        "the descendant adopted the lock"
    );
    let stderr = fs::read_to_string(host.root.join("adopted.err")).expect("descendant stderr");
    assert!(stderr.contains(LOCK_TOKEN_ENV), "{stderr}");
}

#[test]
fn another_transaction_is_refused_while_the_command_runs_and_starts_after() {
    let host = Host::new();
    // The competitor opens its own description of the lock file, like any
    // `pohunek service` command started from elsewhere.
    let output = host.locked_shell(&format!(
        "(unset {LOCK_TOKEN_ENV}; exec \"$POHUNEK\" service lock -- true); echo \"competitor=$?\""
    ));
    assert_code(&output, 0);
    assert!(
        text(&output.stdout).contains("competitor=1"),
        "{}",
        text(&output.stdout)
    );
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("another `pohunek service` command is running"),
        "{stderr}"
    );

    let output = host
        .pohunek(&["service", "lock", "--", "true"])
        .output()
        .expect("lock after release");
    assert_code(&output, 0);
}

#[test]
fn the_command_status_is_the_lock_status() {
    let host = Host::new();
    assert_code(&host.locked_shell("exit 42"), 42);
    assert_code(&host.locked_shell("kill -TERM $$"), 128 + 15);

    let output = host
        .pohunek(&["service", "lock", "--", "/nonexistent/command"])
        .output()
        .expect("run pohunek service lock");
    assert_code(&output, 1);
    assert!(text(&output.stderr).contains("/nonexistent/command"));
}

#[test]
fn a_token_that_proves_no_live_holder_fails_instead_of_locking_anew() {
    let host = Host::new();
    let assert_refused = |output: &Output, case: &str| {
        assert_code(output, 1);
        let stdout = text(&output.stdout);
        assert!(
            stdout.contains("service_inherited_lock_invalid"),
            "{case}: {stdout} {}",
            text(&output.stderr)
        );
    };
    // Nothing holds the lock yet, then a lock with no holder record.
    let token = "ab".repeat(32);
    for value in ["not-a-token", token.as_str()] {
        let output = host
            .pohunek(&["service", "check", "--json"])
            .env(LOCK_TOKEN_ENV, value)
            .output()
            .expect("run check");
        assert_refused(&output, value);
    }

    // Another holder's token: a second, unrelated holder runs, and the
    // command below it presents a token of its own making.
    let script = format!("{LOCK_TOKEN_ENV}='{token}' \"$POHUNEK\" service check --json");
    let output = host.locked_shell(&script);
    assert_refused(&output, "foreign token");
    // The holder's own record is gone once it exits, so its token is dead.
    let script = format!(
        "echo \"${LOCK_TOKEN_ENV}\" > '{}'",
        host.root.join("token").display()
    );
    assert_code(&host.locked_shell(&script), 0);
    let stale = fs::read_to_string(host.root.join("token")).expect("token");
    let output = host
        .pohunek(&["service", "check", "--json"])
        .env(LOCK_TOKEN_ENV, stale.trim_end())
        .output()
        .expect("run check");
    assert_refused(&output, "stale token");
}

/// Polls `child` until it exits, failing after [`WAIT_TIMEOUT`].
fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("poll the lock process") {
            return status;
        }
        assert!(Instant::now() < deadline, "the lock process never exited");
        std::thread::sleep(POLL);
    }
}

/// Waits until `path` exists.
fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(POLL);
    }
}

impl Host {
    /// Starts `pohunek service lock -- sh -c <script>` without waiting.
    fn spawn_locked_shell(&self, script: &str) -> Child {
        self.pohunek(&["service", "lock", "--", "sh", "-c", script])
            .env("POHUNEK", pohunek_bin())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start pohunek service lock")
    }

    /// Whether a new, unrelated transaction may start right now.
    fn lock_is_free(&self) -> bool {
        self.pohunek(&["service", "lock", "--", "true"])
            .output()
            .expect("run pohunek service lock")
            .status
            .success()
    }
}

/// A command that leaves a nested `service lock` adopter running in the
/// background until `stop` exists, and exits once the adopter holds the
/// lock: an adopter that only starts after the holder's command ended is
/// refused, because the holder is releasing the lock by then.
fn background_adopter(started: &Path, stop: &Path, done: &Path) -> String {
    format!(
        "\"$POHUNEK\" service lock -- sh -c ': > {started}; \
         while [ ! -e {stop} ]; do sleep 0.05; done; : > {done}' \
         </dev/null >/dev/null 2>&1 & \
         while [ ! -e {started} ]; do sleep 0.05; done; exit 0",
        started = started.display(),
        stop = stop.display(),
        done = done.display(),
    )
}

#[test]
fn the_holder_waits_for_an_adopter_its_command_left_running() {
    let host = Host::new();
    let (started, stop, done) = (
        host.root.join("started"),
        host.root.join("stop"),
        host.root.join("done"),
    );
    let mut holder = host.spawn_locked_shell(&background_adopter(&started, &stop, &done));
    wait_for_file(&started);
    // The holder's own command has exited, yet the holder keeps the lock
    // while the adopter it left behind runs.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        holder.try_wait().expect("poll").is_none(),
        "the holder released the lock under a running adopter"
    );
    assert!(
        !host.lock_is_free(),
        "a transaction started beside the adopter"
    );

    fs::write(&stop, "").expect("stop the adopter");
    assert!(wait_exit(&mut holder).success());
    assert!(done.exists());
    assert!(
        host.lock_is_free(),
        "the lock stayed held after everything ended"
    );
    assert!(!host.holder_record().exists());
}

#[test]
fn an_adopter_keeps_other_transactions_out_after_its_holder_crashed() {
    let host = Host::new();
    let (started, stop, done) = (
        host.root.join("started"),
        host.root.join("stop"),
        host.root.join("done"),
    );
    let mut holder = host.spawn_locked_shell(&background_adopter(&started, &stop, &done));
    wait_for_file(&started);
    // SIGKILL: the holder cannot clean up, and its flock dies with it.
    holder.kill().expect("kill the holder");
    holder.wait().expect("reap the holder");
    assert!(
        !host.lock_is_free(),
        "a transaction started while an adopter of the dead holder runs"
    );

    fs::write(&stop, "").expect("stop the adopter");
    wait_for_file(&done);
    let deadline = Instant::now() + WAIT_TIMEOUT;
    while !host.lock_is_free() {
        assert!(Instant::now() < deadline, "the lock never became free");
        std::thread::sleep(POLL);
    }
}

#[test]
fn a_signal_reaches_the_command_exactly_once() {
    for signal in ["INT", "HUP", "TERM"] {
        // The lock process alone, and its whole process group, as a
        // terminal or a `kill -- -<pgid>` addresses the job.
        for to_group in [false, true] {
            let case = format!(
                "{signal} to the {}",
                if to_group { "group" } else { "process" }
            );
            let host = Host::new();
            let (ready, stop, count) = (
                host.root.join("ready"),
                host.root.join("stop"),
                host.root.join("count"),
            );
            let script = format!(
                "trap 'echo {signal} >> {count}' INT HUP TERM; : > {ready}; \
                 while [ ! -e {stop} ]; do sleep 0.05; done",
                count = count.display(),
                ready = ready.display(),
                stop = stop.display(),
            );
            let mut holder = host
                .pohunek(&["service", "lock", "--", "sh", "-c", &script])
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start pohunek service lock");
            wait_for_file(&ready);
            let target = if to_group {
                format!("-{}", holder.id())
            } else {
                holder.id().to_string()
            };
            let sent = Command::new("kill")
                .args([format!("-{signal}"), "--".to_owned(), target])
                .status()
                .expect("run kill");
            assert!(sent.success(), "{case}");
            assert_eq!(wait_for_line(&count), signal, "{case}");
            // A second delivery would land within the trap's next loop turn.
            std::thread::sleep(Duration::from_millis(300));
            fs::write(&stop, "").expect("stop the command");
            assert!(wait_exit(&mut holder).success(), "{case}");
            assert_eq!(
                fs::read_to_string(&count).expect("count").lines().count(),
                1,
                "{case}: delivered more than once"
            );
            assert!(host.lock_is_free(), "{case}: the lock stayed held");
        }
    }
}

/// Names the token file `adopted_transaction_probe` adopts with; unset in an
/// ordinary run, where the probe passes without doing anything.
const PROBE_DIR_ENV: &str = "POHUNEK_TEST_PROBE_DIR";

/// Child side of `adopted_transactions_under_one_holder_run_one_at_a_time`.
///
/// Adopts the lock for a transaction exactly as `pohunek service
/// install|upgrade|uninstall` does, reports `held` and waits for `stop`, or
/// writes the refusal's code to `refused`.
#[test]
fn adopted_transaction_probe() {
    use pohunek_cli::service::inherited::Token;
    use pohunek_cli::service::record::{Adoption, Store};

    let Some(dir) = std::env::var_os(PROBE_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    let token = Token::parse(&std::env::var(LOCK_TOKEN_ENV).expect("token")).expect("token");
    let state = PathBuf::from(std::env::var_os("XDG_STATE_HOME").expect("state")).join("pohunek");
    match Store::new(state).adopt(&token, Adoption::Transaction) {
        Ok(lock) => {
            fs::write(dir.join("held"), "held\n").expect("report");
            wait_for_file(&dir.join("stop"));
            drop(lock);
            std::process::exit(0);
        }
        Err(error) => {
            fs::write(dir.join("refused"), format!("{}\n", error.code())).expect("report");
            std::process::exit(3);
        }
    }
}

#[test]
fn adopted_transactions_under_one_holder_run_one_at_a_time() {
    let host = Host::new();
    let probe = std::env::current_exe().expect("test binary");
    let first = host.root.join("first");
    let second = host.root.join("second");
    fs::create_dir(&first).expect("first");
    fs::create_dir(&second).expect("second");
    let run = |dir: &Path| {
        format!(
            "{PROBE_DIR_ENV}='{}' '{}' --exact adopted_transaction_probe --quiet >/dev/null",
            dir.display(),
            probe.display()
        )
    };
    // Two backgrounded transactions under one holder: the second starts
    // while the first runs and is refused.
    let script = format!(
        "{first_run} & while [ ! -e '{held}' ]; do sleep 0.05; done; \
         {second_run}; : > '{stop}'; wait",
        first_run = run(&first),
        held = first.join("held").display(),
        second_run = run(&second),
        stop = first.join("stop").display(),
    );
    let output = host.locked_shell(&script);
    assert_code(&output, 0);
    assert_eq!(
        wait_for_line(&second.join("refused")),
        "service_transaction_in_progress"
    );
}

#[test]
fn a_failed_wait_for_adopters_fails_the_lock_and_still_cleans_up() {
    let host = Host::new();
    let adopted = host.root.join("state/pohunek/service-install.lock.adopted");
    // The command breaks the adopters' lock file, so the holder cannot tell
    // whether an adopter still runs once the command succeeded.
    let output = host.locked_shell(&format!("chmod 644 '{}'", adopted.display()));
    assert_code(&output, 1);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("service-install.lock.adopted"), "{stderr}");
    assert!(
        !host.holder_record().exists(),
        "the holder record outlived the lock"
    );
    fs::set_permissions(&adopted, fs::Permissions::from_mode(0o600)).expect("repair");
    assert!(host.lock_is_free(), "the lock stayed held");
}
