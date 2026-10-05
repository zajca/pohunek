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

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use pohunek_cli::service::preflight::{decide, gate, DaemonPreflight};
use pohunek_cli::service::{Context, Error, VERSION};
use pohunek_daemon::store::{DesiredState, RuntimeRecord, SessionRecord, Store};
use pohunek_paths::{BasePaths, PathEnv, Platform};
use pohunek_platform::process::{HostInspector, ProcessInspector as _};
use pohunek_service_config::preflight::Verdict;
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
        let executable = self.root.join("libexec/pohunek-sessiond");
        let record = SessionRecord {
            schema_version: pohunek_daemon::store::STORE_SCHEMA_VERSION,
            session_id: SESSION.to_owned(),
            desired_state: DesiredState::Running,
            transaction: None,
            info: session_info(),
            recovery: None,
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
        native_session_id: None,
        native_session_path: None,
        active_agent: None,
        active_agent_base: None,
        active_agent_pid: None,
        active_agent_session_id: None,
        active_agent_session_path: None,
        project_id: None,
        project_label: None,
        metadata: std::collections::BTreeMap::new(),
        is_linked_worktree: None,
        repo: None,
        branch: None,
        worktree_path: None,
        warnings: Vec::new(),
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

    let gated = gate(&DaemonPreflight, &host.context, &archive, VERSION, false)
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
async fn the_real_daemon_names_a_session_it_would_not_adopt_and_the_flag_accepts_it() {
    let host = Host::new();
    host.seed_live_session(true);
    let archive = host.archive();

    let error = gate(&DaemonPreflight, &host.context, &archive, VERSION, false)
        .await
        .expect_err("a record that cannot be loaded is refused");
    let Error::UpgradeAtRisk { sessions } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, SESSION);
    assert_eq!(sessions[0].verdict, Verdict::WouldNotBeAdopted);
    assert!(error.to_string().contains(SESSION), "{error}");

    let gated = gate(&DaemonPreflight, &host.context, &archive, VERSION, true)
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
        let error = gate(&DaemonPreflight, &host.context, &archive, VERSION, true)
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

#[tokio::test]
async fn a_missing_daemon_is_a_staged_binary_error() {
    let host = Host::new();
    let empty = host.root.join("empty");
    std::fs::create_dir_all(&empty).expect("empty dir");

    let error = gate(&DaemonPreflight, &host.context, &empty, VERSION, true)
        .await
        .expect_err("no daemon to ask");

    assert!(matches!(error, Error::StagedBinary { .. }), "{error:?}");
}
