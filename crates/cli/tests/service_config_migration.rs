//! `pohunek service check` over a previous release's `service.toml`.
//!
//! The real CLI binary runs against an isolated XDG layout holding the exact
//! schema-2 configuration a v0.33.1 release wrote, the exact schema-3 shape
//! that precedes `[waits]`, the schema-4 shape that predates the input write
//! stall bound, the schema-5 shape that predates `[hooks]`, the schema-6 shape
//! that predates `[events]`, the schema-7 shape that predates
//! `namespace.session_tmp_root`, or a current-schema file. No service manager is
//! involved: the check of an existing installation contacts only the new
//! daemon's `upgrade-preflight`, which reads the store below the isolated
//! roots beside this build's binaries — so `pohunekd` is expected next to
//! `pohunek`, as `service_preflight.rs` already assumes.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-10-14

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

/// The schema-3 `service.toml` of a release before `[waits]`, with the
/// identity the isolated layout resolves. Its `[input]` values differ from
/// the defaults, as an operator may have tuned them.
const SCHEMA_THREE_TEMPLATE: &str = r#"
# Pohunek service configuration, written by `pohunek service install`.
# Every key is required; unknown keys are rejected.

schema_version = 3
prefix = "{PREFIX}"
active_version = "0.34.2"

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

[input]
initial_startup_grace_ms = 9000
submit_delay_ms = 275

[limits]
open_files = 8192
"#;

/// The `[waits]` table, inserted into the schema-3 template to build a
/// schema-4 or later file.
const WAITS_TABLE: &str = "[waits]\nturn_default_ms = 300000\nturn_max_ms = 1800000\n\n[limits]";

/// An isolated XDG layout where the real CLI checks the previous installation.
struct Host {
    /// The frozen `service.toml` shape the fixture starts from.
    template: &'static str,
    /// Overrides the fixture template before it is written; a schema the CLI
    /// neither writes nor migrates is exercised here.
    repair: Box<dyn Fn(&str) -> String>,
    temporary: tempfile::TempDir,
}

impl Host {
    /// The exact configuration a previous release could have written.
    fn schema_two() -> Self {
        Self {
            template: SCHEMA_TWO_TEMPLATE,
            repair: Box::new(std::borrow::ToOwned::to_owned),
            temporary: temp_root(),
        }
    }

    /// A schema-2 file whose installed binary already has this build's version.
    fn schema_two_current_version() -> Self {
        Self {
            template: SCHEMA_TWO_TEMPLATE,
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
            template: SCHEMA_TWO_TEMPLATE,
            repair: Box::new(|template| {
                template.replace("active_version = \"0.33.1\"", "active_version = \"0.33.0\"")
            }),
            temporary: temp_root(),
        }
    }

    /// A previous schema-3 installation whose active version needs an upgrade.
    fn schema_three_previous_version() -> Self {
        Self {
            template: SCHEMA_THREE_TEMPLATE,
            repair: Box::new(std::borrow::ToOwned::to_owned),
            temporary: temp_root(),
        }
    }

    /// A schema-4 file standing in for an installation that predates the
    /// input write stall bound, with the given `[waits]` values. Schema 4 was
    /// never released on its own.
    fn schema_four_with_waits(default_ms: u64, max_ms: u64) -> Self {
        Self {
            template: SCHEMA_THREE_TEMPLATE,
            repair: Box::new(move |template| waits_file(template, 4, default_ms, max_ms)),
            temporary: temp_root(),
        }
    }

    /// A schema-5 file standing in for an installation that predates
    /// `[hooks]`, with the given `[waits]` values. Schema 5 was never
    /// released on its own.
    fn schema_five_with_waits(default_ms: u64, max_ms: u64) -> Self {
        Self {
            template: SCHEMA_THREE_TEMPLATE,
            repair: Box::new(move |template| waits_file(template, 5, default_ms, max_ms)),
            temporary: temp_root(),
        }
    }

    /// A previous schema-6 installation, as the release before `[events]`
    /// wrote it, with the given `[waits]` values.
    fn schema_six_with_waits(default_ms: u64, max_ms: u64) -> Self {
        Self {
            template: SCHEMA_THREE_TEMPLATE,
            repair: Box::new(move |template| waits_file(template, 6, default_ms, max_ms)),
            temporary: temp_root(),
        }
    }

    /// A previous schema-7 installation, as the release before
    /// `namespace.session_tmp_root` wrote it, with the given `[waits]` values.
    fn schema_seven_with_waits(default_ms: u64, max_ms: u64) -> Self {
        Self {
            template: SCHEMA_THREE_TEMPLATE,
            repair: Box::new(move |template| waits_file(template, 7, default_ms, max_ms)),
            temporary: temp_root(),
        }
    }

    /// A current-schema file the `edit` rewrites after it is rendered.
    fn current_edited(edit: fn(String) -> String) -> Self {
        Self {
            template: SCHEMA_THREE_TEMPLATE,
            repair: Box::new(move |template| edit(waits_file(template, 8, 300_000, 1_800_000))),
            temporary: temp_root(),
        }
    }

    /// A current-schema file with the given `[waits]` values.
    fn current_with_waits(default_ms: u64, max_ms: u64) -> Self {
        Self {
            template: SCHEMA_THREE_TEMPLATE,
            repair: Box::new(move |template| waits_file(template, 8, default_ms, max_ms)),
            temporary: temp_root(),
        }
    }

    /// A current-schema file whose `[hooks]` table sets `required` to `entries`.
    fn current_with_required(entries: &'static str) -> Self {
        Self {
            template: SCHEMA_THREE_TEMPLATE,
            repair: Box::new(move |template| {
                waits_file(template, 8, 300_000, 1_800_000).replace(
                    "stderr_tail_bytes = 4096",
                    &format!("stderr_tail_bytes = 4096\nrequired = {entries}"),
                )
            }),
            temporary: temp_root(),
        }
    }

    /// The same configuration with `schema_version` replaced.
    fn schema_of(version: u32) -> Self {
        Self {
            template: SCHEMA_TWO_TEMPLATE,
            repair: Box::new(move |template| {
                template.replace("schema_version = 2", &format!("schema_version = {version}"))
            }),
            temporary: temp_root(),
        }
    }
}

/// Turns the schema-3 template into a schema-`version` file (4 or later) with
/// the given `[waits]` values: the input write stall bound exists from schema 5
/// on, `[hooks]` from schema 6 on, `[events]` from schema 7 on and
/// `namespace.session_tmp_root` from schema 8 on. The `active_version` is the
/// release that first shipped the schema: v0.35.0 for schema 6 and v0.36.0
/// for schema 7; schema 8 keeps the v0.36.0 installation, which an upgrade
/// replaces. Schemas 4 and 5 were never released on their own, so they carry
/// the v0.35.0 release that superseded them.
fn waits_file(template: &str, version: u32, default_ms: u64, max_ms: u64) -> String {
    let active_version = if version >= 7 { "0.36.0" } else { "0.35.0" };
    let template = template
        .replace("schema_version = 3", &format!("schema_version = {version}"))
        .replace(
            "active_version = \"0.34.2\"",
            &format!("active_version = \"{active_version}\""),
        );
    let template = if version >= 5 {
        template.replace(
            "submit_delay_ms = 275",
            "submit_delay_ms = 275\nwrite_stall_timeout_ms = 3000",
        )
    } else {
        template
    };
    let template = template.replace(
        "[limits]",
        &WAITS_TABLE
            .replace("300000", &default_ms.to_string())
            .replace("1800000", &max_ms.to_string()),
    );
    let template = if version >= 6 {
        template.replace("[limits]", "[hooks]\nstderr_tail_bytes = 4096\n\n[limits]")
    } else {
        template
    };
    let template = if version >= 7 {
        template.replace(
            "[limits]",
            "[events]\nmax_file_bytes = 33554432\nmax_files = 8\nmax_age_days = 14\n\n[limits]",
        )
    } else {
        template
    };
    if version >= 8 {
        template.replace(
            "runtime_root = \"{RUNTIME_ROOT}\"",
            "runtime_root = \"{RUNTIME_ROOT}\"\nsession_tmp_root = \"{SESSION_TMP_ROOT}\"",
        )
    } else {
        template
    }
}

fn temp_root() -> tempfile::TempDir {
    pohunek_test_support::tempdir_with_prefix("phk-").expect("temporary root")
}

/// The session bus a Linux child may reach: the real user manager's, in the
/// real-systemd lane (`POHUNEK_SYSTEMD_E2E=1`) only. Everywhere else the child
/// gets none, and a daemon job on Linux is judged with its user manager, so
/// the check refuses.
fn real_bus_address() -> Option<String> {
    if !cfg!(target_os = "linux")
        || std::env::var_os("POHUNEK_SYSTEMD_E2E").is_none_or(|v| v != "1")
    {
        return None;
    }
    std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().or_else(|| {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(|dir| format!("unix:path={}/bus", dir.to_string_lossy()))
    })
}

/// [`check`] for a scenario that needs the check to pass. On Linux without a
/// reachable user manager the check is refused instead, which is asserted here
/// and ends the scenario; the real-systemd lane runs it through.
fn check_passing(host: &Host) -> Option<(bool, Value)> {
    let (success, envelope) = check(host);
    if cfg!(target_os = "linux") && real_bus_address().is_none() {
        assert!(!success, "{envelope}");
        let refusal = envelope.get("err").expect("the refusal document");
        assert_eq!(
            refusal.get("code").and_then(Value::as_str),
            Some("service_manager_environment_unavailable"),
            "{refusal:?}"
        );
        return None;
    }
    Some((success, envelope))
}

/// Builds the fixture and runs `service check --json` against it.
fn check(host: &Host) -> (bool, Value) {
    check_with_setup(host, |_, _, _| {})
}

/// Runs the real check after a fixture has set up an interrupted transaction.
fn check_with_setup(host: &Host, setup: impl FnOnce(&Path, &Path, &[u8])) -> (bool, Value) {
    check_in(host, None, "check", setup)
}

/// [`check_with_setup`] with the cache directory the child resolves
/// (`XDG_CACHE_HOME`) replaced by `cache_home`, running `service <command>`.
fn check_in(
    host: &Host,
    cache_home: Option<&Path>,
    command: &str,
    setup: impl FnOnce(&Path, &Path, &[u8]),
) -> (bool, Value) {
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
    let template = (host.repair)(host.template)
        .replace("{PREFIX}", &root.join("prefix").display().to_string())
        .replace("{UID}", &nix::unistd::Uid::effective().as_raw().to_string())
        .replace("{STATE_ROOT}", &state.join("pohunek").display().to_string())
        .replace("{RUNTIME_ROOT}", &run.join("pohunek").display().to_string())
        .replace(
            "{SESSION_TMP_ROOT}",
            &root
                .join("cache")
                .join("pohunek")
                .join("session-tmp")
                .display()
                .to_string(),
        );
    fs::write(&config, template).expect("write fixture, a checked-in value");
    // The previous release wrote the file owner-private.
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).expect("private service.toml");
    let source = fs::read(&config).expect("read the exact source bytes");
    setup(&config, &state.join("pohunek"), &source);
    let fixture = fs::read(&config).expect("read the exact fixture bytes");
    let store = Store::new(state.join("pohunek"));
    let record_before = fs::read(store.path()).ok();
    let backup_before = fs::read(store.config_backup_path()).ok();

    let env_of = |base: &Path| base.to_str().expect("UTF-8 roots").to_owned();
    // The child gets only what the fixture names: an inherited session bus
    // address would reach the host's real user manager.
    let mut output = Command::new(pohunek_test_support::bin_exe("pohunek"));
    let output = output
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["service", command, "--json"])
        .env("XDG_RUNTIME_DIR", env_of(&run))
        .env("XDG_STATE_HOME", env_of(&state))
        .env("XDG_DATA_HOME", env_of(&root.join("data")))
        .env(
            "XDG_CACHE_HOME",
            env_of(&cache_home.map_or_else(|| root.join("cache"), Path::to_path_buf)),
        )
        .env("XDG_CONFIG_HOME", env_of(&config_home))
        .env("HOME", env_of(&root.join("home")));
    let output = match real_bus_address() {
        Some(address) => output.env("DBUS_SESSION_BUS_ADDRESS", address),
        None => output,
    }
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
        let cache_dir = state
            .parent()
            .and_then(Path::parent)
            .expect("the fixture root")
            .join("cache")
            .join("pohunek");
        let mut spec =
            pohunek_service_config::ServiceConfig::load_upgrade_source(config, Some(&cache_dir))
                .expect("schema-2 source")
                .config
                .to_spec();
        VERSION.clone_into(&mut spec.active_version);
        pohunek_service_config::ServiceConfig::new(spec)
            .expect("target configuration")
            .write(config)
            .expect("write current-schema target");
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
        runtime_stop: false,
        runtime_backup: None,
        launchd_default_path: None,
        launchd_boot_identity: None,
        accepted_risks: None,
        refresh: None,
    };
    store.save(&record).expect("journal interrupted migration");
}

#[test]
fn check_accepts_the_previous_release_config_and_names_the_migration() {
    let host = Host::schema_two_previous_version();
    let Some((success, envelope)) = check_passing(&host) else {
        return;
    };
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
fn check_accepts_a_schema_three_config_and_names_the_migration() {
    let host = Host::schema_three_previous_version();
    let Some((success, envelope)) = check_passing(&host) else {
        return;
    };
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
        Some(3),
        "{report:?}: the check names the schema-3 migration"
    );
}

#[test]
fn check_accepts_a_schema_four_config_and_names_the_migration() {
    let host = Host::schema_four_with_waits(120_000, 2_700_000);
    let Some((success, envelope)) = check_passing(&host) else {
        return;
    };
    assert!(success, "{envelope}");
    assert_eq!(
        envelope
            .get("ok")
            .and_then(|report| report.get("config_schema_migration"))
            .and_then(Value::as_u64),
        Some(4),
        "{envelope}: the check names the schema-4 migration"
    );
}

#[test]
fn check_accepts_a_schema_five_config_and_names_the_migration() {
    let host = Host::schema_five_with_waits(120_000, 2_700_000);
    let Some((success, envelope)) = check_passing(&host) else {
        return;
    };
    assert!(success, "{envelope}");
    assert_eq!(
        envelope
            .get("ok")
            .and_then(|report| report.get("config_schema_migration"))
            .and_then(Value::as_u64),
        Some(5),
        "{envelope}: the check names the schema-5 migration"
    );
}

#[test]
fn check_accepts_a_schema_six_config_and_names_the_migration() {
    let host = Host::schema_six_with_waits(120_000, 2_700_000);
    let Some((success, envelope)) = check_passing(&host) else {
        return;
    };
    assert!(success, "{envelope}");
    assert_eq!(
        envelope
            .get("ok")
            .and_then(|report| report.get("config_schema_migration"))
            .and_then(Value::as_u64),
        Some(6),
        "{envelope}: the check names the schema-6 migration"
    );
}

#[test]
fn check_accepts_a_schema_seven_config_and_names_the_migration() {
    let host = Host::schema_seven_with_waits(120_000, 2_700_000);
    let Some((success, envelope)) = check_passing(&host) else {
        return;
    };
    assert!(success, "{envelope}");
    assert_eq!(
        envelope
            .get("ok")
            .and_then(|report| report.get("config_schema_migration"))
            .and_then(Value::as_u64),
        Some(7),
        "{envelope}: the check names the schema-7 migration"
    );
}

/// The refusal document of a check that must fail on the configuration, with
/// the asserted key named in its message.
fn assert_config_refusal_naming(success: bool, envelope: &Value, key: &str) {
    assert!(!success, "{envelope}: the check refuses the file");
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
            .is_some_and(|message| message.contains(key)),
        "{refusal:?}: the refusal names {key}"
    );
}

#[test]
fn check_refuses_a_current_schema_config_without_a_session_tmp_root_naming_the_key() {
    let host = Host::current_edited(|text| {
        text.replace("session_tmp_root = \"{SESSION_TMP_ROOT}\"\n", "")
    });
    let (success, envelope) = check(&host);
    assert_config_refusal_naming(success, &envelope, "session_tmp_root");
}

#[test]
fn check_refuses_a_relative_session_tmp_root_naming_the_key() {
    let host =
        Host::current_edited(|text| text.replace("\"{SESSION_TMP_ROOT}\"", "\"relative/tmp\""));
    let (success, envelope) = check(&host);
    assert_config_refusal_naming(success, &envelope, "namespace.session_tmp_root");
}

/// The migration preview derives the root of a schema-7 file from the cache
/// directory and refuses a cache directory that cannot hold a recorded path.
#[test]
fn check_refuses_a_schema_seven_config_when_the_cache_directory_cannot_hold_the_derived_root() {
    let host = Host::schema_seven_with_waits(120_000, 2_700_000);
    let mut cache_home = host.temporary.path().join("cache");
    // More than the 4096 bytes a recorded path may have.
    for _ in 0..24 {
        cache_home.push("c".repeat(200));
    }
    let (success, envelope) = check_in(&host, Some(&cache_home), "check", |_, _, _| {});
    assert_config_refusal_naming(success, &envelope, "namespace.session_tmp_root");
}

/// An interrupted `refresh-path` of a schema-7 installation is judged by the
/// adoption gate with the recorded search path the rollback restores; the
/// judged copy revalidates every key, so the gate derives the session
/// temporary root for it instead of failing on an unrecorded one.
#[test]
fn check_judges_a_pending_refresh_over_a_schema_seven_config_without_a_root_error() {
    let host = Host::schema_seven_with_waits(120_000, 2_700_000);
    let (_success, envelope) = check_with_setup(&host, |config, state, source| {
        let digest = format!("{:x}", sha2::Sha256::digest(source));
        let record = Record {
            schema_version: pohunek_cli::service::record::SCHEMA_VERSION,
            operation: Operation::RefreshPath,
            version: "0.36.0".to_owned(),
            prefix: config
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .expect("fixture root")
                .join("prefix"),
            previous_version: Some("0.36.0".to_owned()),
            version_dir_preexisted: true,
            config_backup: None,
            step: Step::Started,
            rolling_back: false,
            runtime_stop: false,
            runtime_backup: None,
            launchd_default_path: None,
            launchd_boot_identity: None,
            accepted_risks: None,
            refresh: Some(pohunek_cli::service::record::RefreshJournal {
                previous: vec!["/usr/bin".into()],
                target: vec!["/usr/bin".into(), "/bin".into()],
                digest_before: digest.clone(),
                digest_after: digest,
                daemon_before: None,
            }),
        };
        Store::new(state.to_path_buf())
            .save(&record)
            .expect("journal the interrupted refresh");
    });
    let message = envelope
        .get("err")
        .and_then(|refusal| refusal.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        !message.contains("session_tmp_root"),
        "{envelope}: the gate does not fail on an unrecorded root"
    );
}

/// A command that never writes the file reads a schema-7 file whatever the
/// cache directory is: the cache directory only matters to the migration.
#[test]
fn status_reads_a_schema_seven_config_whatever_the_cache_directory() {
    let host = Host::schema_seven_with_waits(120_000, 2_700_000);
    let mut cache_home = host.temporary.path().join("cache");
    // More than the 4096 bytes a recorded path may have.
    for _ in 0..24 {
        cache_home.push("c".repeat(200));
    }
    let (_success, envelope) = check_in(&host, Some(&cache_home), "status", |_, _, _| {});
    let code = envelope
        .get("err")
        .and_then(|refusal| refusal.get("code"))
        .and_then(Value::as_str);
    assert_ne!(
        code,
        Some("service_config_invalid"),
        "{envelope}: the file is read without the cache directory"
    );
}

#[test]
fn check_accepts_a_current_schema_config_without_a_migration() {
    let host = Host::current_with_waits(120_000, 2_700_000);
    let Some((success, envelope)) = check_passing(&host) else {
        return;
    };
    assert!(success, "{envelope}");
    assert!(
        envelope
            .get("ok")
            .and_then(|report| report.get("config_schema_migration"))
            .is_some_and(Value::is_null),
        "{envelope}: a current-schema file needs no migration"
    );
}

#[test]
fn check_refuses_a_default_turn_wait_above_its_ceiling() {
    let host = Host::current_with_waits(1_800_001, 1_800_000);
    let (success, envelope) = check(&host);
    assert!(!success, "{envelope}: the default exceeds the ceiling");
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
            .is_some_and(|message| message.contains("waits.turn_default_ms")
                && message.contains("waits.turn_max_ms")),
        "{refusal:?}: the refusal names both keys"
    );
}

#[test]
fn check_refuses_a_turn_wait_outside_its_bounds() {
    for (default_ms, max_ms, key) in [
        (4_999, 1_800_000, "waits.turn_default_ms"),
        (300_000, 3_600_001, "waits.turn_max_ms"),
    ] {
        let host = Host::current_with_waits(default_ms, max_ms);
        let (success, envelope) = check(&host);
        assert!(!success, "{envelope}: {key} is out of range");
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
                .is_some_and(|message| message.contains(key) && message.contains("5000..=3600000")),
            "{refusal:?}: the refusal names {key} and its bounds"
        );
    }
}

#[test]
fn check_refuses_an_invalid_required_hook_list_naming_the_key() {
    for entries in [
        "[\"after-create\"]",
        "[\"pre-remove\"]",
        "[\"post-create\", \"post-create\"]",
    ] {
        let host = Host::current_with_required(entries);
        let (success, envelope) = check(&host);
        assert!(!success, "{envelope}: {entries} is refused");
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
                .is_some_and(|message| message.contains("hooks.required")),
            "{refusal:?}: the refusal names the key"
        );
    }
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
    for version in [1, 9] {
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
fn check_reports_pending_migration_after_the_current_schema_write() {
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

/// Replaces the rendered header of the current-schema target with the wording
/// an older release wrote, optionally changing one body value.
fn rewrite_target_header(config: &Path, alter_body: bool) {
    let text = fs::read_to_string(config).expect("read the written target");
    let body =
        String::from_utf8(pohunek_service_config::body_after_header(text.as_bytes()).to_vec())
            .expect("body is text");
    let body = if alter_body {
        body.replacen("grace_ms = 5000", "grace_ms = 5001", 1)
    } else {
        body
    };
    let old = "# Pohunek service configuration, written by `pohunek service install`.\n\
# Every key is required; unknown keys are rejected.\n\n";
    fs::write(config, format!("{old}{body}")).expect("rewrite the target");
}

#[test]
fn a_pending_migration_resumes_across_a_header_wording_change_but_not_a_body_change() {
    for alter_body in [false, true] {
        let host = Host::schema_two();
        let (success, envelope) = check_with_setup(&host, |config, state, source| {
            pending_migration(config, state, source, Step::Registered, Some(source));
            rewrite_target_header(config, alter_body);
        });
        if alter_body {
            assert!(!success, "{envelope}");
            assert_eq!(
                envelope
                    .get("err")
                    .and_then(|err| err.get("code"))
                    .and_then(Value::as_str),
                Some("service_config_backup_invalid"),
                "{envelope}"
            );
        } else {
            assert!(success, "{envelope}");
            assert_eq!(
                envelope
                    .get("ok")
                    .and_then(|ok| ok.get("pending_action"))
                    .and_then(Value::as_str),
                Some("resume"),
                "{envelope}"
            );
        }
    }
}
