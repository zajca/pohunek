//! `pohunek service upgrade` with live workers against the real native manager.
//!
//! One build has one version, so the test installs this build as the real
//! version A and upgrades to version B, a second directory holding the same
//! build. The engine's `test-util` override lets the staged binaries report A
//! while they are installed as B; staging, the transaction journal,
//! `service.toml`, the daemon replacement, readiness, and version GC all run
//! unchanged.
//!
//! Linux drives the systemd user manager and is ignored unless
//! `POHUNEK_SYSTEMD_E2E=1` opts in; macOS drives `gui/<uid>` in the ordinary
//! test run. See `support/native.rs` for isolation and binary lookup.

#![cfg(any(target_os = "linux", target_os = "macos"))]

// Rust guideline compliant 2026-09-24

#[path = "support/native.rs"]
mod native;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::PathBuf;

use native::{connect, eventually, Installation};
use pohunek_cli::service::definition::{daemon_definition, with_version};
use pohunek_cli::service::layout;
use pohunek_cli::service::record::{ConfigBackup, Operation, Record, Step, Store};
use pohunek_cli::service::{Backend, Engine, VERSION};
use pohunek_platform::process::{HostInspector, ProcessIdentity, ProcessInspector as _};
use pohunek_platform::supervisor::ServiceObservation;
use pohunek_service_config::DEFAULT_INPUT_TIMING;

/// Version directory the upgrade installs this build under.
const UPGRADE_SUFFIX: &str = "-upgrade";

/// Live identity of one worker generation and its PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Runtime {
    job: String,
    worker: ProcessIdentity,
    executable: PathBuf,
    child: ProcessIdentity,
    tty: String,
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn upgrade_with_live_workers_keeps_their_pty_and_version() {
    let mut installation = Installation::new();
    // The upgraded daemon runs this build, which reports the real version
    // rather than the upgrade's directory name; teardown must accept it.
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let prefix = installation.root.join("prefix");
    let old = VERSION.to_owned();
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    let versions = prefix.join("libexec/pohunek");

    Engine::new(&installation.context, &backend)
        .install(&from, &prefix, &old)
        .await
        .expect("install version A");
    let old_daemon = daemon_process(&backend).await;
    let sessions = [
        installation.new_session("upgrade-a").await,
        installation.new_session("upgrade-b").await,
    ];
    let before = runtimes(&installation, &backend, &sessions).await;
    for runtime in &before {
        assert_eq!(
            runtime.executable,
            versions.join(&old).join("pohunek-sessiond")
        );
    }

    let report = Engine::new(&installation.context, &backend)
        .with_reported_version(old.clone())
        .upgrade(&from, &new)
        .await
        .expect("upgrade to version B");
    assert_eq!(
        (report.from_version.as_str(), report.to_version.as_str()),
        (old.as_str(), new.as_str())
    );
    assert!(!report.unchanged);
    assert_eq!(report.gc_error, None);
    assert!(
        report.kept_versions.iter().any(|kept| kept.version == old),
        "GC keeps the version live workers run: {report:?}"
    );
    assert!(!report.removed_versions.contains(&old));
    assert!(versions.join(&old).join("pohunek-sessiond").is_file());

    // Both directories hold one build, so readiness cannot tell the daemons
    // apart; the restart is observed through the job and the process image.
    let daemon_executable = versions.join(&new).join("pohunekd");
    let new_daemon = eventually("the daemon restarted from version B", || async {
        let daemon = backend.daemon().inspect().await.ok()?;
        let process = daemon.process.filter(|process| *process != old_daemon)?;
        (daemon
            .definition
            .as_ref()
            .map(|facts| facts.executable.as_path())
            == Some(daemon_executable.as_path())
            && executable_of(process.pid).as_deref() == Some(daemon_executable.as_path()))
        .then_some(process)
    })
    .await;
    installation.client().await;
    eprintln!("daemon {} -> {}", old_daemon.pid, new_daemon.pid);

    let after = runtimes(&installation, &backend, &sessions).await;
    assert_eq!(after, before, "workers kept PID, child, PTY, and version A");

    let fresh = installation.new_session("upgrade-c").await;
    let fresh = runtimes(&installation, &backend, &[fresh]).await;
    assert_eq!(
        fresh[0].executable,
        versions.join(&new).join("pohunek-sessiond")
    );
}

/// Frozen schema-2 shape from the previous service writer. The placeholders
/// supply this installation's identity; no schema-3 serializer supplies the
/// expected bytes of the migration.
const SCHEMA_TWO_TEMPLATE: &str = r#"# Pohunek service configuration, written by `pohunek service install`.
# Every key is required; unknown keys are rejected.

schema_version = 2
prefix = "{PREFIX}"
active_version = "{VERSION}"

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
allowlist = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_*", "XDG_*"]

[sweep]
grace_ms = 5000

[limits]
open_files = 8192
"#;

/// A schema-2 `service.toml` for this isolated installation. Tests capture
/// these bytes before the transaction and compare them after exact restore.
fn schema_two_config(installation: &Installation) -> Vec<u8> {
    SCHEMA_TWO_TEMPLATE
        .replace("{PREFIX}", &installation.prefix().display().to_string())
        .replace("{VERSION}", VERSION)
        .replace("{UID}", &nix::unistd::Uid::effective().as_raw().to_string())
        .replace(
            "{STATE_ROOT}",
            &installation.context.paths().state_dir.display().to_string(),
        )
        .replace(
            "{RUNTIME_ROOT}",
            &installation
                .context
                .paths()
                .runtime_dir
                .display()
                .to_string(),
        )
        .into_bytes()
}

/// Rewrites `service.toml` into the exact schema-2 form of the installation.
fn install_schema_two(installation: &Installation) -> Vec<u8> {
    let schema_two = schema_two_config(installation);
    std::fs::write(installation.context.config_path(), &schema_two)
        .expect("write the schema-2 source");
    schema_two
}

/// The journaled digest binding the backup to the schema-2 bytes.
fn config_digest_of(bytes: &[u8]) -> String {
    use sha2::Digest as _;

    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// The state directory's completion record, for pending-state assertions.
fn pending_record(installation: &Installation) -> Option<pohunek_cli::service::record::Record> {
    pohunek_cli::service::record::Store::new(installation.context.paths().state_dir.clone())
        .load()
        .expect("load the transaction record")
}

/// The migration backup's path in the installation's state directory.
fn backup_path(installation: &Installation) -> PathBuf {
    pohunek_cli::service::record::Store::new(installation.context.paths().state_dir.clone())
        .config_backup_path()
}

/// The native fixture uses this build for both versions. Once a rollback has
/// proved the exact schema-2 restore and registered the previous job, repair
/// only that synthetic reader: the current binary itself requires schema 3.
async fn repair_synthetic_previous_reader(installation: &Installation, backend: &Backend) {
    let config_path = installation.context.config_path();
    let config = pohunek_service_config::ServiceConfig::load_upgrade_source(&config_path)
        .expect("read the fixture config")
        .config;
    config
        .write(&config_path)
        .expect("write a schema-3 fixture config");
    match backend.daemon().uninstall().await {
        Ok(()) | Err(pohunek_platform::supervisor::Error::NotFound(_)) => {}
        Err(error) => panic!("stop the synthetic previous job: {error}"),
    }
    let definition = daemon_definition(&installation.context, &config).expect("daemon definition");
    backend
        .daemon()
        .install(&definition)
        .await
        .expect("restart the synthetic previous reader");
    let _daemon = daemon_process(backend).await;
    Store::new(installation.context.paths().state_dir.clone())
        .clear()
        .expect("clear the synthetic pending transaction");
}

/// Observe a rollback's public file and manager effects, then cancel before
/// the synthetic previous reader's schema-2 startup wait can time out.
async fn rollback_until_previous_job(
    installation: &Installation,
    backend: &Backend,
    from: &std::path::Path,
    old_bytes: &[u8],
) {
    let original_inode = std::fs::metadata(installation.context.config_path())
        .expect("source config metadata")
        .ino();
    let engine = Engine::new(&installation.context, backend).with_reported_version(VERSION);
    let rollback = engine.upgrade(from, VERSION);
    tokio::pin!(rollback);
    tokio::select! {
        result = &mut rollback => panic!("rollback returned before the previous job was registered: {result:?}"),
        () = pohunek_test_support::wait::wait_until("schema-2 restoration and previous job registration", || async {
            let rolling_back = pending_record(installation)?.rolling_back;
            let restored = std::fs::read(installation.context.config_path()).ok()? == old_bytes;
            let replaced = std::fs::metadata(installation.context.config_path()).ok()?.ino() != original_inode;
            let job = backend.daemon().inspect().await.ok()?;
            (rolling_back
                && restored
                && replaced
                && job.definition?.executable == installation.prefix().join("libexec/pohunek").join(VERSION).join("pohunekd"))
                .then_some(())
        }) => {}
    }
    assert!(
        pending_record(installation)
            .expect("pending rollback")
            .rolling_back
    );
}

/// Persisted states around the two native-service effects without using an
/// in-process engine hook. A subsequent public upgrade or rollback must
/// reconcile the actual manager job and the exact bytes on disk.
#[derive(Clone, Copy)]
enum MigrationBoundary {
    StopIntentWithOldDaemon,
    StoppedWithOldConfig,
    StoppedWithNewConfig,
    ConfigCheckpointWithNewConfig,
    RegisteredWithNewDaemon,
}

async fn pending_migration(
    installation: &Installation,
    backend: &Backend,
    from: &std::path::Path,
    boundary: MigrationBoundary,
) -> Vec<u8> {
    let old = VERSION.to_owned();
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    let prefix = installation.prefix();
    Engine::new(&installation.context, backend)
        .install(from, &prefix, &old)
        .await
        .expect("install previous version");
    let schema_two = install_schema_two(installation);
    let source = pohunek_service_config::ServiceConfig::load_upgrade_source(
        &installation.context.config_path(),
    )
    .expect("read previous schema");
    let layout = source.config.layout();
    let staged = layout::stage(layout, from, &old)
        .await
        .expect("stage the new version");
    layout::publish(layout, &staged, &new).expect("publish the new version");
    let store = Store::new(installation.context.paths().state_dir.clone());
    store
        .write_config_backup(&schema_two)
        .expect("save exact previous config");
    let step = match boundary {
        MigrationBoundary::RegisteredWithNewDaemon => Step::Registered,
        MigrationBoundary::ConfigCheckpointWithNewConfig => Step::Config,
        MigrationBoundary::StopIntentWithOldDaemon
        | MigrationBoundary::StoppedWithOldConfig
        | MigrationBoundary::StoppedWithNewConfig => Step::StopDaemon,
    };
    let record = Record {
        schema_version: pohunek_cli::service::record::SCHEMA_VERSION,
        operation: Operation::Upgrade,
        version: new.clone(),
        prefix,
        previous_version: Some(old),
        version_dir_preexisted: false,
        config_backup: Some(ConfigBackup {
            source_schema_version: 2,
            digest: config_digest_of(&schema_two),
        }),
        step,
        rolling_back: false,
    };
    store.save(&record).expect("journal the migration boundary");
    if !matches!(boundary, MigrationBoundary::StopIntentWithOldDaemon) {
        backend
            .daemon()
            .uninstall()
            .await
            .expect("stop previous daemon");
    }
    if matches!(
        boundary,
        MigrationBoundary::StoppedWithNewConfig
            | MigrationBoundary::ConfigCheckpointWithNewConfig
            | MigrationBoundary::RegisteredWithNewDaemon
    ) {
        let target = with_version(&source.config, &new).expect("new config");
        target
            .write(&installation.context.config_path())
            .expect("write new schema before config checkpoint");
        if matches!(boundary, MigrationBoundary::RegisteredWithNewDaemon) {
            let definition =
                daemon_definition(&installation.context, &target).expect("new daemon definition");
            backend
                .daemon()
                .install(&definition)
                .await
                .expect("register new daemon");
            let _daemon = daemon_process(backend).await;
        }
    }
    schema_two
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn upgrade_migrates_the_previous_release_config_over_a_stopped_daemon() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let prefix = installation.prefix();
    let old = VERSION.to_owned();
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    let versions = prefix.join("libexec/pohunek");

    Engine::new(&installation.context, &backend)
        .install(&from, &prefix, &old)
        .await
        .expect("install version A");
    let session = installation.new_session("migration").await;
    let old_daemon = daemon_process(&backend).await;
    let before = runtimes(&installation, &backend, std::slice::from_ref(&session)).await;

    let schema_two = install_schema_two(&installation);
    // A live session with a running worker precedes the migration, so the
    // adoption preflight of the staged daemon must judge it adoptable.
    let report = Engine::new(&installation.context, &backend)
        .with_reported_version(old.clone())
        .upgrade(&from, &new)
        .await
        .expect("migrating upgrade");
    assert_eq!(report.from_version.as_str(), old.as_str());
    assert_eq!(report.to_version.as_str(), new.as_str());
    assert!(!report.resumed);
    assert_eq!(
        report.config_schema_migration,
        Some(2),
        "{report:?}: the report names the schema migration"
    );
    assert!(report.preflight.is_some(), "a live session was judged");

    eventually("the daemon restarted from version B", || async {
        let daemon = backend.daemon().inspect().await.ok()?;
        daemon.process.filter(|process| {
            *process != old_daemon
                && daemon
                    .definition
                    .as_ref()
                    .map(|facts| facts.executable.clone())
                    == Some(versions.join(&new).join("pohunekd"))
        })
    })
    .await;

    // The migration rewrote the file into the current schema, with no
    // backup left and no transaction pending.
    let current = pohunek_service_config::ServiceConfig::load(&installation.context.config_path())
        .expect("schema 3");
    assert_eq!(current.active_version(), new);
    assert_eq!(current.input_timing(), DEFAULT_INPUT_TIMING);
    assert!(
        !backup_path(&installation).exists(),
        "the backup goes with the record"
    );
    assert_eq!(pending_record(&installation), None);

    // The daemon was down only between its stop and the registered start;
    // the session worker kept its PTY through the migration.
    let after = runtimes(&installation, &backend, std::slice::from_ref(&session)).await;
    assert_eq!(
        after[0].tty, before[0].tty,
        "the worker PTY survived the migration"
    );
    assert_eq!(
        after[0].executable,
        versions.join(&old).join("pohunek-sessiond"),
        "the worker stays on its version"
    );
    let _ = schema_two;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn upgrade_resumes_a_migration_from_its_journaled_state() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let prefix = installation.prefix();
    let old = VERSION.to_owned();
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");

    Engine::new(&installation.context, &backend)
        .install(&from, &prefix, &old)
        .await
        .expect("install version A");
    let schema_two = install_schema_two(&installation);

    // A crashed migration before its first effect: the digest was journaled,
    // nothing else of this build is installed yet, and the daemon still runs
    // against the schema-2 source.
    let record = pohunek_cli::service::record::Record {
        schema_version: pohunek_cli::service::record::SCHEMA_VERSION,
        operation: pohunek_cli::service::record::Operation::Upgrade,
        version: new.clone(),
        prefix: prefix.clone(),
        previous_version: Some(old.clone()),
        version_dir_preexisted: false,
        config_backup: Some(pohunek_cli::service::record::ConfigBackup {
            source_schema_version: 2,
            digest: config_digest_of(&schema_two),
        }),
        step: pohunek_cli::service::record::Step::Started,
        rolling_back: false,
    };
    pohunek_cli::service::record::Store::new(installation.context.paths().state_dir.clone())
        .save(&record)
        .expect("journal the crashed state");
    let running_daemon = daemon_process(&backend).await;

    let report = Engine::new(&installation.context, &backend)
        .with_reported_version(old.clone())
        .upgrade(&from, &new)
        .await
        .expect("resume the migration");
    assert!(report.resumed);
    assert_eq!(report.config_schema_migration, Some(2));

    eventually("the daemon restarted as version B", || async {
        let daemon = backend.daemon().inspect().await.ok()?;
        daemon.process.filter(|process| *process != running_daemon)
    })
    .await;
    assert_eq!(pending_record(&installation), None);
    assert_eq!(
        pohunek_service_config::ServiceConfig::load(&installation.context.config_path())
            .expect("schema 3")
            .active_version(),
        new
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1, a systemd user manager, and built daemon/worker binaries"
)]
async fn upgrade_refuses_a_migration_whose_source_it_cannot_trust() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let prefix = installation.prefix();
    let old = VERSION.to_owned();
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");

    Engine::new(&installation.context, &backend)
        .install(&from, &prefix, &old)
        .await
        .expect("install version A");
    let mut schema_two = install_schema_two(&installation);
    // A crashed migration before its first effect: the digest was journaled.
    let record = pohunek_cli::service::record::Record {
        schema_version: pohunek_cli::service::record::SCHEMA_VERSION,
        operation: pohunek_cli::service::record::Operation::Upgrade,
        version: new.clone(),
        prefix: prefix.clone(),
        previous_version: Some(old.clone()),
        version_dir_preexisted: false,
        config_backup: Some(pohunek_cli::service::record::ConfigBackup {
            source_schema_version: 2,
            digest: config_digest_of(&schema_two),
        }),
        step: pohunek_cli::service::record::Step::Started,
        rolling_back: false,
    };
    pohunek_cli::service::record::Store::new(installation.context.paths().state_dir.clone())
        .save(&record)
        .expect("journal the crashed state");

    // A value changed without breaking the shape: the configuration the
    // digest journals is gone, and the resume refuses closed before any
    // effect. (A stray file where the backup will be written is harmless at
    // this step: the transaction overwrites it from the verified bytes.)
    schema_two[5] ^= 1;
    std::fs::write(installation.context.config_path(), &schema_two).expect("tamper the source");

    let error = Engine::new(&installation.context, &backend)
        .with_reported_version(old.clone())
        .upgrade(&from, &new)
        .await
        .expect_err("refused");
    assert_eq!(error.code(), "service_config_backup_invalid", "{error:?}");
    assert_eq!(
        pending_record(&installation).expect("record kept").step,
        pohunek_cli::service::record::Step::Started,
        "the record is kept for a repaired retry"
    );
    assert!(
        backend.daemon().inspect().await.is_ok(),
        "the refused resume leaves the daemon registered"
    );
    assert_eq!(
        pohunek_service_config::ServiceConfig::load_upgrade_source(
            &installation.context.config_path()
        )
        .expect("the source is unchanged in shape")
        .source_schema_version,
        2,
        "the schema-2 source is untouched"
    );
    // The fail-closed state is this fixture's; the standard teardown
    // uninstall must not have to roll a record back whose files the fixture
    // tampered with.
    pohunek_cli::service::record::Store::new(installation.context.paths().state_dir.clone())
        .clear()
        .expect("clear the fixture record");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn upgrade_replays_a_journaled_stop_intent_while_the_old_daemon_still_runs() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::StopIntentWithOldDaemon,
    )
    .await;
    let old_daemon = daemon_process(&backend).await;

    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    let report = Engine::new(&installation.context, &backend)
        .with_reported_version(VERSION)
        .upgrade(&from, &new)
        .await
        .expect("resume the native stop intent");
    assert!(report.resumed);
    assert_eq!(report.config_schema_migration, Some(2));
    assert_ne!(daemon_process(&backend).await, old_daemon);
    assert_eq!(pending_record(&installation), None);
    assert_ne!(
        std::fs::read(installation.context.config_path()).expect("migrated config"),
        old_bytes
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn upgrade_resumes_after_schema_three_write_before_its_checkpoint() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let _old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::StoppedWithNewConfig,
    )
    .await;
    let written = std::fs::read(installation.context.config_path()).expect("new config bytes");

    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    let report = Engine::new(&installation.context, &backend)
        .with_reported_version(VERSION)
        .upgrade(&from, &new)
        .await
        .expect("resume after the write without a config checkpoint");
    assert!(report.resumed);
    assert_eq!(pending_record(&installation), None);
    assert_eq!(
        std::fs::read(installation.context.config_path()).expect("resumed config"),
        written
    );
    assert_eq!(
        backend
            .daemon()
            .inspect()
            .await
            .expect("replacement job")
            .definition
            .expect("replacement definition")
            .executable,
        installation
            .prefix()
            .join("libexec/pohunek")
            .join(new)
            .join("pohunekd")
    );
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_restores_exact_schema_two_after_a_config_write_without_checkpoint() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::StoppedWithNewConfig,
    )
    .await;

    rollback_until_previous_job(&installation, &backend, &from, &old_bytes).await;
    assert_eq!(
        std::fs::read(installation.context.config_path()).expect("restored config"),
        old_bytes
    );
    repair_synthetic_previous_reader(&installation, &backend).await;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_restarts_the_old_daemon_when_stop_left_no_job() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::StoppedWithOldConfig,
    )
    .await;
    assert!(matches!(
        backend.daemon().inspect().await,
        Err(pohunek_platform::supervisor::Error::NotFound(_))
    ));

    rollback_until_previous_job(&installation, &backend, &from, &old_bytes).await;
    assert_eq!(
        std::fs::read(installation.context.config_path()).expect("restored config"),
        old_bytes
    );
    repair_synthetic_previous_reader(&installation, &backend).await;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_reconciles_a_stop_intent_before_the_old_job_was_stopped() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::StopIntentWithOldDaemon,
    )
    .await;
    let old_daemon = daemon_process(&backend).await;

    rollback_until_previous_job(&installation, &backend, &from, &old_bytes).await;
    assert_eq!(
        std::fs::read(installation.context.config_path()).expect("restored config"),
        old_bytes
    );
    assert_ne!(
        backend
            .daemon()
            .inspect()
            .await
            .expect("previous job")
            .process,
        Some(old_daemon),
        "the journaled stop was reconciled before the restore"
    );
    repair_synthetic_previous_reader(&installation, &backend).await;
}

/// A future schema is refused by a real daemon reader regardless of the
/// operator's runtime-loss choice.
fn make_store_unreadable(installation: &Installation) {
    make_store_unreadable_at(&installation.context);
}

fn make_store_unreadable_at(context: &pohunek_cli::service::Context) {
    let store = context.paths().data_dir.join("metadata.jsonl");
    std::fs::write(&store, b"{\"schema_version\":999}\n")
        .expect("write an unsupported future store schema");
    std::fs::set_permissions(store, std::fs::Permissions::from_mode(0o600))
        .expect("keep the fixture store owner-private");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_rechecks_the_store_after_stopping_the_new_daemon() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::RegisteredWithNewDaemon,
    )
    .await;
    let new_bytes = std::fs::read(installation.context.config_path()).expect("new config");
    // The fixture changes the store at the manager boundary. The previous
    // release's frozen reader then performs the real second compatibility
    // judgment; an injected AdoptionPreflight would not run for v0.33.1.
    let context = installation.context.clone();
    let error = Engine::new(&installation.context, &backend)
        .with_reported_version(VERSION)
        .with_after_rollback_stop(move || make_store_unreadable_at(&context))
        .upgrade(&from, VERSION)
        .await
        .expect_err("second previous-reader gate refuses the changed store");
    assert_eq!(error.code(), "service_rollback_store_unusable", "{error:?}");
    assert!(matches!(
        backend.daemon().inspect().await,
        Err(pohunek_platform::supervisor::Error::NotFound(_))
    ));
    assert_eq!(
        std::fs::read(installation.context.config_path()).expect("config still new"),
        new_bytes
    );
    let record = pending_record(&installation).expect("rollback remains pending");
    assert_eq!(record.step, Step::Registered);
    assert!(record.rolling_back);

    std::fs::remove_file(installation.context.paths().data_dir.join("metadata.jsonl"))
        .expect("repair the injected store");
    rollback_until_previous_job(&installation, &backend, &from, &old_bytes).await;
    repair_synthetic_previous_reader(&installation, &backend).await;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_resumes_after_restoring_exact_old_bytes_before_previous_registration() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::RegisteredWithNewDaemon,
    )
    .await;
    backend
        .daemon()
        .uninstall()
        .await
        .expect("the new job stopped before restore");
    std::fs::write(installation.context.config_path(), &old_bytes)
        .expect("simulate the exact restore before the crash");
    let store = Store::new(installation.context.paths().state_dir.clone());
    let mut record = store.load().expect("read record").expect("pending upgrade");
    record.rolling_back = true;
    store.save(&record).expect("journal interrupted rollback");

    rollback_until_previous_job(&installation, &backend, &from, &old_bytes).await;
    assert_eq!(
        std::fs::read(installation.context.config_path()).expect("restored config"),
        old_bytes
    );
    repair_synthetic_previous_reader(&installation, &backend).await;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_checks_the_exact_backup_before_stopping_a_new_daemon() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let _old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::RegisteredWithNewDaemon,
    )
    .await;
    let new_daemon = daemon_process(&backend).await;
    let new_bytes = std::fs::read(installation.context.config_path()).expect("new config");
    let backup = backup_path(&installation);
    let original = std::fs::read(&backup).expect("read exact backup");
    let mut altered = original.clone();
    altered[5] ^= 1;
    std::fs::write(&backup, altered).expect("tamper the backup");

    let error = Engine::new(&installation.context, &backend)
        .with_reported_version(VERSION)
        .upgrade(&from, VERSION)
        .await
        .expect_err("tampered backup refuses rollback");
    assert_eq!(error.code(), "service_config_backup_invalid", "{error:?}");
    assert_eq!(daemon_process(&backend).await, new_daemon);
    assert_eq!(
        std::fs::read(installation.context.config_path()).expect("untouched config"),
        new_bytes
    );
    assert_eq!(
        pending_record(&installation).expect("record kept").step,
        Step::Registered
    );
    // Repair only the fixture input so ordinary native teardown can finish.
    std::fs::write(backup, original).expect("restore valid fixture backup");
    repair_synthetic_previous_reader(&installation, &backend).await;
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn migration_rejects_a_third_config_during_the_config_checkpoint_gap() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    let _old_bytes = pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::StoppedWithNewConfig,
    )
    .await;
    let current =
        std::fs::read_to_string(installation.context.config_path()).expect("schema-three bytes");
    let altered = current.replace("submit_delay_ms = 150", "submit_delay_ms = 151");
    assert_ne!(altered, current);
    std::fs::write(installation.context.config_path(), &altered)
        .expect("write a valid but unjournaled config");

    for version in [format!("{VERSION}{UPGRADE_SUFFIX}"), VERSION.to_owned()] {
        let error = Engine::new(&installation.context, &backend)
            .with_reported_version(VERSION)
            .upgrade(&from, &version)
            .await
            .expect_err("resume or rollback refuses arbitrary bytes");
        assert_eq!(error.code(), "service_config_backup_invalid", "{error:?}");
        assert_eq!(
            pending_record(&installation).expect("record kept").step,
            Step::StopDaemon
        );
        assert!(matches!(
            backend.daemon().inspect().await,
            Err(pohunek_platform::supervisor::Error::NotFound(_))
        ));
        assert_eq!(
            std::fs::read_to_string(installation.context.config_path()).expect("untouched file"),
            altered
        );
    }
    // The previous file is still in the trusted backup; restore the exact
    // expected new target so teardown can finish the pending rollback.
    std::fs::write(installation.context.config_path(), current).expect("repair the fixture config");
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    Engine::new(&installation.context, &backend)
        .with_reported_version(VERSION)
        .upgrade(&from, &new)
        .await
        .expect("complete the migration after config repair");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn migration_rejects_edited_schema_three_after_the_config_checkpoint() {
    let mut installation = Installation::new();
    installation.reported_version = Some(VERSION.to_owned());
    let from = installation.stage();
    let namespace = installation.context.namespace().expect("namespace");
    let backend = connect(&installation.context, &namespace).await;
    pending_migration(
        &installation,
        &backend,
        &from,
        MigrationBoundary::ConfigCheckpointWithNewConfig,
    )
    .await;
    let expected =
        std::fs::read_to_string(installation.context.config_path()).expect("journaled schema 3");
    let edited = expected.replace("submit_delay_ms = 150", "submit_delay_ms = 151");
    assert_ne!(edited, expected);
    std::fs::write(installation.context.config_path(), &edited)
        .expect("write valid but unjournaled schema 3");

    for version in [format!("{VERSION}{UPGRADE_SUFFIX}"), VERSION.to_owned()] {
        let error = Engine::new(&installation.context, &backend)
            .with_reported_version(VERSION)
            .upgrade(&from, &version)
            .await
            .expect_err("resume and rollback reject edited schema 3");
        assert_eq!(error.code(), "service_config_backup_invalid", "{error:?}");
        assert_eq!(
            pending_record(&installation).expect("record kept").step,
            Step::Config
        );
        assert!(matches!(
            backend.daemon().inspect().await,
            Err(pohunek_platform::supervisor::Error::NotFound(_))
        ));
        assert_eq!(
            std::fs::read_to_string(installation.context.config_path()).expect("untouched file"),
            edited
        );
    }
    std::fs::write(installation.context.config_path(), expected).expect("repair the fixture");
    let new = format!("{VERSION}{UPGRADE_SUFFIX}");
    Engine::new(&installation.context, &backend)
        .with_reported_version(VERSION)
        .upgrade(&from, &new)
        .await
        .expect("complete the migration after restoring the journaled target");
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_refuses_unreadable_store_after_a_stopped_job_with_either_loss_choice() {
    for accept in [false, true] {
        let mut installation = Installation::new();
        installation.reported_version = Some(VERSION.to_owned());
        let from = installation.stage();
        let namespace = installation.context.namespace().expect("namespace");
        let backend = connect(&installation.context, &namespace).await;
        let old_bytes = pending_migration(
            &installation,
            &backend,
            &from,
            MigrationBoundary::StoppedWithOldConfig,
        )
        .await;
        make_store_unreadable(&installation);

        let error = Engine::new(&installation.context, &backend)
            .with_reported_version(VERSION)
            .with_runtime_loss_accepted(accept)
            .upgrade(&from, VERSION)
            .await
            .expect_err("previous reader refuses the store");
        assert_eq!(error.code(), "service_rollback_store_unusable", "{error:?}");
        assert_eq!(
            pending_record(&installation).expect("record kept").step,
            Step::StopDaemon
        );
        assert_eq!(
            std::fs::read(installation.context.config_path()).expect("untouched config"),
            old_bytes
        );
        assert!(matches!(
            backend.daemon().inspect().await,
            Err(pohunek_platform::supervisor::Error::NotFound(_))
        ));
        // The fixture store itself is intentionally unreadable. Repair it so
        // native teardown can restore and remove the installation.
        std::fs::remove_file(installation.context.paths().data_dir.join("metadata.jsonl"))
            .expect("repair the fixture store");
        repair_synthetic_previous_reader(&installation, &backend).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_refuses_unreadable_store_before_a_journaled_stop_with_either_loss_choice() {
    for accept in [false, true] {
        let mut installation = Installation::new();
        installation.reported_version = Some(VERSION.to_owned());
        let from = installation.stage();
        let namespace = installation.context.namespace().expect("namespace");
        let backend = connect(&installation.context, &namespace).await;
        let old_bytes = pending_migration(
            &installation,
            &backend,
            &from,
            MigrationBoundary::StopIntentWithOldDaemon,
        )
        .await;
        let old_daemon = daemon_process(&backend).await;
        make_store_unreadable(&installation);

        let error = Engine::new(&installation.context, &backend)
            .with_reported_version(VERSION)
            .with_runtime_loss_accepted(accept)
            .upgrade(&from, VERSION)
            .await
            .expect_err("previous reader refuses the store before the stop");
        assert_eq!(error.code(), "service_rollback_store_unusable", "{error:?}");
        assert_eq!(daemon_process(&backend).await, old_daemon);
        assert_eq!(
            std::fs::read(installation.context.config_path()).expect("untouched config"),
            old_bytes
        );
        assert_eq!(
            pending_record(&installation).expect("record kept").step,
            Step::StopDaemon
        );
        std::fs::remove_file(installation.context.paths().data_dir.join("metadata.jsonl"))
            .expect("repair the fixture store");
        repair_synthetic_previous_reader(&installation, &backend).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires POHUNEK_SYSTEMD_E2E=1 and a systemd user manager"
)]
async fn rollback_refuses_unreadable_store_before_stopping_a_new_daemon() {
    for accept in [false, true] {
        let mut installation = Installation::new();
        installation.reported_version = Some(VERSION.to_owned());
        let from = installation.stage();
        let namespace = installation.context.namespace().expect("namespace");
        let backend = connect(&installation.context, &namespace).await;
        let _old_bytes = pending_migration(
            &installation,
            &backend,
            &from,
            MigrationBoundary::RegisteredWithNewDaemon,
        )
        .await;
        let new_daemon = daemon_process(&backend).await;
        let new_bytes = std::fs::read(installation.context.config_path()).expect("new config");
        make_store_unreadable(&installation);

        let error = Engine::new(&installation.context, &backend)
            .with_reported_version(VERSION)
            .with_runtime_loss_accepted(accept)
            .upgrade(&from, VERSION)
            .await
            .expect_err("previous reader refuses the store");
        assert_eq!(error.code(), "service_rollback_store_unusable", "{error:?}");
        assert_eq!(
            pending_record(&installation).expect("record kept").step,
            Step::Registered
        );
        assert_eq!(daemon_process(&backend).await, new_daemon);
        assert_eq!(
            std::fs::read(installation.context.config_path()).expect("untouched config"),
            new_bytes
        );
        std::fs::remove_file(installation.context.paths().data_dir.join("metadata.jsonl"))
            .expect("repair the fixture store");
        repair_synthetic_previous_reader(&installation, &backend).await;
    }
}

/// Captures each session's worker job, process, executable, and PTY.
async fn runtimes(
    installation: &Installation,
    backend: &Backend,
    sessions: &[String],
) -> Vec<Runtime> {
    let inspector = HostInspector::new();
    let jobs = backend
        .workers()
        .discover()
        .await
        .expect("discover workers");
    let mut runtimes = Vec::new();
    for session in sessions {
        let info = installation.live(session).await;
        let matching = jobs
            .iter()
            .filter(|job| job.id.as_str().starts_with(&format!("{session}.")))
            .collect::<Vec<&ServiceObservation>>();
        let [job] = matching.as_slice() else {
            panic!("{session} has exactly one worker job: {matching:?}");
        };
        runtimes.push(Runtime {
            job: job.id.to_string(),
            worker: job.process.expect("the worker job has a process"),
            executable: job
                .definition
                .as_ref()
                .expect("the worker job has a definition")
                .executable
                .clone(),
            child: inspector
                .identity(info.pid)
                .expect("inspect the PTY child")
                .expect("the PTY child runs"),
            tty: tty(info.pid),
        });
    }
    runtimes
}

async fn daemon_process(backend: &Backend) -> ProcessIdentity {
    eventually("daemon process", || async {
        backend
            .daemon()
            .inspect()
            .await
            .ok()
            .and_then(|job| job.process)
    })
    .await
}

fn executable_of(pid: u32) -> Option<PathBuf> {
    HostInspector::new()
        .executable(pid)
        .expect("inspect the daemon executable")
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
}

fn tty(pid: u32) -> String {
    let output = std::process::Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    String::from_utf8(output.stdout)
        .expect("ps output is UTF-8")
        .trim()
        .to_owned()
}
