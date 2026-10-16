//! A stopped session launched from an official package version is adopted by a
//! newer version of the same package on resume and fork, and stays on its
//! exact digest in every other case.
//!
//! The packages are the official Claude, Codex and Pi packages as they ship,
//! with their version probes, rebuilt at two package versions whose supported
//! agent ranges differ. The agent is a script that answers the probe with a
//! release only the newer range accepts once the host "updated" it, which is
//! the case a package range widening exists for. Claude and Codex report their
//! conversation through hooks; Pi's is assigned by core at launch.

// Rust guideline compliant 2026-10-16

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use package::PackageDigest;
use pohunek_service_config::preflight::{Verdict, CODE_ADOPTABLE, CODE_ADOPTABLE_BY_NEWER_PACKAGE};
use pohunek_service_config::runtimes::ProfileStatus;
use protocol::{
    BindingProvenance, PackageBindProfileParams, PackageBindStatus, PackageSelectParams, RuntimeId,
    SessionId,
};

use super::catalog_tests::{anchor, catalog, catalog_install, entry, signed_by, signing_key};
use super::test_fixture::{explicit, Fixture, Package};
use crate::agent::host::fixture::{official_package_identity, official_package_sources};
use crate::procwatch::HostInspector;
use crate::session::tests::{
    durable_recovery, native_report_params, params, temp_dir, temp_store_path,
};
use crate::session::{ProfileChange, SessionRegistry};

/// The host profile the sessions launch from.
const PROFILE: &str = "work";

/// The conversation a hook-reporting session reports.
const CONVERSATION: &str = "native-newer-version";

/// One official runtime under test: its package versions and agent ranges, and
/// how its agent answers the version probe and keeps its conversations.
struct Kind {
    runtime: &'static str,
    /// The older package version and the end of the agent range it accepts.
    old_version: &'static str,
    old_below: &'static str,
    /// The newer package version and the end of the agent range it accepts.
    new_version: &'static str,
    new_below: &'static str,
    /// An agent release both ranges accept.
    in_both: &'static str,
    /// An agent release only the newer range accepts.
    only_new: &'static str,
    /// The probe line the agent prints for a release, `{version}` replaced.
    version_line: &'static str,
    /// The variable naming the agent's config home.
    home_env: &'static str,
}

const CLAUDE: Kind = Kind {
    runtime: RuntimeId::CLAUDE,
    old_version: "1.0.1",
    old_below: "2.2.0",
    new_version: "1.0.2",
    new_below: "3.0.0",
    in_both: "2.1.300",
    only_new: "2.2.5",
    version_line: "{version} (Claude Code)",
    home_env: "CLAUDE_CONFIG_DIR",
};

/// Codex at the releases of the owner's host that motivated the range
/// widening: the newer package accepts `codex-cli 0.162.1`.
const CODEX: Kind = Kind {
    runtime: RuntimeId::CODEX,
    old_version: "1.1.0",
    old_below: "0.161.0",
    new_version: "1.1.1",
    new_below: "1.0.0",
    in_both: "0.160.5",
    only_new: "0.162.1",
    version_line: "codex-cli {version}",
    home_env: "CODEX_HOME",
};

const PI: Kind = Kind {
    runtime: "pi",
    old_version: "1.0.2",
    old_below: "1.1.0",
    new_version: "1.0.3",
    new_below: "2.0.0",
    in_both: "1.0.5",
    only_new: "1.1.0",
    version_line: "{version}",
    home_env: "PI_CODING_AGENT_DIR",
};

impl Kind {
    fn package_id(&self) -> String {
        official_package_identity(self.runtime).0
    }

    /// The official package at package version `version`, accepting agent
    /// releases below `below`. `edit` changes the descriptor further.
    fn package(&self, version: &str, below: &str, edit: impl FnOnce(String) -> String) -> Package {
        let (descriptor, detect) = official_package_sources(self.runtime);
        let rewritten: Vec<String> = descriptor
            .lines()
            .map(|line| {
                if line.starts_with("version = ") {
                    format!("version = \"{version}\"")
                } else if line.starts_with("version_probe = ") {
                    let start = line.find("below = \"").expect("the probe declares below") + 9;
                    let end = start + line[start..].find('"').expect("a closed below");
                    format!("{}{below}{}", &line[..start], &line[end..])
                } else {
                    line.to_owned()
                }
            })
            .collect();
        let descriptor = edit(rewritten.join("\n"));
        Package::from_entries(&[
            package::ArchiveEntry {
                path: crate::agent::host::RUNTIME_DESCRIPTOR_PATH.to_owned(),
                contents: descriptor.into_bytes(),
                executable: false,
            },
            package::ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: detect.as_bytes().to_vec(),
                executable: false,
            },
        ])
    }

    fn older(&self) -> Package {
        self.package(self.old_version, self.old_below, |text| text)
    }

    fn newer(&self) -> Package {
        self.package(self.new_version, self.new_below, |text| text)
    }

    /// Writes the agent at `path`: it answers the version probe with `release`
    /// and otherwise keeps running.
    fn write_agent(&self, path: &Path, release: &str) {
        let line = self.version_line.replace("{version}", release);
        fs::write(
            path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf '%s\\n' '{line}'\n  exit 0\nfi\nsleep 600\n"
            ),
        )
        .expect("write the agent");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("make the agent executable");
    }
}

/// Installs and selects `package` of `kind` at `version` through a catalog
/// signed by the test root at `sequence`.
async fn install(fixture: &Fixture, kind: &Kind, package: &Package, version: &str, sequence: u64) {
    let document = signed_by(
        &signing_key("root"),
        catalog(
            sequence,
            vec![entry(package, &kind.package_id(), kind.runtime, version)],
        ),
    );
    fixture
        .registry
        .package_install(catalog_install(fixture, package, &document, false))
        .await
        .expect("official install");
}

/// Pins the [`PROFILE`] host profile to `digest`, as `service upgrade` or
/// `plugin profile migrate` does.
async fn pin_profile(registry: &SessionRegistry, digest: &PackageDigest) {
    let bound = registry
        .package_bind_profile(PackageBindProfileParams {
            profile: PROFILE.to_owned(),
            digest: Some(digest.clone()),
            dry_run: false,
        })
        .await
        .expect("bind the profile");
    assert_eq!(bound.status, PackageBindStatus::Bound);
}

/// A stopped session of the [`PROFILE`] profile launched from `launched`.
struct Stopped {
    fixture: Fixture,
    store_path: PathBuf,
    agent: PathBuf,
    id: SessionId,
}

/// Installs `launched` at `version`, pins the profile to it, starts a session
/// whose conversation exists, and stops it.
async fn stopped_on(tag: &str, kind: &Kind, launched: &Package, version: &str) -> Stopped {
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&signing_key("root")], Vec::new()),
        store_path.clone(),
    );
    let home = fixture.dir.join("agent-home");
    fs::create_dir_all(&home).expect("the agent home");
    let agent = fixture.dir.join(format!("{}-agent", kind.runtime));
    kind.write_agent(&agent, kind.in_both);
    fixture.write_profile(
        PROFILE,
        &format!(
            "base = \"{}\"\nprogram = \"{}\"\n\n[env]\n{} = \"{}\"\n",
            kind.runtime,
            agent.display(),
            kind.home_env,
            home.display()
        ),
    );
    install(&fixture, kind, launched, version, 5).await;
    pin_profile(&fixture.registry, &launched.digest).await;
    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: PROFILE.to_owned(),
            cwd: Some(temp_dir(tag)),
            ..params()
        })
        .await
        .expect("a session of the older package");
    match kind.runtime {
        RuntimeId::CLAUDE => {
            let conversations = home.join("projects").join("work");
            fs::create_dir_all(&conversations).expect("the conversation directory");
            fs::write(conversations.join(format!("{CONVERSATION}.jsonl")), b"{}\n")
                .expect("the conversation");
        }
        "pi" => {
            // Pi's reference is assigned at launch; the conversation file is
            // named after it, the way Pi names it.
            let reference = created
                .native_session_id
                .clone()
                .expect("the session holds its assigned reference");
            let conversations = home.join("sessions").join("work");
            fs::create_dir_all(&conversations).expect("the conversation directory");
            fs::write(conversations.join(format!("0_{reference}.jsonl")), b"{}\n")
                .expect("the conversation");
        }
        _ => {}
    }
    if kind.runtime != "pi" {
        let report = native_report_params(
            &fixture.registry,
            created.id.clone(),
            kind.runtime.to_owned(),
            CONVERSATION.to_owned(),
            None,
        )
        .await;
        assert!(fixture.registry.report_as_root(report).await.recorded);
    }
    fixture
        .registry
        .stop(&created.id)
        .await
        .expect("stop the session");
    assert_eq!(pinned_digest(&store_path, &created.id), launched.digest);
    Stopped {
        fixture,
        store_path,
        agent,
        id: created.id,
    }
}

/// The package digest the durable recovery binding of `id` pins.
fn pinned_digest(store_path: &Path, id: &SessionId) -> PackageDigest {
    match durable_recovery(store_path, id)
        .launch_binding
        .binding()
        .map(|binding| &binding.provenance)
    {
        Some(BindingProvenance::Package { package_digest, .. }) => package_digest.clone(),
        other => panic!("a package pin, not {other:?}"),
    }
}

/// Restarts the daemon over `fixture`'s state.
async fn restart(fixture: &Fixture) -> Fixture {
    let restarted = fixture.reopen();
    Box::pin(restarted.registry.reconcile_workers())
        .await
        .expect("startup reconciliation");
    restarted
}

/// The verdict and code the upgrade preflight gives the stopped session `id`,
/// judged over the host profiles and the revision key the daemon uses.
fn stopped_verdict(fixture: &Fixture, store_path: &Path, id: &SessionId) -> (Verdict, String) {
    super::official_alias_tests::stopped_preflight_verdict(fixture, store_path, id)
}

/// A stopped session of `kind` whose agent left the older package's range,
/// with the newer version installed and the profile moved to it.
async fn moved_to_newer(tag: &str, kind: &Kind) -> (Stopped, Package, Package) {
    let old = kind.older();
    let new = kind.newer();
    let session = stopped_on(tag, kind, &old, kind.old_version).await;
    kind.write_agent(&session.agent, kind.only_new);
    install(&session.fixture, kind, &new, kind.new_version, 6).await;
    pin_profile(&session.fixture.registry, &new.digest).await;
    (session, old, new)
}

/// The adoption a stopped session of `kind` gets once its profile moved to the
/// newer version: the preflight names it, a plain resume runs through the newer
/// version without a profile change, and the relaunch records its digest.
async fn assert_resume_adopted(tag: &str, kind: &Kind) {
    let (session, _old, new) = moved_to_newer(tag, kind).await;
    let restarted = restart(&session.fixture).await;
    assert_eq!(
        stopped_verdict(&restarted, &session.store_path, &session.id),
        (
            Verdict::Adoptable,
            CODE_ADOPTABLE_BY_NEWER_PACKAGE.to_owned()
        ),
        "the preflight names the adoption the resume performs"
    );
    let resumed = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect("the newer version adopts the session without a profile change");
    assert_eq!(
        resumed.runtime.as_ref().map(|runtime| runtime.state),
        Some(protocol::RuntimeState::Live)
    );
    assert_eq!(
        pinned_digest(&session.store_path, &session.id),
        new.digest,
        "the relaunch records the adopting digest"
    );
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

/// A same-directory fork of session `id`.
fn fork_params(id: &SessionId) -> protocol::SessionForkParams {
    protocol::SessionForkParams {
        session_id: id.clone(),
        name: None,
        cwd_mode: protocol::ForkCwdMode::Same,
        cols: 100,
        rows: 30,
        accept_profile_change: false,
    }
}

/// A fork of a stopped session of `kind` runs through the newer version and the
/// fork is pinned to it, while the source keeps its pin until it resumes.
async fn assert_fork_adopted(tag: &str, kind: &Kind) {
    let (session, old, new) = moved_to_newer(tag, kind).await;
    let restarted = restart(&session.fixture).await;
    let forked = restarted
        .registry
        .fork(fork_params(&session.id))
        .await
        .expect("the newer version adopts the fork");
    assert_eq!(pinned_digest(&session.store_path, &forked.id), new.digest);
    assert_eq!(
        pinned_digest(&session.store_path, &session.id),
        old.digest,
        "the source keeps its own pin until it is resumed"
    );
    restarted
        .registry
        .remove(&forked.id)
        .await
        .expect("remove the fork");
}

/// A newer version of `kind` built by `edit` from the newer descriptor does not
/// adopt the stopped session: the session stays on its exact digest, which the
/// profile no longer names.
async fn assert_never_adopted_by(tag: &str, kind: &Kind, edit: impl FnOnce(String) -> String) {
    let old = kind.older();
    let changed = kind.package(kind.new_version, kind.new_below, edit);
    let session = stopped_on(tag, kind, &old, kind.old_version).await;
    kind.write_agent(&session.agent, kind.only_new);
    install(&session.fixture, kind, &changed, kind.new_version, 6).await;
    pin_profile(&session.fixture.registry, &changed.digest).await;

    let restarted = restart(&session.fixture).await;
    let refused = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect_err("the changed version does not adopt the session");
    assert_eq!(refused.code, "agent_profile_changed", "{refused:?}");
    assert_eq!(pinned_digest(&session.store_path, &session.id), old.digest);
}

#[tokio::test]
async fn a_newer_package_version_adopts_a_stopped_claude_session_whose_profile_moved_to_it() {
    assert_resume_adopted("newer-adopts-claude", &CLAUDE).await;
}

#[tokio::test]
async fn a_newer_package_version_adopts_a_stopped_codex_session_whose_profile_moved_to_it() {
    assert_resume_adopted("newer-adopts-codex", &CODEX).await;
}

#[tokio::test]
async fn a_newer_package_version_adopts_a_stopped_pi_session_whose_profile_moved_to_it() {
    assert_resume_adopted("newer-adopts-pi", &PI).await;
}

#[tokio::test]
async fn a_newer_package_version_adopts_a_fork_of_a_stopped_claude_session() {
    assert_fork_adopted("newer-adopts-fork-claude", &CLAUDE).await;
}

#[tokio::test]
async fn a_newer_package_version_adopts_a_fork_of_a_stopped_pi_session() {
    assert_fork_adopted("newer-adopts-fork-pi", &PI).await;
}

#[tokio::test]
async fn a_codex_session_stays_unforkable_once_a_newer_version_adopts_it() {
    let (session, old, _new) = moved_to_newer("newer-codex-no-fork", &CODEX).await;
    let restarted = restart(&session.fixture).await;
    let refused = restarted
        .registry
        .fork(fork_params(&session.id))
        .await
        .expect_err("Codex has no native fork");
    assert_eq!(refused.code, "agent_fork_unsupported", "{refused:?}");
    assert_eq!(pinned_digest(&session.store_path, &session.id), old.digest);
}

#[tokio::test]
async fn a_profile_still_pinned_to_the_launched_version_keeps_the_session_on_it() {
    let old = CLAUDE.older();
    let new = CLAUDE.newer();
    let session = stopped_on("newer-profile-kept", &CLAUDE, &old, CLAUDE.old_version).await;
    CLAUDE.write_agent(&session.agent, CLAUDE.only_new);
    install(&session.fixture, &CLAUDE, &new, CLAUDE.new_version, 6).await;

    let restarted = restart(&session.fixture).await;
    let refused = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect_err("the older version's probe refuses the agent");
    assert_eq!(refused.code, "agent_runtime_unsupported", "{refused:?}");
    assert_eq!(pinned_digest(&session.store_path, &session.id), old.digest);

    // Within the older range the session resumes on its own digest.
    CLAUDE.write_agent(&session.agent, CLAUDE.in_both);
    restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect("the session resumes on the version its profile pins");
    assert_eq!(pinned_digest(&session.store_path, &session.id), old.digest);
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

#[tokio::test]
async fn a_lower_selected_version_never_adopts_a_session_of_a_higher_one() {
    let old = CLAUDE.older();
    let new = CLAUDE.newer();
    let session = stopped_on("newer-no-downgrade", &CLAUDE, &new, CLAUDE.new_version).await;
    install(&session.fixture, &CLAUDE, &old, CLAUDE.old_version, 6).await;
    let selected = session
        .fixture
        .registry
        .package_select(PackageSelectParams {
            digest: old.digest.clone(),
        })
        .await
        .expect("select the lower version");
    assert!(selected.package.selected);
    pin_profile(&session.fixture.registry, &old.digest).await;

    let restarted = restart(&session.fixture).await;
    assert_eq!(
        stopped_verdict(&restarted, &session.store_path, &session.id).1,
        "agent_profile_changed",
        "the profile moved to a lower version, which does not adopt the session"
    );
    let refused = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect_err("no downgrade adoption");
    assert_eq!(refused.code, "agent_profile_changed", "{refused:?}");
    assert_eq!(pinned_digest(&session.store_path, &session.id), new.digest);
}

#[tokio::test]
async fn a_newer_version_with_another_reference_check_never_adopts_a_session() {
    assert_never_adopted_by("newer-other-check", &CLAUDE, |text| {
        assert!(
            text.contains("max_depth = 1"),
            "the descriptor bounds the search"
        );
        text.replacen("max_depth = 1", "max_depth = 2", 1)
    })
    .await;
}

#[tokio::test]
async fn a_newer_version_with_another_assigned_reference_never_adopts_a_session() {
    assert_never_adopted_by("newer-other-assignment", &PI, |text| {
        let assigned = "launch_args = [\"--session-id\", \"{reference}\"]";
        assert!(text.contains(assigned), "Pi assigns its reference");
        text.replacen(
            assigned,
            "launch_args = [\"--session-uuid\", \"{reference}\"]",
            1,
        )
    })
    .await;
}

#[tokio::test]
async fn a_local_package_of_a_newer_version_never_adopts_an_official_session() {
    let old = PI.older();
    let session = stopped_on("newer-local-pi", &PI, &old, PI.old_version).await;
    PI.write_agent(&session.agent, PI.only_new);
    // A package installed by explicit digest declares the official package id
    // at a newer version and is selected. Its record is local, never official.
    let local = PI.newer();
    let path = session.fixture.write_archive(&local);
    let installed = session
        .fixture
        .registry
        .package_install(explicit(&path, &local, true, true))
        .await
        .expect("a local package of the official package id installs");
    assert_eq!(
        installed.package.origin,
        protocol::PackageOrigin::ExplicitDigest
    );
    assert!(installed.package.selected, "{installed:?}");
    pin_profile(&session.fixture.registry, &local.digest).await;

    let restarted = restart(&session.fixture).await;
    let refused = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect_err("a local package never adopts the session");
    assert_eq!(refused.code, "agent_profile_changed", "{refused:?}");
    assert_eq!(pinned_digest(&session.store_path, &session.id), old.digest);
}

#[tokio::test]
async fn a_session_on_the_selected_version_is_not_reported_as_adopted() {
    let old = CLAUDE.older();
    let session = stopped_on("newer-same-digest", &CLAUDE, &old, CLAUDE.old_version).await;
    let restarted = restart(&session.fixture).await;
    assert_eq!(
        stopped_verdict(&restarted, &session.store_path, &session.id),
        (Verdict::Adoptable, CODE_ADOPTABLE.to_owned())
    );
}

/// The launch environment the test registry launches under, so a judgement
/// reads the same home and variables that resume does.
fn test_launch_environment() -> crate::runtime::LaunchEnvironment {
    crate::runtime::LaunchEnvironment::new(
        pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .map(|pattern| (*pattern).to_owned())
            .collect(),
        crate::test_support::thread_environment_source(),
    )
}

/// Writes a release `runtime/` tree at `release` whose catalog, signed by the
/// test root at `sequence`, lists `package` at `version` as the package of
/// `kind`.
fn write_release(release: &Path, kind: &Kind, package: &Package, version: &str, sequence: u64) {
    let packages = release.join("packages");
    fs::create_dir_all(&packages).expect("the release packages directory");
    fs::write(
        packages.join(format!("{}.tar.zst", kind.runtime)),
        &package.bytes,
    )
    .expect("the release archive");
    fs::write(
        release.join("runtime-catalog.json"),
        signed_by(
            &signing_key("root"),
            catalog(
                sequence,
                vec![entry(package, &kind.package_id(), kind.runtime, version)],
            ),
        ),
    )
    .expect("the release catalog");
}

#[tokio::test]
async fn the_release_step_repins_the_profile_and_the_preflight_judges_the_adoption_it_enables() {
    let old = CLAUDE.older();
    let new = CLAUDE.newer();
    let session = stopped_on("newer-release-step", &CLAUDE, &old, CLAUDE.old_version).await;
    CLAUDE.write_agent(&session.agent, CLAUDE.only_new);
    let release = session.fixture.dir.join("release").join("runtime");
    write_release(&release, &CLAUDE, &new, CLAUDE.new_version, 6);
    let trust =
        crate::catalog_anchor::CatalogTrust::Loaded(anchor(&[&signing_key("root")], Vec::new()));

    // The preflight judges the stopped session over the profile text the step
    // writes, which pins the release package; the file on disk still pins the
    // launched version.
    let report = crate::session::upgrade_preflight::run_with(
        &crate::session::upgrade_preflight::PreflightInputs {
            store_path: session.store_path.clone(),
            worker_state_root: session.fixture.dir.join("no-workers"),
            plugins_dir: session.fixture.plugins.clone(),
        },
        &crate::session::upgrade_preflight::PreflightOptions {
            probe_budget: pohunek_service_config::preflight::PROBE_BUDGET,
            entry: std::time::Instant::now(),
            release: Some(crate::session::upgrade_preflight::ReleaseInputs {
                release_dir: release.clone(),
                catalog_trust: trust.clone(),
            }),
            profiles_dir: Some(session.fixture.agents.clone()),
            host_state_dir: Some(session.fixture.dir.join("state")),
            launch: Ok(test_launch_environment()),
        },
        &HostInspector::new(),
    );
    let plan = report.runtimes.as_ref().expect("the release plan");
    assert!(plan.refusals.is_empty(), "{:?}", plan.refusals);
    let planned: Vec<_> = plan
        .profiles
        .iter()
        .map(|profile| {
            (
                profile.name.as_str(),
                profile.status,
                profile.previous_digest.clone(),
                profile.digest.clone(),
            )
        })
        .collect();
    assert_eq!(
        planned,
        [(
            PROFILE,
            ProfileStatus::Repin,
            Some(old.digest.as_str().to_owned()),
            new.digest.as_str().to_owned()
        )]
    );
    let verdict = report
        .stopped_sessions
        .iter()
        .find(|verdict| verdict.session_id == session.id.0)
        .unwrap_or_else(|| {
            panic!(
                "the stopped session is judged: {:?}",
                report.stopped_sessions
            )
        });
    assert_eq!(
        (verdict.verdict, verdict.code.as_str()),
        (Verdict::Adoptable, CODE_ADOPTABLE_BY_NEWER_PACKAGE),
        "{}",
        verdict.detail
    );

    let applied = crate::session::ensure_official_runtimes(&crate::session::EnsureInputs {
        store_path: session.store_path.clone(),
        plugins_dir: session.fixture.plugins.clone(),
        profiles_dir: session.fixture.agents.clone(),
        release_dir: release,
        catalog_trust: trust,
        dry_run: false,
        daemon_version: crate::DAEMON_VERSION.to_owned(),
        environment: Ok(test_launch_environment()),
    })
    .expect("the release step applies");
    assert!(applied.refusals.is_empty(), "{:?}", applied.refusals);
    assert_eq!(
        applied
            .profiles
            .iter()
            .map(|profile| (profile.name.as_str(), profile.status))
            .collect::<Vec<_>>(),
        [(PROFILE, ProfileStatus::Repinned)]
    );

    let restarted = restart(&session.fixture).await;
    restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect("the session resumes through the release package after the step");
    assert_eq!(pinned_digest(&session.store_path, &session.id), new.digest);
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}
