//! Argument plumbing of `packaging/install-daemon.sh`.
//!
//! The wrapper delegates installation to `pohunek service install|upgrade`,
//! whose transactions are tested in the service engine. These tests run the
//! real script against a fake `pohunek` in a fake archive and a fake
//! `systemctl` on `PATH`, recording every invocation.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
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

#[test]
fn existing_service_config_runs_service_upgrade() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.upgrade_call()]
    );
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
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_STATUS_QUERY_EXIT", "5"),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("service_record_invalid"), "{stderr}");
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    assert!(fixture.systemctl_calls().is_empty());
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }

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
    for component in ["cli", "gui", "web"] {
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
fn a_macos_install_keeps_the_owners_own_binaries_below_the_prefix() {
    // Only the Linux installer ever wrote `<prefix>/bin/pohunekd`; on macOS a
    // file there belongs to the owner.
    let fixture = Fixture::new();
    fixture.fake_host("Darwin", "arm64");
    write_executable(&fixture.commands.join("sw_vers"), "#!/bin/sh\necho 15.1\n");
    seal_manifest(&fixture.archive, "aarch64-apple-darwin", "daemon");
    fixture.legacy_install_at(&fixture.prefix);
    let own = fixture.prefix.join("bin/pohunekd");
    let own_worker = fixture.prefix.join("libexec/pohunek-sessiond");
    fs::remove_file(fixture.config_home.join("systemd/user/pohunekd.service")).expect("unit");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert!(own.exists() && own_worker.exists(), "{output:?}");
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
const STATE_QUERY: &str = "--user is-active pohunekd.service";
const LIST_WORKERS: &str = "--user list-units pohunek-session@* --all --plain --no-legend";
const DISABLE: &str = "--user disable --now pohunekd.service";
const START: &str = "--user start pohunekd.service";
const STOP: &str = "--user stop pohunekd.service";
const RELOAD: &str = "--user daemon-reload";
/// Sibling name the wrapper renames the legacy socket node to
/// (`POHUNEK_BARRIER_NAME` in the script).
const BARRIER_NAME: &str = "retiring";

#[test]
fn idle_legacy_install_is_retired_after_preflight_before_installing() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[("POHUNEK_TEST_LEGACY_ACTIVE", "1")],
    );
    assert_success(&output);
    fixture.assert_guarded(&["--accept-runtime-loss"]);
    let preflight_calls = fixture.pohunek_calls();
    let preflight = preflight_calls.get(1).expect("preflight call");
    assert!(
        preflight.starts_with(&fixture.socket.preflight_call())
            && preflight.ends_with(" --accept-runtime-loss"),
        "unexpected preflight call: {preflight}"
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, LIST_WORKERS, RELOAD]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
    // The connect barrier is undone by design, not by cleanup: the node was
    // renamed away for the preflight and removed once the legacy daemon was
    // retired.
    assert!(
        !fixture.socket.path.exists(),
        "barrier socket was not removed after retirement"
    );
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after retirement"
    );
}

#[test]
fn live_legacy_template_workers_refuse_before_anything_changes() {
    // The activating state is exactly what `--state=active` misses and what a
    // stop can destroy mid-startup, so it must refuse like a running worker.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            (
                "POHUNEK_TEST_LIVE_WORKERS",
                "pohunek-session@s-1.service loaded activating start-pre worker",
            ),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pohunek-session@s-1.service"), "{stderr}");
    assert!(stderr.contains("pohunek session stop <id>"), "{stderr}");
    assert!(
        fixture
            .pohunek_calls()
            .get(1)
            .is_some_and(|call| call.starts_with(&fixture.socket.preflight_call())),
        "unexpected preflight call: {:?}",
        fixture.pohunek_calls()
    );
    assert_eq!(fixture.systemctl_calls(), [STATE_QUERY, LIST_WORKERS]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    // The barrier is undone on the abort path so the operator's still-running
    // legacy install stays reachable through its original socket name.
    assert!(fixture.socket.path.exists(), "socket was not restored");
}

#[test]
fn failed_preflight_stops_before_retiring_the_legacy_install() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_PREFLIGHT_STATUS", "23"),
        ],
    );
    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert!(
        fixture
            .pohunek_calls()
            .get(1)
            .is_some_and(|call| call.starts_with(&fixture.socket.preflight_call())),
        "unexpected preflight call: {:?}",
        fixture.pohunek_calls()
    );
    assert_eq!(fixture.systemctl_calls(), [STATE_QUERY]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was not restored");
}

#[test]
fn a_worker_that_survives_the_stop_refuses_without_removing_legacy_files() {
    // A session created between the preflight and the daemon stop gets its
    // template worker after the first inventory, so only the re-check after
    // the stop can catch it; the run then aborts fail-closed.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            (
                "POHUNEK_TEST_POST_STOP_WORKERS",
                "pohunek-session@s-9.service loaded active running worker",
            ),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pohunek-session@s-9.service"), "{stderr}");
    // The post-stop message reports the true state: the daemon is stopped and
    // disabled, the listed worker appeared after the preflight, and the
    // legacy unit files were kept.
    assert!(
        stderr.contains("the legacy daemon is stopped and disabled"),
        "{stderr}"
    );
    assert!(stderr.contains("their runtime may be lost"), "{stderr}");
    assert!(
        stderr.contains("the legacy unit files were kept"),
        "{stderr}"
    );
    assert!(
        stderr.contains("systemctl --user reset-failed <unit>"),
        "{stderr}"
    );
    assert!(
        fixture
            .pohunek_calls()
            .get(1)
            .is_some_and(|call| call.starts_with(&fixture.socket.preflight_call())),
        "unexpected preflight call: {:?}",
        fixture.pohunek_calls()
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, LIST_WORKERS]
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    // After the stop the moved node is stale, so it is removed, not restored
    // to a daemon that no longer listens.
    assert!(
        !fixture.socket.path.exists(),
        "socket node was restored after the stop"
    );
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the stop"
    );
}

#[test]
fn accepted_runtime_loss_still_refuses_a_worker_that_survives_the_stop() {
    // The consent covers daemon-owned PTYs only; removing the unit files a
    // live template worker runs from would orphan it.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            (
                "POHUNEK_TEST_POST_STOP_WORKERS",
                "pohunek-session@s-9.service loaded activating start worker",
            ),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pohunek-session@s-9.service"), "{stderr}");
    assert!(
        stderr.contains("systemctl --user enable --now pohunekd.service"),
        "{stderr}"
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, LIST_WORKERS]
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the stop"
    );
}

#[test]
fn a_failed_worker_inventory_refuses_before_the_legacy_daemon_stops() {
    // An unanswered `list-units` cannot rule out live template workers, so
    // even with the runtime-loss consent the daemon is never disabled.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_LIST_FAILS_BEFORE_STOP", "1"),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not list the legacy template workers; nothing was changed"),
        "{stderr}"
    );
    assert_eq!(fixture.systemctl_calls(), [STATE_QUERY, LIST_WORKERS]);
    assert!(
        !fixture.systemctl_calls().iter().any(|call| call == DISABLE),
        "the legacy daemon was disabled after a failed inventory"
    );
    assert_eq!(
        fixture.pohunek_calls().len(),
        2,
        "{:?}",
        fixture.pohunek_calls()
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was not restored");
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the abort"
    );
}

#[test]
fn a_failed_post_stop_inventory_keeps_the_legacy_files() {
    // A failed re-check cannot rule out a worker created after the preflight,
    // so it aborts like a found one instead of removing the unit files.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_LIST_FAILS_AFTER_STOP", "1"),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the template workers\ncould not be listed"),
        "{stderr}"
    );
    assert!(
        stderr.contains("the legacy unit files were kept"),
        "{stderr}"
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, LIST_WORKERS]
    );
    assert_eq!(
        fixture.pohunek_calls().len(),
        2,
        "{:?}",
        fixture.pohunek_calls()
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(
        !fixture.socket.path.exists(),
        "socket node was restored after the stop"
    );
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the stop"
    );
}

#[test]
fn a_socket_a_restarted_legacy_daemon_bound_survives_the_barrier_restore() {
    // The legacy unit restarts a failed daemon, which binds a new socket at the
    // original path while the barrier is in place. Putting the moved node back
    // must not replace that live socket; the moved node is then stale.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let restarted = fixture.base().join("restarted.sock");
    let listener = UnixListener::bind(&restarted).expect("bind restarted socket");
    let restarted_inode = fs::metadata(&restarted).expect("restarted socket").ino();
    let restarted_arg = restarted.display().to_string();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_PREFLIGHT_STATUS", "23"),
            ("POHUNEK_TEST_RESTARTED_SOCKET", &restarted_arg),
        ],
    );
    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert!(
        !restarted.exists(),
        "the fake preflight did not move the socket"
    );
    assert_eq!(
        fs::symlink_metadata(&fixture.socket.path)
            .expect("socket at the original path")
            .ino(),
        restarted_inode,
        "the restarted daemon's socket was replaced by the moved node"
    );
    UnixStream::connect(&fixture.socket.path).expect("dial the restarted daemon");
    listener
        .accept()
        .expect("the restarted daemon accepts the connection");
    assert!(
        !fixture.socket.retired_exists(),
        "stale barrier node remains after the restore"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a restarted legacy daemon bound a new control socket"),
        "{stderr}"
    );
    assert_eq!(fixture.systemctl_calls(), [STATE_QUERY]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
}

#[test]
fn a_legacy_daemon_that_is_not_proven_stopped_gets_the_barrier_and_preflight() {
    // Only `inactive` or `failed` proves the legacy daemon has no clients; a
    // daemon starting, stopping, or reloading may still own live PTYs, so a
    // refusing preflight must stop the run before the disable.
    for state in [
        "activating",
        "deactivating",
        "reloading",
        "refreshing",
        "unknown",
    ] {
        let fixture = Fixture::new();
        fixture.legacy_install();
        let output = fixture.run(
            &[],
            &[
                ("POHUNEK_TEST_LEGACY_STATE", state),
                ("POHUNEK_TEST_PREFLIGHT_STATUS", "23"),
            ],
        );
        assert_eq!(output.status.code(), Some(23), "{state}: {output:?}");
        assert!(
            fixture
                .pohunek_calls()
                .get(1)
                .is_some_and(|call| call.starts_with(&fixture.socket.preflight_call())),
            "{state}: unexpected preflight call: {:?}",
            fixture.pohunek_calls()
        );
        assert_eq!(fixture.pohunek_calls().len(), 2, "{state}");
        assert_eq!(fixture.systemctl_calls(), [STATE_QUERY], "{state}");
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{state}: {} was removed", legacy.display());
        }
        assert!(
            fixture.socket.path.exists(),
            "{state}: socket was not restored"
        );
        assert!(
            !fixture.socket.retired_exists(),
            "{state}: renamed barrier socket remains after the refusal"
        );
    }

    // An accepting preflight lets the retirement of a transitional daemon
    // proceed through the same barrier.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(&[], &[("POHUNEK_TEST_LEGACY_STATE", "activating")]);
    assert_success(&output);
    assert!(
        fixture
            .pohunek_calls()
            .get(1)
            .is_some_and(|call| call.starts_with(&fixture.socket.preflight_call())),
        "unexpected preflight call: {:?}",
        fixture.pohunek_calls()
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, LIST_WORKERS, RELOAD]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
    assert!(!fixture.socket.retired_exists());
}

#[test]
fn a_daemon_not_proven_stopped_without_a_socket_refuses_before_anything_changes() {
    // A daemon still starting may not have bound its socket yet, so no
    // preflight can ask it about live PTYs.
    for state in ["activating", "deactivating", "active"] {
        let fixture = Fixture::new();
        fixture.legacy_install();
        fs::remove_file(&fixture.socket.path).expect("remove socket");
        let output = fixture.run(
            &["--accept-runtime-loss"],
            &[("POHUNEK_TEST_LEGACY_STATE", state)],
        );
        assert_eq!(output.status.code(), Some(1), "{state}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("not stopped (state: {state})")),
            "{stderr}"
        );
        assert!(stderr.contains("control socket is missing"), "{stderr}");
        assert!(stderr.contains("nothing was changed"), "{stderr}");
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{state}");
        assert_eq!(fixture.systemctl_calls(), [STATE_QUERY], "{state}");
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{state}: {} was removed", legacy.display());
        }
    }
}

#[test]
fn a_legacy_socket_at_the_sun_path_limit_is_retired_through_a_dialable_barrier() {
    // The legacy daemon may bind the longest path `sun_path` holds; the
    // renamed node must stay within that limit so the preflight can dial it.
    let max_bytes = pohunek_paths::Platform::current()
        .expect("supported platform")
        .socket_path_max_bytes();
    let fixture = Fixture::with_socket_path_bytes(max_bytes);
    assert_eq!(fixture.socket.path.as_os_str().len(), max_bytes);
    // Any sibling name longer than the socket's would not fit.
    let mut longer = fixture.socket.path.clone().into_os_string();
    longer.push(".1");
    std::os::unix::net::SocketAddr::from_pathname(&longer)
        .expect_err("a path past the socket's length exceeds sun_path");
    fixture.legacy_install();
    let output = fixture.run(&[], &[("POHUNEK_TEST_LEGACY_ACTIVE", "1")]);
    assert_success(&output);
    let calls = fixture.pohunek_calls();
    assert_eq!(calls.get(1), Some(&fixture.socket.preflight_call()));
    let barrier = fixture.socket.barrier();
    assert!(
        barrier.as_os_str().len() <= fixture.socket.path.as_os_str().len(),
        "barrier {} is longer than the socket it replaces",
        barrier.display()
    );
    std::os::unix::net::SocketAddr::from_pathname(&barrier).expect("barrier path fits sockaddr_un");
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, LIST_WORKERS, RELOAD]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
    assert!(!fixture.socket.path.exists());
    assert!(!fixture.socket.retired_exists());
}

#[test]
fn a_leftover_barrier_node_refuses_before_the_socket_moves() {
    // rename(2) would replace a leftover node and `mv` would move the socket
    // into a leftover directory, so either refuses with nothing changed.
    for leftover_is_dir in [false, true] {
        let fixture = Fixture::new();
        fixture.legacy_install();
        let barrier = fixture.socket.barrier();
        if leftover_is_dir {
            fs::create_dir(&barrier).expect("leftover barrier dir");
        } else {
            write(&barrier, "stale\n");
        }
        let output = fixture.run(&[], &[("POHUNEK_TEST_LEGACY_ACTIVE", "1")]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("connect-barrier node from an interrupted earlier run"),
            "{stderr}"
        );
        assert!(stderr.contains(&barrier.display().to_string()), "{stderr}");
        assert!(stderr.contains("nothing was changed"), "{stderr}");
        assert_eq!(fixture.pohunek_calls(), [STATUS]);
        assert_eq!(fixture.systemctl_calls(), [STATE_QUERY]);
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{} was removed", legacy.display());
        }
        assert!(fixture.socket.path.exists(), "socket was moved");
        assert_eq!(barrier.is_dir(), leftover_is_dir, "leftover was replaced");
        if leftover_is_dir {
            assert_eq!(
                barrier.read_dir().expect("list leftover dir").count(),
                0,
                "socket was moved into the leftover directory"
            );
        } else {
            assert_eq!(read(&barrier), "stale\n", "leftover was replaced");
        }
    }
}

#[test]
fn a_socket_left_at_the_barrier_by_a_killed_run_is_named_for_restoring() {
    // A run killed after the rename leaves a running daemon reachable only at
    // the barrier name; the refusal names the command that restores it.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let barrier = fixture.socket.barrier();
    fs::rename(&fixture.socket.path, &barrier).expect("move socket to barrier");
    let output = fixture.run(&[], &[("POHUNEK_TEST_LEGACY_ACTIVE", "1")]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("control socket is missing"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "mv '{}' '{}'",
            barrier.display(),
            fixture.socket.path.display()
        )),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    assert_eq!(fixture.systemctl_calls(), [STATE_QUERY]);
    assert!(!fixture.socket.path.exists());
    assert!(fixture.socket.retired_exists(), "barrier node was touched");
}

#[test]
fn a_failed_legacy_state_query_refuses_before_anything_changes() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[("POHUNEK_TEST_LEGACY_STATE", "query-fails")],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not query the legacy daemon state"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    assert_eq!(fixture.systemctl_calls(), [STATE_QUERY]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was moved");
    assert!(!fixture.socket.retired_exists(), "socket was moved");
}

#[test]
fn a_stopped_legacy_daemon_is_started_for_the_preflight_snapshot() {
    // The service daemon imports the legacy records only from the manifest a
    // preflight against the running legacy daemon writes, so a stopped one is
    // started for it and retired through the same barrier.
    for state in ["inactive", "failed"] {
        let fixture = Fixture::new();
        fixture.legacy_install();
        let output = fixture.run(&[], &[("POHUNEK_TEST_LEGACY_STATE", state)]);
        assert_success(&output);
        assert_eq!(
            fixture.pohunek_calls(),
            [
                STATUS.to_owned(),
                fixture.socket.preflight_call(),
                fixture.install_call()
            ],
            "{state}"
        );
        assert_eq!(
            fixture.systemctl_calls(),
            [
                STATE_QUERY,
                START,
                STATE_QUERY,
                LIST_WORKERS,
                DISABLE,
                LIST_WORKERS,
                RELOAD
            ],
            "{state}"
        );
        for legacy in fixture.legacy_files() {
            assert!(
                !legacy.exists(),
                "{state}: {} was not removed",
                legacy.display()
            );
        }
        assert!(
            !fixture.socket.path.exists(),
            "{state}: stale socket node remains"
        );
        assert!(
            !fixture.socket.retired_exists(),
            "{state}: renamed barrier socket remains"
        );
    }
}

#[test]
fn a_refused_snapshot_of_a_started_legacy_daemon_stops_it_again() {
    // A refusing preflight (live PTYs, or a store it cannot fingerprint)
    // leaves the legacy install as the run found it: installed and stopped.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(&[], &[("POHUNEK_TEST_PREFLIGHT_STATUS", "23")]);
    assert_eq!(output.status.code(), Some(23), "{output:?}");
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.socket.preflight_call()]
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, START, STATE_QUERY, STOP]
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stopped the legacy daemon again"),
        "{stderr}"
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was not restored");
    assert!(!fixture.socket.retired_exists(), "barrier node remains");

    // Live template workers refuse the same way after the snapshot.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[(
            "POHUNEK_TEST_LIVE_WORKERS",
            "pohunek-session@s-1.service loaded active running worker",
        )],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, START, STATE_QUERY, LIST_WORKERS, STOP]
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was not restored");
}

#[test]
fn an_interrupted_start_of_a_stopped_legacy_daemon_stops_it_again() {
    // A signal before the barrier exists still leaves the daemon this run
    // started stopped, as the run found it; the signal is re-raised.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_INTERRUPT_AT", "start"),
            ("POHUNEK_TEST_INTERRUPT_SIGNAL", "INT"),
        ],
    );
    assert_killed_by(&output, libc::SIGINT);
    assert_eq!(fixture.systemctl_calls(), [STATE_QUERY, START, STOP]);
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was moved");
}

/// A named refused start: extra environment and the expected `systemctl` calls.
type StartCase = (
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static [&'static str],
);

#[test]
fn a_stopped_legacy_daemon_that_cannot_start_is_not_retired() {
    // Without a running legacy daemon there is no snapshot, so the run refuses
    // instead of retiring the install without its migration manifest.
    let cases: [StartCase; 2] = [
        (
            "start fails",
            &[("POHUNEK_TEST_START_STATUS", "1")],
            &[STATE_QUERY, START, STOP],
        ),
        (
            "not active after the start",
            &[("POHUNEK_TEST_STATE_AFTER_START", "failed")],
            &[STATE_QUERY, START, STATE_QUERY, STOP],
        ),
    ];
    for (name, env, systemctl_calls) in cases {
        let fixture = Fixture::new();
        fixture.legacy_install();
        let output = fixture.run(&["--accept-runtime-loss"], env);
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("the legacy install was not retired"),
            "{name}: {stderr}"
        );
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        assert_eq!(fixture.systemctl_calls(), systemctl_calls, "{name}");
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{name}: {} was removed", legacy.display());
        }
        assert!(fixture.socket.path.exists(), "{name}: socket was moved");
        assert!(!fixture.socket.retired_exists(), "{name}: socket was moved");
    }
}

#[test]
fn a_failed_disable_of_a_running_daemon_restores_its_socket() {
    // A daemon the failed `disable --now` left running keeps serving only on
    // its bound socket, so the node goes back to the name new clients open.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_DISABLE_STATUS", "4"),
        ],
    );
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("failed (status 4)"), "{stderr}");
    assert!(
        stderr.contains("the legacy unit files were kept"),
        "{stderr}"
    );
    assert!(
        stderr.contains("may still be running (state: active)"),
        "{stderr}"
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, STATE_QUERY]
    );
    assert_eq!(
        fixture.pohunek_calls().len(),
        2,
        "no install or upgrade may run: {:?}",
        fixture.pohunek_calls()
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was not restored");
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the abort"
    );
}

#[test]
fn a_failed_disable_with_an_unknown_daemon_state_restores_its_socket() {
    // A failed state query cannot prove the daemon stopped, so it is treated
    // as still running.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_DISABLE_STATUS", "1"),
            ("POHUNEK_TEST_STATE_AFTER_DISABLE", "query-fails"),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("may still be running (state: unknown)"),
        "{stderr}"
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, STATE_QUERY]
    );
    assert_eq!(
        fixture.pohunek_calls().len(),
        2,
        "{:?}",
        fixture.pohunek_calls()
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was not restored");
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the abort"
    );
}

#[test]
fn a_failed_disable_of_a_stopped_daemon_removes_the_stale_barrier() {
    // `disable --now` can stop the daemon and still fail; the moved node of a
    // stopped daemon is stale, so it is removed rather than restored.
    for stopped_state in ["inactive", "failed"] {
        let fixture = Fixture::new();
        fixture.legacy_install();
        let output = fixture.run(
            &[],
            &[
                ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
                ("POHUNEK_TEST_DISABLE_STATUS", "5"),
                ("POHUNEK_TEST_STATE_AFTER_DISABLE", stopped_state),
            ],
        );
        assert_eq!(output.status.code(), Some(5), "{stopped_state}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("failed (status 5)"), "{stderr}");
        assert!(stderr.contains("the legacy daemon is stopped"), "{stderr}");
        assert!(
            stderr.contains("the legacy unit files were kept"),
            "{stderr}"
        );
        assert_eq!(
            fixture.systemctl_calls(),
            [STATE_QUERY, LIST_WORKERS, DISABLE, STATE_QUERY],
            "{stopped_state}"
        );
        assert_eq!(
            fixture.pohunek_calls().len(),
            2,
            "no install or upgrade may run: {:?}",
            fixture.pohunek_calls()
        );
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{} was removed", legacy.display());
        }
        assert!(
            !fixture.socket.path.exists(),
            "{stopped_state}: socket node was restored for a stopped daemon"
        );
        assert!(
            !fixture.socket.retired_exists(),
            "{stopped_state}: renamed barrier socket remains"
        );
    }
}

#[test]
fn an_interrupted_retirement_restores_the_socket_of_a_running_daemon() {
    // A signal after the barrier rename, while the daemon still runs, must not
    // leave it reachable only through the moved node: the next run refuses a
    // missing `daemon.sock`. The installer re-raises the signal after the
    // cleanup, so it dies from it.
    for (point, signal, number, systemctl_calls) in [
        (
            "preflight",
            "TERM",
            libc::SIGTERM,
            &[STATE_QUERY, STATE_QUERY][..],
        ),
        (
            "preflight",
            "INT",
            libc::SIGINT,
            &[STATE_QUERY, STATE_QUERY][..],
        ),
        (
            "disable",
            "HUP",
            libc::SIGHUP,
            &[STATE_QUERY, LIST_WORKERS, DISABLE, STATE_QUERY][..],
        ),
    ] {
        let fixture = Fixture::new();
        fixture.legacy_install();
        let output = fixture.run(
            &[],
            &[
                ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
                ("POHUNEK_TEST_INTERRUPT_AT", point),
                ("POHUNEK_TEST_INTERRUPT_SIGNAL", signal),
            ],
        );
        assert_killed_by(&output, number);
        assert_eq!(
            fixture.systemctl_calls(),
            systemctl_calls,
            "{point}/{signal}"
        );
        assert_eq!(
            fixture.pohunek_calls().len(),
            2,
            "{point}: no install or upgrade may run: {:?}",
            fixture.pohunek_calls()
        );
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{point}: {} was removed", legacy.display());
        }
        assert!(
            fixture.socket.path.exists(),
            "{point}: socket was not restored"
        );
        assert!(
            !fixture.socket.retired_exists(),
            "{point}: renamed barrier socket remains after the interruption"
        );
    }
}

#[test]
fn an_interrupted_retirement_of_a_stopped_daemon_removes_the_stale_barrier() {
    // The signal lands while `disable --now` stops the daemon; its moved node
    // is stale, so the cleanup removes it and leaves the original name vacant.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_STATE_AFTER_DISABLE", "inactive"),
            ("POHUNEK_TEST_INTERRUPT_AT", "disable"),
            ("POHUNEK_TEST_INTERRUPT_SIGNAL", "TERM"),
        ],
    );
    assert_killed_by(&output, libc::SIGTERM);
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, DISABLE, STATE_QUERY]
    );
    assert_eq!(
        fixture.pohunek_calls().len(),
        2,
        "no install or upgrade may run: {:?}",
        fixture.pohunek_calls()
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(
        !fixture.socket.path.exists(),
        "socket node was restored for a stopped daemon"
    );
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the interruption"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_unexpected_exit_after_the_barrier_restores_the_socket() {
    // With stderr on `/dev/full` the refusal's first diagnostic fails, so
    // `set -e` ends the run before its own restore; the exit trap restores the
    // socket of the still-running daemon and keeps the failing status.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full");
    let output = fixture
        .command(
            &[],
            &[
                ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
                (
                    "POHUNEK_TEST_LIVE_WORKERS",
                    "pohunek-session@s-1.service loaded active running worker",
                ),
            ],
        )
        .stderr(full)
        .output()
        .expect("run installer");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(
        fixture.systemctl_calls(),
        [STATE_QUERY, LIST_WORKERS, STATE_QUERY]
    );
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "socket was not restored");
    assert!(
        !fixture.socket.retired_exists(),
        "renamed barrier socket remains after the unexpected exit"
    );
}

#[test]
fn rerun_after_a_partial_retirement_takes_a_fresh_snapshot() {
    // The previous run disabled the legacy daemon but stopped before removing
    // its unit files and binaries; the daemon's own shutdown may have changed
    // the store since that run's manifest, so the rerun snapshots it again.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [
            STATUS.to_owned(),
            fixture.socket.preflight_call(),
            fixture.install_call()
        ]
    );
    assert_eq!(
        fixture.systemctl_calls(),
        [
            STATE_QUERY,
            START,
            STATE_QUERY,
            LIST_WORKERS,
            DISABLE,
            LIST_WORKERS,
            RELOAD
        ]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }

    // Only a legacy binary survived the previous run.
    let fixture = Fixture::new();
    write(&fixture.prefix.join("bin/pohunekd"), "legacy\n");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.install_call()]
    );
    assert!(fixture.systemctl_calls().is_empty());
    assert!(!fixture.prefix.join("bin/pohunekd").exists());
}

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
        fixture.legacy_install();
        let prefix = spell(&fixture.prefix);
        let output = fixture.run(
            &[],
            &[
                ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
                ("POHUNEK_INSTALL_PREFIX", &prefix),
            ],
        );
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("POHUNEK_INSTALL_PREFIX must be an absolute path"),
            "{name}: {stderr}"
        );
        assert!(stderr.contains("nothing was changed"), "{name}: {stderr}");
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        assert!(fixture.systemctl_calls().is_empty(), "{name}");
        assert!(fixture.socket.path.exists(), "{name}: the socket moved");
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{name}: {} was removed", legacy.display());
        }
    }
}

#[test]
fn an_upgrade_refuses_a_prefix_other_than_the_configured_one() {
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    let other = fixture.base().join("other");
    fixture.legacy_install_at(&other);
    write(&fixture.prefix.join("bin/pohunekd"), "legacy\n");
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            (
                "POHUNEK_INSTALL_PREFIX",
                other.to_str().expect("utf-8 path"),
            ),
        ],
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
    assert!(fixture.socket.path.exists(), "the socket moved");
    assert!(other.join("bin/pohunekd").exists());
    assert!(other.join("libexec/pohunek-sessiond").exists());
    assert!(fixture.prefix.join("bin/pohunekd").exists());
}

#[test]
fn an_upgrade_retires_legacy_binaries_under_the_configured_prefix() {
    // Without POHUNEK_INSTALL_PREFIX the configured prefix, not `$HOME/.local`,
    // is the one cleaned.
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    fixture.legacy_install();
    let home_prefix = fixture.home.join(".local");
    write(&home_prefix.join("bin/pohunekd"), "unrelated\n");
    let output = fixture
        .command(&[], &[])
        .env_remove("POHUNEK_INSTALL_PREFIX")
        .output()
        .expect("run installer");
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [
            STATUS.to_owned(),
            fixture.socket.preflight_call(),
            fixture.upgrade_call()
        ]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
    assert!(home_prefix.join("bin/pohunekd").exists());

    // The same prefix spelled with redundant slashes is the configured one.
    let fixture = Fixture::new();
    write(&fixture.config_home.join("pohunek/service.toml"), "");
    fixture.legacy_install();
    let spelled = format!("{}//", fixture.prefix.display()).replacen('/', "//", 1);
    let output = fixture.run(&[], &[("POHUNEK_INSTALL_PREFIX", &spelled)]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [
            STATUS.to_owned(),
            fixture.socket.preflight_call(),
            fixture.upgrade_call()
        ]
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
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
        fixture.legacy_install();
        let output = fixture.run(&[], &[("POHUNEK_TEST_CONFIGURED_PREFIX_JSON", value)]);
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("nothing was changed"), "{name}: {stderr}");
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        assert!(fixture.systemctl_calls().is_empty(), "{name}");
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{name}: {} was removed", legacy.display());
        }
    }
}

#[test]
fn a_legacy_unit_of_another_prefix_refuses_before_anything_changes() {
    let fixture = Fixture::new();
    let legacy_prefix = fixture.base().join("legacy-prefix");
    fixture.legacy_install_at(&legacy_prefix);
    write(&fixture.prefix.join("bin/pohunekd"), "unrelated\n");
    let output = fixture.run(&[], &[("POHUNEK_TEST_LEGACY_ACTIVE", "1")]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("which is not <prefix>/bin/pohunekd"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was changed"), "{stderr}");
    assert_eq!(fixture.pohunek_calls(), [STATUS]);
    assert!(fixture.systemctl_calls().is_empty());
    assert!(fixture.socket.path.exists(), "the socket moved");
    assert!(legacy_prefix.join("bin/pohunekd").exists());
    assert!(fixture.prefix.join("bin/pohunekd").exists());

    // A legacy unit spelled with a redundant slash names the same prefix.
    let fixture = Fixture::new();
    fixture.legacy_install_at(&PathBuf::from(format!("{}/", fixture.prefix.display())));
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
}

#[test]
fn legacy_binary_symlinks_are_left_in_place() {
    let fixture = Fixture::new();
    let outside = fixture.base().join("outside");
    write(&outside.join("pohunekd"), "foreign daemon\n");
    write(&outside.join("pohunek-sessiond"), "foreign worker\n");
    create_dirs(&fixture.prefix.join("bin"));
    create_dirs(&fixture.prefix.join("libexec"));
    std::os::unix::fs::symlink(
        outside.join("pohunekd"),
        fixture.prefix.join("bin/pohunekd"),
    )
    .expect("binary symlink");
    std::os::unix::fs::symlink(
        outside.join("pohunek-sessiond"),
        fixture.prefix.join("libexec/pohunek-sessiond"),
    )
    .expect("worker symlink");
    let output = fixture.run(&[], &[]);
    assert_success(&output);
    assert_eq!(
        fixture.pohunek_calls(),
        [STATUS.to_owned(), fixture.install_call()]
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("left"), "{stderr}");
    for link in ["bin/pohunekd", "libexec/pohunek-sessiond"] {
        assert!(
            fixture.prefix.join(link).symlink_metadata().is_ok(),
            "the {link} symlink was removed"
        );
    }
    assert_eq!(read(&outside.join("pohunekd")), "foreign daemon\n");
    assert_eq!(read(&outside.join("pohunek-sessiond")), "foreign worker\n");
}

/// Builds an untrusted directory into a fixture that holds a legacy install.
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
fn an_untrusted_directory_refuses_before_the_legacy_install_changes() {
    // `pohunek service install|upgrade` refuses these directories, so the
    // legacy install must not be retired for an install that cannot follow.
    // The real `service check` judges them with the installer's own rules.
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
            let runtime = fixture.socket.path.parent().expect("runtime root");
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
        fixture.legacy_install();
        setup(&fixture);
        let output = fixture.run(
            &["--accept-runtime-loss"],
            &[
                ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
                ("POHUNEK_TEST_REAL_CHECK", "1"),
            ],
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
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{name}: {} was removed", legacy.display());
        }
        assert!(fixture.socket.path.exists(), "{name}: the socket moved");
    }
}

#[test]
fn a_running_service_transaction_refuses_before_anything_changes() {
    // Another `pohunek service` command holds the transaction lock, so the
    // wrapper's own `service lock` is refused before the wrapper runs again.
    let fixture = Fixture::new();
    fixture.legacy_install();
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

    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[("POHUNEK_TEST_LEGACY_ACTIVE", "1")],
    );
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
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
    assert!(fixture.socket.path.exists(), "the socket moved");
}

#[test]
fn no_transaction_starts_while_the_legacy_install_is_retired() {
    // A `pohunek service` command started from elsewhere between the
    // wrapper's first query and its final command is refused: the wrapper
    // holds the lock for its whole run, the retirement included.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &["--accept-runtime-loss"],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_COMPETITOR", "1"),
        ],
    );
    assert_success(&output);
    assert_eq!(read(&fixture.base().join("competitor-status")), "1\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("another `pohunek service` command is running"),
        "{stderr}"
    );
    fixture.assert_guarded(&["--accept-runtime-loss"]);
    assert_eq!(
        fixture.pohunek_calls().last(),
        Some(&fixture.install_call()),
        "the wrapper's own install still runs under its lock"
    );
    for legacy in fixture.legacy_files() {
        assert!(!legacy.exists(), "{} was not removed", legacy.display());
    }
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
        fixture.legacy_install();
        write(&fixture.base().join("home-file"), "");
        let home = fixture.base().join(home);
        let output = fixture.run(
            &["--accept-runtime-loss"],
            &[
                ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
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
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{name}: {} was removed", legacy.display());
        }
        assert!(fixture.socket.path.exists(), "{name}: the socket moved");
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
        fixture.legacy_install();
        let output = fixture.run(&[], &[("POHUNEK_TEST_LEGACY_ACTIVE", "1"), env]);
        assert_eq!(output.status.code(), Some(1), "{name}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{name}: {stderr}");
        assert!(stderr.contains("nothing was changed"), "{name}: {stderr}");
        fixture.assert_guarded(&[]);
        assert_eq!(fixture.pohunek_calls(), [STATUS], "{name}");
        assert!(fixture.systemctl_calls().is_empty(), "{name}");
        for legacy in fixture.legacy_files() {
            assert!(legacy.exists(), "{name}: {} was removed", legacy.display());
        }
    }

    // A lock variable set by the caller is not trusted: without a held lock
    // the check refuses, and the wrapper does not take one of its own.
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
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
    for legacy in fixture.legacy_files() {
        assert!(legacy.exists(), "{} was removed", legacy.display());
    }
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
fn a_failed_install_after_retirement_explains_how_to_recover() {
    let fixture = Fixture::new();
    fixture.legacy_install();
    let output = fixture.run(
        &[],
        &[
            ("POHUNEK_TEST_LEGACY_ACTIVE", "1"),
            ("POHUNEK_TEST_SERVICE_STATUS", "7"),
        ],
    );
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("already retired"), "{stderr}");
    assert!(stderr.contains("re-run"), "{stderr}");

    let fixture = Fixture::new();
    let output = fixture.run(&[], &[("POHUNEK_TEST_SERVICE_STATUS", "7")]);
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("already retired"));
}

#[test]
fn a_missing_staged_binary_or_extra_argument_fails_without_side_effects() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.archive.join("pohunek-sessiond")).expect("remove worker");
    let output = fixture.run(&[], &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("pohunek-sessiond"));
    assert!(fixture.pohunek_calls().is_empty());

    assert!(fixture.guard_calls().is_empty());

    let fixture = Fixture::new();
    let output = fixture.run(&["--unknown"], &[]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(fixture.pohunek_calls().is_empty());
    assert!(fixture.guard_calls().is_empty());
}

#[test]
fn release_workflow_packages_the_wrapper_and_binaries_only() {
    let workflow = read(&repo_root().join(".github/workflows/release.yml"));
    assert!(
        workflow.contains("packaging/stage-archive"),
        "the release workflow stages archives with packaging/stage-archive"
    );
    let stage = read(&repo_root().join("packaging/stage-archive"));
    for expected in [
        "copy_binary pohunekd",
        "copy_binary pohunek\n",
        "copy_binary pohunek-sessiond",
        r#"cp packaging/install-daemon.sh "$staging/packaging/""#,
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
/// `POHUNEK_TEST_COMPETITOR=1` makes the preflight start a competing real
/// `pohunek service lock -- true` without the inherited lock, as a command
/// started from elsewhere would, and write its exit status to
/// `POHUNEK_TEST_COMPETITOR_STATUS`.
/// `POHUNEK_TEST_PREFLIGHT_STATUS` and `POHUNEK_TEST_SERVICE_STATUS` set the
/// exit status of the migration preflight and of install/upgrade.
/// `POHUNEK_TEST_INTERRUPT_AT=preflight` makes the preflight send
/// `POHUNEK_TEST_INTERRUPT_SIGNAL` to the installer before it returns.
/// A preflight whose `--socket` names no socket node exits 97, as the real
/// CLI fails to dial it. `POHUNEK_TEST_RESTARTED_SOCKET` names a socket the
/// preflight renames to the legacy socket path before it returns, as a
/// legacy daemon restarted by `Restart=on-failure` binds a new one there.
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
if [ "$1" = migration ]; then
    if [ "${3:-}" = --socket ] && [ ! -S "${4:-}" ]; then
        exit 97
    fi
    if [ -n "${POHUNEK_TEST_RESTARTED_SOCKET:-}" ]; then
        mv "$POHUNEK_TEST_RESTARTED_SOCKET" "$XDG_RUNTIME_DIR/pohunek/daemon.sock"
    fi
    if [ "${POHUNEK_TEST_COMPETITOR:-0}" = 1 ]; then
        competitor=0
        (unset POHUNEK_SERVICE_LOCK_TOKEN; exec "$POHUNEK_TEST_REAL_CLI" service lock -- true) \
            || competitor=$?
        printf '%s\n' "$competitor" > "$POHUNEK_TEST_COMPETITOR_STATUS"
    fi
    if [ "${POHUNEK_TEST_INTERRUPT_AT:-}" = preflight ]; then
        kill -s "$POHUNEK_TEST_INTERRUPT_SIGNAL" "$PPID"
    fi
    exit "${POHUNEK_TEST_PREFLIGHT_STATUS:-0}"
fi
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
    socket: BarrierSocket,
    pohunek_log: PathBuf,
    guard_log: PathBuf,
    systemctl_log: PathBuf,
}

/// The legacy daemon's control socket and the connect-barrier node the script
/// renames it to.
struct BarrierSocket {
    /// Path the daemon bound (`XDG_RUNTIME_DIR/pohunek/daemon.sock`).
    path: PathBuf,
}

impl BarrierSocket {
    /// Sibling path the wrapper renames the socket to for the preflight.
    fn barrier(&self) -> PathBuf {
        self.path.with_file_name(BARRIER_NAME)
    }

    /// The `migration preflight --socket` call the wrapper makes against the
    /// renamed node, without the optional `--accept-runtime-loss`.
    fn preflight_call(&self) -> String {
        format!("migration preflight --socket {}", self.barrier().display())
    }

    /// Whether the renamed barrier node remains next to the socket.
    fn retired_exists(&self) -> bool {
        self.barrier().symlink_metadata().is_ok()
    }
}

impl Fixture {
    fn base(&self) -> &Path {
        &self.base
    }

    fn new() -> Self {
        Self::with_runtime_dir(|root| root.join("runtime"))
    }

    /// A fixture whose legacy socket path is exactly `socket_bytes` long,
    /// padded through the runtime directory name.
    fn with_socket_path_bytes(socket_bytes: usize) -> Self {
        Self::with_runtime_dir(|root| {
            let suffix_bytes = Path::new("pohunek/daemon.sock").as_os_str().len() + 1;
            let root_bytes = root.as_os_str().len() + 1;
            let padding = socket_bytes
                .checked_sub(root_bytes + suffix_bytes)
                .filter(|padding| *padding > 0)
                .unwrap_or_else(|| {
                    panic!(
                        "temp root {} leaves no room for a {socket_bytes}-byte socket path",
                        root.display()
                    )
                });
            root.join("r".repeat(padding))
        })
    }

    fn with_runtime_dir(runtime_dir: impl FnOnce(&Path) -> PathBuf) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
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
        write_executable(
            &commands.join("uname"),
            "#!/bin/sh\ncase \"$1\" in -s) echo Linux ;; -m) echo x86_64 ;; *) exit 2 ;; esac\n",
        );
        seal_manifest(&archive, &host_target(), "daemon");
        // `is-active` reports `inactive`, `active` with
        // `POHUNEK_TEST_LEGACY_ACTIVE=1`, or any `POHUNEK_TEST_LEGACY_STATE`;
        // after a successful `start` (exit status `POHUNEK_TEST_START_STATUS`,
        // interrupted like `disable` with `POHUNEK_TEST_INTERRUPT_AT=start`)
        // it reports `POHUNEK_TEST_STATE_AFTER_START` or `active`, and once a
        // stop ran `POHUNEK_TEST_STATE_AFTER_DISABLE` or the initial state. The
        // state `query-fails` answers nothing on stdout and exits 1.
        write_executable(
            &commands.join("systemctl"),
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$POHUNEK_TEST_SYSTEMCTL_LOG\"\n\
             case \"$*\" in\n\
             '--user start '*)\n\
                 start_status=${POHUNEK_TEST_START_STATUS:-0}\n\
                 [ \"$start_status\" -eq 0 ] && : > \"$POHUNEK_TEST_SYSTEMCTL_START_MARKER\"\n\
                 if [ \"${POHUNEK_TEST_INTERRUPT_AT:-}\" = start ]; then\n\
                     kill -s \"$POHUNEK_TEST_INTERRUPT_SIGNAL\" \"$PPID\"\n\
                 fi\n\
                 exit \"$start_status\" ;;\n\
             *disable*|*stop*)\n\
                 : > \"$POHUNEK_TEST_SYSTEMCTL_STOP_MARKER\"\n\
                 if [ \"${POHUNEK_TEST_INTERRUPT_AT:-}\" = disable ]; then\n\
                     kill -s \"$POHUNEK_TEST_INTERRUPT_SIGNAL\" \"$PPID\"\n\
                 fi\n\
                 exit \"${POHUNEK_TEST_DISABLE_STATUS:-0}\" ;;\n\
             *list-units*)\n\
                 if [ -f \"${POHUNEK_TEST_SYSTEMCTL_STOP_MARKER:-}\" ]; then\n\
                     if [ \"${POHUNEK_TEST_LIST_FAILS_AFTER_STOP:-0}\" = 1 ]; then\n\
                         echo 'Failed to connect to bus' >&2; exit 1\n\
                     fi\n\
                     printf '%s' \"${POHUNEK_TEST_POST_STOP_WORKERS:-}\"\n\
                 else\n\
                     if [ \"${POHUNEK_TEST_LIST_FAILS_BEFORE_STOP:-0}\" = 1 ]; then\n\
                         echo 'Failed to connect to bus' >&2; exit 1\n\
                     fi\n\
                     printf '%s' \"${POHUNEK_TEST_LIVE_WORKERS:-}\"\n\
                 fi ;;\n\
             *is-active*)\n\
                 state=inactive\n\
                 [ \"${POHUNEK_TEST_LEGACY_ACTIVE:-0}\" = 1 ] && state=active\n\
                 [ -n \"${POHUNEK_TEST_LEGACY_STATE:-}\" ] && state=$POHUNEK_TEST_LEGACY_STATE\n\
                 if [ -f \"${POHUNEK_TEST_SYSTEMCTL_START_MARKER:-}\" ] \\\n\
                     && [ ! -f \"${POHUNEK_TEST_SYSTEMCTL_STOP_MARKER:-}\" ]; then\n\
                     state=${POHUNEK_TEST_STATE_AFTER_START:-active}\n\
                 fi\n\
                 if [ -f \"${POHUNEK_TEST_SYSTEMCTL_STOP_MARKER:-}\" ] \\\n\
                     && [ -n \"${POHUNEK_TEST_STATE_AFTER_DISABLE:-}\" ]; then\n\
                     state=$POHUNEK_TEST_STATE_AFTER_DISABLE\n\
                 fi\n\
                 if [ \"$state\" = query-fails ]; then\n\
                     echo 'Failed to connect to bus' >&2; exit 1\n\
                 fi\n\
                 case \"$*\" in *--quiet*) ;; *) printf '%s\\n' \"$state\" ;; esac\n\
                 [ \"$state\" = active ] || exit 3 ;;\n\
             esac\n",
        );
        // The legacy daemon binds its control socket as a real AF_UNIX node,
        // so the barrier rename has a socket file to move.
        let runtime = runtime_dir(&base);
        let socket_dir = runtime.join("pohunek");
        create_dirs(&socket_dir);
        // The daemon creates its runtime root owner-private, and the wrapper
        // refuses any other mode as `pohunek service install` does.
        fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700))
            .expect("make runtime dir private");
        let socket = socket_dir.join("daemon.sock");
        UnixListener::bind(&socket).expect("bind barrier socket");
        Self {
            home: base.join("home"),
            prefix: base.join("prefix"),
            config_home: base.join("config"),
            archive,
            commands,
            socket: BarrierSocket { path: socket },
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

    fn legacy_files(&self) -> [PathBuf; 5] {
        let units = self.config_home.join("systemd/user");
        [
            units.join("pohunekd.service"),
            units.join("pohunek-session@.service"),
            units.join("pohunek-sessions.slice"),
            self.prefix.join("bin/pohunekd"),
            self.prefix.join("libexec/pohunek-sessiond"),
        ]
    }

    /// A pre-service install under the fixture prefix: its daemon unit names
    /// `<prefix>/bin/pohunekd`, as the legacy installer rendered it.
    fn legacy_install(&self) {
        self.legacy_install_at(&self.prefix);
    }

    fn legacy_install_at(&self, prefix: &Path) {
        let units = self.config_home.join("systemd/user");
        write(
            &units.join("pohunekd.service"),
            &format!(
                "[Service]\nType=notify\nExecStart={}/bin/pohunekd\n",
                prefix.display()
            ),
        );
        write(&units.join("pohunek-session@.service"), "legacy\n");
        write(&units.join("pohunek-sessions.slice"), "legacy\n");
        write(&prefix.join("bin/pohunekd"), "legacy\n");
        write(&prefix.join("libexec/pohunek-sessiond"), "legacy\n");
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
        let mut command = Command::new("sh");
        command
            .arg(self.archive.join("packaging/install-daemon.sh"))
            .args(args)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", self.base().join("state"))
            .env("POHUNEK_INSTALL_PREFIX", &self.prefix)
            .env("POHUNEK_TEST_CONFIGURED_PREFIX", &self.prefix)
            .env(
                "XDG_RUNTIME_DIR",
                self.socket
                    .path
                    .parent()
                    .and_then(|dir| dir.parent())
                    .map_or(
                        std::env::temp_dir().join("missing-pohunek-runtime"),
                        ToOwned::to_owned,
                    ),
            )
            .env(
                "POHUNEK_TEST_SYSTEMCTL_STOP_MARKER",
                self.base().join("systemctl-stop-marker"),
            )
            .env(
                "POHUNEK_TEST_SYSTEMCTL_START_MARKER",
                self.base().join("systemctl-start-marker"),
            )
            .env("POHUNEK_TEST_POHUNEK_LOG", &self.pohunek_log)
            .env("POHUNEK_TEST_GUARD_LOG", &self.guard_log)
            .env(
                "POHUNEK_TEST_REAL_CLI",
                pohunek_test_support::bin_exe("pohunek"),
            )
            .env(
                "POHUNEK_TEST_COMPETITOR_STATUS",
                self.base().join("competitor-status"),
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
            xdg_runtime_dir: self
                .socket
                .path
                .parent()
                .and_then(Path::parent)
                .map(Into::into),
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
        real.args(args);
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
    let mut command = Command::new("sh");
    command
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
    write(path, contents);
    let mut permissions = fs::metadata(path)
        .expect("executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable mode");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read fixture file")
}

/// Asserts that the wrapper died from `signal`.
///
/// The wrapper runs below `pohunek service lock`, which reports a child a
/// signal ended with 128 plus the signal number, as a shell does.
fn assert_killed_by(output: &Output, signal: i32) {
    assert_eq!(output.status.code(), Some(128 + signal), "{output:?}");
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
