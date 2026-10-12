//! Official runtime aliases: a catalog-authorized package takes `codex`,
//! `claude` or `hermes` over from whatever serves it without the package (the
//! built-in runtime, or nothing), nobody else does, and what a session
//! launched from a built-in runtime may do afterwards.
//!
//! The fixture daemon serves Hermes as a built-in runtime, so the scenarios
//! that take a built-in over inside one daemon use `hermes`. It compiles no
//! Codex in, and whether it serves Claude as a built-in depends on the binary,
//! so the `claude` cases assert only what holds either way; a Codex or Claude
//! session launched from a built-in runtime is written by a daemon that
//! compiles them in ([`compiled_in_host`]), as a previous release did.
//!
//! The catalog is signed with a test key derived from a fixed label.

// Rust guideline compliant 2026-10-12

use std::fs;
use std::path::{Path, PathBuf};

use package::PackageDigest;
use pohunek_service_config::preflight::{Verdict, CODE_ADOPTABLE, CODE_RUNTIME_SOURCE_NOT_SERVED};
use protocol::{
    PackageBindProfileParams, PackageBindStatus, PackageErrorKind, PackageInstallStatus,
    PackageOrigin, PackageSetEnabledParams, PackageUninstallParams, RuntimeId,
};

use super::catalog_tests::{anchor, catalog, catalog_install, entry, signed_by, signing_key};
use super::test_fixture::{explicit, Fixture, Package, INERT_PROGRAM, RUNTIME};
use crate::agent::host::fixture::compiled_in_host;
use crate::agent::host::ServedBy;
use crate::procwatch::HostInspector;
use crate::session::procwatch::OwnRuntime;
use crate::session::tests::{
    durable_recovery, hermetic_shell, params, temp_dir, temp_store_path,
    write_supported_hermes_executable,
};
use crate::session::upgrade_preflight::{self, PreflightInputs};
use crate::session::{SessionRegistry, SessionRegistryConfig};

/// Package id of the official packages these tests install.
const OFFICIAL_PACKAGE: &str = "pohunek.runtime.official";

/// The official aliases a catalog may authorize.
const ALIASES: [&str; 3] = [RuntimeId::CODEX, RuntimeId::CLAUDE, RuntimeId::HERMES];

/// Profile whose base is the alias under test.
const PROFILE: &str = "official";

fn official(alias: &str) -> Package {
    Package::build(OFFICIAL_PACKAGE, "1.0.0", alias, INERT_PROGRAM)
}

fn runtime_id(alias: &str) -> RuntimeId {
    RuntimeId::parse(alias).expect("runtime id")
}

fn uninstall(digest: &PackageDigest) -> PackageUninstallParams {
    PackageUninstallParams {
        digest: digest.clone(),
        remove_modified: false,
    }
}

/// Installs `package` for `alias` through a catalog signed by the test root.
async fn install_official(
    fixture: &Fixture,
    package: &Package,
    alias: &str,
    sequence: u64,
) -> protocol::PackageInstallResult {
    let key = signing_key("root");
    let document = signed_by(
        &key,
        catalog(
            sequence,
            vec![entry(package, OFFICIAL_PACKAGE, alias, "1.0.0")],
        ),
    );
    fixture
        .registry
        .package_install(catalog_install(fixture, package, &document, false))
        .await
        .expect("official install")
}

fn served_by(fixture: &Fixture, alias: &str) -> Option<ServedBy> {
    fixture.host.served_by(&runtime_id(alias))
}

fn is_builtin(fixture: &Fixture, alias: &str) -> bool {
    served_by(fixture, alias) == Some(ServedBy::Builtin)
}

/// What serves each official alias in `fixture` now.
fn served_aliases(fixture: &Fixture) -> Vec<(&'static str, Option<ServedBy>)> {
    ALIASES
        .into_iter()
        .map(|alias| (alias, served_by(fixture, alias)))
        .collect()
}

/// Writes the [`PROFILE`] host profile over the built-in Hermes runtime,
/// launching a program that answers the Hermes version probe and then
/// sleeps.
fn write_hermes_profile(fixture: &Fixture) {
    let program = fixture.dir.join("hermes");
    write_supported_hermes_executable(&program, "sleep 30\n");
    fixture.write_profile(
        PROFILE,
        &format!("base = \"hermes\"\nprogram = \"{}\"\n", program.display()),
    );
}

/// Asserts the refusal of a built-in session `alias` an official package
/// serves now: the typed code, and a recovery hint that names the one
/// migration (a new session the package serves) and no way back or rebind.
fn assert_migration_refusal(refused: &protocol::ProtocolError, alias: &str) {
    assert_eq!(refused.code, "runtime_served_by_package", "{refused:?}");
    let hint = refused.recover.as_deref().expect("a recovery hint");
    assert!(
        hint.contains(&format!("`pohunek session new --agent {alias}`")),
        "{hint}"
    );
    assert!(hint.contains("cannot be rebound"), "{hint}");
    for stale in [
        "disable",
        "uninstall",
        "resume the session from the built-in",
    ] {
        assert!(!hint.contains(stale), "{hint}");
    }
}

/// A same-directory fork of session `id`.
fn fork_params(id: &protocol::SessionId) -> protocol::SessionForkParams {
    protocol::SessionForkParams {
        session_id: id.clone(),
        name: None,
        cwd_mode: protocol::ForkCwdMode::Same,
        cols: 100,
        rows: 30,
        accept_profile_change: false,
    }
}

/// The preflight inputs of `fixture`'s installed state.
fn preflight_inputs(fixture: &Fixture, store_path: &std::path::Path) -> PreflightInputs {
    PreflightInputs {
        store_path: store_path.to_path_buf(),
        worker_state_root: fixture
            .registry
            .inner
            .config
            .worker_state_root
            .clone()
            .expect("the fixture's worker state root"),
        plugins_dir: fixture.plugins.clone(),
    }
}

/// The verdict, code and detail the preflight gives session `id`.
fn preflight_verdict(
    inputs: &PreflightInputs,
    id: &protocol::SessionId,
) -> (Verdict, String, String) {
    let report = upgrade_preflight::run(inputs, &HostInspector::new());
    let verdict = report
        .sessions
        .iter()
        .find(|verdict| verdict.session_id == id.0)
        .unwrap_or_else(|| panic!("no verdict for {}: {:?}", id.0, report.sessions));
    (
        verdict.verdict,
        verdict.code.clone(),
        verdict.detail.clone(),
    )
}

#[tokio::test]
async fn an_official_package_claims_a_reserved_alias_and_is_served() {
    let key = signing_key("root");
    for alias in ALIASES {
        let fixture =
            Fixture::with_anchor(&format!("alias-claim-{alias}"), anchor(&[&key], Vec::new()));
        let package = official(alias);
        let before = served_aliases(&fixture);
        assert!(
            is_builtin(&fixture, RuntimeId::HERMES),
            "Hermes starts built-in"
        );
        assert_eq!(fixture.serving(alias), None, "no package serves {alias}");

        let installed = install_official(&fixture, &package, alias, 5).await;

        assert_eq!(installed.status, PackageInstallStatus::Installed, "{alias}");
        assert_eq!(installed.package.origin, PackageOrigin::Official);
        assert_eq!(installed.package.fault, None, "{alias}");
        assert_eq!(fixture.serving(alias), Some(package.digest.clone()));
        let package_id = installed.package.package.id.clone();
        assert_eq!(
            served_by(&fixture, alias),
            Some(ServedBy::Package(package_id))
        );
        let inspected = fixture
            .registry
            .package_inspect(package.digest.clone())
            .await
            .expect("inspect");
        assert_eq!(inspected.package.fault, None, "{alias} is not faulted");
        assert!(inspected.package.selected && inspected.package.enabled);
        let inventory = fixture.host.registry().inventory();
        let entry = inventory
            .iter()
            .find(|entry| entry.runtime_id == runtime_id(alias))
            .expect("inventory entry");
        assert!(
            matches!(
                entry.binding.provenance,
                protocol::BindingProvenance::Package { .. }
            ),
            "{alias} inventory names the package"
        );
        for (other, served) in before.into_iter().filter(|(other, _)| *other != alias) {
            assert_eq!(served_by(&fixture, other), served, "{other} is untouched");
        }
        assert!(
            is_builtin(&fixture, RuntimeId::SHELL),
            "the shell is untouched"
        );
    }
}

#[tokio::test]
async fn a_local_package_never_takes_an_alias_an_official_package_serves() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("alias-local", anchor(&[&key], Vec::new()));
    let package = official(RuntimeId::CODEX);
    install_official(&fixture, &package, RuntimeId::CODEX, 5).await;
    let before = fixture.snapshot();

    for alias in ALIASES {
        let rival = Package::build("acme.runtime.rival", "1.0.0", alias, INERT_PROGRAM);
        let path = fixture.write_archive(&rival);
        assert_eq!(
            fixture
                .registry
                .package_install(explicit(&path, &rival, true, true))
                .await
                .expect_err(alias),
            PackageErrorKind::RuntimeNotClaimable,
            "{alias}"
        );
    }

    assert_eq!(fixture.snapshot(), before);
    assert_eq!(fixture.serving(RuntimeId::CODEX), Some(package.digest));
}

#[tokio::test]
async fn a_catalog_that_does_not_authorize_the_exact_alias_package_and_digest_refuses_it() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("alias-unauthorized", anchor(&[&key], Vec::new()));
    let package = official(RuntimeId::CODEX);
    let other = Package::build("acme.runtime.other", "1.0.0", RUNTIME, INERT_PROGRAM);

    // Each entry authorizes something other than this archive claiming codex.
    let wrong_runtime = entry(&package, OFFICIAL_PACKAGE, RuntimeId::CLAUDE, "1.0.0");
    let wrong_package = entry(&package, "acme.runtime.impostor", RuntimeId::CODEX, "1.0.0");
    let wrong_digest = entry(&other, OFFICIAL_PACKAGE, RuntimeId::CODEX, "1.0.0");
    let ordinary_id = entry(&package, OFFICIAL_PACKAGE, RUNTIME, "1.0.0");
    let wrong_version = entry(&package, OFFICIAL_PACKAGE, RuntimeId::CODEX, "2.0.0");
    let before = served_by(&fixture, RuntimeId::CODEX);
    assert_eq!(fixture.serving(RuntimeId::CODEX), None);
    let cases = [
        ("runtime", wrong_runtime),
        ("package", wrong_package),
        ("digest", wrong_digest),
        ("ordinary id", ordinary_id),
        ("version", wrong_version),
    ];
    for (name, entry) in cases {
        let document = signed_by(&key, catalog(5, vec![entry]));
        let refused = fixture
            .registry
            .package_install(catalog_install(&fixture, &package, &document, false))
            .await
            .expect_err(name);
        assert_eq!(refused, PackageErrorKind::Untrusted, "{name}");
        assert!(!fixture.root(&package.digest).exists(), "{name}");
        assert_eq!(served_by(&fixture, RuntimeId::CODEX), before, "{name}");
    }
}

#[tokio::test]
async fn the_shell_is_never_claimable_even_through_a_catalog() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("alias-shell", anchor(&[&key], Vec::new()));
    let package = official(RuntimeId::SHELL);
    let document = signed_by(
        &key,
        catalog(
            5,
            vec![entry(&package, OFFICIAL_PACKAGE, RuntimeId::SHELL, "1.0.0")],
        ),
    );

    let refused = fixture
        .registry
        .package_install(catalog_install(&fixture, &package, &document, false))
        .await
        .expect_err("the shell");

    assert_eq!(refused, PackageErrorKind::RuntimeNotClaimable);
    assert!(is_builtin(&fixture, RuntimeId::SHELL));
}

#[tokio::test]
async fn disabling_or_uninstalling_the_official_package_returns_the_alias_to_the_built_in() {
    let key = signing_key("root");
    for alias in ALIASES {
        let fixture = Fixture::with_anchor(
            &format!("alias-return-{alias}"),
            anchor(&[&key], Vec::new()),
        );
        let package = official(alias);
        // The built-in runtime when the daemon serves one, otherwise nothing.
        let without_package = served_by(&fixture, alias);
        if alias == RuntimeId::HERMES {
            assert_eq!(without_package, Some(ServedBy::Builtin));
        }
        install_official(&fixture, &package, alias, 5).await;
        assert_eq!(fixture.serving(alias), Some(package.digest.clone()));

        fixture
            .registry
            .package_set_enabled(PackageSetEnabledParams {
                digest: package.digest.clone(),
                enabled: false,
            })
            .await
            .expect("disable");
        assert_eq!(
            served_by(&fixture, alias),
            without_package,
            "{alias} disabled"
        );
        assert_eq!(fixture.serving(alias), None);

        fixture
            .registry
            .package_set_enabled(PackageSetEnabledParams {
                digest: package.digest.clone(),
                enabled: true,
            })
            .await
            .expect("enable again");
        assert_eq!(
            fixture.serving(alias),
            Some(package.digest.clone()),
            "{alias} takes over again"
        );

        fixture
            .registry
            .package_uninstall(uninstall(&package.digest))
            .await
            .expect("uninstall");
        assert_eq!(
            served_by(&fixture, alias),
            without_package,
            "{alias} uninstalled"
        );
        assert_eq!(fixture.serving(alias), None);
    }
}

#[tokio::test]
async fn a_restart_serves_the_official_package_in_place_of_the_built_in() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("alias-restart", anchor(&[&key], Vec::new()));
    let package = official(RuntimeId::CLAUDE);
    install_official(&fixture, &package, RuntimeId::CLAUDE, 5).await;

    let restarted = fixture.reopen();

    assert_eq!(restarted.serving(RuntimeId::CLAUDE), Some(package.digest));
}

#[tokio::test]
async fn a_profile_on_an_alias_base_is_pinnable_only_once_a_package_serves_it() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("alias-profile", anchor(&[&key], Vec::new()));
    let package = official(RuntimeId::HERMES);
    let text = format!("base = \"{}\"\n", RuntimeId::HERMES);
    let path = fixture.write_profile(PROFILE, &text);
    let bind = |dry_run| PackageBindProfileParams {
        profile: PROFILE.to_owned(),
        digest: None,
        dry_run,
    };

    assert_eq!(
        fixture
            .registry
            .package_bind_profile(bind(false))
            .await
            .expect_err("the built-in serves the base"),
        PackageErrorKind::ProfileBaseBuiltin
    );
    install_official(&fixture, &package, RuntimeId::HERMES, 5).await;

    let preview = fixture
        .registry
        .package_bind_profile(bind(true))
        .await
        .expect("preview");
    assert_eq!(preview.status, PackageBindStatus::Preview);
    assert_eq!(fs::read_to_string(&path).expect("read"), text);

    let bound = fixture
        .registry
        .package_bind_profile(bind(false))
        .await
        .expect("bind");
    assert_eq!(bound.status, PackageBindStatus::Bound);
    assert_eq!(bound.package.digest, package.digest);
    let rewritten = fs::read_to_string(&path).expect("read");
    assert!(rewritten.contains(&format!("package = \"{OFFICIAL_PACKAGE}\"")));
    assert!(rewritten.contains(package.digest.as_str()));
    assert_eq!(
        fixture
            .registry
            .package_uninstall(uninstall(&package.digest))
            .await
            .expect_err("the pin retains the package"),
        PackageErrorKind::Referenced
    );
}

#[tokio::test]
async fn a_built_in_launched_session_fails_closed_after_the_takeover_and_resumes_after_the_return()
{
    let key = signing_key("root");
    let dir = temp_dir("alias-session");
    let store_path = temp_store_path("alias-session");
    let fixture = Fixture::with_anchor_and_store(
        "alias-session-plugins",
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    write_hermes_profile(&fixture);
    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir.clone()),
            ..params()
        })
        .await
        .expect("a session of the built-in runtime");
    let pinned = durable_recovery(&store_path, &created.id).launch_binding;
    assert!(matches!(
        pinned.binding().map(|binding| &binding.provenance),
        Some(protocol::BindingProvenance::Builtin { .. })
    ));
    let _ = fixture.registry.stop(&created.id).await;

    let package = official(RuntimeId::HERMES);
    install_official(&fixture, &package, RuntimeId::HERMES, 5).await;
    let before = fs::read(&store_path).expect("read the store");

    let refused = fixture
        .registry
        .resume(&created.id)
        .await
        .expect_err("the package serves the runtime now");
    assert_migration_refusal(&refused, RuntimeId::HERMES);
    assert_eq!(
        fs::read(&store_path).expect("read the store"),
        before,
        "a refused resume leaves the stored binding untouched"
    );
    fixture
        .registry
        .inspect(&created.id)
        .await
        .expect("the session stays listed");
    assert_eq!(
        durable_recovery(&store_path, &created.id).launch_binding,
        pinned,
        "the pin keeps its built-in provenance"
    );

    fixture
        .registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall");
    let after_return = fixture.registry.resume(&created.id).await;
    assert_ne!(
        after_return.as_ref().err().map(|error| error.code.as_str()),
        Some("runtime_served_by_package"),
        "the built-in serves the pin again"
    );
    let _ = fixture.registry.stop(&created.id).await;
}

/// Launches a session of the [`PROFILE`] host profile on a daemon that
/// compiles Codex and Claude in ([`compiled_in_host`]), over the agents
/// directory, host state and session store `store_path` of `fixture`, and
/// stops it: a session a previous release launched from its built-in runtime.
async fn launch_on_a_compiled_in_daemon(
    fixture: &Fixture,
    store_path: &Path,
    cwd: PathBuf,
) -> protocol::SessionInfo {
    let previous = compiled_in_daemon(fixture, store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(cwd),
            ..params()
        })
        .await
        .expect("a session of the built-in runtime");
    previous.stop(&created.id).await.expect("stop the session");
    created
}

/// A daemon that compiles Codex and Claude in ([`compiled_in_host`]), over the
/// agents directory, host state and session store `store_path` of `fixture`,
/// as a previous release ran.
fn compiled_in_daemon(fixture: &Fixture, store_path: &Path) -> SessionRegistry {
    SessionRegistry::new_with_runtimes(
        SessionRegistryConfig {
            shell_command: hermetic_shell(),
            stop_grace: std::time::Duration::from_millis(50),
            store_path: Some(store_path.to_path_buf()),
            agents_dir: Some(fixture.agents.clone()),
            host_state_dir: Some(fixture.dir.join("state")),
            ..SessionRegistryConfig::default()
        },
        compiled_in_host(),
    )
}

#[tokio::test]
async fn a_built_in_claude_session_is_neither_resumed_nor_forked_after_the_takeover() {
    let key = signing_key("root");
    let dir = temp_dir("alias-claude-fork");
    let store_path = temp_store_path("alias-claude-fork");
    let fixture = Fixture::with_anchor_and_store(
        "alias-claude-fork-plugins",
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    );
    let created = launch_on_a_compiled_in_daemon(&fixture, &store_path, dir).await;
    assert!(
        created.capabilities.fork,
        "the built-in Claude runtime forks"
    );
    assert!(matches!(
        durable_recovery(&store_path, &created.id)
            .launch_binding
            .binding()
            .map(|binding| &binding.provenance),
        Some(protocol::BindingProvenance::Builtin { .. })
    ));
    install_official(&fixture, &official(RuntimeId::CLAUDE), RuntimeId::CLAUDE, 5).await;
    let restarted = fixture.reopen();
    Box::pin(restarted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let before = fs::read(&store_path).expect("read the store");

    let resumed = restarted
        .registry
        .resume(&created.id)
        .await
        .expect_err("the package serves the runtime now");
    assert_migration_refusal(&resumed, RuntimeId::CLAUDE);
    let forked = restarted
        .registry
        .fork(fork_params(&created.id))
        .await
        .expect_err("a fork never launches the package for a built-in session");
    assert_migration_refusal(&forked, RuntimeId::CLAUDE);
    assert_eq!(
        fs::read(&store_path).expect("read the store"),
        before,
        "the refusals leave the stored session untouched"
    );

    restarted
        .registry
        .remove(&created.id)
        .await
        .expect("a refused session is still removable");
}

/// A Codex session a previous release launched from its built-in runtime, on
/// a daemon that serves no `codex`: resume is refused as a removed built-in
/// whose recovery is the official package and a new session, and once the
/// package is installed, as a session the package does not serve. Neither
/// refusal touches the stored session.
#[tokio::test]
async fn a_built_in_codex_session_is_refused_as_removed_until_the_package_serves_codex() {
    let key = signing_key("root");
    let dir = temp_dir("alias-codex-removed");
    let store_path = temp_store_path("alias-codex-removed");
    let fixture = Fixture::with_anchor_and_store(
        "alias-codex-removed-plugins",
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    );
    let created = launch_on_a_compiled_in_daemon(&fixture, &store_path, dir).await;
    assert!(matches!(
        durable_recovery(&store_path, &created.id)
            .launch_binding
            .binding()
            .map(|binding| &binding.provenance),
        Some(protocol::BindingProvenance::Builtin { .. })
    ));
    let restarted = fixture.reopen();
    Box::pin(restarted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    assert_eq!(
        served_by(&restarted, RuntimeId::CODEX),
        None,
        "the daemon ships no Codex"
    );
    let before = fs::read(&store_path).expect("read the store");

    let removed = restarted
        .registry
        .resume(&created.id)
        .await
        .expect_err("nothing serves the built-in the session was launched from");
    assert_eq!(removed.code, "runtime_builtin_removed", "{removed:?}");
    let hint = removed.recover.as_deref().expect("a recovery hint");
    assert!(
        hint.contains(
            "`pohunek plugin install runtime/packages/codex.tar.zst --catalog runtime/runtime-catalog.json`"
        ),
        "{hint}"
    );
    assert!(
        hint.contains("`pohunek session new --agent codex`"),
        "{hint}"
    );
    assert!(hint.contains("cannot be rebound"), "{hint}");
    assert_eq!(
        fs::read(&store_path).expect("read the store"),
        before,
        "the refusal leaves the stored session untouched"
    );

    drop(restarted);

    install_official(&fixture, &official(RuntimeId::CODEX), RuntimeId::CODEX, 5).await;
    let served = fixture.reopen();
    Box::pin(served.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let refused = served
        .registry
        .resume(&created.id)
        .await
        .expect_err("the package serves the runtime now");
    assert_migration_refusal(&refused, RuntimeId::CODEX);

    served
        .registry
        .remove(&created.id)
        .await
        .expect("a refused session is still removable");
}

/// A live Codex session a previous release launched from its built-in runtime,
/// adopted by a daemon that serves no `codex`: input is refused as a removed
/// built-in, and once the official package serves `codex` the live session
/// takes input again.
#[tokio::test]
async fn an_adopted_built_in_codex_session_takes_input_only_once_the_package_serves_codex() {
    let key = signing_key("root");
    let dir = temp_dir("alias-codex-live");
    let store_path = temp_store_path("alias-codex-live");
    let fixture = Fixture::with_anchor_and_store(
        "alias-codex-live-plugins",
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    adopted
        .registry
        .reconcile_workers()
        .await
        .expect("startup reconciliation");
    let live = adopted
        .registry
        .inspect(&created.id)
        .await
        .expect("the live session survives the restart");
    assert_eq!(
        live.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Live)
    );
    let input = || protocol::SessionInputParams {
        session_id: created.id.clone(),
        text: "continue".to_owned(),
        wait: None,
    };

    let refused = adopted
        .registry
        .input(input())
        .await
        .expect_err("nothing serves the runtime the session was launched from");
    assert_eq!(refused.code, "runtime_builtin_removed", "{refused:?}");

    install_official(&fixture, &official(RuntimeId::CODEX), RuntimeId::CODEX, 5).await;
    adopted.host.reload().expect("load the installed package");
    adopted
        .registry
        .input(input())
        .await
        .expect("the package serves codex, so the live session takes input");

    adopted
        .registry
        .stop(&created.id)
        .await
        .expect("stop the adopted session");
    adopted
        .registry
        .remove(&created.id)
        .await
        .expect("remove the adopted session");
}

/// A live session of the built-in Hermes runtime, launched through the
/// [`PROFILE`] host profile, before any package serves `hermes`.
struct LaunchedHermes {
    fixture: Fixture,
    store_path: std::path::PathBuf,
    created: protocol::SessionInfo,
    /// The input rules the session was launched with.
    input_rules: crate::agent::InputRules,
}

async fn launch_built_in_hermes(tag: &str) -> LaunchedHermes {
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    write_hermes_profile(&fixture);
    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in runtime");
    let input_rules = fixture
        .registry
        .inner
        .sessions
        .lock()
        .await
        .get(&created.id)
        .expect("the live session")
        .input_rules;
    LaunchedHermes {
        fixture,
        store_path,
        created,
        input_rules,
    }
}

/// Installs the official Hermes package, whose detection differs from the
/// built-in's, so it serves `hermes` in place of the built-in.
async fn take_hermes_over(fixture: &Fixture) -> Package {
    let package = Package::build_with_detect(
        OFFICIAL_PACKAGE,
        "1.0.0",
        RuntimeId::HERMES,
        INERT_PROGRAM,
        OFFICIAL_MANIFEST,
    );
    install_official(fixture, &package, RuntimeId::HERMES, 5).await;
    package
}

/// Starts a second daemon over `launched`'s state and checks that it adopts
/// the live worker.
async fn adopt(launched: &LaunchedHermes) -> Fixture {
    let restarted = launched.fixture.reopen_with_workers().await;
    restarted
        .registry
        .reconcile_workers()
        .await
        .expect("startup reconciliation");
    let adopted = restarted
        .registry
        .inspect(&launched.created.id)
        .await
        .expect("the live session survives the restart");
    assert_eq!(
        adopted.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Live)
    );
    restarted
}

#[tokio::test]
async fn a_built_in_session_adopted_after_a_takeover_is_observed_only_through_its_pin() {
    let launched = launch_built_in_hermes("alias-adopted").await;
    let created = &launched.created;
    assert!(created.capabilities.resume, "the built-in Hermes resumes");
    let inputs = preflight_inputs(&launched.fixture, &launched.store_path);
    let (verdict, code, _detail) = preflight_verdict(&inputs, &created.id);
    assert_eq!(
        (verdict, code.as_str()),
        (Verdict::Adoptable, CODE_ADOPTABLE),
        "the built-in serves the pin before the takeover"
    );

    take_hermes_over(&launched.fixture).await;

    // The upgrade preflight reports the lost recovery with its migration.
    let (verdict, code, detail) = preflight_verdict(&inputs, &created.id);
    assert_eq!(
        (verdict, code.as_str()),
        (Verdict::WouldLoseRecovery, CODE_RUNTIME_SOURCE_NOT_SERVED),
        "{detail}"
    );
    assert!(detail.contains("runtime_served_by_package"), "{detail}");
    assert!(
        detail.contains("`pohunek session new --agent hermes`"),
        "{detail}"
    );

    let restarted = adopt(&launched).await;
    let launched_rules = launched.input_rules;
    assert_observed_only_through_the_pin(&restarted, &created.id, launched_rules).await;
    assert_hooks_not_attributed_through_the_package(&restarted, &created.id).await;

    // Resume and fork are refused; Hermes declares no fork at all.
    let resumed = restarted
        .registry
        .resume(&created.id)
        .await
        .expect_err("the built-in pin is not served");
    assert_migration_refusal(&resumed, RuntimeId::HERMES);
    let forked = restarted
        .registry
        .fork(fork_params(&created.id))
        .await
        .expect_err("Hermes does not fork");
    assert_eq!(forked.code, "agent_fork_unsupported");

    // Stop and remove release what the session owns.
    restarted
        .registry
        .stop(&created.id)
        .await
        .expect("stop the adopted session");
    restarted
        .registry
        .remove(&created.id)
        .await
        .expect("remove the adopted session");
}

/// Asserts that the adopted session `id` of `fixture` is observed with the
/// generic manifest and its launch-time input framing, never through the
/// package that serves `hermes` now.
async fn assert_observed_only_through_the_pin(
    fixture: &Fixture,
    id: &protocol::SessionId,
    launched_rules: crate::agent::InputRules,
) {
    let own = {
        let sessions = fixture.registry.inner.sessions.lock().await;
        let entry = sessions.get(id).expect("the adopted session");
        assert!(
            entry.pinned.is_none(),
            "no definition stands in for the built-in pin"
        );
        assert_eq!(
            format!("{:?}", entry.default_detector_config.manifest),
            format!("{:?}", Some(crate::detect::generic_shell_manifest())),
        );
        assert_eq!(entry.input_rules, launched_rules);
        OwnRuntime::of(entry)
    };
    let recognized = observe(
        fixture,
        id,
        fact(4002, "official-hermes"),
        std::time::Instant::now(),
        Some(&own),
    );
    assert!(
        recognized.is_empty(),
        "a process the package recognizes is not attributed to the session"
    );
}

/// An identity report of `agent` for session `id` carrying a native id.
fn identity_report(id: &protocol::SessionId, agent: &str) -> protocol::SessionReportAgentParams {
    protocol::SessionReportAgentParams {
        session_id: id.clone(),
        source: "pohunek:hermes".to_owned(),
        agent: agent.to_owned(),
        activity: None,
        seq: None,
        pid: None,
        agent_session_id: Some("native-after-takeover".to_owned()),
        agent_session_path: None,
    }
}

/// Asserts that hook callbacks naming the own runtime of the adopted session
/// `id` are not attributed through the package definition, while hook
/// admission follows the schema its worker journaled.
async fn assert_hooks_not_attributed_through_the_package(
    fixture: &Fixture,
    id: &protocol::SessionId,
) {
    for agent in [RuntimeId::HERMES, PROFILE] {
        let reported = fixture
            .registry
            .report_agent(identity_report(id, agent))
            .await;
        assert!(!reported.recorded, "{agent}: the report is not recorded");
    }
    let released = fixture
        .registry
        .release_agent(protocol::SessionReleaseAgentParams {
            session_id: id.clone(),
            source: "pohunek:hermes".to_owned(),
            agent: RuntimeId::HERMES.to_owned(),
            seq: None,
        })
        .await;
    assert!(!released.released, "the release is not applied");
    // The public socket admits hook reports by the schema the live worker
    // journaled, as the worker does on its own socket.
    fixture
        .registry
        .admit_session_hook(
            id,
            pohunek_worker_protocol::HookAction::Notification,
            RuntimeId::HERMES,
        )
        .await
        .expect("the journaled schema admits a Hermes notification");
    let inspected = fixture.registry.inspect(id).await.expect("inspect");
    assert_ne!(
        inspected.native_session_id.as_deref(),
        Some("native-after-takeover")
    );
    assert_eq!(inspected.active_agent_base, None);
}

#[tokio::test]
async fn an_adopted_built_in_session_stays_unattributed_after_the_built_in_returns() {
    let launched = launch_built_in_hermes("alias-returned").await;
    let id = launched.created.id.clone();
    let package = take_hermes_over(&launched.fixture).await;
    let restarted = adopt(&launched).await;

    // The built-in serves `hermes` again, but the session was adopted without
    // a definition of its pin; that holds until a restart adopts it again.
    restarted
        .registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall the official package");
    assert!(is_builtin(&restarted, RuntimeId::HERMES));

    let own = {
        let sessions = restarted.registry.inner.sessions.lock().await;
        OwnRuntime::of(sessions.get(&id).expect("the adopted session"))
    };
    let now = std::time::Instant::now();
    assert!(
        observe(&restarted, &id, fact(4001, "hermes"), now, Some(&own)).is_empty(),
        "process recognition does not attribute the built-in process"
    );
    let reported = restarted
        .registry
        .report_agent(identity_report(&id, RuntimeId::HERMES))
        .await;
    assert!(
        !reported.recorded,
        "callbacks follow the same rule as process recognition"
    );
    let inspected = restarted.registry.inspect(&id).await.expect("inspect");
    assert_eq!(
        inspected.active_agent_base, None,
        "no active agent flaps in"
    );
    assert_ne!(
        inspected.native_session_id.as_deref(),
        Some("native-after-takeover")
    );

    restarted.registry.stop(&id).await.expect("stop");
    restarted.registry.remove(&id).await.expect("remove");
}

/// Store written by the v0.33.1 daemon for a session whose native launch
/// spec was lost; the migration marks a profile session's spec unresolved.
const V0_33_1_DAMAGED_STORE: &str =
    include_str!("../../store/fixtures/v0.33.1-damaged/metadata.jsonl");

/// Profile the damaged session of [`V0_33_1_DAMAGED_STORE`] is rebound to.
const DAMAGED_PROFILE: &str = "work";

#[tokio::test]
async fn an_unresolved_built_in_spec_is_never_repaired_from_the_package_that_serves_its_runtime() {
    let key = signing_key("root");
    let store_path = temp_store_path("alias-repair");
    fs::write(
        &store_path,
        V0_33_1_DAMAGED_STORE.replace(
            "\"agent\":\"claude\"",
            &format!("\"agent\":\"{DAMAGED_PROFILE}\""),
        ),
    )
    .expect("write the damaged store");
    fs::set_permissions(
        &store_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("private store");
    crate::store::migrate_at_startup(&store_path).expect("migrate the damaged store");
    let fixture = Fixture::with_anchor_and_store(
        "alias-repair-plugins",
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    let profile =
        fixture.write_profile(DAMAGED_PROFILE, "base = \"claude\"\nprogram = \"claude\"\n");
    let package = official(RuntimeId::CLAUDE);
    install_official(&fixture, &package, RuntimeId::CLAUDE, 5).await;
    // The profile now resolves to the package, so only the pin of the
    // damaged binding (unpinned, launched from the built-in) can refuse it.
    fixture
        .registry
        .package_bind_profile(PackageBindProfileParams {
            profile: DAMAGED_PROFILE.to_owned(),
            digest: None,
            dry_run: false,
        })
        .await
        .expect("pin the profile to the official package");
    assert!(fs::read_to_string(&profile)
        .expect("read the profile")
        .contains(package.digest.as_str()));

    let restarted = fixture.reopen();
    Box::pin(restarted.registry.reconcile_workers())
        .await
        .expect("rehydrate the damaged store");

    let sessions = restarted.registry.list().await;
    assert_eq!(sessions.len(), 1, "the session stays listed");
    let session = &sessions[0];
    assert!(!session.capabilities.resume, "{session:?}");
    let warning = session
        .warnings
        .iter()
        .find(|warning| warning.kind == protocol::SessionWarningKind::NativeRecovery)
        .unwrap_or_else(|| panic!("no native recovery warning: {session:?}"));
    assert!(
        warning.message.contains("is not served by this daemon"),
        "{}",
        warning.message
    );
    let refused = restarted
        .registry
        .resume(&session.id)
        .await
        .expect_err("the unrepaired session is never resumed");
    assert_eq!(refused.code, "runtime_served_by_package");
}

#[tokio::test]
async fn an_ordinary_package_keeps_serving_next_to_an_official_alias() {
    let key = signing_key("root");
    let fixture = Fixture::with_anchor("alias-ordinary", anchor(&[&key], Vec::new()));
    let ordinary = Package::pi();
    let path = fixture.write_archive(&ordinary);
    fixture
        .registry
        .package_install(explicit(&path, &ordinary, true, true))
        .await
        .expect("an ordinary package");
    let package = official(RuntimeId::HERMES);

    install_official(&fixture, &package, RuntimeId::HERMES, 5).await;

    assert_eq!(fixture.serving(RUNTIME), Some(ordinary.digest));
    assert_eq!(fixture.serving(RuntimeId::HERMES), Some(package.digest));
}

/// Detection manifest of the official package: other process matchers and
/// other rules than the built-in `hermes`.
const OFFICIAL_MANIFEST: &str = r#"[process]
comm = ["^official-hermes$"]
cmdline = ['(^|/)official-hermes($| )']

[[rules]]
id = "official_blocked"
state = "blocked"
priority = 1
region = "whole_recent"
contains = "official needs you"
"#;

fn fact(pid: crate::procwatch::Pid, comm: &str) -> crate::procwatch::ProcessFact {
    crate::procwatch::ProcessFact {
        pid,
        pgid: pid,
        ppid: 1,
        start_identity: crate::procwatch::StartIdentity::new(u64::from(pid)),
        comm: comm.to_owned(),
        cmdline: vec![format!("/usr/bin/{comm}")],
    }
}

/// The agents process recognition of session `id` observes in `process`.
fn observe(
    fixture: &Fixture,
    id: &protocol::SessionId,
    process: crate::procwatch::ProcessFact,
    now: std::time::Instant,
    own: Option<&OwnRuntime>,
) -> std::collections::HashMap<crate::procwatch::Pid, super::super::ObservedAgent> {
    fixture
        .registry
        .observed_agents_from_facts(id, vec![process], now, own)
}

#[tokio::test]
async fn a_live_built_in_session_keeps_its_own_observation_through_a_takeover() {
    let key = signing_key("root");
    let dir = temp_dir("alias-live");
    let store_path = temp_store_path("alias-live");
    let fixture = Fixture::with_anchor_and_store(
        "alias-live-plugins",
        anchor(&[&key], Vec::new()),
        store_path,
    );
    write_hermes_profile(&fixture);
    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in runtime");
    let builtin_manifest = format!("{:?}", crate::detect::hermes_manifest());

    let package = take_hermes_over(&fixture).await;
    assert_eq!(fixture.serving(RuntimeId::HERMES), Some(package.digest));

    // The entry retains the built-in definition it was launched from.
    let pinned = fixture
        .registry
        .inner
        .sessions
        .lock()
        .await
        .get(&created.id)
        .and_then(|entry| entry.pinned.clone())
        .expect("a built-in session retains its definition");
    assert!(matches!(
        pinned.binding().provenance,
        protocol::BindingProvenance::Builtin { .. }
    ));
    assert_eq!(format!("{:?}", pinned.manifest()), builtin_manifest);

    // Identity callbacks resolve against the built-in, not the package.
    let resolved = fixture
        .registry
        .resolve_session_agent(&created.id, RuntimeId::HERMES)
        .await
        .expect("resolve the callback agent");
    assert!(matches!(
        resolved.definition.binding().provenance,
        protocol::BindingProvenance::Builtin { .. }
    ));
    assert_eq!(
        format!("{:?}", resolved.definition.manifest()),
        builtin_manifest
    );

    // Process recognition uses the built-in matchers of the session, and the
    // package's matchers only recognize processes the package describes.
    let now = std::time::Instant::now();
    let own = OwnRuntime::Pinned(std::sync::Arc::clone(&pinned));
    let builtin_process = observe(&fixture, &created.id, fact(4001, "hermes"), now, Some(&own));
    assert_eq!(
        builtin_process.len(),
        1,
        "the built-in process is recognized"
    );
    let official_process = observe(
        &fixture,
        &created.id,
        fact(4002, "official-hermes"),
        now,
        Some(&own),
    );
    assert_eq!(
        official_process.len(),
        1,
        "the package process is recognized by the registry"
    );
    let unretained = observe(&fixture, &created.id, fact(4001, "hermes"), now, None);
    assert!(
        unretained.is_empty(),
        "without the retained definition the built-in process is lost"
    );

    assert_transition_rules(
        &fixture,
        &created.id,
        &builtin_process,
        &official_process,
        now,
    )
    .await;
    let _ = fixture.registry.stop(&created.id).await;
}

/// Applies the foreground transition to each observed process and checks
/// which manifest the session's detector then runs.
async fn assert_transition_rules(
    fixture: &Fixture,
    id: &protocol::SessionId,
    builtin_process: &std::collections::HashMap<crate::procwatch::Pid, super::super::ObservedAgent>,
    official_process: &std::collections::HashMap<
        crate::procwatch::Pid,
        super::super::ObservedAgent,
    >,
    now: std::time::Instant,
) {
    // The foreground transition applies the rules of the definition each
    // process matched: the session's own launch root gets the built-in rules,
    // a child the package recognizes gets the package rules.
    let blocker = |manifest: &str| manifest.contains("official_blocked");
    let mut sessions = fixture.registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(id).expect("the live session");
    for (process, expect_package_rule) in [(builtin_process, false), (official_process, true)] {
        let observed = process.values().next().expect("one observed process");
        let report = super::super::ActiveAgentReport {
            source: "test".to_owned(),
            agent: RuntimeId::HERMES.to_owned(),
            seq: Some(1),
            pid: Some(observed.pid),
            start_identity: Some(observed.start_identity),
            reported_at: now,
            activity_reported: false,
        };
        super::super::procwatch::apply_observed_transition(
            entry,
            observed,
            report,
            RuntimeId::HERMES.to_owned(),
        );
        let applied = format!("{:?}", entry.detector_config.borrow().config.manifest);
        assert_eq!(blocker(&applied), expect_package_rule, "{}", observed.pid);
        if !expect_package_rule {
            assert_eq!(
                applied,
                format!("{:?}", Some(crate::detect::hermes_manifest().clone()))
            );
        }
    }
    drop(sessions);
}
