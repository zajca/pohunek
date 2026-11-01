//! The durable session policy file is read strictly: a key this build does not
//! know fails the load instead of being dropped, while the same document
//! without it loads.

// Rust guideline compliant 2026-11-01

use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use pohunek_daemon::session::{SessionRegistry, RETENTION_POLICY_NAME};
use protocol::SessionRetentionPolicy;
use serde_json::Value;

mod support;

fn write_policy_file(dir: &Path, document: &Value) {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .expect("make the policy directory private");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join(RETENTION_POLICY_NAME))
        .expect("create the policy file");
    std::io::Write::write_all(&mut file, format!("{document}\n").as_bytes())
        .expect("write the policy file");
}

fn registry_reading(dir: &Path) -> SessionRegistry {
    let mut config = support::hermetic_registry_config();
    config.retention_policy_path = Some(dir.join(RETENTION_POLICY_NAME));
    SessionRegistry::new(config)
}

#[tokio::test]
async fn a_session_policy_file_with_an_unknown_key_fails_to_load() {
    let root = pohunek_test_support::tempdir().expect("private policy root");
    let policy = SessionRetentionPolicy {
        enabled: true,
        ..SessionRetentionPolicy::default()
    };
    let mut document = serde_json::to_value(&policy).expect("policy JSON");

    write_policy_file(root.path(), &document);
    let loaded = registry_reading(root.path())
        .load_retention_policy()
        .await
        .expect("the document without an unknown key loads");
    assert_eq!(loaded, policy);

    document["a_key_from_a_newer_daemon"] = Value::from(1);
    write_policy_file(root.path(), &document);
    let refused = registry_reading(root.path())
        .load_retention_policy()
        .await
        .expect_err("a key this build does not know fails the load");
    assert!(
        refused.msg.contains("a_key_from_a_newer_daemon"),
        "the error names the key: {refused:?}"
    );
}
