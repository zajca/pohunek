//! Schema-6 store migration through the public daemon store boundary.

// Rust guideline compliant 2026-10-12

use std::ffi::OsString;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use pohunek_daemon::agent::{LaunchView, ReferenceCheckFailure, SessionRef};
use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};

/// A schema-6 resume binding of a Pi package 1.0.1 session: its frozen check
/// knows only the default session store.
const SCHEMA_6: &str = include_str!("../src/store/fixtures/schema-6/metadata.jsonl");

/// The reference the fixture binding holds.
const REFERENCE: &str = "0197aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee";

#[test]
fn a_schema_6_binding_migrates_and_keeps_the_check_it_was_launched_with() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-6 store")
        .write_all(SCHEMA_6.as_bytes())
        .expect("write frozen schema-6 store");
    let store = Store::new(path.clone());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from, to, records, ..
    } = migrated
    else {
        panic!("schema-6 store must migrate");
    };
    assert_eq!((from, to, records), (6, STORE_SCHEMA_VERSION, 1));
    let stored = std::fs::read_to_string(&path).expect("read migrated store");
    let line: serde_json::Value = serde_json::from_str(stored.trim()).expect("stored JSON");
    assert_eq!(line["schema_version"], STORE_SCHEMA_VERSION);
    let existence = &line["native_launch"]["assigned"]["existence"];
    for field in [
        "store_env",
        "store_max_depth",
        "relocating_args",
        "relocating_settings",
    ] {
        assert!(
            existence.get(field).is_none(),
            "the migration adds no {field}"
        );
    }

    let bindings = store.load_resume().expect("load migrated binding");
    assert_eq!(bindings.len(), 1);
    let check = bindings[0]
        .native_launch
        .as_ref()
        .and_then(|launch| launch.assigned())
        .expect("the launch reference is assigned")
        .existence()
        .clone();

    // The session-dir variable names a store holding the conversation, but
    // the frozen check searches only the agent directory's default store.
    let agent_dir = pohunek_test_support::tempdir().expect("agent dir");
    let elsewhere = pohunek_test_support::tempdir().expect("session dir");
    std::fs::write(
        elsewhere
            .path()
            .join(format!("2026-10-12T00-00-00-000Z_{REFERENCE}.jsonl")),
        b"{}\n",
    )
    .expect("write the session file");
    std::fs::write(
        agent_dir.path().join("settings.json"),
        br#"{"sessionDir": "/elsewhere"}"#,
    )
    .expect("write settings naming another directory");
    let (agent, session_dir) = (agent_dir.path().to_owned(), elsewhere.path().to_owned());
    let env = move |name: &str| -> Option<OsString> {
        match name {
            "PI_CODING_AGENT_DIR" => Some(agent.clone().into_os_string()),
            "PI_CODING_AGENT_SESSION_DIR" => Some(session_dir.clone().into_os_string()),
            _ => None,
        }
    };
    let reference = SessionRef::id(REFERENCE).expect("reference");
    assert_eq!(
        check.verify(&reference, &env),
        Err(ReferenceCheckFailure::Missing)
    );
    let unset = |name: &str| -> Option<OsString> {
        (name == "PI_CODING_AGENT_DIR").then(|| agent_dir.path().as_os_str().to_owned())
    };
    assert_eq!(
        check.check_relocation(&LaunchView {
            env: &unset,
            args: &["--session-dir".to_owned(), "/elsewhere".to_owned()],
            cwd: Path::new("/fixture"),
        }),
        Ok(()),
        "the frozen check refuses no relocation"
    );
}
