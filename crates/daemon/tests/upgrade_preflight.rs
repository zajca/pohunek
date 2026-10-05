//! The adoption preflight judges the installed state without changing it.
//!
//! Every host is a hermetic environment holding a store written by the
//! v0.33.0 daemon (schema 1) and worker journals of the current schema whose
//! worker is this test process, so the process table proves it running.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use pohunek_daemon::procwatch::{HostInspector, ProcessInspector};
use pohunek_daemon::session::upgrade_preflight::{run, PreflightInputs};
use pohunek_daemon::store::STORE_SCHEMA_VERSION;
use pohunek_service_config::preflight::{
    PreflightReport, StoreState, Verdict, CODE_ADOPTABLE, CODE_GENERATION_MISSING,
    CODE_JOURNAL_MISSING, CODE_JOURNAL_SCHEMA_UNSUPPORTED, CODE_NO_SESSION_RECORD,
    CODE_PROTOCOL_INCOMPATIBLE, CODE_RECORD_UNREADABLE, CODE_RECOVERY_UNMAPPABLE,
    CODE_RESUME_RECORD_UNREADABLE, CODE_RUNTIME_IDENTITY_MISMATCH, CODE_STORE_UNUSABLE,
    CODE_WORKER_ENDED, CODE_WORKER_NOT_RUNNING, REPORT_VERSION,
};
use pohunek_test_support::bin_exe;
use pohunek_test_support::env::TestEnv;
use pohunek_worker_protocol::SUPPORTED_RANGE;
use serde_json::{json, Value};

/// Store written by the v0.33.0 daemon (schema 1).
const V0_33_0_STORE: &str = include_str!("../src/store/fixtures/v0.33.0/metadata.jsonl");

/// Worker executable the journals and records name.
const WORKER_EXECUTABLE: &str = "/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond";

/// A worker generation token of the valid grammar.
const GENERATION: &str = "abcd2345";

/// Journal schema the daemon reads.
const JOURNAL_SCHEMA: u64 = 4;

/// Edits a journal into one the preflight must refuse.
type Mutation = fn(&mut Value);

struct Host {
    env: TestEnv,
    store: PathBuf,
    workers: PathBuf,
}

impl Host {
    fn new() -> Self {
        let env = TestEnv::new().expect("hermetic environment");
        let data_dir = env.data_home().join("pohunek");
        let workers = env.state_home().join("pohunek").join("workers");
        for directory in [&data_dir, &workers] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .expect("create private directory");
        }
        Self {
            store: data_dir.join("metadata.jsonl"),
            workers,
            env,
        }
    }

    fn write_store(&self, lines: &[Value]) {
        let body = lines.iter().fold(String::new(), |mut body, line| {
            body.push_str(&line.to_string());
            body.push('\n');
            body
        });
        fs::write(&self.store, body).expect("write store");
        fs::set_permissions(&self.store, fs::Permissions::from_mode(0o600))
            .expect("make store private");
    }

    fn write_journal(&self, session: &str, worker: &str, journal: &Value) {
        let directory = self.workers.join(session);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .expect("create journal directory");
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(directory.join(format!("{worker}.json")))
            .expect("create journal");
        std::io::Write::write_all(&mut file, journal.to_string().as_bytes())
            .expect("write journal");
    }

    /// Writes raw journal bytes, which need not be a journal.
    fn write_journal_bytes(&self, session: &str, worker: &str, bytes: &[u8]) {
        let directory = self.workers.join(session);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)
            .expect("create journal directory");
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(directory.join(format!("{worker}.json")))
            .expect("create journal");
        std::io::Write::write_all(&mut file, bytes).expect("write journal");
    }

    /// Appends a store line the loader cannot interpret and names no session.
    fn append_corrupt_line(&self) {
        let mut body = fs::read_to_string(&self.store).expect("read store");
        body.push_str("{\"kind\":\"session\",\"session_id\":\n");
        fs::write(&self.store, body).expect("append corrupt line");
    }

    fn inputs(&self) -> PreflightInputs {
        PreflightInputs {
            store_path: self.store.clone(),
            worker_state_root: self.workers.clone(),
            plugins_dir: self.env.state_home().join("pohunek").join("plugins"),
        }
    }

    fn report(&self) -> PreflightReport {
        run(&self.inputs(), &HostInspector::new())
    }

    /// Every file below the environment root with its bytes and mode.
    fn snapshot(&self) -> BTreeMap<PathBuf, (u32, Vec<u8>)> {
        fn walk(path: &Path, into: &mut BTreeMap<PathBuf, (u32, Vec<u8>)>) {
            let metadata = fs::symlink_metadata(path).expect("stat");
            let bytes = if metadata.is_file() {
                fs::read(path).expect("read")
            } else {
                Vec::new()
            };
            into.insert(path.to_path_buf(), (metadata.permissions().mode(), bytes));
            if metadata.is_dir() {
                for entry in fs::read_dir(path).expect("list") {
                    walk(&entry.expect("entry").path(), into);
                }
            }
        }
        let mut into = BTreeMap::new();
        walk(self.env.root(), &mut into);
        into
    }
}

/// The session line of the v0.33.0 fixture, rebound to `session`.
fn session_line(session: &str, worker: &str) -> Value {
    let template = V0_33_0_STORE
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("fixture line"))
        .find(|line| line["kind"] == "session")
        .expect("fixture session line");
    let mut line = template;
    line["session_id"] = json!(session);
    line["info"]["id"] = json!(session);
    line["recovery"]["session_id"] = json!(session);
    line["runtime"] = json!({
        "state": "live",
        "worker_id": worker,
        "runtime_id": "r-fixture",
        "service_id": format!("{session}.{GENERATION}"),
        "generation": GENERATION,
        "executable": WORKER_EXECUTABLE,
    });
    line
}

/// A live journal of this test process, which is provably running.
fn journal(session: &str, worker: &str) -> Value {
    let identity = HostInspector::new()
        .identity(std::process::id())
        .expect("inspect own process")
        .expect("own process exists");
    json!({
        "schema_version": JOURNAL_SCHEMA,
        "session_id": session,
        "worker_id": worker,
        "generation": GENERATION,
        "executable": WORKER_EXECUTABLE,
        "worker_pid": identity.pid,
        "worker_start_identity": identity.start_identity.get().to_string(),
        "boot_identity": "boot-test",
        "runtime_id": "r-fixture",
        "child": {
            "pid": identity.pid,
            "process_group": 0,
            "start_identity": identity.start_identity.get().to_string(),
        },
        "cols": 120,
        "rows": 40,
        "phase": "live",
        "protocol_minimum": SUPPORTED_RANGE.minimum().get(),
        "protocol_maximum": SUPPORTED_RANGE.maximum().get(),
    })
}

fn verdict_of<'a>(report: &'a PreflightReport, session: &str) -> (&'a Verdict, &'a str) {
    let found = report
        .sessions
        .iter()
        .find(|candidate| candidate.session_id == session)
        .unwrap_or_else(|| panic!("no verdict for {session}: {:?}", report.sessions));
    (&found.verdict, found.code.as_str())
}

#[test]
fn clean_sessions_of_an_old_store_are_adoptable_and_nothing_is_written() {
    let host = Host::new();
    host.write_store(&[
        session_line("s-1", "w-fixture"),
        session_line("s-2", "w-two"),
    ]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    host.write_journal("s-2", "w-two", &journal("s-2", "w-two"));
    let before = host.snapshot();

    let report = host.report();

    assert_eq!(report.report_version, REPORT_VERSION);
    assert_eq!(report.store.state, StoreState::WouldMigrate);
    assert_eq!(report.store.schema_from, Some(1));
    assert_eq!(report.store.schema_to, STORE_SCHEMA_VERSION);
    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE)
    );
    assert_eq!(
        verdict_of(&report, "s-2"),
        (&Verdict::Adoptable, CODE_ADOPTABLE)
    );
    assert_eq!(report.at_risk().count(), 0);
    assert_eq!(
        host.snapshot(),
        before,
        "the preflight changes nothing on disk"
    );
}

#[test]
fn a_session_record_that_cannot_be_loaded_is_named_and_not_adopted() {
    let host = Host::new();
    let mut broken = session_line("s-1", "w-fixture");
    broken
        .as_object_mut()
        .expect("record object")
        .remove("runtime");
    host.write_store(&[broken, session_line("s-2", "w-two")]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    host.write_journal("s-2", "w-two", &journal("s-2", "w-two"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldNotBeAdopted, CODE_RECORD_UNREADABLE)
    );
    assert_eq!(verdict_of(&report, "s-2").0, &Verdict::Adoptable);
}

#[test]
fn a_recovery_that_cannot_be_mapped_loses_native_recovery() {
    let host = Host::new();
    let mut line = session_line("s-1", "w-fixture");
    line["recovery"]["resume_mode"] = json!("not-a-mode");
    host.write_store(&[line]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldLoseRecovery, CODE_RECOVERY_UNMAPPABLE)
    );
}

/// A resume line of the fixture rebound to `s-1` with its `agent` removed, so
/// the loader skips it.
fn broken_resume_line() -> Value {
    let mut broken = V0_33_0_STORE
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("fixture line"))
        .find(|line| line["kind"] == "resume")
        .expect("fixture resume line");
    broken["session_id"] = json!("s-1");
    // Its legacy mode maps to no launch spec and it lacks a required field, so
    // neither the migration nor the loader can use it.
    broken["resume_mode"] = json!("no-such-mode");
    broken.as_object_mut().expect("object").remove("agent");
    broken
}

#[test]
fn an_unreadable_resume_line_loses_recovery_only_when_the_record_depends_on_it() {
    let host = Host::new();
    let mut dependent = session_line("s-1", "w-fixture");
    dependent["recovery"] = Value::Null;
    host.write_store(&[dependent, broken_resume_line()]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldLoseRecovery, CODE_RESUME_RECORD_UNREADABLE)
    );

    // The record keeps its own complete recovery, which startup adopts with.
    let host = Host::new();
    host.write_store(&[session_line("s-1", "w-fixture"), broken_resume_line()]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE)
    );
}

#[test]
fn a_record_without_its_kind_still_names_its_live_worker() {
    let host = Host::new();
    let mut kindless = session_line("s-1", "w-fixture");
    kindless.as_object_mut().expect("record").remove("kind");
    host.write_store(&[kindless]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldNotBeAdopted, CODE_RECORD_UNREADABLE)
    );
    assert!(report.unmanaged_workers.is_empty());
}

#[test]
fn a_journal_of_another_runtime_instance_is_not_adopted() {
    let host = Host::new();
    host.write_store(&[session_line("s-1", "w-fixture")]);
    let mut other = journal("s-1", "w-fixture");
    other["runtime_id"] = json!("r-other");
    host.write_journal("s-1", "w-fixture", &other);

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldNotBeAdopted, CODE_RUNTIME_IDENTITY_MISMATCH)
    );
}

#[test]
fn a_migrated_store_over_the_durable_read_limit_is_refused() {
    let host = Host::new();
    // Unversioned lines grow by the schema stamp when migrated, so a store
    // that fits the limit can migrate past it.
    let line = format!("{{\"kind\":\"project\",\"pad\":\"{}\"}}\n", "x".repeat(40));
    let limit = 16 * 1024 * 1024;
    let body = line.repeat(limit / line.len());
    assert!(body.len() <= limit);
    fs::write(&host.store, &body).expect("write store");
    fs::set_permissions(&host.store, fs::Permissions::from_mode(0o600)).expect("private");
    let before = host.snapshot();

    let report = host.report();

    assert!(report.store_refused());
    assert_eq!(report.store.error_code.as_deref(), Some("store_too_large"));
    assert_eq!(host.snapshot(), before);
}

#[test]
fn journal_evidence_failures_each_name_their_reason() {
    let cases: [(&str, Mutation, &str); 6] = [
        (
            "an unsupported journal schema",
            |journal| journal["schema_version"] = json!(3),
            CODE_JOURNAL_SCHEMA_UNSUPPORTED,
        ),
        (
            "a worker process that is gone",
            |journal| {
                let start = journal["worker_start_identity"]
                    .as_str()
                    .and_then(|start| start.parse::<u64>().ok())
                    .expect("start identity");
                journal["worker_start_identity"] = json!((start + 1).to_string());
            },
            CODE_WORKER_NOT_RUNNING,
        ),
        (
            "a worker that ended",
            |journal| journal["phase"] = json!("terminal"),
            CODE_WORKER_ENDED,
        ),
        (
            "a worker protocol outside the window",
            |journal| {
                journal["protocol_minimum"] = json!(1);
                journal["protocol_maximum"] = json!(2);
            },
            CODE_PROTOCOL_INCOMPATIBLE,
        ),
        (
            "a live worker with no PTY child",
            |journal| journal["child"] = Value::Null,
            "worker_child_missing",
        ),
        (
            "a journal of another generation",
            |journal| journal["generation"] = json!("bcde2345"),
            CODE_JOURNAL_MISSING,
        ),
    ];
    for (name, mutate, code) in cases {
        let host = Host::new();
        host.write_store(&[session_line("s-1", "w-fixture")]);
        let mut broken = journal("s-1", "w-fixture");
        mutate(&mut broken);
        host.write_journal("s-1", "w-fixture", &broken);

        let report = host.report();

        assert_eq!(
            verdict_of(&report, "s-1"),
            (&Verdict::WouldNotBeAdopted, code),
            "{name}"
        );
    }
}

#[test]
fn a_record_without_a_generation_is_not_adopted() {
    let host = Host::new();
    let mut line = session_line("s-1", "w-fixture");
    line["runtime"] = json!({ "state": "live", "worker_id": "w-fixture" });
    host.write_store(&[line]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldNotBeAdopted, CODE_GENERATION_MISSING)
    );
}

#[test]
fn a_launch_identity_that_contradicts_the_record_is_not_adopted() {
    let host = Host::new();
    host.write_store(&[session_line("s-1", "w-fixture")]);
    let mut contradicting = journal("s-1", "w-fixture");
    let child = contradicting["child"].clone();
    contradicting["launch_identity"] = json!({
        "provider": "codex",
        "process": child,
        "reference_kind": "id",
        "native_reference": "native-fixture-2",
    });
    host.write_journal("s-1", "w-fixture", &contradicting);

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (
            &Verdict::WouldNotBeAdopted,
            "launch_identity_provider_mismatch"
        )
    );
}

#[test]
fn a_live_worker_without_a_record_is_unmanaged_and_a_dead_journal_is_not_listed() {
    let host = Host::new();
    host.write_store(&[session_line("s-1", "w-fixture")]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    host.write_journal("s-2", "w-orphan", &journal("s-2", "w-orphan"));
    let mut ended = journal("s-3", "w-ended");
    ended["phase"] = json!("terminal");
    host.write_journal("s-3", "w-ended", &ended);

    let report = host.report();

    assert_eq!(report.unmanaged_workers, ["s-2"]);
    assert_eq!(report.at_risk().count(), 0);
    assert!(report
        .sessions
        .iter()
        .all(|session| session.session_id != "s-3"));
}

#[test]
fn an_unreadable_store_line_without_a_session_makes_a_workers_without_a_record_at_risk() {
    let host = Host::new();
    host.write_store(&[session_line("s-1", "w-fixture")]);
    let mut body = fs::read_to_string(&host.store).expect("read store");
    body.push_str("this line is not json\n");
    fs::write(&host.store, body).expect("append corrupt line");
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    host.write_journal("s-2", "w-orphan", &journal("s-2", "w-orphan"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-2"),
        (&Verdict::WouldNotBeAdopted, CODE_NO_SESSION_RECORD)
    );
    assert!(report.unmanaged_workers.is_empty());
}

#[test]
fn a_store_the_daemon_would_refuse_is_reported_and_left_untouched() {
    let host = Host::new();
    let mut newer = session_line("s-1", "w-fixture");
    newer["schema_version"] = json!(STORE_SCHEMA_VERSION + 1);
    host.write_store(&[newer]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    let before = host.snapshot();

    let report = host.report();

    assert!(report.store_refused());
    assert_eq!(
        report.store.error_code.as_deref(),
        Some("store_newer_than_binary")
    );
    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldNotBeAdopted, CODE_STORE_UNUSABLE)
    );
    assert_eq!(host.snapshot(), before);
}

#[test]
fn a_store_at_the_current_schema_is_up_to_date() {
    let host = Host::new();
    host.write_store(&[session_line("s-1", "w-fixture")]);
    pohunek_daemon::store::migrate_at_startup(&host.store).expect("migrate fixture store");
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(report.store.state, StoreState::UpToDate);
    assert_eq!(verdict_of(&report, "s-1").0, &Verdict::Adoptable);
}

#[test]
fn the_binary_prints_the_report_without_creating_or_changing_anything() {
    let host = Host::new();
    host.write_store(&[session_line("s-1", "w-fixture")]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    let before = host.snapshot();

    let output = host
        .env
        .command(bin_exe("pohunekd"))
        .arg("upgrade-preflight")
        .output()
        .expect("run pohunekd");

    assert!(output.status.success(), "{output:?}");
    let report: PreflightReport =
        serde_json::from_slice(&output.stdout).expect("stdout is the report");
    assert_eq!(report.daemon_version, pohunek_daemon::DAEMON_VERSION);
    assert_eq!(verdict_of(&report, "s-1").0, &Verdict::Adoptable);
    assert_eq!(
        host.snapshot(),
        before,
        "no file, directory, lock or log appears"
    );
}

#[test]
fn a_session_whose_runtime_the_record_already_lost_is_not_judged() {
    let host = Host::new();
    // The persisted state of a lost runtime keeps the session `running`.
    let mut lost = session_line("s-1", "w-fixture");
    lost["runtime"]["state"] = json!("lost");
    let mut unreadable_and_lost = session_line("s-2", "w-two");
    unreadable_and_lost["runtime"]["state"] = json!("lost");
    unreadable_and_lost["info"]
        .as_object_mut()
        .expect("info")
        .remove("cwd");
    host.write_store(&[lost, unreadable_and_lost]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert!(report.sessions.is_empty(), "{:?}", report.sessions);
    assert_eq!(report.at_risk().count(), 0);
}

#[test]
fn an_unidentified_unreadable_line_taints_every_session_that_may_depend_on_it() {
    for (name, line) in [
        ("invalid json", "this line is not json".to_owned()),
        (
            "a resume line without a session",
            json!({ "kind": "resume", "agent": "claude" }).to_string(),
        ),
    ] {
        let host = Host::new();
        let mut dependent = session_line("s-1", "w-fixture");
        dependent["recovery"] = Value::Null;
        host.write_store(&[dependent]);
        let mut body = fs::read_to_string(&host.store).expect("read store");
        body.push_str(&line);
        body.push('\n');
        fs::write(&host.store, body).expect("append line");
        host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

        let report = host.report();

        assert_eq!(
            verdict_of(&report, "s-1"),
            (&Verdict::WouldLoseRecovery, CODE_RESUME_RECORD_UNREADABLE),
            "{name}"
        );
    }
}

#[test]
fn a_worker_that_never_initialized_or_has_no_child_is_not_adopted() {
    for (phase, child, code) in [
        ("bootstrap", Some(()), "worker_not_initialized"),
        ("starting", None, "worker_child_missing"),
    ] {
        let host = Host::new();
        host.write_store(&[session_line("s-1", "w-fixture")]);
        let mut early = journal("s-1", "w-fixture");
        early["phase"] = json!(phase);
        if child.is_none() {
            early["child"] = Value::Null;
        }
        host.write_journal("s-1", "w-fixture", &early);

        let report = host.report();

        assert_eq!(
            verdict_of(&report, "s-1"),
            (&Verdict::WouldNotBeAdopted, code),
            "{phase}"
        );
    }
}

#[test]
fn a_corrupt_store_line_hides_no_session_whose_journal_is_unreadable_or_unsupported() {
    let mut unsupported = journal("s-1", "w-fixture");
    unsupported["schema_version"] = json!(3);
    for (name, bytes, code) in [
        (
            "unreadable journal",
            b"not json".to_vec(),
            "worker_journal_unreadable",
        ),
        (
            "unsupported journal schema",
            unsupported.to_string().into_bytes(),
            CODE_JOURNAL_SCHEMA_UNSUPPORTED,
        ),
    ] {
        let host = Host::new();
        host.write_store(&[]);
        host.append_corrupt_line();
        host.write_journal_bytes("s-1", "w-fixture", &bytes);

        let report = host.report();

        assert_eq!(
            verdict_of(&report, "s-1"),
            (&Verdict::WouldNotBeAdopted, code),
            "{name}"
        );
    }

    // Without damage to the store, an unreadable journal of an unknown session
    // is no evidence of a lost session.
    let host = Host::new();
    host.write_store(&[]);
    host.write_journal_bytes("s-1", "w-fixture", b"not json");
    assert_eq!(host.report().at_risk().count(), 0);
}

#[test]
fn a_journal_root_that_cannot_be_scanned_is_never_an_empty_adoptable_report() {
    let host = Host::new();
    host.write_store(&[]);
    fs::set_permissions(&host.workers, fs::Permissions::from_mode(0o755))
        .expect("make the journal root unsafe");

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "(worker journals)"),
        (&Verdict::WouldNotBeAdopted, "evidence_unavailable")
    );
    assert_eq!(report.at_risk().count(), 1);
}

/// Journals an active identity of `provider` on the PTY root of `journal`, as a
/// live agent session that reported its native id does.
fn with_active_identity(journal: &mut Value, provider: &str) {
    use time::format_description::well_known::Rfc3339;
    let expires_at = (time::OffsetDateTime::now_utc() + time::Duration::seconds(30))
        .format(&Rfc3339)
        .expect("format expiry");
    journal["active_identity"] = json!({
        "provider": provider,
        "process": journal["child"].clone(),
        "sequence": 1,
        "expires_at": expires_at,
        "reference_kind": "id",
        "native_reference": "native-reported",
    });
}

/// Rebinds the record of `line` to the runtime `agent`.
fn rebind_agent(line: &mut Value, agent: &str) {
    line["info"]["agent"] = json!(agent);
    line["info"]["agent_base"] = json!(agent);
    line["recovery"]["agent"] = json!(agent);
    line["recovery"]["agent_base"] = json!(agent);
}

#[test]
fn a_built_in_runtime_that_reported_its_identity_is_adoptable_with_or_without_a_journaled_schema() {
    for journaled_schema in [false, true] {
        let host = Host::new();
        host.write_store(&[session_line("s-1", "w-fixture")]);
        let mut reported = journal("s-1", "w-fixture");
        with_active_identity(&mut reported, "claude");
        if journaled_schema {
            reported["hook_schema"] = json!("identity-subagent-v1");
        }
        host.write_journal("s-1", "w-fixture", &reported);

        let report = host.report();

        assert_eq!(
            verdict_of(&report, "s-1"),
            (&Verdict::Adoptable, CODE_ADOPTABLE),
            "journaled schema: {journaled_schema}: {:?}",
            report.sessions
        );
    }
}

#[test]
fn a_runtime_whose_definition_cannot_be_read_stays_unverified() {
    let host = Host::new();
    let mut line = session_line("s-1", "w-fixture");
    rebind_agent(&mut line, "acme-agent");
    host.write_store(&[line]);
    let mut reported = journal("s-1", "w-fixture");
    with_active_identity(&mut reported, "acme-agent");
    host.write_journal("s-1", "w-fixture", &reported);

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldNotBeAdopted, "worker_identity_unverified")
    );
}

#[test]
fn reading_runtime_definitions_creates_nothing_even_in_an_existing_plugin_root() {
    let host = Host::new();
    host.write_store(&[session_line("s-1", "w-fixture")]);
    let mut reported = journal("s-1", "w-fixture");
    with_active_identity(&mut reported, "claude");
    host.write_journal("s-1", "w-fixture", &reported);
    let plugins = host.env.state_home().join("pohunek").join("plugins");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&plugins)
        .expect("create plugin root");
    let before = host.snapshot();

    let report = host.report();

    assert_eq!(verdict_of(&report, "s-1").0, &Verdict::Adoptable);
    assert_eq!(
        host.snapshot(),
        before,
        "no packages directory or lock appears"
    );
}
