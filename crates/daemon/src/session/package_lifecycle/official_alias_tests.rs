//! Official runtime aliases: a catalog-authorized package serves `codex`,
//! `claude` or `hermes`, which nothing serves without the package, nobody
//! else does, and what a session launched from a built-in runtime may do
//! afterwards.
//!
//! The fixture daemon compiles no Codex, Claude or Hermes in; a session
//! launched from a built-in runtime is written by a daemon that compiles them
//! in ([`compiled_in_host`]), as a previous release did.
//!
//! The catalog is signed with a test key derived from a fixed label.

// Rust guideline compliant 2026-10-12

use pohunek_service_config::preflight::PROBE_BUDGET;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use package::PackageDigest;
use pohunek_service_config::preflight::{Verdict, CODE_RUNTIME_SOURCE_NOT_SERVED};
use pohunek_test_support::wait::wait_until;
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
    durable_recovery, hermetic_shell, hook_gate, native_report_params, params, temp_dir,
    temp_store_path, write_supported_hermes_executable, WORKER_IDENTITY_HELPERS,
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
    install_official_version(fixture, package, alias, "1.0.0", sequence).await
}

/// Like [`install_official`], for `package` at `version`.
async fn install_official_version(
    fixture: &Fixture,
    package: &Package,
    alias: &str,
    version: &str,
    sequence: u64,
) -> protocol::PackageInstallResult {
    let key = signing_key("root");
    let document = signed_by(
        &key,
        catalog(
            sequence,
            vec![entry(package, OFFICIAL_PACKAGE, alias, version)],
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

/// Writes the [`PROFILE`] host profile over the Hermes runtime, launching a
/// program that answers the Hermes version probe and then sleeps.
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
    assert!(hint.contains("cannot be adopted by the package"), "{hint}");
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

/// The preflight inputs of `fixture`'s installed state, over the workers
/// `workers_of` started.
fn preflight_inputs(
    fixture: &Fixture,
    workers_of: &SessionRegistry,
    store_path: &std::path::Path,
) -> PreflightInputs {
    PreflightInputs {
        store_path: store_path.to_path_buf(),
        worker_state_root: workers_of
            .inner
            .config
            .worker_state_root
            .clone()
            .expect("the worker state root"),
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
        assert_eq!(
            served_by(&fixture, alias),
            None,
            "nothing serves {alias} without its package"
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

    // A catalog that names the shell fails verification, so the archive it
    // lists is never decoded.
    assert_eq!(refused, PackageErrorKind::Untrusted);
    assert!(!fixture.root(&package.digest).exists());
    assert!(is_builtin(&fixture, RuntimeId::SHELL));
}

#[tokio::test]
async fn disabling_or_uninstalling_the_official_package_leaves_the_alias_unserved() {
    let key = signing_key("root");
    for alias in ALIASES {
        let fixture = Fixture::with_anchor(
            &format!("alias-return-{alias}"),
            anchor(&[&key], Vec::new()),
        );
        let package = official(alias);
        // The daemon compiles no official alias in.
        let without_package = served_by(&fixture, alias);
        assert_eq!(without_package, None, "{alias}");
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
            .expect_err("no package serves the base"),
        PackageErrorKind::ProfileTargetInvalid
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), text);
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

/// A Hermes session a previous release launched from its built-in runtime is
/// refused once the official package serves `hermes`, and stays refused, as a
/// removed built-in, after the package is uninstalled: nothing hands the pin
/// back to a built-in. No refusal touches the stored session or its pin.
#[tokio::test]
async fn a_built_in_launched_session_fails_closed_after_the_takeover_and_after_the_uninstall() {
    let key = signing_key("root");
    let dir = temp_dir("alias-session");
    let store_path = temp_store_path("alias-session");
    let fixture = Fixture::with_anchor_and_store(
        "alias-session-plugins",
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    write_hermes_profile(&fixture);
    let created = launch_on_a_compiled_in_daemon(&fixture, &store_path, dir).await;
    let pinned = durable_recovery(&store_path, &created.id).launch_binding;
    assert!(matches!(
        pinned.binding().map(|binding| &binding.provenance),
        Some(protocol::BindingProvenance::Builtin { .. })
    ));

    let package = official(RuntimeId::HERMES);
    install_official(&fixture, &package, RuntimeId::HERMES, 5).await;
    let restarted = fixture.reopen();
    Box::pin(restarted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let before = fs::read(&store_path).expect("read the store");

    let refused = restarted
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
    restarted
        .registry
        .inspect(&created.id)
        .await
        .expect("the session stays listed");
    assert_eq!(
        durable_recovery(&store_path, &created.id).launch_binding,
        pinned,
        "the pin keeps its built-in provenance"
    );

    restarted
        .registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall");
    let removed = restarted
        .registry
        .resume(&created.id)
        .await
        .expect_err("nothing serves the built-in the session was launched from");
    assert_eq!(removed.code, "runtime_builtin_removed", "{removed:?}");
    assert_eq!(
        fs::read(&store_path).expect("read the store"),
        before,
        "a refused resume leaves the stored binding untouched"
    );
    restarted
        .registry
        .remove(&created.id)
        .await
        .expect("a refused session is still removable");
}

/// Launches a session of the [`PROFILE`] host profile on a daemon that
/// compiles Codex, Claude and Hermes in ([`compiled_in_host`]), over the agents
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

/// A daemon that compiles Codex, Claude and Hermes in ([`compiled_in_host`]),
/// over the agents directory, host state and session store `store_path` of
/// `fixture`, as a previous release ran.
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

/// Asserts the refusal of `operation` on a session of the removed built-in
/// `alias` nothing serves: the typed code, and a recovery hint that names the
/// official package install, a new session and no rebinding.
fn assert_removed_built_in_refusal(
    removed: &protocol::ProtocolError,
    alias: &str,
    operation: &str,
) {
    assert_eq!(
        removed.code, "runtime_builtin_removed",
        "{alias} {operation}: {removed:?}"
    );
    let hint = removed.recover.as_deref().expect("a recovery hint");
    assert!(
        hint.contains(&format!(
            "`pohunek plugin install runtime/packages/{alias}.tar.zst --catalog runtime/runtime-catalog.json`"
        )),
        "{alias} {operation}: {hint}"
    );
    assert!(
        hint.contains(&format!("`pohunek session new --agent {alias}`")),
        "{alias} {operation}: {hint}"
    );
    assert!(
        hint.contains("adopted by it"),
        "{alias} {operation}: {hint}"
    );
}

/// A session of the official alias `alias` a previous release launched from
/// its built-in runtime, on a daemon that serves no `alias`: resume and fork
/// are refused as a removed built-in whose recovery is the official package
/// and a new session, and once the package is installed, as a session the
/// package does not serve, or as a fork the built-in never supported. No
/// refusal touches the stored session.
async fn assert_built_in_session_is_refused_as_removed_until_the_package_serves(
    alias: &'static str,
) {
    let tag = format!("alias-{alias}-removed");
    let key = signing_key("root");
    let dir = temp_dir(&tag);
    let store_path = temp_store_path(&tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        &format!("base = \"{alias}\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n"),
    );
    let created = launch_on_a_compiled_in_daemon(&fixture, &store_path, dir).await;
    assert!(matches!(
        durable_recovery(&store_path, &created.id)
            .launch_binding
            .binding()
            .map(|binding| &binding.provenance),
        Some(protocol::BindingProvenance::Builtin { .. })
    ));
    let forks = created.capabilities.fork;
    let restarted = fixture.reopen();
    Box::pin(restarted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    assert_eq!(
        served_by(&restarted, alias),
        None,
        "the daemon ships no built-in {alias}"
    );
    let before = fs::read(&store_path).expect("read the store");

    let resumed = restarted
        .registry
        .resume(&created.id)
        .await
        .expect_err("nothing serves the built-in the session was launched from");
    // The runtime source is checked before the fork capability.
    let forked = restarted
        .registry
        .fork(fork_params(&created.id))
        .await
        .expect_err("nothing serves the built-in the session was launched from");
    for (operation, removed) in [("resume", resumed), ("fork", forked)] {
        assert_removed_built_in_refusal(&removed, alias, operation);
    }
    assert_eq!(
        fs::read(&store_path).expect("read the store"),
        before,
        "the refusals leave the stored session untouched"
    );

    drop(restarted);

    install_official(&fixture, &official(alias), alias, 5).await;
    let served = fixture.reopen();
    Box::pin(served.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let refused = served
        .registry
        .resume(&created.id)
        .await
        .expect_err("the package serves the runtime now");
    assert_migration_refusal(&refused, alias);
    let refused = served
        .registry
        .fork(fork_params(&created.id))
        .await
        .expect_err("a fork never launches the package for a built-in session");
    if forks {
        assert_migration_refusal(&refused, alias);
    } else {
        assert_eq!(
            refused.code, "agent_fork_unsupported",
            "{alias}: {refused:?}"
        );
    }

    served
        .registry
        .remove(&created.id)
        .await
        .expect("a refused session is still removable");
}

#[tokio::test]
async fn a_built_in_codex_session_is_refused_as_removed_until_the_package_serves_codex() {
    assert_built_in_session_is_refused_as_removed_until_the_package_serves(RuntimeId::CODEX).await;
}

/// The built-in Claude runtime forks, so its fork is refused like its resume.
#[tokio::test]
async fn a_built_in_claude_session_is_refused_as_removed_until_the_package_serves_claude() {
    assert_built_in_session_is_refused_as_removed_until_the_package_serves(RuntimeId::CLAUDE).await;
}

/// The built-in Hermes runtime does not fork: without the package its fork is
/// refused as a removed built-in like its resume, and once the package serves
/// `hermes` as a fork Hermes does not support.
#[tokio::test]
async fn a_built_in_hermes_session_is_refused_as_removed_until_the_package_serves_hermes() {
    assert_built_in_session_is_refused_as_removed_until_the_package_serves(RuntimeId::HERMES).await;
}

/// The worker journals of session `id` in `registry`'s worker state root, which
/// record how a runtime ended, for a failure that finds a session no longer live.
fn worker_journals(registry: &SessionRegistry, id: &protocol::SessionId) -> String {
    let Some(state_root) = registry.inner.config.worker_state_root.as_ref() else {
        return "no worker state root".to_owned();
    };
    let directory = state_root.join(&id.0);
    match fs::read_dir(&directory) {
        Ok(entries) => entries
            .map(|entry| match entry {
                Ok(entry) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    match fs::read_to_string(entry.path()) {
                        Ok(journal) => format!("{name}: {journal}"),
                        Err(error) => format!("{name}: unreadable: {error}"),
                    }
                }
                Err(error) => format!("unreadable entry: {error}"),
            })
            .collect::<Vec<_>>()
            .join(" | "),
        Err(error) => format!(
            "worker journals unreadable at {}: {error}",
            directory.display()
        ),
    }
}

/// A live session of the official alias `alias` a previous release launched
/// from its built-in runtime, adopted by a daemon that serves no `alias`:
/// input is refused as a removed built-in, and once the official package
/// serves `alias` the live session takes input again.
async fn assert_adopted_built_in_session_takes_input_only_once_the_package_serves(
    alias: &'static str,
) {
    let tag = format!("alias-{alias}-live");
    let key = signing_key("root");
    let dir = temp_dir(&tag);
    let store_path = temp_store_path(&tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        &format!("base = \"{alias}\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n"),
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
        Some(protocol::RuntimeState::Live),
        "{:?}; {}",
        live.runtime,
        worker_journals(&adopted.registry, &created.id)
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
    assert_eq!(
        refused.code, "runtime_builtin_removed",
        "{alias}: {refused:?}"
    );

    install_official(&fixture, &official(alias), alias, 5).await;
    adopted.host.reload().expect("load the installed package");
    adopted
        .registry
        .input(input())
        .await
        .unwrap_or_else(|error| {
            panic!("the package serves {alias}, so the live session takes input: {error:?}")
        });

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

#[tokio::test]
async fn an_adopted_built_in_codex_session_takes_input_only_once_the_package_serves_codex() {
    assert_adopted_built_in_session_takes_input_only_once_the_package_serves(RuntimeId::CODEX)
        .await;
}

#[tokio::test]
async fn an_adopted_built_in_claude_session_takes_input_only_once_the_package_serves_claude() {
    assert_adopted_built_in_session_takes_input_only_once_the_package_serves(RuntimeId::CLAUDE)
        .await;
}

#[tokio::test]
async fn an_adopted_built_in_hermes_session_takes_input_only_once_the_package_serves_hermes() {
    assert_adopted_built_in_session_takes_input_only_once_the_package_serves(RuntimeId::HERMES)
        .await;
}

/// A live session of the built-in Hermes runtime, launched through the
/// [`PROFILE`] host profile by a daemon that compiles Hermes in
/// ([`compiled_in_daemon`]), as a previous release did.
struct LaunchedHermes {
    fixture: Fixture,
    store_path: std::path::PathBuf,
    /// The daemon that launched the session and runs its worker.
    previous: SessionRegistry,
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
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in runtime");
    let input_rules = previous
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
        previous,
        created,
        input_rules,
    }
}

/// Installs the official Hermes package, whose detection differs from the
/// built-in's, so it serves `hermes`.
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

/// Starts a daemon of this release over `launched`'s state and checks that it
/// adopts the live worker the previous daemon started.
async fn adopt(launched: &LaunchedHermes) -> Fixture {
    let restarted = launched
        .fixture
        .reopen_with_workers_of(&launched.previous)
        .await;
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
        Some(protocol::RuntimeState::Live),
        "{:?}; {}",
        adopted.runtime,
        worker_journals(&restarted.registry, &launched.created.id)
    );
    restarted
}

#[tokio::test]
async fn a_built_in_session_adopted_after_a_takeover_is_observed_only_through_its_pin() {
    let launched = launch_built_in_hermes("alias-adopted").await;
    let created = &launched.created;
    assert!(created.capabilities.resume, "the built-in Hermes resumes");
    let inputs = preflight_inputs(&launched.fixture, &launched.previous, &launched.store_path);
    // Before the takeover nothing serves the pin, so the adopted session
    // would refuse every interaction until the package is installed.
    let (verdict, code, detail) = preflight_verdict(&inputs, &created.id);
    assert_eq!(
        (verdict, code.as_str()),
        (Verdict::WouldLoseRecovery, CODE_RUNTIME_SOURCE_NOT_SERVED),
        "{detail}"
    );
    assert!(detail.contains("runtime_builtin_removed"), "{detail}");
    assert!(
        detail.contains(
            "`pohunek plugin install runtime/packages/hermes.tar.zst --catalog runtime/runtime-catalog.json`"
        ),
        "{detail}"
    );
    assert!(
        detail.contains("`pohunek session new --agent hermes`"),
        "{detail}"
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

/// An adopted built-in session that this daemon serves no pin for, whose
/// worker connection was given up, so its inspect fails until the worker is
/// reconnected.
struct ReleasedWorker {
    restarted: Fixture,
    id: protocol::SessionId,
    expected: crate::session::RuntimeWatchIdentity,
    /// The inspect failure of the released connection.
    lost: crate::runtime::WorkerError,
    socket_path: PathBuf,
    /// A native-id report of the session's root, built while the worker
    /// still answered.
    native_report: protocol::SessionReportNativeIdParams,
}

impl ReleasedWorker {
    /// A built-in Hermes session adopted after the official package took
    /// `hermes` over.
    async fn hermes(tag: &str) -> Self {
        let launched = launch_built_in_hermes(tag).await;
        let id = launched.created.id.clone();
        take_hermes_over(&launched.fixture).await;
        let restarted = adopt(&launched).await;
        Self::release(restarted, id, RuntimeId::HERMES).await
    }

    /// A session of the built-in `runtime`, adopted by a daemon that ships no
    /// built-in and has no package serving `runtime`.
    async fn built_in(runtime: &'static str, tag: &str) -> Self {
        let key = signing_key("root");
        let dir = temp_dir(tag);
        let store_path = temp_store_path(tag);
        let fixture = Fixture::with_anchor_and_store(
            &format!("{tag}-plugins"),
            anchor(&[&key], Vec::new()),
            store_path.clone(),
        );
        fixture.write_profile(
            PROFILE,
            &format!(
                "base = \"{runtime}\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n"
            ),
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
        Box::pin(adopted.registry.reconcile_workers())
            .await
            .expect("startup reconciliation");
        Self::release(adopted, created.id, runtime).await
    }

    /// Gives up the daemon's connection to the worker of the adopted session
    /// `id` of `runtime`. The session's worker watcher is stopped first, so
    /// the scenario, not the watcher's next poll, decides when the worker is
    /// reconnected.
    async fn release(restarted: Fixture, id: protocol::SessionId, runtime: &str) -> Self {
        let (worker, expected) = {
            let sessions = restarted.registry.inner.sessions.lock().await;
            let entry = sessions.get(&id).expect("the adopted session");
            assert!(entry.pinned.is_none(), "the built-in pin is not served");
            let crate::session::RuntimeHandle::Worker(worker) = &entry.runtime else {
                panic!("the adopted session has a live worker");
            };
            entry.runtime_watch_cancel.cancel();
            (
                worker.clone(),
                crate::session::RuntimeWatchIdentity::from_info(&entry.info)
                    .expect("the adopted runtime identity"),
            )
        };
        let native_report = native_report_params(
            &restarted.registry,
            id.clone(),
            runtime.to_owned(),
            format!("{runtime}-conversation-reconnect"),
            None,
        )
        .await;
        worker
            .release_controller()
            .await
            .expect("release the daemon's controller lease");
        let lost = worker
            .inspect()
            .await
            .expect_err("the released connection cannot inspect the worker");
        let socket_path = restarted
            .registry
            .inner
            .config
            .worker_runtime_root
            .as_ref()
            .expect("the fixture's worker runtime root")
            .join(&id.0)
            .join(pohunek_paths::WORKER_SOCKET_NAME);
        Self {
            restarted,
            id,
            expected,
            lost,
            socket_path,
            native_report,
        }
    }

    /// The watcher's reconnection: the runtime turns `Reconnecting`, then the
    /// reconnected worker replaces the lost connection.
    async fn reconnect(&self) {
        let registry = &self.restarted.registry;
        assert!(
            registry
                .reconnect_worker(
                    &self.id,
                    &self.expected,
                    &self.socket_path,
                    &tokio_util::sync::CancellationToken::new(),
                    &self.lost,
                    registry.inner.config.worker_connect_deadline,
                )
                .await
                .is_some(),
            "the worker is reconnected"
        );
    }

    /// Starts a Hermes notification's hook admission and returns once the
    /// admission logged `waiting`, so it is pending while the worker is away.
    async fn admission_waiting_on(
        &self,
        logs: &crate::runtime::lifecycle::tests::LogCapture,
        waiting: &'static str,
    ) -> tokio::task::JoinHandle<Result<(), crate::session::HookAdmissionError>> {
        let registry = self.restarted.registry.clone();
        let reported = self.id.clone();
        let admission = tokio::spawn(async move {
            registry
                .admit_session_hook(
                    &reported,
                    pohunek_worker_protocol::HookAction::Notification,
                    RuntimeId::HERMES,
                    "hook notification",
                )
                .await
        });
        wait_until("the hook admission waits for the worker", || async {
            logs.text().contains(waiting).then_some(())
        })
        .await;
        admission
    }
}

#[tokio::test]
async fn a_hook_report_while_an_unpinned_sessions_worker_reconnects_is_admitted_by_its_journaled_schema(
) {
    let released = ReleasedWorker::hermes("alias-reconnect").await;
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let admission = released
        .admission_waiting_on(&logs, "the worker cannot report its journaled hook schema")
        .await;

    released.reconnect().await;
    admission
        .await
        .expect("the admission task joins")
        .expect("the report is admitted by the schema the reconnected worker journaled");

    // A report the journaled schema refuses is logged at WARN with the
    // session and the operation.
    released
        .restarted
        .registry
        .admit_session_hook(
            &released.id,
            pohunek_worker_protocol::HookAction::Notification,
            "unlisted-provider",
            "hook notification",
        )
        .await
        .expect_err("the Hermes schema refuses a provider it does not list");
    let refusal = logs
        .text()
        .lines()
        .find(|line| line.contains("the hook report is refused by the session's hook schema"))
        .map(str::to_owned)
        .expect("the refusal is logged");
    assert!(
        refusal.contains("WARN")
            && refusal.contains(&released.id.0)
            && refusal.contains("operation=\"hook notification\""),
        "{refusal}"
    );
}

#[tokio::test]
async fn a_hook_report_while_an_unpinned_sessions_runtime_is_reconnecting_is_admitted_once_its_worker_is_back(
) {
    let released = ReleasedWorker::hermes("alias-reconnecting").await;
    let registry = &released.restarted.registry;
    // The worker stayed unreachable for the connect deadline while its
    // supervisor could not tell whether it is alive.
    assert!(
        matches!(
            registry
                .mark_worker_unavailable(
                    &released.id,
                    &released.expected,
                    protocol::RuntimeState::Reconnecting,
                    crate::runtime::lifecycle::SUPERVISION_UNAVAILABLE,
                )
                .await,
            crate::session::RuntimeTransitionOutcome::Applied(_)
        ),
        "the runtime is reconnecting without a worker handle"
    );
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let admission = released
        .admission_waiting_on(
            &logs,
            "the hook admission waits for the session's worker to come back",
        )
        .await;

    let reconnected =
        crate::runtime::Worker::connect_discovered(&released.socket_path, "reconnected-daemon")
            .await
            .expect("reconnect the worker");
    assert!(
        matches!(
            registry
                .adopt_reconnected_worker(&released.id, &released.expected, reconnected)
                .await,
            crate::session::RuntimeTransitionOutcome::Applied(_)
        ),
        "the reconnected worker replaces the unavailable runtime"
    );
    admission
        .await
        .expect("the admission task joins")
        .expect("the report is admitted by the schema the reconnected worker journaled");
}

#[tokio::test]
async fn a_native_id_report_while_an_unpinned_sessions_worker_reconnects_is_recorded_through_the_reconnected_worker(
) {
    // Claude's hook schema takes a native-id report from the runtime's own
    // process; Hermes's takes its launch identity only from the worker.
    let released = ReleasedWorker::built_in(RuntimeId::CLAUDE, "alias-native-reconnect").await;
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let registry = released.restarted.registry.clone();
    let report = released.native_report.clone();
    let recorded = tokio::spawn(async move { registry.report_as_root(report).await.recorded });
    wait_until("the native-id report waits for the worker", || async {
        logs.text()
            .contains("the worker cannot report its journaled hook schema")
            .then_some(())
    })
    .await;

    released.reconnect().await;
    assert!(
        recorded.await.expect("the report task joins"),
        "the report is validated against the reconnected worker and recorded"
    );
}

/// Validity of a native-id report that expires while its admission waits:
/// long enough for the admission to start waiting on a loaded runner, and
/// well inside the admission's `worker_connect_deadline`.
const SHORT_CLAIM_TTL: time::Duration = time::Duration::seconds(5);

#[tokio::test]
async fn a_native_id_report_that_expires_while_the_worker_reconnects_is_not_recorded() {
    let released = ReleasedWorker::built_in(RuntimeId::CLAUDE, "alias-native-expired").await;
    let expires_at = time::OffsetDateTime::now_utc() + SHORT_CLAIM_TTL;
    let identity = &released.native_report;
    let report = protocol::SessionReportNativeIdParams::new(
        released.id.clone(),
        identity.worker_instance_id().to_owned(),
        RuntimeId::CLAUDE.to_owned(),
        identity.pid(),
        identity.pid_start_identity(),
        identity.sequence(),
        expires_at
            .format(&time::format_description::well_known::Rfc3339)
            .expect("format the expiry"),
        "claude-conversation-expired".to_owned(),
        None,
    )
    .expect("a valid native-id report");
    let logs = crate::runtime::lifecycle::tests::LogCapture::default();
    let _subscriber = logs.install();
    let registry = released.restarted.registry.clone();
    let recorded = tokio::spawn(async move { registry.report_as_root(report).await.recorded });
    wait_until("the native-id report waits for the worker", || async {
        logs.text()
            .contains("the worker cannot report its journaled hook schema")
            .then_some(())
    })
    .await;
    wait_until("the native-id report expires", || async {
        (time::OffsetDateTime::now_utc() > expires_at).then_some(())
    })
    .await;

    released.reconnect().await;
    assert!(
        !recorded.await.expect("the report task joins"),
        "a report that expired while it waited is not recorded"
    );
    // The report was admitted by the reconnected worker's schema and refused
    // only for its expiry, not for the admission bound.
    let logs = logs.text();
    assert!(
        logs.contains("native-id report expired before it was recorded")
            && !logs.contains("the worker's hook schema was unavailable after the bound"),
        "{logs}"
    );
    let info = released
        .restarted
        .registry
        .inspect(&released.id)
        .await
        .expect("the session");
    assert_ne!(
        info.native_session_id.as_deref(),
        Some("claude-conversation-expired")
    );
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
            "hook notification",
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
async fn an_adopted_built_in_session_stays_unattributed_after_another_package_serves_its_runtime() {
    let launched = launch_built_in_hermes("alias-returned").await;
    let id = launched.created.id.clone();
    let package = take_hermes_over(&launched.fixture).await;
    let restarted = adopt(&launched).await;

    // Another official package serves `hermes` once the first is gone, one
    // whose detection recognizes the process the built-in launched, but the
    // session was adopted without a definition of its pin.
    restarted
        .registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall the official package");
    let second = Package::build_with_detect(
        OFFICIAL_PACKAGE,
        "1.0.0",
        RuntimeId::HERMES,
        INERT_PROGRAM,
        BUILT_IN_PROCESS_MANIFEST,
    );
    install_official(&launched.fixture, &second, RuntimeId::HERMES, 6).await;
    restarted.host.reload().expect("load the second package");
    assert_eq!(restarted.serving(RuntimeId::HERMES), Some(second.digest));
    assert_eq!(
        crate::detect::identify_agent(&restarted.host, &fact(4001, "hermes")),
        Some(protocol::RuntimeRef::hermes()),
        "the second package recognizes the built-in's process"
    );

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

/// Detection manifest of a package that recognizes the process the built-in
/// `hermes` launched.
const BUILT_IN_PROCESS_MANIFEST: &str = r#"[process]
comm = ["^hermes$"]
cmdline = ['(^|/)hermes($| )']

[[rules]]
id = "built_in_process_blocked"
state = "blocked"
priority = 1
region = "whole_recent"
contains = "the process needs you"
"#;

/// Override manifest of a host profile over `claude`. Its base waits for a
/// screen idle state, so the override declares one.
const OVERRIDE_MANIFEST: &str = r#"[process]
comm = ["^official-hermes$"]
cmdline = ['(^|/)official-hermes($| )']

[[rules]]
id = "official_blocked"
state = "blocked"
priority = 1
region = "whole_recent"
contains = "official needs you"

[[rules]]
id = "override_idle"
state = "idle"
priority = 2
region = "whole_recent"
contains = "override is idle"
"#;

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

/// A live session keeps observing through the package version it was
/// launched from while a later version of the official package serves its
/// runtime: what serves the runtime id now never stands in for the session's
/// own runtime source.
#[tokio::test]
async fn a_live_session_keeps_its_own_observation_through_a_later_package_version() {
    let key = signing_key("root");
    let dir = temp_dir("alias-live");
    let store_path = temp_store_path("alias-live");
    let fixture = Fixture::with_anchor_and_store(
        "alias-live-plugins",
        anchor(&[&key], Vec::new()),
        store_path,
    );
    let program = fixture.dir.join("hermes");
    write_supported_hermes_executable(&program, "sleep 30\n");
    let first = Package::build_with_detect(
        OFFICIAL_PACKAGE,
        "1.0.0",
        RuntimeId::HERMES,
        program.to_str().expect("utf-8 program path"),
        BUILT_IN_PROCESS_MANIFEST,
    );
    install_official(&fixture, &first, RuntimeId::HERMES, 5).await;
    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: RuntimeId::HERMES.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the first package version");
    let first_manifest = format!(
        "{:?}",
        crate::detect::Manifest::parse_str(BUILT_IN_PROCESS_MANIFEST).expect("the manifest parses")
    );
    let first_provenance = official_provenance("1.0.0", &first);

    let second = Package::build_with_detect(
        OFFICIAL_PACKAGE,
        "2.0.0",
        RuntimeId::HERMES,
        INERT_PROGRAM,
        OFFICIAL_MANIFEST,
    );
    install_official_version(&fixture, &second, RuntimeId::HERMES, "2.0.0", 6).await;
    assert_eq!(fixture.serving(RuntimeId::HERMES), Some(second.digest));

    // The entry retains the definition it was launched from.
    let pinned = fixture
        .registry
        .inner
        .sessions
        .lock()
        .await
        .get(&created.id)
        .and_then(|entry| entry.pinned.clone())
        .expect("a package session retains its definition");
    assert_eq!(pinned.binding().provenance, first_provenance);
    assert_eq!(format!("{:?}", pinned.manifest()), first_manifest);

    // Identity callbacks resolve against the first version, not the selected
    // one.
    let resolved = fixture
        .registry
        .resolve_session_agent(&created.id, RuntimeId::HERMES)
        .await
        .expect("resolve the callback agent")
        .agent;
    assert_eq!(resolved.definition.binding().provenance, first_provenance);
    assert_eq!(
        format!("{:?}", resolved.definition.manifest()),
        first_manifest
    );

    // Process recognition uses the matchers of the session's own version, and
    // the selected version's matchers only recognize processes it describes.
    let now = std::time::Instant::now();
    let own = OwnRuntime::Pinned(std::sync::Arc::clone(&pinned));
    let own_process = observe(&fixture, &created.id, fact(4001, "hermes"), now, Some(&own));
    assert_eq!(
        own_process.len(),
        1,
        "the process of the session's own version is recognized"
    );
    let selected_process = observe(
        &fixture,
        &created.id,
        fact(4002, "official-hermes"),
        now,
        Some(&own),
    );
    assert_eq!(
        selected_process.len(),
        1,
        "the selected version's process is recognized by the registry"
    );
    let unretained = observe(&fixture, &created.id, fact(4001, "hermes"), now, None);
    assert!(
        unretained.is_empty(),
        "without the retained definition the session's own process is lost"
    );

    assert_transition_rules(
        &fixture,
        &created.id,
        &own_process,
        &selected_process,
        &first_manifest,
        now,
    )
    .await;
    let _ = fixture.registry.stop(&created.id).await;
}

/// The provenance of `package`, the official package at `version`.
fn official_provenance(version: &str, package: &Package) -> protocol::BindingProvenance {
    protocol::BindingProvenance::Package {
        package: protocol::PackageIdentity {
            id: protocol::PackageId::parse(OFFICIAL_PACKAGE).expect("package id"),
            version: protocol::PackageVersion::parse(version).expect("version"),
        },
        package_digest: package.digest.clone(),
    }
}

/// Applies the foreground transition to each observed process and checks
/// which manifest the session's detector then runs.
async fn assert_transition_rules(
    fixture: &Fixture,
    id: &protocol::SessionId,
    own_process: &std::collections::HashMap<crate::procwatch::Pid, super::super::ObservedAgent>,
    selected_process: &std::collections::HashMap<
        crate::procwatch::Pid,
        super::super::ObservedAgent,
    >,
    own_manifest: &str,
    now: std::time::Instant,
) {
    // The foreground transition applies the rules of the definition each
    // process matched: the session's own launch root gets the rules of its
    // own version, a child the selected version recognizes gets that
    // version's rules.
    let blocker = |manifest: &str| manifest.contains("official_blocked");
    let mut sessions = fixture.registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(id).expect("the live session");
    for (process, expect_selected_rule) in [(own_process, false), (selected_process, true)] {
        let observed = process.values().next().expect("one observed process");
        let report = super::super::ActiveAgentReport {
            source: "test".to_owned(),
            agent: RuntimeId::HERMES.to_owned(),
            seq: Some(1),
            pid: Some(observed.pid),
            start_identity: Some(observed.start_identity),
            reported_at: now,
            activity_reported: false,
            observed: true,
        };
        super::super::procwatch::apply_observed_transition(
            entry,
            observed,
            report,
            RuntimeId::HERMES.to_owned(),
        );
        let applied = format!("{:?}", entry.detector_config.borrow().config.manifest);
        assert_eq!(blocker(&applied), expect_selected_rule, "{}", observed.pid);
        if !expect_selected_rule {
            assert_eq!(applied, format!("Some({own_manifest})"));
        }
    }
    drop(sessions);
}

// Adoption: an official package with the identity of a built-in runtime serves
// the sessions that built-in launched, and only those launches it honors.

/// The conversation a built-in Claude session reports in these tests.
const CONVERSATION: &str = "native-claude-1";

/// The official package of `alias` as its own descriptor and detection
/// manifest ship it, the bytes `plugin install` receives.
fn real_official(alias: &str) -> Package {
    Package::from_entries(&crate::agent::host::fixture::official_package_entries(
        alias,
    ))
}

/// Installs `package`, the official package of `alias`, through `registry` with
/// a catalog signed by the test root.
async fn install_real_official(
    fixture: &Fixture,
    registry: &SessionRegistry,
    alias: &str,
    package: &Package,
    sequence: u64,
) {
    let (package_id, version) = crate::agent::host::fixture::official_package_identity(alias);
    let key = signing_key("root");
    let document = signed_by(
        &key,
        catalog(sequence, vec![entry(package, &package_id, alias, &version)]),
    );
    registry
        .package_install(catalog_install(fixture, package, &document, false))
        .await
        .expect("official install");
}

/// The resume and fork flags `session list` reports for session `id`.
async fn listed_recovery(registry: &SessionRegistry, id: &protocol::SessionId) -> (bool, bool) {
    let mut list = registry.list().await;
    registry.enrich_native_activity(&mut list).await;
    let info = list
        .iter()
        .find(|info| info.id.0 == id.0)
        .unwrap_or_else(|| panic!("{} is listed", id.0));
    (info.capabilities.resume, info.capabilities.fork)
}

/// The resume and fork refusal codes `session list` reports for session `id`.
async fn listed_refusals(
    registry: &SessionRegistry,
    id: &protocol::SessionId,
) -> (Option<String>, Option<String>) {
    let mut list = registry.list().await;
    registry.enrich_native_activity(&mut list).await;
    let refusals = list
        .into_iter()
        .find(|info| info.id.0 == id.0)
        .unwrap_or_else(|| panic!("{} is listed", id.0))
        .capability_refusals
        .unwrap_or_default();
    (refusals.resume, refusals.fork)
}

/// Rewrites the durable recovery binding of session `id` through `edit`, as an
/// older release or a damaged record would have stored it.
fn edit_durable_recovery(
    store_path: &Path,
    id: &protocol::SessionId,
    edit: impl FnOnce(&mut crate::store::ResumeBinding),
) {
    let store = crate::store::Store::new(store_path.to_path_buf());
    let mut record = store
        .load_sessions()
        .expect("durable sessions")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("the session record");
    edit(record.recovery.as_mut().expect("a recovery binding"));
    store
        .record_session(&record)
        .expect("write the session record");
}

/// The host profile of the built-in Claude runtime `stopped_built_in_claude`
/// writes, with its conversations below `config_home`.
fn claude_profile_text(config_home: &Path) -> String {
    format!(
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n\n[env]\nCLAUDE_CONFIG_DIR = \"{}\"\n",
        config_home.display()
    )
}

/// A stopped session of the built-in Claude runtime, launched by a daemon that
/// compiles it in, whose conversation that daemon recorded.
struct StoppedClaude {
    fixture: Fixture,
    store_path: PathBuf,
    id: protocol::SessionId,
}

async fn stopped_built_in_claude(tag: &str) -> StoppedClaude {
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    // Claude keeps its conversations below `CLAUDE_CONFIG_DIR`; the profile
    // points it at a directory the test owns, which the reference check reads.
    let config_home = fixture.dir.join("claude-home");
    let conversations = config_home.join("projects").join("work");
    fs::create_dir_all(&conversations).expect("the conversation directory");
    fs::write(conversations.join(format!("{CONVERSATION}.jsonl")), b"{}\n")
        .expect("the conversation");
    fixture.write_profile(PROFILE, &claude_profile_text(&config_home));
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let report = native_report_params(
        &previous,
        created.id.clone(),
        RuntimeId::CLAUDE.to_owned(),
        CONVERSATION.to_owned(),
        None,
    )
    .await;
    assert!(previous.report_as_root(report).await.recorded);
    previous.stop(&created.id).await.expect("stop the session");
    StoppedClaude {
        fixture,
        store_path,
        id: created.id,
    }
}

/// Restarts the daemon over `fixture`'s state and reconciles its workers.
async fn restart(fixture: &Fixture) -> Fixture {
    let restarted = fixture.reopen();
    Box::pin(restarted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    restarted
}

#[tokio::test]
async fn a_stopped_built_in_claude_session_resumes_through_the_official_package_and_pins_it() {
    let session = stopped_built_in_claude("claude-adopt-resume").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    let restarted = restart(&session.fixture).await;
    assert_eq!(
        listed_recovery(&restarted.registry, &session.id).await,
        (true, true),
        "the package adopts the session, so it lists as resumable and forkable"
    );
    assert_eq!(
        listed_refusals(&restarted.registry, &session.id).await,
        (None, None)
    );

    // The host profile over the built-in changed its base runtime's launch
    // binding, so its revision moves with the adoption; the owner accepts that
    // change the way `--accept-profile-change` does.
    let resumed = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Accept)
        .await
        .expect("the package adopts the built-in session");
    assert_eq!(
        resumed.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Live)
    );
    match durable_recovery(&session.store_path, &session.id)
        .launch_binding
        .binding()
        .map(|binding| &binding.provenance)
    {
        Some(protocol::BindingProvenance::Package { package_digest, .. }) => {
            assert_eq!(package_digest, &package.digest);
        }
        other => panic!("the relaunch records the package pin, not {other:?}"),
    }
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

#[tokio::test]
async fn a_built_in_claude_session_is_not_listed_as_resumable_while_no_package_serves_it() {
    let session = stopped_built_in_claude("claude-no-package").await;
    let restarted = restart(&session.fixture).await;
    assert_eq!(
        listed_recovery(&restarted.registry, &session.id).await,
        (false, false)
    );
    let resumed = restarted
        .registry
        .resume(&session.id)
        .await
        .expect_err("nothing serves the built-in the session was launched from");
    assert_eq!(resumed.code, "runtime_builtin_removed", "{resumed:?}");
    let forked = restarted
        .registry
        .fork(fork_params(&session.id))
        .await
        .expect_err("nothing serves the built-in the session was launched from");
    assert_eq!(forked.code, "runtime_builtin_removed", "{forked:?}");
    assert_eq!(
        listed_refusals(&restarted.registry, &session.id).await,
        (Some(resumed.code), Some(forked.code)),
        "the listing names the codes resume and fork refuse with"
    );
}

#[tokio::test]
async fn an_unpinned_built_in_claude_session_is_never_resumed_through_the_package() {
    let session = stopped_built_in_claude("claude-unpinned").await;
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &real_official(RuntimeId::CLAUDE),
        5,
    )
    .await;
    edit_durable_recovery(&session.store_path, &session.id, |binding| {
        binding.launch_binding = crate::agent::host::LaunchPin::Unpinned;
    });
    let restarted = restart(&session.fixture).await;
    assert_eq!(
        listed_recovery(&restarted.registry, &session.id).await,
        (false, false)
    );
    let resumed = restarted
        .registry
        .resume(&session.id)
        .await
        .expect_err("an unpinned session names no built-in to adopt");
    assert_eq!(resumed.code, "runtime_served_by_package", "{resumed:?}");
    let forked = restarted
        .registry
        .fork(fork_params(&session.id))
        .await
        .expect_err("an unpinned session names no built-in to adopt");
    assert_eq!(forked.code, "runtime_served_by_package", "{forked:?}");
    assert_eq!(
        listed_refusals(&restarted.registry, &session.id).await,
        (Some(resumed.code), Some(forked.code)),
        "the listing names the codes resume and fork refuse with"
    );
}

#[tokio::test]
async fn a_built_in_claude_session_is_refused_when_the_package_does_not_honor_its_frozen_launch() {
    let session = stopped_built_in_claude("claude-frozen-mismatch").await;
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &real_official(RuntimeId::CLAUDE),
        5,
    )
    .await;
    // The built-in froze an id-kind reference; a path-kind spec is one the
    // package cannot honor.
    edit_durable_recovery(&session.store_path, &session.id, |binding| {
        binding.native_launch = Some(
            crate::agent::NativeSessionLaunch::from_templates(
                crate::agent::SessionRefKind::Path,
                &["--resume", "{reference}"],
                Some(&["--resume", "{reference}", "--fork-session"]),
            )
            .expect("a valid path-kind spec"),
        );
    });
    let restarted = restart(&session.fixture).await;
    assert_eq!(
        listed_recovery(&restarted.registry, &session.id).await,
        (false, false)
    );
    let resumed = restarted
        .registry
        .resume(&session.id)
        .await
        .expect_err("the package does not honor the frozen reference kind");
    assert_eq!(resumed.code, "runtime_served_by_package", "{resumed:?}");
    let forked = restarted
        .registry
        .fork(fork_params(&session.id))
        .await
        .expect_err("the package does not honor the frozen reference kind");
    assert_eq!(forked.code, "runtime_served_by_package", "{forked:?}");
    assert_eq!(
        listed_refusals(&restarted.registry, &session.id).await,
        (Some(resumed.code), Some(forked.code)),
        "the listing names the codes resume and fork refuse with"
    );
}

/// A stopped session of the built-in `alias` that has no fork, launched by a
/// daemon that compiles it in, once the official package of `alias` serves it,
/// neither lists as forkable nor forks through the package.
async fn assert_built_in_session_without_fork_stays_unforkable(alias: &'static str) {
    let tag = format!("alias-{alias}-no-fork");
    let key = signing_key("root");
    let dir = temp_dir(&tag);
    let store_path = temp_store_path(&tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        &format!("base = \"{alias}\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n"),
    );
    let created = launch_on_a_compiled_in_daemon(&fixture, &store_path, dir).await;
    assert!(
        !created.capabilities.fork,
        "the built-in {alias} has no fork"
    );
    install_real_official(&fixture, &fixture.registry, alias, &real_official(alias), 5).await;
    let restarted = restart(&fixture).await;
    assert!(
        !listed_recovery(&restarted.registry, &created.id).await.1,
        "{alias}: the listing does not advertise a fork"
    );
    let forked = restarted
        .registry
        .fork(fork_params(&created.id))
        .await
        .expect_err("the built-in has no fork to adopt");
    assert_eq!(forked.code, "agent_fork_unsupported", "{alias}: {forked:?}");
}

#[tokio::test]
async fn a_built_in_codex_session_stays_unforkable_once_the_package_adopts_it() {
    assert_built_in_session_without_fork_stays_unforkable(RuntimeId::CODEX).await;
}

#[tokio::test]
async fn a_built_in_hermes_session_stays_unforkable_once_the_package_adopts_it() {
    assert_built_in_session_without_fork_stays_unforkable(RuntimeId::HERMES).await;
}

/// The identity report a Claude hook sends for session `id`.
fn claude_identity_report(id: &protocol::SessionId) -> protocol::SessionReportAgentParams {
    protocol::SessionReportAgentParams {
        session_id: id.clone(),
        source: "pohunek:claude".to_owned(),
        agent: RuntimeId::CLAUDE.to_owned(),
        activity: None,
        seq: None,
        pid: None,
        agent_session_id: Some(CONVERSATION.to_owned()),
        agent_session_path: None,
    }
}

#[tokio::test]
async fn a_live_built_in_claude_session_takes_its_callbacks_once_a_package_is_installed_while_running(
) {
    let tag = "claude-live-callbacks";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    assert!(
        !adopted
            .registry
            .report_agent(claude_identity_report(&created.id))
            .await
            .recorded,
        "no package serves the built-in yet, so its callbacks are not attributed"
    );

    install_real_official(
        &adopted,
        &adopted.registry,
        RuntimeId::CLAUDE,
        &real_official(RuntimeId::CLAUDE),
        5,
    )
    .await;
    assert!(
        adopted
            .registry
            .report_agent(claude_identity_report(&created.id))
            .await
            .recorded,
        "the installed package serves the live session's runtime"
    );
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

/// Arms `registry` to park its next resume or fork after its definition
/// resolved and before the record is registered.
fn arm_recovery_hold(registry: &SessionRegistry) -> crate::runtime::lifecycle::tests::StartGate {
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: std::sync::Arc::new(tokio::sync::Notify::new()),
        release: std::sync::Arc::new(tokio::sync::Notify::new()),
    };
    *registry
        .inner
        .recovery_hold
        .lock()
        .expect("recovery hold is never poisoned") = Some(gate.clone());
    gate
}

/// The official Claude package with its fork removed from its descriptor: a
/// package that resumes the conversation but cannot fork it.
fn claude_without_fork() -> Package {
    let mut entries = crate::agent::host::fixture::official_package_entries(RuntimeId::CLAUDE);
    let descriptor = String::from_utf8(entries[0].contents.clone()).expect("UTF-8 descriptor");
    let forking =
        "[fork]\nsupported = true\nargs = [\"--resume\", \"{reference}\", \"--fork-session\"]\n";
    assert!(descriptor.contains(forking), "the descriptor forks");
    entries[0].contents = descriptor
        .replace(forking, "[fork]\nsupported = false\n")
        .into_bytes();
    Package::from_entries(&entries)
}

/// The Debug form of a detector configuration, which compares its manifest.
fn detector_debug(config: &crate::detect::DetectorConfig) -> String {
    format!("{config:?}")
}

/// Asserts that a stopped adopted resume refuses after `change` altered the
/// adopting package's root, with the stored session untouched.
async fn assert_adopted_resume_refuses_a_changed_root(
    tag: &str,
    change: fn(&Fixture, &Package),
    code: &str,
) {
    let session = stopped_built_in_claude(tag).await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    // The registry loads the package before the root changes.
    let restarted = restart(&session.fixture).await;
    change(&session.fixture, &package);
    let before = fs::read(&session.store_path).expect("read the store");

    let refused = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Accept)
        .await
        .expect_err("the changed package root does not serve the adopted session");
    assert_eq!(refused.code, code, "{refused:?}");
    assert_eq!(
        listed_recovery(&restarted.registry, &session.id).await,
        (false, false)
    );
    assert_eq!(
        listed_refusals(&restarted.registry, &session.id).await,
        (Some(code.to_owned()), Some(code.to_owned())),
        "fork resolves the same changed root first"
    );
    assert_eq!(
        fs::read(&session.store_path).expect("read the store"),
        before,
        "the refused resume persists nothing"
    );
}

#[tokio::test]
async fn an_adopted_resume_refuses_a_package_root_modified_after_the_registry_loaded() {
    assert_adopted_resume_refuses_a_changed_root(
        "claude-tampered-root",
        |fixture, package| fixture.tamper(&package.digest),
        "runtime_incompatible",
    )
    .await;
}

#[tokio::test]
async fn an_adopted_resume_refuses_a_package_root_removed_after_the_registry_loaded() {
    assert_adopted_resume_refuses_a_changed_root(
        "claude-removed-root",
        |fixture, package| {
            fs::remove_dir_all(fixture.root(&package.digest)).expect("remove the package root");
        },
        "runtime_incompatible",
    )
    .await;
}

#[tokio::test]
async fn an_uninstall_waits_for_an_adopted_resume_to_persist_its_package_pin() {
    let session = stopped_built_in_claude("claude-uninstall-race").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    let restarted = restart(&session.fixture).await;
    let gate = arm_recovery_hold(&restarted.registry);
    let resume = tokio::spawn({
        let registry = restarted.registry.clone();
        let id = session.id.clone();
        async move {
            registry
                .resume_with(&id, crate::session::ProfileChange::Accept)
                .await
        }
    });
    gate.entered.notified().await;

    // The parked resume holds the package lifecycle shared, so no exclusive
    // transaction such as an uninstall can start until the pin is persisted.
    assert!(
        restarted
            .registry
            .inner
            .package_lifecycle
            .try_write()
            .is_err(),
        "the exclusive package lifecycle is unavailable while the adopted resume is in flight"
    );
    let uninstall = tokio::spawn({
        let registry = restarted.registry.clone();
        let digest = package.digest.clone();
        async move { registry.package_uninstall(uninstall(&digest)).await }
    });

    gate.release.notify_one();
    resume
        .await
        .expect("resume task")
        .expect("the adopted resume completes");
    let removed = uninstall.await.expect("uninstall task");
    assert!(
        removed.is_err(),
        "the uninstall is refused once the resumed session references the package"
    );
    assert!(
        restarted
            .registry
            .inner
            .profiles
            .runtimes()
            .resolve_id(&RuntimeId::parse(RuntimeId::CLAUDE).expect("runtime id"))
            .is_ok(),
        "the package the resumed session pins is still installed"
    );
    match durable_recovery(&session.store_path, &session.id)
        .launch_binding
        .binding()
        .map(|binding| &binding.provenance)
    {
        Some(protocol::BindingProvenance::Package { package_digest, .. }) => {
            assert_eq!(package_digest, &package.digest);
        }
        other => panic!("the resumed run pins the package, not {other:?}"),
    }
}

#[tokio::test]
async fn an_adopted_resume_onto_a_package_without_fork_cannot_fork() {
    let session = stopped_built_in_claude("claude-no-fork-adopter").await;
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &claude_without_fork(),
        5,
    )
    .await;
    let restarted = restart(&session.fixture).await;
    restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Accept)
        .await
        .expect("the package honors the resume");
    assert_eq!(
        listed_recovery(&restarted.registry, &session.id).await,
        (true, false),
        "the resumed session does not list a fork the package cannot make"
    );
    let forked = restarted
        .registry
        .fork(fork_params(&session.id))
        .await
        .expect_err("the package cannot fork");
    assert_eq!(forked.code, "agent_fork_unsupported", "{forked:?}");
}

#[tokio::test]
async fn a_live_built_in_claude_session_follows_its_package_detector_through_install_and_uninstall()
{
    let tag = "claude-live-detector";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let running = |registry: &SessionRegistry| {
        let registry = registry.clone();
        let id = created.id.clone();
        async move {
            let sessions = registry.inner.sessions.lock().await;
            let entry = sessions.get(&id).expect("the live session");
            let config = entry.detector_config.borrow().config.clone();
            detector_debug(&config)
        }
    };
    let generic = detector_debug(&crate::detect::DetectorConfig::generic_shell());
    assert_eq!(running(&adopted.registry).await, generic);

    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(&adopted, &adopted.registry, RuntimeId::CLAUDE, &package, 5).await;
    let served = adopted
        .registry
        .inner
        .profiles
        .runtimes()
        .resolve_id(&RuntimeId::parse(RuntimeId::CLAUDE).expect("runtime id"))
        .expect("the package serves claude");
    let package_config = detector_debug(&crate::detect::DetectorConfig::for_definition(&served));
    assert_ne!(package_config, generic);
    assert_eq!(
        running(&adopted.registry).await,
        package_config,
        "the installed package's detector runs without a callback"
    );

    // An own-runtime active agent runs under the pinned definition, so it
    // follows the package change too.
    assert!(
        adopted
            .registry
            .report_agent(claude_identity_report(&created.id))
            .await
            .recorded,
        "the own-runtime report is recorded"
    );
    assert_eq!(running(&adopted.registry).await, package_config);

    adopted
        .registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall the package");
    assert_eq!(
        running(&adopted.registry).await,
        generic,
        "the uninstalled package's detector is no longer the running one, even under an own-runtime active agent"
    );
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

/// Arms `registry` to park every active-agent report resolution between
/// resolving its agent and committing it, until [`disarm_report_hold`].
fn arm_report_hold(registry: &SessionRegistry) -> crate::runtime::lifecycle::tests::StartGate {
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: std::sync::Arc::new(tokio::sync::Notify::new()),
        release: std::sync::Arc::new(tokio::sync::Notify::new()),
    };
    *registry
        .inner
        .report_hold
        .lock()
        .expect("report hold is never poisoned") = Some(gate.clone());
    gate
}

/// Lets every later report resolution of `registry` commit without parking.
fn disarm_report_hold(registry: &SessionRegistry) {
    *registry
        .inner
        .report_hold
        .lock()
        .expect("report hold is never poisoned") = None;
}

#[tokio::test]
async fn an_agent_report_held_across_an_uninstall_follows_the_current_pin() {
    let tag = "claude-report-pin-race";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(&adopted, &adopted.registry, RuntimeId::CLAUDE, &package, 5).await;
    let generic = detector_debug(&crate::detect::DetectorConfig::generic_shell());
    let served = adopted
        .registry
        .inner
        .profiles
        .runtimes()
        .resolve_id(&RuntimeId::parse(RuntimeId::CLAUDE).expect("runtime id"))
        .expect("the package serves claude");
    let package_config = detector_debug(&crate::detect::DetectorConfig::for_definition(&served));
    assert_ne!(package_config, generic);

    // The report resolves its agent against the package's definition, and
    // the package is uninstalled before the report commits.
    let gate = arm_report_hold(&adopted.registry);
    let reporting = tokio::spawn({
        let registry = adopted.registry.clone();
        let report = claude_identity_report(&created.id);
        async move { registry.report_agent(report).await }
    });
    gate.entered.notified().await;
    adopted
        .registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall the package");
    assert_eq!(
        running_detector(&adopted.registry, &created.id).await,
        generic
    );
    disarm_report_hold(&adopted.registry);
    gate.release.notify_one();
    let held = reporting.await.expect("the held report finishes");

    assert!(
        !held.recorded,
        "re-resolved against the current pin, the report names an unserved runtime"
    );
    assert_eq!(
        running_detector(&adopted.registry, &created.id).await,
        generic,
        "a report resolved before the uninstall never restores the removed detector"
    );

    // Served again, the same report runs the package's detector.
    install_real_official(&adopted, &adopted.registry, RuntimeId::CLAUDE, &package, 6).await;
    assert!(
        adopted
            .registry
            .report_agent(claude_identity_report(&created.id))
            .await
            .recorded,
        "the report is recorded once the package serves the pin again"
    );
    assert_eq!(
        running_detector(&adopted.registry, &created.id).await,
        package_config
    );
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

/// A session of the built-in Claude runtime adopted by a daemon that then
/// installed the official Claude package, with the package's detector
/// configuration.
async fn adopted_claude_beside_its_package(tag: &str) -> (Fixture, protocol::SessionId, String) {
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(&adopted, &adopted.registry, RuntimeId::CLAUDE, &package, 5).await;
    let served = adopted
        .registry
        .inner
        .profiles
        .runtimes()
        .resolve_id(&RuntimeId::parse(RuntimeId::CLAUDE).expect("runtime id"))
        .expect("the package serves claude");
    let package_config = detector_debug(&crate::detect::DetectorConfig::for_definition(&served));
    (adopted, created.id, package_config)
}

#[tokio::test]
async fn an_agent_report_held_across_a_registry_rebuild_resolves_again_and_commits() {
    let (adopted, id, package_config) =
        adopted_claude_beside_its_package("claude-report-rebuild").await;

    // While the report is parked, a host profile named like the runtime gains
    // its own detection manifest and the registry is rebuilt, so the report
    // resolves again, to the profile, and then commits.
    let gate = arm_report_hold(&adopted.registry);
    let reporting = tokio::spawn({
        let registry = adopted.registry.clone();
        let report = claude_identity_report(&id);
        async move { registry.report_agent(report).await }
    });
    gate.entered.notified().await;
    fs::create_dir_all(adopted.agents.join("manifests")).expect("the manifests directory");
    fs::write(
        adopted.agents.join("manifests").join("tuned.toml"),
        OVERRIDE_MANIFEST,
    )
    .expect("the override manifest");
    adopted.write_profile(
        RuntimeId::CLAUDE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\nmanifest = \"tuned\"\n",
    );
    adopted
        .registry
        .reload_runtimes()
        .expect("rebuild the runtime registry");
    disarm_report_hold(&adopted.registry);
    gate.release.notify_one();
    let held = reporting.await.expect("the held report finishes");

    assert!(
        held.recorded,
        "a report resolved again against a registry that still serves claude is recorded"
    );
    assert_eq!(
        adopted
            .registry
            .inspect(&id)
            .await
            .expect("inspect")
            .active_agent
            .as_deref(),
        Some(RuntimeId::CLAUDE)
    );
    let profile_config = {
        let sessions = adopted.registry.inner.sessions.lock().await;
        let pinned = sessions
            .get(&id)
            .expect("the live session")
            .pinned
            .clone()
            .expect("the session's pin is served");
        let resolved = adopted
            .registry
            .inner
            .profiles
            .resolve_agent_pinned(RuntimeId::CLAUDE, &pinned)
            .expect("the profile resolves through the pinned definition");
        detector_debug(&crate::session::detector_config_for_resolved_agent(
            &resolved,
        ))
    };
    assert_ne!(
        profile_config, package_config,
        "the profile's manifest changes what the report resolves to"
    );
    assert_eq!(
        running_detector(&adopted.registry, &id).await,
        profile_config,
        "the report commits what it resolved to after the rebuild"
    );
    adopted
        .registry
        .stop(&id)
        .await
        .expect("stop the adopted session");
    adopted
        .registry
        .remove(&id)
        .await
        .expect("remove the adopted session");
}

#[tokio::test]
async fn an_agent_report_whose_registry_keeps_changing_is_dropped_after_bounded_resolutions() {
    let (adopted, id, _package_config) =
        adopted_claude_beside_its_package("claude-report-exhausted").await;

    // Each resolution is parked and the registry rebuilt under it, one time
    // more than the report resolves again.
    let gate = arm_report_hold(&adopted.registry);
    let reporting = tokio::spawn({
        let registry = adopted.registry.clone();
        let report = claude_identity_report(&id);
        async move { registry.report_agent(report).await }
    });
    let resolutions = crate::session::MAX_STALE_REPORT_RESOLUTIONS + 1;
    for resolution in 1..=resolutions {
        gate.entered.notified().await;
        adopted
            .registry
            .reload_runtimes()
            .expect("rebuild the runtime registry");
        if resolution == resolutions {
            // A report that resolved yet again would now commit unparked.
            disarm_report_hold(&adopted.registry);
        }
        gate.release.notify_one();
    }
    let held = reporting.await.expect("the held report finishes");

    assert!(
        !held.recorded,
        "a report that never resolves against a stable registry is not recorded"
    );
    assert_eq!(
        adopted
            .registry
            .inspect(&id)
            .await
            .expect("inspect")
            .active_agent,
        None
    );
    adopted
        .registry
        .stop(&id)
        .await
        .expect("stop the adopted session");
    adopted
        .registry
        .remove(&id)
        .await
        .expect("remove the adopted session");
}

#[tokio::test]
async fn a_nested_agent_report_held_across_an_uninstall_of_its_package_is_never_committed() {
    let tag = "codex-nested-report-race";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path,
    );
    fixture.write_profile(
        "host",
        "base = \"shell\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let package = real_official(RuntimeId::CODEX);
    install_real_official(&fixture, &fixture.registry, RuntimeId::CODEX, &package, 5).await;
    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: "host".to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a shell session");
    let codex_report = || protocol::SessionReportAgentParams {
        session_id: created.id.clone(),
        source: "pohunek:codex".to_owned(),
        agent: RuntimeId::CODEX.to_owned(),
        activity: None,
        seq: None,
        pid: None,
        agent_session_id: None,
        agent_session_path: None,
    };
    let before = running_detector(&fixture.registry, &created.id).await;

    // The report resolves the nested agent through the registry that serves
    // the package, and the package is uninstalled before the report commits.
    let gate = arm_report_hold(&fixture.registry);
    let reporting = tokio::spawn({
        let registry = fixture.registry.clone();
        let report = codex_report();
        async move { registry.report_agent(report).await }
    });
    gate.entered.notified().await;
    fixture
        .registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall the package");
    disarm_report_hold(&fixture.registry);
    gate.release.notify_one();
    let held = reporting.await.expect("the held report finishes");

    assert!(
        !held.recorded,
        "re-resolved against the current registry, the report names an uninstalled runtime"
    );
    assert_eq!(
        running_detector(&fixture.registry, &created.id).await,
        before,
        "the uninstalled package's detector never runs"
    );

    // Served again, the same report is recorded.
    install_real_official(&fixture, &fixture.registry, RuntimeId::CODEX, &package, 6).await;
    assert!(
        fixture.registry.report_agent(codex_report()).await.recorded,
        "the nested report is recorded while the package serves codex"
    );
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
    fixture
        .registry
        .remove(&created.id)
        .await
        .expect("remove the session");
}

/// The generation of the running detector configuration of session `id`.
async fn detector_generation(registry: &SessionRegistry, id: &protocol::SessionId) -> u64 {
    let sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get(id).expect("the live session");
    let generation = entry.detector_config.borrow().generation;
    generation
}

#[tokio::test]
async fn a_nested_agent_of_another_runtime_keeps_its_detector_through_a_package_change() {
    let tag = "claude-nested-detector";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(&adopted, &adopted.registry, RuntimeId::CLAUDE, &package, 5).await;

    let nested = protocol::SessionReportAgentParams {
        session_id: created.id.clone(),
        source: "pohunek:shell".to_owned(),
        agent: RuntimeId::SHELL.to_owned(),
        activity: None,
        seq: None,
        pid: None,
        agent_session_id: None,
        agent_session_path: None,
    };
    assert!(
        adopted.registry.report_agent(nested).await.recorded,
        "the nested shell agent is recorded"
    );
    let owned_generation = detector_generation(&adopted.registry, &created.id).await;

    adopted
        .registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall the package");
    assert_eq!(
        detector_generation(&adopted.registry, &created.id).await,
        owned_generation,
        "a nested agent of another runtime keeps its detector configuration"
    );
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

/// A host profile over the built-in Claude runtime that names its own
/// detection manifest, which overrides the runtime's.
const TUNED_PROFILE: &str = "tuned";

#[tokio::test]
async fn an_own_runtime_report_naming_a_profile_keeps_its_manifest_override_through_a_package_change(
) {
    let tag = "claude-tuned-detector";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fs::create_dir_all(fixture.agents.join("manifests")).expect("the manifests directory");
    fs::write(
        fixture.agents.join("manifests").join("tuned.toml"),
        OVERRIDE_MANIFEST,
    )
    .expect("the override manifest");
    fixture.write_profile(
        TUNED_PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\nmanifest = \"tuned\"\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: TUNED_PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the tuned profile");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let running = |registry: &SessionRegistry| {
        let registry = registry.clone();
        let id = created.id.clone();
        async move {
            let sessions = registry.inner.sessions.lock().await;
            let entry = sessions.get(&id).expect("the live session");
            let config = entry.detector_config.borrow().config.clone();
            detector_debug(&config)
        }
    };
    // The profile resolves only through the runtime a package serves, so the
    // override is the default once the package is installed.
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(&adopted, &adopted.registry, RuntimeId::CLAUDE, &package, 5).await;
    let served = adopted
        .registry
        .inner
        .profiles
        .runtimes()
        .resolve_id(&RuntimeId::parse(RuntimeId::CLAUDE).expect("runtime id"))
        .expect("the package serves claude");
    let override_config = running(&adopted.registry).await;
    assert_ne!(
        override_config,
        detector_debug(&crate::detect::DetectorConfig::for_definition(&served)),
        "the override differs from the package manifest"
    );
    assert_ne!(
        override_config,
        detector_debug(&crate::detect::DetectorConfig::generic_shell()),
        "the override differs from the generic shell manifest"
    );

    // The session's own profile is the active agent: the report names it.
    let report = protocol::SessionReportAgentParams {
        session_id: created.id.clone(),
        source: "pohunek:tuned".to_owned(),
        agent: TUNED_PROFILE.to_owned(),
        activity: None,
        seq: None,
        pid: None,
        agent_session_id: None,
        agent_session_path: None,
    };
    assert!(
        adopted.registry.report_agent(report).await.recorded,
        "the own-profile report is recorded"
    );

    // A different version of the package becomes the pinned definition.
    install_version(
        &adopted,
        &adopted.registry,
        RuntimeId::CLAUDE,
        &claude_next_version_without_fork(),
        "1.0.2",
        6,
    )
    .await;
    assert_eq!(
        running(&adopted.registry).await,
        override_config,
        "the active own-profile agent keeps its manifest override through the package change"
    );
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

/// The official Claude package without fork, one version above the real one:
/// the next release of the same package identity.
fn claude_next_version_without_fork() -> Package {
    let mut entries = crate::agent::host::fixture::official_package_entries(RuntimeId::CLAUDE);
    let descriptor = String::from_utf8(entries[0].contents.clone()).expect("UTF-8 descriptor");
    let forking =
        "[fork]\nsupported = true\nargs = [\"--resume\", \"{reference}\", \"--fork-session\"]\n";
    let (_, version) = crate::agent::host::fixture::official_package_identity(RuntimeId::CLAUDE);
    let release = format!("version = \"{version}\"");
    assert!(descriptor.contains(forking), "the descriptor forks");
    assert!(
        descriptor.contains(&release),
        "the descriptor declares its version"
    );
    entries[0].contents = descriptor
        .replacen(&release, "version = \"1.0.2\"", 1)
        .replace(forking, "[fork]\nsupported = false\n")
        .into_bytes();
    Package::from_entries(&entries)
}

/// Installs `package` for `alias` at `version` through `registry`, with a
/// catalog signed by the test root.
async fn install_version(
    fixture: &Fixture,
    registry: &SessionRegistry,
    alias: &str,
    package: &Package,
    version: &str,
    sequence: u64,
) {
    let (package_id, _) = crate::agent::host::fixture::official_package_identity(alias);
    let key = signing_key("root");
    let document = signed_by(
        &key,
        catalog(sequence, vec![entry(package, &package_id, alias, version)]),
    );
    registry
        .package_install(catalog_install(fixture, package, &document, false))
        .await
        .expect("official install");
}

/// Arms `registry` to park its next adopted detector refresh between resolving
/// the runtime sources and applying them.
fn arm_refresh_hold(registry: &SessionRegistry) -> crate::runtime::lifecycle::tests::StartGate {
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: std::sync::Arc::new(tokio::sync::Notify::new()),
        release: std::sync::Arc::new(tokio::sync::Notify::new()),
    };
    *registry
        .inner
        .refresh_hold
        .lock()
        .expect("refresh hold is never poisoned") = Some(gate.clone());
    gate
}

#[tokio::test]
async fn a_refresh_that_finds_the_active_agent_ended_sends_the_default_detector() {
    let tag = "claude-refresh-agent-ended";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile(
        PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(&adopted, &adopted.registry, RuntimeId::CLAUDE, &package, 5).await;
    let running = |registry: &SessionRegistry| {
        let registry = registry.clone();
        let id = created.id.clone();
        async move {
            let sessions = registry.inner.sessions.lock().await;
            let entry = sessions.get(&id).expect("the live session");
            let config = entry.detector_config.borrow().config.clone();
            detector_debug(&config)
        }
    };
    let generic = detector_debug(&crate::detect::DetectorConfig::generic_shell());
    assert!(
        adopted
            .registry
            .report_agent(claude_identity_report(&created.id))
            .await
            .recorded,
        "the own-runtime report is recorded"
    );
    let reported = running(&adopted.registry).await;
    assert_ne!(reported, generic);

    // The uninstall's refresh resolves, then parks. The agent ends meanwhile.
    let gate = arm_refresh_hold(&adopted.registry);
    let uninstall = tokio::spawn({
        let registry = adopted.registry.clone();
        let digest = package.digest.clone();
        async move { registry.package_uninstall(uninstall(&digest)).await }
    });
    gate.entered.notified().await;
    let mut sessions = adopted.registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(&created.id).expect("the live session");
    entry.active_agent = None;
    entry.info.active_agent = None;
    entry.info.active_agent_base = None;
    drop(sessions);
    gate.release.notify_one();
    uninstall
        .await
        .expect("uninstall task")
        .expect("uninstall the package");
    assert_eq!(
        running(&adopted.registry).await,
        generic,
        "a refresh that finds the agent ended sends the default configuration"
    );
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

/// Makes the session's active agent the runtime's own process as procwatch
/// observes it, with the configuration procwatch applies for it.
async fn observe_own_runtime_process(registry: &SessionRegistry, id: &protocol::SessionId) {
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(id).expect("the live session");
    entry.active_agent = Some(super::super::ActiveAgentReport {
        source: "pohunek:procwatch".to_owned(),
        agent: RuntimeId::CLAUDE.to_owned(),
        seq: None,
        pid: None,
        start_identity: None,
        reported_at: std::time::Instant::now(),
        activity_reported: false,
        observed: true,
    });
    entry.info.active_agent = Some(RuntimeId::CLAUDE.to_owned());
    entry.info.active_agent_base = Some(entry.info.agent_base.clone());
    // procwatch applies the observed agent's own configuration when it observes
    // it; the refresh must replace that configuration.
    crate::session::send_detector_config(entry, crate::detect::DetectorConfig::generic_shell());
    drop(sessions);
}

/// A live session of the built-in Claude runtime, adopted by the installed
/// package, with a host profile named like the runtime that carries its own
/// manifest override and is pinned to the package.
/// Launch keys of a session agent that sleeps until the test stops it.
const SLEEPING_LAUNCH: &str = "program = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n";

/// Adopts a session of profile [`PROFILE`], launched with `launch`, beside a
/// host profile named like its runtime that carries its own manifest.
async fn adopt_session_beside_a_profile_named_like_its_runtime(
    tag: &str,
    launch: &str,
) -> (Fixture, protocol::SessionId) {
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fs::create_dir_all(fixture.agents.join("manifests")).expect("the manifests directory");
    fs::write(
        fixture.agents.join("manifests").join("tuned.toml"),
        OVERRIDE_MANIFEST,
    )
    .expect("the override manifest");
    // A host profile named like the runtime, with its own detection manifest.
    fixture.write_profile(
        RuntimeId::CLAUDE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\nmanifest = \"tuned\"\n",
    );
    fixture.write_profile(PROFILE, &format!("base = \"claude\"\n{launch}"));
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(&adopted, &adopted.registry, RuntimeId::CLAUDE, &package, 5).await;
    adopted
        .registry
        .package_bind_profile(PackageBindProfileParams {
            profile: RuntimeId::CLAUDE.to_owned(),
            digest: None,
            dry_run: false,
        })
        .await
        .expect("pin the profile to the served package");

    (adopted, created.id)
}

/// Writes session `id`'s durable record as the daemon does after a change.
async fn persist_session(registry: &SessionRegistry, id: &protocol::SessionId) {
    let record = {
        let sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get(id).expect("the live session");
        SessionRegistry::session_record(id, entry, entry.desired_state, None)
    };
    registry
        .write_session_record(record)
        .await
        .expect("persist the session record");
}

/// Installs the next Claude version without selecting it: a package change
/// that the refresh sees while the session keeps its pin, to which the
/// profile of the runtime's name is bound.
async fn install_unselected_next_claude(fixture: &Fixture) {
    let next = claude_next_version_without_fork();
    let (package_id, _) = crate::agent::host::fixture::official_package_identity(RuntimeId::CLAUDE);
    let key = signing_key("root");
    let document = signed_by(
        &key,
        catalog(
            6,
            vec![entry(&next, &package_id, RuntimeId::CLAUDE, "1.0.2")],
        ),
    );
    let mut install = catalog_install(fixture, &next, &document, false);
    install.select = false;
    fixture
        .registry
        .package_install(install)
        .await
        .expect("install the unselected version");
}

/// The running detector configuration of session `id`.
async fn running_detector(registry: &SessionRegistry, id: &protocol::SessionId) -> String {
    let sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get(id).expect("the live session");
    let config = entry.detector_config.borrow().config.clone();
    detector_debug(&config)
}

/// Python agent that reports itself to its worker as the Claude agent once the
/// test releases its gate, releases that identity on the next release, and
/// then holds until the session stops. It is the session's root process, as a
/// directly launched agent is, so procwatch leaves its claim to the root's
/// exit.
const WORKER_REPORTED_CLAUDE: &str = r#"
import datetime
import time

gate = open(sys.argv[1], encoding="ascii")
failure_path = sys.argv[1] + ".failure"

def record_failure(kind, value, trace):
    # The test reads this file, so the reporter never dies without a reason.
    import traceback
    with open(failure_path, "w", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure
pid = os.getpid()
start_identity = process_start_identity(pid)

def claim(kind, sequence):
    return {
        "type": kind,
        "runtime_id": os.environ["POHUNEK_WORKER_INSTANCE_ID"],
        "provider": "claude",
        "pid": pid,
        "start_identity": start_identity,
        "sequence": sequence,
    }

gate.readline()
# Worker claim sequences are the host CLOCK_MONOTONIC in nanoseconds.
sequence = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
report = claim("identity_report", sequence)
report["expires_at"] = (
    datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=int(sys.argv[2]))
).isoformat().replace("+00:00", "Z")
assert send(report) is True
gate.readline()
assert send(claim("identity_release", sequence + 1)) is True
gate.readline()
"#;

/// Lifetime of a claim that outlives every step of its scenario, inside the
/// worker's bound.
const LASTING_CLAIM_TTL_SECS: u32 = 55;

/// Lifetime of a claim the scenario lets expire in the worker. The scenario
/// waits for the daemon to project the claim first, which takes milliseconds.
const EXPIRING_CLAIM_TTL_SECS: u32 = 5;

/// A session of [`PROFILE`] running [`WORKER_REPORTED_CLAUDE`], beside a host
/// profile named like its runtime, with the reporter's gate and its claim
/// lifetime of `ttl_secs`.
struct WorkerReportedClaude {
    adopted: Fixture,
    id: protocol::SessionId,
    gate: fs::File,
    failure_path: PathBuf,
    /// The running configuration of an explicit report of the profile named
    /// like the runtime: its manifest override.
    override_config: String,
    /// The session's own default configuration.
    default_config: String,
}

impl WorkerReportedClaude {
    async fn adopt(tag: &str, ttl_secs: u32) -> Self {
        let root = temp_dir(&format!("{tag}-agent"));
        let reporter = root.join("reporter.py");
        fs::write(
            &reporter,
            [WORKER_IDENTITY_HELPERS, WORKER_REPORTED_CLAUDE].concat(),
        )
        .expect("write the identity reporter");
        let gate_path = root.join("reporter.gate");
        let gate = hook_gate(&gate_path);
        let (adopted, id) = adopt_session_beside_a_profile_named_like_its_runtime(
            tag,
            &format!(
                "program = \"python3\"\nargs = [\"{}\", \"{}\", \"{ttl_secs}\"]\n",
                reporter.display(),
                gate_path.display()
            ),
        )
        .await;
        let (override_config, default_config) = {
            let sessions = adopted.registry.inner.sessions.lock().await;
            let entry = sessions.get(&id).expect("the live session");
            let pinned = entry.pinned.clone().expect("the session's pin is served");
            let shadow = adopted
                .registry
                .inner
                .profiles
                .resolve_agent_pinned(RuntimeId::CLAUDE, &pinned)
                .expect("the profile resolves through the pinned definition");
            (
                detector_debug(&crate::session::detector_config_for_resolved_agent(&shadow)),
                detector_debug(&entry.default_detector_config),
            )
        };
        assert_ne!(
            override_config, default_config,
            "the profile's manifest override is not the session's default detector"
        );
        Self {
            adopted,
            id,
            gate,
            failure_path: PathBuf::from(format!("{}.failure", gate_path.display())),
            override_config,
            default_config,
        }
    }

    /// Lets the reporter take its next step.
    fn release(&mut self) {
        self.gate
            .write_all(b"next\n")
            .expect("release the reporter's next step");
    }

    /// Waits until the session's active agent is `agent` and its detector runs
    /// `config`.
    async fn wait_for(&self, what: &str, agent: Option<&str>, config: &str) {
        let registry = &self.adopted.registry;
        wait_until(what, || async {
            if let Ok(failure) = fs::read_to_string(&self.failure_path) {
                panic!("the identity reporter failed: {failure}");
            }
            let info = registry.inspect(&self.id).await.expect("inspect");
            (info.active_agent.as_deref() == agent
                && running_detector(registry, &self.id).await == config)
                .then_some(())
        })
        .await;
    }

    async fn stop(self) {
        self.adopted
            .registry
            .stop(&self.id)
            .await
            .expect("stop the adopted session");
        self.adopted
            .registry
            .remove(&self.id)
            .await
            .expect("remove the adopted session");
    }
}

#[tokio::test]
async fn a_worker_reported_agent_runs_its_detector_until_its_release() {
    let mut session =
        WorkerReportedClaude::adopt("claude-worker-reported-live", LASTING_CLAIM_TTL_SECS).await;
    session.release();
    let override_config = session.override_config.clone();
    session
        .wait_for(
            "the worker report to run the reported profile's override",
            Some(RuntimeId::CLAUDE),
            &override_config,
        )
        .await;
    session.release();
    let default_config = session.default_config.clone();
    session
        .wait_for(
            "the worker release to restore the session's default detector",
            None,
            &default_config,
        )
        .await;
    session.stop().await;
}

#[tokio::test]
async fn a_worker_report_projected_across_an_uninstall_runs_the_current_pins_detector() {
    let mut session =
        WorkerReportedClaude::adopt("claude-worker-reported-uninstall", LASTING_CLAIM_TTL_SECS)
            .await;
    let registry = session.adopted.registry.clone();
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: std::sync::Arc::new(tokio::sync::Notify::new()),
        release: std::sync::Arc::new(tokio::sync::Notify::new()),
    };
    *registry
        .inner
        .metadata_apply_hold
        .lock()
        .expect("metadata apply hold is never poisoned") = Some(gate.clone());

    // The projection resolves the reported profile against the package's
    // definition, and the package is uninstalled before it commits.
    session.release();
    gate.entered.notified().await;
    // The profile is unbound from the package so the package can go.
    session.adopted.write_profile(
        RuntimeId::CLAUDE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\nmanifest = \"tuned\"\n",
    );
    let package = real_official(RuntimeId::CLAUDE);
    registry
        .package_uninstall(uninstall(&package.digest))
        .await
        .expect("uninstall the package");
    gate.release.notify_one();

    wait_until("the projected report to reach the session", || async {
        (registry
            .inspect(&session.id)
            .await
            .expect("inspect")
            .active_agent
            .as_deref()
            == Some(RuntimeId::CLAUDE))
        .then_some(())
    })
    .await;
    let expected = {
        let sessions = registry.inner.sessions.lock().await;
        let entry = sessions.get(&session.id).expect("the live session");
        entry
            .pinned
            .clone()
            .and_then(|pinned| {
                registry
                    .inner
                    .profiles
                    .resolve_agent_pinned(RuntimeId::CLAUDE, &pinned)
                    .ok()
            })
            .map_or_else(
                || detector_debug(&entry.default_detector_config),
                |resolved| {
                    detector_debug(&crate::session::detector_config_for_resolved_agent(
                        &resolved,
                    ))
                },
            )
    };
    assert_ne!(
        expected, session.override_config,
        "the uninstall changes what the reported profile resolves to"
    );
    assert_eq!(
        running_detector(&registry, &session.id).await,
        expected,
        "a projection resolved before the uninstall runs the current pin's detector"
    );
    session.stop().await;
}

#[tokio::test]
async fn a_worker_reported_profile_named_like_its_runtime_keeps_its_override_through_a_restart() {
    let mut session =
        WorkerReportedClaude::adopt("claude-worker-reported-restart", EXPIRING_CLAIM_TTL_SECS)
            .await;
    session.release();
    let override_config = session.override_config.clone();
    session
        .wait_for(
            "the worker report to run the reported profile's override",
            Some(RuntimeId::CLAUDE),
            &override_config,
        )
        .await;
    // Once the claim expired, the worker no longer reports it, and a restart
    // restores the active agent from the session's durable record alone.
    let worker = {
        let sessions = session.adopted.registry.inner.sessions.lock().await;
        let entry = sessions.get(&session.id).expect("the live session");
        let crate::session::RuntimeHandle::Worker(worker) = &entry.runtime else {
            panic!("the session runs on a worker");
        };
        worker.clone()
    };
    wait_until("the worker to drop the expired claim", || async {
        worker
            .inspect()
            .await
            .expect("inspect the worker")
            .active_identity
            .is_none()
            .then_some(())
    })
    .await;

    let adopted = session
        .adopted
        .reopen_with_workers_of(&session.adopted.registry)
        .await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    assert_eq!(
        adopted
            .registry
            .inspect(&session.id)
            .await
            .expect("inspect")
            .active_agent
            .as_deref(),
        Some(RuntimeId::CLAUDE),
        "the durable record restores the reported agent"
    );
    assert_eq!(
        running_detector(&adopted.registry, &session.id).await,
        override_config,
        "the reconciled session starts with the restored report's override"
    );
    install_unselected_next_claude(&adopted).await;
    assert_eq!(
        running_detector(&adopted.registry, &session.id).await,
        override_config,
        "a restored report keeps the profile's override through a package refresh"
    );
    session.adopted = adopted;
    session.stop().await;
}

/// Records a nested Codex agent as procwatch observes it in session `id`.
async fn observe_nested_codex(registry: &SessionRegistry, id: &protocol::SessionId) {
    let mut sessions = registry.inner.sessions.lock().await;
    let entry = sessions.get_mut(id).expect("the live session");
    entry.active_agent = Some(super::super::ActiveAgentReport {
        source: "pohunek:procwatch".to_owned(),
        agent: RuntimeId::CODEX.to_owned(),
        seq: None,
        pid: None,
        start_identity: None,
        reported_at: std::time::Instant::now(),
        activity_reported: false,
        observed: true,
    });
    entry.info.active_agent = Some(RuntimeId::CODEX.to_owned());
    entry.info.active_agent_base = Some(protocol::RuntimeRef::codex());
}

/// Rewrites session `id`'s durable record without its active agent's origin.
fn forget_active_agent_origin(store_path: &Path, id: &protocol::SessionId) {
    let store = crate::store::Store::new(store_path.to_path_buf());
    let mut record = store
        .load_sessions()
        .expect("durable sessions")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("the session record");
    record.active_agent_origin = None;
    store
        .record_session(&record)
        .expect("write the record without an origin");
}

#[tokio::test]
async fn an_observed_nested_agent_in_a_record_without_an_origin_is_restored_as_observed() {
    let tag = "codex-nested-origin";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fs::create_dir_all(fixture.agents.join("manifests")).expect("the manifests directory");
    fs::write(
        fixture.agents.join("manifests").join("tuned.toml"),
        OVERRIDE_MANIFEST,
    )
    .expect("the override manifest");
    // A host profile named like the nested runtime, with its own manifest.
    fixture.write_profile(
        RuntimeId::CODEX,
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\nmanifest = \"tuned\"\n",
    );
    fixture.write_profile(
        PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the built-in Claude runtime");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    install_real_official(
        &adopted,
        &adopted.registry,
        RuntimeId::CLAUDE,
        &real_official(RuntimeId::CLAUDE),
        5,
    )
    .await;
    install_real_official(
        &adopted,
        &adopted.registry,
        RuntimeId::CODEX,
        &real_official(RuntimeId::CODEX),
        6,
    )
    .await;
    adopted
        .registry
        .package_bind_profile(PackageBindProfileParams {
            profile: RuntimeId::CODEX.to_owned(),
            digest: None,
            dry_run: false,
        })
        .await
        .expect("pin the codex profile to the served package");
    let codex = adopted
        .registry
        .inner
        .profiles
        .runtimes()
        .resolve_id(&RuntimeId::parse(RuntimeId::CODEX).expect("runtime id"))
        .expect("the package serves codex");
    let observed_config = detector_debug(&crate::detect::DetectorConfig::for_definition(&codex));
    let profile_config = crate::session::detector_config_for_resolved_agent(
        &adopted
            .registry
            .inner
            .profiles
            .resolve_agent(RuntimeId::CODEX)
            .expect("the codex profile resolves"),
    );
    assert_ne!(detector_debug(&profile_config), observed_config);

    // procwatch observed a nested Codex agent; the record keeps no origin, as a
    // record before schema 10 does.
    observe_nested_codex(&adopted.registry, &created.id).await;
    persist_session(&adopted.registry, &created.id).await;
    forget_active_agent_origin(&store_path, &created.id);

    let adopted = adopted.reopen_with_workers_of(&adopted.registry).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    assert_eq!(
        running_detector(&adopted.registry, &created.id).await,
        observed_config,
        "an agent named like its own runtime is restored as observed, not as the profile of its name"
    );
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

#[tokio::test]
async fn a_procwatch_observed_own_agent_is_not_shadowed_by_a_profile_named_like_its_runtime() {
    let (adopted, created_id) = adopt_session_beside_a_profile_named_like_its_runtime(
        "claude-observed-shadow",
        SLEEPING_LAUNCH,
    )
    .await;
    observe_own_runtime_process(&adopted.registry, &created_id).await;
    // The daemon restarts from its persisted record.
    persist_session(&adopted.registry, &created_id).await;
    let adopted = adopted.reopen_with_workers_of(&adopted.registry).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");

    install_unselected_next_claude(&adopted).await;
    let served = adopted
        .registry
        .inner
        .profiles
        .runtimes()
        .resolve_id(&RuntimeId::parse(RuntimeId::CLAUDE).expect("runtime id"))
        .expect("the package serves claude");
    // The profile of the runtime's name resolves through the pinned definition,
    // so an explicit report of that name would run its manifest override.
    let pinned_shadow = adopted
        .registry
        .inner
        .profiles
        .resolve_agent_pinned(RuntimeId::CLAUDE, &served)
        .expect("the profile resolves through the pinned definition");
    assert!(
        pinned_shadow
            .profile
            .as_ref()
            .is_some_and(|profile| profile.manifest.is_some()),
        "the profile carries its manifest override"
    );
    let shadow = crate::session::detector_config_for_resolved_agent(
        &adopted
            .registry
            .inner
            .profiles
            .resolve_agent(RuntimeId::CLAUDE)
            .expect("the profile resolves"),
    );
    let running = {
        let sessions = adopted.registry.inner.sessions.lock().await;
        let entry = sessions.get(&created_id).expect("the live session");
        let config = entry.detector_config.borrow().config.clone();
        detector_debug(&config)
    };
    assert_ne!(
        detector_debug(&shadow),
        detector_debug(&crate::detect::DetectorConfig::generic_shell())
    );
    assert_eq!(
        running,
        detector_debug(&crate::detect::DetectorConfig::for_definition(&served)),
        "the observed agent runs the pinned definition, not the profile of its name"
    );
    adopted
        .registry
        .stop(&created_id)
        .await
        .expect("stop the adopted session");
    adopted
        .registry
        .remove(&created_id)
        .await
        .expect("remove the adopted session");
}

#[tokio::test]
async fn an_explicit_report_of_a_differently_named_profile_keeps_its_override_through_a_restart() {
    let tag = "claude-tuned-restart";
    let key = signing_key("root");
    let dir = temp_dir(tag);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fs::create_dir_all(fixture.agents.join("manifests")).expect("the manifests directory");
    fs::write(
        fixture.agents.join("manifests").join("tuned.toml"),
        OVERRIDE_MANIFEST,
    )
    .expect("the override manifest");
    fixture.write_profile(
        TUNED_PROFILE,
        "base = \"claude\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 600\"]\nmanifest = \"tuned\"\n",
    );
    let previous = compiled_in_daemon(&fixture, &store_path);
    let created = previous
        .create(protocol::SessionNewParams {
            agent: TUNED_PROFILE.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("a session of the tuned profile");
    let adopted = fixture.reopen_with_workers_of(&previous).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    install_real_official(
        &adopted,
        &adopted.registry,
        RuntimeId::CLAUDE,
        &real_official(RuntimeId::CLAUDE),
        5,
    )
    .await;
    let override_config = running_detector(&adopted.registry, &created.id).await;
    assert!(
        adopted
            .registry
            .report_agent(protocol::SessionReportAgentParams {
                session_id: created.id.clone(),
                source: "pohunek:tuned".to_owned(),
                agent: TUNED_PROFILE.to_owned(),
                activity: None,
                seq: None,
                pid: None,
                agent_session_id: None,
                agent_session_path: None,
            })
            .await
            .recorded,
        "the explicit report of the tuned profile is recorded"
    );
    persist_session(&adopted.registry, &created.id).await;
    let adopted = adopted.reopen_with_workers_of(&adopted.registry).await;
    Box::pin(adopted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    assert_eq!(
        running_detector(&adopted.registry, &created.id).await,
        override_config,
        "the reconciled session starts with the restored report's override"
    );

    install_version(
        &adopted,
        &adopted.registry,
        RuntimeId::CLAUDE,
        &claude_next_version_without_fork(),
        "1.0.2",
        6,
    )
    .await;
    assert_eq!(
        running_detector(&adopted.registry, &created.id).await,
        override_config,
        "the explicit report of a differently named profile keeps its manifest override"
    );
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

/// Binds the profile over the built-in Claude runtime to the package that now
/// serves it, the way `plugin profile migrate` does.
async fn bind_claude_profile(fixture: &Fixture) {
    fixture
        .registry
        .package_bind_profile(PackageBindProfileParams {
            profile: PROFILE.to_owned(),
            digest: None,
            dry_run: false,
        })
        .await
        .expect("bind the profile to the official package");
}

/// The error code of a resume of `id` that keeps the profile's change refused.
async fn refused_resume_code(registry: &SessionRegistry, id: &protocol::SessionId) -> String {
    registry
        .resume_with(id, crate::session::ProfileChange::Refuse)
        .await
        .expect_err("a changed profile is refused")
        .code
}

#[tokio::test]
async fn a_profile_bound_to_the_adopting_package_resumes_without_accepting_a_change() {
    let session = stopped_built_in_claude("claude-bound-resume").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    bind_claude_profile(&session.fixture).await;
    let restarted = restart(&session.fixture).await;

    let resumed = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Refuse)
        .await
        .expect("the bound profile keeps the revision the session froze");
    assert_eq!(
        resumed.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Live)
    );
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

#[tokio::test]
async fn a_real_profile_edit_after_the_adoption_refuses_the_resume() {
    let session = stopped_built_in_claude("claude-edited-resume").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    bind_claude_profile(&session.fixture).await;
    let config_home = session.fixture.dir.join("claude-home");
    session.fixture.write_profile(
        PROFILE,
        &claude_profile_text(&config_home).replace("sleep 600", "sleep 601"),
    );
    let restarted = restart(&session.fixture).await;

    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        "agent_profile_changed"
    );
}

#[tokio::test]
async fn a_profile_pinned_to_another_package_id_is_refused_not_adopted() {
    let session = stopped_built_in_claude("claude-other-pin").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    let config_home = session.fixture.dir.join("claude-home");
    session.fixture.write_profile(
        PROFILE,
        &format!(
            "package = \"pohunek.runtime.other\"\ndigest = \"{}\"\n{}",
            package.digest.as_str(),
            claude_profile_text(&config_home)
        ),
    );
    let restarted = restart(&session.fixture).await;

    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        "agent_profile_changed"
    );
}

/// The verdict the upgrade preflight gives the stopped session `id` over the
/// state `fixture` holds, and the refusal code of that verdict.
fn stopped_preflight_verdict(
    fixture: &Fixture,
    store_path: &Path,
    id: &protocol::SessionId,
) -> (Verdict, String) {
    stopped_preflight_verdict_entered(fixture, store_path, id, std::time::Instant::now())
}

/// [`stopped_preflight_verdict`] for a command that entered at `entry`.
fn stopped_preflight_verdict_entered(
    fixture: &Fixture,
    store_path: &Path,
    id: &protocol::SessionId,
    entry: std::time::Instant,
) -> (Verdict, String) {
    let report = crate::session::upgrade_preflight::run_with(
        &crate::session::upgrade_preflight::PreflightInputs {
            store_path: store_path.to_path_buf(),
            worker_state_root: fixture.dir.join("no-workers"),
            plugins_dir: fixture.plugins.clone(),
        },
        &crate::session::upgrade_preflight::PreflightOptions {
            probe_budget: PROBE_BUDGET,
            entry,
            release: None,
            profiles_dir: Some(fixture.agents.clone()),
            host_state_dir: Some(fixture.dir.join("state")),
            // The environment the test registry launches under, so the judgement reads the
            // same home and variables that resume does.
            launch: Ok(crate::runtime::LaunchEnvironment::new(
                pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
                    .iter()
                    .map(|pattern| (*pattern).to_owned())
                    .collect(),
                crate::test_support::thread_environment_source(),
            )),
        },
        &crate::procwatch::HostInspector::new(),
    );
    let verdict = report
        .stopped_sessions
        .iter()
        .find(|session| session.session_id == id.0)
        .unwrap_or_else(|| {
            panic!(
                "the stopped session is judged: {:?}",
                report.stopped_sessions
            )
        });
    (verdict.verdict, verdict.code.clone())
}

#[tokio::test]
async fn a_stopped_session_whose_profile_was_removed_is_judged_as_resume_refuses_it() {
    let session = stopped_built_in_claude("preflight-removed-profile").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    bind_claude_profile(&session.fixture).await;
    fs::remove_file(session.fixture.agents.join(format!("{PROFILE}.toml")))
        .expect("remove the profile");

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;

    assert_eq!(verdict, Verdict::WouldLoseRecovery);
    assert_eq!(code, "agent_profile_missing");
    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        code
    );
}

#[tokio::test]
async fn a_stopped_session_whose_profile_changed_is_judged_as_resume_refuses_it() {
    let session = stopped_built_in_claude("preflight-edited-profile").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    bind_claude_profile(&session.fixture).await;
    let config_home = session.fixture.dir.join("claude-home");
    session.fixture.write_profile(
        PROFILE,
        &claude_profile_text(&config_home).replace("sleep 600", "sleep 601"),
    );

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;

    assert_eq!(verdict, Verdict::WouldLoseRecovery);
    assert_eq!(code, "agent_profile_changed");
    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        code
    );
}

/// A stopped Claude session that the official package has adopted and that
/// resumed through it once, so its launch pin names the package rather than
/// the built-in runtime: the upgrade leaves its runtime source unchanged.
async fn stopped_pinned_claude(tag: &str) -> StoppedClaude {
    let session = stopped_built_in_claude(tag).await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    bind_claude_profile(&session.fixture).await;
    let restarted = restart(&session.fixture).await;
    restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Refuse)
        .await
        .expect("the first resume adopts and pins the package");
    restarted
        .registry
        .stop(&session.id)
        .await
        .expect("stop the resumed session");
    StoppedClaude {
        fixture: restarted,
        store_path: session.store_path,
        id: session.id,
    }
}

#[tokio::test]
async fn a_stopped_session_whose_conversation_is_missing_is_judged_as_resume_refuses_it() {
    let session = stopped_pinned_claude("preflight-missing-conversation").await;
    let conversation = session
        .fixture
        .dir
        .join("claude-home")
        .join("projects")
        .join("work")
        .join(format!("{CONVERSATION}.jsonl"));
    fs::remove_file(&conversation).expect("remove the conversation");

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;

    assert_eq!(verdict, Verdict::AlreadyNotResumable);
    assert_eq!(code, "agent_native_reference_missing");
    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        code
    );
}

#[tokio::test]
async fn a_built_in_session_whose_conversation_is_missing_is_judged_lost_when_the_upgrade_adopts_it(
) {
    let session = stopped_built_in_claude("preflight-adopted-missing-conversation").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    fs::remove_file(
        session
            .fixture
            .dir
            .join("claude-home")
            .join("projects")
            .join("work")
            .join(format!("{CONVERSATION}.jsonl")),
    )
    .expect("remove the conversation");

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;

    assert_eq!(verdict, Verdict::WouldLoseRecovery);
    assert_eq!(code, "agent_native_reference_missing");
    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        code
    );
}

#[tokio::test]
async fn a_stopped_session_whose_conversation_exists_stays_adoptable_as_resume_accepts_it() {
    let session = stopped_pinned_claude("preflight-present-conversation").await;

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;
    let resumed = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Refuse)
        .await
        .expect("resume accepts the present conversation");

    assert_eq!(verdict, Verdict::Adoptable);
    assert_eq!(code, pohunek_service_config::preflight::CODE_ADOPTABLE);
    assert_eq!(
        resumed.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Live)
    );
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

#[tokio::test]
async fn a_profile_changed_only_by_the_migration_pin_is_judged_adoptable_as_resume_accepts_it() {
    let session = stopped_built_in_claude("preflight-migrated-profile").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    bind_claude_profile(&session.fixture).await;

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;
    let resumed = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Refuse)
        .await
        .expect("resume accepts the migrated profile");

    assert_eq!(verdict, Verdict::Adoptable);
    assert_eq!(code, pohunek_service_config::preflight::CODE_ADOPTABLE);
    assert_eq!(
        resumed.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Live)
    );
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

/// A stopped session of the built-in Claude runtime, whose package declares no
/// version probe, relaunched as the base kind with no profile from `program`.
async fn stopped_probeless_claude(
    tag: &str,
    program: impl FnOnce(&Fixture) -> String,
) -> StoppedClaude {
    let session = stopped_built_in_claude(tag).await;
    let program = program(&session.fixture);
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    // A base-kind launch with no profile, which launches `program`. The
    // session record, its embedded recovery and its resume projection all name
    // the base kind, so the store stays consistent.
    let store = crate::store::Store::new(session.store_path.clone());
    let mut record = store
        .load_sessions()
        .expect("durable sessions")
        .into_iter()
        .find(|record| record.session_id == session.id.0)
        .expect("the session record");
    record.info.agent = RuntimeId::CLAUDE.to_owned();
    record.info.agent_base = protocol::RuntimeRef::claude();
    let recovery = record.recovery.as_mut().expect("a recovery binding");
    recovery.agent = RuntimeId::CLAUDE.to_owned();
    recovery.agent_base = protocol::RuntimeRef::claude();
    recovery.profile_revision = None;
    recovery.program = program.clone();
    store
        .record_session(&record)
        .expect("write the session record");
    if let Some(mut projection) = store
        .load_resume()
        .expect("durable resume projections")
        .into_iter()
        .find(|binding| binding.session_id == session.id.0)
    {
        projection.agent = RuntimeId::CLAUDE.to_owned();
        projection.agent_base = protocol::RuntimeRef::claude();
        projection.profile_revision = None;
        projection.program = program;
        store
            .record_resume(&projection)
            .expect("write the resume projection");
    }
    session
}

#[tokio::test]
async fn a_stopped_session_of_a_runtime_without_a_probe_whose_executable_is_gone_is_judged_as_resume_refuses_it(
) {
    let session = stopped_probeless_claude("preflight-missing-executable", |fixture| {
        fixture
            .dir
            .join("no-such-agent-executable")
            .to_string_lossy()
            .into_owned()
    })
    .await;
    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;

    assert_eq!(verdict, Verdict::WouldLoseRecovery);
    assert_eq!(code, "agent_binary_missing");
    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        code
    );
}

/// A directory of `path` holding `agent` as an executable, with `mode` on the
/// directory: the resolver trusts a file only from a directory no other user can
/// write.
fn agent_directory(fixture: &Fixture, name: &str, mode: u32, agent: &str) -> PathBuf {
    let directory = fixture.dir.join(name);
    fs::create_dir_all(&directory).expect("the agent directory");
    fs::set_permissions(&directory, fs::Permissions::from_mode(mode))
        .expect("the agent directory's mode");
    let executable = directory.join(agent);
    fs::write(&executable, "#!/bin/sh\nexit 1\n").expect("the agent executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("the agent's mode");
    directory
}

#[tokio::test]
async fn an_agent_in_an_untrusted_directory_is_refused_by_resume_and_the_preflight_alike() {
    const AGENT: &str = "agent-untrusted-directory";
    let session = stopped_probeless_claude("preflight-untrusted-directory", |fixture| {
        // The only copy sits in a directory another user can write.
        agent_directory(fixture, "untrusted-directory", 0o777, AGENT)
            .join(AGENT)
            .to_string_lossy()
            .into_owned()
    })
    .await;

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;

    assert_eq!(verdict, Verdict::WouldLoseRecovery);
    assert_eq!(code, "agent_binary_missing");
    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        code
    );
}

#[tokio::test]
async fn a_trusted_copy_of_the_agent_is_found_by_the_resolver_and_by_the_preflight_alike() {
    const AGENT: &str = "agent-trusted-directory";
    let session = stopped_probeless_claude("preflight-trusted-directory", |fixture| {
        agent_directory(fixture, "trusted-directory", 0o755, AGENT)
            .join(AGENT)
            .to_string_lossy()
            .into_owned()
    })
    .await;

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;
    // Both sides resolve the trusted copy. Resume then stops at the native
    // reference, which a base-kind launch without a declared store cannot
    // satisfy; the preflight does not judge references, so only the resolver
    // is compared here.
    let resumed = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Refuse)
        .await
        .expect_err("resume stops at the native reference");

    assert_ne!(
        code, "agent_binary_missing",
        "the preflight finds the trusted copy: {verdict:?}"
    );
    assert_ne!(
        resumed.code, "agent_binary_missing",
        "resume finds the trusted copy"
    );
}

#[tokio::test]
async fn an_agent_only_in_an_untrusted_path_directory_is_refused_by_resume_and_the_preflight_alike()
{
    const AGENT: &str = "agent-untrusted-path";
    // The resolver reads the process `PATH` and the preflight its subprocess
    // environment, so the test sets `PATH` in a process of its own.
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let session = stopped_probeless_claude("preflight-untrusted-path", |_| AGENT.to_owned()).await;
    // The only copy sits in a directory another user can write.
    let untrusted = agent_directory(&session.fixture, "untrusted-path", 0o777, AGENT);
    let mut environment = pohunek_test_support::process_env::ProcessEnv::lock();
    environment.set("PATH", &untrusted);

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;

    assert_eq!(verdict, Verdict::WouldLoseRecovery);
    assert_eq!(code, "agent_binary_missing");
    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        code
    );
}

#[tokio::test]
async fn a_trusted_copy_later_on_the_path_is_found_by_the_resolver_and_by_the_preflight_alike() {
    const AGENT: &str = "agent-trusted-later";
    // As above, `PATH` and `HOME` change only in a process of the test's own.
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let session = stopped_probeless_claude("preflight-trusted-later", |_| AGENT.to_owned()).await;
    let untrusted = agent_directory(&session.fixture, "untrusted-first", 0o777, AGENT);
    let trusted = agent_directory(&session.fixture, "trusted-later", 0o755, AGENT);
    // The conversation the base-kind launch reads lives under the default Claude
    // home of `HOME`, which the test owns.
    let home = session.fixture.dir.join("home");
    let conversations = home.join(".claude").join("projects").join("work");
    fs::create_dir_all(&conversations).expect("the conversation directory");
    fs::write(conversations.join(format!("{CONVERSATION}.jsonl")), b"{}\n")
        .expect("the conversation");
    let mut environment = pohunek_test_support::process_env::ProcessEnv::lock();
    environment.set(
        "PATH",
        std::env::join_paths([&untrusted, &trusted]).expect("the PATH"),
    );
    environment.set("HOME", &home);

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;
    // Both sides skip the untrusted first copy and resolve the trusted one.
    // Resume then stops at the native reference, which a base-kind launch
    // without a declared store cannot satisfy; the preflight does not judge
    // references, so only the resolver is compared here.
    let resumed = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Refuse)
        .await
        .expect_err("resume stops at the native reference");

    assert_ne!(
        code, "agent_binary_missing",
        "the preflight finds the trusted copy: {verdict:?}"
    );
    assert_ne!(
        resumed.code, "agent_binary_missing",
        "resume finds the trusted copy"
    );
}

/// The entry of a command whose probe budget is already spent: the whole budget
/// passed before the judgement started.
fn spent_budget_entry() -> std::time::Instant {
    std::time::Instant::now()
        .checked_sub(PROBE_BUDGET)
        .expect("the monotonic clock is older than one probe budget")
}

#[tokio::test]
async fn a_spent_probe_budget_resolves_a_runtime_without_a_probe_and_judges_its_reference() {
    // The package declares no version probe, so resolving the executable is the
    // whole check, and it spends none of the budget.
    const AGENT: &str = "agent-probeless-resolves";
    let session = stopped_probeless_claude("preflight-spent-budget-resolves", |fixture| {
        agent_directory(fixture, "probeless-directory", 0o755, AGENT)
            .join(AGENT)
            .to_string_lossy()
            .into_owned()
    })
    .await;

    let (verdict, code) = stopped_preflight_verdict_entered(
        &session.fixture,
        &session.store_path,
        &session.id,
        spent_budget_entry(),
    );
    let restarted = restart(&session.fixture).await;
    let resumed = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Refuse)
        .await
        .expect_err("resume stops at the native reference");

    // The executable resolves without a probe, so the judgement reaches the
    // reference check, which refuses the conversation resume cannot find.
    assert_eq!(verdict, Verdict::WouldLoseRecovery);
    assert_eq!(code, "agent_native_reference_missing");
    assert_eq!(code, resumed.code);
}

#[tokio::test]
async fn a_spent_probe_budget_reports_a_missing_runtime_without_a_probe_as_missing() {
    let session = stopped_probeless_claude("preflight-spent-budget-missing", |fixture| {
        fixture
            .dir
            .join("agent-probeless-absent")
            .to_string_lossy()
            .into_owned()
    })
    .await;

    let (verdict, code) = stopped_preflight_verdict_entered(
        &session.fixture,
        &session.store_path,
        &session.id,
        spent_budget_entry(),
    );
    let restarted = restart(&session.fixture).await;

    assert_eq!(verdict, Verdict::WouldLoseRecovery);
    assert_eq!(code, "agent_binary_missing");
    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        code
    );
}

#[tokio::test]
async fn a_stopped_session_whose_resume_projection_contradicts_its_record_is_judged_as_resume_runs_it(
) {
    let session = stopped_built_in_claude("preflight-contradicting-projection").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    bind_claude_profile(&session.fixture).await;
    // The resume projection now names another agent than the record. A stopped
    // session keeps its own recovery when the projection contradicts it.
    let store = crate::store::Store::new(session.store_path.clone());
    let mut projection = durable_recovery(&session.store_path, &session.id);
    projection.agent = "contradicting-agent".to_owned();
    store
        .record_resume(&projection)
        .expect("write the contradicting projection");

    let (verdict, code) =
        stopped_preflight_verdict(&session.fixture, &session.store_path, &session.id);
    let restarted = restart(&session.fixture).await;
    let resumed = restarted
        .registry
        .resume_with(&session.id, crate::session::ProfileChange::Refuse)
        .await
        .expect("resume uses the session's own recovery");

    assert_eq!(verdict, Verdict::Adoptable, "{code}");
    assert_eq!(code, pohunek_service_config::preflight::CODE_ADOPTABLE);
    assert_eq!(
        resumed.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Live)
    );
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

#[tokio::test]
async fn a_frozen_revision_keyed_by_another_host_is_not_accepted_for_an_adopted_profile() {
    let session = stopped_built_in_claude("claude-forged-revision").await;
    let package = real_official(RuntimeId::CLAUDE);
    install_real_official(
        &session.fixture,
        &session.fixture.registry,
        RuntimeId::CLAUDE,
        &package,
        5,
    )
    .await;
    bind_claude_profile(&session.fixture).await;
    let frozen = durable_recovery(&session.store_path, &session.id);
    let stored = frozen
        .launch_binding
        .binding()
        .expect("the built-in pin")
        .clone();
    let config_home = session.fixture.dir.join("claude-home");
    let inputs = crate::agent::host::ProfileInputs::of(
        &claude_profile_text(&config_home),
        None,
        &serde_json::to_vec(&stored).expect("the stored binding"),
        &frozen.program,
        &frozen.args,
    )
    .expect("the profile inputs");
    // A key the host never used; its MAC must not verify under the host key.
    let other_host = crate::agent::host::RevisionKeys::new(Some(temp_dir("claude-other-host")));
    let forged = other_host
        .revision(&inputs)
        .expect("a revision under another key");
    edit_durable_recovery(&session.store_path, &session.id, |binding| {
        binding.profile_revision = Some(forged);
    });
    let restarted = restart(&session.fixture).await;

    assert_eq!(
        refused_resume_code(&restarted.registry, &session.id).await,
        "agent_profile_changed"
    );
}

/// Writes the damaged v0.33.1 fixture as a stopped session of profile `official`
/// whose runtime has ended, then migrates it as the head daemon does at startup.
/// Its binding needs the runtime registry to complete, so the migration leaves
/// the resume and fork flags false.
fn write_damaged_profile_store(store_path: &std::path::Path) {
    let damaged = include_str!("../../store/fixtures/v0.33.1-damaged/metadata.jsonl");
    let mut body = String::new();
    for line in damaged.lines() {
        let mut value: serde_json::Value = serde_json::from_str(line).expect("the fixture line");
        if value["kind"] == "session" {
            value["runtime"] = serde_json::json!({"state": "lost"});
            value["desired_state"] = serde_json::json!("stopped");
            value["info"]["agent"] = serde_json::json!("official");
            value["recovery"]["agent"] = serde_json::json!("official");
        }
        writeln!(body, "{value}").expect("write to a string");
    }
    std::fs::write(store_path, body).expect("write the store");
    std::fs::set_permissions(store_path, std::fs::Permissions::from_mode(0o600))
        .expect("secure the store");
    crate::store::Store::new(store_path.to_path_buf())
        .migrate_to_current()
        .expect("migrate the legacy store");
}

#[tokio::test]
async fn a_damaged_profile_session_the_head_cannot_repair_is_reported_as_a_loss() {
    damaged_profile_session_loss(false).await;
}

#[tokio::test]
async fn a_damaged_profile_session_the_head_cannot_repair_is_reported_as_a_loss_with_the_package() {
    damaged_profile_session_loss(true).await;
}

/// The previous release, with its built-in runtimes compiled in, resumes and forks
/// the session once its startup repairs the binding. The head cannot repair it,
/// so the preflight must report the loss on the store as it stands before the
/// upgrade, not only on the store after the previous release touched it.
async fn damaged_profile_session_loss(installed: bool) {
    let key = signing_key("root");
    let tag = format!("damaged-profile-loss-{installed}");
    let store_path = temp_store_path(&tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store_path.clone(),
    );
    fixture.write_profile("official", "base = \"claude\"\n");
    write_damaged_profile_store(&store_path);
    let id = protocol::SessionId("s-fixture-session".to_owned());
    let at_rest = crate::store::Store::new(store_path.clone())
        .load_sessions()
        .expect("load the store at rest")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("the session at rest");
    assert_eq!(
        at_rest.info.capabilities,
        protocol::SessionCapabilities {
            resume: false,
            fork: false
        },
        "the migration leaves the unresolved binding's flags false"
    );

    let previous_store = temp_store_path(&format!("{tag}-previous"));
    std::fs::copy(&store_path, &previous_store).expect("copy the store for the previous release");
    let previous = compiled_in_daemon(&fixture, &previous_store);
    Box::pin(previous.reconcile_workers())
        .await
        .expect("the previous release reconciles the store");
    assert_eq!(
        listed_recovery(&previous, &id).await,
        (true, true),
        "the previous release resumes and forks the session"
    );

    if installed {
        let package = real_official(RuntimeId::CLAUDE);
        install_real_official(&fixture, &fixture.registry, RuntimeId::CLAUDE, &package, 5).await;
    }
    let inputs = preflight_inputs(&fixture, &fixture.registry, &store_path);
    let report = upgrade_preflight::run(&inputs, &HostInspector::new());
    let verdict = report
        .stopped_sessions
        .iter()
        .find(|verdict| verdict.session_id == id.0);
    assert!(
        matches!(verdict, Some(verdict) if verdict.verdict == Verdict::WouldLoseRecovery),
        "the preflight reports the recovery the head cannot restore: {:?}",
        report.stopped_sessions
    );

    let restarted = restart(&fixture).await;
    let info = restarted
        .registry
        .inspect(&id)
        .await
        .expect("the session is listed");
    assert!(
        info.warnings
            .iter()
            .any(|warning| warning.message.contains("native recovery is unavailable")),
        "startup reports the recovery as unavailable: {:?}",
        info.warnings
    );
    assert_eq!(
        restarted
            .registry
            .resume_with(&id, crate::session::ProfileChange::Refuse)
            .await
            .expect_err("resume refuses the session")
            .code,
        if installed {
            "runtime_served_by_package"
        } else {
            "runtime_builtin_removed"
        }
    );
}
