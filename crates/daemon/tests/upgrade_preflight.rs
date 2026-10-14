//! The adoption preflight judges the installed state without changing it.
//!
//! Every host is a hermetic environment holding a store written by the
//! v0.33.0 daemon (schema 1) and worker journals of the current schema whose
//! worker is this test process, so the process table proves it running. A
//! test whose verdict needs the session's runtime served by this binary
//! installs the official packages ([`Host::serving`]) and rebinds the
//! fixture's Claude session to one of them, pinned to the installed package
//! ([`served_session_line`]).

#[path = "support/official_runtimes.rs"]
mod official_runtimes;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_daemon::agent::host::PackageStore;
use pohunek_daemon::procwatch::{HostInspector, ProcessInspector};
use pohunek_daemon::runtime::{EnvironmentSource, LaunchEnvironment};
use pohunek_daemon::session::upgrade_preflight::{
    run_with, PreflightInputs, PreflightOptions, ReleaseInputs,
};
use pohunek_daemon::store::STORE_SCHEMA_VERSION;
use pohunek_service_config::preflight::{
    PreflightReport, StoreState, Verdict, CODE_ADOPTABLE, CODE_GENERATION_MISSING,
    CODE_JOURNAL_MISSING, CODE_JOURNAL_SCHEMA_UNSUPPORTED, CODE_NO_SESSION_RECORD,
    CODE_PROTOCOL_INCOMPATIBLE, CODE_RECORD_UNREADABLE, CODE_RECOVERY_UNMAPPABLE,
    CODE_RESUME_RECORD_UNREADABLE, CODE_RUNTIME_IDENTITY_MISMATCH, CODE_RUNTIME_SOURCE_NOT_SERVED,
    CODE_STORE_UNUSABLE, CODE_WORKER_ENDED, CODE_WORKER_NOT_RUNNING, REPORT_VERSION,
};
use pohunek_service_config::preflight::{
    CODE_PROBE_BUDGET_EXHAUSTED, PREFLIGHT_TIMEOUT, PROBE_BUDGET, RELEASE_PLAN_FLAG,
};
use pohunek_service_config::runtimes::{
    PackageStatus, ProfileStatus, CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM,
    CODE_RUNTIME_PLAN_UNAVAILABLE, CODE_SERVICE_CONFIG_UNREADABLE, RELEASE_RUNTIME_DIR,
};
#[path = "support/service_file.rs"]
mod service_file;

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

/// Package version a worker of this build journals.
const WORKER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Last release whose worker marks its children with `POHUNEK_RUNTIME_ID` only.
const LEGACY_MARKER_RELEASE: &str = "0.33.0";

/// First release whose worker marks its children with
/// `POHUNEK_WORKER_INSTANCE_ID`.
const INSTANCE_MARKER_RELEASE: &str = "0.33.1";

/// A hook runtime an official package serves; [`served_session_line`] pins a
/// fixture session to that package, which [`Host::serving`] installs, so its
/// recovery stays served by the binary under test.
const SERVED_RUNTIME: &str = "hermes";

/// The hook schema the [`SERVED_RUNTIME`] package descriptor declares.
const SERVED_RUNTIME_HOOK_SCHEMA: &str = "identity-v1";

/// Edits a journal into one the preflight must refuse.
type Mutation = fn(&mut Value);

/// The default allowlist as the service file records it.
fn default_allowlist() -> Vec<String> {
    pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
        .iter()
        .map(|pattern| (*pattern).to_owned())
        .collect()
}

/// The runtimes a fake agent of the test host is written for.
const FAKE_AGENTS: [&str; 3] = ["claude", "codex", "hermes"];

/// Directory below the test root that holds the fake agents.
const AGENT_BIN_DIR: &str = "bin";

/// Refusal of a version probe that rejects an executable that exists.
const CODE_AGENT_RUNTIME_UNSUPPORTED: &str = "agent_runtime_unsupported";

/// Refusal of a launch program that resolves to no executable.
const CODE_AGENT_BINARY_MISSING: &str = "agent_binary_missing";

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
        let host = Self {
            store: data_dir.join("metadata.jsonl"),
            workers,
            env,
        };
        host.write_fake_agents();
        host
    }

    /// Writes one fake agent per runtime. Each answers the version probe of the
    /// official package with the release that package accepts, and nothing
    /// else, so a session launched from it is judged as a supported agent.
    fn write_fake_agents(&self) {
        let bin = self.agent_path();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&bin)
            .expect("create the agent directory");
        for runtime in FAKE_AGENTS {
            let agent = bin.join(runtime);
            fs::write(
                &agent,
                format!(
                    "#!/bin/sh\n{}exit 1\n",
                    official_runtimes::sh_version_answer(runtime)
                ),
            )
            .expect("write the fake agent");
            fs::set_permissions(&agent, fs::Permissions::from_mode(0o700))
                .expect("make the fake agent executable");
        }
    }

    fn agent_path(&self) -> PathBuf {
        self.env.root().join(AGENT_BIN_DIR)
    }

    /// A host whose plugin root holds the official packages, so the binary
    /// under test serves [`SERVED_RUNTIME`] from its package.
    async fn serving() -> Self {
        let host = Self::new();
        official_runtimes::OfficialCatalog::new()
            .preinstall(&host.inputs().plugins_dir, host.env.root())
            .await;
        host
    }

    /// Writes the store lines. A recorded program named after a fake agent
    /// becomes that agent's absolute path, so the probe runs the fake whatever
    /// the test process's `PATH` holds.
    fn write_store(&self, lines: &[Value]) {
        let body = lines.iter().fold(String::new(), |mut body, line| {
            let mut line = line.clone();
            for pointer in ["/program", "/recovery/program"] {
                let Some(name) = line.pointer(pointer).and_then(Value::as_str) else {
                    continue;
                };
                if FAKE_AGENTS.contains(&name) {
                    let absolute = self.agent_path().join(name).to_string_lossy().into_owned();
                    *line.pointer_mut(pointer).expect("the program field") = json!(absolute);
                }
            }
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

    /// The environment of the test host: its own home, and a `PATH` that holds
    /// only its fake agents, so no test reads the developer's integrations or
    /// tools.
    fn source(&self) -> EnvironmentSource {
        EnvironmentSource::fixed([
            (
                OsString::from("HOME"),
                self.env.home().as_os_str().to_owned(),
            ),
            (OsString::from("PATH"), self.agent_path().into_os_string()),
        ])
    }

    /// The launch environment of the test host: the default allowlist over
    /// [`Self::source`], as a service file of the default allowlist configures.
    fn launch(&self) -> LaunchEnvironment {
        LaunchEnvironment::new(default_allowlist(), self.source())
    }

    /// The preflight options of the test host: no release.
    fn options(&self) -> PreflightOptions {
        PreflightOptions {
            release: None,
            profiles_dir: None,
            host_state_dir: None,
            probe_budget: PROBE_BUDGET,
            entry: std::time::Instant::now(),
            launch: Ok(self.launch()),
        }
    }

    /// Writes the installed service file of the test host with `allowlist`,
    /// at the path the daemon binary reads it from.
    fn write_service(&self, allowlist: &[String]) -> PathBuf {
        let config = self.env.config_home().join("pohunek");
        let runtime = self.env.runtime_dir().join("pohunek");
        for directory in [&config, &runtime] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)
                .expect("create service directory");
        }
        let path = config.join(pohunek_service_config::FILE_NAME);
        service_file::write_service_file(
            &path,
            self.env.root(),
            &fs::canonicalize(self.env.state_home().join("pohunek")).expect("state root"),
            &fs::canonicalize(&runtime).expect("runtime root"),
            allowlist,
        );
        path
    }

    fn report(&self) -> PreflightReport {
        run_with(&self.inputs(), &self.options(), &HostInspector::new())
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

/// [`session_line`] rebound to [`SERVED_RUNTIME`] and pinned to its official
/// package, for a judgement that needs the session's runtime served by the
/// binary under test on a [`Host::serving`].
fn served_session_line(session: &str, worker: &str) -> Value {
    let mut line = session_line(session, worker);
    rebind_agent(&mut line, SERVED_RUNTIME);
    pin_recovery(
        &mut line,
        serde_json::to_value(official_runtimes::package_binding(SERVED_RUNTIME))
            .expect("serialize the package binding"),
    );
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
        "version": WORKER_VERSION,
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

#[tokio::test]
async fn clean_sessions_of_an_old_store_are_adoptable_and_nothing_is_written() {
    let host = Host::serving().await;
    host.write_store(&[
        served_session_line("s-1", "w-fixture"),
        served_session_line("s-2", "w-two"),
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
    let report_json = serde_json::to_value(&report).expect("serialize preflight report");
    assert_eq!(report_json["sessions"][0]["verdict"], "adoptable");
    assert_eq!(
        host.snapshot(),
        before,
        "the preflight changes nothing on disk"
    );
}

#[tokio::test]
async fn a_session_record_that_cannot_be_loaded_is_named_and_not_adopted() {
    let host = Host::serving().await;
    let mut broken = served_session_line("s-1", "w-fixture");
    broken
        .as_object_mut()
        .expect("record object")
        .remove("runtime");
    host.write_store(&[broken, served_session_line("s-2", "w-two")]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    host.write_journal("s-2", "w-two", &journal("s-2", "w-two"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldNotBeAdopted, CODE_RECORD_UNREADABLE)
    );
    assert_eq!(verdict_of(&report, "s-2").0, &Verdict::Adoptable);
    let report_json = serde_json::to_value(&report).expect("serialize preflight report");
    assert_eq!(
        report_json["sessions"][0]["verdict"],
        "would_not_be_adopted"
    );
}

#[tokio::test]
async fn a_recovery_that_cannot_be_mapped_loses_native_recovery() {
    let host = Host::serving().await;
    let mut line = served_session_line("s-1", "w-fixture");
    line["recovery"]["resume_mode"] = json!("not-a-mode");
    host.write_store(&[line]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldLoseRecovery, CODE_RECOVERY_UNMAPPABLE)
    );
    let report_json = serde_json::to_value(&report).expect("serialize preflight report");
    assert_eq!(report_json["sessions"][0]["verdict"], "would_lose_recovery");
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

#[tokio::test]
async fn an_unreadable_resume_line_loses_recovery_only_when_the_record_depends_on_it() {
    let host = Host::serving().await;
    let mut dependent = served_session_line("s-1", "w-fixture");
    dependent["recovery"] = Value::Null;
    host.write_store(&[dependent, broken_resume_line()]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldLoseRecovery, CODE_RESUME_RECORD_UNREADABLE)
    );

    // The record keeps its own complete recovery, which startup adopts with.
    let host = Host::serving().await;
    host.write_store(&[
        served_session_line("s-1", "w-fixture"),
        broken_resume_line(),
    ]);
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
    let cases: [(&str, Mutation, &str); 7] = [
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
        (
            "a journal that names no worker release",
            |journal| {
                journal
                    .as_object_mut()
                    .expect("journal object")
                    .remove("version");
            },
            "worker_marker_legacy",
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

/// A live process carrying the session marker and only the worker marker of
/// a pre-v0.33.1 worker; killed and reaped on drop.
struct LegacyMarkedChild(std::process::Child);

impl LegacyMarkedChild {
    fn spawn(session: &str) -> Self {
        let child = pohunek_test_support::process_env::command("sleep")
            .arg("60")
            .env_remove("POHUNEK_WORKER_INSTANCE_ID")
            .env("POHUNEK_SESSION_ID", session)
            .env("POHUNEK_WORKER_ID", "w-fixture")
            .env("POHUNEK_RUNTIME_ID", "r-fixture")
            .spawn()
            .expect("spawn legacy-marked child");
        Self(child)
    }

    fn running(&mut self) -> bool {
        self.0
            .try_wait()
            .expect("poll legacy-marked child")
            .is_none()
    }
}

impl Drop for LegacyMarkedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A worker released before `POHUNEK_WORKER_INSTANCE_ID` marks its children
/// with a name this daemon does not read, so its live runtime is refused
/// rather than adopted unsweepable; the first marking release is adoptable.
#[tokio::test]
async fn a_live_worker_older_than_the_instance_marker_is_not_adopted() {
    let host = Host::serving().await;
    host.write_store(&[
        served_session_line("s-1", "w-fixture"),
        served_session_line("s-2", "w-two"),
    ]);
    let mut legacy = journal("s-1", "w-fixture");
    legacy["version"] = json!(LEGACY_MARKER_RELEASE);
    host.write_journal("s-1", "w-fixture", &legacy);
    let mut first_marking = journal("s-2", "w-two");
    first_marking["version"] = json!(INSTANCE_MARKER_RELEASE);
    host.write_journal("s-2", "w-two", &first_marking);
    let mut child = LegacyMarkedChild::spawn("s-1");
    let before = host.snapshot();

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldNotBeAdopted, "worker_marker_legacy")
    );
    let detail = &report
        .sessions
        .iter()
        .find(|session| session.session_id == "s-1")
        .expect("verdict of s-1")
        .detail;
    assert!(
        detail.contains(LEGACY_MARKER_RELEASE) && detail.contains("--accept-runtime-loss"),
        "the detail names the worker release and the recovery: {detail}"
    );
    assert_eq!(
        verdict_of(&report, "s-2"),
        (&Verdict::Adoptable, CODE_ADOPTABLE)
    );
    assert_eq!(report.at_risk().count(), 1);
    assert!(child.running(), "the preflight signals nothing");
    assert_eq!(host.snapshot(), before, "the preflight changes nothing");
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

#[tokio::test]
async fn a_live_worker_without_a_record_is_unmanaged_and_a_dead_journal_is_not_listed() {
    let host = Host::serving().await;
    host.write_store(&[served_session_line("s-1", "w-fixture")]);
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

#[tokio::test]
async fn a_store_at_the_current_schema_is_up_to_date() {
    let host = Host::serving().await;
    host.write_store(&[served_session_line("s-1", "w-fixture")]);
    pohunek_daemon::store::migrate_at_startup(&host.store).expect("migrate fixture store");
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(report.store.state, StoreState::UpToDate);
    assert_eq!(verdict_of(&report, "s-1").0, &Verdict::Adoptable);
}

#[tokio::test]
async fn the_binary_prints_the_report_without_creating_or_changing_anything() {
    let host = Host::serving().await;
    host.write_store(&[served_session_line("s-1", "w-fixture")]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    host.write_service(&default_allowlist());
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
fn a_session_whose_runtime_the_record_already_lost_is_judged_only_as_a_stopped_session() {
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

    // Its worker is gone, so it is not a live verdict. Its resume still needs
    // a built-in runtime this binary does not compile in, and no package serves
    // it. The record the loader skips is reported too: startup loses its recovery.
    assert!(report.sessions.is_empty(), "{:?}", report.sessions);
    assert_eq!(
        report
            .stopped_sessions
            .iter()
            .map(|session| (session.session_id.as_str(), session.verdict))
            .collect::<Vec<_>>(),
        [
            ("s-1", Verdict::WouldLoseRecovery),
            ("s-2", Verdict::WouldLoseRecovery)
        ],
        "{:?}",
        report.stopped_sessions
    );
    assert_eq!(report.at_risk().count(), 2);
}

#[tokio::test]
async fn an_unidentified_unreadable_line_taints_every_session_that_may_depend_on_it() {
    for (name, line) in [
        ("invalid json", "this line is not json".to_owned()),
        (
            "a resume line without a session",
            json!({ "kind": "resume", "agent": "claude" }).to_string(),
        ),
    ] {
        let host = Host::serving().await;
        let mut dependent = served_session_line("s-1", "w-fixture");
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
    line["recovery"]["program"] = json!(agent);
}

#[tokio::test]
async fn a_package_runtime_that_reported_its_identity_is_adoptable_with_or_without_a_journaled_schema(
) {
    for journaled_schema in [false, true] {
        let host = Host::serving().await;
        host.write_store(&[served_session_line("s-1", "w-fixture")]);
        let mut reported = journal("s-1", "w-fixture");
        with_active_identity(&mut reported, SERVED_RUNTIME);
        if journaled_schema {
            reported["hook_schema"] = json!(SERVED_RUNTIME_HOOK_SCHEMA);
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

#[tokio::test]
async fn reading_runtime_definitions_creates_nothing_even_in_an_existing_plugin_root() {
    let host = Host::serving().await;
    host.write_store(&[served_session_line("s-1", "w-fixture")]);
    let mut reported = journal("s-1", "w-fixture");
    with_active_identity(&mut reported, SERVED_RUNTIME);
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

/// Descriptor digest of a built-in launch pin; the preflight compares the
/// pin's runtime and package identity, never this digest.
const DESCRIPTOR_DIGEST: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";

/// Archive digest of a package launch pin no installed package has.
const MISSING_PACKAGE_DIGEST: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// Pins the recovery of `line` to `binding`, as a daemon that launched the
/// session from that runtime source records it.
fn pin_recovery(line: &mut Value, binding: Value) {
    line["recovery"]["launch_binding"] = binding;
}

#[tokio::test]
async fn a_session_pinned_to_a_runtime_source_this_binary_does_not_serve_loses_recovery() {
    // `pi` ships as no built-in runtime of this binary, and no package
    // serves it, so neither pin resolves.
    let unserved = [
        (
            "a built-in this binary does not ship",
            json!({
                "runtime_id": "pi",
                "provenance": {
                    "kind": "builtin",
                    "package": {"id": "pohunek.runtime.pi", "version": "1.0.0"},
                    "descriptor_digest": DESCRIPTOR_DIGEST,
                },
            }),
        ),
        (
            "a package digest that is not installed",
            json!({
                "runtime_id": "pi",
                "provenance": {
                    "kind": "package",
                    "package": {"id": "acme.runtime.pi", "version": "1.0.0"},
                    "package_digest": MISSING_PACKAGE_DIGEST,
                },
            }),
        ),
    ];
    for (case, binding) in unserved {
        let host = Host::serving().await;
        let mut line = served_session_line("s-1", "w-fixture");
        rebind_agent(&mut line, "pi");
        pin_recovery(&mut line, binding);
        host.write_store(&[line, served_session_line("s-2", "w-two")]);
        host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
        host.write_journal("s-2", "w-two", &journal("s-2", "w-two"));
        let before = host.snapshot();

        let report = host.report();

        assert_eq!(
            verdict_of(&report, "s-1"),
            (&Verdict::WouldLoseRecovery, CODE_RUNTIME_SOURCE_NOT_SERVED),
            "{case}: {:?}",
            report.sessions
        );
        let detail = &report
            .sessions
            .iter()
            .find(|verdict| verdict.session_id == "s-1")
            .expect("the verdict of s-1")
            .detail;
        assert!(detail.contains("runtime_not_installed"), "{case}: {detail}");
        assert!(
            detail.contains("`pohunek session new --agent pi`"),
            "{case}: {detail}"
        );
        assert!(detail.contains("--accept-runtime-loss"), "{case}: {detail}");
        assert_eq!(
            verdict_of(&report, "s-2"),
            (&Verdict::Adoptable, CODE_ADOPTABLE),
            "{case}: an unpinned session of a served built-in is unchanged"
        );
        assert_eq!(host.snapshot(), before, "{case}: nothing is written");
    }
}

/// The release install step of the official package of `runtime`, as the
/// preflight detail names it.
fn release_install_step(runtime: &str) -> String {
    format!(
        "`pohunek plugin install runtime/packages/{runtime}.tar.zst --catalog runtime/runtime-catalog.json`"
    )
}

/// The v0.33.0 fixture session as the record of a session of `runtime`; the
/// fixture itself is a Claude session, so `claude` keeps the record unmodified.
/// Codex and Hermes never forked, so their record is not forkable.
fn built_in_session_line(runtime: &str) -> Value {
    let mut line = session_line("s-1", "w-fixture");
    if runtime != "claude" {
        rebind_agent(&mut line, runtime);
        line["recovery"]["forkable"] = json!(false);
    }
    line
}

/// The launch pins a release that compiled `runtime` in recorded for a session
/// of its built-in runtime, by name: none (the v0.33.0 record), and the
/// built-in provenance a v0.34 daemon pins.
fn built_in_pins(runtime: &str) -> [(&'static str, Option<Value>); 2] {
    [
        ("unpinned", None),
        (
            "pinned to the built-in",
            Some(json!({
                "runtime_id": runtime,
                "provenance": {
                    "kind": "builtin",
                    "package": {"id": format!("pohunek.runtime.{runtime}"), "version": "1.0.0"},
                    "descriptor_digest": DESCRIPTOR_DIGEST,
                },
            })),
        ),
    ]
}

/// A session of `runtime` the v0.33.0 daemon launched from its built-in runtime
/// is adopted by the official package of the same identity only when it is
/// pinned to the built-in and the package honors the launch it froze. Without
/// the package the pin names a built-in that is gone. An unpinned record is
/// never adopted, so the package that serves the alias is no stand-in for it.
async fn assert_built_in_session_is_adopted_only_when_pinned(runtime: &str) {
    for preinstalled in [false, true] {
        for (pin, binding) in built_in_pins(runtime) {
            let case = format!("{runtime} {pin}, package installed: {preinstalled}");
            let host = Host::new();
            if preinstalled {
                official_runtimes::OfficialCatalog::new()
                    .preinstall(&host.inputs().plugins_dir, host.env.root())
                    .await;
            }
            let mut line = built_in_session_line(runtime);
            if let Some(binding) = binding {
                pin_recovery(&mut line, binding);
            }
            host.write_store(&[line]);
            host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

            let report = host.report();

            if preinstalled && pin == "pinned to the built-in" {
                assert_eq!(
                    verdict_of(&report, "s-1"),
                    (&Verdict::Adoptable, CODE_ADOPTABLE),
                    "{case}: {:?}",
                    report.sessions
                );
                continue;
            }
            assert_eq!(
                verdict_of(&report, "s-1"),
                (&Verdict::WouldLoseRecovery, CODE_RUNTIME_SOURCE_NOT_SERVED),
                "{case}: {:?}",
                report.sessions
            );
            let detail = &report
                .sessions
                .iter()
                .find(|verdict| verdict.session_id == "s-1")
                .expect("the verdict of s-1")
                .detail;
            let refusal = if preinstalled {
                "runtime_served_by_package"
            } else {
                "runtime_builtin_removed"
            };
            assert!(detail.contains(refusal), "{case}: {detail}");
            assert!(
                detail.contains(&format!("`pohunek session new --agent {runtime}`")),
                "{case}: {detail}"
            );
            // Without the package the adopted session refuses input too, which
            // installing the package before the upgrade avoids.
            assert_eq!(
                detail.contains(&release_install_step(runtime)),
                !preinstalled,
                "{case}: {detail}"
            );
        }
    }
}

/// A Claude session the v0.33.0 daemon launched from its built-in runtime is
/// adopted by the installed official package only when the package honors the
/// reference kind the session froze; a path-kind reference is refused.
#[tokio::test]
async fn a_built_in_claude_session_whose_frozen_reference_kind_the_package_does_not_honor_loses_recovery(
) {
    let host = Host::new();
    official_runtimes::OfficialCatalog::new()
        .preinstall(&host.inputs().plugins_dir, host.env.root())
        .await;
    let mut line = built_in_session_line("claude");
    let (_pin, binding) = built_in_pins("claude")
        .into_iter()
        .find(|(pin, _)| *pin == "pinned to the built-in")
        .expect("the pinned record");
    pin_recovery(&mut line, binding.expect("the pin"));
    line["recovery"]["ref_kind"] = json!("path");
    host.write_store(&[line]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

    let report = host.report();

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldLoseRecovery, CODE_RUNTIME_SOURCE_NOT_SERVED),
        "{:?}",
        report.sessions
    );
    let detail = &report.sessions[0].detail;
    assert!(detail.contains("runtime_served_by_package"), "{detail}");
    assert!(
        detail.contains("`pohunek session new --agent claude`"),
        "{detail}"
    );
}

/// The Codex session record the v0.33.0 daemon wrote is adopted by the
/// installed official package only when pinned to the built-in.
#[tokio::test]
async fn a_session_of_the_built_in_codex_is_adopted_only_when_pinned_and_the_package_is_installed()
{
    assert_built_in_session_is_adopted_only_when_pinned("codex").await;
}

/// The unmodified Claude session record the v0.33.0 daemon wrote is adopted by
/// the installed official package only when pinned to the built-in.
#[tokio::test]
async fn a_session_of_the_built_in_claude_is_adopted_only_when_pinned_and_the_package_is_installed()
{
    assert_built_in_session_is_adopted_only_when_pinned("claude").await;
}

/// The Hermes session record the v0.33.0 daemon wrote is adopted by the
/// installed official package only when pinned to the built-in.
#[tokio::test]
async fn a_session_of_the_built_in_hermes_is_adopted_only_when_pinned_and_the_package_is_installed()
{
    assert_built_in_session_is_adopted_only_when_pinned("hermes").await;
}

/// A live session of `runtime` the v0.33.0 daemon launched from its built-in
/// runtime that cannot resume or fork (a host profile with `[resume]
/// resumable = false`) still depends on the runtime source: this binary ships
/// no built-in `runtime`, so without the official package every interaction
/// with the adopted session is refused. The preflight reports that loss
/// instead of `adoptable`. With the package installed the session is adopted
/// and has no resume or fork to lose.
async fn assert_non_resumable_built_in_session_is_at_risk_only_without_the_package(runtime: &str) {
    let recoveries = built_in_pins(runtime)
        .into_iter()
        .map(|(pin, binding)| (format!("recovery record kept, {pin}"), Some(binding)))
        .chain(std::iter::once(("no recovery record".to_owned(), None)));
    for (recovery, kept) in recoveries {
        for preinstalled in [false, true] {
            let case = format!("{runtime}, package installed: {preinstalled}, {recovery}");
            let host = Host::new();
            if preinstalled {
                official_runtimes::OfficialCatalog::new()
                    .preinstall(&host.inputs().plugins_dir, host.env.root())
                    .await;
            }
            let mut line = built_in_session_line(runtime);
            line["info"]["capabilities"] = json!({ "resume": false, "fork": false });
            if let Some(binding) = &kept {
                // The binding of a profile that resumes and forks nothing.
                line["recovery"]["resumable"] = json!(false);
                line["recovery"]["forkable"] = json!(false);
                if let Some(binding) = binding {
                    pin_recovery(&mut line, binding.clone());
                }
            } else {
                line["recovery"] = Value::Null;
            }
            host.write_store(&[line]);
            host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));

            let report = host.report();

            if preinstalled {
                assert_eq!(
                    verdict_of(&report, "s-1"),
                    (&Verdict::Adoptable, CODE_ADOPTABLE),
                    "{case}: {:?}",
                    report.sessions
                );
                continue;
            }
            assert_eq!(
                verdict_of(&report, "s-1"),
                (&Verdict::WouldLoseRecovery, CODE_RUNTIME_SOURCE_NOT_SERVED),
                "{case}: {:?}",
                report.sessions
            );
            let detail = &report
                .sessions
                .iter()
                .find(|verdict| verdict.session_id == "s-1")
                .expect("the verdict of s-1")
                .detail;
            assert!(
                detail.contains("runtime_builtin_removed"),
                "{case}: {detail}"
            );
            assert!(detail.contains("every interaction"), "{case}: {detail}");
            assert!(
                detail.contains(&release_install_step(runtime)),
                "{case}: {detail}"
            );
            // The install restores interaction; resume and fork follow only
            // when the package honors the frozen launch.
            assert!(
                detail.contains("resume and fork run through it when it honors the launch"),
                "{case}: {detail}"
            );
        }
    }
}

/// A live built-in Codex session without native recovery is at risk only
/// while no package serves `codex`.
#[tokio::test]
async fn a_built_in_codex_session_without_native_recovery_is_at_risk_only_without_the_package() {
    assert_non_resumable_built_in_session_is_at_risk_only_without_the_package("codex").await;
}

/// A live built-in Claude session without native recovery is at risk only
/// while no package serves `claude`.
#[tokio::test]
async fn a_built_in_claude_session_without_native_recovery_is_at_risk_only_without_the_package() {
    assert_non_resumable_built_in_session_is_at_risk_only_without_the_package("claude").await;
}

/// A live built-in Hermes session without native recovery is at risk only
/// while no package serves `hermes`.
#[tokio::test]
async fn a_built_in_hermes_session_without_native_recovery_is_at_risk_only_without_the_package() {
    assert_non_resumable_built_in_session_is_at_risk_only_without_the_package("hermes").await;
}

/// The binding of a session the v0.34 daemon launched from the built-in
/// `runtime`, as it pins the session.
fn built_in_binding(runtime: &str) -> Value {
    built_in_pins(runtime)
        .into_iter()
        .find_map(|(pin, binding)| (pin == "pinned to the built-in").then_some(binding))
        .flatten()
        .expect("the built-in pin")
}

/// A session of `runtime` the v0.34 daemon launched from its built-in runtime,
/// as the record of `session` on `worker`.
fn built_in_line(runtime: &str, session: &str, worker: &str) -> Value {
    let mut line = session_line(session, worker);
    rebind_agent(&mut line, runtime);
    if runtime != "claude" {
        line["recovery"]["forkable"] = json!(false);
    }
    pin_recovery(&mut line, built_in_binding(runtime));
    line
}

/// Writes one host profile over `base` named `name`, as the owner keeps it.
fn write_profile(dir: &Path, name: &str, base: &str) {
    let path = dir.join(format!("{name}.toml"));
    fs::write(&path, format!("base = \"{base}\"\n")).expect("write the profile");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("secure the profile");
}

/// A `0700` directory, created with its parents.
fn private_dir(path: PathBuf) -> PathBuf {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&path)
        .expect("create private directory");
    path
}

/// The preflight options of a release whose `runtime/` directory is `release`,
/// judged over the host profiles in `profiles` under the host's environment.
fn release_options(
    host: &Host,
    release: &Path,
    catalog: &official_runtimes::OfficialCatalog,
    profiles: &Path,
) -> PreflightOptions {
    PreflightOptions {
        release: Some(ReleaseInputs {
            release_dir: release.to_path_buf(),
            catalog_trust: catalog.trust(),
        }),
        profiles_dir: Some(profiles.to_path_buf()),
        host_state_dir: None,
        probe_budget: PROBE_BUDGET,
        entry: std::time::Instant::now(),
        launch: Ok(host.launch()),
    }
}

#[test]
fn a_0_34_2_host_with_built_in_sessions_is_adoptable_through_the_release_plan() {
    let host = Host::new();
    let catalog = official_runtimes::OfficialCatalog::new();
    let release = host.env.root().join("release").join("runtime");
    catalog.write_release(&release, &["claude", "codex"]);
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));
    write_profile(&profiles, "official-claude", "claude");
    write_profile(&profiles, "official-codex", "codex");
    let mut stopped = built_in_line("claude", "s-3", "w-three");
    stopped["runtime"]["state"] = json!("lost");
    host.write_store(&[
        built_in_line("claude", "s-1", "w-one"),
        built_in_line("codex", "s-2", "w-two"),
        stopped,
    ]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));
    host.write_journal("s-2", "w-two", &journal("s-2", "w-two"));
    let before = host.snapshot();

    let report = run_with(
        &host.inputs(),
        &release_options(&host, &release, &catalog, &profiles),
        &HostInspector::new(),
    );

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE),
        "{:?}",
        report.sessions
    );
    assert_eq!(
        verdict_of(&report, "s-2"),
        (&Verdict::Adoptable, CODE_ADOPTABLE),
        "{:?}",
        report.sessions
    );
    let stopped = report
        .stopped_sessions
        .iter()
        .find(|session| session.session_id == "s-3")
        .expect("the stopped session is judged");
    assert_eq!(stopped.verdict, Verdict::Adoptable, "{stopped:?}");
    assert_eq!(report.at_risk().count(), 0, "{report:?}");
    let plan = report.runtimes.as_ref().expect("the release plan");
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let packages: Vec<(&str, PackageStatus)> = plan
        .packages
        .iter()
        .map(|package| (package.runtime.as_str(), package.status))
        .collect();
    assert_eq!(
        packages,
        [
            ("claude", PackageStatus::Install),
            ("codex", PackageStatus::Install)
        ]
    );
    let bound: Vec<(&str, ProfileStatus)> = plan
        .profiles
        .iter()
        .map(|profile| (profile.name.as_str(), profile.status))
        .collect();
    assert_eq!(
        bound,
        [
            ("official-claude", ProfileStatus::Bind),
            ("official-codex", ProfileStatus::Bind)
        ]
    );
    assert_eq!(host.snapshot(), before, "the preflight changes nothing");
    assert!(
        !host
            .env
            .state_home()
            .join("pohunek")
            .join("plugins")
            .exists(),
        "no package store appears"
    );
}

#[tokio::test]
async fn a_release_without_a_runtime_directory_keeps_the_installed_verdicts_and_no_plan() {
    let host = Host::new();
    let catalog = official_runtimes::OfficialCatalog::new();
    let absent = host.env.root().join("release").join("runtime");
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));
    host.write_store(&[built_in_line("claude", "s-1", "w-one")]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));

    for options in [
        PreflightOptions::default(),
        release_options(&host, &absent, &catalog, &profiles),
    ] {
        let report = run_with(&host.inputs(), &options, &HostInspector::new());

        assert!(report.runtimes.is_none(), "{:?}", report.runtimes);
        assert_eq!(
            verdict_of(&report, "s-1"),
            (&Verdict::WouldLoseRecovery, CODE_RUNTIME_SOURCE_NOT_SERVED),
            "{:?}",
            report.sessions
        );
    }
}

#[test]
fn a_release_plan_refusal_is_reported_and_leaves_the_verdicts_pessimistic() {
    let host = Host::new();
    let catalog = official_runtimes::OfficialCatalog::new();
    let release = host.env.root().join("release").join("runtime");
    // The release ships codex only, so the claude profile has no package.
    catalog.write_release(&release, &["codex"]);
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));
    write_profile(&profiles, "official-claude", "claude");
    host.write_store(&[built_in_line("claude", "s-1", "w-one")]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));

    let report = run_with(
        &host.inputs(),
        &release_options(&host, &release, &catalog, &profiles),
        &HostInspector::new(),
    );

    let refusals: Vec<(&str, &str)> = report
        .plan_refusals()
        .map(|refusal| (refusal.subject.as_str(), refusal.code.as_str()))
        .collect();
    assert_eq!(
        refusals,
        [("claude", CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM)]
    );
    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::WouldLoseRecovery, CODE_RUNTIME_SOURCE_NOT_SERVED),
        "{:?}",
        report.sessions
    );
}

#[test]
fn a_release_plan_that_cannot_be_computed_is_a_refusal_not_an_empty_plan() {
    let host = Host::new();
    let catalog = official_runtimes::OfficialCatalog::new();
    let release = host.env.root().join("release").join("runtime");
    catalog.write_release(&release, &["claude"]);
    let options = PreflightOptions {
        release: Some(ReleaseInputs {
            release_dir: release,
            catalog_trust: catalog.trust(),
        }),
        // Without the profile directory the plan cannot see what it pins.
        profiles_dir: None,
        host_state_dir: None,
        probe_budget: PROBE_BUDGET,
        entry: std::time::Instant::now(),
        launch: Ok(host.launch()),
    };

    let report = run_with(&host.inputs(), &options, &HostInspector::new());

    let codes: Vec<&str> = report
        .plan_refusals()
        .map(|refusal| refusal.code.as_str())
        .collect();
    assert_eq!(codes, [CODE_RUNTIME_PLAN_UNAVAILABLE]);
    let plan = report
        .runtimes
        .as_ref()
        .expect("the refused plan is reported");
    assert!(plan.packages.is_empty() && plan.profiles.is_empty());
}

#[tokio::test]
async fn a_probe_the_agent_fails_or_cannot_run_loses_the_recovery_resume_would_refuse() {
    let host = Host::serving().await;
    let stale = host.agent_path().join("hermes-stale");
    fs::write(
        &stale,
        "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' 'Hermes Agent v0.0.1 (2026.8.3)'; exit 0; fi\nexit 1\n",
    )
    .expect("write the stale agent");
    fs::set_permissions(&stale, fs::Permissions::from_mode(0o700)).expect("make it executable");
    let absent = host.agent_path().join("hermes-absent");
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    let cases = [
        (stale, CODE_AGENT_RUNTIME_UNSUPPORTED),
        (absent, CODE_AGENT_BINARY_MISSING),
    ];
    for (program, code) in cases {
        let mut line = served_session_line("s-1", "w-fixture");
        line["recovery"]["program"] = json!(program.to_string_lossy());
        host.write_store(&[line]);

        let report = host.report();

        assert_eq!(
            verdict_of(&report, "s-1"),
            (&Verdict::WouldLoseRecovery, code),
            "{}: {:?}",
            program.display(),
            report.sessions
        );
    }
}

#[test]
fn a_release_plan_over_a_store_the_daemon_refuses_is_a_plan_refusal() {
    let host = Host::new();
    let catalog = official_runtimes::OfficialCatalog::new();
    let release = host.env.root().join("release").join("runtime");
    catalog.write_release(&release, &["claude"]);
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));
    write_profile(&profiles, "official-claude", "claude");
    let mut newer = built_in_line("claude", "s-1", "w-one");
    newer["schema_version"] = json!(STORE_SCHEMA_VERSION + 1);
    host.write_store(&[newer]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));
    let before = host.snapshot();

    let report = run_with(
        &host.inputs(),
        &release_options(&host, &release, &catalog, &profiles),
        &HostInspector::new(),
    );

    let codes: Vec<&str> = report
        .plan_refusals()
        .map(|refusal| refusal.code.as_str())
        .collect();
    assert_eq!(codes, [CODE_RUNTIME_PLAN_UNAVAILABLE]);
    assert!(report.store_refused());
    assert_eq!(host.snapshot(), before, "the refused plan writes nothing");
}

#[test]
fn a_planned_package_is_served_while_the_store_holds_other_packages() {
    let host = Host::new();
    let catalog = official_runtimes::OfficialCatalog::new();
    let release = host.env.root().join("release").join("runtime");
    catalog.write_release(&release, &["claude"]);
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));
    write_profile(&profiles, "official-claude", "claude");
    // The plugin root exists but holds no package, so the planned archive is
    // not in the store when the preflight judges the built-in session.
    let plugins = host.env.state_home().join("pohunek").join("plugins");
    private_dir(host.env.state_home().join("pohunek").join("plugins"));
    PackageStore::open(&plugins).expect("the plugin root opens");
    host.write_store(&[built_in_line("claude", "s-1", "w-one")]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));

    let report = run_with(
        &host.inputs(),
        &release_options(&host, &release, &catalog, &profiles),
        &HostInspector::new(),
    );

    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE),
        "{:?}",
        report.sessions
    );
    let plan = report.runtimes.as_ref().expect("the release plan");
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(plan.packages[0].status, PackageStatus::Install);
}

/// The fake agent of `runtime` that answers its version probe only through a
/// helper named `<runtime>-banner`, which the agent finds on `PATH`.
fn write_path_dependent_agent(host: &Host, runtime: &str) -> PathBuf {
    let banner = host.agent_path().join(format!("{runtime}-banner"));
    fs::write(
        &banner,
        format!(
            "#!/bin/sh\n{}exit 1\n",
            official_runtimes::sh_version_answer(runtime)
        ),
    )
    .expect("write the banner helper");
    fs::set_permissions(&banner, fs::Permissions::from_mode(0o700)).expect("make it executable");
    let agent = host.agent_path().join(format!("{runtime}-pathed"));
    fs::write(&agent, format!("#!/bin/sh\n{runtime}-banner \"$@\"\n"))
        .expect("write the path-dependent agent");
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).expect("make it executable");
    agent
}

#[tokio::test]
async fn the_service_file_allowlist_decides_the_path_the_version_probe_runs_under() {
    let host = Host::serving().await;
    let agent = write_path_dependent_agent(&host, SERVED_RUNTIME);
    let mut line = served_session_line("s-1", "w-fixture");
    line["recovery"]["program"] = json!(agent.to_string_lossy());
    host.write_store(&[line]);
    host.write_journal("s-1", "w-fixture", &journal("s-1", "w-fixture"));
    let source = host.source();
    let judge = |allowlist: &[String]| {
        let service = host.write_service(allowlist);
        let launch = LaunchEnvironment::from_service_file(&service, source.clone())
            .expect("the service file reads");
        run_with(
            &host.inputs(),
            &PreflightOptions {
                release: None,
                profiles_dir: None,
                host_state_dir: None,
                probe_budget: PROBE_BUDGET,
                entry: std::time::Instant::now(),
                launch: Ok(launch),
            },
            &HostInspector::new(),
        )
    };
    let with_path = default_allowlist();
    let without_path: Vec<String> = with_path
        .iter()
        .filter(|name| name.as_str() != "PATH")
        .cloned()
        .collect();

    let allowed = judge(&with_path);
    assert_eq!(
        verdict_of(&allowed, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE),
        "{:?}",
        allowed.sessions
    );

    let refused = judge(&without_path);
    assert_eq!(
        verdict_of(&refused, "s-1"),
        (&Verdict::WouldLoseRecovery, CODE_AGENT_RUNTIME_UNSUPPORTED),
        "{:?}",
        refused.sessions
    );
}

#[test]
fn an_unreadable_service_file_refuses_the_plan() {
    let host = Host::new();
    host.write_store(&[built_in_line("claude", "s-1", "w-one")]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));
    let missing = host.env.root().join("missing").join("service.toml");
    let launch = LaunchEnvironment::from_service_file(&missing, host.source());
    assert!(launch.is_err(), "a missing service file is not read");

    let report = run_with(
        &host.inputs(),
        &PreflightOptions {
            release: None,
            profiles_dir: None,
            host_state_dir: None,
            probe_budget: PROBE_BUDGET,
            entry: std::time::Instant::now(),
            launch: launch.map_err(|_detail| "the service file is unreadable".to_owned()),
        },
        &HostInspector::new(),
    );

    let codes: Vec<&str> = report
        .plan_refusals()
        .map(|refusal| refusal.code.as_str())
        .collect();
    assert_eq!(codes, [CODE_SERVICE_CONFIG_UNREADABLE]);
    assert_eq!(report.at_risk().count(), 1, "{:?}", report.sessions);
}

/// The archive digest of the official package of `runtime`, as the binding a
/// session launched from that package records.
fn official_digest(runtime: &str) -> package::PackageDigest {
    match official_runtimes::package_binding(runtime).provenance {
        protocol::BindingProvenance::Package { package_digest, .. } => package_digest,
        protocol::BindingProvenance::Builtin { .. } => {
            panic!("the official {runtime} package has package provenance")
        }
    }
}

/// The directory of the extracted tree of the installed package `digest`.
fn installed_root(host: &Host, digest: &package::PackageDigest) -> PathBuf {
    package::registry::files_path(&host.inputs().plugins_dir, digest)
}

#[tokio::test]
async fn a_built_in_session_is_judged_through_the_installed_package_when_no_release_catalog_is_usable(
) {
    let host = Host::serving().await;
    host.write_store(&[built_in_line("claude", "s-1", "w-one")]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));
    // A release `runtime/` with no catalog: the installed package keeps serving.
    let release = host.env.root().join("release").join("runtime");
    fs::create_dir_all(&release).expect("the release directory");
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));
    let catalog = official_runtimes::OfficialCatalog::new();
    let before = host.snapshot();

    let report = run_with(
        &host.inputs(),
        &release_options(&host, &release, &catalog, &profiles),
        &HostInspector::new(),
    );

    let plan = report.runtimes.as_ref().expect("the release plan");
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let kept: Vec<(&str, PackageStatus)> = plan
        .packages
        .iter()
        .map(|package| (package.runtime.as_str(), package.status))
        .collect();
    assert_eq!(kept, [("claude", PackageStatus::Kept)]);
    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE),
        "{:?}",
        report.sessions
    );
    assert_eq!(host.snapshot(), before, "the preflight changes nothing");
}

#[tokio::test]
async fn a_missing_package_root_is_restored_by_the_plan_and_the_session_is_judged_through_the_archive(
) {
    let host = Host::serving().await;
    host.write_store(&[built_in_line("claude", "s-1", "w-one")]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));
    let digest = official_digest("claude");
    // The installed claude root is gone; the registry then no longer serves it.
    let root = installed_root(&host, &digest);
    fs::remove_dir_all(root.parent().expect("the package directory")).expect("remove the root");
    let release = host.env.root().join("release").join("runtime");
    let catalog = official_runtimes::OfficialCatalog::new();
    catalog.write_release(&release, &["claude"]);
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));

    let report = run_with(
        &host.inputs(),
        &release_options(&host, &release, &catalog, &profiles),
        &HostInspector::new(),
    );

    let plan = report.runtimes.as_ref().expect("the release plan");
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(
        plan.packages
            .iter()
            .map(|package| (package.runtime.as_str(), package.status))
            .collect::<Vec<_>>(),
        [("claude", PackageStatus::Install)]
    );
    assert_eq!(
        verdict_of(&report, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE),
        "the restored archive serves the session: {:?}",
        report.sessions
    );
}

#[tokio::test]
async fn an_altered_package_root_is_a_plan_refusal_and_the_session_is_not_adoptable() {
    let host = Host::serving().await;
    host.write_store(&[built_in_line("claude", "s-1", "w-one")]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));
    let digest = official_digest("claude");
    let descriptor =
        installed_root(&host, &digest).join(pohunek_daemon::agent::host::RUNTIME_DESCRIPTOR_PATH);
    let mut text = fs::read_to_string(&descriptor).expect("read the installed descriptor");
    text.push_str("\n# altered after the install\n");
    fs::write(&descriptor, text).expect("alter the installed descriptor");
    let release = host.env.root().join("release").join("runtime");
    let catalog = official_runtimes::OfficialCatalog::new();
    catalog.write_release(&release, &["claude"]);
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));
    let before = host.snapshot();

    let report = run_with(
        &host.inputs(),
        &release_options(&host, &release, &catalog, &profiles),
        &HostInspector::new(),
    );

    let refusals: Vec<&str> = report
        .plan_refusals()
        .filter(|refusal| refusal.subject == "claude")
        .map(|refusal| refusal.code.as_str())
        .collect();
    assert_eq!(refusals.len(), 1, "{:?}", report.runtimes);
    assert_ne!(
        verdict_of(&report, "s-1").0,
        &Verdict::Adoptable,
        "{:?}",
        report.sessions
    );
    assert_eq!(host.snapshot(), before, "the preflight changes nothing");
}

#[test]
fn an_unreadable_package_store_is_a_plan_refusal_not_a_built_in_only_judgement() {
    let host = Host::new();
    host.write_store(&[built_in_line("claude", "s-1", "w-one")]);
    host.write_journal("s-1", "w-one", &journal("s-1", "w-one"));
    // An existing plugin root that is not owner-only cannot be opened.
    let plugins = host.inputs().plugins_dir;
    fs::DirBuilder::new()
        .recursive(true)
        .create(&plugins)
        .expect("the plugin root");
    fs::set_permissions(&plugins, fs::Permissions::from_mode(0o755)).expect("widen the root");

    let report = host.report();

    let codes: Vec<(&str, &str)> = report
        .plan_refusals()
        .map(|refusal| (refusal.subject.as_str(), refusal.code.as_str()))
        .collect();
    assert_eq!(
        codes,
        [("runtime-plan", CODE_RUNTIME_PLAN_UNAVAILABLE)],
        "{:?}",
        report.runtimes
    );
    assert_ne!(verdict_of(&report, "s-1").0, &Verdict::Adoptable);
    assert_eq!(report.at_risk().count(), 1);
}

#[test]
fn a_stopped_record_the_loader_rejects_is_at_risk_and_not_dropped() {
    let host = Host::new();
    let mut stopped = built_in_line("claude", "s-1", "w-one");
    stopped["runtime"]["state"] = json!("lost");
    host.write_store(&[stopped]);
    // Readable session id and a terminal runtime, but an `info` the loader
    // cannot parse, so the store skips the record at startup.
    let mut body = fs::read_to_string(&host.store).expect("read the store");
    let record = json!({
            "kind": "session",
            "schema_version": STORE_SCHEMA_VERSION,
            "session_id": "s-2",
            "runtime": {"state": "terminal"},
            "info": "not an object",
    });
    body.push_str(&record.to_string());
    body.push('\n');
    fs::write(&host.store, body).expect("append the rejected record");

    let report = host.report();

    let rejected = report
        .stopped_sessions
        .iter()
        .find(|session| session.session_id == "s-2")
        .unwrap_or_else(|| {
            panic!(
                "the rejected record is reported: {:?}",
                report.stopped_sessions
            )
        });
    assert_eq!(rejected.verdict, Verdict::WouldLoseRecovery, "{rejected:?}");
    assert_eq!(rejected.code, CODE_RECORD_UNREADABLE, "{rejected:?}");
    assert!(report.at_risk().any(|session| session.session_id == "s-2"));
}

#[test]
fn an_interrupted_removal_keeps_no_recovery_the_upgrade_must_preserve() {
    let host = Host::new();
    // A stopped session the owner removed: reconciliation finishes the removal.
    let mut removal = session_line("s-1", "w-one");
    removal["runtime"]["state"] = json!("lost");
    removal["desired_state"] = json!("removed");
    // A stopped session the owner only stopped: still a recovery to keep.
    let mut stop = session_line("s-3", "w-three");
    stop["runtime"]["state"] = json!("lost");
    stop["desired_state"] = json!("stopped");
    host.write_store(&[removal, stop]);
    // A removed record the loader skips: it is not a recovery either.
    let mut body = fs::read_to_string(&host.store).expect("read the store");
    body.push_str(
        &json!({
            "kind": "session",
            "schema_version": STORE_SCHEMA_VERSION,
            "session_id": "s-2",
            "desired_state": "removed",
            "runtime": {"state": "terminal"},
            "info": "not an object",
        })
        .to_string(),
    );
    body.push('\n');
    fs::write(&host.store, body).expect("append the skipped removal");

    let report = host.report();

    let stopped: Vec<&str> = report
        .stopped_sessions
        .iter()
        .map(|session| session.session_id.as_str())
        .collect();
    assert_eq!(stopped, ["s-3"], "{:?}", report.stopped_sessions);
    assert_eq!(report.at_risk().count(), 1, "{:?}", report.stopped_sessions);
}

#[test]
fn a_malformed_resume_line_with_a_removal_field_is_not_a_removal_intent() {
    let host = Host::new();
    // A stopped session whose only recovery is its resume line, which the
    // loader skips. A resume line never carries a removal intent.
    let mut stopped = session_line("s-2", "w-two");
    stopped["runtime"]["state"] = json!("lost");
    stopped["recovery"] = Value::Null;
    host.write_store(&[stopped]);
    let mut body = fs::read_to_string(&host.store).expect("read the store");
    body.push_str(
        &json!({
            "kind": "resume",
            "session_id": "s-2",
            "desired_state": "removed",
        })
        .to_string(),
    );
    body.push('\n');
    fs::write(&host.store, body).expect("append the malformed resume line");

    let report = host.report();

    let skipped = report
        .stopped_sessions
        .iter()
        .find(|session| session.session_id == "s-2")
        .unwrap_or_else(|| {
            panic!(
                "the skipped resume line is reported: {:?}",
                report.stopped_sessions
            )
        });
    assert_eq!(skipped.verdict, Verdict::WouldLoseRecovery, "{skipped:?}");
    assert_eq!(skipped.code, CODE_RECORD_UNREADABLE, "{skipped:?}");
    assert!(report.at_risk().any(|session| session.session_id == "s-2"));
}

/// A stopped session of the served runtime, pinned to its official package,
/// whose recovery launches `program`.
fn stopped_served_line(session: &str, worker: &str, program: &Path) -> Value {
    let mut line = served_session_line(session, worker);
    line["runtime"]["state"] = json!("lost");
    line["recovery"]["program"] = json!(program.to_string_lossy());
    line
}

/// The verdict of the stopped session `session` in `report`.
fn stopped_verdict_of<'a>(report: &'a PreflightReport, session: &str) -> (&'a Verdict, &'a str) {
    let found = report
        .stopped_sessions
        .iter()
        .find(|candidate| candidate.session_id == session)
        .unwrap_or_else(|| {
            panic!(
                "no stopped verdict for {session}: {:?}",
                report.stopped_sessions
            )
        });
    (&found.verdict, found.code.as_str())
}

/// An agent that never answers its version probe, written as `bin/<name>`.
fn hanging_agent(host: &Host, name: &str) -> PathBuf {
    let agent = host.agent_path().join(name);
    fs::write(&agent, "#!/bin/sh\n/bin/sleep 30\n").expect("write the hanging agent");
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).expect("make it executable");
    agent
}

#[tokio::test]
async fn a_package_pinned_session_whose_root_is_missing_is_judged_through_the_restored_archive() {
    let host = Host::serving().await;
    let digest = official_digest(SERVED_RUNTIME);
    fs::remove_dir_all(
        installed_root(&host, &digest)
            .parent()
            .expect("the package directory"),
    )
    .expect("remove the root");
    let release = host.env.root().join("release").join("runtime");
    let catalog = official_runtimes::OfficialCatalog::new();
    catalog.write_release(&release, &[SERVED_RUNTIME]);
    let profiles = private_dir(host.env.config_home().join("pohunek").join("agents"));
    let agent = host.agent_path().join(SERVED_RUNTIME);
    host.write_store(&[stopped_served_line("s-1", "w-one", &agent)]);

    let report = run_with(
        &host.inputs(),
        &release_options(&host, &release, &catalog, &profiles),
        &HostInspector::new(),
    );

    let plan = report.runtimes.as_ref().expect("the release plan");
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    assert_eq!(
        stopped_verdict_of(&report, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE),
        "{:?}",
        report.stopped_sessions
    );
}

#[tokio::test]
async fn the_release_plan_judges_the_upgrade_only_when_the_binary_is_given_its_flag() {
    let host = Host::serving().await;
    // The claude root is gone: only the release plan restores it.
    let digest = official_digest("claude");
    fs::remove_dir_all(
        installed_root(&host, &digest)
            .parent()
            .expect("the package directory"),
    )
    .expect("remove the root");
    let mut line = built_in_line("claude", "s-1", "w-one");
    line["runtime"]["state"] = json!("lost");
    host.write_store(&[line]);
    host.write_service(&default_allowlist());
    // A daemon beside a release `runtime/` directory that ships claude.
    let beside = host.env.root().join("release-bin");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&beside)
        .expect("the release directory");
    let daemon = beside.join("pohunekd");
    fs::copy(bin_exe("pohunekd"), &daemon).expect("copy the daemon beside its release");
    let catalog = official_runtimes::OfficialCatalog::new();
    fs::write(
        beside.join(package::ANCHOR_FILE_NAME),
        catalog.anchor_bytes(),
    )
    .expect("write the trust anchor");
    catalog.write_release(&beside.join(RELEASE_RUNTIME_DIR), &["claude"]);
    let run = |flag: Option<&str>| -> PreflightReport {
        let mut command = host.env.command(&daemon);
        command.arg("upgrade-preflight");
        command.args(flag);
        let output = command.output().expect("run the daemon");
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice(&output.stdout).expect("the report")
    };

    // Without the flag (what a rollback runs) the installed packages decide.
    let installed = run(None);
    assert!(installed.runtimes.is_none(), "{:?}", installed.runtimes);
    assert_ne!(stopped_verdict_of(&installed, "s-1").0, &Verdict::Adoptable);

    let planned = run(Some(RELEASE_PLAN_FLAG));
    assert!(planned.runtimes.is_some());
    assert_eq!(
        stopped_verdict_of(&planned, "s-1"),
        (&Verdict::Adoptable, CODE_ADOPTABLE),
        "{:?}",
        planned.stopped_sessions
    );
}

/// Several sessions whose probes all launch `agents`, one executable each.
fn stopped_sessions_launching(host: &Host, agents: &[PathBuf]) {
    let lines: Vec<Value> = agents
        .iter()
        .enumerate()
        .map(|(index, agent)| {
            stopped_served_line(&format!("s-{index:02}"), &format!("w-{index:02}"), agent)
        })
        .collect();
    host.write_store(&lines);
}

#[tokio::test]
async fn sessions_sharing_a_hanging_executable_are_probed_once_and_share_its_verdict() {
    let host = Host::serving().await;
    let agent = hanging_agent(&host, "hanging-shared");
    stopped_sessions_launching(&host, &[agent.clone(), agent.clone(), agent.clone(), agent]);

    let report = run_with(
        &host.inputs(),
        &PreflightOptions {
            release: None,
            profiles_dir: None,
            host_state_dir: None,
            probe_budget: Duration::from_secs(3),
            entry: std::time::Instant::now(),
            launch: Ok(host.launch()),
        },
        &HostInspector::new(),
    );

    let verdicts: Vec<&str> = report
        .stopped_sessions
        .iter()
        .map(|session| session.code.as_str())
        .collect();
    assert_eq!(
        verdicts, ["agent_runtime_unsupported"; 4],
        "{:?}",
        report.stopped_sessions
    );
    assert_eq!(report.at_risk().count(), 4);
}

#[tokio::test]
async fn a_spent_probe_budget_judges_the_remaining_sessions_pessimistically() {
    let host = Host::serving().await;
    let mut agents: Vec<PathBuf> = (0..4)
        .map(|index| hanging_agent(&host, &format!("hanging-{index}")))
        .collect();
    // Resolution runs no probe: a removed program is reported as resume
    // refuses it, even after the budget is spent.
    agents.push(host.agent_path().join("removed-agent"));
    stopped_sessions_launching(&host, &agents);

    let report = run_with(
        &host.inputs(),
        &PreflightOptions {
            release: None,
            profiles_dir: None,
            host_state_dir: None,
            probe_budget: Duration::from_secs(3),
            entry: std::time::Instant::now(),
            launch: Ok(host.launch()),
        },
        &HostInspector::new(),
    );

    // The first probe runs its full deadline; the budget left cuts the next one
    // short, and nothing starts after it.
    let verdicts: Vec<&str> = report
        .stopped_sessions
        .iter()
        .map(|session| session.code.as_str())
        .collect();
    assert_eq!(
        verdicts,
        [
            "agent_runtime_unsupported",
            CODE_PROBE_BUDGET_EXHAUSTED,
            CODE_PROBE_BUDGET_EXHAUSTED,
            CODE_PROBE_BUDGET_EXHAUSTED,
            "agent_binary_missing"
        ],
        "{:?}",
        report.stopped_sessions
    );
    assert!(report
        .stopped_sessions
        .iter()
        .all(|session| session.verdict == Verdict::WouldLoseRecovery));
}

#[tokio::test]
async fn many_sessions_sharing_a_hanging_executable_complete_within_the_preflight_deadline() {
    let host = Host::serving().await;
    let agent = hanging_agent(&host, "hanging-process");
    stopped_sessions_launching(&host, &vec![agent; 12]);
    host.write_service(&default_allowlist());

    let started = std::time::Instant::now();
    let output = host
        .env
        .command(bin_exe("pohunekd"))
        .arg("upgrade-preflight")
        .output()
        .expect("run pohunekd");
    let elapsed = started.elapsed();

    assert!(output.status.success(), "{output:?}");
    assert!(elapsed < PREFLIGHT_TIMEOUT, "the run took {elapsed:?}");
    let report: PreflightReport =
        serde_json::from_slice(&output.stdout).expect("stdout is the report");
    assert_eq!(report.stopped_sessions.len(), 12);
    assert!(report
        .stopped_sessions
        .iter()
        .all(|session| session.code == "agent_runtime_unsupported"));
}

#[tokio::test]
async fn the_probe_deadline_counts_the_work_before_the_probes_from_the_entry_of_the_command() {
    let host = Host::serving().await;
    let agents: Vec<PathBuf> = (0..10)
        .map(|index| hanging_agent(&host, &format!("slow-{index}")))
        .collect();
    stopped_sessions_launching(&host, &agents);
    // Twelve seconds of the journal scan, the plan and the registry load passed
    // before the first probe, as on a slow host.
    let entry = std::time::Instant::now()
        .checked_sub(Duration::from_secs(12))
        .expect("the monotonic clock is older than the simulated entry");

    let report = run_with(
        &host.inputs(),
        &PreflightOptions {
            release: None,
            profiles_dir: None,
            host_state_dir: None,
            entry,
            probe_budget: PROBE_BUDGET,
            launch: Ok(host.launch()),
        },
        &HostInspector::new(),
    );

    // The judgement ends inside the installer's deadline, and the probes that
    // were left are judged pessimistically rather than run past the budget.
    let elapsed = entry.elapsed();
    assert!(
        elapsed < PREFLIGHT_TIMEOUT,
        "the judgement took {elapsed:?}"
    );
    let codes: Vec<&str> = report
        .stopped_sessions
        .iter()
        .map(|session| session.code.as_str())
        .collect();
    assert_eq!(
        report.at_risk().count(),
        10,
        "{:?}",
        report.stopped_sessions
    );
    assert_eq!(
        codes.last(),
        Some(&CODE_PROBE_BUDGET_EXHAUSTED),
        "{codes:?}"
    );
    assert!(codes.contains(&CODE_PROBE_BUDGET_EXHAUSTED), "{codes:?}");
}
