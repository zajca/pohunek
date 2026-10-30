//! Behavioural tests of packages that export commands, over a real plugin
//! root, real archives and a real session registry.

// Rust guideline compliant 2026-10-28

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::symlink;

use pohunek_test_support::wait::HANG_GUARD;
use protocol::{
    CommandExposure, PackageErrorKind, PackageInstallStatus, PackageLinkParams,
    PackageSelectParams, PackageSetEnabledParams, PackageUninstallParams,
};

use super::test_fixture::{
    commands_document, explicit, Fixture, Package, COMMANDS_PACKAGE, COMMAND_PROGRAM,
    HOST_STATE_COMMAND, INERT_PROGRAM, PACKAGE, PLAIN_FILE, RUNTIME,
};
use crate::session::tests::{assigned_agent_script, params, temp_dir};

/// Archive paths of the files a linked fixture directory ships.
const LINK_PROGRAM: &str = "bin/fleet-state";

/// An explicit-digest install that opts in to package commands, as the
/// current CLI does.
fn accepted(
    path: &str,
    package: &Package,
    enable: bool,
    select: bool,
) -> protocol::PackageInstallParams {
    protocol::PackageInstallParams {
        accept_commands: true,
        ..explicit(path, package, enable, select)
    }
}

fn enable(digest: &package::PackageDigest, enabled: bool) -> PackageSetEnabledParams {
    PackageSetEnabledParams {
        digest: digest.clone(),
        enabled,
    }
}

fn select(digest: &package::PackageDigest) -> PackageSelectParams {
    PackageSelectParams {
        digest: digest.clone(),
    }
}

fn uninstall(digest: &package::PackageDigest) -> PackageUninstallParams {
    PackageUninstallParams {
        digest: digest.clone(),
        remove_modified: false,
    }
}

#[tokio::test]
async fn a_commands_only_package_is_reviewed_installed_and_serves_no_runtime() {
    let fixture = Fixture::new("commands-only-install");
    let package = Package::commands_only(COMMANDS_PACKAGE, "1.0.0", HOST_STATE_COMMAND);
    let path = fixture.write_archive(&package);
    let before = fixture.snapshot();

    let mut review = accepted(&path, &package, true, true);
    review.dry_run = true;
    let preview = fixture
        .registry
        .package_install(review)
        .await
        .expect("preview");
    assert_eq!(preview.status, PackageInstallStatus::Preview);
    assert_eq!(preview.runtime, None);
    assert_eq!(preview.package.runtime_id, None);
    assert_eq!(preview.commands.len(), 1);
    let command = &preview.commands[0];
    assert_eq!(command.name.as_str(), "host-state");
    assert_eq!(command.program, COMMAND_PROGRAM);
    assert_eq!(command.args, ["--json"]);
    assert_eq!(command.expose, CommandExposure::Peers);
    assert_eq!(fixture.snapshot(), before, "a preview changes nothing");

    let installed = fixture
        .registry
        .package_install(accepted(&path, &package, true, true))
        .await
        .expect("install");
    assert_eq!(installed.status, PackageInstallStatus::Installed);
    assert_eq!(installed.runtime, None);
    assert_eq!(installed.commands, preview.commands);
    assert!(installed.package.enabled && installed.package.selected);
    assert_eq!(installed.package.fault, None);
    assert_eq!(installed.package.runtime_id, None);

    // It is a package, not an agent: nothing serves it and no session can be
    // started under its id.
    assert!(
        fixture.host.package_report().rejected.is_empty(),
        "a command-only package is not a rejected runtime"
    );
    let launch = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: COMMANDS_PACKAGE.to_owned(),
            ..params()
        })
        .await
        .expect_err("a command-only package is not an agent");
    assert_eq!(launch.code, "agent_profile_not_found", "{launch:?}");
}

#[tokio::test]
async fn a_command_exposure_defaults_to_local() {
    let fixture = Fixture::new("commands-default-exposure");
    let package = Package::commands_only(
        COMMANDS_PACKAGE,
        "1.0.0",
        "\n[commands.plain]\nprogram = \"bin/fleet-state\"\n",
    );
    let path = fixture.write_archive(&package);

    let installed = fixture
        .registry
        .package_install(accepted(&path, &package, false, false))
        .await
        .expect("install");
    assert_eq!(installed.commands.len(), 1);
    assert_eq!(installed.commands[0].expose, CommandExposure::Local);
    assert!(installed.commands[0].args.is_empty());
}

/// Descriptors that each break one rule of the command export, with the rule
/// they break.
fn invalid_documents() -> Vec<(&'static str, String)> {
    let long_name = "a".repeat(33);
    let many_args = format!("[{}]", vec!["\"x\""; 65].join(", "));
    let long_arg = format!("[\"{}\"]", "x".repeat(1025));
    let many_commands = (0..17).fold(String::new(), |mut tables, index| {
        let _ = write!(
            tables,
            "\n[commands.c{index}]\nprogram = \"{COMMAND_PROGRAM}\"\n"
        );
        tables
    });
    let run = |body: &str| format!("\n[commands.run]\n{body}\n");
    let program = |body: &str| run(&format!("program = \"{COMMAND_PROGRAM}\"\n{body}"));
    let named = |name: &str| format!("\n[commands.{name}]\nprogram = \"{COMMAND_PROGRAM}\"\n");
    let bodies: Vec<(&str, String)> = vec![
        ("absolute program", run("program = \"/bin/sh\"")),
        ("parent segment", run("program = \"../bin/fleet-state\"")),
        ("program missing from the manifest", run("program = \"bin/missing\"")),
        (
            "program without the executable bit",
            run(&format!("program = \"{PLAIN_FILE}\"")),
        ),
        ("65 arguments", program(&format!("args = {many_args}"))),
        ("argument over 1024 bytes", program(&format!("args = {long_arg}"))),
        (
            "control character in an argument",
            program("args = [\"a\\nb\"]"),
        ),
        ("uppercase command name", named("Host-State")),
        ("command name starting with a digit", named("1st")),
        ("command name over 32 bytes", named(&long_name)),
        ("17 commands", many_commands),
        ("empty commands table", "\n[commands]\n".to_owned()),
        ("expose outside local and peers", program("expose = \"everyone\"")),
        ("key a command does not define", program("env = [\"A=B\"]")),
        ("neither a runtime nor commands", String::new()),
        (
            "a partial runtime group",
            format!(
                "\n[runtime]\nid = \"pi\"\nname = \"Pi\"\nprogram = \"/bin/sh\"\nargs = []\ndetect_manifest = \"any\"\n{}",
                program("")
            ),
        ),
        (
            "a runtime-only table next to commands only",
            format!("\n[integration]\nhandler = \"other-v1\"\n{}", program("")),
        ),
    ];
    bodies
        .into_iter()
        .map(|(label, body)| {
            (
                label,
                commands_document(COMMANDS_PACKAGE, "1.0.0", true, &body),
            )
        })
        .collect()
}

#[tokio::test]
async fn an_invalid_command_export_is_refused_before_anything_is_recorded() {
    let fixture = Fixture::new("commands-invalid");
    let descriptor_invalid = invalid_documents();
    let before = fixture.snapshot();
    for (label, document) in &descriptor_invalid {
        let package = Package::with_descriptor_and_command_files(document);
        let path = fixture.write_archive(&package);
        let refusal = fixture
            .registry
            .package_install(accepted(&path, &package, true, true))
            .await
            .expect_err(label);
        assert_eq!(refusal, PackageErrorKind::DescriptorInvalid, "{label}");
    }
    assert_eq!(fixture.snapshot(), before, "nothing was recorded");
}

#[tokio::test]
async fn commands_need_the_package_commands_feature() {
    let fixture = Fixture::new("commands-feature");
    let documents = [
        (
            "commands without requires",
            commands_document(COMMANDS_PACKAGE, "1.0.0", false, HOST_STATE_COMMAND),
        ),
        (
            "an unknown feature wins over a key it owns",
            format!(
                "schema = 1\nid = \"{COMMANDS_PACKAGE}\"\nversion = \"1.0.0\"\nruntime_api = 1\nrequires = [\"commands-from-the-future-v1\"]\n[commands]\nrun = 5\n"
            ),
        ),
    ];
    let before = fixture.snapshot();
    for (label, document) in &documents {
        let package = Package::with_descriptor_and_command_files(document);
        let path = fixture.write_archive(&package);
        let refusal = fixture
            .registry
            .package_install(accepted(&path, &package, true, true))
            .await
            .expect_err(label);
        assert_eq!(
            refusal,
            PackageErrorKind::RuntimeFeatureUnsupported,
            "{label}"
        );
    }
    assert_eq!(fixture.snapshot(), before, "nothing was recorded");
}

#[tokio::test]
async fn a_schema_fault_is_reported_before_a_missing_feature_requirement() {
    let fixture = Fixture::new("commands-fault-order");
    // Both documents break the schema and use `[commands]` without `requires`.
    let runtime_with_commands = {
        let base = Package::build(PACKAGE, "1.0.0", RUNTIME, INERT_PROGRAM);
        let mut entries = package::read_archive(&base.bytes, &package::Limits::DEFAULT)
            .expect("the fixture archive reads")
            .entries()
            .to_vec();
        for entry in &mut entries {
            if entry.path == "runtime.toml" {
                let text = String::from_utf8(entry.contents.clone()).expect("utf-8 descriptor");
                entry.contents = format!(
                    "{}{HOST_STATE_COMMAND}",
                    text.replacen("schema = 1", "schema = 2", 1)
                )
                .into_bytes();
            }
        }
        Package::from_entries(&entries)
    };
    let commands_only = Package::with_descriptor_and_command_files(
        &commands_document(COMMANDS_PACKAGE, "1.0.0", false, HOST_STATE_COMMAND).replacen(
            "schema = 1",
            "schema = 2",
            1,
        ),
    );
    let before = fixture.snapshot();
    for (label, package) in [
        ("runtime next to commands", &runtime_with_commands),
        ("commands only", &commands_only),
    ] {
        let path = fixture.write_archive(package);
        let refusal = fixture
            .registry
            .package_install(accepted(&path, package, true, true))
            .await
            .expect_err(label);
        assert_eq!(refusal, PackageErrorKind::DescriptorInvalid, "{label}");
    }
    assert_eq!(fixture.snapshot(), before, "nothing was recorded");
}

#[tokio::test]
async fn a_linked_directory_keeps_the_program_rules() {
    let fixture = Fixture::new("commands-link");
    let directory = temp_dir("commands-link-source");
    fs::create_dir_all(directory.join("bin")).expect("create the program directory");
    let write = |path: &str, bytes: &[u8], mode: u32| {
        use std::os::unix::fs::PermissionsExt as _;
        let target = directory.join(path);
        fs::write(&target, bytes).expect("write a fixture file");
        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).expect("set the mode");
    };
    write(
        "runtime.toml",
        commands_document(COMMANDS_PACKAGE, "1.0.0", true, HOST_STATE_COMMAND).as_bytes(),
        0o644,
    );
    write(LINK_PROGRAM, b"#!/bin/sh\necho ok\n", 0o755);
    let link = |dry_run| PackageLinkParams {
        directory: directory.to_str().expect("utf-8 path").to_owned(),
        dry_run,
        accept_commands: true,
    };

    let installed = fixture
        .registry
        .package_link(link(false))
        .await
        .expect("link");
    assert_eq!(installed.runtime, None);
    assert_eq!(installed.commands.len(), 1);
    assert!(!installed.package.enabled && !installed.package.selected);

    // A program that is a symbolic link is not a file of the package.
    fs::remove_file(directory.join(LINK_PROGRAM)).expect("remove the program");
    write("bin/real", b"#!/bin/sh\necho ok\n", 0o755);
    symlink(directory.join("bin/real"), directory.join(LINK_PROGRAM)).expect("symlink");
    let before = fixture.snapshot();
    fixture
        .registry
        .package_link(link(false))
        .await
        .expect_err("a symbolic link is refused");
    assert_eq!(fixture.snapshot(), before, "nothing was recorded");
}

#[tokio::test]
async fn a_commands_only_package_follows_the_lifecycle_of_a_runtime_package() {
    let fixture = Fixture::new("commands-lifecycle");
    let first = Package::commands_only(COMMANDS_PACKAGE, "1.0.0", HOST_STATE_COMMAND);
    let second = Package::commands_only(COMMANDS_PACKAGE, "2.0.0", HOST_STATE_COMMAND);
    for package in [&first, &second] {
        let path = fixture.write_archive(package);
        fixture
            .registry
            .package_install(accepted(&path, package, false, false))
            .await
            .expect("install");
    }

    let enabled = fixture
        .registry
        .package_set_enabled(enable(&first.digest, true))
        .await
        .expect("enable");
    assert!(enabled.package.enabled && !enabled.package.selected);
    let selected = fixture
        .registry
        .package_select(select(&first.digest))
        .await
        .expect("select");
    assert!(selected.package.selected);

    // Selecting the second version moves the selection with no integration
    // conflict between two packages that declare none.
    fixture
        .registry
        .package_set_enabled(enable(&second.digest, true))
        .await
        .expect("enable the second version");
    let moved = fixture
        .registry
        .package_select(select(&second.digest))
        .await
        .expect("select the second version");
    assert!(moved.package.selected);

    let listed = fixture.registry.package_list().await.expect("list");
    assert_eq!(listed.packages.len(), 2);
    for info in &listed.packages {
        assert_eq!(info.fault, None, "{info:?}");
        assert_eq!(info.runtime_id, None);
        assert_eq!(info.selection_blocked, None);
    }
    let inspected = fixture
        .registry
        .package_inspect(second.digest.clone())
        .await
        .expect("inspect");
    assert_eq!(inspected.runtime, None);
    assert_eq!(inspected.package.fault, None);
    let doctor = fixture.registry.package_doctor(None).await.expect("doctor");
    assert!(doctor.findings.is_empty(), "{:?}", doctor.findings);

    let disabled = fixture
        .registry
        .package_set_enabled(enable(&second.digest, false))
        .await
        .expect("disable");
    assert!(!disabled.package.enabled);

    for package in [&first, &second] {
        fixture
            .registry
            .package_uninstall(uninstall(&package.digest))
            .await
            .expect("uninstall");
    }
    let listed = fixture.registry.package_list().await.expect("list");
    assert!(listed.packages.is_empty());
}

#[tokio::test]
async fn a_package_exporting_both_launches_sessions_and_carries_its_commands() {
    let tag = "commands-both";
    let dir = temp_dir(tag);
    let marker = dir.join("argv.txt");
    let (script, mut gate) = assigned_agent_script(&dir, &marker);
    let fixture = Fixture::new(&format!("{tag}-plugins"));
    let package = Package::build_with_commands(
        PACKAGE,
        "1.0.0",
        RUNTIME,
        script.to_str().expect("utf-8 script path"),
        HOST_STATE_COMMAND,
    );
    let path = fixture.write_archive(&package);

    let installed = fixture
        .registry
        .package_install(accepted(&path, &package, true, true))
        .await
        .expect("install");
    assert_eq!(installed.commands.len(), 1);
    assert_eq!(
        installed
            .runtime
            .as_ref()
            .map(|runtime| runtime.runtime_id.as_str()),
        Some(RUNTIME)
    );
    assert_eq!(fixture.serving(RUNTIME), Some(package.digest.clone()));

    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: RUNTIME.to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("a runtime package that also exports commands launches");
    gate.write_all(b"go\n").expect("release the exit gate");
    fixture
        .registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the session exits");
}

#[tokio::test]
async fn a_request_without_the_opt_in_is_refused_for_every_package_that_declares_commands() {
    let fixture = Fixture::new("commands-opt-in");
    let tag = "commands-opt-in-agent";
    let dir = temp_dir(tag);
    let marker = dir.join("argv.txt");
    let (script, _gate) = assigned_agent_script(&dir, &marker);
    let packages = [
        Package::commands_only(COMMANDS_PACKAGE, "1.0.0", HOST_STATE_COMMAND),
        Package::build_with_commands(
            PACKAGE,
            "1.0.0",
            RUNTIME,
            script.to_str().expect("utf-8 script path"),
            HOST_STATE_COMMAND,
        ),
    ];
    let before = fixture.snapshot();
    for package in &packages {
        let path = fixture.write_archive(package);
        for dry_run in [true, false] {
            let refusal = fixture
                .registry
                .package_install(protocol::PackageInstallParams {
                    dry_run,
                    ..explicit(&path, package, true, true)
                })
                .await
                .expect_err("an old-style request is refused");
            assert_eq!(refusal, PackageErrorKind::CommandsClientUnsupported);
        }
        assert_eq!(fixture.snapshot(), before, "nothing was written");
    }

    // The same requests with the opt-in install as before.
    for package in &packages {
        let path = fixture.write_archive(package);
        let installed = fixture
            .registry
            .package_install(accepted(&path, package, true, true))
            .await
            .expect("an opted-in request installs");
        assert_eq!(installed.commands.len(), 1);
    }

    // A linked directory is held to the same rule, whether it exports commands
    // only or a runtime as well, and a dry run is refused too.
    for package in &packages {
        let directory = temp_dir("commands-opt-in-link");
        let archive = package::read_archive(&package.bytes, &package::Limits::DEFAULT)
            .expect("the fixture archive reads");
        for entry in archive.entries() {
            let target = directory.join(&entry.path);
            fs::create_dir_all(target.parent().expect("entry parent")).expect("create a directory");
            fs::write(&target, &entry.contents).expect("write a package file");
            let mode = if entry.executable { 0o755 } else { 0o644 };
            fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(mode))
                .expect("set the mode");
        }
        let before = fixture.snapshot();
        for dry_run in [true, false] {
            let refusal = fixture
                .registry
                .package_link(PackageLinkParams {
                    directory: directory.to_str().expect("utf-8 path").to_owned(),
                    dry_run,
                    accept_commands: false,
                })
                .await
                .expect_err("an old-style link is refused");
            assert_eq!(refusal, PackageErrorKind::CommandsClientUnsupported);
            assert_eq!(fixture.snapshot(), before, "nothing was written");
        }
    }
}

#[tokio::test]
async fn a_catalog_cannot_authorize_a_package_that_exports_commands_only() {
    use super::catalog_tests::{
        anchor, catalog, catalog_install, entry, signed_by, signing_key, state,
    };

    let key = signing_key("root");
    let fixture = Fixture::with_anchor("commands-catalog", anchor(&[&key], Vec::new()));
    let package = Package::commands_only(COMMANDS_PACKAGE, "1.0.0", HOST_STATE_COMMAND);
    let document = signed_by(
        &key,
        catalog(
            7,
            vec![entry(&package, COMMANDS_PACKAGE, "fleet-state", "1.0.0")],
        ),
    );
    let before = fixture.snapshot();
    for dry_run in [true, false] {
        let refusal = fixture
            .registry
            .package_install(protocol::PackageInstallParams {
                accept_commands: true,
                ..catalog_install(&fixture, &package, &document, dry_run)
            })
            .await
            .expect_err("a catalog binds a runtime id");
        assert_eq!(refusal, PackageErrorKind::Untrusted);
    }
    assert_eq!(fixture.snapshot(), before, "no package was recorded");
    assert_eq!(
        state(&fixture).high_water(),
        None,
        "the watermark is unchanged"
    );

    // An official runtime package that also declares commands is refused too:
    // the release install paths have no consent step that shows a command.
    let both =
        Package::build_with_commands(PACKAGE, "1.0.0", RUNTIME, INERT_PROGRAM, HOST_STATE_COMMAND);
    let document = signed_by(
        &key,
        catalog(8, vec![entry(&both, PACKAGE, RUNTIME, "1.0.0")]),
    );
    let before = fixture.snapshot();
    for dry_run in [true, false] {
        let refusal = fixture
            .registry
            .package_install(protocol::PackageInstallParams {
                accept_commands: true,
                ..catalog_install(&fixture, &both, &document, dry_run)
            })
            .await
            .expect_err("an official package may not declare commands yet");
        assert_eq!(refusal, PackageErrorKind::Untrusted);
        assert_eq!(fixture.snapshot(), before, "no package was recorded");
        assert_eq!(state(&fixture).high_water(), None);
    }
}
