//! launchd side of the supervised lifecycle fixture.
//!
//! Workers are `gui/<uid>` jobs whose private definitions live in the
//! installation's `<state>/pohunek/launchd`, and the daemon is the
//! namespace's login agent. Its definition goes to a `LaunchAgents` directory
//! below the fixture root instead of `~/Library/LaunchAgents`: `bootstrap`
//! loads a definition from any path, and the test never touches the real
//! user's agents. These tests are not ignored; a missing GUI domain fails them.

// Rust guideline compliant 2026-09-24

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use pohunek_paths::BasePaths;
use pohunek_platform::supervisor::launchd::{LaunchdDaemon, LaunchdSupervisor};
use pohunek_platform::supervisor::{DaemonSupervisor, JobLogs, Namespace, Supervisor, WorkerKey};

use super::{uid, MANAGER_CALL_TIMEOUT};

/// Directory below the fixture root receiving the daemon's agent definition.
const AGENTS_DIR: &str = "agents";

/// launchd opens job log files but never creates their directory.
const LOG_DIR_MODE: u32 = 0o700;

/// Mode of the private worker definitions directory the backend enforces.
const DEFINITIONS_MODE: u32 = 0o700;

/// Mode of one private worker definition the backend enforces.
const DEFINITION_MODE: u32 = 0o600;

/// Labels loaded outside the namespace's backend, booted out on teardown.
#[derive(Debug, Default)]
pub(crate) struct Extra {
    labels: Vec<String>,
}

impl Extra {
    /// Records a label to boot out on teardown.
    pub(crate) fn label(&mut self, label: String) {
        self.labels.push(label);
    }

    /// Records a worker job of another namespace to boot out on teardown.
    pub(crate) fn job(&mut self, namespace: &Namespace, key: &WorkerKey) {
        self.labels.push(namespace.worker_label(key));
    }
}

/// Fails the test unless the per-user GUI domain exists.
pub(crate) fn require() {
    let domain = format!("gui/{}", uid());
    assert!(
        launchctl(&["print", &domain]).success(),
        "launchd domain {domain} is unavailable; these tests need a logged-in GUI session"
    );
}

/// Creates the launchd log directory, as `pohunek service install` does.
pub(crate) fn prepare(paths: &BasePaths) {
    pohunek_platform::filesystem::TrustedDir::open_or_create_absolute(
        paths.launchd_log_dir(),
        LOG_DIR_MODE,
    )
    .expect("create the launchd log directory");
}

/// Worker jobs of `namespace` in the installation's private definitions.
#[expect(
    clippy::unused_async,
    reason = "one signature for both targets; systemd connects to D-Bus"
)]
pub(crate) async fn workers(paths: &BasePaths, namespace: &Namespace) -> Box<dyn Supervisor> {
    Box::new(supervisor(paths, namespace))
}

/// The namespace's login agent with its definition below the fixture root.
#[expect(
    clippy::unused_async,
    reason = "one signature for both targets; systemd connects to D-Bus"
)]
pub(crate) async fn daemon(root: &Path, namespace: &Namespace) -> Box<dyn DaemonSupervisor> {
    Box::new(
        LaunchdDaemon::new(
            namespace,
            uid(),
            root.join(AGENTS_DIR),
            MANAGER_CALL_TIMEOUT,
        )
        .expect("valid daemon agent manager"),
    )
}

/// launchd writes the daemon's stdout and stderr below `<log_dir>/launchd`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "one signature for both targets; systemd keeps no daemon log files"
)]
pub(crate) fn daemon_logs(paths: &BasePaths, namespace: &Namespace) -> Option<JobLogs> {
    let label = namespace.daemon_label();
    let directory = paths.launchd_log_dir();
    Some(JobLogs {
        stdout: directory.join(format!("{label}.out.log")),
        stderr: directory.join(format!("{label}.err.log")),
    })
}

/// Log files the backend names for one worker generation.
#[expect(
    clippy::unnecessary_wraps,
    reason = "one signature for both targets; systemd keeps no worker log files"
)]
pub(crate) fn worker_logs(
    paths: &BasePaths,
    namespace: &Namespace,
    key: &WorkerKey,
) -> Option<JobLogs> {
    Some(supervisor(paths, namespace).worker_logs(key))
}

/// Worker jobs of another installation namespace sharing the GUI domain.
#[expect(
    clippy::unused_async,
    reason = "one signature for both targets; systemd connects to D-Bus"
)]
pub(crate) async fn foreign_workers(root: &Path, namespace: &Namespace) -> Box<dyn Supervisor> {
    let logs = root.join("foreign-logs");
    pohunek_platform::filesystem::TrustedDir::open_or_create_absolute(&logs, LOG_DIR_MODE)
        .expect("create the foreign log directory");
    Box::new(
        LaunchdSupervisor::new(
            namespace.clone(),
            uid(),
            root.join("foreign-definitions"),
            logs,
            MANAGER_CALL_TIMEOUT,
        )
        .expect("valid foreign supervisor"),
    )
}

/// Loads a running job from the private definitions directory whose label
/// is in the namespace but names no valid session and generation; returns
/// its label.
pub(crate) fn plant_malformed(
    extra: &mut Extra,
    paths: &BasePaths,
    namespace: &Namespace,
) -> String {
    let label = format!(
        "io.github.zajca.pohunek.{}.worker.malformed",
        namespace.as_str()
    );
    // Every value is a fixed ASCII word, so the document needs no escaping.
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\"><dict>\
         <key>Label</key><string>{label}</string>\
         <key>ProgramArguments</key><array><string>/bin/sleep</string><string>600</string></array>\
         <key>RunAtLoad</key><true/>\
         </dict></plist>\n"
    );
    let directory = paths.launchd_definitions_dir();
    pohunek_platform::filesystem::TrustedDir::open_or_create_absolute(&directory, DEFINITIONS_MODE)
        .expect("create the definitions directory");
    let path = directory.join(format!("{label}.plist"));
    std::fs::write(&path, document).expect("write the malformed definition");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(DEFINITION_MODE))
        .expect("private malformed definition");
    extra.label(label.clone());
    let status = launchctl(&[
        "bootstrap",
        &format!("gui/{}", uid()),
        path.to_str().expect("UTF-8 definition path"),
    ]);
    assert!(status.success(), "bootstrap of {label} failed: {status}");
    label
}

/// Whether a planted label is still loaded.
pub(crate) fn planted_alive(label: &str) -> bool {
    launchctl(&["print", &format!("gui/{}/{label}", uid())]).success()
}

/// Labels of the definition files in the private definitions directory.
#[expect(
    clippy::unnecessary_wraps,
    reason = "one signature for both targets; transient systemd units keep no definition files"
)]
pub(crate) fn definition_labels(paths: &BasePaths) -> Option<Vec<String>> {
    let mut labels = std::fs::read_dir(paths.launchd_definitions_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .and_then(|name| name.strip_suffix(".plist"))
                        .map(ToOwned::to_owned)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    labels.sort();
    Some(labels)
}

/// Boots out every label of `namespace` and removes the daemon agent.
pub(crate) async fn teardown(root: &Path, paths: &BasePaths, namespace: &Namespace, extra: Extra) {
    if let Ok(daemon) = LaunchdDaemon::new(
        namespace,
        uid(),
        root.join(AGENTS_DIR),
        MANAGER_CALL_TIMEOUT,
    ) {
        // Teardown only: a failure is covered by the fallback below.
        let _uninstalled = daemon.uninstall().await;
    }
    let workers = supervisor(paths, namespace);
    if let Ok(jobs) = workers.discover().await {
        for job in jobs {
            let _retired = workers.retire(&job.id).await;
        }
    }
    // Catch-all for labels the typed backend could not parse or reach.
    let mut labels = extra.labels;
    labels.push(namespace.daemon_label());
    if let Ok(entries) = std::fs::read_dir(paths.launchd_definitions_dir()) {
        labels.extend(entries.flatten().filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".plist"))
                .map(ToOwned::to_owned)
        }));
    }
    for label in labels {
        // An already absent label is the desired state.
        let _status = launchctl(&["bootout", &format!("gui/{}/{label}", uid())]);
    }
}

fn supervisor(paths: &BasePaths, namespace: &Namespace) -> LaunchdSupervisor {
    LaunchdSupervisor::new(
        namespace.clone(),
        uid(),
        paths.launchd_definitions_dir(),
        paths.launchd_log_dir(),
        MANAGER_CALL_TIMEOUT,
    )
    .expect("valid worker supervisor")
}

fn launchctl(arguments: &[&str]) -> std::process::ExitStatus {
    Command::new("/bin/launchctl")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run launchctl")
}
