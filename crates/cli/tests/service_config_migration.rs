//! `pohunek service check` over the previous release's `service.toml`.
//!
//! The real CLI binary runs against an isolated XDG layout holding the exact
//! schema-2 configuration a v0.33.1 release wrote. No service manager is
//! involved: the check of an existing installation contacts only the new
//! daemon's `upgrade-preflight`, which reads the store below the isolated
//! roots beside this build's binaries — so `pohunekd` is expected next to
//! `pohunek`, as `service_preflight.rs` already assumes.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-10-09

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use pohunek_cli::service::record::{ConfigBackup, Operation, Record, Step, Store};
use pohunek_cli::service::VERSION;
use serde_json::Value;
use sha2::Digest as _;

/// The schema-2 `service.toml` of a v0.33.1 install, with the identity the
/// isolated layout resolves. The `[input]` table that schema 3 adds does not
/// exist yet.
const SCHEMA_TWO_TEMPLATE: &str = r#"
# Pohunek service configuration, written by `pohunek service install`.
# Every key is required; unknown keys are rejected.

schema_version = 2
prefix = "{PREFIX}"
active_version = "0.33.1"

[namespace]
uid = {UID}
state_root = "{STATE_ROOT}"
runtime_root = "{RUNTIME_ROOT}"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/usr/bin"]
allowlist = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_*"]

[sweep]
grace_ms = 5000

[limits]
open_files = 8192
"#;

/// An isolated XDG layout where the real CLI checks the previous installation.
struct Host {
    /// Overrides the fixture template before it is written; a schema the CLI
    /// neither writes nor migrates is exercised here.
    repair: Box<dyn Fn(&str) -> String>,
    temporary: tempfile::TempDir,
}

impl Host {
    /// The exact configuration a previous release could have written.
    fn schema_two() -> Self {
        Self {
            repair: Box::new(std::borrow::ToOwned::to_owned),
            temporary: temp_root(),
        }
    }

    /// A schema-2 file whose installed binary already has this build's version.
    fn schema_two_current_version() -> Self {
        Self {
            repair: Box::new(|template| {
                template.replace(
                    "active_version = \"0.33.1\"",
                    &format!("active_version = \"{VERSION}\""),
                )
            }),
            temporary: temp_root(),
        }
    }

    /// A previous schema-2 installation whose active version needs an upgrade.
    fn schema_two_previous_version() -> Self {
        Self {
            repair: Box::new(|template| {
                template.replace("active_version = \"0.33.1\"", "active_version = \"0.33.0\"")
            }),
            temporary: temp_root(),
        }
    }

    /// The same configuration with `schema_version` replaced.
    fn schema_of(version: u32) -> Self {
        Self {
            repair: Box::new(move |template| {
                template.replace("schema_version = 2", &format!("schema_version = {version}"))
            }),
            temporary: temp_root(),
        }
    }
}

fn temp_root() -> tempfile::TempDir {
    pohunek_test_support::tempdir_with_prefix("phk-").expect("temporary root")
}

/// Builds the fixture and runs `service check --json` against it.
fn check(host: &Host) -> (bool, Value) {
    check_with_setup(host, |_, _, _| {})
}

/// Runs the real check after a fixture has set up an interrupted transaction.
fn check_with_setup(host: &Host, setup: impl FnOnce(&Path, &Path, &[u8])) -> (bool, Value) {
    let root = host.temporary.path();
    let dir = |name: &str| {
        let path = root.join(name);
        fs::create_dir_all(&path).expect("create XDG root");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("private XDG root");
        path
    };

    // Every root the real binary canonicalizes, all owner-private.
    let run = dir("run");
    fs::create_dir_all(run.join("pohunek")).expect("runtime root");
    fs::set_permissions(run.join("pohunek"), fs::Permissions::from_mode(0o700))
        .expect("private runtime root");
    let state = dir("state");
    fs::create_dir_all(state.join("pohunek")).expect("state root");
    fs::set_permissions(state.join("pohunek"), fs::Permissions::from_mode(0o700))
        .expect("private state root");
    dir("data");
    dir("cache");
    let config_home = dir("config");
    dir("home");
    // Created so the trusted-ancestor lookups of the check find a real tree;
    // the fixture names it.
    dir("prefix");

    // The application config directory, where the previous release wrote.
    let config_dir = config_home.join("pohunek");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
        .expect("private config dir");
    let config = config_dir.join("service.toml");
    let template = (host.repair)(SCHEMA_TWO_TEMPLATE)
        .replace("{PREFIX}", &root.join("prefix").display().to_string())
        .replace("{UID}", &nix::unistd::Uid::effective().as_raw().to_string())
        .replace("{STATE_ROOT}", &state.join("pohunek").display().to_string())
        .replace("{RUNTIME_ROOT}", &run.join("pohunek").display().to_string());
    fs::write(&config, template).expect("write fixture, a checked-in value");
    // The previous release wrote the file owner-private.
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).expect("private service.toml");
    let source = fs::read(&config).expect("read the exact schema-2 bytes");
    setup(&config, &state.join("pohunek"), &source);
    let fixture = fs::read(&config).expect("read the exact fixture bytes");
    let store = Store::new(state.join("pohunek"));
    let record_before = fs::read(store.path()).ok();
    let backup_before = fs::read(store.config_backup_path()).ok();

    let env_of = |base: &Path| base.to_str().expect("UTF-8 roots").to_owned();
    let output = Command::new(pohunek_test_support::bin_exe("pohunek"))
        .args(["service", "check", "--json"])
        .env("XDG_RUNTIME_DIR", env_of(&run))
        .env("XDG_STATE_HOME", env_of(&state))
        .env("XDG_DATA_HOME", env_of(&root.join("data")))
        .env("XDG_CACHE_HOME", env_of(&root.join("cache")))
        .env("XDG_CONFIG_HOME", env_of(&config_home))
        .env("HOME", env_of(&root.join("home")))
        .output()
        .expect("run the real CLI");
    let json: Value = serde_json::from_str(
        String::from_utf8(output.stdout)
            .expect("stdout is text")
            .trim(),
    )
    .expect("the CLI prints exactly one JSON document");

    // The read-only check must have left the fixture byte-exact.
    assert_eq!(
        fs::read(&config).expect("read service.toml back"),
        fixture,
        "the check writes nothing"
    );
    assert_eq!(fs::read(store.path()).ok(), record_before);
    assert_eq!(fs::read(store.config_backup_path()).ok(), backup_before);
    (output.status.success(), json)
}

/// Journals a previous-release configuration migration at a native boundary.
fn pending_migration(
    config: &Path,
    state: &Path,
    source: &[u8],
    step: Step,
    backup: Option<&[u8]>,
) {
    let store = Store::new(state.to_path_buf());
    if let Some(bytes) = backup {
        store.write_config_backup(bytes).expect("write backup");
    }
    if step >= Step::Config {
        let mut spec = pohunek_service_config::ServiceConfig::load_upgrade_source(config)
            .expect("schema-2 source")
            .config
            .to_spec();
        VERSION.clone_into(&mut spec.active_version);
        pohunek_service_config::ServiceConfig::new(spec)
            .expect("target configuration")
            .write(config)
            .expect("write schema-3 target");
    }
    let record = Record {
        schema_version: pohunek_cli::service::record::SCHEMA_VERSION,
        operation: Operation::Upgrade,
        version: VERSION.to_owned(),
        prefix: config
            .parent()
            .expect("config directory")
            .parent()
            .expect("config root")
            .parent()
            .expect("fixture root")
            .join("prefix"),
        previous_version: Some("0.33.1".to_owned()),
        version_dir_preexisted: false,
        config_backup: Some(ConfigBackup {
            source_schema_version: 2,
            digest: format!("{:x}", sha2::Sha256::digest(source)),
        }),
        step,
        rolling_back: false,
    };
    store.save(&record).expect("journal interrupted migration");
}

#[test]
fn check_accepts_the_previous_release_config_and_names_the_migration() {
    let host = Host::schema_two_previous_version();
    let (success, envelope) = check(&host);
    assert!(success, "{envelope}");
    let report = envelope
        .get("ok")
        .expect("the checked upgrade is the report");
    assert_eq!(
        report.get("operation").and_then(Value::as_str),
        Some("upgrade"),
        "{report:?}"
    );
    assert_eq!(
        report
            .get("config_schema_migration")
            .and_then(Value::as_u64),
        Some(2),
        "{report:?}: the check names the schema-2 migration"
    );
    assert_eq!(
        report.get("config_path").and_then(Value::as_str),
        Some(
            host.temporary
                .path()
                .join("config/pohunek/service.toml")
                .to_str()
                .expect("absolute path")
        ),
        "{report:?}"
    );
}

#[test]
fn check_reports_no_migration_when_the_installed_version_is_already_active() {
    let host = Host::schema_two_current_version();
    let (success, envelope) = check(&host);
    assert!(success, "{envelope}");
    assert!(
        envelope
            .get("ok")
            .and_then(|report| report.get("config_schema_migration"))
            .is_some_and(Value::is_null),
        "{envelope}: a same-version refresh leaves the schema-2 file in place"
    );
}

#[test]
fn check_refuses_a_config_this_version_neither_writes_nor_migrates() {
    for version in [1, 4] {
        let host = Host::schema_of(version);
        let (success, envelope) = check(&host);
        assert!(!success, "{envelope}: schema {version} is not a source");
        let refusal = envelope.get("err").expect("the refusal document");
        assert_eq!(
            refusal.get("code").and_then(Value::as_str),
            Some("service_config_invalid"),
            "{refusal:?}"
        );
        assert!(
            refusal
                .get("msg")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains(&format!("schema_version {version}"))),
            "{refusal:?}: the refusal names the found schema"
        );
    }
}

#[test]
fn check_refuses_an_unverifiable_pending_migration_before_contacting_the_manager() {
    for step in [Step::StopDaemon, Step::Registered] {
        for altered in [false, true] {
            let host = Host::schema_two();
            let (success, envelope) = check_with_setup(&host, |config, state, source| {
                let backup = altered.then(|| {
                    let mut bytes = source.to_vec();
                    bytes[5] ^= 1;
                    bytes
                });
                pending_migration(config, state, source, step, backup.as_deref());
            });
            assert!(!success, "{step:?}, altered={altered}: {envelope}");
            let refusal = envelope.get("err").expect("the refusal document");
            assert_eq!(
                refusal.get("code").and_then(Value::as_str),
                Some("service_config_backup_invalid"),
                "{step:?}, altered={altered}: {refusal:?}"
            );
        }
    }
}

#[test]
fn check_reports_pending_migration_after_the_schema_three_write() {
    let host = Host::schema_two();
    let (success, envelope) = check_with_setup(&host, |config, state, source| {
        pending_migration(config, state, source, Step::Registered, Some(source));
    });
    assert!(success, "{envelope}");
    let report = envelope.get("ok").expect("the checked upgrade");
    assert_eq!(
        report.get("pending_action").and_then(Value::as_str),
        Some("resume"),
        "{report:?}"
    );
    assert_eq!(
        report
            .get("config_schema_migration")
            .and_then(Value::as_u64),
        Some(2),
        "{report:?}: the journal still describes a schema-2 migration"
    );
}
