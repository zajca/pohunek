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
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The real CLI under test.
const POHUNEK: &str = env!("CARGO_BIN_EXE_pohunek");

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
        let mut command = Command::new(POHUNEK);
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
            .env("POHUNEK", POHUNEK)
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
