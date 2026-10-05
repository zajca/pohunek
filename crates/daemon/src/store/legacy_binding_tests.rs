//! Schema 1 to 2 mapping of resume bindings written before the native launch
//! spec existed, tested against stores written by the v0.33.0 and v0.33.1
//! daemons through their own code.

// Rust guideline compliant 2026-06-26

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use protocol::SessionCapabilities;
use serde_json::{json, Value};

use super::{ResumeBinding, Store};
use crate::agent::SessionRef;

/// Store written by the v0.33.0 daemon: legacy `resume_mode`/`fork_*` fields.
const V0_33_0_STORE: &str = include_str!("fixtures/v0.33.0/metadata.jsonl");

/// The same store after the v0.33.1 daemon re-recorded it: the legacy fields
/// are gone and no `native_launch` was written in their place.
const V0_33_1_DAMAGED_STORE: &str = include_str!("fixtures/v0.33.1-damaged/metadata.jsonl");

/// Key the migration sets on a binding whose runtime must supply its spec.
const UNRESOLVED_KEY: &str = "native_launch_unresolved";

const LEGACY_KEYS: [&str; 7] = [
    "resume_mode",
    "ref_kind",
    "resumable",
    "fork_mode",
    "fork_resume_mode",
    "fork_ref_kind",
    "forkable",
];

fn store_with(tag: &str, content: &str) -> (Store, PathBuf) {
    let dir = crate::test_support::thread_scoped_dir(&format!("pohunek-legacy-binding-{tag}-"));
    let path = dir.join("metadata.jsonl");
    fs::write(&path, content).expect("write store");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("private store");
    (Store::new(path.clone()), path)
}

fn migrated(tag: &str, content: &str) -> (Store, PathBuf) {
    let (store, path) = store_with(tag, content);
    store.migrate_to_current().expect("migrate");
    (store, path)
}

fn raw_lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("read store")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect()
}

fn raw_lines_of(content: &str) -> Vec<Value> {
    content
        .lines()
        .map(|line| serde_json::from_str(line).expect("json line"))
        .collect()
}

fn argv(binding: &ResumeBinding, id: &str) -> Vec<String> {
    binding
        .native_launch
        .as_ref()
        .expect("binding has a native launch")
        .resume_argv(&SessionRef::id(id).expect("id reference"))
        .expect("resume argv")
}

fn fork_argv(binding: &ResumeBinding, id: &str) -> Option<Vec<String>> {
    binding.native_launch.as_ref().and_then(|launch| {
        launch
            .fork_argv(&SessionRef::id(id).expect("id reference"))
            .ok()
    })
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn assert_no_legacy_keys(object: &Value) {
    for key in LEGACY_KEYS {
        assert!(
            object.get(key).is_none(),
            "legacy key {key} survived: {object}"
        );
    }
}

/// One v0.33.0 resume line with the given legacy fields.
fn legacy_resume_line(agent: &str, program: &str, args: &[&str], legacy: &Value) -> String {
    let mut line = json!({
        "kind": "resume",
        "session_id": "s-legacy",
        "agent": agent,
        "agent_base": agent,
        "cwd": "/workspace/project",
        "cols": 120,
        "rows": 40,
        "native_session_id": "native-legacy",
        "program": program,
        "args": args,
        "input_rules": {"bracketed_paste": false, "submit_delay_ms": 150},
    });
    line.as_object_mut()
        .expect("object")
        .extend(legacy.as_object().expect("legacy object").clone());
    format!("{line}\n")
}

#[test]
fn the_v0_33_0_fixture_binding_maps_to_a_native_launch() {
    let (store, path) = migrated("fixture", V0_33_0_STORE);

    let lines = raw_lines(&path);
    assert_no_legacy_keys(&lines[0]);
    assert_no_legacy_keys(&lines[3]["recovery"]);

    let bindings = store.load_resume().expect("load resume");
    assert_eq!(bindings.len(), 1);
    let binding = &bindings[0];
    assert!(binding.resumable() && binding.forkable());
    assert_eq!(
        argv(binding, "native-fixture-1"),
        strings(&["resume", "native-fixture-1"]),
        "the fixture froze the subcommand resume mode"
    );
    assert_eq!(
        fork_argv(binding, "native-fixture-1"),
        Some(strings(&["--resume", "native-fixture-1", "--fork-session"])),
    );

    let sessions = store.load_sessions().expect("load sessions");
    let recovery = sessions[0].recovery.as_ref().expect("recovery");
    assert_eq!(
        argv(recovery, "native-fixture-2"),
        strings(&["--resume", "native-fixture-2"])
    );
    assert_eq!(
        sessions[0].info.capabilities,
        SessionCapabilities {
            resume: true,
            fork: true
        },
        "the frozen capabilities follow the mapped launch spec"
    );
}

#[test]
fn legacy_modes_of_every_built_in_agent_keep_their_argv() {
    let cases = [
        (
            "claude",
            "claude",
            vec!["--model", "sonnet"],
            json!({"resume_mode": "flag", "ref_kind": "id", "resumable": true,
                   "fork_mode": "claude_session", "fork_resume_mode": "flag",
                   "fork_ref_kind": "id", "forkable": true}),
            vec!["--resume", "native-legacy"],
            Some(vec!["--resume", "native-legacy", "--fork-session"]),
        ),
        (
            "codex",
            "codex",
            vec![],
            json!({"resume_mode": "subcommand", "ref_kind": "id", "resumable": true}),
            vec!["resume", "native-legacy"],
            None,
        ),
        (
            "hermes",
            "hermes",
            vec!["chat"],
            json!({"resume_mode": "flag", "ref_kind": "id", "resumable": true}),
            vec!["--resume", "native-legacy"],
            None,
        ),
    ];
    for (agent, program, args, legacy, resume, fork) in cases {
        let line = legacy_resume_line(agent, program, &args, &legacy);
        let (store, path) = migrated(&format!("argv-{agent}"), &line);

        assert_no_legacy_keys(&raw_lines(&path)[0]);
        let bindings = store.load_resume().expect("load resume");
        assert_eq!(
            argv(&bindings[0], "native-legacy"),
            strings(&resume),
            "{agent}"
        );
        assert_eq!(
            fork_argv(&bindings[0], "native-legacy"),
            fork.map(|fork| strings(&fork)),
            "{agent}"
        );
    }
}

#[test]
fn a_legacy_binding_frozen_as_not_resumable_stays_not_resumable() {
    let line = legacy_resume_line(
        "work",
        "/opt/work-agent",
        &[],
        &json!({"resume_mode": "flag", "ref_kind": "id", "resumable": false}),
    );
    let (store, path) = migrated("not-resumable", &line);

    let raw = &raw_lines(&path)[0];
    assert_no_legacy_keys(raw);
    assert!(
        raw.get(UNRESOLVED_KEY).is_none(),
        "frozen non-recoverable by design: {raw}"
    );
    let binding = &store.load_resume().expect("load")[0];
    assert!(!binding.resumable());
}

#[test]
fn a_legacy_fork_that_cannot_be_expressed_is_dropped_and_resume_is_kept() {
    let mismatched_reference_kind = json!({
        "resume_mode": "flag", "ref_kind": "id", "resumable": true,
        "fork_mode": "claude_session", "fork_resume_mode": "flag",
        "fork_ref_kind": "path", "forkable": true,
    });
    let line = legacy_resume_line("claude", "claude", &[], &mismatched_reference_kind);
    let (store, _path) = migrated("fork-kind", &line);

    let binding = &store.load_resume().expect("load")[0];
    assert!(binding.resumable());
    assert!(
        !binding.forkable(),
        "fork fails closed when it cannot be mapped"
    );
}

#[test]
fn a_legacy_path_reference_binding_keeps_its_reference_kind() {
    let line = legacy_resume_line(
        "work",
        "/opt/work-agent",
        &[],
        &json!({"resume_mode": "subcommand", "ref_kind": "path", "resumable": true}),
    );
    let (store, _path) = migrated("path-kind", &line);

    let binding = &store.load_resume().expect("load")[0];
    assert_eq!(
        binding.reference_kind(),
        Some(crate::agent::SessionRefKind::Path)
    );
}

#[test]
fn migrating_the_same_bytes_twice_yields_identical_stores() {
    for (tag, content) in [
        ("det-0330", V0_33_0_STORE),
        ("det-0331", V0_33_1_DAMAGED_STORE),
    ] {
        let (_first, first_path) = migrated(&format!("{tag}-a"), content);
        let (_second, second_path) = migrated(&format!("{tag}-b"), content);
        assert_eq!(
            fs::read(&first_path).expect("read first"),
            fs::read(&second_path).expect("read second"),
            "{tag}: the migration output must be deterministic"
        );
    }
}

#[test]
fn the_v0_33_1_damaged_fixture_binding_is_repaired_for_a_built_in_agent() {
    let (store, path) = migrated("damaged", V0_33_1_DAMAGED_STORE);

    let lines = raw_lines(&path);
    assert!(lines[0].get(UNRESOLVED_KEY).is_none(), "{}", lines[0]);

    let binding = &store.load_resume().expect("load resume")[0];
    assert_eq!(
        argv(binding, "native-fixture-1"),
        strings(&["--resume", "native-fixture-1"])
    );
    assert_eq!(
        fork_argv(binding, "native-fixture-1"),
        Some(strings(&["--resume", "native-fixture-1", "--fork-session"])),
        "fork is restored where the built-in spec supports it"
    );

    let sessions = store.load_sessions().expect("load sessions");
    let recovery = sessions[0].recovery.as_ref().expect("recovery");
    assert_eq!(recovery.native_launch, binding.native_launch);
    assert_eq!(
        sessions[0].info.capabilities,
        SessionCapabilities {
            resume: true,
            fork: true
        }
    );
}

#[test]
fn a_damaged_codex_binding_is_repaired_without_fork() {
    let content = V0_33_1_DAMAGED_STORE
        .replace("\"agent\":\"claude\"", "\"agent\":\"codex\"")
        .replace("\"agent_base\":\"claude\"", "\"agent_base\":\"codex\"")
        .replace("\"program\":\"claude\"", "\"program\":\"codex\"");
    let (store, _path) = migrated("damaged-codex", &content);

    let binding = &store.load_resume().expect("load resume")[0];
    assert_eq!(
        argv(binding, "native-fixture-1"),
        strings(&["resume", "native-fixture-1"])
    );
    assert!(!binding.forkable());
    let sessions = store.load_sessions().expect("load sessions");
    assert_eq!(
        sessions[0].info.capabilities,
        SessionCapabilities {
            resume: true,
            fork: false
        }
    );
}

#[test]
fn a_damaged_profile_binding_is_marked_for_repair_through_the_registry() {
    let content = V0_33_1_DAMAGED_STORE.replace("\"agent\":\"claude\"", "\"agent\":\"work\"");
    let (store, path) = migrated("damaged-profile", &content);

    let lines = raw_lines(&path);
    assert_eq!(lines[0][UNRESOLVED_KEY], json!(true));
    assert_eq!(lines[3]["recovery"][UNRESOLVED_KEY], json!(true));
    let binding = &store.load_resume().expect("load resume")[0];
    assert!(
        binding.native_launch.is_none(),
        "the registry supplies the spec at load"
    );
    let sessions = store.load_sessions().expect("load sessions");
    assert_eq!(
        sessions[0].info.capabilities,
        SessionCapabilities {
            resume: false,
            fork: false
        },
        "capabilities are decided when the spec is resolved"
    );
}

#[test]
fn bindings_that_are_non_recoverable_by_design_are_left_alone() {
    let pinned = V0_33_1_DAMAGED_STORE.replacen(
        "\"program\":\"claude\"",
        "\"program\":\"claude\",\"launch_binding\":{\"provenance\":{\"kind\":\"builtin\"}}",
        1,
    );
    let shell_without_reference = V0_33_1_DAMAGED_STORE
        .replace("\"agent\":\"claude\"", "\"agent\":\"shell\"")
        .replace("\"agent_base\":\"claude\"", "\"agent_base\":\"shell\"")
        .replace(",\"native_session_id\":\"native-fixture-1\"", "")
        .replace(",\"native_session_id\":\"native-fixture-2\"", "");
    let no_snapshot_program =
        V0_33_1_DAMAGED_STORE.replace("\"program\":\"claude\"", "\"program\":\"\"");
    for (tag, content) in [
        ("pinned", pinned.as_str()),
        ("shell", shell_without_reference.as_str()),
        ("no-program", no_snapshot_program.as_str()),
    ] {
        let (_store, path) = migrated(tag, content);
        let lines = raw_lines(&path);
        assert!(
            lines[0].get(UNRESOLVED_KEY).is_none(),
            "{tag}: {}",
            lines[0]
        );
        assert!(
            lines[0].get("native_launch").is_none(),
            "{tag}: {}",
            lines[0]
        );
    }
}

#[test]
fn a_store_already_at_the_current_schema_is_not_reinterpreted() {
    let current = raw_lines_of(V0_33_1_DAMAGED_STORE).into_iter().fold(
        String::new(),
        |mut store, mut line| {
            line["schema_version"] = json!(2);
            writeln!(store, "{line}").expect("write to a string");
            store
        },
    );
    let (store, path) = store_with("current-schema", &current);

    assert_eq!(
        store.migrate_to_current().expect("migrate"),
        super::SchemaMigration::UpToDate
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), current);
}

/// What the migration does to the recovery of each resume line of `content`,
/// judged on the stored bytes without applying it.
fn assessed(content: &str) -> Vec<super::legacy_binding::RecoveryOutcome> {
    raw_lines_of(content)
        .iter()
        .filter(|line| line["kind"] == "resume" || line["kind"] == "session")
        .map(|line| {
            super::legacy_binding::assess_recovery(line.as_object().expect("record object"))
        })
        .collect()
}

#[test]
fn the_recovery_assessment_matches_what_the_migration_does() {
    use super::legacy_binding::RecoveryOutcome::{Lost, NeedsRegistry, Preserved};

    // A v0.33.0 store maps to launch specs.
    assert_eq!(assessed(V0_33_0_STORE), [Preserved, Preserved]);
    // A v0.33.1 binding of a built-in agent is restored; a profile's needs the registry.
    assert!(assessed(V0_33_1_DAMAGED_STORE)
        .iter()
        .all(|outcome| *outcome == Preserved));
    let profile = V0_33_1_DAMAGED_STORE.replace("\"agent\":\"claude\"", "\"agent\":\"work\"");
    assert_eq!(assessed(&profile), [NeedsRegistry, NeedsRegistry]);
    // Frozen fields no release maps lose the recovery of a binding that has a reference.
    let unmappable = legacy_resume_line(
        "claude",
        "claude",
        &[],
        &json!({ "resume_mode": "no-such-mode", "ref_kind": "id", "resumable": true }),
    );
    assert_eq!(assessed(&unmappable), [Lost]);
    // A binding v0.33.0 itself did not recover has nothing to lose.
    let not_resumable = legacy_resume_line(
        "claude",
        "claude",
        &[],
        &json!({ "resume_mode": "flag", "ref_kind": "id", "resumable": false }),
    );
    assert_eq!(assessed(&not_resumable), [Preserved]);
}
