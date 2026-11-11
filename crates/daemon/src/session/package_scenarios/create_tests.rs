//! Behavioural tests of `package.create_profile` over a real plugin root, real
//! archives, a real agents directory and a real session registry.

// Rust guideline compliant 2026-11-11

use std::fs;

use protocol::{
    PackageCreateProfileParams, PackageCreateProfileStatus, PackageErrorKind,
    PackageUninstallParams, RuntimeId,
};

use super::test_fixture::{explicit, Fixture, Package, RUNTIME};

/// Name of the profile the tests create.
const PROFILE: &str = "work";

/// Fresh fixtures one race test runs.
const RACE_ROUNDS: usize = 8;

fn create(base: &str) -> PackageCreateProfileParams {
    PackageCreateProfileParams {
        profile: PROFILE.to_owned(),
        base: RuntimeId::parse(base).expect("runtime id"),
        home: None,
        args: Vec::new(),
        secret_env: Vec::new(),
        replace: None,
        dry_run: false,
    }
}

async fn install(fixture: &Fixture, package: &Package) {
    let path = fixture.write_archive(package);
    fixture
        .registry
        .packages()
        .package_install(explicit(&path, package, true, true))
        .await
        .expect("install");
}

/// Creates the profile and uninstalls its target at once, in the order of
/// `create_first`, and asserts the invariant: the two never both succeed, and
/// a created profile never pins a package that is gone.
async fn race_once(tag: &str, create_first: bool) {
    let fixture = Fixture::new(tag);
    let package = Package::pi();
    install(&fixture, &package).await;

    let packages = fixture.registry.packages();
    let create_call = packages.package_create_profile(create(RUNTIME));
    let uninstall_call = packages.package_uninstall(PackageUninstallParams {
        digest: package.digest.clone(),
        remove_modified: false,
    });
    let (created, removed) = if create_first {
        tokio::join!(create_call, uninstall_call)
    } else {
        let (removed, created) = tokio::join!(uninstall_call, create_call);
        (created, removed)
    };

    match (&created, &removed) {
        (Ok(result), Err(error)) => {
            assert_eq!(result.status, PackageCreateProfileStatus::Created, "{tag}");
            assert_eq!(*error, PackageErrorKind::Referenced, "{tag}");
            let text = fs::read_to_string(fixture.agents.join(format!("{PROFILE}.toml")))
                .expect("the created profile");
            assert!(text.contains(package.digest.as_str()), "{tag}: {text}");
        }
        (Err(error), Ok(_)) => {
            assert_eq!(*error, PackageErrorKind::ProfileTargetInvalid, "{tag}");
            assert!(!fixture.agents.join(format!("{PROFILE}.toml")).exists());
        }
        (Ok(_), Ok(_)) => panic!("{tag}: the create and the uninstall both succeeded"),
        (Err(create_error), Err(remove_error)) => {
            panic!("{tag}: both refused: {create_error:?} / {remove_error:?}")
        }
    }
}

#[tokio::test]
async fn a_create_and_an_uninstall_of_the_target_never_both_succeed() {
    for round in 0..RACE_ROUNDS {
        race_once(&format!("create-race-create-first-{round}"), true).await;
        race_once(&format!("create-race-uninstall-first-{round}"), false).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_create_and_an_uninstall_racing_on_worker_threads_keep_the_invariant() {
    for round in 0..RACE_ROUNDS {
        race_once(&format!("create-race-threads-{round}"), round % 2 == 0).await;
    }
}
