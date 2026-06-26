//! Daemon-local environment health checks.
//!
//! The host-probe logic is shared with the CLI `doctor` command via the
//! `hostcheck` crate; this module only selects which checks to run on the host
//! that owns the agent runtime and assembles them into a [`DoctorReport`].

use protocol::{DoctorCheck, DoctorReport, DoctorStatus};

use crate::Paths;

/// Build a daemon-local doctor report on the host that owns the agent runtime.
#[must_use]
pub fn report(paths: &Paths) -> DoctorReport {
    DoctorReport::from_checks(vec![
        hostcheck::binary("git", true),
        hostcheck::binary("codex", false),
        hostcheck::binary("claude", false),
        hostcheck::dir_writable(
            "socket_dir_writable",
            &paths.runtime_dir,
            "control socket directory",
        ),
        hostcheck::dir_writable(
            "state_dir_writable",
            &paths.data_dir,
            "state data directory",
        ),
        hostcheck::dir_writable("log_dir_writable", &paths.log_dir, "log directory"),
        hostcheck::netbird(),
        DoctorCheck::new(
            "schema_version",
            DoctorStatus::Warn,
            "not available yet (SQLite store is a later milestone)",
        ),
        hostcheck::binary("rofi", false),
        hostcheck::binary("swaymsg", false),
        hostcheck::binary("python3", false),
        hostcheck::binary("timeout", false),
        hostcheck::terminal(),
        hostcheck::launcher_scripts(&paths.launcher_bin_dir()),
        hostcheck::sway_include(&paths.sway_config_dir()),
    ])
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn paths_at(root: &Path) -> Paths {
        Paths {
            runtime_dir: root.join("runtime"),
            socket: root.join("runtime").join("daemon.sock"),
            lock: root.join("runtime").join("daemon.lock"),
            log_dir: root.join("logs"),
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
