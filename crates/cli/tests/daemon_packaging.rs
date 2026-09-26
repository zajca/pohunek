//! Argument plumbing of `packaging/install-daemon.sh`.
//!
//! The wrapper delegates installation to `pohunek service install|upgrade`,
//! whose transactions are tested in the service engine. These tests run the
//! real script against a fake `pohunek` in a fake archive and a fake
//! `systemctl` on `PATH`, recording every invocation.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[test]
fn fresh_install_runs_service_install_from_the_archive() {
    let fixture = Fixture::new();
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.install_call()]
    );
    assert!(
        !fixture.systemctl_log.exists(),
        "a fresh install never touches systemctl"
    );
}

#[test]
fn existing_service_config_runs_service_upgrade() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.upgrade_call()]
    );
}

#[test]
fn a_pending_install_is_finished_by_service_install_even_with_service_config() {
    // From its `config` step on, an interrupted install has written
    // service.toml; `service upgrade` refuses its record.
    for step in ["config", "registered", "ready"] {
        let fixture = Fixture::new();
        write(&fixture.config_home.join("pohunek/service.toml"), "");
        let output = fixture.run(
            &[],
            &[
                ("POHUNEK_TEST_PENDING_OPERATION", "install"),
                ("POHUNEK_TEST_PENDING_STEP", step),
            ],
        );
        assert_success(&output);
        assert_eq!(
            fixture.pohunek_calls(),
            [STATUS.to_owned(), fixture.install_call()],
            "pending install at {step}"
        );
    }
}

#[test]
fn a_pending_upgrade_keeps_service_upgrade() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_PENDING_OPERATION", "upgrade"),
            ("POHUNEK_TEST_PENDING_STEP", "registered"),
        ],
    );
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.upgrade_call()]
    );
}

#[test]
fn an_unanswered_status_query_aborts_before_anything_changes() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_STATUS_QUERY_EXIT", "5"),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service_record_invalid"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    assert!(fixture.systemctl_calls().is_empty());
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }

    // A report without `pending_transaction` cannot rule a pending install out.
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(&[], &[("POHUNEK_TEST_STATUS_UNEXPECTED", "1")]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("pending_transaction"));
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
}

const STATUS: &str = "service status --json";
const IS_ACTIVE: &str = "--user is-active --quiet pohunekd.service";
const LIST_WORKERS: &str = "--user list-units pohunek-session@* --all --plain --no-legend";
const DISABLE: &str = "--user disable --now pohunekd.service";
const RELOAD: &str = "--user daemon-reload";

#[test]
fn idle_legacy_install_is_retired_after_preflight_before_installing() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[("POHUNEK_TEST_LEGACY_ACTIVE", "1")],
    );
    assert_success(&output);
    let preflight_calls = fixture.pohunek_calls();
    let preflight = preflight_calls.get(1).expect("preflight call");
    assert!(
        preflight.starts_with(&fixture.socket.preflight_prefix())
            && preflight.ends_with(" --accept-runtime-loss"),
        "unexpected preflight call: {preflight}"
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [IS_ACTIVE, LIST_WORKERS, DISABLE, LIST_WORKERS, RELOAD]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
    // The connect barrier is undone by design, not by cleanup: the node was
    // renamed away for the preflight and removed once the legacy daemon was
    // retired.
    assert!(
        !fixture.socket.path.exists(),
        "barrier socket was not removed after retirement"
    );
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after retirement"
    );
}

#[test]
fn live_legacy_template_workers_refuse_before_anything_changes() {
    // The activating state is exactly what `--state=active` misses and what a
    // stop can destroy mid-startup, so it must refuse like a running worker.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            (
                "POHUNEK_TEST_LIVE_WORKERS",
                "pohunek-session@s-1.service loaded activating start-pre worker",
            ),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pohunek-session@s-1.service"), "{stderr}");
    assert!(stderr.contains("pohunek session stop <id>"), "{stderr}");
    assert!(
        fixture
            .pohunek_calls()
            .get(1)
            .is_some_and(|call| call.starts_with(&fixture.socket.preflight_prefix())),
        "unexpected preflight call: {:?}",
        fixture.pohunek_calls()
    );
    assert_eq!(fixture.systemctl_calls(), [IS_ACTIVE, LIST_WORKERS]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    // The barrier is undone on the abort path so the operator's still-running
    // legacy install stays reachable through its original socket name.
    assert!(fixture.socket.path.exists(), "socket was not restored");
}

#[test]
fn failed_preflight_stops_before_retiring_the_legacy_install() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_PREFLIGHT_STATUS", "23"),
        ],
    );
    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert!(
        fixture
            .pohunek_calls()
            .get(1)
            .is_some_and(|call| call.starts_with(&fixture.socket.preflight_prefix())),
        "unexpected preflight call: {:?}",
        fixture.pohunek_calls()
    );
    assert_eq!(fixture.systemctl_calls(), [IS_ACTIVE]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was not restored");
}

#[test]
fn a_worker_that_survives_the_stop_refuses_without_removing_legacy_files() {
    // A session created between the preflight and the daemon stop gets its
    // template worker after the first inventory, so only the re-check after
    // the stop can catch it; the run then aborts fail-closed.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            (
                "POHUNEK_TEST_POST_STOP_WORKERS",
                "pohunek-session@s-9.service loaded active running worker",
            ),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pohunek-session@s-9.service"), "{stderr}");
    // The post-stop message reports the true state: the daemon is stopped and
    // disabled, the listed worker appeared after the preflight, and the
    // legacy unit files were kept.
    assert!(
        stderr.contains("the legacy daemon is stopped and disabled"),
        "{stderr}"
    );
    assert!(stderr.contains("their runtime may be lost"), "{stderr}");
    assert!(
        stderr.contains("the legacy unit files were kept"),
        "{stderr}"
    );
    assert!(
        stderr.contains("systemctl --user reset-failed <unit>"),
        "{stderr}"
    );
    assert!(
        fixture
            .pohunek_calls()
            .get(1)
            .is_some_and(|call| call.starts_with(&fixture.socket.preflight_prefix())),
        "unexpected preflight call: {:?}",
        fixture.pohunek_calls()
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [IS_ACTIVE, LIST_WORKERS, DISABLE, LIST_WORKERS]
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    // After the stop the moved node is stale, so it is removed, not restored
    // to a daemon that no longer listens.
    assert!(
        !fixture.socket.path.exists(),
        "socket node was restored after the stop"
    );
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the stop"
    );
}

#[test]
fn accepted_runtime_loss_still_refuses_a_worker_that_survives_the_stop() {
    // The consent covers daemon-owned PTYs only; removing the unit files a
    // live template worker runs from would orphan it.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            (
                "POHUNEK_TEST_POST_STOP_WORKERS",
                "pohunek-session@s-9.service loaded activating start worker",
            ),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pohunek-session@s-9.service"), "{stderr}");
    assert!(
        stderr.contains("systemctl --user enable --now pohunekd.service"),
        "{stderr}"
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [IS_ACTIVE, LIST_WORKERS, DISABLE, LIST_WORKERS]
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the stop"
    );
}

#[test]
fn rerun_after_a_partial_retirement_finishes_without_a_second_preflight() {
    // The previous run disabled the legacy daemon but stopped before removing
    // its unit files and binaries.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.install_call()]
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [IS_ACTIVE, LIST_WORKERS, DISABLE, LIST_WORKERS, RELOAD]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }

    // Only a legacy binary survived the previous run.
    let fixture = Fixture::new();
    write(&fixture.prefix.join("bin/pohunekd"), "legacy\n");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.install_call()]
    );
    assert!(fixture.systemctl_calls().is_empty());
    assert!(!fixture.prefix.join("bin/pohunekd").exists());
}

#[test]
fn a_failed_install_after_retirement_explains_how_to_recover() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_SERVICE_STATUS", "7"),
        ],
    );
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already retired"), "{stderr}");
    assert!(stderr.contains("re-run"), "{stderr}");

    let fixture = Fixture::new();
    let output = fixture.run(&[], &[("POHUNEK_TEST_SERVICE_STATUS", "7")]);
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("already retired"));
}

#[test]
fn a_missing_staged_binary_or_extra_argument_fails_without_side_effects() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.archive.join("pohunek-sessiond")).expect("remove worker");
    let output = fixture.run(&[], &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("pohunek-sessiond"));
    assert!(fixture.pohunek_calls().is_empty());

    let fixture = Fixture::new();
    let output = fixture.run(&["--unknown"], &[]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(fixture.pohunek_calls().is_empty());
}

#[test]
fn release_workflow_packages_the_wrapper_and_binaries_only() {
    let workflow = read(&repo_root().join(".github/workflows/release.yml"));
    for expected in [
        r#"cp "${bindir}/pohunek" "${staging}/""#,
        r#"cp "${bindir}/pohunek-sessiond" "${staging}/""#,
        r#"mkdir -p "${staging}/packaging""#,
        r#"cp packaging/install-daemon.sh "${staging}/packaging/""#,
    ] {
        assert!(
            workflow.contains(expected),
            "missing release asset: {expected}"
        );
    }
    assert!(
        !workflow.contains("packaging/systemd"),
        "unit templates are rendered by `pohunek service`, never shipped"
    );
    assert!(
        !repo_root().join("packaging/systemd").exists(),
        "packaging/systemd templates are replaced by the typed renderers"
    );
}

#[test]
fn release_workflow_packages_static_shell_completions() {
    let workflow = read(&repo_root().join(".github/workflows/release.yml"));
    for expected in [
        r#""${bindir}/pohunek" completions bash > "${staging}/completions/pohunek.bash""#,
        r#""${bindir}/pohunek" completions zsh > "${staging}/completions/_pohunek""#,
        r#""${bindir}/pohunek" completions fish > "${staging}/completions/pohunek.fish""#,
    ] {
        assert!(
            workflow.contains(expected),
            "missing packaged completion: {expected}"
        );
    }
}

/// Fake archive CLI: logs its arguments and answers `service status --json`
/// in the pretty envelope the real CLI prints, with a pending transaction
/// from `POHUNEK_TEST_PENDING_OPERATION`/`_STEP` or `null`.
/// `POHUNEK_TEST_STATUS_QUERY_EXIT` fails the query with an error document;
/// `POHUNEK_TEST_STATUS_UNEXPECTED` answers without `pending_transaction`.
/// `POHUNEK_TEST_PREFLIGHT_STATUS` and `POHUNEK_TEST_SERVICE_STATUS` set the
/// exit status of the migration preflight and of install/upgrade.
const FAKE_POHUNEK: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$POHUNEK_TEST_POHUNEK_LOG"
[ "$1" = migration ] && exit "${POHUNEK_TEST_PREFLIGHT_STATUS:-0}"
if [ "$1" = service ] && [ "$2" = status ]; then
    if [ -n "${POHUNEK_TEST_STATUS_QUERY_EXIT:-}" ]; then
        printf '{\n  "err": {\n    "code": "service_record_invalid"\n  }\n}\n'
        exit "$POHUNEK_TEST_STATUS_QUERY_EXIT"
    fi
    if [ -n "${POHUNEK_TEST_STATUS_UNEXPECTED:-}" ]; then
        printf '{\n  "ok": {\n    "installed": true\n  }\n}\n'
        exit 0
    fi
    pending=null
    if [ -n "${POHUNEK_TEST_PENDING_OPERATION:-}" ]; then
        pending=$(printf '{\n      "operation": "%s",\n      "version": "1.0.0",\n      "step": "%s"\n    }' \
            "$POHUNEK_TEST_PENDING_OPERATION" "$POHUNEK_TEST_PENDING_STEP")
    fi
    printf '{\n  "ok": {\n    "installed": true,\n    "pending_transaction": %s,\n    "transaction_in_progress": false\n  }\n}\n' "$pending"
    exit 0
fi
exit "${POHUNEK_TEST_SERVICE_STATUS:-0}"
"#;

struct Fixture {
    root: tempfile::TempDir,
    home: PathBuf,
    archive: PathBuf,
    prefix: PathBuf,
    config_home: PathBuf,
    commands: PathBuf,
    socket: BarrierSocket,
    pohunek_log: PathBuf,
    systemctl_log: PathBuf,
}

/// The legacy daemon's control socket and the connect-barrier variants the
/// script derives from its name.
struct BarrierSocket {
    /// Path the daemon bound (`XDG_RUNTIME_DIR/pohunek/daemon.sock`).
    path: PathBuf,
}

impl BarrierSocket {
    /// Expected start of a `migration preflight --socket` call the wrapper
    /// makes against the renamed node, whose name carries the script's pid.
    fn preflight_prefix(&self) -> String {
        let name = self.path.file_name().expect("socket name");
        format!(
            "migration preflight --socket {}/{}.retiring.",
            self.path.parent().expect("socket parent").display(),
            name.to_string_lossy()
        )
    }

    /// Whether the renamed barrier node remains next to the socket.
    fn retired_exists(&self) -> bool {
        self.path
            .parent()
            .expect("socket parent")
            .read_dir()
            .expect("list runtime dir")
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("daemon.sock.retiring.")
            })
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let archive = root.path().join("archive");
        let commands = root.path().join("commands");
        let pohunek_log = root.path().join("pohunek.log");
        let systemctl_log = root.path().join("systemctl.log");
        fs::create_dir_all(archive.join("packaging")).expect("archive");
        fs::copy(
            repo_root().join("packaging/install-daemon.sh"),
            archive.join("packaging/install-daemon.sh"),
        )
        .expect("copy wrapper");
        write_executable(&archive.join("pohunek"), FAKE_POHUNEK);
        write_executable(&archive.join("pohunekd"), "#!/bin/sh\nexit 0\n");
        write_executable(&archive.join("pohunek-sessiond"), "#!/bin/sh\nexit 0\n");
        write_executable(
            &commands.join("systemctl"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$POHUNEK_TEST_SYSTEMCTL_LOG\"\n\
             case \"$*\" in\n\
             *disable*|*stop*) : > \"$POHUNEK_TEST_SYSTEMCTL_STOP_MARKER\" ;;\n\
             *list-units*)\n\
                 if [ -f \"${POHUNEK_TEST_SYSTEMCTL_STOP_MARKER:-}\" ]; then\n\
                     printf '%s' \"${POHUNEK_TEST_POST_STOP_WORKERS:-}\"\n\
                 else\n\
                     printf '%s' \"${POHUNEK_TEST_LIVE_WORKERS:-}\"\n\
                 fi ;;\n\
             *is-active*) [ \"${POHUNEK_TEST_LEGACY_ACTIVE:-0}\" = 1 ] || exit 3 ;;\n\
             esac\n",
        );
        // The legacy daemon binds its control socket as a real AF_UNIX node,
        // so the barrier rename has a socket file to move.
        let runtime = root.path().join("runtime");
        let socket_dir = runtime.join("pohunek");
        fs::create_dir_all(&socket_dir).expect("create runtime dir");
        let socket = socket_dir.join("daemon.sock");
        UnixListener::bind(&socket).expect("bind barrier socket");
        Self {
            home: root.path().join("home"),
            prefix: root.path().join("prefix"),
            config_home: root.path().join("config"),
            archive,
            commands,
            socket: BarrierSocket { path: socket },
            pohunek_log,
            systemctl_log,
            root,
        }
    }

    fn legacy_files(&self) -> [PathBuf; 5] {
        let units = self.config_home.join("systemd/user");
        [
            units.join("pohunekd.service"),
            units.join("pohunek-session@.service"),
            units.join("pohunek-sessions.slice"),
            self.prefix.join("bin/pohunekd"),
            self.prefix.join("libexec/pohunek-sessiond"),
        ]
    }

    fn legacy_install(&self) {
        for file in self.legacy_files() {
            write(&file, "legacy\n");
        }
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.commands.display(),
            std::env::var("PATH").expect("PATH")
        );
        let mut command = Command::new("sh");
        command
            .arg(self.archive.join("packaging/install-daemon.sh"))
            .args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("POHUNEK_INSTALL_PREFIX", &self.prefix)
            .env(
                "XDG_RUNTIME_DIR",
                self.socket
                    .path
                    .parent()
                    .and_then(|dir| dir.parent())
                    .map_or(
                        std::env::temp_dir().join("missing-pohunek-runtime"),
                        ToOwned::to_owned,
                    ),
            )
            .env(
                "POHUNEK_TEST_SYSTEMCTL_STOP_MARKER",
                self.root.path().join("systemctl-stop-marker"),
            )
            .env("POHUNEK_TEST_POHUNEK_LOG", &self.pohunek_log)
            .env("POHUNEK_TEST_SYSTEMCTL_LOG", &self.systemctl_log)
            .env("PATH", path);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("run installer")
    }

    fn install_call(&self) -> String {
        format!(
            "service install --from {} --prefix {}",
            self.archive.display(),
            self.prefix.display()
        )
    }

    fn upgrade_call(&self) -> String {
        format!("service upgrade --from {}", self.archive.display())
    }

    fn pohunek_calls(&self) -> Vec<String> {
        lines(&self.pohunek_log)
    }

    fn systemctl_calls(&self) -> Vec<String> {
        lines(&self.systemctl_log)
    }
}

fn lines(path: &Path) -> Vec<String> {
    match fs::read_to_string(path) {
        Ok(text) => text.lines().map(str::to_owned).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates")
        .parent()
        .expect("repository")
        .to_path_buf()
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    fs::write(path, contents).expect("write file");
}

fn write_executable(path: &Path, contents: &str) {
    write(path, contents);
    let mut permissions = fs::metadata(path)
        .expect("executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable mode");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read fixture file")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
