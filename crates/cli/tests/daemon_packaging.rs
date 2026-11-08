//! Argument plumbing of `packaging/install-daemon.sh`.
//!
//! The wrapper delegates installation to `pohunek service install|upgrade`,
//! whose transactions are tested in the service engine. These tests run the
//! real script against a fake `pohunek` in a fake archive and a fake
//! `systemctl` on `PATH`, recording every invocation.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[test]
fn fresh_install_runs_service_install_from_the_archive() {
    let fixture = Fixture::new();
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.install_call()]
    );
    fixture.assert_guarded(&[]);
    assert!(
        !fixture.systemctl_log.exists(),
        "a fresh install never touches systemctl"
    );
}

/// The commands a finished install or upgrade tells the owner to run next.
fn assert_hook_asset_next_steps(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pohunek integration doctor"), "{stderr}");
    assert!(
        stderr.contains("pohunek integration install --agent <agent>`"),
        "{stderr}"
    );
    // Hermes install needs --access-mode and --allow-host, so an existing
    // plugin is brought current with `update`, which keeps the installed policy.
    assert!(
        stderr.contains(
            "for an outdated Hermes plugin run `pohunek integration update --agent hermes`"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("`--hermes-profile` or `--hermes-home`"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("integration install --agent hermes"),
        "the Hermes recommendation must not name the install action: {stderr}"
    );
}

#[test]
fn a_finished_install_and_upgrade_name_the_hook_asset_commands() {
    let install = Fixture::new();
    let output = install.run(&[], &[]);
    assert_success(&output);
    assert_hook_asset_next_steps(&output);

    let upgrade = Fixture::new();
    write(&upgrade.config_home.join("pohunek/service.toml"), "");
    let output = upgrade.run(&[], &[]);
    assert_success(&output);
    assert_hook_asset_next_steps(&output);
}

#[test]
fn a_refused_upgrade_does_not_claim_a_finished_install() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(
        &[],
        &[(
            "POHUNEK_TEST_CHECK_ERROR",
            "service_upgrade_sessions_at_risk",
        )],
    );
    assert_ne!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("pohunek integration doctor"), "{stderr}");
}

#[test]
fn a_pending_install_is_finished_by_service_install_even_with_service_config() {
    // From its `config` step on, an interrupted install has written
    // service.toml; `service upgrade` refuses its record.
    for step in ["config", "registered", "ready"] {
        let fixture = Fixture::new();
        write(&fixture.config_home.join("pohunek/service.toml"), "");
        let output = fixture.run(
            &[],
            &[
                ("POHUNEK_TEST_PENDING_OPERATION", "install"),
                ("POHUNEK_TEST_PENDING_STEP", step),
            ],
        );
        assert_success(&output);
        assert_eq!(
            fixture.pohunek_calls(),
            [STATUS.to_owned(), fixture.install_call()],
            "pending install at {step}"
        );
    }
}

#[test]
fn an_upgrade_passes_no_loss_acceptance_without_the_flag() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.guard_calls(),
        [fixture.lock_call(&[]), fixture.check_call()]
    );
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.upgrade_call()]
    );
    for call in fixture.guard_calls().iter().chain(&fixture.pohunek_calls()) {
        assert!(!call.contains("--accept-runtime-loss"), "{call}");
    }
}

#[test]
fn an_upgrade_passes_the_loss_acceptance_to_the_check_and_the_upgrade_once() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(&["--accept-runtime-loss"], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.guard_calls(),
        [
            fixture.lock_call(&["--accept-runtime-loss"]),
            format!("{} --accept-runtime-loss", fixture.check_call()),
        ]
    );
    assert_eq!(
        fixture.pohunek_calls(),
        [
            STATUS.to_owned(),
            format!("{} --accept-runtime-loss", fixture.upgrade_call()),
        ]
    );
    for call in [&fixture.guard_calls()[1], &fixture.pohunek_calls()[1]] {
        assert_eq!(call.matches("--accept-runtime-loss").count(), 1, "{call}");
    }
}

#[test]
fn an_upgrade_refused_for_sessions_at_risk_stops_before_anything_changes() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(
        &[],
        &[(
            "POHUNEK_TEST_CHECK_ERROR",
            "service_upgrade_sessions_at_risk",
        )],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("service_upgrade_sessions_at_risk"),
        "{stderr}"
    );
    assert!(stderr.contains("--accept-runtime-loss"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    fixture.assert_guarded(&[]);
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    assert!(fixture.systemctl_calls().is_empty());
}

#[test]
fn an_install_never_receives_the_loss_acceptance() {
    let fixture = Fixture::new();
    let output = fixture.run(&["--accept-runtime-loss"], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.guard_calls(),
        [
            fixture.lock_call(&["--accept-runtime-loss"]),
            fixture.check_call(),
        ]
    );
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.install_call()]
    );
}

#[test]
fn a_pending_upgrade_keeps_service_upgrade() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_PENDING_OPERATION", "upgrade"),
            ("POHUNEK_TEST_PENDING_STEP", "registered"),
        ],
    );
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.upgrade_call()]
    );
}

#[test]
fn an_unanswered_status_query_aborts_before_anything_changes() {
    let fixture = Fixture::new();
    let output = fixture.run(&[], &[("POHUNEK_TEST_STATUS_QUERY_EXIT", "5")]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service_record_invalid"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    assert!(fixture.systemctl_calls().is_empty());

    // A report without `pending_transaction` cannot rule a pending install out.
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(&[], &[("POHUNEK_TEST_STATUS_UNEXPECTED", "1")]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("pending_transaction"));
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
}

/// Asserts that the wrapper refused the archive before it ran any archive
/// binary, queried `systemctl`, or touched the install prefix.
fn assert_refused_untouched(fixture: &Fixture, output: &Output, reason: &str) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(reason), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert!(
        !fixture.pohunek_log.exists() && !fixture.guard_log.exists(),
        "an archive binary ran: {stderr}"
    );
    assert!(fixture.systemctl_calls().is_empty());
    assert!(!fixture.prefix.exists(), "the prefix was created");
    assert!(!fixture.config_home.join("pohunek").exists());
}

#[test]
fn a_modified_archive_member_is_refused_before_any_binary_runs() {
    for member in ["pohunek", "pohunekd", "pohunek-sessiond"] {
        let fixture = Fixture::new();
        let path = fixture.archive.join(member);
        let mut contents = read(&path);
        contents.push_str("# tampered\n");
        fs::write(&path, contents).expect("tamper with member");
        let output = fixture.run(&[], &[]);
        assert_refused_untouched(&fixture, &output, "digest mismatch");
        assert!(String::from_utf8_lossy(&output.stderr).contains(member));
    }
}

#[test]
fn a_missing_archive_member_is_refused_before_any_binary_runs() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.archive.join("pohunek-sessiond")).expect("remove member");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "missing or not a regular file");
}

#[test]
fn a_symlinked_archive_member_is_refused() {
    let fixture = Fixture::new();
    let member = fixture.archive.join("pohunekd");
    let target = fixture.base().join("elsewhere");
    fs::rename(&member, &target).expect("move member");
    std::os::unix::fs::symlink(&target, &member).expect("link member");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "holds a symbolic link");
}

#[test]
fn an_archive_without_a_manifest_is_refused() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.archive.join("MANIFEST")).expect("remove manifest");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "no MANIFEST");
}

#[test]
fn a_malformed_manifest_is_refused() {
    for (manifest, reason) in [
        ("", "MANIFEST is empty"),
        ("not a manifest\n", "not a pohunek archive manifest"),
        (
            "pohunek-archive-manifest 1\nsurprise x\n",
            "unrecognized line",
        ),
        (
            "pohunek-archive-manifest 1\nsha256 abc pohunek\n",
            "malformed digest",
        ),
        (
            "pohunek-archive-manifest 1\nsha256 \
             0000000000000000000000000000000000000000000000000000000000000000 ../pohunek\n",
            "unsafe member path",
        ),
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.archive.join("MANIFEST"), manifest).expect("write manifest");
        let output = fixture.run(&[], &[]);
        assert_refused_untouched(&fixture, &output, reason);
    }
}

#[test]
fn a_manifest_that_omits_a_required_binary_is_refused() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.archive.join("pohunek-sessiond")).expect("remove member");
    seal_manifest(&fixture.archive, &host_target(), "daemon");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "does not list the required binary");
}

#[test]
fn another_components_archive_is_refused() {
    for component in ["cli", "relay"] {
        let fixture = Fixture::new();
        seal_manifest(&fixture.archive, &host_target(), component);
        let output = fixture.run(&[], &[]);
        assert_refused_untouched(
            &fixture,
            &output,
            &format!("this is the {component} archive"),
        );
    }
}

#[test]
fn an_archive_built_for_another_architecture_is_refused() {
    let fixture = Fixture::new();
    seal_manifest(&fixture.archive, "aarch64-apple-darwin", "daemon");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "is built for");
}

#[test]
fn an_archive_member_writable_by_another_account_is_refused() {
    let fixture = Fixture::new();
    let member = fixture.archive.join("pohunekd");
    fs::set_permissions(&member, fs::Permissions::from_mode(0o775)).expect("loosen member");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "writable by another account");

    let fixture = Fixture::new();
    fs::set_permissions(&fixture.archive, fs::Permissions::from_mode(0o757))
        .expect("loosen archive directory");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "writable by another account");
}

#[test]
fn a_directory_inside_the_archive_writable_by_another_account_is_refused() {
    // Another account could replace `packaging/install-daemon.sh` between the
    // digest check and the wrapper's re-execution.
    let fixture = Fixture::new();
    fs::set_permissions(
        fixture.archive.join("packaging"),
        fs::Permissions::from_mode(0o777),
    )
    .expect("loosen packaging directory");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "writable by another account");
}

#[test]
fn a_directory_above_the_archive_writable_by_another_account_is_refused() {
    // Another account could rename the verified tree away and put its own
    // under the same path.
    let fixture = Fixture::new();
    fs::set_permissions(fixture.base(), fs::Permissions::from_mode(0o777))
        .expect("loosen the archive's parent");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "a directory above the archive");
}

#[test]
fn a_non_executable_required_binary_is_refused() {
    let fixture = Fixture::new();
    fs::set_permissions(
        fixture.archive.join("pohunekd"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("drop the executable bit");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "not executable");
}

#[test]
fn unsupported_hosts_are_refused_before_the_archive_is_read() {
    for (system, machine, reason) in [
        ("Linux", "aarch64", "unsupported host: Linux aarch64"),
        ("FreeBSD", "amd64", "unsupported host: FreeBSD amd64"),
        (
            "Darwin",
            "x86_64",
            "Intel Mac or a shell translated by Rosetta",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.fake_host(system, machine);
        let output = fixture.run(&[], &[]);
        assert_refused_untouched(&fixture, &output, reason);
    }
}

#[test]
fn a_macos_host_older_than_the_archive_minimum_is_refused() {
    let fixture = Fixture::new();
    fixture.fake_host("Darwin", "arm64");
    write_executable(
        &fixture.commands.join("sw_vers"),
        "#!/bin/sh\necho 13.6.1\n",
    );
    seal_manifest(&fixture.archive, "aarch64-apple-darwin", "daemon");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "needs macOS 14.0 or newer");
}

#[test]
fn a_macos_minimum_compares_minor_versions() {
    let fixture = Fixture::new();
    fixture.fake_host("Darwin", "arm64");
    write_executable(&fixture.commands.join("sw_vers"), "#!/bin/sh\necho 14.0\n");
    seal_manifest(&fixture.archive, "aarch64-apple-darwin", "daemon");
    assert_success(&fixture.run(&[], &[]));
    // An archive that needs 14.5 refuses macOS 14.0.
    let fixture = Fixture::new();
    fixture.fake_host("Darwin", "arm64");
    write_executable(&fixture.commands.join("sw_vers"), "#!/bin/sh\necho 14.0\n");
    seal_manifest_with_minimum(&fixture.archive, "aarch64-apple-darwin", "14.5");
    let output = fixture.run(&[], &[]);
    assert_refused_untouched(&fixture, &output, "needs macOS 14.5 or newer");
}

#[test]
fn a_macos_arm64_host_accepts_the_macos_archive() {
    let fixture = Fixture::new();
    fixture.fake_host("Darwin", "arm64");
    write_executable(&fixture.commands.join("sw_vers"), "#!/bin/sh\necho 15.1\n");
    seal_manifest(&fixture.archive, "aarch64-apple-darwin", "daemon");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.install_call()]
    );
}

const STATUS: &str = "service status --json";
/// Version recorded in the fixture archive's manifest.
const ARCHIVE_VERSION: &str = "0.5.0";
/// Lowest macOS major version the fixture manifest accepts.
const MINIMUM_MACOS: &str = "14.0";
/// Spells an install prefix from the fixture's prefix.
type PrefixSpelling = fn(&Path) -> String;

#[test]
fn an_unsound_install_prefix_refuses_before_anything_changes() {
    let cases: [(&str, PrefixSpelling); 3] = [
        ("relative", |_| "prefix".to_owned()),
        ("dot-dot", |prefix| {
            format!("{}/../prefix", prefix.display())
        }),
        ("dot", |prefix| format!("{}/./", prefix.display())),
    ];
    for (name, spell) in cases {
        let fixture = Fixture::new();
        let prefix = spell(&fixture.prefix);
        let output = fixture.run(&[], &[("POHUNEK_INSTALL_PREFIX", &prefix)]);
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("POHUNEK_INSTALL_PREFIX must be an absolute path"),
            "{name}: {stderr}"
        );
        assert!(stderr.contains("nothing was changed"), "{name}: {stderr}");
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        assert!(fixture.systemctl_calls().is_empty(), "{name}");
    }
}

#[test]
fn an_upgrade_refuses_a_prefix_other_than_the_configured_one() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let other = fixture.base().join("other");
    let output = fixture.run(
        &[],
        &[(
            "POHUNEK_INSTALL_PREFIX",
            other.to_str().expect("utf-8 path"),
        )],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("differs from the prefix of the installed service"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    assert!(fixture.systemctl_calls().is_empty());
}

#[test]
fn an_upgrade_without_a_readable_configured_prefix_refuses() {
    for (name, value) in [
        ("null", "null"),
        ("escaped", r#""/home/u/.lo\"cal""#),
        ("relative", r#""prefix""#),
    ] {
        let fixture = Fixture::new();
        write(&fixture.config_home.join("pohunek/service.toml"), "");
        let output = fixture.run(&[], &[("POHUNEK_TEST_CONFIGURED_PREFIX_JSON", value)]);
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("nothing was changed"), "{name}: {stderr}");
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        assert!(fixture.systemctl_calls().is_empty(), "{name}");
    }
}

/// Builds an untrusted directory into a fixture.
type UntrustedSetup = fn(&Fixture);

/// Replaces the directory `path` of `fixture` with a symlink to a moved copy.
fn move_behind_symlink(fixture: &Fixture, path: &Path) {
    let real = fixture
        .base()
        .join("real")
        .join(path.strip_prefix(fixture.base()).expect("fixture path"));
    create_dirs(real.parent().expect("parent"));
    fs::rename(path, &real).expect("move directory");
    std::os::unix::fs::symlink(&real, path).expect("directory symlink");
}

#[test]
fn an_untrusted_directory_refuses_before_anything_changes() {
    // `pohunek service install|upgrade` refuses these directories, and the
    // real `service check` judges them with the installer's own rules.
    let cases: [(&str, UntrustedSetup); 7] = [
        ("symlinked prefix", |fixture| {
            move_behind_symlink(fixture, &fixture.prefix);
        }),
        ("symlinked libexec", |fixture| {
            move_behind_symlink(fixture, &fixture.prefix.join("libexec"));
        }),
        ("symlinked supervisor directory", |fixture| {
            // The systemd user unit directory on Linux, `LaunchAgents` on
            // macOS: the directory the service backend writes.
            let supervisor = fixture.supervisor_dir();
            create_dirs(&supervisor);
            move_behind_symlink(fixture, &supervisor);
        }),
        ("symlinked config home", |fixture| {
            move_behind_symlink(fixture, &fixture.config_home);
        }),
        ("group-writable prefix", |fixture| {
            fs::set_permissions(&fixture.prefix, fs::Permissions::from_mode(0o775))
                .expect("chmod prefix");
        }),
        ("shared runtime root", |fixture| {
            let runtime = fixture.runtime_root.join("pohunek");
            fs::set_permissions(runtime, fs::Permissions::from_mode(0o755))
                .expect("chmod runtime root");
        }),
        ("symlinked state root", |fixture| {
            let state = fixture.base().join("state/pohunek");
            create_dirs(&state);
            fs::set_permissions(&state, fs::Permissions::from_mode(0o700))
                .expect("chmod state root");
            move_behind_symlink(fixture, &state);
        }),
    ];
    for (name, setup) in cases {
        let fixture = Fixture::new();
        create_dirs(&fixture.prefix.join("bin"));
        create_dirs(&fixture.prefix.join("libexec"));
        create_dirs(&fixture.config_home);
        setup(&fixture);
        let output = fixture.run(
            &["--accept-runtime-loss"],
            &[("POHUNEK_TEST_REAL_CHECK", "1")],
        );
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        if name == "symlinked state root" {
            // The transaction lock lives in the state root, so the lock
            // itself refuses it before the wrapper runs again.
            assert!(stderr.contains("state directory"), "{name}: {stderr}");
            assert_eq!(
                fixture.guard_calls(),
                [fixture.lock_call(&["--accept-runtime-loss"])],
                "{name}"
            );
            assert!(fixture.pohunek_calls().is_empty(), "{name}");
        } else {
            assert!(stderr.contains("nothing was changed"), "{name}: {stderr}");
            fixture.assert_guarded(&["--accept-runtime-loss"]);
            assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        }
        assert!(fixture.systemctl_calls().is_empty(), "{name}");
    }
}

#[test]
fn a_running_service_transaction_refuses_before_anything_changes() {
    // Another `pohunek service` command holds the transaction lock, so the
    // wrapper's own `service lock` is refused before the wrapper runs again.
    let fixture = Fixture::new();
    let started = fixture.base().join("holder-started");
    let release = fixture.base().join("holder-release");
    let mut holder = fixture
        .real_pohunek(&[
            "service",
            "lock",
            "--",
            "sh",
            "-c",
            &format!(
                ": > '{}'; while [ ! -e '{}' ]; do sleep 0.05; done",
                started.display(),
                release.display()
            ),
        ])
        .spawn()
        .expect("start the competing transaction");
    wait_for(&started);

    let output = fixture.run(&["--accept-runtime-loss"], &[]);
    write(&release, "");
    assert!(holder.wait().expect("holder exits").success());

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("another `pohunek service` command is running"),
        "{stderr}"
    );
    assert_eq!(
        fixture.guard_calls(),
        [fixture.lock_call(&["--accept-runtime-loss"])]
    );
    assert!(fixture.pohunek_calls().is_empty());
    assert!(fixture.systemctl_calls().is_empty());
}

#[test]
fn a_home_the_service_refuses_stops_the_run_before_anything_changes() {
    // `pohunek service install` validates HOME before its first effect; the
    // real `service check` runs that validation before the legacy install is
    // retired.
    for (name, home) in [
        ("nonexistent", "no-such-home"),
        ("regular file", "home-file"),
    ] {
        let fixture = Fixture::new();
        write(&fixture.base().join("home-file"), "");
        let home = fixture.base().join(home);
        let output = fixture.run(
            &["--accept-runtime-loss"],
            &[
                ("POHUNEK_TEST_REAL_CHECK", "1"),
                ("HOME", &home.display().to_string()),
            ],
        );
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("service_environment_invalid"),
            "{name}: {stderr}"
        );
        assert!(stderr.contains("nothing was changed"), "{name}: {stderr}");
        fixture.assert_guarded(&["--accept-runtime-loss"]);
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        assert!(fixture.systemctl_calls().is_empty(), "{name}");
    }
}

#[test]
fn a_failed_or_unlocked_check_stops_the_run_before_anything_changes() {
    for (name, env, message) in [
        (
            "refused",
            ("POHUNEK_TEST_CHECK_ERROR", "service_install_pending"),
            "service_install_pending",
        ),
        (
            "unlocked",
            ("POHUNEK_TEST_CHECK_LOCKED", "false"),
            "did not run under this run's transaction lock",
        ),
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&[], &[env]);
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{name}: {stderr}");
        assert!(stderr.contains("nothing was changed"), "{name}: {stderr}");
        fixture.assert_guarded(&[]);
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        assert!(fixture.systemctl_calls().is_empty(), "{name}");
    }

    // A lock variable set by the caller is not trusted: without a held lock
    // the check refuses, and the wrapper does not take one of its own.
    let fixture = Fixture::new();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_REAL_CHECK", "1"),
            ("POHUNEK_SERVICE_LOCK_TOKEN", "not-a-token"),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("service_inherited_lock_invalid"),
        "{stderr}"
    );
    assert_eq!(fixture.guard_calls(), [fixture.check_call()]);
    assert!(fixture.systemctl_calls().is_empty());
}

/// Waits until `path` exists.
fn wait_for(path: &Path) {
    /// Bound on waiting for a helper process to signal readiness.
    const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    /// Poll interval of that wait.
    const POLL: std::time::Duration = std::time::Duration::from_millis(20);

    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(POLL);
    }
}

#[test]
fn a_pre_service_install_is_refused_naming_the_bridge_release() {
    for args in [&[][..], &["--accept-runtime-loss"][..]] {
        let fixture = Fixture::new();
        fixture.legacy_install();
        let output = fixture.run(args, &[]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("nothing was changed"), "{stderr}");
        assert!(stderr.contains("install release v0.40.1 first"), "{stderr}");
        assert!(fixture.pohunek_calls().is_empty(), "no pohunek call ran");
        assert!(fixture.guard_calls().is_empty(), "no lock was taken");
        assert!(fixture.systemctl_calls().is_empty());
        assert!(
            fixture
                .config_home
                .join("systemd/user/pohunekd.service")
                .exists(),
            "the legacy unit stays"
        );
    }
}

#[test]
fn a_pending_migration_manifest_is_refused_naming_the_bridge_release() {
    let fixture = Fixture::new();
    let data_home = fixture.base().join("data");
    let manifest = data_home.join("pohunek/migrations/durable-session-workers.json");
    write(&manifest, "{}\n");
    let output = fixture.run(
        &[],
        &[("XDG_DATA_HOME", data_home.to_str().expect("utf-8"))],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pending migration manifest"), "{stderr}");
    assert!(stderr.contains("install release v0.40.1 first"), "{stderr}");
    assert!(fixture.pohunek_calls().is_empty(), "no pohunek call ran");
    assert!(manifest.exists(), "the manifest stays");
}

#[test]
fn a_stray_legacy_unit_on_macos_is_named_for_removal() {
    let fixture = Fixture::new();
    fixture.fake_host("Darwin", "arm64");
    write_executable(&fixture.commands.join("sw_vers"), "#!/bin/sh\necho 15.1\n");
    seal_manifest(&fixture.archive, "aarch64-apple-darwin", "daemon");
    fixture.legacy_install();
    let output = fixture.run(&[], &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("remove the file"), "{stderr}");
    assert!(!stderr.contains("v0.40.1"), "{stderr}");
    assert!(fixture.pohunek_calls().is_empty());
}

#[test]
fn an_unknown_argument_fails_without_side_effects() {
    let fixture = Fixture::new();
    let output = fixture.run(&["--unknown"], &[]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(fixture.pohunek_calls().is_empty());
    assert!(fixture.guard_calls().is_empty());
}

#[test]
fn release_workflow_packages_the_wrapper_and_binaries_only() {
    let workflow = read(&repo_root().join(".github/workflows/release-build.yml"));
    assert!(
        workflow.contains("packaging/stage-archive"),
        "the release build workflow stages archives with packaging/stage-archive"
    );
    let stage = read(&repo_root().join("packaging/stage-archive"));
    for expected in [
        "copy_binary pohunekd",
        "copy_binary pohunek\n",
        "copy_binary pohunek-sessiond",
        r#"cp packaging/install-daemon.sh packaging/verify-archive "$staging/packaging/""#,
    ] {
        assert!(
            stage.contains(expected),
            "missing release asset: {expected}"
        );
    }
    assert!(
        !workflow.contains("packaging/systemd") && !stage.contains("packaging/systemd"),
        "unit templates are rendered by `pohunek service`, never shipped"
    );
    assert!(
        !repo_root().join("packaging/systemd").exists(),
        "packaging/systemd templates are replaced by the typed renderers"
    );
}

#[test]
fn release_workflow_packages_static_shell_completions() {
    let stage = read(&repo_root().join("packaging/stage-archive"));
    for expected in [
        r#""$bindir/pohunek" completions bash > "$staging/completions/pohunek.bash""#,
        r#""$bindir/pohunek" completions zsh > "$staging/completions/_pohunek""#,
        r#""$bindir/pohunek" completions fish > "$staging/completions/pohunek.fish""#,
    ] {
        assert!(
            stage.contains(expected),
            "missing packaged completion: {expected}"
        );
    }
}

/// Fake archive CLI: logs its arguments and answers `service status --json`
/// in the pretty envelope the real CLI prints, with a pending transaction
/// from `POHUNEK_TEST_PENDING_OPERATION`/`_STEP` or `null`. With service.toml
/// present it reports `installed: true` and the configured prefix
/// `POHUNEK_TEST_CONFIGURED_PREFIX`, or the raw JSON value
/// `POHUNEK_TEST_CONFIGURED_PREFIX_JSON`; without it `installed: false` and a
/// `null` prefix, as the real CLI does.
/// `POHUNEK_TEST_STATUS_QUERY_EXIT` fails the query with an error document;
/// `POHUNEK_TEST_STATUS_UNEXPECTED` answers without `pending_transaction`.
/// Status reports `transaction_in_progress: true`, as the real CLI does under
/// the wrapper's own lock.
/// `service lock` and `service check` are logged to `POHUNEK_TEST_GUARD_LOG`
/// instead. `service lock` runs the real CLI (`POHUNEK_TEST_REAL_CLI`), which
/// takes the real transaction lock below the fixture's state directory and
/// hands it to the re-executed wrapper. `service check` runs the real CLI
/// with `POHUNEK_TEST_REAL_CHECK=1`; otherwise it refuses without the
/// inherited lock variable, as the real check does for an invalid one, fails
/// with the code `POHUNEK_TEST_CHECK_ERROR`, or passes reporting `locked` as
/// `POHUNEK_TEST_CHECK_LOCKED` (default `true`).
/// `POHUNEK_TEST_SERVICE_STATUS` sets the exit status of install/upgrade.
const FAKE_POHUNEK: &str = r#"#!/bin/sh
if [ "$1" = service ] && { [ "$2" = lock ] || [ "$2" = check ]; }; then
    printf '%s\n' "$*" >> "$POHUNEK_TEST_GUARD_LOG"
    if [ "$2" = lock ] || [ "${POHUNEK_TEST_REAL_CHECK:-0}" = 1 ]; then
        exec "$POHUNEK_TEST_REAL_CLI" "$@"
    fi
    if [ -z "${POHUNEK_SERVICE_LOCK_TOKEN:-}" ]; then
        printf '{\n  "err": {\n    "code": "service_inherited_lock_invalid"\n  }\n}\n'
        exit 1
    fi
    if [ -n "${POHUNEK_TEST_CHECK_ERROR:-}" ]; then
        printf '{\n  "err": {\n    "code": "%s"\n  }\n}\n' "$POHUNEK_TEST_CHECK_ERROR"
        exit 1
    fi
    printf '{\n  "ok": {\n    "operation": "install",\n    "locked": %s\n  }\n}\n' \
        "${POHUNEK_TEST_CHECK_LOCKED:-true}"
    exit 0
fi
printf '%s\n' "$*" >> "$POHUNEK_TEST_POHUNEK_LOG"
if [ "$1" = service ] && [ "$2" = status ]; then
    if [ -n "${POHUNEK_TEST_STATUS_QUERY_EXIT:-}" ]; then
        printf '{\n  "err": {\n    "code": "service_record_invalid"\n  }\n}\n'
        exit "$POHUNEK_TEST_STATUS_QUERY_EXIT"
    fi
    if [ -n "${POHUNEK_TEST_STATUS_UNEXPECTED:-}" ]; then
        printf '{\n  "ok": {\n    "installed": true\n  }\n}\n'
        exit 0
    fi
    pending=null
    if [ -n "${POHUNEK_TEST_PENDING_OPERATION:-}" ]; then
        pending=$(printf '{\n      "operation": "%s",\n      "version": "1.0.0",\n      "step": "%s"\n    }' \
            "$POHUNEK_TEST_PENDING_OPERATION" "$POHUNEK_TEST_PENDING_STEP")
    fi
    installed=false
    prefix=null
    if [ -f "$XDG_CONFIG_HOME/pohunek/service.toml" ]; then
        installed=true
        prefix=${POHUNEK_TEST_CONFIGURED_PREFIX_JSON:-"\"$POHUNEK_TEST_CONFIGURED_PREFIX\""}
    fi
    printf '{\n  "ok": {\n    "installed": %s,\n    "config_path": "%s",\n    "namespace": null,\n    "prefix": %s,\n    "pending_transaction": %s,\n    "transaction_in_progress": true\n  }\n}\n' \
        "$installed" "$XDG_CONFIG_HOME/pohunek/service.toml" "$prefix" "$pending"
    exit 0
fi
exit "${POHUNEK_TEST_SERVICE_STATUS:-0}"
"#;

struct Fixture {
    /// Owns the temporary directory for the fixture's lifetime.
    _root: tempfile::TempDir,
    /// The temporary directory without symlinked components.
    base: PathBuf,
    home: PathBuf,
    archive: PathBuf,
    prefix: PathBuf,
    config_home: PathBuf,
    commands: PathBuf,
    /// `XDG_RUNTIME_DIR` of the run.
    runtime_root: PathBuf,
    pohunek_log: PathBuf,
    guard_log: PathBuf,
    systemctl_log: PathBuf,
}

impl Fixture {
    fn base(&self) -> &Path {
        &self.base
    }

    fn new() -> Self {
        Self::with_runtime_dir(|root| root.join("runtime"))
    }

    fn with_runtime_dir(runtime_dir: impl FnOnce(&Path) -> PathBuf) -> Self {
        let root = pohunek_test_support::tempdir().expect("temp dir");
        // macOS places temporary directories below the `/var` symlink, which
        // the wrapper's trusted-directory check refuses like the installer.
        let base = fs::canonicalize(root.path()).expect("canonical temp dir");
        let archive = base.join("archive");
        let commands = base.join("commands");
        let pohunek_log = base.join("pohunek.log");
        let guard_log = base.join("guard.log");
        let systemctl_log = base.join("systemctl.log");
        // `pohunek service check` requires HOME to be an existing directory.
        create_dirs(&base.join("home"));
        fs::create_dir_all(archive.join("packaging")).expect("archive");
        fs::copy(
            repo_root().join("packaging/install-daemon.sh"),
            archive.join("packaging/install-daemon.sh"),
        )
        .expect("copy wrapper");
        write_executable(&archive.join("pohunek"), FAKE_POHUNEK);
        write_executable(&archive.join("pohunekd"), "#!/bin/sh\nexit 0\n");
        write_executable(&archive.join("pohunek-sessiond"), "#!/bin/sh\nexit 0\n");
        fs::copy(
            repo_root().join("packaging/verify-archive"),
            archive.join("packaging/verify-archive"),
        )
        .expect("copy archive verifier");
        write_executable(
            &commands.join("uname"),
            "#!/bin/sh\ncase \"$1\" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 2 ;; esac\n",
        );
        seal_manifest(&archive, &host_target(), "daemon");
        // `systemctl` only records its calls: the installer never runs it.
        write_executable(
            &commands.join("systemctl"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$POHUNEK_TEST_SYSTEMCTL_LOG\"\n",
        );
        let runtime = runtime_dir(&base);
        let socket_dir = runtime.join("pohunek");
        create_dirs(&socket_dir);
        // The daemon creates its runtime root owner-private, and the wrapper
        // refuses any other mode as `pohunek service install` does.
        fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700))
            .expect("make runtime dir private");
        Self {
            home: base.join("home"),
            prefix: base.join("prefix"),
            config_home: base.join("config"),
            archive,
            commands,
            runtime_root: runtime,
            pohunek_log,
            guard_log,
            systemctl_log,
            _root: root,
            base,
        }
    }

    /// Makes `uname` report `system` and `machine`, as the installer asks.
    fn fake_host(&self, system: &str, machine: &str) {
        write_executable(
            &self.commands.join("uname"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in -s) echo '{system}' ;; -m) echo '{machine}' ;; *) exit 2 ;; esac\n"
            ),
        );
    }

    /// A pre-service install under the fixture prefix: its daemon unit names
    /// `<prefix>/bin/pohunekd`, as the legacy installer rendered it.
    fn legacy_install(&self) {
        let units = self.config_home.join("systemd/user");
        write(
            &units.join("pohunekd.service"),
            &format!(
                "[Service]\nType=notify\nExecStart={}/bin/pohunekd\n",
                self.prefix.display()
            ),
        );
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(args, env).output().expect("run installer")
    }

    /// The installer invocation with the fixture's environment and `env`
    /// applied on top, for runs that redirect a standard stream.
    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let path = format!(
            "{}:{}",
            self.commands.display(),
            std::env::var("PATH").expect("PATH")
        );
        let mut command = Command::new("/bin/sh");
        command
            .env_clear()
            .current_dir(self.base())
            .arg(self.archive.join("packaging/install-daemon.sh"))
            .args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", self.base().join("state"))
            .env("POHUNEK_INSTALL_PREFIX", &self.prefix)
            .env("POHUNEK_TEST_CONFIGURED_PREFIX", &self.prefix)
            .env("XDG_RUNTIME_DIR", &self.runtime_root)
            .env("POHUNEK_TEST_POHUNEK_LOG", &self.pohunek_log)
            .env("POHUNEK_TEST_GUARD_LOG", &self.guard_log)
            .env(
                "POHUNEK_TEST_REAL_CLI",
                pohunek_test_support::bin_exe("pohunek"),
            )
            .env("POHUNEK_TEST_SYSTEMCTL_LOG", &self.systemctl_log)
            .env("PATH", path);
        for (key, value) in env {
            command.env(key, value);
        }
        command
    }

    fn install_call(&self) -> String {
        format!(
            "service install --from {} --prefix {}",
            self.archive.display(),
            self.prefix.display()
        )
    }

    fn upgrade_call(&self) -> String {
        format!("service upgrade --from {}", self.archive.display())
    }

    fn pohunek_calls(&self) -> Vec<String> {
        lines(&self.pohunek_log)
    }

    /// The directory the service backend writes the daemon definition to,
    /// resolved by the CLI's own rule from this fixture's environment.
    fn supervisor_dir(&self) -> PathBuf {
        let env = pohunek_paths::PathEnv {
            xdg_runtime_dir: Some(self.runtime_root.clone().into()),
            xdg_data_home: None,
            xdg_state_home: Some(self.base().join("state").into()),
            xdg_cache_home: None,
            xdg_config_home: Some(self.config_home.clone().into()),
            home: Some(self.home.clone().into()),
        };
        let paths = pohunek_paths::BasePaths::resolve_for(
            pohunek_paths::Platform::current().expect("supported platform"),
            nix::unistd::Uid::effective().as_raw(),
            &env,
        )
        .expect("resolve fixture paths");
        pohunek_cli::service::context::default_supervisor_dir(&paths, Some(&self.home))
            .expect("supervisor directory")
    }

    /// `service lock` and `service check` calls, in order.
    fn guard_calls(&self) -> Vec<String> {
        lines(&self.guard_log)
    }

    /// The `service lock` call that re-executes the wrapper with `args`.
    fn lock_call(&self, args: &[&str]) -> String {
        let mut call = format!(
            "service lock -- sh {}",
            self.archive.join("packaging/install-daemon.sh").display()
        );
        for arg in args {
            call.push(' ');
            call.push_str(arg);
        }
        call
    }

    /// The `service check` call the wrapper makes for the fixture prefix.
    fn check_call(&self) -> String {
        format!("service check --prefix {} --json", self.prefix.display())
    }

    /// Asserts that the wrapper took the lock and ran the check under it.
    fn assert_guarded(&self, args: &[&str]) {
        assert_eq!(
            self.guard_calls(),
            [self.lock_call(args), self.check_call()]
        );
    }

    /// The real CLI with `args` in this fixture's environment.
    fn real_pohunek(&self, args: &[&str]) -> Command {
        let template = self.command(&[], &[]);
        let mut real = Command::new(pohunek_test_support::bin_exe("pohunek"));
        real.env_clear().current_dir(self.base()).args(args);
        for (key, value) in template.get_envs() {
            match value {
                Some(value) => real.env(key, value),
                None => real.env_remove(key),
            };
        }
        real
    }

    fn systemctl_calls(&self) -> Vec<String> {
        lines(&self.systemctl_log)
    }
}

fn lines(path: &Path) -> Vec<String> {
    match fs::read_to_string(path) {
        Ok(text) => text.lines().map(str::to_owned).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

/// The release target of the fixture archive. Every fixture simulates a Linux
/// `x86_64` host (see `Fixture::fake_host`), whatever machine runs the tests, so
/// the Linux-only legacy migration paths behave the same everywhere.
const FIXTURE_TARGET: &str = "x86_64-unknown-linux-gnu";

fn host_target() -> String {
    FIXTURE_TARGET.to_owned()
}

/// Writes the archive MANIFEST over the archive's current contents with
/// `packaging/write-manifest`, as the release workflow does.
fn seal_manifest(archive: &Path, target: &str, component: &str) {
    seal(archive, target, component, MINIMUM_MACOS);
}

/// Seals a daemon manifest that demands `minimum` as the macOS version.
fn seal_manifest_with_minimum(archive: &Path, target: &str, minimum: &str) {
    seal(archive, target, "daemon", minimum);
}

fn seal(archive: &Path, target: &str, component: &str, minimum: &str) {
    let mut command = Command::new("/bin/sh");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("PATH"))
        .current_dir(archive)
        .arg(repo_root().join("packaging/write-manifest"))
        .arg(archive)
        .args([component, ARCHIVE_VERSION, target, "none"]);
    if target.ends_with("apple-darwin") {
        command.arg(minimum);
    }
    let output = command.output().expect("run write-manifest");
    assert_success(&output);
}

fn repo_root() -> PathBuf {
    pohunek_test_support::workspace_root()
}

/// Creates `path` and its missing ancestors without group or other write
/// permission whatever the umask, as the wrapper's directory checks require.
fn create_dirs(path: &Path) {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .expect("create directories");
}

fn write(path: &Path, contents: &str) {
    create_dirs(path.parent().expect("parent"));
    fs::write(path, contents).expect("write file");
}

fn write_executable(path: &Path, contents: &str) {
    create_dirs(path.parent().expect("parent"));
    pohunek_test_support::fs::write_executable(path, contents).expect("write executable");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read fixture file")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
