//! `pohunek session new --no-repo-hooks` (#2366) against a real daemon.
//!
//! The `pohunek` binary runs against the harness's in-process production
//! daemon with a host-global hook layer and the agent-state hook dispatcher.
//! Every fixture repository commits an in-repo script for each lifecycle event
//! that appends `repo:<event>:<session id>` to `repo.log` when it runs; every
//! host-global script appends the event to `host.log`. The host log is the positive control that an event
//! fired; the repository log must stay empty for a session started with the
//! switch, through its whole life and across a daemon restart. The same
//! fixtures without the switch prove that the repository scripts do run.

// Rust guideline compliant 2026-11-15

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use pohunek_service_config::RequiredHook;
use pohunek_test_support::wait::wait_until;
use serde_json::Value;

#[path = "support/plugin_harness.rs"]
mod plugin_harness;

use plugin_harness::{path_str, Harness};

/// Every lifecycle hook event, as the hook file names spell them.
const EVENTS: [&str; 7] = [
    "pre-create",
    "post-create",
    "session-start",
    "agent-state",
    "session-stop",
    "pre-remove",
    "post-remove",
];

/// Mode of a hook script.
const SCRIPT_MODE: u32 = 0o755;

/// An agent over the shell that keeps reporting progress (OSC 9;4) on and
/// off, so its activity keeps changing and the agent-state hook keeps firing,
/// also for the daemon that adopts it after a restart.
const TOGGLING_PROFILE: &str = r#"base = "shell"
program = "/bin/sh"
args = ["-c", 'while :; do printf "\033]9;4;3\007"; sleep 0.3; printf "\033]9;4;0\007"; sleep 0.3; done']
"#;

/// A scenario's daemon, its fixture repository and the two hook logs.
struct Scene {
    harness: Harness,
    repo: PathBuf,
    logs: PathBuf,
}

impl Scene {
    /// A durable daemon with `required` as its `hooks.required`, the toggling
    /// profile, and a host-global script for each of `host_events`.
    async fn start(required: Vec<RequiredHook>, host_events: &[&str]) -> Self {
        let harness = Harness::start_durable_with_host_hooks(required).await;
        let logs = harness.env.root().join("hook-logs");
        fs::create_dir_all(&logs).expect("create the hook log directory");
        harness.profile("toggler", TOGGLING_PROFILE);
        let repo = harness.env.root().join("pr-checkout");
        let scene = Self {
            harness,
            repo,
            logs,
        };
        for event in host_events {
            scene.host_hook(event, &scene.appending("host.log", "$POHUNEK_HOOK_EVENT"));
        }
        scene
    }

    /// A script that appends `entry` to the log `name` and succeeds.
    fn appending(&self, name: &str, entry: &str) -> String {
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"{entry}\" >> '{}'\n",
            self.logs.join(name).display()
        )
    }

    fn host_hook(&self, event: &str, body: &str) {
        write_script(&self.harness.host_hooks_dir().join(event), body);
    }

    fn remove_host_hook(&self, event: &str) {
        fs::remove_file(self.harness.host_hooks_dir().join(event)).expect("remove the host hook");
    }

    /// The entries of the log `name`; none while it does not exist.
    fn entries(&self, name: &str) -> Vec<String> {
        match fs::read_to_string(self.logs.join(name)) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read {name}: {error}"),
        }
    }

    fn host_ran(&self, event: &str) -> bool {
        self.entries("host.log").iter().any(|entry| entry == event)
    }

    /// The repository-script entries of the session `id`.
    fn repo_entries_of(&self, id: &str) -> Vec<String> {
        let suffix = format!(":{id}");
        self.entries("repo.log")
            .into_iter()
            .filter(|entry| entry.ends_with(&suffix))
            .collect()
    }

    async fn wait_for_host(&self, event: &str) {
        wait_until(&format!("the host {event} hook"), || async {
            self.host_ran(event).then_some(())
        })
        .await;
    }

    /// Initializes the fixture repository with `files` committed: each is a
    /// path below the repository and the script it holds.
    fn init_repo(&self, files: &[(String, String)]) {
        fs::create_dir_all(&self.repo).expect("create the repository");
        self.git(&["-c", "init.defaultBranch=main", "init", "-q"]);
        self.git(&["config", "user.email", "test@example.com"]);
        self.git(&["config", "user.name", "Test"]);
        self.git(&["config", "commit.gpgsign", "false"]);
        fs::write(self.repo.join("README.md"), "init\n").expect("write README");
        for (relative, body) in files {
            let path = self.repo.join(relative);
            fs::create_dir_all(path.parent().expect("script parent")).expect("create directory");
            write_script(&path, body);
        }
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "a pull request with hooks"]);
    }

    /// An in-repo script for every event plus the legacy setup script, each
    /// appending to `repo.log`.
    fn repo_hook_files(&self) -> Vec<(String, String)> {
        let mut files: Vec<_> = EVENTS
            .iter()
            .map(|event| {
                (
                    format!(".pohunek/hooks/{event}"),
                    self.appending("repo.log", &format!("repo:{event}:$POHUNEK_SESSION_ID")),
                )
            })
            .collect();
        files.push((
            ".pohunek/setup".to_owned(),
            self.appending("repo.log", "repo:setup:$POHUNEK_SESSION_ID"),
        ));
        files
    }

    fn git(&self, args: &[&str]) -> String {
        let output = pohunek_test_support::git::command()
            .args(args)
            .current_dir(&self.repo)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// `session new --agent toggler` on `branch` of the fixture repository,
    /// with the switch when `skip` is set.
    async fn new_on_branch(&self, branch: &str, skip: bool) -> (i32, Value) {
        let repo = path_str(&self.repo).to_owned();
        let mut args = vec![
            "session", "new", "--agent", "toggler", "--repo", &repo, "--branch", branch,
        ];
        if skip {
            args.push("--no-repo-hooks");
        }
        self.harness.json(&args).await
    }

    async fn created_on_branch(&self, branch: &str, skip: bool) -> String {
        let (code, created) = self.new_on_branch(branch, skip).await;
        assert_eq!(code, 0, "{created}");
        assert_eq!(
            created["ok"]["skip_repo_hooks"].as_bool().unwrap_or(false),
            skip,
            "{created}"
        );
        created["ok"]["id"].as_str().expect("session id").to_owned()
    }

    async fn inspected(&self, id: &str) -> Value {
        let (code, inspected) = self.harness.json(&["session", "inspect", id]).await;
        assert_eq!(code, 0, "{inspected}");
        inspected["ok"].clone()
    }

    async fn stop_and_remove(&self, id: &str) {
        let (code, stopped) = self.harness.json(&["session", "stop", id]).await;
        assert_eq!(code, 0, "{stopped}");
        self.wait_for_host("session-stop").await;
        let (code, removed) = self.harness.json(&["session", "rm", id]).await;
        assert_eq!(code, 0, "{removed}");
    }

    /// The worktrees git lists for the fixture repository.
    fn worktrees(&self) -> usize {
        self.git(&["worktree", "list", "--porcelain"])
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count()
    }

    fn branch_exists(&self, branch: &str) -> bool {
        pohunek_test_support::git::command()
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(&self.repo)
            .output()
            .expect("run git")
            .status
            .success()
    }

    async fn session_count(&self) -> usize {
        let (code, listed) = self.harness.json(&["session", "list"]).await;
        assert_eq!(code, 0, "{listed}");
        listed["ok"].as_array().expect("sessions").len()
    }
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write the hook script");
    fs::set_permissions(path, fs::Permissions::from_mode(SCRIPT_MODE)).expect("script mode");
}

/// D1, rows 1, 3 and 4: a switched worktree session over a checkout holding
/// every in-repo hook and `.pohunek/setup` runs none of them, on create, start,
/// agent-state, stop and removal, and still none after a daemon restart that
/// adopts it, while the host layer runs for every event.
#[tokio::test]
async fn a_switched_session_runs_no_repository_hook_through_its_life_and_a_restart() {
    let scene = Scene::start(Vec::new(), &EVENTS).await;
    scene.init_repo(&scene.repo_hook_files());

    let id = scene.created_on_branch("review/pr-7", true).await;
    for event in ["pre-create", "post-create", "session-start", "agent-state"] {
        scene.wait_for_host(event).await;
    }
    let inspected = scene.inspected(&id).await;
    assert_eq!(inspected["skip_repo_hooks"], true, "{inspected}");
    let human = scene.harness.run(&["session", "inspect", &id]).await;
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("repo_hooks"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );

    let Scene {
        harness,
        repo,
        logs,
    } = scene;
    // The stopped daemon's hooks have ended with its runtime, so every entry
    // of the cleared log comes from the daemon that adopts the session.
    let surviving = harness.shut_down().await;
    fs::remove_file(logs.join("host.log")).expect("clear the host log");
    let scene = Scene {
        harness: Harness::start_over(surviving).await,
        repo,
        logs,
    };
    // The adopted session keeps the switch, and the restarted daemon's own
    // agent-state, stop and remove hooks skip the repository layer.
    scene.wait_for_host("agent-state").await;
    let inspected = scene.inspected(&id).await;
    assert_eq!(inspected["skip_repo_hooks"], true, "{inspected}");
    scene.stop_and_remove(&id).await;
    for event in ["pre-remove", "post-remove"] {
        assert!(scene.host_ran(event), "the host {event} hook ran");
    }

    assert_eq!(
        scene.entries("repo.log"),
        Vec::<String>::new(),
        "no repository hook ran"
    );
    scene.harness.stop().await;
}

/// Row 2 (control): without the switch the same fixture runs the repository
/// script of every event, so an empty repository log above is evidence.
#[tokio::test]
async fn without_the_switch_every_repository_hook_runs() {
    let scene = Scene::start(Vec::new(), &EVENTS).await;
    scene.init_repo(&scene.repo_hook_files());

    let id = scene.created_on_branch("review/pr-8", false).await;
    scene.wait_for_host("agent-state").await;
    let inspected = scene.inspected(&id).await;
    assert!(inspected.get("skip_repo_hooks").is_none(), "{inspected}");
    scene.stop_and_remove(&id).await;

    for event in EVENTS {
        let entry = format!("repo:{event}:{id}");
        wait_until(&format!("the repository {event} hook"), || async {
            scene.repo_entries_of(&id).contains(&entry).then_some(())
        })
        .await;
    }
    let ran = scene.repo_entries_of(&id);
    assert!(
        !ran.contains(&format!("repo:setup:{id}")),
        "the hook file wins over the legacy setup script: {ran:?}"
    );
    scene.harness.stop().await;
}

/// D1 legacy half: a checkout whose only post-create script is the legacy
/// `.pohunek/setup` runs it without the switch and not with it.
#[tokio::test]
async fn a_legacy_setup_script_runs_only_without_the_switch() {
    let scene = Scene::start(Vec::new(), &["post-create"]).await;
    let setup = vec![(
        ".pohunek/setup".to_owned(),
        scene.appending("repo.log", "repo:setup:$POHUNEK_SESSION_ID"),
    )];
    scene.init_repo(&setup);

    let skipped = scene.created_on_branch("review/legacy-skip", true).await;
    assert_eq!(scene.entries("host.log"), vec!["post-create".to_owned()]);
    assert_eq!(scene.entries("repo.log"), Vec::<String>::new());

    let control = scene.created_on_branch("review/legacy-run", false).await;
    assert_eq!(
        scene.entries("repo.log"),
        vec![format!("repo:setup:{control}")]
    );

    for id in [skipped, control] {
        let (code, removed) = scene.harness.json(&["session", "rm", &id]).await;
        assert_eq!(code, 0, "{removed}");
    }
    scene.harness.stop().await;
}

/// Row 11: a `.pohunek` that is a symlink to a directory outside the checkout,
/// whose `hooks/post-create` is itself a symlink to a script elsewhere, runs
/// nothing under the switch. The control run without it follows both links.
#[tokio::test]
async fn linked_hook_paths_cannot_bring_the_repository_layer_back() {
    let scene = Scene::start(Vec::new(), &["post-create"]).await;
    let outside = scene.harness.env.root().join("outside-hooks");
    fs::create_dir_all(outside.join("hooks")).expect("create the outside hook directory");
    let target = scene.harness.env.root().join("outside-script");
    write_script(
        &target,
        &scene.appending("repo.log", "repo:linked-post-create:$POHUNEK_SESSION_ID"),
    );
    std::os::unix::fs::symlink(&target, outside.join("hooks/post-create"))
        .expect("link the hook file");
    write_script(
        &outside.join("setup"),
        &scene.appending("repo.log", "repo:linked-setup:$POHUNEK_SESSION_ID"),
    );
    scene.init_repo(&[]);
    std::os::unix::fs::symlink(&outside, scene.repo.join(".pohunek")).expect("link .pohunek");
    scene.git(&["add", "-A"]);
    scene.git(&["commit", "-q", "-m", "link the hooks"]);

    let skipped = scene.created_on_branch("review/linked-skip", true).await;
    assert_eq!(scene.entries("host.log"), vec!["post-create".to_owned()]);
    assert_eq!(scene.entries("repo.log"), Vec::<String>::new());

    let control = scene.created_on_branch("review/linked-run", false).await;
    assert_eq!(
        scene.entries("repo.log"),
        vec![format!("repo:linked-post-create:{control}")],
        "the links resolve when the layer runs"
    );

    for id in [skipped, control] {
        let (code, removed) = scene.harness.json(&["session", "rm", &id]).await;
        assert_eq!(code, 0, "{removed}");
    }
    scene.harness.stop().await;
}

/// Rows 7 to 9: `hooks.required = ["post-create"]` with no host-global
/// post-create script refuses a switched create with `hook_required_skipped`
/// before anything runs or is created; the same create without the switch,
/// and a switched create once the host has the script, succeed.
#[tokio::test]
async fn a_required_hook_only_the_repository_could_run_refuses_the_switched_create() {
    let scene = Scene::start(vec![RequiredHook::PostCreate], &["pre-create"]).await;
    scene.init_repo(&scene.repo_hook_files());

    let (code, refused) = scene.new_on_branch("review/refused", true).await;
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["err"]["code"], "hook_required_skipped", "{refused}");
    assert_eq!(refused["err"]["class"], "configuration", "{refused}");
    assert_eq!(
        scene.entries("host.log"),
        Vec::<String>::new(),
        "no hook ran"
    );
    assert_eq!(
        scene.entries("repo.log"),
        Vec::<String>::new(),
        "no hook ran"
    );
    assert_eq!(scene.worktrees(), 1, "only the main checkout");
    assert!(!scene.branch_exists("review/refused"), "no branch");
    assert_eq!(scene.session_count().await, 0, "no session");
    let worktree_root = scene.harness.env.root().join("worktrees");
    let leftover = fs::read_dir(&worktree_root).map_or(0, Iterator::count);
    assert_eq!(leftover, 0, "no worktree directory");

    let control = scene.created_on_branch("review/control", false).await;
    assert!(
        scene
            .repo_entries_of(&control)
            .contains(&format!("repo:post-create:{control}")),
        "the repository satisfies the requirement without the switch"
    );

    scene.host_hook(
        "post-create",
        &scene.appending("host.log", "$POHUNEK_HOOK_EVENT"),
    );
    let switched = scene.created_on_branch("review/host-script", true).await;
    assert!(scene.host_ran("post-create"), "the host script ran");
    assert_eq!(scene.repo_entries_of(&switched), Vec::<String>::new());

    for id in [control, switched] {
        let (code, removed) = scene.harness.json(&["session", "rm", &id]).await;
        assert_eq!(code, 0, "{removed}");
    }
    scene.harness.stop().await;
}

/// Row 10: the host post-create script the check found is gone when the
/// post-create step runs (here the host pre-create hook removes it). The
/// switched create is refused with `hook_required_skipped` at that step and
/// rolled back, instead of succeeding with no post-create at all.
#[tokio::test]
async fn a_host_script_removed_after_the_check_still_refuses_the_switched_create() {
    let scene = Scene::start(vec![RequiredHook::PostCreate], &[]).await;
    scene.init_repo(&scene.repo_hook_files());
    let host_post_create = scene.harness.host_hooks_dir().join("post-create");
    scene.host_hook(
        "post-create",
        &scene.appending("host.log", "$POHUNEK_HOOK_EVENT"),
    );
    scene.host_hook(
        "pre-create",
        &format!(
            "#!/bin/sh\nrm -f '{}'\nprintf 'pre-create\\n' >> '{}'\n",
            host_post_create.display(),
            scene.logs.join("host.log").display()
        ),
    );

    let (code, refused) = scene.new_on_branch("review/vanished", true).await;
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["err"]["code"], "hook_required_skipped", "{refused}");
    assert_eq!(scene.entries("host.log"), vec!["pre-create".to_owned()]);
    assert_eq!(scene.entries("repo.log"), Vec::<String>::new());
    assert_eq!(scene.worktrees(), 1, "the checkout is rolled back");
    assert_eq!(scene.session_count().await, 0, "no session");
    scene.remove_host_hook("pre-create");
    scene.harness.stop().await;
}

/// Row 6: a switched create whose required host post-create fails is rolled
/// back with the host remove hooks only; no repository script of the checkout
/// runs during the compensation, and the error names none as skipped.
#[tokio::test]
async fn the_rollback_of_a_switched_create_runs_no_repository_hook() {
    let scene = Scene::start(
        vec![RequiredHook::PostCreate],
        &["pre-create", "pre-remove", "post-remove"],
    )
    .await;
    scene.init_repo(&scene.repo_hook_files());
    scene.host_hook("post-create", "#!/bin/sh\nexit 1\n");

    let (code, failed) = scene.new_on_branch("review/rollback", true).await;
    assert_ne!(code, 0, "{failed}");
    assert_eq!(failed["err"]["code"], "hook_required_failed", "{failed}");
    let message = failed["err"]["msg"].as_str().expect("error message");
    assert!(!message.contains("not run because of it"), "{message}");
    assert_eq!(
        scene.entries("host.log"),
        vec![
            "pre-create".to_owned(),
            "pre-remove".to_owned(),
            "post-remove".to_owned()
        ]
    );
    assert_eq!(scene.entries("repo.log"), Vec::<String>::new());
    assert_eq!(scene.worktrees(), 1, "the checkout is rolled back");
    scene.harness.stop().await;
}

/// Row 12: an in-place session started with the switch in a checkout holding
/// session hooks runs none of them; the host layer runs.
#[tokio::test]
async fn an_in_place_switched_session_runs_no_session_hook_of_its_directory() {
    let scene = Scene::start(
        Vec::new(),
        &["session-start", "agent-state", "session-stop"],
    )
    .await;
    scene.init_repo(&scene.repo_hook_files());
    let cwd = path_str(&scene.repo).to_owned();

    let (code, created) = scene
        .harness
        .json(&[
            "session",
            "new",
            "--agent",
            "toggler",
            "--cwd",
            &cwd,
            "--no-repo-hooks",
        ])
        .await;
    assert_eq!(code, 0, "{created}");
    assert_eq!(created["ok"]["skip_repo_hooks"], true, "{created}");
    let id = created["ok"]["id"].as_str().expect("session id").to_owned();
    for event in ["session-start", "agent-state"] {
        scene.wait_for_host(event).await;
    }
    scene.stop_and_remove(&id).await;

    assert_eq!(scene.entries("repo.log"), Vec::<String>::new());
    scene.harness.stop().await;
}

/// Row 14: a repeat of a keyed create that differs only in the switch is a
/// `session_request_conflict`, in both directions, while an identical repeat
/// replays the session.
#[tokio::test]
async fn a_keyed_repeat_that_differs_only_in_the_switch_conflicts() {
    let scene = Scene::start(Vec::new(), &[]).await;
    scene.init_repo(&[]);
    let cwd = path_str(&scene.repo).to_owned();
    let keyed = |key: &'static str, skip: bool| {
        let mut args = vec![
            "session",
            "new",
            "--agent",
            "toggler",
            "--cwd",
            cwd.as_str(),
            "--request-id",
            key,
        ];
        if skip {
            args.push("--no-repo-hooks");
        }
        args
    };

    for (key, first_skip) in [("req-2366-skip", true), ("req-2366-run", false)] {
        let (code, first) = scene.harness.json(&keyed(key, first_skip)).await;
        assert_eq!(code, 0, "{first}");
        let (code, replay) = scene.harness.json(&keyed(key, first_skip)).await;
        assert_eq!(code, 0, "{replay}");
        assert_eq!(replay["ok"]["replayed"], true, "{replay}");
        assert_eq!(replay["ok"]["id"], first["ok"]["id"], "{replay}");
        let (code, other) = scene.harness.json(&keyed(key, !first_skip)).await;
        assert_ne!(code, 0, "{other}");
        assert_eq!(other["err"]["code"], "session_request_conflict", "{other}");
    }
    scene.harness.stop().await;
}

/// D2 and item 3: `host inspect --json` advertises the switch and names which
/// events have a host-global script, without a path; the human output lists
/// those events.
#[tokio::test]
async fn host_inspect_advertises_the_switch_and_the_host_hook_scripts() {
    let scene = Scene::start(Vec::new(), &["post-create", "session-stop"]).await;

    let (code, inspected) = scene.harness.json(&["host", "inspect", "local"]).await;
    assert_eq!(code, 0, "{inspected}");
    let caps = &inspected["ok"];
    assert_eq!(
        caps["session_new_skip_repo_hooks_supported"], true,
        "{caps}"
    );
    assert_eq!(
        caps["host_hooks"],
        serde_json::json!({
            "pre_create": false,
            "post_create": true,
            "pre_remove": false,
            "post_remove": false,
            "session_start": false,
            "session_stop": true,
            "agent_state": false
        }),
        "{caps}"
    );
    let hooks_dir = scene.harness.host_hooks_dir();
    assert!(
        !inspected.to_string().contains(path_str(&hooks_dir)),
        "no hook path is reported: {inspected}"
    );

    let human = scene.harness.run(&["host", "inspect", "local"]).await;
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("host_hooks:         post-create, session-stop"),
        "{text}"
    );
    scene.harness.stop().await;
}
