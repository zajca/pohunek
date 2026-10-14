//! The workflow plugin unit scenarios, run against the real service manager
//! by `workflow_systemd.rs` (Linux) and `workflow_launchd.rs` (macOS).
//!
//! Each scenario installs plugins in a fresh [`Managed`] installation,
//! enables them through the same code `pohunek workflow enable` runs, and
//! observes the plugin's run log, the marks its fixture programs leave in
//! its state directory, and the manager's own unit lists. Scenarios that
//! decide themselves when the trampoline runs use [`DIRECT_KNOBS`], whose
//! timers never fire during a scenario, and run `pohunek workflow __exec`
//! through the real binary.

// Rust guideline compliant 2026-10-13

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::Stdio;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use pohunek_cli::workflow_units::{disable_plugin, enable_plugin, uninstall_plugin};
use pohunek_platform::supervisor::{PluginUnitKey, PluginUnitKind, ServiceState};
use pohunek_test_support::wait::{poll_until, wait_until};
use serde_json::Value;

use crate::workflow_units::native::{
    self, manager_variable, set_manager_variable, unset_manager_variable,
};
use crate::workflow_units::{
    assert_alternating, describe, variable_names, wait_gone, Managed, Plugin, UnitEnv,
    DIRECT_KNOBS, KNOBS, SUPERVISED_ENVIRONMENT, TICK_OUTPUT,
};

/// Runs a scenario waits for before it counts a schedule as repeating.
const REPEATED_RUNS: usize = 3;

/// Service runs a backoff scenario observes: the first start plus enough
/// restarts to reach and keep the cap.
const BACKOFF_RUNS: usize = 5;

/// Restart delays [`KNOBS`] give the first [`BACKOFF_RUNS`] runs of a service
/// that always exits at once: none for the first start, then
/// `restart_initial_ms` (500) doubling up to `restart_max_ms` (2000).
const BACKOFF_DELAYS_MS: [u64; BACKOFF_RUNS] = [0, 500, 1000, 2000, 2000];

/// A digest no installed archive has.
const STALE_DIGEST: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn outcome_count(records: &[Value], outcome: &str) -> usize {
    records
        .iter()
        .filter(|record| record["outcome"] == outcome)
        .count()
}

/// Enable installs and starts every unit; disable stops the runs, removes
/// every unit and leaves nothing loaded.
pub(crate) async fn enable_starts_units_and_disable_removes_them() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.life")
        .job("tick", "tick", 1)
        .service("serve", "hang");
    let id = plugin.id.clone();
    managed.enable(&plugin).await;

    assert_eq!(managed.unit_files(), managed.names(&id));
    let units = &managed.units;
    wait_until("repeated job runs", || async {
        (outcome_count(&units.unit_records(&id, "job", "tick"), "ok") >= REPEATED_RUNS)
            .then_some(())
    })
    .await;
    // Supervised output is discarded without a cap and only counted.
    for record in units.unit_records(&id, "job", "tick") {
        assert_eq!(record["stdout_bytes"], TICK_OUTPUT, "{record}");
        assert_eq!(record["stderr_bytes"], TICK_OUTPUT, "{record}");
    }
    let child = units.pid(&id, "serve", "child");
    let grandchild = units.pid(&id, "serve", "grandchild");

    managed.disable(&id).await;
    assert!(
        managed.unit_files().is_empty(),
        "{:?}",
        managed.unit_files()
    );
    assert!(managed.loaded().is_empty(), "{:?}", managed.loaded());
    wait_gone(child);
    wait_gone(grandchild);
    let service = managed.units.unit_records(&id, "service", "serve");
    assert_eq!(
        service.len(),
        1,
        "one service run was recorded: {service:?}"
    );
    let listed = managed.units.listed();
    assert_eq!(listed[0]["enabled"], false, "{listed:?}");
}

/// A job run that is still active is never started again: a run started
/// meanwhile by hand is recorded as `overlap_skipped`, and the scheduled
/// runs never overlap.
pub(crate) async fn an_active_job_run_is_never_overlapped() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.overlap").job("gated", "gated", 1);
    let id = plugin.id.clone();
    let digest = managed.enable(&plugin).await;
    let units = &managed.units;

    // Counts, not exact lengths: launchd counts its interval from start to
    // start, so it may start the next run as soon as one ends.
    for round in 1..=REPEATED_RUNS {
        poll_until(&format!("run {round} to start"), || {
            let (started, ended) = units.run_counts(&id, "gated");
            (started >= round && started > ended).then_some(())
        });
        if round == 1 {
            let output = units
                .exec(&id, digest.as_str(), "job", "gated")
                .output()
                .expect("run the trampoline");
            assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        }
        units.release(&id);
        poll_until(&format!("run {round} to end"), || {
            (units.run_counts(&id, "gated").1 >= round).then_some(())
        });
    }

    assert_alternating(&units.marks(&id, "gated"));
    // A run's record follows its `end` mark once its output has drained.
    poll_until("every released run to be recorded", || {
        let records = units.unit_records(&id, "job", "gated");
        (outcome_count(&records, "ok") >= REPEATED_RUNS).then_some(())
    });
    // The skipped run never verified the manifest, so it names no job.
    let skipped: Vec<Value> = units
        .records(&id)
        .into_iter()
        .filter(|record| record["outcome"] == "overlap_skipped")
        .collect();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0]["kind"], "job", "{skipped:?}");
    assert!(skipped[0].get("name").is_none(), "{skipped:?}");
}

/// A job run past its deadline is stopped with its whole process group and
/// recorded as `timeout`.
pub(crate) async fn a_job_past_its_deadline_is_stopped_and_recorded() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.late").job_with_timeout("hang", "hang", 1, 1000);
    let id = plugin.id.clone();
    managed.enable(&plugin).await;
    let units = &managed.units;

    let child = units.pid(&id, "hang", "child");
    let grandchild = units.pid(&id, "hang", "grandchild");
    let record = wait_until("a timeout record", || async {
        units
            .unit_records(&id, "job", "hang")
            .into_iter()
            .find(|record| record["outcome"] == "timeout")
    })
    .await;
    assert_eq!(record["error_code"], "workflow_action_timeout", "{record}");
    wait_gone(child);
    wait_gone(grandchild);
}

/// A service that keeps exiting is restarted after the owner's backoff,
/// which doubles from `restart_initial_ms` and stays at `restart_max_ms`.
pub(crate) async fn a_crashing_service_backs_off_up_to_the_cap() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.crash").service("crash", "crash");
    let id = plugin.id.clone();
    managed.enable(&plugin).await;
    let units = &managed.units;

    let records = wait_until("the service restarts", || async {
        let records = units.unit_records(&id, "service", "crash");
        (records.len() >= BACKOFF_RUNS).then_some(records)
    })
    .await;
    let delays: Vec<u64> = records[..BACKOFF_RUNS]
        .iter()
        .map(|record| record["backoff_ms"].as_u64().expect("a backoff_ms"))
        .collect();
    assert_eq!(delays, BACKOFF_DELAYS_MS, "{records:?}");
    for record in &records[..BACKOFF_RUNS] {
        assert_eq!(record["outcome"], "exit_nonzero", "{record}");
        assert_eq!(record["exit_code"], 3, "{record}");
    }
}

/// A plugin whose service crashes and whose job times out does not keep
/// another plugin's job from running on schedule.
pub(crate) async fn a_failing_plugin_leaves_another_running() {
    let mut managed = Managed::new(KNOBS);
    let failing = Plugin::new("acme.bad")
        .service("crash", "crash")
        .job_with_timeout("hang", "hang", 1, 1000);
    let healthy = Plugin::new("acme.good").job("tick", "tick", 1);
    managed.enable(&failing).await;
    managed.enable(&healthy).await;
    let units = &managed.units;

    wait_until("both plugins to have run repeatedly", || async {
        let bad = units.records("acme.bad");
        let good = units.unit_records("acme.good", "job", "tick");
        (outcome_count(&bad, "exit_nonzero") >= 2
            && outcome_count(&bad, "timeout") >= 1
            && good.len() >= REPEATED_RUNS)
            .then_some(())
    })
    .await;
    let good = units.unit_records("acme.good", "job", "tick");
    assert_eq!(outcome_count(&good, "ok"), good.len(), "{good:?}");
}

/// A unit naming another archive, or a plugin that is gone, is refused
/// before anything runs; the right unit runs once per interval.
pub(crate) async fn stale_and_residue_units_are_refused() {
    let mut managed = Managed::new(DIRECT_KNOBS);
    let plugin = Plugin::new("acme.stale").job("tick", "tick", 3600);
    let id = plugin.id.clone();
    let digest = managed.enable(&plugin).await;
    let units = &managed.units;

    let output = units
        .exec(&id, STALE_DIGEST, "job", "tick")
        .output()
        .expect("run the trampoline");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let record = units.records(&id).pop().expect("a refusal record");
    assert_eq!(record["outcome"], "refused", "{record}");
    assert_eq!(record["error_code"], "workflow_unit_stale", "{record}");
    assert!(units.marks(&id, "tick").is_empty(), "a refused unit ran");

    let output = units
        .exec("acme.gone", digest.as_str(), "job", "tick")
        .output()
        .expect("run the trampoline");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("was refused"),
        "{}",
        describe(&output)
    );

    for expected in ["ok", "not due"] {
        let output = units
            .exec(&id, digest.as_str(), "job", "tick")
            .output()
            .expect("run the trampoline");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{expected}: {}",
            describe(&output)
        );
    }
    let records = units.unit_records(&id, "job", "tick");
    assert_eq!(
        outcome_count(&records, "ok"),
        1,
        "a due run, then none: {records:?}"
    );
    assert_eq!(
        units.marks(&id, "tick").len(),
        2,
        "one run started and ended"
    );

    // A unit of a disabled plugin is refused without running.
    managed.disable(&id).await;
    let units = &managed.units;
    units.reset_schedule(&id, "tick");
    let output = units
        .exec(&id, digest.as_str(), "job", "tick")
        .output()
        .expect("run the trampoline");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let record = units.records(&id).pop().expect("a refusal record");
    assert_eq!(record["outcome"], "refused", "{record}");
    assert_eq!(record["error_code"], "workflow_plugin_disabled", "{record}");
    assert_eq!(units.marks(&id, "tick").len(), 2, "a refused unit ran");
}

/// A job that the owner's changed `[jobs]` bounds no longer admit is
/// refused at run time, as it would be when enabled.
pub(crate) async fn a_job_outside_changed_bounds_is_refused_when_it_runs() {
    let mut managed = Managed::new(DIRECT_KNOBS);
    let plugin = Plugin::new("acme.bounds").job("tick", "tick", 3600);
    let id = plugin.id.clone();
    let digest = managed.enable(&plugin).await;
    let units = &managed.units;
    units.knobs(&DIRECT_KNOBS.replace("min_interval_s = 1", "min_interval_s = 7200"));

    let output = units
        .exec(&id, digest.as_str(), "job", "tick")
        .output()
        .expect("run the trampoline");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let record = units.records(&id).pop().expect("a refusal record");
    assert_eq!(record["outcome"], "refused", "{record}");
    assert_eq!(record["error_code"], "workflow_unit_refused", "{record}");
    assert_eq!(record["name"], "tick", "{record}");
    assert!(units.marks(&id, "tick").is_empty(), "a refused job ran");
}

/// `uninstall --remove-modified` of a plugin whose files verify is refused
/// before anything changes: the plugin stays enabled and its units keep
/// running.
pub(crate) async fn a_refused_uninstall_leaves_the_units_running() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.keep").service("serve", "hang");
    let id = plugin.id.clone();
    managed.enable(&plugin).await;
    let child = managed.units.pid(&id, "serve", "child");

    let output = managed
        .units
        .command(&["workflow", "uninstall", &id, "--remove-modified", "--json"])
        .output()
        .expect("run pohunek workflow uninstall");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    assert_eq!(
        document["err"]["code"], "workflow_root_intact",
        "{document}"
    );
    assert_eq!(managed.unit_files(), managed.names(&id));
    assert!(kill(child, None).is_ok(), "the service run was stopped");
    assert_eq!(managed.units.listed()[0]["enabled"], true);
}

/// Installing the recorded archive of an enabled plugin again changes
/// nothing: its running service is neither stopped nor restarted.
pub(crate) async fn installing_again_leaves_running_units_alone() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.again").service("serve", "hang");
    let id = plugin.id.clone();
    let digest = managed.enable(&plugin).await;
    let child = managed.units.pid(&id, "serve", "child");

    let output = managed.units.install_with(&plugin, &digest, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let document: Value = serde_json::from_slice(&output.stdout).expect("a JSON envelope");
    assert_eq!(document["ok"]["status"], "already_installed", "{document}");
    assert_eq!(document["ok"]["plugin"]["enabled"], true, "{document}");
    assert!(kill(child, None).is_ok(), "the service run was stopped");
    assert_eq!(
        managed.units.pid(&id, "serve", "child"),
        child,
        "the service restarted"
    );
    assert_eq!(managed.unit_files(), managed.names(&id));
}

/// A file restored to `bytes` when dropped, so the teardown of a scenario
/// that damaged it can still disable its plugin.
struct RestoredFile {
    path: PathBuf,
    bytes: Vec<u8>,
}

impl Drop for RestoredFile {
    fn drop(&mut self) {
        if let Err(error) = fs::write(&self.path, &self.bytes) {
            eprintln!("could not restore {}: {error}", self.path.display());
        }
    }
}

/// Enabling an enabled plugin whose unit record is damaged fails before
/// anything changes: the command reports `workflow_store_failed`, the plugin
/// stays enabled and its running unit keeps running.
pub(crate) async fn re_enabling_with_a_damaged_unit_record_changes_nothing() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.damaged").service("serve", "hang");
    let id = plugin.id.clone();
    managed.enable(&plugin).await;
    let child = managed.units.pid(&id, "serve", "child");
    let units_before = managed.unit_files();

    let record = managed
        .units
        .env
        .state_home()
        .join("pohunek/workflow-plugins/units")
        .join(format!("{id}.json"));
    let valid = fs::read(&record).expect("read the unit record");
    let _restored = RestoredFile {
        path: record.clone(),
        bytes: valid,
    };
    fs::write(&record, b"{\"units\": not json").expect("damage the unit record");

    let error = enable_plugin(&managed.units.context, &id)
        .await
        .expect_err("enabling with a damaged unit record is refused");
    assert_eq!(error.code, "workflow_store_failed", "{}", error.message);
    assert_eq!(
        managed.units.listed()[0]["enabled"],
        true,
        "{:?}",
        managed.units.listed()
    );
    assert_eq!(managed.unit_files(), units_before);
    assert_eq!(
        managed.units.pid(&id, "serve", "child"),
        child,
        "the service restarted"
    );
    assert!(
        kill(child, None).is_ok(),
        "the service run is still running"
    );
}

/// A job started by the service manager gets exactly the supervised
/// environment, never a variable of the manager's own environment.
pub(crate) async fn the_manager_environment_never_reaches_a_job() {
    let sentinel = format!("PHW_SENTINEL_{}", std::process::id());
    let _planted = Planted::new(&sentinel);
    assert_eq!(
        manager_variable(&sentinel).as_deref(),
        Some(PLANTED_VALUE),
        "the sentinel is in the manager's environment"
    );
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.env").job("env", "env", 3600);
    let id = plugin.id.clone();
    managed.enable(&plugin).await;

    let environment = managed.units.environment(&id);
    assert_eq!(variable_names(&environment), SUPERVISED_ENVIRONMENT);
}

/// A trampoline run with a session origin pair, a socket override and a
/// secret in its own environment passes none of them on, and names the
/// installed CLI.
pub(crate) async fn the_trampoline_environment_never_reaches_a_job() {
    let mut managed = Managed::new(DIRECT_KNOBS);
    let plugin = Plugin::new("acme.env").job("env", "env", 3600);
    let id = plugin.id.clone();
    let digest = managed.enable(&plugin).await;
    let units = &managed.units;

    let output = units
        .exec(&id, digest.as_str(), "job", "env")
        .env("POHUNEK_SESSION_ID", "s-01KYAPVPFVHD56Z69B9CX3XWN2")
        .env("POHUNEK_DAEMON_ID", "d-planted")
        .env("POHUNEK_SOCKET_PATH", "/planted/override.sock")
        .env("POHUNEK_CONFIG_DIR", "/planted/config")
        .env("POHUNEK_ENV", "1")
        .env("SECRET_TOKEN", "secret-sentinel-0e2f")
        .output()
        .expect("run the trampoline");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let environment = units.environment(&id);
    assert_eq!(variable_names(&environment), SUPERVISED_ENVIRONMENT);
    let value = |name: &str| {
        environment
            .iter()
            .find(|(key, _value)| key == name)
            .map(|(_key, value)| value.clone())
            .expect("a supervised variable")
    };
    let cli = units.env.root().join("prefix/bin/pohunek");
    assert_eq!(value("POHUNEK_CLI"), cli.display().to_string());
    let context: Value =
        serde_json::from_str(&value("POHUNEK_PLUGIN_CONTEXT")).expect("a JSON context");
    assert_eq!(context["invocation"]["kind"], "job", "{context}");
    assert_eq!(context["invocation"]["name"], "env", "{context}");
    assert!(context.get("output").is_none(), "{context}");
}

/// A stop request ends a run cleanly: the trampoline stops the run's whole
/// process group, records it as `interrupted` and exits 0, so a unit the
/// service manager stops reads as stopped, never failed. The job is stopped
/// with `SIGTERM` by hand, which is how launchd stops a unit; the service is
/// stopped through the service manager itself.
pub(crate) async fn a_stopped_unit_ends_its_run_cleanly() {
    let mut managed = Managed::new(DIRECT_KNOBS);
    let plugin = Plugin::new("acme.stop")
        .job("hang", "hang", 3600)
        .service("serve", "hang");
    let id = plugin.id.clone();
    let digest = managed.enable(&plugin).await;
    let units = &managed.units;

    let trampoline = units
        .exec(&id, digest.as_str(), "job", "hang")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the trampoline");
    let child = units.pid(&id, "hang", "child");
    let grandchild = units.pid(&id, "hang", "grandchild");
    let pid = Pid::from_raw(i32::try_from(trampoline.id()).expect("a PID"));
    kill(pid, Signal::SIGTERM).expect("signal the trampoline");
    let output = trampoline
        .wait_with_output()
        .expect("wait for the trampoline");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    wait_gone(child);
    wait_gone(grandchild);
    let record = units
        .unit_records(&id, "job", "hang")
        .pop()
        .expect("a job record");
    assert_eq!(record["outcome"], "interrupted", "{record}");

    let backend = managed.backend().await;
    // A job run the service manager started, then stops.
    units.reset_job(&id, "hang");
    let job = PluginUnitKey::new(&id, PluginUnitKind::Job, "hang").expect("a unit key");
    native::start(&managed.native_names(&job));
    let child = units.pid(&id, "hang", "child");
    let grandchild = units.pid(&id, "hang", "grandchild");
    backend.stop(&job).await.expect("stop the job unit");
    wait_gone(child);
    wait_gone(grandchild);
    // The service started at enable.
    let child = units.pid(&id, "serve", "child");
    let grandchild = units.pid(&id, "serve", "grandchild");
    let service = PluginUnitKey::new(&id, PluginUnitKind::Service, "serve").expect("a unit key");
    backend.stop(&service).await.expect("stop the service unit");
    wait_gone(child);
    wait_gone(grandchild);
    for key in [&job, &service] {
        assert_eq!(
            backend.state(key).await.expect("the unit state"),
            ServiceState::Stopped,
            "{key}: a unit stopped by its manager is stopped, not failed"
        );
        assert!(
            !native::failed(&managed.native_names(key)),
            "{key}: the service manager records no failure"
        );
    }
    let records = poll_until("both stopped runs to be recorded", || {
        let jobs = managed.units.unit_records(&id, "job", "hang");
        let services = managed.units.unit_records(&id, "service", "serve");
        (jobs.len() == 2 && services.len() == 1)
            .then_some([&jobs[1], &services[0]].map(Clone::clone))
    });
    for record in records {
        assert_eq!(record["outcome"], "interrupted", "{record}");
    }
}

/// Changes the installed `bin/crash` program of the one plugin the
/// installation holds, so its root fails verification while its units keep
/// running. Its content is changed rather than removed, so the root is
/// modified and not missing.
fn modify_installed_program(units: &UnitEnv) {
    let packages = units
        .env
        .state_home()
        .join("pohunek/workflow-plugins/packages");
    let mut roots = fs::read_dir(&packages)
        .expect("the plugin store")
        .map(|entry| entry.expect("a store entry").path());
    let root = roots.next().expect("one installed plugin root");
    assert!(roots.next().is_none(), "one installed plugin root");
    let program = root.join("files/bin/crash");
    fs::set_permissions(&program, fs::Permissions::from_mode(0o600))
        .expect("make the installed program writable");
    fs::write(&program, "#!/bin/sh\nexit 4\n").expect("modify the installed program");
}

/// Disabling a plugin whose installed files fail verification stops its
/// running service and removes its unit files and loaded units. The units
/// are found through the unit record, so the command reports no warning that
/// they could not be listed.
pub(crate) async fn disable_stops_the_units_of_a_modified_plugin() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.modified").service("serve", "hang");
    let id = plugin.id.clone();
    managed.enable(&plugin).await;
    let child = managed.units.pid(&id, "serve", "child");
    modify_installed_program(&managed.units);

    let warnings = disable_plugin(&managed.units.context, &id)
        .await
        .expect("disable the plugin");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(
        managed.unit_files().is_empty(),
        "{:?}",
        managed.unit_files()
    );
    assert!(managed.loaded().is_empty(), "{:?}", managed.loaded());
    wait_gone(child);
}

/// Uninstalling a plugin whose installed files fail verification with
/// `--remove-modified --purge` stops its service and removes its units
/// before the plugin record and its directories go.
pub(crate) async fn uninstall_stops_the_units_of_a_modified_plugin() {
    let mut managed = Managed::new(KNOBS);
    let plugin = Plugin::new("acme.gone").service("serve", "hang");
    let id = plugin.id.clone();
    managed.enable(&plugin).await;
    let child = managed.units.pid(&id, "serve", "child");
    modify_installed_program(&managed.units);

    let warnings = uninstall_plugin(&managed.units.context, &id, true, true)
        .await
        .expect("uninstall the plugin");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(
        managed.unit_files().is_empty(),
        "{:?}",
        managed.unit_files()
    );
    assert!(managed.loaded().is_empty(), "{:?}", managed.loaded());
    wait_gone(child);
    assert!(
        !managed.units.state_dir(&id).exists(),
        "the purge removed the state directory"
    );
}

/// The value of a variable planted in the service manager's environment.
const PLANTED_VALUE: &str = "planted-in-the-manager";

/// A variable planted in the service manager's environment for one scenario.
struct Planted(String);

impl Planted {
    fn new(name: &str) -> Self {
        set_manager_variable(name, PLANTED_VALUE);
        Self(name.to_owned())
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        unset_manager_variable(&self.0);
    }
}
