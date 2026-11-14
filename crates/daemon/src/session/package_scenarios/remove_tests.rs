//! `package.remove_profile` for names that are runtimes, over a real plugin
//! root with an installed package and a real session registry (#2346 row 4).

// Rust guideline compliant 2026-11-14

use std::fs;

use protocol::{PackageErrorKind, PackageRemoveProfileParams, ProfileDigest};

use super::test_fixture::{explicit, Fixture, Package, RUNTIME};

fn remove(name: &str) -> PackageRemoveProfileParams {
    PackageRemoveProfileParams {
        profile: name.to_owned(),
        digest: ProfileDigest::parse(&format!("sha256:{}", "0".repeat(64)))
            .expect("a profile digest"),
    }
}

/// A runtime an installed package serves and the built-in `shell` have no
/// profile file; each is refused by kind, and a name that is neither is not
/// found.
#[tokio::test]
async fn a_package_runtime_and_the_builtin_shell_are_refused_by_kind() {
    let fixture = Fixture::new("rm-runtime-names");
    let package = Package::pi();
    let path = fixture.write_archive(&package);
    let packages = fixture.registry.packages();
    packages
        .package_install(explicit(&path, &package, true, true))
        .await
        .expect("install");
    fs::create_dir_all(&fixture.agents).expect("the agents directory");

    assert_eq!(
        packages.package_remove_profile(remove(RUNTIME)).await,
        Err(PackageErrorKind::ProfilePackageRuntime)
    );
    assert_eq!(
        packages.package_remove_profile(remove("shell")).await,
        Err(PackageErrorKind::ProfileBuiltin)
    );
    assert_eq!(
        packages.package_remove_profile(remove("absent")).await,
        Err(PackageErrorKind::ProfileNotFound)
    );
    assert!(
        fixture.serving(RUNTIME).is_some(),
        "the package still serves its runtime"
    );
}
