//! Guard against persisted-record shape changes that skip a schema bump.
//!
//! The recursive key set of a fully populated instance of every persisted
//! record kind (including the nested protocol and launch types they embed) is
//! compared with the checked-in snapshot `fixtures/shape/schema-<N>.txt` for the
//! current [`STORE_SCHEMA_VERSION`]. Optional fields are populated in the
//! samples because `skip_serializing_if` hides an unset field from the output.
//!
//! When a record's shape changes the test fails. To resolve it:
//!
//! 1. bump `STORE_SCHEMA_VERSION`,
//! 2. add a `MIGRATIONS` step for the previous schema,
//! 3. add `fixtures/shape/schema-<new>.txt` with the key set the failure prints
//!    and register it in `SHAPE_SNAPSHOTS`.
//!
//! Every sample is destructured field by field (no `..`), so adding a field to a
//! persisted struct fails to compile here until the sample and snapshot are
//! revisited. Types with private fields ([`NativeSessionLaunch`] and its
//! components) cannot be destructured; their samples are populated through the
//! full JSON form instead.
//!
//! [`NativeSessionLaunch`]: crate::agent::NativeSessionLaunch

use std::collections::BTreeSet;

use protocol::SessionInfo;
use serde_json::{json, Value};

use super::schema::{MIGRATIONS, STORE_SCHEMA_VERSION};
use super::{
    NativeIdentityOrdering, ProjectRecord, Record, ResumeBinding, RuntimeRecord, SessionRecord,
    SessionTransaction, StoredInputRules, WorktreeBinding,
};

/// Key-set snapshots by schema version. A new schema adds its entry here.
const SHAPE_SNAPSHOTS: &[(u32, &str)] = &[
    (2, include_str!("fixtures/shape/schema-2.txt")),
    (3, include_str!("fixtures/shape/schema-3.txt")),
    (4, include_str!("fixtures/shape/schema-4.txt")),
    (5, include_str!("fixtures/shape/schema-5.txt")),
];

/// A profile revision: 64 lowercase hex digits.
const REVISION: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Resume binding whose launch pin is a built-in runtime and whose reference was
/// assigned with an unchecked existence declaration.
fn resume_builtin_json() -> Value {
    json!({
        "session_id": "s-1",
        "name": "named",
        "agent": "claude",
        "agent_base": "claude",
        "cwd": "/work",
        "cols": 120,
        "rows": 40,
        "native_session_id": "native-1",
        "native_session_path": "/work/native.jsonl",
        "project_id": "p-1",
        "is_linked_worktree": true,
        "metadata": {"key": "value"},
        "program": "claude",
        "args": ["--flag"],
        "input_rules": {"bracketed_paste": true, "submit_delay_ms": 5, "restricted": true},
        "native_launch_unresolved": true,
        "native_launch": {
            "reference_kind": "id",
            "resume_args": [{"literal": "--resume"}, "reference"],
            "fork_args": [{"literal": "--fork"}, "reference"],
            "assigned": {
                "launch_args": [{"literal": "--session-id"}, "reference"],
                "existence": {"check": "none"}
            }
        },
        "launch_binding": {
            "runtime_id": "claude",
            "provenance": {
                "kind": "builtin",
                "package": {"id": "acme.runtime", "version": "1.0.0"},
                "descriptor_digest": DIGEST
            }
        },
        "native_reference_provenance": "assigned",
        "profile_revision": REVISION
    })
}

/// Resume binding whose launch pin is an installed package and whose assigned
/// reference declares a file existence check.
fn resume_package_json() -> Value {
    let mut value = resume_builtin_json();
    value["native_launch"]["assigned"]["existence"] = json!({
        "check": "file",
        "root_env": "AGENT_CONFIG_DIR",
        "root_home": ".agent",
        "dir": "sessions",
        "file_name": "_{reference}.jsonl",
        "name_match": "ends_with",
        "max_depth": 1
    });
    value["launch_binding"]["provenance"] = json!({
        "kind": "package",
        "package": {"id": "acme.runtime", "version": "1.0.0"},
        "package_digest": DIGEST
    });
    value
}

fn worktree_json() -> Value {
    json!({
        "session_id": "s-1",
        "repository": "/repo",
        "branch": "feat/x",
        "base_branch": "main",
        "branch_slug": "x",
        "path": "/data/worktrees/s-1",
        "agent": "claude",
        "status": "active",
        "project_id": "p-1",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:01Z"
    })
}

fn project_json() -> Value {
    json!({
        "git_common_dir": "/repo/.git",
        "repo_root": "/repo",
        "custom_name": "repo",
        "origin_url": "https://example.com/repo.git",
        "default_base_branch": "main",
        "is_bare": true,
        "source": "manual",
        "added_at": "2026-01-01T00:00:00Z",
        "last_used_at": "2026-01-01T00:00:01Z"
    })
}

fn session_json() -> Value {
    json!({
        "schema_version": STORE_SCHEMA_VERSION,
        "session_id": "s-1",
        "desired_state": "running",
        "transaction": {
            "id": "tx-1",
            "kind": "recover",
            "phase": "launching",
            "previous_worker_id": "w-0",
            "previous_runtime_id": "r-0",
            "daemon_instance_id": "d-1"
        },
        "info": {
            "id": "s-1",
            "external": false,
            "capabilities": {"resume": true, "fork": true},
            "name": "named",
            "agent": "claude",
            "agent_base": "claude",
            "cwd": "/work",
            "cwd_source": "osc7",
            "pid": 4242,
            "runtime": {
                "state": "live",
                "runtime_generation": "1",
                "worker_id": "w-1",
                "worker_instance_id": "wi-1",
                "started_at": "2026-01-01T00:00:00Z",
                "last_connected_at": "2026-01-01T00:00:01Z",
                "loss_reason": "none"
            },
            "cols": 120,
            "rows": 40,
            "state": "running",
            "state_source": "process",
            "activity": "working",
            "subagents": [{
                "id": "sub-1",
                "parent_id": "sub-0",
                "provider": "claude",
                "agent_type": "explore",
                "lifecycle": "running",
                "activity": "idle",
                "revision": "1",
                "started_at_ms": 1,
                "updated_at_ms": 2,
                "finished_at_ms": 3
            }],
            "active_agent": "claude",
            "active_agent_base": "claude",
            "active_agent_pid": 4243,
            "active_agent_session_id": "native-1",
            "active_agent_session_path": "/work/native.jsonl",
            "native_session_id": "native-1",
            "native_session_path": "/work/native.jsonl",
            "native_last_activity_at": "2026-01-01T00:00:01Z",
            "project_id": "p-1",
            "project_label": "repo",
            "is_linked_worktree": true,
            "repo": "/repo",
            "branch": "feat/x",
            "worktree_path": "/data/worktrees/s-1",
            "warnings": [{"kind": "fetch", "message": "fetch failed", "detail": "offline"}],
            "metadata": {"key": "value"},
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:01Z",
            "exit_code": 0
        },
        "recovery": resume_builtin_json(),
        "native_identity_ordering": {
            "runtime_id": "r-1",
            "pid": 4242,
            "pid_start_identity": 7,
            "sequence": 9,
            "worker_sequence": 11
        },
        "runtime": {
            "state": "live",
            "worker_id": "w-1",
            "runtime_id": "r-1",
            "service_id": "s-1.g1",
            "generation": "g1",
            "executable": "/opt/pohunek/pohunek-sessiond",
            "reason": "none"
        }
    })
}

/// Fails to compile when a persisted struct gains, loses or renames a field.
#[expect(
    clippy::unneeded_field_pattern,
    clippy::too_many_lines,
    reason = "naming every field with `_` is what makes a field change a compile error"
)]
fn assert_every_field_is_reviewed(
    resume: &[ResumeBinding],
    worktree: &WorktreeBinding,
    project: &ProjectRecord,
    session: &SessionRecord,
) {
    for binding in resume {
        let ResumeBinding {
            session_id: _,
            name: _,
            agent: _,
            agent_base: _,
            cwd: _,
            cols: _,
            rows: _,
            native_session_id: _,
            native_session_path: _,
            project_id: _,
            is_linked_worktree: _,
            metadata: _,
            program: _,
            args: _,
            input_rules,
            native_launch: _,
            native_launch_unresolved: _,
            launch_binding: _,
            native_reference_provenance: _,
            profile_revision: _,
        } = binding;
        let StoredInputRules {
            bracketed_paste: _,
            submit_delay_ms: _,
            restricted: _,
        } = input_rules;
    }
    let WorktreeBinding {
        session_id: _,
        repository: _,
        branch: _,
        base_branch: _,
        branch_slug: _,
        path: _,
        agent: _,
        status: _,
        project_id: _,
        created_at: _,
        updated_at: _,
    } = worktree;
    let ProjectRecord {
        git_common_dir: _,
        repo_root: _,
        custom_name: _,
        origin_url: _,
        default_base_branch: _,
        is_bare: _,
        source: _,
        added_at: _,
        last_used_at: _,
    } = project;
    let SessionRecord {
        schema_version: _,
        session_id: _,
        desired_state: _,
        transaction,
        info,
        recovery: _,
        native_identity_ordering,
        runtime,
    } = session;
    if let Some(SessionTransaction {
        id: _,
        kind: _,
        phase: _,
        previous_worker_id: _,
        previous_worker_instance_id: _,
        daemon_instance_id: _,
    }) = transaction
    {}
    if let Some(NativeIdentityOrdering {
        worker_instance_id: _,
        pid: _,
        pid_start_identity: _,
        sequence: _,
        worker_sequence: _,
    }) = native_identity_ordering
    {}
    let RuntimeRecord {
        state: _,
        worker_id: _,
        worker_instance_id: _,
        service_id: _,
        generation: _,
        executable: _,
        reason: _,
    } = runtime;
    let SessionInfo {
        id: _,
        external: _,
        capabilities,
        name: _,
        agent: _,
        agent_base: _,
        cwd: _,
        cwd_source: _,
        pid: _,
        runtime: session_runtime,
        cols: _,
        rows: _,
        state: _,
        state_source: _,
        activity: _,
        subagents,
        active_agent: _,
        active_agent_base: _,
        active_agent_pid: _,
        active_agent_session_id: _,
        active_agent_session_path: _,
        native_session_id: _,
        native_session_path: _,
        native_last_activity_at: _,
        project_id: _,
        project_label: _,
        is_linked_worktree: _,
        repo: _,
        branch: _,
        worktree_path: _,
        warnings,
        metadata: _,
        created_at: _,
        updated_at: _,
        exit_code: _,
    } = info;
    let protocol::SessionCapabilities { resume: _, fork: _ } = capabilities;
    if let Some(protocol::SessionRuntime {
        state: _,
        runtime_generation: _,
        worker_id: _,
        worker_instance_id: _,
        started_at: _,
        last_connected_at: _,
        loss_reason: _,
    }) = session_runtime
    {}
    for protocol::SubagentInfo {
        id: _,
        parent_id: _,
        provider: _,
        agent_type: _,
        lifecycle: _,
        activity: _,
        revision: _,
        started_at_ms: _,
        updated_at_ms: _,
        finished_at_ms: _,
    } in subagents
    {}
    for protocol::SessionWarning {
        kind: _,
        message: _,
        detail: _,
    } in warnings
    {}
}

/// Serializes `record` the way the store writes it and returns the line as JSON.
fn stored_line(record: &Record) -> Value {
    let mut body = String::new();
    super::append_line(&mut body, record).expect("serialize record");
    serde_json::from_str(body.trim_end()).expect("stored line is json")
}

fn collect_paths(prefix: &str, value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                out.insert(path.clone());
                collect_paths(&path, child, out);
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            for item in items {
                collect_paths(&path, item, out);
            }
        }
        _ => {}
    }
}

/// Deserializes `sample` into `T`, requires the serialized form to carry exactly
/// the sample's keys (so a populated field is never dropped) and returns it.
fn typed<T>(sample: &Value, wrap: impl Fn(T) -> Record) -> (T, BTreeSet<String>)
where
    T: serde::de::DeserializeOwned + Clone,
{
    let value: T = serde_json::from_value(sample.clone()).expect("sample deserializes");
    let line = stored_line(&wrap(value.clone()));
    let mut sample_paths = BTreeSet::new();
    let mut with_kind = sample.clone();
    with_kind["schema_version"] = json!(STORE_SCHEMA_VERSION);
    collect_paths("", &with_kind, &mut sample_paths);
    let mut actual = BTreeSet::new();
    collect_paths("", &line, &mut actual);
    actual.remove("kind");
    assert_eq!(
        actual, sample_paths,
        "the serialized record must carry exactly the keys of its populated sample"
    );
    (value, actual)
}

/// The recursive key set of every persisted record kind, one `kind.path` per line.
fn current_shape() -> BTreeSet<String> {
    let mut shape = BTreeSet::new();
    let mut label = |kind: &str, paths: BTreeSet<String>| {
        shape.insert(kind.to_owned());
        shape.extend(paths.into_iter().map(|path| format!("{kind}.{path}")));
    };

    let (resume_a, paths_a) = typed::<ResumeBinding>(&resume_builtin_json(), Record::Resume);
    let (resume_b, paths_b) = typed::<ResumeBinding>(&resume_package_json(), Record::Resume);
    label("resume", paths_a.union(&paths_b).cloned().collect());
    let (worktree, paths) = typed::<WorktreeBinding>(&worktree_json(), Record::Worktree);
    label("worktree", paths);
    let (project, paths) = typed::<ProjectRecord>(&project_json(), Record::Project);
    label("project", paths);
    let (session, paths) =
        typed::<SessionRecord>(&session_json(), |record| Record::Session(Box::new(record)));
    label("session", paths);

    assert_every_field_is_reviewed(&[resume_a, resume_b], &worktree, &project, &session);
    shape
}

fn render(shape: &BTreeSet<String>) -> String {
    shape
        .iter()
        .flat_map(|line| [line.as_str(), "\n"])
        .collect()
}

/// Compares `actual` with the snapshot kept for `version`.
fn check_shape(
    version: u32,
    actual: &BTreeSet<String>,
    snapshots: &[(u32, &str)],
) -> Result<(), String> {
    let rendered = render(actual);
    let Some((_, expected)) = snapshots.iter().find(|(known, _)| *known == version) else {
        return Err(format!(
            "schema version {version} has no shape snapshot. Add \
             crates/daemon/src/store/fixtures/shape/schema-{version}.txt with the \
             content below and register it in SHAPE_SNAPSHOTS:\n{rendered}"
        ));
    };
    let expected: BTreeSet<String> = expected.lines().map(str::to_owned).collect();
    if &expected == actual {
        return Ok(());
    }
    let added: Vec<_> = actual.difference(&expected).collect();
    let removed: Vec<_> = expected.difference(actual).collect();
    Err(format!(
        "the serialized shape of a persisted store record changed without a schema \
         bump (schema {version}).\n  added: {added:?}\n  removed: {removed:?}\n\
         Bump STORE_SCHEMA_VERSION, add a MIGRATIONS step from schema {version}, and \
         add fixtures/shape/schema-<new>.txt with the content below:\n{rendered}"
    ))
}

#[test]
fn the_persisted_record_shape_matches_the_snapshot_of_the_current_schema() {
    if let Err(message) = check_shape(STORE_SCHEMA_VERSION, &current_shape(), SHAPE_SNAPSHOTS) {
        panic!("{message}");
    }
}

#[test]
fn a_changed_shape_without_a_new_snapshot_is_reported_as_an_unbumped_change() {
    let mut shape = current_shape();
    shape.insert("resume.a_new_field".to_owned());
    let message = check_shape(STORE_SCHEMA_VERSION, &shape, SHAPE_SNAPSHOTS)
        .expect_err("an added field must fail the guard");
    assert!(message.contains("without a schema bump"), "{message}");
    assert!(message.contains("resume.a_new_field"), "{message}");

    let mut shape = current_shape();
    assert!(shape.remove("resume.native_launch"));
    let message = check_shape(STORE_SCHEMA_VERSION, &shape, SHAPE_SNAPSHOTS)
        .expect_err("a removed field must fail the guard");
    assert!(message.contains("resume.native_launch"), "{message}");
}

#[test]
fn a_bumped_schema_without_a_snapshot_is_reported() {
    let message = check_shape(STORE_SCHEMA_VERSION + 1, &current_shape(), SHAPE_SNAPSHOTS)
        .expect_err("a new schema needs its own snapshot");
    assert!(message.contains("has no shape snapshot"), "{message}");
}

#[test]
fn every_older_schema_has_a_migration_step() {
    for version in 1..STORE_SCHEMA_VERSION {
        assert!(
            MIGRATIONS.iter().any(|migration| migration.from == version),
            "schema {version} has no migration step to schema {}",
            version + 1
        );
    }
    assert!(
        MIGRATIONS
            .iter()
            .all(|migration| migration.from < STORE_SCHEMA_VERSION),
        "a migration step starts at or above the current schema"
    );
}

#[test]
fn every_snapshot_belongs_to_a_schema_up_to_the_current_one() {
    assert!(
        SHAPE_SNAPSHOTS
            .iter()
            .any(|(version, _)| *version == STORE_SCHEMA_VERSION),
        "the current schema has no snapshot"
    );
    assert!(SHAPE_SNAPSHOTS
        .iter()
        .all(|(version, _)| *version <= STORE_SCHEMA_VERSION));
}
