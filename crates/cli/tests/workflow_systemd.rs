//! Workflow plugin jobs and services against the real systemd user manager.
//!
//! Ignored unless `POHUNEK_SYSTEMD_E2E=1` opts in, like the other real-systemd
//! suites. The plugin units go to the manager's runtime unit directory
//! (`$XDG_RUNTIME_DIR/systemd/user`) under a namespace unique to each
//! scenario, and every scenario removes them again, also when it fails. The
//! scenarios themselves are in `support/workflow_scenarios.rs`, shared with
//! the launchd counterpart `workflow_launchd.rs`.

#![cfg(target_os = "linux")]

// Rust guideline compliant 2026-10-13

#[path = "support/workflow_scenarios.rs"]
mod workflow_scenarios;
#[path = "support/workflow_units.rs"]
mod workflow_units;

macro_rules! systemd_scenario {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"]
        async fn $name() {
            workflow_scenarios::$name().await;
        }
    };
}

systemd_scenario!(enable_starts_units_and_disable_removes_them);
systemd_scenario!(an_active_job_run_is_never_overlapped);
systemd_scenario!(a_job_past_its_deadline_is_stopped_and_recorded);
systemd_scenario!(a_crashing_service_backs_off_up_to_the_cap);
systemd_scenario!(a_failing_plugin_leaves_another_running);
systemd_scenario!(stale_and_residue_units_are_refused);
systemd_scenario!(the_manager_environment_never_reaches_a_job);
systemd_scenario!(the_trampoline_environment_never_reaches_a_job);
systemd_scenario!(a_stopped_unit_ends_its_run_cleanly);
systemd_scenario!(a_job_outside_changed_bounds_is_refused_when_it_runs);
systemd_scenario!(a_refused_uninstall_leaves_the_units_running);
systemd_scenario!(installing_again_leaves_running_units_alone);
systemd_scenario!(re_enabling_with_a_damaged_unit_record_changes_nothing);
systemd_scenario!(disable_stops_the_units_of_a_modified_plugin);
systemd_scenario!(uninstall_stops_the_units_of_a_modified_plugin);
systemd_scenario!(status_shows_running_units_and_failing_runs);
systemd_scenario!(the_dispatcher_unit_follows_the_reacting_plugins);
