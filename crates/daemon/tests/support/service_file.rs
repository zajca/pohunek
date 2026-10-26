//! The installed `service.toml` of a hermetic daemon test host.
//!
//! A judgement that predicts a launch reads the allowlist of this file, so a
//! test that needs one writes it through the same writer the installer uses.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-14

use std::path::Path;
use std::time::Duration;

use pohunek_platform::shell_env::SearchPath;
use pohunek_service_config::{
    ConfigSpec, Deadlines, ServiceConfig, DEFAULT_EVENT_RETENTION, DEFAULT_HOOK_STDERR_TAIL_BYTES,
    DEFAULT_INPUT_TIMING, DEFAULT_WAIT_TIMING,
};

/// Open-file limit the test service file records.
const OPEN_FILES: u64 = 8_192;

/// Writes a valid service file at `path` whose environment allowlist is
/// `allowlist`, installed under `prefix` with the given state and runtime roots.
pub(crate) fn write_service_file(
    path: &Path,
    prefix: &Path,
    state_root: &Path,
    runtime_root: &Path,
    allowlist: &[String],
) {
    let config = ServiceConfig::new(ConfigSpec {
        prefix: prefix.to_path_buf(),
        active_version: "0.34.2".to_owned(),
        uid: nix::unistd::Uid::effective().as_raw(),
        state_root: state_root.to_path_buf(),
        runtime_root: runtime_root.to_path_buf(),
        deadlines: Deadlines {
            worker_connect: Duration::from_secs(20),
            worker_initialize: Duration::from_secs(45),
            launchctl_command: Duration::from_secs(10),
            worker_exit_timeout: Duration::from_secs(3),
            daemon_exit_timeout: Duration::from_secs(10),
            daemon_restart_throttle: Duration::from_secs(1),
            registry_shutdown: Duration::from_secs(5),
        },
        environment_allowlist: allowlist.to_vec(),
        search_path: SearchPath::default(),
        sweep_grace: Duration::from_secs(1),
        input_timing: DEFAULT_INPUT_TIMING,
        wait_timing: DEFAULT_WAIT_TIMING,
        hook_stderr_tail_bytes: DEFAULT_HOOK_STDERR_TAIL_BYTES,
        hook_required: Vec::new(),
        event_retention: DEFAULT_EVENT_RETENTION,
        inventory_probe: pohunek_service_config::DEFAULT_INVENTORY_PROBE,
        open_files: OPEN_FILES,
    })
    .expect("valid service configuration");
    config.write(path).expect("write the service file");
}
