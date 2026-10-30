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
//! The snapshot also carries the serialized value set of every persisted enum, as
//! `<kind>.<path>=<value>` lines. Each enum is listed once through [`unit_values!`]
//! or `data_values!` (variants with data; each entry pairs a pattern with its
//! sample), which generate the sample list and a `match` without a wildcard arm
//! from the same variant list, so a new variant fails to compile until it is
//! listed. Every listed value must round-trip through
//! strict serde; a skipped `Unknown` variant contributes no value and must not.
//! Every string-valued leaf of the samples must be a registered enum path or a
//! free-text path ([`free_text`]); an unclassified leaf fails the guard, which is
//! how a new enum-valued field is caught. A new enum value is a bump of
//! [`STORE_SCHEMA_VERSION`], because an older daemon rejects the line or degrades
//! it to a fallback variant.
//!
//! Every sample is destructured field by field (no `..`), so adding a field to a
//! persisted struct fails to compile here until the sample and snapshot are
//! revisited. Each enum-valued field is also pinned to its enum type in that
//! function, so retyping an enum field fails to compile. A string field turned into
//! an enum compiles; the unclassified-leaf check above catches it because its new
//! values are not registered. Types with private fields ([`NativeSessionLaunch`] and its
//! components) cannot be destructured; their samples are populated through the
//! full JSON form instead.
//!
//! [`NativeSessionLaunch`]: crate::agent::NativeSessionLaunch

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use protocol::{
    AgentActivity, BackgroundKind, BackgroundStatus, BindingProvenance, CwdSource, ProjectSource,
    RuntimeState, SessionInfo, SessionState, SessionWarningKind, StateSource, SubagentLifecycle,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};

use super::schema::{MIGRATIONS, STORE_SCHEMA_VERSION};
use super::{
    ActiveAgentOrigin, DesiredState, NativeIdentityOrdering, ProjectRecord, Record, ResumeBinding,
    RuntimeRecord, SessionRecord, SessionTransaction, StoredInputRules, TransactionKind,
    WorktreeBinding, WorktreeStatus,
};
use crate::agent::{
    ExistenceCheckKind, NameMatch, NativeArg, NativeReferenceProvenance, SessionRefKind,
    SettingsBase,
};

/// Key-set snapshots by schema version. A new schema adds its entry here.
const SHAPE_SNAPSHOTS: &[(u32, &str)] = &[
    (2, include_str!("fixtures/shape/schema-2.txt")),
    (3, include_str!("fixtures/shape/schema-3.txt")),
    (4, include_str!("fixtures/shape/schema-4.txt")),
    (5, include_str!("fixtures/shape/schema-5.txt")),
    (6, include_str!("fixtures/shape/schema-6.txt")),
    (7, include_str!("fixtures/shape/schema-7.txt")),
    (8, include_str!("fixtures/shape/schema-8.txt")),
    (9, include_str!("fixtures/shape/schema-9.txt")),
    (10, include_str!("fixtures/shape/schema-10.txt")),
    (11, include_str!("fixtures/shape/schema-11.txt")),
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
                "existence": {"check": "none"},
                "assign_fork": true
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
        "max_depth": 1,
        "store_env": "AGENT_STORE_DIR",
        "store_max_depth": 0,
        "relocating_args": ["--store-dir"],
        "relocating_settings": [{"base": "root", "path": "settings.json", "key": "storeDir"}]
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

/// Background snapshot with one item of each kind shape: subagent, shell and an
/// `other` kind carrying its provider string.
fn background_json() -> Value {
    json!({
        "revision": "1",
        "observed_at_ms": 1000,
        "items": [
            {"id": "sub-1", "kind": "subagent", "status": "running", "agent_type": "explore", "label": "scan", "first_seen_ms": 1, "last_seen_ms": 2},
            {"id": "bash-1", "kind": "shell", "status": "pending", "label": "watch", "program": "cargo", "first_seen_ms": 1, "last_seen_ms": 2},
            {"id": "x-1", "kind": "other", "provider_kind": "future", "status": "other", "first_seen_ms": 1, "last_seen_ms": 2}
        ]
    })
}

/// Session whose recovery binding is the package-pinned resume sample.
fn session_json() -> Value {
    session_json_with(&resume_package_json())
}

/// Session whose recovery binding is the built-in resume sample.
fn session_builtin_json() -> Value {
    session_json_with(&resume_builtin_json())
}

fn session_json_with(recovery: &Value) -> Value {
    let mut record = json!({
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
                "loss_reason": "none",
                "exit": {"cause": "oom_kill", "signal": 15, "code": 1}
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
        "recovery": recovery,
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
        },
        "active_agent_origin": "reported"
    });
    record["info"]["background"] = background_json();
    record["info"]["output_force_closed"] = json!(true);
    record
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
            native_reference_provenance,
            profile_revision: _,
        } = binding;
        let _: &NativeReferenceProvenance = native_reference_provenance;
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
        status,
        project_id: _,
        created_at: _,
        updated_at: _,
    } = worktree;
    let _: &WorktreeStatus = status;
    let ProjectRecord {
        git_common_dir: _,
        repo_root: _,
        custom_name: _,
        origin_url: _,
        default_base_branch: _,
        is_bare: _,
        source,
        added_at: _,
        last_used_at: _,
    } = project;
    let _: &ProjectSource = source;
    let SessionRecord {
        schema_version: _,
        session_id: _,
        desired_state,
        transaction,
        info,
        recovery: _,
        native_identity_ordering,
        runtime,
        active_agent_origin,
    } = session;
    let _: &DesiredState = desired_state;
    let _: &Option<ActiveAgentOrigin> = active_agent_origin;
    if let Some(SessionTransaction {
        id: _,
        kind,
        phase,
        previous_worker_id: _,
        previous_worker_instance_id: _,
        daemon_instance_id: _,
    }) = transaction
    {
        let _: &TransactionKind = kind;
        let _: &String = phase;
    }
    if let Some(NativeIdentityOrdering {
        worker_instance_id: _,
        pid: _,
        pid_start_identity: _,
        sequence: _,
        worker_sequence: _,
    }) = native_identity_ordering
    {}
    let RuntimeRecord {
        state: record_state,
        worker_id: _,
        worker_instance_id: _,
        service_id: _,
        generation: _,
        executable: _,
        reason,
    } = runtime;
    let _: &Option<String> = reason;
    let _: &RuntimeState = record_state;
    let SessionInfo {
        id: _,
        external: _,
        capabilities,
        name: _,
        agent: _,
        agent_base: _,
        cwd: _,
        cwd_source,
        pid: _,
        runtime: session_runtime,
        cols: _,
        rows: _,
        state,
        state_source,
        activity,
        subagents,
        background,
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
        capability_refusals: _,
        output_force_closed: _,
        metadata: _,
        created_at: _,
        updated_at: _,
        exit_code: _,
    } = info;
    let _: &Option<CwdSource> = cwd_source;
    let _: &SessionState = state;
    let _: &StateSource = state_source;
    let _: &Option<AgentActivity> = activity;
    let protocol::SessionCapabilities { resume: _, fork: _ } = capabilities;
    if let Some(protocol::SessionRuntime {
        state: runtime_state,
        runtime_generation: _,
        worker_id: _,
        worker_instance_id: _,
        started_at: _,
        last_connected_at: _,
        loss_reason,
        exit,
    }) = session_runtime
    {
        let _: &RuntimeState = runtime_state;
        let _: &Option<String> = loss_reason;
        if let Some(protocol::RuntimeExit {
            cause,
            signal: _,
            code: _,
        }) = exit
        {
            let _: &String = cause;
        }
    }
    for protocol::SubagentInfo {
        id: _,
        parent_id: _,
        provider: _,
        agent_type: _,
        lifecycle,
        activity,
        revision: _,
        started_at_ms: _,
        updated_at_ms: _,
        finished_at_ms: _,
    } in subagents
    {
        let _: &SubagentLifecycle = lifecycle;
        let _: &Option<AgentActivity> = activity;
    }
    if let Some(protocol::SessionBackground {
        revision: _,
        observed_at_ms: _,
        items,
    }) = background
    {
        for protocol::BackgroundItem {
            id: _,
            kind,
            provider_kind: _,
            status,
            agent_type: _,
            label: _,
            program: _,
            first_seen_ms: _,
            last_seen_ms: _,
        } in items
        {
            let _: &BackgroundKind = kind;
            let _: &BackgroundStatus = status;
        }
    }
    for protocol::SessionWarning {
        kind,
        message: _,
        detail: _,
    } in warnings
    {
        let _: &SessionWarningKind = kind;
    }
}

/// Serializes `record` the way the store writes it and returns the line as JSON.
fn stored_line(record: &Record) -> Value {
    let mut body = String::new();
    super::append_line(&mut body, record).expect("serialize record");
    serde_json::from_str(body.trim_end()).expect("stored line is json")
}

pub(crate) fn collect_paths(prefix: &str, value: &Value, out: &mut BTreeSet<String>) {
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

/// String-valued leaves of a serialized record: leaf path to the values seen there.
pub(crate) type Leaves = BTreeMap<String, BTreeSet<String>>;

pub(crate) fn collect_string_leaves(prefix: &str, value: &Value, out: &mut Leaves) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_string_leaves(&path, child, out);
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            for item in items {
                collect_string_leaves(&path, item, out);
            }
        }
        Value::String(text) => {
            out.entry(prefix.to_owned())
                .or_default()
                .insert(text.clone());
        }
        _ => {}
    }
}

/// Deserializes `sample` into `T`, requires the serialized form to carry exactly
/// the sample's keys (so a populated field is never dropped) and returns it with
/// its key paths and string leaves.
fn typed<T>(sample: &Value, wrap: impl Fn(T) -> Record) -> (T, BTreeSet<String>, Leaves)
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
    let mut leaves = Leaves::new();
    let mut body = line;
    body.as_object_mut()
        .expect("stored line is an object")
        .remove("kind");
    collect_string_leaves("", &body, &mut leaves);
    (value, actual, leaves)
}

/// Serialized values of `samples`, each checked to deserialize strictly back to
/// itself. A plain string is the value. A data-carrying variant contributes the
/// string under `tag` when the enum is internally tagged, or its only key when it
/// is externally tagged (`{"literal": ..}`), so every variant name is a value.
pub(crate) fn string_values<T>(samples: &[T], tag: Option<&str>) -> Vec<String>
where
    T: Serialize + DeserializeOwned + Debug,
{
    let mut values = BTreeSet::new();
    for sample in samples {
        let json = serde_json::to_value(sample).expect("sample serializes");
        let back: T = serde_json::from_value(json.clone()).expect("sample deserializes strictly");
        assert_eq!(
            serde_json::to_value(&back).expect("round-tripped sample serializes"),
            json,
            "a listed value must round-trip"
        );
        let value = match (&json, tag) {
            (Value::String(text), None) => text.clone(),
            (Value::Object(map), Some(tag)) => map
                .get(tag)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{json} carries no string under `{tag}`"))
                .to_owned(),
            (Value::Object(map), None) if map.len() == 1 => map
                .keys()
                .next()
                .expect("an object with one key has a key")
                .clone(),
            _ => panic!("{json} is not a string, a tagged object or a one-key object"),
        };
        values.insert(value);
    }
    values.into_iter().collect()
}

/// Asserts that an `Unknown` variant never survives a strict round trip, so the
/// daemon cannot persist it and an older store never needs to read it.
pub(crate) fn assert_never_round_trips<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let round_trips = serde_json::to_value(value)
        .ok()
        .and_then(|json| serde_json::from_value::<T>(json).ok())
        .is_some_and(|back| back == *value);
    assert!(
        !round_trips,
        "{value:?} must not round-trip through strict serde"
    );
}

/// Serialized values of a unit-variant enum. The variants after `;` are skipped:
/// they contribute no value and must not round-trip. The generated `match` has no
/// wildcard arm, so a variant missing from both lists fails to compile.
macro_rules! unit_values {
    ($ty:ident: $($variant:ident),+ $(; skip $($skipped:ident),+)?) => {{
        let listed = [$($ty::$variant),+];
        for value in &listed {
            match value {
                $($ty::$variant => {})+
                $($($ty::$skipped => {})+)?
            }
        }
        $($($crate::store::shape_guard::assert_never_round_trips(&$ty::$skipped);)+)?
        string_values(&listed, None)
    }};
}

pub(crate) use unit_values;

/// Serialized values of an enum with data-carrying variants. Each entry pairs a
/// pattern with the sample that must match it, and the generated `match` lists the
/// same patterns without a wildcard arm: a new variant fails to compile until an
/// entry names it, and an entry cannot name a variant without supplying its sample.
/// `$tag` is the internal tag key, or `None` for an externally tagged enum.
macro_rules! data_values {
    ($ty:ty, $tag:expr; $($pattern:pat_param => $sample:expr),+ $(,)?) => {{
        let listed: Vec<$ty> = vec![$($sample),+];
        let matchers: Vec<fn(&$ty) -> bool> = vec![$(|value| matches!(value, $pattern)),+];
        for (value, matcher) in listed.iter().zip(&matchers) {
            assert!(matcher(value), "{value:?} does not match the pattern it is listed under");
        }
        for value in &listed {
            match value {
                $($pattern => {})+
            }
        }
        string_values(&listed, $tag)
    }};
}

pub(crate) use data_values;

fn native_arg_values() -> Vec<String> {
    data_values!(
        NativeArg, None;
        NativeArg::Literal(_) => NativeArg::Literal("--flag".to_owned()),
        NativeArg::Reference => NativeArg::Reference,
    )
}

/// The `kind` tags of the store's line enum, one per record kind.
fn record_kind_values() -> Vec<String> {
    let resume = || serde_json::from_value::<ResumeBinding>(resume_builtin_json()).expect("resume");
    let worktree = || serde_json::from_value::<WorktreeBinding>(worktree_json()).expect("worktree");
    let project = || serde_json::from_value::<ProjectRecord>(project_json()).expect("project");
    let session =
        || Box::new(serde_json::from_value::<SessionRecord>(session_json()).expect("session"));
    data_values!(
        Record, Some("kind");
        Record::Session(_) => Record::Session(session()),
        Record::Resume(_) => Record::Resume(resume()),
        Record::Worktree(_) => Record::Worktree(worktree()),
        Record::Project(_) => Record::Project(project()),
    )
}

fn binding_provenance_values() -> Vec<String> {
    let provenance = |sample: Value| {
        serde_json::from_value::<BindingProvenance>(sample["launch_binding"]["provenance"].clone())
            .expect("provenance sample deserializes")
    };
    data_values!(
        BindingProvenance, Some("kind");
        BindingProvenance::Builtin { .. } => provenance(resume_builtin_json()),
        BindingProvenance::Package { .. } => provenance(resume_package_json()),
    )
}

/// Records that embed a [`ResumeBinding`]: the resume record and a session's
/// `recovery`.
const RESUME_ROOTS: &[&str] = &["resume", "session.recovery"];

fn under(roots: &[&str], relative: &str) -> Vec<String> {
    roots
        .iter()
        .map(|root| format!("{root}.{relative}"))
        .collect()
}

/// One persisted enum: every leaf path it is stored at and its serialized values.
pub(crate) struct EnumEntry {
    pub(crate) paths: Vec<String>,
    pub(crate) values: Vec<String>,
}

#[expect(
    clippy::too_many_lines,
    reason = "one registration per persisted enum path"
)]
fn persisted_enums() -> Vec<EnumEntry> {
    let mut registry = Vec::new();
    let mut add = |paths: Vec<String>, values: Vec<String>| {
        registry.push(EnumEntry { paths, values });
    };
    let own = |path: &str| vec![path.to_owned()];

    add(
        own("session.desired_state"),
        unit_values!(DesiredState: Running, Stopped, Removed),
    );
    add(
        own("session.transaction.kind"),
        unit_values!(TransactionKind: Create, Stop, Recover, Remove),
    );
    let runtime_state = || {
        unit_values!(
            RuntimeState: Starting, Live, Reconnecting, Terminal, Lost, Conflict, Incompatible; skip Unknown
        )
    };
    add(
        vec![
            "session.runtime.state".to_owned(),
            "session.info.runtime.state".to_owned(),
        ],
        runtime_state(),
    );
    add(
        own("session.active_agent_origin"),
        unit_values!(ActiveAgentOrigin: Reported, Observed),
    );
    add(
        own("session.info.state"),
        unit_values!(SessionState: Starting, Running, Stopped, Done, Failed),
    );
    add(
        own("session.info.state_source"),
        unit_values!(StateSource: OscTitle, OscProgress, Screen, Process, Report; skip Unknown),
    );
    add(
        vec![
            "session.info.activity".to_owned(),
            "session.info.subagents[].activity".to_owned(),
        ],
        unit_values!(AgentActivity: Working, Blocked, Idle; skip Unknown),
    );
    add(
        own("session.info.cwd_source"),
        unit_values!(CwdSource: Launch, Procwatch, Osc7; skip Unknown),
    );
    add(
        own("session.info.subagents[].lifecycle"),
        unit_values!(SubagentLifecycle: Running, Completed, Failed, Cancelled, Lost; skip Unknown),
    );
    add(
        own("session.info.background.items[].kind"),
        unit_values!(
            BackgroundKind: Subagent, Workflow, Shell, Monitor, McpTask, Teammate, Dream,
            AutoModeScan, MemoryImport, CloudSession, Other
        ),
    );
    add(
        own("session.info.background.items[].status"),
        unit_values!(BackgroundStatus: Running, Pending, Other),
    );
    add(
        own("session.info.warnings[].kind"),
        unit_values!(
            SessionWarningKind: Fetch, BaseBranchFallback, SetupScript, Hook, NativeRecovery,
            BranchAheadOfRemote; skip Unknown
        ),
    );
    add(
        own("worktree.status"),
        unit_values!(WorktreeStatus: Active, Merged, Deleted),
    );
    add(
        own("project.source"),
        unit_values!(ProjectSource: Auto, Manual; skip Unknown),
    );

    add(
        under(RESUME_ROOTS, "native_launch.reference_kind"),
        unit_values!(SessionRefKind: Id, Path),
    );
    for relative in [
        "native_launch.resume_args[]",
        "native_launch.fork_args[]",
        "native_launch.assigned.launch_args[]",
    ] {
        add(under(RESUME_ROOTS, relative), native_arg_values());
    }
    add(
        under(RESUME_ROOTS, "native_launch.assigned.existence.check"),
        unit_values!(ExistenceCheckKind: None, File),
    );
    add(
        under(RESUME_ROOTS, "native_launch.assigned.existence.name_match"),
        unit_values!(NameMatch: Exact, EndsWith),
    );
    add(
        under(
            RESUME_ROOTS,
            "native_launch.assigned.existence.relocating_settings[].base",
        ),
        unit_values!(SettingsBase: Root, Cwd),
    );
    add(
        under(RESUME_ROOTS, "launch_binding.provenance.kind"),
        binding_provenance_values(),
    );
    add(
        under(RESUME_ROOTS, "native_reference_provenance"),
        unit_values!(NativeReferenceProvenance: Reported, Assigned),
    );
    registry
}

/// Free-text leaves of a resume binding, relative to each of [`RESUME_ROOTS`].
const RESUME_FREE: &[&str] = &[
    "session_id",
    "name",
    "agent",
    "agent_base",
    "cwd",
    "native_session_id",
    "native_session_path",
    "project_id",
    "metadata.*",
    "program",
    "args[]",
    "native_launch.resume_args[].literal",
    "native_launch.fork_args[].literal",
    "native_launch.assigned.launch_args[].literal",
    "native_launch.assigned.existence.root_env",
    "native_launch.assigned.existence.root_home",
    "native_launch.assigned.existence.dir",
    "native_launch.assigned.existence.file_name",
    "native_launch.assigned.existence.store_env",
    "native_launch.assigned.existence.relocating_args[]",
    "native_launch.assigned.existence.relocating_settings[].path",
    "native_launch.assigned.existence.relocating_settings[].key",
    "launch_binding.runtime_id",
    "launch_binding.provenance.package.id",
    "launch_binding.provenance.package.version",
    "launch_binding.provenance.descriptor_digest",
    "launch_binding.provenance.package_digest",
    "profile_revision",
];
/// Free-text leaves of the other record kinds, by full path.
const ABSOLUTE_FREE: &[&str] = &[
    "worktree.session_id",
    "worktree.repository",
    "worktree.branch",
    "worktree.base_branch",
    "worktree.branch_slug",
    "worktree.path",
    "worktree.agent",
    "worktree.project_id",
    "worktree.created_at",
    "worktree.updated_at",
    "project.git_common_dir",
    "project.repo_root",
    "project.custom_name",
    "project.origin_url",
    "project.default_base_branch",
    "project.added_at",
    "project.last_used_at",
    "session.session_id",
    "session.transaction.id",
    "session.transaction.phase",
    "session.transaction.previous_worker_id",
    "session.transaction.previous_runtime_id",
    "session.transaction.daemon_instance_id",
    "session.info.id",
    "session.info.name",
    "session.info.agent",
    "session.info.agent_base",
    "session.info.cwd",
    "session.info.runtime.runtime_generation",
    "session.info.runtime.worker_id",
    "session.info.runtime.worker_instance_id",
    "session.info.runtime.started_at",
    "session.info.runtime.last_connected_at",
    "session.info.runtime.loss_reason",
    "session.info.runtime.exit.cause",
    "session.info.subagents[].id",
    "session.info.subagents[].parent_id",
    "session.info.subagents[].provider",
    "session.info.subagents[].agent_type",
    "session.info.subagents[].revision",
    "session.info.active_agent",
    "session.info.active_agent_base",
    "session.info.active_agent_session_id",
    "session.info.active_agent_session_path",
    "session.info.native_session_id",
    "session.info.native_session_path",
    "session.info.native_last_activity_at",
    "session.info.project_id",
    "session.info.project_label",
    "session.info.repo",
    "session.info.branch",
    "session.info.worktree_path",
    "session.info.warnings[].message",
    "session.info.warnings[].detail",
    "session.info.metadata.*",
    "session.info.created_at",
    "session.info.updated_at",
    "session.info.background.revision",
    "session.info.background.items[].id",
    "session.info.background.items[].provider_kind",
    "session.info.background.items[].agent_type",
    "session.info.background.items[].label",
    "session.info.background.items[].program",
    "session.native_identity_ordering.runtime_id",
    "session.runtime.worker_id",
    "session.runtime.runtime_id",
    "session.runtime.service_id",
    "session.runtime.generation",
    "session.runtime.executable",
    "session.runtime.reason",
];

/// String-valued leaves that are not a closed enum: ids, paths, timestamps,
/// digests, provider strings, argument literals, user metadata (`metadata.*`),
/// and the open-string fields an older reader cannot reject
/// (`transaction.phase`, `runtime.reason`, `info.runtime.loss_reason`,
/// `info.runtime.exit.cause`). Each entry lists the leaf paths it covers; an
/// entry that matches no sample leaf fails the guard.
fn free_text() -> Vec<Vec<String>> {
    let mut entries: Vec<Vec<String>> = RESUME_FREE
        .iter()
        .map(|relative| under(RESUME_ROOTS, relative))
        .collect();
    entries.extend(ABSOLUTE_FREE.iter().map(|path| vec![(*path).to_owned()]));
    entries
}

/// Maps a map-valued leaf (`...metadata.<key>`) to its `...metadata.*` entry.
pub(crate) fn normalized_leaf(path: &str) -> String {
    let segments: Vec<&str> = path.split('.').collect();
    match segments.iter().position(|segment| *segment == "metadata") {
        Some(index) if index + 1 < segments.len() => {
            format!("{}.*", segments[..=index].join("."))
        }
        _ => path.to_owned(),
    }
}

/// Checks that every string leaf is a registered enum value or free text, that
/// every path of every registry entry matches a leaf, and returns the `path=value` snapshot lines.
pub(crate) fn classify_leaves(
    leaves: &Leaves,
    enums: &[EnumEntry],
    free: &[Vec<String>],
) -> Result<BTreeSet<String>, String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for (path, values) in leaves {
        let key = normalized_leaf(path);
        seen.insert(key.clone());
        if let Some(entry) = enums.iter().find(|entry| entry.paths.contains(&key)) {
            for value in values.iter().filter(|value| !entry.values.contains(*value)) {
                problems.push(format!(
                    "{path}={value} is not a value of the enum registered for that path"
                ));
            }
        } else if !free.iter().any(|paths| paths.contains(&key)) {
            problems.push(format!(
                "string leaf {path} is neither a registered enum path (persisted_enums) nor free text (free_text)"
            ));
        }
    }
    for entry in enums
        .iter()
        .filter(|entry| !entry.paths.iter().all(|path| seen.contains(path)))
    {
        problems.push(format!(
            "registered enum paths {:?} match no sample leaf",
            entry.paths
        ));
    }
    for paths in free
        .iter()
        .filter(|paths| !paths.iter().all(|path| seen.contains(path)))
    {
        problems.push(format!("free-text entry {paths:?} matches no sample leaf"));
    }
    if !problems.is_empty() {
        return Err(format!(
            "the persisted string leaves are not fully classified:\n  {}",
            problems.join("\n  ")
        ));
    }
    Ok(enums
        .iter()
        .flat_map(|entry| {
            entry.paths.iter().flat_map(|path| {
                entry
                    .values
                    .iter()
                    .map(move |value| format!("{path}={value}"))
            })
        })
        .collect())
}

/// The recursive key set and enum value set of every persisted record kind, one
/// `kind.path` or `kind.path=value` per line.
fn current_shape() -> BTreeSet<String> {
    let (mut shape, leaves) = sampled_shape();
    let values = classify_leaves(&leaves, &persisted_enums(), &free_text())
        .unwrap_or_else(|message| panic!("{message}"));
    shape.extend(values);

    let labels: BTreeSet<String> = shape
        .iter()
        .filter(|line| !line.contains('.'))
        .cloned()
        .collect();
    let tags = record_kind_values();
    assert_eq!(
        tags.iter().cloned().collect::<BTreeSet<_>>(),
        labels,
        "the sampled record kinds must be exactly the serialized `kind` tags of `Record`"
    );
    shape.extend(tags.into_iter().map(|tag| format!("record.kind={tag}")));
    shape
}

/// Key lines and string leaves (prefixed with the record kind) of every sample.
fn sampled_shape() -> (BTreeSet<String>, Leaves) {
    let mut shape = BTreeSet::new();
    let mut leaves = Leaves::new();
    let mut label = |kind: &str, paths: BTreeSet<String>, found: Leaves| {
        shape.insert(kind.to_owned());
        shape.extend(paths.into_iter().map(|path| format!("{kind}.{path}")));
        for (path, values) in found {
            leaves
                .entry(format!("{kind}.{path}"))
                .or_default()
                .extend(values);
        }
    };

    let (resume_a, paths_a, leaves_a) =
        typed::<ResumeBinding>(&resume_builtin_json(), Record::Resume);
    let (resume_b, paths_b, leaves_b) =
        typed::<ResumeBinding>(&resume_package_json(), Record::Resume);
    let mut resume_leaves = leaves_a;
    for (path, values) in leaves_b {
        resume_leaves.entry(path).or_default().extend(values);
    }
    label(
        "resume",
        paths_a.union(&paths_b).cloned().collect(),
        resume_leaves,
    );
    let (worktree, paths, found) = typed::<WorktreeBinding>(&worktree_json(), Record::Worktree);
    label("worktree", paths, found);
    let (project, paths, found) = typed::<ProjectRecord>(&project_json(), Record::Project);
    label("project", paths, found);
    let wrap = |record| Record::Session(Box::new(record));
    let (session, session_paths, mut session_leaves) =
        typed::<SessionRecord>(&session_json(), wrap);
    let (_, builtin_paths, builtin_leaves) = typed::<SessionRecord>(&session_builtin_json(), wrap);
    for (path, values) in builtin_leaves {
        session_leaves.entry(path).or_default().extend(values);
    }
    label(
        "session",
        session_paths.union(&builtin_paths).cloned().collect(),
        session_leaves,
    );

    assert_every_field_is_reviewed(&[resume_a, resume_b], &worktree, &project, &session);
    (shape, leaves)
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
        "the serialized shape (keys or enum values) of a persisted store record changed without a schema \
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

#[test]
fn a_changed_enum_value_set_is_reported_as_an_unbumped_change() {
    let mut shape = current_shape();
    shape.insert("session.info.activity=future".to_owned());
    let message = check_shape(STORE_SCHEMA_VERSION, &shape, SHAPE_SNAPSHOTS)
        .expect_err("an added enum value must fail the guard");
    assert!(message.contains("without a schema bump"), "{message}");
    assert!(
        message.contains("session.info.activity=future"),
        "{message}"
    );

    let mut shape = current_shape();
    assert!(shape.remove("project.source=manual"));
    let message = check_shape(STORE_SCHEMA_VERSION, &shape, SHAPE_SNAPSHOTS)
        .expect_err("a removed enum value must fail the guard");
    assert!(message.contains("project.source=manual"), "{message}");
}

#[test]
fn an_unknown_variant_contributes_no_value() {
    let shape = current_shape();
    assert!(
        shape.iter().all(|line| !line.ends_with("=unknown")),
        "an `Unknown` variant must never be a snapshot value"
    );
    // The fallback variants that are real persisted values stay in the snapshot.
    assert!(shape.contains("session.info.background.items[].kind=other"));
    assert!(shape.contains("session.info.background.items[].status=other"));
}

#[test]
fn an_unclassified_string_leaf_fails_the_guard() {
    let (_, mut leaves) = sampled_shape();
    leaves
        .entry("session.info.new_enum_field".to_owned())
        .or_default()
        .insert("a".to_owned());
    let message = classify_leaves(&leaves, &persisted_enums(), &free_text())
        .expect_err("an unclassified leaf must fail");
    assert!(message.contains("session.info.new_enum_field"), "{message}");
}

#[test]
fn a_sample_value_outside_its_enum_fails_the_guard() {
    let (_, mut leaves) = sampled_shape();
    leaves
        .get_mut("session.info.activity")
        .expect("activity leaf")
        .insert("future".to_owned());
    let message = classify_leaves(&leaves, &persisted_enums(), &free_text())
        .expect_err("a value outside the enum must fail");
    assert!(
        message.contains("session.info.activity=future"),
        "{message}"
    );
}

#[test]
fn a_registry_entry_without_a_leaf_fails_the_guard() {
    let (_, mut leaves) = sampled_shape();
    leaves.remove("project.source");
    let message = classify_leaves(&leaves, &persisted_enums(), &free_text())
        .expect_err("an enum path without a leaf must fail");
    assert!(message.contains("project.source"), "{message}");

    let (_, leaves) = sampled_shape();
    let mut free = free_text();
    free.push(vec!["session.info.removed_field".to_owned()]);
    let message = classify_leaves(&leaves, &persisted_enums(), &free)
        .expect_err("a free-text entry without a leaf must fail");
    assert!(message.contains("session.info.removed_field"), "{message}");
}

/// Externally tagged enum standing in for a persisted one that gained a variant.
#[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArgWithNewVariant {
    Literal(String),
    Reference,
    Env(String),
}

#[test]
fn a_data_variant_contributes_its_variant_name_as_a_value() {
    let current = string_values(
        &[NativeArg::Literal("x".to_owned()), NativeArg::Reference],
        None,
    );
    assert_eq!(current, ["literal", "reference"]);
    let shape = current_shape();
    assert!(shape.contains("session.recovery.native_launch.resume_args[]=literal"));
    assert!(shape.contains("resume.native_launch.resume_args[]=reference"));

    let extended = string_values(
        &[
            ArgWithNewVariant::Literal("x".to_owned()),
            ArgWithNewVariant::Reference,
            ArgWithNewVariant::Env("HOME".to_owned()),
        ],
        None,
    );
    assert_eq!(extended, ["env", "literal", "reference"]);

    let mut shape = current_shape();
    shape.insert("resume.native_launch.resume_args[]=env".to_owned());
    let message = check_shape(STORE_SCHEMA_VERSION, &shape, SHAPE_SNAPSHOTS)
        .expect_err("a new data variant must fail the guard");
    assert!(message.contains("resume_args[]=env"), "{message}");
}

#[test]
#[should_panic(expected = "does not match the pattern it is listed under")]
fn a_sample_must_match_the_pattern_it_is_listed_under() {
    data_values!(
        NativeArg, None;
        NativeArg::Literal(_) => NativeArg::Reference,
        NativeArg::Reference => NativeArg::Reference,
    );
}

#[test]
#[should_panic(expected = "must not round-trip through strict serde")]
fn a_value_that_round_trips_is_not_accepted_as_unknown() {
    assert_never_round_trips(&AgentActivity::Idle);
}
