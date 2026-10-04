//! Behavioural tests of the package lifecycle over a real plugin root, real
//! archives and a real session registry.

// Rust guideline compliant 2026-10-04

use std::fs;
use std::io::Write as _;

use pohunek_test_support::wait::HANG_GUARD;
use protocol::{
    PackageErrorKind, PackageFault, PackageFindingKind, PackageInstallStatus, PackageLinkParams,
    PackageOrigin, PackageSelectParams, PackageSetEnabledParams, PackageTrust,
    PackageUninstallParams,
};

use super::test_fixture::{explicit, Fixture, Package, INERT_PROGRAM, PACKAGE, RUNTIME};
use crate::agent::host::RESERVED_RUNTIME_IDS;
use crate::session::tests::{
    assigned_agent_script, durable_recovery, params, temp_dir, temp_store_path,
};

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

fn uninstall(digest: &package::PackageDigest, remove_modified: bool) -> PackageUninstallParams {
    PackageUninstallParams {
        digest: digest.clone(),
        remove_modified,
    }
}

/// The official aliases: the reserved runtime ids other than the shell.
fn aliases() -> Vec<&'static str> {
    RESERVED_RUNTIME_IDS
        .into_iter()
        .filter(|id| *id != protocol::RuntimeId::SHELL)
        .collect()
}

#[tokio::test]
async fn a_package_goes_from_install_to_uninstall_and_the_runtime_registry_follows() {
    let fixture = Fixture::new("package-round-trip");
    let package = Package::pi();
    let path = fixture.write_archive(&package);

    let installed = fixture
        .registry
        .package_install(explicit(&path, &package, false, false))
        .await
        .expect("install");
    assert_eq!(installed.status, PackageInstallStatus::Installed);
    assert!(installed.reloaded);
    assert_eq!(installed.package.origin, PackageOrigin::ExplicitDigest);
    assert!(!installed.package.enabled && !installed.package.selected);
    assert_eq!(installed.package.fault, None);
    assert_eq!(installed.runtime.runtime_id.as_str(), RUNTIME);
    assert_eq!(installed.runtime.program, INERT_PROGRAM);
    assert_eq!(installed.runtime.args, ["--model", "fast"]);
    assert!(installed.runtime.resumable && installed.runtime.forkable);
    assert_eq!(fixture.serving(RUNTIME), None, "recorded but not loaded");

    let listed = fixture.registry.package_list().await.expect("list");
    assert_eq!(listed.generation, 1);
    assert_eq!(listed.packages.len(), 1);
    assert_eq!(listed.packages[0].digest, package.digest);

    let enabled = fixture
        .registry
        .package_set_enabled(enable(&package.digest, true))
        .await
        .expect("enable");
    assert!(enabled.package.enabled && !enabled.package.selected);
    assert_eq!(fixture.serving(RUNTIME), None, "enabled but not selected");

    let selected = fixture
        .registry
        .package_select(select(&package.digest))
        .await
        .expect("select");
    assert!(selected.package.selected && selected.reloaded);
    assert_eq!(fixture.serving(RUNTIME), Some(package.digest.clone()));

    let disabled = fixture
        .registry
        .package_set_enabled(enable(&package.digest, false))
        .await
        .expect("disable");
    assert!(!disabled.package.enabled);
    assert_eq!(
        fixture.serving(RUNTIME),
        None,
        "disabled packages leave the registry"
    );

    fixture
        .registry
        .package_set_enabled(enable(&package.digest, true))
        .await
        .expect("enable again");
    assert_eq!(fixture.serving(RUNTIME), Some(package.digest.clone()));

    let inspected = fixture
        .registry
        .package_inspect(package.digest.clone())
        .await
        .expect("inspect");
    assert_eq!(
        inspected.runtime.expect("descriptor loads").display_name,
        "Pi"
    );

    let removed = fixture
        .registry
        .package_uninstall(uninstall(&package.digest, false))
        .await
        .expect("uninstall");
    assert!(removed.reloaded);
    assert_eq!(fixture.serving(RUNTIME), None);
    assert!(fixture
        .registry
        .package_list()
        .await
        .expect("list")
        .packages
        .is_empty());
    assert!(
        !fixture.root(&package.digest).exists(),
        "the root is deleted"
    );
    assert_eq!(
        fixture
            .registry
            .package_inspect(package.digest.clone())
            .await
            .expect_err("gone"),
        PackageErrorKind::NotInstalled
    );
}

#[tokio::test]
async fn an_install_that_cannot_load_leaves_no_record_and_no_root() {
    let fixture = Fixture::new("package-atomic");
    let first = Package::pi();
    let first_path = fixture.write_archive(&first);
    fixture
        .registry
        .package_install(explicit(&first_path, &first, true, true))
        .await
        .expect("a valid package installs");
    let before = fixture.snapshot();

    let mut refusals: Vec<(String, package::PackageDigest, PackageErrorKind)> = Vec::new();
    let malformed = Package::with_descriptor("this is not toml {");
    refusals.push((
        fixture.write_archive(&malformed),
        malformed.digest.clone(),
        PackageErrorKind::DescriptorInvalid,
    ));
    let missing = Package::without_descriptor();
    refusals.push((
        fixture.write_archive(&missing),
        missing.digest.clone(),
        PackageErrorKind::DescriptorInvalid,
    ));
    let shell = Package::build(
        "acme.runtime.shell",
        "1.0.0",
        protocol::RuntimeId::SHELL,
        INERT_PROGRAM,
    );
    refusals.push((
        fixture.write_archive(&shell),
        shell.digest.clone(),
        PackageErrorKind::RuntimeNotClaimable,
    ));
    for alias in aliases() {
        let package = Package::build("acme.runtime.alias", "1.0.0", alias, INERT_PROGRAM);
        refusals.push((
            fixture.write_archive(&package),
            package.digest.clone(),
            PackageErrorKind::RuntimeNotClaimable,
        ));
    }
    for (path, digest, kind) in refusals {
        let params = protocol::PackageInstallParams {
            archive_path: path,
            trust: PackageTrust::ExplicitDigest { digest },
            enable: true,
            select: true,
            dry_run: false,
        };
        assert_eq!(
            fixture
                .registry
                .package_install(params)
                .await
                .expect_err("refused"),
            kind
        );
        assert_eq!(
            fixture.snapshot(),
            before,
            "a refused install changes nothing"
        );
    }

    assert_eq!(fixture.snapshot(), before);
    assert_eq!(fixture.serving(RUNTIME), Some(first.digest));
}

#[tokio::test]
async fn an_unusable_archive_or_path_leaves_no_record_and_no_root() {
    let fixture = Fixture::new("package-unusable");
    let first = Package::pi();
    let first_path = fixture.write_archive(&first);
    fixture
        .registry
        .package_install(explicit(&first_path, &first, true, true))
        .await
        .expect("a valid package installs");
    let before = fixture.snapshot();

    // An archive whose digest is not the one the owner pinned.
    let other = Package::build(PACKAGE, "2.0.0", RUNTIME, INERT_PROGRAM);
    let other_path = fixture.write_archive(&other);
    let wrong_digest = protocol::PackageInstallParams {
        archive_path: other_path.clone(),
        trust: PackageTrust::ExplicitDigest {
            digest: first.digest.clone(),
        },
        enable: true,
        select: true,
        dry_run: false,
    };
    assert_eq!(
        fixture
            .registry
            .package_install(wrong_digest)
            .await
            .expect_err("digest"),
        PackageErrorKind::ArchiveInvalid
    );

    // Bytes that are not an archive, and paths that are not readable files.
    let garbage = fixture.write("garbage.tar.zst", b"not an archive");
    let garbage_params = explicit(&garbage, &other, true, true);
    assert_eq!(
        fixture
            .registry
            .package_install(garbage_params)
            .await
            .expect_err("garbage"),
        PackageErrorKind::ArchiveInvalid
    );
    for path in [
        "relative.tar.zst".to_owned(),
        format!("{}/missing", fixture.dir.display()),
    ] {
        assert_eq!(
            fixture
                .registry
                .package_install(explicit(&path, &other, true, true))
                .await
                .expect_err("unreadable"),
            PackageErrorKind::SourceUnreadable
        );
    }
    assert_eq!(fixture.snapshot(), before);
    assert_eq!(fixture.serving(RUNTIME), Some(first.digest));
}

#[tokio::test]
async fn a_dry_run_validates_and_changes_nothing() {
    let fixture = Fixture::new("package-dry-run");
    let package = Package::pi();
    let path = fixture.write_archive(&package);
    let mut params = explicit(&path, &package, true, true);
    params.dry_run = true;

    let preview = fixture
        .registry
        .package_install(params.clone())
        .await
        .expect("preview");

    assert_eq!(preview.status, PackageInstallStatus::Preview);
    assert!(!preview.reloaded);
    assert!(preview.package.enabled && preview.package.selected);
    assert_eq!(preview.runtime.runtime_id.as_str(), RUNTIME);
    assert!(fixture
        .registry
        .package_list()
        .await
        .expect("list")
        .packages
        .is_empty());
    assert!(!fixture.root(&package.digest).exists());

    // The preview applies the same refusals as the install.
    let alias = Package::build("acme.runtime.alias", "1.0.0", aliases()[0], INERT_PROGRAM);
    let alias_path = fixture.write_archive(&alias);
    let mut refused = explicit(&alias_path, &alias, true, true);
    refused.dry_run = true;
    assert_eq!(
        fixture
            .registry
            .package_install(refused)
            .await
            .expect_err("refused"),
        PackageErrorKind::RuntimeNotClaimable
    );
}

#[tokio::test]
async fn installing_again_reports_the_recorded_state_and_restores_a_missing_root() {
    let fixture = Fixture::new("package-reinstall");
    let package = Package::pi();
    let path = fixture.write_archive(&package);
    fixture
        .registry
        .package_install(explicit(&path, &package, false, false))
        .await
        .expect("install");

    let again = fixture
        .registry
        .package_install(explicit(&path, &package, true, true))
        .await
        .expect("idempotent");
    assert_eq!(again.status, PackageInstallStatus::AlreadyInstalled);
    assert!(!again.package.enabled, "the recorded state is kept");

    fs::remove_dir_all(fixture.root(&package.digest)).expect("remove the root");
    let listed = fixture.registry.package_list().await.expect("list");
    assert_eq!(listed.packages[0].fault, Some(PackageFault::RootMissing));
    let restored = fixture
        .registry
        .package_install(explicit(&path, &package, false, false))
        .await
        .expect("restore");
    assert_eq!(restored.status, PackageInstallStatus::RootRestored);
    assert_eq!(restored.package.fault, None);
}

#[tokio::test]
async fn identity_and_runtime_id_conflicts_are_refused() {
    let fixture = Fixture::new("package-conflicts");
    let first = Package::pi();
    let first_path = fixture.write_archive(&first);
    fixture
        .registry
        .package_install(explicit(&first_path, &first, true, true))
        .await
        .expect("install");

    // The same package id and version from another archive.
    let rebuilt = Package::build(PACKAGE, "1.0.0", RUNTIME, "/bin/true");
    let rebuilt_path = fixture.write_archive(&rebuilt);
    assert_eq!(
        fixture
            .registry
            .package_install(explicit(&rebuilt_path, &rebuilt, true, true))
            .await
            .expect_err("identity"),
        PackageErrorKind::IdentityInstalled
    );
    assert!(!fixture.root(&rebuilt.digest).exists());

    // Another package id serving a runtime id that is already served.
    let rival = Package::build("acme.runtime.rival", "1.0.0", RUNTIME, INERT_PROGRAM);
    let rival_path = fixture.write_archive(&rival);
    for (enable, select) in [(true, true), (false, false)] {
        assert_eq!(
            fixture
                .registry
                .package_install(explicit(&rival_path, &rival, enable, select))
                .await
                .expect_err("runtime conflict"),
            PackageErrorKind::RuntimeConflict
        );
    }
    assert!(!fixture.root(&rival.digest).exists());
    assert_eq!(fixture.serving(RUNTIME), Some(first.digest.clone()));

    // A built-in runtime id is never claimable by a local package.
    for alias in aliases() {
        let package = Package::build("acme.runtime.alias", "1.0.0", alias, INERT_PROGRAM);
        let path = fixture.write_archive(&package);
        assert_eq!(
            fixture
                .registry
                .package_install(explicit(&path, &package, true, true))
                .await
                .expect_err("alias"),
            PackageErrorKind::RuntimeNotClaimable
        );
    }
}

#[tokio::test]
async fn enabling_or_selecting_cannot_create_a_runtime_id_conflict() {
    let fixture = Fixture::new("package-enable-conflict");
    let first = Package::pi();
    let first_path = fixture.write_archive(&first);
    fixture
        .registry
        .package_install(explicit(&first_path, &first, false, false))
        .await
        .expect("install while nothing serves the runtime id");
    let rival = Package::build("acme.runtime.rival", "1.0.0", RUNTIME, INERT_PROGRAM);
    let rival_path = fixture.write_archive(&rival);
    fixture
        .registry
        .package_install(explicit(&rival_path, &rival, true, true))
        .await
        .expect("the rival takes the runtime id");
    let before = fixture.snapshot();

    assert_eq!(
        fixture
            .registry
            .package_set_enabled(enable(&first.digest, true))
            .await
            .expect_err("conflict"),
        PackageErrorKind::RuntimeConflict
    );
    assert_eq!(
        fixture
            .registry
            .package_select(select(&first.digest))
            .await
            .expect_err("conflict"),
        PackageErrorKind::RuntimeConflict
    );
    assert_eq!(fixture.snapshot(), before, "the refusals changed nothing");
    assert_eq!(fixture.serving(RUNTIME), Some(rival.digest));
}

#[tokio::test]
async fn a_second_version_installs_side_by_side_and_serves_only_once_selected() {
    let fixture = Fixture::new("package-versions");
    let first = Package::pi();
    let second = Package::build(PACKAGE, "1.1.0", RUNTIME, "/bin/true");
    for (package, select) in [(&first, true), (&second, false)] {
        let path = fixture.write_archive(package);
        fixture
            .registry
            .package_install(explicit(&path, package, true, select))
            .await
            .expect("install");
    }

    assert_eq!(fixture.serving(RUNTIME), Some(first.digest.clone()));
    let listed = fixture.registry.package_list().await.expect("list");
    let selected: Vec<_> = listed.packages.iter().filter(|p| p.selected).collect();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].digest, first.digest);

    fixture
        .registry
        .package_select(select(&second.digest))
        .await
        .expect("select the second version");
    assert_eq!(fixture.serving(RUNTIME), Some(second.digest));
}

#[tokio::test]
async fn a_modified_root_is_reported_and_refuses_every_change_but_removal_as_modified() {
    let fixture = Fixture::new("package-modified");
    let package = Package::pi();
    let path = fixture.write_archive(&package);
    fixture
        .registry
        .package_install(explicit(&path, &package, true, true))
        .await
        .expect("install");

    // A verified root is not removed as modified.
    assert_eq!(
        fixture
            .registry
            .package_uninstall(uninstall(&package.digest, true))
            .await
            .expect_err("intact"),
        PackageErrorKind::RootIntact
    );

    fixture.tamper(&package.digest);
    let listed = fixture.registry.package_list().await.expect("list");
    assert_eq!(listed.packages[0].fault, Some(PackageFault::RootModified));
    assert_eq!(listed.packages[0].runtime_id, None);
    let doctor = fixture.registry.package_doctor(None).await.expect("doctor");
    assert_eq!(doctor.findings.len(), 1);
    assert_eq!(doctor.findings[0].kind, PackageFindingKind::Fault);
    assert_eq!(doctor.findings[0].fault, Some(PackageFault::RootModified));
    let filtered = fixture
        .registry
        .package_doctor(Some(
            protocol::PackageId::parse("acme.runtime.other").expect("id"),
        ))
        .await
        .expect("doctor");
    assert!(
        filtered.findings.is_empty(),
        "the filter restricts the report"
    );

    assert_eq!(
        fixture
            .registry
            .package_set_enabled(enable(&package.digest, true))
            .await
            .expect_err("enable"),
        PackageErrorKind::RootInvalid
    );
    assert_eq!(
        fixture
            .registry
            .package_select(select(&package.digest))
            .await
            .expect_err("select"),
        PackageErrorKind::RootInvalid
    );
    assert_eq!(
        fixture
            .registry
            .package_uninstall(uninstall(&package.digest, false))
            .await
            .expect_err("uninstall"),
        PackageErrorKind::RootInvalid
    );

    let removed = fixture
        .registry
        .package_uninstall(uninstall(&package.digest, true))
        .await
        .expect("removed as modified");
    assert!(removed.reloaded);
    assert!(fixture
        .registry
        .package_list()
        .await
        .expect("list")
        .packages
        .is_empty());
    assert!(!fixture.root(&package.digest).exists());
    assert_eq!(fixture.serving(RUNTIME), None);
}

#[tokio::test]
async fn doctor_reports_roots_without_a_record() {
    let fixture = Fixture::new("package-unregistered");
    let package = Package::pi();
    let path = fixture.write_archive(&package);
    fixture
        .registry
        .package_install(explicit(&path, &package, true, true))
        .await
        .expect("install");
    let stray =
        package::PackageDigest::parse(&format!("sha256:{}", "ab".repeat(32))).expect("digest");
    fs::create_dir(fixture.root(&stray)).expect("a root nobody recorded");

    let doctor = fixture.registry.package_doctor(None).await.expect("doctor");

    assert_eq!(doctor.findings.len(), 1);
    assert_eq!(
        doctor.findings[0].kind,
        PackageFindingKind::UnregisteredRoot
    );
    assert_eq!(doctor.findings[0].digest, stray);
    assert!(doctor.findings[0].package.is_none());
}

#[tokio::test]
async fn a_linked_directory_is_copied_disabled_and_never_loaded() {
    let fixture = Fixture::new("package-link");
    let directory = fixture.dir.join("developer");
    fs::create_dir(&directory).expect("developer directory");
    let source = crate::agent::host::fixture::pi_shaped_document(
        std::path::Path::new(INERT_PROGRAM),
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .replace(
        "detect_manifest = \"any\"",
        "detect_manifest = \"detect.toml\"",
    );
    fs::write(directory.join("runtime.toml"), &source).expect("descriptor");
    fs::write(
        directory.join("detect.toml"),
        "[[rules]]\nid = \"idle_prompt\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n",
    )
    .expect("manifest");
    let link = |dry_run| PackageLinkParams {
        directory: directory.to_str().expect("utf-8").to_owned(),
        dry_run,
    };
    let before = fixture.snapshot();

    let preview = fixture
        .registry
        .package_link(link(true))
        .await
        .expect("preview");
    assert_eq!(preview.status, PackageInstallStatus::Preview);
    assert_eq!(fixture.snapshot(), before);

    let linked = fixture
        .registry
        .package_link(link(false))
        .await
        .expect("link");
    assert_eq!(linked.status, PackageInstallStatus::Installed);
    assert_eq!(linked.package.origin, PackageOrigin::Link);
    assert!(!linked.package.enabled && !linked.package.selected);
    assert_eq!(
        fixture.serving(RUNTIME),
        None,
        "a link is never loaded on its own"
    );
    let digest = linked.package.digest.clone();

    // The installed copy is independent of the directory, and other content
    // under the same id and version is refused.
    fs::write(
        directory.join("runtime.toml"),
        source.replace(INERT_PROGRAM, "/bin/true"),
    )
    .expect("edit the directory");
    let listed = fixture.registry.package_list().await.expect("list");
    assert_eq!(listed.packages[0].fault, None);
    assert_eq!(
        fixture
            .registry
            .package_link(link(false))
            .await
            .expect_err("the directory changed without a version bump"),
        PackageErrorKind::IdentityInstalled
    );

    // The same content links again as already installed.
    fs::write(directory.join("runtime.toml"), &source).expect("restore");
    let again = fixture
        .registry
        .package_link(link(false))
        .await
        .expect("again");
    assert_eq!(again.status, PackageInstallStatus::AlreadyInstalled);
    assert_eq!(again.package.digest, digest);

    for relative in ["developer".to_owned(), String::new()] {
        assert_eq!(
            fixture
                .registry
                .package_link(PackageLinkParams {
                    directory: relative,
                    dry_run: false,
                })
                .await
                .expect_err("not absolute"),
            PackageErrorKind::SourceUnreadable
        );
    }
}

#[tokio::test]
async fn a_linked_directory_cannot_claim_a_reserved_runtime_id() {
    let fixture = Fixture::new("package-link-reserved");
    let directory = fixture.dir.join("developer");
    fs::create_dir(&directory).expect("developer directory");
    let alias = aliases()[0];
    let document = crate::agent::host::fixture::pi_shaped_document(
        std::path::Path::new(INERT_PROGRAM),
        crate::agent::host::fixture::PI_SHAPED_NO_CHECK,
    )
    .replace("id = \"pi\"", &format!("id = \"{alias}\""))
    .replace(
        "detect_manifest = \"any\"",
        "detect_manifest = \"detect.toml\"",
    );
    fs::write(directory.join("runtime.toml"), document).expect("descriptor");
    fs::write(directory.join("detect.toml"), "[[rules]]\nid = \"idle_prompt\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n").expect("manifest");

    let refused = fixture
        .registry
        .package_link(PackageLinkParams {
            directory: directory.to_str().expect("utf-8").to_owned(),
            dry_run: false,
        })
        .await
        .expect_err("alias");

    assert_eq!(refused, PackageErrorKind::RuntimeNotClaimable);
}

#[tokio::test]
async fn a_host_without_a_package_store_refuses_package_methods() {
    let registry = crate::session::SessionRegistry::default();
    assert_eq!(
        registry.package_list().await.expect_err("no store"),
        PackageErrorKind::RegistryFailed
    );
}

/// Opens a durable-session fixture whose package launches the agent script
/// and returns it with the script's exit gate.
async fn session_fixture(
    tag: &str,
) -> (
    Fixture,
    Package,
    std::path::PathBuf,
    std::fs::File,
    std::path::PathBuf,
) {
    let dir = temp_dir(tag);
    let marker = dir.join("argv.txt");
    let (script, gate) = assigned_agent_script(&dir, &marker);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_store(&format!("{tag}-plugins"), store_path.clone());
    let package = Package::build(
        PACKAGE,
        "1.0.0",
        RUNTIME,
        script.to_str().expect("utf-8 script path"),
    );
    let path = fixture.write_archive(&package);
    fixture
        .registry
        .package_install(explicit(&path, &package, true, true))
        .await
        .expect("install the agent package");
    (fixture, package, dir, gate, store_path)
}

fn new_params(dir: &std::path::Path) -> protocol::SessionNewParams {
    protocol::SessionNewParams {
        agent: RUNTIME.to_owned(),
        cwd: Some(dir.to_path_buf()),
        ..params()
    }
}

#[tokio::test]
async fn a_pinned_session_blocks_uninstall_even_of_a_modified_root() {
    let (fixture, package, dir, _gate, _store) = session_fixture("package-retained").await;
    let created = fixture
        .registry
        .create(new_params(&dir))
        .await
        .expect("create");

    let listed = fixture.registry.package_list().await.expect("list");
    assert!(listed.packages[0].referenced);
    assert_eq!(
        fixture
            .registry
            .package_uninstall(uninstall(&package.digest, false))
            .await
            .expect_err("pinned"),
        PackageErrorKind::Referenced
    );

    fixture.tamper(&package.digest);
    assert_eq!(
        fixture
            .registry
            .package_uninstall(uninstall(&package.digest, true))
            .await
            .expect_err("a retained tampered root is not removed"),
        PackageErrorKind::Referenced
    );
    assert!(fixture.root(&package.digest).exists());

    let _ = fixture.registry.stop(&created.id).await;
    fixture
        .registry
        .remove(&created.id)
        .await
        .expect("remove the session");
    fixture
        .registry
        .package_uninstall(uninstall(&package.digest, true))
        .await
        .expect("unpinned, the modified root is removed");
}

#[tokio::test]
async fn a_disabled_package_refuses_fresh_launches_and_still_resumes_its_session() {
    let (fixture, package, dir, mut gate, store) = session_fixture("package-disabled").await;
    let created = fixture
        .registry
        .create(new_params(&dir))
        .await
        .expect("create");
    gate.write_all(b"go\n").expect("release the exit gate");
    fixture
        .registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the session exits");
    let pin = durable_recovery(&store, &created.id).launch_binding;
    assert!(pin.binding().is_some(), "the session pinned the package");

    fixture
        .registry
        .package_set_enabled(enable(&package.digest, false))
        .await
        .expect("disable");

    let fresh = fixture
        .registry
        .create(new_params(&dir))
        .await
        .expect_err("a disabled package serves no fresh launch");
    assert_eq!(fresh.code, "agent_profile_not_found");
    fixture
        .registry
        .resume(&created.id)
        .await
        .expect("the pinned session keeps resuming from its own digest");
    let _ = fixture.registry.stop(&created.id).await;
}

#[tokio::test]
async fn doctor_reports_a_pinned_digest_the_registry_does_not_record() {
    let (fixture, package, dir, mut gate, store) = session_fixture("package-pinned-missing").await;
    let created = fixture
        .registry
        .create(new_params(&dir))
        .await
        .expect("create");
    gate.write_all(b"go\n").expect("release the exit gate");
    fixture
        .registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the session exits");
    drop(fixture);

    // A registry over the same sessions but an empty plugin root.
    let bare = Fixture::with_store("package-pinned-missing-bare", store);
    let doctor = bare.registry.package_doctor(None).await.expect("doctor");

    assert_eq!(doctor.findings.len(), 1);
    assert_eq!(
        doctor.findings[0].kind,
        PackageFindingKind::PinnedNotInstalled
    );
    assert_eq!(doctor.findings[0].digest, package.digest);
    assert!(doctor.findings[0].referenced);
}
