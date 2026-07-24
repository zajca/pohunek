use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn fresh_install_materializes_worker_units_and_binaries() {
    let fixture = Fixture::new("fresh");
    let output = fixture.run_installer(&[]);
    assert_success(&output);

    assert_executable(&fixture.prefix.join("bin/pohunek"));
    assert_executable(&fixture.prefix.join("bin/pohunekd"));
    assert_executable(&fixture.prefix.join("libexec/pohunek-sessiond"));
    let daemon_unit = read(&fixture.units.join("pohunekd.service"));
    let worker_unit = read(&fixture.units.join("pohunek-session@.service"));
    assert!(daemon_unit.contains("Type=notify"), "{daemon_unit}");
    assert!(daemon_unit.contains(&format!(
        "ExecStart={}/bin/pohunekd",
        fixture.prefix.display()
    )));
    assert!(worker_unit.contains("Restart=no"), "{worker_unit}");
    assert!(
        worker_unit.contains("Slice=pohunek-sessions.slice"),
        "{worker_unit}"
    );
    assert!(worker_unit.contains(&format!(
        "ExecStart={}/libexec/pohunek-sessiond --session-id %i",
        fixture.prefix.display()
    )));
    assert!(
        !fixture.preflight_log.exists(),
        "fresh install must not interrogate a nonexistent legacy daemon"
    );
    let systemctl = read(&fixture.systemctl_log);
    assert!(systemctl.contains("--user daemon-reload\n"), "{systemctl}");
    assert!(
        systemctl.contains("--user enable pohunekd.service\n"),
        "{systemctl}"
    );
    assert!(
        systemctl.contains("--user restart pohunekd.service\n"),
        "{systemctl}"
    );
    // Live-runtime-preserving installation (RFC §20.1): the installer restarts
    // ONLY pohunekd.service. It must never stop, restart, or try-restart a
    // worker instance or the worker slice, which would tear down live sessions.
    assert!(
        !systemctl.contains("pohunek-session@"),
        "installer must not act on worker units: {systemctl}"
    );
    assert!(
        !systemctl.contains("pohunek-sessions.slice"),
        "installer must not act on the worker slice: {systemctl}"
    );
    for verb in ["stop", "try-restart", "kill"] {
        assert!(
            !systemctl.contains(&format!("--user {verb} ")),
            "installer must not `{verb}` any unit: {systemctl}"
        );
    }
    assert_eq!(
        systemctl.matches("restart").count(),
        1,
        "installer must restart exactly one unit (pohunekd.service): {systemctl}"
    );
    let verify = read(&fixture.verify_log);
    assert!(verify.contains("pohunek-session@.service"), "{verify}");
    assert!(verify.contains("pohunek-sessions.slice"), "{verify}");
}

#[test]
fn upgrade_refuses_before_overwrite_when_preflight_rejects_live_sessions() {
    let fixture = Fixture::new("refuse");
    let installed_daemon = fixture.prefix.join("bin/pohunekd");
    write_executable(&installed_daemon, "#!/bin/sh\nprintf old-daemon\n");

    let output = fixture.run_installer_with_status(&[], "23");
    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert_eq!(read(&installed_daemon), "#!/bin/sh\nprintf old-daemon\n");
    assert_eq!(read(&fixture.preflight_log), "migration preflight\n");
    assert!(
        !fixture.systemctl_log.exists(),
        "failed preflight must not reload or restart systemd"
    );
}

#[test]
fn accepted_upgrade_records_authority_before_restarting_daemon() {
    let fixture = Fixture::new("accepted");
    write_executable(
        &fixture.prefix.join("bin/pohunekd"),
        "#!/bin/sh\nprintf old-daemon\n",
    );

    let output = fixture.run_installer(&["--accept-runtime-loss"]);
    assert_success(&output);
    assert_eq!(
        read(&fixture.preflight_log),
        "migration preflight --accept-runtime-loss\n"
    );
    assert!(read(&fixture.systemctl_log).contains("--user restart pohunekd.service\n"));
}

#[test]
fn release_workflow_packages_complete_daemon_runtime_set() {
    let workflow = read(&repo_root().join(".github/workflows/release.yml"));
    for expected in [
        r#"cp "${bindir}/pohunek" "${staging}/""#,
        r#"cp "${bindir}/pohunek-sessiond" "${staging}/""#,
        r#"cp packaging/install-daemon.sh "${staging}/packaging/""#,
        r#"cp packaging/systemd/pohunekd.service.in "${staging}/packaging/systemd/""#,
        r#"cp packaging/systemd/pohunek-session@.service.in "${staging}/packaging/systemd/""#,
        r#"cp packaging/systemd/pohunek-sessions.slice "${staging}/packaging/systemd/""#,
    ] {
        assert!(
            workflow.contains(expected),
            "missing release asset: {expected}"
        );
    }
}

struct Fixture {
    root: PathBuf,
    archive: PathBuf,
    prefix: PathBuf,
    units: PathBuf,
    commands: PathBuf,
    preflight_log: PathBuf,
    systemctl_log: PathBuf,
    verify_log: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = temp_dir(tag);
        let archive = root.join("archive");
        let prefix = root.join("prefix");
        let config = root.join("config");
        let units = config.join("systemd/user");
        let commands = root.join("commands");
        fs::create_dir_all(archive.join("packaging/systemd")).expect("create archive");
        fs::create_dir_all(&commands).expect("create command dir");

        let repository = repo_root();
        copy(
            &repository.join("packaging/install-daemon.sh"),
            &archive.join("packaging/install-daemon.sh"),
        );
        for name in [
            "pohunekd.service.in",
            "pohunek-session@.service.in",
            "pohunek-sessions.slice",
        ] {
            copy(
                &repository.join("packaging/systemd").join(name),
                &archive.join("packaging/systemd").join(name),
            );
        }

        let preflight_log = root.join("preflight.log");
        write_executable(
            &archive.join("pohunek"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$POHUNEK_TEST_PREFLIGHT_LOG\"\nexit \"${POHUNEK_TEST_PREFLIGHT_STATUS:-0}\"\n",
        );
        write_executable(&archive.join("pohunekd"), "#!/bin/sh\nexit 0\n");
        write_executable(&archive.join("pohunek-sessiond"), "#!/bin/sh\nexit 0\n");

        let systemctl_log = root.join("systemctl.log");
        write_executable(
            &commands.join("systemctl"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$POHUNEK_TEST_SYSTEMCTL_LOG\"\n",
        );
        let verify_log = root.join("verify.log");
        write_executable(
            &commands.join("systemd-analyze"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$POHUNEK_TEST_VERIFY_LOG\"\n",
        );

        Self {
            root,
            archive,
            prefix,
            units,
            commands,
            preflight_log,
            systemctl_log,
            verify_log,
        }
    }

    fn run_installer(&self, args: &[&str]) -> Output {
        let output = self.run_installer_with_status(args, "0");
        assert_success(&output);
        output
    }

    fn run_installer_with_status(&self, args: &[&str], status: &str) -> Output {
        let path = format!(
            "{}:{}",
            self.commands.display(),
            std::env::var("PATH").expect("PATH")
        );
        Command::new("sh")
            .arg(self.archive.join("packaging/install-daemon.sh"))
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("POHUNEK_INSTALL_PREFIX", &self.prefix)
            .env("POHUNEK_TEST_PREFLIGHT_LOG", &self.preflight_log)
            .env("POHUNEK_TEST_PREFLIGHT_STATUS", status)
            .env("POHUNEK_TEST_SYSTEMCTL_LOG", &self.systemctl_log)
            .env("POHUNEK_TEST_VERIFY_LOG", &self.verify_log)
            .env("PATH", path)
            .output()
            .expect("run installer")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
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

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "pohunek-daemon-packaging-{tag}-{}-{nanos}-{counter}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp directory");
    path
}

fn copy(source: &Path, destination: &Path) {
    fs::copy(source, destination).expect("copy fixture asset");
}

fn write_executable(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("executable parent"))
        .expect("create executable parent");
    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path)
        .expect("executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable mode");
}

fn assert_executable(path: &Path) {
    let mode = fs::metadata(path)
        .expect("installed executable")
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "{} is not executable", path.display());
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
