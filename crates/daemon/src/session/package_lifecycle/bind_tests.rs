//! Behavioural tests of `package.bind_profile` over a real plugin root, real
//! archives, a real agents directory and a real session registry.

// Rust guideline compliant 2026-10-05

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt as _, PermissionsExt as _};
use std::path::Path;

use package::PackageDigest;
use protocol::{
    PackageBindProfileParams, PackageBindStatus, PackageErrorKind, PackageUninstallParams,
};

use super::test_fixture::{explicit, Fixture, Package, INERT_PROGRAM, PACKAGE, RUNTIME};
use crate::agent::host::RESERVED_RUNTIME_IDS;

/// Program of the second fixture version.
const SECOND_PROGRAM: &str = "/bin/true";

/// A value that must never reach any error or log field.
const SENTINEL: &str = "sk-sentinel-secret-9f3a";

/// Name of the profile the tests bind.
const PROFILE: &str = "work";

/// Fresh fixtures one race test runs.
const RACE_ROUNDS: usize = 8;

fn bind(profile: &str, digest: Option<&PackageDigest>, dry_run: bool) -> PackageBindProfileParams {
    PackageBindProfileParams {
        profile: profile.to_owned(),
        digest: digest.cloned(),
        dry_run,
    }
}

/// A profile of base `pi`, with a secret `[env]` value and no pin.
fn unpinned() -> String {
    format!(
        "# the work profile\nbase = \"{RUNTIME}\"\nprogram = \"{INERT_PROGRAM}\"\n\n[env]\nTOKEN = \"{SENTINEL}\"\n"
    )
}

fn pinned(digest: &PackageDigest) -> String {
    format!("base = \"{RUNTIME}\"\npackage = \"{PACKAGE}\"\ndigest = \"{digest}\"\n")
}

/// Installs `package` enabled and selected.
async fn install(fixture: &Fixture, package: &Package) {
    install_with(fixture, package, true, true).await;
}

async fn install_with(fixture: &Fixture, package: &Package, enable: bool, select: bool) {
    let path = fixture.write_archive(package);
    fixture
        .registry
        .package_install(explicit(&path, package, enable, select))
        .await
        .expect("install");
}

fn second_version() -> Package {
    Package::build(PACKAGE, "2.0.0", RUNTIME, SECOND_PROGRAM)
}

/// The inode and bytes of the profile file, to compare before and after a
/// call that must not write.
fn state_of(path: &Path) -> (u64, Vec<u8>) {
    (
        fs::metadata(path).expect("metadata").ino(),
        fs::read(path).expect("read"),
    )
}

fn entries(fixture: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(&fixture.agents)
        .expect("list agents")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn pin_of(fixture: &Fixture, name: &str) -> Option<String> {
    let text = fs::read_to_string(fixture.agents.join(format!("{name}.toml"))).expect("read");
    text.lines()
        .find_map(|line| line.strip_prefix("digest = "))
        .map(|value| value.trim_matches('"').to_owned())
}

#[tokio::test]
async fn an_unpinned_package_served_profile_is_pinned_to_the_selected_package() {
    let fixture = Fixture::new("bind-unpinned");
    let package = Package::pi();
    install(&fixture, &package).await;
    let path = fixture.write_profile(PROFILE, &unpinned());

    let preview = fixture
        .registry
        .package_bind_profile(bind(PROFILE, None, true))
        .await
        .expect("preview");
    assert_eq!(preview.status, PackageBindStatus::Preview);
    assert_eq!(preview.previous, None);
    assert_eq!(preview.package.digest, package.digest);
    assert!(!preview.reloaded);
    assert_eq!(fs::read_to_string(&path).expect("read"), unpinned());

    let bound = fixture
        .registry
        .package_bind_profile(bind(PROFILE, None, false))
        .await
        .expect("bind");

    assert_eq!(bound.status, PackageBindStatus::Bound);
    assert_eq!(bound.profile, PROFILE);
    assert_eq!(bound.base.as_str(), RUNTIME);
    assert_eq!(bound.package.digest, package.digest);
    assert!(bound.package.referenced && bound.reloaded);
    assert_eq!(bound.runtime.program, INERT_PROGRAM);
    let text = fs::read_to_string(&path).expect("read");
    assert_eq!(
        text,
        unpinned().replacen(
            &format!("base = \"{RUNTIME}\"\n"),
            &format!(
                "base = \"{RUNTIME}\"\npackage = \"{PACKAGE}\"\ndigest = \"{}\"\n",
                package.digest
            ),
            1
        ),
        "only the two pin keys are added"
    );
    assert_eq!(entries(&fixture), ["work.toml"]);
    let retained = fixture
        .registry
        .retained_package_digests()
        .await
        .expect("retained");
    assert!(
        retained.contains(&package.digest),
        "retention sees the pin at once"
    );
    fixture
        .registry
        .profiles()
        .resolve_agent(PROFILE)
        .expect("the bound profile resolves");
}

#[tokio::test]
async fn re_pinning_to_another_digest_changes_the_profile_revision() {
    let fixture = Fixture::new("bind-repin");
    let first = Package::pi();
    let second = second_version();
    install(&fixture, &first).await;
    install_with(&fixture, &second, true, false).await;
    fixture.write_profile(PROFILE, &pinned(&first.digest));
    let profiles = fixture.registry.profiles();
    let before = profiles
        .revision_of(&profiles.resolve_agent(PROFILE).expect("resolves"))
        .expect("revision")
        .expect("a profile has a revision");

    let bound = fixture
        .registry
        .package_bind_profile(bind(PROFILE, Some(&second.digest), false))
        .await
        .expect("re-pin");

    assert_eq!(bound.status, PackageBindStatus::Bound);
    assert_eq!(bound.previous, Some(first.digest.clone()));
    assert_eq!(pin_of(&fixture, PROFILE), Some(second.digest.to_string()));
    let after = profiles
        .revision_of(&profiles.resolve_agent(PROFILE).expect("resolves"))
        .expect("revision")
        .expect("a profile has a revision");
    assert_ne!(before, after);
}

#[tokio::test]
async fn an_already_pinned_profile_is_reported_unchanged_without_a_write() {
    let fixture = Fixture::new("bind-unchanged");
    let package = Package::pi();
    install(&fixture, &package).await;
    let path = fixture.write_profile(PROFILE, &pinned(&package.digest));
    let before = state_of(&path);

    for dry_run in [true, false] {
        let result = fixture
            .registry
            .package_bind_profile(bind(PROFILE, None, dry_run))
            .await
            .expect("unchanged");
        assert_eq!(result.status, PackageBindStatus::Unchanged);
        assert_eq!(result.previous, Some(package.digest.clone()));
        assert!(!result.reloaded);
        assert_eq!(state_of(&path), before, "no write, not even a rename");
    }
}

#[tokio::test]
async fn a_dry_run_changes_nothing_and_keeps_stale_temporaries() {
    let fixture = Fixture::new("bind-dry-run");
    let package = Package::pi();
    install(&fixture, &package).await;
    let path = fixture.write_profile(PROFILE, &unpinned());
    let stale = fixture.write_profile(
        ".pohunek-profile-bind-00112233445566778899aabbccddeeff",
        "x",
    );
    let before = state_of(&path);

    let preview = fixture
        .registry
        .package_bind_profile(bind(PROFILE, Some(&package.digest), true))
        .await
        .expect("preview");

    assert_eq!(preview.status, PackageBindStatus::Preview);
    assert_eq!(state_of(&path), before);
    assert!(stale.exists(), "a dry run removes nothing");
}

#[tokio::test]
async fn a_builtin_base_is_refused() {
    let fixture = Fixture::new("bind-builtin");
    let package = Package::pi();
    install(&fixture, &package).await;
    let mut bases = vec![protocol::RuntimeId::SHELL];
    bases.extend(RESERVED_RUNTIME_IDS);
    for base in bases {
        let text = format!("base = \"{base}\"\n");
        let path = fixture.write_profile(PROFILE, &text);
        for digest in [None, Some(&package.digest)] {
            let error = fixture
                .registry
                .package_bind_profile(bind(PROFILE, digest, false))
                .await
                .expect_err("a built-in base takes no pin");
            assert_eq!(error, PackageErrorKind::ProfileBaseBuiltin, "{base}");
        }
        assert_eq!(fs::read_to_string(&path).expect("read"), text);
    }
}

#[tokio::test]
async fn a_target_that_cannot_be_pinned_is_refused_and_the_profile_is_untouched() {
    let fixture = Fixture::new("bind-target");
    let package = Package::pi();
    let other = Package::build("acme.runtime.other", "1.0.0", "other", INERT_PROGRAM);
    install(&fixture, &package).await;
    install(&fixture, &other).await;
    let path = fixture.write_profile(PROFILE, &unpinned());
    let before = state_of(&path);

    // A package that serves another runtime, one that is not installed and a
    // package whose root no longer verifies.
    let missing = Package::build("acme.runtime.gone", "1.0.0", "gone", INERT_PROGRAM);
    let bad_digests = [&other.digest, &missing.digest];
    for digest in bad_digests {
        let error = fixture
            .registry
            .package_bind_profile(bind(PROFILE, Some(digest), false))
            .await
            .expect_err("not a target");
        assert_eq!(error, PackageErrorKind::ProfileTargetInvalid);
    }
    fixture.tamper(&package.digest);
    for digest in [None, Some(&package.digest)] {
        let error = fixture
            .registry
            .package_bind_profile(bind(PROFILE, digest, false))
            .await
            .expect_err("faulted");
        assert_eq!(error, PackageErrorKind::ProfileTargetInvalid);
    }
    assert_eq!(state_of(&path), before);
}

#[tokio::test]
async fn no_selected_enabled_package_leaves_the_profile_unpinned() {
    let fixture = Fixture::new("bind-none-selected");
    let package = Package::pi();
    install_with(&fixture, &package, true, false).await;
    let path = fixture.write_profile(PROFILE, &unpinned());

    let error = fixture
        .registry
        .package_bind_profile(bind(PROFILE, None, false))
        .await
        .expect_err("none selected");
    assert_eq!(error, PackageErrorKind::ProfileTargetInvalid);
    assert_eq!(fs::read_to_string(&path).expect("read"), unpinned());

    let bound = fixture
        .registry
        .package_bind_profile(bind(PROFILE, Some(&package.digest), false))
        .await
        .expect("an explicit digest needs no selection");
    assert_eq!(bound.status, PackageBindStatus::Bound);
}

#[tokio::test]
async fn two_selected_enabled_packages_serving_the_base_are_ambiguous() {
    let fixture = Fixture::new("bind-ambiguous");
    let first = Package::pi();
    let second = Package::build("acme.runtime.pi-two", "1.0.0", RUNTIME, SECOND_PROGRAM);
    install(&fixture, &first).await;
    // The lifecycle refuses a second package for the same runtime id, so the
    // registry is edited directly to reach the state a damaged host can be in.
    let registry = package::registry::Registry::open_at(&fixture.plugins, package::Limits::DEFAULT)
        .expect("registry");
    registry
        .set_enabled(&first.digest, false)
        .expect("disable the first");
    fixture.host.reload().expect("reload");
    install(&fixture, &second).await;
    registry
        .set_enabled(&first.digest, true)
        .expect("force the first back on");
    fixture.host.reload().expect("reload");
    fixture.write_profile(PROFILE, &unpinned());
    let listed = fixture.registry.package_list().await.expect("list");
    let candidates = listed
        .packages
        .iter()
        .filter(|info| info.selected && info.enabled && info.runtime_id.is_some())
        .count();
    assert_eq!(candidates, 2, "the fixture reaches the ambiguous state");

    let error = fixture
        .registry
        .package_bind_profile(bind(PROFILE, None, false))
        .await
        .expect_err("ambiguous");

    assert_eq!(error, PackageErrorKind::ProfileTargetInvalid);
}

#[tokio::test]
async fn an_unusable_profile_is_refused_without_echoing_its_content() {
    let fixture = Fixture::new("bind-unusable");
    let package = Package::pi();
    install(&fixture, &package).await;

    let unreadable = [
        (
            "syntax",
            format!("base = \"{RUNTIME}\"\nTOKEN = {SENTINEL}\n"),
        ),
        ("nobase", format!("program = \"{SENTINEL}\"\n")),
        (
            "badbase",
            format!("base = \"Not A Runtime\"\n# {SENTINEL}\n"),
        ),
        (
            "unknown",
            format!("base = \"{RUNTIME}\"\nextra = \"{SENTINEL}\"\n"),
        ),
        (
            "halfpin",
            format!("base = \"{RUNTIME}\"\npackage = 5\n# {SENTINEL}\n"),
        ),
    ];
    for (name, text) in unreadable {
        let path = fixture.write_profile(name, &text);
        let error = fixture
            .registry
            .package_bind_profile(bind(name, None, false))
            .await
            .expect_err("unusable");
        assert_eq!(error, PackageErrorKind::ProfileUnusable, "{name}");
        assert_eq!(fs::read_to_string(&path).expect("read"), text, "{name}");
    }

    let target = fixture.write_profile("target", &unpinned());
    symlink(&target, fixture.agents.join("linked.toml")).expect("symlink");
    let hard = fixture.write_profile("hard", &unpinned());
    fs::hard_link(&hard, fixture.agents.join("hard.alias")).expect("hard link");
    let writable = fixture.write_profile("writable", &unpinned());
    fs::set_permissions(&writable, fs::Permissions::from_mode(0o660)).expect("widen");
    let oversized = fixture.write_profile("huge", &format!("{}\n", "#".repeat(2 << 20)));
    for name in ["linked", "hard", "writable", "huge"] {
        let error = fixture
            .registry
            .package_bind_profile(bind(name, None, false))
            .await
            .expect_err("unusable");
        assert_eq!(error, PackageErrorKind::ProfileUnusable, "{name}");
    }
    assert_eq!(fs::read_to_string(&target).expect("read"), unpinned());
    assert_eq!(fs::read_to_string(&hard).expect("read"), unpinned());
    assert_eq!(fs::read_to_string(&writable).expect("read"), unpinned());
    assert!(fs::metadata(&oversized).expect("metadata").len() > 1 << 20);
    assert!(fs::symlink_metadata(fixture.agents.join("linked.toml"))
        .expect("metadata")
        .file_type()
        .is_symlink());
}

#[tokio::test]
async fn a_missing_profile_or_name_is_not_found() {
    let fixture = Fixture::new("bind-not-found");
    install(&fixture, &Package::pi()).await;
    for name in ["absent", "bad name", "../escape"] {
        let error = fixture
            .registry
            .package_bind_profile(bind(name, None, false))
            .await
            .expect_err("not found");
        assert_eq!(error, PackageErrorKind::ProfileNotFound, "{name}");
    }
}

#[tokio::test]
async fn the_next_bind_removes_a_temporary_an_interrupted_rewrite_left() {
    let fixture = Fixture::new("bind-recovery");
    let first = Package::pi();
    let second = second_version();
    install(&fixture, &first).await;
    install_with(&fixture, &second, true, false).await;
    let path = fixture.write_profile(PROFILE, &pinned(&first.digest));
    let stale = fixture.write_profile(
        ".pohunek-profile-bind-00112233445566778899aabbccddeeff",
        &format!("base = \"{RUNTIME}\"\n[env]\nTOKEN = \"{SENTINEL}\"\n"),
    );

    // The profile still resolves and pins its digest while the temporary
    // lies next to it.
    assert_eq!(pin_of(&fixture, PROFILE), Some(first.digest.to_string()));
    fixture
        .registry
        .profiles()
        .resolve_agent(PROFILE)
        .expect("resolves with the temporary present");
    let retained = fixture
        .registry
        .retained_package_digests()
        .await
        .expect("retained");
    assert!(retained.contains(&first.digest) && !retained.contains(&second.digest));

    let bound = fixture
        .registry
        .package_bind_profile(bind(PROFILE, Some(&second.digest), false))
        .await
        .expect("bind");

    assert_eq!(bound.status, PackageBindStatus::Bound);
    assert!(!stale.exists());
    assert_eq!(entries(&fixture), ["work.toml"]);
    assert_eq!(pin_of(&fixture, PROFILE), Some(second.digest.to_string()));
    assert!(path.exists());
}

/// Binds the profile to `second` and uninstalls `second` at once, in the
/// order of `bind_first`, and asserts the invariant: the two never both
/// succeed, and the profile never pins a package that is gone.
async fn race_once(tag: &str, bind_first: bool) {
    let fixture = Fixture::new(tag);
    let first = Package::pi();
    let second = second_version();
    install(&fixture, &first).await;
    install_with(&fixture, &second, true, false).await;
    fixture.write_profile(PROFILE, &pinned(&first.digest));

    let bind_call =
        fixture
            .registry
            .package_bind_profile(bind(PROFILE, Some(&second.digest), false));
    let uninstall_call = fixture.registry.package_uninstall(PackageUninstallParams {
        digest: second.digest.clone(),
        remove_modified: false,
    });
    let (bound, removed) = if bind_first {
        tokio::join!(bind_call, uninstall_call)
    } else {
        let (removed, bound) = tokio::join!(uninstall_call, bind_call);
        (bound, removed)
    };

    match (&bound, &removed) {
        (Ok(_), Ok(_)) => panic!("{tag}: the bind and the uninstall both succeeded"),
        (Ok(_), Err(error)) => assert_eq!(*error, PackageErrorKind::Referenced, "{tag}"),
        (Err(error), Ok(_)) => {
            assert_eq!(*error, PackageErrorKind::ProfileTargetInvalid, "{tag}");
        }
        (Err(bind_error), Err(remove_error)) => {
            panic!("{tag}: both refused: {bind_error:?} / {remove_error:?}")
        }
    }
    let listed = fixture.registry.package_list().await.expect("list");
    let pinned_digest = pin_of(&fixture, PROFILE).expect("the profile keeps a pin");
    assert!(
        listed
            .packages
            .iter()
            .any(|package| package.digest.to_string() == pinned_digest),
        "{tag}: the profile pins {pinned_digest}, which is not installed"
    );
    fixture
        .registry
        .profiles()
        .resolve_agent(PROFILE)
        .unwrap_or_else(|error| panic!("{tag}: the profile cannot launch: {error:?}"));
}

#[tokio::test]
async fn a_bind_and_an_uninstall_of_the_target_never_both_succeed() {
    for round in 0..RACE_ROUNDS {
        race_once(&format!("bind-race-bind-first-{round}"), true).await;
        race_once(&format!("bind-race-uninstall-first-{round}"), false).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bind_and_an_uninstall_racing_on_worker_threads_keep_the_invariant() {
    for round in 0..RACE_ROUNDS {
        race_once(&format!("bind-race-threads-{round}"), round % 2 == 0).await;
    }
}

// ----- the candidate must launch as the target ------------------------------

/// A package at `version` whose descriptor is the fixture's with `transform`
/// applied, so its native recovery capabilities differ from the default
/// (assigned reference, fork supported).
fn capability_variant(version: &str, transform: impl FnOnce(String) -> String) -> Package {
    use crate::agent::host::fixture::{pi_shaped_document, PI_SHAPED_NO_CHECK};
    let document = pi_shaped_document(std::path::Path::new(INERT_PROGRAM), PI_SHAPED_NO_CHECK)
        .replace("version = \"1.0.0\"", &format!("version = \"{version}\""))
        .replace(
            "detect_manifest = \"any\"",
            "detect_manifest = \"detect.toml\"",
        );
    Package::with_descriptor(&transform(document))
}

/// The fixture descriptor with a hook-reported reference instead of an
/// assigned one.
fn hook_strategy(document: &str) -> String {
    let start = document
        .find("[native_reference]")
        .expect("the native reference table");
    format!(
        "{}[native_reference]\nstrategy = \"hook\"\n",
        &document[..start]
    )
}

/// A hook runtime that cannot fork.
fn hook_without_fork(document: &str) -> String {
    hook_strategy(document).replace(
        "[fork]\nsupported = true\nargs = [\"--fork\", \"{reference}\"]",
        "[fork]\nsupported = false",
    )
}

const RESUME_OVERRIDE: &str =
    "\n[resume]\nreference_kind = \"id\"\nargs = [\"--session\", \"{reference}\"]\n";

/// Binds `profile` to `target` in a preview and a commit and expects both to
/// be refused with the original untouched.
async fn expect_refused(fixture: &Fixture, path: &Path, target: &Package) {
    let before = state_of(path);
    for dry_run in [true, false] {
        let refused = fixture
            .registry
            .package_bind_profile(bind(PROFILE, Some(&target.digest), dry_run))
            .await
            .expect_err("the candidate would not launch");
        assert_eq!(
            refused,
            PackageErrorKind::ProfileUnusable,
            "dry_run {dry_run}"
        );
        assert_eq!(state_of(path), before, "the profile is untouched");
    }
}

#[tokio::test]
async fn a_resume_override_that_conflicts_with_an_assigned_target_is_refused() {
    let fixture = Fixture::new("bind-hook-to-assigned");
    let hook = capability_variant("1.0.0", |document| hook_strategy(&document));
    let assigned = capability_variant("2.0.0", |document| document);
    install(&fixture, &hook).await;
    install_with(&fixture, &assigned, true, false).await;
    // Valid against the hook runtime, which lets a profile restate [resume].
    let path = fixture.write_profile(
        PROFILE,
        &format!("{}{RESUME_OVERRIDE}", pinned(&hook.digest)),
    );
    fixture
        .registry
        .package_bind_profile(bind(PROFILE, Some(&hook.digest), false))
        .await
        .expect("already pinned to the hook version");

    expect_refused(&fixture, &path, &assigned).await;
}

#[tokio::test]
async fn a_fork_override_is_refused_when_the_target_cannot_fork() {
    let fixture = Fixture::new("bind-fork-removed");
    let forking = capability_variant("1.0.0", |document| hook_strategy(&document));
    let plain = capability_variant("2.0.0", |document| hook_without_fork(&document));
    install(&fixture, &forking).await;
    install_with(&fixture, &plain, true, false).await;
    let override_with_fork = format!(
        "{}{RESUME_OVERRIDE}fork_args = [\"--fork\", \"{{reference}}\"]\n",
        pinned(&forking.digest)
    );
    let path = fixture.write_profile(PROFILE, &override_with_fork);

    expect_refused(&fixture, &path, &plain).await;
}

#[tokio::test]
async fn a_profile_without_overrides_migrates_across_a_strategy_change() {
    let fixture = Fixture::new("bind-strategy-change");
    let hook = capability_variant("1.0.0", |document| hook_strategy(&document));
    let assigned = capability_variant("2.0.0", |document| document);
    install(&fixture, &hook).await;
    install_with(&fixture, &assigned, true, false).await;
    fixture.write_profile(PROFILE, &pinned(&hook.digest));

    let bound = fixture
        .registry
        .package_bind_profile(bind(PROFILE, Some(&assigned.digest), false))
        .await
        .expect("nothing in the profile conflicts with the target");

    assert_eq!(bound.status, PackageBindStatus::Bound);
    assert_eq!(pin_of(&fixture, PROFILE), Some(assigned.digest.to_string()));
}

#[tokio::test]
async fn the_rewritten_profile_must_fit_the_size_limit_the_loader_applies() {
    let fixture = Fixture::new("bind-size-boundary");
    let package = Package::pi();
    install(&fixture, &package).await;
    let limit = usize::try_from(crate::agent::MAX_PROFILE_BYTES).expect("limit fits");
    let pad = |length: usize| {
        let base = format!("base = \"{RUNTIME}\"\n");
        let comment = "#".repeat(length - base.len() - 1);
        format!("{base}{comment}\n")
    };

    // A few bytes under the limit, the two added keys push it over.
    let near = pad(limit - 10);
    let path = fixture.write_profile(PROFILE, &near);
    assert_eq!(near.len(), limit - 10);
    let before = state_of(&path);
    for dry_run in [true, false] {
        let refused = fixture
            .registry
            .package_bind_profile(bind(PROFILE, None, dry_run))
            .await
            .expect_err("the rewrite would exceed the loader's limit");
        assert_eq!(refused, PackageErrorKind::ProfileUnusable);
        assert_eq!(state_of(&path), before, "the original is untouched");
    }

    // With room for the keys it migrates and still loads.
    let room = pad(limit - 400);
    fixture.write_profile(PROFILE, &room);
    let bound = fixture
        .registry
        .package_bind_profile(bind(PROFILE, None, false))
        .await
        .expect("a profile with room migrates");
    assert_eq!(bound.status, PackageBindStatus::Bound);
    let written = fs::read(&path).expect("read").len();
    assert!(written <= limit, "{written} bytes");
}

// ----- manifest overrides are read under the lock with a bound --------------

/// A profile that names the detection-manifest override `manifest`.
fn with_manifest(manifest: &str) -> String {
    format!("{}manifest = \"{manifest}\"\n", unpinned_head())
}

fn unpinned_head() -> String {
    format!("base = \"{RUNTIME}\"\nprogram = \"{INERT_PROGRAM}\"\n")
}

fn manifests_dir(fixture: &Fixture) -> std::path::PathBuf {
    let dir = fixture.agents.join("manifests");
    fs::create_dir_all(&dir).expect("manifests directory");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("private manifests");
    dir
}

#[tokio::test]
async fn an_oversized_manifest_override_is_refused_in_preview_and_commit() {
    let fixture = Fixture::new("bind-manifest-oversized");
    install(&fixture, &Package::pi()).await;
    let big = manifests_dir(&fixture).join("big.toml");
    let file = fs::File::create(&big).expect("create");
    file.set_len(u64::try_from(crate::detect::MAX_MANIFEST_SOURCE_BYTES).expect("fits") + 1)
        .expect("sparse file");
    fs::set_permissions(&big, fs::Permissions::from_mode(0o600)).expect("secure");
    let path = fixture.write_profile(PROFILE, &with_manifest("big"));
    let before = state_of(&path);

    for dry_run in [true, false] {
        let refused = fixture
            .registry
            .package_bind_profile(bind(PROFILE, None, dry_run))
            .await
            .expect_err("the manifest is over the parser's limit");
        assert_eq!(
            refused,
            PackageErrorKind::ProfileUnusable,
            "dry_run {dry_run}"
        );
        assert_eq!(state_of(&path), before, "the profile is untouched");
    }
}

#[tokio::test]
async fn a_non_regular_manifest_override_is_refused_without_blocking() {
    let fixture = Fixture::new("bind-manifest-fifo");
    install(&fixture, &Package::pi()).await;
    let pipe = manifests_dir(&fixture).join("pipe.toml");
    nix::unistd::mkfifo(
        &pipe,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("create a fifo");
    let path = fixture.write_profile(PROFILE, &with_manifest("pipe"));
    let before = state_of(&path);

    for dry_run in [true, false] {
        let refused = fixture
            .registry
            .package_bind_profile(bind(PROFILE, None, dry_run))
            .await
            .expect_err("a fifo is not a manifest");
        assert_eq!(
            refused,
            PackageErrorKind::ProfileUnusable,
            "dry_run {dry_run}"
        );
        assert_eq!(state_of(&path), before, "the profile is untouched");
    }
}
