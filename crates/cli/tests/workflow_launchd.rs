//! Workflow plugin jobs and services against real launchd in `gui/<uid>`.
//!
//! Runs in the ordinary macOS test run; a missing `gui/<uid>` domain fails
//! it. The plugin agents are defined in a temporary `LaunchAgents` directory
//! under a namespace unique to each scenario, and every scenario boots them
//! out again, also when it fails. The scenarios themselves are in
//! `support/workflow_scenarios.rs`, shared with the systemd counterpart
//! `workflow_systemd.rs`.

#![cfg(target_os = "macos")]

// Rust guideline compliant 2026-10-13

#[path = "support/workflow_scenarios.rs"]
mod workflow_scenarios;
#[path = "support/workflow_units.rs"]
mod workflow_units;

macro_rules! launchd_scenario {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            workflow_scenarios::$name().await;
        }
    };
}

launchd_scenario!(enable_starts_units_and_disable_removes_them);
launchd_scenario!(an_active_job_run_is_never_overlapped);
launchd_scenario!(a_job_past_its_deadline_is_stopped_and_recorded);
launchd_scenario!(a_crashing_service_backs_off_up_to_the_cap);
launchd_scenario!(a_failing_plugin_leaves_another_running);
launchd_scenario!(stale_and_residue_units_are_refused);
launchd_scenario!(the_manager_environment_never_reaches_a_job);
launchd_scenario!(the_trampoline_environment_never_reaches_a_job);
launchd_scenario!(a_stopped_unit_ends_its_run_cleanly);
