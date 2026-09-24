//! Argument plumbing of `packaging/install-daemon.sh`.
//!
//! The wrapper delegates installation to `pohunek service install|upgrade`,
//! whose transactions are tested in the service engine. These tests run the
//! real script against a fake `pohunek` in a fake archive and a fake
//! `systemctl` on `PATH`, recording every invocation.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[test]
fn fresh_install_runs_service_install_from_the_archive() {
    let fixture = Fixture::new();
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(fixture.pohunek_calls(), [fixture.install_call()]);
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
        [format!(
            "service upgrade --from {}",
            fixture.archive.display()
        )]
    );
}

const IS_ACTIVE: &str = "--user is-active --quiet pohunekd.service";
const LIST_WORKERS: &str = "--user list-units pohunek-session@* --state=active --plain --no-legend";
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
    assert_eq!(
        fixture.pohunek_calls(),
        [
            "migration preflight --accept-runtime-loss".to_owned(),
            fixture.install_call(),
        ]
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [IS_ACTIVE, LIST_WORKERS, DISABLE, RELOAD]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
}

#[test]
fn live_legacy_template_workers_refuse_before_anything_changes() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            (
                "POHUNEK_TEST_LIVE_WORKERS",
                "pohunek-session@s-1.service loaded active running worker",
            ),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pohunek-session@s-1.service"), "{stderr}");
    assert!(stderr.contains("pohunek session stop <id>"), "{stderr}");
    assert_eq!(fixture.pohunek_calls(), ["migration preflight"]);
    assert_eq!(fixture.systemctl_calls(), [IS_ACTIVE, LIST_WORKERS]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
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
    assert_eq!(fixture.pohunek_calls(), ["migration preflight"]);
    assert_eq!(fixture.systemctl_calls(), [IS_ACTIVE]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
}

#[test]
fn rerun_after_a_partial_retirement_finishes_without_a_second_preflight() {
    // The previous run disabled the legacy daemon but stopped before removing
    // its unit files and binaries.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(fixture.pohunek_calls(), [fixture.install_call()]);
    assert_eq!(
        fixture.systemctl_calls(),
        [IS_ACTIVE, LIST_WORKERS, DISABLE, RELOAD]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }

    // Only a legacy binary survived the previous run.
    let fixture = Fixture::new();
    write(&fixture.prefix.join("bin/pohunekd"), "legacy\n");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(fixture.pohunek_calls(), [fixture.install_call()]);
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

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    archive: PathBuf,
    prefix: PathBuf,
    config_home: PathBuf,
    commands: PathBuf,
    pohunek_log: PathBuf,
    systemctl_log: PathBuf,
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
        write_executable(
            &archive.join("pohunek"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$POHUNEK_TEST_POHUNEK_LOG\"\n\
             [ \"$1\" = migration ] && exit \"${POHUNEK_TEST_PREFLIGHT_STATUS:-0}\"\n\
             exit \"${POHUNEK_TEST_SERVICE_STATUS:-0}\"\n",
        );
        write_executable(&archive.join("pohunekd"), "#!/bin/sh\nexit 0\n");
        write_executable(&archive.join("pohunek-sessiond"), "#!/bin/sh\nexit 0\n");
        write_executable(
            &commands.join("systemctl"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$POHUNEK_TEST_SYSTEMCTL_LOG\"\n\
             case \"$*\" in\n\
             *list-units*) printf '%s' \"${POHUNEK_TEST_LIVE_WORKERS:-}\" ;;\n\
             *is-active*) [ \"${POHUNEK_TEST_LEGACY_ACTIVE:-0}\" = 1 ] || exit 3 ;;\n\
             esac\n",
        );
        Self {
            home: root.path().join("home"),
            prefix: root.path().join("prefix"),
            config_home: root.path().join("config"),
            archive,
            commands,
            pohunek_log,
            systemctl_log,
            _root: root,
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
