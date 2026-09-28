//! `pohunek service lock` and the lock it hands down, through the real binary.
//!
//! Every run uses an isolated `HOME` and XDG layout below a temporary root and
//! a cleared environment, so the tests never see the operator's installation
//! or a `POHUNEK_*` variable of the calling shell. Nothing here installs a
//! service: `service check` only reads, and the lock itself touches only the
//! application state directory.

// Rust guideline compliant 2026-09-28

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The real CLI under test.
const POHUNEK: &str = env!("CARGO_BIN_EXE_pohunek");

/// Variable the lock names its handed-down descriptor in.
const LOCK_FD_ENV: &str = "POHUNEK_SERVICE_LOCK_FD";

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

    fn prefix(&self) -> PathBuf {
        self.root.join("home/.local")
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

    fn lock_file(&self) -> PathBuf {
        self.root.join("state/pohunek/service-install.lock")
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

#[test]
fn the_locked_command_runs_its_own_service_commands_under_the_lock() {
    let host = Host::new();
    let prefix = host.prefix().display().to_string();
    let output = host.locked_shell(&format!(
        "\"$POHUNEK\" service check --prefix '{prefix}' --json"
    ));
    assert_code(&output, 0);
    let stdout = text(&output.stdout);
    assert!(stdout.contains("\"locked\": true"), "{stdout}");
    assert!(stdout.contains("\"operation\": \"install\""), "{stdout}");
    assert!(
        stdout.contains(&format!("\"prefix\": \"{prefix}\"")),
        "{stdout}"
    );
    assert!(!host.prefix().exists(), "check created the prefix");
    assert!(!host.root.join("config/pohunek/service.toml").exists());

    // A nested lock reuses the handed-down lock instead of waiting for it.
    let output = host.locked_shell("\"$POHUNEK\" service lock -- sh -c 'exit 5'");
    assert_code(&output, 5);
}

#[test]
fn another_transaction_is_refused_while_the_command_runs_and_starts_after() {
    let host = Host::new();
    // The competitor opens its own description of the lock file, like any
    // `pohunek service` command started from elsewhere.
    let output = host.locked_shell(&format!(
        "(unset {LOCK_FD_ENV}; exec \"$POHUNEK\" service lock -- true); echo \"competitor=$?\""
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
fn a_variable_that_names_no_held_lock_fails_instead_of_locking_anew() {
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
    // Create the state directory and its lock file first.
    assert_code(
        &host
            .pohunek(&["service", "lock", "--", "true"])
            .output()
            .expect("lock"),
        0,
    );

    for value in ["not-a-number", "1", "97"] {
        let output = host
            .pohunek(&["service", "check", "--json"])
            .env(LOCK_FD_ENV, value)
            .output()
            .expect("run check");
        assert_refused(&output, value);
    }

    // An open descriptor of an unrelated file, and one of the lock file
    // itself that no process holds.
    let unrelated = host.root.join("unrelated");
    fs::write(&unrelated, "").expect("write unrelated file");
    for (case, file) in [
        ("unrelated file", unrelated.as_path()),
        ("unlocked lock file", host.lock_file().as_path()),
    ] {
        let output = open_as_fd_7(&host, file, &["service", "check", "--json"]);
        assert_refused(&output, case);
    }

    // A descriptor of the lock file while another process holds it.
    let script = format!(
        "exec 7<'{}'; {LOCK_FD_ENV}=7 \"$POHUNEK\" service check --json",
        host.lock_file().display()
    );
    let output = host
        .pohunek(&[
            "service",
            "lock",
            "--",
            "sh",
            "-c",
            &format!("unset {LOCK_FD_ENV}; {script}"),
        ])
        .env("POHUNEK", POHUNEK)
        .output()
        .expect("run check beside the holder");
    assert_refused(&output, "foreign description");
}

/// Runs `pohunek args` with `file` open read-only as descriptor 7 and named
/// in the lock variable.
fn open_as_fd_7(host: &Host, file: &Path, args: &[&str]) -> Output {
    let words: Vec<String> = args.iter().map(|arg| format!("'{arg}'")).collect();
    let script = format!(
        "exec 7<'{}'; exec \"$POHUNEK\" {}",
        file.display(),
        words.join(" ")
    );
    let mut command = Command::new("sh");
    command.args(["-c", &script]);
    host.isolate(&mut command);
    command
        .env("POHUNEK", POHUNEK)
        .env(LOCK_FD_ENV, "7")
        .output()
        .expect("run pohunek with an inherited descriptor")
}
