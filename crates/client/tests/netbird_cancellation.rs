#![cfg(target_os = "linux")]

use std::fs;
use std::process::Command;

use pohunek_client::{
    default_overlay_registry, discover_hosts_with_options, DiscoveryOptions, OriginSource,
};
use pohunek_test_support::process_env::ProcessEnv;
use pohunek_test_support::wait;

struct ProcessCleanup {
    pid: u32,
    start_time: String,
}

impl ProcessCleanup {
    fn watch(pid: u32) -> Self {
        Self {
            pid,
            start_time: process_start_time(pid).expect("NetBird process start time"),
        }
    }
}

impl Drop for ProcessCleanup {
    fn drop(&mut self) {
        // The process may already be gone; rechecking the start time prevents
        // cleanup from signalling an unrelated process after PID reuse.
        if process_start_time(self.pid).as_deref() == Some(&self.start_time) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &self.pid.to_string()])
                .status();
        }
    }
}

fn process_start_time(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_ascii_whitespace().nth(19).map(str::to_owned)
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_discovery_kills_the_active_netbird_status_process() {
    let root = pohunek_test_support::tempdir_with_prefix("pohunek-netbird-discovery-")
        .expect("private test root");
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).expect("create fixture bin");
    let pid_file = root.path().join("status-pid");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 300\n",
            pid_file.display()
        ),
    )
    .expect("write slow NetBird fixture");

    let mut process_env = ProcessEnv::lock();
    process_env.set("PATH", bin.as_os_str());
    process_env.remove("POHUNEK_REMOTE_PORT");
    let registry = default_overlay_registry().expect("production NetBird registry");
    let options = DiscoveryOptions::new().with_origin_source(OriginSource::Omitted);
    let discovery =
        tokio::spawn(async move { discover_hosts_with_options(&registry, options).await });

    let pid: u32 = wait::wait_until("NetBird status process to start", || async {
        fs::read_to_string(&pid_file)
            .ok()
            .and_then(|value| value.parse().ok())
    })
    .await;
    let _cleanup = ProcessCleanup::watch(pid);
    discovery.abort();
    let cancelled = discovery.await.expect_err("discovery must be cancelled");
    assert!(cancelled.is_cancelled());
    wait::wait_until("NetBird status process to exit", || async {
        (!std::path::Path::new(&format!("/proc/{pid}")).exists()).then_some(())
    })
    .await;
    drop(process_env);
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_discovery_calls_complete_through_one_netbird_provider() {
    let root = pohunek_test_support::tempdir_with_prefix("pohunek-netbird-concurrent-")
        .expect("private test root");
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).expect("create fixture bin");
    pohunek_test_support::fs::write_executable(
        bin.join("netbird"),
        "#!/bin/sh\nprintf '%s\\n' '{\"netbirdIp\":\"100.64.0.1\",\"peers\":[]}'\n",
    )
    .expect("write healthy NetBird fixture");

    let mut process_env = ProcessEnv::lock();
    process_env.set("PATH", bin.as_os_str());
    process_env.remove("POHUNEK_REMOTE_PORT");
    let registry = default_overlay_registry().expect("production NetBird registry");
    let options = DiscoveryOptions::new().with_origin_source(OriginSource::Omitted);
    let (first, second, third, fourth) = tokio::join!(
        discover_hosts_with_options(&registry, options),
        discover_hosts_with_options(&registry, options),
        discover_hosts_with_options(&registry, options),
        discover_hosts_with_options(&registry, options),
    );
    for result in [first, second, third, fourth] {
        assert!(result.expect("concurrent discovery completed").is_empty());
    }
}
