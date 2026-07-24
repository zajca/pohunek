use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::{
    AgentActivity, AgentKind, CwdSource, Event, ForkCwdMode, ProjectSource, RuntimeState,
    SessionAttachParams, SessionForkParams, SessionId, SessionInfo, SessionNativeRecoveredEvent,
    SessionNewParams, SessionReleaseAgentParams, SessionReportAgentParams,
    SessionReportNativeIdParams, SessionRuntime, SessionState, StateSource,
};

use crate::agent::LaunchCommand;
use crate::agent::{InputRules, ResumeMode, SessionRefKind};
use crate::detect::{ActivityTransition, DetectorConfig, ManifestRegion, MatchContext};
use crate::external::TranscriptIndex;
use crate::integration::{
    ENV_DAEMON_ID, ENV_FLAG, ENV_PROTOCOL_VERSION, ENV_SESSION_ID, ENV_SOCKET_PATH,
};
use crate::procwatch::{ExitWatch, OwnershipMarkers, Pid, ProcessFact, ProcessInspector};
use crate::project::detect::project_id;

use super::{
    RuntimeExit, SessionRegistry, SessionRegistryConfig, ShellCommand, MAX_SESSION_NAME_BYTES,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn params() -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: "shell".to_owned(),
        cwd: Some(PathBuf::from("/tmp")),
        cols: 80,
        rows: 24,
        project: None,
        repo: None,
        branch: None,
        base_branch: None,
        input: None,
        metadata: BTreeMap::new(),
    }
}

#[test]
fn production_registry_rejects_missing_durable_worker_backend() {
    let error = SessionRegistry::new_production(SessionRegistryConfig::default())
        .expect_err("production registry must fail closed without worker runtime root");
    assert_eq!(error.code, "worker_backend_required");

    let configured = SessionRegistryConfig {
        worker_runtime_root: Some(PathBuf::from("/run/user/1000/pohunek/workers")),
        worker_state_root: Some(PathBuf::from("/home/user/.local/state/pohunek/workers")),
        ..SessionRegistryConfig::default()
    };
    SessionRegistry::new_production(configured).expect("configured production registry");
}

#[tokio::test]
async fn exhausted_session_id_sequence_fails_without_wrapping_to_zero() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());
    registry.inner.next_id.store(u64::MAX, Ordering::Relaxed);

    let error = registry
        .create(params())
        .await
        .expect_err("exhausted session-id sequence");

    assert_eq!(error.code, "session_id_exhausted");
    assert_eq!(registry.inner.next_id.load(Ordering::Relaxed), u64::MAX);
    assert!(
        registry
            .inspect(&SessionId("s-0".to_owned()))
            .await
            .is_err(),
        "overflow must never allocate s-0"
    );
}

fn metadata(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn metadata_patch(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.map(str::to_owned)))
        .collect()
}

/// A plain attach (no self-feed origin) for the given session id.
fn attach_params(id: &SessionId) -> SessionAttachParams {
    SessionAttachParams {
        session_id: id.clone(),
        origin_session_id: None,
        origin_daemon_id: None,
        origin_worker_id: None,
    }
}

fn temp_store_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pohunek-session-{tag}-{}-{nanos}-{n}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join("metadata.jsonl")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = temp_store_path(tag)
        .parent()
        .expect("store parent")
        .join("dir");
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_host_hook(config_dir: &std::path::Path, event: &str, body: &str) {
    let hooks = config_dir.join("hooks");
    fs::create_dir_all(&hooks).expect("create hooks dir");
    fs::write(hooks.join(event), body).expect("write hook");
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, body).expect("write executable");
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms).expect("chmod executable");
}

#[cfg(unix)]
fn write_resume_agent_script(path: &std::path::Path, marker: &std::path::Path) {
    write_executable(
        path,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nsleep 30\n",
            marker.display()
        ),
    );
}

#[cfg(unix)]
fn terminate_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
}

async fn wait_for_file_contains(path: &std::path::Path, needle: &str) -> String {
    for _ in 0..500 {
        if let Ok(contents) = fs::read_to_string(path) {
            if contents.contains(needle) {
                return contents;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "timed out waiting for {} to contain {needle:?}",
        path.display()
    );
}

async fn wait_for_line_count(path: &std::path::Path, expected: usize) -> String {
    for _ in 0..500 {
        if let Ok(contents) = fs::read_to_string(path) {
            if contents.lines().count() >= expected {
                return contents;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "timed out waiting for {} to contain at least {expected} lines",
        path.display()
    );
}

fn transition(activity: AgentActivity) -> ActivityTransition {
    ActivityTransition {
        activity,
        source: protocol::StateSource::Process,
    }
}

#[test]
fn external_observer_defaults_off() {
    assert!(
        !SessionRegistryConfig::default().observe_external_agents,
        "external observation watches provider transcript trees and must remain opt-in"
    );
}

#[derive(Debug, Default)]
struct MockInspector {
    inner: Mutex<MockInspectorState>,
}

#[derive(Debug, Default)]
struct MockInspectorState {
    descendants: HashMap<Pid, Vec<ProcessFact>>,
    descendants_error: Option<std::io::ErrorKind>,
    cwd: HashMap<Pid, PathBuf>,
    exits: HashMap<Pid, tokio::sync::watch::Sender<bool>>,
    ownership_markers: HashMap<Pid, OwnershipMarkers>,
}

impl MockInspector {
    fn set_descendants(&self, root: Pid, facts: Vec<ProcessFact>) {
        let mut inner = self.inner.lock().expect("mock inspector lock");
        for fact in &facts {
            inner
                .cwd
                .entry(fact.pid)
                .or_insert_with(|| PathBuf::from("/tmp"));
            inner
                .exits
                .entry(fact.pid)
                .or_insert_with(|| tokio::sync::watch::channel(false).0);
        }
        inner.descendants.insert(root, facts);
    }

    fn fail_descendants_with(&self, kind: std::io::ErrorKind) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .descendants_error = Some(kind);
    }

    fn fire_exit(&self, pid: Pid) {
        let sender = {
            let mut inner = self.inner.lock().expect("mock inspector lock");
            inner
                .exits
                .entry(pid)
                .or_insert_with(|| tokio::sync::watch::channel(false).0)
                .clone()
        };
        sender.send_replace(true);
    }

    fn set_cwd(&self, pid: Pid, cwd: PathBuf) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .cwd
            .insert(pid, cwd);
    }

    fn set_ownership_markers(&self, pid: Pid, markers: OwnershipMarkers) {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .ownership_markers
            .insert(pid, markers);
    }
}

impl ProcessInspector for MockInspector {
    fn same_user_processes(&self) -> std::io::Result<Vec<ProcessFact>> {
        let mut facts = self
            .inner
            .lock()
            .expect("mock inspector lock")
            .descendants
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        facts.sort_by_key(|fact| fact.pid);
        facts.dedup_by_key(|fact| fact.pid);
        Ok(facts)
    }

    fn descendants(&self, root: Pid) -> std::io::Result<Vec<ProcessFact>> {
        let inner = self.inner.lock().expect("mock inspector lock");
        if let Some(kind) = inner.descendants_error {
            return Err(std::io::Error::new(kind, "mock descendants failure"));
        }
        Ok(inner.descendants.get(&root).cloned().unwrap_or_default())
    }

    fn cwd(&self, pid: Pid) -> std::io::Result<PathBuf> {
        self.inner
            .lock()
            .expect("mock inspector lock")
            .cwd
            .get(&pid)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "missing mock cwd"))
    }

    fn exit_watch(&self, pid: Pid) -> std::io::Result<ExitWatch> {
        let receiver = {
            let mut inner = self.inner.lock().expect("mock inspector lock");
            inner
                .exits
                .entry(pid)
                .or_insert_with(|| tokio::sync::watch::channel(false).0)
                .subscribe()
        };
        Ok(ExitWatch::from_test_signal(receiver))
    }

    fn ownership_markers(&self, pid: Pid) -> std::io::Result<OwnershipMarkers> {
        Ok(self
            .inner
            .lock()
            .expect("mock inspector lock")
            .ownership_markers
            .get(&pid)
            .cloned()
            .unwrap_or_default())
    }
}

fn title_activity(config: &DetectorConfig, title: &str) -> Option<AgentActivity> {
    config
        .manifest
        .as_ref()?
        .match_context(&MatchContext::default().with_region_text(ManifestRegion::OscTitle, title))
        .map(|matched| matched.activity)
}

fn pty_command<'a>(program: &str, args: impl IntoIterator<Item = &'a str>) -> LaunchCommand {
    LaunchCommand {
        program: program.to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
        env: Vec::new(),
        cwd: PathBuf::from("/tmp"),
        cols: 80,
        rows: 24,
    }
}

fn parse_env_dump(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn pohunek_env_keys(env: &std::collections::HashMap<String, String>) -> Vec<String> {
    let mut keys: Vec<String> = env
        .keys()
        .filter(|key| key.starts_with("POHUNEK_"))
        .cloned()
        .collect();
    keys.sort();
    keys
}

async fn next_session_updated(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> SessionInfo {
    let event = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx.recv().await.expect("receive session event");
            if event.event == protocol::event::SESSION_UPDATED {
                break event;
            }
        }
    })
    .await
    .expect("session_updated event");
    serde_json::from_value(event.payload["session"].clone()).expect("session info payload")
}

async fn next_session_removed(rx: &mut tokio::sync::broadcast::Receiver<Event>) -> SessionInfo {
    let event = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx.recv().await.expect("receive session event");
            if event.event == protocol::event::SESSION_REMOVED {
                break event;
            }
        }
    })
    .await
    .expect("session_removed event");
    serde_json::from_value(event.payload["session"].clone()).expect("session info payload")
}

async fn wait_for_cwd_source(
    registry: &SessionRegistry,
    id: &SessionId,
    cwd: &std::path::Path,
    source: CwdSource,
) -> SessionInfo {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let info = registry.inspect(id).await.expect("inspect session");
        if info.cwd == cwd && info.cwd_source == Some(source) {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for cwd {} from {source:?}",
            cwd.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Run git in `dir`, asserting success (test helper for the worktree path).
fn git_in(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Initialize a throwaway git repo on `main` with one commit, for the
/// worktree-binding path in `create`.
fn init_git_repo(tag: &str) -> PathBuf {
    let dir = temp_store_path(tag)
        .parent()
        .expect("store parent")
        .join("repo");
    std::fs::create_dir_all(&dir).expect("create repo dir");
    let init = std::process::Command::new("git")
        .args(["-c", "init.defaultBranch=main", "init", "-q"])
        .arg(&dir)
        .output()
        .expect("git init");
    assert!(init.status.success(), "git init failed");
    git_in(&dir, &["config", "user.email", "test@example.com"]);
    git_in(&dir, &["config", "user.name", "Test"]);
    git_in(&dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("README.md"), "init\n").expect("write README");
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-q", "-m", "init"]);
    dir
}

/// Initialize a throwaway **bare** repo (no working tree) carrying a commit and
/// HEAD, for the bare-project paths. A `--bare` clone of a normal repo gives a
/// bare repo that still has a `main` branch, so `git worktree add` off it works.
fn init_bare_git_repo(tag: &str) -> PathBuf {
    let source = init_git_repo(&format!("{tag}-src"));
    let bare = temp_store_path(tag)
        .parent()
        .expect("store parent")
        .join("bare.git");
    let clone = std::process::Command::new("git")
        .args(["clone", "--bare", "-q"])
        .arg(&source)
        .arg(&bare)
        .output()
        .expect("git clone --bare");
    assert!(
        clone.status.success(),
        "git clone --bare failed: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
    bare
}

#[tokio::test]
async fn session_new_metadata_is_validated_and_exposed() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let expected = metadata(&[("owner", "cli"), ("ticket", "DMD-1356")]);

    let created = registry
        .create(SessionNewParams {
            metadata: expected.clone(),
            ..params()
        })
        .await
        .expect("create session with metadata");
    assert_eq!(created.metadata, expected);
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .metadata,
        expected
    );
    let listed = registry.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].metadata, expected);

    let invalid: BTreeMap<String, String> = (0..33)
        .map(|index| (format!("key-{index}"), "value".to_owned()))
        .collect();
    let err = registry
        .create(SessionNewParams {
            metadata: invalid,
            ..params()
        })
        .await
        .expect_err("too many metadata keys must be rejected");
    assert_eq!(err.code, "bad_request");
    assert!(
        err.msg.contains("metadata"),
        "metadata validation error must be clear: {err:?}"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn set_metadata_merges_deletes_updates_timestamp_and_emits_event() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            metadata: metadata(&[("drop", "soon"), ("keep", "yes"), ("ticket", "old")]),
            ..params()
        })
        .await
        .expect("create session");
    let before_updated_at = created.updated_at.clone();
    let mut events = registry.subscribe();
    let expected = metadata(&[("keep", "yes"), ("owner", "daemon"), ("ticket", "new")]);

    let result = registry
        .set_metadata(
            &created.id,
            metadata_patch(&[
                ("drop", None),
                ("owner", Some("daemon")),
                ("ticket", Some("new")),
            ]),
        )
        .await
        .expect("set metadata");

    assert_eq!(result.session.metadata, expected);
    assert_ne!(result.session.updated_at, before_updated_at);
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .metadata,
        expected
    );
    let event_info = next_session_updated(&mut events).await;
    assert_eq!(event_info.id, created.id);
    assert_eq!(event_info.metadata, expected);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn set_metadata_unknown_session_returns_not_found() {
    let registry = SessionRegistry::default();

    let err = registry
        .set_metadata(&SessionId("s-missing".to_owned()), BTreeMap::new())
        .await
        .expect_err("unknown session id must fail");

    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn create_with_name_trims_and_stores_it() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            name: Some("  triage build  ".to_owned()),
            ..params()
        })
        .await
        .expect("create session");

    assert_eq!(created.name.as_deref(), Some("triage build"));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn rename_sets_then_clears_name_updates_timestamp_and_emits_event() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    assert_eq!(created.name, None);
    let before_updated_at = created.updated_at.clone();
    let mut events = registry.subscribe();

    let renamed = registry
        .rename(&created.id, Some("  feature work  ".to_owned()))
        .await
        .expect("rename session");
    assert_eq!(renamed.session.name.as_deref(), Some("feature work"));
    assert_ne!(renamed.session.updated_at, before_updated_at);
    let event_info = next_session_updated(&mut events).await;
    assert_eq!(event_info.id, created.id);
    assert_eq!(event_info.name.as_deref(), Some("feature work"));

    // An all-whitespace (or `None`) name clears it back to id-only display.
    let cleared = registry
        .rename(&created.id, Some("   ".to_owned()))
        .await
        .expect("clear name");
    assert_eq!(cleared.session.name, None);
    assert_eq!(
        registry.inspect(&created.id).await.expect("inspect").name,
        None
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn rename_rejects_overlong_and_control_character_names() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");

    let too_long = registry
        .rename(&created.id, Some("x".repeat(MAX_SESSION_NAME_BYTES + 1)))
        .await
        .expect_err("overlong name must fail");
    assert_eq!(too_long.code, "bad_request");

    let control = registry
        .rename(&created.id, Some("line1\nline2".to_owned()))
        .await
        .expect_err("control character must fail");
    assert_eq!(control.code, "bad_request");

    // A rejected rename leaves the prior name untouched.
    assert_eq!(
        registry.inspect(&created.id).await.expect("inspect").name,
        None
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn rename_unknown_session_returns_not_found() {
    let registry = SessionRegistry::default();

    let err = registry
        .rename(&SessionId("s-missing".to_owned()), Some("x".to_owned()))
        .await
        .expect_err("unknown session id must fail");

    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn invalid_metadata_rejected_for_create_or_set_and_set_leaves_session_unchanged() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let mut invalid_create = BTreeMap::new();
    invalid_create.insert("owner".to_owned(), "x".repeat(4097));
    let err = registry
        .create(SessionNewParams {
            metadata: invalid_create,
            ..params()
        })
        .await
        .expect_err("oversized metadata value must be rejected");
    assert_eq!(err.code, "bad_request");
    assert!(registry.list().await.is_empty());

    let created = registry
        .create(SessionNewParams {
            metadata: metadata(&[("owner", "cli")]),
            ..params()
        })
        .await
        .expect("create valid session");
    let original = created.metadata.clone();
    let original_updated_at = created.updated_at.clone();
    let err = registry
        .set_metadata(
            &created.id,
            BTreeMap::from([("x".repeat(65), Some("bad".to_owned()))]),
        )
        .await
        .expect_err("oversized metadata key must be rejected");
    assert_eq!(err.code, "bad_request");

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.metadata, original);
    assert_eq!(
        inspected.updated_at, original_updated_at,
        "failed metadata patch must not mutate the session"
    );

    let _ = registry.stop(&created.id).await;

    let key_64_bytes = "é".repeat(32);
    assert_eq!(key_64_bytes.len(), 64);
    let accepted = registry
        .create(SessionNewParams {
            metadata: BTreeMap::from([(key_64_bytes, "byte-boundary".to_owned())]),
            ..params()
        })
        .await
        .expect("64-byte UTF-8 metadata key is accepted");
    let _ = registry.stop(&accepted.id).await;

    let key_66_bytes = "é".repeat(33);
    assert_eq!(key_66_bytes.len(), 66);
    let err = registry
        .create(SessionNewParams {
            metadata: BTreeMap::from([(key_66_bytes, "too-long".to_owned())]),
            ..params()
        })
        .await
        .expect_err("metadata key limit is measured in bytes");
    assert_eq!(err.code, "bad_request");

    let serialized_too_large: BTreeMap<String, String> = (0..super::MAX_SESSION_METADATA_KEYS)
        .map(|index| (format!("key-{index:02}"), "x".repeat(512)))
        .collect();
    assert!(
        serde_json::to_vec(&serialized_too_large)
            .expect("metadata serializes")
            .len()
            > super::MAX_SESSION_METADATA_SERIALIZED_BYTES
    );
    let err = registry
        .create(SessionNewParams {
            metadata: serialized_too_large,
            ..params()
        })
        .await
        .expect_err("metadata serialized size limit must be enforced");
    assert_eq!(err.code, "bad_request");
    assert!(
        err.msg.contains("serialized size"),
        "serialized-size rejection should be clear: {err:?}"
    );
}

#[tokio::test]
async fn failed_launch_rolls_back_the_bound_worktree() {
    // Worktree binding persists the branch checkout before the PTY is spawned.
    // A spawn failure (here: a missing shell program) must roll that back, or
    // the orphan worktree keeps the branch checked out and blocks the next
    // `session.new` on it with `worktree_branch_in_use`.
    let repo = init_git_repo("rollback");
    let store = temp_store_path("rollback");
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/nonexistent/pohunek-no-such-shell",
            std::iter::empty::<String>(),
        ),
        store_path: Some(store),
        worktree_root: Some(worktree_root.clone()),
        ..SessionRegistryConfig::default()
    });

    let create_params = SessionNewParams {
        cwd: None,
        name: None,
        repo: Some(repo.clone()),
        branch: Some("feat/x".to_owned()),
        ..params()
    };
    let err = registry
        .create(create_params)
        .await
        .expect_err("launch must fail with a missing shell program");
    // A missing program (ENOENT) at spawn surfaces the precise
    // `agent_binary_missing` diagnostic naming the program, with a recover
    // hint — not the generic `spawn_failed`.
    assert_eq!(err.code, "agent_binary_missing", "got: {err:?}");
    assert!(
        err.msg.contains("pohunek-no-such-shell"),
        "error must name the missing program: {err:?}"
    );
    assert!(
        err.recover.is_some(),
        "missing-binary error carries a hint: {err:?}"
    );

    // The worktree bound before the failed spawn must be gone, so its branch
    // is freed for a retry.
    let leftover: Vec<_> = std::fs::read_dir(&worktree_root)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "a failed launch must leave no orphan worktree under {}: {leftover:?}",
        worktree_root.display()
    );

    // And git no longer holds feat/x in any worktree, so a fresh bind succeeds.
    let listing = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list");
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        !listing.contains("feat/x"),
        "branch checkout must be pruned from git's worktree list: {listing}"
    );
}

#[tokio::test]
async fn failed_initial_input_rollback_frees_the_bound_worktree() {
    // A worktree-bound session whose `--input` injection fails must roll the
    // worktree back, not just the PTY: `stop()` alone leaves the checkout in
    // place, blocking the next `session.new` on the branch with
    // `worktree_branch_in_use`. Drive the exact rollback the failed-input
    // branch of `create` performs and assert the branch is freed.
    let repo = init_git_repo("input-rollback");
    let store = temp_store_path("input-rollback");
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        store_path: Some(store),
        worktree_root: Some(worktree_root.clone()),
        ..SessionRegistryConfig::default()
    });

    // A real worktree-bound session (launch succeeds, branch checked out).
    let info = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            repo: Some(repo.clone()),
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created");
    assert!(
        info.worktree_path.is_some(),
        "session must be worktree-bound for this test: {info:?}"
    );

    registry.rollback_failed_initial_input(&info.id, true).await;

    // The worktree bound for this session must be gone so its branch is free.
    let leftover: Vec<_> = std::fs::read_dir(&worktree_root)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(
        leftover.is_empty(),
        "rollback must leave no orphan worktree under {}: {leftover:?}",
        worktree_root.display()
    );

    // git no longer holds feat/x in any worktree, so a fresh bind succeeds.
    let listing = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list");
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(
        !listing.contains("feat/x"),
        "branch checkout must be pruned so a fresh bind succeeds: {listing}"
    );
}

/// A registry with persistence + worktree binding configured, using the
/// default shell so a launch actually succeeds (for the project-wiring tests).
fn project_registry(tag: &str) -> (SessionRegistry, PathBuf) {
    let store = temp_store_path(tag);
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        store_path: Some(store),
        worktree_root: Some(worktree_root),
        ..SessionRegistryConfig::default()
    });
    let repo = init_git_repo(tag);
    (registry, repo)
}

#[tokio::test]
async fn session_new_auto_registers_project_from_cwd_and_stamps_ids() {
    // The first observable change (M3): starting a session inside a git work
    // tree with no flags runs in-place and silently records an Auto project,
    // stamping the session's project_id / is_linked_worktree.
    let (registry, repo) = project_registry("auto-register");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            ..params()
        })
        .await
        .expect("session is created in the repo");

    let canonical_repo = std::fs::canonicalize(&repo).expect("canonical repo");
    assert_eq!(info.cwd, canonical_repo, "in-place runs in the checkout");
    assert_eq!(info.worktree_path, None, "in-place binds no worktree");
    assert_eq!(info.is_linked_worktree, Some(false), "the main checkout");
    let project_id = info.project_id.clone().expect("a project was stamped");

    let projects = registry
        .projects()
        .expect("projects configured")
        .store()
        .load_projects()
        .expect("load projects");
    assert_eq!(projects.len(), 1, "exactly one project auto-registered");
    assert_eq!(projects[0].source, ProjectSource::Auto);
    assert_eq!(
        projects[0].id(),
        project_id,
        "session id matches the record"
    );
    assert_eq!(
        projects[0].git_common_dir,
        std::fs::canonicalize(repo.join(".git")).expect("canonical .git")
    );
}

#[tokio::test]
async fn in_place_session_on_a_bare_project_is_refused() {
    // A bare repo has no working tree; an in-place agent would land in the bare
    // git dir. The default (no --branch) start must be refused with a message
    // steering the operator to --branch, not silently launched in the git dir.
    let (registry, _repo) = project_registry("bare-inplace");
    let bare = init_bare_git_repo("bare-inplace");

    let err = registry
        .create(SessionNewParams {
            cwd: Some(bare.clone()),
            ..params()
        })
        .await
        .expect_err("in-place on a bare repo must be refused");
    assert!(
        err.msg.contains("bare repository") && err.msg.contains("--branch"),
        "error must explain the bare repo and steer to --branch: {err:?}"
    );
    // Nothing was launched.
    assert!(
        registry.list().await.is_empty(),
        "no session is created for a refused in-place bare start"
    );
}

#[tokio::test]
async fn worktree_session_on_a_bare_project_is_allowed() {
    // The steer in `in_place_session_on_a_bare_project_is_refused` is only valid
    // if --branch actually works on a bare repo: a worktree is added off it.
    let (registry, _repo) = project_registry("bare-worktree");
    let bare = init_bare_git_repo("bare-worktree");

    let info = registry
        .create(SessionNewParams {
            cwd: Some(bare.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            base_branch: Some("main".to_owned()),
            ..params()
        })
        .await
        .expect("a worktree session is allowed on a bare repo");
    assert!(
        info.worktree_path.is_some(),
        "a worktree was bound off the bare repo: {info:?}"
    );
    assert_eq!(info.branch.as_deref(), Some("feat/x"));
}

#[tokio::test]
async fn session_new_in_a_non_git_cwd_records_no_project() {
    // A plain shell in a non-git directory: no project, no stamping, today's
    // behavior unchanged.
    let (registry, _repo) = project_registry("non-git");
    let non_git = std::env::temp_dir().join(format!(
        "pohunek-nongit-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&non_git).expect("create non-git dir");

    let info = registry
        .create(SessionNewParams {
            cwd: Some(non_git.clone()),
            ..params()
        })
        .await
        .expect("plain shell session is created");

    assert_eq!(info.project_id, None, "no git ⇒ no project");
    assert_eq!(info.is_linked_worktree, None);
    assert_eq!(info.worktree_path, None);
    assert!(
        registry
            .projects()
            .expect("projects configured")
            .store()
            .load_projects()
            .expect("load")
            .is_empty(),
        "a non-git directory must register nothing"
    );
}

#[tokio::test]
async fn session_new_with_project_ref_binds_the_main_checkout_in_place() {
    // Resolve a project by its id reference (the only remote-capable option):
    // an in-place session launches in the project's main checkout.
    let (registry, repo) = project_registry("by-ref");
    // Auto-register by starting once in the repo, then reference it by id.
    let first = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            ..params()
        })
        .await
        .expect("first session auto-registers the project");
    let project_id = first.project_id.clone().expect("project stamped");

    let info = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            project: Some(project_id.clone()),
            ..params()
        })
        .await
        .expect("session created from a --project reference");

    assert_eq!(info.project_id.as_deref(), Some(project_id.as_str()));
    assert_eq!(info.worktree_path, None, "no --branch ⇒ in-place");
    assert_eq!(info.is_linked_worktree, Some(false));
    assert_eq!(
        info.cwd,
        std::fs::canonicalize(&repo).expect("canonical repo"),
        "in-place runs in the project's main checkout"
    );
}

#[tokio::test]
async fn session_new_with_unknown_project_ref_is_rejected() {
    let (registry, _repo) = project_registry("unknown-ref");
    let err = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            project: Some("does-not-exist".to_owned()),
            ..params()
        })
        .await
        .expect_err("an unknown project reference must error");
    assert_eq!(err.code, "project_not_found", "got: {err:?}");
}

#[tokio::test]
async fn session_new_branch_with_detected_project_binds_worktree_carrying_project_id() {
    // `--branch` in a detected project builds a worktree-per-session off the
    // project's repo; the worktree's binding carries the project id so prune /
    // `project show` can find pohunek's own worktrees later (M5).
    let (registry, repo) = project_registry("wt-project");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created from the detected project");

    assert!(info.worktree_path.is_some(), "--branch binds a worktree");
    assert_eq!(info.is_linked_worktree, Some(true));
    let project_id = info.project_id.clone().expect("project stamped on session");

    let binding = registry
        .projects()
        .expect("projects configured")
        .store()
        .load_worktrees()
        .expect("load worktrees")
        .into_iter()
        .find(|b| b.session_id == info.id.0)
        .expect("this session has a worktree binding");
    assert_eq!(
        binding.project_id.as_deref(),
        Some(project_id.as_str()),
        "the worktree binding must carry the project id"
    );
}

#[tokio::test]
async fn cwd_hint_remaps_between_registered_worktrees() {
    let (registry, repo) = project_registry("cwd-remap-worktrees");
    let first = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/a".to_owned()),
            ..params()
        })
        .await
        .expect("first worktree session");
    let second = registry
        .create(SessionNewParams {
            cwd: Some(repo),
            name: None,
            branch: Some("feat/b".to_owned()),
            ..params()
        })
        .await
        .expect("second worktree session");

    let first_path = first.worktree_path.clone().expect("first worktree");
    let second_path = second.worktree_path.clone().expect("second worktree");
    let second_nested = second_path.join("nested");
    fs::create_dir_all(&second_nested).expect("create nested cwd");

    registry
        .record_cwd_hint(&first.id, second_nested.display().to_string())
        .await;
    let moved = registry.inspect(&first.id).await.expect("inspect moved");

    assert_eq!(moved.cwd, second_nested);
    assert_eq!(moved.cwd_source, Some(CwdSource::Osc7));
    assert_eq!(moved.worktree_path.as_deref(), Some(second_path.as_path()));
    assert_eq!(moved.branch.as_deref(), Some("feat/b"));
    assert_eq!(moved.is_linked_worktree, Some(true));
    assert_eq!(moved.project_id, first.project_id);

    registry
        .record_cwd_hint(&first.id, first_path.display().to_string())
        .await;
    let restored = registry.inspect(&first.id).await.expect("inspect restored");

    assert_eq!(restored.cwd, first_path);
    assert_eq!(restored.cwd_source, Some(CwdSource::Osc7));
    assert_eq!(restored.worktree_path, first.worktree_path);
    assert_eq!(restored.branch.as_deref(), Some("feat/a"));
    assert_eq!(restored.is_linked_worktree, Some(true));
    assert_eq!(restored.project_id, first.project_id);

    let _ = registry.stop(&first.id).await;
    let _ = registry.stop(&second.id).await;
}

#[tokio::test]
async fn cwd_hint_into_an_unregistered_repo_registers_no_project() {
    // Regression: cwd hints (OSC 7 / procwatch focus) used to upsert an Auto
    // project record for whatever repo the observed cwd landed in, so a repo a
    // watched process merely sat in — e.g. a throwaway fixture repo created by
    // a nested test-suite daemon — permanently polluted the registry. Hints
    // must only *derive* the association; registration stays on session.new
    // and explicit `project add`.
    let (registry, repo) = project_registry("hint-no-register");
    let session = registry
        .create(SessionNewParams {
            cwd: Some(repo),
            ..params()
        })
        .await
        .expect("session in the repo");

    let other_repo = init_git_repo("hint-no-register-other");
    registry
        .record_cwd_hint(&session.id, other_repo.display().to_string())
        .await;
    let moved = registry.inspect(&session.id).await.expect("inspect moved");

    assert_eq!(moved.cwd, other_repo);
    assert_eq!(moved.cwd_source, Some(CwdSource::Osc7));
    let other_git_common_dir =
        std::fs::canonicalize(other_repo.join(".git")).expect("canonical other .git");
    assert_eq!(
        moved.project_id.as_deref(),
        Some(project_id(&other_git_common_dir).as_str()),
        "the association still carries the stable derived project id"
    );

    let projects = registry
        .projects()
        .expect("projects configured")
        .store()
        .load_projects()
        .expect("load projects");
    assert_eq!(
        projects.len(),
        1,
        "only the session.new repo is registered; the hinted repo must not be: {projects:?}"
    );
    assert_eq!(projects[0].id(), session.project_id.expect("stamped"));

    let _ = registry.stop(&session.id).await;
}

#[tokio::test]
async fn session_new_with_project_ref_bumps_last_used_at() {
    // The data model defines last_used_at as bumped on each session start; the
    // --project reference path must do that too, not only auto-detection.
    let (registry, repo) = project_registry("touch");
    let first = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            ..params()
        })
        .await
        .expect("first session auto-registers");
    let project_id = first.project_id.clone().expect("project stamped");
    let projects = registry.projects().expect("projects");
    let store = projects.store();

    // Backdate last_used_at so the bump is unambiguously observable.
    let mut record = store
        .load_projects()
        .expect("load")
        .into_iter()
        .next()
        .expect("one project");
    record.last_used_at = "2000-01-01T00:00:00Z".to_owned();
    store.record_project(&record).expect("backdate");

    registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            project: Some(project_id),
            ..params()
        })
        .await
        .expect("session created by --project reference");

    let after = store
        .load_projects()
        .expect("reload")
        .into_iter()
        .next()
        .expect("one project");
    assert_ne!(
        after.last_used_at, "2000-01-01T00:00:00Z",
        "a --project reference must bump last_used_at"
    );
}

#[tokio::test]
async fn session_new_with_explicit_non_git_repo_errors() {
    // An explicitly named --repo that is not a git work tree must error, not
    // silently launch a plain shell somewhere else (no silent defaults).
    let (registry, _repo) = project_registry("explicit-nonrepo");
    let nonrepo = std::env::temp_dir().join(format!(
        "pohunek-nonrepo-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&nonrepo).expect("create non-git dir");

    let err = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            repo: Some(nonrepo),
            ..params()
        })
        .await
        .expect_err("an explicit non-git --repo must error");
    assert_eq!(err.code, "not_a_git_repo", "got: {err:?}");
}

#[tokio::test]
async fn session_new_rejects_project_and_repo_together() {
    // --project and --repo both name the target repo; accepting both would
    // persist an incoherent binding, so the daemon rejects the combination.
    let (registry, repo) = project_registry("mutual-exclusion");
    let err = registry
        .create(SessionNewParams {
            cwd: None,
            name: None,
            project: Some("anything".to_owned()),
            repo: Some(repo),
            ..params()
        })
        .await
        .expect_err("--project and --repo together must be rejected");
    assert!(err.msg.contains("mutually exclusive"), "got: {err:?}");
}

#[tokio::test]
async fn remove_project_with_prune_removes_owned_worktrees_and_forgets_the_record() {
    let (registry, repo) = project_registry("prune");
    // A worktree session: its binding carries the project id.
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    let project_id = info.project_id.clone().expect("project stamped");
    assert!(worktree.exists());
    // Stop the session first; its worktree binding is intentionally kept.
    registry.stop(&info.id).await.expect("stop session");

    let result = registry
        .remove_project(&project_id, true)
        .await
        .expect("remove with prune");
    assert!(result.removed, "the project record was removed");
    assert_eq!(result.pruned_worktrees, 1, "the owned worktree was pruned");
    assert!(!worktree.exists(), "pruned worktree directory is gone");
    assert!(
        registry
            .projects()
            .expect("projects")
            .store()
            .load_projects()
            .expect("load")
            .is_empty(),
        "the project record is forgotten"
    );
}

#[tokio::test]
async fn remove_project_prune_skips_a_worktree_with_a_live_session() {
    // A worktree a RUNNING session is using must not be pruned out from under
    // it; it is skipped and reported. Because a worktree was skipped, the
    // record is KEPT (removed = false) so its surviving binding keeps pointing
    // at a real project (Option (b)); a later `rm` forgets it once idle.
    let (registry, repo) = project_registry("prune-skip");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    let project_id = info.project_id.clone().expect("project stamped");
    // The session is left RUNNING (not stopped) — it is live in the worktree.

    let result = registry
        .remove_project(&project_id, true)
        .await
        .expect("remove with prune");
    assert!(
        !result.removed,
        "the record is kept while a live worktree remains"
    );
    assert_eq!(
        result.pruned_worktrees, 0,
        "the live worktree is not pruned"
    );
    assert_eq!(
        result.skipped_worktrees,
        vec![info.id.0.clone()],
        "the live session is reported as skipped"
    );
    assert!(
        worktree.exists(),
        "a live session's worktree is left on disk"
    );
    assert!(
        !registry
            .projects()
            .expect("projects")
            .store()
            .load_projects()
            .expect("load")
            .is_empty(),
        "the record stays so the skipped worktree's binding is not dangling"
    );
}

#[tokio::test]
async fn remove_project_without_prune_leaves_worktrees_intact() {
    let (registry, repo) = project_registry("no-prune");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    let project_id = info.project_id.clone().expect("project stamped");
    registry.stop(&info.id).await.expect("stop session");

    let result = registry
        .remove_project(&project_id, false)
        .await
        .expect("remove without prune");
    assert!(result.removed);
    assert_eq!(
        result.pruned_worktrees, 0,
        "nothing pruned without the flag"
    );
    assert!(
        worktree.exists(),
        "a plain rm must leave the worktree on disk"
    );
}

#[tokio::test]
async fn remove_worktree_removes_an_owned_idle_worktree() {
    let (registry, repo) = project_registry("wt-remove");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    assert!(worktree.exists());
    // Stop the session so it is terminal; its binding (ownership proof) stays.
    registry.stop(&info.id).await.expect("stop session");

    let result = registry
        .remove_worktree(&worktree)
        .await
        .expect("remove owned worktree");
    assert!(result.removed, "the owned worktree was removed");
    assert!(!worktree.exists(), "the worktree directory is gone");
}

#[tokio::test]
async fn remove_worktree_refuses_a_live_session() {
    let (registry, repo) = project_registry("wt-remove-live");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            name: None,
            branch: Some("feat/x".to_owned()),
            ..params()
        })
        .await
        .expect("worktree session created");
    let worktree = info.worktree_path.clone().expect("worktree path");
    // The session is left RUNNING — it is live in the worktree.

    let err = registry
        .remove_worktree(&worktree)
        .await
        .expect_err("a live worktree is refused");
    assert_eq!(err.code, "worktree_in_use");
    assert!(
        worktree.exists(),
        "a live session's worktree is left on disk"
    );
}

#[tokio::test]
async fn remove_worktree_refuses_an_unowned_path() {
    // The main checkout has no worktree binding, so it is not pohunek-owned and
    // must be refused rather than removed.
    let (registry, repo) = project_registry("wt-remove-unowned");
    let err = registry
        .remove_worktree(&repo)
        .await
        .expect_err("an unowned path is refused");
    assert_eq!(err.code, "worktree_not_owned");
    assert!(repo.exists(), "the main checkout is untouched");
}

#[tokio::test]
async fn missing_program_spawn_returns_agent_binary_missing() {
    // A plain shell session whose program does not exist fails at the PTY
    // spawn (ENOENT). That must map to the typed `agent_binary_missing` error
    // naming the program and carrying a recover hint, not `spawn_failed`.
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new(
            "/nonexistent/pohunek-missing-program",
            std::iter::empty::<String>(),
        ),
        ..SessionRegistryConfig::default()
    });

    let err = registry
        .create(params())
        .await
        .expect_err("missing program must fail to spawn");

    assert_eq!(err.code, "agent_binary_missing", "got: {err:?}");
    assert!(
        err.msg.contains("pohunek-missing-program"),
        "error must name the missing program: {err:?}"
    );
    assert!(err.recover.is_some(), "must carry a recover hint: {err:?}");
}

#[tokio::test]
async fn detects_successful_process_exit() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "exit 0"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    let exit = registry
        .wait_for_exit(&created.id, Duration::from_secs(2))
        .await
        .expect("session exits");

    assert_eq!(exit.state, SessionState::Done);
    assert_eq!(exit.exit_code, Some(0));
}

#[tokio::test]
async fn session_start_hook_runs_after_spawn_without_blocking_create() {
    let config_dir = temp_dir("session-start-config");
    let cwd = temp_dir("session-start-cwd");
    let marker = config_dir.join("session-start.marker");
    write_host_hook(
            &config_dir,
            "session-start",
            &format!(
                "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_AGENT\" >> {}\n",
                marker.display()
            ),
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir.clone()),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session returns while hook runs best-effort");

    let contents =
        wait_for_file_contains(&marker, &format!("session-start:{}:shell", created.id.0)).await;
    assert_eq!(contents.lines().count(), 1, "session-start fires once");

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn session_start_hook_is_best_effort_when_hook_hangs() {
    let config_dir = temp_dir("session-start-hang-config");
    let cwd = temp_dir("session-start-hang-cwd");
    write_host_hook(&config_dir, "session-start", "#!/bin/sh\nsleep 30\n");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        hook_timeout: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });

    let started = std::time::Instant::now();
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session returns despite a hanging session-start hook");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "session-start hook must be best-effort and not wedge create"
    );

    let _ = registry.stop(&created.id).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn session_stop_hook_reports_stopped_done_and_failed_reasons_once() {
    async fn run_case(tag: &str, command: &str, stop: bool, expected_reason: &str) {
        let config_dir = temp_dir(&format!("session-stop-config-{tag}"));
        let cwd = temp_dir(&format!("session-stop-cwd-{tag}"));
        let store_path = temp_store_path(&format!("session-stop-store-{tag}"));
        let agents_dir = temp_agents_dir_with(
            &format!("session-stop-agent-{tag}"),
            "resumable",
            &format!("base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"{command}\"]\n"),
        );
        let marker = config_dir.join("session-stop.marker");
        write_host_hook(
                &config_dir,
                "session-stop",
                &format!(
                    "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_STOP_REASON\" >> {}\n",
                    marker.display()
                ),
            );
        let registry = SessionRegistry::new(SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            config_dir: Some(config_dir),
            store_path: Some(store_path.clone()),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        });

        let created = registry
            .create(SessionNewParams {
                cwd: Some(cwd),
                ..resumable_params()
            })
            .await
            .expect("create session");
        let recorded = registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: format!("native-{tag}"),
                transcript_path: None,
            })
            .await;
        assert!(recorded.recorded, "native id captured for {tag}");
        assert_eq!(
            crate::store::Store::new(store_path.clone())
                .load_resume()
                .expect("load before terminal")
                .len(),
            1,
            "terminal transition precondition: one resume binding for {tag}"
        );

        if stop {
            registry.stop(&created.id).await.expect("stop session");
        } else {
            registry
                .wait_for_exit(&created.id, Duration::from_secs(2))
                .await
                .expect("session exits");
        }

        let expected = format!("session-stop:{}:{expected_reason}", created.id.0);
        let contents = wait_for_file_contains(&marker, &expected).await;
        assert_eq!(
            contents.lines().count(),
            1,
            "session-stop fires once for {tag}: {contents:?}"
        );
        assert!(
            crate::store::Store::new(store_path)
                .load_resume()
                .expect("load after terminal")
                .is_empty(),
            "terminal transition must remove resume binding for {tag}"
        );
    }

    run_case("stopped", "sleep 30", true, "stopped").await;
    run_case("done", "sleep 0.2; exit 0", false, "done").await;
    run_case("failed", "sleep 0.2; exit 7", false, "failed").await;
}

#[tokio::test]
async fn agent_state_hook_fires_once_per_distinct_activity_value() {
    let config_dir = temp_dir("agent-state-config");
    let cwd = temp_dir("agent-state-cwd");
    let marker = config_dir.join("agent-state.marker");
    write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nprintf '%s:%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_SESSION_ID\" \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display()
            ),
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let mut contents = wait_for_line_count(&marker, 1).await;
    assert!(contents.contains(&format!("agent-state:{}:working", created.id.0)));

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    contents = fs::read_to_string(&marker).expect("read marker");
    assert_eq!(
        contents.lines().count(),
        1,
        "same-state refresh must not fire another hook: {contents:?}"
    );

    for (activity, expected_count) in [
        (AgentActivity::Blocked, 2),
        (AgentActivity::Working, 3),
        (AgentActivity::Idle, 4),
        (AgentActivity::Working, 5),
    ] {
        registry
            .record_activity(&created.id, transition(activity))
            .await;
        contents = wait_for_line_count(&marker, expected_count).await;
    }

    let lines: Vec<String> = contents.lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 5, "only distinct values fire: {contents:?}");
    assert_eq!(
        lines,
        vec![
            format!("agent-state:{}:working", created.id.0),
            format!("agent-state:{}:blocked", created.id.0),
            format!("agent-state:{}:working", created.id.0),
            format!("agent-state:{}:idle", created.id.0),
            format!("agent-state:{}:working", created.id.0),
        ]
    );

    registry.stop(&created.id).await.expect("stop session");
    registry.shutdown_agent_state_hooks().await;
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "tracked for session module decomposition"
)]
async fn session_layer_hooks_run_with_cleared_env_and_exact_allowlist() {
    let config_dir = temp_dir("session-hook-env-config");
    let cwd = temp_dir("session-hook-env-cwd");
    let start_env = config_dir.join("session-start.env");
    let state_env = config_dir.join("agent-state.env");
    write_host_hook(
        &config_dir,
        "session-start",
        &format!("#!/bin/sh\nenv > {}\n", start_env.display()),
    );
    write_host_hook(
        &config_dir,
        "agent-state",
        &format!("#!/bin/sh\nenv > {}\n", state_env.display()),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    wait_for_file_contains(&start_env, "POHUNEK_HOOK_EVENT=session-start").await;
    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    wait_for_file_contains(&state_env, "POHUNEK_ACTIVITY=working").await;

    let start = parse_env_dump(&fs::read_to_string(&start_env).expect("read start env"));
    assert_eq!(
        start.get("POHUNEK_HOOK_EVENT").map(String::as_str),
        Some("session-start")
    );
    assert_eq!(
        start.get("POHUNEK_SESSION_ID").map(String::as_str),
        Some(created.id.0.as_str())
    );
    assert_eq!(
        start.get("POHUNEK_AGENT").map(String::as_str),
        Some("shell")
    );
    assert_eq!(
        pohunek_env_keys(&start),
        [
            "POHUNEK_AGENT",
            "POHUNEK_HOOK_EVENT",
            "POHUNEK_PROJECT_ID",
            "POHUNEK_SESSION_ID",
        ]
        .map(str::to_owned)
        .to_vec()
    );

    let state = parse_env_dump(&fs::read_to_string(&state_env).expect("read state env"));
    assert_eq!(
        state.get("POHUNEK_HOOK_EVENT").map(String::as_str),
        Some("agent-state")
    );
    assert_eq!(
        state.get("POHUNEK_ACTIVITY").map(String::as_str),
        Some("working")
    );
    assert_eq!(
        pohunek_env_keys(&state),
        [
            "POHUNEK_ACTIVITY",
            "POHUNEK_AGENT",
            "POHUNEK_HOOK_EVENT",
            "POHUNEK_PROJECT_ID",
            "POHUNEK_SESSION_ID",
        ]
        .map(str::to_owned)
        .to_vec()
    );

    for env in [&start, &state] {
        assert!(env.contains_key("PATH"), "PATH is passed through");
        assert!(
            !env.keys().any(|key| key.starts_with("CARGO")),
            "daemon inherited CARGO_* env must be cleared: {:?}",
            env.keys().collect::<Vec<_>>()
        );
        for forbidden in [
            "GITHUB_TOKEN",
            "ANTHROPIC_API_KEY",
            "POHUNEK_SOCKET_PATH",
            "POHUNEK_DAEMON_ID",
            "POHUNEK_ENV",
            "POHUNEK_PROTOCOL_VERSION",
            "POHUNEK_REPO",
            "POHUNEK_WORKTREE",
            "POHUNEK_BRANCH",
            "POHUNEK_BASE_BRANCH",
        ] {
            assert!(
                !env.contains_key(forbidden),
                "{forbidden} must not be exposed to a session-layer hook"
            );
        }
    }

    registry.stop(&created.id).await.expect("stop session");
    registry.shutdown_agent_state_hooks().await;
}

#[tokio::test]
async fn in_place_session_fires_session_hooks_but_no_worktree_hooks() {
    let config_dir = temp_dir("in-place-hooks-config");
    let repo = init_git_repo("in-place-hooks-repo");
    let marker = config_dir.join("hooks.marker");
    for event_name in [
        "pre-create",
        "post-create",
        "pre-remove",
        "post-remove",
        "session-start",
        "session-stop",
        "agent-state",
    ] {
        write_host_hook(
            &config_dir,
            event_name,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$POHUNEK_HOOK_EVENT\" >> {}\n",
                marker.display()
            ),
        );
    }
    let store = temp_store_path("in-place-hooks-store");
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store),
        worktree_root: Some(worktree_root),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();

    let created = registry
        .create(SessionNewParams {
            cwd: Some(repo),
            ..params()
        })
        .await
        .expect("create in-place session");
    assert_eq!(created.worktree_path, None, "no --branch means in-place");
    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    wait_for_file_contains(&marker, "agent-state").await;
    registry.stop(&created.id).await.expect("stop session");
    let contents = wait_for_line_count(&marker, 3).await;

    let lines: Vec<&str> = contents.lines().collect();
    assert!(lines.contains(&"session-start"));
    assert!(lines.contains(&"agent-state"));
    assert!(lines.contains(&"session-stop"));
    for forbidden in ["pre-create", "post-create", "pre-remove", "post-remove"] {
        assert!(
            !lines.contains(&forbidden),
            "in-place sessions must not run {forbidden}: {contents:?}"
        );
    }

    registry.shutdown_agent_state_hooks().await;
}

#[tokio::test]
async fn project_backed_session_hooks_receive_project_id() {
    let config_dir = temp_dir("project-session-hooks-config");
    let repo = init_git_repo("project-session-hooks-repo");
    let marker = config_dir.join("project-hooks.marker");
    for event_name in ["session-start", "session-stop", "agent-state"] {
        write_host_hook(
                &config_dir,
                event_name,
                &format!(
                    "#!/bin/sh\nprintf '%s:%s\\n' \"$POHUNEK_HOOK_EVENT\" \"$POHUNEK_PROJECT_ID\" >> {}\n",
                    marker.display()
                ),
            );
    }
    let store = temp_store_path("project-session-hooks-store");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();

    let created = registry
        .create(SessionNewParams {
            cwd: Some(repo),
            ..params()
        })
        .await
        .expect("create project-backed in-place session");
    let project_id = created.project_id.clone().expect("project id stamped");
    wait_for_file_contains(&marker, &format!("session-start:{project_id}")).await;

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    wait_for_file_contains(&marker, &format!("agent-state:{project_id}")).await;

    registry.stop(&created.id).await.expect("stop session");
    let contents = wait_for_file_contains(&marker, &format!("session-stop:{project_id}")).await;
    for event_name in ["session-start", "agent-state", "session-stop"] {
        assert!(
            contents.contains(&format!("{event_name}:{project_id}")),
            "{event_name} must receive project id {project_id}: {contents:?}"
        );
    }

    registry.shutdown_agent_state_hooks().await;
}

#[tokio::test]
async fn agent_state_hook_dispatcher_survives_lag_and_shutdown_cancellation() {
    let config_dir = temp_dir("agent-state-lag-config");
    let cwd = temp_dir("agent-state-lag-cwd");
    let marker = config_dir.join("agent-state-lag.marker");
    write_host_hook(
        &config_dir,
        "agent-state",
        &format!(
            "#!/bin/sh\nsleep 0.1\nprintf '%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
            marker.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let (tx, rx) = tokio::sync::broadcast::channel(1);
    for n in 0..8 {
        let _ = tx.send(Event::new(
            protocol::event::SESSION_UPDATED,
            serde_json::json!({ "n": n }),
        ));
    }
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = super::spawn_agent_state_hook_dispatcher(registry.clone(), rx, shutdown.clone());
    wait_for_file_contains(&marker, "working").await;
    for n in 8..16 {
        let _ = tx.send(Event::new(
            protocol::event::SESSION_UPDATED,
            serde_json::json!({ "n": n }),
        ));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let contents = fs::read_to_string(&marker).expect("read marker after same-state lag");
    assert_eq!(
        contents.lines().count(),
        1,
        "lag re-read of the already-fired activity must not double-fire: {contents:?}"
    );

    registry
        .record_activity(&created.id, transition(AgentActivity::Blocked))
        .await;
    tx.send(Event::new(
        protocol::event::AGENT_STATE,
        serde_json::json!({
            "session_id": created.id.clone(),
            "activity": "blocked",
            "source": "process",
        }),
    ))
    .expect("send blocked event");
    wait_for_file_contains(&marker, "blocked").await;

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("dispatcher joins after cancellation")
        .expect("dispatcher task succeeds");

    registry.stop(&created.id).await.expect("stop session");
}

#[tokio::test]
async fn agent_state_hook_dispatcher_flushes_buffered_event_on_shutdown() {
    let config_dir = temp_dir("agent-state-shutdown-config");
    let cwd = temp_dir("agent-state-shutdown-cwd");
    let marker = config_dir.join("agent-state-shutdown.marker");
    write_host_hook(
        &config_dir,
        "agent-state",
        &format!(
            "#!/bin/sh\nsleep 0.15\nprintf '%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
            marker.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let handle = super::spawn_agent_state_hook_dispatcher(registry.clone(), rx, shutdown.clone());
    tx.send(Event::new(
        protocol::event::AGENT_STATE,
        serde_json::json!({
            "session_id": created.id.clone(),
            "activity": "working",
            "source": "process",
        }),
    ))
    .expect("send buffered event");
    shutdown.cancel();
    handle.await.expect("dispatcher joins after cancellation");

    let contents = fs::read_to_string(&marker)
        .expect("dispatcher must await the hook flushed during shutdown");
    assert!(
        contents.contains("working"),
        "dispatcher must flush and await buffered activity before joining: {contents:?}"
    );
    registry.stop(&created.id).await.expect("stop session");
}

#[tokio::test]
async fn agent_state_hook_coalesces_flaps_while_hook_is_in_flight() {
    let config_dir = temp_dir("agent-state-coalesce-config");
    let cwd = temp_dir("agent-state-coalesce-cwd");
    let marker = config_dir.join("agent-state-coalesce.marker");
    let release = config_dir.join("agent-state-coalesce.release");
    write_host_hook(
            &config_dir,
            "agent-state",
            &format!(
                "#!/bin/sh\nprintf 'start:%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\nif [ \"$POHUNEK_ACTIVITY\" = working ]; then\n  while [ ! -f {} ]; do sleep 0.02; done\nfi\nprintf 'done:%s\\n' \"$POHUNEK_ACTIVITY\" >> {}\n",
                marker.display(),
                release.display(),
                marker.display(),
            ),
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        hook_timeout: Duration::from_secs(1),
        config_dir: Some(config_dir),
        ..SessionRegistryConfig::default()
    });
    registry.spawn_agent_state_hooks();
    let created = registry
        .create(SessionNewParams {
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("create session");

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let contents = wait_for_file_contains(&marker, "start:working").await;
    assert_eq!(
        contents.lines().collect::<Vec<_>>(),
        vec!["start:working"],
        "the first hook must be in flight before the flap sequence starts"
    );
    registry
        .record_activity(&created.id, transition(AgentActivity::Blocked))
        .await;
    registry
        .record_activity(&created.id, transition(AgentActivity::Idle))
        .await;
    registry
        .record_activity(&created.id, transition(AgentActivity::Blocked))
        .await;
    fs::write(&release, "").expect("release first hook");

    let contents = wait_for_file_contains(&marker, "done:blocked").await;
    assert_eq!(
        contents.lines().collect::<Vec<_>>(),
        vec![
            "start:working",
            "done:working",
            "start:blocked",
            "done:blocked"
        ],
        "only one hook runs in flight per session and intermediate flap is coalesced"
    );

    registry.stop(&created.id).await.expect("stop session");
    registry.shutdown_agent_state_hooks().await;
}

#[test]
fn invalid_agent_activity_parse_returns_error() {
    let err = super::parse_agent_activity(&serde_json::json!("future-state"))
        .expect_err("unknown activity should remain an explicit parse error");
    assert!(
        err.to_string().contains("future-state"),
        "parse error should name the invalid activity: {err}"
    );
}

#[tokio::test]
async fn stop_marks_running_session_stopped() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    let stopped = registry.stop(&created.id).await.expect("stop session");
    let inspected = registry
        .inspect(&created.id)
        .await
        .expect("inspect session");

    assert!(stopped.stopped);
    assert_eq!(inspected.state, SessionState::Stopped);
}

#[tokio::test]
async fn remove_evicts_an_already_stopped_session() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    registry.stop(&created.id).await.expect("stop session");
    let mut events = registry.subscribe();

    let removed = registry.remove(&created.id).await.expect("remove session");

    assert!(removed.removed);
    // The session was already terminal, so removal did not stop it again.
    assert!(!removed.stopped);
    let event = next_session_removed(&mut events).await;
    assert_eq!(event.id, created.id);
    let err = registry
        .inspect(&created.id)
        .await
        .expect_err("removed session is gone");
    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn remove_stops_a_live_session_then_evicts() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");

    let removed = registry.remove(&created.id).await.expect("remove session");

    assert!(removed.removed);
    // The session was still live, so removal stopped it first.
    assert!(removed.stopped);
    let err = registry
        .inspect(&created.id)
        .await
        .expect_err("removed session is gone");
    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn remove_unknown_session_is_session_not_found() {
    let registry = SessionRegistry::default();

    let err = registry
        .remove(&SessionId("s-missing".to_owned()))
        .await
        .expect_err("unknown session cannot be removed");

    assert_eq!(err.code, "session_not_found");
}

#[tokio::test]
async fn attach_tokens_are_one_shot_and_expired_tokens_are_pruned() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(500),
        attach_token_ttl: Duration::from_millis(1),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    let expired = registry
        .attach(&attach_params(&created.id))
        .await
        .expect("attach token");
    tokio::time::sleep(Duration::from_millis(5)).await;
    let fresh = registry
        .attach(&attach_params(&created.id))
        .await
        .expect("fresh attach token");

    {
        let pending = registry.inner.pending_attaches.lock().await;
        assert!(
            !pending.contains_key(&expired.stream_id),
            "expired pending attach token should be pruned"
        );
        assert!(
            pending.contains_key(&fresh.stream_id),
            "fresh pending attach token should remain"
        );
    };

    let redeemed = registry
        .redeem_attach(&fresh.stream_id)
        .await
        .expect("redeem fresh attach token");
    let second_redeem = registry
        .redeem_attach(&fresh.stream_id)
        .await
        .expect_err("stream id is one-shot");
    assert_eq!(second_redeem.code, "attach_not_found");

    registry.finish_attach(&redeemed.stream_id).await;
    let stopped = registry.stop(&created.id).await.expect("stop session");
    assert!(stopped.stopped);
}

#[tokio::test]
async fn attach_from_inside_the_same_session_is_rejected() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry.create(params()).await.expect("create session");
    let daemon_id = registry.daemon_instance_id().to_owned();
    let worker_id = created
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.worker_id.clone())
        .expect("created session has a durable worker id");

    let self_feed = |session: &SessionId, worker: Option<&str>| SessionAttachParams {
        session_id: session.clone(),
        origin_session_id: Some(session.clone()),
        origin_daemon_id: Some(daemon_id.clone()),
        origin_worker_id: worker.map(str::to_owned),
    };

    // Origin id AND worker id both match this session's own worker: the
    // client is inside this session's own PTY, so attaching would loop its
    // output into its own input. Reject it.
    let err = registry
        .attach(&self_feed(&created.id, Some(&worker_id)))
        .await
        .expect_err("self-feeding attach must be rejected");
    assert_eq!(err.code, "attach_self_feedback");
    assert_eq!(err.class, protocol::ErrorClass::Daemon);
    assert!(
        err.recover.is_some(),
        "self-feedback error must carry a recovery hint: {err:?}"
    );
    // The rejected attach mints no pending token.
    assert!(
        registry.inner.pending_attaches.lock().await.is_empty(),
        "a rejected self-feeding attach must not leave a pending token"
    );

    // Matching session id and worker id, but a stale/foreign daemon instance
    // id: the daemon instance id is not an ownership identity (e.g. it
    // changes across a daemon restart while the worker id stays stable), so
    // it must not weaken the worker-id-based guard.
    let err = registry
        .attach(&SessionAttachParams {
            session_id: created.id.clone(),
            origin_session_id: Some(created.id.clone()),
            origin_daemon_id: Some("some-other-daemon".to_owned()),
            origin_worker_id: Some(worker_id.clone()),
        })
        .await
        .expect_err("a stale daemon id must not weaken the worker-id-based guard");
    assert_eq!(err.code, "attach_self_feedback");

    // Same session id but a DIFFERENT worker id (a colliding id on another
    // worker, or a stale value from a previous generation): no loop, so it
    // must be allowed, not falsely rejected.
    registry
        .attach(&self_feed(&created.id, Some("some-other-worker")))
        .await
        .expect("matching session id with a different worker id is allowed");
    // Origin id without any worker id cannot be pinned to this session's worker.
    registry
        .attach(&self_feed(&created.id, None))
        .await
        .expect("origin id without a worker id is allowed");
    // A different session's terminal (this daemon) is a legitimate origin.
    registry
        .attach(&SessionAttachParams {
            session_id: created.id.clone(),
            origin_session_id: Some(SessionId("s-other".to_owned())),
            origin_daemon_id: Some(daemon_id.clone()),
            origin_worker_id: Some(worker_id.clone()),
        })
        .await
        .expect("attach from a different session's terminal is allowed");
    // A plain terminal (no origin reported) is allowed.
    registry
        .attach(&attach_params(&created.id))
        .await
        .expect("attach with no origin is allowed");

    registry.stop(&created.id).await.expect("stop session");
}

#[test]
fn daemon_instance_ids_are_distinct_per_registry() {
    // Two registries built in this one process must still get distinct ids
    // (the process-local counter disambiguates same-instant construction), so
    // the self-feeding-attach guard never conflates two daemon instances.
    let a = SessionRegistry::default();
    let b = SessionRegistry::default();
    assert_ne!(
        a.daemon_instance_id(),
        b.daemon_instance_id(),
        "each registry must get a distinct daemon instance id"
    );
    assert!(a.daemon_instance_id().starts_with("d-"));
}

#[tokio::test]
async fn inspect_missing_session_returns_not_found() {
    let registry = SessionRegistry::default();
    let missing = registry
        .inspect_str("s-missing")
        .await
        .expect_err("missing session");

    assert_eq!(missing.code, "session_not_found");
}

#[test]
fn bracketed_paste_input_frame_wraps_text_and_submit_together() {
    let writes = super::build_input_writes(
        "hello\nworld",
        InputRules {
            bracketed_paste: true,
            submit_delay: Duration::ZERO,
        },
    );

    assert_eq!(
        writes.immediate,
        b"\x1b[200~hello\nworld\x1b[201~\r".to_vec()
    );
    assert_eq!(writes.delayed_submit, None);
}

#[test]
fn delayed_submit_input_frame_splits_text_and_submit() {
    let writes = super::build_input_writes(
        "hello Claude",
        InputRules {
            bracketed_paste: false,
            submit_delay: Duration::from_millis(150),
        },
    );

    assert_eq!(writes.immediate, b"hello Claude".to_vec());
    assert_eq!(
        writes.delayed_submit,
        Some((Duration::from_millis(150), b"\r".to_vec()))
    );
}

#[test]
fn bare_codex_launch_receives_initial_input_as_prompt_arg() {
    let resolved = crate::agent::ProfileRegistry::default()
        .resolve_agent("codex")
        .expect("resolve bare codex");
    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("codex", []),
        Some("# Pohunek Assistant".to_owned()),
    );

    assert_eq!(plan.command.args, vec!["# Pohunek Assistant".to_owned()]);
    assert_eq!(plan.pending_initial_input, None);
}

#[test]
fn bare_claude_launch_receives_initial_input_as_prompt_arg() {
    let resolved = crate::agent::ProfileRegistry::default()
        .resolve_agent("claude")
        .expect("resolve bare claude");
    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("claude", []),
        Some("# Pohunek Assistant".to_owned()),
    );

    assert_eq!(plan.command.args, vec!["# Pohunek Assistant".to_owned()]);
    assert_eq!(plan.pending_initial_input, None);
}

#[test]
fn shell_launch_keeps_initial_input_for_pty_injection() {
    let resolved = crate::agent::ProfileRegistry::default()
        .resolve_agent("shell")
        .expect("resolve shell");
    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("/bin/sh", ["-c", "sleep 30"]),
        Some("hello shell".to_owned()),
    );

    assert_eq!(plan.pending_initial_input.as_deref(), Some("hello shell"));
}

#[test]
fn host_profile_launch_keeps_initial_input_for_pty_injection() {
    let agents_dir = temp_dir("profile-initial-prompt-agents");
    fs::write(
        agents_dir.join("wrapped-codex.toml"),
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    )
    .expect("write profile");
    let registry = crate::agent::ProfileRegistry::new(Some(agents_dir));
    let resolved = registry
        .resolve_agent("wrapped-codex")
        .expect("resolve profile");

    let plan = super::plan_initial_input_delivery(
        &resolved,
        pty_command("/bin/sh", ["-c", "sleep 30"]),
        Some("# Pohunek Assistant".to_owned()),
    );

    assert_eq!(
        plan.command.args,
        vec!["-c".to_owned(), "sleep 30".to_owned()]
    );
    assert_eq!(
        plan.pending_initial_input.as_deref(),
        Some("# Pohunek Assistant")
    );
}

#[test]
fn hook_env_injected_for_every_agent_kind_with_socket() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        socket_path: Some(PathBuf::from("/run/pohunek/daemon.sock")),
        ..SessionRegistryConfig::default()
    });
    let id = SessionId("s-7".to_owned());

    for agent in [AgentKind::Shell, AgentKind::Codex, AgentKind::Claude] {
        let env = registry.hook_env(agent, &id);
        let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(lookup(ENV_FLAG).as_deref(), Some("1"));
        assert_eq!(
            lookup(ENV_SOCKET_PATH).as_deref(),
            Some("/run/pohunek/daemon.sock")
        );
        assert_eq!(lookup(ENV_SESSION_ID).as_deref(), Some("s-7"));
        assert_eq!(
            lookup(ENV_PROTOCOL_VERSION).as_deref(),
            Some(protocol::PROTOCOL_VERSION.get().to_string().as_str())
        );
    }
}

#[test]
fn hook_env_absent_without_configured_socket() {
    let registry = SessionRegistry::default();
    let id = SessionId("s-1".to_owned());
    assert!(registry.hook_env(AgentKind::Shell, &id).is_empty());
    assert!(registry.hook_env(AgentKind::Claude, &id).is_empty());
    assert!(registry.hook_env(AgentKind::Codex, &id).is_empty());
}

#[test]
fn session_pty_env_marks_session_id_for_every_agent_kind() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        socket_path: Some(PathBuf::from("/run/pohunek/daemon.sock")),
        ..SessionRegistryConfig::default()
    });
    let id = SessionId("s-7".to_owned());

    // Every kind carries the hook handshake plus POHUNEK_DAEMON_ID. The
    // daemon id keeps self-feeding attach detection scoped to this daemon
    // instance, and POHUNEK_SESSION_ID must not be duplicated on top of the
    // hook env that already carries it.
    for agent in [AgentKind::Shell, AgentKind::Codex, AgentKind::Claude] {
        let env = registry.session_pty_env(agent, &id);
        let lookup = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let session_ids: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k == ENV_SESSION_ID)
            .map(|(_, v)| v.as_str())
            .collect();
        // Present exactly once (agents must not get it duplicated on top of
        // the hook env that already carries it).
        assert_eq!(
            session_ids,
            vec!["s-7"],
            "{agent:?} must carry POHUNEK_SESSION_ID exactly once"
        );
        let daemon_ids: Vec<&str> = env
            .iter()
            .filter(|(k, _)| k == ENV_DAEMON_ID)
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            daemon_ids,
            vec![registry.daemon_instance_id()],
            "{agent:?} must carry POHUNEK_DAEMON_ID once, equal to this instance's id"
        );
        assert_eq!(
            lookup(ENV_FLAG).as_deref(),
            Some("1"),
            "{agent:?} must carry the hook gate flag"
        );
        assert_eq!(
            lookup(ENV_SOCKET_PATH).as_deref(),
            Some("/run/pohunek/daemon.sock"),
            "{agent:?} must carry the daemon socket path"
        );
        assert_eq!(
            lookup(ENV_PROTOCOL_VERSION).as_deref(),
            Some(protocol::PROTOCOL_VERSION.get().to_string().as_str()),
            "{agent:?} must carry the protocol version"
        );
    }
}

#[tokio::test]
async fn report_agent_on_shell_session_sets_active_agent_without_changing_launch_identity() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let result = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(1),
            pid: None,
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(result.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.agent, "shell");
    assert_eq!(inspected.agent_base, AgentKind::Shell);
    assert_eq!(inspected.active_agent.as_deref(), Some("codex"));
    assert_eq!(inspected.active_agent_base, Some(AgentKind::Codex));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("codex-native")
    );
    assert_eq!(inspected.active_agent_session_path, None);
    assert_eq!(inspected.native_session_id, None);
    assert_eq!(inspected.native_session_path, None);
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::Report);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_reconfigures_detector_and_release_restores_default_config() {
    let agents_dir = temp_agents_dir_with(
        "active-detector-config",
        "nested-codex",
        "base = \"codex\"\n\
             program = \"/bin/sh\"\n\
             args = [\"-c\", \"sleep 30\"]\n\
             manifest = \"nested-active\"\n",
    );
    write_agent_manifest(
        &agents_dir,
        "nested-active",
        r#"
            [[rules]]
            id = "profile-title"
            state = "blocked"
            priority = 1
            region = "osc_title"
            contains = "profile-only-title"
            "#,
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    let mut detector_config_rx = {
        let sessions = registry.inner.sessions.lock().await;
        sessions
            .get(&created.id)
            .expect("session entry")
            .detector_config
            .subscribe()
    };

    let default_config = detector_config_rx.borrow().clone();
    assert_eq!(title_activity(&default_config, "profile-only-title"), None);

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:nested-codex".to_owned(),
            agent: "nested-codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(1),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    detector_config_rx
        .changed()
        .await
        .expect("active detector config");
    let active_config = detector_config_rx.borrow().clone();
    assert_eq!(
        title_activity(&active_config, "profile-only-title"),
        Some(AgentActivity::Blocked)
    );

    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:nested-codex".to_owned(),
            agent: "nested-codex".to_owned(),
            seq: Some(1),
        })
        .await;
    assert!(release.released);
    detector_config_rx
        .changed()
        .await
        .expect("default detector config");
    let restored_config = detector_config_rx.borrow().clone();
    assert_eq!(title_activity(&restored_config, "profile-only-title"), None);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn osc_7_output_updates_cwd_before_next_procwatch_tick() {
    /// Gives the immediate procwatch tick after spawn time to observe launch cwd.
    const INITIAL_PROCWATCH_SETTLE: Duration = Duration::from_millis(100);

    let cwd = temp_dir("osc7-cwd");
    let script = format!(
        "IFS= read -r _; printf '\\033]7;file://{}\\007'; sleep 30",
        cwd.display()
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", script.as_str()]),
        stop_grace: Duration::from_millis(50),
        procwatch_poll: Duration::from_mins(1),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    tokio::time::sleep(INITIAL_PROCWATCH_SETTLE).await;
    registry
        .input(protocol::SessionInputParams {
            session_id: created.id.clone(),
            text: "trigger".to_owned(),
        })
        .await
        .expect("trigger OSC 7 output");

    let updated = wait_for_cwd_source(&registry, &created.id, &cwd, CwdSource::Osc7).await;

    assert_eq!(updated.cwd, cwd);
    assert_eq!(updated.cwd_source, Some(CwdSource::Osc7));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn release_agent_clears_current_active_agent_but_ignores_stale_sequence() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let newer = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(10),
            pid: None,
            agent_session_id: Some("codex-newer".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(newer.recorded);

    let stale_release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(9),
        })
        .await;
    assert!(!stale_release.released);
    let still_active = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(still_active.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        still_active.active_agent_session_id.as_deref(),
        Some("codex-newer")
    );

    let current_release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(10),
        })
        .await;
    assert!(current_release.released);
    let cleared = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    assert_eq!(cleared.active_agent_session_id, None);
    assert_eq!(cleared.active_agent_session_path, None);
    assert_eq!(cleared.activity, None);
    assert_eq!(cleared.state_source, StateSource::Process);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_release_with_no_sequence_does_not_clear_newer_sequenced_report() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let result = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(10),
            pid: None,
            agent_session_id: Some("codex-newer".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(result.recorded);

    let stale_release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: None,
        })
        .await;
    assert!(!stale_release.released);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("codex-newer")
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_with_no_sequence_does_not_overwrite_newer_sequenced_report() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let newer = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(10),
            pid: None,
            agent_session_id: Some("codex-newer".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(newer.recorded);

    let stale_report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: None,
            pid: None,
            agent_session_id: Some("codex-stale".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(!stale_report.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("codex-newer")
    );
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::Report);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_release_tombstone_rejects_delayed_lower_sequence() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(10),
            pid: None,
            agent_session_id: Some("codex-seq-10".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(11),
        })
        .await;
    assert!(release.released);
    let cleared = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    assert_eq!(cleared.active_agent_session_id, None);
    assert_eq!(cleared.active_agent_session_path, None);
    assert_eq!(cleared.activity, None);

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let detector_updated = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(detector_updated.activity, Some(AgentActivity::Working));
    assert_eq!(detector_updated.state_source, StateSource::Process);

    let delayed_report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(10),
            pid: None,
            agent_session_id: Some("codex-stale".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(!delayed_report.recorded);
    let still_clear = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(still_clear.active_agent, None);
    assert_eq!(still_clear.active_agent_base, None);
    assert_eq!(still_clear.active_agent_session_id, None);
    assert_eq!(still_clear.active_agent_session_path, None);
    assert_eq!(still_clear.activity, Some(AgentActivity::Working));
    assert_eq!(still_clear.state_source, StateSource::Process);

    let higher_report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(12),
            pid: None,
            agent_session_id: Some("codex-seq-12".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(higher_report.recorded);
    let active_again = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(active_again.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        active_again.active_agent_session_id.as_deref(),
        Some("codex-seq-12")
    );
    assert_eq!(active_again.activity, Some(AgentActivity::Blocked));
    assert_eq!(active_again.state_source, StateSource::Report);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_activity_blocks_detector_until_release() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(1),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let blocked = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(blocked.activity, Some(AgentActivity::Blocked));
    assert_eq!(blocked.state_source, StateSource::Report);

    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(1),
        })
        .await;
    assert!(release.released);

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let working = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(working.activity, Some(AgentActivity::Working));
    assert_eq!(working.state_source, StateSource::Process);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_without_activity_keeps_detector_activity_enabled() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: None,
            seq: Some(1),
            pid: None,
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    registry
        .record_activity(&created.id, transition(AgentActivity::Working))
        .await;
    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent.as_deref(), Some("codex"));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some("codex-native")
    );
    assert_eq!(inspected.activity, Some(AgentActivity::Working));
    assert_eq!(inspected.state_source, StateSource::Process);

    let release = registry
        .release_agent(SessionReleaseAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            seq: Some(1),
        })
        .await;
    assert!(release.released);
    let released = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(released.active_agent, None);
    assert_eq!(released.active_agent_session_id, None);
    assert_eq!(released.activity, Some(AgentActivity::Working));
    assert_eq!(released.state_source, StateSource::Process);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_agent_active_metadata_is_cleared_on_terminal_session() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");
    assert_eq!(created.native_session_id, None);
    assert_eq!(created.native_session_path, None);

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Blocked),
            seq: Some(1),
            pid: None,
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: Some("/tmp/codex-session.jsonl".to_owned()),
        })
        .await;
    assert!(report.recorded);

    registry.stop(&created.id).await.expect("stop session");

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent, None);
    assert_eq!(inspected.active_agent_base, None);
    assert_eq!(inspected.active_agent_session_id, None);
    assert_eq!(inspected.active_agent_session_path, None);
    assert_eq!(inspected.activity, None);
    assert_eq!(inspected.native_session_id, None);
    assert_eq!(inspected.native_session_path, None);
}

#[tokio::test]
async fn report_agent_returns_false_for_unknown_agent() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let result = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:unknown".to_owned(),
            agent: "not-a-real-agent".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(1),
            pid: None,
            agent_session_id: None,
            agent_session_path: None,
        })
        .await;
    assert!(!result.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent, None);
    assert_eq!(inspected.activity, None);

    let _ = registry.stop(&created.id).await;
}

fn codex_fact(process_id: Pid, parent_id: Pid) -> ProcessFact {
    ProcessFact {
        pid: process_id,
        ppid: parent_id,
        comm: "codex".to_owned(),
        cmdline: vec!["/usr/bin/codex".to_owned()],
    }
}

fn claude_fact(process_id: Pid, parent_id: Pid) -> ProcessFact {
    ProcessFact {
        pid: process_id,
        ppid: parent_id,
        comm: "claude".to_owned(),
        cmdline: vec!["/usr/bin/claude".to_owned()],
    }
}

/// PID used by the P3 release-path matrix to model the nested Codex process.
const RELEASE_MATRIX_AGENT_PID: Pid = 100;
/// `SessionStart` sequence used by the P3 release-path matrix.
const RELEASE_MATRIX_REPORT_SEQ: u64 = 1_000;
/// `SessionEnd` sequence used by the P3 release-path matrix.
const RELEASE_MATRIX_RELEASE_SEQ: u64 = RELEASE_MATRIX_REPORT_SEQ + 1;
/// Hook source used by Codex state callbacks.
const CODEX_HOOK_SOURCE: &str = "pohunek:codex";
/// Agent name used by Codex state callbacks.
const CODEX_HOOK_AGENT: &str = "codex";
/// Native session id used by the P3 release-path matrix.
const RELEASE_MATRIX_NATIVE_ID: &str = "codex-native";
/// Hook sequence used by direct-agent root-pid regression tests.
const DIRECT_AGENT_REPORT_SEQ: u64 = 2_000;
/// Number of reconcile ticks used to catch direct-agent active-agent flapping.
const DIRECT_AGENT_RESCAN_COUNT: usize = 3;
/// PID reused across two scans to model OS pid reuse before an exit watch fires.
const PID_REUSE_AGENT_PID: Pid = 225;
/// PID used by the ownership-marker tests to model a nested daemon's agent.
const FOREIGN_AGENT_PID: Pid = 240;
/// Delay separating pid-reuse observations so `first_seen` changes if reset.
const PID_REUSE_RESCAN_DELAY: Duration = Duration::from_millis(5);

async fn mock_procwatch_registry(tag: &str) -> (SessionRegistry, Arc<MockInspector>, SessionInfo) {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
            stop_grace: Duration::from_millis(50),
            procwatch_poll: Duration::from_mins(1),
            active_agent_claim_ttl: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    let created = registry
        .create(SessionNewParams {
            name: Some(tag.to_owned()),
            ..params()
        })
        .await
        .expect("create shell session");
    (registry, inspector, created)
}

async fn mock_direct_codex_registry(
    tag: &str,
) -> (SessionRegistry, Arc<MockInspector>, SessionInfo) {
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let agents_dir = temp_agents_dir_with(
        tag,
        "direct-codex",
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    );
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            procwatch_poll: Duration::from_mins(1),
            active_agent_claim_ttl: Duration::from_millis(50),
            agents_dir: Some(agents_dir),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    let created = registry
        .create(SessionNewParams {
            name: Some(tag.to_owned()),
            agent: "direct-codex".to_owned(),
            ..params()
        })
        .await
        .expect("create direct codex session");
    (registry, inspector, created)
}

async fn wait_for_active_agent_pid(
    registry: &SessionRegistry,
    id: &SessionId,
    expected: Option<Pid>,
) -> SessionInfo {
    for _ in 0..50 {
        let info = registry.inspect(id).await.expect("inspect session");
        if info.active_agent_pid == expected {
            return info;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for active_agent_pid {expected:?}");
}

async fn report_release_matrix_agent(registry: &SessionRegistry, created: &SessionInfo) {
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(RELEASE_MATRIX_REPORT_SEQ),
            pid: Some(RELEASE_MATRIX_AGENT_PID),
            agent_session_id: Some(RELEASE_MATRIX_NATIVE_ID.to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.active_agent.as_deref(), Some(CODEX_HOOK_AGENT));
    assert_eq!(inspected.active_agent_base, Some(AgentKind::Codex));
    assert_eq!(inspected.active_agent_pid, Some(RELEASE_MATRIX_AGENT_PID));
    assert_eq!(
        inspected.active_agent_session_id.as_deref(),
        Some(RELEASE_MATRIX_NATIVE_ID)
    );
}

#[tokio::test]
async fn direct_agent_root_pid_hook_claim_survives_procwatch_reconciles() {
    let (registry, inspector, created) = mock_direct_codex_registry("direct-root").await;
    inspector.set_descendants(created.pid, Vec::new());
    inspector.set_cwd(created.pid, temp_dir("direct-root-cwd"));

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(DIRECT_AGENT_REPORT_SEQ),
            pid: Some(created.pid),
            agent_session_id: Some("direct-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    for _ in 0..DIRECT_AGENT_RESCAN_COUNT {
        registry
            .rescan_procwatch_at(&created.id, created.pid, Instant::now())
            .await;
        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(inspected.active_agent.as_deref(), Some(CODEX_HOOK_AGENT));
        assert_eq!(inspected.active_agent_base, Some(AgentKind::Codex));
        assert_eq!(inspected.active_agent_pid, Some(created.pid));
        assert_eq!(
            inspected.active_agent_session_id.as_deref(),
            Some("direct-native")
        );
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn direct_agent_without_hook_does_not_auto_report_root() {
    let (registry, inspector, created) = mock_direct_codex_registry("direct-no-hook").await;
    inspector.set_descendants(created.pid, Vec::new());
    inspector.set_cwd(created.pid, temp_dir("direct-no-hook-cwd"));

    for _ in 0..DIRECT_AGENT_RESCAN_COUNT {
        registry
            .rescan_procwatch_at(&created.id, created.pid, Instant::now())
            .await;
        let inspected = registry.inspect(&created.id).await.expect("inspect");
        assert_eq!(inspected.active_agent, None);
        assert_eq!(inspected.active_agent_base, None);
        assert_eq!(inspected.active_agent_pid, None);
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_refreshes_agent_base_when_pid_is_reused() {
    let (registry, inspector, created) = mock_procwatch_registry("pid-reuse-base").await;
    inspector.set_descendants(
        created.pid,
        vec![codex_fact(PID_REUSE_AGENT_PID, created.pid)],
    );
    let first_scan = Instant::now();
    registry
        .rescan_procwatch_at(&created.id, created.pid, first_scan)
        .await;
    let first_seen = {
        let sessions = registry.inner.sessions.lock().await;
        let observed = sessions
            .get(&created.id)
            .expect("session entry")
            .observed_agents
            .iter()
            .find(|observed| observed.pid == PID_REUSE_AGENT_PID)
            .expect("observed codex pid");
        assert_eq!(observed.agent_base, AgentKind::Codex);
        observed.first_seen
    };

    inspector.set_descendants(
        created.pid,
        vec![claude_fact(PID_REUSE_AGENT_PID, created.pid)],
    );
    let second_scan = first_seen + PID_REUSE_RESCAN_DELAY;
    registry
        .rescan_procwatch_at(&created.id, created.pid, second_scan)
        .await;

    {
        let sessions = registry.inner.sessions.lock().await;
        let observed = sessions
            .get(&created.id)
            .expect("session entry")
            .observed_agents
            .iter()
            .find(|observed| observed.pid == PID_REUSE_AGENT_PID)
            .expect("observed reused pid");
        assert_eq!(observed.agent_base, AgentKind::Claude);
        assert_eq!(observed.first_seen, second_scan);
    };
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_reconciles_unbound_claim_after_descendants_error() {
    let (registry, inspector, created) = mock_procwatch_registry("ttl-descendants-error").await;
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(DIRECT_AGENT_REPORT_SEQ),
            pid: None,
            agent_session_id: Some("codex-unbound".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    let reported_at = {
        let sessions = registry.inner.sessions.lock().await;
        sessions
            .get(&created.id)
            .expect("session entry")
            .active_agent
            .as_ref()
            .expect("active report")
            .reported_at
    };

    inspector.fail_descendants_with(std::io::ErrorKind::Other);
    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            reported_at + Duration::from_millis(50),
        )
        .await;
    let cleared = registry.inspect(&created.id).await.expect("inspect");

    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    assert_eq!(cleared.active_agent_pid, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_updates_cwd_from_active_agent_pid() {
    let (registry, inspector, created) = mock_procwatch_registry("procwatch-cwd").await;
    let root_cwd = temp_dir("procwatch-root-cwd");
    let agent_cwd = temp_dir("procwatch-agent-cwd");

    inspector.set_cwd(created.pid, root_cwd.clone());
    inspector.set_descendants(
        created.pid,
        vec![codex_fact(RELEASE_MATRIX_AGENT_PID, created.pid)],
    );
    inspector.set_cwd(RELEASE_MATRIX_AGENT_PID, agent_cwd.clone());

    report_release_matrix_agent(&registry, &created).await;
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let updated = registry.inspect(&created.id).await.expect("inspect");

    assert_eq!(
        updated.cwd, agent_cwd,
        "procwatch must use the active agent pid as the cwd focus"
    );
    assert_ne!(
        updated.cwd, root_cwd,
        "root shell cwd must not win while an active agent pid is bound"
    );
    assert_eq!(updated.cwd_source, Some(CwdSource::Procwatch));

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_skips_agents_owned_by_another_daemon_or_session() {
    // Regression: a nested daemon (a test-suite loopback instance, a
    // self-hosted dev run) spawns its own agent PTYs *inside* this session's
    // process subtree. Those processes look exactly like this session's agent
    // (`comm = codex`), but their ownership markers name the other daemon —
    // adopting one hijacked the session's active agent, cwd focus, and project
    // association. Only a process carrying this session's own markers (or no
    // markers at all) may be adopted.
    let (registry, inspector, created) = mock_procwatch_registry("foreign-agent").await;
    let root_cwd = temp_dir("foreign-agent-root-cwd");
    let agent_cwd = temp_dir("foreign-agent-agent-cwd");
    inspector.set_cwd(created.pid, root_cwd.clone());
    inspector.set_descendants(
        created.pid,
        vec![codex_fact(FOREIGN_AGENT_PID, created.pid)],
    );
    inspector.set_cwd(FOREIGN_AGENT_PID, agent_cwd.clone());

    // A foreign daemon's agent: never adopted, cwd focus stays on the root.
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some("d-foreign".to_owned()),
            session_id: Some("s-1".to_owned()),
        },
    );
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let skipped = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(skipped.active_agent, None, "foreign daemon must be skipped");
    assert_eq!(skipped.active_agent_pid, None);
    assert_eq!(skipped.cwd, root_cwd, "cwd focus must stay on the root");
    let sessions = registry.inner.sessions.lock().await;
    assert!(
        sessions
            .get(&created.id)
            .expect("session entry")
            .observed_agents
            .is_empty(),
        "a foreign-owned process must not become an observed agent"
    );
    drop(sessions);

    // This daemon, but another session's agent: also skipped.
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some(registry.daemon_instance_id().to_owned()),
            session_id: Some(format!("{}-other", created.id.0)),
        },
    );
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let sibling = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        sibling.active_agent, None,
        "sibling session must be skipped"
    );

    // The session's own agent (inherited markers) is adopted as before.
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some(registry.daemon_instance_id().to_owned()),
            session_id: Some(created.id.0.clone()),
        },
    );
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let adopted = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(adopted.active_agent.as_deref(), Some("codex"));
    assert_eq!(adopted.active_agent_pid, Some(FOREIGN_AGENT_PID));
    assert_eq!(adopted.cwd, agent_cwd, "own agent drives the cwd focus");

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn external_rescan_skips_processes_marked_by_any_pohunek_daemon() {
    // A process carrying pohunek ownership markers is a PTY child of *some*
    // daemon instance — it is managed, not external, even when the owned-pid
    // tree walk cannot connect it to a local session (nested daemons, ppid
    // gaps). It must not surface as an external session.
    let inspector = Arc::new(MockInspector::default());
    let registry_inspector: Arc<dyn ProcessInspector> = Arc::<MockInspector>::clone(&inspector);
    let registry = SessionRegistry::new_with_inspector(
        SessionRegistryConfig {
            stop_grace: Duration::from_millis(50),
            ..SessionRegistryConfig::default()
        },
        registry_inspector,
    );
    inspector.set_descendants(1, vec![codex_fact(FOREIGN_AGENT_PID, 1)]);
    inspector.set_ownership_markers(
        FOREIGN_AGENT_PID,
        OwnershipMarkers {
            daemon_id: Some("d-foreign".to_owned()),
            session_id: None,
        },
    );

    registry
        .rescan_external_agents(&TranscriptIndex::default())
        .await;
    assert!(
        registry.list().await.is_empty(),
        "a marked process must not surface as an external session"
    );

    // The same process without markers is genuinely external and surfaces.
    inspector.set_ownership_markers(FOREIGN_AGENT_PID, OwnershipMarkers::default());
    registry
        .rescan_external_agents(&TranscriptIndex::default())
        .await;
    let sessions = registry.list().await;
    assert_eq!(sessions.len(), 1, "unmarked agent surfaces as external");
    assert_eq!(sessions[0].external, Some(true));
    assert_eq!(sessions[0].pid, FOREIGN_AGENT_PID);
}

#[tokio::test]
async fn active_agent_release_matrix_covers_hook_fast_path_and_procwatch_backstop() {
    let (hook_registry, _hook_inspector, hook_created) =
        mock_procwatch_registry("hook-release-matrix").await;
    report_release_matrix_agent(&hook_registry, &hook_created).await;

    let release = hook_registry
        .release_agent(SessionReleaseAgentParams {
            session_id: hook_created.id.clone(),
            source: CODEX_HOOK_SOURCE.to_owned(),
            agent: CODEX_HOOK_AGENT.to_owned(),
            seq: Some(RELEASE_MATRIX_RELEASE_SEQ),
        })
        .await;
    assert!(release.released);
    let hook_cleared = hook_registry
        .inspect(&hook_created.id)
        .await
        .expect("inspect");
    assert_eq!(hook_cleared.active_agent, None);
    assert_eq!(hook_cleared.active_agent_base, None);
    assert_eq!(hook_cleared.active_agent_pid, None);
    assert_eq!(hook_cleared.active_agent_session_id, None);
    let _ = hook_registry.stop(&hook_created.id).await;

    let (backstop_registry, backstop_inspector, backstop_created) =
        mock_procwatch_registry("procwatch-release-matrix").await;
    backstop_inspector.set_descendants(
        backstop_created.pid,
        vec![codex_fact(RELEASE_MATRIX_AGENT_PID, backstop_created.pid)],
    );
    backstop_registry
        .rescan_procwatch_at(&backstop_created.id, backstop_created.pid, Instant::now())
        .await;
    report_release_matrix_agent(&backstop_registry, &backstop_created).await;

    backstop_inspector.set_descendants(backstop_created.pid, Vec::new());
    backstop_inspector.fire_exit(RELEASE_MATRIX_AGENT_PID);
    let backstop_cleared =
        wait_for_active_agent_pid(&backstop_registry, &backstop_created.id, None).await;
    assert_eq!(backstop_cleared.active_agent, None);
    assert_eq!(backstop_cleared.active_agent_base, None);
    assert_eq!(backstop_cleared.active_agent_session_id, None);
    let _ = backstop_registry.stop(&backstop_created.id).await;
}

#[tokio::test]
async fn procwatch_releases_hook_claim_when_observed_pid_exits() {
    let (registry, inspector, created) = mock_procwatch_registry("hook-exit").await;
    inspector.set_descendants(created.pid, vec![codex_fact(100, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;

    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(10),
            pid: Some(100),
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .active_agent_pid,
        Some(100)
    );

    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(100);
    let cleared = wait_for_active_agent_pid(&registry, &created.id, None).await;

    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    assert_eq!(cleared.active_agent_session_id, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_late_hook_report_for_dead_pid_is_cleared_on_reconcile() {
    let (registry, inspector, created) = mock_procwatch_registry("late-hook").await;
    inspector.set_descendants(created.pid, vec![codex_fact(100, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(100);
    wait_for_active_agent_pid(&registry, &created.id, None).await;

    let late = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(20),
            pid: Some(100),
            agent_session_id: Some("late-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(late.recorded);

    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let cleared = registry.inspect(&created.id).await.expect("inspect");

    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_pid, None);
    assert_eq!(cleared.active_agent_session_id, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_auto_report_releases_immediately_on_sigkill_style_exit() {
    let (registry, inspector, created) = mock_procwatch_registry("sigkill").await;
    inspector.set_descendants(created.pid, vec![codex_fact(100, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let active = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(active.active_agent.as_deref(), Some("codex"));
    assert_eq!(active.active_agent_base, Some(AgentKind::Codex));
    assert_eq!(active.active_agent_pid, Some(100));

    inspector.set_descendants(created.pid, Vec::new());
    inspector.fire_exit(100);
    let cleared = wait_for_active_agent_pid(&registry, &created.id, None).await;

    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_base, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_expires_unbound_hook_claim_after_ttl() {
    let (registry, _inspector, created) = mock_procwatch_registry("ttl").await;
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(30),
            pid: None,
            agent_session_id: Some("codex-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);
    let reported_at = {
        let sessions = registry.inner.sessions.lock().await;
        sessions
            .get(&created.id)
            .expect("session entry")
            .active_agent
            .as_ref()
            .expect("active report")
            .reported_at
    };

    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            reported_at + Duration::from_millis(49),
        )
        .await;
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .active_agent_pid,
        None
    );
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect")
            .active_agent
            .as_deref(),
        Some("codex")
    );

    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            reported_at + Duration::from_millis(50),
        )
        .await;
    let cleared = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(cleared.active_agent, None);
    assert_eq!(cleared.active_agent_pid, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn procwatch_rebinds_restart_to_new_observed_pid() {
    let (registry, inspector, created) = mock_procwatch_registry("restart").await;
    inspector.set_descendants(created.pid, vec![codex_fact(100, created.pid)]);
    registry
        .rescan_procwatch_at(&created.id, created.pid, Instant::now())
        .await;
    let report = registry
        .report_agent(SessionReportAgentParams {
            session_id: created.id.clone(),
            source: "pohunek:codex".to_owned(),
            agent: "codex".to_owned(),
            activity: Some(AgentActivity::Working),
            seq: Some(40),
            pid: Some(100),
            agent_session_id: Some("old-native".to_owned()),
            agent_session_path: None,
        })
        .await;
    assert!(report.recorded);

    inspector.set_descendants(created.pid, vec![codex_fact(200, created.pid)]);
    registry
        .rescan_procwatch_at(
            &created.id,
            created.pid,
            Instant::now() + Duration::from_secs(1),
        )
        .await;
    let rebound = registry.inspect(&created.id).await.expect("inspect");

    assert_eq!(rebound.active_agent.as_deref(), Some("codex"));
    assert_eq!(rebound.active_agent_base, Some(AgentKind::Codex));
    assert_eq!(rebound.active_agent_pid, Some(200));
    assert_eq!(rebound.active_agent_session_id, None);
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_native_id_records_binding_and_updates_info() {
    let store_path = temp_store_path("report");
    let agents_dir = temp_resumable_agents_dir("report");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    assert_eq!(created.native_session_id, None);

    let result = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-abc".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(result.recorded);

    // In-memory info now carries the native id.
    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(inspected.native_session_id.as_deref(), Some("native-abc"));

    // The binding was persisted to the store.
    let persisted = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load store");
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].session_id, created.id.0);
    assert_eq!(
        persisted[0].native_session_id.as_deref(),
        Some("native-abc")
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_native_id_ignores_reports_from_a_different_agent_base() {
    let store_path = temp_store_path("report-agent-mismatch");
    let agents_dir = temp_resumable_agents_dir("report-agent-mismatch");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create claude session");

    let claude_report = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "claude-native".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(claude_report.recorded);

    let codex_report = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "codex".to_owned(),
            native_session_id: "codex-thread".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(
        !codex_report.recorded,
        "a codex hook must not overwrite a claude session binding"
    );

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        inspected.native_session_id.as_deref(),
        Some("claude-native")
    );

    let persisted = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load store");
    assert_eq!(persisted.len(), 1);
    assert_eq!(
        persisted[0].native_session_id.as_deref(),
        Some("claude-native")
    );

    let _ = registry.stop(&created.id).await;
}

/// Create a temp `agents/` dir holding one profile file; return the dir path.
fn temp_agents_dir_with(tag: &str, name: &str, body: &str) -> PathBuf {
    let dir = temp_store_path(tag)
        .parent()
        .expect("store parent")
        .join("agents");
    std::fs::create_dir_all(&dir).expect("create agents dir");
    std::fs::write(dir.join(format!("{name}.toml")), body).expect("write profile");
    dir
}

fn write_agent_manifest(dir: &std::path::Path, name: &str, body: &str) {
    let manifests = dir.join("manifests");
    std::fs::create_dir_all(&manifests).expect("create manifests dir");
    std::fs::write(manifests.join(format!("{name}.toml")), body).expect("write manifest");
}

fn temp_resumable_agents_dir(tag: &str) -> PathBuf {
    temp_agents_dir_with(
        tag,
        "resumable",
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    )
}

#[cfg(unix)]
fn temp_agent_that_exits_then_resumes(tag: &str, marker: &std::path::Path) -> PathBuf {
    let runtime = temp_dir(&format!("{tag}-runtime"));
    let script = runtime.join("resume-agent");
    write_executable(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\ncase \" $* \" in *\" --resume \"*) sleep 30 ;; *) sleep 0.2; exit 0 ;; esac\n",
                marker.display()
            ),
        );
    temp_agents_dir_with(
        tag,
        "resumable",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"sonnet\"]\n",
            script.display()
        ),
    )
}

fn resumable_params() -> SessionNewParams {
    SessionNewParams {
        name: None,
        agent: "resumable".to_owned(),
        ..params()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn fork_live_claude_session_mints_new_id_and_builds_fork_argv() {
    let dir = temp_dir("fork-claude-runtime");
    let script = dir.join("fork-agent");
    let marker = dir.join("argv.txt");
    write_executable(
        &script,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nsleep 30\n",
            marker.display()
        ),
    );
    let agents_dir = temp_agents_dir_with(
        "fork-claude",
        "forkable",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"sonnet\"]\n",
            script.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "forkable".to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create forkable session");
    assert_eq!(created.state, SessionState::Running);
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-live".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded);

    let forked = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: Some("forked review".to_owned()),
            cwd_mode: ForkCwdMode::Same,
            cols: 100,
            rows: 30,
        })
        .await
        .expect("fork live claude session");

    assert_ne!(forked.id, created.id, "fork must mint a fresh pohunek id");
    assert_eq!(forked.name.as_deref(), Some("forked review"));
    assert_eq!(forked.cwd, created.cwd);
    assert_eq!((forked.cols, forked.rows), (100, 30));
    assert_eq!(forked.native_session_id.as_deref(), Some("native-live"));

    let argv = wait_for_file_contains(&marker, "--fork-session").await;
    let lines = argv.lines().collect::<Vec<_>>();
    assert!(
        lines.windows(5).any(|window| {
            window
                == [
                    "--model",
                    "sonnet",
                    "--resume",
                    "native-live",
                    "--fork-session",
                ]
        }),
        "fork argv must preserve frozen args and append the Claude fork flag: {argv:?}"
    );

    let _ = registry.stop(&created.id).await;
    let _ = registry.stop(&forked.id).await;
}

#[tokio::test]
async fn fork_shell_session_reports_agent_not_resumable() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(params())
        .await
        .expect("create shell session");

    let err = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
        })
        .await
        .expect_err("shell sessions cannot be forked");

    assert_eq!(err.code, "agent_not_resumable");
    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn fork_codex_session_reports_agent_fork_unsupported() {
    let dir = temp_dir("fork-codex-runtime");
    let script = dir.join("codex-agent");
    write_executable(&script, "#!/bin/sh\nsleep 30\n");
    let agents_dir = temp_agents_dir_with(
        "fork-codex",
        "codex-fork",
        &format!("base = \"codex\"\nprogram = \"{}\"\n", script.display()),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            agent: "codex-fork".to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("create codex session");
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "codex".to_owned(),
            native_session_id: "codex-native".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded);

    let err = registry
        .fork(SessionForkParams {
            session_id: created.id.clone(),
            name: None,
            cwd_mode: ForkCwdMode::Same,
            cols: 80,
            rows: 24,
        })
        .await
        .expect_err("codex fork is intentionally unsupported");

    assert_eq!(err.code, "agent_fork_unsupported");
    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn report_native_id_path_profile_stores_path_and_ignores_wire_agent() {
    // The load-bearing C.3 fix: a `ref_kind = "path"` profile must store the
    // native reference into `native_session_path` (clearing `native_session_id`),
    // chosen by the FROZEN snapshot — never by the wire `agent` literal, which
    // the SessionStart hook bakes to a base-kind name carrying no profile id.
    let store_path = temp_store_path("path-profile");
    let agents_dir = temp_agents_dir_with(
            "path-profile",
            "pathy",
            "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[resume]\nref_kind = \"path\"\n",
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "pathy".to_owned(),
            ..params()
        })
        .await
        .expect("create path-profile session");

    let result = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            // Wire agent is a base-kind literal (what the hook reports); ignored
            // for ref-kind selection.
            agent: "claude".to_owned(),
            native_session_id: "opaque-native-id".to_owned(),
            transcript_path: Some("/home/u/.claude/t.jsonl".to_owned()),
        })
        .await;
    assert!(result.recorded);

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!(
        inspected.native_session_path.as_deref(),
        Some("/home/u/.claude/t.jsonl"),
        "a path-kind profile stores into native_session_path"
    );
    assert_eq!(
        inspected.native_session_id, None,
        "the id field is left empty for a path-kind session"
    );

    let persisted = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load store");
    assert_eq!(persisted.len(), 1);
    assert_eq!(
        persisted[0].native_session_path.as_deref(),
        Some("/home/u/.claude/t.jsonl")
    );
    assert_eq!(persisted[0].native_session_id, None);
    assert_eq!(persisted[0].ref_kind, Some(SessionRefKind::Path));
    assert_eq!(persisted[0].resume_mode, Some(ResumeMode::Flag));
    assert_eq!(persisted[0].program, "/bin/sh");
    assert!(persisted[0].resumable);

    registry
        .resize(&created.id, 120, 40)
        .await
        .expect("resize path-profile session");
    let resized = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load resized binding");
    assert_eq!(
        (resized[0].cols, resized[0].rows),
        (120, 40),
        "path-kind resume binding must refresh dimensions after resize"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn non_resumable_profile_ignores_native_id_reports() {
    let store_path = temp_store_path("noresume-profile");
    let agents_dir = temp_agents_dir_with(
            "noresume-profile",
            "noresume",
            "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[resume]\nresumable = false\n",
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "noresume".to_owned(),
            ..params()
        })
        .await
        .expect("create non-resumable profile session");

    let result = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "codex".to_owned(),
            native_session_id: "native-ignored".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(
        !result.recorded,
        "non-resumable profile must reject native-id reports fail-closed"
    );
    assert!(
        crate::store::Store::new(store_path)
            .load_resume()
            .expect("load")
            .is_empty(),
        "non-resumable profile must not persist a resume binding"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn non_resumable_profile_binding_reports_agent_not_resumable() {
    let registry = SessionRegistry::default();
    let binding = crate::store::ResumeBinding {
        session_id: "s-noresume".to_owned(),
        name: None,
        agent: "noresume".to_owned(),
        agent_base: AgentKind::Codex,
        cwd: temp_dir("noresume-binding-cwd"),
        cols: 80,
        rows: 24,
        native_session_id: Some("native-ignored".to_owned()),
        native_session_path: None,
        project_id: None,
        is_linked_worktree: None,
        metadata: BTreeMap::new(),
        program: "/bin/sh".to_owned(),
        args: Vec::new(),
        input_rules: crate::store::StoredInputRules::default(),
        resume_mode: Some(ResumeMode::Flag),
        ref_kind: Some(SessionRefKind::Id),
        resumable: false,
    };

    let err = registry
        .resume_binding(binding)
        .await
        .expect_err("non-resumable binding must fail");
    assert_eq!(err.code, "agent_not_resumable");
}

#[tokio::test]
async fn resume_binding_never_persists_profile_env_secrets() {
    // C.4 no-secrets invariant: a profile's `[env]` (which may hold secrets) is
    // never written to the store. The serialized resume line must contain none
    // of the env keys OR values — env is re-resolved by agent name at resume.
    let store_path = temp_store_path("env-secret");
    let agents_dir = temp_agents_dir_with(
            "env-secret",
            "withenv",
            "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n[env]\nSECRET_TOKEN = \"supersecretvalue\"\n",
        );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "withenv".to_owned(),
            ..params()
        })
        .await
        .expect("create env-profile session");
    let result = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-xyz".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(result.recorded);

    let raw = std::fs::read_to_string(&store_path).expect("read store file");
    assert!(
        !raw.contains("SECRET_TOKEN") && !raw.contains("supersecretvalue"),
        "profile env (key or value) must never reach the store: {raw}"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn stopping_a_session_drops_its_resume_binding() {
    let store_path = temp_store_path("drop-on-stop");
    let agents_dir = temp_resumable_agents_dir("drop-on-stop");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-stop".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded);
    assert_eq!(
        crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load")
            .len(),
        1
    );

    // Stopping the session must drop the binding so a restart does not
    // resurrect a session the user ended.
    let stopped = registry.stop(&created.id).await.expect("stop");
    assert!(stopped.stopped);
    assert!(
        crate::store::Store::new(store_path)
            .load_resume()
            .expect("load")
            .is_empty(),
        "stopped session must not leave a resume binding"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_harness_exit_during_daemon_shutdown_keeps_recovery_binding() {
    let store_path = temp_store_path("shutdown-keeps-binding");
    let agents_dir = temp_resumable_agents_dir("shutdown-keeps-binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-shutdown".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded, "native id captured");
    assert_eq!(
        crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load before shutdown")
            .len(),
        1,
        "precondition: captured session has one resume binding"
    );

    registry.begin_daemon_shutdown();
    registry
        .record_exit(
            &created.id,
            RuntimeExit {
                exit_code: None,
                success: false,
            },
            false,
        )
        .await;

    let persisted = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load after shutdown exit");
    let _ = registry.stop(&created.id).await;
    terminate_pid(created.pid);

    assert_eq!(
        persisted.len(),
        1,
        "a synthetic harness exit during daemon shutdown must keep recovery metadata"
    );
    assert_eq!(persisted[0].session_id, created.id.0);
    assert_eq!(
        persisted[0].native_session_id.as_deref(),
        Some("native-shutdown")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_native_recovery_from_lost_preserves_identity_emits_event_and_is_idempotent() {
    let store_path = temp_store_path("manual-resume");
    let marker = temp_dir("manual-resume-marker").join("argv.txt");
    let agents_dir = temp_agent_that_exits_then_resumes("manual-resume", &marker);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-manual".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded, "native id captured");

    let done = registry
        .wait_for_exit(&created.id, Duration::from_secs(2))
        .await
        .expect("session exits");
    assert_eq!(done.state, SessionState::Done);
    let original_created_at = done.created_at.clone();
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("terminal entry");
    entry.info.runtime = Some(SessionRuntime {
        state: RuntimeState::Lost,
        worker_id: Some("worker-before-recovery".to_owned()),
        runtime_id: Some("runtime-before-recovery".to_owned()),
        started_at: Some(original_created_at.clone()),
        last_connected_at: None,
        loss_reason: Some("test_runtime_lost".to_owned()),
    });
    entry.runtime = super::RuntimeHandle::Unavailable(RuntimeState::Lost);
    drop(sessions);
    let mut events = registry.subscribe();

    let resumed = registry
        .resume(&created.id)
        .await
        .expect("resume terminal session");
    assert_eq!(resumed.id, created.id);
    assert_eq!(resumed.created_at, original_created_at);
    assert_eq!(resumed.state, SessionState::Running);
    assert_eq!(resumed.native_session_id.as_deref(), Some("native-manual"));

    let argv = wait_for_file_contains(&marker, "native-manual").await;
    assert!(
        argv.contains("--resume") && argv.contains("native-manual"),
        "resume argv must target the captured native id: {argv:?}"
    );

    let event = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = events.recv().await.expect("receive recovery event");
            if event.event == protocol::event::SESSION_NATIVE_RECOVERED {
                break event;
            }
        }
    })
    .await
    .expect("session_native_recovered event");
    let recovered_event: SessionNativeRecoveredEvent =
        serde_json::from_value(event.payload).expect("recovery event payload");
    assert_eq!(recovered_event.session.id, created.id);
    assert_eq!(
        recovered_event.previous_runtime_id.as_deref(),
        Some("runtime-before-recovery")
    );
    // The durable-worker backend always mints a fresh runtime generation on
    // `initialize`, including for explicit native recovery, so the recovered
    // event must carry a *new* id distinct from the replaced generation.
    assert_ne!(
        recovered_event.runtime_id.as_deref(),
        Some("runtime-before-recovery"),
        "native recovery must mint a new worker runtime, not reuse the previous one"
    );
    assert!(
        recovered_event.runtime_id.is_some(),
        "native recovery must mint a fresh durable-worker runtime id"
    );

    let repeated = registry
        .resume(&created.id)
        .await
        .expect_err("live recovered session is not recoverable again");
    assert_eq!(repeated.code, "session_runtime_not_recoverable");
    assert_eq!(
        registry
            .inspect(&created.id)
            .await
            .expect("inspect after repeated recovery")
            .pid,
        resumed.pid
    );

    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_native_recovery_accepts_terminal_runtime() {
    let marker = temp_dir("terminal-recovery-marker").join("argv.txt");
    let agents_dir = temp_agent_that_exits_then_resumes("terminal-recovery", &marker);
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        socket_path: Some(PathBuf::from("/run/pohunek/d.sock")),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    assert!(
        registry
            .report_native_id(SessionReportNativeIdParams {
                session_id: created.id.clone(),
                agent: "claude".to_owned(),
                native_session_id: "native-terminal".to_owned(),
                transcript_path: None,
            })
            .await
            .recorded
    );
    let terminal = registry
        .wait_for_exit(&created.id, Duration::from_secs(2))
        .await
        .expect("session exits");
    assert_eq!(terminal.state, SessionState::Done);

    let recovered = registry
        .resume(&created.id)
        .await
        .expect("terminal runtime is eligible for native recovery");
    assert_eq!(recovered.id, created.id);
    assert_eq!(recovered.created_at, created.created_at);
    assert_eq!(recovered.state, SessionState::Running);
    assert!(wait_for_file_contains(&marker, "native-terminal")
        .await
        .contains("--resume"));

    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_native_recovery_rejects_nonterminal_runtime_states() {
    let agents_dir = temp_resumable_agents_dir("recovery-preconditions");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(resumable_params())
        .await
        .expect("create resumable session");

    for state in [
        RuntimeState::Starting,
        RuntimeState::Live,
        RuntimeState::Reconnecting,
        RuntimeState::Conflict,
        RuntimeState::Incompatible,
    ] {
        let mut sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get_mut(&created.id).expect("live entry");
        entry.info.runtime = Some(SessionRuntime {
            state,
            worker_id: Some("worker-live".to_owned()),
            runtime_id: Some("runtime-live".to_owned()),
            started_at: None,
            last_connected_at: None,
            loss_reason: None,
        });
        drop(sessions);
        let error = registry
            .resume(&created.id)
            .await
            .expect_err("nonterminal runtime must reject native recovery");
        assert_eq!(
            error.code, "session_runtime_not_recoverable",
            "unexpected error for {state:?}"
        );
    }

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn native_recovered_event_carries_previous_and_new_runtime_ids() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });
    let mut session = registry.create(params()).await.expect("create session");
    session.runtime = Some(SessionRuntime {
        state: RuntimeState::Live,
        worker_id: Some("worker-new".to_owned()),
        runtime_id: Some("runtime-new".to_owned()),
        started_at: Some(session.created_at.clone()),
        last_connected_at: Some(session.updated_at.clone()),
        loss_reason: None,
    });
    let mut events = registry.subscribe();

    registry.emit_native_recovered(&session, Some("runtime-old".to_owned()));

    let event = events.recv().await.expect("native recovery event");
    assert_eq!(event.event, protocol::event::SESSION_NATIVE_RECOVERED);
    let payload: SessionNativeRecoveredEvent =
        serde_json::from_value(event.payload).expect("recovery payload");
    assert_eq!(payload.session.id, session.id);
    assert_eq!(payload.previous_runtime_id.as_deref(), Some("runtime-old"));
    assert_eq!(payload.runtime_id.as_deref(), Some("runtime-new"));

    let _ = registry.stop(&session.id).await;
}

#[tokio::test]
async fn resize_after_capture_updates_persisted_binding() {
    let store_path = temp_store_path("resize-binding");
    let agents_dir = temp_resumable_agents_dir("resize-binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    // Capture a native id so a resume binding exists at the launch size.
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-resize".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded);
    let before = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load before");
    assert_eq!(before.len(), 1);
    assert_eq!((before[0].cols, before[0].rows), (80, 24));

    // Resizing the live session must refresh the persisted dimensions so a
    // restart resumes at the new size, not the stale capture-time size.
    registry
        .resize(&created.id, 132, 50)
        .await
        .expect("resize session");

    let after = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load after");
    assert_eq!(
        after.len(),
        1,
        "resize must upsert, not duplicate: {after:?}"
    );
    assert_eq!(after[0].session_id, created.id.0);
    assert_eq!(after[0].native_session_id.as_deref(), Some("native-resize"));
    assert_eq!(
        (after[0].cols, after[0].rows),
        (132, 50),
        "persisted binding must carry the post-resize dimensions"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn set_metadata_after_capture_updates_persisted_binding() {
    let store_path = temp_store_path("metadata-binding");
    let agents_dir = temp_resumable_agents_dir("metadata-binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let created = registry
        .create(SessionNewParams {
            metadata: metadata(&[("owner", "cli"), ("ticket", "old")]),
            ..resumable_params()
        })
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-metadata".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded);

    let expected = metadata(&[("owner", "daemon"), ("reviewer", "qa"), ("ticket", "old")]);
    registry
        .set_metadata(
            &created.id,
            metadata_patch(&[("owner", Some("daemon")), ("reviewer", Some("qa"))]),
        )
        .await
        .expect("set metadata after capture");

    let persisted = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load binding");
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].session_id, created.id.0);
    assert_eq!(persisted[0].metadata, expected);

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn resume_binding_restores_metadata_from_store() {
    let store_path = temp_store_path("resume-metadata");
    let expected = metadata(&[("owner", "daemon"), ("ticket", "DMD-1356")]);
    let store = crate::store::Store::new(store_path.clone());
    store
        .record_resume(&crate::store::ResumeBinding {
            session_id: "s-42".to_owned(),
            name: None,
            agent: "claude".to_owned(),
            agent_base: AgentKind::Claude,
            cwd: temp_dir("resume-metadata-cwd"),
            cols: 80,
            rows: 24,
            native_session_id: Some("native-metadata".to_owned()),
            native_session_path: None,
            project_id: None,
            is_linked_worktree: None,
            metadata: expected.clone(),
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
            input_rules: crate::store::StoredInputRules::default(),
            resume_mode: Some(ResumeMode::Flag),
            ref_kind: Some(SessionRefKind::Id),
            resumable: true,
        })
        .expect("seed resume binding");
    let binding = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load resume binding")
        .into_iter()
        .next()
        .expect("one binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path),
        ..SessionRegistryConfig::default()
    });

    let resumed = registry
        .resume_binding(binding)
        .await
        .expect("resume binding");

    assert_eq!(resumed.metadata, expected);
    assert_eq!(
        registry
            .inspect(&resumed.id)
            .await
            .expect("inspect resumed")
            .metadata,
        expected
    );

    let _ = registry.stop(&resumed.id).await;
}

#[tokio::test]
async fn resize_without_captured_native_id_persists_no_binding() {
    let store_path = temp_store_path("resize-no-binding");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        ..SessionRegistryConfig::default()
    });

    let created = registry.create(params()).await.expect("create session");
    // No native id captured yet: resizing must not fabricate an unusable
    // recovery binding.
    registry
        .resize(&created.id, 100, 30)
        .await
        .expect("resize session");

    assert!(
        crate::store::Store::new(store_path)
            .load_resume()
            .expect("load")
            .is_empty(),
        "resize without a native id must not create a resume binding"
    );

    let _ = registry.stop(&created.id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn resume_after_profile_edit_and_resize_uses_original_snapshot() {
    let store_path = temp_store_path("resume-edit-resize");
    let dir = temp_dir("resume-edit-resize-runtime");
    let script_v1 = dir.join("agent-v1");
    let script_v2 = dir.join("agent-v2");
    let marker_v1 = dir.join("v1-argv.txt");
    let marker_v2 = dir.join("v2-argv.txt");
    write_resume_agent_script(&script_v1, &marker_v1);
    write_resume_agent_script(&script_v2, &marker_v2);
    let agents_dir = temp_agents_dir_with(
        "resume-edit-resize",
        "editable",
        &format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"sonnet\"]\n",
            script_v1.display()
        ),
    );
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir.clone()),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(SessionNewParams {
            agent: "editable".to_owned(),
            name: None,
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("create editable profile session");
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-edit-resize".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded);
    registry
        .resize(&created.id, 123, 55)
        .await
        .expect("resize captured session");
    let binding = crate::store::Store::new(store_path.clone())
        .load_resume()
        .expect("load resized binding")
        .into_iter()
        .next()
        .expect("resume binding exists");
    assert_eq!((binding.cols, binding.rows), (123, 55));

    registry
        .stop(&created.id)
        .await
        .expect("stop original session after snapshot capture");
    fs::write(&marker_v1, "").expect("clear v1 marker");
    fs::write(
        agents_dir.join("editable.toml"),
        format!(
            "base = \"claude\"\nprogram = \"{}\"\nargs = [\"--model\", \"opus\"]\n",
            script_v2.display()
        ),
    )
    .expect("edit profile");

    let restarted = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let resumed = restarted
        .resume_binding(binding)
        .await
        .expect("resume from frozen binding");

    assert_eq!((resumed.cols, resumed.rows), (123, 55));
    let argv = wait_for_file_contains(&marker_v1, "native-edit-resize").await;
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        vec!["--model", "sonnet", "--resume", "native-edit-resize"],
        "resume must use launch-time program/args, not the edited profile"
    );
    assert!(
        !marker_v2.exists()
            || fs::read_to_string(&marker_v2)
                .unwrap_or_default()
                .is_empty(),
        "edited profile program must not run during resume"
    );

    let _ = restarted.stop(&resumed.id).await;
}

#[tokio::test]
async fn resume_binding_persists_project_context_for_restart() {
    // F5: a resumed session's project context is restored from the persisted
    // binding, not re-detected. So the binding must carry `project_id` /
    // `is_linked_worktree` captured from the live session — verified here by
    // round-tripping through the store (record on native-id capture, read back
    // as explicit native-recovery metadata).
    let store = temp_store_path("resume-project-ctx");
    let worktree_root = store.parent().expect("store parent").join("worktrees");
    let agents_dir = temp_resumable_agents_dir("resume-project-ctx");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        store_path: Some(store),
        worktree_root: Some(worktree_root),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });
    let repo = init_git_repo("resume-project-ctx");
    let info = registry
        .create(SessionNewParams {
            cwd: Some(repo.clone()),
            ..resumable_params()
        })
        .await
        .expect("in-place session in the repo");
    let project_id = info.project_id.clone().expect("a project was stamped");
    assert_eq!(info.is_linked_worktree, Some(false), "the main checkout");

    // Capturing the native id persists the resume binding from live state.
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: info.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-resume".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded, "native id captured");

    let bindings = registry
        .projects()
        .expect("projects")
        .store()
        .load_resume()
        .expect("load resume bindings");
    assert_eq!(bindings.len(), 1, "exactly one resume binding persisted");
    assert_eq!(
        bindings[0].project_id.as_deref(),
        Some(project_id.as_str()),
        "project id is persisted so restart restores it without re-detecting"
    );
    assert_eq!(
        bindings[0].is_linked_worktree,
        Some(false),
        "the main-checkout flag is persisted too"
    );
}

#[tokio::test]
async fn concurrent_resize_and_recapture_keep_store_consistent_with_memory() {
    let store_path = temp_store_path("concurrent-persist");
    let agents_dir = temp_resumable_agents_dir("concurrent-persist");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-concurrent".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded);

    // Race a resize against a second native-id report (SessionStart re-fires
    // on resume/clear/compact). The persisted binding must end at the live
    // size, never the pre-resize one: persist_resume_binding re-reads under
    // persist_lock, so whichever writer runs last reflects the resize.
    let resizer = {
        let registry = registry.clone();
        let id = created.id.clone();
        tokio::spawn(async move { registry.resize(&id, 200, 60).await })
    };
    let recapture = {
        let registry = registry.clone();
        let id = created.id.clone();
        tokio::spawn(async move {
            registry
                .report_native_id(SessionReportNativeIdParams {
                    session_id: id,
                    agent: "claude".to_owned(),
                    native_session_id: "native-concurrent".to_owned(),
                    transcript_path: None,
                })
                .await
        })
    };
    resizer
        .await
        .expect("resize task")
        .expect("resize succeeds");
    recapture.await.expect("recapture task");

    let inspected = registry.inspect(&created.id).await.expect("inspect");
    assert_eq!((inspected.cols, inspected.rows), (200, 60));
    let persisted = crate::store::Store::new(store_path)
        .load_resume()
        .expect("load");
    assert_eq!(persisted.len(), 1, "no duplicate binding: {persisted:?}");
    assert_eq!(
        (persisted[0].cols, persisted[0].rows),
        (inspected.cols, inspected.rows),
        "persisted binding must match the live size after a concurrent resize + recapture"
    );

    let _ = registry.stop(&created.id).await;
}

#[tokio::test]
async fn resize_then_stop_leaves_no_binding() {
    let store_path = temp_store_path("resize-then-stop");
    let agents_dir = temp_resumable_agents_dir("resize-then-stop");
    let registry = SessionRegistry::new(SessionRegistryConfig {
        stop_grace: Duration::from_millis(50),
        store_path: Some(store_path.clone()),
        agents_dir: Some(agents_dir),
        ..SessionRegistryConfig::default()
    });

    let created = registry
        .create(resumable_params())
        .await
        .expect("create session");
    let recorded = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "claude".to_owned(),
            native_session_id: "native-resize-stop".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(recorded.recorded);
    registry
        .resize(&created.id, 90, 30)
        .await
        .expect("resize session");
    assert_eq!(
        crate::store::Store::new(store_path.clone())
            .load_resume()
            .expect("load")
            .len(),
        1,
        "resize must refresh the existing binding"
    );

    // Stopping after a resize must still drop the (resize-refreshed) binding.
    let stopped = registry.stop(&created.id).await.expect("stop");
    assert!(stopped.stopped);
    assert!(
        crate::store::Store::new(store_path)
            .load_resume()
            .expect("load")
            .is_empty(),
        "a resized-then-stopped session must not leave a resume binding"
    );
}

#[tokio::test]
async fn report_native_id_ignores_unknown_invalid_and_terminal() {
    let registry = SessionRegistry::new(SessionRegistryConfig {
        shell_command: ShellCommand::new("/bin/sh", ["-c", "sleep 30"]),
        stop_grace: Duration::from_millis(50),
        ..SessionRegistryConfig::default()
    });

    // Unknown session id.
    let unknown = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: SessionId("s-missing".to_owned()),
            agent: "claude".to_owned(),
            native_session_id: "native-1".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(!unknown.recorded);

    let created = registry.create(params()).await.expect("create session");

    // Invalid (empty) native id on a live session.
    let invalid = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "shell".to_owned(),
            native_session_id: String::new(),
            transcript_path: None,
        })
        .await;
    assert!(!invalid.recorded);

    // Terminal session.
    let _ = registry.stop(&created.id).await;
    let terminal = registry
        .report_native_id(SessionReportNativeIdParams {
            session_id: created.id.clone(),
            agent: "shell".to_owned(),
            native_session_id: "native-late".to_owned(),
            transcript_path: None,
        })
        .await;
    assert!(!terminal.recorded);
}

#[test]
fn claude_input_rules_use_configured_submit_delay() {
    let config = SessionRegistryConfig {
        claude_submit_delay: Duration::from_millis(75),
        ..SessionRegistryConfig::default()
    };

    let rules = super::input_rules_for_agent(AgentKind::Claude, &config);

    assert!(!rules.bracketed_paste);
    assert_eq!(rules.submit_delay, Duration::from_millis(75));
}

// ---------------------------------------------------------------------------
// session.diff
// ---------------------------------------------------------------------------
//
// The git-diff computation matrix below drives `diff::compute_session_diff`
// directly against a plain fixture repo (no live session/registry needed —
// it is a pure read over a worktree path). The base-precedence, hostile-ref,
// no-worktree, and unresolved-base tests drive the real `SessionRegistry::diff`
// entry point, reusing `project_registry`/`init_git_repo`/`git_in`/`params`.

#[test]
fn session_diff_modified_tracked_file_appears_as_a_change() {
    let repo = init_git_repo("diff-modified");
    std::fs::write(repo.join("README.md"), "modified\n").expect("modify tracked file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert_eq!(result.base, "main");
    assert!(result.diff.contains("diff --git a/README.md b/README.md"));
    assert!(result.diff.contains("-init"));
    assert!(result.diff.contains("+modified"));
}

#[test]
fn session_diff_added_tracked_file_reflects_unstaged_edits_made_after_staging() {
    // `git diff <commit>` compares the commit tree directly to the working
    // tree (bypassing the index), so a staged-then-further-edited new file
    // must show its final, unstaged content — not just the staged blob.
    let repo = init_git_repo("diff-added");
    std::fs::write(repo.join("added.txt"), "staged content\n").expect("write added file");
    git_in(&repo, &["add", "added.txt"]);
    std::fs::write(
        repo.join("added.txt"),
        "staged content\nunstaged extra line\n",
    )
    .expect("unstaged edit atop the staged add");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result.diff.contains("new file mode"));
    assert!(result.diff.contains("+staged content"));
    assert!(
        result.diff.contains("+unstaged extra line"),
        "git diff <commit> must reflect the working tree, not just the staged blob: {}",
        result.diff
    );
}

#[test]
fn session_diff_deleted_tracked_file_appears_as_a_deletion() {
    let repo = init_git_repo("diff-deleted");
    std::fs::remove_file(repo.join("README.md")).expect("delete tracked file from disk");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result.diff.contains("deleted file mode"));
    assert!(result.diff.contains("-init"));
}

#[test]
fn session_diff_renamed_tracked_file_with_an_edit_is_detected_as_a_rename() {
    let repo = init_git_repo("diff-renamed");
    // A single-line file renamed and edited has too little byte overlap with
    // its original to clear git's default 50% rename-similarity threshold;
    // commit enough shared content first so the rename is actually detected.
    std::fs::write(
        repo.join("README.md"),
        "line one\nline two\nline three\nline four\nline five\n",
    )
    .expect("write substantial tracked content");
    git_in(&repo, &["add", "README.md"]);
    git_in(&repo, &["commit", "-q", "-m", "more content"]);

    git_in(&repo, &["mv", "README.md", "renamed.md"]);
    std::fs::write(
        repo.join("renamed.md"),
        "line one\nline two\nline three\nline four\nline five\nline six\n",
    )
    .expect("edit the renamed file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result.diff.contains("rename from README.md"));
    assert!(result.diff.contains("rename to renamed.md"));
}

#[test]
fn session_diff_untracked_text_file_appears_as_an_added_file_diff() {
    let repo = init_git_repo("diff-untracked-text");
    std::fs::write(repo.join("new.txt"), "hello\n").expect("write untracked file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result.diff.contains("diff --git a/new.txt b/new.txt"));
    assert!(result.diff.contains("new file mode"));
    assert!(result.diff.contains("+hello"));
}

#[test]
fn session_diff_binary_tracked_file_change_shows_gits_binary_stanza() {
    let repo = init_git_repo("diff-binary-tracked");
    std::fs::write(repo.join("bin.dat"), [0u8, 1, 2, 3, 255, 254]).expect("write binary file");
    git_in(&repo, &["add", "bin.dat"]);
    git_in(&repo, &["commit", "-q", "-m", "add binary"]);
    std::fs::write(repo.join("bin.dat"), [0u8, 9, 9, 9, 255, 254]).expect("modify binary file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result
        .diff
        .contains("Binary files a/bin.dat and b/bin.dat differ"));
}

#[test]
fn session_diff_untracked_binary_file_shows_gits_binary_stanza() {
    let repo = init_git_repo("diff-binary-untracked");
    std::fs::write(repo.join("new.bin"), [0u8, 1, 2, 255]).expect("write untracked binary file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(!result.truncated);
    assert!(result
        .diff
        .contains("Binary files /dev/null and b/new.bin differ"));
}

#[test]
fn cap_to_budget_stops_at_a_file_boundary_once_the_cap_is_exceeded() {
    // Two synthetic "files" shaped like real `diff --git` chunks: the first
    // fits comfortably under the cap, the second alone is already far over it.
    let first = format!("diff --git a/first b/first\n{}\n", "x".repeat(100));
    let second = format!(
        "diff --git a/second b/second\n{}\n",
        "y".repeat(protocol::MAX_SESSION_DIFF_BYTES)
    );
    let combined = format!("{first}{second}");

    let (capped, truncated) = super::diff::cap_to_budget(&combined);

    assert!(truncated);
    assert_eq!(
        capped, first,
        "must include the first file whole and stop exactly at the next file boundary"
    );
}

#[test]
fn session_diff_truncates_a_large_untracked_file_and_keeps_the_response_envelope_within_control_line_bytes(
) {
    let repo = init_git_repo("diff-truncate");
    // Comfortably over the cap once rendered as unified-diff `+` content; JSON
    // escaping only adds overhead on top of the raw byte count, never removes
    // it, so this is guaranteed to exceed `MAX_SESSION_DIFF_BYTES` once diffed.
    let huge = "x".repeat(protocol::MAX_SESSION_DIFF_BYTES + 4096);
    std::fs::write(repo.join("huge.txt"), &huge).expect("write huge untracked file");

    let result = super::diff::compute_session_diff(&repo, "main").expect("diff succeeds");

    assert!(
        result.truncated,
        "a file exceeding the cap on its own must truncate"
    );
    assert!(
        result.diff.is_empty(),
        "the only file did not fit at all, so nothing is included: {} bytes",
        result.diff.len()
    );

    let response = protocol::Response::ok(
        "req-1".to_owned(),
        serde_json::to_value(&result).expect("serialize SessionDiffResult"),
    );
    let serialized = serde_json::to_string(&response).expect("serialize Response envelope");
    assert!(
        serialized.len() < protocol::MAX_CONTROL_LINE_BYTES,
        "a truncated envelope must still fit one control line: {} bytes",
        serialized.len()
    );
}

#[test]
fn resolve_base_prefers_the_explicit_param_over_everything_else() {
    let resolved = super::diff::resolve_base(Some("explicit-ref".to_owned()), None, "s-none", None)
        .expect("an explicit base short-circuits before touching the store or a repository");

    assert_eq!(resolved, "explicit-ref");
}

#[test]
fn resolve_base_falls_back_to_the_repository_default_branch_when_no_store_or_binding_exists() {
    let repo = init_git_repo("diff-default-fallback");

    let resolved = super::diff::resolve_base(None, None, "s-none", Some(&repo))
        .expect("the repository's current branch resolves");

    assert_eq!(resolved, "main");
}

#[tokio::test]
async fn session_diff_session_without_a_worktree_fails_with_a_typed_error() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());
    let info = registry
        .create(params())
        .await
        .expect("plain session is created");

    let err = registry
        .diff(&info.id, None)
        .await
        .expect_err("a session with no worktree must fail session.diff");

    assert_eq!(err.code, "session_no_worktree");
    assert!(err.recover.is_some(), "must carry a recover hint: {err:?}");
}

#[tokio::test]
async fn session_diff_rejects_an_empty_explicit_base() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());

    let err = registry
        .diff(
            &SessionId("s-does-not-matter".to_owned()),
            Some(String::new()),
        )
        .await
        .expect_err("an empty base must be rejected before the session is even looked up");

    assert_eq!(err.code, "invalid_branch");
}

#[tokio::test]
async fn session_diff_rejects_a_dash_leading_explicit_base() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());

    let err = registry
        .diff(
            &SessionId("s-does-not-matter".to_owned()),
            Some("--upload-pack=evil".to_owned()),
        )
        .await
        .expect_err("a dash-leading base must be rejected as a possible argv flag injection");

    assert_eq!(err.code, "invalid_branch");
}

#[tokio::test]
async fn session_diff_rejects_a_control_character_in_the_explicit_base() {
    let registry = SessionRegistry::new(SessionRegistryConfig::default());

    let err = registry
        .diff(
            &SessionId("s-does-not-matter".to_owned()),
            Some("feat/x\u{7}".to_owned()),
        )
        .await
        .expect_err("a control character in the base must be rejected");

    assert_eq!(err.code, "invalid_branch");
}

#[tokio::test]
async fn session_diff_explicit_base_overrides_the_recorded_worktree_binding() {
    let (registry, repo) = project_registry("diff-explicit-base");
    git_in(&repo, &["branch", "release"]);

    let info = registry
        .create(SessionNewParams {
            repo: Some(repo.clone()),
            branch: Some("feat/explicit-base".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created");

    let result = registry
        .diff(&info.id, Some("release".to_owned()))
        .await
        .expect("diff succeeds with an explicit base");

    assert_eq!(result.base, "release");
}

#[tokio::test]
async fn session_diff_falls_back_to_the_recorded_worktree_base_branch_when_no_explicit_base_given()
{
    let (registry, repo) = project_registry("diff-recorded-base");
    git_in(&repo, &["branch", "release"]);

    let info = registry
        .create(SessionNewParams {
            repo: Some(repo.clone()),
            branch: Some("feat/recorded-base".to_owned()),
            base_branch: Some("release".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created with an explicit base branch");

    let result = registry
        .diff(&info.id, None)
        .await
        .expect("diff succeeds using the recorded base branch");

    assert_eq!(
        result.base, "release",
        "must use the worktree binding's recorded base branch, not the repository's current branch"
    );
}

#[tokio::test]
async fn session_diff_reports_a_typed_error_when_the_base_ref_does_not_resolve() {
    let (registry, repo) = project_registry("diff-bad-base");
    let info = registry
        .create(SessionNewParams {
            repo: Some(repo.clone()),
            branch: Some("feat/bad-base".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created");

    let err = registry
        .diff(
            &info.id,
            Some("totally-bogus-ref-that-does-not-exist".to_owned()),
        )
        .await
        .expect_err("an unresolvable base ref must fail, not silently succeed with an empty diff");

    assert_eq!(err.code, "session_diff_base_unresolved");
    assert!(!err.msg.is_empty());
}

#[tokio::test]
async fn session_diff_registry_end_to_end_reflects_worktree_changes_against_the_repository_default_base(
) {
    let (registry, repo) = project_registry("diff-e2e");
    let info = registry
        .create(SessionNewParams {
            repo: Some(repo.clone()),
            branch: Some("feat/e2e".to_owned()),
            ..params()
        })
        .await
        .expect("worktree-bound session is created");
    let worktree = info
        .worktree_path
        .clone()
        .expect("session must be worktree-bound for this test");

    std::fs::write(worktree.join("README.md"), "changed in the worktree\n")
        .expect("modify the tracked file inside the bound worktree");

    let result = registry.diff(&info.id, None).await.expect("diff succeeds");

    assert_eq!(result.base, "main");
    assert!(!result.truncated);
    assert!(result.diff.contains("README.md"));
    assert!(result.diff.contains("+changed in the worktree"));
}
