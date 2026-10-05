//! Behavioural tests of compatibility-gated selection over a real plugin root,
//! real archives, real agent processes and a real session registry.

// Rust guideline compliant 2026-10-05

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use package::registry::RetainedDigests;
use package::PackageDigest;
use pohunek_test_support::wait::HANG_GUARD;
use protocol::{
    PackageBindProfileParams, PackageErrorKind, PackageFindingKind, PackageInfo,
    PackageInstallStatus, PackageSelectParams, PackageSelectionBlock, PackageSelectionBlockReason,
    PackageUninstallParams,
};

use super::selection::{Declared, Integration, Integrations};
use super::test_fixture::{explicit, Fixture, Package, INERT_PROGRAM, PACKAGE, RUNTIME};
use crate::session::tests::{assigned_agent_script, params, temp_dir, temp_store_path};

/// Handler and schema of an integration that reports subagents.
const SUBAGENT: (&str, &str) = ("codex-hook-v1", "identity-subagent-v1");

/// Handler and schema of an integration that reports identity only.
const IDENTITY: (&str, &str) = ("hermes-hook-v1", "identity-v1");

/// Program of a version that is never launched.
const SECOND_PROGRAM: &str = "/bin/true";

/// Fresh fixtures one race test runs.
const RACE_ROUNDS: usize = 6;

/// A plugin root holding the selected old version, whose program launches an
/// agent script, plus the exit gate of that script.
struct Versions {
    fixture: Fixture,
    old: Package,
    dir: PathBuf,
    gate: fs::File,
    store: PathBuf,
}

/// Installs the old version, enabled and selected, with `integration`.
async fn versions(tag: &str, integration: Option<(&str, &str)>) -> Versions {
    let dir = temp_dir(tag);
    let marker = dir.join("argv.txt");
    let (script, gate) = assigned_agent_script(&dir, &marker);
    let store = temp_store_path(tag);
    let fixture = Fixture::with_store(&format!("{tag}-plugins"), store.clone());
    let old = Package::build_with(
        PACKAGE,
        "1.0.0",
        RUNTIME,
        script.to_str().expect("utf-8 script path"),
        integration,
    );
    install(&fixture, &old, true).await;
    Versions {
        fixture,
        old,
        dir,
        gate,
        store,
    }
}

/// A later version of the fixture package with `integration`.
fn update(version: &str, integration: Option<(&str, &str)>) -> Package {
    Package::build_with(PACKAGE, version, RUNTIME, SECOND_PROGRAM, integration)
}

/// Installs `package` enabled; `select` asks for it to become selected.
async fn install(
    fixture: &Fixture,
    package: &Package,
    select: bool,
) -> protocol::PackageInstallResult {
    let path = fixture.write_archive(package);
    fixture
        .registry
        .package_install(explicit(&path, package, true, select))
        .await
        .expect("install")
}

fn select(digest: &PackageDigest) -> PackageSelectParams {
    PackageSelectParams {
        digest: digest.clone(),
    }
}

fn block_by(retained: &PackageDigest) -> PackageSelectionBlock {
    PackageSelectionBlock {
        reason: PackageSelectionBlockReason::IncompatibleWithRetained,
        retained: retained.clone(),
    }
}

fn new_params(dir: &std::path::Path) -> protocol::SessionNewParams {
    protocol::SessionNewParams {
        agent: RUNTIME.to_owned(),
        cwd: Some(dir.to_path_buf()),
        ..params()
    }
}

async fn info(fixture: &Fixture, digest: &PackageDigest) -> PackageInfo {
    fixture
        .registry
        .package_inspect(digest.clone())
        .await
        .expect("inspect")
        .package
}

/// Stops and removes `id`, which releases its pin.
async fn end(fixture: &Fixture, id: &protocol::SessionId) {
    let _ = fixture.registry.stop(id).await;
    fixture
        .registry
        .remove(id)
        .await
        .expect("remove the session");
}

/// Asserts that `candidate` is installed, unselected and held back by `old`
/// in the list, the inspection and the doctor report, and that selecting it
/// is refused without changing anything.
async fn assert_held_back(fixture: &Fixture, candidate: &Package, old: &Package) {
    let listed = fixture.registry.package_list().await.expect("list");
    let entry = listed
        .packages
        .iter()
        .find(|package| package.digest == candidate.digest)
        .expect("the candidate stays installed");
    assert!(!entry.selected);
    assert_eq!(entry.selection_blocked, Some(block_by(&old.digest)));
    assert_eq!(
        info(fixture, &candidate.digest).await.selection_blocked,
        Some(block_by(&old.digest))
    );
    let selected = listed
        .packages
        .iter()
        .find(|package| package.digest == old.digest)
        .expect("the old version is installed");
    assert!(selected.selected, "the old version stays selected");
    assert_eq!(selected.selection_blocked, None);

    let doctor = fixture.registry.package_doctor(None).await.expect("doctor");
    let finding = doctor
        .findings
        .iter()
        .find(|finding| finding.kind == PackageFindingKind::SelectionBlocked)
        .expect("doctor reports the held back package");
    assert_eq!(finding.digest, candidate.digest);
    assert_eq!(finding.blocked_by.as_ref(), Some(&old.digest));
    assert_eq!(finding.fault, None);

    let before = fixture.snapshot();
    assert_eq!(
        fixture
            .registry
            .package_select(select(&candidate.digest))
            .await
            .expect_err("an incompatible package is not selected"),
        PackageErrorKind::IntegrationIncompatible
    );
    assert_eq!(
        fixture.snapshot(),
        before,
        "a refused select changes nothing"
    );
    assert_eq!(fixture.serving(RUNTIME), Some(old.digest.clone()));
}

/// Asserts that `candidate` is no longer held back, that nothing selected it
/// on its own, and that an explicit select now serves it.
async fn assert_selectable_then_select(fixture: &Fixture, candidate: &Package, old: &Package) {
    assert_eq!(
        info(fixture, &candidate.digest).await.selection_blocked,
        None
    );
    assert_eq!(
        fixture.serving(RUNTIME),
        Some(old.digest.clone()),
        "the reason clearing never selects the package"
    );
    fixture
        .registry
        .package_select(select(&candidate.digest))
        .await
        .expect("selectable once the reference is gone");
    assert_eq!(fixture.serving(RUNTIME), Some(candidate.digest.clone()));
}

#[tokio::test]
async fn an_incompatible_update_stays_installed_and_unselected_while_a_live_session_uses_the_old_version(
) {
    let v = versions("selection-live", Some(SUBAGENT)).await;
    let created = v
        .fixture
        .registry
        .create(new_params(&v.dir))
        .await
        .expect("create");

    let new = update("2.0.0", Some(IDENTITY));
    let installed = install(&v.fixture, &new, true).await;
    assert_eq!(installed.status, PackageInstallStatus::Installed);
    assert!(!installed.package.selected, "selection is withheld");
    assert_eq!(
        installed.package.selection_blocked,
        Some(block_by(&v.old.digest))
    );
    assert_eq!(
        installed.runtime.hook_schema.as_deref(),
        Some("identity-v1")
    );
    assert_eq!(
        installed.runtime.integration_handler.as_deref(),
        Some("hermes-hook-v1")
    );
    assert_held_back(&v.fixture, &new, &v.old).await;

    end(&v.fixture, &created.id).await;
    assert_selectable_then_select(&v.fixture, &new, &v.old).await;
}

#[tokio::test]
async fn a_dry_run_install_reports_the_withheld_selection() {
    let v = versions("selection-preview", Some(SUBAGENT)).await;
    let created = v
        .fixture
        .registry
        .create(new_params(&v.dir))
        .await
        .expect("create");
    let new = update("2.0.0", Some(IDENTITY));
    let path = v.fixture.write_archive(&new);
    let mut request = explicit(&path, &new, true, true);
    request.dry_run = true;
    let before = v.fixture.snapshot();

    let preview = v
        .fixture
        .registry
        .package_install(request)
        .await
        .expect("preview");

    assert_eq!(preview.status, PackageInstallStatus::Preview);
    assert!(!preview.package.selected);
    assert_eq!(
        preview.package.selection_blocked,
        Some(block_by(&v.old.digest))
    );
    assert_eq!(v.fixture.snapshot(), before, "a preview changes nothing");
    end(&v.fixture, &created.id).await;
}

#[tokio::test]
async fn an_exited_session_with_a_resume_binding_still_holds_the_update_back() {
    let mut v = versions("selection-resume", Some(SUBAGENT)).await;
    let created = v
        .fixture
        .registry
        .create(new_params(&v.dir))
        .await
        .expect("create");
    v.gate.write_all(b"go\n").expect("release the exit gate");
    v.fixture
        .registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the session exits");

    let new = update("2.0.0", Some(IDENTITY));
    install(&v.fixture, &new, true).await;
    assert_held_back(&v.fixture, &new, &v.old).await;

    v.fixture
        .registry
        .remove(&created.id)
        .await
        .expect("remove the session");
    assert_selectable_then_select(&v.fixture, &new, &v.old).await;
}

#[tokio::test]
async fn a_profile_pin_holds_the_update_back_until_the_pin_is_gone() {
    let v = versions("selection-profile", Some(SUBAGENT)).await;
    let profile = v.fixture.write_profile(
        "work",
        &format!(
            "base = \"{RUNTIME}\"\npackage = \"{PACKAGE}\"\ndigest = \"{}\"\n",
            v.old.digest
        ),
    );
    let new = update("2.0.0", Some(IDENTITY));
    install(&v.fixture, &new, true).await;
    assert_held_back(&v.fixture, &new, &v.old).await;

    fs::remove_file(&profile).expect("drop the pin");
    assert_selectable_then_select(&v.fixture, &new, &v.old).await;
}

#[tokio::test]
async fn uninstalling_the_old_version_releases_the_update_without_selecting_it() {
    let v = versions("selection-uninstall", Some(SUBAGENT)).await;
    let created = v
        .fixture
        .registry
        .create(new_params(&v.dir))
        .await
        .expect("create");
    let new = update("2.0.0", Some(IDENTITY));
    install(&v.fixture, &new, true).await;
    end(&v.fixture, &created.id).await;

    v.fixture
        .registry
        .package_uninstall(PackageUninstallParams {
            digest: v.old.digest.clone(),
            remove_modified: false,
        })
        .await
        .expect("uninstall the old version");

    assert_eq!(info(&v.fixture, &new.digest).await.selection_blocked, None);
    assert_eq!(
        v.fixture.serving(RUNTIME),
        None,
        "no fallback and no auto-select"
    );
    v.fixture
        .registry
        .package_select(select(&new.digest))
        .await
        .expect("select");
    assert_eq!(v.fixture.serving(RUNTIME), Some(new.digest));
}

#[tokio::test]
async fn a_compatible_update_selects_normally_while_the_old_version_is_in_use() {
    let v = versions("selection-compatible", Some(SUBAGENT)).await;
    let created = v
        .fixture
        .registry
        .create(new_params(&v.dir))
        .await
        .expect("create");

    let new = update("1.1.0", Some(SUBAGENT));
    let installed = install(&v.fixture, &new, true).await;
    assert!(installed.package.selected);
    assert_eq!(installed.package.selection_blocked, None);
    assert_eq!(v.fixture.serving(RUNTIME), Some(new.digest.clone()));
    let retained = v
        .fixture
        .registry
        .retained_package_digests()
        .await
        .expect("retained");
    assert!(
        retained.contains(&v.old.digest),
        "the session keeps its pin"
    );

    v.fixture
        .registry
        .package_select(select(&v.old.digest))
        .await
        .expect("selecting back is compatible too");
    end(&v.fixture, &created.id).await;
}

#[tokio::test]
async fn adding_or_dropping_an_integration_is_an_incompatible_difference() {
    for (old, new) in [(None, Some(IDENTITY)), (Some(SUBAGENT), None)] {
        let v = versions("selection-presence", old).await;
        let created = v
            .fixture
            .registry
            .create(new_params(&v.dir))
            .await
            .expect("create");
        let candidate = update("2.0.0", new);
        install(&v.fixture, &candidate, true).await;
        assert_held_back(&v.fixture, &candidate, &v.old).await;
        end(&v.fixture, &created.id).await;
        assert_selectable_then_select(&v.fixture, &candidate, &v.old).await;
    }
}

#[tokio::test]
async fn the_same_integration_in_a_package_without_references_is_never_held_back() {
    let v = versions("selection-unreferenced", Some(SUBAGENT)).await;
    let new = update("2.0.0", Some(IDENTITY));
    let installed = install(&v.fixture, &new, true).await;
    assert!(
        installed.package.selected,
        "nothing references the old version"
    );
    assert_eq!(installed.package.selection_blocked, None);
    assert_eq!(v.fixture.serving(RUNTIME), Some(new.digest));
}

#[tokio::test]
async fn a_retained_version_that_cannot_be_read_holds_the_update_back() {
    let v = versions("selection-unreadable", Some(SUBAGENT)).await;
    let created = v
        .fixture
        .registry
        .create(new_params(&v.dir))
        .await
        .expect("create");
    let new = update("2.0.0", Some(SUBAGENT));
    install(&v.fixture, &new, false).await;
    v.fixture.tamper(&v.old.digest);

    assert_eq!(
        v.fixture
            .registry
            .package_select(select(&new.digest))
            .await
            .expect_err("the retained version's integration is unknown"),
        PackageErrorKind::IntegrationIncompatible
    );
    assert_eq!(
        info(&v.fixture, &new.digest).await.selection_blocked,
        Some(block_by(&v.old.digest))
    );
    end(&v.fixture, &created.id).await;
}

#[tokio::test]
async fn a_descriptor_that_predates_the_hook_schema_resolves_through_its_handler() {
    let fixture = Fixture::new("selection-legacy");
    let legacy = legacy_package("1.0.0", SUBAGENT.0);
    let ambiguous = legacy_package("1.1.0", "acme-unknown-v1");
    let current = update("2.0.0", Some(SUBAGENT));
    let registry = fixture.host.package_store().expect("store").registry();
    for package in [&legacy, &ambiguous] {
        registry
            .install(&package::registry::InstallRequest {
                archive: &package.bytes,
                expected: &package.digest,
                identity: identity_of(package),
                source: package::registry::PackageSource::ExplicitDigest,
                enabled: true,
                select: false,
                installed_at_unix_seconds: 1,
            })
            .expect("install the pre-schema package");
    }
    let state = registry.state().expect("state");
    let context = fixture.registry.package_context().expect("context");
    let mut integrations = Integrations::default();

    let resolved = integrations_of(&mut integrations, &context, &state, &legacy.digest);
    assert!(
        matches!(&resolved, Integration::Known(Some(Declared { .. }))),
        "{resolved:?}"
    );
    assert_eq!(
        integrations_of(&mut integrations, &context, &state, &ambiguous.digest),
        Integration::Unresolvable,
        "a handler no schema is driven by is unresolvable"
    );

    // A retained pre-schema version is compatible with the integration its
    // handler resolves to and incompatible with any other.
    let retained = RetainedDigests::from_iter([legacy.digest.clone()]);
    let candidate = fixture_record(&fixture, &current);
    assert_eq!(
        super::selection::conflict(
            &context,
            &state,
            &retained,
            (&candidate.0, &candidate.1),
            &super::selection::declared_by(&candidate.2),
        ),
        None
    );
    let other = update("2.1.0", Some(IDENTITY));
    let other = fixture_record(&fixture, &other);
    assert_eq!(
        super::selection::conflict(
            &context,
            &state,
            &retained,
            (&other.0, &other.1),
            &super::selection::declared_by(&other.2),
        ),
        Some(legacy.digest)
    );
}

#[tokio::test]
async fn a_profile_cannot_pin_a_version_whose_integration_differs_from_the_selected_one() {
    let v = versions("selection-bind", Some(SUBAGENT)).await;
    let new = update("2.0.0", Some(IDENTITY));
    install(&v.fixture, &new, false).await;
    let text = format!("base = \"{RUNTIME}\"\nprogram = \"{INERT_PROGRAM}\"\n");
    let path = v.fixture.write_profile("work", &text);

    for dry_run in [true, false] {
        assert_eq!(
            v.fixture
                .registry
                .package_bind_profile(PackageBindProfileParams {
                    profile: "work".to_owned(),
                    digest: Some(new.digest.clone()),
                    dry_run,
                })
                .await
                .expect_err("the pin would make incompatible versions coexist"),
            PackageErrorKind::IntegrationIncompatible,
            "dry_run {dry_run}"
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), text);
    }
    v.fixture
        .registry
        .package_bind_profile(PackageBindProfileParams {
            profile: "work".to_owned(),
            digest: Some(v.old.digest.clone()),
            dry_run: false,
        })
        .await
        .expect("pinning the selected version is fine");
}

#[tokio::test]
async fn the_verdict_is_recomputed_from_the_registry_and_the_sessions_after_a_restart() {
    let mut v = versions("selection-restart", Some(SUBAGENT)).await;
    let created = v
        .fixture
        .registry
        .create(new_params(&v.dir))
        .await
        .expect("create");
    v.gate.write_all(b"go\n").expect("release the exit gate");
    v.fixture
        .registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the session exits");
    let new = update("2.0.0", Some(IDENTITY));
    install(&v.fixture, &new, true).await;

    let restarted = v.fixture.reopen();
    assert_held_back(&restarted, &new, &v.old).await;
    let store = crate::store::Store::new(v.store.clone());
    store
        .remove_session(&created.id.0)
        .expect("drop the durable session");
    store
        .remove_resume(&created.id.0)
        .expect("drop the resume binding");
    assert_selectable_then_select(&restarted, &new, &v.old).await;
}

#[tokio::test]
async fn a_launch_resolved_before_a_select_is_refused_instead_of_pinning_the_previous_version() {
    let v = versions("selection-stale", Some(SUBAGENT)).await;
    let resolved = v
        .fixture
        .registry
        .inner
        .profiles
        .resolve_agent(RUNTIME)
        .expect("the selected version resolves");
    let stale = resolved.definition;
    drop(
        v.fixture
            .registry
            .guard_package_launch(&stale)
            .await
            .expect("the selected version launches"),
    );

    let new = update("2.0.0", Some(IDENTITY));
    install(&v.fixture, &new, true).await;
    assert_eq!(v.fixture.serving(RUNTIME), Some(new.digest.clone()));
    let refused = v
        .fixture
        .registry
        .guard_package_launch(&stale)
        .await
        .expect_err("the version is no longer selected");
    assert_eq!(refused.code, "runtime_package_changed");

    v.fixture.write_profile(
        "work",
        &format!(
            "base = \"{RUNTIME}\"\npackage = \"{PACKAGE}\"\ndigest = \"{}\"\n",
            v.old.digest
        ),
    );
    drop(
        v.fixture
            .registry
            .guard_package_launch(&stale)
            .await
            .expect("a profile pin launches its own digest whatever is selected"),
    );
}

/// The invariant of a select racing a launch: either the select won and the
/// launch pinned the new version, or the launch pinned the old version and
/// the select was refused.
async fn race_once(tag: &str) {
    let v = versions(tag, Some(SUBAGENT)).await;
    let new = update("2.0.0", Some(IDENTITY));
    install(&v.fixture, &new, false).await;
    let launch = v.fixture.registry.create(new_params(&v.dir));
    let choose = v.fixture.registry.package_select(select(&new.digest));
    let (launch, chosen) = tokio::join!(launch, choose);

    let retained = v
        .fixture
        .registry
        .retained_package_digests()
        .await
        .expect("retained");
    match chosen {
        Ok(_) => {
            // The select ran first; nothing referenced the old version yet.
            assert!(!retained.contains(&v.old.digest), "{tag}");
            match launch {
                Ok(created) => {
                    assert!(retained.contains(&new.digest), "{tag}");
                    end(&v.fixture, &created.id).await;
                }
                Err(error) => assert_eq!(
                    error.code, "runtime_package_changed",
                    "{tag}: a launch that resolved the old version is refused"
                ),
            }
        }
        Err(kind) => {
            assert_eq!(kind, PackageErrorKind::IntegrationIncompatible, "{tag}");
            let created = launch.expect("the launch ran before the select");
            assert!(retained.contains(&v.old.digest), "{tag}");
            assert_eq!(
                v.fixture.serving(RUNTIME),
                Some(v.old.digest.clone()),
                "{tag}"
            );
            end(&v.fixture, &created.id).await;
        }
    }
}

#[tokio::test]
async fn a_select_and_a_launch_never_leave_incompatible_versions_in_use_together() {
    for round in 0..RACE_ROUNDS {
        race_once(&format!("selection-race-{round}")).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_select_and_a_launch_racing_on_worker_threads_keep_the_invariant() {
    for round in 0..RACE_ROUNDS {
        race_once(&format!("selection-race-threads-{round}")).await;
    }
}

#[tokio::test]
async fn the_integration_update_path_sees_the_schemas_of_retained_versions() {
    let v = versions("selection-retention", Some(SUBAGENT)).await;
    let created = v
        .fixture
        .registry
        .create(new_params(&v.dir))
        .await
        .expect("create");

    let seen = |registry: &crate::session::SessionRegistry| {
        let registry = registry.clone();
        async move {
            registry
                .integration_install(|retained| {
                    Ok((
                        retained
                            .for_handler(SUBAGENT.0)
                            .map(|schema| schema.id)
                            .collect::<Vec<_>>(),
                        retained.for_handler(IDENTITY.0).count(),
                    ))
                })
                .await
                .expect("retention")
        }
    };
    let (schemas, other) = seen(&v.fixture.registry).await;
    assert_eq!(schemas, [SUBAGENT.1]);
    assert_eq!(other, 0);

    end(&v.fixture, &created.id).await;
    let (schemas, _) = seen(&v.fixture.registry).await;
    assert!(schemas.is_empty());
}

#[tokio::test]
async fn an_integration_install_keeps_the_lifecycle_guard_after_its_caller_goes_away() {
    let v = versions("selection-install-abort", Some(SUBAGENT)).await;
    let new = update("1.1.0", Some(SUBAGENT));
    install(&v.fixture, &new, false).await;
    let registry = v.fixture.registry.clone();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let caller = tokio::spawn({
        let registry = registry.clone();
        async move {
            registry
                .integration_install(move |_retained| {
                    entered_tx.send(()).expect("announce the activation");
                    release_rx.recv().expect("wait for the release");
                    done_tx.send(()).expect("announce completion");
                    Ok(())
                })
                .await
        }
    });
    tokio::task::spawn_blocking(move || entered_rx.recv().expect("the install started"))
        .await
        .expect("join");

    caller.abort();
    assert!(caller.await.expect_err("aborted").is_cancelled());
    assert!(
        registry.inner.package_lifecycle.try_write().is_err(),
        "the install task still owns the shared guard"
    );

    let selecting = tokio::spawn({
        let registry = registry.clone();
        let digest = new.digest.clone();
        async move { registry.package_select(select(&digest)).await }
    });
    let uninstalling = tokio::spawn({
        let registry = registry.clone();
        let digest = new.digest.clone();
        async move {
            registry
                .package_uninstall(PackageUninstallParams {
                    digest,
                    remove_modified: false,
                })
                .await
        }
    });
    assert!(
        done_rx.try_recv().is_err(),
        "nothing completed before the release"
    );
    release_tx.send(()).expect("release the install");
    tokio::task::spawn_blocking(move || done_rx.recv().expect("the install finished"))
        .await
        .expect("join");

    let chosen = selecting.await.expect("join");
    let removed = uninstalling.await.expect("join");
    assert!(
        chosen.is_ok() || removed.is_ok(),
        "the exclusive changes ran after the install: {chosen:?} {removed:?}"
    );
    let _free = registry
        .inner
        .package_lifecycle
        .try_write()
        .expect("the guard is released with the task");
}

// ----- fixtures of the pre-schema test -----------------------------------

/// A package whose descriptor names only `handler`, as one written before
/// `[integration] hook_schema` existed.
fn legacy_package(version: &str, handler: &str) -> Package {
    use crate::agent::host::fixture::{pi_shaped_document, PI_SHAPED_NO_CHECK};
    let document = pi_shaped_document(std::path::Path::new(INERT_PROGRAM), PI_SHAPED_NO_CHECK)
        .replace("version = \"1.0.0\"", &format!("version = \"{version}\""))
        .replace(
            "detect_manifest = \"any\"",
            "detect_manifest = \"detect.toml\"",
        );
    Package::with_descriptor(&format!(
        "{document}\n[integration]\nhandler = \"{handler}\"\n"
    ))
}

fn identity_of(package: &Package) -> protocol::PackageIdentity {
    // Every fixture package of this module is the fixture package at a
    // version; the version is read back from the archive's descriptor.
    let archive = package::read_archive(&package.bytes, &package::Limits::DEFAULT)
        .expect("the fixture archive reads");
    let descriptor = archive
        .entries()
        .iter()
        .find(|entry| entry.path == "runtime.toml")
        .expect("descriptor");
    let text = std::str::from_utf8(&descriptor.contents).expect("utf-8");
    let version = text
        .lines()
        .find_map(|line| line.strip_prefix("version = "))
        .expect("version")
        .trim_matches('"');
    protocol::PackageIdentity {
        id: protocol::PackageId::parse(PACKAGE).expect("package id"),
        version: protocol::PackageVersion::parse(version).expect("version"),
    }
}

fn integrations_of(
    integrations: &mut Integrations,
    context: &super::Context,
    state: &package::registry::RegistryState,
    digest: &PackageDigest,
) -> Integration {
    let record = state.package(digest).expect("installed");
    integrations.of(context, record)
}

/// The digest, package id and definition of `package`, parsed from its
/// archive, for a candidate that is not installed.
fn fixture_record(
    _fixture: &Fixture,
    package: &Package,
) -> (
    PackageDigest,
    protocol::PackageId,
    crate::agent::host::RuntimeDefinition,
) {
    let archive = package::read_archive(&package.bytes, &package::Limits::DEFAULT)
        .expect("the fixture archive reads");
    let definition =
        crate::agent::host::definition_from_archive(archive.entries(), &package.digest)
            .expect("definition");
    (
        package.digest.clone(),
        protocol::PackageId::parse(PACKAGE).expect("package id"),
        definition,
    )
}
