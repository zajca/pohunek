//! Daemon-local environment health checks.
//!
//! The host-probe logic is shared with the CLI `doctor` command via the
//! `hostcheck` crate; this module only selects which checks to run on the host
//! that owns the agent runtime and assembles them into a [`DoctorReport`].

use hostcheck::StandardCheckInputs;
use protocol::DoctorReport;

use crate::Paths;

/// Build a daemon-local doctor report on the host that owns the agent runtime.
#[must_use]
pub fn report(paths: &Paths) -> DoctorReport {
    let launcher_bin_dir = paths.launcher_bin_dir();
    let sway_config_dir = paths.sway_config_dir();

    DoctorReport::from_checks(hostcheck::standard_checks(StandardCheckInputs {
        socket_dir: &paths.runtime_dir,
        state_dir: &paths.data_dir,
        log_dir: &paths.log_dir,
        launcher_bin_dir: &launcher_bin_dir,
        sway_config_dir: &sway_config_dir,
    }))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use protocol::DoctorStatus;

    use super::*;

    fn paths_at(root: &Path) -> Paths {
        Paths {
            runtime_dir: root.join("runtime"),
            socket: root.join("runtime").join("daemon.sock"),
            lock: root.join("runtime").join("daemon.lock"),
            log_dir: root.join("logs"),
            state_dir: root.join("state"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            config_home: root.join("config"),
            config_dir: root.join("config").join("pohunek"),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pohunek-daemon-doctor-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after unix epoch")
                .as_nanos()
        ))
    }

    #[test]
    fn report_contains_writable_daemon_paths() {
        let root = temp_dir("report");
        let paths = paths_at(&root);

        let report = report(&paths);

        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "socket_dir_writable" && check.status == DoctorStatus::Ok));
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "state_dir_writable" && check.status == DoctorStatus::Ok));
        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "log_dir_writable" && check.status == DoctorStatus::Ok));
    }
}
