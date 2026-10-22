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
    stopped_from(tag, kind, launched, Some(version)).await
}

/// [`stopped_on`] for `launched` installed by its explicit digest, as a local
/// package that declares the official package id.
async fn stopped_on_local(tag: &str, kind: &Kind, launched: &Package) -> Stopped {
    stopped_from(tag, kind, launched, None).await
}

/// [`stopped_on`], installing `launched` through the catalog at `version`, or
/// by its explicit digest without one.
async fn stopped_from(
    tag: &str,
    kind: &Kind,
    launched: &Package,
    version: Option<&str>,
) -> Stopped {
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
    if let Some(version) = version {
        install(&fixture, kind, launched, version, 5).await;
    } else {
        let path = fixture.write_archive(launched);
        fixture
            .registry
            .package_install(explicit(&path, launched, true, true))
            .await
            .expect("a local package of the official package id installs");
    }
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

/// How the newer version of a fail-closed case differs, or how it is installed.
#[derive(Clone, Copy)]
enum Newer {
    /// The descriptor is edited by the function.
    Edited(fn(&str) -> String),
    /// The newer version is installed and selected, then disabled.
    Disabled,
    /// The newer version is installed without being selected.
    Unselected,
    /// The newer version is not a semantic version.
    Unparsable,
    /// The newer version differs from the launched one in build metadata only,
    /// which semantic version precedence ignores.
    Rebuilt,
}

/// What a resume of the stopped session does once its profile names the newer
/// version that does not adopt it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expected {
    /// It resumes from its frozen digest, as it would without the newer version.
    Frozen,
    /// It is refused as a changed profile, and stays on its frozen digest.
    ProfileChanged,
}

/// One newer version that never adopts a stopped session.
struct FailClosed {
    name: &'static str,
    kind: &'static Kind,
    newer: Newer,
    expected: Expected,
}

/// A package version every validator accepts that is not a semantic version.
const UNPARSABLE_VERSION: &str = "1.0.2.1";

fn deeper_reference_search(text: &str) -> String {
    assert!(
        text.contains("max_depth = 1"),
        "the descriptor bounds the search"
    );
    text.replacen("max_depth = 1", "max_depth = 2", 1)
}

fn path_reference_kind(text: &str) -> String {
    assert!(
        text.contains("reference_kind = \"id\""),
        "Claude resumes by id"
    );
    text.replacen("reference_kind = \"id\"", "reference_kind = \"path\"", 1)
}

fn without_fork(text: &str) -> String {
    let forking =
        "[fork]\nsupported = true\nargs = [\"--resume\", \"{reference}\", \"--fork-session\"]\n";
    assert!(text.contains(forking), "Claude forks");
    text.replacen(forking, "[fork]\nsupported = false\n", 1)
}

fn other_assignment(text: &str) -> String {
    let assigned = "launch_args = [\"--session-id\", \"{reference}\"]";
    assert!(text.contains(assigned), "Pi assigns its reference");
    text.replacen(
        assigned,
        "launch_args = [\"--session-uuid\", \"{reference}\"]",
        1,
    )
}

/// Installs the newer version of `case` beside the launched one and returns its
/// digest, as the step or the owner would have installed it.
async fn install_newer(fixture: &Fixture, case: &FailClosed) -> PackageDigest {
    let kind = case.kind;
    let (package, version, select) = match case.newer {
        Newer::Edited(edit) => (
            kind.package(kind.new_version, kind.new_below, |text| edit(&text)),
            kind.new_version.to_owned(),
            true,
        ),
        Newer::Disabled => (kind.newer(), kind.new_version.to_owned(), true),
        Newer::Unselected => (kind.newer(), kind.new_version.to_owned(), false),
        Newer::Unparsable => (
            kind.package(UNPARSABLE_VERSION, kind.new_below, |text| text),
            UNPARSABLE_VERSION.to_owned(),
            true,
        ),
        Newer::Rebuilt => {
            let rebuilt = format!("{}+rebuild", kind.old_version);
            (
                kind.package(&rebuilt, kind.new_below, |text| text),
                rebuilt,
                true,
            )
        }
    };
    let document = signed_by(
        &signing_key("root"),
        catalog(
            6,
            vec![entry(&package, &kind.package_id(), kind.runtime, &version)],
        ),
    );
    let mut params = catalog_install(fixture, &package, &document, false);
    params.select = select;
    let installed = fixture
        .registry
        .package_install(params)
        .await
        .unwrap_or_else(|error| panic!("{}: the newer version installs: {error:?}", case.name));
    assert_eq!(installed.package.selected, select, "{installed:?}");
    if matches!(case.newer, Newer::Disabled) {
        fixture
            .registry
            .package_set_enabled(protocol::PackageSetEnabledParams {
                digest: package.digest.clone(),
                enabled: false,
            })
            .await
            .expect("disable the newer version");
    }
    package.digest
}

/// Each newer version that does not adopt a stopped session leaves it on its
/// frozen digest, even after its profile was moved to that version, as the
/// release step moves it. A strictly newer official version of the same
/// package that only fails the adoption contract resumes the session from the
/// frozen digest, as before the move; a version that cannot be ordered is a
/// changed profile.
#[tokio::test]
async fn a_newer_version_that_does_not_adopt_keeps_the_session_on_its_frozen_digest() {
    let cases = [
        FailClosed {
            name: "another native reference check",
            kind: &CLAUDE,
            newer: Newer::Edited(deeper_reference_search),
            expected: Expected::Frozen,
        },
        FailClosed {
            name: "another reference kind",
            kind: &CODEX,
            newer: Newer::Edited(path_reference_kind),
            expected: Expected::Frozen,
        },
        FailClosed {
            name: "no fork where the frozen launch forks",
            kind: &CLAUDE,
            newer: Newer::Edited(without_fork),
            expected: Expected::Frozen,
        },
        FailClosed {
            name: "another assigned reference template",
            kind: &PI,
            newer: Newer::Edited(other_assignment),
            expected: Expected::Frozen,
        },
        FailClosed {
            name: "a disabled newer version",
            kind: &CLAUDE,
            newer: Newer::Disabled,
            expected: Expected::Frozen,
        },
        FailClosed {
            name: "an unselected newer version",
            kind: &CLAUDE,
            newer: Newer::Unselected,
            expected: Expected::Frozen,
        },
        FailClosed {
            name: "the launched version with other build metadata",
            kind: &CLAUDE,
            newer: Newer::Rebuilt,
            expected: Expected::ProfileChanged,
        },
        FailClosed {
            name: "a version that is not a semantic version",
            kind: &CLAUDE,
            newer: Newer::Unparsable,
            expected: Expected::ProfileChanged,
        },
    ];
    for (index, case) in cases.iter().enumerate() {
        assert_fail_closed(index, case).await;
    }
}

/// Runs the fail-closed `case` as the `index`th: the profile of a stopped
/// session moves to the newer version, and the resume does what `case` expects.
async fn assert_fail_closed(index: usize, case: &FailClosed) {
    let kind = case.kind;
    let old = kind.older();
    let session = stopped_on(
        &format!("newer-fail-closed-{index}"),
        kind,
        &old,
        kind.old_version,
    )
    .await;
    let newer = install_newer(&session.fixture, case).await;
    pin_profile(&session.fixture.registry, &newer).await;

    let restarted = restart(&session.fixture).await;
    let verdict = stopped_verdict(&restarted, &session.store_path, &session.id);
    let resumed = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await;
    match case.expected {
        Expected::Frozen => {
            assert_eq!(
                verdict,
                (Verdict::Adoptable, CODE_ADOPTABLE.to_owned()),
                "{}",
                case.name
            );
            let resumed = resumed.unwrap_or_else(|error| {
                panic!(
                    "{}: the session resumes from its frozen digest: {error:?}",
                    case.name
                )
            });
            assert_eq!(
                resumed.runtime.as_ref().map(|runtime| runtime.state),
                Some(protocol::RuntimeState::Live),
                "{}",
                case.name
            );
            assert_eq!(
                pinned_digest(&session.store_path, &session.id),
                old.digest,
                "{}: the relaunch keeps the frozen digest",
                case.name
            );
            restarted
                .registry
                .remove(&session.id)
                .await
                .expect("remove the resumed session");
        }
        Expected::ProfileChanged => {
            assert_eq!(verdict.1, "agent_profile_changed", "{}", case.name);
            let refused = resumed.expect_err(case.name);
            assert_eq!(
                refused.code, "agent_profile_changed",
                "{}: {refused:?}",
                case.name
            );
            assert_eq!(
                pinned_digest(&session.store_path, &session.id),
                old.digest,
                "{}: the session keeps its frozen digest",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn a_newer_official_version_never_adopts_a_session_of_a_local_package() {
    let local = PI.older();
    let session = stopped_on_local("newer-local-launch", &PI, &local).await;
    let newer = PI.newer();
    install(&session.fixture, &PI, &newer, PI.new_version, 6).await;
    pin_profile(&session.fixture.registry, &newer.digest).await;

    let restarted = restart(&session.fixture).await;
    let refused = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect_err("an official version never adopts a local package's session");
    assert_eq!(refused.code, "agent_profile_changed", "{refused:?}");
    assert_eq!(
        pinned_digest(&session.store_path, &session.id),
        local.digest
    );
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
    assert_release_step("newer-release-step", CLAUDE.newer(), true).await;
}

/// The release package does not adopt the session (another native reference
/// check). The step still repins the profile, and the preflight judges the
/// session resumable from its frozen digest before the package is installed,
/// as resume then resumes it.
#[tokio::test]
async fn the_release_step_repins_the_profile_of_a_session_its_package_does_not_adopt() {
    let changed = CLAUDE.package(CLAUDE.new_version, CLAUDE.new_below, |text| {
        deeper_reference_search(&text)
    });
    assert_release_step("newer-release-step-frozen", changed, false).await;
}

/// The upgrade preflight of `session`'s host with the release step of
/// `release`, judged over the host profiles and revision key the daemon uses.
fn release_preflight(
    session: &Stopped,
    release: &Path,
    trust: &crate::catalog_anchor::CatalogTrust,
) -> pohunek_service_config::preflight::PreflightReport {
    crate::session::upgrade_preflight::run_with(
        &crate::session::upgrade_preflight::PreflightInputs {
            store_path: session.store_path.clone(),
            worker_state_root: session.fixture.dir.join("no-workers"),
            plugins_dir: session.fixture.plugins.clone(),
        },
        &crate::session::upgrade_preflight::PreflightOptions {
            probe_budget: pohunek_service_config::preflight::PROBE_BUDGET,
            entry: std::time::Instant::now(),
            release: Some(crate::session::upgrade_preflight::ReleaseInputs {
                release_dir: release.to_path_buf(),
                catalog_trust: trust.clone(),
            }),
            profiles_dir: Some(session.fixture.agents.clone()),
            host_state_dir: Some(session.fixture.dir.join("state")),
            launch: Ok(test_launch_environment()),
        },
        &HostInspector::new(),
    )
}

/// Applies the release step of `release` to `session`'s host.
fn apply_release_step(
    session: &Stopped,
    release: PathBuf,
    trust: crate::catalog_anchor::CatalogTrust,
) -> pohunek_service_config::runtimes::EnsureReport {
    crate::session::ensure_official_runtimes(&crate::session::EnsureInputs {
        store_path: session.store_path.clone(),
        plugins_dir: session.fixture.plugins.clone(),
        profiles_dir: session.fixture.agents.clone(),
        release_dir: release,
        catalog_trust: trust,
        dry_run: false,
        daemon_version: crate::DAEMON_VERSION.to_owned(),
        environment: Ok(test_launch_environment()),
    })
    .expect("the release step applies")
}

async fn assert_release_step(tag: &str, new: Package, adopts: bool) {
    assert_release_step_of(&CLAUDE, tag, new, adopts).await;
}

/// Pi is not a reserved runtime: the release step keeps its installed official
/// package current only because it is installed, selected and enabled. The
/// profile of a stopped Pi session moves to the release's version, and the
/// preflight judges the adoption that enables.
#[tokio::test]
async fn a_stopped_pi_session_is_adopted_by_the_updated_package() {
    assert_release_step_of(&PI, "newer-release-step-pi", PI.newer(), true).await;
}

/// The release step plans the release package `new` of `kind` for a stopped
/// session launched from the older version, and its profile is repinned. When
/// `adopts`, the preflight names the adoption and the resume runs through
/// `new`; otherwise the preflight and the resume keep the frozen digest.
async fn assert_release_step_of(kind: &Kind, tag: &str, new: Package, adopts: bool) {
    let old = kind.older();
    let session = stopped_on(tag, kind, &old, kind.old_version).await;
    let agent = if adopts { kind.only_new } else { kind.in_both };
    kind.write_agent(&session.agent, agent);
    let release = session.fixture.dir.join("release").join("runtime");
    write_release(&release, kind, &new, kind.new_version, 6);
    let trust =
        crate::catalog_anchor::CatalogTrust::Loaded(anchor(&[&signing_key("root")], Vec::new()));

    // The preflight judges the stopped session over the profile text the step
    // writes, which pins the release package; the file on disk still pins the
    // launched version.
    let report = release_preflight(&session, &release, &trust);
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
        (
            Verdict::Adoptable,
            if adopts {
                CODE_ADOPTABLE_BY_NEWER_PACKAGE
            } else {
                CODE_ADOPTABLE
            }
        ),
        "{}",
        verdict.detail
    );

    let applied = apply_release_step(&session, release, trust);
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
        .expect("the session resumes after the step");
    let relaunched = if adopts { &new.digest } else { &old.digest };
    assert_eq!(&pinned_digest(&session.store_path, &session.id), relaunched);
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

/// Records `package` of `kind` in the registry of `session`'s host with
/// `source`, as a registry written outside the daemon's install path would
/// hold it (the install path refuses a reserved runtime id for a local
/// package). The package is enabled and not selected.
fn seed_record(
    session: &Stopped,
    package: &Package,
    version_package: &str,
    source: package::registry::PackageSource,
) {
    let limits = package::Limits::DEFAULT;
    let entries = package::read_archive(&package.bytes, &limits)
        .expect("the fixture archive reads")
        .entries()
        .to_vec();
    let protocol::BindingProvenance::Package {
        package: identity, ..
    } = crate::agent::host::definition_from_archive(&entries, &package.digest)
        .expect("the official package parses")
        .binding()
        .provenance
        .clone()
    else {
        panic!("{version_package} has package provenance");
    };
    package::registry::Registry::open_at(&session.fixture.plugins, limits)
        .expect("open the registry")
        .install(&package::registry::InstallRequest {
            archive: &package.bytes,
            expected: &package.digest,
            identity,
            source,
            enabled: true,
            select: false,
            installed_at_unix_seconds: 1,
        })
        .expect("seed the record");
}

/// A profile pinned to the release package, a registry record of that package
/// with `source`, and the release step planning it: the preflight verdict and
/// the resume outcome must be the same judgement. `adopts` says whether the
/// release package serves the session.
async fn assert_release_record(tag: &str, source: package::registry::PackageSource, adopts: bool) {
    let old = CLAUDE.older();
    let new = CLAUDE.newer();
    let session = stopped_on(tag, &CLAUDE, &old, CLAUDE.old_version).await;
    CLAUDE.write_agent(&session.agent, CLAUDE.only_new);
    seed_record(&session, &new, "claude", source);
    let bound = fs::read_to_string(session.fixture.agents.join(format!("{PROFILE}.toml")))
        .expect("read the bound profile");
    assert!(bound.contains(old.digest.as_str()), "{bound}");
    session.fixture.write_profile(
        PROFILE,
        &bound.replace(old.digest.as_str(), new.digest.as_str()),
    );
    let release = session.fixture.dir.join("release").join("runtime");
    write_release(&release, &CLAUDE, &new, CLAUDE.new_version, 6);
    let trust =
        crate::catalog_anchor::CatalogTrust::Loaded(anchor(&[&signing_key("root")], Vec::new()));

    let report = release_preflight(&session, &release, &trust);
    let verdict = report
        .stopped_sessions
        .iter()
        .find(|verdict| verdict.session_id == session.id.0)
        .unwrap_or_else(|| panic!("the session is judged: {:?}", report.stopped_sessions));
    let judged = (verdict.verdict, verdict.code.clone());

    // The step refuses to restore a package whose record is not official; the
    // resume is judged over whatever it left.
    let _ = crate::session::ensure_official_runtimes(&crate::session::EnsureInputs {
        store_path: session.store_path.clone(),
        plugins_dir: session.fixture.plugins.clone(),
        profiles_dir: session.fixture.agents.clone(),
        release_dir: release,
        catalog_trust: trust,
        dry_run: false,
        daemon_version: crate::DAEMON_VERSION.to_owned(),
        environment: Ok(test_launch_environment()),
    });
    let restarted = restart(&session.fixture).await;
    let resumed = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await;
    if adopts {
        assert_eq!(
            judged,
            (
                Verdict::Adoptable,
                CODE_ADOPTABLE_BY_NEWER_PACKAGE.to_owned()
            ),
            "{}",
            verdict.detail
        );
        resumed.expect("the release package adopts the session");
        assert_eq!(pinned_digest(&session.store_path, &session.id), new.digest);
    } else {
        let refused = resumed.expect_err("a non-official record is not an official pin");
        assert_eq!(refused.code, "agent_profile_changed", "{refused:?}");
        assert_eq!(
            judged,
            (Verdict::WouldLoseRecovery, refused.code.clone()),
            "the preflight judges what resume refuses: {}",
            verdict.detail
        );
    }
}

#[tokio::test]
async fn a_planned_digest_with_an_official_record_is_the_official_newer_pin() {
    assert_release_record(
        "newer-record-official",
        package::registry::PackageSource::Official,
        true,
    )
    .await;
}

#[tokio::test]
async fn a_planned_digest_with_an_explicit_digest_record_is_not_the_official_newer_pin() {
    assert_release_record(
        "newer-record-explicit",
        package::registry::PackageSource::ExplicitDigest,
        false,
    )
    .await;
}

/// Arms `registry` to park its next resume or fork once its recovery
/// definition is resolved.
fn arm_definition_hold(registry: &SessionRegistry) -> crate::runtime::lifecycle::tests::StartGate {
    let gate = crate::runtime::lifecycle::tests::StartGate {
        session_id: None,
        entered: std::sync::Arc::new(tokio::sync::Notify::new()),
        release: std::sync::Arc::new(tokio::sync::Notify::new()),
    };
    *registry
        .inner
        .definition_hold
        .lock()
        .expect("definition hold is never poisoned") = Some(gate.clone());
    gate
}

/// Rewrites the bound [`PROFILE`] file from the `old` digest to the `new` one,
/// as the release step does from another process.
fn repin_profile_file(fixture: &Fixture, old: &Package, new: &Package) {
    let bound = fs::read_to_string(fixture.agents.join(format!("{PROFILE}.toml")))
        .expect("read the bound profile");
    assert!(bound.contains(old.digest.as_str()), "{bound}");
    fixture.write_profile(
        PROFILE,
        &bound.replace(old.digest.as_str(), new.digest.as_str()),
    );
}

/// The profile revision that describes a launch of the exact digest `old`
/// under the profile file as it is now.
fn revision_on(
    registry: &SessionRegistry,
    store_path: &Path,
    id: &SessionId,
) -> crate::agent::host::ProfileRevision {
    let binding = durable_recovery(store_path, id);
    let launched = registry
        .inner
        .profiles
        .runtimes()
        .resolve_pinned(&binding.agent_base, &binding.launch_binding)
        .expect("the launched digest still resolves");
    let agent = registry
        .inner
        .profiles
        .resolve_agent_pinned(PROFILE, &launched)
        .expect("the profile resolves on the launched digest");
    registry
        .inner
        .profiles
        .revision_of(&agent)
        .expect("revision key")
        .expect("a profile has a revision")
}

/// A resume or fork (`fork`) of a session on the older version resolves its
/// recovery definition once. The profile is repinned to the newer version, which
/// would adopt the session, after the definition resolved: the operation
/// launches the digest it resolved, and the profile revision it records
/// describes that digest.
async fn assert_repin_during_recovery(tag: &str, fork: bool) {
    let old = CLAUDE.older();
    let new = CLAUDE.newer();
    let session = stopped_on(tag, &CLAUDE, &old, CLAUDE.old_version).await;
    install(&session.fixture, &CLAUDE, &new, CLAUDE.new_version, 6).await;
    let restarted = restart(&session.fixture).await;
    let gate = arm_definition_hold(&restarted.registry);
    let task = tokio::spawn({
        let registry = restarted.registry.clone();
        let id = session.id.clone();
        async move {
            if fork {
                registry.fork(fork_params(&id)).await
            } else {
                registry.resume_with(&id, ProfileChange::Refuse).await
            }
        }
    });
    gate.entered.notified().await;
    repin_profile_file(&session.fixture, &old, &new);
    gate.release.notify_one();
    let recovered = task
        .await
        .expect("recovery task")
        .expect("the operation completes on the definition it resolved");

    let id = if fork { &recovered.id } else { &session.id };
    assert_eq!(
        pinned_digest(&session.store_path, id),
        old.digest,
        "the relaunch records the digest it resolved"
    );
    assert_eq!(
        durable_recovery(&session.store_path, id).profile_revision,
        Some(revision_on(&restarted.registry, &session.store_path, id)),
        "the recorded profile revision describes the digest that launched"
    );
    // The repin is part of the profile now: stopping and recovering the session
    // again needs no further decision and adopts the newer version.
    if fork {
        let report = native_report_params(
            &restarted.registry,
            id.clone(),
            CLAUDE.runtime.to_owned(),
            CONVERSATION.to_owned(),
            None,
        )
        .await;
        assert!(restarted.registry.report_as_root(report).await.recorded);
    }
    restarted.registry.stop(id).await.expect("stop the session");
    restarted
        .registry
        .resume_with(id, ProfileChange::Refuse)
        .await
        .expect("the next recovery matches the revision the race recorded");
    assert_eq!(pinned_digest(&session.store_path, id), new.digest);
    restarted
        .registry
        .remove(id)
        .await
        .expect("remove the session");
}

/// A metadata edit made while a resume relaunches is in the reply, the session
/// and the record the relaunch commits.
#[tokio::test]
async fn a_metadata_edit_during_a_resume_survives_the_relaunch() {
    let old = CLAUDE.older();
    let session = stopped_on("meta-during-resume", &CLAUDE, &old, CLAUDE.old_version).await;
    let restarted = restart(&session.fixture).await;
    let gate = arm_definition_hold(&restarted.registry);
    let task = tokio::spawn({
        let registry = restarted.registry.clone();
        let id = session.id.clone();
        async move { registry.resume_with(&id, ProfileChange::Refuse).await }
    });
    gate.entered.notified().await;
    restarted
        .registry
        .set_metadata(
            &session.id,
            std::collections::BTreeMap::from([("ticket".to_owned(), Some("new".to_owned()))]),
        )
        .await
        .expect("edit the metadata");
    gate.release.notify_one();
    let recovered = task
        .await
        .expect("recovery task")
        .expect("the resume completes");

    let expected = std::collections::BTreeMap::from([("ticket".to_owned(), "new".to_owned())]);
    assert_eq!(recovered.metadata, expected, "the reply carries the edit");
    assert_eq!(
        restarted
            .registry
            .inspect(&session.id)
            .await
            .expect("inspect")
            .metadata,
        expected
    );
    let durable = crate::store::Store::new(session.store_path.clone())
        .load_sessions()
        .expect("read the durable records")
        .into_iter()
        .find(|record| record.session_id == session.id.0)
        .expect("the session has a durable record");
    assert_eq!(durable.info.metadata, expected);
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the session");
}

#[tokio::test]
async fn a_repin_during_a_resume_leaves_the_binding_and_the_profile_revision_consistent() {
    assert_repin_during_recovery("repin-during-resume", false).await;
}

#[tokio::test]
async fn a_repin_during_a_fork_leaves_the_binding_and_the_profile_revision_consistent() {
    assert_repin_during_recovery("repin-during-fork", true).await;
}

/// A stopped session of the bare Claude runtime (no host profile) launched from
/// `launched`. The agent is a fake `claude` the registry finds through its
/// launch `PATH`, ahead of any real one.
async fn stopped_bare(tag: &str, launched: &Package, version: &str) -> Stopped {
    let bin = crate::test_support::thread_scoped_dir("bare-launch-path-");
    crate::test_support::prepend_thread_launch_path(&bin);
    let agent = bin.join("claude");
    CLAUDE.write_agent(&agent, CLAUDE.in_both);
    let store_path = temp_store_path(tag);
    let fixture = Fixture::with_anchor_and_store(
        &format!("{tag}-plugins"),
        anchor(&[&signing_key("root")], Vec::new()),
        store_path.clone(),
    );
    install(&fixture, &CLAUDE, launched, version, 5).await;
    let created = fixture
        .registry
        .create(protocol::SessionNewParams {
            agent: RuntimeId::CLAUDE.to_owned(),
            cwd: Some(temp_dir(tag)),
            ..params()
        })
        .await
        .expect("a bare session of the older package");
    let conversations = crate::test_support::thread_environment_home()
        .join(".claude")
        .join("projects")
        .join("work");
    fs::create_dir_all(&conversations).expect("the conversation directory");
    fs::write(conversations.join(format!("{CONVERSATION}.jsonl")), b"{}\n")
        .expect("the conversation");
    let report = native_report_params(
        &fixture.registry,
        created.id.clone(),
        RuntimeId::CLAUDE.to_owned(),
        CONVERSATION.to_owned(),
        None,
    )
    .await;
    assert!(fixture.registry.report_as_root(report).await.recorded);
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

/// A bare-runtime session whose agent left the older range, with the newer
/// version installed and selected.
async fn bare_moved_to_newer(tag: &str) -> (Stopped, Package, Package) {
    let old = CLAUDE.older();
    let new = CLAUDE.newer();
    let session = stopped_bare(tag, &old, CLAUDE.old_version).await;
    CLAUDE.write_agent(&session.agent, CLAUDE.only_new);
    install(&session.fixture, &CLAUDE, &new, CLAUDE.new_version, 6).await;
    (session, old, new)
}

#[tokio::test]
async fn a_newer_package_version_adopts_a_stopped_bare_runtime_session() {
    let (session, _old, new) = bare_moved_to_newer("bare-adopts-resume").await;
    let restarted = restart(&session.fixture).await;
    assert_eq!(
        stopped_verdict(&restarted, &session.store_path, &session.id),
        (
            Verdict::Adoptable,
            CODE_ADOPTABLE_BY_NEWER_PACKAGE.to_owned()
        ),
        "the preflight names the adoption the resume performs"
    );
    restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect("the newer version adopts the bare session");
    assert_eq!(pinned_digest(&session.store_path, &session.id), new.digest);
    restarted
        .registry
        .remove(&session.id)
        .await
        .expect("remove the resumed session");
}

#[tokio::test]
async fn a_newer_package_version_adopts_a_fork_of_a_stopped_bare_runtime_session() {
    let (session, old, new) = bare_moved_to_newer("bare-adopts-fork").await;
    let restarted = restart(&session.fixture).await;
    let forked = restarted
        .registry
        .fork(fork_params(&session.id))
        .await
        .expect("the newer version adopts the fork");
    assert_eq!(pinned_digest(&session.store_path, &forked.id), new.digest);
    assert_eq!(pinned_digest(&session.store_path, &session.id), old.digest);
    restarted
        .registry
        .remove(&forked.id)
        .await
        .expect("remove the fork");
}

/// A session of a host profile whose frozen revision outlives the profile file
/// is not a bare-runtime session: the newer version does not adopt it.
#[tokio::test]
async fn a_newer_package_version_does_not_adopt_a_session_whose_frozen_profile_is_gone() {
    let (session, old, _new) = moved_to_newer("bare-profile-gone", &CLAUDE).await;
    fs::remove_file(session.fixture.agents.join(format!("{PROFILE}.toml")))
        .expect("delete the profile");
    let restarted = restart(&session.fixture).await;
    let binding = durable_recovery(&session.store_path, &session.id);
    let resolved =
        crate::session::resume::recovery_definition(&restarted.registry.inner.profiles, &binding)
            .expect("the frozen digest still resolves");
    assert!(
        matches!(
            &resolved.binding().provenance,
            BindingProvenance::Package { package_digest, .. } if *package_digest == old.digest
        ),
        "the session stays on its frozen digest"
    );
    let refused = restarted
        .registry
        .resume_with(&session.id, ProfileChange::Refuse)
        .await
        .expect_err("the deleted profile cannot be resumed under");
    assert_eq!(refused.code, "agent_profile_missing", "{refused:?}");
    assert_eq!(
        stopped_verdict(&restarted, &session.store_path, &session.id),
        (
            Verdict::WouldLoseRecovery,
            "agent_profile_missing".to_owned()
        ),
        "the preflight judges what resume refuses"
    );
}
