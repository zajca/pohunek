//! Official runtime aliases: a catalog-authorized package takes `codex`,
//! `claude` or `hermes` over from the built-in runtime, nobody else does, and
//! what a session launched from the built-in may do afterwards.
//!
//! The catalog is signed with a test key derived from a fixed label.

// Rust guideline compliant 2026-10-05

use std::fs;

use package::PackageDigest;
use protocol::{
    PackageBindProfileParams, PackageBindStatus, PackageErrorKind, PackageInstallStatus,
    PackageOrigin, PackageSetEnabledParams, PackageUninstallParams, RuntimeId,
};

use super::catalog_tests::{anchor, catalog, catalog_install, entry, signed_by, signing_key};
use super::test_fixture::{explicit, Fixture, Package, INERT_PROGRAM, RUNTIME};
use crate::agent::host::ServedBy;
use crate::session::tests::{durable_recovery, params, temp_dir, temp_store_path};

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

#[tokio::test]
async fn an_official_package_claims_a_reserved_alias_and_is_served() {
    let key = signing_key("root");
    for alias in ALIASES {
        let fixture =
            Fixture::with_anchor(&format!("alias-claim-{alias}"), anchor(&[&key], Vec::new()));
        let package = official(alias);
        assert!(is_builtin(&fixture, alias), "{alias} starts built-in");
        assert_eq!(fixture.serving(alias), None);

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
        for other in ALIASES.into_iter().filter(|other| *other != alias) {
            assert!(is_builtin(&fixture, other), "{other} is untouched");
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
        assert!(is_builtin(&fixture, RuntimeId::CODEX), "{name}");
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
        assert!(is_builtin(&fixture, alias), "{alias} disabled");
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
        assert!(is_builtin(&fixture, alias), "{alias} uninstalled");
        assert_eq!(fixture.serving(alias), None);
        assert!(fixture
            .host
            .registry()
            .shadowed_builtin(&runtime_id(alias))
            .is_none());
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
    let package = official(RuntimeId::CODEX);
    let text = format!("base = \"{}\"\n", RuntimeId::CODEX);
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
    install_official(&fixture, &package, RuntimeId::CODEX, 5).await;

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
    fixture.write_profile(
        PROFILE,
        "base = \"codex\"\nprogram = \"/bin/sh\"\nargs = [\"-c\", \"sleep 30\"]\n",
    );
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

    let package = official(RuntimeId::CODEX);
    install_official(&fixture, &package, RuntimeId::CODEX, 5).await;
    let before = fs::read(&store_path).expect("read the store");

    let refused = fixture
        .registry
        .resume(&created.id)
        .await
        .expect_err("the package serves the runtime now");
    assert_eq!(refused.code, "runtime_served_by_package");
    assert!(refused
        .recover
        .as_deref()
        .is_some_and(|hint| hint.contains("uninstall")));
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
