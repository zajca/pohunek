//! The adoption preflight of `pohunek service check|upgrade` against the real
//! `pohunekd` binary.
//!
//! No service manager is involved: the preflight reads the store and the
//! worker journals below the XDG roots the CLI hands the daemon, so each test
//! writes those with the daemon's own store and the worker's own journal.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-06-26

#[path = "support/native.rs"]
mod native;
#[path = "../../daemon/tests/support/official_runtimes.rs"]
mod official_runtimes;

use std::future::Future;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use pohunek_cli::service::manager_env::{user_manager_environment, UserManager};
use pohunek_cli::service::preflight::{
    decide, gate, gate_upgrade, AdoptionPreflight as _, DaemonPreflight, ReleasePlan,
};
use pohunek_cli::service::{Context, Error, VERSION};
use pohunek_daemon::store::{DesiredState, RuntimeRecord, SessionRecord, Store};
use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_platform::process::{HostInspector, ProcessInspector as _};
use pohunek_service_config::preflight::{
    PreflightReport, SessionVerdict, StoreReport, StoreState, Verdict, RELEASE_PLAN_FLAG,
    REPORT_VERSION,
};
use pohunek_service_config::runtimes::{
    CatalogStatus, EnsureReport, Refusal, ENSURE_REPORT_VERSION,
};
use pohunek_session_worker::{Journal, JournalRecord, WorkerOrigin};
use protocol::{
    RuntimeGeneration, RuntimeRef, RuntimeState, SessionCapabilities, SessionId, SessionInfo,
    SessionRuntime, SessionState, StateSource,
};

/// A worker generation token of the valid grammar.
const GENERATION: &str = "abcd2345";
/// Session of the tests.
const SESSION: &str = "s-01KYAPVPFVHD56Z69B9CX3XWN2";
/// Worker of the session.
const WORKER: &str = "w-1";
/// Runtime with a hook-reported native identity, whose definition the daemon
/// under test serves from its official package
/// ([`Host::install_official_packages`]) to verify the journaled identity.
const IDENTITY_RUNTIME: &str = "hermes";

struct Host {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    context: Context,
}

impl Host {
    fn new() -> Self {
        let temporary = pohunek_test_support::tempdir_with_prefix("phk-").expect("temporary root");
        let root = temporary.path().to_path_buf();
        let dir = |name: &str| {
            let path = root.join(name);
            std::fs::create_dir_all(&path).expect("create XDG root");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("make XDG root private");
            path.into_os_string()
        };
        let paths = BasePaths::resolve_for(
            Platform::current().expect("platform"),
            nix::unistd::Uid::effective().as_raw(),
            &PathEnv {
                xdg_runtime_dir: Some(dir("run")),
                xdg_data_home: Some(dir("data")),
                xdg_state_home: Some(dir("state")),
                xdg_cache_home: Some(dir("cache")),
                xdg_config_home: Some(dir("config")),
                home: Some(dir("home")),
            },
        )
        .expect("resolve isolated paths");
        let context = Context::new(
            paths,
            nix::unistd::Uid::effective().as_raw(),
            Some(root.join("home")),
            Some(root.join("run")),
            root.join("supervisor"),
            pohunek_test_support::bin_exe("pohunek"),
        );
        Self {
            _temporary: temporary,
            root,
            context,
        }
    }

    /// Installs the official runtime packages into the plugin root the daemon
    /// under test reads.
    async fn install_official_packages(&self) {
        let scratch = self.root.join("official-packages");
        std::fs::create_dir_all(&scratch).expect("create the package scratch directory");
        official_runtimes::OfficialCatalog::new()
            .preinstall(&self.context.paths().plugins_dir(), &scratch)
            .await;
    }

    /// The directory a release archive unpacks to: the real daemon only.
    fn archive(&self) -> PathBuf {
        let archive = self.root.join("archive");
        std::fs::create_dir_all(&archive).expect("archive dir");
        let cli = pohunek_test_support::bin_exe("pohunek");
        let daemon = native::binary("POHUNEK_DAEMON_BIN", &cli, "pohunekd");
        std::fs::copy(daemon, archive.join("pohunekd")).expect("stage the daemon");
        archive
    }

    /// Writes a live session through the daemon's store and the worker's journal.
    fn seed_live_session(&self, broken_record: bool) {
        self.seed_session(broken_record, false);
    }

    /// Like [`Self::seed_live_session`]; with `reported_identity` the session
    /// is an agent of the hook runtime [`IDENTITY_RUNTIME`], recorded with a
    /// recovery pinned to its official package, whose worker journaled the
    /// active identity it reported and no hook schema.
    fn seed_session(&self, broken_record: bool, reported_identity: bool) {
        let executable = self.root.join("libexec/pohunek-sessiond");
        let mut info = session_info();
        if reported_identity {
            IDENTITY_RUNTIME.clone_into(&mut info.agent);
            info.agent_base = RuntimeRef::hermes();
        }
        let record = SessionRecord {
            schema_version: pohunek_daemon::store::STORE_SCHEMA_VERSION,
            session_id: SESSION.to_owned(),
            desired_state: DesiredState::Running,
            transaction: None,
            info,
            recovery: None,
            active_agent_origin: None,
            native_identity_ordering: None,
            runtime: RuntimeRecord {
                state: RuntimeState::Live,
                worker_id: Some(WORKER.to_owned()),
                worker_instance_id: None,
                service_id: Some(format!("{SESSION}.{GENERATION}")),
                generation: Some(GENERATION.to_owned()),
                executable: Some(executable.clone()),
                reason: None,
            },
        };
        let store = self.context.paths().data_dir.join("metadata.jsonl");
        Store::new(store.clone())
            .record_session(&record)
            .expect("record session");
        if reported_identity {
            let body = std::fs::read_to_string(&store).expect("read store");
            let mut line: serde_json::Value =
                serde_json::from_str(body.trim()).expect("store line");
            // The recovery a daemon of this release records for the session,
            // pinned to the package it launched the session from.
            line["recovery"] = serde_json::json!({
                "session_id": SESSION,
                "agent": IDENTITY_RUNTIME,
                "agent_base": IDENTITY_RUNTIME,
                "cwd": "/work",
                "cols": 80,
                "rows": 24,
                "launch_binding": official_runtimes::package_binding(IDENTITY_RUNTIME),
            });
            std::fs::write(&store, format!("{line}\n")).expect("write store");
        }
        if broken_record {
            let body = std::fs::read_to_string(&store).expect("read store");
            let mut line: serde_json::Value =
                serde_json::from_str(body.trim()).expect("store line");
            line.as_object_mut().expect("record").remove("runtime");
            std::fs::write(&store, format!("{line}\n")).expect("write store");
        }
        let own = HostInspector::new()
            .identity(std::process::id())
            .expect("inspect own process")
            .expect("own process exists");
        let mut journal = JournalRecord::bootstrap(
            SESSION.to_owned(),
            WORKER.to_owned(),
            WorkerOrigin {
                executable,
                version: VERSION.to_owned(),
                generation: GENERATION.to_owned(),
            },
            own.pid,
            own.start_identity.to_string(),
            "boot".to_owned(),
            (5, 6),
            "2026-09-24T00:00:00Z".to_owned(),
        );
        journal.phase = pohunek_session_worker::RuntimePhase::Live;
        journal.child = Some(pohunek_session_worker::ChildIdentity {
            pid: own.pid,
            process_group: 0,
            start_identity: own.start_identity.to_string(),
        });
        if reported_identity {
            let expires_at = (time::OffsetDateTime::now_utc() + time::Duration::seconds(30))
                .format(&time::format_description::well_known::Rfc3339)
                .expect("format expiry");
            journal.active_identity =
                journal
                    .child
                    .clone()
                    .map(|process| pohunek_session_worker::ActiveIdentity {
                        provider: IDENTITY_RUNTIME.to_owned(),
                        process,
                        sequence: 1,
                        expires_at,
                        reference_kind: Some("id".to_owned()),
                        native_reference: Some("native-reported".to_owned()),
                    });
        }
        let path = self
            .context
            .paths()
            .worker_journal(SESSION, WORKER)
            .expect("journal path");
        Journal::new(path).write(&journal).expect("write journal");
    }
}

fn session_info() -> SessionInfo {
    SessionInfo {
        name: None,
        id: SessionId(SESSION.to_owned()),
        external: Some(false),
        capabilities: SessionCapabilities::default(),
        agent: "shell".to_owned(),
        agent_base: RuntimeRef::shell(),
        cwd: PathBuf::from("/work"),
        cwd_source: None,
        pid: 1,
        cols: 80,
        rows: 24,
        state: SessionState::Running,
        state_source: StateSource::Process,
        activity: None,
        subagents: Vec::new(),
        background: None,
        native_session_id: None,
        native_session_path: None,
        active_agent: None,
        active_agent_base: None,
        active_agent_pid: None,
        active_agent_session_id: None,
        active_agent_session_path: None,
        native_last_activity_at: None,
        project_id: None,
        project_label: None,
        metadata: std::collections::BTreeMap::new(),
        is_linked_worktree: None,
        repo: None,
        branch: None,
        worktree_path: None,
        warnings: Vec::new(),
        capability_refusals: None,
        output_force_closed: false,
        created_at: "2026-09-24T00:00:00Z".to_owned(),
        updated_at: "2026-09-24T00:00:00Z".to_owned(),
        exit_code: None,
        runtime: Some(SessionRuntime {
            state: RuntimeState::Live,
            runtime_generation: RuntimeGeneration::new(1),
            worker_id: Some(WORKER.to_owned()),
            worker_instance_id: None,
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
        }),
    }
}

#[tokio::test]
async fn the_real_daemon_reports_a_clean_live_session_adoptable() {
    let host = Host::new();
    host.seed_live_session(false);
    let archive = host.archive();

    let gated = gate(
        &DaemonPreflight,
        &host.context,
        &archive,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        false,
    )
    .await
    .expect("a clean state passes the gate");

    assert!(!gated.accepted);
    assert_eq!(gated.report.daemon_version, VERSION);
    let verdicts: Vec<_> = gated
        .report
        .sessions
        .iter()
        .map(|session| (session.session_id.as_str(), session.verdict))
        .collect();
    assert_eq!(verdicts, [(SESSION, Verdict::Adoptable)]);
}

#[tokio::test]
async fn an_already_not_resumable_session_is_listed_without_requiring_the_flag() {
    let host = Host::new();
    let not_resumable = SessionVerdict {
        session_id: SESSION.to_owned(),
        name: None,
        verdict: Verdict::AlreadyNotResumable,
        code: "agent_native_reference_missing".to_owned(),
        detail: "resume refuses the session's native reference before any launch".to_owned(),
    };
    let report = daemon_report(None, vec![not_resumable.clone()]);
    let daemon = reporting_daemon(&host.root, "not-resumable", &report);

    let gated = gate_upgrade(
        &DaemonPreflight,
        &host.context,
        &daemon,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        ReleasePlan::Included,
        false,
    )
    .await
    .expect("a session the running release already refuses is not at risk");
    assert!(!gated.accepted);
    assert_eq!(gated.report.stopped_sessions, vec![not_resumable]);
    assert_eq!(gated.report.at_risk().count(), 0);
}

#[tokio::test]
async fn the_real_daemon_names_a_session_it_would_not_adopt_and_the_flag_accepts_it() {
    let host = Host::new();
    host.seed_live_session(true);
    let archive = host.archive();

    let error = gate(
        &DaemonPreflight,
        &host.context,
        &archive,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        false,
    )
    .await
    .expect_err("a record that cannot be loaded is refused");
    let Error::UpgradeAtRisk { sessions } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, SESSION);
    assert_eq!(sessions[0].verdict, Verdict::WouldNotBeAdopted);
    assert!(error.to_string().contains(SESSION), "{error}");

    let gated = gate(
        &DaemonPreflight,
        &host.context,
        &archive,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        true,
    )
    .await
    .expect("the flag accepts the loss");
    assert!(gated.accepted);
    assert_eq!(gated.report.at_risk().count(), 1);
    // `decide` is the whole policy: the same report is refused without the flag.
    let refused = decide(gated.report, false).expect_err("refused without the flag");
    assert!(
        matches!(refused, Error::UpgradeAtRisk { .. }),
        "{refused:?}"
    );
}

/// Stages a `pohunekd` that answers `--version` and then runs `body`.
fn scripted_daemon(root: &Path, body: &str) -> PathBuf {
    let archive = root.join("scripted");
    std::fs::create_dir_all(&archive).expect("archive dir");
    let script = archive.join("pohunekd");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'pohunekd {VERSION}'; exit 0; fi\n{body}\n"
        ),
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make script executable");
    archive
}

#[tokio::test]
async fn a_preflight_that_fails_or_prints_garbage_fails_closed() {
    let host = Host::new();
    for (name, body, expected) in [
        ("exits non-zero", "echo boom >&2; exit 3", "boom"),
        ("prints garbage", "echo not-json", "not valid"),
        (
            "prints a foreign report version",
            "echo '{\"report_version\":99}'",
            "not valid",
        ),
    ] {
        let archive = scripted_daemon(&host.root, body);
        let error = gate(
            &DaemonPreflight,
            &host.context,
            &archive,
            VERSION,
            "/usr/bin:/bin",
            &std::collections::BTreeMap::new(),
            true,
        )
        .await
        .expect_err(name);
        assert!(
            matches!(error, Error::UpgradePreflightFailed { .. }),
            "{name}: {error:?}"
        );
        assert!(error.to_string().contains(expected), "{name}: {error}");
        assert_eq!(error.code(), "service_upgrade_preflight_failed");
    }
}

/// A report of the release daemon with `runtimes` and one stopped session.
fn daemon_report(runtimes: Option<EnsureReport>, stopped: Vec<SessionVerdict>) -> PreflightReport {
    PreflightReport {
        report_version: REPORT_VERSION,
        daemon_version: VERSION.to_owned(),
        store: StoreReport {
            path: PathBuf::from("/data/metadata.jsonl"),
            state: StoreState::UpToDate,
            schema_from: None,
            schema_to: 4,
            records: 1,
            error_code: None,
            error: None,
        },
        sessions: Vec::new(),
        stopped_sessions: stopped,
        unmanaged_workers: Vec::new(),
        runtimes,
    }
}

/// Stages a `pohunekd` that prints `report` as its upgrade preflight.
fn reporting_daemon(root: &Path, name: &str, report: &PreflightReport) -> PathBuf {
    let root = root.join(name);
    std::fs::create_dir_all(&root).expect("report dir");
    let report_file = root.join("report.json");
    std::fs::write(&report_file, serde_json::to_vec(report).expect("serialize")).expect("write");
    scripted_daemon(&root, &format!("cat '{}'", report_file.display()))
}

#[tokio::test]
async fn the_daemon_report_fields_reach_the_upgrade_gate_through_the_process_boundary() {
    let host = Host::new();
    let refused = daemon_report(
        Some(EnsureReport {
            report_version: ENSURE_REPORT_VERSION,
            daemon_version: VERSION.to_owned(),
            dry_run: true,
            catalog: CatalogStatus::Current,
            packages: Vec::new(),
            profiles: Vec::new(),
            integrations: Vec::new(),
            refusals: vec![Refusal {
                subject: "claude".to_owned(),
                code: "package_unavailable_for_platform".to_owned(),
                detail: "no package for this platform".to_owned(),
            }],
            gaps: Vec::new(),
        }),
        Vec::new(),
    );
    let daemon = reporting_daemon(&host.root, "refused", &refused);
    let error = gate_upgrade(
        &DaemonPreflight,
        &host.context,
        &daemon,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        ReleasePlan::Included,
        true,
    )
    .await
    .expect_err("a refused plan is not overridable");
    let Error::UpgradeRuntimesRefused { refusals } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(refusals, &refused.runtimes.expect("plan").refusals);

    let stopped = SessionVerdict {
        session_id: SESSION.to_owned(),
        name: None,
        verdict: Verdict::WouldLoseRecovery,
        code: "runtime_source_not_served".to_owned(),
        detail: "the stopped session's runtime source is not served".to_owned(),
    };
    let at_risk = daemon_report(None, vec![stopped.clone()]);
    let daemon = reporting_daemon(&host.root, "at-risk", &at_risk);
    let error = gate_upgrade(
        &DaemonPreflight,
        &host.context,
        &daemon,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        ReleasePlan::Included,
        false,
    )
    .await
    .expect_err("a stopped session at risk needs the flag");
    let Error::UpgradeAtRisk { sessions } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(sessions, std::slice::from_ref(&stopped));

    let gated = gate_upgrade(
        &DaemonPreflight,
        &host.context,
        &daemon,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        ReleasePlan::Included,
        true,
    )
    .await
    .expect("the flag accepts the stopped session");
    assert!(gated.accepted);
}

#[tokio::test]
async fn the_daemon_judges_with_the_path_its_job_carries() {
    let host = Host::new();
    host.seed_live_session(false);
    let tools = host.root.join("tools");
    std::fs::create_dir_all(&tools).expect("tools dir");
    std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o755))
        .expect("make tools dir readable");
    let search_path = tools.display().to_string();
    // The wrapper refuses to run unless it has the job's PATH, then runs the
    // real daemon, so only the search path decides whether the judgment runs.
    let real = host.archive().join("pohunekd");
    let wrapper = scripted_daemon(
        &host.root.join("wrapper"),
        &format!(
            "[ \"$PATH\" = '{}' ] || exit 7\nexec '{}' \"$@\"",
            search_path,
            real.display()
        ),
    );

    let gated = gate(
        &DaemonPreflight,
        &host.context,
        &wrapper,
        VERSION,
        &search_path,
        &std::collections::BTreeMap::new(),
        false,
    )
    .await
    .expect("the daemon runs with the job's PATH and judges");
    assert_eq!(gated.report.sessions.len(), 1);

    let error = gate(
        &DaemonPreflight,
        &host.context,
        &wrapper,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        false,
    )
    .await
    .expect_err("without the job's PATH the wrapper refuses to judge");
    assert!(
        matches!(error, Error::UpgradePreflightFailed { .. }),
        "{error:?}"
    );
}

/// Stands in for the systemd user manager's `Environment` property.
#[derive(Debug)]
struct FakeUserManager(Result<Vec<String>, String>);

impl UserManager for FakeUserManager {
    fn environment(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + '_>> {
        let value = self.0.clone();
        Box::pin(async move { value })
    }
}

#[tokio::test]
async fn the_daemon_judges_with_the_user_managers_path_and_an_unreadable_one_fails_first() {
    let host = Host::new();
    host.seed_live_session(false);
    let tools = host.root.join("manager-tools");
    std::fs::create_dir_all(&tools).expect("tools dir");
    std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let manager_path = tools.display().to_string();
    let marker = host.root.join("wrapper-ran");
    let real = host.archive().join("pohunekd");
    // The wrapper runs the real daemon only with the user manager's PATH, and
    // records that it ran.
    let wrapper = scripted_daemon(
        &host.root.join("wrapper"),
        &format!(
            "[ \"$PATH\" = '{manager_path}' ] || exit 7\n: > '{}'\nexec '{}' \"$@\"",
            marker.display(),
            real.display()
        ),
    );
    let manager = FakeUserManager(Ok(vec![
        "HOME=/home/u".to_owned(),
        format!("PATH={manager_path}"),
    ]));

    let search_path = search_path_of(&manager)
        .await
        .expect("the manager reports its PATH");
    assert_eq!(search_path, manager_path);
    let gated = gate(
        &DaemonPreflight,
        &host.context,
        &wrapper,
        VERSION,
        &search_path,
        &std::collections::BTreeMap::new(),
        false,
    )
    .await
    .expect("the daemon runs with the user manager's PATH and judges");
    assert_eq!(gated.report.sessions.len(), 1);
    assert!(marker.exists(), "the daemon ran with the manager's PATH");

    std::fs::remove_file(&marker).expect("reset marker");
    let unreadable = FakeUserManager(Err("no session bus".to_owned()));
    let error = search_path_of(&unreadable)
        .await
        .expect_err("an unreadable environment is an error, not an empty PATH");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(error.to_string().contains("no session bus"), "{error}");

    let no_path = FakeUserManager(Ok(vec!["HOME=/home/u".to_owned()]));
    let error = search_path_of(&no_path)
        .await
        .expect_err("an environment without PATH is an error");
    assert_eq!(error.code(), "service_manager_environment_unavailable");
    assert!(!marker.exists(), "no daemon ran without the manager's PATH");
}

#[tokio::test]
async fn a_user_manager_path_is_judged_as_the_job_gets_it_even_with_an_open_directory() {
    let host = Host::new();
    // A directory any user may write to: the daemon job runs with it anyway, so
    // the judgment must use the same PATH and not refuse it.
    let open = host.root.join("open-tools");
    std::fs::create_dir_all(&open).expect("open dir");
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).expect("mode");
    let value = format!("{}:/usr/bin", open.display());
    let manager = FakeUserManager(Ok(vec!["HOME=/home/u".to_owned(), format!("PATH={value}")]));

    let search_path = search_path_of(&manager)
        .await
        .expect("the job's PATH is used as it is");
    assert_eq!(search_path, value);

    // A control character is no problem for a process environment: the value
    // is carried as it is.
    let control = FakeUserManager(Ok(vec!["PATH=/usr/bin:\u{7}/bin".to_owned()]));
    assert_eq!(
        search_path_of(&control).await.expect("carried"),
        "/usr/bin:\u{7}/bin"
    );
}

/// Stages a `pohunekd` that reports `reported` as its version, appends its
/// arguments to `record`, and prints `report` for an argument list matching the
/// shell pattern `accept`. Any other list exits 9, as a release without the flag
/// does when it is given an argument it does not know.
fn recording_stand_in(
    root: &Path,
    reported: &str,
    record: &Path,
    accept: &str,
    report: &PreflightReport,
) -> PathBuf {
    let archive = root.join(format!("stand-in-{reported}"));
    std::fs::create_dir_all(&archive).expect("archive dir");
    let report_file = archive.join("report.json");
    std::fs::write(&report_file, serde_json::to_vec(report).expect("serialize")).expect("write");
    let script = archive.join("pohunekd");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'pohunekd {reported}'; exit 0; fi\n\
             printf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n{accept}) cat '{}' ;;\n*) exit 9 ;;\nesac\n",
            record.display(),
            report_file.display()
        ),
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make script executable");
    archive
}

#[tokio::test]
async fn a_raised_probe_budget_reaches_only_this_releases_upgrade_judgement() {
    let host = Host::new();
    host.seed_live_session(false);
    let report = daemon_report(None, Vec::new());
    let record = host.root.join("argv-record");
    let no_variables = std::collections::BTreeMap::new();
    // A previous release accepts only the protocol it had: the bare upgrade-preflight argument.
    let previous = recording_stand_in(
        &host.root,
        "0.0.1",
        &record,
        "\"upgrade-preflight\"",
        &report,
    );
    let current = recording_stand_in(
        &host.root,
        VERSION,
        &record,
        "\"upgrade-preflight\"*",
        &report,
    );
    let raised = host
        .context
        .clone()
        .with_preflight_budget(std::time::Duration::from_secs(90));
    let argv = || std::fs::read_to_string(&record).unwrap_or_default();

    // The rollback judges with the previous release's daemon: no flag, so it runs.
    gate(
        &DaemonPreflight,
        &raised,
        &previous,
        "0.0.1",
        "/usr/bin:/bin",
        &no_variables,
        false,
    )
    .await
    .expect("the rollback's judgement runs the previous release's protocol");
    assert_eq!(
        argv(),
        "upgrade-preflight\n",
        "a rollback never sends the budget"
    );
    std::fs::remove_file(&record).expect("reset record");

    // This release's upgrade with a raised budget sends the flag.
    gate_upgrade(
        &DaemonPreflight,
        &raised,
        &current,
        VERSION,
        "/usr/bin:/bin",
        &no_variables,
        ReleasePlan::Omitted,
        false,
    )
    .await
    .expect("this release accepts the raised budget");
    assert_eq!(argv(), "upgrade-preflight --probe-budget-secs 90\n");
    std::fs::remove_file(&record).expect("reset record");

    // The default budget keeps the argument list every release accepts.
    gate_upgrade(
        &DaemonPreflight,
        &host.context,
        &current,
        VERSION,
        "/usr/bin:/bin",
        &no_variables,
        ReleasePlan::Omitted,
        false,
    )
    .await
    .expect("the default budget judges");
    assert_eq!(
        argv(),
        "upgrade-preflight\n",
        "the default argument list is unchanged"
    );
}

#[tokio::test]
async fn the_manager_variables_reach_the_spawned_daemon_and_their_values_stay_out_of_errors() {
    const NAME: &str = "CLAUDE_CONFIG_DIR";
    const SENTINEL: &str = "/manager/sentinel-store-7f3a";
    let host = Host::new();
    host.seed_live_session(false);
    let variables = std::collections::BTreeMap::from([(NAME.to_owned(), SENTINEL.to_owned())]);
    let received = host.root.join("received-store");
    let real = host.archive().join("pohunekd");
    // The stand-in records the variable it was started with, then runs the real daemon.
    let recording = scripted_daemon(
        &host.root.join("recording-variables"),
        &format!(
            "printf '%s' \"${NAME}\" > '{}'\nexec '{}' \"$@\"",
            received.display(),
            real.display()
        ),
    );

    let gated = gate(
        &DaemonPreflight,
        &host.context,
        &recording,
        VERSION,
        "/usr/bin:/bin",
        &variables,
        false,
    )
    .await
    .expect("the daemon judges with the manager's variable");
    assert_eq!(gated.report.sessions.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&received).expect("the daemon's record"),
        SENTINEL,
        "the spawned daemon received the manager's variable"
    );

    // A daemon that cannot start fails the run with diagnostics. The value of the
    // variable is not part of any error, because it may be a secret.
    let failing = scripted_daemon(&host.root.join("failing-variables"), "exit 3");
    let error = gate(
        &DaemonPreflight,
        &host.context,
        &failing,
        VERSION,
        "/usr/bin:/bin",
        &variables,
        false,
    )
    .await
    .expect_err("a daemon that exits without a report fails the gate");
    assert!(!error.to_string().contains(SENTINEL), "{error}");
    assert!(!format!("{error:?}").contains(SENTINEL), "{error:?}");
}

#[tokio::test]
async fn a_user_manager_path_reaches_the_spawned_daemon_byte_for_byte() {
    let host = Host::new();
    host.seed_live_session(false);
    let open = host.root.join("resolving-tools");
    std::fs::create_dir_all(&open).expect("open dir");
    let open_text = open.display().to_string();
    // A relative entry and a repeated directory: the job runs with this exact
    // value, so the spawned daemon must receive it unchanged.
    let value = format!("relative/bin:{open_text}:/usr/bin:{open_text}");
    let manager = FakeUserManager(Ok(vec![format!("PATH={value}")]));
    let received = host.root.join("received-path");
    let real = host.archive().join("pohunekd");
    // The stand-in records the PATH it was started with, then runs the real daemon.
    let recording = scripted_daemon(
        &host.root.join("recording"),
        &format!(
            "printf '%s' \"$PATH\" > '{}'\nexec '{}' \"$@\"",
            received.display(),
            real.display()
        ),
    );

    let search_path = search_path_of(&manager)
        .await
        .expect("a value a process environment carries is used");
    assert_eq!(search_path, value);
    let gated = gate(
        &DaemonPreflight,
        &host.context,
        &recording,
        VERSION,
        &search_path,
        &std::collections::BTreeMap::new(),
        false,
    )
    .await
    .expect("the daemon judges");
    assert_eq!(gated.report.sessions.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&received).expect("the daemon's record"),
        value,
        "the spawned daemon received the job's PATH unchanged"
    );

    // Only a NUL byte cannot be carried by a process environment.
    let nul = FakeUserManager(Ok(vec!["PATH=/usr/bin\u{0}/bin".to_owned()]));
    assert_eq!(
        search_path_of(&nul)
            .await
            .expect_err("a NUL byte is refused")
            .code(),
        "service_manager_environment_unavailable"
    );
}

#[tokio::test]
async fn the_release_plan_flag_reaches_the_daemon_only_for_the_upgrade_judgement() {
    let host = Host::new();
    host.seed_live_session(false);
    let real = host.archive().join("pohunekd");
    // Each stand-in runs the real daemon only with the argument its caller must
    // pass: the upgrade's judgement with the release-plan flag, a rollback's
    // judgement with none.
    let with_plan = scripted_daemon(
        &host.root.join("with-plan"),
        &format!(
            "[ \"$2\" = '{RELEASE_PLAN_FLAG}' ] || exit 7\nexec '{}' \"$@\"",
            real.display()
        ),
    );
    let without_plan = scripted_daemon(
        &host.root.join("without-plan"),
        &format!(
            "[ \"$2\" != '{RELEASE_PLAN_FLAG}' ] || exit 8\nexec '{}' \"$@\"",
            real.display()
        ),
    );

    // The report carries the release plan only when the flag reached the
    // daemon; this host has no installed configuration, so the plan itself is
    // reported as unreadable, which is the daemon's own answer.
    let report = DaemonPreflight
        .judge(
            &host.context,
            &with_plan.join("pohunekd"),
            "/usr/bin:/bin",
            &std::collections::BTreeMap::new(),
            None,
            ReleasePlan::Included,
        )
        .await
        .expect("the upgrade's judgement passes the release-plan flag");
    assert!(report.runtimes.is_some(), "the plan was judged");

    gate(
        &DaemonPreflight,
        &host.context,
        &without_plan,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        false,
    )
    .await
    .expect("a rollback's judgement passes no flag");

    let error = gate_upgrade(
        &DaemonPreflight,
        &host.context,
        &without_plan,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        ReleasePlan::Included,
        false,
    )
    .await
    .expect_err("the upgrade's judgement without the flag is not what the daemon answers");
    assert!(
        matches!(error, Error::UpgradePreflightFailed { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_missing_daemon_is_a_staged_binary_error() {
    let host = Host::new();
    let empty = host.root.join("empty");
    std::fs::create_dir_all(&empty).expect("empty dir");

    let error = gate(
        &DaemonPreflight,
        &host.context,
        &empty,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        true,
    )
    .await
    .expect_err("no daemon to ask");

    assert!(matches!(error, Error::StagedBinary { .. }), "{error:?}");
}

#[tokio::test]
async fn a_live_agent_session_that_reported_its_identity_is_adoptable_under_the_real_daemon() {
    let host = Host::new();
    host.install_official_packages().await;
    host.seed_session(false, true);
    let archive = host.archive();

    let gated = gate(
        &DaemonPreflight,
        &host.context,
        &archive,
        VERSION,
        "/usr/bin:/bin",
        &std::collections::BTreeMap::new(),
        false,
    )
    .await
    .expect("a reported active identity is the normal case and passes the gate");

    let verdicts: Vec<_> = gated
        .report
        .sessions
        .iter()
        .map(|session| {
            (
                session.session_id.as_str(),
                session.verdict,
                session.code.as_str(),
            )
        })
        .collect();
    assert_eq!(verdicts, [(SESSION, Verdict::Adoptable, "adoptable")]);
}

/// The `PATH` the manager gives the job: a manager environment without one is an
/// error, as the job would have no `PATH` to run with.
async fn search_path_of(manager: &dyn UserManager) -> Result<String, Error> {
    user_manager_environment(manager, &[])
        .await?
        .search_path
        .ok_or_else(|| Error::ManagerEnvironment {
            detail: "it sets no PATH entry".to_owned(),
        })
}
