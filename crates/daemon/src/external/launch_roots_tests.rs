//! Tests of the transcript roots the observer derives from the host's config
//! homes.
//!
//! The pure derivation runs over a rig with its own `HOME`, agents directory
//! and base environment; the watcher tests drive the real backend through
//! `start_transcript_index_with` and wait on the observer's wake-up signal, never
//! on a fixed delay.

// Rust guideline compliant 2026-10-12

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pohunek_test_support::process_env::ProcessEnv;
use pohunek_worker_protocol::DEFAULT_ENVIRONMENT_ALLOWLIST;
use protocol::{ErrorClass, ProtocolError, RuntimeRef};

use super::watch::RootProvider;
use super::{
    observed_roots, start_transcript_index_with, ExternalSessions, LaunchHomeRoots, SkippedHome,
    TranscriptRoot, WatcherHandle,
};
use crate::agent::host::fixture::{install_official_packages, official_host, pin_official_base};
use crate::agent::host::{BuiltinSource, PackageSource, PackageStore, RuntimeHost};
use crate::agent::host::{HeadPaths, TranscriptParser};
use crate::agent::ProfileRegistry;
use crate::integration::ConfigHomes;
use crate::runtime::environment::{base_environment, EnvironmentSource};
use crate::test_support::{scoped_dir, ScopedDir};

/// Upper bound on waiting for the watcher to follow a change; a healthy run
/// finishes in a few passes.
const FOLLOW_TIMEOUT: Duration = Duration::from_secs(30);
/// Interval at which a waiting test asks for another pass.
const NUDGE_TICK: Duration = Duration::from_millis(200);

/// A private root holding the user's home directory, the host profiles and the
/// config homes the profiles name.
struct Rig {
    root: ScopedDir,
    homes: ConfigHomes,
}

impl Rig {
    fn new() -> Self {
        Self::with_daemon_env(&[])
    }

    /// A rig whose daemon environment holds `daemon_env` on top of `HOME`;
    /// only what the default allowlist forwards reaches the launch base.
    fn with_daemon_env(daemon_env: &[(&str, &Path)]) -> Self {
        Self::build(daemon_env, |_root| official_host())
    }

    /// A rig whose host serves the built-ins, the official Codex and Claude
    /// packages and the packages `install` puts into the plugin root it is
    /// given.
    fn with_packages(install: impl FnOnce(&Path)) -> Self {
        Self::with_packages_and_env(&[], install)
    }

    /// [`Self::with_packages`] over a daemon environment holding `daemon_env`
    /// on top of `HOME`.
    fn with_packages_and_env(daemon_env: &[(&str, &Path)], install: impl FnOnce(&Path)) -> Self {
        Self::build(daemon_env, |root| {
            let plugins = root.join("plugins");
            install_official_packages(&plugins);
            install(&plugins);
            RuntimeHost::with_packages(
                BuiltinSource::new("/bin/sh"),
                PackageSource::new(PackageStore::open(&plugins).expect("open the package store")),
            )
            .expect("the host builds")
        })
    }

    fn build(daemon_env: &[(&str, &Path)], host: impl FnOnce(&Path) -> RuntimeHost) -> Self {
        let root = scoped_dir("pohunek-roots-");
        let home = root.join("home");
        fs::create_dir_all(&home).expect("create the user home");
        fs::create_dir_all(root.join("agents")).expect("create the agents directory");
        let mut variables = vec![(OsString::from("HOME"), home.as_os_str().to_owned())];
        variables.extend(
            daemon_env
                .iter()
                .map(|(name, value)| (OsString::from(name), value.as_os_str().to_owned())),
        );
        let base = base_environment(
            DEFAULT_ENVIRONMENT_ALLOWLIST,
            &EnvironmentSource::fixed(variables),
        )
        .expect("a base environment");
        let registry = ProfileRegistry::with_runtimes(Some(root.join("agents")), host(&root));
        Self {
            homes: ConfigHomes::new(registry, base),
            root,
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// A config home below the rig root whose transcript tree `subdir` exists.
    fn used_home(&self, name: &str, subdir: &str) -> PathBuf {
        let home = self.root.join(name);
        fs::create_dir_all(home.join(subdir)).expect("create a used config home");
        home
    }

    fn profile(&self, name: &str, base: &str, variable: &str, home: &Path) {
        fs::write(
            self.root.join("agents").join(format!("{name}.toml")),
            pin_official_base(&format!(
                "base = \"{base}\"\n[env]\n{variable} = \"{}\"\n",
                home.display()
            )),
        )
        .expect("write a profile");
    }

    fn claude_profile(&self, name: &str, home: &Path) {
        self.profile(name, "claude", "CLAUDE_CONFIG_DIR", home);
    }

    /// The roots the observer would watch right now.
    fn roots(&self) -> Vec<TranscriptRoot> {
        observed_roots(self.homes.launch_homes()).0
    }

    fn skipped(&self) -> BTreeSet<SkippedHome> {
        observed_roots(self.homes.launch_homes()).1
    }

    /// A provider over this rig, as production builds one over a registry.
    fn provider(&self) -> LaunchHomeRoots {
        let homes = self.homes.clone();
        LaunchHomeRoots::from_source(Arc::new(move || Ok(homes.clone())))
    }
}

/// The `(agent, path)` pairs of `roots`.
fn pairs(roots: &[TranscriptRoot]) -> BTreeSet<(String, PathBuf)> {
    roots
        .iter()
        .map(|root| (root.agent_base.as_wire().to_owned(), root.path.clone()))
        .collect()
}

fn root(agent: &RuntimeRef, path: PathBuf) -> (String, PathBuf) {
    (agent.as_wire().to_owned(), path)
}

#[test]
fn every_distinct_home_of_the_host_is_a_root() {
    let rig = Rig::new();
    fs::create_dir_all(rig.home().join(".claude/projects")).expect("ambient Claude home");
    fs::create_dir_all(rig.home().join(".codex/sessions")).expect("ambient Codex home");
    let work = rig.used_home("work-home", "projects");
    let personal = rig.used_home("personal-home", "sessions");
    rig.claude_profile("work", &work);
    rig.profile("personal", "codex", "CODEX_HOME", &personal);

    assert_eq!(
        pairs(&rig.roots()),
        BTreeSet::from([
            root(&RuntimeRef::claude(), rig.home().join(".claude/projects")),
            root(&RuntimeRef::codex(), rig.home().join(".codex/sessions")),
            root(&RuntimeRef::claude(), work.join("projects")),
            root(&RuntimeRef::codex(), personal.join("sessions")),
        ])
    );
}

#[test]
fn a_runtime_home_is_a_root_before_its_transcript_tree_exists() {
    let rig = Rig::new();

    assert_eq!(
        pairs(&rig.roots()),
        BTreeSet::from([
            root(&RuntimeRef::claude(), rig.home().join(".claude/projects")),
            root(&RuntimeRef::codex(), rig.home().join(".codex/sessions")),
        ])
    );
    assert!(rig.skipped().is_empty());
}

#[test]
fn a_profile_added_later_is_a_root_and_a_removed_one_is_dropped() {
    let rig = Rig::new();
    let before = pairs(&rig.roots());
    let late = rig.used_home("late-home", "projects");

    rig.claude_profile("late", &late);
    let added = pairs(&rig.roots());
    assert_eq!(
        added.difference(&before).collect::<Vec<_>>(),
        [&root(&RuntimeRef::claude(), late.join("projects"))]
    );

    fs::remove_file(rig.root.join("agents/late.toml")).expect("remove the profile");
    assert_eq!(pairs(&rig.roots()), before);
}

#[test]
fn an_edited_profile_moves_its_root() {
    let rig = Rig::new();
    let first = rig.used_home("first-home", "projects");
    let second = rig.used_home("second-home", "projects");
    rig.claude_profile("work", &first);
    assert!(pairs(&rig.roots()).contains(&root(&RuntimeRef::claude(), first.join("projects"))));

    rig.claude_profile("work", &second);
    let roots = pairs(&rig.roots());

    assert!(roots.contains(&root(&RuntimeRef::claude(), second.join("projects"))));
    assert!(!roots.contains(&root(&RuntimeRef::claude(), first.join("projects"))));
}

#[test]
fn homes_that_resolve_to_one_directory_yield_one_root() {
    let rig = Rig::new();
    let real = rig.used_home("real-home", "projects");
    let alias = rig.root.join("alias-home");
    symlink(&real, &alias).expect("symlink the home");
    let ambient = rig.home().join(".claude");
    fs::create_dir_all(ambient.join("projects")).expect("ambient home");
    rig.claude_profile("a-direct", &real);
    rig.claude_profile("b-aliased", &alias);
    rig.claude_profile("c-ambient", &ambient);

    let roots = rig.roots();
    let claude: Vec<_> = roots
        .iter()
        .filter(|root| root.agent_base == RuntimeRef::claude())
        .collect();

    // The ambient home (the runtime's own) and the shared real home.
    assert_eq!(
        claude.len(),
        2,
        "{:?}",
        claude.iter().map(|root| &root.path).collect::<Vec<_>>()
    );
    assert!(claude.iter().any(|root| root.path == real.join("projects")));
}

#[test]
fn a_profile_without_a_transcript_tree_or_a_resolvable_home_is_skipped() {
    let rig = Rig::new();
    let fresh = rig.root.join("fresh-home");
    fs::create_dir_all(&fresh).expect("a home that was never used");
    rig.claude_profile("fresh", &fresh);
    fs::write(
        rig.root.join("agents/tilde.toml"),
        pin_official_base("base = \"claude\"\n[env]\nCLAUDE_CONFIG_DIR = \"~/accounts/work\"\n"),
    )
    .expect("write a profile");

    let (roots, skipped) = observed_roots(rig.homes.launch_homes());

    assert!(roots
        .iter()
        .all(|root| !root.path.starts_with(&fresh) && !root.path.starts_with("~")));
    assert_eq!(
        skipped
            .iter()
            .map(|home| (home.profile.as_deref(), home.code.is_some()))
            .collect::<Vec<_>>(),
        [(Some("fresh"), false), (Some("tilde"), true)]
    );
}

#[test]
fn the_daemon_process_environment_does_not_steer_the_roots() {
    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let steered = scoped_dir("pohunek-roots-steered");
    let mut process = ProcessEnv::lock();
    process.set("CLAUDE_CONFIG_DIR", steered.as_path());
    process.set("CODEX_HOME", steered.as_path());
    let rig = Rig::with_daemon_env(&[
        ("CLAUDE_CONFIG_DIR", steered.as_path()),
        ("CODEX_HOME", steered.as_path()),
    ]);

    let roots = rig.roots();

    assert_eq!(
        pairs(&roots),
        BTreeSet::from([
            root(&RuntimeRef::claude(), rig.home().join(".claude/projects")),
            root(&RuntimeRef::codex(), rig.home().join(".codex/sessions")),
        ])
    );
    assert!(roots.iter().all(|root| !root.path.starts_with(&*steered)));
}

#[tokio::test]
async fn a_pass_that_cannot_resolve_homes_keeps_the_previous_roots() {
    let rig = Rig::new();
    let homes = rig.homes.clone();
    let failing = Arc::new(AtomicBool::new(false));
    let provider = {
        let failing = Arc::clone(&failing);
        LaunchHomeRoots::from_source(Arc::new(move || {
            if failing.load(Ordering::SeqCst) {
                Err(ProtocolError::new(
                    ErrorClass::Runtime,
                    "worker_initialize_invalid",
                    "no base environment",
                    None,
                ))
            } else {
                Ok(homes.clone())
            }
        }))
    };

    let settled = provider.roots().await;
    assert!(!settled.is_empty());
    failing.store(true, Ordering::SeqCst);

    assert_eq!(provider.roots().await, settled);
}

#[tokio::test]
async fn the_registry_provider_resolves_homes_from_its_own_launch_environment() {
    use crate::session::{SessionRegistry, SessionRegistryConfig};

    if !pohunek_test_support::process_env::in_own_process() {
        return;
    }
    let steered = scoped_dir("pohunek-roots-registry-steered");
    let rig = Rig::new();
    let work = rig.used_home("work-home", "projects");
    rig.claude_profile("work", &work);
    let mut process = ProcessEnv::lock();
    process.set("CLAUDE_CONFIG_DIR", steered.as_path());
    let registry = SessionRegistry::new_with_runtimes_and_environment(
        SessionRegistryConfig {
            agents_dir: Some(rig.root.join("agents")),
            ..SessionRegistryConfig::default()
        },
        official_host(),
        EnvironmentSource::fixed([
            (OsString::from("HOME"), rig.home().into_os_string()),
            (
                OsString::from("CLAUDE_CONFIG_DIR"),
                steered.as_os_str().to_owned(),
            ),
        ]),
        DEFAULT_ENVIRONMENT_ALLOWLIST
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    );
    drop(process);

    let roots = LaunchHomeRoots::new(registry).roots().await;

    assert_eq!(
        pairs(&roots),
        BTreeSet::from([
            root(&RuntimeRef::claude(), rig.home().join(".claude/projects")),
            root(&RuntimeRef::codex(), rig.home().join(".codex/sessions")),
            root(&RuntimeRef::claude(), work.join("projects")),
        ])
    );
}

fn write_transcript(path: &Path, session_id: &str) {
    fs::create_dir_all(path.parent().expect("transcript parent")).expect("create parent");
    fs::write(
        path,
        format!("{{\"session_id\":\"{session_id}\",\"cwd\":\"/work\"}}\n"),
    )
    .expect("write transcript");
}

/// A running observer over a rig.
struct Watched {
    sessions: ExternalSessions,
    index: super::TranscriptIndex,
    handle: WatcherHandle,
}

impl Watched {
    async fn start(rig: &Rig) -> Self {
        let sessions = ExternalSessions::new();
        let (index, handle) = start_transcript_index_with(rig.provider(), &sessions).await;
        Self {
            sessions,
            index,
            handle,
        }
    }

    fn session_of(&self, path: &Path) -> Option<String> {
        self.index
            .inner
            .lock()
            .expect("index")
            .get(path)
            .and_then(|candidate| candidate.native_session_id.clone())
    }

    async fn wait_until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let wait = async {
            while !done(self) {
                self.handle.request_reconcile();
                let _ =
                    tokio::time::timeout(NUDGE_TICK, self.sessions.inner.rescan.notified()).await;
            }
        };
        tokio::time::timeout(FOLLOW_TIMEOUT, wait)
            .await
            .unwrap_or_else(|_elapsed| panic!("timed out waiting for {what}"));
    }

    async fn wait_for_session(&self, path: &Path, session_id: &str) {
        self.wait_until(&format!("{session_id} to be indexed"), |watched| {
            watched.session_of(path).as_deref() == Some(session_id)
        })
        .await;
    }
}

#[tokio::test]
async fn a_transcript_appearing_in_either_home_is_observed() {
    let rig = Rig::new();
    let ambient = rig.home().join(".claude/projects/p/existing.jsonl");
    write_transcript(&ambient, "ambient-existing");
    let work = rig.used_home("work-home", "projects");
    rig.claude_profile("work", &work);
    let work_existing = work.join("projects/p/existing.jsonl");
    write_transcript(&work_existing, "work-existing");

    let watched = Watched::start(&rig).await;

    assert_eq!(
        watched.session_of(&ambient).as_deref(),
        Some("ambient-existing")
    );
    assert_eq!(
        watched.session_of(&work_existing).as_deref(),
        Some("work-existing")
    );

    let ambient_live = rig.home().join(".claude/projects/p/live.jsonl");
    let work_live = work.join("projects/p/live.jsonl");
    write_transcript(&ambient_live, "ambient-live");
    write_transcript(&work_live, "work-live");
    watched
        .wait_for_session(&ambient_live, "ambient-live")
        .await;
    watched.wait_for_session(&work_live, "work-live").await;
    watched.sessions.shutdown();
}

#[tokio::test]
async fn a_profile_added_later_is_observed_and_a_removed_one_leaves_the_index() {
    let rig = Rig::new();
    let watched = Watched::start(&rig).await;
    let late = rig.used_home("late-home", "projects");
    let transcript = late.join("projects/p/s.jsonl");
    write_transcript(&transcript, "late-session");
    assert_eq!(watched.session_of(&transcript), None);

    rig.claude_profile("late", &late);
    watched.wait_for_session(&transcript, "late-session").await;

    fs::remove_file(rig.root.join("agents/late.toml")).expect("remove the profile");
    watched
        .wait_until(
            "the removed profile's transcripts to leave the index",
            |w| w.session_of(&transcript).is_none(),
        )
        .await;
    assert!(transcript.is_file(), "the transcript itself is untouched");
    watched.sessions.shutdown();
}

#[test]
fn two_runtimes_sharing_one_directory_each_keep_their_transcript_tree() {
    let rig = Rig::new();
    let shared = rig.root.join("shared-home");
    fs::create_dir_all(shared.join("projects")).expect("Claude tree");
    fs::create_dir_all(shared.join("sessions")).expect("Codex tree");
    rig.claude_profile("claude-shared", &shared);
    rig.profile("codex-shared", "codex", "CODEX_HOME", &shared);

    let roots = pairs(&rig.roots());

    assert!(roots.contains(&root(&RuntimeRef::claude(), shared.join("projects"))));
    assert!(roots.contains(&root(&RuntimeRef::codex(), shared.join("sessions"))));
}

#[test]
fn only_profile_derived_roots_are_private() {
    let rig = Rig::new();
    fs::create_dir_all(rig.home().join(".claude/projects")).expect("ambient tree");
    let work = rig.used_home("work-home", "projects");
    rig.claude_profile("work", &work);

    let roots = rig.roots();
    let private = |path: &Path| {
        roots
            .iter()
            .find(|root| root.path == path)
            .map(|root| root.private)
    };

    assert_eq!(private(&rig.home().join(".claude/projects")), Some(false));
    assert_eq!(private(&work.join("projects")), Some(true));
}

#[tokio::test]
async fn a_transcript_below_a_profile_home_publishes_no_path() {
    use crate::procwatch::{ProcessFact, StartIdentity};

    let rig = Rig::new();
    let private_home = rig.used_home("zq-private-account-3c81", "projects");
    rig.claude_profile("work", &private_home);
    // The record names no transcript path, so the fallback is the file's own
    // path, which lies below the private directory.
    let transcript = private_home.join("projects/p/s.jsonl");
    fs::create_dir_all(transcript.parent().expect("parent")).expect("project dir");
    fs::write(
        &transcript,
        "{\"session_id\":\"native\",\"cwd\":\"/work\"}\n",
    )
    .expect("write");
    let ambient = rig.home().join(".claude/projects/p/s.jsonl");
    fs::create_dir_all(ambient.parent().expect("parent")).expect("ambient dir");
    fs::write(
        &ambient,
        "{\"session_id\":\"ambient\",\"cwd\":\"/other\"}\n",
    )
    .expect("write");

    let sessions = ExternalSessions::new();
    let (index, _handle) = start_transcript_index_with(rig.provider(), &sessions).await;
    let fact = ProcessFact {
        pid: 4242,
        pgid: 4242,
        ppid: 1,
        start_identity: StartIdentity::new(4242),
        comm: "claude".to_owned(),
        cmdline: vec!["claude".to_owned()],
    };

    let private = index
        .best_match(&RuntimeRef::claude(), Path::new("/work"), &fact)
        .expect("the private transcript matches by cwd");
    let public = index
        .best_match(&RuntimeRef::claude(), Path::new("/other"), &fact)
        .expect("the ambient transcript matches by cwd");

    assert!(
        private.is_private(),
        "a profile-derived transcript is private"
    );
    assert_eq!(private.publishable_path(), None);
    assert!(
        private
            .native_session_path
            .contains("zq-private-account-3c81"),
        "the internal provenance keeps the real path"
    );
    assert!(!public.is_private());
    assert!(public
        .publishable_path()
        .is_some_and(|path| path.contains(".claude")));
    sessions.shutdown();
}

/// A process fact for `best_match`; only its command line is consulted.
fn fact() -> crate::procwatch::ProcessFact {
    crate::procwatch::ProcessFact {
        pid: 4242,
        pgid: 4242,
        ppid: 1,
        start_identity: crate::procwatch::StartIdentity::new(4242),
        comm: "agent".to_owned(),
        cmdline: vec!["agent".to_owned()],
    }
}

fn plain_root(agent: &RuntimeRef, path: &Path, private: bool) -> TranscriptRoot {
    TranscriptRoot {
        parser: TranscriptParser::JsonlHeadV1,
        head_paths: HeadPaths::default(),
        agent_base: agent.clone(),
        path: path.to_path_buf(),
        private,
        max_depth: None,
    }
}

#[tokio::test]
async fn a_candidate_whose_root_left_is_never_offered_while_the_scan_is_paused() {
    use tokio_util::sync::CancellationToken;

    let base = scoped_dir("pohunek-roots-handover");
    let private_root = base.join("zq-private-account-9d27").join("projects");
    let transcript = private_root.join("p/s.jsonl");
    write_transcript(&transcript, "native");
    let index = super::TranscriptIndex::default();
    let roots = vec![plain_root(&RuntimeRef::claude(), &private_root, true)];
    index.scan_roots(roots, CancellationToken::new()).await;
    let offered = |index: &super::TranscriptIndex| {
        index.best_match(&RuntimeRef::claude(), Path::new("/work"), &fact())
    };
    let before = offered(&index).expect("the indexed candidate matches by cwd");
    assert_eq!(before.publishable_path(), None);

    // The scan has replaced its roots and has not pruned yet.
    *index.roots.lock().expect("roots") = Arc::new(super::RootSnapshot::new(Vec::new()));
    assert!(offered(&index).is_none(), "no current owner, no candidate");

    // The same directory is now a public root: the candidate was indexed as
    // private and is not offered as public.
    *index.roots.lock().expect("roots") = Arc::new(super::RootSnapshot::new(vec![plain_root(
        &RuntimeRef::claude(),
        &private_root,
        false,
    )]));
    assert!(
        offered(&index).is_none(),
        "the provenance no longer matches"
    );
}

#[tokio::test]
async fn a_nested_root_added_or_removed_reassigns_unchanged_transcripts() {
    use tokio_util::sync::CancellationToken;

    let base = scoped_dir("pohunek-roots-nested");
    let outer = base.join("a").join("projects");
    let inner = outer.join("sessions");
    write_transcript(&inner.join("2026/x.jsonl"), "native");
    let index = super::TranscriptIndex::default();
    let claude_only = vec![plain_root(&RuntimeRef::claude(), &outer, false)];
    let nested = vec![
        plain_root(&RuntimeRef::claude(), &outer, false),
        plain_root(&RuntimeRef::codex(), &inner, true),
    ];
    let owner_of = |index: &super::TranscriptIndex| {
        let claude = index
            .best_match(&RuntimeRef::claude(), Path::new("/work"), &fact())
            .is_some();
        let codex = index
            .best_match(&RuntimeRef::codex(), Path::new("/work"), &fact())
            .map(|candidate| candidate.is_private());
        (claude, codex)
    };

    index
        .scan_roots(claude_only.clone(), CancellationToken::new())
        .await;
    assert_eq!(owner_of(&index), (true, None));

    // The transcript is not touched; only the roots change.
    index.scan_roots(nested, CancellationToken::new()).await;
    assert_eq!(owner_of(&index), (false, Some(true)));

    index
        .scan_roots(claude_only, CancellationToken::new())
        .await;
    assert_eq!(owner_of(&index), (true, None));
}

#[tokio::test]
async fn a_nested_private_root_published_before_the_prune_hides_the_old_attribution() {
    use tokio_util::sync::CancellationToken;

    let base = scoped_dir("pohunek-roots-nested-pause");
    let outer = base.join("a").join("projects");
    let inner = outer.join("zq-private-nested-4be1");
    write_transcript(&inner.join("p/x.jsonl"), "native");
    let index = super::TranscriptIndex::default();
    let outer_only = vec![plain_root(&RuntimeRef::claude(), &outer, false)];
    index.scan_roots(outer_only, CancellationToken::new()).await;
    let public = index
        .best_match(&RuntimeRef::claude(), Path::new("/work"), &fact())
        .expect("indexed under the outer root");
    assert!(public.publishable_path().is_some());

    // The scan has published a nested private root and has not scanned yet.
    *index.roots.lock().expect("roots") = Arc::new(super::RootSnapshot::new(vec![
        plain_root(&RuntimeRef::claude(), &outer, false),
        plain_root(&RuntimeRef::claude(), &inner, true),
    ]));

    assert!(
        index
            .best_match(&RuntimeRef::claude(), Path::new("/work"), &fact())
            .is_none(),
        "the old public attribution is not offered"
    );
}

#[tokio::test]
async fn an_inspection_error_keeps_a_profile_root_and_its_candidates() {
    use tokio_util::sync::CancellationToken;

    use super::watch::RootScan;

    let rig = Rig::new();
    let home = rig.root.join("loop-home");
    fs::create_dir_all(&home).expect("home");
    let tree = home.join("projects");
    write_transcript(&tree.join("p/s.jsonl"), "native");
    rig.claude_profile("looped", &home);
    let index = super::TranscriptIndex::default();
    index
        .scan_roots(rig.roots(), CancellationToken::new())
        .await;
    assert!(index
        .best_match(&RuntimeRef::claude(), Path::new("/work"), &fact())
        .is_some());

    // The tree can no longer be inspected: a symlink loop answers ELOOP, not
    // NotFound.
    fs::remove_dir_all(&tree).expect("remove the tree");
    symlink("projects", &tree).expect("loop");
    let roots = rig.roots();
    assert!(
        roots.iter().any(|root| root.path == tree),
        "an uninspectable tree stays a root"
    );
    assert!(rig.skipped().is_empty());

    let scans = index.scan_roots(roots, CancellationToken::new()).await;
    assert!(scans.contains(&RootScan::Incomplete), "{scans:?}");
    assert!(
        index
            .best_match(&RuntimeRef::claude(), Path::new("/work"), &fact())
            .is_some(),
        "the candidates survive an uninspectable pass"
    );
}

/// Package id of the package-defined runtime the descriptor scenarios install.
const ACME_PACKAGE_ID: &str = "acme.runtime.agent";
/// Runtime id that package serves.
const ACME_RUNTIME: &str = "acme";

/// The descriptor of [`ACME_RUNTIME`]: a config home relocated by `ACME_HOME`
/// (default `.acme`), followed by `transcripts`, which is a `[transcripts]`
/// table or empty.
fn acme_descriptor(transcripts: &str) -> String {
    format!(
        r#"schema = 1
id = "{ACME_PACKAGE_ID}"
version = "1.0.0"
runtime_api = 1

[runtime]
id = "{ACME_RUNTIME}"
name = "Acme"
program = "/bin/sh"
args = []
detect_manifest = "detect.toml"

[input]
bracketed_paste = false
submit_delay_ms = 0
text_policy = "unrestricted"

[resume]
supported = true
reference_kind = "id"
args = ["--session", "{{reference}}"]

[fork]
supported = false

[native_reference]
strategy = "assigned"
launch_args = ["--session-id", "{{reference}}"]

[native_reference.existence]
check = "none"

[config_home]
env = "ACME_HOME"
default = ".acme"
{transcripts}"#
    )
}

/// Installs `descriptor` as an enabled, selected package of `plugins`.
fn install_acme(plugins: &Path, descriptor: &str) {
    install_acme_version(plugins, descriptor, "1.0.0", true);
}

/// Installs `descriptor` at `version` as an enabled package of `plugins`,
/// selected when `select` is set, and returns its archive digest.
fn install_acme_version(
    plugins: &Path,
    descriptor: &str,
    version: &str,
    select: bool,
) -> protocol::PackageDigest {
    use package::registry::{InstallRequest, PackageSource as InstallSource, Registry};
    use package::{build_archive, read_archive, ArchiveEntry, Limits};

    let entries = [
        ArchiveEntry {
            path: "runtime.toml".to_owned(),
            contents: descriptor
                .replace("version = \"1.0.0\"", &format!("version = \"{version}\""))
                .into_bytes(),
            executable: false,
        },
        ArchiveEntry {
            path: "detect.toml".to_owned(),
            contents: b"[[rules]]\nid = \"idle_prompt\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n".to_vec(),
            executable: false,
        },
    ];
    let bytes = build_archive(&entries, &Limits::DEFAULT).expect("the archive builds");
    let digest = read_archive(&bytes, &Limits::DEFAULT)
        .expect("the archive reads")
        .digest()
        .clone();
    Registry::open_at(plugins, Limits::DEFAULT)
        .expect("the plugin root opens")
        .install(&InstallRequest {
            archive: &bytes,
            expected: &digest,
            identity: protocol::PackageIdentity {
                id: protocol::PackageId::parse(ACME_PACKAGE_ID).expect("package id"),
                version: protocol::PackageVersion::parse(version).expect("package version"),
            },
            source: InstallSource::ExplicitDigest,
            enabled: true,
            select,
            installed_at_unix_seconds: 1_700_000_000,
        })
        .expect("the package installs");
    digest
}

/// The roots of the official Codex and Claude runtimes on a host without
/// profiles.
fn official_roots(rig: &Rig) -> BTreeSet<(String, PathBuf)> {
    BTreeSet::from([
        root(&RuntimeRef::claude(), rig.home().join(".claude/projects")),
        root(&RuntimeRef::codex(), rig.home().join(".codex/sessions")),
    ])
}

#[tokio::test]
async fn a_runtime_without_a_transcript_declaration_is_observed_without_a_root() {
    use super::watch::TranscriptWatch;

    let rig = Rig::with_packages(|plugins| install_acme(plugins, &acme_descriptor("")));
    let acme = RuntimeRef::from_wire(ACME_RUNTIME);
    // Its home holds the trees other runtimes declare; none is its own.
    let stray = [
        rig.home().join(".acme/sessions/p/s.jsonl"),
        rig.home().join(".acme/projects/p/s.jsonl"),
    ];
    for path in &stray {
        write_transcript(path, "acme-stray");
    }
    let claude = rig.home().join(".claude/projects/p/s.jsonl");
    write_transcript(&claude, "claude-native");
    fs::create_dir_all(rig.home().join(".codex/sessions")).expect("Codex tree");

    // The runtime is served and its config home resolves: only the missing
    // declaration keeps it out.
    let homes = rig.homes.launch_homes();
    let declared = homes
        .iter()
        .find(|home| home.runtime == acme)
        .expect("the package runtime declares a config home");
    assert!(declared.transcripts.is_none());
    assert_eq!(
        declared.home.as_ref().expect("the home resolves").dir,
        rig.home().join(".acme")
    );
    assert_eq!(pairs(&rig.roots()), official_roots(&rig));
    assert!(rig.skipped().is_empty(), "{:?}", rig.skipped());

    let watched = Watched::start(&rig).await;
    watched
        .wait_until("the watcher to cover every root", |watched| {
            watched.handle.state() == TranscriptWatch::Active
        })
        .await;
    watched.wait_for_session(&claude, "claude-native").await;
    for path in &stray {
        assert_eq!(watched.session_of(path), None, "{}", path.display());
    }
    assert!(watched
        .index
        .best_match(&acme, Path::new("/work"), &fact())
        .is_none());
    watched.sessions.shutdown();
}

#[tokio::test]
async fn a_package_runtime_is_observed_below_the_transcript_dir_it_declares() {
    let rig = Rig::with_packages(|plugins| {
        install_acme(
            plugins,
            &acme_descriptor("\n[transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"logs/chats\"\n"),
        );
    });
    let acme = RuntimeRef::from_wire(ACME_RUNTIME);
    let tree = rig.home().join(".acme/logs/chats");
    let transcript = tree.join("p/s.jsonl");
    write_transcript(&transcript, "acme-native");
    let outside = rig.home().join(".acme/sessions/p/s.jsonl");
    write_transcript(&outside, "acme-outside");

    let mut expected = official_roots(&rig);
    expected.insert(root(&acme, tree));
    assert_eq!(pairs(&rig.roots()), expected);

    let watched = Watched::start(&rig).await;
    watched.wait_for_session(&transcript, "acme-native").await;
    assert_eq!(watched.session_of(&outside), None);
    let candidate = watched
        .index
        .best_match(&acme, Path::new("/work"), &fact())
        .expect("the transcript belongs to the package runtime");
    assert_eq!(candidate.native_session_id.as_deref(), Some("acme-native"));
    assert!(watched
        .index
        .best_match(&RuntimeRef::claude(), Path::new("/work"), &fact())
        .is_none());
    watched.sessions.shutdown();
}

#[tokio::test]
async fn the_transcript_head_names_the_session_by_key_precedence() {
    let rig = Rig::new();
    let tree = rig.home().join(".claude/projects/p");
    fs::create_dir_all(&tree).expect("Claude tree");
    // `jsonl-head-v1` takes `session_id`, then `sessionId`, then
    // `conversation_id`, then `conversationId` from the first record that
    // carries any of them, and `transcript_path` before `transcriptPath`.
    let cases = [
        (
            "all.jsonl",
            r#"{"conversationId":"conv-camel","conversation_id":"conv-snake","sessionId":"session-camel","session_id":"session-snake","transcriptPath":"/t/camel.jsonl","transcript_path":"/t/snake.jsonl","cwd":"/work"}"#,
            "session-snake",
            Some("/t/snake.jsonl"),
        ),
        (
            "camel.jsonl",
            r#"{"conversationId":"conv-camel","conversation_id":"conv-snake","sessionId":"session-camel","cwd":"/work"}"#,
            "session-camel",
            None,
        ),
        (
            "conversation.jsonl",
            r#"{"conversationId":"conv-camel","conversation_id":"conv-snake","cwd":"/work"}"#,
            "conv-snake",
            None,
        ),
        (
            "conversation-camel.jsonl",
            r#"{"conversationId":"conv-camel","cwd":"/work"}"#,
            "conv-camel",
            None,
        ),
        (
            "first-record.jsonl",
            "{\"cwd\":\"/work\"}\n{\"conversationId\":\"conv-first\"}\n{\"session_id\":\"session-later\"}",
            "conv-first",
            None,
        ),
    ];
    for (name, records, _, _) in &cases {
        fs::write(tree.join(name), format!("{records}\n")).expect("write a transcript");
    }

    let watched = Watched::start(&rig).await;
    for (name, _, session, transcript_path) in &cases {
        let path = tree.join(name);
        watched.wait_for_session(&path, session).await;
        let indexed = watched
            .index
            .inner
            .lock()
            .expect("index")
            .get(&path)
            .cloned()
            .expect("indexed");
        let fallback = path.to_string_lossy().into_owned();
        assert_eq!(
            indexed.native_session_path,
            transcript_path.map_or(fallback, ToOwned::to_owned),
            "{name}"
        );
    }
    watched.sessions.shutdown();
}

#[test]
fn a_profile_pinned_to_another_version_observes_the_tree_that_version_declares() {
    let declaring = |dir: &str| {
        acme_descriptor(&format!(
            "\n[transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"{dir}\"\n"
        ))
    };
    let mut pinned = None;
    let rig = Rig::with_packages(|plugins| {
        pinned = Some(install_acme_version(
            plugins,
            &declaring("logs/chats"),
            "1.0.0",
            false,
        ));
        install_acme_version(plugins, &declaring("chats"), "1.1.0", true);
    });
    let pinned = pinned.expect("the pinned version is installed");
    // The profile names no `ACME_HOME`, so it shares the runtime's own home.
    fs::write(
        rig.root.join("agents/old.toml"),
        format!(
            "base = \"{ACME_RUNTIME}\"\npackage = \"{ACME_PACKAGE_ID}\"\ndigest = \"{}\"\n",
            pinned.as_str()
        ),
    )
    .expect("write a pinned profile");
    let home = rig.home().join(".acme");
    fs::create_dir_all(home.join("logs/chats")).expect("the pinned version's tree");
    let acme = RuntimeRef::from_wire(ACME_RUNTIME);

    let roots = rig.roots();

    let mut expected = official_roots(&rig);
    expected.insert(root(&acme, home.join("chats")));
    expected.insert(root(&acme, home.join("logs/chats")));
    assert_eq!(pairs(&roots), expected);
    assert!(rig.skipped().is_empty(), "{:?}", rig.skipped());
}

/// Runtime id of a package that claims Codex's transcripts; it sorts after
/// `codex`, so configuration order would hand it the tree.
const COPYCAT_RUNTIME: &str = "zcodex";

/// Installs [`COPYCAT_RUNTIME`] with the config home `env` / `default` and the
/// transcript tree `dir` below it.
fn install_copycat(plugins: &Path, env: &str, default: &str, dir: &str) {
    let descriptor = acme_descriptor(&format!(
        "\n[transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"{dir}\"\n"
    ))
    .replace(
        &format!("id = \"{ACME_RUNTIME}\""),
        &format!("id = \"{COPYCAT_RUNTIME}\""),
    )
    .replace(
        "env = \"ACME_HOME\"\ndefault = \".acme\"",
        &format!("env = \"{env}\"\ndefault = \"{default}\""),
    );
    install_acme(plugins, &descriptor);
}

/// Asserts that only Claude's tree is a root, that Codex and the copycat are
/// both reported as ambiguous, and that a running observer indexes Claude's
/// transcript but attributes none of `shared` to either runtime.
async fn assert_codex_tree_attributed_to_neither(rig: &Rig, shared: &[PathBuf]) {
    use super::watch::TranscriptWatch;
    use super::AMBIGUOUS_OWNER_CODE;

    let claude = rig.home().join(".claude/projects/p/s.jsonl");
    write_transcript(&claude, "claude-native");
    for (number, path) in shared.iter().enumerate() {
        write_transcript(path, &format!("shared-{number}"));
    }

    assert_eq!(
        pairs(&rig.roots()),
        BTreeSet::from([root(
            &RuntimeRef::claude(),
            rig.home().join(".claude/projects")
        )]),
        "the overlapping trees are roots of neither runtime"
    );
    let reported: BTreeSet<(String, Option<String>)> = rig
        .skipped()
        .into_iter()
        .map(|home| (home.runtime, home.code))
        .collect();
    assert_eq!(
        reported,
        BTreeSet::from([
            ("codex".to_owned(), Some(AMBIGUOUS_OWNER_CODE.to_owned())),
            (
                COPYCAT_RUNTIME.to_owned(),
                Some(AMBIGUOUS_OWNER_CODE.to_owned())
            ),
        ])
    );

    let watched = Watched::start(rig).await;
    watched
        .wait_until("the watcher to cover every root", |watched| {
            watched.handle.state() == TranscriptWatch::Active
        })
        .await;
    watched.wait_for_session(&claude, "claude-native").await;
    for path in shared {
        assert_eq!(watched.session_of(path), None, "{}", path.display());
    }
    for runtime in [RuntimeRef::codex(), RuntimeRef::from_wire(COPYCAT_RUNTIME)] {
        assert!(
            watched
                .index
                .best_match(&runtime, Path::new("/work"), &fact())
                .is_none(),
            "{}",
            runtime.as_wire()
        );
    }
    watched.sessions.shutdown();
}

#[tokio::test]
async fn a_tree_two_runtimes_declare_is_attributed_to_neither_and_reported() {
    let rig =
        Rig::with_packages(|plugins| install_copycat(plugins, "CODEX_HOME", ".codex", "sessions"));
    let shared = [rig.home().join(".codex/sessions/p/s.jsonl")];

    assert_codex_tree_attributed_to_neither(&rig, &shared).await;
}

#[tokio::test]
async fn a_tree_nested_in_another_runtimes_tree_is_attributed_to_neither() {
    let rig = Rig::with_packages(|plugins| {
        install_copycat(plugins, "CODEX_HOME", ".codex", "sessions/2026");
    });
    let shared = [
        rig.home().join(".codex/sessions/2026/p/s.jsonl"),
        rig.home().join(".codex/sessions/2025/p/s.jsonl"),
    ];

    assert_codex_tree_attributed_to_neither(&rig, &shared).await;
}

#[tokio::test]
async fn a_tree_symlinked_into_another_runtimes_tree_is_attributed_to_neither() {
    let rig = Rig::with_packages(|plugins| install_copycat(plugins, "ACME_HOME", ".acme", "chats"));
    let target = rig.home().join(".codex/sessions/2026");
    fs::create_dir_all(&target).expect("Codex tree");
    fs::create_dir_all(rig.home().join(".acme")).expect("copycat home");
    symlink(&target, rig.home().join(".acme/chats")).expect("link into Codex's tree");
    let shared = [
        target.join("p/s.jsonl"),
        rig.home().join(".codex/sessions/2025/p/s.jsonl"),
    ];

    assert_codex_tree_attributed_to_neither(&rig, &shared).await;
}

/// Feature that lets a descriptor keep its transcripts outside the config home.
const DATA_ROOT_FEATURE: &str = "transcripts-data-root-v1";

/// A `[transcripts]` table for a log tree below `XDG_DATA_HOME`.
const DATA_ROOT_TABLE: &str = "\n[transcripts]\nparser = \"muse-session-v1\"\n\
     root_env = \"XDG_DATA_HOME\"\nroot_home = \".local/share\"\n\
     dir = \"muse/sessions\"\nmax_depth = 4\n";

/// The descriptor of [`ACME_RUNTIME`] keeping its session logs in the data root.
fn data_root_descriptor() -> String {
    acme_descriptor(DATA_ROOT_TABLE).replace(
        "runtime_api = 1\n",
        &format!("runtime_api = 1\nrequires = [\"{DATA_ROOT_FEATURE}\"]\n"),
    )
}

/// A session log fixture with its session id and workspace filled in.
fn session_fixture(name: &str, session: &str, workspace: &str) -> String {
    let template = match name {
        "framed" => include_str!("../../tests/fixtures/muse/session-framed.jsonl"),
        "bare" => include_str!("../../tests/fixtures/muse/session-bare.jsonl"),
        other => panic!("no session log fixture {other}"),
    };
    template
        .replace("@SESSION@", session)
        .replace("@WORKSPACE@", workspace)
}

/// Writes `content` as the log of `session` on `date` (`YYYY/MM/DD`) below
/// `tree` and returns its path.
fn write_session(tree: &Path, date: &str, session: &str, content: &str) -> PathBuf {
    let path = tree.join(date).join(session).join("session.jsonl");
    fs::create_dir_all(path.parent().expect("session directory")).expect("session directory");
    fs::write(&path, content).expect("write the session log");
    path
}

const FRAMED_SESSION: &str = "0b9a4a8e-6d1f-4a64-9b5e-2c0f3a7d1e01";
const BARE_SESSION: &str = "5f2c7d90-3b1a-4e8f-8a47-9d6e0c4b2f02";

#[tokio::test]
async fn a_runtime_that_keeps_session_logs_in_the_data_root_is_attributed_by_them() {
    let rig = Rig::with_packages(|plugins| install_acme(plugins, &data_root_descriptor()));
    let acme = RuntimeRef::from_wire(ACME_RUNTIME);
    let tree = rig.home().join(".local/share/muse/sessions");
    let framed = write_session(
        &tree,
        "2026/10/08",
        FRAMED_SESSION,
        &session_fixture("framed", FRAMED_SESSION, "/work"),
    );
    let bare = write_session(
        &tree,
        "2026/10/09",
        BARE_SESSION,
        &session_fixture("bare", BARE_SESSION, "/other"),
    );
    // The config home holds a tree of its own that is not the log tree.
    let config_tree = rig.home().join(".acme/muse/sessions");
    let stray = write_session(
        &config_tree,
        "2026/10/08",
        FRAMED_SESSION,
        &session_fixture("framed", FRAMED_SESSION, "/work"),
    );

    let mut expected = official_roots(&rig);
    expected.insert(root(&acme, tree));
    assert_eq!(pairs(&rig.roots()), expected);

    let watched = Watched::start(&rig).await;
    watched.wait_for_session(&framed, FRAMED_SESSION).await;
    watched.wait_for_session(&bare, BARE_SESSION).await;
    assert_eq!(watched.session_of(&stray), None);
    // A foreground `acme` process of a shell session is told from its
    // workspace, framed and bare logs alike.
    let candidate = watched
        .index
        .best_match(&acme, Path::new("/work"), &fact())
        .expect("the framed log names the workspace");
    assert_eq!(candidate.native_session_id.as_deref(), Some(FRAMED_SESSION));
    let candidate = watched
        .index
        .best_match(&acme, Path::new("/other"), &fact())
        .expect("the bare log names the workspace");
    assert_eq!(candidate.native_session_id.as_deref(), Some(BARE_SESSION));
    assert!(watched
        .index
        .best_match(&RuntimeRef::claude(), Path::new("/work"), &fact())
        .is_none());
    watched.sessions.shutdown();
}

/// Feature that lets a descriptor name the head key paths of its transcripts.
const HEAD_PATHS_FEATURE: &str = "transcripts-head-paths-v1";

/// The `[transcripts]` table of a runtime whose rollouts nest the session id and
/// the working directory below `payload`.
const HEAD_PATHS_TABLE: &str = "\n[transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"sessions\"\n\
     id_path = [\"payload\", \"id\"]\ncwd_path = [\"payload\", \"cwd\"]\n";

/// The descriptor of [`ACME_RUNTIME`] with a transcript table that needs the
/// head-paths feature.
fn head_paths_descriptor(table: &str) -> String {
    acme_descriptor(table).replace(
        "runtime_api = 1\n",
        &format!("runtime_api = 1\nrequires = [\"{HEAD_PATHS_FEATURE}\"]\n"),
    )
}

/// The head of a Codex rollout: the keys of a real `session_meta` record, with
/// synthetic values and no transcript content.
fn rollout_head(id: &str, cwd: &str) -> String {
    serde_json::json!({
        "timestamp": "2026-10-31T10:00:00.000Z",
        "type": "session_meta",
        "payload": {
            "id": id,
            "timestamp": "2026-10-31T10:00:00.000Z",
            "cwd": cwd,
            "originator": "codex_cli_rs",
            "cli_version": "0.0.0",
            "source": "cli",
            "thread_source": "user",
            "model_provider": "stub",
            "base_instructions": {"text": "synthetic"},
            "git": {},
        },
    })
    .to_string()
}

/// A later record of a rollout, whose payload carries an id of its own.
fn rollout_item(id: &str, cwd: &str) -> String {
    serde_json::json!({
        "timestamp": "2026-10-31T10:00:01.000Z",
        "type": "turn_context",
        "payload": {"id": id, "cwd": cwd},
    })
    .to_string()
}

const ROLLOUT_ID: &str = "019a0000-0000-7000-8000-000000000001";
const OTHER_ID: &str = "019a0000-0000-7000-8000-000000000002";

#[tokio::test]
async fn a_rollout_head_is_attributed_by_the_declared_nested_id_and_cwd() {
    let rig = Rig::with_packages(|plugins| {
        install_acme(plugins, &head_paths_descriptor(HEAD_PATHS_TABLE));
    });
    let path = rig.home().join(".acme/sessions/2026/10/31/rollout-a.jsonl");
    fs::create_dir_all(path.parent().expect("rollout directory")).expect("rollout directory");
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            rollout_head(ROLLOUT_ID, "/work"),
            rollout_item(OTHER_ID, "/elsewhere")
        ),
    )
    .expect("write the rollout");

    let indexed = indexed_sessions(&rig).await;

    assert_eq!(
        indexed.get(ROLLOUT_ID),
        Some(&Some(PathBuf::from("/work"))),
        "{indexed:?}"
    );
    assert!(!indexed.contains_key(OTHER_ID), "{indexed:?}");
}

#[tokio::test]
async fn a_nested_path_reads_only_the_first_record_and_a_top_level_key_wins() {
    let rig = Rig::with_packages(|plugins| {
        install_acme(plugins, &head_paths_descriptor(HEAD_PATHS_TABLE));
    });
    let tree = rig.home().join(".acme/sessions/2026/10/31");
    fs::create_dir_all(&tree).expect("rollout directory");
    // The head is not JSON: a later record's payload never stands in for it.
    fs::write(
        tree.join("rollout-torn.jsonl"),
        format!("{{\"truncated\n{}\n", rollout_item(OTHER_ID, "/elsewhere")),
    )
    .expect("write the torn rollout");
    // A top-level `sessionId` and `cwd` beat the nested values.
    let top = serde_json::json!({
        "sessionId": "top-level",
        "cwd": "/top",
        "payload": {"id": ROLLOUT_ID, "cwd": "/work"},
    });
    fs::write(tree.join("rollout-top.jsonl"), format!("{top}\n")).expect("write the rollout");
    // A non-string id is ignored; a relative cwd is ignored while the id stays.
    let odd = serde_json::json!({"payload": {"id": 7, "cwd": "/ignored"}});
    fs::write(tree.join("rollout-odd.jsonl"), format!("{odd}\n")).expect("write the rollout");
    let relative = serde_json::json!({"payload": {"id": "relative-cwd", "cwd": "relative/dir"}});
    fs::write(tree.join("rollout-rel.jsonl"), format!("{relative}\n")).expect("write the rollout");

    let indexed = indexed_sessions(&rig).await;

    assert_eq!(
        indexed.get("top-level"),
        Some(&Some(PathBuf::from("/top"))),
        "{indexed:?}"
    );
    assert_eq!(indexed.get("relative-cwd"), Some(&None), "{indexed:?}");
    assert_eq!(indexed.len(), 2, "{indexed:?}");
}

#[tokio::test]
async fn without_declared_paths_a_nested_rollout_head_is_not_attributed() {
    let rig = Rig::with_packages(|plugins| {
        install_acme(
            plugins,
            &acme_descriptor("\n[transcripts]\nparser = \"jsonl-head-v1\"\ndir = \"sessions\"\n"),
        );
    });
    let path = rig.home().join(".acme/sessions/2026/10/31/rollout-a.jsonl");
    fs::create_dir_all(path.parent().expect("rollout directory")).expect("rollout directory");
    fs::write(&path, format!("{}\n", rollout_head(ROLLOUT_ID, "/work"))).expect("write");

    assert!(indexed_sessions(&rig).await.is_empty());
}

/// The session ids, with their working directories, a full pass over the data
/// root of `rig` indexes.
async fn indexed_sessions(rig: &Rig) -> std::collections::BTreeMap<String, Option<PathBuf>> {
    use tokio_util::sync::CancellationToken;

    let index = super::TranscriptIndex::default();
    index
        .scan_roots(rig.roots(), CancellationToken::new())
        .await;
    let indexed = index.inner.lock().expect("index");
    indexed
        .values()
        .filter_map(|candidate| Some((candidate.native_session_id.clone()?, candidate.cwd.clone())))
        .collect()
}

#[tokio::test]
async fn a_log_outside_the_session_shape_or_with_a_foreign_id_is_no_session() {
    let rig = Rig::with_packages(|plugins| install_acme(plugins, &data_root_descriptor()));
    let tree = rig.home().join(".local/share/muse/sessions");
    let good = "11111111-1111-4111-8111-111111111111";
    write_session(
        &tree,
        "2026/10/08",
        good,
        &session_fixture("bare", good, "/work"),
    );

    // The id in the record must be the directory's.
    let foreign = "22222222-2222-4222-8222-222222222222";
    write_session(
        &tree,
        "2026/10/08",
        foreign,
        &session_fixture("bare", good, "/work"),
    );
    // A log nested below a session, one at another depth, and one not dated.
    for (index, log) in [
        format!("2026/10/08/{good}/subagent/aaaa/session.jsonl"),
        format!("2026/10/08/{good}/subagents/bbbb/session.jsonl"),
        "2026/10/session.jsonl".to_owned(),
        "2026/10/08/session.jsonl".to_owned(),
        format!("x/y/z/{good}/session.jsonl"),
        format!("2026/10/08/{good}/session.json"),
        format!("26/10/08/{good}/session.jsonl"),
        format!(
            "2026/10/08/{}/session.jsonl",
            "ABCDEF12-3456-4789-8ABC-DEF012345678"
        ),
        format!(
            "2026/10/08/{}/session.jsonl",
            "abcdef12345647898abcdef012345678"
        ),
    ]
    .iter()
    .enumerate()
    {
        let nested = format!("33333333-3333-4333-8333-33333333333{index}");
        let path = tree.join(log);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::write(&path, session_fixture("bare", &nested, "/work")).expect("write");
    }
    // A directory name that is no UUID.
    write_session(
        &tree,
        "2026/10/08",
        "not-a-uuid",
        &session_fixture("bare", "not-a-uuid", "/work"),
    );

    assert_eq!(
        indexed_sessions(&rig).await,
        [(good.to_owned(), Some(PathBuf::from("/work")))].into()
    );
}

#[tokio::test]
async fn a_session_log_is_read_within_the_record_and_byte_bounds() {
    let rig = Rig::with_packages(|plugins| install_acme(plugins, &data_root_descriptor()));
    let tree = rig.home().join(".local/share/muse/sessions");
    let filler = "{\"payload_type\":\"runtime.session.other\"}\n";
    let with_metadata_at = |line: usize, session: &str| {
        let mut content = filler.repeat(line - 1);
        content.push_str(&session_fixture("bare", session, "/work"));
        content
    };
    let at_limit = "44444444-4444-4444-8444-444444444444";
    let past_limit = "55555555-5555-4555-8555-555555555555";
    let past_bytes = "66666666-6666-4666-8666-666666666666";
    let first_wins = "77777777-7777-4777-8777-777777777777";
    write_session(
        &tree,
        "2026/10/08",
        at_limit,
        &with_metadata_at(32, at_limit),
    );
    write_session(
        &tree,
        "2026/10/08",
        past_limit,
        &with_metadata_at(33, past_limit),
    );
    // The record starts after the byte bound, in a very long first line.
    let mut long = "x".repeat(70_000);
    long.push('\n');
    long.push_str(&session_fixture("bare", past_bytes, "/work"));
    write_session(&tree, "2026/10/08", past_bytes, &long);
    // Only the first metadata record counts: a later one for another
    // workspace changes nothing.
    let mut twice = session_fixture("bare", first_wins, "/first");
    twice.push_str(&session_fixture("bare", first_wins, "/second"));
    write_session(&tree, "2026/10/08", first_wins, &twice);

    assert_eq!(
        indexed_sessions(&rig).await,
        [
            (at_limit.to_owned(), Some(PathBuf::from("/work"))),
            (first_wins.to_owned(), Some(PathBuf::from("/first"))),
        ]
        .into()
    );
}

#[tokio::test]
async fn a_workspace_that_is_not_absolute_leaves_the_log_without_a_working_directory() {
    let rig = Rig::with_packages(|plugins| install_acme(plugins, &data_root_descriptor()));
    let acme = RuntimeRef::from_wire(ACME_RUNTIME);
    let tree = rig.home().join(".local/share/muse/sessions");
    let relative = "88888888-8888-4888-8888-888888888888";
    write_session(
        &tree,
        "2026/10/08",
        relative,
        &session_fixture("bare", relative, "work"),
    );

    let watched = Watched::start(&rig).await;
    watched
        .wait_for_session(
            &tree.join(format!("2026/10/08/{relative}/session.jsonl")),
            relative,
        )
        .await;
    for cwd in ["work", "/work", "/"] {
        assert!(
            watched
                .index
                .best_match(&acme, Path::new(cwd), &fact())
                .is_none(),
            "{cwd}"
        );
    }
    watched.sessions.shutdown();
}

#[test]
fn the_data_root_follows_the_environment_and_falls_back_to_the_home() {
    let moved = scoped_dir("pohunek-roots-data-moved");
    let acme = RuntimeRef::from_wire(ACME_RUNTIME);
    let install = |plugins: &Path| install_acme(plugins, &data_root_descriptor());

    let unset = Rig::with_packages(install);
    assert!(pairs(&unset.roots()).contains(&root(
        &acme,
        unset.home().join(".local/share/muse/sessions")
    )));

    let set = Rig::with_packages_and_env(&[("XDG_DATA_HOME", moved.as_path())], install);
    let roots = pairs(&set.roots());
    assert!(roots.contains(&root(&acme, moved.join("muse/sessions"))));
    assert!(!roots.contains(&root(&acme, set.home().join(".local/share/muse/sessions"))));

    // A relative value is no root and no fallback: the home is reported.
    let relative = Rig::with_packages_and_env(&[("XDG_DATA_HOME", Path::new("data"))], install);
    assert!(pairs(&relative.roots())
        .iter()
        .all(|(runtime, _path)| runtime != ACME_RUNTIME));
    assert!(relative
        .skipped()
        .iter()
        .any(|home| home.runtime == ACME_RUNTIME && home.profile.is_none() && home.code.is_some()));
}

#[test]
fn a_profile_moves_the_data_root_with_its_environment() {
    let mut digest = None;
    let rig = Rig::with_packages(|plugins| {
        digest = Some(install_acme_version(
            plugins,
            &data_root_descriptor(),
            "1.0.0",
            true,
        ));
    });
    let digest = digest.expect("the package is installed");
    let moved = rig.used_home("profile-data", "muse/sessions");
    fs::write(
        rig.root.join("agents/moved.toml"),
        format!(
            "base = \"{ACME_RUNTIME}\"\npackage = \"{ACME_PACKAGE_ID}\"\ndigest = \"{}\"\n\
             [env]\nXDG_DATA_HOME = \"{}\"\n",
            digest.as_str(),
            moved.display()
        ),
    )
    .expect("write the profile");
    let acme = RuntimeRef::from_wire(ACME_RUNTIME);

    let roots = rig.roots();

    // The runtime's own root stays below `HOME`; the profile's follows its
    // `XDG_DATA_HOME` and is private to it.
    assert_eq!(
        pairs(&roots)
            .into_iter()
            .filter(|(runtime, _path)| runtime == ACME_RUNTIME)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            root(&acme, rig.home().join(".local/share/muse/sessions")),
            root(&acme, moved.join("muse/sessions")),
        ])
    );
    assert!(roots
        .iter()
        .any(|root| root.path == moved.join("muse/sessions") && root.private));
}
