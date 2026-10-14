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

use pohunek_test_support::wait::wait_until;
use workflow_units::{native, variable_names, Managed, Plugin, RuntimeRoot, KNOBS};

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
launchd_scenario!(a_job_outside_changed_bounds_is_refused_when_it_runs);
launchd_scenario!(a_refused_uninstall_leaves_the_units_running);
launchd_scenario!(installing_again_leaves_running_units_alone);
launchd_scenario!(re_enabling_with_a_damaged_unit_record_changes_nothing);
launchd_scenario!(disable_stops_the_units_of_a_modified_plugin);
launchd_scenario!(uninstall_stops_the_units_of_a_modified_plugin);
launchd_scenario!(status_shows_running_units_and_failing_runs);

/// A launchd user-session variable, removed again when the guard drops, also
/// when the scenario fails.
struct ManagerVariable(&'static str);

impl ManagerVariable {
    fn set(name: &'static str, value: &str) -> Self {
        native::set_manager_variable(name, value);
        Self(name)
    }
}

impl Drop for ManagerVariable {
    fn drop(&mut self) {
        native::unset_manager_variable(self.0);
    }
}

/// An `XDG_RUNTIME_DIR` that launchd merges into a unit's environment does
/// not refuse the runs of an installation that uses the macOS default
/// runtime root: the units do not pin that variable, and the trampoline
/// resolves the recorded default root instead, so the first job run is
/// recorded as completed, and the job's own environment does not carry the
/// variable either, so its `POHUNEK_CLI` reaches the same installation.
#[tokio::test(flavor = "multi_thread")]
async fn an_inherited_runtime_dir_does_not_refuse_runs_of_the_default_root() {
    let mut managed = Managed::with_runtime(KNOBS, RuntimeRoot::PlatformDefault);
    let conflicting = managed
        .units
        .env
        .runtime_dir()
        .to_str()
        .expect("a UTF-8 runtime directory")
        .to_owned();
    let _variable = ManagerVariable::set("XDG_RUNTIME_DIR", &conflicting);
    let plugin = Plugin::new("acme.runtime").job("env", "env", 3600);
    let id = plugin.id.clone();
    managed.enable(&plugin).await;

    wait_until("the first job run", || async {
        (!managed.units.unit_records(&id, "job", "env").is_empty()).then_some(())
    })
    .await;
    let records = managed.units.unit_records(&id, "job", "env");
    assert_eq!(records[0]["outcome"], "ok", "{records:?}");
    let environment = managed.units.environment(&id);
    assert!(
        !variable_names(&environment).contains(&"XDG_RUNTIME_DIR".to_owned()),
        "the inherited runtime directory reached the job: {environment:?}"
    );
}
