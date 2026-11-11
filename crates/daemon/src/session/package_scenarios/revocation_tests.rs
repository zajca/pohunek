//! Revoked package digests: a catalog recorded after a package was installed
//! revokes its digest, and the daemon refuses fresh launches, resumes,
//! selections and pins of it while a running session keeps running.
//!
//! Every scenario installs an official package through a signed catalog, then
//! records a later catalog that revokes it, over a real plugin root, a real
//! session store and the real session registry.

// Rust guideline compliant 2026-11-10

use std::io::Write as _;
use std::path::PathBuf;

use ed25519_dalek::SigningKey;
use package::PackageDigest;
use pohunek_test_support::wait::HANG_GUARD;
use protocol::{
    PackageBindProfileParams, PackageErrorKind, PackageFindingKind, PackageSelectParams,
    SessionWarningKind,
};

use super::catalog_tests::{anchor, catalog, catalog_install, entry, signed_by, signing_key};
use super::test_fixture::{explicit, Fixture, Package, PACKAGE, RUNTIME};
use crate::session::tests::{assigned_agent_script, params, temp_dir, temp_store_path};

/// An official package installed through catalog sequence 1, with the agent
/// script it launches.
struct Official {
    fixture: Fixture,
    key: SigningKey,
    dir: PathBuf,
    script: String,
    gate: std::fs::File,
    first: Package,
}

async fn official(tag: &str) -> Official {
    let dir = temp_dir(tag);
    let (script, gate) = assigned_agent_script(&dir, &dir.join("argv.txt"));
    let script = script.to_str().expect("utf-8 script path").to_owned();
    let store = temp_store_path(tag);
    let key = signing_key("root");
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&key], Vec::new()),
        store.clone(),
    );
    let first = Package::build(PACKAGE, "1.0.0", RUNTIME, &script);
    let document = signed_by(
        &key,
        catalog(1, vec![entry(&first, PACKAGE, RUNTIME, "1.0.0")]),
    );
    fixture
        .registry
        .packages()
        .package_install(catalog_install(&fixture, &first, &document, false))
        .await
        .expect("install the official package");
    Official {
        fixture,
        key,
        dir,
        script,
        gate,
        first,
    }
}

impl Official {
    /// Records catalog sequence 2, which revokes the first digest, by
    /// installing a second version through it. The second version is enabled
    /// and left unselected.
    async fn revoke_first(&self) -> Package {
        let second = Package::build(PACKAGE, "2.0.0", RUNTIME, &self.script);
        let mut body = catalog(2, vec![entry(&second, PACKAGE, RUNTIME, "2.0.0")]);
        body.revoked_digests = vec![self.first.digest.clone()];
        let document = signed_by(&self.key, body);
        let mut install = catalog_install(&self.fixture, &second, &document, false);
        install.select = false;
        self.fixture
            .registry
            .packages()
            .package_install(install)
            .await
            .expect("install the second version through the revoking catalog");
        second
    }

    fn launch(&self) -> protocol::SessionNewParams {
        protocol::SessionNewParams {
            agent: RUNTIME.to_owned(),
            cwd: Some(self.dir.clone()),
            ..params()
        }
    }
}

fn select(digest: &PackageDigest) -> PackageSelectParams {
    PackageSelectParams {
        digest: digest.clone(),
    }
}

#[tokio::test]
async fn a_revoked_selected_digest_refuses_fresh_launches_until_the_operator_selects_another() {
    let official = official("revoked-selected").await;
    let packages = official.fixture.registry.packages();
    let second = official.revoke_first().await;

    let refused = official
        .fixture
        .registry
        .create(official.launch())
        .await
        .expect_err("the selected digest is revoked");
    assert_eq!(refused.code, "package_revoked");
    assert!(
        refused
            .recover
            .as_deref()
            .is_some_and(|hint| hint.contains("pohunek plugin select")),
        "the error names the recovery command"
    );
    assert_eq!(
        official.fixture.serving(RUNTIME),
        Some(official.first.digest.clone()),
        "the selection is not changed on its own"
    );

    let info = packages
        .package_inspect(official.first.digest.clone())
        .await
        .expect("inspect");
    assert!(info.package.revoked);
    let doctor = packages.package_doctor(None).await.expect("doctor");
    assert!(doctor.findings.iter().any(|finding| {
        finding.kind == PackageFindingKind::Revoked && finding.digest == official.first.digest
    }));

    packages
        .package_select(select(&second.digest))
        .await
        .expect("select the other version");
    let mut gate = official.gate.try_clone().expect("gate");
    let created = official
        .fixture
        .registry
        .create(official.launch())
        .await
        .expect("a launch from the new selection works");
    gate.write_all(b"go\n").expect("release the exit gate");
    official
        .fixture
        .registry
        .wait_for_exit(&created.id, HANG_GUARD)
        .await
        .expect("the session exits");

    assert_eq!(
        packages
            .package_select(select(&official.first.digest))
            .await
            .expect_err("a revoked digest cannot be selected again"),
        PackageErrorKind::Revoked
    );
}

#[tokio::test]
async fn a_live_session_keeps_running_is_flagged_and_cannot_be_resumed_from_a_revoked_digest() {
    let official = official("revoked-live").await;
    let created = official
        .fixture
        .registry
        .create(official.launch())
        .await
        .expect("create before the revocation");
    official.revoke_first().await;

    let listed = official.fixture.registry.list().await;
    let session = listed
        .iter()
        .find(|session| session.id == created.id)
        .expect("listed");
    assert!(
        session
            .warnings
            .iter()
            .any(|warning| warning.kind == SessionWarningKind::Hook
                && warning.detail.as_deref()
                    == Some(format!("package_revoked digest={}", official.first.digest).as_str())),
        "the session list flags the session"
    );
    assert!(
        session.can_stop(),
        "the session still runs and stays controllable"
    );
    let doctor = official
        .fixture
        .registry
        .packages()
        .package_doctor(None)
        .await
        .expect("doctor");
    let finding = doctor
        .findings
        .iter()
        .find(|finding| finding.kind == PackageFindingKind::Revoked)
        .expect("the revoked finding");
    assert_eq!(
        finding.running_sessions,
        std::slice::from_ref(&created.id),
        "the doctor names the session that still runs from the digest"
    );

    official
        .fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop");
    let refused = official
        .fixture
        .registry
        .resume(&created.id)
        .await
        .expect_err("a stopped session does not resume from a revoked digest");
    assert_eq!(refused.code, "package_revoked");
    let after = official
        .fixture
        .registry
        .packages()
        .package_doctor(None)
        .await
        .expect("doctor");
    assert!(
        after
            .findings
            .iter()
            .filter(|finding| finding.kind == PackageFindingKind::Revoked)
            .all(|finding| finding.running_sessions.is_empty()),
        "a stopped session no longer runs from the digest"
    );
}

/// The session list reads the registry and the revocation set once for every
/// session: sessions that share a revoked digest are all flagged, and a
/// session on a digest that is not revoked is not.
#[tokio::test]
async fn the_session_list_flags_exactly_the_sessions_pinned_to_a_revoked_digest() {
    let official = official("revoked-list").await;
    let registry = &official.fixture.registry;
    let first = registry.create(official.launch()).await.expect("first");
    let sibling = registry.create(official.launch()).await.expect("sibling");
    let second = official.revoke_first().await;
    official
        .fixture
        .registry
        .packages()
        .package_select(select(&second.digest))
        .await
        .expect("select the version that is not revoked");
    let current = registry
        .create(official.launch())
        .await
        .expect("a launch from the new selection");

    let listed = registry.list().await;
    let flagged = |id: &protocol::SessionId| {
        listed
            .iter()
            .find(|session| &session.id == id)
            .expect("listed")
            .warnings
            .iter()
            .filter(|warning| {
                warning.detail.as_deref()
                    == Some(format!("package_revoked digest={}", official.first.digest).as_str())
            })
            .count()
    };
    assert_eq!(flagged(&first.id), 1);
    assert_eq!(flagged(&sibling.id), 1);
    assert_eq!(flagged(&current.id), 0);

    for id in [&first.id, &sibling.id, &current.id] {
        registry.stop(id).await.expect("stop");
    }
}

#[tokio::test]
async fn a_profile_pinned_to_a_revoked_digest_refuses_launches_and_new_pins() {
    let official = official("revoked-pinned").await;
    let second = official.revoke_first().await;
    official.fixture.write_profile(
        "work",
        &format!(
            "base = \"{RUNTIME}\"\npackage = \"{PACKAGE}\"\ndigest = \"{}\"\n",
            official.first.digest
        ),
    );

    let refused = official
        .fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: "work".to_owned(),
            ..official.launch()
        })
        .await
        .expect_err("the pinned digest is revoked");
    assert_eq!(refused.code, "package_revoked");

    let bind = |digest: &PackageDigest| PackageBindProfileParams {
        expected_previous: None,
        profile: "work".to_owned(),
        digest: Some(digest.clone()),
        dry_run: false,
    };
    assert_eq!(
        official
            .fixture
            .registry
            .packages()
            .package_bind_profile(bind(&official.first.digest))
            .await
            .expect_err("a revoked digest cannot be pinned"),
        PackageErrorKind::Revoked
    );
    official
        .fixture
        .registry
        .packages()
        .package_bind_profile(bind(&second.digest))
        .await
        .expect("a profile moves to a version that is not revoked");
}

#[tokio::test]
async fn a_revoked_digest_stays_revoked_across_a_restart_and_a_later_catalog_that_lists_it() {
    let official = official("revoked-restart").await;
    official.revoke_first().await;

    let restarted = official.fixture.reopen();
    let refused = restarted
        .registry
        .create(official.launch())
        .await
        .expect_err("the restarted daemon reads the revocation back");
    assert_eq!(refused.code, "package_revoked");

    let relisted = signed_by(
        &official.key,
        catalog(3, vec![entry(&official.first, PACKAGE, RUNTIME, "1.0.0")]),
    );
    let mut install = catalog_install(&official.fixture, &official.first, &relisted, false);
    install.select = false;
    assert_eq!(
        official
            .fixture
            .registry
            .packages()
            .package_install(install)
            .await
            .expect_err("a later catalog does not resurrect a revoked digest"),
        PackageErrorKind::Revoked
    );
}

#[tokio::test]
async fn a_package_the_owner_trusts_by_digest_is_not_revoked_by_a_catalog() {
    let dir = temp_dir("revoked-local");
    let (script, _gate) = assigned_agent_script(&dir, &dir.join("argv.txt"));
    let script = script.to_str().expect("utf-8 script path").to_owned();
    let key = signing_key("root");
    let fixture = Fixture::with_anchor_and_store(
        "revoked-local-plugins",
        anchor(&[&key], Vec::new()),
        temp_store_path("revoked-local"),
    );
    let local = Package::build(PACKAGE, "1.0.0", RUNTIME, &script);
    let path = fixture.write_archive(&local);
    fixture
        .registry
        .packages()
        .package_install(explicit(&path, &local, true, true))
        .await
        .expect("install by explicit digest");

    let other = Package::build("acme.runtime.other", "1.0.0", "other", &script);
    let mut body = catalog(
        1,
        vec![entry(&other, "acme.runtime.other", "other", "1.0.0")],
    );
    body.revoked_digests = vec![local.digest.clone()];
    let document = signed_by(&key, body);
    fixture
        .registry
        .packages()
        .package_install(catalog_install(&fixture, &other, &document, false))
        .await
        .expect("record the revoking catalog");

    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: RUNTIME.to_owned(),
            cwd: Some(dir),
            ..params()
        })
        .await
        .expect("an explicit-digest package is unaffected");
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
    assert!(
        !fixture
            .registry
            .packages()
            .package_inspect(local.digest)
            .await
            .expect("inspect")
            .package
            .revoked
    );
}

#[tokio::test]
async fn an_unreadable_revocation_file_refuses_official_launches() {
    let official = official("revoked-damaged").await;
    let plugins = official.fixture.plugins.clone();
    std::fs::write(plugins.join("revoked-digests.json"), b"{ not json").expect("damage");
    std::fs::set_permissions(
        plugins.join("revoked-digests.json"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .expect("mode");

    let refused = official
        .fixture
        .registry
        .create(official.launch())
        .await
        .expect_err("enforcement fails closed");
    assert_eq!(refused.code, "runtime_incompatible");
    let listed = official
        .fixture
        .registry
        .packages()
        .package_list()
        .await
        .expect("the package list stays readable");
    assert!(
        listed.packages.iter().all(|package| !package.revoked),
        "an unreadable set flags nothing"
    );
    assert_eq!(
        official
            .fixture
            .registry
            .packages()
            .package_doctor(None)
            .await
            .expect_err("the doctor reports the unreadable file"),
        PackageErrorKind::RevocationsUnreadable
    );
    assert_eq!(
        std::fs::read(plugins.join("revoked-digests.json")).expect("read"),
        b"{ not json",
        "the damaged file is never replaced"
    );
}
