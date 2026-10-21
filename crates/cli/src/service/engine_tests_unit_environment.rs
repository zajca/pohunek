//! What the daemon unit's drop-ins do to the environment the daemon job is
//! judged with: a recorded `PATH` (#1383), the store variables (#1384) and the
//! bootstrap keys (#1394). Drop-ins and environment files are real files below a
//! temporary root; the user manager is a double that reports what the real one
//! reports for them (pinned by the systemd lane in `crates/platform/tests`).

use std::collections::VecDeque;

use super::*;

/// The `PATH` `service.toml` records in these scenarios.
const RECORDED: &str = "/usr/bin";

/// A user manager that reports `entries` as its environment and the next of
/// `units` as the daemon unit; the last unit repeats.
#[derive(Debug)]
struct ScriptedManager {
    entries: Vec<String>,
    units: Mutex<VecDeque<Result<UnitEnvironment, EnvironmentFailure>>>,
}

impl ScriptedManager {
    fn new(entries: &[&str], units: Vec<UnitEnvironment>) -> Arc<Self> {
        Arc::new(Self {
            entries: entries.iter().map(|entry| (*entry).to_owned()).collect(),
            units: Mutex::new(units.into_iter().map(Ok).collect()),
        })
    }
}

impl UserManager for ScriptedManager {
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, EnvironmentFailure>> + Send + '_>> {
        let entries = self.entries.clone();
        Box::pin(async move { Ok(entries) })
    }

    fn unit_environment<'a>(
        &'a self,
        _unit: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<UnitEnvironment, EnvironmentFailure>> + Send + 'a>>
    {
        let mut units = self.units.lock().expect("units lock");
        let unit = if units.len() > 1 {
            units.pop_front().expect("a unit")
        } else {
            units.front().cloned().expect("a unit")
        };
        Box::pin(async move { unit })
    }
}

/// A loaded daemon unit with `files` as drop-ins in one directory and `merged`
/// as the `Environment=` the manager reports for the unit.
fn loaded_with(files: &[(&str, &str)], merged: &[&str]) -> (tempfile::TempDir, UnitEnvironment) {
    let dir = pohunek_test_support::tempdir().expect("scratch directory");
    let directory = dir.path().join("daemon.service.d");
    std::fs::create_dir_all(&directory).expect("drop-in directory");
    let mut paths = Vec::new();
    for (name, text) in files {
        let path = directory.join(name);
        std::fs::write(&path, text).expect("drop-in");
        paths.push(path.display().to_string());
    }
    let unit = UnitEnvironment {
        drop_in_paths: paths,
        environment: merged.iter().map(|entry| (*entry).to_owned()).collect(),
        ..loaded_unit()
    };
    (dir, unit)
}

/// An unloaded daemon unit whose drop-ins survive on disk.
fn removed_with(files: &[(&str, &str)]) -> (tempfile::TempDir, UnitEnvironment) {
    let (dir, loaded) = loaded_with(files, &[]);
    let unit = UnitEnvironment {
        load_state: "not-found".to_owned(),
        drop_ins_on_disk: loaded.drop_in_paths,
        ..UnitEnvironment::default()
    };
    (dir, unit)
}

/// A file below `dir` for an `EnvironmentFile=`; the path is returned.
fn file_in(dir: &tempfile::TempDir, name: &str, text: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, text).expect("environment file");
    path.display().to_string()
}

fn context_over(manager: Arc<ScriptedManager>) -> (Harness, Context) {
    let harness = Harness::new();
    let context = harness.context.clone().with_user_manager(manager);
    // The daemon unit's name derives from the application roots.
    context.namespace().expect("the roots exist");
    (harness, context)
}

/// The judgement of a daemon job that records `RECORDED` (`recorded`) or none.
async fn judge_job(
    unit: UnitEnvironment,
    entries: &[&str],
    allowlist: &[&str],
    recorded: bool,
) -> Result<JobPath, Error> {
    let (_harness, context) = context_over(ScriptedManager::new(entries, vec![unit]));
    let search_path = if recorded {
        SearchPath::new(vec![std::path::PathBuf::from(RECORDED)]).expect("a trusted directory")
    } else {
        SearchPath::empty()
    };
    let allowlist: Vec<String> = allowlist.iter().map(|name| (*name).to_owned()).collect();
    crate::service::engine::recorded_job_path(
        &context,
        &search_path,
        &allowlist,
        None,
        crate::service::engine::PathUse::Forward,
    )
    .await
}

const MANAGER_ENTRIES: [&str; 1] = ["PATH=/manager/bin"];
const STORE: &str = "CLAUDE_CONFIG_DIR";

fn variable(job: &JobPath, name: &str) -> Option<String> {
    job.variables.get(name).cloned()
}

// ---- recorded PATH (#1383) -------------------------------------------------

#[tokio::test]
async fn a_recorded_path_without_unit_overrides_is_the_jobs_path() {
    let job = judge_job(loaded_unit(), &MANAGER_ENTRIES, &[], true)
        .await
        .expect("path");
    assert_eq!(job.value, RECORDED);
    assert_eq!(job.source, PathSource::Recorded);
    assert_eq!(job.path_drop_in, None);
    let (harness, _) = context_over(ScriptedManager::new(&[], vec![loaded_unit()]));
    assert!(crate::service::engine::job_warnings(&harness.context, &job).is_empty());
}

#[tokio::test]
async fn a_drop_in_path_replaces_the_recorded_path_and_warns() {
    let (_dir, unit) = loaded_with(
        &[("10-path.conf", "[Service]\nEnvironment=PATH=/x/bin\n")],
        &["PATH=/usr/bin", "PATH=/x/bin"],
    );
    let job = judge_job(unit, &MANAGER_ENTRIES, &[], true)
        .await
        .expect("path");
    assert_eq!(job.value, "/x/bin");
    assert_eq!(job.source, PathSource::DropIn);
    let drop_in = job.path_drop_in.clone().expect("the drop-in is named");
    assert!(drop_in.ends_with("10-path.conf"), "{drop_in}");
    let (harness, _) = context_over(ScriptedManager::new(&[], vec![loaded_unit()]));
    let warnings = crate::service::engine::job_warnings(&harness.context, &job);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("10-path.conf") && warning.contains("replaces")),
        "{warnings:?}"
    );
    assert!(warnings.iter().all(|warning| !warning.contains("/x/bin")));
}

#[tokio::test]
async fn the_last_drop_in_path_wins_over_a_recorded_path() {
    let (_dir, unit) = loaded_with(
        &[
            ("10-a.conf", "[Service]\nEnvironment=PATH=/a/bin\n"),
            ("20-b.conf", "[Service]\nEnvironment=PATH=/b/bin\n"),
        ],
        &["PATH=/usr/bin", "PATH=/a/bin", "PATH=/b/bin"],
    );
    let job = judge_job(unit, &MANAGER_ENTRIES, &[], true)
        .await
        .expect("path");
    assert_eq!(job.value, "/b/bin");
    assert!(job.path_drop_in.expect("drop-in").ends_with("20-b.conf"));
}

#[tokio::test]
async fn a_drop_in_without_path_keeps_the_recorded_path() {
    let (_dir, unit) = loaded_with(
        &[("10-other.conf", "[Service]\nEnvironment=OTHER=1\n")],
        &["PATH=/usr/bin", "OTHER=1"],
    );
    let job = judge_job(unit, &MANAGER_ENTRIES, &[], true)
        .await
        .expect("path");
    assert_eq!(job.value, RECORDED);
    assert_eq!(job.source, PathSource::Recorded);
}

#[tokio::test]
async fn an_unclear_drop_in_line_refuses_a_recorded_path() {
    for line in [
        "Environment=\"PATH=/a/bin",
        "Environment=PATH=/a/bi\\x6e",
        "Environment=PA\\x54H=/a/bin",
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_dir, unit) = loaded_with(&[("10-odd.conf", &text)], &["PATH=/usr/bin"]);
        let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
        assert!(text.contains("10-odd.conf"), "{line}: {text}");
    }
}

#[tokio::test]
async fn environment_files_are_judged_for_a_recorded_path() {
    let scratch = pohunek_test_support::tempdir().expect("scratch directory");
    let sets = file_in(&scratch, "sets.env", "PATH=/file/bin\n");
    let plain = file_in(&scratch, "plain.env", "OTHER=1\n");
    for (directive, file, optional, expected) in [
        (
            format!("EnvironmentFile={sets}"),
            sets.clone(),
            false,
            false,
        ),
        (
            format!("EnvironmentFile={plain}"),
            plain.clone(),
            false,
            true,
        ),
        (
            "EnvironmentFile=-/nonexistent/pohunek-c2.env".to_owned(),
            "/nonexistent/pohunek-c2.env".to_owned(),
            true,
            true,
        ),
        (
            "EnvironmentFile=/nonexistent/pohunek-c2.env".to_owned(),
            "/nonexistent/pohunek-c2.env".to_owned(),
            false,
            false,
        ),
        (
            "EnvironmentFile=/nonexistent/*.env".to_owned(),
            "/nonexistent/*.env".to_owned(),
            false,
            false,
        ),
    ] {
        let text = format!("[Service]\n{directive}\n");
        let (_dir, mut unit) = loaded_with(&[("10-file.conf", &text)], &["PATH=/usr/bin"]);
        unit.environment_files = vec![(file, optional)];
        let result = judge_job(unit, &MANAGER_ENTRIES, &[], true).await;
        if expected {
            assert_eq!(result.expect("path").value, RECORDED, "{directive}");
        } else {
            refusal(result);
        }
    }
}

#[tokio::test]
async fn pass_environment_path_does_not_affect_a_recorded_path() {
    let (_dir, mut unit) = loaded_with(
        &[("10-pass.conf", "[Service]\nPassEnvironment=PATH\n")],
        &["PATH=/usr/bin"],
    );
    unit.pass_environment = vec!["PATH".to_owned()];
    let job = judge_job(unit, &MANAGER_ENTRIES, &[], true)
        .await
        .expect("Environment= overrides PassEnvironment=");
    assert_eq!(job.value, RECORDED);
}

#[tokio::test]
async fn unset_environment_path_refuses_a_recorded_path() {
    let (_dir, mut unit) = loaded_with(
        &[("10-unset.conf", "[Service]\nUnsetEnvironment=PATH\n")],
        &["PATH=/usr/bin"],
    );
    unit.unset_environment = vec!["PATH".to_owned()];
    let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
    assert!(text.contains("UnsetEnvironment"), "{text}");
}

#[tokio::test]
async fn a_fresh_install_keeps_the_recorded_path() {
    let unit = UnitEnvironment {
        load_state: "not-found".to_owned(),
        ..UnitEnvironment::default()
    };
    let job = judge_job(unit, &MANAGER_ENTRIES, &[], true)
        .await
        .expect("path");
    assert_eq!(job.value, RECORDED);
    assert_eq!(job.source, PathSource::Recorded);
}

#[tokio::test]
async fn a_removed_unit_with_a_recorded_path_is_judged_by_its_surviving_drop_in() {
    let (_dir, unit) = removed_with(&[("10-path.conf", "[Service]\nEnvironment=PATH=/x/bin\n")]);
    let job = judge_job(unit, &MANAGER_ENTRIES, &[], true)
        .await
        .expect("path");
    assert_eq!(job.value, "/x/bin");
    assert_eq!(job.source, PathSource::DropIn);

    let (_dir, unit) = removed_with(&[("10-other.conf", "[Service]\nEnvironment=OTHER=1\n")]);
    let job = judge_job(unit, &MANAGER_ENTRIES, &[], true)
        .await
        .expect("path");
    assert_eq!(job.value, RECORDED);
}

#[tokio::test]
async fn drop_ins_the_orphan_reader_refuses_refuse_a_recorded_path() {
    for line in [
        "Environment=PATH=%h/bin",
        "EnvironmentFile=/nonexistent/pohunek-c2.env",
        "UnsetEnvironment=PATH",
        "Environment=\"PATH=/a",
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_dir, unit) = removed_with(&[("10-odd.conf", &text)]);
        let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
        assert!(
            text.contains("10-odd.conf") || text.contains("pohunek-c2.env"),
            "{line}: {text}"
        );
    }
}

#[tokio::test]
async fn a_broken_or_stale_unit_refuses_a_recorded_path() {
    for state in ["masked", "error", "bad-setting"] {
        let unit = UnitEnvironment {
            load_state: state.to_owned(),
            ..UnitEnvironment::default()
        };
        let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
        assert!(text.contains(state), "{text}");
    }
    let (_dir, mut unit) = loaded_with(
        &[("10-path.conf", "[Service]\nEnvironment=PATH=/x/bin\n")],
        &["PATH=/x/bin"],
    );
    unit.need_daemon_reload = true;
    let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
    assert!(text.contains("daemon-reload"), "{text}");
}

#[tokio::test]
async fn a_drop_in_reset_drops_the_recorded_path() {
    let (_dir, unit) = loaded_with(&[("10-reset.conf", "[Service]\nEnvironment=\n")], &[]);
    let text = refusal(judge_job(unit.clone(), &MANAGER_ENTRIES, &[], true).await);
    assert!(
        text.contains("10-reset.conf") && text.contains("resets Environment="),
        "{text}"
    );
    // The `PATH` reader refuses on its own account, naming what it removes.
    let error = crate::service::manager_env::recorded_search_path(
        RECORDED,
        &unit,
        "daemon.service",
        &mut crate::service::manager_env::UnitFileReader::default(),
    )
    .await
    .expect_err("the reset removes the recorded PATH");
    assert!(error.to_string().contains("removes the PATH"), "{error}");

    // A reset followed by a PATH assignment is read.
    let (_dir, unit) = loaded_with(
        &[(
            "10-reset.conf",
            "[Service]\nEnvironment=\nEnvironment=PATH=/y/bin\n",
        )],
        &["PATH=/y/bin"],
    );
    let path = crate::service::manager_env::recorded_search_path(
        RECORDED,
        &unit,
        "daemon.service",
        &mut crate::service::manager_env::UnitFileReader::default(),
    )
    .await
    .expect("path");
    assert_eq!(path.value, "/y/bin");
}

#[tokio::test]
async fn a_manager_value_that_differs_from_the_drop_in_refuses_a_recorded_path() {
    let (_dir, unit) = loaded_with(
        &[("10-path.conf", "[Service]\nEnvironment=PATH=/x/bin\n")],
        &["PATH=/other/bin"],
    );
    let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
    assert!(text.contains("differs"), "{text}");
}

// ---- store variables (#1384) -----------------------------------------------

#[tokio::test]
async fn manager_variables_apply_without_unit_overrides() {
    let job = judge_job(
        loaded_unit(),
        &["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"],
        &[STORE],
        true,
    )
    .await
    .expect("job");
    assert_eq!(variable(&job, STORE).as_deref(), Some("/m"));
}

#[tokio::test]
async fn a_drop_in_store_variable_overrides_the_managers() {
    let (_dir, unit) = loaded_with(
        &[(
            "10-store.conf",
            "[Service]\nEnvironment=CLAUDE_CONFIG_DIR=/d\n",
        )],
        &["CLAUDE_CONFIG_DIR=/d"],
    );
    for recorded in [true, false] {
        let job = judge_job(
            unit.clone(),
            &["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"],
            &[STORE],
            recorded,
        )
        .await
        .expect("job");
        assert_eq!(variable(&job, STORE).as_deref(), Some("/d"), "{recorded}");
    }
}

#[tokio::test]
async fn allowlist_patterns_select_drop_in_variables() {
    let (_dir, unit) = loaded_with(
        &[(
            "10-store.conf",
            "[Service]\nEnvironment=CLAUDE_X=1 OTHER_Y=2\n",
        )],
        &["CLAUDE_X=1", "OTHER_Y=2"],
    );
    let job = judge_job(unit, &MANAGER_ENTRIES, &["CLAUDE_*"], true)
        .await
        .expect("job");
    assert_eq!(variable(&job, "CLAUDE_X").as_deref(), Some("1"));
    assert_eq!(variable(&job, "OTHER_Y"), None);
}

#[tokio::test]
async fn unselected_drop_in_variables_are_ignored() {
    let (_dir, unit) = loaded_with(
        &[("10-other.conf", "[Service]\nEnvironment=OTHER=1\n")],
        &["OTHER=1"],
    );
    let job = judge_job(
        unit,
        &["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"],
        &[STORE],
        true,
    )
    .await
    .expect("job");
    assert_eq!(job.variables.len(), 1);
    assert_eq!(variable(&job, STORE).as_deref(), Some("/m"));
}

#[tokio::test]
async fn a_store_variable_only_in_the_unit_file_does_not_apply() {
    let scratch = pohunek_test_support::tempdir().expect("scratch directory");
    let fragment = file_in(
        &scratch,
        "daemon.service",
        "[Service]\nEnvironment=CLAUDE_CONFIG_DIR=/u\n",
    );
    let entries = ["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"];
    let unit = UnitEnvironment {
        fragment_path: fragment,
        environment: vec!["CLAUDE_CONFIG_DIR=/u".to_owned()],
        ..loaded_unit()
    };
    let job = judge_job(unit.clone(), &entries, &[STORE], true)
        .await
        .expect("job");
    assert_eq!(variable(&job, STORE).as_deref(), Some("/m"));

    // A drop-in that does not touch the variable changes nothing either.
    let (_dir, with_drop_in) = loaded_with(
        &[("10-other.conf", "[Service]\nEnvironment=OTHER=1\n")],
        &["CLAUDE_CONFIG_DIR=/u", "OTHER=1"],
    );
    let with_drop_in = UnitEnvironment {
        fragment_path: unit.fragment_path,
        ..with_drop_in
    };
    let job = judge_job(with_drop_in, &entries, &[STORE], true)
        .await
        .expect("job");
    assert_eq!(variable(&job, STORE).as_deref(), Some("/m"));
}

#[tokio::test]
async fn an_unclear_drop_in_line_refuses_a_store_variable() {
    for line in [
        "Environment=\"CLAUDE_CONFIG_DIR=/x",
        "Environment=CLAUDE_CONFIG_DIR=/x\\x41",
        "Environment=CLAUDE_CONFIG\\x5fDIR=/x",
        "UnsetEnvironment=\"CLAUDE_CONFIG_DIR",
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_dir, unit) = loaded_with(&[("10-odd.conf", &text)], &[]);
        for recorded in [true, false] {
            let text = refusal(
                judge_job(
                    unit.clone(),
                    &["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"],
                    &[STORE],
                    recorded,
                )
                .await,
            );
            assert!(text.contains("10-odd.conf"), "{line}: {text}");
        }
    }
}

#[tokio::test]
async fn an_environment_file_store_variable_is_refused() {
    let scratch = pohunek_test_support::tempdir().expect("scratch directory");
    let file = file_in(&scratch, "store.env", "CLAUDE_CONFIG_DIR=/e\n");
    let text = format!("[Service]\nEnvironmentFile={file}\n");
    let (_dir, mut unit) = loaded_with(&[("10-file.conf", &text)], &[]);
    unit.environment_files = vec![(file.clone(), false)];
    let text = refusal(
        judge_job(
            unit,
            &["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"],
            &[STORE],
            true,
        )
        .await,
    );
    assert!(
        text.contains(&file)
            && text.contains("Move the assignment to an Environment= line of a drop-in"),
        "{text}"
    );
    assert!(!text.contains("/e"), "values stay out: {text}");
}

#[tokio::test]
async fn pass_environment_store_variables_are_refused_unless_overridden() {
    let (_dir, unit) = loaded_with(
        &[(
            "10-pass.conf",
            "[Service]\nPassEnvironment=CLAUDE_CONFIG_DIR\n",
        )],
        &[],
    );
    let text = refusal(
        judge_job(
            unit,
            &["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"],
            &[STORE],
            true,
        )
        .await,
    );
    assert!(text.contains("PassEnvironment="), "{text}");

    let (_dir, unit) = loaded_with(
        &[(
            "10-pass.conf",
            "[Service]\nPassEnvironment=CLAUDE_CONFIG_DIR\nEnvironment=CLAUDE_CONFIG_DIR=/d\n",
        )],
        &["CLAUDE_CONFIG_DIR=/d"],
    );
    let job = judge_job(
        unit,
        &["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"],
        &[STORE],
        true,
    )
    .await
    .expect("job");
    assert_eq!(variable(&job, STORE).as_deref(), Some("/d"));
}

#[tokio::test]
async fn unset_environment_removes_store_variables_as_systemd_does() {
    let entries = ["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"];
    for (line, expected) in [
        ("UnsetEnvironment=CLAUDE_CONFIG_DIR", None),
        ("UnsetEnvironment=CLAUDE_CONFIG_DIR=/m", None),
        ("UnsetEnvironment=CLAUDE_CONFIG_DIR=/other", Some("/m")),
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_dir, mut unit) = loaded_with(&[("10-unset.conf", &text)], &[]);
        unit.unset_environment = vec![line.trim_start_matches("UnsetEnvironment=").to_owned()];
        let job = judge_job(unit, &entries, &[STORE], true).await.expect(line);
        assert_eq!(variable(&job, STORE).as_deref(), expected, "{line}");
    }
    // Named only by the unit file the install replaces.
    let (_dir, mut unit) = loaded_with(
        &[("10-other.conf", "[Service]\nEnvironment=OTHER=1\n")],
        &["OTHER=1"],
    );
    unit.unset_environment = vec![STORE.to_owned()];
    let job = judge_job(unit, &entries, &[STORE], true)
        .await
        .expect("job");
    assert_eq!(variable(&job, STORE).as_deref(), Some("/m"));
}

#[tokio::test]
async fn drop_in_variables_follow_the_allowlist_rules() {
    // A denylisted name and a reserved-prefix name are skipped, as for the
    // manager's own variables.
    let (_dir, unit) = loaded_with(
        &[(
            "10-names.conf",
            "[Service]\nEnvironment=INVOCATION_ID=x WATCHDOG_USEC=1 POHUNEK_X=1 CLAUDE_Y=2\n",
        )],
        &[
            "INVOCATION_ID=x",
            "WATCHDOG_USEC=1",
            "POHUNEK_X=1",
            "CLAUDE_Y=2",
        ],
    );
    let job = judge_job(
        unit,
        &MANAGER_ENTRIES,
        &["INVOCATION_ID", "WATCHDOG_*", "POHUNEK_*", "CLAUDE_*"],
        true,
    )
    .await
    .expect("job");
    assert_eq!(job.variables.keys().collect::<Vec<_>>(), ["CLAUDE_Y"]);
}

#[tokio::test]
async fn recorded_and_manager_branches_select_the_same_variables() {
    let (_dir, unit) = loaded_with(
        &[(
            "10-store.conf",
            "[Service]\nEnvironment=CLAUDE_CONFIG_DIR=/d CLAUDE_X=1\n",
        )],
        &["CLAUDE_CONFIG_DIR=/d", "CLAUDE_X=1"],
    );
    let entries = ["PATH=/manager/bin", "CLAUDE_Z=3"];
    let recorded = judge_job(unit.clone(), &entries, &[STORE, "CLAUDE_*"], true)
        .await
        .expect("recorded");
    let managed = judge_job(unit, &entries, &[STORE, "CLAUDE_*"], false)
        .await
        .expect("managed");
    assert_eq!(recorded.variables, managed.variables);
    assert_eq!(recorded.variables.len(), 3);
}

#[tokio::test]
async fn a_removed_unit_with_a_drop_in_store_variable_is_judged_by_it() {
    let (_dir, unit) = removed_with(&[(
        "10-store.conf",
        "[Service]\nEnvironment=CLAUDE_CONFIG_DIR=/d\n",
    )]);
    let job = judge_job(
        unit,
        &["PATH=/manager/bin", "CLAUDE_CONFIG_DIR=/m"],
        &[STORE],
        true,
    )
    .await
    .expect("job");
    assert_eq!(variable(&job, STORE).as_deref(), Some("/d"));

    let (_dir, unit) = removed_with(&[(
        "10-store.conf",
        "[Service]\nEnvironment=CLAUDE_CONFIG_DIR=%h/store\n",
    )]);
    let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[STORE], true).await);
    assert!(text.contains("specifier"), "{text}");
}

// ---- bootstrap keys (#1394) ------------------------------------------------

const BOOTSTRAP_KEYS: [&str; 6] = [
    "HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
];

#[tokio::test]
async fn a_drop_in_bootstrap_key_is_refused() {
    for key in BOOTSTRAP_KEYS {
        let text = format!("[Service]\nEnvironment={key}=/other\n");
        let (_dir, unit) = loaded_with(&[("10-boot.conf", &text)], &[]);
        for recorded in [true, false] {
            let text = refusal(judge_job(unit.clone(), &MANAGER_ENTRIES, &[], recorded).await);
            assert!(
                text.contains(key) && text.contains("10-boot.conf"),
                "{key}: {text}"
            );
            assert!(!text.contains("/other"), "values stay out: {text}");
        }
    }
}

#[tokio::test]
async fn an_unclear_drop_in_bootstrap_key_is_refused() {
    for line in [
        "Environment=\"XDG_CONFIG_HOME=/x",
        "Environment=XDG_CONFIG_HOME=/x\\x41",
        "PassEnvironment=\"HOME",
    ] {
        let text = format!("[Service]\n{line}\n");
        let (_dir, unit) = loaded_with(&[("10-odd.conf", &text)], &[]);
        let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
        assert!(text.contains("10-odd.conf"), "{line}: {text}");
    }
}

#[tokio::test]
async fn an_environment_file_bootstrap_key_is_refused() {
    let scratch = pohunek_test_support::tempdir().expect("scratch directory");
    let file = file_in(&scratch, "boot.env", "XDG_STATE_HOME=/s\n");
    let text = format!("[Service]\nEnvironmentFile={file}\n");
    let (_dir, mut unit) = loaded_with(&[("10-file.conf", &text)], &[]);
    unit.environment_files = vec![(file.clone(), false)];
    let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
    assert!(
        text.contains("XDG_STATE_HOME") && text.contains(&file),
        "{text}"
    );
}

#[tokio::test]
async fn unset_environment_bootstrap_key_is_refused() {
    let (_dir, mut unit) = loaded_with(
        &[("10-unset.conf", "[Service]\nUnsetEnvironment=HOME\n")],
        &[],
    );
    unit.unset_environment = vec!["HOME".to_owned()];
    let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
    assert!(
        text.contains("HOME") && text.contains("UnsetEnvironment"),
        "{text}"
    );
}

#[tokio::test]
async fn pass_environment_bootstrap_key_has_no_effect() {
    let (_dir, mut unit) = loaded_with(
        &[("10-pass.conf", "[Service]\nPassEnvironment=HOME\n")],
        &[],
    );
    unit.pass_environment = vec!["HOME".to_owned()];
    judge_job(unit, &MANAGER_ENTRIES, &[], true)
        .await
        .expect("the unit file's Environment= overrides PassEnvironment=");
}

#[tokio::test]
async fn a_removed_unit_with_a_drop_in_bootstrap_key_is_refused() {
    let (_dir, unit) = removed_with(&[("10-boot.conf", "[Service]\nEnvironment=HOME=/other\n")]);
    let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], true).await);
    assert!(
        text.contains("HOME") && text.contains("10-boot.conf"),
        "{text}"
    );
}

#[tokio::test]
async fn a_drop_in_reset_drops_the_bootstrap_keys() {
    let (_dir, unit) = loaded_with(&[("10-reset.conf", "[Service]\nEnvironment=\n")], &[]);
    let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], false).await);
    assert!(
        text.contains("resets Environment=") && text.contains("HOME"),
        "{text}"
    );
}

// ---- every gate reads again, and judges the unit's value (S1, R15, R16, V11) --

/// A harness whose daemon job records `RECORDED`.
fn recorded_harness() -> Harness {
    let mut harness = Harness::new();
    harness.context = harness.context.clone().with_path_discovery(
        crate::service::context::PathDiscovery::Managed {
            login_shell: None,
            shell_defaulted: false,
            fallback_directories: vec![RECORDED.to_owned()],
        },
    );
    harness
}

#[tokio::test]
async fn the_upgrade_judges_every_gate_with_the_drop_in_path_over_the_recorded_one() {
    let mut harness = recorded_harness();
    let (_dir, unit) = loaded_with(
        &[("10-path.conf", "[Service]\nEnvironment=PATH=/x/bin\n")],
        &["PATH=/x/bin"],
    );
    harness.context = harness
        .context
        .clone()
        .with_user_manager(ScriptedManager::new(&["PATH=/manager/bin"], vec![unit]));
    harness.install(V1).await.expect("install");
    harness.runtimes.script().search_paths.clear();
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    let report = harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    let judged = judged.lock().expect("search path lock").clone();
    assert!(judged.len() >= 2, "{judged:?}");
    assert!(judged.iter().all(|path| path == "/x/bin"), "{judged:?}");
    let runtime_paths = harness.runtimes.script().search_paths.clone();
    assert!(!runtime_paths.is_empty());
    assert!(
        runtime_paths.iter().all(|path| path == "/x/bin"),
        "{runtime_paths:?}"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("10-path.conf")),
        "{:?}",
        report.warnings
    );
}

#[tokio::test]
async fn the_upgrade_refuses_a_recorded_path_override_before_any_effect() {
    let mut harness = recorded_harness();
    harness.install(V1).await.expect("install");
    let scratch = pohunek_test_support::tempdir().expect("scratch directory");
    let file = file_in(&scratch, "path.env", "PATH=/file/bin\n");
    let text = format!("[Service]\nEnvironmentFile={file}\n");
    let (_dir, mut unit) = loaded_with(&[("10-file.conf", &text)], &[]);
    unit.environment_files = vec![(file, false)];
    harness.context = harness
        .context
        .clone()
        .with_user_manager(ScriptedManager::new(&["PATH=/manager/bin"], vec![unit]));

    let refused = harness
        .engine()
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("the override is refused");
    assert_eq!(refused.code(), "service_manager_environment_unavailable");
    assert_eq!(harness.pending(), None, "no journal was written");
    harness.assert_installed(V1);
}

#[tokio::test]
async fn each_gate_reads_the_unit_environment_again() {
    let mut harness = recorded_harness();
    harness.install(V1).await.expect("install");
    let (_dir, with_drop_in) = loaded_with(
        &[("10-path.conf", "[Service]\nEnvironment=PATH=/x/bin\n")],
        &["PATH=/x/bin"],
    );
    harness.context = harness
        .context
        .clone()
        .with_user_manager(ScriptedManager::new(
            &["PATH=/manager/bin"],
            vec![loaded_unit(), with_drop_in],
        ));
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    let judged = judged.lock().expect("search path lock").clone();
    assert_eq!(
        judged.first().map(String::as_str),
        Some(RECORDED),
        "{judged:?}"
    );
    assert_eq!(
        judged.last().map(String::as_str),
        Some("/x/bin"),
        "{judged:?}"
    );
}

#[tokio::test]
async fn the_preflight_judges_store_references_at_the_drop_in_location() {
    let mut harness = recorded_harness();
    harness.install(V1).await.expect("install");
    let (_dir, unit) = loaded_with(
        &[("10-store.conf", "[Service]\nEnvironment=LC_CTYPE=/d\n")],
        &["LC_CTYPE=/d"],
    );
    harness.context = harness
        .context
        .clone()
        .with_user_manager(ScriptedManager::new(
            &["PATH=/manager/bin", "LC_CTYPE=/m"],
            vec![unit],
        ));
    let preflight = ScriptedPreflight::default();
    let variables = Arc::clone(&preflight.variables);

    harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect("upgrade");

    let variables = variables.lock().expect("variables lock").clone();
    assert!(!variables.is_empty());
    assert!(
        variables
            .iter()
            .all(|set| set.get("LC_CTYPE").map(String::as_str) == Some("/d")),
        "{variables:?}"
    );
}

#[tokio::test]
async fn the_rollback_gate_judges_the_drop_in_path_over_the_recorded_one() {
    let mut harness = recorded_harness();
    let (_dir, unit) = loaded_with(
        &[("10-path.conf", "[Service]\nEnvironment=PATH=/x/bin\n")],
        &["PATH=/x/bin"],
    );
    harness.context = harness
        .context
        .clone()
        .with_user_manager(ScriptedManager::new(&["PATH=/manager/bin"], vec![unit]));
    harness.install(V1).await.expect("install");
    let mut engine = harness.engine();
    engine.interrupt_after = Some(Step::Registered);
    engine
        .upgrade(&harness.staged(V2), V2)
        .await
        .expect_err("interrupted after the registration");
    let preflight = ScriptedPreflight::default();
    let judged = Arc::clone(&preflight.search_paths);

    let report = harness
        .engine()
        .with_adoption_preflight(Box::new(preflight))
        .upgrade(&harness.staged(V1), V1)
        .await
        .expect("the pending upgrade is rolled back");

    let judged = judged.lock().expect("search path lock").clone();
    assert!(
        !judged.is_empty() && judged.iter().all(|path| path == "/x/bin"),
        "{judged:?}"
    );
    assert!(report.rolled_back.is_some());
    harness.assert_installed(V1);
}

/// The syntax whose reading is not reproduced stays refused for a loaded unit:
/// the same files are refused once a rollback removes the unit.
#[tokio::test]
async fn a_loaded_unit_with_an_unfollowed_drop_in_syntax_is_still_refused() {
    for (content, reason) in [
        (
            "[Service]\nEnvironment=PATH=/a/bin\n[Bogus\nEnvironment=X=1\n",
            "without closing it",
        ),
        (
            "[Service]\nEnvironment=PATH=/a/bin\n\u{feff}Environment=X=1\n",
            "byte order mark",
        ),
    ] {
        let (_dir, unit) = loaded_with(&[("10-odd.conf", content)], &["PATH=/a/bin"]);
        let text = refusal(judge_job(unit, &MANAGER_ENTRIES, &[], false).await);
        assert!(
            text.contains("10-odd.conf") && text.contains(reason),
            "{text}"
        );
    }
}
